use crate::model::TrackRole;

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
    Drop,
    AddTrack {
        role: TrackRole,
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

impl PromptEngine {
    #[must_use]
    pub fn interpret(prompt: &str, current_bpm: u16) -> EditPlan {
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

        if contains_any(&normalized, &["drop"]) {
            return EditPlan {
                action: Action::Drop,
                summary: "Built a high-energy drop with a lead, denser drums, and bass movement"
                    .to_owned(),
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

        if contains_any(&normalized, &["add", "insert", "bring in"]) {
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

        let drop = PromptEngine::interpret("insert a sick drop here", 112);
        assert_eq!(drop.action, Action::Drop);
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
}
