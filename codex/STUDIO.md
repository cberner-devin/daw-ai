# DAW-AI synth edit contract

DAW-AI is a deterministic browser synthesizer. Codex plans edits; the Rust server validates the plan; the Web Audio client renders it.

## Track roles

- `drums`: synthesized kick, snare, and hats
- `bass`: monophonic subtractive bass
- `chords`: sustained polyphonic pad
- `lead`: monophonic melodic synth
- `texture`: long atmospheric tones

Use `all` when an edit should affect the complete mix. Use a role name for a targeted edit.

## Actions

Every action object has `kind`, `target`, `name`, and `value`. Use `name: "None"` and `value: 0` when those fields do not apply.

- `gain`: multiply regional level; value 0.0 through 2.0
- `mute`: silence the target in the region
- `drop`: create a build, short pre-impact gap, and high-energy impact; target `all`. Its value is the fraction of the selected region used for the build, from 0.25 through 0.60. Use 0.40 for a standard drop. The core reuses an existing lead, starts its hook at impact, and supplies the drum, bass, filter, and level contrast, so do not add another lead or rhythm action merely to make the drop audible.
- `add-track`: add the target role in the region
- `effect`: add a named effect with mix 0.0 through 1.0
- `remove-effect`: disable a named effect, or `Effects` for all effects, in the region
- `filter`: tonal shift from -1.0 (warmer/darker) through 1.0 (brighter)
- `rhythm`: density shift from -1.0 (sparser) through 1.0 (busier)
- `tempo`: set BPM from 60 through 180; target `all`

Supported effect names are `Reverb`, `Room`, `Echo`, `Chorus`, `Low-pass filter`, `Punch compressor`, `Shimmer`, and `Effects` for removal only.

Prefer the smallest action list that fulfills the request. A request such as "warm and spacious" needs both a negative filter action and reverb. Removing an effect must never become a mute action.

Treat arrangement terms as musical structures, not labels. For "add a drop" or "build up and then drop," return one `drop` action with an appropriate build fraction. Add another action only when the user asks for a distinct characteristic that the drop does not provide, such as reverb or a darker tone.
