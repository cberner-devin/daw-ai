use std::collections::{BTreeMap, HashMap};
use std::fmt::Write as FmtWrite;
use std::fs::File;
use std::io::{self, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Condvar;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::audio_analysis;
use crate::gemini::{EDIT_TIMEOUT_SECONDS, GeminiEdit, GeminiPlanner, PlannerError};
use crate::gemini_tools::{base64_audio, render_audio_request};
use crate::model::{ChannelOperationAction, Studio, StudioError, json_string};
use crate::prompt::{EditPlan, PromptEngine};
use crate::storage::ProjectStore;

const MAX_REQUEST_BYTES: usize = 6 * 1024 * 1024;
const MAX_ACTIVE_EDIT_JOBS: usize = 4;
const MAX_RETAINED_EDIT_JOBS: usize = 64;
const AUDIO_REQUEST_HEADER: &str = "x-daw-ai-audio";
const GEMINI_POLL_INTERVAL_MS: u64 = 1_000;
const DEMO_POLL_INTERVAL_MS: u64 = 25;
const INDEX_HTML: &str = include_str!("../web/index.html");
const APP_CSS: &str = include_str!("../web/app.css");
const APP_JS: &str = include_str!("../web/app.js");
static SHUTDOWN_REQUESTED: AtomicBool = AtomicBool::new(false);

pub fn run(port: u16) -> io::Result<()> {
    install_shutdown_handlers();
    let router = Router::new(port)?;
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
    audio_cache: Arc<AudioCache>,
}

#[derive(Clone)]
enum Planner {
    Gemini,
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

struct EditJobs {
    next_id: AtomicU64,
    jobs: Mutex<BTreeMap<u64, EditJob>>,
}

struct EditJob {
    operation_id: String,
    started_at: Instant,
    finished_at: Option<Instant>,
    poll_after_ms: u64,
    applied_steps: usize,
    project_version: Option<u64>,
    state: EditJobState,
}

enum EditJobState {
    Queued,
    Running { phase: &'static str, detail: String },
    Completed { message: String },
    Failed { status: u16, error: String },
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

#[derive(Default)]
struct AudioCacheState {
    entries: Vec<(String, String)>,
    rendering: Option<String>,
}

#[derive(Default)]
struct AudioCache {
    state: Mutex<AudioCacheState>,
    completed: Condvar,
}

#[derive(Debug)]
enum AudioCacheError {
    Render(String),
}

impl AudioCache {
    fn playback_json(
        &self,
        project: &crate::model::Project,
        start: f32,
    ) -> Result<String, AudioCacheError> {
        self.playback_json_with(project, start, audio_analysis::render_project_region)
    }

    fn playback_json_with(
        &self,
        project: &crate::model::Project,
        start: f32,
        render: impl FnOnce(
            &crate::model::Project,
            f32,
        ) -> Result<(audio_analysis::AudioRegion, f32), String>,
    ) -> Result<String, AudioCacheError> {
        let graph = project.to_json();
        let cache_key = format!("{start:.3}\n{graph}");
        loop {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some((_, response)) = state
                .entries
                .iter()
                .find(|(cached_key, _)| cached_key == &cache_key)
            {
                return Ok(response.clone());
            }
            match state.rendering.as_ref() {
                Some(_) => {
                    drop(
                        self.completed
                            .wait(state)
                            .unwrap_or_else(std::sync::PoisonError::into_inner),
                    );
                }
                None => {
                    state.rendering = Some(cache_key.clone());
                    break;
                }
            }
        }

        let rendered = render(project, start).map(|(region, end)| {
            let wav = audio_analysis::wav_bytes(&region.samples);
            serde_json::json!({
                "projectVersion": project.version,
                "sampleRate": audio_analysis::SAMPLE_RATE,
                "channels": 1,
                "start": start,
                "end": end,
                "wav": base64_audio(&wav),
            })
            .to_string()
        });
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.rendering = None;
        if let Ok(response) = &rendered {
            state
                .entries
                .retain(|(cached_key, _)| cached_key != &cache_key);
            state.entries.push((cache_key, response.clone()));
            if state.entries.len() > 4 {
                state.entries.remove(0);
            }
        }
        self.completed.notify_all();
        rendered.map_err(AudioCacheError::Render)
    }
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
                applied_steps: 0,
                project_version: None,
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

    fn set_running(&self, id: u64, phase: &'static str, detail: impl Into<String>) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.state = EditJobState::Running {
                phase,
                detail: detail.into(),
            };
        }
    }

    fn publish_update(&self, id: u64, project_version: u64, summary: &str) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.applied_steps += 1;
            job.project_version = Some(project_version);
            job.state = EditJobState::Running {
                phase: "editing",
                detail: format!("Applied step {}: {summary}", job.applied_steps),
            };
        }
    }

    fn finalize_updates(&self, id: u64, project_version: u64) {
        if let Some(job) = self.lock().get_mut(&id) {
            job.project_version = Some(project_version);
            job.state = EditJobState::Running {
                phase: "finalizing",
                detail: "Gemini finished the sound graph edit".to_owned(),
            };
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

fn planner_failure(error: PlannerError) -> EditFailure {
    let status = match error {
        PlannerError::ProjectChanged => 409,
        PlannerError::SaveFailed => 500,
        _ => 503,
    };
    EditFailure::new(status, error.to_string())
}

impl Router {
    fn new(_port: u16) -> io::Result<Self> {
        let planner = match std::env::var("DAW_AI_PROMPT_ENGINE") {
            Ok(value) if value == "demo" => Planner::Demo,
            _ => Planner::Gemini,
        };
        let (store, studio) = ProjectStore::open_from_environment()?;
        Ok(Self {
            studio: Arc::new(Mutex::new(studio)),
            store: Some(store),
            planner,
            edit_jobs: Arc::new(EditJobs::new()),
            audio_cache: Arc::new(AudioCache::default()),
        })
    }

    #[cfg(test)]
    fn demo() -> Self {
        Self {
            studio: Arc::new(Mutex::new(Studio::new())),
            store: None,
            planner: Planner::Demo,
            edit_jobs: Arc::new(EditJobs::new()),
            audio_cache: Arc::new(AudioCache::default()),
        }
    }

    fn handle(&self, request: &Request) -> Response {
        let Some(public_host) = request.public_host() else {
            return Response::json(400, error_json("invalid host"));
        };
        if request.is_mutation() && !request.is_trusted_mutation(public_host) {
            return Response::json(403, error_json("cross-origin request rejected"));
        }
        if let Some(start_milliseconds) = playback_audio_start(&request.path) {
            if request.method != "GET" {
                return Response::json(405, error_json("method not allowed"))
                    .with_header("Allow", "GET");
            }
            if !request.is_trusted_audio(public_host) {
                return Response::json(403, error_json("cross-origin audio request rejected"));
            }
            return self.playback_audio(start_milliseconds);
        }
        if request.path.starts_with("/api/audio") {
            return Response::json(404, error_json("audio region not found"));
        }
        if let Some(operation_id) = edit_operation_id(&request.path) {
            return if request.method == "GET" {
                self.edit_operation_status(operation_id)
            } else {
                Response::json(405, error_json("method not allowed")).with_header("Allow", "GET")
            };
        }
        if request.path.starts_with("/api/edit-operations/") {
            return Response::json(404, error_json("edit operation not found"));
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
            ("GET", "/api/gemini-sessions") => self.gemini_sessions(),
            ("POST", "/api/edits") => self.start_edit(&request.body),
            ("POST", "/api/channels") => self.change_channel(&request.body),
            ("POST", "/api/mix") => self.change_mix(&request.body),
            ("POST", "/api/sound-tools") => self.change_sound_tool(&request.body),
            ("POST", "/api/logs") => Self::client_log(&request.body),
            ("POST", "/api/undo") => self.undo(),
            ("POST", "/api/reset") => self.reset(),
            (
                _,
                "/api/edits" | "/api/channels" | "/api/mix" | "/api/sound-tools" | "/api/logs"
                | "/api/undo" | "/api/reset",
            ) => Response::json(405, error_json("method not allowed")).with_header("Allow", "POST"),
            (_, "/api/project" | "/api/health" | "/api/gemini-sessions") => {
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
            if let Some(operation) = project
                .edit_operations
                .iter()
                .find(|operation| operation.operation_id == operation_id)
            {
                return Response::json(200, recovered_operation_json(operation));
            }
        }
        let poll_after_ms = match &self.planner {
            Planner::Gemini => GEMINI_POLL_INTERVAL_MS,
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

    fn playback_audio(&self, start_milliseconds: u64) -> Response {
        let project = self.lock_studio().project().clone();
        let chunk_milliseconds = (audio_analysis::MAX_REGION_SECONDS * 1_000.0) as u64;
        if start_milliseconds % chunk_milliseconds != 0 {
            return Response::json(
                422,
                error_json("playback start must align to an audio chunk boundary"),
            );
        }
        let start = start_milliseconds as f32 / 1_000.0;
        if !start.is_finite() || start < 0.0 || start >= project.duration {
            return Response::json(422, error_json("playback start is outside the project"));
        }
        match self.audio_cache.playback_json(&project, start) {
            Ok(body) => Response::json(200, body),
            Err(AudioCacheError::Render(error)) => {
                eprintln!("error: could not render playback audio: {error}");
                Response::json(500, error_json("could not render playback audio"))
            }
        }
    }

    fn edit_status(&self, job_id: u64) -> Response {
        self.edit_jobs
            .response(job_id)
            .unwrap_or_else(|| Response::json(404, error_json("edit job not found")))
    }

    fn edit_operation_status(&self, operation_id: &str) -> Response {
        if !valid_operation_id(operation_id) {
            return Response::json(404, error_json("edit operation not found"));
        }
        if let Some(response) = self.edit_jobs.response_for_operation(operation_id) {
            return response;
        }
        self.lock_studio()
            .project()
            .edit_operations
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .map_or_else(
                || Response::json(404, error_json("edit operation not found")),
                |operation| Response::json(200, recovered_operation_json(operation)),
            )
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
            "Gemini is planning, editing, and listening to the sound graph",
        );
        if matches!(&self.planner, Planner::Gemini) {
            return self.perform_gemini_edit(job_id, edit);
        }
        let plan = self
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
        let summary = candidate
            .apply_plan_for_operation(edit.start, edit.end, &edit.prompt, edit.operation_id, plan)
            .map_err(|error| EditFailure::new(422, studio_error_message(error)))?;
        self.commit(&mut studio, candidate)
            .map_err(|_| EditFailure::new(500, "could not save the sound graph"))?;
        Ok(summary)
    }

    fn perform_gemini_edit(&self, job_id: u64, edit: EditRequest) -> Result<String, EditFailure> {
        let mut expected_version = edit.project.version;
        let mut published_update = false;
        let completed = GeminiPlanner::interpret_with_audio_renderer_updates(
            &edit.prompt,
            edit.start,
            edit.end,
            &edit.project,
            |request| {
                self.edit_jobs.set_running(
                    job_id,
                    "rendering",
                    "Rendering the current sound graph with the Rust audio engine",
                );
                let result = render_audio_request(request);
                self.edit_jobs.set_running(
                    job_id,
                    "planning",
                    "Gemini is listening to the backend audio render",
                );
                result
            },
            |graph_edit| {
                self.commit_gemini_update(
                    job_id,
                    &edit,
                    &mut expected_version,
                    &mut published_update,
                    graph_edit,
                )
            },
        )
        .map_err(planner_failure)?;
        if !published_update {
            return Err(EditFailure::new(
                503,
                "Gemini completed without publishing a sound graph edit",
            ));
        }
        self.complete_gemini_operation(
            job_id,
            &edit,
            &mut expected_version,
            &completed.plan.summary,
        )
        .map_err(planner_failure)?;
        Ok(completed.plan.summary)
    }

    fn commit_gemini_update(
        &self,
        job_id: u64,
        edit: &EditRequest,
        expected_version: &mut u64,
        published_update: &mut bool,
        graph_edit: GeminiEdit,
    ) -> Result<(), PlannerError> {
        let summary = graph_edit.plan.summary.clone();
        let mut studio = self.lock_studio();
        if studio.project().version != *expected_version {
            return Err(PlannerError::ProjectChanged);
        }
        let mut candidate = studio.clone();
        candidate
            .replace_graph(
                graph_edit.project,
                edit.start,
                edit.end,
                &edit.prompt,
                graph_edit.plan,
            )
            .map_err(|error| PlannerError::InvalidOutput(studio_error_message(error).to_owned()))?;
        if !candidate.record_operation_step(&edit.operation_id, &summary) {
            return Err(PlannerError::InvalidOutput(
                "could not record the published edit operation".to_owned(),
            ));
        }
        self.commit(&mut studio, candidate)
            .map_err(|_| PlannerError::SaveFailed)?;
        *expected_version = studio.project().version;
        *published_update = true;
        self.edit_jobs
            .publish_update(job_id, *expected_version, &summary);
        Ok(())
    }

    fn complete_gemini_operation(
        &self,
        job_id: u64,
        edit: &EditRequest,
        expected_version: &mut u64,
        message: &str,
    ) -> Result<(), PlannerError> {
        let mut studio = self.lock_studio();
        if studio.project().version != *expected_version {
            return Err(PlannerError::ProjectChanged);
        }
        let mut candidate = studio.clone();
        if !candidate.mark_operation_complete(&edit.operation_id, message) {
            return Err(PlannerError::InvalidOutput(
                "could not mark the completed edit operation".to_owned(),
            ));
        }
        self.commit(&mut studio, candidate)
            .map_err(|_| PlannerError::SaveFailed)?;
        *expected_version = studio.project().version;
        self.edit_jobs.finalize_updates(job_id, *expected_version);
        Ok(())
    }

    fn plan_edit(
        &self,
        prompt: &str,
        start: f32,
        end: f32,
        project: &crate::model::Project,
    ) -> Result<EditPlan, String> {
        match &self.planner {
            Planner::Demo => Ok(PromptEngine::interpret_project(prompt, project, start, end)),
            Planner::Gemini => unreachable!("Gemini edits use incremental planning"),
            #[cfg(test)]
            Planner::GatedDemo(gate) => {
                gate.wait_until_released();
                Ok(PromptEngine::interpret_project(prompt, project, start, end))
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

    fn change_channel(&self, body: &str) -> Response {
        let form = parse_form(body);
        let operation_id = form.get("operation_id").map(String::as_str);
        if operation_id.is_some_and(|operation_id| !valid_operation_id(operation_id)) {
            return Response::json(422, error_json("operation ID is invalid"));
        }
        #[derive(Clone, Copy)]
        enum Change {
            Add(crate::model::TrackRole),
            Delete(u64),
        }
        let change = match form.get("action").map(String::as_str) {
            Some("add") => form
                .get("role")
                .and_then(|role| crate::model::TrackRole::from_name(role))
                .map(Change::Add),
            Some("delete") => form
                .get("track_id")
                .and_then(|track_id| track_id.parse::<u64>().ok())
                .map(Change::Delete),
            _ => None,
        };
        let Some(change) = change else {
            return Response::json(422, studio_error(StudioError::InvalidChannel));
        };
        let mut studio = self.lock_studio();
        if let Some(operation_id) = operation_id {
            if let Some(recorded) = studio
                .project()
                .channel_operations
                .iter()
                .find(|recorded| recorded.operation_id == operation_id)
            {
                let matches = match change {
                    Change::Add(role) => {
                        recorded.action == ChannelOperationAction::Add
                            && recorded.role == Some(role)
                    }
                    Change::Delete(track_id) => {
                        recorded.action == ChannelOperationAction::Delete
                            && recorded.track_id == track_id
                    }
                };
                return if matches {
                    Response::json(200, studio.to_json())
                } else {
                    Response::json(409, error_json("channel operation ID was already used"))
                };
            }
        }
        let mut candidate = studio.clone();
        let result = match change {
            Change::Add(role) => candidate
                .add_channel(role)
                .map(|track_id| (ChannelOperationAction::Add, track_id, Some(role))),
            Change::Delete(track_id) => candidate
                .delete_channel(track_id)
                .map(|()| (ChannelOperationAction::Delete, track_id, None)),
        };
        match result {
            Ok((action, track_id, role)) => {
                if operation_id.is_some_and(|operation_id| {
                    !candidate.record_channel_operation(operation_id, action, track_id, role)
                }) {
                    return Response::json(
                        409,
                        error_json("channel operation ID was already used"),
                    );
                }
                match self.commit(&mut studio, candidate) {
                    Ok(()) => Response::json(200, studio.to_json()),
                    Err(response) => response,
                }
            }
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

    fn gemini_sessions(&self) -> Response {
        match crate::gemini_tools::session_summaries() {
            Ok(sessions) => {
                Response::json(200, serde_json::json!({"sessions": sessions}).to_string())
            }
            Err(error) => {
                eprintln!("warning: could not list Gemini sessions: {error}");
                Response::json(500, error_json("could not list Gemini sessions"))
            }
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
                    | "/api/channels"
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
        self.is_trusted_request(host)
    }

    fn is_trusted_audio(&self, host: &str) -> bool {
        self.headers
            .get(AUDIO_REQUEST_HEADER)
            .is_some_and(|value| value == "1")
            && self.is_trusted_request(host)
    }

    fn is_trusted_request(&self, host: &str) -> bool {
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
                "media-src 'self' data:; object-src 'none'; frame-ancestors 'none'; base-uri 'none';\r\n",
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

fn recovered_operation_json(operation: &crate::model::EditOperation) -> String {
    let common = format!(
        concat!(
            "\"id\":\"recovered\",\"operationId\":{},\"elapsedSeconds\":0,",
            "\"timeoutSeconds\":{},\"appliedSteps\":{},\"projectVersion\":{}"
        ),
        json_string(&operation.operation_id),
        EDIT_TIMEOUT_SECONDS,
        operation.applied_steps,
        operation.project_version
    );
    if operation.completed {
        format!(
            "{{{common},\"status\":\"completed\",\"phase\":\"completed\",\"message\":{}}}",
            json_string(&operation.message)
        )
    } else {
        format!(
            concat!(
                "{{{},\"status\":\"failed\",\"phase\":\"failed\",",
                "\"errorStatus\":500,",
                "\"error\":\"Gemini stopped before completing the edit.\"}}"
            ),
            common
        )
    }
}

fn accepted_edit_job_json(id: u64, operation_id: &str, poll_after_ms: u64) -> String {
    format!(
        concat!(
            "{{\"id\":\"{}\",\"operationId\":{},\"status\":\"queued\",\"phase\":\"queued\",",
            "\"detail\":\"Waiting for the edit worker\",\"elapsedSeconds\":0,",
            "\"appliedSteps\":0,\"projectVersion\":null,",
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
    let project_version = job
        .project_version
        .map_or_else(|| "null".to_owned(), |version| version.to_string());
    let common = format!(
        concat!(
            "\"id\":\"{}\",\"operationId\":{},\"elapsedSeconds\":{},",
            "\"timeoutSeconds\":{},\"appliedSteps\":{},\"projectVersion\":{}"
        ),
        id,
        json_string(&job.operation_id),
        elapsed,
        EDIT_TIMEOUT_SECONDS,
        job.applied_steps,
        project_version
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

fn playback_audio_start(path: &str) -> Option<u64> {
    if path == "/api/audio" {
        return Some(0);
    }
    path.strip_prefix("/api/audio/")?.parse::<u64>().ok()
}

fn edit_operation_id(path: &str) -> Option<&str> {
    let operation_id = path.strip_prefix("/api/edit-operations/")?;
    valid_operation_id(operation_id).then_some(operation_id)
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
        StudioError::InvalidChannel => "invalid channel change",
        StudioError::UnknownSoundTool => "sound tool not found",
        StudioError::InvalidSoundTool => "invalid sound tool setting",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Project;
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

    fn audio_request(path: &str) -> Request {
        let mut request = request("GET", path, "");
        request
            .headers
            .insert(AUDIO_REQUEST_HEADER.to_owned(), "1".to_owned());
        request
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
                audio_cache: Arc::new(AudioCache::default()),
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
    fn gemini_sessions_are_always_persistent_and_listed_for_debugging() {
        let router = Router::demo();
        let response = router.handle(&request("GET", "/api/gemini-sessions", ""));
        assert_eq!(response.status, 200);
        let body: serde_json::Value =
            serde_json::from_str(&response.body).expect("Gemini session list JSON");
        assert!(body["sessions"].is_array());
        assert_eq!(
            router
                .handle(&request("POST", "/api/gemini-sessions", ""))
                .status,
            405
        );
    }

    #[test]
    fn edit_job_status_reports_phase_progress_and_failures() {
        let jobs = EditJobs::new();
        let (id, operation_id, created) = jobs.create(750, None).expect("edit job");
        assert!(created);
        jobs.set_running(id, "planning", "Gemini is arranging the requested change");
        let running: serde_json::Value =
            serde_json::from_str(&jobs.response(id).expect("running job response").body)
                .expect("running job JSON");
        assert_eq!(running["status"], "running");
        assert_eq!(running["phase"], "planning");
        assert_eq!(
            running["detail"],
            "Gemini is arranging the requested change"
        );
        assert_eq!(running["pollAfterMs"], 750);
        assert_eq!(running["operationId"], operation_id);
        assert_eq!(running["appliedSteps"], 0);
        assert!(running["projectVersion"].is_null());

        jobs.publish_update(id, 7, "Added a bass layer");
        let updated: serde_json::Value =
            serde_json::from_str(&jobs.response(id).expect("updated job response").body)
                .expect("updated job JSON");
        assert_eq!(updated["phase"], "editing");
        assert_eq!(updated["detail"], "Applied step 1: Added a bass layer");
        assert_eq!(updated["appliedSteps"], 1);
        assert_eq!(updated["projectVersion"], 7);

        jobs.fail(id, 503, "Gemini timed out".to_owned());
        let failed: serde_json::Value =
            serde_json::from_str(&jobs.response(id).expect("failed job response").body)
                .expect("failed job JSON");
        assert_eq!(failed["status"], "failed");
        assert_eq!(failed["errorStatus"], 503);
        assert_eq!(failed["error"], "Gemini timed out");
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
    fn channel_api_adds_and_deletes_complete_tracks() {
        let router = Router::demo();
        let added = router.handle(&request(
            "POST",
            "/api/channels",
            "operation_id=add-lead&action=add&role=lead",
        ));
        assert_eq!(added.status, 200);
        let added: serde_json::Value = serde_json::from_str(&added.body).expect("added project");
        let tracks = added["tracks"].as_array().expect("tracks");
        assert_eq!(tracks.len(), 4);
        let lead = tracks.last().expect("lead track");
        assert_eq!(lead["role"], "lead");
        assert_eq!(lead["clips"][0]["start"], 0.0);
        assert_eq!(lead["clips"][0]["end"], added["duration"]);
        let track_id = lead["id"].as_u64().expect("lead track ID");
        assert_eq!(added["channelOperations"][0]["operationId"], "add-lead");
        assert_eq!(added["channelOperations"][0]["trackId"], track_id);
        let duplicate = router.handle(&request(
            "POST",
            "/api/channels",
            "operation_id=add-lead&action=add&role=lead",
        ));
        assert_eq!(duplicate.status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&duplicate.body).expect("duplicate add")
                ["tracks"]
                .as_array()
                .expect("tracks")
                .len(),
            4
        );
        assert_eq!(
            router
                .handle(&request(
                    "POST",
                    "/api/channels",
                    "operation_id=add-lead&action=add&role=texture",
                ))
                .status,
            409
        );

        let deleted = router.handle(&request(
            "POST",
            "/api/channels",
            &format!("operation_id=delete-lead&action=delete&track_id={track_id}"),
        ));
        assert_eq!(deleted.status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&deleted.body)
                .expect("deleted project")["tracks"]
                .as_array()
                .expect("tracks")
                .len(),
            3
        );
        assert_eq!(
            router
                .handle(&request("POST", "/api/channels", "action=add&role=unknown"))
                .status,
            422
        );
    }

    #[test]
    fn gemini_updates_commit_as_incremental_recoverable_steps() {
        let router = Router::demo();
        let (job_id, operation_id, _) = router.edit_jobs.create(750, None).expect("edit job");
        let project = router.lock_studio().project().clone();
        let edit = EditRequest {
            operation_id: operation_id.clone(),
            prompt: "shape the bass in two steps".to_owned(),
            start: 4.0,
            end: 8.0,
            project: project.clone(),
        };
        let plan = |waveform: &str, summary: &str| EditPlan {
            action: crate::prompt::Action::Configure {
                track_id: 2,
                target: crate::model::TrackRole::Bass,
                tool: "instrument",
                tool_id: 201,
                clip_id: None,
                parameter: "waveform",
                value: waveform.to_owned(),
            },
            summary: summary.to_owned(),
        };
        let mut session = Studio::from_project(project);
        let first_plan = plan("sawtooth", "Brightened the bass");
        session
            .apply_plan(4.0, 8.0, &edit.prompt, first_plan.clone())
            .expect("first session step");
        let mut expected_version = edit.project.version;
        let mut published_update = false;
        router
            .commit_gemini_update(
                job_id,
                &edit,
                &mut expected_version,
                &mut published_update,
                GeminiEdit {
                    plan: first_plan,
                    project: session.project().clone(),
                },
            )
            .expect("first live step");
        assert!(
            router
                .lock_studio()
                .project()
                .edits
                .iter()
                .all(|edit| edit.operation_id.is_none())
        );

        let second_plan = plan("triangle", "Softened the bass");
        session
            .apply_plan(4.0, 8.0, &edit.prompt, second_plan.clone())
            .expect("second session step");
        router
            .commit_gemini_update(
                job_id,
                &edit,
                &mut expected_version,
                &mut published_update,
                GeminiEdit {
                    plan: second_plan,
                    project: session.project().clone(),
                },
            )
            .expect("second live step");
        router
            .complete_gemini_operation(
                job_id,
                &edit,
                &mut expected_version,
                "Finished shaping the bass",
            )
            .expect("terminal operation marker");

        let studio = router.lock_studio();
        assert_eq!(studio.project().version, 4);
        assert_eq!(studio.project().tracks[1].instrument.waveform, "triangle");
        assert_eq!(studio.project().edits.len(), 2);
        assert!(
            studio
                .project()
                .edits
                .iter()
                .all(|edit| edit.operation_id.is_none())
        );
        let operation = &studio.project().edit_operations[0];
        assert_eq!(operation.operation_id, operation_id);
        assert!(operation.completed);
        assert_eq!(operation.applied_steps, 2);
        assert_eq!(operation.message, "Finished shaping the bass");
        crate::model::Project::from_json(&studio.project().to_json())
            .expect("persistable incremental graph");
        drop(studio);

        let status: serde_json::Value = serde_json::from_str(
            &router
                .edit_jobs
                .response(job_id)
                .expect("incremental job status")
                .body,
        )
        .expect("incremental job JSON");
        assert_eq!(status["appliedSteps"], 2);
        assert_eq!(status["projectVersion"], 4);
        assert_eq!(status["phase"], "finalizing");

        router.edit_jobs.remove(job_id);
        let recovered = router.handle(&request(
            "POST",
            "/api/edits",
            &format!(
                "operation_id={operation_id}&start=4&end=8&prompt=shape+the+bass+in+two+steps"
            ),
        ));
        assert_eq!(recovered.status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recovered.body)
                .expect("recovered terminal operation")["status"],
            "completed"
        );
    }

    #[test]
    fn partial_gemini_update_is_not_recovered_as_a_completed_operation() {
        let router = Router::demo();
        let (job_id, operation_id, _) = router.edit_jobs.create(750, None).expect("edit job");
        let project = router.lock_studio().project().clone();
        let edit = EditRequest {
            operation_id: operation_id.clone(),
            prompt: "shape the bass in two steps".to_owned(),
            start: 4.0,
            end: 8.0,
            project: project.clone(),
        };
        let first_plan = EditPlan {
            action: crate::prompt::Action::Configure {
                track_id: 2,
                target: crate::model::TrackRole::Bass,
                tool: "instrument",
                tool_id: 201,
                clip_id: None,
                parameter: "waveform",
                value: "sawtooth".to_owned(),
            },
            summary: "Brightened the bass".to_owned(),
        };
        let mut session = Studio::from_project(project);
        session
            .apply_plan(4.0, 8.0, &edit.prompt, first_plan.clone())
            .expect("first of two session steps");
        let mut expected_version = edit.project.version;
        let mut published_update = false;
        router
            .commit_gemini_update(
                job_id,
                &edit,
                &mut expected_version,
                &mut published_update,
                GeminiEdit {
                    plan: first_plan,
                    project: session.project().clone(),
                },
            )
            .expect("first live step");
        assert!(
            router
                .lock_studio()
                .project()
                .edits
                .iter()
                .all(|edit| edit.operation_id.as_deref() != Some(operation_id.as_str()))
        );

        router.edit_jobs.remove(job_id);
        let persisted = crate::model::Project::from_json(&router.lock_studio().project().to_json())
            .expect("persisted partial operation");
        let restarted = Router {
            studio: Arc::new(Mutex::new(Studio::from_project(persisted))),
            store: None,
            planner: Planner::Demo,
            edit_jobs: Arc::new(EditJobs::new()),
            audio_cache: Arc::new(AudioCache::default()),
        };
        let retried = restarted.handle(&request(
            "POST",
            "/api/edits",
            &format!(
                "operation_id={operation_id}&start=4&end=8&prompt=shape+the+bass+in+two+steps"
            ),
        ));
        assert_eq!(retried.status, 200);
        let retried_json: serde_json::Value =
            serde_json::from_str(&retried.body).expect("retried edit job");
        assert_eq!(retried_json["status"], "failed");
        assert_eq!(retried_json["operationId"], operation_id);
        assert_eq!(retried_json["appliedSteps"], 1);
        assert_eq!(retried_json["projectVersion"], expected_version);
        let recovered = restarted.handle(&request(
            "GET",
            &format!("/api/edit-operations/{operation_id}"),
            "",
        ));
        assert_eq!(recovered.status, 200);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&recovered.body)
                .expect("recovered partial operation")["status"],
            "failed"
        );
    }

    #[test]
    fn gemini_completion_is_independent_of_edit_log_compaction() {
        let router = Router::demo();
        let mut populated = Studio::new();
        let regional_plan = || EditPlan {
            action: crate::prompt::Action::Gain {
                amount: 1.0,
                target: Some(crate::model::TrackRole::Bass),
            },
            summary: "Retained regional state".to_owned(),
        };
        for _ in 1..crate::model::EDIT_LOG_LIMIT {
            populated
                .apply_plan(4.0, 8.0, "retain regional state", regional_plan())
                .expect("regional edit");
        }
        populated
            .apply_plan_for_operation(
                4.0,
                8.0,
                "retain prior operation identity",
                "previous-operation".to_owned(),
                regional_plan(),
            )
            .expect("prior operation");
        let project = populated.project().clone();
        *router.lock_studio() = Studio::from_project(project.clone());

        let (job_id, operation_id, _) = router.edit_jobs.create(750, None).expect("edit job");
        let edit = EditRequest {
            operation_id: operation_id.clone(),
            prompt: "change the bass waveform".to_owned(),
            start: 4.0,
            end: 8.0,
            project: project.clone(),
        };
        let plan = EditPlan {
            action: crate::prompt::Action::Configure {
                track_id: 2,
                target: crate::model::TrackRole::Bass,
                tool: "instrument",
                tool_id: 201,
                clip_id: None,
                parameter: "waveform",
                value: "sawtooth".to_owned(),
            },
            summary: "Changed the bass waveform".to_owned(),
        };
        let mut session = Studio::from_project(project);
        session
            .apply_plan(4.0, 8.0, &edit.prompt, plan.clone())
            .expect("nonregional Gemini edit");
        let mut expected_version = edit.project.version;
        let mut published_update = false;
        router
            .commit_gemini_update(
                job_id,
                &edit,
                &mut expected_version,
                &mut published_update,
                GeminiEdit {
                    plan,
                    project: session.project().clone(),
                },
            )
            .expect("compacted Gemini update");
        router
            .complete_gemini_operation(
                job_id,
                &edit,
                &mut expected_version,
                "Changed the bass waveform",
            )
            .expect("independent completion record");

        let studio = router.lock_studio();
        assert_eq!(studio.project().edits.len(), crate::model::EDIT_LOG_LIMIT);
        assert_eq!(
            studio
                .project()
                .edits
                .last()
                .and_then(|edit| edit.operation_id.as_deref()),
            Some("previous-operation")
        );
        let completed = studio
            .project()
            .edit_operations
            .iter()
            .find(|operation| operation.operation_id == operation_id)
            .expect("Gemini operation record");
        assert!(completed.completed);
        assert_eq!(completed.message, "Changed the bass waveform");
        assert!(studio.project().edit_operations.iter().any(|operation| {
            operation.operation_id == "previous-operation" && operation.completed
        }));
    }

    #[test]
    fn accepts_bounded_client_error_and_warning_logs() {
        let router = Router::demo();
        let error = router.handle(&request(
            "POST",
            "/api/logs",
            "level=error&context=starting+audio&message=Media+playback+failed",
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
        let response = router.handle(&request(
            "POST",
            "/api/sound-tools",
            "track_id=2&tool=modulator&tool_id=250&parameter=target&value=instrument.oscillator2.level",
        ));
        assert_eq!(response.status, 200);
        let saved = ProjectStore::open(path.clone()).expect("saved project").1;
        assert_eq!(saved.project().tracks[1].instrument.waveform, "sawtooth");
        assert_eq!(
            saved.project().tracks[1].modulators[0].target,
            "instrument.oscillator2.level"
        );
        std::fs::remove_file(path).expect("remove test graph");
    }

    #[test]
    fn deleting_an_automated_channel_persists_without_its_envelope() {
        let (router, path) = persisted_demo();
        let bass_id = router.lock_studio().project().tracks[1].id;
        {
            let mut studio = router.lock_studio();
            let mut candidate = studio.clone();
            candidate
                .apply_plan(
                    0.0,
                    4.0,
                    "automate bass volume",
                    crate::prompt::EditPlan {
                        summary: "Automated bass volume".to_owned(),
                        action: crate::prompt::Action::Automation {
                            track_id: bass_id,
                            parameter: "track.volume".to_owned(),
                            curve: "linear",
                            points: vec![
                                crate::prompt::AutomationPoint {
                                    time: 0.0,
                                    value: 0.2,
                                },
                                crate::prompt::AutomationPoint {
                                    time: 1.0,
                                    value: 1.2,
                                },
                            ],
                            target: crate::model::TrackRole::Bass,
                        },
                    },
                )
                .expect("valid automation");
            assert!(router.commit(&mut studio, candidate).is_ok());
        }

        let deleted = router.handle(&request(
            "POST",
            "/api/channels",
            &format!("action=delete&track_id={bass_id}"),
        ));
        assert_eq!(deleted.status, 200, "{}", deleted.body);
        let saved = ProjectStore::open(path.clone()).expect("saved project").1;
        assert!(
            saved
                .project()
                .tracks
                .iter()
                .all(|track| track.id != bass_id)
        );
        assert!(
            !saved
                .project()
                .to_json()
                .contains("\"type\":\"automation\"")
        );
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
    fn serves_audio_rendered_by_the_backend_engine() {
        let router = Router::demo();
        let response = router.handle(&audio_request("/api/audio"));
        assert_eq!(response.status, 200);
        let audio: serde_json::Value =
            serde_json::from_str(&response.body).expect("playback audio JSON");
        assert_eq!(audio["projectVersion"], 1);
        assert_eq!(audio["sampleRate"], audio_analysis::SAMPLE_RATE);
        assert_eq!(audio["channels"], 1);
        assert_eq!(audio["start"], 0.0);
        assert_eq!(audio["end"], audio_analysis::MAX_REGION_SECONDS);
        let wav = audio["wav"].as_str().expect("base64 WAV");
        assert!(wav.starts_with("UklGR"));
        assert!(wav.len() > 1_000);
        assert_eq!(
            router.handle(&request("POST", "/api/audio", "")).status,
            405
        );
        let later = router.handle(&audio_request("/api/audio/16000"));
        assert_eq!(later.status, 200);
        let later: serde_json::Value =
            serde_json::from_str(&later.body).expect("later playback audio JSON");
        assert_eq!(later["start"], 16.0);
        assert_eq!(
            router.handle(&audio_request("/api/audio/15500")).status,
            422
        );
    }

    #[test]
    fn rejects_untrusted_audio_requests() {
        let router = Router::demo();
        assert_eq!(router.handle(&request("GET", "/api/audio", "")).status, 403);

        let mut hostile = audio_request("/api/audio");
        hostile
            .headers
            .insert("origin".to_owned(), "http://127.0.0.1:18867".to_owned());
        hostile
            .headers
            .insert("sec-fetch-site".to_owned(), "cross-site".to_owned());
        assert_eq!(router.handle(&hostile).status, 403);
    }

    #[test]
    fn coalesces_matching_audio_renders_and_serializes_competing_work() {
        let cache = Arc::new(AudioCache::default());
        let project = Project::demo();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        let started = Arc::new((Mutex::new(false), Condvar::new()));

        let first_cache = Arc::clone(&cache);
        let first_project = project.clone();
        let first_gate = Arc::clone(&gate);
        let first_started = Arc::clone(&started);
        let first = thread::spawn(move || {
            first_cache.playback_json_with(&first_project, 0.0, |project, _| {
                let (lock, ready) = &*first_started;
                *lock
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
                ready.notify_all();

                let (lock, released) = &*first_gate;
                drop(
                    released
                        .wait_while(
                            lock.lock()
                                .unwrap_or_else(std::sync::PoisonError::into_inner),
                            |released| !*released,
                        )
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                );
                audio_analysis::render_region(project, &[1], 0.0, 0.01).map(|region| (region, 0.01))
            })
        });

        let (lock, ready) = &*started;
        drop(
            ready
                .wait_while(
                    lock.lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner),
                    |started| !*started,
                )
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );

        let second_cache = Arc::clone(&cache);
        let second_project = project.clone();
        let second = thread::spawn(move || {
            second_cache.playback_json_with(&second_project, 0.0, |_, _| {
                panic!("coalesced render ran twice")
            })
        });
        let competing_cache = Arc::clone(&cache);
        let competing_project = project.clone();
        let competing = thread::spawn(move || {
            competing_cache.playback_json_with(&competing_project, 16.0, |project, _| {
                audio_analysis::render_region(project, &[1], 0.0, 0.01)
                    .map(|region| (region, 16.01))
            })
        });

        let (lock, released) = &*gate;
        *lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        released.notify_all();
        let first = first
            .join()
            .expect("first render thread")
            .expect("first render");
        let second = second
            .join()
            .expect("coalesced render thread")
            .expect("coalesced render");
        let competing = competing
            .join()
            .expect("competing render thread")
            .expect("competing render");
        assert_eq!(first, second);
        assert_ne!(first, competing);
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
        let project: serde_json::Value = serde_json::from_str(&project.body).expect("project JSON");
        assert_eq!(project["tracks"][1]["instrument"]["waveform"], "square");

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
