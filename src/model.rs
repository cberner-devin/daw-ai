use std::fmt::Write;

use crate::prompt::{Action, PromptEngine};

const HISTORY_LIMIT: usize = 50;

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
    pub name: String,
    pub mix: f32,
    pub enabled: bool,
}

#[derive(Clone, Debug)]
pub struct Instrument {
    pub engine: String,
    pub waveform: String,
    pub attack: String,
    pub release: String,
}

#[derive(Clone, Debug)]
pub struct Clip {
    pub id: u64,
    pub label: String,
    pub start: f32,
    pub end: f32,
    pub style: String,
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

    #[must_use]
    pub fn to_json(&self) -> String {
        let mut output = String::with_capacity(4096);
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
            track.write_json(&mut output);
        }

        output.push_str("],\"edits\":[");
        for (index, edit) in self.edits.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            edit.write_json(&mut output);
        }
        output.push_str("]}");
        output
    }
}

impl Track {
    fn write_json(&self, output: &mut String) {
        write!(
            output,
            concat!(
                "{{\"id\":{},\"name\":{},\"role\":{},\"color\":{},",
                "\"volume\":{},\"muted\":{},\"instrument\":{{",
                "\"engine\":{},\"waveform\":{},\"attack\":{},\"release\":{}}},\"effects\":["
            ),
            self.id,
            json_string(&self.name),
            json_string(self.role.as_str()),
            json_string(&self.color),
            decimal(self.volume),
            self.muted,
            json_string(&self.instrument.engine),
            json_string(&self.instrument.waveform),
            json_string(&self.instrument.attack),
            json_string(&self.instrument.release)
        )
        .expect("writing to a string cannot fail");

        for (index, effect) in self.effects.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                "{{\"name\":{},\"mix\":{},\"enabled\":{}}}",
                json_string(&effect.name),
                decimal(effect.mix),
                effect.enabled
            )
            .expect("writing to a string cannot fail");
        }

        output.push_str("],\"clips\":[");
        for (index, clip) in self.clips.iter().enumerate() {
            if index > 0 {
                output.push(',');
            }
            write!(
                output,
                concat!(
                    "{{\"id\":{},\"label\":{},\"start\":{},\"end\":{},",
                    "\"style\":{}}}"
                ),
                clip.id,
                json_string(&clip.label),
                decimal(clip.start),
                decimal(clip.end),
                json_string(&clip.style)
            )
            .expect("writing to a string cannot fail");
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
}

impl Action {
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
            Self::Drop => write!(output, "{{\"type\":\"drop\",\"target\":\"all\"}}"),
            Self::AddTrack { role } => write!(
                output,
                "{{\"type\":\"add-track\",\"target\":{}}}",
                json_string(role.as_str())
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
    InvalidSelection,
    UnknownTrack,
    InvalidMix,
}

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
        Self {
            project: Project::demo(),
            history: Vec::new(),
            next_id: 100,
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
        let plan = PromptEngine::interpret(prompt, self.project.bpm);
        self.apply_plan(start, end, prompt, plan)
    }

    pub fn validate_edit(&self, start: f32, end: f32, prompt: &str) -> Result<(), StudioError> {
        let prompt = prompt.trim();
        if prompt.is_empty() {
            return Err(StudioError::EmptyPrompt);
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
        self.validate_action_targets(&plan.action)?;
        let prompt = prompt.trim();
        self.remember();

        self.apply_action(&plan.action, start, end);

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
        self.project.version += 1;
        Ok(summary)
    }

    fn validate_action_targets(&self, action: &Action) -> Result<(), StudioError> {
        let mut roles: Vec<TrackRole> =
            self.project.tracks.iter().map(|track| track.role).collect();
        collect_created_roles(action, &mut roles);
        validate_targets_exist(action, &roles)
    }

    fn apply_action(&mut self, action: &Action, start: f32, end: f32) {
        match action {
            Action::Compound { actions } => {
                for action in actions {
                    self.apply_action(action, start, end);
                }
            }
            Action::AddTrack { role } => self.add_track(*role, start, end, "AI variation"),
            Action::Drop => self.add_track(TrackRole::Lead, start, end, "Drop hook"),
            Action::Tempo { bpm } => self.project.bpm = *bpm,
            Action::Gain { .. }
            | Action::Mute { .. }
            | Action::Effect { .. }
            | Action::RemoveEffect { .. }
            | Action::Filter { .. }
            | Action::Rhythm { .. } => {}
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
        self.next_id += 1;
        id
    }

    fn add_track(&mut self, role: TrackRole, start: f32, end: f32, label: &str) {
        let track_id = self.take_id();
        let clip_id = self.take_id();
        let mut track = generated_track(track_id, role);
        track.clips = vec![Clip {
            id: clip_id,
            label: label.to_owned(),
            start,
            end,
            style: "generated".to_owned(),
        }];
        self.project.tracks.push(track);
    }
}

fn collect_created_roles(action: &Action, roles: &mut Vec<TrackRole>) {
    match action {
        Action::Compound { actions } => {
            for action in actions {
                collect_created_roles(action, roles);
            }
        }
        Action::AddTrack { role } => roles.push(*role),
        Action::Drop => roles.push(TrackRole::Lead),
        Action::Gain { .. }
        | Action::Mute { .. }
        | Action::Effect { .. }
        | Action::RemoveEffect { .. }
        | Action::Filter { .. }
        | Action::Rhythm { .. }
        | Action::Tempo { .. } => {}
    }
}

fn validate_targets_exist(action: &Action, roles: &[TrackRole]) -> Result<(), StudioError> {
    match action {
        Action::Compound { actions } => {
            for action in actions {
                validate_targets_exist(action, roles)?;
            }
            Ok(())
        }
        Action::Gain { target, .. }
        | Action::Mute { target }
        | Action::Effect { target, .. }
        | Action::RemoveEffect { target, .. }
        | Action::Filter { target, .. }
        | Action::Rhythm { target, .. } => {
            if target.is_some_and(|target| !roles.contains(&target)) {
                Err(StudioError::UnknownTrack)
            } else {
                Ok(())
            }
        }
        Action::Drop | Action::AddTrack { .. } | Action::Tempo { .. } => Ok(()),
    }
}

fn demo_track(id: u64, role: TrackRole, name: &str, color: &str) -> Track {
    let mut track = generated_track(id, role);
    track.name = name.to_owned();
    track.color = color.to_owned();
    track.clips = vec![Clip {
        id: id + 10,
        label: match role {
            TrackRole::Drums => "Pocket beat",
            TrackRole::Bass => "Warm pulse",
            TrackRole::Chords => "Four-chord glow",
            TrackRole::Lead => "Lead phrase",
            TrackRole::Texture => "Air layer",
        }
        .to_owned(),
        start: 0.0,
        end: 32.0,
        style: "foundation".to_owned(),
    }];
    track
}

fn generated_track(id: u64, role: TrackRole) -> Track {
    let (name, color, engine, waveform, attack, release, effects) = match role {
        TrackRole::Drums => (
            "AI Drum Rack",
            "#ffb86b",
            "Synthesized drum rack",
            "Noise + sine",
            "Fast",
            "Tight",
            vec![effect("Punch compressor", 0.34)],
        ),
        TrackRole::Bass => (
            "AI Bass",
            "#74e0bc",
            "Monophonic subtractive synth",
            "Rounded square",
            "8 ms",
            "180 ms",
            vec![effect("Low-pass filter", 0.46)],
        ),
        TrackRole::Chords => (
            "AI Chords",
            "#8ca9ff",
            "Polyphonic pad",
            "Triangle",
            "90 ms",
            "1.2 s",
            vec![effect("Chorus", 0.28), effect("Room", 0.2)],
        ),
        TrackRole::Lead => (
            "AI Lead",
            "#d99cff",
            "Monophonic lead synth",
            "Sawtooth",
            "12 ms",
            "320 ms",
            vec![effect("Echo", 0.24)],
        ),
        TrackRole::Texture => (
            "AI Texture",
            "#ff91ad",
            "Granular atmosphere",
            "Filtered noise",
            "600 ms",
            "2.4 s",
            vec![effect("Shimmer", 0.38)],
        ),
    };

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
            engine: engine.to_owned(),
            waveform: waveform.to_owned(),
            attack: attack.to_owned(),
            release: release.to_owned(),
        },
        effects,
        clips: Vec::new(),
    }
}

fn effect(name: &str, mix: f32) -> Effect {
    Effect {
        name: name.to_owned(),
        mix,
        enabled: true,
    }
}

fn decimal(value: f32) -> String {
    let mut value = format!("{value:.3}");
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
        assert!(project.to_json().contains("Neon First Light"));
    }

    #[test]
    fn applies_prompt_and_can_undo_it() {
        let mut studio = Studio::new();
        let summary = studio
            .apply_prompt(8.0, 16.0, "insert a sick drop here")
            .expect("valid edit");

        assert!(summary.contains("drop"));
        assert_eq!(studio.project().tracks.len(), 4);
        assert_eq!(studio.project().edits.len(), 1);
        assert!(studio.undo());
        assert_eq!(studio.project().tracks.len(), 3);
        assert!(studio.project().edits.is_empty());
        assert!(!studio.undo());
    }

    #[test]
    fn validates_prompt_selection_and_mix_changes() {
        let mut studio = Studio::new();
        assert_eq!(
            studio.apply_prompt(0.0, 2.0, "  "),
            Err(StudioError::EmptyPrompt)
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
            .expect("valid regional drop");

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
