# DAW-AI patches

This is `surge-rs` from the official `surge-synthesizer/surge-rs` repository at
commit `7bfeafc76d1c57860a177e9e076bed7ec764009a`.

DAW-AI exposes Surge XT's existing block input buffer so resampled audio can be
routed through the synth's native Audio Input effect and the same native effect
chain used by MIDI instruments. The alpha binding otherwise exposes output only.
