# DAW-AI synth edit contract

DAW-AI is a deterministic browser synthesizer. Codex plans edits; the Rust server validates the plan; the Web Audio client renders it.

## Sound graph

The current project JSON is the source of truth. Each track is represented as small, explicit sound tools:

- `clips` contain a beat-relative `events` list. Every event has `time`, `duration`, MIDI `pitch`, and normalized `velocity`; its `type` is `note`, `kick`, `snare`, or `hat`. `loopBeats` controls repetition inside the clip's second-based `start`/`end` range.
- `instrument` contains a synth `waveform` and numeric `attack`, `release`, and `tone` parameters.
- `effects` contain an enabled state and numeric `mix`. The `routing.audio` list gives their serial order between the instrument and `master`.
- `modulators` contain a shape, rate in Hz, depth, enabled state, and parameter target. `modulationTargets` is the authoritative list of routable parameter IDs and their ranges; `routing.control` mirrors the active control connections.

Prefer these exact field names when reasoning about the current sound. The project is deliberately code- and configuration-friendly, with stable IDs and no opaque binary state.

## Track roles

- `drums`: synthesized kick, snare, and hats
- `bass`: monophonic subtractive bass
- `chords`: sustained polyphonic pad
- `lead`: monophonic melodic synth
- `texture`: long atmospheric tones

Use `all` when an edit should affect the complete mix. Use a role name for a targeted edit.

## Actions

Every action object has all schema fields. Use `name: "None"`, `value: 0`, `trackId: 0`, `tool: "None"`, `toolId: 0`, `clipId: 0`, `parameter: "None"`, and `setting: ""` when fields do not apply. IDs in a `configure` action must come directly from the current project JSON.

- `gain`: multiply regional level; value 0.0 through 2.0
- `mute`: silence the target in the region
- `drop`: create a build, short pre-impact gap, and high-energy impact; target `all`. Its value is the fraction of the selected region used for the build, from 0.25 through 0.60. Use 0.40 for a standard drop. The core reuses an existing lead, starts its hook at impact, and supplies the drum, bass, filter, and level contrast, so do not add another lead or rhythm action merely to make the drop audible.
- `add-track`: add the target role in the region
- `instrument`: change a role's baseline synth waveform. `name` must be `sine`, `triangle`, `sawtooth`, or `square`; use value `0`.
- `modulator`: add a sine modulator to a role. Set `name` to an exact ID from that track's `modulationTargets`, including `instrument.attack`, `instrument.release`, `instrument.tone`, `instrument.pitch`, `track.volume`, or a stable-ID effect target such as `effect:210.mix`; value is depth from 0.0 through 1.0.
- `configure`: edit an existing sound tool by stable ID. Set `trackId`, `tool`, `toolId`, `parameter`, and string `setting`; set `clipId` for an event and otherwise use `0`. It supports every Advanced parameter: instrument `waveform`/`attack`/`release`/`tone`; effect `mix`/`enabled`; modulator `shape`/`rate`/`depth`/`target`/`enabled`; event `time`/`duration`/`pitch`/`velocity`; and routing `position`. A modulator `target` setting must be an exact ID from the owning track's `modulationTargets`. Set `target` to the owning track role and use `name: "None"`, `value: 0`.
- `effect`: add a named effect with mix 0.0 through 1.0
- `remove-effect`: disable a named effect, or `Effects` for all effects, in the region
- `filter`: tonal shift from -1.0 (warmer/darker) through 1.0 (brighter)
- `rhythm`: density shift from -1.0 (sparser) through 1.0 (busier)
- `tempo`: set BPM from 60 through 180; target `all`

Supported effect names are `Reverb`, `Room`, `Echo`, `Chorus`, `Low-pass filter`, `Punch compressor`, `Shimmer`, and `Effects` for removal only.

Prefer the smallest action list that fulfills the request. A request such as "warm and spacious" needs both a negative filter action and reverb. Removing an effect must never become a mute action.

Actions are applied in order. Place `add-track` before any role-based instrument or modulator action that depends on the new track; those role-based actions bind to the most recently added matching track. Stable effect targets bind to the matching-role track that owns that effect ID. Never invent stable IDs for a newly added track in the same plan.

Treat arrangement terms as musical structures, not labels. For "add a drop" or "build up and then drop," return one `drop` action with an appropriate build fraction. Add another action only when the user asks for a distinct characteristic that the drop does not provide, such as reverb or a darker tone.
