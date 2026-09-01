# rt-handoff

The wire format for moving a pane or a tab from one rt process to another.
See `docs/superpowers/specs/2026-08-29-cross-instance-pane-transfer-design.md`.

## Why this crate has no dependencies

The format must still decode correctly in an rt built years from now. The way
that gets tested is: a future rt vendors an old copy of this crate and checks
its encoder against the old decoder. That only works while the crate is a
self-contained pile of Rust with no build graph behind it. Do not add
dependencies. Not `serde`, not `bytes`, not `thiserror`.

## What is frozen

Tag numbers, message type numbers, attribute bit values, the frame header
layout, and the run grammar. Tags may be ADDED. Nothing may be changed.
