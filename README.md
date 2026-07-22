# DAW-AI

DAW-AI is a local, prompt-driven music studio for making music without learning a traditional DAW. Select a region of the timeline, describe the change in everyday language, and hear the arrangement update immediately.

The project is a Rust server with a responsive browser client. Surge XT executes instrument nodes in the backend through its official Rust bindings, and the browser only plays the resulting WAV. Prompted edits are produced by Gemini 3.5 Flash, which hears renders made by that same backend engine.

## Run it

Prerequisites:

- Rust 1.85 or newer
- `curl`
- CMake, a C++20 compiler, Clang/libclang, pkg-config, and OpenSSL development headers
- A [Gemini API key](https://ai.google.dev/gemini-api/docs/api-key)
- `just` (optional, but recommended)

On Ubuntu, the native build prerequisites can be installed with `apt install cmake clang libclang-dev pkg-config libssl-dev`. The first build downloads and compiles Surge XT and its submodules, so it is substantially slower than subsequent builds.

Set the standard environment variable:

```sh
export GEMINI_API_KEY="your-key"
```

For a system service, use systemd's `LoadCredential=` with a credential named `gemini-api-key`; DAW-AI automatically reads it from `CREDENTIALS_DIRECTORY`. This keeps the key out of the unit environment, process arguments, and home directories. For interactive use, `~/gemini_creds.txt` remains a fallback. A raw key, `GEMINI_API_KEY=...`, `Gemini API key: ...`, and `export GEMINI_API_KEY=...` are accepted. `DAW_AI_GEMINI_API_KEY` and `DAW_AI_GEMINI_CREDENTIALS` provide explicit overrides.

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
4. Switch to **Advanced** to choose Surge XT starter patches and edit native envelope, filter, resonance, and pitch controls alongside clip notes, ordered effect chains, modulators, routing, levels, and mute states. The **Debug** tab lists retained Gemini sessions and provides a copyable environment and browser-error report.

The current project is stored as `sound-graph.json` in the working directory. Set `DAW_AI_PROJECT_PATH` to use another path. DAW-AI validates an existing file at startup, creates the demo graph when it is missing, and safely saves every accepted prompt, mixer change, Advanced edit, undo, and reset. This makes the graph directly inspectable and editable while the server is stopped.

For each prompt, Gemini receives the selected edit range and the checked-in synth contract under `gemini/`. It can read the latest graph, apply validated edit batches, search for musical context, and choose the channels plus absolute project start/end times to render as WAV audio directly into its next multimodal turn. Listening is independent of edit scope, so Gemini can hear context before or after a transition. The Rust backend drives the complete Surge XT engine through the official alpha `surge-rs` bindings: Surge handles MIDI notes, polyphonic voices, oscillators, envelopes, filters, patch parameters, and audio blocks, after which DAW-AI applies its routed effects, automation, and master mix. The integration enforces an audible baseline before the first edit and another listen after every successful batch. Gemini evaluates pulse, subdivision, groove, tension, impact, timbre, and contrast from the audio itself, then iterates as needed.

When the producer claims completion, a fresh Gemini interaction receives the user request and exact latest WAV but none of the producer transcript. This independent judge accepts the result or returns detailed audible evidence and required corrections. A rejection forces another concrete edit and listen before the producer can request a new verdict. There is no predetermined iteration, judge-review, or tool-call limit; the overall 20-minute request timeout is the loop boundary. The server publishes each successful tool batch as an undoable edit while Gemini is still working, then records a completion marker only after the judge accepts. Direct Advanced edits and channel creation or deletion use the same persisted graph.

Prompted edits run as asynchronous jobs so reverse proxies never need to hold one request open while Gemini works. The browser polls short status requests, fetches each published intermediate project and the completed project, and shows the current phase, applied steps, and elapsed time. Gemini may spend up to 20 minutes on an edit; if the project changes before that edit finishes, the result is rejected instead of overwriting newer work.

Every Gemini session is retained locally with request/response JSON, graph state, metadata, and rendered WAV artifacts. By default sessions live beside `DAW_AI_PROJECT_PATH` in `gemini-sessions/`, or in the working directory's `gemini-sessions/` when no project path is configured. Override this with `DAW_AI_GEMINI_SESSION_DIR`. The Debug tab lists the latest sessions by timestamp.

## Development

Development checks additionally require Node.js 22 or newer and Chrome or Chromium. Set `CHROME_PATH` when the browser workflow suite cannot discover the executable. Verify browser discovery with `just qa-browser-setup`.

Run formatting checks, Clippy with warnings denied, Rust tests, and the headless-browser workflow suite:

```sh
just test
```

The server binds only to `127.0.0.1`, requires no web authentication, and embeds the client assets in the executable. Its same-origin API lives under `/api`. Server warnings and errors go to stderr; handled and unhandled browser errors are forwarded to the same log with bounded messages and retained in the Debug report for the current page session. Reverse proxies can publish any valid hostname without DAW-AI configuration, whether they preserve `Host` or provide the public authority as `X-Forwarded-Host`. Cross-origin mutations remain rejected. The test suite injects the deterministic demo planner with `DAW_AI_PROMPT_ENGINE=demo` and an isolated project path, so CI never needs Gemini credentials or model usage.

### Dependency policy

Third-party packages are reserved for complex, standards-sensitive boundaries or charter-mandated sound engines. `serde_json` handles JSON parsing and string escaping for Gemini and persisted project data. The official alpha `surge-rs` binding builds and statically links the complete Surge XT engine. Because Surge XT and its binding are GPL-3.0-or-later, the combined DAW-AI binary is licensed GPL-3.0-or-later. Domain-specific sound-graph validation and serialization remain in the project, while the narrow HTTP server, form decoder, temporary-file handling, CLI parser, browser harness, and `curl`-backed outbound HTTPS boundary continue to use platform tools and APIs.
