use std::fmt::{self, Write};

use crate::prompt::{Action, PromptEngine};

const HISTORY_LIMIT: usize = 50;
const TRACK_LIMIT: usize = 128;
pub(crate) const EDIT_LOG_LIMIT: usize = 256;
pub(crate) const MAX_PROMPT_CHARACTERS: usize = 2_000;
pub(crate) const FILTER_CUTOFF_MIN_HZ: f32 = 80.0;
pub(crate) const FILTER_CUTOFF_MAX_HZ: f32 = 16_000.0;
pub(crate) const FILTER_CUTOFF_DEFAULT_HZ: f32 = 1_200.0;
pub(crate) const FILTER_RESONANCE_MIN: f32 = 0.1;
pub(crate) const FILTER_RESONANCE_MAX: f32 = 20.0;
pub(crate) const FILTER_RESONANCE_DEFAULT: f32 = 0.7;
pub(crate) const SURGE_ENGINE: &str = "Surge XT";
pub(crate) const SURGE_PRESETS: &[&str] = &[
    "Init",
    "Surge Percussion",
    "Surge Bass",
    "Surge Pad",
    "Surge Lead",
    "Surge Atmosphere",
];

struct ModulationTarget {
    id: String,
    label: String,
    minimum: f32,
    maximum: f32,
    scale: f32,
    mode: &'static str,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrackRole {
    Drums,
    Bass,
    Chords,
    Lead,
    Texture,
}

impl TrackRole {
    #[must_use]
    pub fn from_name(value: &str) -> Option<Self> {
        match value {
            "drums" => Some(Self::Drums),
            "bass" => Some(Self::Bass),
            "chords" => Some(Self::Chords),
            "lead" => Some(Self::Lead),
            "texture" => Some(Self::Texture),
            _ => None,
        }
    }

    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Drums => "drums",
            Self::Bass => "bass",
            Self::Chords => "chords",
            Self::Lead => "lead",
            Self::Texture => "texture",
        }
    }

    #[must_use]
    pub const fn display_name(self) -> &'static str {
        match self {
            Self::Drums => "drums",
            Self::Bass => "bass",
            Self::Chords => "chords",
            Self::Lead => "lead synth",
            Self::Texture => "texture",
        }
    }
}

pub(crate) const fn legacy_filter_cutoff_hz(role: TrackRole) -> f32 {
    match role {
        TrackRole::Drums => 3_150.0,
        TrackRole::Bass => 420.0,
        TrackRole::Chords => 980.0,
        TrackRole::Lead => 1_260.0,
        TrackRole::Texture => 1_470.0,
    }
}

#[derive(Clone, Debug)]
pub struct Effect {
    pub id: u64,
    pub name: String,
    pub mix: f32,
    pub cutoff_hz: Option<f32>,
    pub resonance: Option<f32>,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct Instrument {
    pub id: u64,
    pub engine: String,
    pub preset: String,
    pub attack: f32,
    pub release: f32,
    pub cutoff: f32,
    pub resonance: f32,
    pub pitch: f32,
}

#[derive(Clone, Debug)]
pub struct ClipEvent {
    pub id: u64,
    pub kind: String,
    pub time: f32,
    pub duration: f32,
    pub pitch: u8,
    pub velocity: f32,
}

#[derive(Clone, Debug)]
pub struct Clip {
    pub id: u64,
    pub label: String,
    pub start: f32,
    pub end: f32,
    pub source_start: f32,
    pub style: String,
    pub loop_beats: f32,
    pub events: Vec<ClipEvent>,
}

#[derive(Clone, Debug)]
pub struct Modulator {
    pub id: u64,
    pub name: String,
    pub shape: String,
    pub rate: f32,
    pub rate_mode: String,
    pub trigger: String,
    pub depth: f32,
    pub target: String,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct Routing {
    pub effect_order: Vec<u64>,
    pub output: String,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub id: u64,
    pub name: String,
    pub role: TrackRole,
    pub color: String,
    pub volume: f32,
    pub muted: bool,
    pub instrument: Instrument,
    pub effects: Vec<Effect>,
    pub modulators: Vec<Modulator>,
    pub routing: Routing,
    pub clips: Vec<Clip>,
}

#[derive(Clone, Debug)]
pub struct Edit {
    pub id: u64,
    pub operation_id: Option<String>,
    pub start: f32,
    pub end: f32,
    pub prompt: String,
    pub summary: String,
    pub action: Action,
}

#[derive(Clone, Debug)]
pub struct EditOperation {
    pub operation_id: String,
    pub completed: bool,
    pub applied_steps: usize,
    pub project_version: u64,
    pub message: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelOperationAction {
    Add,
    Delete,
}

#[derive(Clone, Debug)]
pub struct ChannelOperation {
    pub operation_id: String,
    pub action: ChannelOperationAction,
    pub track_id: u64,
    pub role: Option<TrackRole>,
    pub project_version: u64,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub name: String,
    pub bpm: u16,
    pub duration: f32,
    pub version: u64,
    pub tracks: Vec<Track>,
    pub edits: Vec<Edit>,
    pub edit_operations: Vec<EditOperation>,
    pub channel_operations: Vec<ChannelOperation>,
}

#[derive(Debug, PartialEq)]
pub struct ProjectFileError(String);

impl ProjectFileError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self(message.into())
    }
}

impl fmt::Display for ProjectFileError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Project {
    #[must_use]
    pub fn demo() -> Self {
        Self {
            name: "Neon First Light".to_owned(),
            bpm: 112,
            duration: 32.0,
            version: 1,
            tracks: vec![
                demo_track(1, TrackRole::Drums, "Pulse Kit", "#ffb86b"),
                demo_track(2, TrackRole::Bass, "Soft Current", "#74e0bc"),
                demo_track(3, TrackRole::Chords, "Glass Chords", "#8ca9ff"),
            ],
            edits: Vec::new(),
            edit_operations: Vec::new(),
            channel_operations: Vec::new(),
        }
    }

    pub fn from_json(source: &str) -> Result<Self, ProjectFileError> {
        crate::project_file::parse_project(source)
    }

    fn highest_id(&self) -> u64 {
        let mut highest = self
            .edits
            .iter()
            .map(|edit| edit.id)
            .chain(
                self.channel_operations
                    .iter()
                    .map(|operation| operation.track_id),
            )
            .max()
            .unwrap_or(0);
        for track in &self.tracks {
            highest = highest.max(track.id).max(track.instrument.id);
            for effect in &track.effects {
                highest = highest.max(effect.id);
            }
            for modulator in &track.modulators {
                highest = highest.max(modulator.id);
            }
            for clip in &track.clips {
                highest = highest.max(clip.id);
                for event in &clip.events {
                    highest = highest.max(event.id);
                }
            }
        }
        highest
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        let mut output = String::with_capacity(4096);
        self.write_graph_json(&mut output);
        output.push_str(",\"edits\":[");
        for (index, edit) in self.edits.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            edit.write_json(&mut output);
        }
        output.push_str("],\"editOperations\":[");
        for (index, operation) in self.edit_operations.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            operation.write_json(&mut output);
        }
        output.push_str("],\"channelOperations\":[");
        for (index, operation) in self.channel_operations.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            operation.write_json(&mut output);
        }
        output.push_str("]}");
        output
    }

    #[must_use]
    pub(crate) fn planner_json(&self) -> String {
        let mut output = String::with_capacity(4096);
        self.write_graph_json(&mut output);
        output.push_str(",\"regionalEdits\":[");
        let regional_count = self
            .edits
            .iter()
            .filter(|edit| edit.action.has_regional_state())
            .count();
        for (index, edit) in self
            .edits
            .iter()
            .filter(|edit| edit.action.has_regional_state())
            .skip(regional_count.saturating_sub(EDIT_LOG_LIMIT))
            .enumerate()
        {
            if index > 0 {
                output.push(',');
            }
            edit.write_regional_json(&mut output);
        }
        output.push_str("]}");
        output
    }

    fn compact_edit_log(&mut self) {
        let mut excess = self.edits.len().saturating_sub(EDIT_LOG_LIMIT);
        self.edits.retain(|edit| {
            if excess > 0 && !edit.action.has_regional_state() {
                excess -= 1;
                false
            } else {
                true
            }
        });
        if excess > 0 {
            self.edits.drain(..excess);
        }
        if self.edit_operations.len() > EDIT_LOG_LIMIT {
            self.edit_operations
                .drain(..self.edit_operations.len() - EDIT_LOG_LIMIT);
        }
        if self.channel_operations.len() > EDIT_LOG_LIMIT {
            self.channel_operations
                .drain(..self.channel_operations.len() - EDIT_LOG_LIMIT);
        }
    }

    fn write_graph_json(&self, output: &mut String) {
        write!(
            output,
            "{{\"name\":{},\"bpm\":{},\"duration\":{},\"version\":{},\"tracks\":[",
            json_string(&self.name),
            self.bpm,
            decimal(self.duration),
            self.version
        )
        .expect("writing to a string cannot fail");

        for (index, track) in self.tracks.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            track.write_json(output);
        }
        output.push(']');
    }
}

impl Track {
    fn write_json(&self, output: &mut String) {
        write!(
            output,
            concat!(
                "{{\"id\":{},\"name\":{},\"role\":{},\"color\":{},",
                "\"volume\":{},\"muted\":{},\"instrument\":{{",
                "\"id\":{},\"type\":\"instrument\",\"engine\":{},\"preset\":{},",
                "\"parameters\":{{\"attack\":{},\"release\":{},",
                "\"cutoff\":{},\"resonance\":{},\"pitch\":{}}}}},\"effects\":["
            ),
            self.id,
            json_string(&self.name),
            json_string(self.role.as_str()),
            json_string(&self.color),
            decimal(self.volume),
            self.muted,
            self.instrument.id,
            json_string(&self.instrument.engine),
            json_string(&self.instrument.preset),
            decimal(self.instrument.attack),
            decimal(self.instrument.release),
            decimal(self.instrument.cutoff),
            decimal(self.instrument.resonance),
            decimal(self.instrument.pitch)
        )
        .expect("writing to a string cannot fail");

        for (index, effect) in self.effects.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                concat!(
                    "{{\"id\":{},\"type\":\"effect\",\"name\":{},",
                    "\"enabled\":{},\"parameters\":{{\"mix\":{}"
                ),
                effect.id,
                json_string(&effect.name),
                effect.enabled,
                decimal(effect.mix)
            )
            .expect("writing to a string cannot fail");
            if let Some(cutoff_hz) = effect.cutoff_hz {
                write!(output, ",\"cutoff\":{}", decimal(cutoff_hz))
                    .expect("writing to a string cannot fail");
            }
            if let Some(resonance) = effect.resonance {
                write!(output, ",\"resonance\":{}", decimal(resonance))
                    .expect("writing to a string cannot fail");
            }
            output.push_str("}}");
        }

        output.push_str("],\"modulators\":[");
        for (index, modulator) in self.modulators.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                concat!(
                    "{{\"id\":{},\"type\":\"modulator\",\"name\":{},",
                    "\"shape\":{},\"enabled\":{},\"target\":{},\"rateMode\":{},",
                    "\"trigger\":{},",
                    "\"parameters\":{{\"rate\":{},\"depth\":{}}}}}"
                ),
                modulator.id,
                json_string(&modulator.name),
                json_string(&modulator.shape),
                modulator.enabled,
                json_string(&modulator.target),
                json_string(&modulator.rate_mode),
                json_string(&modulator.trigger),
                decimal(modulator.rate),
                decimal(modulator.depth)
            )
            .expect("writing to a string cannot fail");
        }

        output.push_str("],\"modulationTargets\":[");
        for (index, target) in modulation_targets(self).iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                concat!(
                    "{{\"id\":{},\"label\":{},\"minimum\":{},",
                    "\"maximum\":{},\"scale\":{},\"mode\":{}}}"
                ),
                json_string(&target.id),
                json_string(&target.label),
                decimal(target.minimum),
                decimal(target.maximum),
                decimal(target.scale),
                json_string(target.mode)
            )
            .expect("writing to a string cannot fail");
        }

        output.push_str("],\"automationTargets\":[");
        for (index, target) in automation_targets(self).iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                concat!(
                    "{{\"id\":{},\"label\":{},\"minimum\":{},",
                    "\"maximum\":{},\"scale\":{},\"mode\":{}}}"
                ),
                json_string(&target.id),
                json_string(&target.label),
                decimal(target.minimum),
                decimal(target.maximum),
                decimal(target.scale),
                json_string(target.mode)
            )
            .expect("writing to a string cannot fail");
        }

        output.push_str("],\"routing\":{\"audio\":[");
        write!(
            output,
            "{},{}",
            json_string("clips"),
            json_string(&format!("instrument:{}", self.instrument.id))
        )
        .expect("writing to a string cannot fail");
        for effect_id in &self.routing.effect_order {
            write!(output, ",{}", json_string(&format!("effect:{effect_id}")))
                .expect("writing to a string cannot fail");
        }
        write!(
            output,
            ",{}],\"control\":[",
            json_string(&self.routing.output)
        )
        .expect("writing to a string cannot fail");
        for (index, modulator) in self
            .modulators
            .iter()
            .filter(|modulator| modulator.enabled)
            .enumerate()
        {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                "{{\"source\":{},\"target\":{}}}",
                json_string(&format!("modulator:{}", modulator.id)),
                json_string(&modulator.target)
            )
            .expect("writing to a string cannot fail");
        }
        output.push_str("],\"output\":");
        output.push_str(&json_string(&self.routing.output));
        output.push_str(",\"edges\":[");
        let instrument = format!("instrument:{}", self.instrument.id);
        write_signal_edge(output, false, "clips", &instrument, "midi");
        for modulator in self
            .modulators
            .iter()
            .filter(|modulator| modulator.enabled && modulator.trigger == "midi")
        {
            write_signal_edge(
                output,
                true,
                "clips",
                &format!("modulator:{}", modulator.id),
                "midi",
            );
        }
        let mut audio_source = instrument;
        for effect_id in &self.routing.effect_order {
            let effect = format!("effect:{effect_id}");
            write_signal_edge(output, true, &audio_source, &effect, "audio");
            audio_source = effect;
        }
        write_signal_edge(output, true, &audio_source, &self.routing.output, "audio");
        for modulator in self.modulators.iter().filter(|modulator| modulator.enabled) {
            write_signal_edge(
                output,
                true,
                &format!("modulator:{}", modulator.id),
                &modulator.target,
                "control",
            );
        }
        output.push_str("]},\"clips\":[");
        for (index, clip) in self.clips.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                concat!(
                    "{{\"id\":{},\"label\":{},\"start\":{},\"end\":{},\"sourceStart\":{},",
                    "\"style\":{},\"loopBeats\":{},\"events\":["
                ),
                clip.id,
                json_string(&clip.label),
                decimal(clip.start),
                decimal(clip.end),
                decimal(clip.source_start),
                json_string(&clip.style),
                decimal(clip.loop_beats)
            )
            .expect("writing to a string cannot fail");
            for (event_index, event) in clip.events.iter().enumerate() {
                if event_index > 0 {
                    output.push(',');
                }
                write!(
                    output,
                    concat!(
                        "{{\"id\":{},\"type\":{},\"time\":{},\"duration\":{},",
                        "\"pitch\":{},\"velocity\":{}}}"
                    ),
                    event.id,
                    json_string(&event.kind),
                    decimal(event.time),
                    decimal(event.duration),
                    event.pitch,
                    decimal(event.velocity)
                )
                .expect("writing to a string cannot fail");
            }
            output.push_str("]}");
        }
        output.push_str("]}");
    }
}

impl Edit {
    fn write_json(&self, output: &mut String) {
        write!(output, "{{\"id\":{}", self.id).expect("writing to a string cannot fail");
        if let Some(operation_id) = &self.operation_id {
            write!(output, ",\"operationId\":{}", json_string(operation_id))
                .expect("writing to a string cannot fail");
        }
        write!(
            output,
            concat!(
                ",\"start\":{},\"end\":{},\"prompt\":{},",
                "\"summary\":{},\"action\":"
            ),
            decimal(self.start),
            decimal(self.end),
            json_string(&self.prompt),
            json_string(&self.summary)
        )
        .expect("writing to a string cannot fail");
        self.action.write_json(output);
        output.push('}');
    }

    fn write_regional_json(&self, output: &mut String) {
        write!(
            output,
            "{{\"start\":{},\"end\":{},\"action\":",
            decimal(self.start),
            decimal(self.end)
        )
        .expect("writing to a string cannot fail");
        self.action.write_regional_json(output);
        output.push('}');
    }
}

impl EditOperation {
    fn write_json(&self, output: &mut String) {
        write!(
            output,
            concat!(
                "{{\"operationId\":{},\"status\":{},\"appliedSteps\":{},",
                "\"projectVersion\":{},\"message\":{}}}"
            ),
            json_string(&self.operation_id),
            json_string(if self.completed {
                "completed"
            } else {
                "partial"
            }),
            self.applied_steps,
            self.project_version,
            json_string(&self.message)
        )
        .expect("writing to a string cannot fail");
    }
}

impl ChannelOperation {
    fn write_json(&self, output: &mut String) {
        let action = match self.action {
            ChannelOperationAction::Add => "add",
            ChannelOperationAction::Delete => "delete",
        };
        write!(
            output,
            concat!(
                "{{\"operationId\":{},\"action\":{},\"trackId\":{},",
                "\"role\":{},\"projectVersion\":{}}}"
            ),
            json_string(&self.operation_id),
            json_string(action),
            self.track_id,
            self.role
                .map_or_else(|| "null".to_owned(), |role| json_string(role.as_str())),
            self.project_version
        )
        .expect("writing to a string cannot fail");
    }
}

impl Action {
    fn retain_after_track_deletion(&mut self, track_id: u64) -> bool {
        match self {
            Self::Compound { actions } => {
                actions.retain_mut(|action| action.retain_after_track_deletion(track_id));
                !actions.is_empty()
            }
            Self::Timed { action, .. } => action.retain_after_track_deletion(track_id),
            Self::Automation {
                track_id: owner_id, ..
            } => *owner_id != track_id,
            _ => true,
        }
    }

    fn has_regional_state(&self) -> bool {
        match self {
            Self::Compound { actions } => actions.iter().any(Self::has_regional_state),
            Self::Timed { action, .. } => action.has_regional_state(),
            Self::Gain { .. }
            | Self::Mute { .. }
            | Self::Automation { .. }
            | Self::Effect { .. }
            | Self::RemoveEffect { .. }
            | Self::Filter { .. }
            | Self::Rhythm { .. } => true,
            Self::MidiClip { .. }
            | Self::AddTrack { .. }
            | Self::Instrument { .. }
            | Self::Modulator { .. }
            | Self::Configure { .. }
            | Self::Tempo { .. } => false,
        }
    }

    fn write_regional_json(&self, output: &mut String) {
        if let Self::Compound { actions } = self {
            output.push_str("{\"type\":\"compound\",\"actions\":[");
            for (index, action) in actions
                .iter()
                .filter(|action| action.has_regional_state())
                .enumerate()
            {
                if index > 0 {
                    output.push(',');
                }
                action.write_regional_json(output);
            }
            output.push_str("]}");
        } else {
            debug_assert!(self.has_regional_state());
            self.write_json(output);
        }
    }

    fn write_json(&self, output: &mut String) {
        match self {
            Self::Compound { actions } => {
                output.push_str("{\"type\":\"compound\",\"actions\":[");
                for (index, action) in actions.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    action.write_json(output);
                }
                output.push_str("]}");
                return;
            }
            Self::Timed { start, end, action } => {
                write!(
                    output,
                    "{{\"type\":\"timed\",\"start\":{},\"end\":{},\"action\":",
                    decimal(*start),
                    decimal(*end)
                )
                .expect("writing to a string cannot fail");
                action.write_json(output);
                output.push('}');
                return;
            }
            Self::Gain { amount, target } => write!(
                output,
                "{{\"type\":\"gain\",\"value\":{},\"target\":{}}}",
                decimal(*amount),
                role_json(*target)
            ),
            Self::Mute { target } => write!(
                output,
                "{{\"type\":\"mute\",\"target\":{}}}",
                role_json(*target)
            ),
            Self::MidiClip {
                track_id,
                target,
                label,
                start,
                end,
                loop_beats,
                notes,
            } => {
                write!(
                    output,
                    concat!(
                        "{{\"type\":\"midi-clip\",\"target\":{},\"trackId\":{},",
                        "\"label\":{},\"start\":{},\"end\":{},\"loopBeats\":{},\"events\":["
                    ),
                    json_string(target.as_str()),
                    track_id,
                    json_string(label),
                    decimal(*start),
                    decimal(*end),
                    decimal(*loop_beats)
                )
                .expect("writing to a string cannot fail");
                for (index, note) in notes.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    write!(
                        output,
                        concat!(
                            "{{\"type\":\"note\",\"time\":{},\"duration\":{},",
                            "\"pitch\":{},\"velocity\":{}}}"
                        ),
                        decimal(note.time),
                        decimal(note.duration),
                        note.pitch,
                        decimal(note.velocity)
                    )
                    .expect("writing to a string cannot fail");
                }
                output.push_str("]}");
                return;
            }
            Self::AddTrack { role } => write!(
                output,
                "{{\"type\":\"add-track\",\"target\":{}}}",
                json_string(role.as_str())
            ),
            Self::Instrument { preset, target } => write!(
                output,
                concat!(
                    "{{\"type\":\"instrument\",\"name\":{},",
                    "\"value\":0.0,\"target\":{}}}"
                ),
                json_string(preset),
                json_string(target.as_str())
            ),
            Self::Modulator {
                parameter,
                shape,
                rate,
                depth,
                target,
            } => write!(
                output,
                concat!(
                    "{{\"type\":\"modulator\",\"name\":{},",
                    "\"shape\":{},\"rate\":{},\"value\":{},\"target\":{}}}"
                ),
                json_string(parameter),
                json_string(shape),
                decimal(*rate),
                decimal(*depth),
                json_string(target.as_str())
            ),
            Self::Configure {
                track_id,
                target,
                tool,
                tool_id,
                clip_id,
                parameter,
                value,
            } => write!(
                output,
                concat!(
                    "{{\"type\":\"configure\",\"target\":{},\"trackId\":{},",
                    "\"tool\":{},\"toolId\":{},\"clipId\":{},",
                    "\"parameter\":{},\"setting\":{}}}"
                ),
                json_string(target.as_str()),
                track_id,
                json_string(tool),
                tool_id,
                clip_id.unwrap_or(0),
                json_string(parameter),
                json_string(value)
            ),
            Self::Automation {
                track_id,
                parameter,
                curve,
                points,
                target,
            } => {
                write!(
                    output,
                    concat!(
                        "{{\"type\":\"automation\",\"trackId\":{},\"name\":{},\"curve\":{},",
                        "\"target\":{},\"points\":["
                    ),
                    track_id,
                    json_string(parameter),
                    json_string(curve),
                    json_string(target.as_str())
                )
                .expect("writing to a string cannot fail");
                for (index, point) in points.iter().enumerate() {
                    if index > 0 {
                        output.push(',');
                    }
                    write!(
                        output,
                        "{{\"time\":{},\"value\":{}}}",
                        decimal(point.time),
                        decimal(point.value)
                    )
                    .expect("writing to a string cannot fail");
                }
                output.push_str("]}");
                return;
            }
            Self::Effect { name, mix, target } => write!(
                output,
                concat!(
                    "{{\"type\":\"effect\",\"name\":{},\"value\":{},",
                    "\"target\":{}}}"
                ),
                json_string(name),
                decimal(*mix),
                role_json(*target)
            ),
            Self::RemoveEffect { name, target } => write!(
                output,
                concat!(
                    "{{\"type\":\"remove-effect\",\"name\":{},",
                    "\"target\":{}}}"
                ),
                json_string(name),
                role_json(*target)
            ),
            Self::Filter { amount, target } => write!(
                output,
                "{{\"type\":\"filter\",\"value\":{},\"target\":{}}}",
                decimal(*amount),
                role_json(*target)
            ),
            Self::Rhythm { amount, target } => write!(
                output,
                "{{\"type\":\"rhythm\",\"value\":{},\"target\":{}}}",
                decimal(*amount),
                role_json(*target)
            ),
            Self::Tempo { bpm } => write!(
                output,
                "{{\"type\":\"tempo\",\"value\":{bpm},\"target\":\"all\"}}"
            ),
        }
        .expect("writing to a string cannot fail");
    }
}

#[derive(Debug, PartialEq)]
pub enum StudioError {
    EmptyPrompt,
    InvalidPrompt,
    InvalidSelection,
    UnknownTrack,
    InvalidMix,
    InvalidChannel,
    UnknownSoundTool,
    InvalidSoundTool,
}

#[derive(Clone)]
pub struct Studio {
    project: Project,
    history: Vec<Project>,
    next_id: u64,
}

impl Default for Studio {
    fn default() -> Self {
        Self::new()
    }
}

impl Studio {
    #[must_use]
    pub fn new() -> Self {
        let project = Project::demo();
        let next_id = project
            .highest_id()
            .checked_add(1)
            .expect("demo project exhausted the ID namespace");
        Self {
            project,
            history: Vec::new(),
            next_id,
        }
    }

    #[must_use]
    pub fn from_project(mut project: Project) -> Self {
        project.compact_edit_log();
        let next_id = project
            .highest_id()
            .checked_add(1)
            .expect("project exhausted the ID namespace");
        Self {
            project,
            history: Vec::new(),
            next_id,
        }
    }

    #[must_use]
    pub const fn project(&self) -> &Project {
        &self.project
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        let mut json = self.project.to_json();
        json.pop();
        write!(json, ",\"canUndo\":{}}}", !self.history.is_empty())
            .expect("writing to a string cannot fail");
        json
    }

    pub fn apply_prompt(
        &mut self,
        start: f32,
        end: f32,
        prompt: &str,
    ) -> Result<String, StudioError> {
        let plan = PromptEngine::interpret_project(prompt, &self.project, start, end);
        self.apply_plan(start, end, prompt, plan)
    }

    pub fn validate_edit(&self, start: f32, end: f32, prompt: &str) -> Result<(), StudioError> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(StudioError::EmptyPrompt);
        }
        if prompt.chars().count() > MAX_PROMPT_CHARACTERS {
            return Err(StudioError::InvalidPrompt);
        }
        if !start.is_finite()
            || !end.is_finite()
            || start < 0.0
            || end <= start
            || end > self.project.duration
        {
            return Err(StudioError::InvalidSelection);
        }
        Ok(())
    }

    pub fn apply_plan(
        &mut self,
        start: f32,
        end: f32,
        prompt: &str,
        plan: crate::prompt::EditPlan,
    ) -> Result<String, StudioError> {
        self.apply_plan_inner(start, end, prompt, None, plan)
    }

    pub(crate) fn apply_plan_for_operation(
        &mut self,
        start: f32,
        end: f32,
        prompt: &str,
        operation_id: String,
        plan: crate::prompt::EditPlan,
    ) -> Result<String, StudioError> {
        self.apply_plan_inner(start, end, prompt, Some(operation_id), plan)
    }

    fn apply_plan_inner(
        &mut self,
        start: f32,
        end: f32,
        prompt: &str,
        operation_id: Option<String>,
        plan: crate::prompt::EditPlan,
    ) -> Result<String, StudioError> {
        self.validate_edit(start, end, prompt)?;
        let mut candidate = Self {
            project: self.project.clone(),
            history: Vec::new(),
            next_id: self.next_id,
        };
        candidate.apply_action(&plan.action, start, end)?;
        let prompt = prompt.trim();
        self.remember();
        self.project = candidate.project;
        self.next_id = candidate.next_id;

        let summary = plan.summary;
        let edit_id = self.take_id();
        self.project.edits.push(Edit {
            id: edit_id,
            operation_id: operation_id.clone(),
            start,
            end,
            prompt: prompt.to_owned(),
            summary: summary.clone(),
            action: plan.action,
        });
        self.project.compact_edit_log();
        self.project.version += 1;
        if let Some(operation_id) = operation_id {
            self.project.edit_operations.push(EditOperation {
                operation_id,
                completed: true,
                applied_steps: 1,
                project_version: self.project.version,
                message: summary.clone(),
            });
            self.project.compact_edit_log();
        }
        Ok(summary)
    }

    pub fn replace_graph(
        &mut self,
        mut project: Project,
        start: f32,
        end: f32,
        prompt: &str,
        plan: crate::prompt::EditPlan,
    ) -> Result<String, StudioError> {
        self.validate_edit(start, end, prompt)?;
        project.edits = self.project.edits.clone();
        project.edit_operations = self.project.edit_operations.clone();
        project.channel_operations = self.project.channel_operations.clone();
        project.version = self.project.version;
        let next_id = project
            .highest_id()
            .checked_add(1)
            .expect("project exhausted the ID namespace");

        let prompt = prompt.trim();
        let summary = plan.summary;
        self.remember();
        self.project = project;
        self.next_id = next_id;
        let edit_id = self.take_id();
        self.project.edits.push(Edit {
            id: edit_id,
            operation_id: None,
            start,
            end,
            prompt: prompt.to_owned(),
            summary: summary.clone(),
            action: plan.action,
        });
        self.project.compact_edit_log();
        self.project.version += 1;
        Ok(summary)
    }

    pub(crate) fn record_operation_step(&mut self, operation_id: &str, message: &str) -> bool {
        if let Some(operation) = self
            .project
            .edit_operations
            .iter_mut()
            .find(|operation| operation.operation_id == operation_id)
        {
            if operation.completed {
                return false;
            }
            operation.applied_steps += 1;
            operation.project_version = self.project.version;
            operation.message = message.to_owned();
        } else {
            self.project.edit_operations.push(EditOperation {
                operation_id: operation_id.to_owned(),
                completed: false,
                applied_steps: 1,
                project_version: self.project.version,
                message: message.to_owned(),
            });
        }
        self.project.compact_edit_log();
        true
    }

    pub(crate) fn mark_operation_complete(&mut self, operation_id: &str, message: &str) -> bool {
        let Some(index) =
            self.project.edit_operations.iter().position(|operation| {
                operation.operation_id == operation_id && !operation.completed
            })
        else {
            return false;
        };
        self.project.version += 1;
        let operation = &mut self.project.edit_operations[index];
        operation.completed = true;
        operation.project_version = self.project.version;
        operation.message = message.to_owned();
        true
    }

    fn apply_action(&mut self, action: &Action, start: f32, end: f32) -> Result<(), StudioError> {
        match action {
            Action::Compound { actions } => {
                for action in actions {
                    self.apply_action(action, start, end)?;
                }
                Ok(())
            }
            Action::Timed {
                start: relative_start,
                end: relative_end,
                action,
            } => {
                let duration = end - start;
                self.apply_action(
                    action,
                    start + duration * relative_start,
                    start + duration * relative_end,
                )
            }
            Action::AddTrack { role } => {
                if self.project.tracks.len() >= TRACK_LIMIT {
                    return Err(StudioError::InvalidChannel);
                }
                self.add_track(*role, start, end, "AI variation");
                Ok(())
            }
            Action::MidiClip {
                track_id,
                target,
                label,
                start: clip_start,
                end: clip_end,
                loop_beats,
                notes,
            } => self.add_midi_clip(
                *track_id,
                *target,
                label,
                *clip_start,
                *clip_end,
                *loop_beats,
                notes,
                start,
                end,
            ),
            Action::Tempo { bpm } => {
                self.project.bpm = *bpm;
                Ok(())
            }
            Action::Instrument { preset, target } => {
                let track_index = role_action_track_index(&self.project, *target, None)
                    .ok_or(StudioError::UnknownTrack)?;
                let instrument = &mut self.project.tracks[track_index].instrument;
                if !valid_surge_preset(preset) {
                    return Err(StudioError::InvalidSoundTool);
                }
                instrument.preset = (*preset).to_owned();
                Ok(())
            }
            Action::Modulator {
                parameter,
                shape,
                rate,
                depth,
                target,
            } => self.add_modulator(*target, parameter, shape, *rate, *depth),
            Action::Configure {
                track_id,
                target,
                tool,
                tool_id,
                clip_id,
                parameter,
                value,
                ..
            } => {
                let track = self
                    .project
                    .tracks
                    .iter_mut()
                    .find(|track| track.id == *track_id && track.role == *target)
                    .ok_or(StudioError::UnknownTrack)?;
                configure_track_tool(track, tool, *tool_id, *clip_id, parameter, value)
            }
            Action::Automation {
                track_id,
                parameter,
                points,
                target,
                ..
            } => {
                let track = self
                    .project
                    .tracks
                    .iter()
                    .find(|track| {
                        track.id == *track_id
                            && track.role == *target
                            && valid_automation_target(track, parameter)
                    })
                    .ok_or(StudioError::UnknownSoundTool)?;
                let target = automation_targets(track)
                    .into_iter()
                    .find(|target| target.id == *parameter)
                    .expect("validated automation target exists");
                if points
                    .iter()
                    .any(|point| !(target.minimum..=target.maximum).contains(&point.value))
                {
                    return Err(StudioError::InvalidSoundTool);
                }
                Ok(())
            }
            Action::Gain { target, .. }
            | Action::Mute { target }
            | Action::Effect { target, .. }
            | Action::RemoveEffect { target, .. }
            | Action::Filter { target, .. }
            | Action::Rhythm { target, .. } => {
                if target.is_some_and(|target| {
                    !self.project.tracks.iter().any(|track| track.role == target)
                }) {
                    Err(StudioError::UnknownTrack)
                } else {
                    Ok(())
                }
            }
        }
    }

    pub fn set_mix(
        &mut self,
        track_id: u64,
        volume: Option<f32>,
        muted: Option<bool>,
    ) -> Result<(), StudioError> {
        if volume.is_none() && muted.is_none() {
            return Err(StudioError::InvalidMix);
        }
        if volume.is_some_and(|value| !value.is_finite() || !(0.0..=1.5).contains(&value)) {
            return Err(StudioError::InvalidMix);
        }
        if !self.project.tracks.iter().any(|track| track.id == track_id) {
            return Err(StudioError::UnknownTrack);
        }

        self.remember();
        let track = self
            .project
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .expect("track existence was checked");
        if let Some(volume) = volume {
            track.volume = volume;
        }
        if let Some(muted) = muted {
            track.muted = muted;
        }
        self.project.version += 1;
        Ok(())
    }

    pub fn add_channel(&mut self, role: TrackRole) -> Result<u64, StudioError> {
        if self.project.tracks.len() >= TRACK_LIMIT {
            return Err(StudioError::InvalidChannel);
        }
        self.remember();
        let track_id = self.add_track(role, 0.0, self.project.duration, "New track");
        self.project.version += 1;
        Ok(track_id)
    }

    pub fn delete_channel(&mut self, track_id: u64) -> Result<(), StudioError> {
        let Some(index) = self
            .project
            .tracks
            .iter()
            .position(|track| track.id == track_id)
        else {
            return Err(StudioError::UnknownTrack);
        };
        if self.project.tracks.len() == 1 {
            return Err(StudioError::InvalidChannel);
        }

        self.remember();
        self.project.tracks.remove(index);
        self.project
            .edits
            .retain_mut(|edit| edit.action.retain_after_track_deletion(track_id));
        self.project.version += 1;
        Ok(())
    }

    pub(crate) fn record_channel_operation(
        &mut self,
        operation_id: &str,
        action: ChannelOperationAction,
        track_id: u64,
        role: Option<TrackRole>,
    ) -> bool {
        if self
            .project
            .channel_operations
            .iter()
            .any(|operation| operation.operation_id == operation_id)
        {
            return false;
        }
        self.project.channel_operations.push(ChannelOperation {
            operation_id: operation_id.to_owned(),
            action,
            track_id,
            role,
            project_version: self.project.version,
        });
        self.project.compact_edit_log();
        true
    }

    pub fn configure_sound_tool(
        &mut self,
        track_id: u64,
        tool: &str,
        tool_id: u64,
        clip_id: Option<u64>,
        parameter: &str,
        value: &str,
    ) -> Result<(), StudioError> {
        let mut project = self.project.clone();
        let track = project
            .tracks
            .iter_mut()
            .find(|track| track.id == track_id)
            .ok_or(StudioError::UnknownTrack)?;
        configure_track_tool(track, tool, tool_id, clip_id, parameter, value)?;

        self.remember();
        project.version = self.project.version + 1;
        self.project = project;
        Ok(())
    }

    pub fn undo(&mut self) -> bool {
        let Some(mut previous) = self.history.pop() else {
            return false;
        };
        previous.version = self.project.version + 1;
        self.project = previous;
        true
    }

    pub fn reset(&mut self) {
        self.remember();
        let version = self.project.version + 1;
        self.project = Project::demo();
        self.project.version = version;
    }

    fn remember(&mut self) {
        if self.history.len() == HISTORY_LIMIT {
            self.history.remove(0);
        }
        self.history.push(self.project.clone());
    }

    fn take_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self
            .next_id
            .checked_add(1)
            .expect("project ID namespace exhausted");
        id
    }

    fn add_track(&mut self, role: TrackRole, start: f32, end: f32, label: &str) -> u64 {
        let track_id = self.take_id();
        let mut track = generated_track(track_id, role);
        track.instrument.id = self.take_id();
        for effect in &mut track.effects {
            effect.id = self.take_id();
        }
        track.routing.effect_order = track.effects.iter().map(|effect| effect.id).collect();
        for modulator in &mut track.modulators {
            modulator.id = self.take_id();
        }
        track.clips = vec![self.generated_clip(label, start, end, "generated", role)];
        self.project.tracks.push(track);
        track_id
    }

    fn generated_clip(
        &mut self,
        label: &str,
        start: f32,
        end: f32,
        style: &str,
        role: TrackRole,
    ) -> Clip {
        let clip_id = self.take_id();
        let mut generated = clip(clip_id, label, start, end, style, role);
        for event in &mut generated.events {
            event.id = self.take_id();
        }
        generated
    }

    #[allow(clippy::too_many_arguments)]
    fn add_midi_clip(
        &mut self,
        track_id: u64,
        role: TrackRole,
        label: &str,
        relative_start: f32,
        relative_end: f32,
        loop_beats: f32,
        notes: &[crate::prompt::MidiNote],
        selection_start: f32,
        selection_end: f32,
    ) -> Result<(), StudioError> {
        if label.trim().is_empty()
            || label.chars().count() > 64
            || !relative_start.is_finite()
            || !relative_end.is_finite()
            || !(0.0..1.0).contains(&relative_start)
            || !(0.0..=1.0).contains(&relative_end)
            || relative_end <= relative_start
            || !loop_beats.is_finite()
            || !(0.25..=16.0).contains(&loop_beats)
            || notes.len() > 32
            || notes.iter().any(|note| {
                !note.time.is_finite()
                    || !(0.0..loop_beats).contains(&note.time)
                    || !note.duration.is_finite()
                    || !(0.0625..=loop_beats).contains(&note.duration)
                    || !note.velocity.is_finite()
                    || !(0.01..=1.0).contains(&note.velocity)
            })
        {
            return Err(StudioError::InvalidSoundTool);
        }
        let track_index = if track_id == 0 {
            self.project
                .tracks
                .iter()
                .rposition(|track| track.role == role)
        } else {
            self.project
                .tracks
                .iter()
                .position(|track| track.id == track_id && track.role == role)
        }
        .ok_or(StudioError::UnknownTrack)?;
        let selection_duration = selection_end - selection_start;
        let start = selection_start + selection_duration * relative_start;
        let end = selection_start + selection_duration * relative_end;
        let clip = Clip {
            id: self.take_id(),
            label: label.trim().to_owned(),
            start,
            end,
            source_start: start,
            style: "generated".to_owned(),
            loop_beats,
            events: notes
                .iter()
                .map(|note| ClipEvent {
                    id: self.take_id(),
                    kind: "note".to_owned(),
                    time: note.time,
                    duration: note.duration,
                    pitch: note.pitch,
                    velocity: note.velocity,
                })
                .collect(),
        };
        self.replace_track_region(track_index, start, end, clip);
        Ok(())
    }

    fn add_modulator(
        &mut self,
        role: TrackRole,
        parameter: &str,
        shape: &str,
        rate: f32,
        depth: f32,
    ) -> Result<(), StudioError> {
        if !self.project.tracks.iter().any(|track| track.role == role) {
            return Err(StudioError::UnknownTrack);
        }
        if !matches!(
            shape,
            "sine" | "triangle" | "square" | "random" | "envelope"
        ) || !rate.is_finite()
            || !(0.01..=20.0).contains(&rate)
            || !depth.is_finite()
            || !(0.0..=1.0).contains(&depth)
        {
            return Err(StudioError::InvalidSoundTool);
        }
        let track_index = role_action_track_index(&self.project, role, Some(parameter))
            .ok_or(StudioError::InvalidSoundTool)?;
        let id = self.take_id();
        self.project.tracks[track_index].modulators.push(Modulator {
            id,
            name: "AI modulation".to_owned(),
            shape: shape.to_owned(),
            rate,
            rate_mode: "hz".to_owned(),
            trigger: "free".to_owned(),
            depth,
            target: parameter.to_owned(),
            enabled: true,
        });
        Ok(())
    }

    fn replace_track_region(
        &mut self,
        track_index: usize,
        start: f32,
        end: f32,
        replacement: Clip,
    ) {
        let clips = std::mem::take(&mut self.project.tracks[track_index].clips);
        let mut retained = Vec::with_capacity(clips.len() + 1);
        for clip in clips {
            if clip.end <= start || clip.start >= end {
                retained.push(clip);
                continue;
            }

            let spans_left_boundary = clip.start < start;
            if spans_left_boundary {
                let mut left = clip.clone();
                left.end = start;
                retained.push(left);
            }
            if clip.end > end {
                let mut right = clip;
                if spans_left_boundary {
                    right.id = self.take_id();
                }
                right.start = end;
                retained.push(right);
            }
        }
        retained.push(replacement);
        retained.sort_by(|left, right| left.start.total_cmp(&right.start));
        self.project.tracks[track_index].clips = retained;
    }
}

fn configure_track_tool(
    track: &mut Track,
    tool: &str,
    tool_id: u64,
    clip_id: Option<u64>,
    parameter: &str,
    value: &str,
) -> Result<(), StudioError> {
    match tool {
        "instrument" => configure_instrument(&mut track.instrument, tool_id, parameter, value),
        "effect" => {
            let effect = track
                .effects
                .iter_mut()
                .find(|effect| effect.id == tool_id)
                .ok_or(StudioError::UnknownSoundTool)?;
            match parameter {
                "mix" => effect.mix = parse_range(value, 0.0, 1.0)?,
                "cutoff" if effect.cutoff_hz.is_some() => {
                    effect.cutoff_hz = Some(parse_range(
                        value,
                        FILTER_CUTOFF_MIN_HZ,
                        FILTER_CUTOFF_MAX_HZ,
                    )?);
                }
                "resonance" if effect.resonance.is_some() => {
                    effect.resonance = Some(parse_range(
                        value,
                        FILTER_RESONANCE_MIN,
                        FILTER_RESONANCE_MAX,
                    )?);
                }
                "enabled" => effect.enabled = parse_bool(value)?,
                _ => return Err(StudioError::InvalidSoundTool),
            }
            Ok(())
        }
        "modulator" => {
            if parameter == "target" && !valid_modulator_target(track, value) {
                return Err(StudioError::InvalidSoundTool);
            }
            let modulator = track
                .modulators
                .iter_mut()
                .find(|modulator| modulator.id == tool_id)
                .ok_or(StudioError::UnknownSoundTool)?;
            match parameter {
                "shape"
                    if matches!(
                        value,
                        "sine" | "triangle" | "square" | "random" | "envelope"
                    ) =>
                {
                    modulator.shape = value.to_owned();
                }
                "rate" => modulator.rate = parse_range(value, 0.01, 20.0)?,
                "rateMode" if matches!(value, "hz" | "tempo") => {
                    modulator.rate_mode = value.to_owned();
                }
                "trigger" if matches!(value, "free" | "midi") => {
                    modulator.trigger = value.to_owned();
                }
                "depth" => modulator.depth = parse_range(value, 0.0, 1.0)?,
                "target" => modulator.target = value.to_owned(),
                "enabled" => modulator.enabled = parse_bool(value)?,
                _ => return Err(StudioError::InvalidSoundTool),
            }
            Ok(())
        }
        "event" => {
            let clip = track
                .clips
                .iter_mut()
                .find(|clip| Some(clip.id) == clip_id)
                .ok_or(StudioError::UnknownSoundTool)?;
            let event = clip
                .events
                .iter_mut()
                .find(|event| event.id == tool_id)
                .ok_or(StudioError::UnknownSoundTool)?;
            match parameter {
                "time" => event.time = parse_range_exclusive(value, 0.0, clip.loop_beats)?,
                "duration" => event.duration = parse_range(value, 0.0625, clip.loop_beats)?,
                "pitch" => event.pitch = parse_integer_range(value, 0, 127)? as u8,
                "velocity" => event.velocity = parse_range(value, 0.01, 1.0)?,
                _ => return Err(StudioError::InvalidSoundTool),
            }
            clip.events
                .sort_by(|left, right| left.time.total_cmp(&right.time));
            Ok(())
        }
        "routing" if parameter == "position" => {
            let position = parse_integer_range(
                value,
                0,
                track.routing.effect_order.len().saturating_sub(1) as u64,
            )? as usize;
            let current = track
                .routing
                .effect_order
                .iter()
                .position(|effect_id| *effect_id == tool_id)
                .ok_or(StudioError::UnknownSoundTool)?;
            let effect_id = track.routing.effect_order.remove(current);
            track.routing.effect_order.insert(position, effect_id);
            Ok(())
        }
        "routing" => Err(StudioError::InvalidSoundTool),
        _ => Err(StudioError::UnknownSoundTool),
    }
}

fn configure_instrument(
    instrument: &mut Instrument,
    tool_id: u64,
    parameter: &str,
    value: &str,
) -> Result<(), StudioError> {
    if instrument.id != tool_id {
        return Err(StudioError::UnknownSoundTool);
    }
    if parameter == "preset" {
        return if valid_surge_preset(value) {
            instrument.preset = value.to_owned();
            Ok(())
        } else {
            Err(StudioError::InvalidSoundTool)
        };
    }
    match parameter {
        "attack" => instrument.attack = parse_range(value, 0.0, 1.0)?,
        "release" => instrument.release = parse_range(value, 0.0, 1.0)?,
        "cutoff" => instrument.cutoff = parse_range(value, 0.0, 1.0)?,
        "resonance" => instrument.resonance = parse_range(value, 0.0, 1.0)?,
        "pitch" => instrument.pitch = parse_range(value, 0.0, 1.0)?,
        _ => return Err(StudioError::InvalidSoundTool),
    }
    Ok(())
}

pub(crate) fn valid_surge_preset(value: &str) -> bool {
    SURGE_PRESETS.contains(&value)
}

fn parse_range(value: &str, minimum: f32, maximum: f32) -> Result<f32, StudioError> {
    value
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite() && (minimum..=maximum).contains(value))
        .ok_or(StudioError::InvalidSoundTool)
}

fn parse_range_exclusive(value: &str, minimum: f32, maximum: f32) -> Result<f32, StudioError> {
    value
        .parse::<f32>()
        .ok()
        .filter(|value| value.is_finite() && *value >= minimum && *value < maximum)
        .ok_or(StudioError::InvalidSoundTool)
}

fn parse_integer_range(value: &str, minimum: u64, maximum: u64) -> Result<u64, StudioError> {
    value
        .parse::<u64>()
        .ok()
        .filter(|value| (minimum..=maximum).contains(value))
        .ok_or(StudioError::InvalidSoundTool)
}

fn parse_bool(value: &str) -> Result<bool, StudioError> {
    match value {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => Err(StudioError::InvalidSoundTool),
    }
}

fn modulation_targets(track: &Track) -> Vec<ModulationTarget> {
    let mut targets = vec![
        ModulationTarget {
            id: "instrument.attack".to_owned(),
            label: "Surge amp envelope attack".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.release".to_owned(),
            label: "Surge amp envelope release".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.cutoff".to_owned(),
            label: "Surge filter 1 cutoff".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.resonance".to_owned(),
            label: "Surge filter 1 resonance".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.pitch".to_owned(),
            label: "Surge scene pitch".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 0.1,
            mode: "add",
        },
        ModulationTarget {
            id: "track.volume".to_owned(),
            label: "Track volume".to_owned(),
            minimum: 0.0,
            maximum: 1.5,
            scale: 1.0,
            mode: "multiply",
        },
    ];
    for effect in &track.effects {
        targets.push(ModulationTarget {
            id: format!("effect:{}.mix", effect.id),
            label: format!("{} mix", effect.name),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        });
        if effect.cutoff_hz.is_some() {
            targets.push(ModulationTarget {
                id: format!("effect:{}.cutoff", effect.id),
                label: format!("{} cutoff", effect.name),
                minimum: FILTER_CUTOFF_MIN_HZ,
                maximum: FILTER_CUTOFF_MAX_HZ,
                scale: 4.0,
                mode: "exponential",
            });
        }
        if effect.resonance.is_some() {
            targets.push(ModulationTarget {
                id: format!("effect:{}.resonance", effect.id),
                label: format!("{} resonance", effect.name),
                minimum: FILTER_RESONANCE_MIN,
                maximum: FILTER_RESONANCE_MAX,
                scale: 10.0,
                mode: "add",
            });
        }
    }
    targets
}

pub(crate) fn valid_modulator_target(track: &Track, value: &str) -> bool {
    modulation_targets(track)
        .iter()
        .any(|target| target.id == value)
}

fn automation_targets(track: &Track) -> Vec<ModulationTarget> {
    let mut targets = modulation_targets(track);
    for modulator in &track.modulators {
        targets.push(ModulationTarget {
            id: format!("modulator:{}.rate", modulator.id),
            label: format!("{} rate", modulator.name),
            minimum: 0.01,
            maximum: 20.0,
            scale: 1.0,
            mode: "linear",
        });
        targets.push(ModulationTarget {
            id: format!("modulator:{}.depth", modulator.id),
            label: format!("{} depth", modulator.name),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "linear",
        });
    }
    targets
}

pub(crate) fn valid_automation_target(track: &Track, value: &str) -> bool {
    automation_target_range(track, value).is_some()
}

pub(crate) fn automation_target_range(track: &Track, value: &str) -> Option<(f32, f32)> {
    automation_targets(track)
        .into_iter()
        .find(|target| target.id == value)
        .map(|target| (target.minimum, target.maximum))
}

fn role_action_track_index(
    project: &Project,
    role: TrackRole,
    modulator_target: Option<&str>,
) -> Option<usize> {
    project.tracks.iter().rposition(|track| {
        track.role == role
            && modulator_target.is_none_or(|target| valid_modulator_target(track, target))
    })
}

fn demo_track(id: u64, role: TrackRole, name: &str, color: &str) -> Track {
    let mut track = generated_track(id, role);
    track.name = name.to_owned();
    track.color = color.to_owned();
    track.clips = vec![clip(
        id + 10,
        match role {
            TrackRole::Drums => "Pocket beat",
            TrackRole::Bass => "Warm pulse",
            TrackRole::Chords => "Four-chord glow",
            TrackRole::Lead => "Lead phrase",
            TrackRole::Texture => "Air layer",
        },
        0.0,
        32.0,
        "foundation",
        role,
    )];
    track
}

fn generated_track(id: u64, role: TrackRole) -> Track {
    let (name, color, preset, attack, release, cutoff, effect_specs, modulator) = match role {
        TrackRole::Drums => (
            "AI Drum Rack",
            "#ffb86b",
            "Surge Percussion",
            0.0,
            0.25,
            0.82,
            vec![("Punch compressor", 0.34)],
            ("Pulse envelope", "envelope", 2.0, 0.12, "instrument.cutoff"),
        ),
        TrackRole::Bass => (
            "AI Bass",
            "#74e0bc",
            "Surge Bass",
            0.02,
            0.3,
            0.45,
            vec![("Low-pass filter", 0.46)],
            ("Bass movement", "sine", 0.25, 0.18, "instrument.cutoff"),
        ),
        TrackRole::Chords => (
            "AI Chords",
            "#8ca9ff",
            "Surge Pad",
            0.36,
            0.62,
            0.58,
            vec![("Chorus", 0.28), ("Room", 0.2)],
            ("Slow bloom", "triangle", 0.125, 0.16, "instrument.cutoff"),
        ),
        TrackRole::Lead => (
            "AI Lead",
            "#d99cff",
            "Surge Lead",
            0.02,
            0.38,
            0.64,
            vec![("Echo", 0.24)],
            ("Lead vibrato", "sine", 5.0, 0.08, "instrument.pitch"),
        ),
        TrackRole::Texture => (
            "AI Texture",
            "#ff91ad",
            "Surge Atmosphere",
            0.58,
            0.74,
            0.7,
            vec![("Shimmer", 0.38)],
            (
                "Atmosphere drift",
                "random",
                0.18,
                0.22,
                "instrument.cutoff",
            ),
        ),
    };

    let instrument_id = tool_id(id, 1);
    let effects = effect_specs
        .into_iter()
        .enumerate()
        .map(|(index, (name, mix))| effect(tool_id(id, index as u64 + 10), name, mix))
        .collect::<Vec<_>>();
    let effect_order = effects.iter().map(|effect| effect.id).collect();

    Track {
        id,
        name: name.to_owned(),
        role,
        color: color.to_owned(),
        volume: match role {
            TrackRole::Drums => 0.78,
            TrackRole::Bass => 0.95,
            TrackRole::Chords => 0.85,
            TrackRole::Lead => 0.56,
            TrackRole::Texture => 0.44,
        },
        muted: false,
        instrument: Instrument {
            id: instrument_id,
            engine: SURGE_ENGINE.to_owned(),
            preset: preset.to_owned(),
            attack,
            release,
            cutoff,
            resonance: 0.18,
            pitch: 0.5,
        },
        effects,
        modulators: vec![Modulator {
            id: tool_id(id, 50),
            name: modulator.0.to_owned(),
            shape: modulator.1.to_owned(),
            rate: modulator.2,
            rate_mode: "hz".to_owned(),
            trigger: if modulator.1 == "envelope" {
                "midi".to_owned()
            } else {
                "free".to_owned()
            },
            depth: modulator.3,
            target: modulator.4.to_owned(),
            enabled: true,
        }],
        routing: Routing {
            effect_order,
            output: "master".to_owned(),
        },
        clips: Vec::new(),
    }
}

fn clip(id: u64, label: &str, start: f32, end: f32, style: &str, role: TrackRole) -> Clip {
    Clip {
        id,
        label: label.to_owned(),
        start,
        end,
        source_start: start,
        style: style.to_owned(),
        loop_beats: 4.0,
        events: pattern_events(id, role),
    }
}

fn pattern_events(clip_id: u64, role: TrackRole) -> Vec<ClipEvent> {
    let specs: Vec<(&str, f32, f32, u8, f32)> = match role {
        TrackRole::Drums => vec![
            ("note", 0.0, 0.25, 36, 0.92),
            ("note", 0.0, 0.12, 42, 0.58),
            ("note", 0.5, 0.12, 42, 0.42),
            ("note", 1.0, 0.25, 38, 0.78),
            ("note", 1.0, 0.12, 42, 0.58),
            ("note", 1.5, 0.12, 42, 0.42),
            ("note", 2.0, 0.25, 36, 0.88),
            ("note", 2.0, 0.12, 42, 0.58),
            ("note", 2.5, 0.12, 42, 0.42),
            ("note", 3.0, 0.25, 38, 0.78),
            ("note", 3.0, 0.12, 42, 0.58),
            ("note", 3.5, 0.12, 42, 0.42),
        ],
        TrackRole::Bass => vec![
            ("note", 0.0, 0.7, 33, 0.82),
            ("note", 1.0, 0.7, 33, 0.72),
            ("note", 2.0, 0.7, 36, 0.78),
            ("note", 3.0, 0.7, 31, 0.74),
        ],
        TrackRole::Chords => vec![
            ("note", 0.0, 1.85, 57, 0.62),
            ("note", 0.0, 1.85, 60, 0.56),
            ("note", 0.0, 1.85, 64, 0.54),
            ("note", 2.0, 1.85, 53, 0.6),
            ("note", 2.0, 1.85, 57, 0.54),
            ("note", 2.0, 1.85, 60, 0.52),
        ],
        TrackRole::Lead => vec![
            ("note", 0.0, 0.75, 69, 0.72),
            ("note", 1.0, 0.75, 76, 0.75),
            ("note", 2.0, 0.75, 71, 0.72),
            ("note", 3.0, 0.75, 67, 0.66),
        ],
        TrackRole::Texture => vec![("note", 0.0, 3.8, 64, 0.5), ("note", 0.0, 3.4, 71, 0.38)],
    };
    specs
        .into_iter()
        .enumerate()
        .map(
            |(index, (kind, time, duration, pitch, velocity))| ClipEvent {
                id: clip_id * 100 + index as u64 + 1,
                kind: kind.to_owned(),
                time,
                duration,
                pitch,
                velocity,
            },
        )
        .collect()
}

const fn tool_id(track_id: u64, offset: u64) -> u64 {
    track_id * 100 + offset
}

fn effect(id: u64, name: &str, mix: f32) -> Effect {
    let is_filter = name == "Low-pass filter";
    Effect {
        id,
        name: name.to_owned(),
        mix,
        cutoff_hz: is_filter.then_some(FILTER_CUTOFF_DEFAULT_HZ),
        resonance: is_filter.then_some(FILTER_RESONANCE_DEFAULT),
        enabled: true,
    }
}

fn decimal(value: f32) -> String {
    let mut value = format!("{value:.6}");
    while value.ends_with('0') {
        value.pop();
    }
    if value.ends_with('.') {
        value.push('0');
    }
    value
}

fn role_json(role: Option<TrackRole>) -> String {
    json_string(role.map_or("all", TrackRole::as_str))
}

fn write_signal_edge(
    output: &mut String,
    comma: bool,
    source: &str,
    target: &str,
    signal_type: &str,
) {
    if comma {
        output.push(',');
    }
    write!(
        output,
        "{{\"source\":{},\"target\":{},\"type\":{}}}",
        json_string(source),
        json_string(target),
        json_string(signal_type)
    )
    .expect("writing to a string cannot fail");
}

pub(crate) fn json_string(value: &str) -> String {
    serde_json::to_string(value).expect("strings must serialize to JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_project_contains_a_playable_arrangement() {
        let project = Project::demo();
        assert_eq!(project.bpm, 112);
        assert_eq!(project.tracks.len(), 3);
        assert!(
            project
                .tracks
                .iter()
                .all(|track| track.instrument.engine == SURGE_ENGINE)
        );
        assert_eq!(project.tracks[0].instrument.preset, "Surge Percussion");
        assert_eq!(project.tracks[1].instrument.preset, "Surge Bass");
        assert_eq!(project.tracks[2].instrument.preset, "Surge Pad");
        assert!(project.tracks.iter().all(|track| !track.clips.is_empty()));
        assert!(project.tracks.iter().all(|track| {
            !track.clips[0].events.is_empty()
                && !track.modulators.is_empty()
                && track.routing.effect_order.len() == track.effects.len()
        }));
        let json = project.to_json();
        assert!(json.contains("Neon First Light"));
        assert!(json.contains("\"routing\""));
        assert!(json.contains("\"loopBeats\""));
        assert!(
            json.contains("\"source\":\"clips\",\"target\":\"instrument:101\",\"type\":\"midi\"")
        );
        assert!(json.contains(
            "\"source\":\"instrument:101\",\"target\":\"effect:110\",\"type\":\"audio\""
        ));
        assert!(json.contains(
            "\"source\":\"modulator:150\",\"target\":\"instrument.cutoff\",\"type\":\"control\""
        ));
        assert!(
            json.contains("\"source\":\"clips\",\"target\":\"modulator:150\",\"type\":\"midi\"")
        );
    }

    #[test]
    fn surge_presets_and_native_parameters_are_configurable() {
        let mut studio = Studio::new();
        let bass_id = studio.project().tracks[1].id;
        let instrument_id = studio.project().tracks[1].instrument.id;

        studio
            .configure_sound_tool(
                bass_id,
                "instrument",
                instrument_id,
                None,
                "preset",
                "Surge Pad",
            )
            .expect("published Surge preset");
        let instrument = &studio.project().tracks[1].instrument;
        assert_eq!(instrument.engine, SURGE_ENGINE);
        assert_eq!(instrument.preset, "Surge Pad");

        studio
            .configure_sound_tool(bass_id, "instrument", instrument_id, None, "cutoff", "0.4")
            .expect("manual Surge parameter");
        assert_eq!(studio.project().tracks[1].instrument.cutoff, 0.4);
        assert_eq!(studio.project().tracks[1].instrument.preset, "Surge Pad");
        assert_eq!(
            studio.configure_sound_tool(
                bass_id,
                "instrument",
                instrument_id,
                None,
                "preset",
                "Unknown",
            ),
            Err(StudioError::InvalidSoundTool)
        );
    }

    #[test]
    fn advanced_channels_are_playable_undoable_and_keep_one_output_path() {
        let mut studio = Studio::new();
        let original = studio.project().to_json();
        let track_id = studio.add_channel(TrackRole::Lead).expect("new channel");
        let added = studio.project().tracks.last().expect("added track");
        assert_eq!(added.id, track_id);
        assert_eq!(added.role, TrackRole::Lead);
        assert_eq!(added.clips[0].start, 0.0);
        assert_eq!(added.clips[0].end, studio.project().duration);
        assert_eq!(added.routing.output, "master");

        studio.delete_channel(track_id).expect("delete channel");
        assert_eq!(studio.project().tracks.len(), 3);
        assert!(studio.undo());
        assert!(
            studio
                .project()
                .tracks
                .iter()
                .any(|track| track.id == track_id)
        );
        assert!(studio.undo());
        let mut restored = studio.project().clone();
        restored.version = 1;
        assert_eq!(restored.to_json(), original);

        studio.delete_channel(2).expect("delete bass");
        studio.delete_channel(3).expect("delete chords");
        assert_eq!(studio.delete_channel(1), Err(StudioError::InvalidChannel));
    }

    #[test]
    fn sound_tools_are_configurable_and_undoable() {
        let mut studio = Studio::new();
        let bass = &studio.project().tracks[1];
        let bass_id = bass.id;
        let instrument_id = bass.instrument.id;
        let effect_id = bass.effects[0].id;
        let modulator_id = bass.modulators[0].id;
        let clip_id = bass.clips[0].id;
        let event_id = bass.clips[0].events[0].id;
        let chords_id = studio.project().tracks[2].id;
        let later_effect_id = studio.project().tracks[2].routing.effect_order[1];

        studio
            .configure_sound_tool(bass_id, "instrument", instrument_id, None, "attack", "0.12")
            .expect("configurable instrument");
        studio
            .configure_sound_tool(bass_id, "instrument", instrument_id, None, "cutoff", "0.41")
            .expect("configurable Surge cutoff");
        studio
            .configure_sound_tool(
                bass_id,
                "instrument",
                instrument_id,
                None,
                "resonance",
                "0.27",
            )
            .expect("configurable Surge resonance");
        studio
            .configure_sound_tool(bass_id, "instrument", instrument_id, None, "pitch", "0.55")
            .expect("configurable Surge pitch");
        studio
            .configure_sound_tool(bass_id, "effect", effect_id, None, "mix", "0.72")
            .expect("configurable effect");
        studio
            .configure_sound_tool(bass_id, "effect", effect_id, None, "cutoff", "640")
            .expect("configurable filter cutoff");
        studio
            .configure_sound_tool(bass_id, "effect", effect_id, None, "resonance", "8.5")
            .expect("configurable filter resonance");
        studio
            .configure_sound_tool(
                bass_id,
                "modulator",
                modulator_id,
                None,
                "target",
                "track.volume",
            )
            .expect("configurable modulator");
        studio
            .configure_sound_tool(
                bass_id,
                "modulator",
                modulator_id,
                None,
                "rateMode",
                "tempo",
            )
            .expect("tempo-synced modulator");
        studio
            .configure_sound_tool(bass_id, "modulator", modulator_id, None, "trigger", "midi")
            .expect("MIDI-triggered modulator");
        studio
            .configure_sound_tool(bass_id, "event", event_id, Some(clip_id), "pitch", "40")
            .expect("configurable clip event");
        studio
            .configure_sound_tool(
                bass_id,
                "event",
                event_id,
                Some(clip_id),
                "duration",
                "0.0625",
            )
            .expect("precise clip duration");
        studio
            .configure_sound_tool(chords_id, "routing", later_effect_id, None, "position", "0")
            .expect("configurable effect routing");

        let bass = &studio.project().tracks[1];
        assert_eq!(bass.instrument.attack, 0.12);
        assert_eq!(bass.instrument.cutoff, 0.41);
        assert_eq!(bass.instrument.resonance, 0.27);
        assert_eq!(bass.instrument.pitch, 0.55);
        assert_eq!(bass.effects[0].mix, 0.72);
        assert_eq!(bass.effects[0].cutoff_hz, Some(640.0));
        assert_eq!(bass.effects[0].resonance, Some(8.5));
        assert_eq!(bass.modulators[0].target, "track.volume");
        assert_eq!(bass.modulators[0].rate_mode, "tempo");
        assert_eq!(bass.modulators[0].trigger, "midi");
        assert_eq!(bass.clips[0].events[0].pitch, 40);
        assert_eq!(bass.clips[0].events[0].duration, 0.0625);
        assert!(studio.to_json().contains("\"duration\":0.0625"));
        assert!(studio.to_json().contains(&format!(
            "\"source\":\"clips\",\"target\":\"modulator:{modulator_id}\",\"type\":\"midi\""
        )));
        assert_eq!(
            studio.project().tracks[2].routing.effect_order[0],
            later_effect_id
        );
        assert!(studio.undo());
        assert_ne!(
            studio.project().tracks[2].routing.effect_order[0],
            later_effect_id
        );
    }

    #[test]
    fn publishes_and_accepts_every_modulation_target() {
        let mut studio = Studio::new();
        let bass = &studio.project().tracks[1];
        let bass_id = bass.id;
        let modulator_id = bass.modulators[0].id;
        let targets = modulation_targets(bass)
            .into_iter()
            .map(|target| target.id)
            .collect::<Vec<_>>();

        assert!(targets.contains(&"instrument.attack".to_owned()));
        assert!(targets.contains(&"instrument.release".to_owned()));
        assert!(targets.contains(&"instrument.cutoff".to_owned()));
        assert!(targets.contains(&"instrument.pitch".to_owned()));
        assert!(targets.contains(&"instrument.resonance".to_owned()));
        assert!(targets.contains(&"track.volume".to_owned()));
        assert!(targets.contains(&"effect:210.mix".to_owned()));
        assert!(targets.contains(&"effect:210.cutoff".to_owned()));
        assert!(targets.contains(&"effect:210.resonance".to_owned()));

        for target in &targets {
            studio
                .configure_sound_tool(bass_id, "modulator", modulator_id, None, "target", target)
                .expect("published target must be accepted by validation");
        }
        let json = studio.to_json();
        assert!(json.contains("\"modulationTargets\""));
        for target in targets {
            assert!(json.contains(&json_string(&target)));
        }
    }

    #[test]
    fn publishes_and_validates_parameter_automation_targets() {
        let mut studio = Studio::new();
        let bass = &studio.project().tracks[1];
        let bass_id = bass.id;
        let modulator_id = bass.modulators[0].id;
        let targets = automation_targets(bass)
            .into_iter()
            .map(|target| target.id)
            .collect::<Vec<_>>();
        assert!(targets.contains(&"track.volume".to_owned()));
        assert!(targets.contains(&"effect:210.cutoff".to_owned()));
        assert!(targets.contains(&format!("modulator:{modulator_id}.rate")));
        assert!(targets.contains(&format!("modulator:{modulator_id}.depth")));
        assert!(studio.to_json().contains("\"automationTargets\""));

        let valid = crate::prompt::EditPlan {
            summary: "Raised the bass level".to_owned(),
            action: Action::Timed {
                start: 0.25,
                end: 0.75,
                action: Box::new(Action::Automation {
                    track_id: bass_id,
                    parameter: "track.volume".to_owned(),
                    curve: "linear",
                    points: vec![
                        crate::prompt::AutomationPoint {
                            time: 0.0,
                            value: 0.1,
                        },
                        crate::prompt::AutomationPoint {
                            time: 1.0,
                            value: 1.4,
                        },
                    ],
                    target: TrackRole::Bass,
                }),
            },
        };
        studio
            .apply_plan(0.0, 8.0, "raise the bass through the transition", valid)
            .expect("published automation target");
        let saved = studio.project().to_json();
        assert!(saved.contains("\"type\":\"timed\""));
        assert!(saved.contains("\"type\":\"automation\""));

        let invalid = crate::prompt::EditPlan {
            summary: "Invalid level".to_owned(),
            action: Action::Automation {
                track_id: bass_id,
                parameter: "track.volume".to_owned(),
                curve: "linear",
                points: vec![
                    crate::prompt::AutomationPoint {
                        time: 0.0,
                        value: 0.0,
                    },
                    crate::prompt::AutomationPoint {
                        time: 1.0,
                        value: 2.0,
                    },
                ],
                target: TrackRole::Bass,
            },
        };
        assert!(
            studio
                .apply_plan(0.0, 8.0, "raise it too far", invalid)
                .is_err()
        );
        assert_eq!(studio.project().to_json(), saved);
    }

    #[test]
    fn deleting_a_track_prunes_only_its_owned_automation() {
        let mut studio = Studio::new();
        let bass_id = studio.project().tracks[1].id;
        studio
            .apply_plan(
                0.0,
                4.0,
                "automate the bass and change tempo",
                crate::prompt::EditPlan {
                    summary: "Automated bass and changed tempo".to_owned(),
                    action: Action::Compound {
                        actions: vec![
                            Action::Automation {
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
                                target: TrackRole::Bass,
                            },
                            Action::Tempo { bpm: 128 },
                        ],
                    },
                },
            )
            .expect("valid compound edit");

        studio
            .delete_channel(bass_id)
            .expect("delete automated bass");
        assert!(!studio.to_json().contains("\"type\":\"automation\""));
        assert!(studio.to_json().contains("\"type\":\"tempo\""));
        assert!(studio.undo());
        assert!(studio.to_json().contains("\"type\":\"automation\""));
        assert!(
            studio
                .project()
                .tracks
                .iter()
                .any(|track| track.id == bass_id)
        );
    }

    #[test]
    fn disabled_modulators_are_not_active_control_routes() {
        let mut studio = Studio::new();
        let bass = &studio.project().tracks[1];
        let bass_id = bass.id;
        let modulator_id = bass.modulators[0].id;
        studio
            .configure_sound_tool(bass_id, "modulator", modulator_id, None, "enabled", "false")
            .expect("disable modulator");

        let json = studio.to_json();
        assert!(json.contains(&format!("\"id\":{modulator_id},\"type\":\"modulator\"")));
        assert!(json.contains("\"enabled\":false"));
        assert!(!json.contains(&format!("\"source\":\"modulator:{modulator_id}\"")));
    }

    #[test]
    fn generated_modulators_use_collision_free_sound_tool_ids() {
        let mut studio = Studio::new();
        let drums = &studio.project().tracks[0];
        let drums_id = drums.id;
        let seeded_modulator_id = drums.modulators[0].id;
        let seeded_depth = drums.modulators[0].depth;

        for index in 0..30 {
            let plan = crate::prompt::EditPlan {
                action: Action::Modulator {
                    parameter: "instrument.cutoff".to_owned(),
                    shape: "sine",
                    rate: 0.5,
                    depth: 0.2,
                    target: TrackRole::Drums,
                },
                summary: format!("Added drum modulator {index}"),
            };
            studio
                .apply_plan(0.0, 4.0, "add drum modulation", plan)
                .expect("valid drum modulator");
        }

        let mut sound_tool_ids = Vec::new();
        for track in &studio.project().tracks {
            sound_tool_ids.push(track.instrument.id);
            sound_tool_ids.extend(track.effects.iter().map(|effect| effect.id));
            sound_tool_ids.extend(track.modulators.iter().map(|modulator| modulator.id));
        }
        let unique_ids = sound_tool_ids
            .iter()
            .copied()
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(unique_ids.len(), sound_tool_ids.len());

        let newest_modulator_id = studio.project().tracks[0]
            .modulators
            .last()
            .expect("generated modulator")
            .id;
        studio
            .configure_sound_tool(
                drums_id,
                "modulator",
                newest_modulator_id,
                None,
                "depth",
                "0.73",
            )
            .expect("newest modulator remains addressable");
        let drums = &studio.project().tracks[0];
        assert_eq!(
            drums
                .modulators
                .iter()
                .find(|modulator| modulator.id == newest_modulator_id)
                .expect("newest modulator by stable ID")
                .depth,
            0.73
        );
        assert_eq!(
            drums
                .modulators
                .iter()
                .find(|modulator| modulator.id == seeded_modulator_id)
                .expect("seeded modulator by stable ID")
                .depth,
            seeded_depth
        );
    }

    #[test]
    fn role_actions_target_the_latest_matching_track() {
        let mut studio = Studio::new();
        let original_bass_id = studio.project().tracks[1].id;
        let plan = crate::prompt::EditPlan {
            action: Action::Compound {
                actions: vec![
                    Action::AddTrack {
                        role: TrackRole::Bass,
                    },
                    Action::Instrument {
                        preset: "Surge Lead",
                        target: TrackRole::Bass,
                    },
                    Action::Modulator {
                        parameter: "instrument.attack".to_owned(),
                        shape: "triangle",
                        rate: 0.5,
                        depth: 0.3,
                        target: TrackRole::Bass,
                    },
                ],
            },
            summary: "Added a moving saw bass".to_owned(),
        };
        studio
            .apply_plan(4.0, 8.0, "add a moving saw bass", plan)
            .expect("configure newly added duplicate role");

        let basses = studio
            .project()
            .tracks
            .iter()
            .filter(|track| track.role == TrackRole::Bass)
            .collect::<Vec<_>>();
        assert_eq!(basses.len(), 2);
        assert_eq!(basses[0].id, original_bass_id);
        assert_eq!(basses[0].instrument.preset, "Surge Bass");
        assert_eq!(basses[0].modulators.len(), 1);
        assert_eq!(basses[1].instrument.preset, "Surge Lead");
        assert_eq!(basses[1].modulators.len(), 2);
        assert_eq!(basses[1].modulators[1].target, "instrument.attack");

        let later_effect_id = basses[1].effects[0].id;
        let effect_target = format!("effect:{later_effect_id}.mix");
        let plan = crate::prompt::EditPlan {
            action: Action::Modulator {
                parameter: effect_target.clone(),
                shape: "sine",
                rate: 0.5,
                depth: 0.2,
                target: TrackRole::Bass,
            },
            summary: "Modulated the later bass effect".to_owned(),
        };
        studio
            .apply_plan(4.0, 8.0, "move the later bass filter", plan)
            .expect("resolve a stable effect on a later duplicate role");
        let basses = studio
            .project()
            .tracks
            .iter()
            .filter(|track| track.role == TrackRole::Bass)
            .collect::<Vec<_>>();
        assert_eq!(basses[0].modulators.len(), 1);
        assert_eq!(basses[1].modulators.last().unwrap().target, effect_target);
    }

    #[test]
    fn sound_tool_validation_preserves_the_project() {
        let mut studio = Studio::new();
        let before = studio.to_json();
        let bass = &studio.project().tracks[1];
        let bass_id = bass.id;
        let instrument_id = bass.instrument.id;
        assert_eq!(
            studio
                .configure_sound_tool(bass_id, "instrument", instrument_id, None, "attack", "20",),
            Err(StudioError::InvalidSoundTool)
        );
        assert_eq!(studio.to_json(), before);
    }

    #[test]
    fn applies_stable_id_sound_tool_actions() {
        let mut studio = Studio::new();
        let bass = &studio.project().tracks[1];
        let clip_id = bass.clips[0].id;
        let event_id = bass.clips[0].events[0].id;
        let plan = crate::prompt::EditPlan {
            action: Action::Configure {
                track_id: bass.id,
                target: TrackRole::Bass,
                tool: "event",
                tool_id: event_id,
                clip_id: Some(clip_id),
                parameter: "velocity",
                value: "0.5".to_owned(),
            },
            summary: "Adjusted the bass event".to_owned(),
        };
        studio
            .apply_plan(0.0, 4.0, "soften the first bass event", plan)
            .expect("valid stable-ID action");
        assert_eq!(studio.project().tracks[1].clips[0].events[0].velocity, 0.5);
        assert!(
            studio
                .project()
                .to_json()
                .contains("\"type\":\"configure\"")
        );
    }

    #[test]
    fn composes_genre_prompt_from_generic_midi_and_sound_tools() {
        let mut studio = Studio::new();
        let original_bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Bass)
            .expect("seeded bass");
        let original_bass_id = original_bass.id;
        let original_bass_modulator = original_bass.modulators[0].clone();
        let summary = studio
            .apply_prompt(8.0, 16.0, "insert a dubstep drop here")
            .expect("valid edit");

        assert!(summary.contains("half-time drums"));
        assert_eq!(studio.project().tracks.len(), 4);
        assert_eq!(studio.project().edits.len(), 1);
        let drums = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Drums)
            .expect("drum track");
        let drum_clip = drums
            .clips
            .iter()
            .find(|clip| clip.label == "Half-time drums")
            .expect("authored drum MIDI");
        assert_eq!((drum_clip.start, drum_clip.end), (8.0, 16.0));
        assert!(drum_clip.events.iter().all(|event| event.kind == "note"));
        assert!(drum_clip.events.iter().any(|event| event.pitch == 36));
        assert!(drum_clip.events.iter().any(|event| event.pitch == 38));
        assert!(drum_clip.events.iter().any(|event| event.pitch == 41));
        assert!(drum_clip.events.iter().any(|event| event.pitch == 49));

        let original_bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.id == original_bass_id)
            .expect("original bass");
        assert_eq!(original_bass.instrument.preset, "Surge Bass");
        assert_eq!(original_bass.modulators[0].id, original_bass_modulator.id);
        assert_eq!(
            original_bass.modulators[0].shape,
            original_bass_modulator.shape
        );
        let bass_rest = original_bass
            .clips
            .iter()
            .find(|clip| clip.label == "Bass rest")
            .expect("old bass is cleared in the drop region");
        assert_eq!((bass_rest.start, bass_rest.end), (8.0, 16.0));
        assert!(bass_rest.events.is_empty());
        assert!(
            original_bass
                .clips
                .iter()
                .filter(|clip| clip.start < 16.0 && clip.end > 8.0)
                .all(|clip| clip.events.is_empty())
        );

        let bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| {
                track
                    .clips
                    .iter()
                    .any(|clip| clip.label == "Syncopated bass")
            })
            .expect("bass track");
        let drop_bass_id = bass.id;
        assert_eq!(bass.instrument.preset, "Surge Bass");
        assert!(
            bass.clips
                .iter()
                .any(|clip| clip.label == "Syncopated bass")
        );
        let wobble = bass.modulators.last().expect("authored bass modulation");
        assert_eq!(wobble.target, "instrument.cutoff");
        assert_eq!(wobble.shape, "square");
        assert_eq!(wobble.rate, 2.0);
        assert_eq!(wobble.depth, 0.72);
        assert_eq!(bass.modulators.len(), 2);
        assert_eq!(wobble.name, "AI modulation");
        let wobble_id = wobble.id;

        let action_json = studio.project().to_json();
        assert!(action_json.contains("\"type\":\"midi-clip\""));
        assert!(action_json.contains("\"type\":\"add-track\""));
        assert!(action_json.contains("\"type\":\"modulator\""));
        assert!(!action_json.contains("\"type\":\"drop\""));

        studio
            .apply_prompt(8.0, 16.0, "make the drop hit harder")
            .expect("valid refinement");
        let bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.id == drop_bass_id)
            .expect("refined bass");
        assert_eq!(studio.project().tracks.len(), 4);
        assert_eq!(bass.modulators.len(), 2);
        assert_eq!(bass.modulators.last().expect("wobble").id, wobble_id);
        assert_eq!(
            bass.clips
                .iter()
                .filter(|clip| clip.label == "Syncopated bass")
                .count(),
            1
        );
        let action_json = studio.project().to_json();
        assert_eq!(action_json.matches("\"type\":\"gain\"").count(), 1);
        assert!(action_json.contains("\"type\":\"configure\""));

        assert!(studio.undo());
        assert_eq!(studio.project().edits.len(), 1);
        assert!(studio.undo());
        assert_eq!(studio.project().tracks.len(), 3);
        assert!(studio.project().edits.is_empty());
        assert!(!studio.undo());
    }

    #[test]
    fn genre_refinement_reenables_its_region_owned_modulator() {
        let mut studio = Studio::new();
        studio
            .apply_prompt(8.0, 16.0, "insert a dubstep drop here")
            .expect("valid edit");
        let drop_bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| {
                track
                    .clips
                    .iter()
                    .any(|clip| clip.label == "Syncopated bass")
            })
            .expect("drop bass");
        let track_id = drop_bass.id;
        let modulator_id = drop_bass.modulators.last().expect("drop modulation").id;

        studio
            .configure_sound_tool(
                track_id,
                "modulator",
                modulator_id,
                None,
                "enabled",
                "false",
            )
            .expect("disable drop modulation");
        studio
            .apply_prompt(8.0, 16.0, "make the drop hit harder")
            .expect("valid refinement");

        let drop_bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .expect("same drop bass");
        assert!(
            drop_bass
                .modulators
                .iter()
                .find(|modulator| modulator.id == modulator_id)
                .expect("same drop modulation")
                .enabled
        );
    }

    #[test]
    fn genre_plan_targets_role_tracks_with_material_in_the_selection() {
        let mut studio = Studio::new();
        let original_bass_id = studio.project().tracks[1].id;
        let original_drums_id = studio.project().tracks[0].id;
        studio
            .apply_prompt(20.0, 24.0, "add a bass")
            .expect("later bass part");
        studio
            .apply_prompt(20.0, 24.0, "add drums")
            .expect("later drum part");
        let later_bass_id = studio
            .project()
            .tracks
            .iter()
            .rfind(|track| track.role == TrackRole::Bass)
            .expect("later bass")
            .id;
        let later_drums_id = studio
            .project()
            .tracks
            .iter()
            .rfind(|track| track.role == TrackRole::Drums)
            .expect("later drums")
            .id;

        studio
            .apply_prompt(8.0, 16.0, "insert a dubstep drop here")
            .expect("drop over the original parts");

        let track = |id| {
            studio
                .project()
                .tracks
                .iter()
                .find(|track| track.id == id)
                .expect("track by ID")
        };
        assert!(
            track(original_bass_id)
                .clips
                .iter()
                .any(|clip| clip.label == "Bass rest" && clip.events.is_empty())
        );
        assert!(
            !track(later_bass_id)
                .clips
                .iter()
                .any(|clip| clip.label == "Bass rest")
        );
        assert!(
            track(original_drums_id)
                .clips
                .iter()
                .any(|clip| clip.label == "Half-time drums")
        );
        assert!(
            !track(later_drums_id)
                .clips
                .iter()
                .any(|clip| clip.label == "Half-time drums")
        );
    }

    #[test]
    fn midi_removal_clears_instead_of_recomposing_the_selection() {
        let mut studio = Studio::new();
        let bass_id = studio.project().tracks[1].id;
        let summary = studio
            .apply_prompt(8.0, 16.0, "remove the bass MIDI clip")
            .expect("clear bass MIDI");

        assert!(summary.contains("Cleared"));
        let bass = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.id == bass_id)
            .expect("original bass");
        let rest = bass
            .clips
            .iter()
            .find(|clip| clip.label == "AI MIDI rest")
            .expect("silent replacement");
        assert_eq!((rest.start, rest.end), (8.0, 16.0));
        assert!(rest.events.is_empty());
        assert!(matches!(
            &studio.project().edits[0].action,
            Action::MidiClip {
                track_id,
                target: TrackRole::Bass,
                notes,
                ..
            } if *track_id == bass_id && notes.is_empty()
        ));
    }

    #[test]
    fn midi_prompt_creates_a_missing_role_before_writing_notes() {
        let mut studio = Studio::new();
        studio
            .apply_prompt(0.0, 4.0, "add a lead MIDI clip")
            .expect("create and author a missing lead");

        let lead = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Lead)
            .expect("lead track");
        assert!(lead.clips.iter().any(|clip| clip.label == "AI MIDI clip"));
        assert!(matches!(
            studio.project().edits[0].action,
            Action::Compound { ref actions }
                if matches!(actions.as_slice(), [Action::AddTrack { role: TrackRole::Lead }, Action::MidiClip { target: TrackRole::Lead, .. }])
        ));
    }

    #[test]
    fn midi_clip_replaces_only_its_relative_selection_region() {
        let mut studio = Studio::new();
        studio
            .apply_prompt(0.0, 8.0, "add a lead")
            .expect("existing lead");
        let lead_id = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Lead)
            .expect("lead track")
            .id;
        let plan = crate::prompt::EditPlan {
            action: Action::MidiClip {
                track_id: lead_id,
                target: TrackRole::Lead,
                label: "Replacement MIDI".to_owned(),
                start: 0.4,
                end: 1.0,
                loop_beats: 4.0,
                notes: vec![crate::prompt::MidiNote {
                    time: 0.0,
                    duration: 1.0,
                    pitch: 72,
                    velocity: 0.8,
                }],
            },
            summary: "Rewrote the lead MIDI".to_owned(),
        };
        studio
            .apply_plan(2.0, 6.0, "rewrite the lead MIDI", plan)
            .expect("MIDI replacement over existing lead");

        let clips = &studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Lead)
            .expect("lead track")
            .clips;
        assert_eq!(clips.len(), 3);
        assert_eq!(clips[0].label, "AI variation");
        assert!((clips[0].end - 3.6).abs() < 0.001);
        assert_eq!(clips[1].label, "Replacement MIDI");
        assert!((clips[1].start - 3.6).abs() < 0.001);
        assert_eq!(clips[1].end, 6.0);
        assert!((clips[1].source_start - 3.6).abs() < 0.001);
        assert_eq!(clips[2].label, "AI variation");
        assert_eq!((clips[2].start, clips[2].end), (6.0, 8.0));
        assert_eq!(clips[2].source_start, 0.0);
        assert_ne!(clips[0].id, clips[2].id);
        assert_eq!(clips[0].events[0].id, clips[2].events[0].id);
        assert!(clips.windows(2).all(|pair| pair[0].end <= pair[1].start));
    }

    #[test]
    fn rejects_midi_note_duration_longer_than_its_loop() {
        let mut studio = Studio::new();
        let bass_id = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Bass)
            .expect("bass track")
            .id;
        let before = studio.to_json();
        let plan = crate::prompt::EditPlan {
            action: Action::MidiClip {
                track_id: bass_id,
                target: TrackRole::Bass,
                label: "Unsafe short loop".to_owned(),
                start: 0.0,
                end: 1.0,
                loop_beats: 0.25,
                notes: vec![crate::prompt::MidiNote {
                    time: 0.0,
                    duration: 16.0,
                    pitch: 29,
                    velocity: 1.0,
                }],
            },
            summary: "Wrote a short bass loop".to_owned(),
        };

        assert_eq!(
            studio.apply_plan(0.0, 4.0, "write a short bass loop", plan),
            Err(StudioError::InvalidSoundTool)
        );
        assert_eq!(studio.to_json(), before);
    }

    #[test]
    fn compound_actions_commit_only_after_sequential_validation() {
        let mut studio = Studio::new();
        studio
            .apply_prompt(0.0, 8.0, "add a lead")
            .expect("existing lead");
        let lead = studio
            .project()
            .tracks
            .iter()
            .find(|track| track.role == TrackRole::Lead)
            .expect("lead track");
        let track_id = lead.id;
        let clip_id = lead.clips[0].id;
        let event_id = lead.clips[0].events[0].id;
        let before = studio.to_json();
        let before_next_id = studio.next_id;
        let before_history = studio.history.len();
        let stale_configuration = crate::prompt::EditPlan {
            action: Action::Compound {
                actions: vec![
                    Action::MidiClip {
                        track_id,
                        target: TrackRole::Lead,
                        label: "Replacement MIDI".to_owned(),
                        start: 0.0,
                        end: 1.0,
                        loop_beats: 4.0,
                        notes: vec![crate::prompt::MidiNote {
                            time: 0.0,
                            duration: 1.0,
                            pitch: 72,
                            velocity: 0.8,
                        }],
                    },
                    Action::Configure {
                        track_id,
                        target: TrackRole::Lead,
                        tool: "event",
                        tool_id: event_id,
                        clip_id: Some(clip_id),
                        parameter: "pitch",
                        value: "80".to_owned(),
                    },
                ],
            },
            summary: "Replaced then configured stale material".to_owned(),
        };

        assert_eq!(
            studio.apply_plan(
                0.0,
                8.0,
                "replace then retune the old lead",
                stale_configuration
            ),
            Err(StudioError::UnknownSoundTool)
        );
        assert_eq!(studio.to_json(), before);
        assert_eq!(studio.next_id, before_next_id);
        assert_eq!(studio.history.len(), before_history);
    }

    #[test]
    fn validates_prompt_selection_and_mix_changes() {
        let mut studio = Studio::new();
        assert_eq!(
            studio.apply_prompt(0.0, 2.0, "  "),
            Err(StudioError::EmptyPrompt)
        );
        assert_eq!(
            studio.apply_prompt(0.0, 2.0, &"x".repeat(MAX_PROMPT_CHARACTERS + 1)),
            Err(StudioError::InvalidPrompt)
        );
        assert_eq!(
            studio.apply_prompt(4.0, 2.0, "louder"),
            Err(StudioError::InvalidSelection)
        );
        assert_eq!(
            studio.set_mix(999, Some(1.0), None),
            Err(StudioError::UnknownTrack)
        );
        assert_eq!(
            studio.set_mix(1, Some(2.0), None),
            Err(StudioError::InvalidMix)
        );

        studio
            .set_mix(1, Some(0.5), Some(true))
            .expect("valid mix change");
        assert_eq!(studio.project().tracks[0].volume, 0.5);
        assert!(studio.project().tracks[0].muted);
    }

    #[test]
    fn rejects_modifier_targets_that_the_plan_does_not_create() {
        let mut studio = Studio::new();
        let initial = studio.to_json();
        let missing_lead = crate::prompt::EditPlan {
            action: Action::Gain {
                amount: 1.2,
                target: Some(TrackRole::Lead),
            },
            summary: "Lifted the lead".to_owned(),
        };
        assert_eq!(
            studio.apply_plan(4.0, 8.0, "make the lead louder", missing_lead),
            Err(StudioError::UnknownTrack)
        );
        assert_eq!(studio.to_json(), initial);

        for dependent in [
            Action::Instrument {
                preset: "Surge Lead",
                target: TrackRole::Lead,
            },
            Action::Modulator {
                parameter: "instrument.pitch".to_owned(),
                shape: "sine",
                rate: 5.0,
                depth: 0.2,
                target: TrackRole::Lead,
            },
        ] {
            let misordered = crate::prompt::EditPlan {
                action: Action::Compound {
                    actions: vec![
                        dependent,
                        Action::AddTrack {
                            role: TrackRole::Lead,
                        },
                    ],
                },
                summary: "Misordered lead setup".to_owned(),
            };
            assert_eq!(
                studio.apply_plan(4.0, 8.0, "configure then add a lead", misordered),
                Err(StudioError::UnknownTrack)
            );
            assert_eq!(studio.to_json(), initial);
        }

        let created_lead = crate::prompt::EditPlan {
            action: Action::Compound {
                actions: vec![
                    Action::AddTrack {
                        role: TrackRole::Lead,
                    },
                    Action::Gain {
                        amount: 1.2,
                        target: Some(TrackRole::Lead),
                    },
                ],
            },
            summary: "Added and lifted a lead".to_owned(),
        };
        studio
            .apply_plan(4.0, 8.0, "add a louder lead", created_lead)
            .expect("the plan creates its target");
        assert!(
            studio
                .project()
                .tracks
                .iter()
                .any(|track| track.role == TrackRole::Lead)
        );
    }

    #[test]
    fn escapes_user_text_in_json() {
        let mut studio = Studio::new();
        studio
            .apply_prompt(0.0, 2.0, "add a \"spark\"\nplease")
            .expect("valid edit");
        let json = studio.project().to_json();
        assert!(json.contains("add a \\\"spark\\\"\\nplease"));
        assert!(!json.contains("\"spark\"\nplease"));
    }

    #[test]
    fn reset_is_undoable() {
        let mut studio = Studio::new();
        studio
            .apply_prompt(1.0, 3.0, "add texture")
            .expect("valid edit");
        studio.reset();
        assert!(studio.project().edits.is_empty());
        assert!(studio.undo());
        assert_eq!(studio.project().edits.len(), 1);
    }

    #[test]
    fn serialized_studio_reports_real_undo_availability() {
        let mut studio = Studio::new();
        assert!(studio.to_json().contains("\"canUndo\":false"));
        studio
            .apply_prompt(0.0, 2.0, "brighter")
            .expect("valid edit");
        assert!(studio.to_json().contains("\"canUndo\":true"));
        assert!(studio.undo());
        assert!(studio.to_json().contains("\"canUndo\":false"));
    }

    #[test]
    fn replaces_a_gemini_graph_as_one_undoable_edit() {
        let mut studio = Studio::new();
        let before = studio.project().to_json();
        let plan = crate::prompt::EditPlan {
            action: Action::Configure {
                track_id: 2,
                target: TrackRole::Bass,
                tool: "instrument",
                tool_id: 201,
                clip_id: None,
                parameter: "preset",
                value: "Surge Lead".to_owned(),
            },
            summary: "Brightened the bass".to_owned(),
        };
        let mut session = Studio::from_project(studio.project().clone());
        session
            .apply_plan(4.0, 8.0, "brighten the bass", plan.clone())
            .expect("valid session edit");

        studio
            .replace_graph(
                session.project().clone(),
                4.0,
                8.0,
                "brighten the bass",
                plan,
            )
            .expect("valid graph replacement");
        assert_eq!(studio.project().edits.len(), 1);
        assert_eq!(studio.project().version, 2);
        assert_eq!(studio.project().tracks[1].instrument.preset, "Surge Lead");
        assert!(studio.undo());
        let mut restored = studio.project().clone();
        restored.version = 1;
        assert_eq!(restored.to_json(), before);
    }

    #[test]
    fn planner_projection_omits_materialized_history_payloads() {
        let mut studio = Studio::new();
        for _ in 0..64 {
            studio
                .apply_prompt(8.0, 16.0, "make the drop hit harder")
                .expect("repeatable genre edit");
        }

        let full_json = studio.project().to_json();
        let planner_json = studio.project().planner_json();
        let current_clip_count = studio
            .project()
            .tracks
            .iter()
            .map(|track| track.clips.len())
            .sum::<usize>();
        assert!(full_json.len() > 128 * 1024);
        assert!(planner_json.len() < 64 * 1024);
        assert_eq!(
            planner_json.matches("\"events\":[").count(),
            current_clip_count
        );
        assert!(!planner_json.contains("\"type\":\"midi-clip\""));
        assert!(!planner_json.contains("\"prompt\":"));
        assert!(planner_json.contains("\"regionalEdits\":["));
        assert!(planner_json.contains("\"type\":\"effect\""));
    }

    #[test]
    fn materialized_edit_log_is_bounded() {
        let mut studio = Studio::new();
        for _ in 0..EDIT_LOG_LIMIT + 8 {
            studio
                .apply_prompt(0.0, 2.0, "increase volume")
                .expect("valid edit");
        }

        assert_eq!(studio.project().edits.len(), EDIT_LOG_LIMIT);
        assert!(Project::from_json(&studio.project().to_json()).is_ok());
    }

    #[test]
    fn regional_edits_do_not_mutate_baseline_effect_chains() {
        let mut studio = Studio::new();
        let baseline: Vec<Vec<String>> = studio
            .project()
            .tracks
            .iter()
            .map(|track| {
                track
                    .effects
                    .iter()
                    .map(|effect| effect.name.clone())
                    .collect()
            })
            .collect();

        studio
            .apply_prompt(8.0, 16.0, "add echo to the chords")
            .expect("valid regional effect");
        studio
            .apply_prompt(16.0, 24.0, "insert a sick drop here")
            .expect("valid regional composition");

        let after: Vec<Vec<String>> = studio
            .project()
            .tracks
            .iter()
            .take(baseline.len())
            .map(|track| {
                track
                    .effects
                    .iter()
                    .map(|effect| effect.name.clone())
                    .collect()
            })
            .collect();
        assert_eq!(after, baseline);
        assert_eq!(studio.project().edits.len(), 2);
    }
}
