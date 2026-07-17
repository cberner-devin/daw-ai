use std::collections::HashMap;
use std::fmt;
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::model::{Project, TrackRole};
use crate::prompt::{Action, EditPlan};

const EDIT_SCHEMA: &str = include_str!("../codex/edit-plan.schema.json");
const STUDIO_CONTRACT: &str = include_str!("../codex/STUDIO.md");
const CODEX_TIMEOUT: Duration = Duration::from_secs(90);
static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(1);

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

impl CodexPlanner {
    pub fn interpret(
        prompt: &str,
        start: f32,
        end: f32,
        project: &Project,
    ) -> Result<EditPlan, PlannerError> {
        let schema = TempSchema::create()?;
        let task = format!(
            concat!(
                "You are the planning engine inside DAW-AI. Do not edit files or run tools. ",
                "Return only a synth edit matching the provided JSON schema.\n\n",
                "{contract}\n\nCurrent project JSON:\n{project}\n\n",
                "Selected region: {start:.3} to {end:.3} seconds.\n",
                "User request: {prompt}\n"
            ),
            contract = STUDIO_CONTRACT,
            project = project.to_json(),
            start = start,
            end = end,
            prompt = prompt,
        );
        let current_directory = std::env::current_dir().map_err(PlannerError::Io)?;
        let mut child = Command::new("codex")
            .arg("exec")
            .arg("--ephemeral")
            .arg("--sandbox")
            .arg("read-only")
            .arg("--output-schema")
            .arg(&schema.path)
            .arg("--color")
            .arg("never")
            .arg("--cd")
            .arg(current_directory)
            .arg(task)
            .stdin(Stdio::null())
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
                let _ = stdout_reader.join();
                let _ = stderr_reader.join();
                return Err(PlannerError::TimedOut);
            }
            thread::sleep(Duration::from_millis(50));
        };
        let output = stdout_reader
            .join()
            .map_err(|_| PlannerError::InvalidOutput("stdout reader stopped".to_owned()))??;
        let _stderr = stderr_reader
            .join()
            .map_err(|_| PlannerError::InvalidOutput("stderr reader stopped".to_owned()))??;
        if !status.success() {
            return Err(PlannerError::Failed);
        }
        let output = String::from_utf8(output)
            .map_err(|_| PlannerError::InvalidOutput("response was not UTF-8".to_owned()))?;
        plan_from_json(&output)
    }
}

fn read_stream(mut stream: impl Read) -> Result<Vec<u8>, PlannerError> {
    let mut bytes = Vec::new();
    stream.read_to_end(&mut bytes).map_err(PlannerError::Io)?;
    Ok(bytes)
}

struct TempSchema {
    path: PathBuf,
}

impl TempSchema {
    fn create() -> Result<Self, PlannerError> {
        for _ in 0..64 {
            let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "daw-ai-edit-schema-{}-{id}.json",
                std::process::id()
            ));
            match Self::create_at(path) {
                Err(PlannerError::Io(error))
                    if error.kind() == std::io::ErrorKind::AlreadyExists =>
                {
                    continue;
                }
                result => return result,
            }
        }
        Err(PlannerError::Io(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "could not reserve a temporary schema path",
        )))
    }

    fn create_at(path: PathBuf) -> Result<Self, PlannerError> {
        // create_new atomically refuses existing files and symlinks.
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(PlannerError::Io)?;
        if let Err(error) = file.write_all(EDIT_SCHEMA.as_bytes()) {
            drop(file);
            let _ = fs::remove_file(&path);
            return Err(PlannerError::Io(error));
        }
        Ok(Self { path })
    }
}

impl Drop for TempSchema {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
    }
}

fn plan_from_json(source: &str) -> Result<EditPlan, PlannerError> {
    let value = JsonParser::new(source).parse()?;
    let object = value
        .as_object()
        .ok_or_else(|| invalid("top-level response must be an object"))?;
    let summary = string_field(object, "summary")?.trim().to_owned();
    if summary.is_empty() || summary.chars().count() > 160 {
        return Err(invalid("summary length is invalid"));
    }
    let actions = object
        .get("actions")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| invalid("actions must be an array"))?;
    if actions.is_empty() || actions.len() > 4 {
        return Err(invalid("one to four actions are required"));
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
        "drop" if target.is_none() => Ok(Action::Drop),
        "add-track" => target
            .map(|role| Action::AddTrack { role })
            .ok_or_else(|| invalid("add-track requires a role target")),
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

fn invalid(message: &str) -> PlannerError {
    PlannerError::InvalidOutput(message.to_owned())
}

#[derive(Debug)]
enum JsonValue {
    Null,
    Bool,
    Number(f64),
    String(String),
    Array(Vec<JsonValue>),
    Object(HashMap<String, JsonValue>),
}

impl JsonValue {
    fn as_object(&self) -> Option<&HashMap<String, Self>> {
        if let Self::Object(value) = self {
            Some(value)
        } else {
            None
        }
    }

    fn as_array(&self) -> Option<&[Self]> {
        if let Self::Array(value) = self {
            Some(value)
        } else {
            None
        }
    }

    fn as_string(&self) -> Option<&str> {
        if let Self::String(value) = self {
            Some(value)
        } else {
            None
        }
    }

    const fn as_number(&self) -> Option<f64> {
        if let Self::Number(value) = self {
            Some(*value)
        } else {
            None
        }
    }
}

struct JsonParser<'a> {
    bytes: &'a [u8],
    index: usize,
}

impl<'a> JsonParser<'a> {
    const fn new(source: &'a str) -> Self {
        Self {
            bytes: source.as_bytes(),
            index: 0,
        }
    }

    fn parse(mut self) -> Result<JsonValue, PlannerError> {
        let value = self.parse_value()?;
        self.skip_whitespace();
        if self.index != self.bytes.len() {
            return Err(invalid("unexpected content after JSON response"));
        }
        Ok(value)
    }

    fn parse_value(&mut self) -> Result<JsonValue, PlannerError> {
        self.skip_whitespace();
        match self.bytes.get(self.index) {
            Some(b'{') => self.parse_object(),
            Some(b'[') => self.parse_array(),
            Some(b'"') => self.parse_string().map(JsonValue::String),
            Some(b't') => self.parse_literal(b"true", JsonValue::Bool),
            Some(b'f') => self.parse_literal(b"false", JsonValue::Bool),
            Some(b'n') => self.parse_literal(b"null", JsonValue::Null),
            Some(b'-' | b'0'..=b'9') => self.parse_number(),
            _ => Err(invalid("invalid JSON value")),
        }
    }

    fn parse_object(&mut self) -> Result<JsonValue, PlannerError> {
        self.index += 1;
        let mut object = HashMap::new();
        loop {
            self.skip_whitespace();
            if self.consume(b'}') {
                return Ok(JsonValue::Object(object));
            }
            let key = self.parse_string()?;
            self.skip_whitespace();
            if !self.consume(b':') {
                return Err(invalid("missing colon in JSON object"));
            }
            let value = self.parse_value()?;
            object.insert(key, value);
            self.skip_whitespace();
            if self.consume(b'}') {
                return Ok(JsonValue::Object(object));
            }
            if !self.consume(b',') {
                return Err(invalid("missing comma in JSON object"));
            }
        }
    }

    fn parse_array(&mut self) -> Result<JsonValue, PlannerError> {
        self.index += 1;
        let mut values = Vec::new();
        loop {
            self.skip_whitespace();
            if self.consume(b']') {
                return Ok(JsonValue::Array(values));
            }
            values.push(self.parse_value()?);
            self.skip_whitespace();
            if self.consume(b']') {
                return Ok(JsonValue::Array(values));
            }
            if !self.consume(b',') {
                return Err(invalid("missing comma in JSON array"));
            }
        }
    }

    fn parse_string(&mut self) -> Result<String, PlannerError> {
        if !self.consume(b'"') {
            return Err(invalid("expected JSON string"));
        }
        let mut output = Vec::new();
        while let Some(byte) = self.bytes.get(self.index).copied() {
            self.index += 1;
            match byte {
                b'"' => {
                    return String::from_utf8(output)
                        .map_err(|_| invalid("JSON string was not valid UTF-8"));
                }
                b'\\' => self.parse_escape(&mut output)?,
                0..=31 => return Err(invalid("control character in JSON string")),
                _ => output.push(byte),
            }
        }
        Err(invalid("unterminated JSON string"))
    }

    fn parse_escape(&mut self, output: &mut Vec<u8>) -> Result<(), PlannerError> {
        let Some(escaped) = self.bytes.get(self.index).copied() else {
            return Err(invalid("unterminated JSON escape"));
        };
        self.index += 1;
        match escaped {
            b'"' | b'\\' | b'/' => output.push(escaped),
            b'b' => output.push(8),
            b'f' => output.push(12),
            b'n' => output.push(b'\n'),
            b'r' => output.push(b'\r'),
            b't' => output.push(b'\t'),
            b'u' => {
                let character = self.parse_unicode_escape()?;
                let mut bytes = [0_u8; 4];
                output.extend_from_slice(character.encode_utf8(&mut bytes).as_bytes());
            }
            _ => return Err(invalid("invalid JSON escape")),
        }
        Ok(())
    }

    fn parse_unicode_escape(&mut self) -> Result<char, PlannerError> {
        let first = self.parse_hex_codepoint()?;
        let codepoint = match first {
            0xD800..=0xDBFF => {
                if !self.consume(b'\\') || !self.consume(b'u') {
                    return Err(invalid("unpaired high surrogate in JSON string"));
                }
                let second = self.parse_hex_codepoint()?;
                if !(0xDC00..=0xDFFF).contains(&second) {
                    return Err(invalid("unpaired high surrogate in JSON string"));
                }
                0x1_0000 + ((first - 0xD800) << 10) + (second - 0xDC00)
            }
            0xDC00..=0xDFFF => return Err(invalid("unpaired low surrogate in JSON string")),
            _ => first,
        };
        char::from_u32(codepoint).ok_or_else(|| invalid("invalid Unicode escape in JSON string"))
    }

    fn parse_hex_codepoint(&mut self) -> Result<u32, PlannerError> {
        if self.index + 4 > self.bytes.len() {
            return Err(invalid("short Unicode escape in JSON string"));
        }
        let mut value = 0_u32;
        for byte in &self.bytes[self.index..self.index + 4] {
            value = value * 16
                + match byte {
                    b'0'..=b'9' => u32::from(byte - b'0'),
                    b'a'..=b'f' => u32::from(byte - b'a' + 10),
                    b'A'..=b'F' => u32::from(byte - b'A' + 10),
                    _ => return Err(invalid("invalid Unicode escape in JSON string")),
                };
        }
        self.index += 4;
        Ok(value)
    }

    fn parse_number(&mut self) -> Result<JsonValue, PlannerError> {
        let start = self.index;
        while matches!(
            self.bytes.get(self.index),
            Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')
        ) {
            self.index += 1;
        }
        let number = std::str::from_utf8(&self.bytes[start..self.index])
            .ok()
            .and_then(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .ok_or_else(|| invalid("invalid JSON number"))?;
        Ok(JsonValue::Number(number))
    }

    fn parse_literal(
        &mut self,
        literal: &[u8],
        value: JsonValue,
    ) -> Result<JsonValue, PlannerError> {
        if self.bytes.get(self.index..self.index + literal.len()) == Some(literal) {
            self.index += literal.len();
            Ok(value)
        } else {
            Err(invalid("invalid JSON literal"))
        }
    }

    fn skip_whitespace(&mut self) {
        while matches!(
            self.bytes.get(self.index),
            Some(b' ' | b'\n' | b'\r' | b'\t')
        ) {
            self.index += 1;
        }
    }

    fn consume(&mut self, byte: u8) -> bool {
        if self.bytes.get(self.index) == Some(&byte) {
            self.index += 1;
            true
        } else {
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn temporary_schema_creation_does_not_follow_symlinks() {
        use std::os::unix::fs::symlink;

        let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
        let target =
            std::env::temp_dir().join(format!("daw-ai-schema-target-{}-{id}", std::process::id()));
        let link =
            std::env::temp_dir().join(format!("daw-ai-schema-link-{}-{id}", std::process::id()));
        fs::write(&target, "preserve this file").expect("writable test target");
        symlink(&target, &link).expect("creatable test symlink");

        let error = match TempSchema::create_at(link.clone()) {
            Ok(_) => panic!("an existing symlink must not be opened"),
            Err(PlannerError::Io(error)) => error,
            Err(error) => panic!("unexpected error: {error}"),
        };
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(
            fs::read_to_string(&target).expect("readable test target"),
            "preserve this file"
        );

        fs::remove_file(link).expect("removable test symlink");
        fs::remove_file(target).expect("removable test target");
    }

    #[test]
    fn parses_a_compound_structured_edit() {
        let plan = plan_from_json(
            r#"{
                "summary":"Warmed the chords and added space",
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
    fn rejects_invalid_structured_edits() {
        let invalid = r#"{
            "summary":"Unsafe tempo",
            "actions":[{"kind":"tempo","target":"all","name":"None","value":999}]
        }"#;
        assert!(plan_from_json(invalid).is_err());
    }

    #[test]
    fn decodes_json_surrogate_pairs_and_rejects_unpaired_surrogates() {
        let valid = r#"{
            "summary":"Added sparkle \uD83C\uDFB6",
            "actions":[{"kind":"filter","target":"chords","name":"None","value":0.2}]
        }"#;
        assert_eq!(
            plan_from_json(valid).expect("valid surrogate pair").summary,
            "Added sparkle \u{1F3B6}"
        );

        for summary in [r#""Bad \uD83C text""#, r#""Bad \uDFB6 text""#] {
            let invalid = format!(
                r#"{{"summary":{summary},"actions":[{{"kind":"filter","target":"chords","name":"None","value":0.2}}]}}"#
            );
            assert!(plan_from_json(&invalid).is_err());
        }
    }
}
