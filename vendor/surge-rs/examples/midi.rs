use midly::{MidiMessage, Smf, TrackEventKind};
use ringbuf::{HeapCons, HeapProd, HeapRb, traits::{Consumer, Observer, Producer, Split}};
use bus::{Bus, BusReader};
use surge_rs::synthesizer::EasySurge;

use std::{thread::{self}, time::{self, Duration}};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

const MIDI: &[u8] = include_bytes!("./data/waydrive.mid");

struct Event {
    time: u32,
    channel: u8,
    message: MidiMessage,
}

struct MidiPlayer {
    commands: Vec<Event>,
    playhead: u32,
}

impl MidiPlayer {
    fn new(pointer: &[u8]) -> Self {
        let midi = Smf::parse(pointer).unwrap();
        let mut commands = Vec::new();

        for (i, track) in midi.tracks.iter().enumerate() {
            let mut ticks = 0;
            for event in track {
                ticks += event.delta.as_int();

                if let TrackEventKind::Midi { message, .. } = event.kind {
                       commands.push(Event {
                           time: ticks,
                           channel: i as u8,
                           message
                       });
                }
            }
        }
        commands.sort_by_key(|e| e.time);

        Self { commands, playhead: 0 }
    }
}

// receive a bunch of water and filter out into paths.
// a river?
struct RiverFlow {
    synth: EasySurge,
    id: u8,
    comchain: BusReader<(MidiMessage, u8)>,
    producer: HeapProd::<f32>,
}

impl RiverFlow {
    fn new(synth: EasySurge, id: u8, comchain: BusReader<(MidiMessage, u8)>) -> (Self, HeapCons<f32>) {
        let (producer, consumer) = HeapRb::<f32>::new(1024).split();
        (Self { synth, id, comchain, producer }, consumer)
    }

    fn get(&mut self) -> [[f32; 32]; 2] {
        if let Ok(message) = self.comchain.try_recv() {
            if message.1 == self.id {
                match message.0 {
                    MidiMessage::NoteOn { key, vel } => { self.synth.play_note(0, key.as_int() as i8, vel.as_int() as i8, 0); }
                    MidiMessage::NoteOff { key, vel } => { self.synth.release_note(0, key.as_int() as i8, vel.as_int() as i8); }
                    MidiMessage::PitchBend { bend } => { self.synth.pitch_bend(0, bend.as_int() as i32); }
                    _ => {}
                }
            }
        }

        self.synth.process();
        self.synth.pull_buffer()
    }
}

fn main() {

    let (mut a_prd, mut a_cns) = HeapRb::<f32>::new(8192).split();

    let mut comchain = Bus::<(MidiMessage, u8)>::new(64);
    let synthcc1 = comchain.add_rx();
    let synthcc2 = comchain.add_rx();
    let synthcc3 = comchain.add_rx();
    let synthcc4 = comchain.add_rx();

    let mut synth1 = EasySurge::new(48000.0);
    let mut synth2 = EasySurge::new(48000.0);
    let mut synth3 = EasySurge::new(48000.0);
    let mut synth4 = EasySurge::new(48000.0);

    // s1 conf.
    synth1.set_parameter("A Osc 1 Type",                0.55).unwrap();
    synth1.set_parameter("A Osc 1 M1 Amount",           0.5).unwrap();
    synth1.set_parameter("A Osc 1 M1 Ratio",            0.12).unwrap();
    synth1.set_parameter("A Amp EG Attack",             0.1).unwrap();
    synth1.set_parameter("A Amp EG Release",            0.45).unwrap();
    synth1.set_parameter("A Portamento",                0.35).unwrap();
    synth1.set_parameter("Global Volume",               0.6).unwrap();
    // s2 conf.
    synth2.set_parameter("A Play Mode",                 0.8).unwrap();
    synth2.set_parameter("A Osc 1 Type",                0.55).unwrap();
    synth2.set_parameter("A Osc 1 M1 Amount",           0.5).unwrap();
    synth2.set_parameter("A Osc 1 M1 Ratio",            0.2).unwrap();
    synth2.set_parameter("A Osc 1 M2 Amount",           0.23).unwrap();
    synth2.set_parameter("A Osc 1 M2 Ratio",            0.7).unwrap();
    synth2.set_parameter("A Osc 1 Volume",              0.5).unwrap();
    synth2.set_parameter("A Osc 1 Retrigger",           1.00).unwrap();
    synth2.set_parameter("A Osc 2 Mute",                0.0).unwrap();
    synth2.set_parameter("A Osc 2 Type",                0.8).unwrap();
    synth2.set_parameter("A Osc 2 Exciter",             0.8).unwrap();
    synth2.set_parameter("A Osc 2 Stiffness",           0.5).unwrap();
    synth2.set_parameter("A Osc 2 Retrigger",           1.00).unwrap();
    synth2.set_parameter("A Osc 3 Mute",                0.0).unwrap();
    synth2.set_parameter("A Osc 3 Octave",              0.8).unwrap();
    synth2.set_parameter("A Osc 3 Volume",              0.9).unwrap();
    synth2.set_parameter("A Amp EG Attack",             0.05).unwrap();
    synth2.set_parameter("A Amp EG Release",            0.5).unwrap();
    synth2.set_parameter("A Portamento",                0.3).unwrap();
    synth2.set_parameter("A Pitch Bend Up Range",       0.3).unwrap();
    synth2.set_parameter("A Pitch Bend Down Range",     0.3).unwrap();
    synth2.set_parameter("Global Volume",               0.8).unwrap();
    // s3 conf.
    synth3.set_parameter("A Osc 1 Type",                0.9).unwrap();
    synth3.set_parameter("A Osc 2 Type",                0.6).unwrap();
    synth3.set_parameter("A Osc 2 Octave",              0.5).unwrap();
    synth3.set_parameter("A Osc 3 Mute",                0.0).unwrap();
    synth3.set_parameter("A Osc 3 Octave",              0.4).unwrap();
    synth3.set_parameter("A Filter 1 Type",             0.03).unwrap();
    synth3.set_parameter("A Filter 1 Cutoff",           0.5).unwrap();
    synth3.set_parameter("A Filter EG Decay",           0.55).unwrap();
    synth3.set_parameter("A Filter EG Sustain",         0.0).unwrap();
    synth3.set_parameter("A Filter 1 FEG Mod Amount",   0.8).unwrap();
    synth3.set_parameter("A Amp EG Release",            0.4).unwrap();
    synth3.set_parameter("A FM Routing",                0.2).unwrap();
    synth3.set_parameter("A FM Depth",                  0.6).unwrap();
    synth3.set_parameter("A Portamento",                0.45).unwrap();
    synth3.set_parameter("Global Volume",               0.8).unwrap();
    // s4 conf.
    synth4.set_parameter("A Osc 1 Type",                0.55).unwrap();
    synth4.set_parameter("A Osc 1 M1 Amount",           0.2).unwrap();
    synth4.set_parameter("A Osc 1 M1 Ratio",            0.2).unwrap();
    synth4.set_parameter("A Amp EG Attack",             0.0).unwrap();
    synth4.set_parameter("A Amp EG Release",            0.6).unwrap();
    synth4.set_parameter("A Portamento",                0.35).unwrap();
    synth4.set_parameter("Global Volume",               0.7).unwrap();

    let (mut controller1, mut datain1) = RiverFlow::new(synth1, 1, synthcc1);
    let (mut controller2, mut datain2) = RiverFlow::new(synth2, 2, synthcc2);
    let (mut controller3, mut datain3) = RiverFlow::new(synth3, 3, synthcc3);
    let (mut controller4, mut datain4) = RiverFlow::new(synth4, 4, synthcc4);

    // dispatcher.
    thread::spawn(move || {
        loop {
            if a_prd.vacant_len() >= 256 {
                let datalist = vec![
                    controller1.get(),
                    controller2.get(),
                    controller3.get(),
                    controller4.get(),
                ];

                let push = (0..32).flat_map(|i| {
                    [0, 1].map(|ch| {
                        let sum = datalist.iter()
                            .map(|co| co[ch][i])
                            .sum::<f32>();
                        sum / datalist.len() as f32
                    })
                });
                a_prd.push_iter(push);
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
    });
    eprintln!("spawn dispatcher thread.");

    // commander.
    thread::spawn(move || {
        let start = time::Instant::now();
        let mut idx = 0;

        let player = MidiPlayer::new(MIDI);

        loop {
            let elapsed = (start.elapsed().as_millis() as f32 * 1.75) as u32;

            let mut did = false;
            while idx < player.commands.len() && player.commands[idx].time <= elapsed {
                if !did {
                    did = true;
                    print!("{}", elapsed);
                }
                print!("|\tCH{}", player.commands[idx].channel);
                //println!("{}\t{}\t{}", elapsed, idx, player.commands[idx].channel);
                comchain.broadcast((
                        player.commands[idx].message,
                        player.commands[idx].channel,
                ));

                idx += 1;
            }
            if did { println!(); }
            if idx == player.commands.len() { break; }

            // 200 for swing mode!
            thread::sleep(Duration::from_millis(2));
        }
    });
    eprintln!("spawn commander thread.");

    // audio commander..
    let host = cpal::default_host();
    let device = host.default_output_device().expect("no audio output device.");
    let config = device.default_output_config().unwrap();

    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _| {
            let _ = a_cns.pop_slice(data);
        },
        |err| eprintln!("audio error: {}.", err),
        None,
    ).expect("general audio error.");
    eprintln!("spawn audio thread.");

    stream.play().unwrap();
    eprintln!("audio start.");

    loop {}
}
