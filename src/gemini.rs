use std::fmt;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{Map, Value as JsonValue};

#[cfg(test)]
use crate::gemini_tools::render_audio_request;
use crate::gemini_tools::{
    APPLY_TOOL_NAME, AUDIO_TOOL_NAME, AudioRender, AudioRenderRequest, EditSession, READ_TOOL_NAME,
    apply_sound_graph_edits, base64_audio, prepare_audio_render, read_sound_graph,
    tool_declarations,
};
use crate::model::{Project, TrackRole};
use crate::prompt::{Action, AutomationPoint, EditPlan, MAX_COMPOUND_ACTIONS, MidiNote};

pub(crate) const EDIT_SCHEMA: &str = include_str!("../gemini/edit-plan.schema.json");
const STUDIO_CONTRACT: &str = include_str!("../gemini/STUDIO.md");
const GEMINI_MODEL: &str = "gemini-3.5-flash";
const DEFAULT_INTERACTIONS_ENDPOINT: &str =
    "https://generativelanguage.googleapis.com/v1/interactions";
const SYSTEMD_CREDENTIAL_NAME: &str = "gemini-api-key";
const JUDGE_TOOL_NAME: &str = "report_audio_verdict";
pub(crate) const EDIT_TIMEOUT_SECONDS: u64 = 20 * 60;
const EDIT_TIMEOUT: Duration = Duration::from_secs(EDIT_TIMEOUT_SECONDS);
type Object = Map<String, JsonValue>;

#[derive(Debug)]
pub enum PlannerError {
    Unavailable(String),
    TimedOut,
    Failed(String),
    ProjectChanged,
    SaveFailed,
    InvalidOutput(String),
    Io(std::io::Error),
}

impl fmt::Display for PlannerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable(message) => write!(formatter, "{message}"),
            Self::TimedOut => write!(
                formatter,
                "Gemini took too long to complete the edit; try again"
            ),
            Self::Failed(message) => {
                write!(formatter, "Gemini could not complete the edit: {message}")
            }
            Self::ProjectChanged => write!(formatter, "the project changed; submit the edit again"),
            Self::SaveFailed => write!(formatter, "could not save the sound graph"),
            Self::InvalidOutput(message) => {
                write!(
                    formatter,
                    "Gemini returned an invalid synth edit: {message}"
                )
            }
            Self::Io(error) => write!(formatter, "Gemini integration failed: {error}"),
        }
    }
}

pub struct GeminiPlanner;

pub struct GeminiEdit {
    pub plan: EditPlan,
    pub project: Project,
}

#[derive(Default)]
struct LoopState {
    plans: Vec<EditPlan>,
    initial_audio_heard: bool,
    listened_since_edit: bool,
    audio_listens: usize,
    judge_reviews: usize,
    judge_rejections: usize,
    last_judged_plan_count: Option<usize>,
    last_judge_feedback: Option<String>,
    latest_audio: Option<LatestAudio>,
}

struct LatestAudio {
    description: String,
    wav: Vec<u8>,
}

struct JudgeVerdict {
    accepted: bool,
    summary: String,
    audible_evidence: String,
    feedback: Vec<String>,
}

impl GeminiPlanner {
    pub(crate) fn interpret_with_audio_renderer_updates(
        prompt: &str,
        start: f32,
        end: f32,
        project: &Project,
        mut render_audio: impl FnMut(AudioRenderRequest) -> Result<AudioRender, String>,
        mut on_update: impl FnMut(GeminiEdit) -> Result<(), PlannerError>,
    ) -> Result<GeminiEdit, PlannerError> {
        let session = EditSession::create(project, prompt, start, end).map_err(PlannerError::Io)?;
        let result = run_session(
            &session,
            prompt,
            start,
            end,
            &mut render_audio,
            &mut on_update,
        );
        let (status, detail) = match &result {
            Ok(edit) => ("completed", edit.plan.summary.clone()),
            Err(error) => ("failed", error.to_string()),
        };
        let (applied_steps, audio_listens, judge_reviews, judge_rejections) =
            session.stats().unwrap_or((0, 0, 0, 0));
        // Keep the model/API transcript even if this final metadata update cannot be written.
        if let Err(error) = session.update_status(
            status,
            &detail,
            applied_steps,
            audio_listens,
            judge_reviews,
            judge_rejections,
        ) {
            eprintln!("warning: could not finalize Gemini session metadata: {error}");
        }
        result
    }
}

fn run_session(
    session: &EditSession,
    prompt: &str,
    start: f32,
    end: f32,
    render_audio: &mut impl FnMut(AudioRenderRequest) -> Result<AudioRender, String>,
    on_update: &mut impl FnMut(GeminiEdit) -> Result<(), PlannerError>,
) -> Result<GeminiEdit, PlannerError> {
    let api_key = load_api_key()?;
    let endpoint = std::env::var("DAW_AI_GEMINI_ENDPOINT")
        .unwrap_or_else(|_| DEFAULT_INTERACTIONS_ENDPOINT.to_owned());
    run_session_with_transport(
        session,
        prompt,
        start,
        end,
        render_audio,
        on_update,
        (
            &mut |sequence, request, remaining| {
                call_interactions(
                    session,
                    &format!("interaction-{sequence:03}"),
                    request,
                    &api_key,
                    &endpoint,
                    remaining,
                )
            },
            &mut |sequence, request, remaining| {
                call_interactions(
                    session,
                    &format!("judge-{sequence:03}"),
                    request,
                    &api_key,
                    &endpoint,
                    remaining,
                )
            },
        ),
    )
}

fn run_session_with_transport(
    session: &EditSession,
    prompt: &str,
    start: f32,
    end: f32,
    render_audio: &mut impl FnMut(AudioRenderRequest) -> Result<AudioRender, String>,
    on_update: &mut impl FnMut(GeminiEdit) -> Result<(), PlannerError>,
    transports: (
        &mut impl FnMut(usize, &JsonValue, Duration) -> Result<String, PlannerError>,
        &mut impl FnMut(usize, &JsonValue, Duration) -> Result<String, PlannerError>,
    ),
) -> Result<GeminiEdit, PlannerError> {
    let (transport, judge_transport) = transports;
    let started = Instant::now();
    let tools = {
        let mut tools = tool_declarations();
        tools.push(serde_json::json!({
            "type": "google_search",
            "search_types": ["web_search"]
        }));
        tools
    };
    let mut input = JsonValue::String(planner_task(prompt, start, end));
    let mut previous_interaction_id: Option<String> = None;
    let mut sequence = 0_usize;
    let mut state = LoopState::default();

    loop {
        let remaining = EDIT_TIMEOUT
            .checked_sub(started.elapsed())
            .ok_or(PlannerError::TimedOut)?;
        sequence += 1;
        let mut request = serde_json::json!({
            "model": GEMINI_MODEL,
            "input": input,
            "tools": tools,
            "system_instruction": system_instruction(),
            "generation_config": {"thinking_level": "high"},
            "store": true
        });
        if let Some(previous) = &previous_interaction_id {
            request
                .as_object_mut()
                .expect("interaction request object")
                .insert(
                    "previous_interaction_id".to_owned(),
                    JsonValue::String(previous.clone()),
                );
        }
        let response_source = transport(sequence, &request, remaining)?;
        let response = serde_json::from_str::<JsonValue>(&response_source)
            .map_err(|error| invalid(&format!("interaction response was not JSON: {error}")))?;
        previous_interaction_id = Some(
            response
                .get("id")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| invalid("interaction response omitted its ID"))?
                .to_owned(),
        );
        let calls = function_calls(&response)?;
        if calls.is_empty() {
            if state.plans.is_empty() {
                input = JsonValue::String(format!(
                    "You have not made an edit. Call {READ_TOOL_NAME}, listen to the original audio with {AUDIO_TOOL_NAME}, then apply a concrete graph change with {APPLY_TOOL_NAME}. You choose the render start and end in absolute project seconds; include context outside the edit selection when useful."
                ));
                continue;
            }
            if !state.listened_since_edit {
                input = JsonValue::String(format!(
                    "The last edit has not been heard. Call {AUDIO_TOOL_NAME} with the channels and absolute project range that best reveal the result, evaluate the audio against the request, and continue editing if it is weak."
                ));
                continue;
            }
            if state.last_judged_plan_count == Some(state.plans.len()) {
                input = JsonValue::String(format!(
                    "The independent audio judge already rejected this exact graph. Its feedback was:\n\n{}\n\nApply at least one concrete graph edit with {APPLY_TOOL_NAME}, then render and listen again before claiming completion.",
                    state
                        .last_judge_feedback
                        .as_deref()
                        .unwrap_or("The audible result did not yet fulfill the request.")
                ));
                continue;
            }
            let latest_audio = state
                .latest_audio
                .as_ref()
                .ok_or_else(|| invalid("completion was claimed without a retained audio render"))?;
            session
                .update_status(
                    "judging",
                    "An independent Gemini interaction is judging the latest audio",
                    applied_steps(&state),
                    state.audio_listens,
                    state.judge_reviews,
                    state.judge_rejections,
                )
                .map_err(PlannerError::Io)?;
            let judge_sequence = state.judge_reviews + 1;
            let judge_remaining = EDIT_TIMEOUT
                .checked_sub(started.elapsed())
                .ok_or(PlannerError::TimedOut)?;
            let verdict = judge_completion(
                prompt,
                start,
                end,
                latest_audio,
                judge_sequence,
                judge_remaining,
                judge_transport,
            )?;
            state.judge_reviews += 1;
            state.last_judged_plan_count = Some(state.plans.len());
            if !verdict.accepted {
                state.judge_rejections += 1;
                let feedback = verdict.producer_feedback();
                state.last_judge_feedback = Some(feedback.clone());
                session
                    .update_status(
                        "running",
                        &format!(
                            "Independent audio judge rejected completion: {}",
                            verdict.summary
                        ),
                        applied_steps(&state),
                        state.audio_listens,
                        state.judge_reviews,
                        state.judge_rejections,
                    )
                    .map_err(PlannerError::Io)?;
                input = JsonValue::String(format!(
                    "The independent audio judge rejected your completion claim. This verdict came from a fresh interaction that heard the latest WAV without your transcript.\n\n{feedback}\n\nUse this as required revision guidance. Apply a concrete graph edit, render the most informative channels and absolute project range, listen, and only then claim completion again."
                ));
                continue;
            }
            session
                .update_status(
                    "running",
                    &format!(
                        "Independent audio judge accepted the result: {}",
                        verdict.summary
                    ),
                    applied_steps(&state),
                    state.audio_listens,
                    state.judge_reviews,
                    state.judge_rejections,
                )
                .map_err(PlannerError::Io)?;
            let (plan, project) = session
                .finish(state.plans)
                .map_err(|message| invalid(&message))?;
            return Ok(GeminiEdit { plan, project });
        }

        let mut results = Vec::with_capacity(calls.len() * 2);
        for call in calls {
            let output = execute_tool(
                session,
                sequence,
                &call,
                &mut state,
                render_audio,
                on_update,
            )?;
            results.push(serde_json::json!({
                "type": "function_result",
                "name": call.name,
                "call_id": call.id,
                "result": output.result
            }));
            results.extend(output.supplemental_input);
        }
        session
            .update_status(
                "running",
                "Gemini is editing and listening",
                applied_steps(&state),
                state.audio_listens,
                state.judge_reviews,
                state.judge_rejections,
            )
            .map_err(PlannerError::Io)?;
        input = JsonValue::Array(results);
    }
}

struct FunctionCall {
    id: String,
    name: String,
    arguments: JsonValue,
}

struct ToolOutput {
    result: Vec<JsonValue>,
    supplemental_input: Vec<JsonValue>,
}

impl ToolOutput {
    fn text(message: String) -> Self {
        Self {
            result: vec![serde_json::json!({"type": "text", "text": message})],
            supplemental_input: Vec::new(),
        }
    }
}

fn function_calls(response: &JsonValue) -> Result<Vec<FunctionCall>, PlannerError> {
    if let Some(error) = response.get("error") {
        return Err(PlannerError::Failed(api_error_message(error)));
    }
    let status = response
        .get("status")
        .and_then(JsonValue::as_str)
        .unwrap_or("completed");
    if matches!(
        status,
        "failed" | "cancelled" | "incomplete" | "budget_exceeded"
    ) {
        return Err(PlannerError::Failed(format!(
            "interaction ended with status {status}"
        )));
    }
    let steps = response
        .get("steps")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| invalid("interaction response omitted its steps"))?;
    steps
        .iter()
        .filter(|step| step.get("type").and_then(JsonValue::as_str) == Some("function_call"))
        .map(|step| {
            Ok(FunctionCall {
                id: step
                    .get("id")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| invalid("function call omitted its ID"))?
                    .to_owned(),
                name: step
                    .get("name")
                    .and_then(JsonValue::as_str)
                    .ok_or_else(|| invalid("function call omitted its name"))?
                    .to_owned(),
                arguments: step
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| serde_json::json!({})),
            })
        })
        .collect()
}

fn judge_completion(
    prompt: &str,
    start: f32,
    end: f32,
    audio: &LatestAudio,
    sequence: usize,
    remaining: Duration,
    transport: &mut impl FnMut(usize, &JsonValue, Duration) -> Result<String, PlannerError>,
) -> Result<JudgeVerdict, PlannerError> {
    let request = judge_request(prompt, start, end, audio);
    let response_source = transport(sequence, &request, remaining)?;
    let response = serde_json::from_str::<JsonValue>(&response_source)
        .map_err(|error| invalid(&format!("judge response was not JSON: {error}")))?;
    let calls = function_calls(&response)?;
    if calls.len() != 1 || calls[0].name != JUDGE_TOOL_NAME {
        return Err(invalid(&format!(
            "independent audio judge must call {JUDGE_TOOL_NAME} exactly once"
        )));
    }
    JudgeVerdict::from_arguments(&calls[0].arguments)
}

fn judge_request(prompt: &str, start: f32, end: f32, audio: &LatestAudio) -> JsonValue {
    serde_json::json!({
        "model": GEMINI_MODEL,
        "input": [{
            "type": "user_input",
            "content": [
                {
                    "type": "text",
                    "text": format!(
                        "User request: {prompt}\nSelected edit scope: {start:.3} to {end:.3} project seconds.\nLatest evidence render: {}\n\nDecide whether the audio itself clearly fulfills the request. The producer chose this listening window; reject completion if it omits context needed to prove a transition, contrast, or other requested musical effect.",
                        audio.description
                    )
                },
                {
                    "type": "audio",
                    "mime_type": "audio/wav",
                    "data": base64_audio(&audio.wav)
                }
            ]
        }],
        "tools": [judge_tool_declaration()],
        "system_instruction": judge_system_instruction(),
        "generation_config": {
            "thinking_level": "high",
            "tool_choice": {
                "allowed_tools": {
                    "mode": "any",
                    "tools": [JUDGE_TOOL_NAME]
                }
            }
        },
        "store": false
    })
}

fn judge_tool_declaration() -> JsonValue {
    serde_json::json!({
        "type": "function",
        "name": JUDGE_TOOL_NAME,
        "description": "Return the independent final verdict after listening to the supplied audio.",
        "parameters": {
            "type": "object",
            "additionalProperties": false,
            "required": ["accepted", "summary", "audibleEvidence", "feedback"],
            "properties": {
                "accepted": {
                    "type": "boolean",
                    "description": "True only when the supplied audio clearly fulfills the user's musical request."
                },
                "summary": {
                    "type": "string",
                    "description": "A concise verdict explaining the decision."
                },
                "audibleEvidence": {
                    "type": "string",
                    "description": "Specific features actually heard in the audio that support the verdict."
                },
                "feedback": {
                    "type": "array",
                    "description": "If rejected, detailed actionable musical corrections for the producer; empty only when accepted.",
                    "items": {"type": "string"},
                    "maxItems": 8
                }
            }
        }
    })
}

fn judge_system_instruction() -> &'static str {
    concat!(
        "You are DAW-AI's independent final audio judge. You are a fresh interaction and must not ",
        "assume the producer's edits worked. Judge from the supplied WAV, the user request, and the ",
        "render/selection boundaries only. Be strict: accept only when the requested musical identity ",
        "or transformation is plainly audible, not merely technically plausible. Evaluate arrangement ",
        "and transition, pulse and subdivision, groove, dynamics and contrast, timbre, channel balance, ",
        "and stereo presentation where each is relevant. If the evidence render is too narrow or omits ",
        "necessary channels or before/after context, reject it. On rejection, identify concrete audible ",
        "shortcomings and give detailed, prioritized musical corrections. Always call the verdict tool."
    )
}

impl JudgeVerdict {
    fn from_arguments(arguments: &JsonValue) -> Result<Self, PlannerError> {
        let object = arguments
            .as_object()
            .ok_or_else(|| invalid("judge verdict arguments must be an object"))?;
        let accepted = object
            .get("accepted")
            .and_then(JsonValue::as_bool)
            .ok_or_else(|| invalid("judge verdict accepted must be a boolean"))?;
        let summary = judge_text(object, "summary", 600)?;
        let audible_evidence = judge_text(object, "audibleEvidence", 1_200)?;
        let values = object
            .get("feedback")
            .and_then(JsonValue::as_array)
            .ok_or_else(|| invalid("judge verdict feedback must be an array"))?;
        if values.len() > 8 {
            return Err(invalid("judge verdict feedback has more than eight items"));
        }
        let feedback = values
            .iter()
            .map(|value| {
                let text = value
                    .as_str()
                    .ok_or_else(|| invalid("judge feedback items must be strings"))?;
                let text = bounded_text(text, 1_000);
                if text.is_empty() {
                    return Err(invalid("judge feedback items cannot be empty"));
                }
                Ok(text)
            })
            .collect::<Result<Vec<_>, _>>()?;
        if !accepted && feedback.is_empty() {
            return Err(invalid(
                "a rejected judge verdict must include detailed feedback",
            ));
        }
        Ok(Self {
            accepted,
            summary,
            audible_evidence,
            feedback,
        })
    }

    fn producer_feedback(&self) -> String {
        let corrections = self
            .feedback
            .iter()
            .enumerate()
            .map(|(index, feedback)| format!("{}. {feedback}", index + 1))
            .collect::<Vec<_>>()
            .join("\n");
        format!(
            "Verdict: {}\nAudible evidence: {}\nRequired corrections:\n{}",
            self.summary, self.audible_evidence, corrections
        )
    }
}

fn judge_text(object: &Object, name: &str, maximum: usize) -> Result<String, PlannerError> {
    let value = object
        .get(name)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| invalid(&format!("judge verdict {name} must be a string")))?;
    let value = bounded_text(value, maximum);
    if value.is_empty() {
        return Err(invalid(&format!("judge verdict {name} cannot be empty")));
    }
    Ok(value)
}

fn execute_tool(
    session: &EditSession,
    sequence: usize,
    call: &FunctionCall,
    state: &mut LoopState,
    render_audio: &mut impl FnMut(AudioRenderRequest) -> Result<AudioRender, String>,
    on_update: &mut impl FnMut(GeminiEdit) -> Result<(), PlannerError>,
) -> Result<ToolOutput, PlannerError> {
    match call.name.as_str() {
        READ_TOOL_NAME => Ok(ToolOutput::text(match read_sound_graph(session.path()) {
            Ok(graph) => graph,
            Err(error) => format!("Tool error: {error}"),
        })),
        APPLY_TOOL_NAME if !state.initial_audio_heard => Ok(ToolOutput::text(format!(
            "Tool error: listen to the current project with {AUDIO_TOOL_NAME} before the first edit so you have an audible baseline. Choose absolute project start/end times that reveal the selection and any useful surrounding context."
        ))),
        APPLY_TOOL_NAME if !state.plans.is_empty() && !state.listened_since_edit => {
            Ok(ToolOutput::text(format!(
                "Tool error: listen to the result of the preceding edit with {AUDIO_TOOL_NAME} before applying another batch."
            )))
        }
        APPLY_TOOL_NAME => match apply_sound_graph_edits(session.path(), &call.arguments) {
            Ok(message) => {
                let (plan, project) = session
                    .take_update()
                    .map_err(|message| invalid(&message))?
                    .ok_or_else(|| invalid("edit tool did not publish its graph update"))?;
                on_update(GeminiEdit {
                    plan: plan.clone(),
                    project,
                })?;
                state.plans.push(plan);
                state.listened_since_edit = false;
                Ok(ToolOutput::text(message))
            }
            Err(error) => Ok(ToolOutput::text(format!("Tool error: {error}"))),
        },
        AUDIO_TOOL_NAME => match prepare_audio_render(session.path(), &call.arguments)
            .and_then(render_audio)
        {
            Ok(audio) => {
                state.initial_audio_heard = true;
                state.listened_since_edit = true;
                state.audio_listens += 1;
                let audio_name = session
                    .record_audio(sequence * 100 + state.audio_listens, &audio.wav)
                    .map_err(PlannerError::Io)?;
                let description = format!("{} Session artifact: {audio_name}.", audio.description);
                let output = ToolOutput {
                    result: vec![serde_json::json!({
                        "type": "text",
                        "text": description.clone()
                    })],
                    supplemental_input: vec![serde_json::json!({
                        "type": "user_input",
                        "content": [
                            {
                                "type": "text",
                                "text": format!(
                                    "Audio produced by {AUDIO_TOOL_NAME} for function call {}. Listen to this WAV before deciding what to do next.",
                                    call.id
                                )
                            },
                            {
                                "type": "audio",
                                "mime_type": "audio/wav",
                                "data": base64_audio(&audio.wav)
                            }
                        ]
                    })],
                };
                state.latest_audio = Some(LatestAudio {
                    description,
                    wav: audio.wav,
                });
                Ok(output)
            }
            Err(error) => Ok(ToolOutput::text(format!("Tool error: {error}"))),
        },
        _ => Ok(ToolOutput::text(format!(
            "Tool error: unknown tool {}",
            call.name
        ))),
    }
}

fn call_interactions(
    session: &EditSession,
    exchange_name: &str,
    request: &JsonValue,
    api_key: &str,
    endpoint: &str,
    remaining: Duration,
) -> Result<String, PlannerError> {
    let request_path = session
        .path()
        .join(format!(".{exchange_name}-pending.json"));
    fs::write(&request_path, request.to_string()).map_err(PlannerError::Io)?;
    let max_time = remaining.as_secs().max(1).to_string();
    let mut command = Command::new("curl");
    command
        .arg("--silent")
        .arg("--show-error")
        .arg("--fail-with-body")
        .arg("--connect-timeout")
        .arg("15")
        .arg("--max-time")
        .arg(max_time)
        .arg("--request")
        .arg("POST")
        .arg("--header")
        .arg("Content-Type: application/json")
        .arg("--data-binary")
        .arg(format!("@{}", request_path.display()))
        .arg("--config")
        .arg("-")
        .arg(endpoint)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = command.spawn().map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            PlannerError::Unavailable(
                "curl is required for the Gemini API connection; install curl and try again"
                    .to_owned(),
            )
        } else {
            PlannerError::Io(error)
        }
    })?;
    let mut stdin = child.stdin.take().expect("piped curl stdin");
    writeln!(stdin, "header = \"x-goog-api-key: {api_key}\"").map_err(PlannerError::Io)?;
    drop(stdin);
    let output = child.wait_with_output().map_err(PlannerError::Io)?;
    let _ = fs::remove_file(&request_path);
    let response = String::from_utf8(output.stdout)
        .map_err(|_| invalid("Gemini API response was not UTF-8"))?;
    session
        .record_exchange(exchange_name, request, &response)
        .map_err(PlannerError::Io)?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        if output.status.code() == Some(28) {
            return Err(PlannerError::TimedOut);
        }
        let message = serde_json::from_str::<JsonValue>(&response)
            .ok()
            .and_then(|body| body.get("error").map(api_error_message))
            .filter(|message| !message.is_empty())
            .unwrap_or_else(|| bounded_text(&stderr, 1_000));
        return Err(PlannerError::Failed(message));
    }
    Ok(response)
}

fn load_api_key() -> Result<String, PlannerError> {
    for name in ["DAW_AI_GEMINI_API_KEY", "GEMINI_API_KEY"] {
        if let Some(value) = std::env::var_os(name).filter(|value| !value.is_empty()) {
            return validate_api_key(&value.to_string_lossy());
        }
    }
    let path = credential_path(
        std::env::var_os("DAW_AI_GEMINI_CREDENTIALS").map(PathBuf::from),
        std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from),
        std::env::var_os("HOME").map(PathBuf::from),
    )
    .ok_or_else(|| missing_credentials(None))?;
    let source = fs::read_to_string(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            missing_credentials(Some(&path))
        } else {
            PlannerError::Io(error)
        }
    })?;
    let lines = source
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty() && !line.starts_with('#'))
        .collect::<Vec<_>>();
    let candidate = lines
        .iter()
        .find_map(|line| labeled_api_key(line))
        .or_else(|| {
            lines.iter().find_map(|line| {
                (!line.contains(['=', ':']) && !line.contains(char::is_whitespace)).then_some(*line)
            })
        })
        .unwrap_or_default()
        .trim()
        .trim_matches(['\'', '"']);
    validate_api_key(candidate)
}

fn credential_path(
    explicit: Option<PathBuf>,
    systemd_directory: Option<PathBuf>,
    home: Option<PathBuf>,
) -> Option<PathBuf> {
    explicit
        .or_else(|| systemd_directory.map(|path| path.join(SYSTEMD_CREDENTIAL_NAME)))
        .or_else(|| home.map(|path| path.join("gemini_creds.txt")))
}

fn labeled_api_key(line: &str) -> Option<&str> {
    let line = line.strip_prefix("export ").unwrap_or(line).trim();
    let (label, value) = line.split_once('=').or_else(|| line.split_once(':'))?;
    let label = label.trim().to_ascii_lowercase();
    (label.contains("key") || label == "token").then_some(value.trim())
}

fn validate_api_key(value: &str) -> Result<String, PlannerError> {
    let value = value.trim();
    if value.is_empty()
        || value.len() > 512
        || value
            .chars()
            .any(|character| character.is_control() || matches!(character, '"' | '\\'))
    {
        return Err(PlannerError::Unavailable(
            "the Gemini API key is empty or malformed".to_owned(),
        ));
    }
    Ok(value.to_owned())
}

fn missing_credentials(path: Option<&Path>) -> PlannerError {
    let location = path.map_or_else(
        || "~/gemini_creds.txt".to_owned(),
        |path| path.display().to_string(),
    );
    PlannerError::Unavailable(format!(
        "Gemini API credentials are required; set GEMINI_API_KEY, load the {SYSTEMD_CREDENTIAL_NAME} systemd credential, or put the key in {location}"
    ))
}

fn planner_task(prompt: &str, start: f32, end: f32) -> String {
    format!(
        "Selected edit region: {start:.3} to {end:.3} seconds. This bounds graph edits, not listening.\nUser request: {prompt}\n\nBegin by reading the current sound graph and rendering the channels and absolute project start/end times that best reveal the original music. Include context outside the selected edit region when transition or contrast matters. Then edit, listen to the new audio, critically compare it with the request, and iterate until the musical result is convincing. An independent audio judge will review the exact latest render when you claim completion."
    )
}

fn system_instruction() -> String {
    format!(
        concat!(
            "You are the autonomous sound-graph producer inside DAW-AI. Use the registered tools; ",
            "you cannot alter the graph by merely describing changes. Research unfamiliar musical ",
            "goals when useful. The selected region bounds edits only; every audio-tool call chooses ",
            "its own absolute project start and end, so include surrounding context when useful. Before ",
            "the first edit, read the graph and hear the original music. After every successful edit ",
            "batch, hear the updated music and reason from the ",
            "actual audio - not event-count proxies - about groove, beat subdivision, energy contour, ",
            "tension, impact, timbre, and contrast. If a style depends on intensification, express it ",
            "through composition and rhythmic subdivision when appropriate; do not assume the project ",
            "tempo must change. Continue until the audible result fulfills the request. A separate fresh ",
            "audio judge decides whether a completion claim is accepted and its rejection feedback is ",
            "required revision guidance. There is no ",
            "predetermined tool-call or iteration limit; the request timeout is the only loop limit.\n\n{}"
        ),
        STUDIO_CONTRACT
    )
}

fn api_error_message(error: &JsonValue) -> String {
    error
        .get("message")
        .and_then(JsonValue::as_str)
        .unwrap_or("the Gemini API returned an error")
        .to_owned()
}

fn bounded_text(value: &str, maximum: usize) -> String {
    value
        .trim()
        .chars()
        .take(maximum)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect()
}

fn edit_count(action: &Action) -> usize {
    match action {
        Action::Compound { actions } => actions.iter().map(edit_count).sum(),
        _ => 1,
    }
}

fn applied_steps(state: &LoopState) -> usize {
    state
        .plans
        .iter()
        .map(|plan| edit_count(&plan.action))
        .sum()
}

pub(crate) fn plan_from_json(source: &str) -> Result<EditPlan, PlannerError> {
    let value: JsonValue =
        serde_json::from_str(source).map_err(|error| invalid(&error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("top-level response must be an object"))?;
    let summary = concise_model_text(object, "summary", 160)?;
    let _musical_plan = concise_model_text(object, "musicalPlan", 300)?;
    let actions = object
        .get("actions")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| invalid("actions must be an array"))?;
    if actions.is_empty() || actions.len() > MAX_COMPOUND_ACTIONS {
        return Err(invalid("one to nine actions are required"));
    }
    let mut parsed = actions
        .iter()
        .map(action_from_json)
        .collect::<Result<Vec<_>, _>>()?;
    let action = if parsed.len() == 1 {
        parsed.pop().expect("one parsed action")
    } else {
        Action::Compound { actions: parsed }
    };
    Ok(EditPlan { action, summary })
}

fn action_from_json(value: &JsonValue) -> Result<Action, PlannerError> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("each action must be an object"))?;
    let kind = string_field(object, "kind")?;
    let target_name = string_field(object, "target")?;
    let target = role_from_name(target_name)?;
    let name = string_field(object, "name")?;
    let value = number_field(object, "value")?;
    let action = match kind {
        "gain" if (0.0..=2.0).contains(&value) => Ok(Action::Gain {
            amount: value as f32,
            target,
        }),
        "mute" => Ok(Action::Mute { target }),
        "midi-clip" if name == "MIDI Clip" => {
            let target = target.ok_or_else(|| invalid("midi-clip requires a role target"))?;
            let label = string_field(object, "setting")?.trim();
            let start = number_field(object, "start")?;
            let end = number_field(object, "end")?;
            if label.is_empty()
                || label.chars().count() > 64
                || !(0.0..1.0).contains(&start)
                || !(0.0..=1.0).contains(&end)
                || end <= start
                || !(0.25..=16.0).contains(&value)
            {
                return Err(invalid("midi-clip fields are invalid"));
            }
            return Ok(Action::MidiClip {
                track_id: integer_field(object, "trackId")?,
                target,
                label: label.to_owned(),
                start: start as f32,
                end: end as f32,
                loop_beats: value as f32,
                notes: midi_notes_field(object, value)?,
            });
        }
        "add-track" => target
            .map(|role| Action::AddTrack { role })
            .ok_or_else(|| invalid("add-track requires a role target")),
        "instrument" if value == 0.0 => Ok(Action::Instrument {
            waveform: waveform_name(name)?,
            target: target.ok_or_else(|| invalid("instrument requires a role target"))?,
        }),
        "modulator" if (0.0..=1.0).contains(&value) => Ok(Action::Modulator {
            parameter: modulator_parameter(name)?,
            shape: modulator_shape(string_field(object, "setting")?)?,
            rate: modulator_rate(number_field(object, "rate")?)?,
            depth: value as f32,
            target: target.ok_or_else(|| invalid("modulator requires a role target"))?,
        }),
        "configure" => {
            if name != "None" {
                return Err(invalid("configure name must be None"));
            }
            let clip_id = integer_field(object, "clipId")?;
            let parameter = sound_parameter_name(string_field(object, "parameter")?)?;
            let setting = configuration_value(object, parameter, value)?;
            Ok(Action::Configure {
                track_id: integer_field(object, "trackId")?,
                target: target.ok_or_else(|| invalid("configure requires a role target"))?,
                tool: sound_tool_name(string_field(object, "tool")?)?,
                tool_id: integer_field(object, "toolId")?,
                clip_id: (clip_id != 0).then_some(clip_id),
                parameter,
                value: setting,
            })
        }
        "automation" if value == 0.0 => Ok(Action::Automation {
            track_id: nonzero_integer_field(object, "trackId")?,
            parameter: automation_parameter(name)?,
            curve: automation_curve(string_field(object, "setting")?)?,
            points: automation_points_field(object)?,
            target: target.ok_or_else(|| invalid("automation requires a role target"))?,
        }),
        "effect" if (0.0..=1.0).contains(&value) => Ok(Action::Effect {
            name: effect_name(name, false)?,
            mix: value as f32,
            target,
        }),
        "remove-effect" => Ok(Action::RemoveEffect {
            name: effect_name(name, true)?,
            target,
        }),
        "filter" if (-1.0..=1.0).contains(&value) => Ok(Action::Filter {
            amount: value as f32,
            target,
        }),
        "rhythm" if (-1.0..=1.0).contains(&value) => Ok(Action::Rhythm {
            amount: value as f32,
            target,
        }),
        "tempo" if target.is_none() && value.fract() == 0.0 && (60.0..=180.0).contains(&value) => {
            Ok(Action::Tempo { bpm: value as u16 })
        }
        _ => Err(invalid("action fields are inconsistent or out of range")),
    }?;
    let start = object
        .get("start")
        .map(|_| number_field(object, "start"))
        .transpose()?
        .unwrap_or(0.0);
    let end = object
        .get("end")
        .map(|_| number_field(object, "end"))
        .transpose()?
        .unwrap_or(1.0);
    if !(0.0..1.0).contains(&start) || !(0.0..=1.0).contains(&end) || end <= start {
        return Err(invalid("action timing fields are invalid"));
    }
    if start == 0.0 && end == 1.0 {
        return Ok(action);
    }
    if !matches!(
        action,
        Action::Gain { .. }
            | Action::Mute { .. }
            | Action::AddTrack { .. }
            | Action::Automation { .. }
            | Action::Effect { .. }
            | Action::RemoveEffect { .. }
            | Action::Filter { .. }
            | Action::Rhythm { .. }
    ) {
        return Err(invalid(
            "partial timing is supported for arrangement, mix, effect, and automation actions",
        ));
    }
    Ok(Action::Timed {
        start: start as f32,
        end: end as f32,
        action: Box::new(action),
    })
}

fn automation_points_field(object: &Object) -> Result<Vec<AutomationPoint>, PlannerError> {
    let points = object
        .get("points")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| invalid("automation points must be an array"))?;
    if !(2..=16).contains(&points.len()) {
        return Err(invalid("automation requires between 2 and 16 points"));
    }
    let points = points
        .iter()
        .map(|point| {
            let point = point
                .as_object()
                .ok_or_else(|| invalid("each automation point must be an object"))?;
            Ok(AutomationPoint {
                time: number_field(point, "time")? as f32,
                value: number_field(point, "value")? as f32,
            })
        })
        .collect::<Result<Vec<_>, PlannerError>>()?;
    if points.first().map(|point| point.time) != Some(0.0)
        || points.last().map(|point| point.time) != Some(1.0)
        || points
            .iter()
            .any(|point| !(0.0..=1.0).contains(&point.time))
        || points
            .windows(2)
            .any(|points| points[1].time <= points[0].time)
    {
        return Err(invalid(
            "automation point times must increase from 0 through 1",
        ));
    }
    Ok(points)
}

fn midi_notes_field(object: &Object, loop_beats: f64) -> Result<Vec<MidiNote>, PlannerError> {
    let events = object
        .get("events")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| invalid("midi-clip events must be an array"))?;
    if events.len() > 32 {
        return Err(invalid("midi-clip supports up to 32 notes"));
    }
    events
        .iter()
        .map(|event| {
            let event = event
                .as_object()
                .ok_or_else(|| invalid("each MIDI note must be an object"))?;
            let time = number_field(event, "time")?;
            let duration = number_field(event, "duration")?;
            let pitch = integer_field(event, "pitch")?;
            let velocity = number_field(event, "velocity")?;
            if !(0.0..loop_beats).contains(&time)
                || !(0.0625..=loop_beats).contains(&duration)
                || pitch > 127
                || !(0.01..=1.0).contains(&velocity)
            {
                return Err(invalid("MIDI note fields are out of range"));
            }
            Ok(MidiNote {
                time: time as f32,
                duration: duration as f32,
                pitch: pitch as u8,
                velocity: velocity as f32,
            })
        })
        .collect()
}

fn role_from_name(name: &str) -> Result<Option<TrackRole>, PlannerError> {
    match name {
        "all" => Ok(None),
        "drums" => Ok(Some(TrackRole::Drums)),
        "bass" => Ok(Some(TrackRole::Bass)),
        "chords" => Ok(Some(TrackRole::Chords)),
        "lead" => Ok(Some(TrackRole::Lead)),
        "texture" => Ok(Some(TrackRole::Texture)),
        _ => Err(invalid("unknown action target")),
    }
}

fn effect_name(name: &str, allow_all: bool) -> Result<&'static str, PlannerError> {
    match name {
        "Reverb" => Ok("Reverb"),
        "Room" => Ok("Room"),
        "Echo" => Ok("Echo"),
        "Chorus" => Ok("Chorus"),
        "Low-pass filter" => Ok("Low-pass filter"),
        "Punch compressor" => Ok("Punch compressor"),
        "Drive" => Ok("Drive"),
        "Shimmer" => Ok("Shimmer"),
        "Effects" if allow_all => Ok("Effects"),
        _ => Err(invalid("unknown effect name")),
    }
}

fn waveform_name(name: &str) -> Result<&'static str, PlannerError> {
    match name {
        "sine" => Ok("sine"),
        "triangle" => Ok("triangle"),
        "sawtooth" => Ok("sawtooth"),
        "square" => Ok("square"),
        _ => Err(invalid("unknown instrument waveform")),
    }
}

fn modulator_parameter(name: &str) -> Result<String, PlannerError> {
    match name {
        "instrument.attack"
        | "instrument.release"
        | "instrument.tone"
        | "instrument.pitch"
        | "instrument.oscillator1.tuning"
        | "instrument.oscillator1.level"
        | "instrument.oscillator2.tuning"
        | "instrument.oscillator2.level"
        | "track.volume" => Ok(name.to_owned()),
        _ if effect_modulation_target(name).is_some() => Ok(name.to_owned()),
        _ => Err(invalid("unknown modulation target")),
    }
}

fn automation_parameter(name: &str) -> Result<String, PlannerError> {
    if modulator_parameter(name).is_ok() || modulator_automation_target(name).is_some() {
        Ok(name.to_owned())
    } else {
        Err(invalid("unknown automation target"))
    }
}

fn automation_curve(name: &str) -> Result<&'static str, PlannerError> {
    match name {
        "linear" => Ok("linear"),
        "hold" => Ok("hold"),
        _ => Err(invalid("unknown automation curve")),
    }
}

fn modulator_shape(name: &str) -> Result<&'static str, PlannerError> {
    match name {
        "sine" => Ok("sine"),
        "triangle" => Ok("triangle"),
        "square" => Ok("square"),
        "random" => Ok("random"),
        "envelope" => Ok("envelope"),
        _ => Err(invalid("unknown modulator shape")),
    }
}

fn modulator_rate(value: f64) -> Result<f32, PlannerError> {
    if (0.01..=20.0).contains(&value) {
        Ok(value as f32)
    } else {
        Err(invalid("modulator rate is out of range"))
    }
}

fn effect_modulation_target(name: &str) -> Option<u64> {
    let target = name.strip_prefix("effect:")?;
    [".mix", ".cutoff", ".resonance"]
        .iter()
        .find_map(|suffix| target.strip_suffix(suffix))?
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
}

fn modulator_automation_target(name: &str) -> Option<u64> {
    let target = name.strip_prefix("modulator:")?;
    [".rate", ".depth"]
        .iter()
        .find_map(|suffix| target.strip_suffix(suffix))?
        .parse::<u64>()
        .ok()
        .filter(|id| *id > 0)
}

fn sound_tool_name(name: &str) -> Result<&'static str, PlannerError> {
    match name {
        "instrument" => Ok("instrument"),
        "effect" => Ok("effect"),
        "modulator" => Ok("modulator"),
        "event" => Ok("event"),
        "routing" => Ok("routing"),
        _ => Err(invalid("unknown configurable sound tool")),
    }
}

fn sound_parameter_name(name: &str) -> Result<&'static str, PlannerError> {
    match name {
        "waveform" => Ok("waveform"),
        "oscillator1.waveform" => Ok("oscillator1.waveform"),
        "oscillator1.tuning" => Ok("oscillator1.tuning"),
        "oscillator1.level" => Ok("oscillator1.level"),
        "oscillator2.waveform" => Ok("oscillator2.waveform"),
        "oscillator2.tuning" => Ok("oscillator2.tuning"),
        "oscillator2.level" => Ok("oscillator2.level"),
        "attack" => Ok("attack"),
        "release" => Ok("release"),
        "tone" => Ok("tone"),
        "mix" => Ok("mix"),
        "cutoff" => Ok("cutoff"),
        "resonance" => Ok("resonance"),
        "enabled" => Ok("enabled"),
        "shape" => Ok("shape"),
        "rate" => Ok("rate"),
        "rateMode" => Ok("rateMode"),
        "trigger" => Ok("trigger"),
        "depth" => Ok("depth"),
        "target" => Ok("target"),
        "time" => Ok("time"),
        "duration" => Ok("duration"),
        "pitch" => Ok("pitch"),
        "velocity" => Ok("velocity"),
        "position" => Ok("position"),
        _ => Err(invalid("unknown sound-tool parameter")),
    }
}

fn configuration_value(
    object: &Object,
    parameter: &str,
    numeric_value: f64,
) -> Result<String, PlannerError> {
    let setting = string_field(object, "setting")?.trim();
    if !setting.is_empty() {
        if setting.chars().count() <= 64 {
            return Ok(setting.to_owned());
        }
        return Err(invalid("configure setting is longer than 64 characters"));
    }
    if matches!(
        parameter,
        "oscillator1.tuning"
            | "oscillator1.level"
            | "oscillator2.tuning"
            | "oscillator2.level"
            | "attack"
            | "release"
            | "tone"
            | "mix"
            | "cutoff"
            | "resonance"
            | "rate"
            | "depth"
            | "time"
            | "duration"
            | "pitch"
            | "velocity"
            | "position"
    ) {
        return Ok(numeric_value.to_string());
    }
    Err(invalid(&format!(
        "configure parameter {parameter} requires a non-empty string setting"
    )))
}

fn concise_model_text(object: &Object, name: &str, maximum: usize) -> Result<String, PlannerError> {
    let value = string_field(object, name)?.trim();
    if value.is_empty() {
        return Err(invalid(&format!("{name} must not be empty")));
    }
    if value.chars().count() <= maximum {
        return Ok(value.to_owned());
    }
    let mut shortened = value
        .chars()
        .take(maximum.saturating_sub(3))
        .collect::<String>();
    shortened.push_str("...");
    Ok(shortened)
}

fn string_field<'a>(object: &'a Object, name: &str) -> Result<&'a str, PlannerError> {
    object
        .get(name)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| invalid(&format!("{name} must be a string")))
}

fn number_field(object: &Object, name: &str) -> Result<f64, PlannerError> {
    object
        .get(name)
        .and_then(JsonValue::as_f64)
        .filter(|number| number.is_finite())
        .ok_or_else(|| invalid(&format!("{name} must be a finite number")))
}

fn integer_field(object: &Object, name: &str) -> Result<u64, PlannerError> {
    let value = number_field(object, name)?;
    if value.fract() == 0.0 && (0.0..=9_007_199_254_740_991.0).contains(&value) {
        Ok(value as u64)
    } else {
        Err(invalid(&format!(
            "{name} must be a non-negative safe integer"
        )))
    }
}

fn nonzero_integer_field(object: &Object, name: &str) -> Result<u64, PlannerError> {
    let value = integer_field(object, name)?;
    if value == 0 {
        Err(invalid(&format!("{name} must identify an existing track")))
    } else {
        Ok(value)
    }
}

fn invalid(message: &str) -> PlannerError {
    PlannerError::InvalidOutput(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(name: &str, arguments: JsonValue) -> FunctionCall {
        FunctionCall {
            id: format!("call-{name}"),
            name: name.to_owned(),
            arguments,
        }
    }

    #[test]
    fn systemd_credential_precedes_the_home_directory_fallback() {
        assert_eq!(
            credential_path(
                None,
                Some(PathBuf::from("/run/credentials/daw-ai.service")),
                Some(PathBuf::from("/root")),
            ),
            Some(PathBuf::from(
                "/run/credentials/daw-ai.service/gemini-api-key"
            ))
        );
        assert_eq!(
            credential_path(
                Some(PathBuf::from("/explicit/key")),
                Some(PathBuf::from("/run/credentials/daw-ai.service")),
                Some(PathBuf::from("/root")),
            ),
            Some(PathBuf::from("/explicit/key"))
        );
    }

    fn waveform_edit(waveform: &str) -> JsonValue {
        serde_json::json!({
            "summary": "Changed the bass waveform",
            "musicalPlan": "Give the bass a brighter harmonic profile.",
            "actions": [{
                "kind": "configure", "target": "bass", "name": "None", "value": 0,
                "trackId": 2, "tool": "instrument", "toolId": 201, "clipId": 0,
                "parameter": "waveform", "setting": waveform, "start": 0, "end": 1,
                "rate": 0, "events": []
            }]
        })
    }

    #[test]
    fn audio_is_required_before_and_after_each_edit_batch() {
        let session =
            EditSession::create(&Project::demo(), "shape the bass", 4.0, 8.0).expect("session");
        let mut state = LoopState::default();
        let mut updates = 0;
        let mut render_audio = render_audio_request;
        let blocked = execute_tool(
            &session,
            1,
            &call(APPLY_TOOL_NAME, waveform_edit("sawtooth")),
            &mut state,
            &mut render_audio,
            &mut |_| {
                updates += 1;
                Ok(())
            },
        )
        .expect("baseline enforcement");
        assert!(
            blocked.result[0]["text"]
                .as_str()
                .unwrap()
                .contains("audible baseline")
        );
        assert_eq!(updates, 0);

        let audio = call(
            AUDIO_TOOL_NAME,
            serde_json::json!({"trackIds": [1, 2, 3], "start": 4, "end": 8}),
        );
        let baseline = execute_tool(
            &session,
            2,
            &audio,
            &mut state,
            &mut render_audio,
            &mut |_| Ok(()),
        )
        .expect("baseline audio");
        assert_eq!(baseline.result.len(), 1);
        assert_eq!(baseline.result[0]["type"], "text");
        let audio_input = &baseline.supplemental_input[0]["content"][1];
        assert_eq!(audio_input["type"], "audio");
        assert_eq!(audio_input["mime_type"], "audio/wav");
        assert!(audio_input["data"].as_str().unwrap().starts_with("UklGR"));

        execute_tool(
            &session,
            3,
            &call(APPLY_TOOL_NAME, waveform_edit("sawtooth")),
            &mut state,
            &mut render_audio,
            &mut |_| {
                updates += 1;
                Ok(())
            },
        )
        .expect("first edit");
        assert_eq!(updates, 1);
        assert!(!state.listened_since_edit);

        let blocked = execute_tool(
            &session,
            4,
            &call(APPLY_TOOL_NAME, waveform_edit("triangle")),
            &mut state,
            &mut render_audio,
            &mut |_| Ok(()),
        )
        .expect("post-edit listening enforcement");
        assert!(
            blocked.result[0]["text"]
                .as_str()
                .unwrap()
                .contains("preceding edit")
        );

        execute_tool(
            &session,
            5,
            &audio,
            &mut state,
            &mut render_audio,
            &mut |_| Ok(()),
        )
        .expect("edited audio");
        assert!(state.listened_since_edit);
        assert_eq!(state.audio_listens, 2);
    }

    #[test]
    fn parses_interactions_function_calls() {
        let response = serde_json::json!({
            "id": "interaction-1",
            "status": "requires_action",
            "steps": [
                {"type": "thought", "content": []},
                {"type": "function_call", "id": "call-1", "name": READ_TOOL_NAME, "arguments": {}}
            ]
        });
        let calls = function_calls(&response).expect("function calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].id, "call-1");
        assert_eq!(calls[0].name, READ_TOOL_NAME);
    }

    #[test]
    fn scripted_dubstep_session_hears_acceleration_without_changing_bpm() {
        let session = EditSession::create(
            &Project::demo(),
            "turn this section into a dubstep drop",
            8.0,
            16.0,
        )
        .expect("session");
        let audio_arguments = serde_json::json!({
            "trackIds": [1, 2, 3], "start": 6, "end": 16
        });
        let edit = serde_json::json!({
            "summary": "Built a half-time drop with accelerating hats and syncopated bass",
            "musicalPlan": "Contrast a sparse half-time entrance with sixteenth-note hat motion and a driven wobbling bass.",
            "actions": [
                {
                    "kind": "midi-clip", "target": "drums", "name": "MIDI Clip", "value": 4,
                    "trackId": 1, "tool": "None", "toolId": 0, "clipId": 0,
                    "parameter": "None", "setting": "Half-time impact", "start": 0, "end": 0.5,
                    "rate": 0, "events": [
                        {"time": 0, "duration": 0.25, "pitch": 36, "velocity": 1},
                        {"time": 2, "duration": 0.25, "pitch": 38, "velocity": 1},
                        {"time": 0, "duration": 0.125, "pitch": 42, "velocity": 0.55},
                        {"time": 1, "duration": 0.125, "pitch": 42, "velocity": 0.5},
                        {"time": 2, "duration": 0.125, "pitch": 42, "velocity": 0.6},
                        {"time": 3, "duration": 0.125, "pitch": 42, "velocity": 0.52}
                    ]
                },
                {
                    "kind": "midi-clip", "target": "drums", "name": "MIDI Clip", "value": 4,
                    "trackId": 1, "tool": "None", "toolId": 0, "clipId": 0,
                    "parameter": "None", "setting": "Sixteenth-note surge", "start": 0.5, "end": 1,
                    "rate": 0, "events": [
                        {"time": 0, "duration": 0.25, "pitch": 36, "velocity": 1},
                        {"time": 2, "duration": 0.25, "pitch": 38, "velocity": 1},
                        {"time": 0, "duration": 0.0625, "pitch": 42, "velocity": 0.58},
                        {"time": 0.25, "duration": 0.0625, "pitch": 42, "velocity": 0.48},
                        {"time": 0.5, "duration": 0.0625, "pitch": 42, "velocity": 0.55},
                        {"time": 0.75, "duration": 0.0625, "pitch": 42, "velocity": 0.46},
                        {"time": 1, "duration": 0.0625, "pitch": 42, "velocity": 0.6},
                        {"time": 1.25, "duration": 0.0625, "pitch": 42, "velocity": 0.5},
                        {"time": 1.5, "duration": 0.0625, "pitch": 42, "velocity": 0.57},
                        {"time": 1.75, "duration": 0.0625, "pitch": 42, "velocity": 0.48},
                        {"time": 2, "duration": 0.0625, "pitch": 42, "velocity": 0.63},
                        {"time": 2.25, "duration": 0.0625, "pitch": 42, "velocity": 0.52},
                        {"time": 2.5, "duration": 0.0625, "pitch": 42, "velocity": 0.58},
                        {"time": 2.75, "duration": 0.0625, "pitch": 42, "velocity": 0.49},
                        {"time": 3, "duration": 0.0625, "pitch": 42, "velocity": 0.62},
                        {"time": 3.25, "duration": 0.0625, "pitch": 42, "velocity": 0.52},
                        {"time": 3.5, "duration": 0.0625, "pitch": 42, "velocity": 0.6},
                        {"time": 3.75, "duration": 0.0625, "pitch": 42, "velocity": 0.5}
                    ]
                },
                {
                    "kind": "midi-clip", "target": "bass", "name": "MIDI Clip", "value": 4,
                    "trackId": 2, "tool": "None", "toolId": 0, "clipId": 0,
                    "parameter": "None", "setting": "Syncopated drop bass", "start": 0, "end": 1,
                    "rate": 0, "events": [
                        {"time": 0, "duration": 0.75, "pitch": 29, "velocity": 1},
                        {"time": 1.25, "duration": 0.5, "pitch": 32, "velocity": 0.9},
                        {"time": 2.5, "duration": 0.5, "pitch": 27, "velocity": 0.92},
                        {"time": 3.5, "duration": 0.25, "pitch": 29, "velocity": 0.95}
                    ]
                },
                {
                    "kind": "configure", "target": "bass", "name": "None", "value": 0,
                    "trackId": 2, "tool": "instrument", "toolId": 201, "clipId": 0,
                    "parameter": "waveform", "setting": "sawtooth", "start": 0, "end": 1,
                    "rate": 0, "events": []
                },
                {
                    "kind": "modulator", "target": "bass", "name": "instrument.tone", "value": 0.72,
                    "trackId": 0, "tool": "None", "toolId": 0, "clipId": 0,
                    "parameter": "None", "setting": "square", "start": 0, "end": 1,
                    "rate": 4, "events": []
                },
                {
                    "kind": "effect", "target": "bass", "name": "Drive", "value": 0.65,
                    "trackId": 0, "tool": "None", "toolId": 0, "clipId": 0,
                    "parameter": "None", "setting": "", "start": 0, "end": 1,
                    "rate": 0, "events": []
                },
                {
                    "kind": "effect", "target": "bass", "name": "Punch compressor", "value": 0.7,
                    "trackId": 0, "tool": "None", "toolId": 0, "clipId": 0,
                    "parameter": "None", "setting": "", "start": 0, "end": 1,
                    "rate": 0, "events": []
                }
            ]
        });
        let revision = waveform_edit("square");
        let responses = [
            serde_json::json!({
                "id": "i1", "status": "requires_action", "steps": [
                    {"type": "function_call", "id": "read", "name": READ_TOOL_NAME, "arguments": {}},
                    {"type": "function_call", "id": "baseline", "name": AUDIO_TOOL_NAME, "arguments": audio_arguments}
                ]
            }),
            serde_json::json!({
                "id": "i2", "status": "requires_action", "steps": [
                    {"type": "function_call", "id": "edit", "name": APPLY_TOOL_NAME, "arguments": edit}
                ]
            }),
            serde_json::json!({
                "id": "i3", "status": "requires_action", "steps": [
                    {"type": "function_call", "id": "listen", "name": AUDIO_TOOL_NAME, "arguments": audio_arguments}
                ]
            }),
            serde_json::json!({
                "id": "i4", "status": "completed", "steps": [
                    {"type": "model_output", "content": [{"type": "text", "text": "The drop now reads clearly."}]}
                ]
            }),
            serde_json::json!({
                "id": "i5", "status": "requires_action", "steps": [
                    {"type": "function_call", "id": "revision", "name": APPLY_TOOL_NAME, "arguments": revision}
                ]
            }),
            serde_json::json!({
                "id": "i6", "status": "requires_action", "steps": [
                    {"type": "function_call", "id": "relisten", "name": AUDIO_TOOL_NAME, "arguments": audio_arguments}
                ]
            }),
            serde_json::json!({
                "id": "i7", "status": "completed", "steps": [
                    {"type": "model_output", "content": [{"type": "text", "text": "The revised drop is complete."}]}
                ]
            }),
        ];
        let judge_responses = [
            serde_json::json!({
                "id": "j1", "status": "requires_action", "steps": [{
                    "type": "function_call", "id": "verdict-1", "name": JUDGE_TOOL_NAME,
                    "arguments": {
                        "accepted": false,
                        "summary": "The drop lacks a forceful bass identity",
                        "audibleEvidence": "The half-time pulse is audible, but the bass remains too soft and rounded against the drums.",
                        "feedback": [
                            "Give the bass a more harmonically aggressive waveform and preserve a clear kick-and-snare anchor.",
                            "Keep the faster hat subdivision audible against the heavier half-time groove."
                        ]
                    }
                }]
            }),
            serde_json::json!({
                "id": "j2", "status": "requires_action", "steps": [{
                    "type": "function_call", "id": "verdict-2", "name": JUDGE_TOOL_NAME,
                    "arguments": {
                        "accepted": true,
                        "summary": "The requested drop is now plainly audible",
                        "audibleEvidence": "A clear half-time impact, rapid hat subdivision, and aggressive syncopated bass distinguish the drop.",
                        "feedback": []
                    }
                }]
            }),
        ];
        let mut response_index = 0;
        let mut judge_index = 0;
        let mut saw_separate_audio_input = false;
        let mut saw_judge_feedback = false;
        let mut updates = 0;
        let mut render_audio = render_audio_request;
        let result = run_session_with_transport(
            &session,
            "turn this section into a dubstep drop",
            8.0,
            16.0,
            &mut render_audio,
            &mut |_| {
                updates += 1;
                Ok(())
            },
            (
                &mut |sequence, request, _| {
                    if sequence == 2 {
                        let input = request["input"].as_array().unwrap();
                        assert!(
                            input
                                .iter()
                                .filter(|step| step["type"] == "function_result")
                                .all(|result| result["result"]
                                    .as_array()
                                    .unwrap()
                                    .iter()
                                    .all(|part| part["type"] == "text"))
                        );
                        saw_separate_audio_input = input
                            .iter()
                            .filter(|step| step["type"] == "user_input")
                            .flat_map(|step| step["content"].as_array().unwrap())
                            .any(|part| {
                                part["type"] == "audio" && part["mime_type"] == "audio/wav"
                            });
                    }
                    if sequence == 5 {
                        let feedback = request["input"].as_str().unwrap();
                        saw_judge_feedback = feedback.contains("fresh interaction")
                            && feedback.contains("harmonically aggressive waveform");
                    }
                    let response = responses[response_index].to_string();
                    response_index += 1;
                    Ok(response)
                },
                &mut |sequence, request, _| {
                    assert_eq!(sequence, judge_index + 1);
                    assert_eq!(request["store"], false);
                    assert!(request.get("previous_interaction_id").is_none());
                    assert_eq!(
                        request["generation_config"]["tool_choice"]["allowed_tools"]["mode"],
                        "any"
                    );
                    let judge_input = &request["input"][0];
                    assert_eq!(judge_input["type"], "user_input");
                    assert!(judge_input["content"].as_array().unwrap().iter().any(
                        |part| part["type"] == "audio" && part["mime_type"] == "audio/wav"
                    ));
                    let response = judge_responses[judge_index].to_string();
                    judge_index += 1;
                    Ok(response)
                },
            ),
        )
        .expect("scripted Gemini session");

        assert_eq!(response_index, 7);
        assert_eq!(judge_index, 2);
        assert!(saw_separate_audio_input);
        assert!(saw_judge_feedback);
        assert_eq!(updates, 2);
        assert_eq!(result.project.bpm, Project::demo().bpm);
        let drums = result
            .project
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Drums)
            .expect("drums");
        assert!(
            drums
                .clips
                .iter()
                .any(|clip| clip.label == "Half-time impact")
        );
        let surge = drums
            .clips
            .iter()
            .find(|clip| clip.label == "Sixteenth-note surge")
            .expect("accelerating subdivision");
        assert!(
            surge
                .events
                .windows(2)
                .any(|events| { (events[1].time - events[0].time - 0.25).abs() < f32::EPSILON })
        );
        assert_eq!(session.stats().unwrap(), (8, 3, 2, 1));
    }

    #[test]
    fn gemini_prompt_requires_direct_audio_iteration_without_a_tempo_assumption() {
        let task = planner_task("make the bass hit harder", 4.0, 8.0);
        let instruction = system_instruction();
        assert!(task.contains("absolute project start/end times"));
        assert!(task.contains("context outside the selected edit region"));
        assert!(task.contains("critically compare it with the request"));
        assert!(instruction.contains("selected region bounds edits only"));
        assert!(instruction.contains("chooses its own absolute project start and end"));
        assert!(instruction.contains("hear the original music"));
        assert!(instruction.contains("hear the updated music"));
        assert!(instruction.contains("actual audio - not event-count proxies"));
        assert!(instruction.contains("rhythmic subdivision"));
        assert!(instruction.contains("tempo must change"));
        assert!(instruction.contains("separate fresh audio judge"));
        assert!(instruction.contains("no predetermined tool-call or iteration limit"));
    }

    #[test]
    fn parses_scoped_parameter_automation() {
        let plan = plan_from_json(
            r#"{
                "summary":"Opened the bass filter through the transition",
                "musicalPlan":"Build brightness only through the middle of the selection.",
                "actions":[{
                    "kind":"automation","target":"bass","name":"effect:210.cutoff","value":0,
                    "trackId":2,"tool":"None","toolId":0,"clipId":0,"parameter":"None",
                    "setting":"linear","start":0.2,"end":0.8,"rate":0,
                    "points":[
                        {"time":0,"value":300},
                        {"time":0.5,"value":4000},
                        {"time":1,"value":12000}
                    ]
                }]
            }"#,
        )
        .expect("valid scoped automation");

        assert_eq!(
            plan.action,
            Action::Timed {
                start: 0.2,
                end: 0.8,
                action: Box::new(Action::Automation {
                    track_id: 2,
                    parameter: "effect:210.cutoff".to_owned(),
                    curve: "linear",
                    points: vec![
                        AutomationPoint {
                            time: 0.0,
                            value: 300.0,
                        },
                        AutomationPoint {
                            time: 0.5,
                            value: 4000.0,
                        },
                        AutomationPoint {
                            time: 1.0,
                            value: 12000.0,
                        },
                    ],
                    target: TrackRole::Bass,
                }),
            }
        );
    }

    #[test]
    fn parses_a_compound_structured_edit() {
        let plan = plan_from_json(
            r#"{
                "summary":"Warmed the chords and added space",
                "musicalPlan":"Darken the chord timbre and add a long ambient tail.",
                "actions":[
                    {"kind":"filter","target":"chords","name":"None","value":-0.3},
                    {"kind":"effect","target":"chords","name":"Reverb","value":0.42},
                    {"kind":"effect","target":"chords","name":"Drive","value":0.2}
                ]
            }"#,
        )
        .expect("valid plan");
        assert_eq!(
            plan.action,
            Action::Compound {
                actions: vec![
                    Action::Filter {
                        amount: -0.3,
                        target: Some(TrackRole::Chords),
                    },
                    Action::Effect {
                        name: "Reverb",
                        mix: 0.42,
                        target: Some(TrackRole::Chords),
                    },
                    Action::Effect {
                        name: "Drive",
                        mix: 0.2,
                        target: Some(TrackRole::Chords),
                    },
                ],
            }
        );
    }

    #[test]
    fn parses_an_explicit_midi_clip() {
        let plan = plan_from_json(
            r#"{
                "summary":"Wrote a syncopated bass phrase",
                "musicalPlan":"Replace the selected bass with a low, syncopated two-beat MIDI loop.",
                "actions":[{
                    "kind":"midi-clip","target":"bass","name":"MIDI Clip","value":2,
                    "trackId":2,"tool":"None","toolId":0,"clipId":0,"parameter":"None",
                    "setting":"Syncopated bass","start":0,"end":1,"rate":0,
                    "events":[
                        {"time":0,"duration":0.5,"pitch":29,"velocity":1},
                        {"time":1.25,"duration":0.5,"pitch":32,"velocity":0.85}
                    ]
                }]
            }"#,
        )
        .expect("valid MIDI clip plan");
        let Action::MidiClip {
            track_id,
            target,
            loop_beats,
            notes,
            ..
        } = plan.action
        else {
            panic!("expected MIDI clip");
        };
        assert_eq!(track_id, 2);
        assert_eq!(target, TrackRole::Bass);
        assert_eq!(loop_beats, 2.0);
        assert_eq!(notes.len(), 2);
        assert_eq!(notes[1].pitch, 32);
    }

    #[test]
    fn parses_an_empty_midi_clip_as_a_region_clear() {
        let plan = plan_from_json(
            r#"{
                "summary":"Cleared the selected bass region",
                "musicalPlan":"Make room for a replacement bass part by clearing the old MIDI.",
                "actions":[{
                    "kind":"midi-clip","target":"bass","name":"MIDI Clip","value":4,
                    "trackId":2,"tool":"None","toolId":0,"clipId":0,"parameter":"None",
                    "setting":"Bass rest","start":0,"end":1,"rate":0,"events":[]
                }]
            }"#,
        )
        .expect("valid empty MIDI clip plan");

        assert!(matches!(
            plan.action,
            Action::MidiClip {
                track_id: 2,
                target: TrackRole::Bass,
                notes,
                ..
            } if notes.is_empty()
        ));
    }

    #[test]
    fn rejects_midi_note_duration_longer_than_its_loop() {
        let invalid = r#"{
            "summary":"Wrote a short bass loop",
            "musicalPlan":"Replace the selection with a quarter-beat bass loop.",
            "actions":[{
                "kind":"midi-clip","target":"bass","name":"MIDI Clip","value":0.25,
                "trackId":2,"tool":"None","toolId":0,"clipId":0,"parameter":"None",
                "setting":"Short bass loop","start":0,"end":1,"rate":0,
                "events":[{"time":0,"duration":16,"pitch":29,"velocity":1}]
            }]
        }"#;
        assert!(plan_from_json(invalid).is_err());
    }

    #[test]
    fn rejects_invalid_structured_edits() {
        let invalid = r#"{
            "summary":"Unsafe tempo",
            "musicalPlan":"Raise the tempo beyond the supported range.",
            "actions":[{"kind":"tempo","target":"all","name":"None","value":999}]
        }"#;
        assert!(plan_from_json(invalid).is_err());
    }

    #[test]
    fn automation_schema_and_parser_both_require_points() {
        let schema: JsonValue = serde_json::from_str(EDIT_SCHEMA).expect("edit schema");
        let action_schema = &schema["properties"]["actions"]["items"];
        let requires_points = action_schema["allOf"]
            .as_array()
            .expect("action conditionals")
            .iter()
            .any(|conditional| {
                conditional["if"]["properties"]["kind"]["const"] == "automation"
                    && conditional["then"]["required"]
                        .as_array()
                        .is_some_and(|required| required.iter().any(|field| field == "points"))
            });
        assert!(requires_points, "automation schema must require points");

        let missing_points = r#"{
            "summary":"Automated bass volume",
            "musicalPlan":"Raise the bass through the selected region.",
            "actions":[{
                "kind":"automation","target":"bass","name":"track.volume","value":0,
                "trackId":2,"tool":"None","toolId":0,"clipId":0,"parameter":"None",
                "setting":"linear","start":0,"end":1,"rate":0,"events":[]
            }]
        }"#;
        assert!(plan_from_json(missing_points).is_err());
    }

    #[test]
    fn parses_sound_tool_actions() {
        let plan = plan_from_json(
            r#"{
                "summary":"Changed the bass source and added movement",
                "musicalPlan":"Use a bright bass oscillator and square-wave tone modulation.",
                "actions":[
                    {"kind":"instrument","target":"bass","name":"sawtooth","value":0},
                    {"kind":"modulator","target":"bass","name":"instrument.tone","value":0.25,"setting":"square","rate":2}
                ]
            }"#,
        )
        .expect("valid sound tool plan");
        assert_eq!(
            plan.action,
            Action::Compound {
                actions: vec![
                    Action::Instrument {
                        waveform: "sawtooth",
                        target: TrackRole::Bass,
                    },
                    Action::Modulator {
                        parameter: "instrument.tone".to_owned(),
                        shape: "square",
                        rate: 2.0,
                        depth: 0.25,
                        target: TrackRole::Bass,
                    },
                ],
            }
        );
    }

    #[test]
    fn parses_any_published_modulation_target() {
        let plan = plan_from_json(
            r#"{
                "summary":"Route movement to the bass filter cutoff",
                "musicalPlan":"Add slow sine movement to the existing bass filter cutoff.",
                "actions":[{
                    "kind":"modulator","target":"bass","name":"effect:210.cutoff","value":0.25,
                    "trackId":0,"tool":"None","toolId":0,"clipId":0,"parameter":"None","setting":"sine","rate":0.5
                }]
            }"#,
        )
        .expect("valid stable-ID modulation target");
        assert_eq!(
            plan.action,
            Action::Modulator {
                parameter: "effect:210.cutoff".to_owned(),
                shape: "sine",
                rate: 0.5,
                depth: 0.25,
                target: TrackRole::Bass,
            }
        );
    }

    #[test]
    fn parses_stable_id_sound_tool_configuration() {
        let plan = plan_from_json(
            r#"{
                "summary":"Shortened the selected bass event",
                "musicalPlan":"Tighten the first bass note while preserving its pitch and velocity.",
                "actions":[{
                    "kind":"configure","target":"bass","name":"None","value":0,
                    "trackId":2,"tool":"event","toolId":1201,"clipId":12,
                    "parameter":"duration","setting":"0.0625"
                }]
            }"#,
        )
        .expect("valid configuration action");
        assert_eq!(
            plan.action,
            Action::Configure {
                track_id: 2,
                target: TrackRole::Bass,
                tool: "event",
                tool_id: 1201,
                clip_id: Some(12),
                parameter: "duration",
                value: "0.0625".to_owned(),
            }
        );
    }

    #[test]
    fn parses_numeric_configuration_from_the_numeric_value_field() {
        let plan = plan_from_json(
            r#"{
                "summary":"Raised the bass filter mix",
                "musicalPlan":"Make the existing low-pass filter more prominent.",
                "actions":[{
                    "kind":"configure","target":"bass","name":"None","value":0.9,
                    "trackId":2,"tool":"effect","toolId":210,"clipId":0,
                    "parameter":"mix","setting":"","start":0,"end":1,"rate":0,"events":[]
                }]
            }"#,
        )
        .expect("numeric configure value");
        assert_eq!(
            plan.action,
            Action::Configure {
                track_id: 2,
                target: TrackRole::Bass,
                tool: "effect",
                tool_id: 210,
                clip_id: None,
                parameter: "mix",
                value: "0.9".to_owned(),
            }
        );
    }

    #[test]
    fn shortens_model_prose_without_rejecting_a_valid_edit() {
        let long_summary = "bass ".repeat(50);
        let long_plan = "wobble ".repeat(100);
        let source = serde_json::json!({
            "summary": long_summary,
            "musicalPlan": long_plan,
            "actions": [{
                "kind": "effect", "target": "bass", "name": "Drive", "value": 0.5
            }]
        })
        .to_string();
        let plan = plan_from_json(&source).expect("valid edit with verbose prose");
        assert_eq!(plan.summary.chars().count(), 160);
        assert!(plan.summary.ends_with("..."));
    }

    #[test]
    fn decodes_json_surrogate_pairs_and_rejects_unpaired_surrogates() {
        let valid = r#"{
            "summary":"Added sparkle \uD83C\uDFB6",
            "musicalPlan":"Open the chord tone slightly.",
            "actions":[{"kind":"filter","target":"chords","name":"None","value":0.2}]
        }"#;
        assert_eq!(
            plan_from_json(valid).expect("valid surrogate pair").summary,
            "Added sparkle \u{1F3B6}"
        );

        for summary in [r#""Bad \uD83C text""#, r#""Bad \uDFB6 text""#] {
            let invalid = format!(
                r#"{{"summary":{summary},"musicalPlan":"Open the chord tone slightly.","actions":[{{"kind":"filter","target":"chords","name":"None","value":0.2}}]}}"#
            );
            assert!(plan_from_json(&invalid).is_err());
        }
    }
}
