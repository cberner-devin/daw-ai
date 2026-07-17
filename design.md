# DAW-AI interface design

## Chosen visual scheme

The studio uses a **midnight neon** scheme. It should feel focused and musical rather than like a dense engineering tool: near-black blue surfaces recede, warm off-white text stays readable, and a sharp acid-lime accent makes the primary action and transport state unmistakable.

Core colors:

- Canvas: `#101218`
- Raised surface: `#222530`
- Soft surface: `#191b23`
- Primary text: `#f4f2ed`
- Secondary text: `#a3a4ad`
- Primary accent: `#bdff5d`
- Secondary accent: `#a88aff`
- Error/destructive accent: `#ff8e9e`
- Ambient mint: `#74e0bc`

Track colors are role-based and remain consistent in the timeline and mixer:

- Drums: warm orange `#ffb86b`
- Bass: mint `#74e0bc`
- Chords: periwinkle `#8ca9ff`
- Lead: violet `#d99cff`
- Texture: rose `#ff91ad`

## Usage rules

- Reserve acid lime for the main action, active transport, focus rings, and small status highlights.
- Use track colors for musical identity, not general controls.
- Keep panels dark and low-contrast so clips, selections, and the playhead carry the visual hierarchy.
- Use mint and violet only as subtle ambient glows outside track-specific elements.
- Preserve strong text contrast and visible keyboard focus in every responsive layout.
- Prefer compact rounded controls and restrained gradients; avoid glossy skeuomorphism or a generic dashboard appearance.

The source of truth for exact implementation tokens is the `:root` block in `web/app.css`. This document records the design intent so future UI work preserves the scheme rather than replacing it accidentally.
