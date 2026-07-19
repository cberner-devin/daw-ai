use std::f32::consts::PI;

use crate::model::{Clip, ClipEvent, Project, Track, TrackRole};
use crate::prompt::Action;

pub(crate) const SAMPLE_RATE: u32 = 16_000;
pub(crate) const MAX_REGION_SECONDS: f32 = 16.0;
const FFT_SIZE: usize = 512;
const FFT_HOP: usize = 256;
const MEL_BANDS: usize = 64;
const AUTOMATION_SAMPLES: usize = SAMPLE_RATE as usize / 400;

#[derive(Clone, Copy, Default)]
struct EffectMixes {
    low_pass: f32,
    echo: f32,
    reverb: f32,
    room: f32,
    shimmer: f32,
    chorus: f32,
    compression: f32,
    filter_bypass: bool,
}

#[derive(Clone, Copy)]
struct AutomationFrame {
    gain: f32,
    tone_cutoff: f32,
    effect_filter_cutoff: f32,
    effect_filter_bypass: bool,
    echo: f32,
    reverb: f32,
    chorus: f32,
    compression: f32,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EffectStage {
    Echo,
    Reverb,
    Chorus,
    Compression,
}

struct PatternEvent<'a> {
    event: &'a ClipEvent,
    time: f32,
    duration: f32,
    velocity: f32,
    onset_index: usize,
    density_event: bool,
}

pub(crate) struct AudioRegion {
    pub samples: Vec<f32>,
    pub event_count: usize,
}

pub(crate) struct RegionAnalysis {
    pub peak: f32,
    pub rms: f32,
    pub zero_crossing_rate: f32,
    pub spectral_centroid_hz: f32,
    pub low_energy_ratio: f32,
    pub mid_energy_ratio: f32,
    pub high_energy_ratio: f32,
}

pub(crate) struct MelSpectrogram {
    pub png: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub frames: usize,
    pub minimum_db: f32,
    pub maximum_db: f32,
}

pub(crate) fn render_region(
    project: &Project,
    track_ids: &[u64],
    start: f32,
    end: f32,
) -> Result<AudioRegion, String> {
    if !start.is_finite()
        || !end.is_finite()
        || start < 0.0
        || end <= start
        || end > project.duration
        || end - start > MAX_REGION_SECONDS
    {
        return Err(format!(
            "analysis range must be inside the project and no longer than {MAX_REGION_SECONDS} seconds"
        ));
    }
    if track_ids.is_empty() {
        return Err("at least one channel ID is required".to_owned());
    }
    let sample_count = ((end - start) * SAMPLE_RATE as f32).ceil() as usize;
    let mut mix = vec![0.0; sample_count.max(1)];
    let mut event_count = 0;
    for &track_id in track_ids {
        let track = project
            .tracks
            .iter()
            .find(|track| track.id == track_id)
            .ok_or_else(|| format!("channel {track_id} does not exist"))?;
        if track.muted {
            continue;
        }
        let mut rendered = vec![0.0; mix.len()];
        render_track(project, track, start, end, &mut rendered, &mut event_count);
        process_track_audio(project, track, start, &mut rendered);
        for (output, sample) in mix.iter_mut().zip(rendered) {
            *output += sample;
        }
    }
    for sample in &mut mix {
        *sample = (*sample * 0.58).tanh();
    }
    Ok(AudioRegion {
        samples: mix,
        event_count,
    })
}

fn render_track(
    project: &Project,
    track: &Track,
    start: f32,
    end: f32,
    output: &mut [f32],
    event_count: &mut usize,
) {
    let beat_duration = 60.0 / project.bpm as f32;
    for clip in &track.clips {
        if clip.end <= start || clip.start >= end {
            continue;
        }
        let loop_duration = clip.loop_beats * beat_duration;
        if loop_duration <= 0.0 {
            continue;
        }
        let (onsets, pattern) = clip_pattern(clip);
        if onsets.is_empty() {
            continue;
        }
        let maximum_voice = pattern
            .iter()
            .map(|event| event.duration * beat_duration + maximum_release(track))
            .fold(0.0_f32, f32::max);
        let lookback = (start - maximum_voice).max(clip.start);
        let first_cycle = (((lookback - clip.source_start) / loop_duration).floor() as i64).max(0);
        let last_cycle = (((end - clip.source_start) / loop_duration).floor() as i64).max(0);
        for cycle in first_cycle..=last_cycle {
            for candidate in &pattern {
                let onset = clip.source_start
                    + cycle as f32 * loop_duration
                    + candidate.time * beat_duration;
                if onset < clip.start || onset >= clip.end || onset >= end {
                    continue;
                }
                let rhythm = regional_rhythm(project, track.role, onset);
                if candidate.density_event && rhythm <= 0.15 {
                    continue;
                }
                if !candidate.density_event
                    && rhythm < -0.15
                    && (cycle as usize * onsets.len() + candidate.onset_index) % 2 != 0
                {
                    continue;
                }
                let body_duration = (candidate.duration * beat_duration).max(0.01);
                let attack =
                    parameter_at(track, "instrument.attack", track.instrument.attack, onset);
                let release =
                    parameter_at(track, "instrument.release", track.instrument.release, onset);
                if onset + body_duration + release <= start {
                    continue;
                }
                *event_count += 1;
                render_event(
                    track,
                    candidate.event.id,
                    &candidate.event.kind,
                    candidate.event.pitch,
                    candidate.velocity,
                    onset,
                    body_duration,
                    attack,
                    release,
                    start,
                    output,
                );
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn render_event(
    track: &Track,
    event_id: u64,
    event_kind: &str,
    pitch: u8,
    velocity: f32,
    onset: f32,
    body_duration: f32,
    attack: f32,
    release: f32,
    region_start: f32,
    output: &mut [f32],
) {
    let first = (((onset - region_start) * SAMPLE_RATE as f32).floor() as i64).max(0) as usize;
    let last = (((onset + body_duration + release - region_start) * SAMPLE_RATE as f32).ceil()
        as usize)
        .min(output.len());
    let frequency = 440.0 * 2.0_f32.powf((pitch as f32 - 69.0) / 12.0);
    let drum_kind = if track.role == TrackRole::Drums || event_kind != "note" {
        drum_kind(event_kind, pitch)
    } else {
        "tone"
    };
    let role_level = match track.role {
        TrackRole::Drums => 0.5,
        TrackRole::Bass => 0.24,
        TrackRole::Chords => 0.09,
        TrackRole::Lead => 0.13,
        TrackRole::Texture => 0.07,
    };
    let level = velocity.clamp(0.01, 1.0) * role_level;
    let first_project_time = region_start + first as f32 / SAMPLE_RATE as f32;
    let mut phase = 2.0 * PI * frequency * (first_project_time - onset).max(0.0);
    for (index, sample) in output.iter_mut().enumerate().take(last).skip(first) {
        let project_time = region_start + index as f32 / SAMPLE_RATE as f32;
        let elapsed = project_time - onset;
        if elapsed < 0.0 {
            continue;
        }
        let envelope = voice_envelope(elapsed, attack, body_duration, release);
        let pitch_offset = parameter_at(track, "instrument.pitch", 0.0, project_time);
        let current_frequency = frequency * 2.0_f32.powf(pitch_offset / 12.0);
        let value = match drum_kind {
            "kick" | "tom" => {
                let sweep = if drum_kind == "kick" { 3.2 } else { 1.8 };
                let progress = (elapsed / body_duration).clamp(0.0, 1.0);
                let current = current_frequency * (sweep + (1.0 - sweep) * progress);
                phase += 2.0 * PI * current / SAMPLE_RATE as f32;
                phase.sin()
            }
            "snare" => {
                phase += 2.0 * PI * current_frequency / SAMPLE_RATE as f32;
                0.22 * waveform(&track.instrument.waveform, phase)
                    + 0.78 * deterministic_noise(event_id, index)
            }
            "hat" | "cymbal" => deterministic_noise(event_id, index),
            "percussion" => {
                phase += 2.0 * PI * current_frequency / SAMPLE_RATE as f32;
                0.45 * waveform(&track.instrument.waveform, phase)
                    + 0.55 * deterministic_noise(event_id, index)
            }
            _ => {
                phase += 2.0 * PI * current_frequency / SAMPLE_RATE as f32;
                waveform(&track.instrument.waveform, phase)
            }
        };
        let drum_scale = match drum_kind {
            "snare" => 0.55,
            "hat" => 0.2,
            "cymbal" => 0.16,
            "percussion" => 0.35,
            _ => 1.0,
        };
        *sample += value * envelope * level * drum_scale;
    }
}

fn clip_pattern(clip: &Clip) -> (Vec<f32>, Vec<PatternEvent<'_>>) {
    let mut onsets = clip
        .events
        .iter()
        .map(|event| event.time)
        .collect::<Vec<_>>();
    onsets.sort_by(f32::total_cmp);
    onsets.dedup();
    let mut pattern = clip
        .events
        .iter()
        .map(|event| PatternEvent {
            event,
            time: event.time,
            duration: event.duration,
            velocity: event.velocity,
            onset_index: onsets
                .iter()
                .position(|onset| *onset == event.time)
                .expect("event onset came from this clip"),
            density_event: false,
        })
        .collect::<Vec<_>>();
    for (onset_index, previous) in onsets.iter().copied().enumerate() {
        let next = onsets
            .get(onset_index + 1)
            .copied()
            .unwrap_or(onsets[0] + clip.loop_beats);
        let gap = next - previous;
        if gap < 0.5 {
            continue;
        }
        let midpoint = (previous + gap / 2.0) % clip.loop_beats;
        if onsets
            .iter()
            .any(|onset| (*onset - midpoint).abs() < 0.000_001)
        {
            continue;
        }
        for event in clip.events.iter().filter(|event| event.time == previous) {
            pattern.push(PatternEvent {
                event,
                time: midpoint,
                duration: (event.duration * 0.7).max(0.0625),
                velocity: (event.velocity * 0.82).max(0.01),
                onset_index,
                density_event: true,
            });
        }
    }
    (onsets, pattern)
}

fn maximum_release(track: &Track) -> f32 {
    if track
        .modulators
        .iter()
        .any(|modulator| modulator.enabled && modulator.target == "instrument.release")
    {
        5.0
    } else {
        track.instrument.release.max(0.001)
    }
}

fn voice_envelope(elapsed: f32, attack: f32, body: f32, release: f32) -> f32 {
    if elapsed < attack.max(0.001) {
        (elapsed / attack.max(0.001)).clamp(0.0, 1.0)
    } else if elapsed < body {
        1.0
    } else {
        (1.0 - (elapsed - body) / release.max(0.001)).clamp(0.0, 1.0)
    }
}

fn waveform(kind: &str, phase: f32) -> f32 {
    match kind {
        "square" => phase.sin().signum(),
        "triangle" => 2.0 / PI * phase.sin().asin(),
        "sawtooth" => {
            let cycles = phase / (2.0 * PI);
            2.0 * (cycles - (cycles + 0.5).floor())
        }
        _ => phase.sin(),
    }
}

fn deterministic_noise(event_id: u64, sample: usize) -> f32 {
    let mut value = event_id ^ (sample as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15);
    value ^= value >> 30;
    value = value.wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value ^= value >> 27;
    value = value.wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^= value >> 31;
    (value as u32 as f32 / u32::MAX as f32) * 2.0 - 1.0
}

fn drum_kind(event_kind: &str, pitch: u8) -> &'static str {
    if event_kind != "note" {
        return match event_kind {
            "kick" => "kick",
            "snare" => "snare",
            "tom" => "tom",
            "hat" => "hat",
            "cymbal" => "cymbal",
            _ => "percussion",
        };
    }
    match pitch {
        35 | 36 => "kick",
        37..=40 => "snare",
        41 | 43 | 45 | 47 | 48 | 50 => "tom",
        42 | 44 | 46 => "hat",
        49 | 51 | 52 | 53 | 55 | 57 | 59 => "cymbal",
        _ => "percussion",
    }
}

fn regional_rhythm(project: &Project, role: TrackRole, time: f32) -> f32 {
    let mut rhythm = 0.0;
    for edit in &project.edits {
        if time >= edit.start && time < edit.end {
            apply_regional_rhythm(&edit.action, role, &mut rhythm);
        }
    }
    rhythm
}

fn apply_regional_rhythm(action: &Action, role: TrackRole, rhythm: &mut f32) {
    match action {
        Action::Compound { actions } => {
            for action in actions {
                apply_regional_rhythm(action, role, rhythm);
            }
        }
        Action::Rhythm { amount, target } if target_matches(*target, role) => {
            *rhythm += *amount;
        }
        _ => {}
    }
}

fn parameter_at(track: &Track, target: &str, base: f32, time: f32) -> f32 {
    let amount = track
        .modulators
        .iter()
        .filter(|modulator| modulator.enabled && modulator.target == target)
        .map(|modulator| modulator_value(modulator, time))
        .sum::<f32>();
    let (minimum, maximum, scale, multiply) = match target {
        "instrument.attack" => (0.001, 2.0, 0.5, false),
        "instrument.release" => (0.02, 5.0, 2.0, false),
        "instrument.tone" => (0.0, 1.0, 1.0, false),
        "instrument.pitch" => (-2.0, 2.0, 2.0, false),
        "track.volume" => (0.0, 1.5, 1.0, true),
        _ if target.starts_with("effect:") && target.ends_with(".mix") => (0.0, 1.0, 1.0, false),
        _ => return base,
    };
    let value = if multiply {
        base * (1.0 + amount * scale)
    } else {
        base + amount * scale
    };
    value.clamp(minimum, maximum)
}

fn modulator_value(modulator: &crate::model::Modulator, time: f32) -> f32 {
    let phase = time * modulator.rate * PI * 2.0;
    let value = match modulator.shape.as_str() {
        "triangle" => 2.0 / PI * phase.sin().asin(),
        "square" => {
            if phase.sin() >= 0.0 {
                1.0
            } else {
                -1.0
            }
        }
        "envelope" => phase.sin().abs() * 2.0 - 1.0,
        "random" => {
            ((time * modulator.rate * 8.0).floor() * 91.17 + modulator.id as f32).sin() * 0.8
        }
        _ => phase.sin(),
    };
    value * modulator.depth
}

fn process_track_audio(project: &Project, track: &Track, start: f32, samples: &mut [f32]) {
    let frame_count = samples.len().div_ceil(AUTOMATION_SAMPLES);
    let frames = (0..frame_count)
        .map(|index| {
            let time = start + (index * AUTOMATION_SAMPLES) as f32 / SAMPLE_RATE as f32;
            automation_at(project, track, time)
        })
        .collect::<Vec<_>>();
    for (index, sample) in samples.iter_mut().enumerate() {
        *sample *= frames[index / AUTOMATION_SAMPLES].gain;
    }
    dynamic_low_pass(samples, &frames, |frame| frame.tone_cutoff, |_| false);
    dynamic_low_pass(
        samples,
        &frames,
        |frame| frame.effect_filter_cutoff,
        |frame| frame.effect_filter_bypass,
    );
    for stage in effect_stages(track) {
        match stage {
            EffectStage::Echo => {
                dynamic_delay_mix(samples, &frames, 30.0 / project.bpm as f32, |frame| {
                    frame.echo
                })
            }
            EffectStage::Reverb => {
                dynamic_delay_mix(samples, &frames, 0.085, |frame| frame.reverb);
            }
            EffectStage::Chorus => {
                dynamic_delay_mix(samples, &frames, 0.018, |frame| frame.chorus);
            }
            EffectStage::Compression => {
                for (index, sample) in samples.iter_mut().enumerate() {
                    let mix = frames[index / AUTOMATION_SAMPLES].compression;
                    let compressed = (*sample * 2.5).tanh() / 2.5_f32.tanh();
                    *sample += compressed * mix;
                }
            }
        }
    }
}

fn effect_stages(track: &Track) -> Vec<EffectStage> {
    let mut stages = track
        .routing
        .effect_order
        .iter()
        .filter_map(|effect_id| {
            track
                .effects
                .iter()
                .find(|effect| effect.id == *effect_id)
                .and_then(|effect| effect_stage(&effect.name))
        })
        .fold(Vec::new(), |mut stages, stage| {
            if !stages.contains(&stage) {
                stages.push(stage);
            }
            stages
        });
    for stage in [
        EffectStage::Echo,
        EffectStage::Reverb,
        EffectStage::Chorus,
        EffectStage::Compression,
    ] {
        if !stages.contains(&stage) {
            stages.push(stage);
        }
    }
    stages
}

fn effect_stage(name: &str) -> Option<EffectStage> {
    let normalized = name.to_ascii_lowercase();
    if normalized.contains("echo") || normalized.contains("delay") {
        Some(EffectStage::Echo)
    } else if matches!(normalized.as_str(), "reverb" | "room" | "shimmer") {
        Some(EffectStage::Reverb)
    } else if normalized.contains("chorus") {
        Some(EffectStage::Chorus)
    } else if normalized.contains("compressor") || normalized.contains("compression") {
        Some(EffectStage::Compression)
    } else {
        None
    }
}

fn automation_at(project: &Project, track: &Track, time: f32) -> AutomationFrame {
    let clip_active = track
        .clips
        .iter()
        .any(|clip| time >= clip.start && time < clip.end);
    let mut gain = if clip_active {
        parameter_at(track, "track.volume", track.volume, time)
    } else {
        0.0
    };
    let instrument_tone = parameter_at(track, "instrument.tone", track.instrument.tone, time);
    let mut filter = 0.0;
    let mut effects = EffectMixes::default();
    for effect in track.effects.iter().filter(|effect| effect.enabled) {
        let target = format!("effect:{}.mix", effect.id);
        apply_effect(
            &effect.name,
            parameter_at(track, &target, effect.mix, time),
            &mut effects,
        );
    }
    for edit in &project.edits {
        if time >= edit.start && time < edit.end {
            apply_regional_automation(
                &edit.action,
                track.role,
                &mut gain,
                &mut filter,
                &mut effects,
            );
        }
    }
    let role_filter = base_filter_for_role(track.role);
    let tone_cutoff =
        (role_filter * (0.7 + instrument_tone * 0.6) * (1.0 + filter)).clamp(180.0, 9_000.0);
    let effect_filter_cutoff = if effects.low_pass <= 0.0 {
        20_000.0
    } else {
        let wet_cutoff = (role_filter * 0.35).clamp(180.0, 9_000.0);
        20_000.0 * (wet_cutoff / 20_000.0).powf(effects.low_pass.clamp(0.0, 1.0))
    };
    AutomationFrame {
        gain,
        tone_cutoff,
        effect_filter_cutoff,
        effect_filter_bypass: effects.filter_bypass || effects.low_pass <= 0.0,
        echo: (effects.echo * 0.55).min(0.6),
        reverb: (effects.reverb.max(effects.room).max(effects.shimmer) * 0.7).min(0.6),
        chorus: (effects.chorus * 0.5).min(0.5),
        compression: (effects.compression * 0.45).min(0.5),
    }
}

fn apply_regional_automation(
    action: &Action,
    role: TrackRole,
    gain: &mut f32,
    filter: &mut f32,
    effects: &mut EffectMixes,
) {
    match action {
        Action::Compound { actions } => {
            for action in actions {
                apply_regional_automation(action, role, gain, filter, effects);
            }
        }
        Action::Gain { amount, target } if target_matches(*target, role) => *gain *= *amount,
        Action::Mute { target } if target_matches(*target, role) => *gain = 0.0,
        Action::Filter { amount, target } if target_matches(*target, role) => {
            *filter += *amount;
            effects.filter_bypass = false;
        }
        Action::Effect { name, mix, target } if target_matches(*target, role) => {
            apply_effect(name, *mix, effects);
        }
        Action::RemoveEffect { name, target } if target_matches(*target, role) => {
            remove_effect(name, filter, effects);
        }
        _ => {}
    }
}

fn target_matches(target: Option<TrackRole>, role: TrackRole) -> bool {
    target.is_none_or(|target| target == role)
}

fn apply_effect(name: &str, mix: f32, effects: &mut EffectMixes) {
    let normalized = name.to_ascii_lowercase();
    if normalized.contains("echo") || normalized.contains("delay") {
        effects.echo = effects.echo.max(mix);
    }
    if normalized == "reverb" {
        effects.reverb = effects.reverb.max(mix);
    }
    if normalized == "room" {
        effects.room = effects.room.max(mix);
    }
    if normalized == "shimmer" {
        effects.shimmer = effects.shimmer.max(mix);
    }
    if normalized.contains("chorus") {
        effects.chorus = effects.chorus.max(mix);
    }
    if normalized.contains("compressor") || normalized.contains("compression") {
        effects.compression = effects.compression.max(mix);
    }
    if normalized.contains("low-pass")
        || normalized.contains("low pass")
        || normalized.contains("filter")
    {
        effects.low_pass = effects.low_pass.max(mix);
        effects.filter_bypass = false;
    }
}

fn remove_effect(name: &str, filter: &mut f32, effects: &mut EffectMixes) {
    let normalized = name.to_ascii_lowercase();
    let remove_all = matches!(normalized.as_str(), "effect" | "effects" | "fx");
    if normalized.contains("echo") || normalized.contains("delay") || remove_all {
        effects.echo = 0.0;
    }
    if normalized == "reverb" || remove_all {
        effects.reverb = 0.0;
    }
    if normalized == "room" || remove_all {
        effects.room = 0.0;
    }
    if normalized == "shimmer" || remove_all {
        effects.shimmer = 0.0;
    }
    if remove_all {
        *filter = 0.0;
    }
    if normalized.contains("chorus") || remove_all {
        effects.chorus = 0.0;
    }
    if normalized.contains("compressor") || normalized.contains("compression") || remove_all {
        effects.compression = 0.0;
    }
    if normalized.contains("low-pass")
        || normalized.contains("low pass")
        || normalized.contains("filter")
        || (remove_all && effects.low_pass > 0.0)
    {
        effects.low_pass = 0.0;
        *filter = 0.0;
        effects.filter_bypass = true;
    }
}

fn base_filter_for_role(role: TrackRole) -> f32 {
    match role {
        TrackRole::Drums => 9_000.0,
        TrackRole::Bass => 1_200.0,
        TrackRole::Chords => 2_800.0,
        TrackRole::Lead => 3_600.0,
        TrackRole::Texture => 4_200.0,
    }
}

fn dynamic_low_pass(
    samples: &mut [f32],
    frames: &[AutomationFrame],
    cutoff: impl Fn(&AutomationFrame) -> f32,
    bypass: impl Fn(&AutomationFrame) -> bool,
) {
    let mut filtered = 0.0;
    for (index, sample) in samples.iter_mut().enumerate() {
        let frame = &frames[index / AUTOMATION_SAMPLES];
        let alpha = 1.0 - (-2.0 * PI * cutoff(frame) / SAMPLE_RATE as f32).exp();
        filtered += alpha * (*sample - filtered);
        if !bypass(frame) {
            *sample = filtered;
        }
    }
}

fn dynamic_delay_mix(
    samples: &mut [f32],
    frames: &[AutomationFrame],
    delay_seconds: f32,
    mix: impl Fn(&AutomationFrame) -> f32,
) {
    let delay = (delay_seconds * SAMPLE_RATE as f32).round() as usize;
    if delay == 0 || delay >= samples.len() {
        return;
    }
    for index in delay..samples.len() {
        let source = index - delay;
        samples[index] += samples[source] * mix(&frames[source / AUTOMATION_SAMPLES]);
    }
}

pub(crate) fn analyze(region: &AudioRegion) -> RegionAnalysis {
    let peak = region
        .samples
        .iter()
        .copied()
        .map(f32::abs)
        .fold(0.0, f32::max);
    let rms = if region.samples.is_empty() {
        0.0
    } else {
        (region
            .samples
            .iter()
            .map(|sample| sample * sample)
            .sum::<f32>()
            / region.samples.len() as f32)
            .sqrt()
    };
    let zero_crossings = region
        .samples
        .windows(2)
        .filter(|pair| pair[0].is_sign_positive() != pair[1].is_sign_positive())
        .count();
    let zero_crossing_rate = zero_crossings as f32 / region.samples.len().max(1) as f32;
    let spectrum = average_spectrum(&region.samples);
    let total = spectrum.iter().sum::<f32>().max(f32::EPSILON);
    let mut weighted = 0.0;
    let mut low = 0.0;
    let mut mid = 0.0;
    let mut high = 0.0;
    for (bin, power) in spectrum.iter().copied().enumerate() {
        let frequency = bin as f32 * SAMPLE_RATE as f32 / FFT_SIZE as f32;
        weighted += frequency * power;
        if frequency < 250.0 {
            low += power;
        } else if frequency < 2_500.0 {
            mid += power;
        } else {
            high += power;
        }
    }
    RegionAnalysis {
        peak,
        rms,
        zero_crossing_rate,
        spectral_centroid_hz: weighted / total,
        low_energy_ratio: low / total,
        mid_energy_ratio: mid / total,
        high_energy_ratio: high / total,
    }
}

fn average_spectrum(samples: &[f32]) -> Vec<f32> {
    let frame_count = frame_count(samples.len());
    let stride = (frame_count / 64).max(1);
    let mut spectrum = vec![0.0; FFT_SIZE / 2 + 1];
    let mut measured = 0;
    for frame in (0..frame_count).step_by(stride) {
        let powers = frame_power(samples, frame * FFT_HOP);
        for (total, power) in spectrum.iter_mut().zip(powers) {
            *total += power;
        }
        measured += 1;
    }
    if measured > 0 {
        for power in &mut spectrum {
            *power /= measured as f32;
        }
    }
    spectrum
}

pub(crate) fn mel_spectrogram(region: &AudioRegion) -> MelSpectrogram {
    let frames = frame_count(region.samples.len());
    let filters = mel_filters();
    let mut values = vec![vec![0.0; MEL_BANDS]; frames];
    let mut maximum_db = -120.0_f32;
    for (frame, bands) in values.iter_mut().enumerate() {
        let powers = frame_power(&region.samples, frame * FFT_HOP);
        for (band, filter) in bands.iter_mut().zip(&filters) {
            let energy = filter
                .iter()
                .map(|(bin, weight)| powers[*bin] * weight)
                .sum::<f32>();
            *band = 10.0 * energy.max(1e-12).log10();
            maximum_db = maximum_db.max(*band);
        }
    }
    let minimum_db = maximum_db - 72.0;
    let width = frames.clamp(128, 1024) as u32;
    let height = (MEL_BANDS * 4) as u32;
    let mut pixels = vec![0_u8; width as usize * height as usize * 3];
    for x in 0..width as usize {
        let frame = x * frames / width as usize;
        for y in 0..height as usize {
            let band = MEL_BANDS - 1 - y * MEL_BANDS / height as usize;
            let normalized = ((values[frame][band] - minimum_db) / 72.0).clamp(0.0, 1.0);
            let color = heat_color(normalized);
            let offset = (y * width as usize + x) * 3;
            pixels[offset..offset + 3].copy_from_slice(&color);
        }
    }
    MelSpectrogram {
        png: encode_png_rgb(width, height, &pixels),
        width,
        height,
        frames,
        minimum_db,
        maximum_db,
    }
}

fn frame_count(sample_count: usize) -> usize {
    sample_count.saturating_sub(1) / FFT_HOP + 1
}

fn frame_power(samples: &[f32], offset: usize) -> Vec<f32> {
    let mut real = vec![0.0; FFT_SIZE];
    let mut imaginary = vec![0.0; FFT_SIZE];
    for (index, value) in real.iter_mut().enumerate() {
        let window = 0.5 - 0.5 * (2.0 * PI * index as f32 / (FFT_SIZE - 1) as f32).cos();
        *value = samples.get(offset + index).copied().unwrap_or(0.0) * window;
    }
    fft(&mut real, &mut imaginary);
    real.into_iter()
        .zip(imaginary)
        .take(FFT_SIZE / 2 + 1)
        .map(|(real, imaginary)| (real * real + imaginary * imaginary) / FFT_SIZE as f32)
        .collect()
}

fn fft(real: &mut [f32], imaginary: &mut [f32]) {
    let length = real.len();
    let mut reversed = 0;
    for index in 1..length {
        let mut bit = length >> 1;
        while reversed & bit != 0 {
            reversed ^= bit;
            bit >>= 1;
        }
        reversed ^= bit;
        if index < reversed {
            real.swap(index, reversed);
            imaginary.swap(index, reversed);
        }
    }
    let mut size = 2;
    while size <= length {
        let angle = -2.0 * PI / size as f32;
        for start in (0..length).step_by(size) {
            for offset in 0..size / 2 {
                let phase = angle * offset as f32;
                let cosine = phase.cos();
                let sine = phase.sin();
                let even = start + offset;
                let odd = even + size / 2;
                let odd_real = real[odd] * cosine - imaginary[odd] * sine;
                let odd_imaginary = real[odd] * sine + imaginary[odd] * cosine;
                real[odd] = real[even] - odd_real;
                imaginary[odd] = imaginary[even] - odd_imaginary;
                real[even] += odd_real;
                imaginary[even] += odd_imaginary;
            }
        }
        size *= 2;
    }
}

fn mel_filters() -> Vec<Vec<(usize, f32)>> {
    let minimum_mel = hz_to_mel(30.0);
    let maximum_mel = hz_to_mel(SAMPLE_RATE as f32 / 2.0);
    let points = (0..MEL_BANDS + 2)
        .map(|index| {
            let mel =
                minimum_mel + (maximum_mel - minimum_mel) * index as f32 / (MEL_BANDS + 1) as f32;
            ((mel_to_hz(mel) * FFT_SIZE as f32 / SAMPLE_RATE as f32).floor() as usize)
                .min(FFT_SIZE / 2)
        })
        .collect::<Vec<_>>();
    (0..MEL_BANDS)
        .map(|band| {
            let left = points[band].min(FFT_SIZE / 2 - 2);
            let center = points[band + 1].clamp(left + 1, FFT_SIZE / 2 - 1);
            let right = points[band + 2].clamp(center + 1, FFT_SIZE / 2);
            (left..=right)
                .map(|bin| {
                    let weight = if bin <= center {
                        (bin - left) as f32 / (center - left) as f32
                    } else {
                        (right - bin) as f32 / (right - center) as f32
                    };
                    (bin, weight.max(0.0))
                })
                .collect()
        })
        .collect()
}

fn hz_to_mel(hertz: f32) -> f32 {
    2_595.0 * (1.0 + hertz / 700.0).log10()
}

fn mel_to_hz(mel: f32) -> f32 {
    700.0 * (10.0_f32.powf(mel / 2_595.0) - 1.0)
}

fn heat_color(value: f32) -> [u8; 3] {
    let stops = [
        [5.0, 4.0, 20.0],
        [49.0, 18.0, 92.0],
        [22.0, 103.0, 145.0],
        [74.0, 190.0, 145.0],
        [247.0, 225.0, 93.0],
    ];
    let position = value * (stops.len() - 1) as f32;
    let index = (position.floor() as usize).min(stops.len() - 2);
    let fraction = position - index as f32;
    let mut color = [0; 3];
    for (channel, value) in color.iter_mut().enumerate() {
        *value = (stops[index][channel] * (1.0 - fraction) + stops[index + 1][channel] * fraction)
            .round() as u8;
    }
    color
}

fn encode_png_rgb(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let row_bytes = width as usize * 3;
    let mut raw = Vec::with_capacity((row_bytes + 1) * height as usize);
    for row in pixels.chunks_exact(row_bytes) {
        raw.push(0);
        raw.extend_from_slice(row);
    }
    let mut compressed = vec![0x78, 0x01];
    for (index, block) in raw.chunks(65_535).enumerate() {
        compressed.push(u8::from((index + 1) * 65_535 >= raw.len()));
        let length = block.len() as u16;
        compressed.extend_from_slice(&length.to_le_bytes());
        compressed.extend_from_slice(&(!length).to_le_bytes());
        compressed.extend_from_slice(block);
    }
    compressed.extend_from_slice(&adler32(&raw).to_be_bytes());

    let mut png = b"\x89PNG\r\n\x1a\n".to_vec();
    let mut header = Vec::with_capacity(13);
    header.extend_from_slice(&width.to_be_bytes());
    header.extend_from_slice(&height.to_be_bytes());
    header.extend_from_slice(&[8, 2, 0, 0, 0]);
    png_chunk(&mut png, b"IHDR", &header);
    png_chunk(&mut png, b"IDAT", &compressed);
    png_chunk(&mut png, b"IEND", &[]);
    png
}

fn png_chunk(output: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    output.extend_from_slice(&(data.len() as u32).to_be_bytes());
    output.extend_from_slice(kind);
    output.extend_from_slice(data);
    let mut checksum_input = Vec::with_capacity(4 + data.len());
    checksum_input.extend_from_slice(kind);
    checksum_input.extend_from_slice(data);
    output.extend_from_slice(&crc32(&checksum_input).to_be_bytes());
}

fn adler32(data: &[u8]) -> u32 {
    let mut first = 1_u32;
    let mut second = 0_u32;
    for &byte in data {
        first = (first + u32::from(byte)) % 65_521;
        second = (second + first) % 65_521;
    }
    (second << 16) | first
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = u32::MAX;
    for &byte in data {
        crc ^= u32::from(byte);
        for _ in 0..8 {
            crc = (crc >> 1) ^ (0xedb8_8320 & 0_u32.wrapping_sub(crc & 1));
        }
    }
    !crc
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Edit, Modulator};

    #[test]
    fn renders_analyzes_and_encodes_a_demo_region() {
        let region = render_region(&Project::demo(), &[1, 2, 3], 0.0, 2.0).expect("audio region");
        assert_eq!(region.samples.len(), SAMPLE_RATE as usize * 2);
        assert!(region.event_count > 0);
        let analysis = analyze(&region);
        assert!(analysis.peak > 0.01);
        assert!(analysis.rms > 0.001);
        assert!(analysis.spectral_centroid_hz > 20.0);
        let spectrogram = mel_spectrogram(&region);
        assert!(spectrogram.png.starts_with(b"\x89PNG\r\n\x1a\n"));
        assert!(spectrogram.png.len() > 1_000);
        assert_eq!(spectrogram.height, 256);
        assert!(spectrogram.maximum_db > spectrogram.minimum_db);
    }

    #[test]
    fn rejects_unknown_channels_and_oversized_ranges() {
        let project = Project::demo();
        assert!(render_region(&project, &[999], 0.0, 1.0).is_err());
        assert!(render_region(&project, &[1], 0.0, MAX_REGION_SECONDS + 0.1).is_err());
    }

    #[test]
    fn regional_actions_shape_the_listening_render() {
        let mut project = Project::demo();
        let track_index = project
            .tracks
            .iter()
            .position(|track| track.role == TrackRole::Chords)
            .expect("demo chords");
        let track_id = project.tracks[track_index].id;
        let baseline_frame = automation_at(&project, &project.tracks[track_index], 1.0);
        let baseline = render_region(&project, &[track_id], 0.0, 2.0).expect("baseline render");
        project.edits.push(Edit {
            id: 9_001,
            operation_id: None,
            start: 0.0,
            end: 2.0,
            prompt: "Regional listening regression".to_owned(),
            summary: "Applied regional sound and rhythm".to_owned(),
            action: Action::Compound {
                actions: vec![
                    Action::Filter {
                        amount: -0.5,
                        target: Some(TrackRole::Chords),
                    },
                    Action::Effect {
                        name: "Echo",
                        mix: 0.8,
                        target: Some(TrackRole::Chords),
                    },
                    Action::RemoveEffect {
                        name: "Room",
                        target: Some(TrackRole::Chords),
                    },
                    Action::Rhythm {
                        amount: 0.8,
                        target: Some(TrackRole::Chords),
                    },
                ],
            },
        });

        let active_frame = automation_at(&project, &project.tracks[track_index], 1.0);
        assert!(active_frame.tone_cutoff < baseline_frame.tone_cutoff);
        assert!(active_frame.echo > baseline_frame.echo);
        assert!(active_frame.reverb < baseline_frame.reverb);
        assert!(regional_rhythm(&project, TrackRole::Chords, 1.0) > 0.15);
        let active = render_region(&project, &[track_id], 0.0, 2.0).expect("regional render");
        assert!(active.event_count > baseline.event_count);
        assert!(sample_difference(&active.samples, &baseline.samples) > 0.001);
    }

    #[test]
    fn enabled_modulators_reach_every_listening_parameter() {
        let mut baseline_project = Project::demo();
        let track_index = baseline_project
            .tracks
            .iter()
            .position(|track| track.role == TrackRole::Bass)
            .expect("demo bass");
        baseline_project.tracks[track_index].modulators.clear();
        let track_id = baseline_project.tracks[track_index].id;
        let effect_target = format!(
            "effect:{}.mix",
            baseline_project.tracks[track_index].effects[0].id
        );
        let baseline =
            render_region(&baseline_project, &[track_id], 0.0, 1.0).expect("baseline render");

        for target in [
            "instrument.attack".to_owned(),
            "instrument.release".to_owned(),
            "instrument.tone".to_owned(),
            "instrument.pitch".to_owned(),
            "track.volume".to_owned(),
            effect_target,
        ] {
            let mut project = baseline_project.clone();
            project.tracks[track_index].modulators.push(Modulator {
                id: 9_002,
                name: "Listening regression".to_owned(),
                shape: "square".to_owned(),
                rate: 0.25,
                depth: 0.8,
                target: target.clone(),
                enabled: true,
            });
            let modulated =
                render_region(&project, &[track_id], 0.0, 1.0).expect("modulated render");
            assert!(
                sample_difference(&modulated.samples, &baseline.samples) > 0.000_01,
                "{target} must affect the listening render"
            );

            project.tracks[track_index].modulators[0].enabled = false;
            let disabled = render_region(&project, &[track_id], 0.0, 1.0).expect("disabled render");
            assert_eq!(disabled.samples, baseline.samples);
        }
    }

    fn sample_difference(left: &[f32], right: &[f32]) -> f32 {
        left.iter()
            .zip(right)
            .map(|(left, right)| (left - right).abs())
            .sum::<f32>()
            / left.len().max(1) as f32
    }
}
