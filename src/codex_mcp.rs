use std::fs::{self, OpenOptions};
use std::io::{self, BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value as JsonValue;

use crate::audio_analysis::{self, MAX_REGION_SECONDS};
use crate::codex::{EDIT_SCHEMA, plan_from_json};
use crate::model::{Project, StudioError, json_string};
use crate::prompt::{Action, EditPlan};
use crate::storage::{ProjectStore, replace_text_file};

pub(crate) const MCP_SESSION_ENV: &str = "DAW_AI_MCP_SESSION";
pub(crate) const MCP_TOOL_NAME: &str = "apply_sound_graph_edits";
pub(crate) const MEL_TOOL_NAME: &str = "render_mel_spectrogram";
pub(crate) const ANALYZE_TOOL_NAME: &str = "analyze_audio_region";
const GRAPH_FILE: &str = "sound-graph.json";
const REQUEST_FILE: &str = "request.json";
const OPERATIONS_FILE: &str = "edit-operations.jsonl";
const PROGRESS_FILE_PREFIX: &str = "edit-progress-";
const MAX_OPERATIONS_BYTES: u64 = 1024 * 1024;
const AUDIO_REGION_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["trackIds", "start", "end"],
  "properties": {
    "trackIds": {
      "type": "array",
      "description": "One or more stable channel IDs from sound-graph.json.",
      "items": { "type": "integer", "minimum": 1 },
      "minItems": 1,
      "maxItems": 32,
      "uniqueItems": true
    },
    "start": { "type": "number", "minimum": 0 },
    "end": { "type": "number", "exclusiveMinimum": 0 }
  }
}"#;
static SESSION_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct EditSession {
    path: PathBuf,
}

impl EditSession {
    pub(crate) fn create(
        project: &Project,
        prompt: &str,
        start: f32,
        end: f32,
    ) -> io::Result<Self> {
        let path = reserve_session_directory()?;
        let result = (|| {
            write_new(&path.join(GRAPH_FILE), &project.planner_json())?;
            write_new(
                &path.join(REQUEST_FILE),
                &format!(
                    "{{\"start\":{start},\"end\":{end},\"prompt\":{}}}",
                    json_string(prompt)
                ),
            )?;
            write_new(&path.join(OPERATIONS_FILE), "")?;
            Ok(Self { path: path.clone() })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&path);
        }
        result
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn finish(&self) -> Result<(EditPlan, Project), String> {
        let plans = read_plans(&self.path)?;
        let mut actions = Vec::new();
        let mut summary = None;
        for plan in plans {
            append_actions(plan.action, &mut actions);
            summary = Some(plan.summary);
        }
        if actions.is_empty() {
            return Err(format!(
                "Codex did not use the registered {MCP_TOOL_NAME} tool"
            ));
        }
        if actions.len() > 8 {
            return Err("Codex applied more than eight sound-graph actions".to_owned());
        }
        let action = if actions.len() == 1 {
            actions.pop().expect("one action")
        } else {
            Action::Compound { actions }
        };
        let graph = fs::read_to_string(self.path.join(GRAPH_FILE))
            .map_err(|error| format!("could not read Codex sound graph: {error}"))?;
        let project = Project::from_json(&graph)
            .map_err(|error| format!("Codex left an invalid sound graph: {error}"))?;
        Ok((
            EditPlan {
                action,
                summary: summary.expect("plans were nonempty"),
            },
            project,
        ))
    }

    pub(crate) fn updates_after(
        &self,
        delivered: usize,
    ) -> Result<Vec<(EditPlan, Project)>, String> {
        let plans = read_plans(&self.path)?;
        if delivered > plans.len() {
            return Err("Codex edit progress moved backwards".to_owned());
        }
        plans
            .into_iter()
            .enumerate()
            .skip(delivered)
            .map(|(index, plan)| {
                let source = fs::read_to_string(progress_path(&self.path, index + 1))
                    .map_err(|error| format!("could not read Codex edit progress: {error}"))?;
                let project = Project::from_json(&source)
                    .map_err(|error| format!("Codex edit progress is invalid: {error}"))?;
                Ok((plan, project))
            })
            .collect()
    }
}

impl Drop for EditSession {
    fn drop(&mut self) {
        if let Err(error) = fs::remove_dir_all(&self.path) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!("warning: could not remove Codex edit session: {error}");
            }
        }
    }
}

pub(crate) fn run_from_environment() -> io::Result<()> {
    let path = std::env::var_os(MCP_SESSION_ENV)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("{MCP_SESSION_ENV} is required for MCP mode"),
            )
        })?;
    run(io::stdin().lock(), io::stdout().lock(), &path)
}

fn run(reader: impl BufRead, mut writer: impl Write, session_path: &Path) -> io::Result<()> {
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        if let Some(response) = handle_message(&line, session_path) {
            writer.write_all(response.as_bytes())?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
    }
    Ok(())
}

fn handle_message(source: &str, session_path: &Path) -> Option<String> {
    let value = match serde_json::from_str::<JsonValue>(source) {
        Ok(value) => value,
        Err(error) => {
            return Some(json_rpc_error(
                "null",
                -32700,
                &format!("invalid JSON: {error}"),
            ));
        }
    };
    let Some(request) = value.as_object() else {
        return Some(json_rpc_error("null", -32600, "request must be an object"));
    };
    let id = request.get("id").map(ToString::to_string);
    let method = request.get("method").and_then(JsonValue::as_str);
    let Some(method) = method else {
        return id.map(|id| json_rpc_error(&id, -32600, "method must be a string"));
    };
    match method {
        "notifications/initialized" | "notifications/cancelled" => None,
        "initialize" => id.map(|id| initialize_response(&id, request.get("params"))),
        "ping" => id.map(|id| json_rpc_result(&id, "{}")),
        "tools/list" => id.map(|id| tools_response(&id)),
        "tools/call" => id.map(|id| call_tool_response(&id, request.get("params"), session_path)),
        _ => id.map(|id| json_rpc_error(&id, -32601, "method not found")),
    }
}

fn initialize_response(id: &str, params: Option<&JsonValue>) -> String {
    let protocol = params
        .and_then(JsonValue::as_object)
        .and_then(|params| params.get("protocolVersion"))
        .and_then(JsonValue::as_str)
        .unwrap_or("2025-06-18");
    json_rpc_result(
        id,
        &format!(
            concat!(
                "{{\"protocolVersion\":{},\"capabilities\":{{\"tools\":{{\"listChanged\":false}}}},",
                "\"serverInfo\":{{\"name\":\"daw-ai\",\"version\":{}}},",
                "\"instructions\":{}}}"
            ),
            json_string(protocol),
            json_string(env!("CARGO_PKG_VERSION")),
            json_string(
                "Read sound-graph.json before editing. Use the read-only listening tools to inspect relevant channels when useful. Use apply_sound_graph_edits for every intended change; it validates and rewrites the graph. Keep all batches to eight actions total."
            )
        ),
    )
}

fn tools_response(id: &str) -> String {
    let edit_schema =
        serde_json::from_str::<JsonValue>(EDIT_SCHEMA).expect("embedded edit schema is valid JSON");
    let audio_schema = serde_json::from_str::<JsonValue>(AUDIO_REGION_SCHEMA)
        .expect("embedded audio analysis schema is valid JSON");
    json_rpc_result(
        id,
        &serde_json::json!({
            "tools": [
                {
                    "name": MCP_TOOL_NAME,
                    "description": "Apply one validated batch of generic MIDI clip, instrument, effect, modulator, routing, mix, arrangement, or tempo operations to sound-graph.json. Returns a precise validation error without changing the graph when the batch is invalid. Call iteratively when useful, with no more than eight actions across the full edit.",
                    "inputSchema": edit_schema,
                    "annotations": {
                        "title": "Apply sound graph edits",
                        "readOnlyHint": false,
                        "destructiveHint": false,
                        "idempotentHint": false,
                        "openWorldHint": false
                    }
                },
                {
                    "name": MEL_TOOL_NAME,
                    "description": "Render selected channels and a bounded time range from the current sound graph, then return a 64-band Mel spectrogram as a PNG image. Use it to inspect frequency balance, transients, density, and changes between edit batches.",
                    "inputSchema": audio_schema,
                    "annotations": {
                        "title": "Render Mel spectrogram",
                        "readOnlyHint": true,
                        "destructiveHint": false,
                        "idempotentHint": true,
                        "openWorldHint": false
                    }
                },
                {
                    "name": ANALYZE_TOOL_NAME,
                    "description": "Render selected channels and report deterministic audio-region measurements including peak, RMS, zero-crossing rate, spectral centroid, and low/mid/high frequency energy ratios.",
                    "inputSchema": audio_schema,
                    "annotations": {
                        "title": "Analyze audio region",
                        "readOnlyHint": true,
                        "destructiveHint": false,
                        "idempotentHint": true,
                        "openWorldHint": false
                    }
                }
            ]
        })
        .to_string(),
    )
}

fn call_tool_response(id: &str, params: Option<&JsonValue>, session_path: &Path) -> String {
    let result = params
        .and_then(JsonValue::as_object)
        .ok_or_else(|| "tool-call params must be an object".to_owned())
        .and_then(|params| {
            let name = params
                .get("name")
                .and_then(JsonValue::as_str)
                .ok_or_else(|| "tool name is required".to_owned())?;
            let arguments = params
                .get("arguments")
                .ok_or_else(|| "tool arguments are required".to_owned())?;
            match name {
                MCP_TOOL_NAME => apply_graph_edits(session_path, &arguments.to_string())
                    .map(|message| text_content(&message)),
                MEL_TOOL_NAME => render_mel_spectrogram(session_path, arguments),
                ANALYZE_TOOL_NAME => analyze_audio_region(session_path, arguments),
                _ => Err(format!("unknown tool: {name}")),
            }
        });
    let (is_error, content) = match result {
        Ok(content) => (false, content),
        Err(message) => (true, text_content(&message)),
    };
    json_rpc_result(
        id,
        &format!("{{\"content\":{content},\"isError\":{is_error}}}"),
    )
}

fn text_content(text: &str) -> String {
    format!("[{{\"type\":\"text\",\"text\":{}}}]", json_string(text))
}

fn current_project(session_path: &Path) -> Result<Project, String> {
    let source = fs::read_to_string(session_path.join(GRAPH_FILE))
        .map_err(|error| format!("could not read sound-graph.json: {error}"))?;
    Project::from_json(&source).map_err(|error| format!("sound-graph.json is invalid: {error}"))
}

fn audio_region_arguments(
    project: &Project,
    arguments: &JsonValue,
) -> Result<(Vec<u64>, f32, f32), String> {
    let arguments = arguments
        .as_object()
        .ok_or_else(|| "audio analysis arguments must be an object".to_owned())?;
    let values = arguments
        .get("trackIds")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "trackIds must be an array".to_owned())?;
    if values.is_empty() || values.len() > 32 {
        return Err("trackIds must contain between 1 and 32 channel IDs".to_owned());
    }
    let mut track_ids = Vec::with_capacity(values.len());
    for value in values {
        let track_id = value
            .as_u64()
            .filter(|track_id| *track_id > 0)
            .ok_or_else(|| "trackIds must contain positive integers".to_owned())?;
        if track_ids.contains(&track_id) {
            return Err(format!("channel {track_id} was requested more than once"));
        }
        if !project.tracks.iter().any(|track| track.id == track_id) {
            let available = project
                .tracks
                .iter()
                .map(|track| format!("{} ({}, {})", track.id, track.name, track.role.as_str()))
                .collect::<Vec<_>>()
                .join(", ");
            return Err(format!(
                "channel {track_id} does not exist; available channel IDs: {available}"
            ));
        }
        track_ids.push(track_id);
    }
    let number = |name: &str| {
        arguments
            .get(name)
            .and_then(JsonValue::as_f64)
            .filter(|value| value.is_finite())
            .map(|value| value as f32)
            .ok_or_else(|| format!("{name} must be a finite number"))
    };
    let start = number("start")?;
    let end = number("end")?;
    if start < 0.0 || end <= start || end > project.duration {
        return Err(format!(
            "analysis range must be between 0 and {:.3} seconds with end after start",
            project.duration
        ));
    }
    if end - start > MAX_REGION_SECONDS {
        return Err(format!(
            "analysis ranges are limited to {MAX_REGION_SECONDS} seconds"
        ));
    }
    Ok((track_ids, start, end))
}

fn render_mel_spectrogram(session_path: &Path, arguments: &JsonValue) -> Result<String, String> {
    let project = current_project(session_path)?;
    let (track_ids, start, end) = audio_region_arguments(&project, arguments)?;
    let region = audio_analysis::render_region(&project, &track_ids, start, end)?;
    let spectrogram = audio_analysis::mel_spectrogram(&region);
    let channels = selected_channel_labels(&project, &track_ids);
    let description = format!(
        concat!(
            "Rendered {} from {:.3} to {:.3} seconds at {} Hz: ",
            "{} events, {} Mel bands, {} source frames, {}x{} PNG, {:.1} to {:.1} dB."
        ),
        channels,
        start,
        end,
        audio_analysis::SAMPLE_RATE,
        region.event_count,
        64,
        spectrogram.frames,
        spectrogram.width,
        spectrogram.height,
        spectrogram.minimum_db,
        spectrogram.maximum_db
    );
    Ok(format!(
        concat!(
            "[{{\"type\":\"text\",\"text\":{}}},",
            "{{\"type\":\"image\",\"data\":{},\"mimeType\":\"image/png\"}}]"
        ),
        json_string(&description),
        json_string(&base64(&spectrogram.png))
    ))
}

fn analyze_audio_region(session_path: &Path, arguments: &JsonValue) -> Result<String, String> {
    let project = current_project(session_path)?;
    let (track_ids, start, end) = audio_region_arguments(&project, arguments)?;
    let region = audio_analysis::render_region(&project, &track_ids, start, end)?;
    let analysis = audio_analysis::analyze(&region);
    let channels = selected_channel_metadata(&project, &track_ids);
    let report = serde_json::json!({
        "trackIds": track_ids,
        "channels": channels,
        "start": start,
        "end": end,
        "sampleRate": audio_analysis::SAMPLE_RATE,
        "eventsRendered": region.event_count,
        "peak": round_measurement(analysis.peak),
        "rms": round_measurement(analysis.rms),
        "zeroCrossingRate": round_measurement(analysis.zero_crossing_rate),
        "spectralCentroidHz": (analysis.spectral_centroid_hz * 10.0).round() / 10.0,
        "energyRatios": {
            "lowBelow250Hz": round_measurement(analysis.low_energy_ratio),
            "mid250To2500Hz": round_measurement(analysis.mid_energy_ratio),
            "highAbove2500Hz": round_measurement(analysis.high_energy_ratio)
        }
    });
    Ok(text_content(&report.to_string()))
}

fn selected_channel_labels(project: &Project, track_ids: &[u64]) -> String {
    track_ids
        .iter()
        .filter_map(|track_id| project.tracks.iter().find(|track| track.id == *track_id))
        .map(|track| format!("{} ({}, ID {})", track.name, track.role.as_str(), track.id))
        .collect::<Vec<_>>()
        .join(", ")
}

fn selected_channel_metadata(project: &Project, track_ids: &[u64]) -> Vec<JsonValue> {
    track_ids
        .iter()
        .filter_map(|track_id| project.tracks.iter().find(|track| track.id == *track_id))
        .map(|track| {
            serde_json::json!({
                "id": track.id,
                "name": track.name,
                "role": track.role.as_str()
            })
        })
        .collect()
}

fn round_measurement(value: f32) -> f32 {
    (value * 10_000.0).round() / 10_000.0
}

fn base64(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut output = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let first = chunk[0];
        let second = chunk.get(1).copied().unwrap_or(0);
        let third = chunk.get(2).copied().unwrap_or(0);
        output.push(ALPHABET[(first >> 2) as usize] as char);
        output.push(ALPHABET[(((first & 0x03) << 4) | (second >> 4)) as usize] as char);
        output.push(if chunk.len() > 1 {
            ALPHABET[(((second & 0x0f) << 2) | (third >> 6)) as usize] as char
        } else {
            '='
        });
        output.push(if chunk.len() > 2 {
            ALPHABET[(third & 0x3f) as usize] as char
        } else {
            '='
        });
    }
    output
}

fn apply_graph_edits(session_path: &Path, source: &str) -> Result<String, String> {
    let plan = plan_from_json(source).map_err(|error| error.to_string())?;
    let prior_plans = read_plans(session_path)?;
    let prior_action_count = prior_plans
        .iter()
        .map(|plan| action_count(&plan.action))
        .sum::<usize>();
    let new_action_count = action_count(&plan.action);
    if prior_action_count + new_action_count > 8 {
        return Err(format!(
            "This batch would exceed the eight-action edit limit ({prior_action_count} already applied, {new_action_count} requested)."
        ));
    }
    let (start, end, prompt) = read_request(session_path)?;
    let graph_path = session_path.join(GRAPH_FILE);
    if !graph_path.is_file() {
        return Err("sound-graph.json is missing from the edit session".to_owned());
    }
    let (store, mut studio) = ProjectStore::open(graph_path)
        .map_err(|error| format!("Could not load sound-graph.json: {error}"))?;
    let original_project = studio.project().clone();
    let summary = studio
        .apply_plan(start, end, &prompt, plan.clone())
        .map_err(studio_error_message)?;
    store
        .save(studio.project())
        .map_err(|error| format!("Could not write sound-graph.json: {error}"))?;
    let progress_path = progress_path(session_path, prior_plans.len() + 1);
    if let Err(error) = write_new(&progress_path, &studio.project().to_json()) {
        let _ = fs::remove_file(&progress_path);
        return match store.save(&original_project) {
            Ok(()) => Err(format!("could not record Codex edit progress: {error}")),
            Err(rollback_error) => Err(format!(
                "could not record Codex edit progress: {error}; also could not restore sound-graph.json: {rollback_error}"
            )),
        };
    }
    if let Err(error) = append_operation(session_path, source) {
        let _ = fs::remove_file(progress_path);
        return match store.save(&original_project) {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(format!(
                "{error}; also could not restore sound-graph.json: {rollback_error}"
            )),
        };
    }
    Ok(format!(
        "Applied {new_action_count} action(s) and updated sound-graph.json to version {}: {summary}",
        studio.project().version
    ))
}

fn progress_path(session_path: &Path, sequence: usize) -> PathBuf {
    session_path.join(format!("{PROGRESS_FILE_PREFIX}{sequence}.json"))
}

fn read_request(session_path: &Path) -> Result<(f32, f32, String), String> {
    let source = fs::read_to_string(session_path.join(REQUEST_FILE))
        .map_err(|error| format!("could not read edit request: {error}"))?;
    let value = serde_json::from_str::<JsonValue>(&source)
        .map_err(|error| format!("edit request is invalid: {error}"))?;
    let request = value
        .as_object()
        .ok_or_else(|| "edit request must be an object".to_owned())?;
    let number = |name: &str| {
        request
            .get(name)
            .and_then(JsonValue::as_f64)
            .filter(|value| value.is_finite())
            .map(|value| value as f32)
            .ok_or_else(|| format!("edit request {name} must be a finite number"))
    };
    let prompt = request
        .get("prompt")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| "edit request prompt must be a string".to_owned())?
        .to_owned();
    Ok((number("start")?, number("end")?, prompt))
}

fn read_plans(session_path: &Path) -> Result<Vec<EditPlan>, String> {
    let path = session_path.join(OPERATIONS_FILE);
    let metadata = fs::metadata(&path)
        .map_err(|error| format!("could not inspect edit operations: {error}"))?;
    if metadata.len() > MAX_OPERATIONS_BYTES {
        return Err("edit operations exceeded the session limit".to_owned());
    }
    let source = fs::read_to_string(path)
        .map_err(|error| format!("could not read edit operations: {error}"))?;
    source
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| plan_from_json(line).map_err(|error| error.to_string()))
        .collect()
}

fn append_operation(session_path: &Path, source: &str) -> Result<(), String> {
    let value = serde_json::from_str::<JsonValue>(source)
        .map_err(|error| format!("could not record invalid tool arguments: {error}"))?;
    let path = session_path.join(OPERATIONS_FILE);
    let mut operations = fs::read_to_string(&path)
        .map_err(|error| format!("could not read edit operations: {error}"))?;
    operations.push_str(&value.to_string());
    operations.push('\n');
    if operations.len() as u64 > MAX_OPERATIONS_BYTES {
        return Err("edit operations exceeded the session limit".to_owned());
    }
    replace_text_file(&path, &operations)
        .map_err(|error| format!("could not record edit operations: {error}"))
}

fn append_actions(action: Action, actions: &mut Vec<Action>) {
    if let Action::Compound { actions: children } = action {
        for child in children {
            append_actions(child, actions);
        }
    } else {
        actions.push(action);
    }
}

fn action_count(action: &Action) -> usize {
    match action {
        Action::Compound { actions } => actions.iter().map(action_count).sum(),
        _ => 1,
    }
}

fn studio_error_message(error: StudioError) -> String {
    match error {
        StudioError::EmptyPrompt => "The edit request is empty.".to_owned(),
        StudioError::InvalidPrompt => "The edit request is too long.".to_owned(),
        StudioError::InvalidSelection => {
            "The selected region is outside the sound graph duration.".to_owned()
        }
        StudioError::UnknownTrack => concat!(
            "An action targets a track that does not exist. Use a published track ID and role, ",
            "or add the role before editing it."
        )
        .to_owned(),
        StudioError::InvalidMix => "A mixer value is outside its published range.".to_owned(),
        StudioError::InvalidChannel => "A channel change exceeds the sound graph limits.".to_owned(),
        StudioError::UnknownSoundTool => concat!(
            "An action references a sound-tool, clip, or event ID that is not in sound-graph.json. ",
            "Read the graph again and use its stable IDs."
        )
        .to_owned(),
        StudioError::InvalidSoundTool => concat!(
            "A sound-tool value or connection is incompatible or outside its published range. ",
            "Use modulationTargets and the ranges in the graph contract."
        )
        .to_owned(),
    }
}

fn reserve_session_directory() -> io::Result<PathBuf> {
    for _ in 0..64 {
        let id = SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("daw-ai-codex-session-{}-{id}", std::process::id()));
        match fs::create_dir(&path) {
            Ok(()) => {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;

                    if let Err(error) =
                        fs::set_permissions(&path, fs::Permissions::from_mode(0o700))
                    {
                        let _ = fs::remove_dir(&path);
                        return Err(error);
                    }
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a Codex edit session",
    ))
}

fn write_new(path: &Path, source: &str) -> io::Result<()> {
    write_new_with(path, |file| {
        file.write_all(source.as_bytes())?;
        file.write_all(b"\n")
    })
}

fn write_new_with(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> io::Result<()>,
) -> io::Result<()> {
    let mut file = OpenOptions::new().write(true).create_new(true).open(path)?;
    let result = write(&mut file).and_then(|()| file.sync_all());
    drop(file);
    if result.is_err() {
        let _ = fs::remove_file(path);
    }
    result
}

fn json_rpc_result(id: &str, result: &str) -> String {
    format!("{{\"jsonrpc\":\"2.0\",\"id\":{id},\"result\":{result}}}")
}

fn json_rpc_error(id: &str, code: i32, message: &str) -> String {
    format!(
        "{{\"jsonrpc\":\"2.0\",\"id\":{id},\"error\":{{\"code\":{code},\"message\":{}}}}}",
        json_string(message)
    )
}

#[cfg(test)]
mod tests {
    use std::io::{BufReader, Cursor};

    use super::*;

    fn tool_call(id: u64, tool_id: u64, waveform: &str) -> String {
        format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":{},\"method\":\"tools/call\",",
                "\"params\":{{\"name\":{},\"arguments\":{{",
                "\"summary\":\"Changed the bass waveform\",",
                "\"musicalPlan\":\"Use a brighter oscillator for the selected bass.\",",
                "\"actions\":[{{\"kind\":\"configure\",\"target\":\"bass\",",
                "\"name\":\"None\",\"value\":0,\"trackId\":2,",
                "\"tool\":\"instrument\",\"toolId\":{},\"clipId\":0,",
                "\"parameter\":\"waveform\",\"setting\":{},",
                "\"start\":0,\"end\":1,\"rate\":0,\"events\":[]}}]}}}}}}"
            ),
            id,
            json_string(MCP_TOOL_NAME),
            tool_id,
            json_string(waveform)
        )
    }

    fn listening_tool_call(id: u64, name: &str, track_ids: &str, start: f32, end: f32) -> String {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": {
                "name": name,
                "arguments": {
                    "trackIds": serde_json::from_str::<JsonValue>(track_ids).unwrap(),
                    "start": start,
                    "end": end
                }
            }
        })
        .to_string()
    }

    #[test]
    fn serves_and_applies_registered_graph_tools() {
        let session = EditSession::create(&Project::demo(), "brighten the bass", 4.0, 8.0)
            .expect("edit session");
        let input = format!(
            concat!(
                "{{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"initialize\",",
                "\"params\":{{\"protocolVersion\":\"test-version\"}}}}\n",
                "{{\"jsonrpc\":\"2.0\",\"method\":\"notifications/initialized\"}}\n",
                "{{\"jsonrpc\":\"2.0\",\"id\":2,\"method\":\"tools/list\"}}\n",
                "{}\n"
            ),
            tool_call(3, 201, "sawtooth")
        );
        let mut output = Vec::new();
        run(
            BufReader::new(Cursor::new(input)),
            &mut output,
            session.path(),
        )
        .expect("MCP transcript");
        let output = String::from_utf8(output).expect("UTF-8 output");
        assert!(output.contains("test-version"));
        assert!(output.contains(MCP_TOOL_NAME));
        assert!(output.contains(MEL_TOOL_NAME));
        assert!(output.contains(ANALYZE_TOOL_NAME));
        assert!(output.contains("\"isError\":false"));

        let first_update = session.updates_after(0).expect("first published update");
        assert_eq!(first_update.len(), 1);
        assert_eq!(first_update[0].0.summary, "Changed the bass waveform");
        assert_eq!(first_update[0].1.tracks[1].instrument.waveform, "sawtooth");

        let second = handle_message(&tool_call(4, 201, "triangle"), session.path())
            .expect("second tool response");
        assert!(second.contains("\"isError\":false"));
        let second_update = session.updates_after(1).expect("second published update");
        assert_eq!(second_update.len(), 1);
        assert_eq!(second_update[0].1.tracks[1].instrument.waveform, "triangle");

        let (plan, graph) = session.finish().expect("completed graph edit");
        assert_eq!(plan.summary, "Changed the bass waveform");
        assert_eq!(graph.tracks[1].instrument.waveform, "triangle");
    }

    #[test]
    fn returns_useful_tool_errors_without_changing_the_graph() {
        let project = Project::demo();
        let session =
            EditSession::create(&project, "brighten the bass", 4.0, 8.0).expect("edit session");
        let response =
            handle_message(&tool_call(1, 999, "sawtooth"), session.path()).expect("tool response");
        assert!(response.contains("\"isError\":true"));
        assert!(response.contains("stable IDs"));
        assert!(session.finish().is_err());
        let graph = fs::read_to_string(session.path().join(GRAPH_FILE)).unwrap();
        assert_eq!(
            Project::from_json(&graph).unwrap().to_json(),
            project.to_json()
        );
    }

    #[test]
    fn listening_tools_return_an_image_and_audio_measurements() {
        let session = EditSession::create(&Project::demo(), "inspect the mix", 0.0, 2.0)
            .expect("edit session");
        let spectrogram = handle_message(
            &listening_tool_call(1, MEL_TOOL_NAME, "[1,2]", 0.0, 1.0),
            session.path(),
        )
        .expect("spectrogram response");
        assert!(spectrogram.contains("\"mimeType\":\"image/png\""));
        assert!(spectrogram.contains("iVBORw0KGgo"));
        assert!(spectrogram.contains("\"isError\":false"));
        assert!(spectrogram.contains("Pulse Kit (drums, ID 1)"));
        assert!(spectrogram.contains("Soft Current (bass, ID 2)"));

        let analysis = handle_message(
            &listening_tool_call(2, ANALYZE_TOOL_NAME, "[2,3]", 0.0, 1.0),
            session.path(),
        )
        .expect("analysis response");
        let analysis: JsonValue = serde_json::from_str(&analysis).expect("analysis envelope");
        assert_eq!(analysis["result"]["isError"], false);
        let report = analysis["result"]["content"][0]["text"]
            .as_str()
            .expect("analysis text");
        let report: JsonValue = serde_json::from_str(report).expect("analysis report");
        assert!(report.get("spectralCentroidHz").is_some());
        assert!(report.get("energyRatios").is_some());
        assert_eq!(
            report["channels"],
            serde_json::json!([
                {"id": 2, "name": "Soft Current", "role": "bass"},
                {"id": 3, "name": "Glass Chords", "role": "chords"}
            ])
        );

        let invalid = handle_message(
            &listening_tool_call(3, ANALYZE_TOOL_NAME, "[999]", 0.0, 1.0),
            session.path(),
        )
        .expect("invalid analysis response");
        assert!(invalid.contains("available channel IDs"));
        assert!(invalid.contains("1 (Pulse Kit, drums)"));
        assert!(invalid.contains("\"isError\":true"));
    }

    #[test]
    fn failed_new_file_writes_remove_the_incomplete_snapshot() {
        let session =
            EditSession::create(&Project::demo(), "test cleanup", 4.0, 8.0).expect("edit session");
        let path = session.path().join("failed-progress.json");
        let result = write_new_with(&path, |file| {
            file.write_all(b"incomplete")?;
            Err(io::Error::other("simulated snapshot failure"))
        });
        assert!(result.is_err());
        assert!(!path.exists());
    }
}
