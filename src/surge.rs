use std::{
    collections::HashMap,
    sync::{Mutex, MutexGuard},
};

use surge_rs::glue::synthesizer::{SurgeId, SurgeSynthesizer};

use crate::model::Instrument;

pub(crate) const BLOCK_SIZE: usize = 32;

// The alpha binding does not expose Surge's parameter count. This comfortably
// covers the current engine while from_synth_side_id rejects unused indices.
const MAX_NATIVE_PARAMETERS: i32 = 800;
const FILTER_LP12: f32 = 1.0 / 31.0;
const OSC_SINE: f32 = 1.0 / 11.0;
const OSC_FM3: f32 = 5.0 / 11.0;
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
}

impl Engine {
    pub(crate) fn new(instrument: &Instrument, sample_rate: f32) -> Result<Self, String> {
        let guard = SURGE_ENGINE_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let mut synth = SurgeSynthesizer::new(sample_rate);
        synth.process();
        let mut engine = Self {
            _guard: guard,
            parameters: parameter_map(&synth),
            synth,
        };
        engine.apply_preset(&instrument.preset)?;
        engine.set_instrument_parameters(instrument)?;
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

    fn set_instrument_parameters(&mut self, instrument: &Instrument) -> Result<(), String> {
        for (name, value) in [
            ("attack", instrument.attack),
            ("release", instrument.release),
            ("cutoff", instrument.cutoff),
            ("resonance", instrument.resonance),
            ("pitch", instrument.pitch),
        ] {
            self.set_parameter(name, value)?;
        }
        Ok(())
    }

    fn apply_preset(&mut self, preset: &str) -> Result<(), String> {
        let preset = preset_parameters(preset)
            .ok_or_else(|| format!("unsupported Surge XT preset: {preset}"))?;
        for &(parameter, value) in STARTER_PATCH_BASE.iter().chain(preset) {
            self.set_parameter(parameter, value)?;
        }
        // Oscillator types can change the names of their mode parameters.
        self.synth.process();
        self.parameters = parameter_map(&self.synth);
        Ok(())
    }
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
        "Surge Percussion" => Some(&[
            ("A Osc 1 Type", OSC_FM3),
            ("A Osc 1 Volume", 0.82),
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
            let mut engine = Engine::new(&instrument, 16_000.0).expect("Surge XT engine");
            engine.process();
        }
    }
}
