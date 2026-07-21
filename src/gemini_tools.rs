use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value as JsonValue;

#[cfg(test)]
use crate::audio_analysis;
use crate::audio_analysis::MAX_REGION_SECONDS;
use crate::gemini::{EDIT_SCHEMA, plan_from_json};
use crate::model::{Project, StudioError, json_string};
use crate::prompt::{Action, EditPlan, MAX_COMPOUND_ACTIONS};
use crate::storage::ProjectStore;

pub(crate) const APPLY_TOOL_NAME: &str = "apply_sound_graph_edits";
pub(crate) const READ_TOOL_NAME: &str = "read_sound_graph";
pub(crate) const AUDIO_TOOL_NAME: &str = "render_audio_region";
const GRAPH_FILE: &str = "sound-graph.json";
const REQUEST_FILE: &str = "request.json";
const SESSION_FILE: &str = "session.json";
const PROGRESS_DIRECTORY: &str = "edit-progress";
const PENDING_PROGRESS_DIRECTORY: &str = ".edit-progress.pending";
const PROGRESS_PLAN_FILE: &str = "plan.json";
const PROGRESS_GRAPH_FILE: &str = "project.json";
const AUDIO_REGION_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["trackIds", "start", "end"],
  "properties": {
    "trackIds": {
      "type": "array",
      "description": "One or more stable channel IDs from sound-graph.json. Choose the full mix when judging an arrangement-level result and isolated channels when diagnosing a part.",
      "items": { "type": "integer", "minimum": 1 },
      "minItems": 1,
      "maxItems": 32,
      "uniqueItems": true
    },
    "start": {
      "type": "number",
      "minimum": 0,
      "description": "Absolute start time in project seconds. This listening range is independent of the selected edit region and may include context before it."
    },
    "end": {
      "type": "number",
      "exclusiveMinimum": 0,
      "description": "Absolute end time in project seconds, after start and no more than 16 seconds later. It may include context after the selected edit region."
    }
  }
}"#;
static SESSION_ID: AtomicU64 = AtomicU64::new(1);

pub(crate) struct EditSession {
    path: PathBuf,
    persistent: bool,
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
            write_new(
                &path.join(SESSION_FILE),
                &serde_json::json!({
                    "id": path.file_name().unwrap_or_default().to_string_lossy(),
                    "createdAt": unix_milliseconds(),
                    "updatedAt": unix_milliseconds(),
                    "status": "running",
                    "model": "gemini-3.5-flash",
                    "prompt": prompt,
                    "start": start,
                    "end": end,
                    "appliedSteps": 0,
                    "audioListens": 0,
                    "judgeReviews": 0,
                    "judgeRejections": 0,
                    "detail": "Gemini session started"
                })
                .to_string(),
            )?;
            Ok(Self {
                path: path.clone(),
                persistent: !cfg!(test),
            })
        })();
        if result.is_err() {
            let _ = fs::remove_dir_all(&path);
        }
        result
    }

    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn record_exchange(
        &self,
        name: &str,
        request: &JsonValue,
        response: &str,
    ) -> io::Result<()> {
        write_new(
            &self.path.join(format!("{name}-request.json")),
            &request.to_string(),
        )?;
        write_new(&self.path.join(format!("{name}-response.json")), response)
    }

    pub(crate) fn record_audio(&self, sequence: usize, wav: &[u8]) -> io::Result<String> {
        let name = format!("audio-{sequence:03}.wav");
        write_new_with(&self.path.join(&name), |file| file.write_all(wav))?;
        Ok(name)
    }

    pub(crate) fn update_status(
        &self,
        status: &str,
        detail: &str,
        applied_steps: usize,
        audio_listens: usize,
        judge_reviews: usize,
        judge_rejections: usize,
    ) -> io::Result<()> {
        let path = self.path.join(SESSION_FILE);
        let source = fs::read_to_string(&path)?;
        let mut value = serde_json::from_str::<JsonValue>(&source)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid session record"))?;
        object.insert("status".to_owned(), JsonValue::String(status.to_owned()));
        object.insert("detail".to_owned(), JsonValue::String(detail.to_owned()));
        object.insert("updatedAt".to_owned(), unix_milliseconds().into());
        object.insert("appliedSteps".to_owned(), applied_steps.into());
        object.insert("audioListens".to_owned(), audio_listens.into());
        object.insert("judgeReviews".to_owned(), judge_reviews.into());
        object.insert("judgeRejections".to_owned(), judge_rejections.into());
        write_replace(&path, &value.to_string())
    }

    pub(crate) fn stats(&self) -> io::Result<(usize, usize, usize, usize)> {
        let source = fs::read_to_string(self.path.join(SESSION_FILE))?;
        let value = serde_json::from_str::<JsonValue>(&source)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let applied_steps = value
            .get("appliedSteps")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0) as usize;
        let audio_listens = value
            .get("audioListens")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0) as usize;
        let judge_reviews = value
            .get("judgeReviews")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0) as usize;
        let judge_rejections = value
            .get("judgeRejections")
            .and_then(JsonValue::as_u64)
            .unwrap_or(0) as usize;
        Ok((
            applied_steps,
            audio_listens,
            judge_reviews,
            judge_rejections,
        ))
    }

    pub(crate) fn finish(&self, plans: Vec<EditPlan>) -> Result<(EditPlan, Project), String> {
        let mut actions = Vec::new();
        let mut summary = None;
        for plan in plans {
            actions.push(plan.action);
            summary = Some(plan.summary);
        }
        if actions.is_empty() {
            return Err(format!(
                "Gemini did not use the registered {APPLY_TOOL_NAME} tool"
            ));
        }
        let action = bounded_compound(actions);
        let graph = fs::read_to_string(self.path.join(GRAPH_FILE))
            .map_err(|error| format!("could not read Gemini sound graph: {error}"))?;
        let project = Project::from_json(&graph)
            .map_err(|error| format!("Gemini left an invalid sound graph: {error}"))?;
        Ok((
            EditPlan {
                action,
                summary: summary.expect("plans were nonempty"),
            },
            project,
        ))
    }

    pub(crate) fn take_update(&self) -> Result<Option<(EditPlan, Project)>, String> {
        let path = progress_path(&self.path);
        if !path.exists() {
            return Ok(None);
        }
        if !path.is_dir() {
            return Err("Gemini edit progress handoff is not a directory".to_owned());
        }
        let plan_source = fs::read_to_string(path.join(PROGRESS_PLAN_FILE))
            .map_err(|error| format!("could not read Gemini edit plan progress: {error}"))?;
        let graph_source = fs::read_to_string(path.join(PROGRESS_GRAPH_FILE))
            .map_err(|error| format!("could not read Gemini sound graph progress: {error}"))?;
        let plan = plan_from_json(&plan_source).map_err(|error| error.to_string())?;
        let project = Project::from_json(&graph_source)
            .map_err(|error| format!("Gemini edit progress is invalid: {error}"))?;
        fs::remove_dir_all(&path)
            .map_err(|error| format!("could not consume Gemini edit progress: {error}"))?;
        Ok(Some((plan, project)))
    }
}

impl Drop for EditSession {
    fn drop(&mut self) {
        if self.persistent {
            return;
        }
        if let Err(error) = fs::remove_dir_all(&self.path) {
            if error.kind() != io::ErrorKind::NotFound {
                eprintln!("warning: could not remove Gemini test session: {error}");
            }
        }
    }
}

pub(crate) fn tool_declarations() -> Vec<JsonValue> {
    let edit_schema =
        serde_json::from_str::<JsonValue>(EDIT_SCHEMA).expect("embedded edit schema is valid JSON");
    let audio_schema = serde_json::from_str::<JsonValue>(AUDIO_REGION_SCHEMA)
        .expect("embedded audio schema is valid JSON");
    vec![
        serde_json::json!({
            "type": "function",
            "name": READ_TOOL_NAME,
            "description": "Read the latest DAW-AI sound graph with stable channel, clip, event, instrument, effect, modulator, automation-target, and routing IDs. Call this before editing and again whenever an edit creates IDs needed by a later batch.",
            "parameters": {
                "type": "object",
                "properties": {},
                "additionalProperties": false
            }
        }),
        serde_json::json!({
            "type": "function",
            "name": APPLY_TOOL_NAME,
            "description": "Apply one validated batch of generic MIDI, instrument, effect, modulation, automation, routing, mix, arrangement, or tempo operations. A validation error leaves the graph unchanged. Use a focused batch, then render and listen before making another batch.",
            "parameters": edit_schema
        }),
        serde_json::json!({
            "type": "function",
            "name": AUDIO_TOOL_NAME,
            "description": "Render model-chosen channels and absolute project start/end times from the latest sound graph as WAV audio for direct musical listening. The listening range is independent of the selected edit scope: include surrounding context when transition, contrast, or continuity matters. Listen before the first edit and after every successful edit.",
            "parameters": audio_schema
        }),
    ]
}

#[derive(Debug)]
pub(crate) struct AudioRender {
    pub(crate) description: String,
    pub(crate) wav: Vec<u8>,
}

#[derive(Debug)]
pub(crate) struct AudioRenderRequest {
    pub(crate) project: Project,
    pub(crate) track_ids: Vec<u64>,
    pub(crate) start: f32,
    pub(crate) end: f32,
    pub(crate) description: String,
}

pub(crate) fn read_sound_graph(session_path: &Path) -> Result<String, String> {
    fs::read_to_string(session_path.join(GRAPH_FILE))
        .map_err(|error| format!("could not read current sound graph: {error}"))
}

pub(crate) fn apply_sound_graph_edits(
    session_path: &Path,
    arguments: &JsonValue,
) -> Result<String, String> {
    apply_graph_edits(session_path, &arguments.to_string())
}

#[cfg(test)]
pub(crate) fn render_audio(
    session_path: &Path,
    arguments: &JsonValue,
) -> Result<AudioRender, String> {
    render_audio_request(prepare_audio_render(session_path, arguments)?)
}

pub(crate) fn prepare_audio_render(
    session_path: &Path,
    arguments: &JsonValue,
) -> Result<AudioRenderRequest, String> {
    let project = current_project(session_path)?;
    let (track_ids, start, end) = audio_region_arguments(&project, arguments)?;
    let description = format!(
        "Rendered {} from {:.3} to {:.3} seconds through the same stereo 48 kHz Web Audio engine used for DAW playback. Listen to the audio itself and describe the audible rhythm, subdivision, energy contour, timbre, transitions, and shortcomings before deciding what to do next.",
        selected_channel_labels(&project, &track_ids),
        start,
        end,
    );
    Ok(AudioRenderRequest {
        project,
        track_ids,
        start,
        end,
        description,
    })
}

#[cfg(test)]
pub(crate) fn render_audio_request(request: AudioRenderRequest) -> Result<AudioRender, String> {
    let region = audio_analysis::render_region(
        &request.project,
        &request.track_ids,
        request.start,
        request.end,
    )?;
    Ok(AudioRender {
        description: request.description,
        wav: wav_bytes(&region.samples),
    })
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
            "render range must be between 0 and {:.3} seconds with end after start",
            project.duration
        ));
    }
    if end - start > MAX_REGION_SECONDS {
        return Err(format!(
            "render ranges are limited to {MAX_REGION_SECONDS} seconds"
        ));
    }
    Ok((track_ids, start, end))
}

fn selected_channel_labels(project: &Project, track_ids: &[u64]) -> String {
    track_ids
        .iter()
        .filter_map(|track_id| project.tracks.iter().find(|track| track.id == *track_id))
        .map(|track| format!("{} ({}, ID {})", track.name, track.role.as_str(), track.id))
        .collect::<Vec<_>>()
        .join(", ")
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

pub(crate) fn base64_audio(bytes: &[u8]) -> String {
    base64(bytes)
}

#[cfg(test)]
fn wav_bytes(samples: &[f32]) -> Vec<u8> {
    let data_bytes = u32::try_from(samples.len().saturating_mul(2)).unwrap_or(u32::MAX);
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36_u32.saturating_add(data_bytes)).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16_u32.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&1_u16.to_le_bytes());
    wav.extend_from_slice(&audio_analysis::SAMPLE_RATE.to_le_bytes());
    wav.extend_from_slice(&(audio_analysis::SAMPLE_RATE * 2).to_le_bytes());
    wav.extend_from_slice(&2_u16.to_le_bytes());
    wav.extend_from_slice(&16_u16.to_le_bytes());
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_bytes.to_le_bytes());
    for sample in samples {
        let pcm = (sample.clamp(-1.0, 1.0) * f32::from(i16::MAX)).round() as i16;
        wav.extend_from_slice(&pcm.to_le_bytes());
    }
    wav
}

fn apply_graph_edits(session_path: &Path, source: &str) -> Result<String, String> {
    let plan = plan_from_json(source).map_err(|error| error.to_string())?;
    let new_action_count = action_count(&plan.action);
    wait_for_progress_handoff(session_path);
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
    if let Err(error) = publish_progress(session_path, source, studio.project()) {
        return match store.save(&original_project) {
            Ok(()) => Err(error),
            Err(rollback_error) => Err(format!(
                "{error}; also could not restore sound-graph.json: {rollback_error}"
            )),
        };
    }
    Ok(serde_json::json!({
        "message": format!(
            "Applied {new_action_count} action(s) and updated the sound graph to version {}: {summary}",
            studio.project().version
        ),
        "version": studio.project().version,
        "summary": summary,
        "channels": sound_tool_inventory(studio.project())
    })
    .to_string())
}

fn sound_tool_inventory(project: &Project) -> Vec<JsonValue> {
    project
        .tracks
        .iter()
        .map(|track| {
            serde_json::json!({
                "id": track.id,
                "name": track.name,
                "role": track.role.as_str(),
                "instrumentId": track.instrument.id,
                "effects": track.effects.iter().map(|effect| {
                    serde_json::json!({"id": effect.id, "name": effect.name})
                }).collect::<Vec<_>>(),
                "modulators": track.modulators.iter().map(|modulator| {
                    serde_json::json!({
                        "id": modulator.id,
                        "name": modulator.name,
                        "target": modulator.target
                    })
                }).collect::<Vec<_>>(),
                "clips": track.clips.iter().map(|clip| {
                    serde_json::json!({"id": clip.id, "label": clip.label})
                }).collect::<Vec<_>>()
            })
        })
        .collect()
}

fn wait_for_progress_handoff(session_path: &Path) {
    let path = progress_path(session_path);
    // The single slot transfers ownership in order and bounds temporary graph storage.
    while path.exists() {
        thread::sleep(Duration::from_millis(10));
    }
}

fn publish_progress(session_path: &Path, plan: &str, project: &Project) -> Result<(), String> {
    let pending = session_path.join(PENDING_PROGRESS_DIRECTORY);
    let published = progress_path(session_path);
    let result = (|| {
        fs::create_dir(&pending)
            .map_err(|error| format!("could not prepare Gemini edit progress: {error}"))?;
        write_new(&pending.join(PROGRESS_PLAN_FILE), plan)
            .map_err(|error| format!("could not record Gemini edit plan progress: {error}"))?;
        write_new(&pending.join(PROGRESS_GRAPH_FILE), &project.to_json())
            .map_err(|error| format!("could not record Gemini sound graph progress: {error}"))?;
        fs::rename(&pending, &published)
            .map_err(|error| format!("could not publish Gemini edit progress: {error}"))
    })();
    if result.is_err() {
        let _ = fs::remove_dir_all(&pending);
    }
    result
}

fn progress_path(session_path: &Path) -> PathBuf {
    session_path.join(PROGRESS_DIRECTORY)
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

fn bounded_compound(mut actions: Vec<Action>) -> Action {
    while actions.len() > MAX_COMPOUND_ACTIONS {
        let mut grouped = Vec::with_capacity(actions.len().div_ceil(MAX_COMPOUND_ACTIONS));
        let mut remaining = actions.into_iter();
        loop {
            let children = remaining
                .by_ref()
                .take(MAX_COMPOUND_ACTIONS)
                .collect::<Vec<_>>();
            if children.is_empty() {
                break;
            }
            grouped.push(action_group(children));
        }
        actions = grouped;
    }
    action_group(actions)
}

fn action_group(mut actions: Vec<Action>) -> Action {
    if actions.len() == 1 {
        actions.pop().expect("one action")
    } else {
        Action::Compound { actions }
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
    let root = session_root();
    fs::create_dir_all(&root)?;
    set_private_directory(&root)?;
    for _ in 0..64 {
        let id = SESSION_ID.fetch_add(1, Ordering::Relaxed);
        let path = root.join(format!(
            "{}-{}-{id}",
            unix_milliseconds(),
            std::process::id()
        ));
        match fs::create_dir(&path) {
            Ok(()) => {
                if let Err(error) = set_private_directory(&path) {
                    let _ = fs::remove_dir(&path);
                    return Err(error);
                }
                return Ok(path);
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(error) => return Err(error),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "could not reserve a Gemini edit session",
    ))
}

pub(crate) fn session_root() -> PathBuf {
    if let Some(path) =
        std::env::var_os("DAW_AI_GEMINI_SESSION_DIR").filter(|path| !path.is_empty())
    {
        return PathBuf::from(path);
    }
    if let Some(path) = std::env::var_os("DAW_AI_PROJECT_PATH").filter(|path| !path.is_empty()) {
        if let Some(parent) = Path::new(&path).parent() {
            return parent.join("gemini-sessions");
        }
    }
    if cfg!(test) {
        return std::env::temp_dir().join(format!("daw-ai-gemini-tests-{}", std::process::id()));
    }
    std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("gemini-sessions")
}

pub(crate) fn session_summaries() -> io::Result<Vec<JsonValue>> {
    let root = session_root();
    let entries = match fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error),
    };
    let mut sessions = Vec::new();
    for entry in entries {
        let entry = entry?;
        let path = entry.path().join(SESSION_FILE);
        if !path.is_file() {
            continue;
        }
        let Ok(source) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(value) = serde_json::from_str::<JsonValue>(&source) else {
            continue;
        };
        sessions.push(value);
    }
    sessions.sort_by_key(|session| {
        std::cmp::Reverse(
            session
                .get("createdAt")
                .and_then(JsonValue::as_u64)
                .unwrap_or(0),
        )
    });
    sessions.truncate(100);
    Ok(sessions)
}

fn unix_milliseconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .unwrap_or(u64::MAX)
}

fn set_private_directory(path: &Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;

        fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
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

fn write_replace(path: &Path, source: &str) -> io::Result<()> {
    let temporary = path.with_extension(format!("tmp-{}", std::process::id()));
    let result = (|| {
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)?;
        file.write_all(source.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        fs::rename(&temporary, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&temporary);
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waveform_edit(tool_id: u64, waveform: &str) -> JsonValue {
        serde_json::json!({
            "summary": "Changed the bass waveform",
            "musicalPlan": "Give the bass a more useful harmonic profile.",
            "actions": [{
                "kind": "configure", "target": "bass", "name": "None", "value": 0,
                "trackId": 2, "tool": "instrument", "toolId": tool_id, "clipId": 0,
                "parameter": "waveform", "setting": waveform, "start": 0, "end": 1,
                "rate": 0, "events": []
            }]
        })
    }

    #[test]
    fn declares_direct_graph_editing_and_audio_tools() {
        let declarations = tool_declarations();
        let names = declarations
            .iter()
            .filter_map(|tool| tool.get("name").and_then(JsonValue::as_str))
            .collect::<Vec<_>>();
        assert_eq!(names, [READ_TOOL_NAME, APPLY_TOOL_NAME, AUDIO_TOOL_NAME]);
        assert!(
            declarations[2]["description"]
                .as_str()
                .unwrap()
                .contains("direct musical listening")
        );
    }

    #[test]
    fn persists_session_metadata_and_wav_artifacts() {
        let session =
            EditSession::create(&Project::demo(), "test the drop", 0.0, 2.0).expect("edit session");
        let rendered = render_audio(
            session.path(),
            &serde_json::json!({"trackIds": [1, 2], "start": 0, "end": 1}),
        )
        .expect("audio render");
        assert_eq!(&rendered.wav[..4], b"RIFF");
        assert_eq!(&rendered.wav[8..12], b"WAVE");
        let artifact = session
            .record_audio(1, &rendered.wav)
            .expect("WAV artifact");
        assert!(session.path().join(artifact).is_file());
        session
            .update_status("completed", "Done", 2, 1, 1, 0)
            .expect("session metadata");
        let session_id = session.path().file_name().unwrap().to_string_lossy();
        let summaries = session_summaries().expect("session summaries");
        let summary = summaries
            .iter()
            .find(|summary| summary["id"] == session_id.as_ref())
            .expect("current session summary");
        assert_eq!(summary["status"], "completed");
        assert_eq!(summary["appliedSteps"], 2);
        assert_eq!(summary["audioListens"], 1);
        assert_eq!(summary["judgeReviews"], 1);
        assert_eq!(summary["judgeRejections"], 0);
    }

    #[test]
    fn applies_valid_batches_and_returns_useful_errors_without_mutation() {
        let original = Project::demo();
        let session =
            EditSession::create(&original, "shape the bass", 4.0, 8.0).expect("edit session");
        let error = apply_sound_graph_edits(session.path(), &waveform_edit(999, "sawtooth"))
            .expect_err("unknown stable ID");
        assert!(error.contains("stable IDs"));
        assert_eq!(
            current_project(session.path()).unwrap().to_json(),
            original.to_json()
        );

        let response = apply_sound_graph_edits(session.path(), &waveform_edit(201, "sawtooth"))
            .expect("valid graph edit");
        assert!(response.contains("updated the sound graph"));
        let (plan, project) = session.take_update().unwrap().expect("published update");
        assert_eq!(plan.summary, "Changed the bass waveform");
        assert_eq!(project.tracks[1].instrument.waveform, "sawtooth");
    }

    #[test]
    fn audio_render_validates_stable_channel_ids() {
        let session =
            EditSession::create(&Project::demo(), "listen", 0.0, 2.0).expect("edit session");
        let error = render_audio(
            session.path(),
            &serde_json::json!({"trackIds": [999], "start": 0, "end": 1}),
        )
        .expect_err("unknown channel");
        assert!(error.contains("available channel IDs"));
        assert!(error.contains("1 (Pulse Kit, drums)"));
    }

    #[test]
    fn audio_render_range_is_independent_of_the_edit_selection() {
        let session = EditSession::create(&Project::demo(), "listen in context", 8.0, 12.0)
            .expect("edit session");
        let request = prepare_audio_render(
            session.path(),
            &serde_json::json!({"trackIds": [1, 2, 3], "start": 2, "end": 7}),
        )
        .expect("context render outside selection");

        assert_eq!(request.start, 2.0);
        assert_eq!(request.end, 7.0);
        assert!(request.description.contains("2.000 to 7.000 seconds"));
    }
}
