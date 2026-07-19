use std::collections::HashSet;

use serde_json::{Map, Value as JsonValue};

use crate::model::{
    Clip, ClipEvent, Edit, Effect, Instrument, MAX_PROMPT_CHARACTERS, Modulator, Project,
    ProjectFileError, Routing, Track, TrackRole,
};
use crate::prompt::{Action, MidiNote};

const MAX_TRACKS: usize = 128;
const MAX_TOOLS_PER_TRACK: usize = 256;
const MAX_CLIPS_PER_TRACK: usize = 2_048;
const MAX_EVENTS_PER_CLIP: usize = 2_048;
const MAX_EDITS: usize = 10_000;
const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

type Object = Map<String, JsonValue>;

pub(crate) fn parse_project(source: &str) -> Result<Project, ProjectFileError> {
    let value =
        serde_json::from_str(source).map_err(|error| invalid(format!("invalid JSON: {error}")))?;
    let root = object(&value, "sound graph")?;
    let name = limited_string(root, "name", 1, 160)?;
    let bpm = integer(root, "bpm")?;
    if !(60..=180).contains(&bpm) {
        return Err(invalid("bpm must be between 60 and 180"));
    }
    let duration = finite_number(root, "duration")?;
    if !(0.25..=86_400.0).contains(&duration) {
        return Err(invalid("duration must be between 0.25 and 86400 seconds"));
    }
    let version = integer(root, "version")?;
    let track_values = array(root, "tracks")?;
    if track_values.is_empty() || track_values.len() > MAX_TRACKS {
        return Err(invalid(format!(
            "tracks must contain between 1 and {MAX_TRACKS} entries"
        )));
    }

    let mut ids = HashSet::new();
    let mut event_ids = HashSet::new();
    let tracks = track_values
        .iter()
        .enumerate()
        .map(|(index, value)| parse_track(value, index, duration, &mut ids, &mut event_ids))
        .collect::<Result<Vec<_>, _>>()?;
    let edits = match (root.get("edits"), root.get("regionalEdits")) {
        (Some(_), Some(_)) => {
            return Err(invalid(
                "sound graph cannot contain both edits and regionalEdits",
            ));
        }
        (Some(value), None) => {
            let values = value
                .as_array()
                .ok_or_else(|| invalid("edits must be an array"))?;
            if values.len() > MAX_EDITS {
                return Err(invalid(format!(
                    "edits supports at most {MAX_EDITS} entries"
                )));
            }
            values
                .iter()
                .enumerate()
                .map(|(index, value)| parse_edit(value, index, duration, &mut ids))
                .collect::<Result<Vec<_>, _>>()?
        }
        (None, Some(value)) => {
            let values = value
                .as_array()
                .ok_or_else(|| invalid("regionalEdits must be an array"))?;
            if values.len() > MAX_EDITS {
                return Err(invalid(format!(
                    "regionalEdits supports at most {MAX_EDITS} entries"
                )));
            }
            let mut next_id = ids
                .iter()
                .chain(event_ids.iter())
                .copied()
                .max()
                .unwrap_or(0)
                .checked_add(1)
                .ok_or_else(|| invalid("sound graph ID namespace is exhausted"))?;
            values
                .iter()
                .enumerate()
                .map(|(index, value)| {
                    parse_regional_edit(value, index, duration, &mut next_id, &mut ids)
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        (None, None) => Vec::new(),
    };
    let mut operation_ids = HashSet::new();
    if edits
        .iter()
        .filter_map(|edit| edit.operation_id.as_deref())
        .any(|operation_id| !operation_ids.insert(operation_id))
    {
        return Err(invalid("edit operation IDs must be unique"));
    }
    if !ids.is_disjoint(&event_ids) {
        return Err(invalid(
            "MIDI event IDs must not collide with sound graph object IDs",
        ));
    }

    Ok(Project {
        name,
        bpm: bpm as u16,
        duration,
        version,
        tracks,
        edits,
    })
}

fn parse_track(
    value: &JsonValue,
    index: usize,
    project_duration: f32,
    ids: &mut HashSet<u64>,
    event_ids: &mut HashSet<u64>,
) -> Result<Track, ProjectFileError> {
    let context = format!("track {}", index + 1);
    let track = object(value, &context)?;
    let id = unique_id(track, "id", ids, &context)?;
    let name = limited_string(track, "name", 1, 160)?;
    let role = parse_role(string(track, "role")?)?;
    let color = limited_string(track, "color", 1, 32)?;
    let volume = range(track, "volume", 0.0, 1.5)?;
    let muted = boolean(track, "muted")?;

    let instrument_value = field(track, "instrument")?;
    let instrument_object = object(instrument_value, "instrument")?;
    let instrument_id = unique_id(instrument_object, "id", ids, "instrument")?;
    expect_type(instrument_object, "instrument")?;
    let engine = limited_string(instrument_object, "engine", 1, 64)?;
    let waveform = limited_string(instrument_object, "waveform", 1, 32)?;
    if !matches!(
        waveform.as_str(),
        "sine" | "triangle" | "sawtooth" | "square"
    ) {
        return Err(invalid("instrument waveform is unsupported"));
    }
    let instrument_parameters = object(
        field(instrument_object, "parameters")?,
        "instrument parameters",
    )?;
    let instrument = Instrument {
        id: instrument_id,
        engine,
        waveform,
        attack: range(instrument_parameters, "attack", 0.001, 2.0)?,
        release: range(instrument_parameters, "release", 0.02, 5.0)?,
        tone: range(instrument_parameters, "tone", 0.0, 1.0)?,
    };

    let effect_values = array(track, "effects")?;
    if effect_values.len() > MAX_TOOLS_PER_TRACK {
        return Err(invalid(format!(
            "{context} supports at most {MAX_TOOLS_PER_TRACK} effects"
        )));
    }
    let effects = effect_values
        .iter()
        .map(|value| parse_effect(value, ids))
        .collect::<Result<Vec<_>, _>>()?;

    let modulator_values = array(track, "modulators")?;
    if modulator_values.len() > MAX_TOOLS_PER_TRACK {
        return Err(invalid(format!(
            "{context} supports at most {MAX_TOOLS_PER_TRACK} modulators"
        )));
    }
    let modulators = modulator_values
        .iter()
        .map(|value| parse_modulator(value, ids))
        .collect::<Result<Vec<_>, _>>()?;

    let routing_object = object(field(track, "routing")?, "routing")?;
    let output = limited_string(routing_object, "output", 1, 64)?;
    if output != "master" {
        return Err(invalid("routing output must be master"));
    }
    let effect_order = parse_effect_order(routing_object, instrument.id, &effects)?;
    validate_control_routing(routing_object, &modulators)?;
    validate_routing_edges(
        routing_object,
        instrument.id,
        &effect_order,
        &output,
        &modulators,
    )?;

    let clip_values = array(track, "clips")?;
    if clip_values.len() > MAX_CLIPS_PER_TRACK {
        return Err(invalid(format!(
            "{context} supports at most {MAX_CLIPS_PER_TRACK} clips"
        )));
    }
    let clips = clip_values
        .iter()
        .map(|value| parse_clip(value, project_duration, ids, event_ids))
        .collect::<Result<Vec<_>, _>>()?;

    let parsed = Track {
        id,
        name,
        role,
        color,
        volume,
        muted,
        instrument,
        effects,
        modulators,
        routing: Routing {
            effect_order,
            output,
        },
        clips,
    };
    validate_modulator_targets(&parsed)?;
    Ok(parsed)
}

fn parse_effect(value: &JsonValue, ids: &mut HashSet<u64>) -> Result<Effect, ProjectFileError> {
    let effect = object(value, "effect")?;
    let id = unique_id(effect, "id", ids, "effect")?;
    expect_type(effect, "effect")?;
    let name = limited_string(effect, "name", 1, 64)?;
    if !is_effect_name(&name) {
        return Err(invalid(format!("unsupported effect: {name}")));
    }
    let parameters = object(field(effect, "parameters")?, "effect parameters")?;
    Ok(Effect {
        id,
        name,
        mix: range(parameters, "mix", 0.0, 1.0)?,
        enabled: boolean(effect, "enabled")?,
    })
}

fn parse_modulator(
    value: &JsonValue,
    ids: &mut HashSet<u64>,
) -> Result<Modulator, ProjectFileError> {
    let modulator = object(value, "modulator")?;
    let id = unique_id(modulator, "id", ids, "modulator")?;
    expect_type(modulator, "modulator")?;
    let shape = limited_string(modulator, "shape", 1, 32)?;
    if !matches!(
        shape.as_str(),
        "sine" | "triangle" | "square" | "random" | "envelope"
    ) {
        return Err(invalid("modulator shape is unsupported"));
    }
    let parameters = object(field(modulator, "parameters")?, "modulator parameters")?;
    Ok(Modulator {
        id,
        name: limited_string(modulator, "name", 1, 64)?,
        shape,
        rate: range(parameters, "rate", 0.01, 20.0)?,
        depth: range(parameters, "depth", 0.0, 1.0)?,
        target: limited_string(modulator, "target", 1, 64)?,
        enabled: boolean(modulator, "enabled")?,
    })
}

fn parse_effect_order(
    routing: &Object,
    instrument_id: u64,
    effects: &[Effect],
) -> Result<Vec<u64>, ProjectFileError> {
    let audio = array(routing, "audio")?;
    if audio.len() != effects.len() + 3 {
        return Err(invalid(
            "routing audio chain must include clips, instrument, every effect, and master",
        ));
    }
    if audio[0].as_str() != Some("clips")
        || audio[1].as_str() != Some(format!("instrument:{instrument_id}").as_str())
        || audio.last().and_then(JsonValue::as_str) != Some("master")
    {
        return Err(invalid("routing audio chain has invalid endpoints"));
    }
    let known = effects
        .iter()
        .map(|effect| effect.id)
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    let order = audio[2..audio.len() - 1]
        .iter()
        .map(|value| {
            let id = value
                .as_str()
                .and_then(|value| value.strip_prefix("effect:"))
                .and_then(|value| value.parse::<u64>().ok())
                .ok_or_else(|| invalid("routing effect entry is invalid"))?;
            if !known.contains(&id) || !seen.insert(id) {
                return Err(invalid("routing must contain every effect exactly once"));
            }
            Ok(id)
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(order)
}

fn validate_control_routing(
    routing: &Object,
    modulators: &[Modulator],
) -> Result<(), ProjectFileError> {
    let routes = array(routing, "control")?;
    let expected = modulators
        .iter()
        .filter(|modulator| modulator.enabled)
        .map(|modulator| {
            (
                format!("modulator:{}", modulator.id),
                modulator.target.clone(),
            )
        })
        .collect::<HashSet<_>>();
    if routes.len() != expected.len() {
        return Err(invalid(
            "routing control must contain every enabled modulator exactly once",
        ));
    }
    let mut seen = HashSet::new();
    for (index, route) in routes.iter().enumerate() {
        let route = object(route, &format!("routing control entry {}", index + 1))?;
        let connection = (
            string(route, "source")?.to_owned(),
            string(route, "target")?.to_owned(),
        );
        if !expected.contains(&connection) || !seen.insert(connection) {
            return Err(invalid(
                "routing control connections must match enabled modulators",
            ));
        }
    }
    Ok(())
}

fn validate_routing_edges(
    routing: &Object,
    instrument_id: u64,
    effect_order: &[u64],
    output: &str,
    modulators: &[Modulator],
) -> Result<(), ProjectFileError> {
    let instrument = format!("instrument:{instrument_id}");
    let mut expected = HashSet::from([("clips".to_owned(), instrument.clone(), "midi".to_owned())]);
    let mut audio_source = instrument;
    for effect_id in effect_order {
        let effect = format!("effect:{effect_id}");
        expected.insert((audio_source, effect.clone(), "audio".to_owned()));
        audio_source = effect;
    }
    expected.insert((audio_source, output.to_owned(), "audio".to_owned()));
    expected.extend(
        modulators
            .iter()
            .filter(|modulator| modulator.enabled)
            .map(|modulator| {
                (
                    format!("modulator:{}", modulator.id),
                    modulator.target.clone(),
                    "control".to_owned(),
                )
            }),
    );

    let edges = array(routing, "edges")?;
    if edges.len() != expected.len() {
        return Err(invalid(
            "routing edges must contain the complete typed signal graph",
        ));
    }
    let mut seen = HashSet::new();
    for (index, edge) in edges.iter().enumerate() {
        let edge = object(edge, &format!("routing edge {}", index + 1))?;
        let connection = (
            string(edge, "source")?.to_owned(),
            string(edge, "target")?.to_owned(),
            string(edge, "type")?.to_owned(),
        );
        if !expected.contains(&connection) || !seen.insert(connection) {
            return Err(invalid(
                "routing edges are inconsistent with the published sound tools",
            ));
        }
    }
    Ok(())
}

fn parse_clip(
    value: &JsonValue,
    project_duration: f32,
    ids: &mut HashSet<u64>,
    event_ids: &mut HashSet<u64>,
) -> Result<Clip, ProjectFileError> {
    let clip = object(value, "MIDI clip")?;
    let id = unique_id(clip, "id", ids, "MIDI clip")?;
    let start = range(clip, "start", 0.0, project_duration)?;
    let end = range(clip, "end", 0.0, project_duration)?;
    if end <= start {
        return Err(invalid("MIDI clip end must be after its start"));
    }
    let source_start = range(clip, "sourceStart", 0.0, start)?;
    let loop_beats = range(clip, "loopBeats", 0.25, 16.0)?;
    let event_values = array(clip, "events")?;
    if event_values.len() > MAX_EVENTS_PER_CLIP {
        return Err(invalid(format!(
            "MIDI clips support at most {MAX_EVENTS_PER_CLIP} events"
        )));
    }
    let mut clip_event_ids = HashSet::new();
    let events = event_values
        .iter()
        .map(|value| parse_clip_event(value, loop_beats, &mut clip_event_ids, event_ids))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Clip {
        id,
        label: limited_string(clip, "label", 1, 64)?,
        start,
        end,
        source_start,
        style: limited_string(clip, "style", 1, 64)?,
        loop_beats,
        events,
    })
}

fn parse_clip_event(
    value: &JsonValue,
    loop_beats: f32,
    clip_event_ids: &mut HashSet<u64>,
    event_ids: &mut HashSet<u64>,
) -> Result<ClipEvent, ProjectFileError> {
    let event = object(value, "MIDI event")?;
    let id = unique_id(event, "id", clip_event_ids, "MIDI event")?;
    event_ids.insert(id);
    let kind = limited_string(event, "type", 1, 32)?;
    if kind != "note" {
        return Err(invalid("MIDI event type must be note"));
    }
    let time = finite_number(event, "time")?;
    if !(0.0..loop_beats).contains(&time) {
        return Err(invalid("MIDI event time must be inside its clip loop"));
    }
    Ok(ClipEvent {
        id,
        kind,
        time,
        duration: range(event, "duration", 0.0625, loop_beats)?,
        pitch: bounded_integer(event, "pitch", 0, 127)? as u8,
        velocity: range(event, "velocity", 0.01, 1.0)?,
    })
}

fn parse_edit(
    value: &JsonValue,
    index: usize,
    project_duration: f32,
    ids: &mut HashSet<u64>,
) -> Result<Edit, ProjectFileError> {
    let edit = object(value, "edit")?;
    let context = format!("edit {}", index + 1);
    let id = unique_id(edit, "id", ids, &context)?;
    let start = range(edit, "start", 0.0, project_duration)?;
    let end = range(edit, "end", 0.0, project_duration)?;
    if end <= start {
        return Err(invalid(format!("{context} end must be after its start")));
    }
    Ok(Edit {
        id,
        operation_id: edit
            .get("operationId")
            .map(|_| limited_string(edit, "operationId", 1, 128))
            .transpose()?,
        start,
        end,
        prompt: limited_string(edit, "prompt", 1, MAX_PROMPT_CHARACTERS)?,
        summary: limited_string(edit, "summary", 1, 160)?,
        action: parse_action(field(edit, "action")?, 0)?,
    })
}

fn parse_regional_edit(
    value: &JsonValue,
    index: usize,
    project_duration: f32,
    next_id: &mut u64,
    ids: &mut HashSet<u64>,
) -> Result<Edit, ProjectFileError> {
    let edit = object(value, "regional edit")?;
    let context = format!("regional edit {}", index + 1);
    let start = range(edit, "start", 0.0, project_duration)?;
    let end = range(edit, "end", 0.0, project_duration)?;
    if end <= start {
        return Err(invalid(format!("{context} end must be after its start")));
    }
    if *next_id > MAX_SAFE_INTEGER {
        return Err(invalid("sound graph ID namespace is exhausted"));
    }
    let id = *next_id;
    *next_id = next_id
        .checked_add(1)
        .ok_or_else(|| invalid("sound graph ID namespace is exhausted"))?;
    if !ids.insert(id) {
        return Err(invalid(format!("duplicate sound graph ID: {id}")));
    }
    Ok(Edit {
        id,
        operation_id: None,
        start,
        end,
        prompt: "Prior regional edit".to_owned(),
        summary: "Active regional state".to_owned(),
        action: parse_action(field(edit, "action")?, 0)?,
    })
}

fn parse_action(value: &JsonValue, depth: usize) -> Result<Action, ProjectFileError> {
    if depth > 8 {
        return Err(invalid("compound action nesting is too deep"));
    }
    let action = object(value, "edit action")?;
    let action_type = string(action, "type")?;
    if action_type == "compound" {
        let values = array(action, "actions")?;
        if values.is_empty() || values.len() > 8 {
            return Err(invalid(
                "compound actions require one to eight child actions",
            ));
        }
        return Ok(Action::Compound {
            actions: values
                .iter()
                .map(|value| parse_action(value, depth + 1))
                .collect::<Result<Vec<_>, _>>()?,
        });
    }
    let target = action_target(action)?;
    match action_type {
        "gain" => Ok(Action::Gain {
            amount: range(action, "value", 0.0, 2.0)?,
            target,
        }),
        "mute" => Ok(Action::Mute { target }),
        "midi-clip" => {
            let role = required_target(target, "midi-clip")?;
            let loop_beats = range(action, "loopBeats", 0.25, 16.0)?;
            let start = relative_range(action, "start")?;
            let end = relative_range(action, "end")?;
            if end <= start {
                return Err(invalid("midi-clip edit end must be after its start"));
            }
            let events = array(action, "events")?;
            if events.len() > 32 {
                return Err(invalid("midi-clip edit supports at most 32 events"));
            }
            Ok(Action::MidiClip {
                track_id: integer(action, "trackId")?,
                target: role,
                label: limited_string(action, "label", 1, 64)?,
                start,
                end,
                loop_beats,
                notes: events
                    .iter()
                    .map(|event| parse_midi_note(event, loop_beats))
                    .collect::<Result<Vec<_>, _>>()?,
            })
        }
        "add-track" => Ok(Action::AddTrack {
            role: required_target(target, "add-track")?,
        }),
        "instrument" => Ok(Action::Instrument {
            waveform: waveform(string(action, "name")?)?,
            target: required_target(target, "instrument")?,
        }),
        "modulator" => Ok(Action::Modulator {
            parameter: limited_string(action, "name", 1, 64)?,
            shape: modulator_shape(string(action, "shape")?)?,
            rate: range(action, "rate", 0.01, 20.0)?,
            depth: range(action, "value", 0.0, 1.0)?,
            target: required_target(target, "modulator")?,
        }),
        "configure" => {
            let clip_id = integer(action, "clipId")?;
            Ok(Action::Configure {
                track_id: integer(action, "trackId")?,
                target: required_target(target, "configure")?,
                tool: sound_tool(string(action, "tool")?)?,
                tool_id: integer(action, "toolId")?,
                clip_id: (clip_id != 0).then_some(clip_id),
                parameter: sound_parameter(string(action, "parameter")?)?,
                value: limited_string(action, "setting", 1, 64)?,
            })
        }
        "effect" => Ok(Action::Effect {
            name: effect_name(string(action, "name")?, false)?,
            mix: range(action, "value", 0.0, 1.0)?,
            target,
        }),
        "remove-effect" => Ok(Action::RemoveEffect {
            name: effect_name(string(action, "name")?, true)?,
            target,
        }),
        "filter" => Ok(Action::Filter {
            amount: range(action, "value", -1.0, 1.0)?,
            target,
        }),
        "rhythm" => Ok(Action::Rhythm {
            amount: range(action, "value", -1.0, 1.0)?,
            target,
        }),
        "tempo" if target.is_none() => Ok(Action::Tempo {
            bpm: bounded_integer(action, "value", 60, 180)? as u16,
        }),
        _ => Err(invalid(format!("unsupported edit action: {action_type}"))),
    }
}

fn parse_midi_note(value: &JsonValue, loop_beats: f32) -> Result<MidiNote, ProjectFileError> {
    let event = object(value, "MIDI note")?;
    if event.get("type").is_some() && string(event, "type")? != "note" {
        return Err(invalid("MIDI edit event type must be note"));
    }
    let time = finite_number(event, "time")?;
    if !(0.0..loop_beats).contains(&time) {
        return Err(invalid("MIDI edit event time must be inside its loop"));
    }
    Ok(MidiNote {
        time,
        duration: range(event, "duration", 0.0625, loop_beats)?,
        pitch: bounded_integer(event, "pitch", 0, 127)? as u8,
        velocity: range(event, "velocity", 0.01, 1.0)?,
    })
}

fn validate_modulator_targets(track: &Track) -> Result<(), ProjectFileError> {
    let effect_targets = track
        .effects
        .iter()
        .map(|effect| format!("effect:{}.mix", effect.id))
        .collect::<HashSet<_>>();
    for modulator in &track.modulators {
        if !matches!(
            modulator.target.as_str(),
            "instrument.attack"
                | "instrument.release"
                | "instrument.tone"
                | "instrument.pitch"
                | "track.volume"
        ) && !effect_targets.contains(&modulator.target)
        {
            return Err(invalid(format!(
                "modulator {} targets an unknown parameter",
                modulator.id
            )));
        }
    }
    Ok(())
}

fn action_target(action: &Object) -> Result<Option<TrackRole>, ProjectFileError> {
    match string(action, "target")? {
        "all" => Ok(None),
        value => parse_role(value).map(Some),
    }
}

fn required_target(target: Option<TrackRole>, action: &str) -> Result<TrackRole, ProjectFileError> {
    target.ok_or_else(|| invalid(format!("{action} requires a track role target")))
}

fn parse_role(value: &str) -> Result<TrackRole, ProjectFileError> {
    match value {
        "drums" => Ok(TrackRole::Drums),
        "bass" => Ok(TrackRole::Bass),
        "chords" => Ok(TrackRole::Chords),
        "lead" => Ok(TrackRole::Lead),
        "texture" => Ok(TrackRole::Texture),
        _ => Err(invalid(format!("unknown track role: {value}"))),
    }
}

fn waveform(value: &str) -> Result<&'static str, ProjectFileError> {
    match value {
        "sine" => Ok("sine"),
        "triangle" => Ok("triangle"),
        "sawtooth" => Ok("sawtooth"),
        "square" => Ok("square"),
        _ => Err(invalid("unsupported instrument waveform")),
    }
}

fn modulator_shape(value: &str) -> Result<&'static str, ProjectFileError> {
    match value {
        "sine" => Ok("sine"),
        "triangle" => Ok("triangle"),
        "square" => Ok("square"),
        "random" => Ok("random"),
        "envelope" => Ok("envelope"),
        _ => Err(invalid("unsupported modulator shape")),
    }
}

fn sound_tool(value: &str) -> Result<&'static str, ProjectFileError> {
    match value {
        "instrument" => Ok("instrument"),
        "effect" => Ok("effect"),
        "modulator" => Ok("modulator"),
        "event" => Ok("event"),
        "routing" => Ok("routing"),
        _ => Err(invalid("unsupported configurable sound tool")),
    }
}

fn sound_parameter(value: &str) -> Result<&'static str, ProjectFileError> {
    match value {
        "waveform" => Ok("waveform"),
        "attack" => Ok("attack"),
        "release" => Ok("release"),
        "tone" => Ok("tone"),
        "mix" => Ok("mix"),
        "enabled" => Ok("enabled"),
        "shape" => Ok("shape"),
        "rate" => Ok("rate"),
        "depth" => Ok("depth"),
        "target" => Ok("target"),
        "time" => Ok("time"),
        "duration" => Ok("duration"),
        "pitch" => Ok("pitch"),
        "velocity" => Ok("velocity"),
        "position" => Ok("position"),
        _ => Err(invalid("unsupported sound-tool parameter")),
    }
}

fn effect_name(value: &str, allow_all: bool) -> Result<&'static str, ProjectFileError> {
    match value {
        "Reverb" => Ok("Reverb"),
        "Room" => Ok("Room"),
        "Echo" => Ok("Echo"),
        "Chorus" => Ok("Chorus"),
        "Low-pass filter" => Ok("Low-pass filter"),
        "Punch compressor" => Ok("Punch compressor"),
        "Shimmer" => Ok("Shimmer"),
        "Effects" if allow_all => Ok("Effects"),
        _ => Err(invalid(format!("unsupported effect: {value}"))),
    }
}

fn is_effect_name(value: &str) -> bool {
    effect_name(value, false).is_ok()
}

fn expect_type(value: &Object, expected: &str) -> Result<(), ProjectFileError> {
    if string(value, "type")? == expected {
        Ok(())
    } else {
        Err(invalid(format!("sound tool type must be {expected}")))
    }
}

fn object<'a>(value: &'a JsonValue, context: &str) -> Result<&'a Object, ProjectFileError> {
    value
        .as_object()
        .ok_or_else(|| invalid(format!("{context} must be an object")))
}

fn field<'a>(object: &'a Object, name: &str) -> Result<&'a JsonValue, ProjectFileError> {
    object
        .get(name)
        .ok_or_else(|| invalid(format!("{name} is required")))
}

fn array<'a>(object: &'a Object, name: &str) -> Result<&'a [JsonValue], ProjectFileError> {
    field(object, name)?
        .as_array()
        .map(Vec::as_slice)
        .ok_or_else(|| invalid(format!("{name} must be an array")))
}

fn string<'a>(object: &'a Object, name: &str) -> Result<&'a str, ProjectFileError> {
    field(object, name)?
        .as_str()
        .ok_or_else(|| invalid(format!("{name} must be a string")))
}

fn limited_string(
    object: &Object,
    name: &str,
    minimum: usize,
    maximum: usize,
) -> Result<String, ProjectFileError> {
    let value = string(object, name)?.trim();
    let length = value.chars().count();
    if !(minimum..=maximum).contains(&length) {
        return Err(invalid(format!(
            "{name} length must be between {minimum} and {maximum} characters"
        )));
    }
    Ok(value.to_owned())
}

fn boolean(object: &Object, name: &str) -> Result<bool, ProjectFileError> {
    field(object, name)?
        .as_bool()
        .ok_or_else(|| invalid(format!("{name} must be true or false")))
}

fn finite_number(object: &Object, name: &str) -> Result<f32, ProjectFileError> {
    let value = field(object, name)?
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| invalid(format!("{name} must be a finite number")))?;
    if value < f64::from(f32::MIN) || value > f64::from(f32::MAX) {
        return Err(invalid(format!(
            "{name} is outside the supported number range"
        )));
    }
    Ok(value as f32)
}

fn range(object: &Object, name: &str, minimum: f32, maximum: f32) -> Result<f32, ProjectFileError> {
    let value = finite_number(object, name)?;
    if !(minimum..=maximum).contains(&value) {
        return Err(invalid(format!(
            "{name} must be between {minimum} and {maximum}"
        )));
    }
    Ok(value)
}

fn relative_range(object: &Object, name: &str) -> Result<f32, ProjectFileError> {
    range(object, name, 0.0, 1.0)
}

fn integer(object: &Object, name: &str) -> Result<u64, ProjectFileError> {
    let value = field(object, name)?
        .as_f64()
        .filter(|value| value.is_finite())
        .ok_or_else(|| invalid(format!("{name} must be a number")))?;
    if value.fract() != 0.0 || !(0.0..=MAX_SAFE_INTEGER as f64).contains(&value) {
        return Err(invalid(format!(
            "{name} must be a non-negative safe integer"
        )));
    }
    Ok(value as u64)
}

fn bounded_integer(
    object: &Object,
    name: &str,
    minimum: u64,
    maximum: u64,
) -> Result<u64, ProjectFileError> {
    let value = integer(object, name)?;
    if !(minimum..=maximum).contains(&value) {
        return Err(invalid(format!(
            "{name} must be between {minimum} and {maximum}"
        )));
    }
    Ok(value)
}

fn unique_id(
    object: &Object,
    name: &str,
    ids: &mut HashSet<u64>,
    context: &str,
) -> Result<u64, ProjectFileError> {
    let id = integer(object, name)?;
    if id == 0 {
        return Err(invalid(format!("{context} ID must be greater than zero")));
    }
    if !ids.insert(id) {
        return Err(invalid(format!("duplicate sound graph ID: {id}")));
    }
    Ok(id)
}

fn invalid(message: impl Into<String>) -> ProjectFileError {
    ProjectFileError::new(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trips_the_demo_sound_graph() {
        let original = Project::demo();
        let parsed = parse_project(&original.to_json()).expect("valid demo graph");
        assert_eq!(parsed.to_json(), original.to_json());
    }

    #[test]
    fn round_trips_materialized_edits() {
        let mut studio = crate::model::Studio::new();
        studio
            .apply_prompt(4.0, 8.0, "increase volume")
            .expect("valid edit");
        let source = studio.project().to_json();
        let parsed = parse_project(&source).expect("valid edited graph");
        assert_eq!(parsed.to_json(), source);
    }

    #[test]
    fn accepts_retained_clip_slices_with_shared_event_ids() {
        let mut studio = crate::model::Studio::new();
        studio
            .apply_prompt(0.0, 4.0, "add a lead")
            .expect("valid lead edit");
        studio
            .apply_prompt(1.0, 3.0, "rewrite the lead MIDI clip")
            .expect("valid MIDI replacement");
        let source = studio.project().to_json();

        let parsed = parse_project(&source).expect("valid retained clip slices");
        assert_eq!(parsed.to_json(), source);
    }

    #[test]
    fn rejects_duplicate_ids_and_invalid_routing() {
        let duplicate = Project::demo()
            .to_json()
            .replacen("\"id\":201", "\"id\":1", 1);
        assert!(parse_project(&duplicate).is_err());

        let routing = Project::demo()
            .to_json()
            .replacen("\"effect:110\"", "\"effect:999\"", 1);
        assert!(parse_project(&routing).is_err());

        let control = Project::demo().to_json().replacen(
            "{\"source\":\"modulator:150\",\"target\":\"instrument.tone\"}",
            "{\"source\":\"modulator:999\",\"target\":\"instrument.tone\"}",
            1,
        );
        assert!(parse_project(&control).is_err());

        let edge = Project::demo().to_json().replacen(
            "\"target\":\"master\",\"type\":\"audio\"",
            "\"target\":\"effect:999\",\"type\":\"audio\"",
            1,
        );
        assert!(parse_project(&edge).is_err());
    }

    #[test]
    fn parses_the_bounded_regional_session_projection() {
        let mut studio = crate::model::Studio::new();
        studio
            .apply_prompt(4.0, 8.0, "increase volume")
            .expect("valid regional edit");
        studio
            .apply_prompt(8.0, 12.0, "use a sawtooth bass")
            .expect("valid baseline edit");

        let source = studio.project().planner_json();
        let parsed = parse_project(&source).expect("valid session projection");
        assert_eq!(parsed.edits.len(), 1);
        assert!(source.contains("\"regionalEdits\":["));
        assert!(!source.contains("\"prompt\":"));
        assert!(!source.contains("\"type\":\"instrument\",\"name\":\"sawtooth\""));
    }
}
