use crate::model::{Project, TrackRole};

#[derive(Clone, Debug, PartialEq)]
pub struct MidiNote {
    pub time: f32,
    pub duration: f32,
    pub pitch: u8,
    pub velocity: f32,
}

#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Compound {
        actions: Vec<Action>,
    },
    Gain {
        amount: f32,
        target: Option<TrackRole>,
    },
    Mute {
        target: Option<TrackRole>,
    },
    MidiClip {
        track_id: u64,
        target: TrackRole,
        label: String,
        start: f32,
        end: f32,
        loop_beats: f32,
        notes: Vec<MidiNote>,
    },
    AddTrack {
        role: TrackRole,
    },
    Instrument {
        waveform: &'static str,
        target: TrackRole,
    },
    Modulator {
        parameter: String,
        shape: &'static str,
        rate: f32,
        depth: f32,
        target: TrackRole,
    },
    Configure {
        track_id: u64,
        target: TrackRole,
        tool: &'static str,
        tool_id: u64,
        clip_id: Option<u64>,
        parameter: &'static str,
        value: String,
    },
    Effect {
        name: &'static str,
        mix: f32,
        target: Option<TrackRole>,
    },
    RemoveEffect {
        name: &'static str,
        target: Option<TrackRole>,
    },
    Filter {
        amount: f32,
        target: Option<TrackRole>,
    },
    Rhythm {
        amount: f32,
        target: Option<TrackRole>,
    },
    Tempo {
        bpm: u16,
    },
}

#[derive(Clone, Debug, PartialEq)]
pub struct EditPlan {
    pub action: Action,
    pub summary: String,
}

pub struct PromptEngine;

#[derive(Clone, Copy)]
struct PromptContext<'a> {
    project: &'a Project,
    selection_start: f32,
    selection_end: f32,
}

#[derive(Clone, Copy)]
struct DropBass {
    track_id: u64,
    instrument_id: u64,
    modulator_id: Option<u64>,
}

impl PromptEngine {
    #[must_use]
    pub fn interpret(prompt: &str, current_bpm: u16) -> EditPlan {
        Self::interpret_with_context(prompt, current_bpm, None)
    }

    #[must_use]
    pub fn interpret_project(
        prompt: &str,
        project: &Project,
        selection_start: f32,
        selection_end: f32,
    ) -> EditPlan {
        Self::interpret_with_context(
            prompt,
            project.bpm,
            Some(PromptContext {
                project,
                selection_start,
                selection_end,
            }),
        )
    }

    fn interpret_with_context(
        prompt: &str,
        current_bpm: u16,
        context: Option<PromptContext<'_>>,
    ) -> EditPlan {
        let normalized = prompt.trim().to_lowercase();
        let target = detect_role(&normalized);
        let target_name = target.map_or("the mix", TrackRole::display_name);
        let wants_reverb =
            contains_any(&normalized, &["reverb", "spacious", "space", "room", "wet"]);
        let wants_echo = contains_any(&normalized, &["delay", "echo"]);
        let wants_any_effect = contains_any(&normalized, &["effect", "effects", "fx"]);
        let wants_all_effects = contains_any(&normalized, &["effects", "all effect", "all fx"]);
        let wants_warm = contains_any(&normalized, &["dark", "warm", "warmth", "muffled"]);
        let wants_bright = contains_any(&normalized, &["bright", "open", "crisp", "sparkle"]);
        let wants_removal = contains_any(
            &normalized,
            &[
                "remove", "without", "take out", "take off", "turn off", "disable",
            ],
        );
        let wants_addition = contains_any(&normalized, &["add", "insert", "bring in"]);

        if contains_any(&normalized, &["drop", "dubstep"]) {
            return electronic_drop_plan(context);
        }

        if contains_any(
            &normalized,
            &["midi clip", "rewrite the notes", "recompose the notes"],
        ) {
            if let Some(target) = target {
                let midi_clip = midi_clip_for_role(target, "AI MIDI clip", 0.0, 1.0);
                if context.is_some_and(|context| {
                    !context
                        .project
                        .tracks
                        .iter()
                        .any(|track| track.role == target)
                }) {
                    return EditPlan {
                        action: Action::Compound {
                            actions: vec![Action::AddTrack { role: target }, midi_clip],
                        },
                        summary: format!(
                            "Added a {} and composed an explicit MIDI clip",
                            target.display_name()
                        ),
                    };
                }
                return EditPlan {
                    action: midi_clip,
                    summary: format!(
                        "Recomposed the {} as an explicit MIDI clip",
                        target.display_name()
                    ),
                };
            }
        }

        if contains_any(
            &normalized,
            &["lfo", "modulator", "modulation", "vibrato", "tremolo"],
        ) {
            if let Some(target) = target {
                let parameter = if contains_any(&normalized, &["pitch", "vibrato"]) {
                    "instrument.pitch"
                } else if contains_any(&normalized, &["volume", "level", "tremolo"]) {
                    "track.volume"
                } else {
                    "instrument.tone"
                };
                return EditPlan {
                    action: Action::Modulator {
                        parameter: parameter.to_owned(),
                        shape: "sine",
                        rate: if parameter == "instrument.pitch" {
                            5.0
                        } else {
                            0.5
                        },
                        depth: 0.2,
                        target,
                    },
                    summary: format!("Added moving modulation to the {}", target.display_name()),
                };
            }
        }

        if let (Some(waveform), Some(target)) = (waveform_name(&normalized), target) {
            if wants_addition {
                return EditPlan {
                    action: Action::Compound {
                        actions: vec![
                            Action::AddTrack { role: target },
                            Action::Instrument { waveform, target },
                        ],
                    },
                    summary: format!(
                        "Added a new {} part with a {waveform} waveform",
                        target.display_name()
                    ),
                };
            }
            return EditPlan {
                action: Action::Instrument { waveform, target },
                summary: format!(
                    "Changed the {} instrument to a {waveform} waveform",
                    target.display_name()
                ),
            };
        }

        let mut removed_effects = removable_effect_names(&normalized);
        if wants_removal && (wants_any_effect || !removed_effects.is_empty()) {
            if wants_all_effects || removed_effects.is_empty() {
                removed_effects = vec!["Effects"];
            }
            let effect_name = removed_effects
                .iter()
                .map(|name| name.to_lowercase())
                .collect::<Vec<_>>()
                .join(" and ");
            let mut actions = removed_effects
                .into_iter()
                .map(|name| Action::RemoveEffect { name, target })
                .collect::<Vec<_>>();
            let action = if actions.len() == 1 {
                actions.pop().expect("one removal action")
            } else {
                Action::Compound { actions }
            };
            return EditPlan {
                action,
                summary: format!("Removed {effect_name} from {target_name} in the selection"),
            };
        }

        if wants_reverb && wants_warm {
            return EditPlan {
                action: Action::Compound {
                    actions: vec![
                        Action::Effect {
                            name: "Reverb",
                            mix: 0.42,
                            target,
                        },
                        Action::Filter {
                            amount: -0.3,
                            target,
                        },
                    ],
                },
                summary: format!("Warmed {target_name} and added spacious reverb"),
            };
        }

        if contains_any(
            &normalized,
            &["louder", "increase volume", "turn up", "boost"],
        ) {
            return EditPlan {
                action: Action::Gain {
                    amount: 1.28,
                    target,
                },
                summary: format!("Lifted {target_name} in the selection"),
            };
        }

        if contains_any(
            &normalized,
            &[
                "quieter",
                "decrease volume",
                "lower volume",
                "turn down",
                "softer",
            ],
        ) {
            return EditPlan {
                action: Action::Gain {
                    amount: 0.72,
                    target,
                },
                summary: format!("Pulled {target_name} back in the selection"),
            };
        }

        if contains_any(&normalized, &["mute", "silence", "remove", "take out"]) {
            return EditPlan {
                action: Action::Mute { target },
                summary: format!("Muted {target_name} in the selection"),
            };
        }

        if wants_reverb {
            return EditPlan {
                action: Action::Effect {
                    name: "Reverb",
                    mix: 0.42,
                    target,
                },
                summary: format!("Added spacious reverb to {target_name}"),
            };
        }

        if wants_echo {
            return EditPlan {
                action: Action::Effect {
                    name: "Echo",
                    mix: 0.34,
                    target,
                },
                summary: format!("Added a tempo-synced echo to {target_name}"),
            };
        }

        if contains_any(&normalized, &["faster", "speed up", "more tempo"]) {
            let bpm = current_bpm.saturating_add(8).min(180);
            return EditPlan {
                action: Action::Tempo { bpm },
                summary: format!("Raised the session tempo to {bpm} BPM"),
            };
        }

        if contains_any(&normalized, &["slower", "slow down", "less tempo"]) {
            let bpm = current_bpm.saturating_sub(8).max(60);
            return EditPlan {
                action: Action::Tempo { bpm },
                summary: format!("Lowered the session tempo to {bpm} BPM"),
            };
        }

        if wants_bright {
            return EditPlan {
                action: Action::Filter {
                    amount: 0.3,
                    target,
                },
                summary: format!("Opened the tone of {target_name}"),
            };
        }

        if wants_warm {
            return EditPlan {
                action: Action::Filter {
                    amount: -0.3,
                    target,
                },
                summary: format!("Warmed and softened {target_name}"),
            };
        }

        if contains_any(
            &normalized,
            &["busy", "energy", "punch", "intense", "exciting"],
        ) {
            return EditPlan {
                action: Action::Rhythm {
                    amount: 0.35,
                    target,
                },
                summary: format!("Added rhythmic energy to {target_name}"),
            };
        }

        if contains_any(&normalized, &["simple", "sparse", "strip back", "minimal"]) {
            return EditPlan {
                action: Action::Rhythm {
                    amount: -0.35,
                    target,
                },
                summary: format!("Simplified the rhythm of {target_name}"),
            };
        }

        if wants_addition {
            if let Some(role) = target {
                return EditPlan {
                    action: Action::AddTrack { role },
                    summary: format!("Added a new {} part in the selection", role.display_name()),
                };
            }
        }

        creative_fallback(&normalized, target)
    }
}

fn electronic_drop_plan(context: Option<PromptContext<'_>>) -> EditPlan {
    let drop_bass = context.and_then(drop_bass_for_selection);
    let mut actions = Vec::new();
    if context.is_some() && drop_bass.is_none() {
        if let Some(track_id) = context.and_then(latest_bass_track_id) {
            actions.push(Action::MidiClip {
                track_id,
                target: TrackRole::Bass,
                label: "Bass rest".to_owned(),
                start: 0.0,
                end: 1.0,
                loop_beats: 4.0,
                notes: Vec::new(),
            });
        }
        actions.push(Action::AddTrack {
            role: TrackRole::Bass,
        });
    }
    actions.extend([
        Action::MidiClip {
            track_id: 0,
            target: TrackRole::Drums,
            label: "Half-time drums".to_owned(),
            start: 0.0,
            end: 1.0,
            loop_beats: 4.0,
            notes: midi_notes(&[
                (0.0, 0.25, 36, 1.0),
                (0.0, 0.25, 41, 0.58),
                (0.0, 0.5, 49, 0.72),
                (0.0, 0.125, 42, 0.55),
                (0.5, 0.125, 42, 0.48),
                (1.0, 0.125, 42, 0.58),
                (1.5, 0.125, 42, 0.5),
                (2.0, 0.25, 38, 0.95),
                (2.0, 0.125, 42, 0.62),
                (2.5, 0.125, 42, 0.5),
                (3.0, 0.25, 36, 0.88),
                (3.0, 0.125, 42, 0.6),
                (3.5, 0.125, 42, 0.52),
                (3.75, 0.125, 42, 0.46),
            ]),
        },
        Action::MidiClip {
            track_id: drop_bass.map_or(0, |bass| bass.track_id),
            target: TrackRole::Bass,
            label: "Syncopated bass".to_owned(),
            start: 0.0,
            end: 1.0,
            loop_beats: 4.0,
            notes: midi_notes(&[
                (0.0, 0.75, 29, 1.0),
                (0.75, 0.25, 29, 0.82),
                (1.25, 0.5, 32, 0.9),
                (2.0, 0.25, 29, 0.96),
                (2.5, 0.5, 27, 0.88),
                (3.25, 0.25, 29, 0.92),
                (3.75, 0.25, 36, 0.8),
            ]),
        },
    ]);
    if let Some(bass) = drop_bass {
        actions.push(Action::Configure {
            track_id: bass.track_id,
            target: TrackRole::Bass,
            tool: "instrument",
            tool_id: bass.instrument_id,
            clip_id: None,
            parameter: "waveform",
            value: "sawtooth".to_owned(),
        });
    } else {
        actions.push(Action::Instrument {
            waveform: "sawtooth",
            target: TrackRole::Bass,
        });
    }
    if let Some((track_id, tool_id)) =
        drop_bass.and_then(|bass| bass.modulator_id.map(|id| (bass.track_id, id)))
    {
        actions.extend([
            Action::Configure {
                track_id,
                target: TrackRole::Bass,
                tool: "modulator",
                tool_id,
                clip_id: None,
                parameter: "shape",
                value: "square".to_owned(),
            },
            Action::Configure {
                track_id,
                target: TrackRole::Bass,
                tool: "modulator",
                tool_id,
                clip_id: None,
                parameter: "rate",
                value: "2".to_owned(),
            },
            Action::Configure {
                track_id,
                target: TrackRole::Bass,
                tool: "modulator",
                tool_id,
                clip_id: None,
                parameter: "depth",
                value: "0.72".to_owned(),
            },
            Action::Configure {
                track_id,
                target: TrackRole::Bass,
                tool: "modulator",
                tool_id,
                clip_id: None,
                parameter: "enabled",
                value: "true".to_owned(),
            },
        ]);
    } else {
        actions.push(Action::Modulator {
            parameter: "instrument.tone".to_owned(),
            shape: "square",
            rate: 2.0,
            depth: 0.72,
            target: TrackRole::Bass,
        });
    }
    actions.push(Action::Effect {
        name: "Punch compressor",
        mix: 0.68,
        target: Some(TrackRole::Bass),
    });
    if drop_bass.is_none() {
        actions.push(Action::Gain {
            amount: 0.58,
            target: Some(TrackRole::Chords),
        });
    }
    EditPlan {
        action: Action::Compound { actions },
        summary: "Recomposed the selection with half-time drums and syncopated modulated bass"
            .to_owned(),
    }
}

fn latest_bass_track_id(context: PromptContext<'_>) -> Option<u64> {
    context
        .project
        .tracks
        .iter()
        .rev()
        .find(|track| track.role == TrackRole::Bass)
        .map(|track| track.id)
}

fn drop_bass_for_selection(context: PromptContext<'_>) -> Option<DropBass> {
    context.project.tracks.iter().rev().find_map(|track| {
        let owns_selection = track.role == TrackRole::Bass
            && track.clips.iter().any(|clip| {
                clip.label == "Syncopated bass"
                    && same_time(clip.start, context.selection_start)
                    && same_time(clip.end, context.selection_end)
            });
        owns_selection.then(|| DropBass {
            track_id: track.id,
            instrument_id: track.instrument.id,
            modulator_id: track
                .modulators
                .iter()
                .rev()
                .find(|modulator| modulator.target == "instrument.tone")
                .map(|modulator| modulator.id),
        })
    })
}

fn same_time(left: f32, right: f32) -> bool {
    (left - right).abs() <= 0.001
}

fn midi_clip_for_role(role: TrackRole, label: &str, start: f32, end: f32) -> Action {
    let notes = match role {
        TrackRole::Drums => midi_notes(&[
            (0.0, 0.25, 36, 0.92),
            (1.0, 0.25, 38, 0.78),
            (2.0, 0.25, 36, 0.88),
            (3.0, 0.25, 38, 0.78),
        ]),
        TrackRole::Bass => midi_notes(&[
            (0.0, 0.7, 33, 0.82),
            (1.0, 0.7, 33, 0.72),
            (2.0, 0.7, 36, 0.78),
            (3.0, 0.7, 31, 0.74),
        ]),
        TrackRole::Chords => midi_notes(&[
            (0.0, 1.85, 57, 0.62),
            (0.0, 1.85, 60, 0.56),
            (0.0, 1.85, 64, 0.54),
            (2.0, 1.85, 53, 0.6),
            (2.0, 1.85, 57, 0.54),
            (2.0, 1.85, 60, 0.52),
        ]),
        TrackRole::Lead => midi_notes(&[
            (0.0, 0.75, 69, 0.72),
            (1.0, 0.75, 76, 0.75),
            (2.0, 0.75, 71, 0.72),
            (3.0, 0.75, 67, 0.66),
        ]),
        TrackRole::Texture => midi_notes(&[(0.0, 3.8, 64, 0.5), (0.0, 3.4, 71, 0.38)]),
    };
    Action::MidiClip {
        track_id: 0,
        target: role,
        label: label.to_owned(),
        start,
        end,
        loop_beats: 4.0,
        notes,
    }
}

fn midi_notes(specs: &[(f32, f32, u8, f32)]) -> Vec<MidiNote> {
    specs
        .iter()
        .map(|(time, duration, pitch, velocity)| MidiNote {
            time: *time,
            duration: *duration,
            pitch: *pitch,
            velocity: *velocity,
        })
        .collect()
}

fn detect_role(prompt: &str) -> Option<TrackRole> {
    if contains_any(
        prompt,
        &[
            "drum",
            "drums",
            "beat",
            "beats",
            "percussion",
            "kick",
            "kicks",
            "snare",
            "snares",
            "hat",
            "hats",
            "hi-hat",
            "hi-hats",
        ],
    ) {
        Some(TrackRole::Drums)
    } else if contains_any(prompt, &["bass", "bassline", "low end", "sub", "sub-bass"]) {
        Some(TrackRole::Bass)
    } else if contains_any(
        prompt,
        &["chord", "chords", "pad", "pads", "harmony", "keys", "piano"],
    ) {
        Some(TrackRole::Chords)
    } else if contains_any(
        prompt,
        &[
            "lead", "leads", "melody", "melodies", "synth", "synths", "hook", "hooks",
        ],
    ) {
        Some(TrackRole::Lead)
    } else if contains_any(
        prompt,
        &["texture", "textures", "atmosphere", "ambience", "noise"],
    ) {
        Some(TrackRole::Texture)
    } else {
        None
    }
}

fn contains_any(value: &str, needles: &[&str]) -> bool {
    needles.iter().any(|needle| contains_term(value, needle))
}

fn contains_term(value: &str, term: &str) -> bool {
    value.match_indices(term).any(|(start, _)| {
        let before = value[..start].chars().next_back();
        let after = value[start + term.len()..].chars().next();
        before.is_none_or(|character| !character.is_alphanumeric())
            && after.is_none_or(|character| !character.is_alphanumeric())
    })
}

fn removable_effect_names(prompt: &str) -> Vec<&'static str> {
    let mut names = Vec::new();
    if contains_any(prompt, &["reverb"]) {
        names.push("Reverb");
    }
    if contains_any(prompt, &["room"]) {
        names.push("Room");
    }
    if contains_any(prompt, &["echo", "delay"]) {
        names.push("Echo");
    }
    if contains_any(prompt, &["chorus"]) {
        names.push("Chorus");
    }
    if contains_any(prompt, &["low-pass", "low pass", "filter"]) {
        names.push("Low-pass filter");
    }
    if contains_any(
        prompt,
        &["punch compressor", "compressor", "compression", "punch"],
    ) {
        names.push("Punch compressor");
    }
    if contains_any(prompt, &["shimmer"]) {
        names.push("Shimmer");
    }
    names
}

fn waveform_name(prompt: &str) -> Option<&'static str> {
    if contains_any(prompt, &["saw", "sawtooth"]) {
        Some("sawtooth")
    } else if contains_any(prompt, &["square", "pulse wave"]) {
        Some("square")
    } else if contains_any(prompt, &["triangle wave", "triangle waveform"]) {
        Some("triangle")
    } else if contains_any(prompt, &["sine", "sine wave"]) {
        Some("sine")
    } else {
        None
    }
}

fn creative_fallback(prompt: &str, target: Option<TrackRole>) -> EditPlan {
    let fingerprint = prompt.bytes().fold(0_u32, |sum, byte| {
        sum.wrapping_mul(31).wrapping_add(u32::from(byte))
    });
    let target_name = target.map_or("the mix", TrackRole::display_name);

    match fingerprint % 3 {
        0 => EditPlan {
            action: Action::Effect {
                name: "Shimmer",
                mix: 0.28,
                target,
            },
            summary: format!("Created a shimmering variation for {target_name}"),
        },
        1 => EditPlan {
            action: Action::Filter {
                amount: 0.18,
                target,
            },
            summary: format!("Shaped a fresh tonal variation for {target_name}"),
        },
        _ => EditPlan {
            action: Action::Rhythm {
                amount: 0.22,
                target,
            },
            summary: format!("Composed a new rhythmic variation for {target_name}"),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn understands_direct_and_complex_requests() {
        let volume = PromptEngine::interpret("increase volume on the drums", 112);
        assert_eq!(
            volume.action,
            Action::Gain {
                amount: 1.28,
                target: Some(TrackRole::Drums)
            }
        );

        let drop = PromptEngine::interpret("insert a dubstep drop here", 112);
        let Action::Compound { actions } = drop.action else {
            panic!("a genre request should be composed from generic actions");
        };
        assert!(matches!(
            &actions[0],
            Action::MidiClip {
                target: TrackRole::Drums,
                ..
            }
        ));
        assert!(matches!(
            &actions[1],
            Action::MidiClip {
                target: TrackRole::Bass,
                ..
            }
        ));
    }

    #[test]
    fn always_turns_a_nonempty_idea_into_a_change() {
        let plan = PromptEngine::interpret("make it feel like sunrise", 112);
        assert!(!plan.summary.is_empty());
    }

    #[test]
    fn clamps_tempo_changes() {
        assert_eq!(
            PromptEngine::interpret("faster", 178).action,
            Action::Tempo { bpm: 180 }
        );
        assert_eq!(
            PromptEngine::interpret("slower", 62).action,
            Action::Tempo { bpm: 60 }
        );
    }

    #[test]
    fn treats_added_effect_as_an_effect_on_the_target_track() {
        assert_eq!(
            PromptEngine::interpret("add reverb to the chords", 112).action,
            Action::Effect {
                name: "Reverb",
                mix: 0.42,
                target: Some(TrackRole::Chords),
            }
        );
    }

    #[test]
    fn distinguishes_effect_removal_from_track_removal() {
        assert_eq!(
            PromptEngine::interpret("remove reverb from the chords", 112).action,
            Action::RemoveEffect {
                name: "Reverb",
                target: Some(TrackRole::Chords),
            }
        );
        assert_eq!(
            PromptEngine::interpret("remove the chords", 112).action,
            Action::Mute {
                target: Some(TrackRole::Chords),
            }
        );

        for (prompt, name, target) in [
            ("remove echo from the lead", "Echo", TrackRole::Lead),
            ("remove room from the chords", "Room", TrackRole::Chords),
            ("remove chorus from the chords", "Chorus", TrackRole::Chords),
            (
                "remove the low-pass filter from bass",
                "Low-pass filter",
                TrackRole::Bass,
            ),
            (
                "remove punch compressor from drums",
                "Punch compressor",
                TrackRole::Drums,
            ),
            ("remove shimmer from texture", "Shimmer", TrackRole::Texture),
        ] {
            assert_eq!(
                PromptEngine::interpret(prompt, 112).action,
                Action::RemoveEffect {
                    name,
                    target: Some(target),
                }
            );
        }
        assert_eq!(
            PromptEngine::interpret("remove effects from the bass", 112).action,
            Action::RemoveEffect {
                name: "Effects",
                target: Some(TrackRole::Bass),
            }
        );
    }

    #[test]
    fn combines_warmth_and_space_in_one_edit() {
        let plan = PromptEngine::interpret("make the chords warm and spacious", 112);
        assert_eq!(
            plan.action,
            Action::Compound {
                actions: vec![
                    Action::Effect {
                        name: "Reverb",
                        mix: 0.42,
                        target: Some(TrackRole::Chords),
                    },
                    Action::Filter {
                        amount: -0.3,
                        target: Some(TrackRole::Chords),
                    },
                ],
            }
        );
        assert!(plan.summary.contains("Warmed"));
    }

    #[test]
    fn modifier_intents_take_precedence_over_structural_addition() {
        assert_eq!(
            PromptEngine::interpret("add a bass", 112).action,
            Action::AddTrack {
                role: TrackRole::Bass,
            }
        );
        assert_eq!(
            PromptEngine::interpret("add warmth to the bass", 112).action,
            Action::Filter {
                amount: -0.3,
                target: Some(TrackRole::Bass),
            }
        );
        assert_eq!(
            PromptEngine::interpret("add sparkle to the chords", 112).action,
            Action::Filter {
                amount: 0.3,
                target: Some(TrackRole::Chords),
            }
        );
        assert_eq!(
            PromptEngine::interpret("add punch to the drums", 112).action,
            Action::Rhythm {
                amount: 0.35,
                target: Some(TrackRole::Drums),
            }
        );
        assert_eq!(
            PromptEngine::interpret("add a sawtooth lead", 112).action,
            Action::Compound {
                actions: vec![
                    Action::AddTrack {
                        role: TrackRole::Lead,
                    },
                    Action::Instrument {
                        waveform: "sawtooth",
                        target: TrackRole::Lead,
                    },
                ],
            }
        );
    }

    #[test]
    fn role_aliases_match_whole_terms_only() {
        assert_eq!(
            PromptEngine::interpret("make that louder", 112).action,
            Action::Gain {
                amount: 1.28,
                target: None,
            }
        );
        assert_eq!(
            PromptEngine::interpret("make it subtle but louder", 112).action,
            Action::Gain {
                amount: 1.28,
                target: None,
            }
        );
    }

    #[test]
    fn understands_instrument_and_modulator_requests() {
        assert_eq!(
            PromptEngine::interpret("use a sawtooth waveform for the bass", 112).action,
            Action::Instrument {
                waveform: "sawtooth",
                target: TrackRole::Bass,
            }
        );
        assert_eq!(
            PromptEngine::interpret("add vibrato modulation to the lead", 112).action,
            Action::Modulator {
                parameter: "instrument.pitch".to_owned(),
                shape: "sine",
                rate: 5.0,
                depth: 0.2,
                target: TrackRole::Lead,
            }
        );
        assert_eq!(
            PromptEngine::interpret("add a sawtooth LFO to the bass", 112).action,
            Action::Modulator {
                parameter: "instrument.tone".to_owned(),
                shape: "sine",
                rate: 0.5,
                depth: 0.2,
                target: TrackRole::Bass,
            }
        );
    }
}
