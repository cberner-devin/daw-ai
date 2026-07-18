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

Codex also offers standalone installers; see the linked official CLI documentation when Node.js is not available. DAW-AI invokes `codex exec` with GPT-5.6 Sol at high reasoning in an ephemeral, isolated workspace. It registers a local DAW-AI MCP server whose sound-graph tool validates each operation and returns actionable errors before the result reaches the live project.

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

The current project is stored as `sound-graph.json` in the working directory. Set `DAW_AI_PROJECT_PATH` to use another path. DAW-AI validates an existing file at startup, creates the demo graph when it is missing, and safely saves every accepted prompt, mixer change, Advanced edit, undo, and reset. This makes the graph directly inspectable and editable while the server is stopped.

For each prompt, Codex receives an isolated writable projection of that file containing the current graph and bounded active regional state, plus the selected range and the checked-in synth contract under `codex/`. Global temporary roots are excluded from the Codex sandbox. The JSON is a stable sound graph: MIDI clips own beat-relative notes; instruments, effects, and modulators expose numeric parameters; and routing publishes explicitly typed MIDI, audio, and control edges. Codex first forms a musical plan, then uses the registered `apply_sound_graph_edits` tool one or more times to write exact note timing, duration, pitch, and velocity as well as arrangement, instrument, modulation, level, effect, tempo, tone, and rhythmic-density changes. The server validates the completed graph again and commits it as one undoable change. Direct Advanced edits use the same persisted graph.

## Development

Development checks additionally require Node.js 22 or newer and Chrome or Chromium. Set `CHROME_PATH` when the browser executable is outside its usual system or Playwright-cache locations. Verify browser discovery with `just qa-browser-setup`.

Run formatting checks, Clippy with warnings denied, Rust tests, and the headless-browser workflow suite:

```sh
just test
```

The server binds only to `127.0.0.1`, requires no web authentication, and embeds the client assets in the executable. Its same-origin API lives under `/api`. Server warnings and errors go to stderr; handled and unhandled browser errors are forwarded to the same log with bounded messages. Reverse proxies can publish any hostname without DAW-AI configuration by keeping the upstream `Host` loopback-only and sending the public authority as `X-Forwarded-Host`. If a proxy preserves the public `Host`, set `DAW_AI_TRUSTED_HOSTS` to a comma-separated allowlist such as `studio.example,preview.example:8443`; entries without a port accept that hostname on any port. Arbitrary public hosts remain rejected to preserve DNS-rebinding protection. The test suite injects the deterministic demo planner with `DAW_AI_PROMPT_ENGINE=demo` and an isolated project path, so CI never needs Codex credentials or model usage.
