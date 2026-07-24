use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
};

use surge_rs::glue::synthesizer::{SurgeId, SurgeSynthesizer};

use crate::model::{Effect, Instrument};

pub(crate) const BLOCK_SIZE: usize = 32;

// The alpha binding does not expose Surge's parameter count. This comfortably
// covers the current engine while from_synth_side_id rejects unused indices.
const MAX_NATIVE_PARAMETERS: i32 = 800;
const FILTER_LP12: f32 = 1.0 / 31.0;
const OSC_SINE: f32 = 1.0 / 11.0;
const OSC_SH_NOISE: f32 = 3.0 / 11.0;
const OSC_FM2: f32 = 6.0 / 11.0;
const OSC_MODERN: f32 = 8.0 / 11.0;

const NATIVE_PARAMETERS: &[(&str, &str)] = &[
    ("attack", "A Amp EG Attack"),
    ("release", "A Amp EG Release"),
    ("cutoff", "A Filter 1 Cutoff"),
    ("resonance", "A Filter 1 Resonance"),
    ("pitch", "A Pitch"),
];

const STARTER_PATCH_BASE: &[(&str, f32)] = &[
    ("A Filter 1 Type", FILTER_LP12),
    ("A Osc 1 Retrigger", 1.0),
    ("A Osc 2 Retrigger", 1.0),
    ("A Osc 3 Retrigger", 1.0),
    ("Global Volume", 1.0),
];

static SURGE_ENGINE_LOCK: Mutex<()> = Mutex::new(());

pub(crate) struct Engine {
    synth: SurgeSynthesizer,
    _guard: MutexGuard<'static, ()>,
    parameters: HashMap<String, i32>,
    effect_mix_parameters: HashMap<u64, String>,
    effect_parameters: HashMap<(u64, String), String>,
}

impl Engine {
    pub(crate) fn new(
        instrument: &Instrument,
        effects: &[Effect],
        effect_order: &[u64],
        sample_rate: f32,
    ) -> Result<Self, String> {
        let guard = SURGE_ENGINE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut synth = SurgeSynthesizer::new(sample_rate);
        synth.process();
        let mut engine = Self {
            _guard: guard,
            parameters: parameter_map(&synth),
            synth,
            effect_mix_parameters: HashMap::new(),
            effect_parameters: HashMap::new(),
        };
        engine.apply_preset(&instrument.preset)?;
        engine.set_instrument_parameters(instrument)?;
        if effects
            .iter()
            .any(|effect| effect.enabled && is_native_effect(&effect.name))
        {
            engine.apply_effects(effects, effect_order)?;
        }
        Ok(engine)
    }

    pub(crate) fn play_note(&mut self, key: u8, velocity: f32, note_id: u64) {
        self.synth.play_note(
            0,
            key.min(127) as i8,
            (velocity.clamp(0.0, 1.0) * 127.0).round() as i8,
            0,
            note_id as i32,
            0,
        );
    }

    pub(crate) fn release_note(&mut self, key: u8, note_id: u64) {
        self.synth
            .release_note(0, key.min(127) as i8, 0, note_id as i32);
    }

    pub(crate) fn set_parameter(&mut self, graph_name: &str, value: f32) -> Result<(), String> {
        let native_name = NATIVE_PARAMETERS
            .iter()
            .find_map(|(graph, native)| (*graph == graph_name).then_some(*native))
            .unwrap_or(graph_name);
        let index = self
            .parameters
            .get(native_name)
            .copied()
            .ok_or_else(|| format!("Surge XT parameter is unavailable: {native_name}"))?;
        let mut id = SurgeId::empty();
        if !self.synth.from_synth_side_id(index, &mut id) {
            return Err(format!("Surge XT rejected parameter: {native_name}"));
        }
        self.synth
            .set_parameter01(&mut id, value.clamp(0.0, 1.0), None, None);
        Ok(())
    }

    pub(crate) fn process(&mut self) -> [[f32; BLOCK_SIZE]; 2] {
        self.synth.process();
        self.synth.pull_buffer()
    }

    pub(crate) fn set_effect_mix(&mut self, effect_id: u64, value: f32) -> Result<(), String> {
        let Some(parameter) = self.effect_mix_parameters.get(&effect_id).cloned() else {
            return Ok(());
        };
        self.set_parameter(&parameter, value)
    }

    pub(crate) fn set_effect_parameter(
        &mut self,
        effect_id: u64,
        parameter: &str,
        value: f32,
    ) -> Result<(), String> {
        let Some(native) = self
            .effect_parameters
            .get(&(effect_id, parameter.to_owned()))
            .cloned()
        else {
            return Ok(());
        };
        self.set_parameter(&native, value)
    }

    fn set_instrument_parameters(&mut self, instrument: &Instrument) -> Result<(), String> {
        for (name, value) in [
            ("attack", instrument.attack),
            ("release", instrument.release),
            ("cutoff", instrument.cutoff),
            ("resonance", instrument.resonance),
            ("pitch", instrument.pitch),
        ] {
            if instrument.overrides(name) {
                self.set_parameter(name, value)?;
            }
        }
        Ok(())
    }

    pub(crate) fn instrument_parameter_value(&self, graph_name: &str) -> Option<f32> {
        let native_name = NATIVE_PARAMETERS
            .iter()
            .find_map(|(graph, native)| (*graph == graph_name).then_some(*native))
            .unwrap_or(graph_name);
        self.parameter_value(native_name)
    }

    fn apply_preset(&mut self, preset: &str) -> Result<(), String> {
        if let Some(factory) = crate::surge_presets::find(preset) {
            let mut data = std::fs::read(&factory.path)
                .map_err(|error| format!("could not read Surge XT preset {preset}: {error}"))?;
            self.synth.load_raw(&mut data, Some(true));
        } else {
            let preset = preset_parameters(preset)
                .ok_or_else(|| format!("unsupported Surge XT preset: {preset}"))?;
            for &(parameter, value) in STARTER_PATCH_BASE.iter().chain(preset) {
                self.set_parameter(parameter, value)?;
            }
        }
        // Oscillator types can change the names of their mode parameters.
        self.synth.process();
        self.parameters = parameter_map(&self.synth);
        Ok(())
    }

    fn apply_effects(&mut self, effects: &[Effect], effect_order: &[u64]) -> Result<(), String> {
        let slots = [
            "FX A1", "FX A2", "FX A3", "FX A4", "FX G1", "FX G2", "FX G3", "FX G4",
        ];
        let mut available = slots
            .into_iter()
            .filter(|slot| {
                self.parameter_value(&format!("{slot} FX Type"))
                    .is_some_and(|value| value < 0.02)
            })
            .collect::<Vec<_>>()
            .into_iter();
        for effect_id in effect_order {
            let Some(effect) = effects.iter().find(|effect| {
                effect.id == *effect_id && effect.enabled && is_native_effect(&effect.name)
            }) else {
                continue;
            };
            let type_index = effect_type_index(&effect.name)
                .ok_or_else(|| format!("unsupported Surge XT effect: {}", effect.name))?;
            let slot = available.next().ok_or_else(|| {
                "Surge XT has no free serial effect slots after loading the instrument preset"
                    .to_owned()
            })?;
            self.set_parameter(
                &format!("{slot} FX Type"),
                type_index as f32 / (SURGE_EFFECT_TYPES.len() - 1) as f32,
            )?;
            self.synth.process();
            self.parameters = parameter_map(&self.synth);
            let mix_parameter = format!("{slot} Mix");
            if self.parameters.contains_key(&mix_parameter) {
                self.set_parameter(&mix_parameter, effect.mix)?;
                self.effect_mix_parameters.insert(effect.id, mix_parameter);
            }
            for spec in crate::model::effect_parameter_specs(&effect.name) {
                let native = format!("{slot} {}", spec.native);
                if self.parameters.contains_key(&native) {
                    if effect
                        .parameter_overrides
                        .iter()
                        .any(|parameter| parameter == spec.name)
                        && let Some(value) = effect.parameters.get(spec.name)
                    {
                        self.set_parameter(&native, *value)?;
                    }
                    self.effect_parameters
                        .insert((effect.id, spec.name.to_owned()), native);
                }
            }
        }
        Ok(())
    }

    fn parameter_value(&self, name: &str) -> Option<f32> {
        let index = self.parameters.get(name)?;
        let mut id = SurgeId::empty();
        self.synth
            .from_synth_side_id(*index, &mut id)
            .then(|| self.synth.get_parameter01(&mut id))
    }
}

pub(crate) const SURGE_EFFECT_TYPES: &[&str] = &[
    "Off",
    "Delay",
    "Reverb 1",
    "Phaser",
    "Rotary Speaker",
    "Distortion",
    "EQ",
    "Frequency Shifter",
    "Conditioner",
    "Chorus",
    "Vocoder",
    "Reverb 2",
    "Flanger",
    "Ring Modulator",
    "Airwindows",
    "Neuron",
    "Graphic EQ",
    "Resonator",
    "CHOW",
    "Exciter",
    "Ensemble",
    "Combulator",
    "Nimbus",
    "Tape",
    "Treemonster",
    "Waveshaper",
    "Mid-Side Tool",
    "Spring Reverb",
    "Bonsai",
    "Audio Input",
    "Floaty Delay",
    "Convolution",
];

pub(crate) fn effect_type_index(name: &str) -> Option<usize> {
    let native = match name {
        "Reverb" | "Room" => "Reverb 2",
        "Echo" => "Delay",
        "Low-pass filter" => "EQ",
        "Punch compressor" => "Conditioner",
        "Drive" => "Distortion",
        "Shimmer" => "Nimbus",
        name => name,
    };
    SURGE_EFFECT_TYPES
        .iter()
        .position(|candidate| *candidate == native)
        .filter(|index| *index > 0)
}

pub(crate) fn is_native_effect(name: &str) -> bool {
    effect_type_index(name).is_some()
}

fn parameter_map(synth: &SurgeSynthesizer) -> HashMap<String, i32> {
    let mut map = HashMap::new();
    for index in 0..MAX_NATIVE_PARAMETERS {
        let mut id = SurgeId::empty();
        if synth.from_synth_side_id(index, &mut id) {
            map.insert(synth.get_parameter_name(&mut id), index);
        }
    }
    map
}

fn preset_parameters(preset: &str) -> Option<&'static [(&'static str, f32)]> {
    match preset {
        "Init" => Some(&[
            ("A Osc 1 Type", 0.0),
            ("A Osc 1 Volume", 0.72),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Kick" => Some(&[
            ("A Osc 1 Type", OSC_SINE),
            ("A Osc 1 Volume", 1.0),
            ("A Amp EG Attack", 0.0),
            ("A Amp EG Decay", 0.4),
            ("A Amp EG Sustain", 0.0),
            ("A Amp EG Release", 0.2),
            ("A Filter 1 Cutoff", 0.35),
            ("A Filter 1 Resonance", 0.15),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Snare" => Some(&[
            ("A Osc 1 Type", OSC_SH_NOISE),
            ("A Osc 1 Volume", 0.72),
            ("A Amp EG Attack", 0.0),
            ("A Amp EG Decay", 0.3),
            ("A Amp EG Sustain", 0.0),
            ("A Amp EG Release", 0.18),
            ("A Filter 1 Cutoff", 0.72),
            ("A Filter 1 Resonance", 0.1),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Closed Hat" => Some(&[
            ("A Osc 1 Type", OSC_SH_NOISE),
            ("A Osc 1 Volume", 0.42),
            ("A Amp EG Attack", 0.0),
            ("A Amp EG Decay", 0.24),
            ("A Amp EG Sustain", 0.0),
            ("A Amp EG Release", 0.1),
            ("A Filter 1 Cutoff", 0.9),
            ("A Filter 1 Resonance", 0.08),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Open Hat" => Some(&[
            ("A Osc 1 Type", OSC_SH_NOISE),
            ("A Osc 1 Volume", 0.4),
            ("A Amp EG Attack", 0.0),
            ("A Amp EG Decay", 0.38),
            ("A Amp EG Sustain", 0.0),
            ("A Amp EG Release", 0.28),
            ("A Filter 1 Cutoff", 0.88),
            ("A Filter 1 Resonance", 0.08),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Crash" => Some(&[
            ("A Osc 1 Type", OSC_SH_NOISE),
            ("A Osc 1 Volume", 0.36),
            ("A Amp EG Attack", 0.0),
            ("A Amp EG Decay", 0.64),
            ("A Amp EG Sustain", 0.0),
            ("A Amp EG Release", 0.58),
            ("A Filter 1 Cutoff", 0.84),
            ("A Filter 1 Resonance", 0.06),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Percussion" => Some(&[
            ("A Osc 1 Type", OSC_SH_NOISE),
            ("A Osc 1 Volume", 0.6),
            ("A Amp EG Attack", 0.0),
            ("A Amp EG Decay", 0.3),
            ("A Amp EG Sustain", 0.0),
            ("A Amp EG Release", 0.18),
            ("A Filter 1 Cutoff", 0.75),
            ("A Filter 1 Resonance", 0.1),
            ("A Osc 2 Mute", 1.0),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Bass" => Some(&[
            ("A Osc 1 Type", 0.0),
            ("A Osc 1 Volume", 0.95),
            ("A Osc 2 Mute", 0.0),
            ("A Osc 2 Type", OSC_SINE),
            ("A Osc 2 Octave", 0.25),
            ("A Osc 2 Volume", 0.7),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Pad" => Some(&[
            ("A Osc 1 Type", OSC_MODERN),
            ("A Osc 1 Volume", 0.9),
            ("A Osc 2 Mute", 0.0),
            ("A Osc 2 Type", OSC_SINE),
            ("A Osc 2 Volume", 0.75),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Lead" => Some(&[
            ("A Osc 1 Type", OSC_MODERN),
            ("A Osc 1 Volume", 0.78),
            ("A Osc 2 Mute", 0.0),
            ("A Osc 2 Type", 0.0),
            ("A Osc 2 Volume", 0.28),
            ("A Osc 3 Mute", 1.0),
        ]),
        "Surge Atmosphere" => Some(&[
            ("A Osc 1 Type", OSC_FM2),
            ("A Osc 1 Volume", 0.58),
            ("A Osc 2 Mute", 0.0),
            ("A Osc 2 Type", OSC_SINE),
            ("A Osc 2 Octave", 0.75),
            ("A Osc 2 Volume", 0.32),
            ("A Osc 3 Mute", 1.0),
        ]),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn binding_supports_multiple_headless_engines() {
        let instrument = crate::model::Project::demo().tracks[0].instrument.clone();
        for _ in 0..2 {
            let mut engine = Engine::new(&instrument, &[], &[], 16_000.0).expect("Surge XT engine");
            engine.process();
        }
    }

    #[test]
    fn factory_patch_loads_into_the_headless_engine() {
        let mut instrument = crate::model::Project::demo().tracks[2].instrument.clone();
        instrument.preset = "Factory/Pads/Flux Capacitor".to_owned();
        instrument.parameter_overrides.clear();
        let mut engine =
            Engine::new(&instrument, &[], &[], 16_000.0).expect("factory Surge XT patch");
        engine.play_note(60, 0.8, 1);
        let energy = (0..32)
            .map(|_| engine.process())
            .flat_map(|block| block[0])
            .map(f32::abs)
            .sum::<f32>();
        assert!(energy > 0.001, "factory patch rendered silence");
    }

    #[test]
    fn factory_patch_parameters_change_only_when_explicitly_overridden() {
        let mut instrument = crate::model::Project::demo().tracks[2].instrument.clone();
        instrument.preset = "Factory/Leads/Violini Solo".to_owned();
        instrument.parameter_overrides.clear();
        instrument.cutoff = 0.01;
        let native = Engine::new(&instrument, &[], &[], 16_000.0)
            .expect("factory Surge XT patch")
            .instrument_parameter_value("cutoff")
            .expect("native cutoff");
        assert!((native - instrument.cutoff).abs() > 0.01);

        instrument.parameter_overrides.push("cutoff".to_owned());
        let overridden = Engine::new(&instrument, &[], &[], 16_000.0)
            .expect("overridden factory Surge XT patch")
            .instrument_parameter_value("cutoff")
            .expect("overridden cutoff");
        assert!((overridden - instrument.cutoff).abs() < 0.001);
    }

    #[test]
    fn every_exposed_native_effect_loads_in_a_headless_slot() {
        let instrument = crate::model::Project::demo().tracks[1].instrument.clone();
        for name in SURGE_EFFECT_TYPES
            .iter()
            .skip(1)
            .filter(|name| **name != "Audio Input")
        {
            let effect = Effect {
                id: 77,
                name: (*name).to_owned(),
                mix: 0.5,
                cutoff_hz: None,
                resonance: None,
                enabled: true,
                parameters: crate::model::effect_parameter_specs(name)
                    .iter()
                    .map(|spec| (spec.name.to_owned(), spec.default))
                    .collect(),
                parameter_overrides: Vec::new(),
            };
            let mut engine = Engine::new(&instrument, &[effect], &[77], 16_000.0)
                .unwrap_or_else(|error| panic!("{name} did not load: {error}"));
            engine.process();
        }
    }

    #[test]
    fn friendly_effect_aliases_are_native_surge_effects() {
        for alias in [
            "Reverb",
            "Room",
            "Echo",
            "Low-pass filter",
            "Punch compressor",
            "Drive",
            "Shimmer",
        ] {
            assert!(is_native_effect(alias), "{alias} bypassed Surge XT");
        }
    }
}
