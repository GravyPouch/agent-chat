# BoopFace (ported to Rust)

`crates/boopface` is a Rust port of parts of [BoopFace](https://github.com/CX-ArtLab/BoopFace)
by CX ART Lab, MIT licensed (see [`LICENSE`](LICENSE)). Ported from BoopFace v0.8.0 and
BoopFace Embedded v0.1.0, upstream commit `24ee5f698e58fa4f131b5ae7827373814e191f4c`.

The BoopFace character, its name and visual identity are CX ART Lab's original design.
agent-chat uses it as an avatar and does not present it as its own character.

| Rust file | Ported from |
|---|---|
| `src/geometry.rs` | `src/boopface-core.js` (head outline and transform), `src/boopface-geometry.js` (`flattenHeadPoints`), `src/boopface-head-roundness.js` (`setHeadRoundness`) |
| `src/svg.rs` | `src/boopface-core.js` (SVG shell, eyes, lids, head clip), `src/boopface-antenna.js` (antenna dot), `src/skins/native.js` (layered-gradient skin painter at the resting light), `src/skins/eyelids.js` |
| `src/engine.rs` | `embedded/lib/BoopFace/src/BoopFace.h`, `BoopFace.cpp` (actions, clip player, crossfade, gaze edge squash); idle glances and blinks from `src/boopface-core.js` (`_updateLook`, `_updateBlink`), seeded |
| `src/web_motion.rs` | `embedded/lib/BoopFace/src/BoopFaceWebMotion.h`, converted by a script (same numbers) |

Not ported: pointer following, drag and squeeze gestures, the antenna spring, head
motion, the 188 numbered skins and their patterns, the 1-bit rasteriser and display
drivers, and the gaze tracker.

agent-chat's own additions (not from BoopFace): `src/xep0392.rs` and `src/color.rs`
(XEP-0392 colours, HSLuv), `src/params.rs` (JID → face; BoopFace has no seed) and
`src/director.rs` (chat state → action).
