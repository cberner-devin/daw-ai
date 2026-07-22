# DAW-AI patches

This is `surge-sys` from the official `surge-synthesizer/surge-rs` repository at
commit `7bfeafc76d1c57860a177e9e076bed7ec764009a`.

DAW-AI exports every CMake `-D` definition to the bridge compilation. Upstream
only exported definitions beginning with `SURGE`, so the bridge and engine saw
different feature macros and compiled incompatible C++ class layouts.

DAW-AI pins the cloned Surge XT engine to commit
`3c64680043bf8ef65cfcc6019e847c3f655c14fc`, the engine revision current when
the Rust binding commit was published. Building the alpha binding against a
later nightly changes native C++ class layouts and causes memory corruption.
