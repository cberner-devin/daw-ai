#![allow(unused_imports)]   // i want the staircase!
use crate::glue;
use glue::hcode;
use glue::hell_ffi;
use glue::parameter;
use glue::synthesizer;
// staircase.........;

use synthesizer::{SurgeSynthesizer, SurgeId};

use std::collections::HashMap;
use std::path::Path;

macro_rules! eslog {
    ($($arg:tt)*) => {
        let fn_name = {
            fn f() {}       // insane workaround to get the function name.
            fn g<T>(_: T) -> &'static str { std::any::type_name::<T>() }
            let name = g(f);
            &name[..name.len() - 3]
        };
        eprint!("[{:<16}]  ", fn_name.split("::").last().unwrap_or("broken."));
        eprintln!($($arg)*);
    }
}

#[derive(Debug)]
pub enum EasySurgeError {
    NoSuchParameter,
}

type Result<T> = std::result::Result<T, EasySurgeError>;
type ParamMap = HashMap<String, i32>;

pub struct EasySurge {
    synth: SurgeSynthesizer,
    map: HashMap<String, i32>,
}

impl EasySurge {
    pub fn new(sample_rate: f32) -> Self {
        let mut esurge = Self {
            synth: SurgeSynthesizer::new(sample_rate),
            map: HashMap::new(),
        };

        esurge.update_table();
        esurge
    }

    fn update_table(&mut self) {
        eslog!("table update WORKING...");
        self.process_say();

        let mut map = HashMap::new();
        for i in 0..800 {
            let mut id = SurgeId::empty();
            if self.synth.from_synth_side_id(i, &mut id) {
                let name = self.synth.get_parameter_name(&mut id);
                //eslog!("registered: {}.", name);
                map.insert(name, i);
            } else {
                //eslog!("no id for {}.", i);
            }
        }
        EasySurge::helper_mdiff(&map, &self.map);


        self.map = map;
        eslog!("table updated OK.");
    }

    pub fn load_patch(&mut self, data: &[u8]) {
        const XML_OFFSET: usize = 0x5c;

        let mut able = data[XML_OFFSET..].to_vec();
        self.synth.load_raw(&mut able, Some(false));
    }

    // TODO: maybe remove this, or alternatively call load_patch.
    pub fn load_patch_by_path(&mut self, fxp_path: &Path, name: &str) {
        self.synth.load_patch_by_path(fxp_path, -1, name, false);
    }

    pub fn query_parameter(&self, name: &str) -> Result<(f32, String, String)> {
        if let Some(&idx) = self.map.get(name) {
            let mut id = SurgeId::empty();
            self.synth.from_synth_side_id(idx, &mut id);

            Ok((
                self.synth.get_parameter01(&mut id),
                self.synth.get_parameter_display(&mut id),
                self.synth.get_parameter_display_alt(&mut id)
            ))
        } else {
            Err(EasySurgeError::NoSuchParameter)
        }
    }

    pub fn set_parameter(&mut self, name: &str, value: f32) -> Result<()> {
        eslog!("setting for \"{}\"...", name);
        if let Some(&idx) = self.map.get(name) {
            let mut id = SurgeId::empty();
            self.synth.from_synth_side_id(idx, &mut id);

            self.synth.set_parameter01(&mut id, value, None, None);

            self.update_table();    // TODO: only run this when specific parameters are changed.
            Ok(())
        } else {
            Err(EasySurgeError::NoSuchParameter)
        }
    }

    pub fn process(&mut self) {
        self.synth.process();
    }

    fn process_say(&mut self) {
        eslog!("internal process triggered.");
        self.process();
    }

    pub fn pull_buffer(&mut self) -> [[f32; 32]; 2] {
        self.synth.pull_buffer()
    }

    pub fn play_note(&mut self, channel: i8, key: i8, velocity: i8, detune: i8) {
        self.synth.play_note(channel, key, velocity, detune, 0, 0);
    }

    pub fn release_note(&mut self, channel: i8, key: i8, velocity: i8) {
        self.synth.release_note(channel, key, velocity, 0);
    }

    pub fn pitch_bend(&mut self, channel: i8, value: i32) {
        self.synth.pitch_bend(channel, value);
    }

    pub fn reset_pitch_bend(&mut self, channel: i8) {
        self.synth.reset_pitch_bend(channel);
    }

    pub fn shut_all_notes(&mut self) {
        self.synth.all_notes_off();
    }

    pub fn shut_all_sound(&mut self) {
        self.synth.all_sounds_off();
    }

    fn helper_mdiff(new: &ParamMap, old: &ParamMap) {
        let mut changed = false;

        for (k, v) in new {
            if !old.contains_key(k) {
                eslog!("REGISTRY add -> {}@{}", k, v);
                changed = true;
            }
        }

        for (k, v) in old {
            if !new.contains_key(k) {
                eslog!("REGISTRY rem -> {}@{}", k, v);
                changed = true;
            }
        }

        if !changed { eslog!("table is the same as before. wasted cycles!"); }
    }
}
