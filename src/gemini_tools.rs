use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value as JsonValue};

use crate::audio_analysis::{self, MAX_REGION_SECONDS};
use crate::model::{MidiClipSpec, Project, Studio, StudioError, TrackRole, json_string};
use crate::prompt::{Action, EditPlan, MAX_COMPOUND_ACTIONS, MidiNote};
use crate::storage::ProjectStore;

pub(crate) const READ_TOOL_NAME: &str = "read_sound_graph";
pub(crate) const AUDIO_TOOL_NAME: &str = "render_audio_region";
pub(crate) const PRESET_TOOL_NAME: &str = "list_surge_presets";
const GRAPH_FILE: &str = "sound-graph.json";
const REQUEST_FILE: &str = "request.json";
const SESSION_FILE: &str = "session.json";
const PROGRESS_DIRECTORY: &str = "edit-progress";
const PENDING_PROGRESS_DIRECTORY: &str = ".edit-progress.pending";
const PROGRESS_PLAN_FILE: &str = "plan.json";
const PROGRESS_GRAPH_FILE: &str = "project.json";
const UNDO_GRAPH_FILE: &str = "undo-sound-graph.json";
pub(crate) const MUTATION_TOOL_NAMES: &[&str] = &[
    "new_track",
    "delete_track",
    "set_surge_preset",
    "add_midi_clip",
    "update_midi_clip",
    "delete_midi_clip",
    "add_effect",
    "update_effect",
    "delete_effect",
    "add_modulator",
    "update_modulator",
    "delete_modulator",
    "set_parameter",
    "set_track_mute",
    "set_tempo",
    "undo",
];
const AUDIO_REGION_SCHEMA: &str = r#"{
  "type": "object",
  "additionalProperties": false,
  "required": ["start", "end"],
  "properties": {
    "tracks": {
      "description": "Tracks to render. Omit or use \"all\" for the full mix, or provide stable track IDs from sound-graph.json to isolate selected tracks.",
      "oneOf": [
        { "type": "string", "enum": ["all"] },
        {
          "type": "array",
          "items": { "type": "integer", "minimum": 1 },
          "minItems": 1,
          "maxItems": 32,
          "uniqueItems": true
        }
      ]
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
    #[cfg(test)]
    pub(crate) fn create(
        project: &Project,
        prompt: &str,
        start: f32,
        end: f32,
    ) -> io::Result<Self> {
        Self::create_in(&session_root(), project, prompt, start, end)
    }

    pub(crate) fn create_in(
        root: &Path,
        project: &Project,
        prompt: &str,
        start: f32,
        end: f32,
    ) -> io::Result<Self> {
        let path = reserve_session_directory(root)?;
        let result = (|| {
            let project = project.clone();
            write_new(&path.join(GRAPH_FILE), &project.to_json())?;
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
                    "model": crate::gemini::GEMINI_MODEL,
                    "prompt": prompt,
                    "start": start,
                    "end": end,
                    "appliedSteps": 0,
                    "audioListens": 0,
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

    pub(crate) fn synchronize_project(&self, project: &Project) -> Result<(), String> {
        write_replace(
            &self.path.join(GRAPH_FILE),
            &format!("{}\n", project.to_json()),
        )
        .map_err(|error| format!("could not synchronize committed sound graph: {error}"))
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
        write_replace(&path, &value.to_string())
    }

    pub(crate) fn stats(&self) -> io::Result<(usize, usize)> {
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
        Ok((applied_steps, audio_listens))
    }

    pub(crate) fn finish(&self, plans: Vec<EditPlan>) -> Result<(EditPlan, Project), String> {
        let mut actions = Vec::new();
        let mut summary = None;
        for plan in plans {
            actions.push(plan.action);
            summary = Some(plan.summary);
        }
        if actions.is_empty() {
            return Err("Gemini did not use a registered graph mutation tool".to_owned());
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
        let plan = if let Some(summary) = serde_json::from_str::<JsonValue>(&plan_source)
            .ok()
            .filter(|value| value.get("graphMutation") == Some(&JsonValue::Bool(true)))
            .and_then(|value| {
                value
                    .get("summary")
                    .and_then(JsonValue::as_str)
                    .map(str::to_owned)
            }) {
            EditPlan {
                action: Action::GraphMutation,
                summary,
            }
        } else {
            return Err("Gemini edit progress did not contain a graph mutation".to_owned());
        };
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
    let audio_schema = serde_json::from_str::<JsonValue>(AUDIO_REGION_SCHEMA)
        .expect("embedded audio schema is valid JSON");
    let mut tools = vec![
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
            "name": AUDIO_TOOL_NAME,
            "description": "Optionally render all tracks (the default) or a list of model-chosen track IDs and absolute project start/end times from the latest sound graph as WAV audio. Use it whenever hearing the original or an edited result would improve your decision; you decide whether and when to listen. The listening range is independent of the selected edit scope.",
            "parameters": audio_schema
        }),
        function(
            PRESET_TOOL_NAME,
            "Search the installed Surge XT factory preset catalog. Returns stable preset IDs, names, categories, and all available categories. Use a returned ID with set_surge_preset.",
            object_schema(
                serde_json::json!({
                    "query":{"type":"string","maxLength":80,"description":"Optional case-insensitive text matched against preset ID, category, and name."},
                    "category":{"type":"string","maxLength":80,"description":"Optional exact category returned by this tool."},
                    "limit":{"type":"integer","minimum":1,"maximum":100,"description":"Maximum matching presets to return; defaults to 40."}
                }),
                &[],
            ),
        ),
    ];
    tools.extend(mutation_tool_declarations());
    tools
}

fn object_schema(properties: JsonValue, required: &[&str]) -> JsonValue {
    serde_json::json!({
        "type": "object",
        "properties": properties,
        "required": required,
        "additionalProperties": false
    })
}

fn function(name: &str, description: &str, parameters: JsonValue) -> JsonValue {
    serde_json::json!({
        "type": "function",
        "name": name,
        "description": description,
        "parameters": parameters
    })
}

fn mutation_tool_declarations() -> Vec<JsonValue> {
    let id = || serde_json::json!({"type":"integer","minimum":1});
    let role =
        || serde_json::json!({"type":"string","enum":["drums","bass","chords","lead","texture"]});
    let notes = || {
        serde_json::json!({
            "type":"array","maxItems":32,"items":{"type":"object","properties":{
                "time":{"type":"number","minimum":0,"maximum":16},
                "duration":{"type":"number","minimum":0.0625,"maximum":16},
                "pitch":{"type":"integer","minimum":0,"maximum":127},
                "velocity":{"type":"number","minimum":0.01,"maximum":1}
            },"required":["time","duration","pitch","velocity"],"additionalProperties":false}
        })
    };
    let clip_properties = || {
        serde_json::json!({
            "trackId":id(), "label":{"type":"string","minLength":1,"maxLength":64},
            "start":{"type":"number","minimum":0}, "end":{"type":"number","minimum":0},
            "loopBeats":{"type":"number","minimum":0.25,"maximum":16}, "events":notes()
        })
    };
    vec![
        function(
            "new_track",
            "Create one empty track with its required instrument and no MIDI clips. Returns stable IDs for subsequent calls.",
            object_schema(serde_json::json!({"role":role()}), &["role"]),
        ),
        function(
            "delete_track",
            "Delete one track by stable ID. Use undo if this was a mistake.",
            object_schema(serde_json::json!({"trackId":id()}), &["trackId"]),
        ),
        function(
            "set_surge_preset",
            "Load one installed Surge XT factory preset onto a track using a stable preset ID returned by list_surge_presets.",
            object_schema(
                serde_json::json!({
                    "trackId":id(),
                    "presetId":{"type":"string","minLength":1,"maxLength":200}
                }),
                &["trackId", "presetId"],
            ),
        ),
        function(
            "add_midi_clip",
            "Add a MIDI clip to one track without changing other clips.",
            object_schema(
                clip_properties(),
                &["trackId", "label", "start", "end", "loopBeats", "events"],
            ),
        ),
        function(
            "update_midi_clip",
            "Replace all fields and events of one existing MIDI clip. This changes the whole clip; to preserve material outside an edit region, keep it and add a separate regional clip, or explicitly split it into clips that preserve the surrounding material.",
            object_schema(
                {
                    let mut p = clip_properties();
                    p.as_object_mut().unwrap().insert("clipId".to_owned(), id());
                    p
                },
                &[
                    "trackId",
                    "clipId",
                    "label",
                    "start",
                    "end",
                    "loopBeats",
                    "events",
                ],
            ),
        ),
        function(
            "delete_midi_clip",
            "Delete one MIDI clip by stable track and clip IDs.",
            object_schema(
                serde_json::json!({"trackId":id(),"clipId":id()}),
                &["trackId", "clipId"],
            ),
        ),
        function(
            "add_effect",
            "Add a named effect to one track and set its mix. Returns its stable ID.",
            object_schema(
                serde_json::json!({"trackId":id(),"name":{"type":"string","enum":["Delay","Reverb 1","Phaser","Rotary Speaker","Distortion","EQ","Frequency Shifter","Conditioner","Chorus","Vocoder","Reverb 2","Flanger","Ring Modulator","Airwindows","Neuron","Graphic EQ","Resonator","CHOW","Exciter","Ensemble","Combulator","Nimbus","Tape","Treemonster","Waveshaper","Mid-Side Tool","Spring Reverb","Bonsai","Floaty Delay","Convolution"]},"mix":{"type":"number","minimum":0,"maximum":1}}),
                &["trackId", "name", "mix"],
            ),
        ),
        function(
            "update_effect",
            "Update one effect parameter by stable IDs.",
            parameter_schema("effectId"),
        ),
        function(
            "delete_effect",
            "Delete one effect by stable track and effect IDs.",
            object_schema(
                serde_json::json!({"trackId":id(),"effectId":id()}),
                &["trackId", "effectId"],
            ),
        ),
        function(
            "add_modulator",
            "Add a modulator to one track and return its stable ID.",
            object_schema(
                serde_json::json!({"trackId":id(),"target":{"type":"string","minLength":1,"maxLength":96},"shape":{"type":"string","enum":["sine","triangle","square","random","envelope"]},"rate":{"type":"number","minimum":0.01,"maximum":20},"depth":{"type":"number","minimum":0,"maximum":1}}),
                &["trackId", "target", "shape", "rate", "depth"],
            ),
        ),
        function(
            "update_modulator",
            "Update one modulator parameter by stable IDs.",
            parameter_schema("modulatorId"),
        ),
        function(
            "delete_modulator",
            "Delete one modulator by stable track and modulator IDs.",
            object_schema(
                serde_json::json!({"trackId":id(),"modulatorId":id()}),
                &["trackId", "modulatorId"],
            ),
        ),
        function(
            "set_parameter",
            "Set one instrument, effect, modulator, MIDI event, or routing parameter using stable IDs from read_sound_graph.",
            object_schema(
                serde_json::json!({"trackId":id(),"tool":{"type":"string","enum":["instrument","effect","modulator","event","routing"]},"toolId":id(),"clipId":{"type":"integer","minimum":0},"parameter":{"type":"string","minLength":1,"maxLength":64},"value":{"type":"string","minLength":1,"maxLength":96}}),
                &["trackId", "tool", "toolId", "clipId", "parameter", "value"],
            ),
        ),
        function(
            "set_track_mute",
            "Set the sole authoritative mute state of one track.",
            object_schema(
                serde_json::json!({"trackId":id(),"muted":{"type":"boolean"}}),
                &["trackId", "muted"],
            ),
        ),
        function(
            "set_tempo",
            "Set project tempo in beats per minute.",
            object_schema(
                serde_json::json!({"bpm":{"type":"integer","minimum":60,"maximum":180}}),
                &["bpm"],
            ),
        ),
        function(
            "undo",
            "Undo the most recent successful graph mutation made in this edit session.",
            object_schema(serde_json::json!({}), &[]),
        ),
    ]
}

fn parameter_schema(id_name: &str) -> JsonValue {
    object_schema(
        serde_json::json!({
            "trackId":{"type":"integer","minimum":1},
            (id_name):{"type":"integer","minimum":1},
            "parameter":{"type":"string","minLength":1,"maxLength":64},
            "value":{"type":"string","minLength":1,"maxLength":96}
        }),
        &["trackId", id_name, "parameter", "value"],
    )
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

pub(crate) fn list_surge_presets(arguments: &JsonValue) -> Result<String, String> {
    let object = arguments
        .as_object()
        .ok_or_else(|| "preset catalog arguments must be an object".to_owned())?;
    let query = object
        .get("query")
        .map(|value| {
            value
                .as_str()
                .map(str::to_lowercase)
                .ok_or_else(|| "query must be a string".to_owned())
        })
        .transpose()?;
    let category = object
        .get("category")
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "category must be a string".to_owned())
        })
        .transpose()?;
    let limit = object
        .get("limit")
        .map(|value| {
            value
                .as_u64()
                .filter(|value| (1..=100).contains(value))
                .map(|value| value as usize)
                .ok_or_else(|| "limit must be an integer from 1 through 100".to_owned())
        })
        .transpose()?
        .unwrap_or(40);
    let catalog = crate::surge_presets::catalog();
    let mut categories = catalog
        .iter()
        .map(|preset| preset.category.clone())
        .collect::<Vec<_>>();
    categories.sort();
    categories.dedup();
    let matches = catalog
        .iter()
        .filter(|preset| {
            category
                .as_ref()
                .is_none_or(|category| preset.category == *category)
                && query.as_ref().is_none_or(|query| {
                    preset.id.to_lowercase().contains(query)
                        || preset.category.to_lowercase().contains(query)
                        || preset.name.to_lowercase().contains(query)
                })
        })
        .collect::<Vec<_>>();
    let presets = matches
        .iter()
        .take(limit)
        .map(|preset| {
            serde_json::json!({
                "id":preset.id,
                "category":preset.category,
                "name":preset.name
            })
        })
        .collect::<Vec<_>>();
    Ok(serde_json::json!({
        "installed":!catalog.is_empty(),
        "total":catalog.len(),
        "matched":matches.len(),
        "returned":presets.len(),
        "categories":categories,
        "presets":presets
    })
    .to_string())
}

pub(crate) fn is_mutation_tool(name: &str) -> bool {
    MUTATION_TOOL_NAMES.contains(&name)
}

pub(crate) fn apply_agent_mutation(
    session_path: &Path,
    name: &str,
    arguments: &JsonValue,
) -> Result<String, String> {
    wait_for_progress_handoff(session_path);
    let graph_path = session_path.join(GRAPH_FILE);
    let (store, mut studio) = ProjectStore::open(graph_path)
        .map_err(|error| format!("Could not load sound-graph.json: {error}"))?;
    let original = studio.project().clone();
    let (selection_start, selection_end) = edit_selection(session_path)?;
    let object = arguments
        .as_object()
        .ok_or_else(|| "tool arguments must be an object".to_owned())?;
    let mut result_id = None;
    let summary = match name {
        "new_track" => {
            let role = required_role(object, "role")?;
            let id = studio
                .add_empty_channel(role)
                .map_err(studio_error_message)?;
            result_id = Some(id);
            format!("Created {} track {id}", role.as_str())
        }
        "delete_track" => {
            let id = required_id(object, "trackId")?;
            studio.delete_channel(id).map_err(studio_error_message)?;
            format!("Deleted track {id}")
        }
        "set_surge_preset" => {
            let track_id = required_id(object, "trackId")?;
            let preset_id = required_string(object, "presetId")?;
            if crate::surge_presets::find(preset_id).is_none() {
                return Err(format!(
                    "Surge XT factory preset is not installed: {preset_id}; use {PRESET_TOOL_NAME} to discover available preset IDs"
                ));
            }
            let instrument_id = studio
                .project()
                .tracks
                .iter()
                .find(|track| track.id == track_id)
                .map(|track| track.instrument.id)
                .ok_or_else(|| format!("track {track_id} does not exist"))?;
            studio
                .configure_sound_tool(
                    track_id,
                    "instrument",
                    instrument_id,
                    None,
                    "preset",
                    preset_id,
                )
                .map_err(studio_error_message)?;
            format!("Loaded Surge XT preset {preset_id} on track {track_id}")
        }
        "add_midi_clip" => {
            let (track_id, spec) = clip_arguments(object)?;
            validate_clip_selection(&spec, selection_start, selection_end)?;
            let id = studio
                .create_midi_clip(track_id, &spec)
                .map_err(studio_error_message)?;
            result_id = Some(id);
            format!("Added MIDI clip {id} to track {track_id}")
        }
        "update_midi_clip" => {
            let clip_id = required_id(object, "clipId")?;
            let (track_id, spec) = clip_arguments(object)?;
            studio
                .replace_midi_clip(track_id, clip_id, &spec, selection_start, selection_end)
                .map_err(studio_error_message)?;
            format!("Updated MIDI clip {clip_id} on track {track_id}")
        }
        "delete_midi_clip" => {
            let track_id = required_id(object, "trackId")?;
            let clip_id = required_id(object, "clipId")?;
            studio
                .delete_midi_clip(track_id, clip_id, selection_start, selection_end)
                .map_err(studio_error_message)?;
            format!("Deleted MIDI clip {clip_id} from track {track_id}")
        }
        "add_effect" => {
            let track_id = required_id(object, "trackId")?;
            let effect_name = required_string(object, "name")?;
            let mix = required_number(object, "mix")?;
            let effect_id = studio
                .create_effect(track_id, effect_name, mix as f32)
                .map_err(studio_error_message)?;
            result_id = Some(effect_id);
            format!("Added {effect_name} effect {effect_id} to track {track_id}")
        }
        "update_effect" => update_parameter(&mut studio, object, "effect", "effectId")?,
        "delete_effect" => {
            let track_id = required_id(object, "trackId")?;
            let effect_id = required_id(object, "effectId")?;
            studio
                .delete_effect(track_id, effect_id)
                .map_err(studio_error_message)?;
            format!("Deleted effect {effect_id} from track {track_id}")
        }
        "add_modulator" => {
            let track_id = required_id(object, "trackId")?;
            let target = required_string(object, "target")?;
            let shape = required_string(object, "shape")?;
            let rate = required_number(object, "rate")? as f32;
            let depth = required_number(object, "depth")? as f32;
            let id = studio
                .create_modulator(track_id, target, shape, rate, depth)
                .map_err(studio_error_message)?;
            result_id = Some(id);
            format!("Added modulator {id} to track {track_id}")
        }
        "update_modulator" => update_parameter(&mut studio, object, "modulator", "modulatorId")?,
        "delete_modulator" => {
            let track_id = required_id(object, "trackId")?;
            let modulator_id = required_id(object, "modulatorId")?;
            studio
                .delete_modulator(track_id, modulator_id)
                .map_err(studio_error_message)?;
            format!("Deleted modulator {modulator_id} from track {track_id}")
        }
        "set_parameter" => {
            let track_id = required_id(object, "trackId")?;
            let tool = required_string(object, "tool")?;
            let tool_id = required_id(object, "toolId")?;
            let clip_id = object
                .get("clipId")
                .and_then(JsonValue::as_u64)
                .filter(|id| *id > 0);
            let parameter = required_string(object, "parameter")?;
            let value = required_string(object, "value")?;
            studio
                .configure_sound_tool(track_id, tool, tool_id, clip_id, parameter, value)
                .map_err(studio_error_message)?;
            format!("Set {tool} {tool_id} {parameter} on track {track_id}")
        }
        "set_track_mute" => {
            let track_id = required_id(object, "trackId")?;
            let muted = object
                .get("muted")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| "muted must be a boolean".to_owned())?;
            studio
                .set_mix(track_id, None, Some(muted))
                .map_err(studio_error_message)?;
            format!("Set track {track_id} muted to {muted}")
        }
        "set_tempo" => {
            let bpm = required_id(object, "bpm")?
                .try_into()
                .map_err(|_| "bpm is out of range".to_owned())?;
            studio.set_tempo(bpm).map_err(studio_error_message)?;
            format!("Set tempo to {bpm} BPM")
        }
        "undo" => return undo_agent_mutation(session_path, &store, &original),
        _ => return Err(format!("unknown graph mutation tool: {name}")),
    };

    let undo_path = session_path.join(UNDO_GRAPH_FILE);
    let previous_undo = fs::read_to_string(&undo_path).ok();
    let transaction = (|| {
        write_replace(&undo_path, &original.to_json())
            .map_err(|error| format!("could not save undo snapshot: {error}"))?;
        store
            .save(studio.project())
            .map_err(|error| format!("Could not write sound-graph.json: {error}"))?;
        publish_progress(session_path, &plan_json(&summary), studio.project())
    })();
    if let Err(error) = transaction {
        let graph_rollback = store
            .save(&original)
            .map_err(|rollback| rollback.to_string());
        let undo_rollback = match previous_undo {
            Some(source) => write_replace(&undo_path, source.trim_end())
                .map_err(|rollback| rollback.to_string()),
            None => match fs::remove_file(&undo_path) {
                Ok(()) => Ok(()),
                Err(rollback) if rollback.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(rollback) => Err(rollback.to_string()),
            },
        };
        if let Err(rollback) = graph_rollback.and(undo_rollback) {
            return Err(format!(
                "{error}; could not restore failed mutation: {rollback}"
            ));
        }
        return Err(error);
    }
    Ok(serde_json::json!({
        "message": summary,
        "version": studio.project().version,
        "id": result_id,
        "channels": sound_tool_inventory(studio.project())
    })
    .to_string())
}

fn edit_selection(session_path: &Path) -> Result<(f32, f32), String> {
    let source = fs::read_to_string(session_path.join(REQUEST_FILE))
        .map_err(|error| format!("could not read edit request: {error}"))?;
    let request: JsonValue = serde_json::from_str(&source)
        .map_err(|error| format!("edit request was invalid: {error}"))?;
    let start = request
        .get("start")
        .and_then(JsonValue::as_f64)
        .ok_or_else(|| "edit request omitted selection start".to_owned())? as f32;
    let end = request
        .get("end")
        .and_then(JsonValue::as_f64)
        .ok_or_else(|| "edit request omitted selection end".to_owned())? as f32;
    if !start.is_finite() || !end.is_finite() || start < 0.0 || end <= start {
        return Err("edit request selection is invalid".to_owned());
    }
    Ok((start, end))
}

fn validate_clip_selection(
    spec: &MidiClipSpec,
    selection_start: f32,
    selection_end: f32,
) -> Result<(), String> {
    if spec.start < selection_start || spec.end > selection_end {
        return Err(format!(
            "MIDI clip must stay within the selected region ({selection_start}-{selection_end}s)"
        ));
    }
    Ok(())
}

fn required_id(object: &Map<String, JsonValue>, name: &str) -> Result<u64, String> {
    object
        .get(name)
        .and_then(JsonValue::as_u64)
        .filter(|value| *value > 0)
        .ok_or_else(|| format!("{name} must be a positive integer"))
}

fn required_number(object: &Map<String, JsonValue>, name: &str) -> Result<f64, String> {
    object
        .get(name)
        .and_then(JsonValue::as_f64)
        .filter(|value| value.is_finite())
        .ok_or_else(|| format!("{name} must be a finite number"))
}

fn required_string<'a>(object: &'a Map<String, JsonValue>, name: &str) -> Result<&'a str, String> {
    object
        .get(name)
        .and_then(JsonValue::as_str)
        .filter(|value| !value.trim().is_empty())
        .ok_or_else(|| format!("{name} must be a nonempty string"))
}

fn required_role(object: &Map<String, JsonValue>, name: &str) -> Result<TrackRole, String> {
    TrackRole::from_name(required_string(object, name)?)
        .ok_or_else(|| format!("{name} is not a supported track role"))
}

fn clip_arguments(object: &Map<String, JsonValue>) -> Result<(u64, MidiClipSpec), String> {
    let events = object
        .get("events")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "events must be an array".to_owned())?;
    let notes = events
        .iter()
        .map(|event| {
            let event = event
                .as_object()
                .ok_or_else(|| "each event must be an object".to_owned())?;
            let pitch = required_id_or_zero(event, "pitch")?
                .try_into()
                .map_err(|_| "pitch is out of range".to_owned())?;
            Ok(MidiNote {
                time: required_number(event, "time")? as f32,
                duration: required_number(event, "duration")? as f32,
                pitch,
                velocity: required_number(event, "velocity")? as f32,
            })
        })
        .collect::<Result<Vec<_>, String>>()?;
    Ok((
        required_id(object, "trackId")?,
        MidiClipSpec {
            label: required_string(object, "label")?.to_owned(),
            start: required_number(object, "start")? as f32,
            end: required_number(object, "end")? as f32,
            loop_beats: required_number(object, "loopBeats")? as f32,
            notes,
        },
    ))
}

fn required_id_or_zero(object: &Map<String, JsonValue>, name: &str) -> Result<u64, String> {
    object
        .get(name)
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| format!("{name} must be a nonnegative integer"))
}

fn update_parameter(
    studio: &mut Studio,
    object: &Map<String, JsonValue>,
    tool: &str,
    id_name: &str,
) -> Result<String, String> {
    let track_id = required_id(object, "trackId")?;
    let tool_id = required_id(object, id_name)?;
    let parameter = required_string(object, "parameter")?;
    let value = required_string(object, "value")?;
    studio
        .configure_sound_tool(track_id, tool, tool_id, None, parameter, value)
        .map_err(studio_error_message)?;
    Ok(format!(
        "Updated {tool} {tool_id} {parameter} on track {track_id}"
    ))
}

fn plan_json(summary: &str) -> String {
    serde_json::json!({"graphMutation":true,"summary":summary}).to_string()
}

fn undo_agent_mutation(
    session_path: &Path,
    store: &ProjectStore,
    current: &Project,
) -> Result<String, String> {
    let undo_path = session_path.join(UNDO_GRAPH_FILE);
    let source = fs::read_to_string(&undo_path).map_err(|error| {
        if error.kind() == io::ErrorKind::NotFound {
            "nothing to undo in this edit session".to_owned()
        } else {
            format!("could not read undo snapshot: {error}")
        }
    })?;
    let mut restored = Project::from_json(&source)
        .map_err(|error| format!("undo snapshot is invalid: {error}"))?;
    restored.version = current.version.saturating_add(1);
    let summary = "Undid the previous graph mutation";
    let transaction = (|| {
        store
            .save(&restored)
            .map_err(|error| format!("could not restore undo snapshot: {error}"))?;
        fs::remove_file(&undo_path)
            .map_err(|error| format!("could not consume undo snapshot: {error}"))?;
        publish_progress(session_path, &plan_json(summary), &restored)
    })();
    if let Err(error) = transaction {
        let graph_rollback = store.save(current).map_err(|rollback| rollback.to_string());
        let undo_rollback = if undo_path.exists() {
            Ok(())
        } else {
            write_replace(&undo_path, source.trim_end()).map_err(|rollback| rollback.to_string())
        };
        if let Err(rollback) = graph_rollback.and(undo_rollback) {
            return Err(format!(
                "{error}; could not restore failed undo: {rollback}"
            ));
        }
        return Err(error);
    }
    Ok(serde_json::json!({
        "message":summary,
        "version":restored.version,
        "channels":sound_tool_inventory(&restored)
    })
    .to_string())
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
        "Rendered {} from {:.3} to {:.3} seconds through the same custom Rust audio engine used for DAW playback. Listen to the audio itself and describe the audible rhythm, subdivision, energy contour, timbre, transitions, and shortcomings before deciding what to do next.",
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
    render_audio_request_with_backend(request, false)
}

pub(crate) fn render_audio_request_with_backend(
    request: AudioRenderRequest,
    builtin: bool,
) -> Result<AudioRender, String> {
    let region = if builtin {
        audio_analysis::render_region_builtin(
            &request.project,
            &request.track_ids,
            request.start,
            request.end,
        )
    } else {
        audio_analysis::render_region(
            &request.project,
            &request.track_ids,
            request.start,
            request.end,
        )
    }?;
    Ok(AudioRender {
        description: request.description,
        wav: audio_analysis::wav_bytes(&region.samples),
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
    let track_ids = match arguments.get("tracks") {
        None => project.tracks.iter().map(|track| track.id).collect(),
        Some(JsonValue::String(value)) if value == "all" => {
            project.tracks.iter().map(|track| track.id).collect()
        }
        Some(JsonValue::Array(values)) => {
            if values.is_empty() || values.len() > 32 {
                return Err("tracks must contain between 1 and 32 track IDs".to_owned());
            }
            let mut track_ids = Vec::with_capacity(values.len());
            for value in values {
                let track_id = value
                    .as_u64()
                    .filter(|track_id| *track_id > 0)
                    .ok_or_else(|| "tracks must contain positive integers".to_owned())?;
                if track_ids.contains(&track_id) {
                    return Err(format!("track {track_id} was requested more than once"));
                }
                if !project.tracks.iter().any(|track| track.id == track_id) {
                    let available = project
                        .tracks
                        .iter()
                        .map(|track| {
                            format!("{} ({}, {})", track.id, track.name, track.role.as_str())
                        })
                        .collect::<Vec<_>>()
                        .join(", ");
                    return Err(format!(
                        "track {track_id} does not exist; available track IDs: {available}"
                    ));
                }
                track_ids.push(track_id);
            }
            track_ids
        }
        Some(_) => return Err("tracks must be \"all\" or an array of track IDs".to_owned()),
    };
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

fn reserve_session_directory(root: &Path) -> io::Result<PathBuf> {
    fs::create_dir_all(root)?;
    set_private_directory(root)?;
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

#[cfg(test)]
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

#[cfg(test)]
pub(crate) fn session_summaries() -> io::Result<Vec<JsonValue>> {
    session_summaries_in(&session_root())
}

pub(crate) fn session_summaries_in(root: &Path) -> io::Result<Vec<JsonValue>> {
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

    #[test]
    fn declares_direct_graph_editing_and_audio_tools() {
        let declarations = tool_declarations();
        let names = declarations
            .iter()
            .filter_map(|tool| tool.get("name").and_then(JsonValue::as_str))
            .collect::<Vec<_>>();
        assert_eq!(
            names[0..3],
            [READ_TOOL_NAME, AUDIO_TOOL_NAME, PRESET_TOOL_NAME]
        );
        assert_eq!(&names[3..], MUTATION_TOOL_NAMES);
        assert!(
            declarations[1]["description"]
                .as_str()
                .unwrap()
                .contains("you decide whether and when to listen")
        );
    }

    #[test]
    fn persists_session_metadata_and_wav_artifacts() {
        let session =
            EditSession::create(&Project::demo(), "test the drop", 0.0, 2.0).expect("edit session");
        let rendered = render_audio(
            session.path(),
            &serde_json::json!({"tracks": [1, 2], "start": 0, "end": 1}),
        )
        .expect("audio render");
        assert_eq!(&rendered.wav[..4], b"RIFF");
        assert_eq!(&rendered.wav[8..12], b"WAVE");
        let artifact = session
            .record_audio(1, &rendered.wav)
            .expect("WAV artifact");
        assert!(session.path().join(artifact).is_file());
        session
            .update_status("completed", "Done", 2, 1)
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
    }

    #[test]
    fn crud_mutations_publish_stable_ids_and_undo_the_last_change() {
        let original = Project::demo();
        let session =
            EditSession::create(&original, "shape the bass", 4.0, 8.0).expect("edit session");
        let response = apply_agent_mutation(
            session.path(),
            "new_track",
            &serde_json::json!({"role":"lead"}),
        )
        .expect("new track");
        let response: JsonValue = serde_json::from_str(&response).unwrap();
        let track_id = response["id"].as_u64().expect("created track ID");
        let (plan, project) = session.take_update().unwrap().expect("published update");
        assert_eq!(plan.action, Action::GraphMutation);
        let track = project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .expect("created track");
        assert!(track.clips.is_empty());

        apply_agent_mutation(session.path(), "undo", &serde_json::json!({})).expect("undo");
        let (_, project) = session.take_update().unwrap().expect("published undo");
        assert_eq!(project.tracks.len(), original.tracks.len());
        assert!(!project.tracks.iter().any(|track| track.id == track_id));
    }

    #[test]
    fn factory_presets_can_be_searched_and_loaded_by_stable_id() {
        let catalog: JsonValue = serde_json::from_str(
            &list_surge_presets(&serde_json::json!({
                "query":"Flux Capacitor",
                "category":"Pads"
            }))
            .expect("preset catalog"),
        )
        .expect("catalog JSON");
        assert!(catalog["total"].as_u64().unwrap() > 100);
        assert_eq!(catalog["matched"], 1);
        assert_eq!(catalog["presets"][0]["id"], "Factory/Pads/Flux Capacitor");

        let session =
            EditSession::create(&Project::demo(), "change the patch", 0.0, 2.0).expect("session");
        apply_agent_mutation(
            session.path(),
            "set_surge_preset",
            &serde_json::json!({
                "trackId":3,
                "presetId":"Factory/Pads/Flux Capacitor"
            }),
        )
        .expect("factory preset mutation");
        let (_, project) = session.take_update().unwrap().expect("published update");
        assert_eq!(
            project.tracks[2].instrument.preset,
            "Factory/Pads/Flux Capacitor"
        );
    }

    #[test]
    fn failed_progress_publication_rolls_back_graph_and_undo_snapshot() {
        let original = Project::demo();
        let session = EditSession::create(&original, "change tempo", 0.0, 4.0).expect("session");
        let mut prior_undo = Studio::from_project(original.clone());
        prior_undo.set_tempo(90).expect("prior undo state");
        write_replace(
            &session.path().join(UNDO_GRAPH_FILE),
            &prior_undo.project().to_json(),
        )
        .expect("prior undo snapshot");
        let undo_before = fs::read_to_string(session.path().join(UNDO_GRAPH_FILE)).expect("undo");
        fs::create_dir(session.path().join(PENDING_PROGRESS_DIRECTORY))
            .expect("blocked progress handoff");

        let error =
            apply_agent_mutation(session.path(), "set_tempo", &serde_json::json!({"bpm":130}))
                .expect_err("progress publication failure");

        assert!(error.contains("could not prepare Gemini edit progress"));
        let restored = ProjectStore::open(session.path().join(GRAPH_FILE))
            .expect("restored graph")
            .1;
        assert_eq!(restored.project().to_json(), original.to_json());
        assert_eq!(
            fs::read_to_string(session.path().join(UNDO_GRAPH_FILE)).expect("restored undo"),
            undo_before
        );

        fs::create_dir(session.path().join(PENDING_PROGRESS_DIRECTORY))
            .expect("blocked undo handoff");
        let error = apply_agent_mutation(session.path(), "undo", &serde_json::json!({}))
            .expect_err("undo publication failure");
        assert!(error.contains("could not prepare Gemini edit progress"));
        let restored = ProjectStore::open(session.path().join(GRAPH_FILE))
            .expect("graph after failed undo")
            .1;
        assert_eq!(restored.project().to_json(), original.to_json());
        assert_eq!(
            fs::read_to_string(session.path().join(UNDO_GRAPH_FILE)).expect("undo after failure"),
            undo_before
        );
    }

    #[test]
    fn committed_graph_metadata_is_synchronized_before_the_next_mutation() {
        let session =
            EditSession::create(&Project::demo(), "two edits", 0.0, 8.0).expect("edit session");
        apply_agent_mutation(session.path(), "set_tempo", &serde_json::json!({"bpm":120}))
            .expect("first mutation");
        let (plan, submitted) = session.take_update().unwrap().expect("first update");

        let mut live = Studio::from_project(Project::demo());
        live.replace_graph(submitted, 0.0, 8.0, "two edits", plan)
            .expect("server commit metadata");
        session
            .synchronize_project(live.project())
            .expect("canonical synchronization");

        apply_agent_mutation(
            session.path(),
            "update_midi_clip",
            &serde_json::json!({
                "trackId":1,"clipId":11,"label":"Updated drums","start":0,"end":8,
                "loopBeats":4,"events":[
                    {"time":0,"duration":0.25,"pitch":36,"velocity":0.9}
                ]
            }),
        )
        .expect("second mutation after synchronization");
        let (_, submitted) = session.take_update().unwrap().expect("second update");
        live.replace_graph(
            submitted,
            0.0,
            8.0,
            "two edits",
            EditPlan {
                action: Action::GraphMutation,
                summary: "Updated drums".to_owned(),
            },
        )
        .expect("second server commit has no ID collision");
        Project::from_json(&live.project().to_json()).expect("committed graph validates");
        let clips = &live.project().tracks[0].clips;
        assert_eq!((clips[0].start, clips[0].end), (0.0, 8.0));
        assert_eq!((clips[1].start, clips[1].end), (8.0, 32.0));

        let error = apply_agent_mutation(
            session.path(),
            "add_midi_clip",
            &serde_json::json!({
                "trackId":1,"label":"Outside selection","start":8,"end":12,
                "loopBeats":4,"events":[]
            }),
        )
        .expect_err("MIDI outside the selected region");
        assert!(error.contains("selected region"));

        apply_agent_mutation(
            session.path(),
            "delete_midi_clip",
            &serde_json::json!({"trackId":1,"clipId":11}),
        )
        .expect("selection-scoped MIDI deletion");
        let (_, deleted) = session.take_update().unwrap().expect("delete update");
        assert!(deleted.tracks[0].clips.iter().all(|clip| clip.start >= 8.0));
    }

    #[test]
    fn mute_is_an_explicit_reversible_track_state_and_effect_delete_is_physical() {
        let session =
            EditSession::create(&Project::demo(), "edit safely", 0.0, 4.0).expect("edit session");
        apply_agent_mutation(
            session.path(),
            "set_track_mute",
            &serde_json::json!({"trackId":2,"muted":true}),
        )
        .expect("mute");
        let (_, muted) = session.take_update().unwrap().expect("mute update");
        assert!(muted.tracks[1].muted);

        apply_agent_mutation(
            session.path(),
            "set_track_mute",
            &serde_json::json!({"trackId":2,"muted":false}),
        )
        .expect("unmute");
        let (_, unmuted) = session.take_update().unwrap().expect("unmute update");
        assert!(!unmuted.tracks[1].muted);

        let response = apply_agent_mutation(
            session.path(),
            "add_effect",
            &serde_json::json!({"trackId":2,"name":"Drive","mix":0.5}),
        )
        .expect("add effect");
        let effect_id = serde_json::from_str::<JsonValue>(&response).unwrap()["id"]
            .as_u64()
            .unwrap();
        session.take_update().unwrap().expect("effect update");
        apply_agent_mutation(
            session.path(),
            "delete_effect",
            &serde_json::json!({"trackId":2,"effectId":effect_id}),
        )
        .expect("delete effect");
        let (_, deleted) = session.take_update().unwrap().expect("delete update");
        assert!(
            deleted.tracks[1]
                .effects
                .iter()
                .all(|effect| effect.id != effect_id)
        );
        assert!(!deleted.tracks[1].routing.effect_order.contains(&effect_id));
    }

    #[test]
    fn audio_render_validates_stable_channel_ids() {
        let session =
            EditSession::create(&Project::demo(), "listen", 0.0, 2.0).expect("edit session");
        let error = render_audio(
            session.path(),
            &serde_json::json!({"tracks": [999], "start": 0, "end": 1}),
        )
        .expect_err("unknown channel");
        assert!(error.contains("available track IDs"));
        assert!(error.contains("1 (Pulse Kit, drums)"));
    }

    #[test]
    fn audio_render_defaults_to_all_tracks_and_accepts_explicit_all() {
        let session =
            EditSession::create(&Project::demo(), "listen", 0.0, 2.0).expect("edit session");
        let omitted =
            prepare_audio_render(session.path(), &serde_json::json!({"start": 0, "end": 1}))
                .expect("default all-track render");
        let explicit = prepare_audio_render(
            session.path(),
            &serde_json::json!({"tracks": "all", "start": 0, "end": 1}),
        )
        .expect("explicit all-track render");
        let expected = Project::demo()
            .tracks
            .iter()
            .map(|track| track.id)
            .collect::<Vec<_>>();

        assert_eq!(omitted.track_ids, expected);
        assert_eq!(explicit.track_ids, expected);
    }

    #[test]
    fn audio_render_range_is_independent_of_the_edit_selection() {
        let session = EditSession::create(&Project::demo(), "listen in context", 8.0, 12.0)
            .expect("edit session");
        let request = prepare_audio_render(
            session.path(),
            &serde_json::json!({"tracks": [1, 2, 3], "start": 2, "end": 7}),
        )
        .expect("context render outside selection");

        assert_eq!(request.start, 2.0);
        assert_eq!(request.end, 7.0);
        assert!(request.description.contains("2.000 to 7.000 seconds"));
    }
}
