use std::fmt::{self, Write};

use crate::prompt::{Action, PromptEngine};

const HISTORY_LIMIT: usize = 50;
pub(crate) const EDIT_LOG_LIMIT: usize = 256;
pub(crate) const MAX_PROMPT_CHARACTERS: usize = 2_000;

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

#[derive(Clone, Debug)]
pub struct Effect {
    pub id: u64,
    pub name: String,
    pub mix: f32,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct Instrument {
    pub id: u64,
    pub engine: String,
    pub waveform: String,
    pub attack: f32,
    pub release: f32,
    pub tone: f32,
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
    pub start: f32,
    pub end: f32,
    pub prompt: String,
    pub summary: String,
    pub action: Action,
}

#[derive(Clone, Debug)]
pub struct Project {
    pub name: String,
    pub bpm: u16,
    pub duration: f32,
    pub version: u64,
    pub tracks: Vec<Track>,
    pub edits: Vec<Edit>,
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
        }
    }

    pub fn from_json(source: &str) -> Result<Self, ProjectFileError> {
        crate::project_file::parse_project(source)
    }

    fn highest_id(&self) -> u64 {
        let mut highest = self.edits.iter().map(|edit| edit.id).max().unwrap_or(0);
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
                "\"id\":{},\"type\":\"instrument\",\"engine\":{},\"waveform\":{},",
                "\"parameters\":{{\"attack\":{},\"release\":{},\"tone\":{}}}}},\"effects\":["
            ),
            self.id,
            json_string(&self.name),
            json_string(self.role.as_str()),
            json_string(&self.color),
            decimal(self.volume),
            self.muted,
            self.instrument.id,
            json_string(&self.instrument.engine),
            json_string(&self.instrument.waveform),
            decimal(self.instrument.attack),
            decimal(self.instrument.release),
            decimal(self.instrument.tone)
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
                    "\"enabled\":{},\"parameters\":{{\"mix\":{}}}}}"
                ),
                effect.id,
                json_string(&effect.name),
                effect.enabled,
                decimal(effect.mix)
            )
            .expect("writing to a string cannot fail");
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
                    "\"shape\":{},\"enabled\":{},\"target\":{},",
                    "\"parameters\":{{\"rate\":{},\"depth\":{}}}}}"
                ),
                modulator.id,
                json_string(&modulator.name),
                json_string(&modulator.shape),
                modulator.enabled,
                json_string(&modulator.target),
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
        write!(
            output,
            concat!(
                "{{\"id\":{},\"start\":{},\"end\":{},\"prompt\":{},",
                "\"summary\":{},\"action\":"
            ),
            self.id,
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

impl Action {
    fn has_regional_state(&self) -> bool {
        match self {
            Self::Compound { actions } => actions.iter().any(Self::has_regional_state),
            Self::Gain { .. }
            | Self::Mute { .. }
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
            Self::Instrument { waveform, target } => write!(
                output,
                concat!(
                    "{{\"type\":\"instrument\",\"name\":{},",
                    "\"value\":0.0,\"target\":{}}}"
                ),
                json_string(waveform),
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

    fn apply_action(&mut self, action: &Action, start: f32, end: f32) -> Result<(), StudioError> {
        match action {
            Action::Compound { actions } => {
                for action in actions {
                    self.apply_action(action, start, end)?;
                }
                Ok(())
            }
            Action::AddTrack { role } => {
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
            Action::Instrument { waveform, target } => {
                let track_index = role_action_track_index(&self.project, *target, None)
                    .ok_or(StudioError::UnknownTrack)?;
                self.project.tracks[track_index].instrument.waveform = (*waveform).to_owned();
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

    fn add_track(&mut self, role: TrackRole, start: f32, end: f32, label: &str) {
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
    match parameter {
        "waveform" if matches!(value, "sine" | "triangle" | "sawtooth" | "square") => {
            instrument.waveform = value.to_owned();
        }
        "attack" => instrument.attack = parse_range(value, 0.001, 2.0)?,
        "release" => instrument.release = parse_range(value, 0.02, 5.0)?,
        "tone" => instrument.tone = parse_range(value, 0.0, 1.0)?,
        _ => return Err(StudioError::InvalidSoundTool),
    }
    Ok(())
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
            label: "Instrument attack".to_owned(),
            minimum: 0.001,
            maximum: 2.0,
            scale: 0.5,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.release".to_owned(),
            label: "Instrument release".to_owned(),
            minimum: 0.02,
            maximum: 5.0,
            scale: 2.0,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.tone".to_owned(),
            label: "Instrument tone".to_owned(),
            minimum: 0.0,
            maximum: 1.0,
            scale: 1.0,
            mode: "add",
        },
        ModulationTarget {
            id: "instrument.pitch".to_owned(),
            label: "Instrument pitch".to_owned(),
            minimum: -2.0,
            maximum: 2.0,
            scale: 2.0,
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
    targets.extend(track.effects.iter().map(|effect| ModulationTarget {
        id: format!("effect:{}.mix", effect.id),
        label: format!("{} mix", effect.name),
        minimum: 0.0,
        maximum: 1.0,
        scale: 1.0,
        mode: "add",
    }));
    targets
}

fn valid_modulator_target(track: &Track, value: &str) -> bool {
    modulation_targets(track)
        .iter()
        .any(|target| target.id == value)
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
    let (name, color, engine, waveform, attack, release, tone, effect_specs, modulator) = match role
    {
        TrackRole::Drums => (
            "AI Drum Rack",
            "#ffb86b",
            "Synthesized drum rack",
            "sine",
            0.002,
            0.18,
            0.82,
            vec![("Punch compressor", 0.34)],
            ("Pulse envelope", "envelope", 2.0, 0.12, "instrument.tone"),
        ),
        TrackRole::Bass => (
            "AI Bass",
            "#74e0bc",
            "Monophonic subtractive synth",
            "square",
            0.008,
            0.18,
            0.32,
            vec![("Low-pass filter", 0.46)],
            ("Bass movement", "sine", 0.25, 0.18, "instrument.tone"),
        ),
        TrackRole::Chords => (
            "AI Chords",
            "#8ca9ff",
            "Polyphonic pad",
            "triangle",
            0.09,
            1.2,
            0.48,
            vec![("Chorus", 0.28), ("Room", 0.2)],
            ("Slow bloom", "triangle", 0.125, 0.16, "instrument.tone"),
        ),
        TrackRole::Lead => (
            "AI Lead",
            "#d99cff",
            "Monophonic lead synth",
            "sawtooth",
            0.012,
            0.32,
            0.64,
            vec![("Echo", 0.24)],
            ("Lead vibrato", "sine", 5.0, 0.08, "instrument.pitch"),
        ),
        TrackRole::Texture => (
            "AI Texture",
            "#ff91ad",
            "Granular atmosphere",
            "sine",
            0.6,
            2.4,
            0.7,
            vec![("Shimmer", 0.38)],
            ("Atmosphere drift", "random", 0.18, 0.22, "instrument.tone"),
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
            TrackRole::Drums => 0.82,
            TrackRole::Bass => 0.74,
            TrackRole::Chords => 0.58,
            TrackRole::Lead => 0.56,
            TrackRole::Texture => 0.44,
        },
        muted: false,
        instrument: Instrument {
            id: instrument_id,
            engine: engine.to_owned(),
            waveform: waveform.to_owned(),
            attack,
            release,
            tone,
        },
        effects,
        modulators: vec![Modulator {
            id: tool_id(id, 50),
            name: modulator.0.to_owned(),
            shape: modulator.1.to_owned(),
            rate: modulator.2,
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
    Effect {
        id,
        name: name.to_owned(),
        mix,
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
    let mut output = String::with_capacity(value.len() + 2);
    output.push('"');
    for character in value.chars() {
        match character {
            '"' => output.push_str("\\\""),
            '\\' => output.push_str("\\\\"),
            '\n' => output.push_str("\\n"),
            '\r' => output.push_str("\\r"),
            '\t' => output.push_str("\\t"),
            character if character.is_control() => {
                write!(output, "\\u{:04x}", u32::from(character))
                    .expect("writing to a string cannot fail");
            }
            character => output.push(character),
        }
    }
    output.push('"');
    output
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn demo_project_contains_a_playable_arrangement() {
        let project = Project::demo();
        assert_eq!(project.bpm, 112);
        assert_eq!(project.tracks.len(), 3);
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
            "\"source\":\"modulator:150\",\"target\":\"instrument.tone\",\"type\":\"control\""
        ));
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
            .configure_sound_tool(
                bass_id,
                "instrument",
                instrument_id,
                None,
                "waveform",
                "sawtooth",
            )
            .expect("configurable instrument");
        studio
            .configure_sound_tool(bass_id, "effect", effect_id, None, "mix", "0.72")
            .expect("configurable effect");
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
        assert_eq!(bass.instrument.waveform, "sawtooth");
        assert_eq!(bass.effects[0].mix, 0.72);
        assert_eq!(bass.modulators[0].target, "track.volume");
        assert_eq!(bass.clips[0].events[0].pitch, 40);
        assert_eq!(bass.clips[0].events[0].duration, 0.0625);
        assert!(studio.to_json().contains("\"duration\":0.0625"));
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
        assert!(targets.contains(&"instrument.tone".to_owned()));
        assert!(targets.contains(&"instrument.pitch".to_owned()));
        assert!(targets.contains(&"track.volume".to_owned()));
        assert!(targets.iter().any(|target| target.starts_with("effect:")));

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
                    parameter: "instrument.tone".to_owned(),
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
                        waveform: "sawtooth",
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
        assert_eq!(basses[0].instrument.waveform, "square");
        assert_eq!(basses[0].modulators.len(), 1);
        assert_eq!(basses[1].instrument.waveform, "sawtooth");
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
        assert_eq!(original_bass.instrument.waveform, "square");
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
        assert_eq!(bass.instrument.waveform, "sawtooth");
        assert!(
            bass.clips
                .iter()
                .any(|clip| clip.label == "Syncopated bass")
        );
        let wobble = bass.modulators.last().expect("authored bass modulation");
        assert_eq!(wobble.target, "instrument.tone");
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
                waveform: "sawtooth",
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
    fn replaces_a_codex_graph_as_one_undoable_edit() {
        let mut studio = Studio::new();
        let before = studio.project().to_json();
        let plan = crate::prompt::EditPlan {
            action: Action::Configure {
                track_id: 2,
                target: TrackRole::Bass,
                tool: "instrument",
                tool_id: 201,
                clip_id: None,
                parameter: "waveform",
                value: "sawtooth".to_owned(),
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
        assert_eq!(studio.project().tracks[1].instrument.waveform, "sawtooth");
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
