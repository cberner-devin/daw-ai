DAW-AI
------

This project is an AI powered DAW that is intended for people interested in making music
who do not have the skills to use a commercial DAW.

It should be simple to use and the interface should rely heavily on AI powered interactions.

### UI

The primary UI should be a timeline view of the track. The user can then select a portion of the track
with their mouse and enter a prompt for the AI describing the change to be made. This might be something
as simple as "increase volume" or as complex as "insert a sick drop here". The AI then makes those changes.

There is also an advanced view which the user can open that shows the details of the instruments, effects,...etc
that the agent has implemented for the track. This should be similar to a traditional DAW, but prioritize simple
interfaces over super powerful tools.

### Sound tools

The following sound tools should be implemented and available in the advanced view of the UI and also to the AI model.

* MIDI Clip: Contains notes, including their timing, duration, pitch, and velocity.
* Instrument: Produces sound from musical events. Instruments may be synthesizers or sample-based instruments and expose configurable parameters.
* Effect: Processes sound produced by an instrument, such as a filter, distortion, compressor, delay, or reverb, and exposes configurable parameters. May be chained with previous Effect.
* Modulator: Generates time-varying control values—such as envelopes, LFOs, or arbitrary curves—which can control any Instrument or Effect parameter.

Routing: Instruments, effects, and modulators can be connected into a signal chain.

### AI editing

The AI edits the sound graph. It is able to use any of the tools, and may construct the graph iteratively over many modifications.

The AI should first form a musical plan based on the user’s request, the selected region, and the existing composition,
and then produce the corresponding sound-graph changes. The system instructions should include concrete examples and
concise guidance connecting musical concepts to the available sound tools.

### Vetoed Implementations

The implementation MUST NOT hardcode niche sound tools such as a dubstep "drop" tool. All the tools should
be simple primitives that the AI (or user) uses to build the sound

### Implementation

The interface should be a local webserver with no authentication required. It should run on port 8888 by default.
It should support reverse proxy deployments without any configuration of the hostname by the user.

The backend is written in Rust. The client code should be responsive and the UI should work on mobile or a desktop
browser.

The AI used should be the local Codex agent with the 5.6-Sol model on High reasoning. Installing and authenticating Codex is a required part of the installation
process that the user must complete.

Since Codex is best at writing code and config files, the internal synth and other tools that DAW-AI uses should
be represented a way that is friendly for Codex.
