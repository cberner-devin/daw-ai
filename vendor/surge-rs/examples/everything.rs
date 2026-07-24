use ringbuf::{HeapRb, traits::{Consumer, Observer, Producer, Split}};
use surge_rs::synthesizer::EasySurge;

use std::{thread, time::Duration};
use textplots::{Chart, Plot, Shape};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};

enum Command {
    NoteOn { note: i8 },
    NoteOff { note: i8 },
    AllOff,
}

fn main() {
    let (mut a_prd, mut a_cns) = HeapRb::<f32>::new(1024).split();
    let (mut v_prd, mut v_cns) = HeapRb::<f32>::new(1024).split();
    let (mut c_prd, mut c_cns) = HeapRb::<Command>::new(32).split();

    // synthcom.
    thread::spawn(move || {
        let mut synth = EasySurge::new(48000.0);
        synth.set_parameter("A Osc 1 Type",             0.45).unwrap();
        synth.set_parameter("A Osc 1 M1 Amount",        0.5).unwrap();
        synth.set_parameter("A Osc 1 M1 Ratio",         0.62).unwrap();
        synth.set_parameter("A Amp EG Attack",          0.25).unwrap();
        synth.set_parameter("A Amp EG Decay",           0.15).unwrap();
        synth.set_parameter("A Amp EG Sustain",         0.5).unwrap();
        synth.set_parameter("A Amp EG Release",         0.5).unwrap();
        synth.set_parameter("A Portamento",             0.35).unwrap();

        loop {
            while let Some(cmd) = c_cns.try_pop() { // good idea!
                match cmd {
                    Command::NoteOn { note } => { synth.play_note(0, note, 127, 0); }
                    Command::NoteOff { note } => { synth.release_note(0, note, 127); }
                    Command::AllOff => { synth.shut_all_notes(); }
                }
            }

            if a_prd.vacant_len() >= 64 {
                synth.process();
                let samples = synth.pull_buffer();

                let interleaved = samples[0].iter()
                    .zip(samples[1].iter())
                    .flat_map(|(&l, &r)| [l, r]);
                let _ = a_prd.push_iter(interleaved.clone());
                let _ = v_prd.push_iter(interleaved);
            } else {
                thread::sleep(Duration::from_millis(2));
            }
        }
    });
    eprintln!("spawn synthcom thread.");

    // draw.
    thread::spawn(move || {
        loop {
            let mut data = vec![0.0; 1024];
            let n = v_cns.pop_slice(&mut data);
            if n == 0 { eprintln!("no samples."); }

            let mut data: Vec<_> = data.iter()
                .step_by(2)
                .enumerate()
                .map(|(i, &y)| (i as f32, y))
                .collect();

            //print!("{}[2J", 27 as char);
            Chart::new(200, 100, 0.0, data.len() as f32)
                .lineplot(&Shape::Lines(&data))
                .display();

            thread::sleep(Duration::from_millis(1000));
         }
    });
    eprintln!("spawn draw thread.");

    // cpal.
    let host = cpal::default_host();
    let device = host.default_output_device().expect("no audio output device.");
    let config = device.default_output_config().unwrap();

    let stream = device.build_output_stream(
        &config.into(),
        move |data: &mut [f32], _| {
            let n = a_cns.pop_slice(data);
            //println!("{}", n");
            // maybe later.
        },
        |err| eprintln!("audio error: {}.", err),
        None,
    ).expect("general audio error.");
    eprintln!("spawn audio thread.");

    stream.play().unwrap();
    eprintln!("audio start.");

    let mut initial = 40;
    loop {
        let notes = [initial, initial + 4, initial + 7];
        for n in notes {
            c_prd.try_push(Command::NoteOn { note: n });
            println!("NOTEON: {}.", n);
            thread::sleep(Duration::from_millis(200));
        }
        thread::sleep(Duration::from_millis(600));

        c_prd.try_push(Command::AllOff);
        println!("ALLOFF.");

        thread::sleep(Duration::from_millis(50));
        for n in notes {
            c_prd.try_push(Command::NoteOn { note: n + 2 });
            println!("NOTEON: {}.", n);
        }
        thread::sleep(Duration::from_millis(50));
        for n in notes {
            c_prd.try_push(Command::NoteOff { note: n + 2 });
            println!("NOTEON: {}.", n);
        }

        thread::sleep(Duration::from_millis(50));
        for n in notes {
            c_prd.try_push(Command::NoteOn { note: n + 1 });
            println!("NOTEON: {}.", n);
        }
        thread::sleep(Duration::from_millis(100));
        for n in notes {
            c_prd.try_push(Command::NoteOff { note: n + 1 });
            println!("NOTEOFF: {}.", n);
        }

        initial += 8;
        thread::sleep(Duration::from_millis(200));
    }
}
