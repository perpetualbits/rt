//! `rt_app` — the testable core of the rt binary.
//!
//! The GL rendering and the winit run-loop live in `main.rs` (they need a
//! display and cannot be unit-tested here). What *can* be tested — and is the
//! subtlest, most bug-prone part of a keyboard-driven app — is the translation
//! from a physical winit key event into rt's semantic [`Action`], plus the
//! encoding of ordinary typed keys into the bytes a PTY expects. Both live here
//! as pure functions with unit tests.

pub mod clip_history; // in-memory clipboard history: bounded most-recently-used ring
pub mod damage; // pure pixel-rect damage accumulator
pub mod input; // winit key/modifiers -> Chord, and typed-key -> PTY bytes
pub mod raster; // CPU anti-aliased coverage masks (disc/ring/bar), used by render.rs
pub mod render; // GL glyph-atlas renderer (also declared in main.rs for the bin);
                // exposed here so the offscreen pixel-identity gate can drive it
pub mod select; // anchored text selection mode — pure logic, no I/O
pub mod touch; // multi-touch gesture state (also declared in main.rs for the bin) — pure logic

pub use input::{chord_from_winit, encode_key};
