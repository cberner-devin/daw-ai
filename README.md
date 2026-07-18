# DAW-AI

DAW-AI is a local, prompt-driven music studio for making music without learning a traditional DAW. Select a region of the timeline, describe the change in everyday language, and hear the arrangement update immediately.

The project is a dependency-free Rust server with a responsive browser client. Audio is synthesized with the Web Audio API, so the included session is playable without samples. Prompted edits are planned by the locally installed Codex CLI using your existing Codex authentication.

## Run it

Prerequisites:

- Rust 1.85 or newer
- [Codex CLI](https://developers.openai.com/codex/cli/) installed and authenticated
- `just` (optional, but recommended)

Install Codex and complete its browser sign-in before starting DAW-AI:

```sh
npm install --global @openai/codex
codex login
codex login status
```

Codex also offers standalone installers; see the linked official CLI documentation when Node.js is not available. DAW-AI invokes `codex exec` with GPT-5.6 Sol at high reasoning in an ephemeral, read-only sandbox and validates its structured edit plan before changing the project.

Start the studio on the charter's default port:

```sh
just run
```

Then open <http://127.0.0.1:8888>.

Use another port when needed:

```sh
just run 9000
```

You can also run it directly:

```sh
cargo run -- --port 8888
```

## Studio workflow

1. Drag over any part of the arrangement to set the edit region. On touch devices, swipe to pan normally or tap **Select region** before dragging a selection.
2. Enter a request such as `increase the volume`, `add a bass`, `make the chords warm and spacious`, or `turn this section into a dubstep drop`.
3. Press **Make change**, then use the transport to hear the result.
4. Open **Advanced** to edit clip notes/drums, synth envelopes and waveforms, ordered effect chains, modulators, routing, levels, and mute states.

Codex receives the selected range, current project JSON, and the checked-in synth contract under `codex/`. The JSON is a stable sound graph: MIDI clips own beat-relative notes; instruments, effects, and modulators expose numeric parameters; and routing publishes explicitly typed MIDI, audio, and control edges. Codex first states a musical plan, then returns schema-constrained graph operations that can write exact note timing, duration, pitch, and velocity as well as arrangement, instrument, modulation, level, effect, tempo, tone, and rhythmic-density changes. Direct Advanced edits use the same graph and are undoable alongside prompted edits.

## Development

Development checks additionally require Node.js 22 or newer and Chrome or Chromium. Set `CHROME_PATH` when the browser executable is outside its usual system or Playwright-cache locations. Verify browser discovery with `just qa-browser-setup`.

Run formatting checks, Clippy with warnings denied, Rust tests, and the headless-browser workflow suite:

```sh
just test
```

The server binds only to `127.0.0.1`, requires no web authentication, and embeds the client assets in the executable. Its same-origin API lives under `/api`. Reverse proxies can publish any hostname without DAW-AI configuration by keeping the upstream `Host` loopback-only and sending the public authority as `X-Forwarded-Host`; this preserves both origin checks and DNS-rebinding protection. The test suite injects the deterministic demo planner with `DAW_AI_PROMPT_ENGINE=demo`, so CI never needs Codex credentials or model usage.
