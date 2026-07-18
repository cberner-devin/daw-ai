use std::collections::HashMap;
use std::fmt;
use std::io::{Read, Write};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use crate::codex_mcp::{EditSession, MCP_SESSION_ENV, MCP_TOOL_NAME};
use crate::json::{JsonParser, JsonValue};
use crate::model::{Project, TrackRole, json_string};
use crate::prompt::{Action, EditPlan, MidiNote};

pub(crate) const EDIT_SCHEMA: &str = include_str!("../codex/edit-plan.schema.json");
const STUDIO_CONTRACT: &str = include_str!("../codex/STUDIO.md");
const CODEX_MODEL: &str = "gpt-5.6-sol";
const CODEX_REASONING: &str = "model_reasoning_effort=\"high\"";
const CODEX_TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug)]
pub enum PlannerError {
    Unavailable,
    TimedOut,
    Failed,
    InvalidOutput(String),
    Io(std::io::Error),
}

impl fmt::Display for PlannerError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Unavailable => write!(
                formatter,
                "Codex CLI is required; install Codex, run `codex login`, and try again"
            ),
            Self::TimedOut => write!(formatter, "Codex took too long to plan the edit; try again"),
            Self::Failed => write!(
                formatter,
                "Codex could not plan the edit; run `codex login status` and try again"
            ),
            Self::InvalidOutput(message) => {
                write!(formatter, "Codex returned an invalid synth edit: {message}")
            }
            Self::Io(error) => write!(formatter, "Codex integration failed: {error}"),
        }
    }
}

pub struct CodexPlanner;

pub struct CodexEdit {
    pub plan: EditPlan,
    pub project: Project,
}

impl CodexPlanner {
    pub fn interpret(
        prompt: &str,
        start: f32,
        end: f32,
        project: &Project,
    ) -> Result<CodexEdit, PlannerError> {
        let session = EditSession::create(project, prompt, start, end).map_err(PlannerError::Io)?;
        let task = planner_task(prompt, start, end);
        let executable = std::env::current_exe().map_err(PlannerError::Io)?;
        let mcp_config = format!(
            concat!(
                "mcp_servers={{daw_ai={{command={},args=[\"--mcp\"],",
                "env_vars=[\"{}\"],required=true,enabled_tools=[\"{}\"],",
                "default_tools_approval_mode=\"approve\"}}}}"
            ),
            json_string(&executable.to_string_lossy()),
            MCP_SESSION_ENV,
            MCP_TOOL_NAME
        );
        let mut child = Command::new("codex")
            .arg("exec")
            .arg("--ephemeral")
            .arg("--ignore-user-config")
            .arg("--model")
            .arg(CODEX_MODEL)
            .arg("--config")
            .arg(CODEX_REASONING)
            .arg("--config")
            .arg("approval_policy=\"never\"")
            .arg("--config")
            .arg("web_search=\"disabled\"")
            .arg("--config")
            .arg("sandbox_workspace_write.exclude_tmpdir_env_var=true")
            .arg("--config")
            .arg("sandbox_workspace_write.exclude_slash_tmp=true")
            .arg("--config")
            .arg(mcp_config)
            .arg("--skip-git-repo-check")
            .arg("--sandbox")
            .arg("workspace-write")
            .arg("--color")
            .arg("never")
            .arg("--cd")
            .arg(session.path())
            .env(MCP_SESSION_ENV, session.path())
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|error| {
                if error.kind() == std::io::ErrorKind::NotFound {
                    PlannerError::Unavailable
                } else {
                    PlannerError::Io(error)
                }
            })?;

        let mut stdin = child.stdin.take().expect("piped stdin");
        let stdin_writer =
            thread::spawn(move || stdin.write_all(task.as_bytes()).map_err(PlannerError::Io));
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");
        let stdout_reader = thread::spawn(move || read_stream(stdout));
        let stderr_reader = thread::spawn(move || read_stream(stderr));
        let started = Instant::now();
        let status = loop {
            if let Some(status) = child.try_wait().map_err(PlannerError::Io)? {
                break status;
            }
            if started.elapsed() >= CODEX_TIMEOUT {
                let _ = child.kill();
                let _ = child.wait();
                let _ = stdin_writer.join();
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(PlannerError::TimedOut);
            }
            thread::sleep(Duration::from_millis(50));
        };
        let output = stdout_reader
            .join()
            .map_err(|_| PlannerError::InvalidOutput("stdout reader stopped".to_owned()))??;
        let stderr = stderr_reader
            .join()
            .map_err(|_| PlannerError::InvalidOutput("stderr reader stopped".to_owned()))??;
        let stdin_result = stdin_writer
            .join()
            .map_err(|_| PlannerError::InvalidOutput("stdin writer stopped".to_owned()))?;
        let stderr = String::from_utf8_lossy(&stderr);
        if !status.success() {
            if !stderr.trim().is_empty() {
                eprintln!(
                    "error: Codex edit failed: {}",
                    bounded_log_text(&stderr, 4_096)
                );
            }
            return Err(PlannerError::Failed);
        }
        stdin_result?;
        String::from_utf8(output)
            .map_err(|_| PlannerError::InvalidOutput("response was not UTF-8".to_owned()))?;
        log_codex_warnings(&stderr);
        let (plan, project) = session.finish().map_err(|message| invalid(&message))?;
        Ok(CodexEdit { plan, project })
    }
}

pub fn run_mcp() -> std::io::Result<()> {
    crate::codex_mcp::run_from_environment()
}

fn planner_task(prompt: &str, start: f32, end: f32) -> String {
    format!(
        concat!(
            "You are the sound-graph editing engine inside DAW-AI. Read sound-graph.json in ",
            "the current directory before deciding what to change. First form a musical plan, ",
            "then use the registered daw_ai {tool} tool for every intended graph change. ",
            "You may work iteratively, but use no more than eight actions in total. The tool ",
            "validates each batch, updates sound-graph.json, and gives errors you must correct. ",
            "Do not stop after merely describing or manually editing the graph: at least one ",
            "successful {tool} call is required. After the graph is complete, reply briefly.\n\n",
            "{contract}\n\n",
            "Selected region: {start:.3} to {end:.3} seconds.\n",
            "User request: {prompt}\n"
        ),
        contract = STUDIO_CONTRACT,
        tool = MCP_TOOL_NAME,
        start = start,
        end = end,
        prompt = prompt,
    )
}

fn log_codex_warnings(stderr: &str) {
    for line in stderr
        .lines()
        .filter(|line| {
            let line = line.to_ascii_lowercase();
            line.contains("warning") || line.contains("error")
        })
        .take(16)
    {
        eprintln!("warning: Codex: {}", bounded_log_text(line, 1_024));
    }
}

fn bounded_log_text(value: &str, maximum: usize) -> String {
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

fn read_stream(mut stream: impl Read) -> Result<Vec<u8>, PlannerError> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).map_err(PlannerError::Io)?;
    Ok(bytes)
}

pub(crate) fn plan_from_json(source: &str) -> Result<EditPlan, PlannerError> {
    let value = JsonParser::new(source)
        .parse()
        .map_err(|error| invalid(&error.to_string()))?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("top-level response must be an object"))?;
    let summary = string_field(object, "summary")?.trim().to_owned();
    if summary.is_empty() || summary.chars().count() > 160 {
        return Err(invalid("summary length is invalid"));
    }
    let musical_plan = string_field(object, "musicalPlan")?.trim();
    if musical_plan.is_empty() || musical_plan.chars().count() > 300 {
        return Err(invalid("musical plan length is invalid"));
    }
    let actions = object
        .get("actions")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| invalid("actions must be an array"))?;
    if actions.is_empty() || actions.len() > 8 {
        return Err(invalid("one to eight actions are required"));
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
    match kind {
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
            Ok(Action::MidiClip {
                track_id: integer_field(object, "trackId")?,
                target,
                label: label.to_owned(),
                start: start as f32,
                end: end as f32,
                loop_beats: value as f32,
                notes: midi_notes_field(object, value)?,
            })
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
        "configure" if name == "None" && value == 0.0 => {
            let setting = string_field(object, "setting")?;
            if setting.is_empty() || setting.chars().count() > 64 {
                return Err(invalid("configure setting length is invalid"));
            }
            let clip_id = integer_field(object, "clipId")?;
            Ok(Action::Configure {
                track_id: integer_field(object, "trackId")?,
                target: target.ok_or_else(|| invalid("configure requires a role target"))?,
                tool: sound_tool_name(string_field(object, "tool")?)?,
                tool_id: integer_field(object, "toolId")?,
                clip_id: (clip_id != 0).then_some(clip_id),
                parameter: sound_parameter_name(string_field(object, "parameter")?)?,
                value: setting.to_owned(),
            })
        }
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
    }
}

fn midi_notes_field(
    object: &HashMap<String, JsonValue>,
    loop_beats: f64,
) -> Result<Vec<MidiNote>, PlannerError> {
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
        "instrument.attack" | "instrument.release" | "instrument.tone" | "instrument.pitch"
        | "track.volume" => Ok(name.to_owned()),
        _ if effect_modulation_target(name).is_some() => Ok(name.to_owned()),
        _ => Err(invalid("unknown modulation target")),
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
    name.strip_prefix("effect:")?
        .strip_suffix(".mix")?
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
        "attack" => Ok("attack"),
        "release" => Ok("release"),
        "tone" => Ok("tone"),
        "mix" => Ok("mix"),
        "enabled" => Ok("enabled"),
        "shape" => Ok("shape"),
        "rate" => Ok("rate"),
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

fn string_field<'a>(
    object: &'a HashMap<String, JsonValue>,
    name: &str,
) -> Result<&'a str, PlannerError> {
    object
        .get(name)
        .and_then(JsonValue::as_string)
        .ok_or_else(|| invalid(&format!("{name} must be a string")))
}

fn number_field(object: &HashMap<String, JsonValue>, name: &str) -> Result<f64, PlannerError> {
    object
        .get(name)
        .and_then(JsonValue::as_number)
        .filter(|number| number.is_finite())
        .ok_or_else(|| invalid(&format!("{name} must be a finite number")))
}

fn integer_field(object: &HashMap<String, JsonValue>, name: &str) -> Result<u64, PlannerError> {
    let value = number_field(object, name)?;
    if value.fract() == 0.0 && (0.0..=9_007_199_254_740_991.0).contains(&value) {
        Ok(value as u64)
    } else {
        Err(invalid(&format!(
            "{name} must be a non-negative safe integer"
        )))
    }
}

fn invalid(message: &str) -> PlannerError {
    PlannerError::InvalidOutput(message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_a_compound_structured_edit() {
        let plan = plan_from_json(
            r#"{
                "summary":"Warmed the chords and added space",
                "musicalPlan":"Darken the chord timbre and add a long ambient tail.",
                "actions":[
                    {"kind":"filter","target":"chords","name":"None","value":-0.3},
                    {"kind":"effect","target":"chords","name":"Reverb","value":0.42}
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
                "summary":"Route movement to the bass filter mix",
                "musicalPlan":"Add slow sine movement to the existing bass filter mix.",
                "actions":[{
                    "kind":"modulator","target":"bass","name":"effect:210.mix","value":0.25,
                    "trackId":0,"tool":"None","toolId":0,"clipId":0,"parameter":"None","setting":"sine","rate":0.5
                }]
            }"#,
        )
        .expect("valid stable-ID modulation target");
        assert_eq!(
            plan.action,
            Action::Modulator {
                parameter: "effect:210.mix".to_owned(),
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
