//! A Rust port of [BoopFace](https://github.com/CX-ArtLab/BoopFace), the
//! robot avatar by CX ART Lab (MIT; see `third_party/boopface`). The BoopFace
//! name, character and visual identity belong to CX ART Lab; this crate draws
//! their character, it is not agent-chat's own design.
//!
//! What it does:
//!
//! - [`FaceParams::from_jid`]: a stable face for a bare JID, coloured with the
//!   [`xep0392`] hue (agent-chat's mapping; BoopFace has no seed).
//! - [`render_svg`]: the static idle face as SVG, for GPUI's `img()`.
//! - [`Engine`]: the BoopFace Embedded motion engine (actions in, eye geometry
//!   out), seeded so it is deterministic; [`render_head_svg`] plus
//!   [`Eye::outline`] draw it.
//! - [`Director`]: which action fits a chat's state (agent-chat's own).
//!
//! No GPUI dependency: everything here is plain data and strings.

pub mod color;
pub mod director;
pub mod engine;
pub mod geometry;
pub mod params;
pub mod svg;
#[rustfmt::skip]
mod web_motion;
pub mod xep0392;

pub use director::{Director, Signal};
pub use engine::{Action, Engine, EngineConfig, Eye, Frame};
pub use params::{FaceParams, Look, Palette};
pub use svg::{VIEW_BOX, render_frame_svg, render_head_svg, render_svg};
/// SHA-256 of the BoopFace Web sources the motion clips were sampled from.
pub use web_motion::SOURCE_SHA256 as MOTION_SOURCE_SHA256;
