use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as FmtWrite;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
#[cfg(test)]
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::codex::{CodexEdit, CodexPlanner, EDIT_TIMEOUT_SECONDS};
use crate::model::{Studio, StudioError, json_string};
use crate::prompt::{EditPlan, PromptEngine};
use crate::storage::ProjectStore;

const MAX_REQUEST_BYTES: usize = 64 * 1024;
const MAX_ACTIVE_EDIT_JOBS: usize = 4;
const MAX_RETAINED_EDIT_JOBS: usize = 64;
const CODEX_POLL_INTERVAL_MS: u64 = 1_000;
const DEMO_POLL_INTERVAL_MS: u64 = 25;
const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_CSS: &str = include_str!("../web/app.css");
const APP_JS: &str = include_str!("../web/app.js");
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn run(port: u16) -> io::Result<()> {
    install_shutdown_handlers();
    let router = Router::new()?;
    let address = format!("127.0.0.1:{port}");
    let listener = TcpListener::bind(&address)?;
    listener.set_nonblocking(true)?;
    println!("DAW-AI is ready at http://{address}");
    println!("Sound graph: {}", router.project_path().display());

    while !SHUTDOWN_REQUESTED.load(Ordering::SeqCst) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                let router = router.clone();
                thread::spawn(move || {
                    if let Err(error) = serve_connection(&mut stream, &router) {
                        eprintln!("request failed: {error}");
                    }
                });
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(error) => eprintln!("connection failed: {error}"),
        }
    }
    crate::codex::terminate_active_process_groups();
    Ok(())
}

#[cfg(unix)]
fn install_shutdown_handlers() {
    const SIGINT: i32 = 2;
    const SIGTERM: i32 = 15;

    unsafe extern "C" {
        fn signal(signal: i32, handler: unsafe extern "C" fn(i32)) -> usize;
    }

    unsafe extern "C" fn request_shutdown(_signal: i32) {
        SHUTDOWN_REQUESTED.store(true, Ordering::SeqCst);
    }

    let _ = unsafe { signal(SIGINT, request_shutdown) };
    let _ = unsafe { signal(SIGTERM, request_shutdown) };
}

#[cfg(not(unix))]
fn install_shutdown_handlers() {}

fn serve_connection(stream: &mut TcpStream, router: &Router) -> io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(5)))?;
    let response = match Request::read(stream) {
        Ok(request) => {
            let response = router.handle(&request);
            log_http_response(&request, &response);
            response
        }
        Err(error) => {
            eprintln!("warning: rejected HTTP request: {error}");
            Response::json(400, error_json(&error))
        }
    };
    response.write(stream)
}

#[derive(Clone)]
struct Router {
    studio: Arc<Mutex<Studio>>,
    store: Option<ProjectStore>,
    planner: Planner,
    edit_jobs: Arc<EditJobs>,
}

#[derive(Clone)]
enum Planner {
    Codex,
    Demo,
    #[cfg(test)]
    GatedDemo(Arc<PlannerGate>),
}

#[cfg(test)]
struct PlannerGate {
    state: Mutex<(bool, bool)>,
    changed: Condvar,
}

#[cfg(test)]
impl PlannerGate {
    fn new() -> Self {
        Self {
            state: Mutex::new((false, false)),
            changed: Condvar::new(),
        }
    }

    fn wait_until_released(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn wait_until_started(&self) {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (state, _) = self
            .changed
            .wait_timeout_while(state, Duration::from_secs(2), |state| !state.0)
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        assert!(state.0, "planner did not reach the test gate");
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.1 = true;
        self.changed.notify_all();
    }
}

enum PlannedEdit {
    Plan(EditPlan),
    Graph(CodexEdit),
}

struct EditJobs {
    next_id: AtomicU64,
    jobs: Mutex<BTreeMap<u64, EditJob>>,
}

struct EditJob {
    operation_id: String,
    started_at: Instant,
    finished_at: Option<Instant>,
    poll_after_ms: u64,
    state: EditJobState,
}

enum EditJobState {
    Queued,
    Running {
        phase: &'static str,
        detail: &'static str,
    },
    Completed {
        message: String,
    },
    Failed {
        status: u16,
        error: String,
    },
}

struct EditRequest {
    operation_id: String,
    prompt: String,
    start: f32,
    end: f32,
    project: crate::model::Project,
}

struct EditFailure {
    status: u16,
    message: String,
}

impl EditJobs {
    fn new() -> Self {
        Self {
            next_id: AtomicU64::new(1),
            jobs: Mutex::new(BTreeMap::new()),
        }
    }

    fn create(
        &self,
        poll_after_ms: u64,
        requested_operation_id: Option<&str>,
    ) -> Result<(u64, String, bool), ()> {
        let mut jobs = self.lock();
        if let Some(operation_id) = requested_operation_id {
            if let Some((id, job)) = jobs
                .iter()
                .find(|(_, job)| job.operation_id == operation_id)
            {
                return Ok((*id, job.operation_id.clone(), false));
            }
        }
        let active_jobs = jobs
            .values()
            .filter(|job| {
                matches!(
                    &job.state,
                    EditJobState::Queued | EditJobState::Running { .. }
                )
            })
            .count();
        if active_jobs >= MAX_ACTIVE_EDIT_JOBS {
            return Err(());
        }
        while jobs.len() >= MAX_RETAINED_EDIT_JOBS {
            let Some(id) = jobs.iter().find_map(|(id, job)| {
                matches!(
                    &job.state,
                    EditJobState::Completed { .. } | EditJobState::Failed { .. }
                )
                .then_some(*id)
            }) else {
                return Err(());
            };
            jobs.remove(&id);
        }
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let operation_id = requested_operation_id
            .map(str::to_owned)
            .unwrap_or_else(|| new_operation_id(id));
        jobs.insert(
            id,
            EditJob {
                operation_id: operation_id.clone(),
                started_at: Instant::now(),
                finished_at: None,
                poll_after_ms,
                state: EditJobState::Queued,
            },
        );
        Ok((id, operation_id, true))
    }

    fn response_for_operation(&self, operation_id: &str) -> Option<Response> {
        let id = self
            .jobs
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .iter()
            .find(|(_, job)| job.operation_id == operation_id)
            .map(|(id, _)| *id)?;
        self.response(id)
    }

    fn remove(&self, id: u64) {
        self.lock().remove(&id);
    }

    fn set_running(&self, id: u64, phase: &'static str, detail: &'static str) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.state = EditJobState::Running { phase, detail };
        }
    }

    fn complete(&self, id: u64, message: String) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.finished_at = Some(Instant::now());
            job.state = EditJobState::Completed { message };
        }
    }

    fn fail(&self, id: u64, status: u16, error: String) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.finished_at = Some(Instant::now());
            job.state = EditJobState::Failed { status, error };
        }
    }

    fn response(&self, id: u64) -> Option<Response> {
        self.lock()
            .get(&id)
            .map(|job| Response::json(200, edit_job_json(id, job)))
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<u64, EditJob>> {
        self.jobs
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

impl EditFailure {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            message: message.into(),
        }
    }
}

impl Router {
    fn new() -> io::Result<Self> {
        let planner = match std::env::var("DAW_AI_PROMPT_ENGINE") {
            Ok(value) if value == "demo" => Planner::Demo,
            _ => Planner::Codex,
        };
        let (store, studio) = ProjectStore::open_from_environment()?;
        Ok(Self {
            studio: Arc::new(Mutex::new(studio)),
            store: Some(store),
            planner,
            edit_jobs: Arc::new(EditJobs::new()),
        })
    }

    #[cfg(test)]
    fn demo() -> Self {
        Self {
            studio: Arc::new(Mutex::new(Studio::new())),
            store: None,
            planner: Planner::Demo,
            edit_jobs: Arc::new(EditJobs::new()),
        }
    }

    fn handle(&self, request: &Request) -> Response {
        let Some(public_host) = request.public_host() else {
            return Response::json(400, error_json("invalid host"));
        };
        if request.is_mutation() && !request.is_trusted_mutation(public_host) {
            return Response::json(403, error_json("cross-origin request rejected"));
        }
        if let Some(job_id) = edit_job_id(&request.path) {
            return if request.method == "GET" {
                self.edit_status(job_id)
            } else {
                Response::json(405, error_json("method not allowed")).with_header("Allow", "GET")
            };
        }
        if request.path.starts_with("/api/edits/") {
            return Response::json(404, error_json("edit job not found"));
        }

        match (request.method.as_str(), request.path.as_str()) {
            ("GET", "/") => Response::static_asset("text/html; charset=utf-8", INDEX_HTML),
            ("GET", "/app.css") => Response::static_asset("text/css; charset=utf-8", APP_CSS),
            ("GET", "/app.js") => Response::static_asset("text/javascript; charset=utf-8", APP_JS),
            ("GET", "/api/health") => Response::json(200, "{\"status\":\"ok\"}".to_owned()),
            ("GET", "/api/project") => {
                let studio = self.lock_studio();
                Response::json(200, studio.to_json())
            }
            ("POST", "/api/edits") => self.start_edit(&request.body),
            ("POST", "/api/mix") => self.change_mix(&request.body),
            ("POST", "/api/sound-tools") => self.change_sound_tool(&request.body),
            ("POST", "/api/logs") => Self::client_log(&request.body),
            ("POST", "/api/undo") => self.undo(),
            ("POST", "/api/reset") => self.reset(),
            (
                _,
                "/api/edits" | "/api/mix" | "/api/sound-tools" | "/api/logs" | "/api/undo"
                | "/api/reset",
            ) => Response::json(405, error_json("method not allowed")).with_header("Allow", "POST"),
            (_, "/api/project" | "/api/health") => {
                Response::json(405, error_json("method not allowed")).with_header("Allow", "GET")
            }
            _ => Response::json(404, error_json("not found")),
        }
    }

    fn start_edit(&self, body: &str) -> Response {
        let form = parse_form(body);
        let operation_id = form.get("operation_id").map(String::as_str);
        if operation_id.is_some_and(|operation_id| !valid_operation_id(operation_id)) {
            return Response::json(422, error_json("operation ID is invalid"));
        }
        if let Some(response) = operation_id
            .and_then(|operation_id| self.edit_jobs.response_for_operation(operation_id))
        {
            return response;
        }
        let Some(prompt) = form.get("prompt") else {
            return Response::json(422, error_json("prompt is required"));
        };
        let Some(start) = form
            .get("start")
            .and_then(|value| value.parse::<f32>().ok())
        else {
            return Response::json(422, error_json("selection start is required"));
        };
        let Some(end) = form.get("end").and_then(|value| value.parse::<f32>().ok()) else {
            return Response::json(422, error_json("selection end is required"));
        };
        let project = {
            let studio = self.lock_studio();
            if let Err(error) = studio.validate_edit(start, end, prompt) {
                return Response::json(422, studio_error(error));
            }
            studio.project().clone()
        };
        if let Some(operation_id) = operation_id {
            if let Some(edit) = project
                .edits
                .iter()
                .find(|edit| edit.operation_id.as_deref() == Some(operation_id))
            {
                return Response::json(200, recovered_edit_json(operation_id, edit));
            }
        }
        let poll_after_ms = match &self.planner {
            Planner::Codex => CODEX_POLL_INTERVAL_MS,
            Planner::Demo => DEMO_POLL_INTERVAL_MS,
            #[cfg(test)]
            Planner::GatedDemo(_) => DEMO_POLL_INTERVAL_MS,
        };
        let Ok((job_id, operation_id, created)) =
            self.edit_jobs.create(poll_after_ms, operation_id)
        else {
            return Response::json(503, error_json("too many edits are already being planned"));
        };
        if !created {
            return self
                .edit_jobs
                .response(job_id)
                .expect("an existing edit job has a response");
        }
        let edit = EditRequest {
            operation_id: operation_id.clone(),
            prompt: prompt.to_owned(),
            start,
            end,
            project,
        };
        let worker = self.clone();
        let spawn = thread::Builder::new()
            .name(format!("daw-ai-edit-{job_id}"))
            .spawn(move || worker.run_edit_job(job_id, edit));
        if let Err(error) = spawn {
            self.edit_jobs.remove(job_id);
            eprintln!("error: could not start edit worker: {error}");
            return Response::json(503, error_json("could not start the edit worker"));
        }
        Response::json(
            202,
            accepted_edit_job_json(job_id, &operation_id, poll_after_ms),
        )
    }

    fn edit_status(&self, job_id: u64) -> Response {
        self.edit_jobs
            .response(job_id)
            .unwrap_or_else(|| Response::json(404, error_json("edit job not found")))
    }

    fn run_edit_job(&self, job_id: u64, edit: EditRequest) {
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.perform_edit(job_id, edit)
        }));
        match result {
            Ok(Ok(message)) => self.edit_jobs.complete(job_id, message),
            Ok(Err(failure)) => {
                eprintln!(
                    "error: edit job {job_id} failed: {}",
                    single_line(&failure.message)
                );
                self.edit_jobs.fail(job_id, failure.status, failure.message);
            }
            Err(_) => {
                let message = "the edit worker stopped unexpectedly".to_owned();
                eprintln!("error: edit job {job_id} failed: {message}");
                self.edit_jobs.fail(job_id, 500, message);
            }
        }
    }

    fn perform_edit(&self, job_id: u64, edit: EditRequest) -> Result<String, EditFailure> {
        self.edit_jobs.set_running(
            job_id,
            "planning",
            "Codex is planning and editing the sound graph",
        );
        let planned_edit = self
            .plan_edit(&edit.prompt, edit.start, edit.end, &edit.project)
            .map_err(|message| EditFailure::new(503, message))?;
        self.edit_jobs.set_running(
            job_id,
            "applying",
            "Validating and saving the updated sound graph",
        );
        let mut studio = self.lock_studio();
        if studio.project().version != edit.project.version {
            return Err(EditFailure::new(
                409,
                "the project changed; submit the edit again",
            ));
        }
        let mut candidate = studio.clone();
        let result = match planned_edit {
            PlannedEdit::Plan(plan) => candidate.apply_plan_for_operation(
                edit.start,
                edit.end,
                &edit.prompt,
                edit.operation_id,
                plan,
            ),
            PlannedEdit::Graph(graph_edit) => candidate.replace_graph_for_operation(
                graph_edit.project,
                edit.start,
                edit.end,
                &edit.prompt,
                edit.operation_id,
                graph_edit.plan,
            ),
        };
        let summary = result.map_err(|error| EditFailure::new(422, studio_error_message(error)))?;
        self.commit(&mut studio, candidate)
            .map_err(|_| EditFailure::new(500, "could not save the sound graph"))?;
        Ok(summary)
    }

    fn plan_edit(
        &self,
        prompt: &str,
        start: f32,
        end: f32,
        project: &crate::model::Project,
    ) -> Result<PlannedEdit, String> {
        match &self.planner {
            Planner::Demo => Ok(PlannedEdit::Plan(PromptEngine::interpret_project(
                prompt, project, start, end,
            ))),
            Planner::Codex => CodexPlanner::interpret(prompt, start, end, project)
                .map(PlannedEdit::Graph)
                .map_err(|error| error.to_string()),
            #[cfg(test)]
            Planner::GatedDemo(gate) => {
                gate.wait_until_released();
                Ok(PlannedEdit::Plan(PromptEngine::interpret_project(
                    prompt, project, start, end,
                )))
            }
        }
    }

    fn change_mix(&self, body: &str) -> Response {
        let form = parse_form(body);
        let Some(track_id) = form
            .get("track_id")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return Response::json(422, error_json("track_id is required"));
        };
        let volume = match form.get("volume") {
            Some(value) => match value.parse::<f32>() {
                Ok(volume) => Some(volume),
                Err(_) => return Response::json(422, error_json("volume must be a number")),
            },
            None => None,
        };
        let muted = match form.get("muted") {
            Some(value) if value == "true" => Some(true),
            Some(value) if value == "false" => Some(false),
            Some(_) => return Response::json(422, error_json("muted must be true or false")),
            None => None,
        };

        let mut studio = self.lock_studio();
        let mut candidate = studio.clone();
        match candidate.set_mix(track_id, volume, muted) {
            Ok(()) => match self.commit(&mut studio, candidate) {
                Ok(()) => Response::json(200, studio.to_json()),
                Err(response) => response,
            },
            Err(error) => Response::json(422, studio_error(error)),
        }
    }

    fn change_sound_tool(&self, body: &str) -> Response {
        let form = parse_form(body);
        let Some(track_id) = form
            .get("track_id")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return Response::json(422, error_json("track_id is required"));
        };
        let Some(tool) = form.get("tool") else {
            return Response::json(422, error_json("tool is required"));
        };
        let Some(tool_id) = form
            .get("tool_id")
            .and_then(|value| value.parse::<u64>().ok())
        else {
            return Response::json(422, error_json("tool_id is required"));
        };
        let clip_id = match form.get("clip_id") {
            Some(value) => match value.parse::<u64>() {
                Ok(value) => Some(value),
                Err(_) => return Response::json(422, error_json("clip_id must be an integer")),
            },
            None => None,
        };
        let Some(parameter) = form.get("parameter") else {
            return Response::json(422, error_json("parameter is required"));
        };
        let Some(value) = form.get("value") else {
            return Response::json(422, error_json("value is required"));
        };

        let mut studio = self.lock_studio();
        let mut candidate = studio.clone();
        match candidate.configure_sound_tool(track_id, tool, tool_id, clip_id, parameter, value) {
            Ok(()) => match self.commit(&mut studio, candidate) {
                Ok(()) => Response::json(200, studio.to_json()),
                Err(response) => response,
            },
            Err(error) => Response::json(422, studio_error(error)),
        }
    }

    fn client_log(body: &str) -> Response {
        let form = parse_form(body);
        let Some(level) = form.get("level").map(String::as_str) else {
            return Response::json(422, error_json("log level is required"));
        };
        if !matches!(level, "warning" | "error") {
            return Response::json(422, error_json("log level must be warning or error"));
        }
        let Some(message) = form.get("message").map(|message| message.trim()) else {
            return Response::json(422, error_json("log message is required"));
        };
        if message.is_empty() || message.chars().count() > 4_096 {
            return Response::json(422, error_json("log message length is invalid"));
        }
        let context = form
            .get("context")
            .map(|context| context.trim())
            .filter(|context| !context.is_empty())
            .unwrap_or("browser");
        if context.chars().count() > 160 {
            return Response::json(422, error_json("log context length is invalid"));
        }
        eprintln!(
            "client {level}: {}: {}",
            single_line(context),
            single_line(message)
        );
        Response::json(200, "{\"status\":\"logged\"}".to_owned())
    }

    fn undo(&self) -> Response {
        let mut studio = self.lock_studio();
        let mut candidate = studio.clone();
        if candidate.undo() {
            match self.commit(&mut studio, candidate) {
                Ok(()) => Response::json(200, studio.to_json()),
                Err(response) => response,
            }
        } else {
            Response::json(409, error_json("nothing to undo"))
        }
    }

    fn reset(&self) -> Response {
        let mut studio = self.lock_studio();
        let mut candidate = studio.clone();
        candidate.reset();
        match self.commit(&mut studio, candidate) {
            Ok(()) => Response::json(200, studio.to_json()),
            Err(response) => response,
        }
    }

    fn commit(
        &self,
        studio: &mut std::sync::MutexGuard<'_, Studio>,
        candidate: Studio,
    ) -> Result<(), Response> {
        if let Some(store) = &self.store {
            if let Err(error) = store.save(candidate.project()) {
                eprintln!("error: could not save sound graph: {error}");
                return Err(Response::json(
                    500,
                    error_json("could not save the sound graph"),
                ));
            }
        }
        **studio = candidate;
        Ok(())
    }

    fn project_path(&self) -> &std::path::Path {
        self.store
            .as_ref()
            .expect("production router has a project store")
            .path()
    }

    fn lock_studio(&self) -> std::sync::MutexGuard<'_, Studio> {
        self.studio
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

struct Request {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: String,
}

impl Request {
    fn read(stream: &mut impl Read) -> Result<Self, String> {
        let mut bytes = Vec::with_capacity(2048);
        let header_end = loop {
            let mut chunk = [0_u8; 2048];
            let count = stream.read(&mut chunk).map_err(|error| error.to_string())?;
            if count == 0 {
                return Err("incomplete request".to_owned());
            }
            bytes.extend_from_slice(&chunk[..count]);
            if bytes.len() > MAX_REQUEST_BYTES {
                return Err("request is too large".to_owned());
            }
            if let Some(position) = find_bytes(&bytes, b"\r\n\r\n") {
                break position + 4;
            }
        };

        let headers = std::str::from_utf8(&bytes[..header_end])
            .map_err(|_| "request headers must be UTF-8".to_owned())?;
        let mut lines = headers.split("\r\n");
        let request_line = lines
            .next()
            .ok_or_else(|| "missing request line".to_owned())?;
        let mut request_parts = request_line.split_whitespace();
        let method = request_parts
            .next()
            .ok_or_else(|| "missing method".to_owned())?
            .to_owned();
        let target = request_parts
            .next()
            .ok_or_else(|| "missing path".to_owned())?;
        if request_parts.next().is_none() {
            return Err("missing HTTP version".to_owned());
        }
        let path = target.split('?').next().unwrap_or(target).to_owned();

        let headers: HashMap<String, String> = lines
            .filter_map(|line| line.split_once(':'))
            .map(|(name, value)| (name.trim().to_lowercase(), value.trim().to_owned()))
            .collect();
        let content_length = headers.get("content-length").map_or(Ok(0_usize), |value| {
            value
                .parse::<usize>()
                .map_err(|_| "invalid content length".to_owned())
        })?;
        let body_end = header_end
            .checked_add(content_length)
            .ok_or_else(|| "request is too large".to_owned())?;
        if body_end > MAX_REQUEST_BYTES {
            return Err("request is too large".to_owned());
        }

        while bytes.len() < body_end {
            let remaining = body_end - bytes.len();
            let mut chunk = [0_u8; 2048];
            let count = stream
                .read(&mut chunk[..remaining.min(2048)])
                .map_err(|error| error.to_string())?;
            if count == 0 {
                return Err("incomplete request body".to_owned());
            }
            bytes.extend_from_slice(&chunk[..count]);
        }

        let body = std::str::from_utf8(&bytes[header_end..body_end])
            .map_err(|_| "request body must be UTF-8".to_owned())?
            .to_owned();
        Ok(Self {
            method,
            path,
            headers,
            body,
        })
    }

    fn is_mutation(&self) -> bool {
        self.method == "POST"
            && (matches!(
                self.path.as_str(),
                "/api/edits"
                    | "/api/mix"
                    | "/api/sound-tools"
                    | "/api/logs"
                    | "/api/undo"
                    | "/api/reset"
            ) || self.path.starts_with("/api/edits/"))
    }

    fn public_host(&self) -> Option<&str> {
        let transport_host = self.headers.get("host")?;
        parse_authority(transport_host)?;
        let forwarded = match self.headers.get("x-forwarded-host") {
            Some(value) => Some(forwarded_host(value)?),
            None => None,
        };
        forwarded.or(Some(transport_host))
    }

    fn is_trusted_mutation(&self, host: &str) -> bool {
        if self
            .headers
            .get("sec-fetch-site")
            .is_some_and(|site| site.eq_ignore_ascii_case("cross-site"))
        {
            return false;
        }

        self.headers
            .get("origin")
            .is_none_or(|origin| origin_matches_host(origin, host))
    }
}

struct Response {
    status: u16,
    content_type: &'static str,
    body: String,
    headers: Vec<(&'static str, &'static str)>,
}

impl Response {
    fn json(status: u16, body: String) -> Self {
        Self {
            status,
            content_type: "application/json; charset=utf-8",
            body,
            headers: vec![("Cache-Control", "no-store")],
        }
    }

    fn static_asset(content_type: &'static str, body: &str) -> Self {
        Self {
            status: 200,
            content_type,
            body: body.to_owned(),
            headers: vec![("Cache-Control", "no-cache")],
        }
    }

    fn with_header(mut self, name: &'static str, value: &'static str) -> Self {
        self.headers.push((name, value));
        self
    }

    fn write(&self, stream: &mut impl Write) -> io::Result<()> {
        let reason = match self.status {
            200 => "OK",
            202 => "Accepted",
            400 => "Bad Request",
            403 => "Forbidden",
            404 => "Not Found",
            405 => "Method Not Allowed",
            409 => "Conflict",
            422 => "Unprocessable Content",
            _ => "Error",
        };
        let mut head = format!(
            concat!(
                "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\n",
                "Connection: close\r\nX-Content-Type-Options: nosniff\r\n",
                "Content-Security-Policy: default-src 'self'; script-src 'self'; ",
                "style-src 'self' 'unsafe-inline'; connect-src 'self'; img-src 'self' data:; ",
                "object-src 'none'; frame-ancestors 'none'; base-uri 'none';\r\n",
                "Referrer-Policy: no-referrer\r\n"
            ),
            self.status,
            reason,
            self.content_type,
            self.body.len()
        );
        for (name, value) in &self.headers {
            head.push_str(name);
            head.push_str(": ");
            head.push_str(value);
            head.push_str("\r\n");
        }
        head.push_str("\r\n");
        stream.write_all(head.as_bytes())?;
        stream.write_all(self.body.as_bytes())
    }
}

fn new_operation_id(id: u64) -> String {
    let mut random = [0_u8; 16];
    if File::open("/dev/urandom")
        .and_then(|mut source| source.read_exact(&mut random))
        .is_ok()
    {
        let mut token = String::with_capacity(32);
        for byte in random {
            write!(token, "{byte:02x}").expect("writing to a string cannot fail");
        }
        return token;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{nanos:x}-{:x}-{id:x}", std::process::id())
}

fn valid_operation_id(operation_id: &str) -> bool {
    !operation_id.is_empty()
        && operation_id.len() <= 128
        && operation_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn recovered_edit_json(operation_id: &str, edit: &crate::model::Edit) -> String {
    format!(
        concat!(
            "{{\"id\":\"recovered\",\"operationId\":{},\"status\":\"completed\",",
            "\"phase\":\"completed\",\"message\":{},\"elapsedSeconds\":0,",
            "\"timeoutSeconds\":{}}}"
        ),
        json_string(operation_id),
        json_string(&edit.summary),
        EDIT_TIMEOUT_SECONDS
    )
}

fn accepted_edit_job_json(id: u64, operation_id: &str, poll_after_ms: u64) -> String {
    format!(
        concat!(
            "{{\"id\":\"{}\",\"operationId\":{},\"status\":\"queued\",\"phase\":\"queued\",",
            "\"detail\":\"Waiting for the edit worker\",\"elapsedSeconds\":0,",
            "\"timeoutSeconds\":{},\"pollAfterMs\":{}}}"
        ),
        id,
        json_string(operation_id),
        EDIT_TIMEOUT_SECONDS,
        poll_after_ms
    )
}

fn edit_job_json(id: u64, job: &EditJob) -> String {
    let ended_at = job.finished_at.unwrap_or_else(Instant::now);
    let elapsed = ended_at.saturating_duration_since(job.started_at).as_secs();
    let common = format!(
        "\"id\":\"{}\",\"operationId\":{},\"elapsedSeconds\":{},\"timeoutSeconds\":{}",
        id,
        json_string(&job.operation_id),
        elapsed,
        EDIT_TIMEOUT_SECONDS
    );
    match &job.state {
        EditJobState::Queued => format!(
            concat!(
                "{{{},\"status\":\"queued\",\"phase\":\"queued\",",
                "\"detail\":\"Waiting for the edit worker\",",
                "\"pollAfterMs\":{}}}"
            ),
            common, job.poll_after_ms
        ),
        EditJobState::Running { phase, detail } => format!(
            concat!(
                "{{{},\"status\":\"running\",\"phase\":{},\"detail\":{},",
                "\"pollAfterMs\":{}}}"
            ),
            common,
            json_string(phase),
            json_string(detail),
            job.poll_after_ms
        ),
        EditJobState::Completed { message } => format!(
            "{{{},\"status\":\"completed\",\"phase\":\"completed\",\"message\":{}}}",
            common,
            json_string(message)
        ),
        EditJobState::Failed { status, error } => format!(
            "{{{},\"status\":\"failed\",\"phase\":\"failed\",\"errorStatus\":{},\"error\":{}}}",
            common,
            status,
            json_string(error)
        ),
    }
}

fn edit_job_id(path: &str) -> Option<u64> {
    let id = path.strip_prefix("/api/edits/")?;
    (!id.is_empty() && id.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| id.parse::<u64>().ok())
        .flatten()
}

fn parse_form(body: &str) -> HashMap<String, String> {
    body.split('&')
        .filter(|part| !part.is_empty())
        .map(|part| {
            let (key, value) = part.split_once('=').unwrap_or((part, ""));
            (url_decode(key), url_decode(value))
        })
        .collect()
}

fn forwarded_host(value: &str) -> Option<&str> {
    let host = value.split(',').next()?.trim();
    parse_authority(host).map(|_| host)
}

fn origin_matches_host(origin: &str, host: &str) -> bool {
    let (authority, default_port) = origin
        .strip_prefix("http://")
        .map(|authority| (authority, 80))
        .or_else(|| {
            origin
                .strip_prefix("https://")
                .map(|authority| (authority, 443))
        })
        .unwrap_or(("", 0));
    if default_port == 0 {
        return false;
    }
    let Some((origin_host, origin_port)) = parse_authority(authority) else {
        return false;
    };
    let Some((request_host, request_port)) = parse_authority(host) else {
        return false;
    };
    origin_host.eq_ignore_ascii_case(request_host)
        && origin_port.unwrap_or(default_port) == request_port.unwrap_or(default_port)
}

fn parse_authority(value: &str) -> Option<(&str, Option<u16>)> {
    if value.is_empty()
        || value
            .bytes()
            .any(|byte| byte.is_ascii_control() || byte.is_ascii_whitespace())
        || value.contains(['/', '\\', '?', '#', '@', ','])
    {
        return None;
    }

    if value.starts_with('[') {
        let end = value.find(']')?;
        let hostname = &value[..=end];
        if hostname.len() <= 2 {
            return None;
        }
        let remainder = &value[end + 1..];
        let port = if remainder.is_empty() {
            None
        } else {
            Some(parse_port(remainder.strip_prefix(':')?)?)
        };
        return Some((hostname, port));
    }

    let (hostname, port) = value
        .rsplit_once(':')
        .map_or((value, None), |(hostname, port)| (hostname, Some(port)));
    if hostname.is_empty()
        || hostname.contains(':')
        || !hostname
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
    {
        return None;
    }
    let port = match port {
        Some(port) => Some(parse_port(port)?),
        None => None,
    };
    Some((hostname, port))
}

fn parse_port(value: &str) -> Option<u16> {
    value.parse::<u16>().ok().filter(|port| *port > 0)
}

fn url_decode(value: &str) -> String {
    let bytes = value.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'+' => output.push(b' '),
            b'%' if index + 2 < bytes.len() => {
                if let (Some(high), Some(low)) =
                    (hex_value(bytes[index + 1]), hex_value(bytes[index + 2]))
                {
                    output.push(high * 16 + low);
                    index += 2;
                } else {
                    output.push(bytes[index]);
                }
            }
            byte => output.push(byte),
        }
        index += 1;
    }
    String::from_utf8_lossy(&output).into_owned()
}

const fn hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        b'A'..=b'F' => Some(byte - b'A' + 10),
        _ => None,
    }
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

fn error_json(message: &str) -> String {
    format!("{{\"error\":{}}}", json_string(message))
}

fn single_line(value: &str) -> String {
    value
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn log_http_response(request: &Request, response: &Response) {
    if response.status >= 500 {
        eprintln!(
            "error: {} {} returned {}",
            request.method, request.path, response.status
        );
    } else if response.status >= 400 {
        eprintln!(
            "warning: {} {} returned {}",
            request.method, request.path, response.status
        );
    }
}

fn studio_error(error: StudioError) -> String {
    error_json(studio_error_message(error))
}

const fn studio_error_message(error: StudioError) -> &'static str {
    match error {
        StudioError::EmptyPrompt => "describe the change you want",
        StudioError::InvalidPrompt => "prompt is too long",
        StudioError::InvalidSelection => "select a valid part of the track",
        StudioError::UnknownTrack => "track not found",
        StudioError::InvalidMix => "invalid mixer setting",
        StudioError::UnknownSoundTool => "sound tool not found",
        StudioError::InvalidSoundTool => "invalid sound tool setting",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static TEST_FILE_ID: AtomicU64 = AtomicU64::new(1);

    fn request(method: &str, path: &str, body: &str) -> Request {
        Request {
            method: method.to_owned(),
            path: path.to_owned(),
            headers: HashMap::from([("host".to_owned(), "127.0.0.1:8888".to_owned())]),
            body: body.to_owned(),
        }
    }

    fn wait_for_edit(router: &Router, accepted: &Response) -> serde_json::Value {
        assert_eq!(accepted.status, 202);
        let accepted: serde_json::Value =
            serde_json::from_str(&accepted.body).expect("accepted edit JSON");
        assert_eq!(accepted["status"], "queued");
        assert_eq!(accepted["timeoutSeconds"], EDIT_TIMEOUT_SECONDS);
        assert!(
            accepted["operationId"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        );
        let path = format!(
            "/api/edits/{}",
            accepted["id"].as_str().expect("edit job ID")
        );
        for _ in 0..200 {
            let response = router.handle(&request("GET", &path, ""));
            assert_eq!(response.status, 200);
            let job: serde_json::Value =
                serde_json::from_str(&response.body).expect("edit status JSON");
            if matches!(job["status"].as_str(), Some("completed" | "failed")) {
                return job;
            }
            thread::sleep(Duration::from_millis(5));
        }
        panic!("edit job did not finish");
    }

    fn persisted_demo() -> (Router, std::path::PathBuf) {
        let id = TEST_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "daw-ai-server-test-{}-{id}.json",
            std::process::id()
        ));
        let (store, studio) = ProjectStore::open(path.clone()).expect("test project store");
        (
            Router {
                studio: Arc::new(Mutex::new(studio)),
                store: Some(store),
                planner: Planner::Demo,
                edit_jobs: Arc::new(EditJobs::new()),
            },
            path,
        )
    }

    #[test]
    fn serves_the_app_and_project_api() {
        let router = Router::demo();
        let page = router.handle(&request("GET", "/", ""));
        assert_eq!(page.status, 200);
        assert!(page.body.contains("DAW-AI"));

        let project = router.handle(&request("GET", "/api/project", ""));
        assert_eq!(project.status, 200);
        assert!(project.body.contains("\"tracks\""));
    }

    #[test]
    fn edit_job_status_reports_phase_progress_and_failures() {
        let jobs = EditJobs::new();
        let (id, operation_id, created) = jobs.create(750, None).expect("edit job");
        assert!(created);
        jobs.set_running(id, "planning", "Codex is arranging the requested change");
        let running: serde_json::Value =
            serde_json::from_str(&jobs.response(id).expect("running job response").body)
                .expect("running job JSON");
        assert_eq!(running["status"], "running");
        assert_eq!(running["phase"], "planning");
        assert_eq!(running["detail"], "Codex is arranging the requested change");
        assert_eq!(running["pollAfterMs"], 750);
        assert_eq!(running["operationId"], operation_id);

        jobs.fail(id, 503, "Codex timed out".to_owned());
        let failed: serde_json::Value =
            serde_json::from_str(&jobs.response(id).expect("failed job response").body)
                .expect("failed job JSON");
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["errorStatus"], 503);
        assert_eq!(failed["error"], "Codex timed out");
    }

    #[test]
    fn edit_jobs_bound_concurrent_project_snapshots() {
        let jobs = EditJobs::new();
        let active = (0..MAX_ACTIVE_EDIT_JOBS)
            .map(|_| jobs.create(750, None).expect("active edit job").0)
            .collect::<Vec<_>>();
        assert!(jobs.create(750, None).is_err());

        jobs.fail(active[0], 503, "planner stopped".to_owned());
        assert!(jobs.create(750, None).is_ok());
    }

    #[test]
    fn edit_api_updates_the_shared_project() {
        let router = Router::demo();
        let response = router.handle(&request(
            "POST",
            "/api/edits",
            "start=4&end=8&prompt=increase+volume",
        ));
        let completed = wait_for_edit(&router, &response);
        assert_eq!(completed["status"], "completed");
        assert!(completed["message"].as_str().unwrap().contains("Lifted"));
        assert!(completed.get("project").is_none());

        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("increase volume"));
        assert_eq!(
            router
                .handle(&request("GET", "/api/edits/999999", ""))
                .status,
            404
        );
        assert_eq!(
            router.handle(&request("POST", "/api/edits/1", "")).status,
            405
        );
    }

    #[test]
    fn sound_tool_api_updates_the_shared_graph() {
        let router = Router::demo();
        let response = router.handle(&request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=instrument&tool_id=201&parameter=waveform&value=sawtooth",
        ));
        assert_eq!(response.status, 200);
        assert!(response.body.contains("\"waveform\":\"sawtooth\""));

        let invalid = router.handle(&request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=instrument&tool_id=201&parameter=attack&value=99",
        ));
        assert_eq!(invalid.status, 422);
        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("\"waveform\":\"sawtooth\""));
    }

    #[test]
    fn accepts_bounded_client_error_and_warning_logs() {
        let router = Router::demo();
        let error = router.handle(&request(
            "POST",
            "/api/logs",
            "level=error&context=starting+audio&message=AudioContext+failed",
        ));
        assert_eq!(error.status, 200);
        assert_eq!(error.body, "{\"status\":\"logged\"}");

        let warning = router.handle(&request(
            "POST",
            "/api/logs",
            "level=warning&message=Recovered+from+an+invalid+node",
        ));
        assert_eq!(warning.status, 200);
        assert_eq!(
            router
                .handle(&request("POST", "/api/logs", "level=info&message=no"))
                .status,
            422
        );
        assert_eq!(router.handle(&request("GET", "/api/logs", "")).status, 405);
    }

    #[test]
    fn successful_api_mutations_persist_the_sound_graph() {
        let (router, path) = persisted_demo();
        let response = router.handle(&request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=instrument&tool_id=201&parameter=waveform&value=sawtooth",
        ));
        assert_eq!(response.status, 200);
        let saved = ProjectStore::open(path.clone()).expect("saved project").1;
        assert_eq!(saved.project().tracks[1].instrument.waveform, "sawtooth");
        std::fs::remove_file(path).expect("remove test graph");
    }

    #[test]
    fn completed_async_edits_persist_the_sound_graph() {
        let (router, path) = persisted_demo();
        let accepted = router.handle(&request(
            "POST",
            "/api/edits",
            "operation_id=persisted-operation&start=4&end=8&prompt=increase+volume",
        ));
        let operation_id = serde_json::from_str::<serde_json::Value>(&accepted.body)
            .expect("accepted edit JSON")["operationId"]
            .as_str()
            .expect("operation ID")
            .to_owned();
        let completed = wait_for_edit(&router, &accepted);
        assert_eq!(completed["status"], "completed");

        let recovered = router.handle(&request(
            "POST",
            "/api/edits",
            "operation_id=persisted-operation&start=4&end=8&prompt=increase+volume",
        ));
        assert_eq!(recovered.status, 200);
        let recovered: serde_json::Value =
            serde_json::from_str(&recovered.body).expect("recovered edit JSON");
        assert_eq!(recovered["status"], "completed");
        assert_eq!(recovered["operationId"], operation_id);

        let saved = ProjectStore::open(path.clone()).expect("saved project").1;
        assert_eq!(saved.project().edits.len(), 1);
        assert_eq!(saved.project().edits[0].prompt, "increase volume");
        assert_eq!(
            saved.project().edits[0].operation_id.as_deref(),
            Some(operation_id.as_str())
        );
        std::fs::remove_file(path).expect("remove test graph");
    }

    #[test]
    fn repeated_operation_id_reuses_the_active_edit_job() {
        let gate = Arc::new(PlannerGate::new());
        let mut router = Router::demo();
        router.planner = Planner::GatedDemo(gate.clone());
        let body = "operation_id=client-operation&start=4&end=8&prompt=increase+volume";
        let accepted = router.handle(&request("POST", "/api/edits", body));
        gate.wait_until_started();

        let duplicate = router.handle(&request("POST", "/api/edits", body));
        assert_eq!(duplicate.status, 200);
        let accepted_json: serde_json::Value =
            serde_json::from_str(&accepted.body).expect("accepted edit JSON");
        let duplicate_json: serde_json::Value =
            serde_json::from_str(&duplicate.body).expect("duplicate edit JSON");
        assert_eq!(duplicate_json["id"], accepted_json["id"]);
        assert_eq!(duplicate_json["operationId"], "client-operation");
        assert_eq!(duplicate_json["status"], "running");

        gate.release();
        let completed = wait_for_edit(&router, &accepted);
        assert_eq!(completed["status"], "completed");
        let project: serde_json::Value =
            serde_json::from_str(&router.handle(&request("GET", "/api/project", "")).body)
                .expect("project JSON");
        assert_eq!(project["edits"].as_array().expect("project edits").len(), 1);
        assert_eq!(project["edits"][0]["operationId"], "client-operation");
    }

    #[test]
    fn async_edit_rejects_a_result_when_the_project_changed_while_planning() {
        let gate = Arc::new(PlannerGate::new());
        let mut router = Router::demo();
        router.planner = Planner::GatedDemo(gate.clone());
        let accepted = router.handle(&request(
            "POST",
            "/api/edits",
            "start=4&end=8&prompt=increase+volume",
        ));
        gate.wait_until_started();

        let mutation = router.handle(&request("POST", "/api/mix", "track_id=1&volume=0.5"));
        assert_eq!(mutation.status, 200);
        let newer_project = router.handle(&request("GET", "/api/project", "")).body;
        gate.release();

        let failed = wait_for_edit(&router, &accepted);
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["errorStatus"], 409);
        assert_eq!(
            failed["error"],
            "the project changed; submit the edit again"
        );
        assert_eq!(
            router.handle(&request("GET", "/api/project", "")).body,
            newer_project
        );
        assert!(!newer_project.contains("increase volume"));
    }

    #[test]
    fn validates_api_requests_and_methods() {
        let router = Router::demo();
        assert_eq!(
            router
                .handle(&request("POST", "/api/edits", "start=1&end=2"))
                .status,
            422
        );
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/edits",
                    "operation_id=not%20valid&start=4&end=8&prompt=increase+volume",
                ))
                .status,
            422
        );
        let before = router.handle(&request("GET", "/api/project", "")).body;
        let accepted = router.handle(&request(
            "POST",
            "/api/edits",
            "start=4&end=8&prompt=make+the+lead+louder",
        ));
        let failed = wait_for_edit(&router, &accepted);
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["errorStatus"], 422);
        assert_eq!(failed["error"], "track not found");
        assert_eq!(
            router.handle(&request("GET", "/api/project", "")).body,
            before
        );
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/mix",
                    "track_id=1&volume=bad&muted=true",
                ))
                .status,
            422
        );
        assert_eq!(
            router.handle(&request("GET", "/api/project", "")).body,
            before
        );
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/mix",
                    "track_id=1&volume=0.5&muted=maybe",
                ))
                .status,
            422
        );
        assert_eq!(
            router.handle(&request("GET", "/api/project", "")).body,
            before
        );
        assert_eq!(router.handle(&request("GET", "/missing", "")).status, 404);
        assert_eq!(router.handle(&request("GET", "/api/undo", "")).status, 405);
    }

    #[test]
    fn parses_http_request_and_encoded_forms() {
        let body = "prompt=warm+%26+wide&start=0&end=4";
        let raw = format!(
            "POST /api/edits HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        );
        let parsed = Request::read(&mut raw.as_bytes()).expect("valid request");
        assert_eq!(parsed.path, "/api/edits");
        assert_eq!(parsed.headers["host"], "localhost");
        assert_eq!(parse_form(&parsed.body)["prompt"], "warm & wide");
    }

    #[test]
    fn rejects_content_lengths_that_overflow_the_request_bound() {
        let raw = format!(
            "POST /api/edits HTTP/1.1\r\nHost: localhost\r\nContent-Length: {}\r\n\r\n",
            usize::MAX
        );
        let error = Request::read(&mut raw.as_bytes())
            .err()
            .expect("oversized request must be rejected");
        assert_eq!(error, "request is too large");
    }

    #[test]
    fn rejects_cross_origin_mutations_without_changing_state() {
        let router = Router::demo();
        let mut hostile = request("POST", "/api/edits", "start=4&end=8&prompt=increase+volume");
        hostile
            .headers
            .insert("origin".to_owned(), "http://127.0.0.1:18867".to_owned());
        hostile
            .headers
            .insert("sec-fetch-site".to_owned(), "cross-site".to_owned());

        assert_eq!(router.handle(&hostile).status, 403);
        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(!project.body.contains("increase volume"));

        hostile.path = "/api/sound-tools".to_owned();
        hostile.body =
            "track_id=2&tool=instrument&tool_id=201&parameter=waveform&value=sawtooth".to_owned();
        assert_eq!(router.handle(&hostile).status, 403);
        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("\"waveform\":\"square\""));
        assert!(!project.body.contains("\"waveform\":\"sawtooth\""));

        hostile
            .headers
            .insert("x-forwarded-host".to_owned(), "studio.example".to_owned());
        hostile
            .headers
            .insert("origin".to_owned(), "https://attacker.example".to_owned());
        hostile
            .headers
            .insert("sec-fetch-site".to_owned(), "same-origin".to_owned());
        assert_eq!(router.handle(&hostile).status, 403);

        hostile
            .headers
            .insert("origin".to_owned(), "https://studio.example".to_owned());
        assert_eq!(router.handle(&hostile).status, 200);
        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("\"waveform\":\"sawtooth\""));
    }

    #[test]
    fn supports_reverse_proxy_hosts_without_configuration() {
        let router = Router::demo();
        let mut forwarded = request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=instrument&tool_id=201&parameter=waveform&value=sawtooth",
        );
        forwarded.headers.insert(
            "x-forwarded-host".to_owned(),
            "studio.example:443, proxy.internal".to_owned(),
        );
        forwarded
            .headers
            .insert("origin".to_owned(), "https://studio.example".to_owned());
        forwarded
            .headers
            .insert("sec-fetch-site".to_owned(), "same-origin".to_owned());
        assert_eq!(router.handle(&forwarded).status, 200);
    }

    #[test]
    fn supports_public_hosts_without_configuration() {
        let router = Router::demo();
        let mut public = request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=instrument&tool_id=201&parameter=waveform&value=sawtooth",
        );
        public
            .headers
            .insert("host".to_owned(), "studio.example".to_owned());
        public
            .headers
            .insert("origin".to_owned(), "https://studio.example".to_owned());
        public
            .headers
            .insert("sec-fetch-site".to_owned(), "same-origin".to_owned());

        assert_eq!(router.handle(&public).status, 200);
        public
            .headers
            .insert("origin".to_owned(), "https://attacker.example".to_owned());
        assert_eq!(router.handle(&public).status, 403);
    }

    #[test]
    fn serves_the_project_for_any_valid_hostname() {
        let router = Router::demo();
        let mut public = request("GET", "/api/project", "");
        public
            .headers
            .insert("host".to_owned(), "music.private.example:8443".to_owned());

        let response = router.handle(&public);
        assert_eq!(response.status, 200);
        assert!(response.body.contains("Neon First Light"));

        public.method = "POST".to_owned();
        public.path = "/api/sound-tools".to_owned();
        public.body =
            "track_id=2&tool=instrument&tool_id=201&parameter=waveform&value=sawtooth".to_owned();
        public.headers.insert(
            "origin".to_owned(),
            "http://music.private.example:8443".to_owned(),
        );
        public
            .headers
            .insert("sec-fetch-site".to_owned(), "same-origin".to_owned());
        assert_eq!(router.handle(&public).status, 200);

        let project = router.handle(&request("GET", "/api/project", ""));
        assert!(project.body.contains("\"waveform\":\"sawtooth\""));
    }

    #[test]
    fn rejects_malformed_host_authorities() {
        let router = Router::demo();
        let mut invalid = request("GET", "/api/project", "");
        invalid
            .headers
            .insert("host".to_owned(), "studio.example/path".to_owned());

        let response = router.handle(&invalid);
        assert_eq!(response.status, 400);
        assert!(!response.body.contains("Neon First Light"));
    }

    #[test]
    fn parses_public_and_ipv6_authorities() {
        assert_eq!(
            parse_authority("studio.example:8443"),
            Some(("studio.example", Some(8443)))
        );
        assert_eq!(parse_authority("[::1]:8888"), Some(("[::1]", Some(8888))));
        assert_eq!(parse_authority("studio.example/path"), None);
    }

    #[test]
    fn response_contains_security_and_length_headers() {
        let response = Response::json(200, "{\"ok\":true}".to_owned());
        let mut bytes = Vec::new();
        response.write(&mut bytes).expect("writable buffer");
        let rendered = String::from_utf8(bytes).expect("UTF-8 response");
        assert!(rendered.contains("Content-Length: 11"));
        assert!(rendered.contains("X-Content-Type-Options: nosniff"));
    }
}
