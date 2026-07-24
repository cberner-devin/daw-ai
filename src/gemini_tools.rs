use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value as JsonValue};

use crate::audio_analysis::{self, MAX_REGION_SECONDS};
use crate::model::{
    AudioClip, AudioClipSliceSpec, MidiClipSpec, Project, Studio, StudioError, TrackRole,
    json_string,
};
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
    "resample_audio_region",
    "slice_audio_clip",
    "delete_audio_clip",
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
            "Browse one level of the installed Surge XT factory preset hierarchy. Start at Factory, choose a child folder from the returned musical metadata, and continue until preset IDs are returned for set_surge_preset.",
            object_schema(
                serde_json::json!({
                    "path":{"type":"string","minLength":7,"maxLength":160,"description":"Exact folder path returned by a prior call. Omit to browse the Factory root."}
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
            "type":"array","maxItems":128,"description":"Loop mode supports at most 32 events; once mode supports at most 128.","items":{"type":"object","properties":{
                "time":{"type":"number","minimum":0,"maximum":64,"description":"Beat offset from the clip start."},
                "duration":{"type":"number","minimum":0.0625,"maximum":64},
                "pitch":{"type":"integer","minimum":0,"maximum":127,"description":"For a dedicated starter drum voice use only its canonical pitch: kick 36, snare 38, closedHat 42, openHat 46, crash 49. Never combine drum voices on one Surge track."},
                "velocity":{"type":"number","minimum":0.01,"maximum":1}
            },"required":["time","duration","pitch","velocity"],"additionalProperties":false}
        })
    };
    let clip_properties = || {
        serde_json::json!({
            "trackId":id(), "label":{"type":"string","minLength":1,"maxLength":64},
            "startBeat":{"type":"number","minimum":0,"description":"Absolute beat from the start of the project."},
            "durationBeats":{"type":"number","minimum":0.25,"maximum":64},
            "playback":{"description":"Default to loop for drums, bass grooves, chord accompaniment, arpeggios, and riffs. Use once mainly for melodies, fills, transitions, or material whose individual events genuinely develop without repetition.","oneOf":[
                {"type":"object","properties":{"mode":{"type":"string","enum":["loop"]},"lengthBeats":{"type":"number","minimum":0.25,"maximum":16}},"required":["mode","lengthBeats"],"additionalProperties":false},
                {"type":"object","properties":{"mode":{"type":"string","enum":["once"]}},"required":["mode"],"additionalProperties":false}
            ]},
            "events":notes()
        })
    };
    vec![
        function(
            "new_track",
            "Create one empty Surge XT instrument track with no MIDI clips. For drums, one track is one drum voice, not a General MIDI kit: choose drumVoice and create separate tracks for kick, snare, hats, and crash. Returns stable IDs for subsequent calls.",
            object_schema(
                serde_json::json!({
                    "role":role(),
                    "drumVoice":{"type":"string","enum":["kick","snare","closedHat","openHat","crash"],"description":"Only for role=drums; defaults to kick. Each drum track renders one dedicated Surge patch."}
                }),
                &["role"],
            ),
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
            "Add a beat-positioned MIDI clip without changing other clips. Default rhythmic accompaniment to a 4-, 8-, or 16-beat loop; use once mainly for melody and genuinely non-repeating fills, transitions, or development.",
            object_schema(
                clip_properties(),
                &[
                    "trackId",
                    "label",
                    "startBeat",
                    "durationBeats",
                    "playback",
                    "events",
                ],
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
                    "startBeat",
                    "durationBeats",
                    "playback",
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
            "resample_audio_region",
            "Render selected tracks into a new immutable WAV asset and place it as an audio clip. Use this before slicing, reversing, or rearranging generated material.",
            object_schema(
                serde_json::json!({
                    "sourceTracks":{"oneOf":[{"type":"string","enum":["all"]},{"type":"array","items":id(),"minItems":1,"maxItems":32,"uniqueItems":true}]},
                    "sourceStart":{"type":"number","minimum":0},
                    "sourceEnd":{"type":"number","exclusiveMinimum":0},
                    "targetTrackId":id(),
                    "destinationStart":{"type":"number","minimum":0},
                    "label":{"type":"string","minLength":1,"maxLength":64},
                    "gain":{"type":"number","minimum":0,"maximum":2},
                    "reversed":{"type":"boolean"}
                }),
                &[
                    "sourceTracks",
                    "sourceStart",
                    "sourceEnd",
                    "targetTrackId",
                    "destinationStart",
                    "label",
                    "gain",
                    "reversed",
                ],
            ),
        ),
        function(
            "slice_audio_clip",
            "Create a nondestructive slice from an existing audio clip, optionally reversed, and place it at a new project time.",
            object_schema(
                serde_json::json!({
                    "trackId":id(),"clipId":id(),
                    "sourceStart":{"type":"number","minimum":0},
                    "sourceEnd":{"type":"number","exclusiveMinimum":0},
                    "destinationStart":{"type":"number","minimum":0},
                    "label":{"type":"string","minLength":1,"maxLength":64},
                    "reversed":{"type":"boolean"}
                }),
                &[
                    "trackId",
                    "clipId",
                    "sourceStart",
                    "sourceEnd",
                    "destinationStart",
                    "label",
                    "reversed",
                ],
            ),
        ),
        function(
            "delete_audio_clip",
            "Delete one audio clip without deleting its immutable source asset.",
            object_schema(
                serde_json::json!({"trackId":id(),"clipId":id()}),
                &["trackId", "clipId"],
            ),
        ),
        function(
            "add_effect",
            "Add a named effect with renderer-independent default controls and set its mix. Returns its stable ID; read the graph afterward to discover the effect family's exact configurable parameters.",
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
    let path = object
        .get("path")
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| "path must be a string".to_owned())
        })
        .transpose()?
        .unwrap_or_else(|| "Factory".to_owned());
    let catalog = crate::surge_presets::catalog();
    let level = crate::surge_presets::browse(&catalog, &path)
        .ok_or_else(|| format!("preset folder does not exist: {path}; browse from Factory"))?;
    let folders = level
        .folders
        .iter()
        .map(|folder| {
            let (description, suggested_roles) = preset_folder_metadata(&folder.path);
            serde_json::json!({
                "name":folder.name,
                "path":folder.path,
                "presetCount":folder.preset_count,
                "description":description,
                "suggestedRoles":suggested_roles
            })
        })
        .collect::<Vec<_>>();
    let presets = level
        .presets
        .iter()
        .map(|preset| {
            serde_json::json!({
                "id":preset.id,
                "name":preset.name,
                "nameHints":preset_name_hints(&preset.name)
            })
        })
        .collect::<Vec<_>>();
    let (description, suggested_roles) = preset_folder_metadata(&level.path);
    Ok(serde_json::json!({
        "installed":!catalog.is_empty(),
        "total":catalog.len(),
        "path":level.path,
        "parent":level.parent,
        "description":description,
        "suggestedRoles":suggested_roles,
        "folders":folders,
        "presets":presets
    })
    .to_string())
}

fn preset_folder_metadata(path: &str) -> (&'static str, &'static [&'static str]) {
    let category = path
        .strip_prefix("Factory/")
        .unwrap_or(path)
        .split('/')
        .next()
        .unwrap_or(path);
    match category {
        "Basses" => (
            "Bass patches ranging from subs to harmonically rich and designed basses.",
            &["bass"],
        ),
        "Brass" => (
            "Synth and modeled brass colors for stabs, chords, and leads.",
            &["chords", "lead"],
        ),
        "Chords" => (
            "Patches designed for chordal playing and rhythmic stabs.",
            &["chords"],
        ),
        "FX" => (
            "Sound effects, transitions, atmospheres, impacts, and unusual textures.",
            &["texture"],
        ),
        "Keys" => (
            "Keyboard-like patches for harmony, riffs, and melodic parts.",
            &["chords", "lead"],
        ),
        "Leads" => (
            "Monophonic and polyphonic foreground synth voices.",
            &["lead"],
        ),
        "MPE" => (
            "Expressive patches designed for multidimensional performance.",
            &["lead", "chords", "texture"],
        ),
        "Pads" => (
            "Sustained, spacious, and evolving harmonic textures.",
            &["chords", "texture"],
        ),
        "Percussion" => (
            "Individual synthesized kicks, snares, toms, and percussion sounds.",
            &["drums"],
        ),
        "Plucks" => (
            "Short, percussive tonal patches for riffs, arpeggios, and melodies.",
            &["lead", "chords"],
        ),
        "Polysynths" => (
            "General polyphonic synthesizer patches for chords and stacked melodies.",
            &["chords", "lead"],
        ),
        "Sequences" => (
            "Rhythmic and internally animated patches that may carry their own motion.",
            &["lead", "texture"],
        ),
        "Splits" => (
            "Keyboard-split patches combining multiple timbral regions.",
            &["chords", "lead"],
        ),
        "Templates" => (
            "Sound-design starting points rather than finished role-specific patches.",
            &["bass", "chords", "lead", "texture"],
        ),
        "Tutorials" => (
            "Educational patches demonstrating Surge synthesis and modulation techniques.",
            &["texture"],
        ),
        "Vocoder" => (
            "Patches intended for vocoder-style or formant-focused sounds.",
            &["lead", "texture"],
        ),
        "Winds" => (
            "Synthesized and modeled wind-instrument colors.",
            &["lead", "chords"],
        ),
        _ => ("Installed Surge XT factory preset folders.", &[]),
    }
}

fn preset_name_hints(name: &str) -> Vec<&'static str> {
    let name = name.to_ascii_lowercase();
    [
        ("sub", "sub-bass"),
        ("dist", "distorted"),
        ("dirty", "distorted"),
        ("saw", "saw"),
        ("fm", "fm"),
        ("acid", "acid"),
        ("pluck", "pluck"),
        ("bell", "bell"),
        ("warm", "warm"),
        ("soft", "soft"),
        ("pad", "pad"),
        ("drone", "drone"),
        ("kick", "kick"),
        ("snare", "snare"),
        ("seq", "sequence"),
        ("vocal", "vocal"),
        ("choir", "choir"),
    ]
    .into_iter()
    .filter_map(|(needle, hint)| name.contains(needle).then_some(hint))
    .collect()
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
            let drum_voice = object.get("drumVoice").and_then(JsonValue::as_str);
            if role != TrackRole::Drums && drum_voice.is_some() {
                return Err("drumVoice is only valid when role is drums".to_owned());
            }
            let voice = if role == TrackRole::Drums {
                Some(drum_voice.unwrap_or("kick"))
            } else {
                None
            };
            if let Some(voice) = voice {
                let (preset, _) = drum_voice_spec(voice)?;
                let instrument_id = studio
                    .project()
                    .tracks
                    .iter()
                    .find(|track| track.id == id)
                    .map(|track| track.instrument.id)
                    .ok_or_else(|| format!("track {id} does not exist"))?;
                studio
                    .configure_sound_tool(id, "instrument", instrument_id, None, "preset", preset)
                    .map_err(studio_error_message)?;
            }
            result_id = Some(id);
            if let Some(voice) = voice {
                format!("Created drums {voice} voice track {id}")
            } else {
                format!("Created {} track {id}", role.as_str())
            }
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
            let (track_id, spec) = clip_arguments(object, studio.project().bpm)?;
            validate_clip_selection(&spec, selection_start, selection_end)?;
            validate_surge_drum_notes(studio.project(), track_id, &spec.notes)?;
            let id = studio
                .create_midi_clip(track_id, &spec)
                .map_err(studio_error_message)?;
            result_id = Some(id);
            format!("Added MIDI clip {id} to track {track_id}")
        }
        "update_midi_clip" => {
            let clip_id = required_id(object, "clipId")?;
            let (track_id, spec) = clip_arguments(object, studio.project().bpm)?;
            validate_surge_drum_notes(studio.project(), track_id, &spec.notes)?;
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
        "resample_audio_region" => {
            let track_id = required_id(object, "targetTrackId")?;
            let label = required_string(object, "label")?;
            let start = required_number(object, "destinationStart")? as f32;
            let duration = required_number(object, "sourceDuration")? as f32;
            if start < selection_start || start + duration > selection_end {
                return Err(
                    "resampled audio clip must fit inside the selected edit region".to_owned(),
                );
            }
            let gain = required_number(object, "gain")? as f32;
            let reversed = object
                .get("reversed")
                .and_then(JsonValue::as_bool)
                .ok_or_else(|| "reversed must be a boolean".to_owned())?;
            let asset = required_string(object, "asset")?;
            let id = studio
                .create_audio_clip(
                    track_id,
                    AudioClip {
                        id: 0,
                        label: label.to_owned(),
                        start,
                        end: 0.0,
                        asset: asset.to_owned(),
                        source_offset: 0.0,
                        source_duration: duration,
                        gain,
                        reversed,
                    },
                )
                .map_err(studio_error_message)?;
            result_id = Some(id);
            format!("Resampled audio clip {id} onto track {track_id}")
        }
        "slice_audio_clip" => {
            let track_id = required_id(object, "trackId")?;
            let clip_id = required_id(object, "clipId")?;
            let source_start = required_number(object, "sourceStart")? as f32;
            let source_end = required_number(object, "sourceEnd")? as f32;
            let destination_start = required_number(object, "destinationStart")? as f32;
            if destination_start < selection_start
                || destination_start + (source_end - source_start) > selection_end
            {
                return Err("audio slice must fit inside the selected edit region".to_owned());
            }
            let id = studio
                .slice_audio_clip(
                    track_id,
                    clip_id,
                    AudioClipSliceSpec {
                        label: required_string(object, "label")?,
                        source_start,
                        source_end,
                        destination_start,
                        reversed: object
                            .get("reversed")
                            .and_then(JsonValue::as_bool)
                            .ok_or_else(|| "reversed must be a boolean".to_owned())?,
                    },
                )
                .map_err(studio_error_message)?;
            result_id = Some(id);
            format!("Created audio slice {id} from clip {clip_id}")
        }
        "delete_audio_clip" => {
            let track_id = required_id(object, "trackId")?;
            let clip_id = required_id(object, "clipId")?;
            studio
                .delete_audio_clip(track_id, clip_id)
                .map_err(studio_error_message)?;
            format!("Deleted audio clip {clip_id} from track {track_id}")
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

fn drum_voice_spec(voice: &str) -> Result<(&'static str, u8), String> {
    match voice {
        "kick" => Ok(("Surge Kick", 36)),
        "snare" => Ok(("Surge Snare", 38)),
        "closedHat" => Ok(("Surge Closed Hat", 42)),
        "openHat" => Ok(("Surge Open Hat", 46)),
        "crash" => Ok(("Surge Crash", 49)),
        _ => Err("drumVoice must be kick, snare, closedHat, openHat, or crash".to_owned()),
    }
}

fn validate_surge_drum_notes(
    project: &Project,
    track_id: u64,
    notes: &[crate::prompt::MidiNote],
) -> Result<(), String> {
    let track = project
        .tracks
        .iter()
        .find(|track| track.id == track_id)
        .ok_or_else(|| format!("track {track_id} does not exist"))?;
    if track.role != TrackRole::Drums {
        return Ok(());
    }
    let expected = match track.instrument.preset.as_str() {
        "Surge Kick" => Some(36),
        "Surge Snare" => Some(38),
        "Surge Closed Hat" => Some(42),
        "Surge Open Hat" => Some(46),
        "Surge Crash" => Some(49),
        _ => None,
    };
    if let Some(expected) = expected {
        if notes.iter().any(|note| note.pitch != expected) {
            return Err(format!(
                "this Surge drum voice accepts only MIDI pitch {expected}; create a separate drums track with the matching drumVoice for other sounds"
            ));
        }
    } else {
        let mut pitches = notes.iter().map(|note| note.pitch).collect::<Vec<_>>();
        pitches.sort_unstable();
        pitches.dedup();
        if pitches.len() > 1 {
            return Err(
                "a Surge percussion preset is one pitched instrument, not a General MIDI kit; use one pitch on this track and create separate drums tracks for other voices"
                    .to_owned(),
            );
        }
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

fn clip_arguments(
    object: &Map<String, JsonValue>,
    bpm: u16,
) -> Result<(u64, MidiClipSpec), String> {
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
    let start_beat = required_number(object, "startBeat")? as f32;
    let duration_beats = required_number(object, "durationBeats")? as f32;
    let playback = object
        .get("playback")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| "playback must be an object".to_owned())?;
    let playback_mode = required_string(playback, "mode")?;
    let loop_beats = match playback_mode {
        "loop" => required_number(playback, "lengthBeats")? as f32,
        "once" => duration_beats,
        _ => return Err("playback mode must be loop or once".to_owned()),
    };
    let seconds_per_beat = 60.0 / f32::from(bpm);
    Ok((
        required_id(object, "trackId")?,
        MidiClipSpec {
            label: required_string(object, "label")?.to_owned(),
            start: start_beat * seconds_per_beat,
            end: (start_beat + duration_beats) * seconds_per_beat,
            playback_mode: playback_mode.to_owned(),
            loop_beats,
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
        "Rendered {} from {:.3} to {:.3} seconds",
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
    let backend = if builtin { "built-in" } else { "Surge XT" };
    Ok(AudioRender {
        description: format!(
            "{} using the {backend} rendering engine selected for DAW playback. Listen to the audio itself and describe the audible rhythm, subdivision, energy contour, timbre, transitions, and shortcomings before deciding what to do next.",
            request.description
        ),
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
                }).collect::<Vec<_>>(),
                "audioClips": track.audio_clips.iter().map(|clip| {
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
        let midi = declarations
            .iter()
            .find(|tool| tool["name"] == "add_midi_clip")
            .expect("MIDI clip declaration");
        assert!(
            midi["description"]
                .as_str()
                .unwrap()
                .contains("Default rhythmic accompaniment")
        );
        assert!(
            midi["parameters"]["properties"]["playback"]["description"]
                .as_str()
                .unwrap()
                .contains("Default to loop for drums")
        );
    }

    #[test]
    fn studio_contract_documents_every_registered_tool() {
        let contract = include_str!("../gemini/STUDIO.md");
        for name in [READ_TOOL_NAME, AUDIO_TOOL_NAME, PRESET_TOOL_NAME]
            .into_iter()
            .chain(MUTATION_TOOL_NAMES.iter().copied())
        {
            assert!(
                contract.contains(&format!("`{name}`")),
                "gemini/STUDIO.md does not document {name}"
            );
        }
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
    fn drum_tracks_are_dedicated_surge_voices() {
        let original = Project::initial();
        let session = EditSession::create(&original, "add hats", 0.0, 4.0).expect("edit session");
        let response = apply_agent_mutation(
            session.path(),
            "new_track",
            &serde_json::json!({"role":"drums","drumVoice":"closedHat"}),
        )
        .expect("new drum voice");
        let response: JsonValue = serde_json::from_str(&response).unwrap();
        let track_id = response["id"].as_u64().expect("created track ID");
        let (_, project) = session.take_update().unwrap().expect("published update");
        let track = project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .expect("created drum voice");
        assert_eq!(track.instrument.preset, "Surge Closed Hat");
        assert!(track.instrument.parameter_overrides.is_empty());

        let error = apply_agent_mutation(
            session.path(),
            "add_midi_clip",
            &serde_json::json!({
                "trackId":track_id,
                "label":"Invalid combined kit",
                "startBeat":0,
                "durationBeats":4,
                "playback":{"mode":"loop","lengthBeats":4},
                "events":[
                    {"time":0,"duration":0.125,"pitch":42,"velocity":0.8},
                    {"time":1,"duration":0.125,"pitch":36,"velocity":0.9}
                ]
            }),
        )
        .expect_err("combined kit must be rejected");
        assert!(error.contains("only MIDI pitch 42"), "{error}");
    }

    #[test]
    fn resampled_audio_can_be_sliced_reversed_and_rendered() {
        let original = Project::demo();
        let session =
            EditSession::create(&original, "glitch the drums", 0.0, 8.0).expect("edit session");
        let rendered = render_audio(
            session.path(),
            &serde_json::json!({"tracks":[1],"start":0,"end":2}),
        )
        .expect("source render");
        let name = session
            .record_audio(99, &rendered.wav)
            .expect("source asset");
        let asset = session.path().join(name).to_string_lossy().into_owned();
        let response = apply_agent_mutation(
            session.path(),
            "resample_audio_region",
            &serde_json::json!({
                "targetTrackId":1,"destinationStart":0,"label":"Drum resample",
                "gain":1,"reversed":false,"sourceDuration":2,"asset":asset
            }),
        )
        .expect("resample");
        let response: JsonValue = serde_json::from_str(&response).unwrap();
        let clip_id = response["id"].as_u64().expect("audio clip ID");
        session.take_update().unwrap().expect("resample update");
        apply_agent_mutation(
            session.path(),
            "slice_audio_clip",
            &serde_json::json!({
                "trackId":1,"clipId":clip_id,"sourceStart":0.5,"sourceEnd":1,
                "destinationStart":3,"label":"Reverse pull","reversed":true
            }),
        )
        .expect("slice");
        let (_, project) = session.take_update().unwrap().expect("slice update");
        assert_eq!(project.tracks[0].audio_clips.len(), 2);
        assert!(project.tracks[0].audio_clips[1].reversed);
        let audio =
            audio_analysis::render_region(&project, &[1], 0.0, 4.0).expect("audio clips render");
        assert!(audio.samples.iter().any(|sample| sample.abs() > 0.001));
    }

    #[test]
    fn factory_presets_can_be_browsed_and_loaded_by_stable_id() {
        let root: JsonValue =
            serde_json::from_str(&list_surge_presets(&serde_json::json!({})).expect("preset root"))
                .expect("root JSON");
        assert!(root["total"].as_u64().unwrap() > 100);
        assert_eq!(root["path"], "Factory");
        let pads = root["folders"]
            .as_array()
            .unwrap()
            .iter()
            .find(|folder| folder["path"] == "Factory/Pads")
            .expect("Pads folder");
        assert!(pads["presetCount"].as_u64().unwrap() > 10);
        assert_eq!(pads["suggestedRoles"][0], "chords");

        let catalog: JsonValue = serde_json::from_str(
            &list_surge_presets(&serde_json::json!({"path":"Factory/Pads"})).expect("Pads catalog"),
        )
        .expect("catalog JSON");
        assert_eq!(catalog["parent"], "Factory");
        assert!(
            catalog["presets"]
                .as_array()
                .unwrap()
                .iter()
                .any(|preset| preset["id"] == "Factory/Pads/Flux Capacitor")
        );

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
    fn midi_tools_support_repeating_patterns_and_long_once_phrases() {
        let mut studio = Studio::from_project(Project::demo());
        studio.set_tempo(120).expect("tempo");
        let session =
            EditSession::create(studio.project(), "write a melody", 0.0, 16.0).expect("session");
        let events = (0..64)
            .map(|index| {
                serde_json::json!({
                    "time":index as f32 / 2.0,
                    "duration":0.25,
                    "pitch":60 + index % 12,
                    "velocity":0.8
                })
            })
            .collect::<Vec<_>>();
        apply_agent_mutation(
            session.path(),
            "add_midi_clip",
            &serde_json::json!({
                "trackId":3,
                "label":"Sixteen-bar melody",
                "startBeat":0,
                "durationBeats":32,
                "playback":{"mode":"once"},
                "events":events
            }),
        )
        .expect("once phrase");
        let (_, project) = session.take_update().unwrap().expect("phrase update");
        let phrase = project.tracks[2].clips.last().expect("phrase clip");
        assert_eq!(phrase.playback_mode, "once");
        assert_eq!(phrase.loop_beats, 32.0);
        assert_eq!((phrase.start, phrase.end), (0.0, 16.0));
        assert_eq!(phrase.events.len(), 64);

        let loop_events = (0..33)
            .map(|index| {
                serde_json::json!({
                    "time":index as f32 / 16.0,
                    "duration":0.0625,
                    "pitch":36,
                    "velocity":0.7
                })
            })
            .collect::<Vec<_>>();
        let error = apply_agent_mutation(
            session.path(),
            "add_midi_clip",
            &serde_json::json!({
                "trackId":1,
                "label":"Oversized loop",
                "startBeat":0,
                "durationBeats":4,
                "playback":{"mode":"loop","lengthBeats":4},
                "events":loop_events
            }),
        )
        .expect_err("loop event budget");
        assert!(error.contains("outside its published range"), "{error}");
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
                "trackId":1,"clipId":11,"label":"Updated drums","startBeat":0,
                "durationBeats":16,"playback":{"mode":"loop","lengthBeats":4},"events":[
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
                "trackId":1,"label":"Outside selection","startBeat":16,
                "durationBeats":8,"playback":{"mode":"loop","lengthBeats":4},"events":[]
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
    fn audio_render_description_names_the_selected_backend() {
        let session =
            EditSession::create(&Project::demo(), "listen", 0.0, 2.0).expect("edit session");
        let arguments = serde_json::json!({"tracks":[2],"start":0,"end":0.1});
        let surge = render_audio_request_with_backend(
            prepare_audio_render(session.path(), &arguments).expect("Surge request"),
            false,
        )
        .expect("Surge render");
        let builtin = render_audio_request_with_backend(
            prepare_audio_render(session.path(), &arguments).expect("built-in request"),
            true,
        )
        .expect("built-in render");

        assert!(surge.description.contains("Surge XT rendering engine"));
        assert!(builtin.description.contains("built-in rendering engine"));
        assert!(!surge.description.contains("custom Rust audio engine"));
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
