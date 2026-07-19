DAW-AI
------

This project is an AI powered DAW that is intended for people interested in making music
who do not have the skills to use a commercial DAW.

It should be simple to use and the interface should rely heavily on AI powered interactions.

### UI

#### AI Mode
"AI Mode" is the primary UI and it is a timeline view of the track. The user can then select a portion of the track
with their mouse and enter a prompt for the AI describing the change to be made. This might be something
as simple as "increase volume" or as complex as "insert a sick drop here". The AI then makes those changes.

#### Advanced Mode
There is also an advanced view which the user can open that shows the details of the instruments, effects,...etc
that the agent has implemented for the track. This should be similar to a traditional DAW, but prioritize simple
interfaces over super powerful tools. It allows the user to directly edit any of the channels, and also create
or delete entire channels of the sound graph.

These two views are separate tabs, each filling most of the screen, and there is a prominent tab near the top to
switch between AI Mode and Advanced.

There is also a third tab "Debug" which is a debugging pane showing error information, and other information
that is useful to a coding assistant. The information is easy for the user to copy and paste into an
external coding assistant, if they need help debugging issues in DAW AI itself. It can be assumed that
the user and coding assistant have access to the machine DAW AI is deployed on, to read additional logs...etc.

### Sound tools

The following sound tools should be implemented and available in the advanced view of the UI and also to the AI model.

* MIDI Clip: Contains notes, including their timing, duration, pitch, and velocity.
* Instrument: Produces sound from musical events. Instruments may be synthesizers or sample-based instruments and expose configurable parameters.
* Effect: Processes sound produced by an instrument, such as a filter, distortion, compressor, delay, or reverb, and exposes configurable parameters. May be chained with previous Effect.
* Modulator: Generates time-varying control values—such as envelopes, LFOs, or arbitrary curves—which can control any Instrument or Effect parameter.

Routing: Instruments, effects, and modulators can be connected into a sound graph.

Edges in the sound graph carry one of the following types:

* MIDI events: Timed musical events such as note-on, note-off, pitch, velocity, and other performance controls.
* Audio signal: Mono or stereo digital audio.
* Control signal: A time-varying numeric value used to control an Instrument or Effect parameter.

Connections must have compatible types:

* MIDI Clip -> Instrument: MIDI events
* Instrument -> Effect: Audio signal
* Effect -> Effect or Output: Audio signal
* Modulator -> Instrument or Effect parameter: Control signal

### AI editing

The AI edits the sound graph. It is able to use any of the tools, and may construct the graph iteratively over many modifications.

The AI should first form a musical plan based on the user’s request, the selected region, and the existing composition,
and then produce the corresponding sound-graph changes. The system instructions should include concrete examples and
concise guidance connecting musical concepts to the available sound tools.

### Error logs

The backend server logs errors and warnings to stderr. If the client code encounters an error it sends
it to the backend server to be included in the logs.

### Vetoed Implementations

The implementation MUST NOT hardcode niche sound tools such as a dubstep "drop" tool. All the tools should
be simple primitives that the AI (or user) uses to build the sound

### Implementation

The interface should be a local webserver with no authentication required. It should run on port 8888 by default.

The backend is written in Rust. The client code should be responsive and the UI should work on mobile or a desktop
browser.

The AI used should be the local Codex agent with the 5.6-Sol model on High reasoning. Installing and authenticating Codex is a required part of the installation
process that the user must complete.

Since Codex is best at writing code and config files, the internal synth and other tools that DAW-AI uses should
be represented a way that is friendly for Codex:
* The sound graph should be stored in a file on disk that Codex can edit directly
* Additionally, tools should be provided that are registered with Codex and that make the
  modifications to the sound graph and return useful error messages to Codex.
* Codex my perform multiple edits to fulfil a request, which show to the user incrementally

#### Codex "Listening"
Codex is not able to take audio as input, so DAW AI should register a few tools to help Codex analyze
the track to determine whether it meets the user's request. One of these should be a tool that
takes a channel(s) and a time range and renders a Mel Spectrogram as a PNG and returns it to Codex.
Other useful analysis tools are available as well.

#### Codex loop
Codex is told to operate in an implementation loop. It should:
* Make edits to the sound graph
* Use the "listening" tool(s)
* Consider whether the request has been completed
* Repeat, if necessary

DAW AI MUST NOT limit the number of iterations or tools calls, except with a long timeout on the whole request.

DAW AI should display incremental updates to the progress bar as Codex progresses.

### Deployment

The expected deployment is either as a local webserver, or on a private network where a gateway handles authentication.
To support the latter case, the DAW AI server must not restrict the hostname in requests.
Also to support the reverse proxy case, the server must be designed for reasonable timeouts and other characters
appropriate to deploy it behind nginx.
