//! rt's cross-process pane-handoff wire format.
//!
//! This crate is the compatibility contract between two rt processes of
//! possibly different versions. Read the spec before changing anything:
//! `docs/superpowers/specs/2026-08-29-cross-instance-pane-transfer-design.md`.
//!
//! It has no dependencies and must not gain any — see README.md.

pub mod error;
pub mod frame;

/// The protocol version this build speaks. Unrelated to the crate version.
pub const PROTO_V1: u32 = 1;

/// Leading bytes of a `Hello` body, so a wrong-protocol peer is diagnosed
/// immediately rather than as a confusing field error.
pub const MAGIC: &[u8; 9] = b"RTHANDOFF";
