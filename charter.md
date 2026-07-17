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


### Implementation

The interface should be a local webserver with no authentication required. It should run on port 8888 by default.
The backend is written in Rust. The client code should be responsive and the UI should work on mobile or a desktop
browser.

The AI used should be the local Codex agent. Installing and authenticating Codex is a required part of the installation
process that the user must complete.

Since Codex is best at writing code and config files, the internal synth and other tools that DAW-AI uses should
be represented a way that is friendly for Codex.
