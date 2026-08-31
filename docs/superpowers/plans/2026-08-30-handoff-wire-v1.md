# rt-handoff Wire Format v1 — Implementation Plan (Phase 1)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `rt-handoff`, a zero-dependency crate holding the frozen v1 wire format for moving panes and tabs between rt processes, with round-trip property tests, unknown-tag injection tests, and a committed golden corpus.

**Architecture:** A pure encode/decode library over a fixed 8-byte frame header, LEB128 varints and tag-length-value fields. No sockets, no fds, no GUI, no engine — those arrive in phase 2+. The crate's in-memory model (`PaneWire`, `Grid`, `Node`, message structs) is engine-neutral by design: it describes cells and DEC mode numbers, never rt's internal types, which is what makes the format survive both a version change and an engine change.

**Tech Stack:** Rust 2021, no external crates (not even `libc` in this phase). Tests are `#[cfg(test)]` unit modules plus integration tests under `crates/rt-handoff/tests/`. Randomised tests use a hand-rolled xorshift64 PRNG with printed seeds, matching the house pattern in `crates/vt-conformance/src/lib.rs:192`.

**Spec:** `docs/superpowers/specs/2026-08-29-cross-instance-pane-transfer-design.md` — read the "Wire format v1" section before starting. This plan implements it and nothing else.

## Global Constraints

- **Zero dependencies.** `crates/rt-handoff/Cargo.toml` has an empty `[dependencies]` and an empty `[dev-dependencies]`. The spec requires this so a future rt can vendor an old copy of the crate to check itself against. A task that wants a crate is a task that has gone wrong.
- **The format is frozen once it ships.** Tag numbers, message type numbers, attribute bit values and field layouts in the spec are contract, not suggestion. Copy them exactly.
- **R1** A tag is never reused, renumbered, or given a new meaning. Tags are only ever added.
- **R2** An unknown tag is skipped by its length. Never an error.
- **R3** An absent tag means the documented default.
- **R4** A v1-or-later encoder always emits every field the spec marks required.
- **R5** A field's meaning never depends on the negotiated version; only its presence does.
- **R6** The decoder returns the list of tags it skipped, and every message carries `tag_names` (0x0F) so the caller can name them.
- **R7** An encoder emits fields in **ascending tag order**; a decoder accepts any order.
- **Protocol version is `1`.** Crate version is `0.3.19`, matching the workspace. They are unrelated numbers and must not be conflated.
- **Endianness:** all fixed-width integers are little-endian. All other integers are LEB128 unsigned varints.
- **`MAX_PAYLOAD` is 64 MiB** (`64 * 1024 * 1024`).
- **Commits** follow the repo's conventional-commit style (`feat:`, `fix:`, `test:`, `chore:`) and carry whatever session trailer the executing harness requires.
- **Every task ends green.** `cargo test -p rt-handoff` passes before the commit.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/rt-handoff/Cargo.toml` | Manifest. Empty dependency lists, deliberately. |
| `crates/rt-handoff/README.md` | Why this crate has no dependencies and must not gain any. |
| `crates/rt-handoff/src/lib.rs` | Module wiring, `PROTO_V1`, `MAGIC`, re-exports. |
| `crates/rt-handoff/src/error.rs` | `WireError`, `Result`. Every decode failure is one of these. |
| `crates/rt-handoff/src/frame.rs` | The 8-byte frame header. Length-prefixed message envelope. |
| `crates/rt-handoff/src/buf.rs` | `Writer`/`Reader`: LEB128, fixed-width ints, length-prefixed bytes and strings. |
| `crates/rt-handoff/src/tlv.rs` | Field writer (enforces R7), field reader, `walk` (enforces R2 + R6). |
| `crates/rt-handoff/src/style.rs` | `Colour`, `Style`, attribute bits, the style table. |
| `crates/rt-handoff/src/grid.rs` | `Run`, `Line`, `Grid` and the run-grammar validator. |
| `crates/rt-handoff/src/pane.rs` | `PaneWire` and its sub-structs: the 0x01–0x51 field set. |
| `crates/rt-handoff/src/tree.rs` | `Node`: leaf / hsplit / vsplit / tabs. |
| `crates/rt-handoff/src/msg.rs` | `Hello`, `Offer`, `Claim`, `ScrollChunk`, `PaneFds`, `Adopted`, `Failed`, and the frame dispatch. |
| `crates/rt-handoff/src/testgen.rs` | Deterministic model generator (xorshift64). Compiled unconditionally so `tests/` can reach it. |
| `crates/rt-handoff/tests/roundtrip.rs` | Encode → decode → compare, 10 000 seeds. |
| `crates/rt-handoff/tests/forward_compat.rs` | Unknown-tag injection at every nesting level. |
| `crates/rt-handoff/tests/golden.rs` | The committed corpus every future version must decode. |
| `crates/rt-handoff/tests/fixtures/wire-v1/*.bin` | The corpus itself. |
| `crates/rt-handoff/src/bin/regen_fixtures.rs` | Regenerates the corpus. Run by hand, never by tests. |

Splitting `pane.rs` from `grid.rs` and `style.rs` matters: the grid encoding is the part that runs millions of times and the part most likely to be optimised later, and it must stay readable on its own.

---

### Task 1: Crate scaffold, error type, framing

**Files:**
- Create: `crates/rt-handoff/Cargo.toml`
- Create: `crates/rt-handoff/README.md`
- Create: `crates/rt-handoff/src/lib.rs`
- Create: `crates/rt-handoff/src/error.rs`
- Create: `crates/rt-handoff/src/frame.rs`
- Modify: `Cargo.toml` (workspace `members`)
- Modify: `ci/verify.sh` (add the crate to the test battery)

**Interfaces:**
- Consumes: nothing.
- Produces: `rt_handoff::error::{WireError, Result}`; `rt_handoff::frame::{Frame, MAX_PAYLOAD, HEADER_LEN}`; `rt_handoff::{PROTO_V1, MAGIC}`.

- [ ] **Step 1: Create the manifest**

`crates/rt-handoff/Cargo.toml`:

```toml
[package]
name = "rt-handoff"
version = "0.3.19"
edition = "2021"
license = "GPL-3.0-or-later"
description = "rt's frozen cross-process pane-handoff wire format. No dependencies, on purpose: a future rt must be able to vendor an old copy of this crate to check itself against."
publish = false

# INTENTIONALLY EMPTY. See README.md. Adding a dependency here breaks the
# vendor-an-old-copy compatibility check that the whole format relies on.
[dependencies]

[dev-dependencies]
```

- [ ] **Step 2: Create the README**

`crates/rt-handoff/README.md`:

```markdown
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
```

- [ ] **Step 3: Add the crate to the workspace**

In the root `Cargo.toml`, add to `members`, after the `"crates/rt-session"` line:

```toml
    "crates/rt-handoff",             # frozen cross-process pane-handoff wire format (no deps)
```

- [ ] **Step 4: Write the failing framing test**

`crates/rt-handoff/src/frame.rs` — write the whole file as tests first:

```rust
//! The frame envelope: an 8-byte little-endian header followed by a payload.
//!
//! Framing is deliberately dumber than the payload: a fixed header a receiver
//! can read without knowing anything about the protocol version, so a version
//! mismatch is diagnosed from a decoded `Hello` rather than from a parse crash.

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips() {
        let f = Frame { msg_type: 0x05, flags: 0, payload: vec![1, 2, 3] };
        let mut out = Vec::new();
        f.encode(&mut out).unwrap();
        assert_eq!(out.len(), HEADER_LEN + 3);
        let (back, used) = Frame::decode(&out).unwrap();
        assert_eq!(back, f);
        assert_eq!(used, out.len());
    }

    #[test]
    fn two_frames_decode_back_to_back() {
        let a = Frame { msg_type: 1, flags: 0, payload: vec![9] };
        let b = Frame { msg_type: 2, flags: 3, payload: vec![] };
        let mut out = Vec::new();
        a.encode(&mut out).unwrap();
        b.encode(&mut out).unwrap();
        let (fa, used_a) = Frame::decode(&out).unwrap();
        let (fb, used_b) = Frame::decode(&out[used_a..]).unwrap();
        assert_eq!(fa, a);
        assert_eq!(fb, b);
        assert_eq!(used_a + used_b, out.len());
    }

    #[test]
    fn truncated_header_is_truncated_not_panic() {
        let err = Frame::decode(&[0, 0, 0]).unwrap_err();
        assert_eq!(err, WireError::Truncated { need: HEADER_LEN, had: 3 });
    }

    #[test]
    fn truncated_payload_reports_the_full_need() {
        let f = Frame { msg_type: 7, flags: 0, payload: vec![1, 2, 3, 4] };
        let mut out = Vec::new();
        f.encode(&mut out).unwrap();
        out.truncate(HEADER_LEN + 2);
        let err = Frame::decode(&out).unwrap_err();
        assert_eq!(err, WireError::Truncated { need: HEADER_LEN + 4, had: HEADER_LEN + 2 });
    }

    #[test]
    fn oversize_declared_length_is_rejected_without_allocating() {
        // A hostile or corrupt header claiming 4 GiB must not become a Vec.
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        let err = Frame::decode(&buf).unwrap_err();
        assert_eq!(err, WireError::PayloadTooLarge { len: u32::MAX as u64 });
    }

    #[test]
    fn oversize_payload_is_rejected_on_encode() {
        let f = Frame { msg_type: 1, flags: 0, payload: vec![0; MAX_PAYLOAD + 1] };
        let err = f.encode(&mut Vec::new()).unwrap_err();
        assert_eq!(err, WireError::PayloadTooLarge { len: MAX_PAYLOAD as u64 + 1 });
    }
}
```

- [ ] **Step 5: Run the test to verify it fails**

Run: `cargo test -p rt-handoff`
Expected: FAIL — `cannot find type Frame in this scope`, plus errors for `error.rs` not existing yet.

- [ ] **Step 6: Write the error type**

`crates/rt-handoff/src/error.rs`:

```rust
//! Every way a decode can fail. One flat enum, comparable with `assert_eq!`,
//! so tests pin the exact failure rather than "it errored somehow".

/// A decode or encode failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WireError {
    /// The buffer ended before the value did. `need` is the total length that
    /// would have been required from the start of the item, `had` what existed.
    Truncated { need: usize, had: usize },
    /// A LEB128 varint ran past ten bytes, so it cannot fit a u64.
    VarintOverflow,
    /// A length-prefixed string was not valid UTF-8.
    BadUtf8,
    /// A frame declared, or was asked to carry, more than `MAX_PAYLOAD` bytes.
    PayloadTooLarge { len: u64 },
    /// A frame header carried a message type this build has no decoder for.
    UnknownMsgType(u16),
    /// A field the spec marks required was absent (rule R4 was violated by the peer).
    MissingField { tag: u64 },
    /// A field was present but its contents are impossible.
    BadValue { tag: u64, why: &'static str },
    /// A body decoded successfully but had bytes left over.
    TrailingBytes { left: usize },
}

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            WireError::Truncated { need, had } => write!(f, "truncated: needed {need} bytes, had {had}"),
            WireError::VarintOverflow => write!(f, "varint longer than 10 bytes"),
            WireError::BadUtf8 => write!(f, "string field was not valid UTF-8"),
            WireError::PayloadTooLarge { len } => write!(f, "payload of {len} bytes exceeds the 64 MiB cap"),
            WireError::UnknownMsgType(t) => write!(f, "unknown message type 0x{t:02x}"),
            WireError::MissingField { tag } => write!(f, "required field 0x{tag:02x} was absent"),
            WireError::BadValue { tag, why } => write!(f, "field 0x{tag:02x} is invalid: {why}"),
            WireError::TrailingBytes { left } => write!(f, "{left} trailing bytes after a complete body"),
        }
    }
}

impl std::error::Error for WireError {}

/// Every fallible operation in this crate returns this.
pub type Result<T> = std::result::Result<T, WireError>;
```

- [ ] **Step 7: Write the framing implementation**

Prepend to `crates/rt-handoff/src/frame.rs`, above the `mod tests` block (keep the module doc comment at the very top):

```rust
use crate::error::{Result, WireError};

/// The fixed header: u32 payload length, u16 message type, u16 flags.
pub const HEADER_LEN: usize = 8;

/// The largest payload a single frame may carry. A pane's screen and one
/// scrollback chunk both fit far inside this; the cap exists so a corrupt or
/// hostile length cannot make a receiver allocate the world.
pub const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

/// One length-prefixed message on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub msg_type: u16,
    pub flags: u16,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Append this frame's bytes to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(WireError::PayloadTooLarge { len: self.payload.len() as u64 });
        }
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.msg_type.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.payload);
        Ok(())
    }

    /// Decode one frame from the front of `buf`, returning it and the number of
    /// bytes consumed. The length is validated against `MAX_PAYLOAD` BEFORE any
    /// allocation, so a bogus header costs nothing.
    pub fn decode(buf: &[u8]) -> Result<(Frame, usize)> {
        if buf.len() < HEADER_LEN {
            return Err(WireError::Truncated { need: HEADER_LEN, had: buf.len() });
        }
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if len > MAX_PAYLOAD {
            return Err(WireError::PayloadTooLarge { len: len as u64 });
        }
        let total = HEADER_LEN + len;
        if buf.len() < total {
            return Err(WireError::Truncated { need: total, had: buf.len() });
        }
        Ok((
            Frame {
                msg_type: u16::from_le_bytes([buf[4], buf[5]]),
                flags: u16::from_le_bytes([buf[6], buf[7]]),
                payload: buf[HEADER_LEN..total].to_vec(),
            },
            total,
        ))
    }
}
```

- [ ] **Step 8: Write the crate root**

`crates/rt-handoff/src/lib.rs`:

```rust
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
```

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cargo test -p rt-handoff`
Expected: PASS, 6 tests.

- [ ] **Step 10: Add the crate to the multi-arch battery**

In `ci/verify.sh`, extend both the package list and the test command so the format is checked on riscv64 too — varint and little-endian handling is exactly the kind of thing that only breaks on another architecture:

```bash
PKGS=(-p vt-parser -p vt-conformance -p rt-handoff)
```

and in `tests_cmd`, change `cargo test -q -p vt-parser -p vt-conformance` to:

```bash
tests_cmd='cargo test -q -p vt-parser -p vt-conformance -p rt-handoff 2>&1 | grep -E "test result:|error\[|error:|FAILED|panicked"'
```

- [ ] **Step 11: Commit**

```bash
git add crates/rt-handoff Cargo.toml ci/verify.sh
git commit -m "feat(handoff): crate scaffold, error type and frame envelope"
```

---

### Task 2: LEB128 varints and buffer primitives

**Files:**
- Create: `crates/rt-handoff/src/buf.rs`
- Modify: `crates/rt-handoff/src/lib.rs` (add `pub mod buf;`)

**Interfaces:**
- Consumes: `error::{Result, WireError}` from Task 1.
- Produces: `buf::Writer` with `varint(u64)`, `u8(u8)`, `u32le(u32)`, `bytes(&[u8])`, `str(&str)`, `into_vec() -> Vec<u8>`; `buf::Reader<'a>` with `varint() -> Result<u64>`, `u8() -> Result<u8>`, `u32le() -> Result<u32>`, `take(usize) -> Result<&'a [u8]>`, `bytes() -> Result<&'a [u8]>`, `str() -> Result<String>`, `remaining() -> usize`, `finish() -> Result<()>`.

- [ ] **Step 1: Write the failing tests**

`crates/rt-handoff/src/buf.rs`, tests first:

```rust
//! Byte-level primitives shared by every encoder in this crate.
//!
//! `Writer` never fails: it appends to a `Vec`. `Reader` fails only by running
//! out of bytes or by meeting a varint that cannot be a u64.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WireError;

    #[test]
    fn varints_round_trip_across_the_boundaries() {
        for v in [0u64, 1, 63, 127, 128, 129, 255, 256, 16383, 16384, 300, u32::MAX as u64, u64::MAX] {
            let mut w = Writer::new();
            w.varint(v);
            let bytes = w.into_vec();
            let mut r = Reader::new(&bytes);
            assert_eq!(r.varint().unwrap(), v, "value {v}");
            assert_eq!(r.remaining(), 0, "value {v} left bytes behind");
        }
    }

    #[test]
    fn small_varints_are_one_byte() {
        let mut w = Writer::new();
        w.varint(127);
        assert_eq!(w.into_vec().len(), 1);
    }

    #[test]
    fn truncated_varint_is_truncated() {
        // 0x80 has the continuation bit set but nothing follows.
        let mut r = Reader::new(&[0x80]);
        assert_eq!(r.varint().unwrap_err(), WireError::Truncated { need: 1, had: 0 });
    }

    #[test]
    fn overlong_varint_overflows_rather_than_wrapping() {
        let bytes = [0x80u8; 11];
        let mut r = Reader::new(&bytes);
        assert_eq!(r.varint().unwrap_err(), WireError::VarintOverflow);
    }

    #[test]
    fn a_ten_byte_varint_past_u64_is_rejected_not_truncated() {
        // Nine 0xFF bytes carry 63 bits; the tenth byte holds one meaningful
        // bit. A tenth byte of 0x03 needs bit 64, so it is not a u64 — it must
        // error rather than decode to the same value 0x01 would give.
        let mut over = [0xFFu8; 10];
        over[9] = 0x03;
        assert_eq!(Reader::new(&over).varint().unwrap_err(), WireError::VarintOverflow);

        let mut max = [0xFFu8; 10];
        max[9] = 0x01;
        assert_eq!(Reader::new(&max).varint().unwrap(), u64::MAX);
    }

    #[test]
    fn strings_round_trip_including_non_ascii() {
        for s in ["", "hello", "héllo wörld", "日本語", "a\u{0}b"] {
            let mut w = Writer::new();
            w.str(s);
            let bytes = w.into_vec();
            let mut r = Reader::new(&bytes);
            assert_eq!(r.str().unwrap(), s);
            r.finish().unwrap();
        }
    }

    #[test]
    fn invalid_utf8_is_bad_utf8() {
        let mut w = Writer::new();
        w.bytes(&[0xff, 0xfe]);
        let bytes = w.into_vec();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.str().unwrap_err(), WireError::BadUtf8);
    }

    #[test]
    fn fixed_width_is_little_endian() {
        let mut w = Writer::new();
        w.u32le(0x0102_0304);
        assert_eq!(w.into_vec(), vec![0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn finish_rejects_trailing_bytes() {
        let r = Reader::new(&[1, 2, 3]);
        assert_eq!(r.finish().unwrap_err(), WireError::TrailingBytes { left: 3 });
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt-handoff buf`
Expected: FAIL — `cannot find type Writer in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/rt-handoff/src/buf.rs`, below the module doc comment:

```rust
use crate::error::{Result, WireError};

/// An append-only byte builder. Infallible by construction.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer { buf: Vec::new() }
    }

    /// LEB128 unsigned. Seven bits per byte, high bit means "more follows".
    pub fn varint(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(byte);
                return;
            }
            self.buf.push(byte | 0x80);
        }
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u32le(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Length-prefixed raw bytes.
    pub fn bytes(&mut self, b: &[u8]) {
        self.varint(b.len() as u64);
        self.buf.extend_from_slice(b);
    }

    /// Length-prefixed UTF-8.
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    /// Raw append, no length prefix. For nesting an already-encoded body.
    pub fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// A cursor over a byte slice. Every method either advances or fails.
#[derive(Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(WireError::Truncated { need: n, had: self.remaining() });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32le(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// LEB128 unsigned, capped at ten bytes — the most a u64 can occupy.
    ///
    /// The tenth byte carries only ONE meaningful bit (9 * 7 = 63 bits precede
    /// it), so a tenth byte above 1 encodes a value that cannot be a u64. It is
    /// rejected rather than silently truncated: a peer's corrupt varint must be
    /// an error, and two byte sequences must never decode to one value — that
    /// would put a hole in the canonical encoding the golden corpus rests on.
    pub fn varint(&mut self) -> Result<u64> {
        let mut out: u64 = 0;
        let mut shift = 0u32;
        for i in 0..10 {
            let byte = self.u8()?;
            let chunk = (byte & 0x7f) as u64;
            if i == 9 && chunk > 1 {
                return Err(WireError::VarintOverflow);
            }
            out |= chunk << shift;
            if byte & 0x80 == 0 {
                return Ok(out);
            }
            shift += 7;
        }
        Err(WireError::VarintOverflow)
    }

    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.varint()? as usize;
        self.take(n)
    }

    pub fn str(&mut self) -> Result<String> {
        let b = self.bytes()?;
        std::str::from_utf8(b).map(str::to_owned).map_err(|_| WireError::BadUtf8)
    }

    /// Assert the body is fully consumed. Catches an encoder that wrote more
    /// than the decoder reads, which is otherwise a silent divergence.
    pub fn finish(self) -> Result<()> {
        if self.remaining() != 0 {
            return Err(WireError::TrailingBytes { left: self.remaining() });
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Wire the module into the crate root**

In `crates/rt-handoff/src/lib.rs`, add below `pub mod error;`:

```rust
pub mod buf;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rt-handoff`
Expected: PASS, 14 tests.

- [ ] **Step 6: Commit**

```bash
git add crates/rt-handoff/src/buf.rs crates/rt-handoff/src/lib.rs
git commit -m "feat(handoff): LEB128 varints and buffer primitives"
```

---

### Task 3: TLV fields — the forward-compatibility machinery

This task is where rules R2, R6 and R7 become code. Every message body in the
crate goes through it, so no decoder can forget to skip unknown tags and no
encoder can forget to sort.

**Files:**
- Create: `crates/rt-handoff/src/tlv.rs`
- Modify: `crates/rt-handoff/src/lib.rs` (add `pub mod tlv;`)

**Interfaces:**
- Consumes: `buf::{Writer, Reader}`, `error::{Result, WireError}`.
- Produces: `tlv::FieldWriter` with `new()`, `field(u64, &[u8])`, `into_vec() -> Vec<u8>`; `tlv::walk(&[u8], impl FnMut(u64, &[u8]) -> Result<bool>) -> Result<Vec<u64>>`; `tlv::tags_of(&[u8]) -> Result<Vec<u64>>`.

- [ ] **Step 1: Write the failing tests**

`crates/rt-handoff/src/tlv.rs`, tests first:

```rust
//! Tag-length-value fields.
//!
//! The whole cross-version story lives here. A decoder walks fields it knows
//! and skips the rest by length (R2), reporting which tags it skipped (R6). An
//! encoder emits in ascending tag order (R7), which makes the encoding
//! canonical so the golden corpus can be compared byte-for-byte.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WireError;

    #[test]
    fn fields_round_trip() {
        let mut fw = FieldWriter::new();
        fw.field(0x01, &[1, 2, 3]);
        fw.field(0x20, b"hello");
        let body = fw.into_vec();

        let mut seen: Vec<(u64, Vec<u8>)> = Vec::new();
        let unknown = walk(&body, |tag, val| {
            seen.push((tag, val.to_vec()));
            Ok(true)
        })
        .unwrap();

        assert_eq!(seen, vec![(0x01, vec![1, 2, 3]), (0x20, b"hello".to_vec())]);
        assert!(unknown.is_empty());
    }

    #[test]
    fn unknown_tags_are_skipped_and_reported() {
        let mut fw = FieldWriter::new();
        fw.field(0x01, &[7]);
        fw.field(0x99, &[0; 40]); // a field from a future version
        fw.field(0xFA, b"also future");
        let body = fw.into_vec();

        let mut known = Vec::new();
        let unknown = walk(&body, |tag, val| {
            if tag == 0x01 {
                known.push(val.to_vec());
                Ok(true)
            } else {
                Ok(false) // "I do not know this tag"
            }
        })
        .unwrap();

        assert_eq!(known, vec![vec![7]]);
        assert_eq!(unknown, vec![0x99, 0xFA]);
    }

    #[test]
    fn decoder_accepts_descending_order_even_though_encoders_must_not_emit_it() {
        // R7 binds encoders, not decoders. Hand-build a descending body.
        let mut w = crate::buf::Writer::new();
        w.varint(0x20);
        w.bytes(b"b");
        w.varint(0x01);
        w.bytes(b"a");
        let body = w.into_vec();

        let mut seen = Vec::new();
        walk(&body, |tag, _| {
            seen.push(tag);
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen, vec![0x20, 0x01]);
    }

    #[test]
    fn writer_emits_ascending_and_tags_of_agrees() {
        let mut fw = FieldWriter::new();
        fw.field(0x01, &[]);
        fw.field(0x0F, &[]);
        fw.field(0x40, &[]);
        let body = fw.into_vec();
        assert_eq!(tags_of(&body).unwrap(), vec![0x01, 0x0F, 0x40]);
    }

    #[test]
    #[should_panic(expected = "ascending")]
    fn writer_panics_on_out_of_order_tags() {
        let mut fw = FieldWriter::new();
        fw.field(0x20, &[]);
        fw.field(0x01, &[]);
    }

    #[test]
    fn truncated_field_value_is_truncated() {
        let mut w = crate::buf::Writer::new();
        w.varint(0x01);
        w.varint(10); // claims ten bytes
        w.raw(&[1, 2]); // provides two
        let body = w.into_vec();
        let err = walk(&body, |_, _| Ok(true)).unwrap_err();
        assert_eq!(err, WireError::Truncated { need: 10, had: 2 });
    }

    #[test]
    fn empty_body_walks_cleanly() {
        assert!(walk(&[], |_, _| Ok(true)).unwrap().is_empty());
    }

    #[test]
    fn a_field_may_be_empty() {
        let mut fw = FieldWriter::new();
        fw.field(0x03, &[]);
        let body = fw.into_vec();
        let mut lens = Vec::new();
        walk(&body, |_, val| {
            lens.push(val.len());
            Ok(true)
        })
        .unwrap();
        assert_eq!(lens, vec![0]);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt-handoff tlv`
Expected: FAIL — `cannot find type FieldWriter in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/rt-handoff/src/tlv.rs`, below the module doc comment:

```rust
use crate::buf::{Reader, Writer};
use crate::error::Result;

/// Builds a TLV body, enforcing R7 (ascending tag order).
#[derive(Debug, Default)]
pub struct FieldWriter {
    w: Writer,
    last_tag: Option<u64>,
}

impl FieldWriter {
    pub fn new() -> Self {
        FieldWriter { w: Writer::new(), last_tag: None }
    }

    /// Append one field. Tags must be strictly ascending — a violation is an
    /// encoder bug, not a runtime condition, so it panics in every build. A
    /// non-canonical encoding would silently break the golden corpus instead.
    pub fn field(&mut self, tag: u64, value: &[u8]) {
        if let Some(prev) = self.last_tag {
            assert!(tag > prev, "fields must be emitted in ascending tag order (R7): 0x{tag:02x} after 0x{prev:02x}");
        }
        self.last_tag = Some(tag);
        self.w.varint(tag);
        self.w.bytes(value);
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.w.into_vec()
    }
}

/// Walk a TLV body. `f` is called with each field and returns whether it
/// claimed the tag; unclaimed tags are skipped and returned in order.
///
/// This is the single implementation of rules R2 and R6. Message decoders call
/// it rather than reading fields themselves, so neither rule can be forgotten
/// in one message and honoured in another.
pub fn walk<'a>(body: &'a [u8], mut f: impl FnMut(u64, &'a [u8]) -> Result<bool>) -> Result<Vec<u64>> {
    let mut r = Reader::new(body);
    let mut unknown = Vec::new();
    while !r.is_empty() {
        let tag = r.varint()?;
        let value = r.bytes()?;
        if !f(tag, value)? {
            unknown.push(tag);
        }
    }
    Ok(unknown)
}

/// Every tag present in a body, in wire order. Used by the golden corpus test
/// and by `tag_names` construction.
pub fn tags_of(body: &[u8]) -> Result<Vec<u64>> {
    let mut tags = Vec::new();
    walk(body, |tag, _| {
        tags.push(tag);
        Ok(true)
    })?;
    Ok(tags)
}
```

- [ ] **Step 4: Wire the module into the crate root**

In `crates/rt-handoff/src/lib.rs`, add:

```rust
pub mod tlv;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rt-handoff`
Expected: PASS, 22 tests.

- [ ] **Step 6: Commit**

```bash
git add crates/rt-handoff/src/tlv.rs crates/rt-handoff/src/lib.rs
git commit -m "feat(handoff): TLV fields with ascending-order encoding and unknown-tag skipping"
```

---

### Task 4: Colours, styles and the style table

**Files:**
- Create: `crates/rt-handoff/src/style.rs`
- Modify: `crates/rt-handoff/src/lib.rs` (add `pub mod style;`)

**Interfaces:**
- Consumes: `buf::{Writer, Reader}`, `error::{Result, WireError}`.
- Produces: `style::Colour` (`Default` | `Indexed(u8)` | `Rgb(u8,u8,u8)`) with `write(&self, &mut Writer)` and `read(&mut Reader, u64) -> Result<Colour>`; `style::Style { fg, bg, underline, attrs: u32, link_id: u32 }` with `write`/`read`; `style::attrs::*` bit constants and `attrs::KNOWN`; `style::write_table(&[Style]) -> Vec<u8>` and `style::read_table(&[u8]) -> Result<Vec<Style>>`.

The `u64` argument to `read` is the tag being decoded, so a `BadValue` names
the field it came from rather than a generic "somewhere in the pane".

- [ ] **Step 1: Write the failing tests**

`crates/rt-handoff/src/style.rs`, tests first:

```rust
//! Cell appearance: colours, attribute bits, and the per-pane style table that
//! deduplicates them.
//!
//! Indexed colours stay indexed. Flattening them to RGB would make a moved
//! pane stop following the receiving window's colour scheme, which is a
//! visible regression and an irreversible one — the index is gone.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buf::{Reader, Writer};
    use crate::error::WireError;

    fn round_trip_colour(c: Colour) -> Colour {
        let mut w = Writer::new();
        c.write(&mut w);
        let bytes = w.into_vec();
        assert_eq!(bytes.len(), 4, "a colour is always four bytes");
        let mut r = Reader::new(&bytes);
        Colour::read(&mut r, 0x3F).unwrap()
    }

    #[test]
    fn every_colour_kind_round_trips() {
        for c in [Colour::Default, Colour::Indexed(0), Colour::Indexed(255), Colour::Rgb(1, 2, 3), Colour::Rgb(255, 255, 255)] {
            assert_eq!(round_trip_colour(c), c, "colour {c:?}");
        }
    }

    #[test]
    fn indexed_is_not_flattened_to_rgb() {
        // The whole point: an index survives as an index.
        assert_eq!(round_trip_colour(Colour::Indexed(4)), Colour::Indexed(4));
    }

    #[test]
    fn unknown_colour_kind_is_bad_value() {
        let mut r = Reader::new(&[9, 0, 0, 0]);
        assert_eq!(
            Colour::read(&mut r, 0x27).unwrap_err(),
            WireError::BadValue { tag: 0x27, why: "colour kind must be 0, 1 or 2" }
        );
    }

    #[test]
    fn style_round_trips() {
        let s = Style {
            fg: Colour::Indexed(12),
            bg: Colour::Rgb(10, 20, 30),
            underline: Colour::Default,
            attrs: attrs::BOLD | attrs::ITALIC | attrs::CURLY_UNDERLINE,
            link_id: 7,
        };
        let mut w = Writer::new();
        s.write(&mut w);
        let bytes = w.into_vec();
        let mut r = Reader::new(&bytes);
        assert_eq!(Style::read(&mut r, 0x3F).unwrap(), s);
    }

    #[test]
    fn unknown_attribute_bits_are_masked_off() {
        // A future version sets bit 20. We must not carry it into our model,
        // where it would be re-emitted as a bit we cannot describe.
        let s = Style { attrs: attrs::BOLD | (1 << 20), ..Style::default() };
        let mut w = Writer::new();
        s.write(&mut w);
        let bytes = w.into_vec();
        let mut r = Reader::new(&bytes);
        assert_eq!(Style::read(&mut r, 0x3F).unwrap().attrs, attrs::BOLD);
    }

    #[test]
    fn attribute_bit_values_are_frozen() {
        // These numbers are the contract. If this test needs editing, stop.
        assert_eq!(attrs::BOLD, 1);
        assert_eq!(attrs::DIM, 2);
        assert_eq!(attrs::ITALIC, 4);
        assert_eq!(attrs::UNDERLINE, 8);
        assert_eq!(attrs::DOUBLE_UNDERLINE, 16);
        assert_eq!(attrs::CURLY_UNDERLINE, 32);
        assert_eq!(attrs::DOTTED_UNDERLINE, 64);
        assert_eq!(attrs::DASHED_UNDERLINE, 128);
        assert_eq!(attrs::BLINK, 256);
        assert_eq!(attrs::RAPID_BLINK, 512);
        assert_eq!(attrs::REVERSE, 1024);
        assert_eq!(attrs::HIDDEN, 2048);
        assert_eq!(attrs::STRIKEOUT, 4096);
        assert_eq!(attrs::OVERLINE, 8192);
        assert_eq!(attrs::KNOWN, 16383);
    }

    #[test]
    fn style_table_round_trips() {
        let table = vec![
            Style::default(),
            Style { fg: Colour::Indexed(1), ..Style::default() },
            Style { attrs: attrs::REVERSE, link_id: 3, ..Style::default() },
        ];
        let bytes = write_table(&table);
        assert_eq!(read_table(&bytes).unwrap(), table);
    }

    #[test]
    fn empty_style_table_round_trips() {
        let bytes = write_table(&[]);
        assert_eq!(read_table(&bytes).unwrap(), Vec::<Style>::new());
    }

    #[test]
    fn style_table_with_trailing_junk_is_rejected() {
        let mut bytes = write_table(&[Style::default()]);
        bytes.push(0xff);
        assert_eq!(read_table(&bytes).unwrap_err(), WireError::TrailingBytes { left: 1 });
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt-handoff style`
Expected: FAIL — `cannot find type Colour in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/rt-handoff/src/style.rs`, below the module doc comment:

```rust
use crate::buf::{Reader, Writer};
use crate::error::{Result, WireError};

/// Text attribute bits. Frozen at v1: these numbers are on the wire.
pub mod attrs {
    pub const BOLD: u32 = 1;
    pub const DIM: u32 = 2;
    pub const ITALIC: u32 = 4;
    pub const UNDERLINE: u32 = 8;
    pub const DOUBLE_UNDERLINE: u32 = 16;
    pub const CURLY_UNDERLINE: u32 = 32;
    pub const DOTTED_UNDERLINE: u32 = 64;
    pub const DASHED_UNDERLINE: u32 = 128;
    pub const BLINK: u32 = 256;
    pub const RAPID_BLINK: u32 = 512;
    pub const REVERSE: u32 = 1024;
    pub const HIDDEN: u32 = 2048;
    pub const STRIKEOUT: u32 = 4096;
    pub const OVERLINE: u32 = 8192;
    /// Every bit this version understands. Higher bits are reserved for later
    /// versions and are masked off on decode.
    pub const KNOWN: u32 = 16383;
}

/// A cell colour. Four bytes on the wire: a kind byte and three payload bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Colour {
    /// The receiver's default foreground/background for this role.
    #[default]
    Default,
    /// An index into the 256-colour palette. Stays an index, deliberately.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Colour {
    pub fn write(&self, w: &mut Writer) {
        match *self {
            Colour::Default => {
                w.u8(0);
                w.u8(0);
                w.u8(0);
                w.u8(0);
            }
            Colour::Indexed(i) => {
                w.u8(1);
                w.u8(i);
                w.u8(0);
                w.u8(0);
            }
            Colour::Rgb(r, g, b) => {
                w.u8(2);
                w.u8(r);
                w.u8(g);
                w.u8(b);
            }
        }
    }

    pub fn read(r: &mut Reader<'_>, tag: u64) -> Result<Colour> {
        let kind = r.u8()?;
        let a = r.u8()?;
        let b = r.u8()?;
        let c = r.u8()?;
        match kind {
            0 => Ok(Colour::Default),
            1 => Ok(Colour::Indexed(a)),
            2 => Ok(Colour::Rgb(a, b, c)),
            _ => Err(WireError::BadValue { tag, why: "colour kind must be 0, 1 or 2" }),
        }
    }
}

/// Everything about how a run of cells is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Colour,
    pub bg: Colour,
    pub underline: Colour,
    pub attrs: u32,
    /// Index into the pane's `uri_table` (OSC 8), or 0 for "no hyperlink".
    pub link_id: u32,
}

impl Style {
    pub fn write(&self, w: &mut Writer) {
        self.fg.write(w);
        self.bg.write(w);
        self.underline.write(w);
        w.varint(self.attrs as u64);
        w.varint(self.link_id as u64);
    }

    pub fn read(r: &mut Reader<'_>, tag: u64) -> Result<Style> {
        let fg = Colour::read(r, tag)?;
        let bg = Colour::read(r, tag)?;
        let underline = Colour::read(r, tag)?;
        // Mask off bits from a future version: carrying them would mean
        // re-emitting a bit we cannot describe or render.
        let attrs = (r.varint()? as u32) & attrs::KNOWN;
        let link_id = r.varint()? as u32;
        Ok(Style { fg, bg, underline, attrs, link_id })
    }
}

/// Encode a pane's style table: a varint count followed by that many styles.
pub fn write_table(styles: &[Style]) -> Vec<u8> {
    let mut w = Writer::new();
    w.varint(styles.len() as u64);
    for s in styles {
        s.write(&mut w);
    }
    w.into_vec()
}

/// Decode a style table, rejecting trailing bytes.
pub fn read_table(body: &[u8]) -> Result<Vec<Style>> {
    let mut r = Reader::new(body);
    let n = r.varint()? as usize;
    let mut out = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        out.push(Style::read(&mut r, 0x3F)?);
    }
    r.finish()?;
    Ok(out)
}
```

- [ ] **Step 4: Wire the module into the crate root**

In `crates/rt-handoff/src/lib.rs`, add:

```rust
pub mod style;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rt-handoff`
Expected: PASS, 31 tests.

- [ ] **Step 6: Commit**

```bash
git add crates/rt-handoff/src/style.rs crates/rt-handoff/src/lib.rs
git commit -m "feat(handoff): colours, attribute bits and the style table"
```

---

### Task 5: The grid — runs, lines, and the grammar validator

The densest part of the format and the one that runs most often. A pane's
screen and every scrollback chunk are made of these.

**Files:**
- Create: `crates/rt-handoff/src/grid.rs`
- Modify: `crates/rt-handoff/src/lib.rs` (add `pub mod grid;`)

**Interfaces:**
- Consumes: `buf::{Writer, Reader}`, `error::{Result, WireError}`.
- Produces: `grid::{Run, Line, Grid}`; `grid::line_flags::{WRAPPED, DECDWL, DECDHL_TOP, DECDHL_BOTTOM}`; `grid::run_flags::{CHAR_COUNTS, WIDE}`; `Run::blank(style_id, cell_span)`, `Run::text(style_id, &str)`, `Run::wide(style_id, &str)`, `Run::cells(&self) -> u32`; `Line::write/read`, `Grid::write(&self) -> Vec<u8>`, `Grid::read(&[u8], u64) -> Result<Grid>`.

- [ ] **Step 1: Write the failing tests**

`crates/rt-handoff/src/grid.rs`, tests first:

```rust
//! Cell data: runs of identically-styled cells, grouped into lines.
//!
//! `cell_span` is measured in COLUMNS. A wide-character run covers two columns
//! per character, so its cell count is `cell_span / 2`. A run never mixes wide
//! and narrow cells — it breaks instead. Every invariant here is checked on
//! decode, because a malformed grid from a peer must be an error, never a
//! panic in the renderer three frames later.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WireError;

    fn round_trip(g: &Grid) -> Grid {
        Grid::read(&g.write(), 0x40).unwrap()
    }

    #[test]
    fn empty_grid_round_trips() {
        let g = Grid { lines: vec![] };
        assert_eq!(round_trip(&g), g);
    }

    #[test]
    fn a_blank_line_round_trips() {
        let g = Grid { lines: vec![Line { flags: 0, runs: vec![Run::blank(0, 80)] }] };
        assert_eq!(round_trip(&g), g);
    }

    #[test]
    fn plain_ascii_round_trips() {
        let g = Grid {
            lines: vec![Line { flags: 0, runs: vec![Run::text(0, "$ cargo test"), Run::blank(0, 68)] }],
        };
        assert_eq!(round_trip(&g), g);
    }

    #[test]
    fn wrapped_flag_survives() {
        let g = Grid {
            lines: vec![
                Line { flags: line_flags::WRAPPED, runs: vec![Run::text(1, "abc")] },
                Line { flags: 0, runs: vec![Run::text(1, "def")] },
            ],
        };
        let back = round_trip(&g);
        assert_eq!(back, g);
        assert_eq!(back.lines[0].flags & line_flags::WRAPPED, line_flags::WRAPPED);
        assert_eq!(back.lines[1].flags & line_flags::WRAPPED, 0);
    }

    #[test]
    fn wide_run_spans_two_columns_per_character() {
        let r = Run::wide(0, "日本語");
        assert_eq!(r.cell_span, 6, "three wide chars occupy six columns");
        assert_eq!(r.cells(), 3);
        let g = Grid { lines: vec![Line { flags: 0, runs: vec![r] }] };
        assert_eq!(round_trip(&g), g);
    }

    #[test]
    fn combining_marks_use_per_cell_char_counts() {
        // Two cells: "e" plus a combining acute, then a plain "x".
        let r = Run {
            flags: run_flags::CHAR_COUNTS,
            style_id: 0,
            cell_span: 2,
            text: "e\u{0301}x".to_string(),
            char_counts: vec![2, 1],
        };
        let g = Grid { lines: vec![Line { flags: 0, runs: vec![r] }] };
        assert_eq!(round_trip(&g), g);
    }

    #[test]
    fn wide_run_with_odd_span_is_rejected() {
        let bad = Grid {
            lines: vec![Line {
                flags: 0,
                runs: vec![Run { flags: run_flags::WIDE, style_id: 0, cell_span: 3, text: "日".into(), char_counts: vec![] }],
            }],
        };
        let err = Grid::read(&bad.write_unchecked(), 0x40).unwrap_err();
        assert_eq!(err, WireError::BadValue { tag: 0x40, why: "wide run must span an even number of columns" });
    }

    #[test]
    fn char_count_that_disagrees_with_the_text_is_rejected() {
        let bad = Grid {
            lines: vec![Line {
                flags: 0,
                runs: vec![Run { flags: run_flags::CHAR_COUNTS, style_id: 0, cell_span: 2, text: "ab".into(), char_counts: vec![1, 5] }],
            }],
        };
        let err = Grid::read(&bad.write_unchecked(), 0x40).unwrap_err();
        assert_eq!(err, WireError::BadValue { tag: 0x40, why: "char_counts must sum to the character count of text" });
    }

    #[test]
    fn text_length_that_disagrees_with_the_span_is_rejected() {
        let bad = Grid {
            lines: vec![Line {
                flags: 0,
                runs: vec![Run { flags: 0, style_id: 0, cell_span: 9, text: "abc".into(), char_counts: vec![] }],
            }],
        };
        let err = Grid::read(&bad.write_unchecked(), 0x40).unwrap_err();
        assert_eq!(err, WireError::BadValue { tag: 0x40, why: "text character count must equal the run's cell count" });
    }

    #[test]
    fn a_short_line_is_not_padded_by_the_decoder() {
        // Trailing blanks are omitted by the producer. Padding to the pane's
        // width is the adopter's job in phase 2, not the decoder's.
        let g = Grid { lines: vec![Line { flags: 0, runs: vec![Run::text(0, "hi")] }] };
        let back = round_trip(&g);
        assert_eq!(back.lines[0].runs.len(), 1);
        assert_eq!(back.lines[0].runs[0].cell_span, 2);
    }

    #[test]
    fn many_lines_round_trip() {
        let lines = (0..500)
            .map(|i| Line { flags: 0, runs: vec![Run::text(0, &format!("line {i}")), Run::blank(0, 40)] })
            .collect();
        let g = Grid { lines };
        assert_eq!(round_trip(&g), g);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt-handoff grid`
Expected: FAIL — `cannot find type Grid in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/rt-handoff/src/grid.rs`, below the module doc comment:

```rust
use crate::buf::{Reader, Writer};
use crate::error::{Result, WireError};

/// Per-line flags. Frozen at v1.
pub mod line_flags {
    /// This line soft-wraps into the next one.
    pub const WRAPPED: u32 = 1;
    /// DECDWL: double-width line.
    pub const DECDWL: u32 = 2;
    /// DECDHL: top half of a double-height line.
    pub const DECDHL_TOP: u32 = 4;
    /// DECDHL: bottom half of a double-height line.
    pub const DECDHL_BOTTOM: u32 = 8;
}

/// Per-run flags. Frozen at v1.
pub mod run_flags {
    /// A per-cell character-count array follows the text (combining marks).
    pub const CHAR_COUNTS: u8 = 1;
    /// Every cell in this run occupies two columns.
    pub const WIDE: u8 = 2;
}

/// A stretch of cells sharing one style.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Run {
    pub flags: u8,
    pub style_id: u32,
    /// Width in COLUMNS, not cells. See `cells()`.
    pub cell_span: u32,
    /// The characters of the run's cells, concatenated. Empty means blanks.
    pub text: String,
    /// One entry per cell when `CHAR_COUNTS` is set; empty otherwise.
    pub char_counts: Vec<u32>,
}

impl Run {
    /// `cell_span` blank cells in `style_id` — the common case, and the reason
    /// a mostly-empty 200-column line costs a handful of bytes.
    pub fn blank(style_id: u32, cell_span: u32) -> Run {
        Run { flags: 0, style_id, cell_span, text: String::new(), char_counts: Vec::new() }
    }

    /// A run of single-width, single-character cells.
    pub fn text(style_id: u32, s: &str) -> Run {
        Run {
            flags: 0,
            style_id,
            cell_span: s.chars().count() as u32,
            text: s.to_string(),
            char_counts: Vec::new(),
        }
    }

    /// A run of double-width cells: two columns per character.
    pub fn wide(style_id: u32, s: &str) -> Run {
        Run {
            flags: run_flags::WIDE,
            style_id,
            cell_span: (s.chars().count() as u32) * 2,
            text: s.to_string(),
            char_counts: Vec::new(),
        }
    }

    /// The number of CELLS this run covers, as opposed to columns.
    pub fn cells(&self) -> u32 {
        if self.flags & run_flags::WIDE != 0 {
            self.cell_span / 2
        } else {
            self.cell_span
        }
    }

    fn write(&self, w: &mut Writer) {
        w.u8(self.flags);
        w.varint(self.style_id as u64);
        w.varint(self.cell_span as u64);
        w.str(&self.text);
        if self.flags & run_flags::CHAR_COUNTS != 0 {
            for c in &self.char_counts {
                w.varint(*c as u64);
            }
        }
    }

    fn read(r: &mut Reader<'_>, tag: u64) -> Result<Run> {
        let flags = r.u8()?;
        let style_id = r.varint()? as u32;
        let cell_span = r.varint()? as u32;
        let text = r.str()?;

        if flags & run_flags::WIDE != 0 && cell_span % 2 != 0 {
            return Err(WireError::BadValue { tag, why: "wide run must span an even number of columns" });
        }
        let cells = if flags & run_flags::WIDE != 0 { cell_span / 2 } else { cell_span };

        let mut char_counts = Vec::new();
        if flags & run_flags::CHAR_COUNTS != 0 {
            for _ in 0..cells {
                char_counts.push(r.varint()? as u32);
            }
            let want: u64 = char_counts.iter().map(|c| *c as u64).sum();
            if want != text.chars().count() as u64 {
                return Err(WireError::BadValue { tag, why: "char_counts must sum to the character count of text" });
            }
        } else if !text.is_empty() && text.chars().count() as u32 != cells {
            return Err(WireError::BadValue { tag, why: "text character count must equal the run's cell count" });
        }

        Ok(Run { flags, style_id, cell_span, text, char_counts })
    }
}

/// One row: flags plus its runs.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Line {
    pub flags: u32,
    pub runs: Vec<Run>,
}

impl Line {
    pub fn write(&self, w: &mut Writer) {
        w.varint(self.flags as u64);
        w.varint(self.runs.len() as u64);
        for run in &self.runs {
            run.write(w);
        }
    }

    pub fn read(r: &mut Reader<'_>, tag: u64) -> Result<Line> {
        let flags = r.varint()? as u32;
        let n = r.varint()? as usize;
        let mut runs = Vec::with_capacity(n.min(4096));
        for _ in 0..n {
            runs.push(Run::read(r, tag)?);
        }
        Ok(Line { flags, runs })
    }
}

/// A screen or a scrollback region: a count and that many lines.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Grid {
    pub lines: Vec<Line>,
}

impl Grid {
    pub fn write(&self) -> Vec<u8> {
        debug_assert!(
            self.lines.iter().flat_map(|l| &l.runs).all(|r| {
                let cells = r.cells();
                if r.flags & run_flags::CHAR_COUNTS != 0 {
                    r.char_counts.len() as u32 == cells
                } else {
                    r.text.is_empty() || r.text.chars().count() as u32 == cells
                }
            }),
            "a run's text and cell count disagree; build runs with Run::blank/text/wide"
        );
        self.write_unchecked()
    }

    /// `write` without the debug assertion, so tests can build a deliberately
    /// malformed grid and prove the DECODER rejects it.
    pub fn write_unchecked(&self) -> Vec<u8> {
        let mut w = Writer::new();
        w.varint(self.lines.len() as u64);
        for line in &self.lines {
            line.write(&mut w);
        }
        w.into_vec()
    }

    pub fn read(body: &[u8], tag: u64) -> Result<Grid> {
        let mut r = Reader::new(body);
        let n = r.varint()? as usize;
        let mut lines = Vec::with_capacity(n.min(65536));
        for _ in 0..n {
            lines.push(Line::read(&mut r, tag)?);
        }
        r.finish()?;
        Ok(Grid { lines })
    }
}
```

- [ ] **Step 4: Wire the module into the crate root**

In `crates/rt-handoff/src/lib.rs`, add:

```rust
pub mod grid;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rt-handoff`
Expected: PASS, 42 tests.

- [ ] **Step 6: Commit**

```bash
git add crates/rt-handoff/src/grid.rs crates/rt-handoff/src/lib.rs
git commit -m "feat(handoff): run/line/grid encoding with a validating decoder"
```

---

### Task 6: `PaneWire` — identity, metadata, screens, and `tag_names`

Builds the pane message with its scalar metadata (0x01–0x0F), style table
(0x3F) and screens (0x40/0x41). The terminal-state fields (0x20–0x2A, 0x50,
0x51) are added in Task 7, which also extends the required-field set. Splitting
here keeps each decoder small enough to read in one sitting.

**Files:**
- Create: `crates/rt-handoff/src/pane.rs`
- Modify: `crates/rt-handoff/src/lib.rs` (add `pub mod pane;`)

**Interfaces:**
- Consumes: `buf`, `tlv`, `style::{Style, write_table, read_table}`, `grid::Grid`, `error`.
- Produces: `pane::PaneWire` (fields below), `pane::tags::*` constants, `pane::name_of_tag(u64) -> Option<&'static str>`, `PaneWire::encode(&self) -> Vec<u8>`, `PaneWire::decode(&[u8]) -> Result<PaneWire>`.

Required fields in THIS task: `0x01 pane_uid`, `0x02 title`, `0x04 cols`,
`0x05 rows`, `0x0A child_pid`, `0x0F tag_names`, `0x3F style_table`,
`0x40 screen_primary`. Absence of any of them is `MissingField`.

`tag_names` is built from the tags actually emitted, which is why encoding
collects `(tag, bytes)` pairs first and writes them afterwards: the value of
0x0F depends on the whole set, but R7 says it must sit between 0x0E and 0x20.

- [ ] **Step 1: Write the failing tests**

`crates/rt-handoff/src/pane.rs`, tests first:

```rust
//! The pane message: everything about one pane except its file descriptors.
//!
//! Field numbers come from the spec's PaneState table and are frozen. Adding a
//! field means adding a tag, never changing one.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WireError;
    use crate::grid::{Grid, Line, Run};
    use crate::style::{Colour, Style};

    /// The smallest pane that satisfies every required field.
    fn minimal() -> PaneWire {
        PaneWire {
            pane_uid: 42,
            title: "zsh".to_string(),
            cols: 80,
            rows: 24,
            child_pid: 1234,
            style_table: vec![Style::default()],
            screen_primary: Grid { lines: vec![Line { flags: 0, runs: vec![Run::blank(0, 80)] }] },
            ..PaneWire::default()
        }
    }

    /// Encode, decode, compare — normalising `tag_names`, which the ENCODER
    /// produces and the DECODER reads back, so a freshly-built pane never has
    /// it. Everything else must survive the trip untouched.
    fn assert_round_trips(p: &PaneWire) {
        let back = PaneWire::decode(&p.encode()).unwrap();
        let mut expected = p.clone();
        expected.tag_names = back.tag_names.clone();
        assert_eq!(back, expected);
    }

    #[test]
    fn minimal_pane_round_trips() {
        assert_round_trips(&minimal());
    }

    #[test]
    fn every_optional_field_round_trips() {
        let p = PaneWire {
            cwd: Some("/home/roland/git/rt".to_string()),
            scrollback_limit: Some(50_000),
            columns_count: Some(3),
            group: Some(2),
            broadcast: Some(true),
            shell_argv: vec!["/bin/zsh".to_string(), "-l".to_string()],
            env_extras: vec![("RT_OUT".to_string(), "/run/user/1000/rt/jacks/a/out".to_string())],
            show_titlebar: Some(false),
            palette: Some((0..256).map(|i| (i as u8, 0, 0)).collect()),
            screen_alt: Some(Grid { lines: vec![Line { flags: 0, runs: vec![Run::text(0, "alt")] }] }),
            ..minimal()
        };
        assert_round_trips(&p);
    }

    #[test]
    fn absent_optionals_stay_absent() {
        let back = PaneWire::decode(&minimal().encode()).unwrap();
        assert_eq!(back.cwd, None);
        assert_eq!(back.columns_count, None);
        assert_eq!(back.screen_alt, None);
        assert!(back.shell_argv.is_empty());
    }

    #[test]
    fn tag_names_covers_every_emitted_tag() {
        let bytes = minimal().encode();
        let emitted = crate::tlv::tags_of(&bytes).unwrap();
        let back = PaneWire::decode(&bytes).unwrap();
        for tag in emitted {
            assert!(back.tag_names.iter().any(|(t, _)| *t == tag), "tag 0x{tag:02x} has no name");
        }
        // 0x0F names itself, so a receiver can report it too.
        assert!(back.tag_names.iter().any(|(t, n)| *t == tags::TAG_NAMES && n == "tag_names"));
    }

    #[test]
    fn fields_are_emitted_in_ascending_order() {
        let tags = crate::tlv::tags_of(&minimal().encode()).unwrap();
        let mut sorted = tags.clone();
        sorted.sort_unstable();
        assert_eq!(tags, sorted, "R7 violated");
    }

    #[test]
    fn unknown_tags_are_skipped_and_recorded() {
        // Splice a field from an imagined future version between 0x40 and the end.
        let mut fw = crate::tlv::FieldWriter::new();
        for (tag, val) in minimal().fields() {
            fw.field(tag, &val);
        }
        fw.field(0x7000, b"something from rt 0.9");
        let body = fw.into_vec();

        let back = PaneWire::decode(&body).unwrap();
        assert_eq!(back.unknown_tags, vec![0x7000]);
        assert_eq!(back.title, "zsh", "known fields must be unaffected");
    }

    #[test]
    fn each_required_field_is_enforced() {
        for missing in [tags::PANE_UID, tags::TITLE, tags::COLS, tags::ROWS, tags::CHILD_PID, tags::TAG_NAMES, tags::STYLE_TABLE, tags::SCREEN_PRIMARY] {
            let mut fw = crate::tlv::FieldWriter::new();
            for (tag, val) in minimal().fields() {
                if tag != missing {
                    fw.field(tag, &val);
                }
            }
            let err = PaneWire::decode(&fw.into_vec()).unwrap_err();
            assert_eq!(err, WireError::MissingField { tag: missing }, "tag 0x{missing:02x}");
        }
    }

    #[test]
    fn a_palette_of_the_wrong_length_is_rejected() {
        let mut fields = minimal().fields();
        fields.push((tags::PALETTE, vec![1, 2, 3])); // three bytes, not 256 * 3
        fields.sort_by_key(|(t, _)| *t);
        let mut fw = crate::tlv::FieldWriter::new();
        for (tag, val) in &fields {
            fw.field(*tag, val);
        }
        let err = PaneWire::decode(&fw.into_vec()).unwrap_err();
        assert_eq!(err, WireError::BadValue { tag: tags::PALETTE, why: "palette must hold exactly 256 entries" });
    }

    #[test]
    fn a_large_pane_round_trips_and_stays_compact() {
        let lines = (0..5_000)
            .map(|i| Line { flags: 0, runs: vec![Run::text(0, &format!("[{i:05}] make[2]: Entering directory")), Run::blank(0, 140)] })
            .collect();
        let p = PaneWire { screen_primary: Grid { lines }, ..minimal() };
        let bytes = p.encode();
        assert_round_trips(&p);
        // Run-length encoding is why the spec specifies no compression: a
        // typical line must cost tens of bytes, not hundreds.
        assert!(bytes.len() < 5_000 * 60, "5k lines took {} bytes", bytes.len());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt-handoff pane`
Expected: FAIL — `cannot find type PaneWire in this scope`.

- [ ] **Step 3: Write the tag table and the model**

Prepend to `crates/rt-handoff/src/pane.rs`, below the module doc comment:

```rust
use crate::error::{Result, WireError};
use crate::grid::Grid;
use crate::style::{self, Style};
use crate::tlv::{self, FieldWriter};

/// Field tags. Frozen: see rule R1. Task 7 adds 0x20–0x2A, 0x50 and 0x51.
pub mod tags {
    pub const PANE_UID: u64 = 0x01;
    pub const TITLE: u64 = 0x02;
    pub const CWD: u64 = 0x03;
    pub const COLS: u64 = 0x04;
    pub const ROWS: u64 = 0x05;
    pub const SCROLLBACK_LIMIT: u64 = 0x06;
    pub const COLUMNS_COUNT: u64 = 0x07;
    pub const GROUP: u64 = 0x08;
    pub const BROADCAST: u64 = 0x09;
    pub const CHILD_PID: u64 = 0x0A;
    pub const SHELL_ARGV: u64 = 0x0B;
    pub const ENV_EXTRAS: u64 = 0x0C;
    pub const SHOW_TITLEBAR: u64 = 0x0D;
    pub const PALETTE: u64 = 0x0E;
    pub const TAG_NAMES: u64 = 0x0F;
    pub const STYLE_TABLE: u64 = 0x3F;
    pub const SCREEN_PRIMARY: u64 = 0x40;
    pub const SCREEN_ALT: u64 = 0x41;
}

/// The short name a receiver prints when it skips a field it does not know
/// (rule R6). Donors ship this table so an old receiver can still name a new
/// field; a tag with no name is reported by number.
pub fn name_of_tag(tag: u64) -> Option<&'static str> {
    Some(match tag {
        tags::PANE_UID => "pane_uid",
        tags::TITLE => "title",
        tags::CWD => "cwd",
        tags::COLS => "cols",
        tags::ROWS => "rows",
        tags::SCROLLBACK_LIMIT => "scrollback_limit",
        tags::COLUMNS_COUNT => "columns",
        tags::GROUP => "group",
        tags::BROADCAST => "broadcast",
        tags::CHILD_PID => "child_pid",
        tags::SHELL_ARGV => "shell_argv",
        tags::ENV_EXTRAS => "env_extras",
        tags::SHOW_TITLEBAR => "show_titlebar",
        tags::PALETTE => "palette",
        tags::TAG_NAMES => "tag_names",
        tags::STYLE_TABLE => "style_table",
        tags::SCREEN_PRIMARY => "screen_primary",
        tags::SCREEN_ALT => "screen_alt",
        _ => return None,
    })
}

/// One pane, ready to be handed to another rt process.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PaneWire {
    pub pane_uid: u64,
    pub title: String,
    pub cwd: Option<String>,
    pub cols: u32,
    pub rows: u32,
    pub scrollback_limit: Option<u32>,
    pub columns_count: Option<u32>,
    pub group: Option<u32>,
    pub broadcast: Option<bool>,
    pub child_pid: u32,
    pub shell_argv: Vec<String>,
    pub env_extras: Vec<(String, String)>,
    pub show_titlebar: Option<bool>,
    /// Exactly 256 entries when present.
    pub palette: Option<Vec<(u8, u8, u8)>>,
    pub style_table: Vec<Style>,
    pub screen_primary: Grid,
    pub screen_alt: Option<Grid>,
    /// Names for every tag the donor emitted (0x0F). Filled on decode.
    pub tag_names: Vec<(u64, String)>,
    /// Tags this build skipped. Always empty when encoding. Rule R6.
    pub unknown_tags: Vec<u64>,
}
```

- [ ] **Step 4: Write the encoder**

Append to the implementation part of `crates/rt-handoff/src/pane.rs`:

```rust
impl PaneWire {
    /// Every field this pane emits, as `(tag, bytes)` in ascending tag order,
    /// `tag_names` included. `encode` is just this list written out, so a test
    /// can drop or corrupt one field and still produce an otherwise-valid body.
    pub fn fields(&self) -> Vec<(u64, Vec<u8>)> {
        /// Encode one field's value with a fresh writer.
        fn enc(f: impl FnOnce(&mut crate::buf::Writer)) -> Vec<u8> {
            let mut w = crate::buf::Writer::new();
            f(&mut w);
            w.into_vec()
        }

        let mut out: Vec<(u64, Vec<u8>)> = Vec::new();
        out.push((tags::PANE_UID, enc(|w| w.varint(self.pane_uid))));
        out.push((tags::TITLE, enc(|w| w.str(&self.title))));
        if let Some(cwd) = &self.cwd {
            out.push((tags::CWD, enc(|w| w.str(cwd))));
        }
        out.push((tags::COLS, enc(|w| w.varint(self.cols as u64))));
        out.push((tags::ROWS, enc(|w| w.varint(self.rows as u64))));
        if let Some(v) = self.scrollback_limit {
            out.push((tags::SCROLLBACK_LIMIT, enc(|w| w.varint(v as u64))));
        }
        if let Some(v) = self.columns_count {
            out.push((tags::COLUMNS_COUNT, enc(|w| w.varint(v as u64))));
        }
        if let Some(v) = self.group {
            out.push((tags::GROUP, enc(|w| w.varint(v as u64))));
        }
        if let Some(v) = self.broadcast {
            out.push((tags::BROADCAST, enc(|w| w.u8(v as u8))));
        }
        out.push((tags::CHILD_PID, enc(|w| w.varint(self.child_pid as u64))));
        if !self.shell_argv.is_empty() {
            out.push((tags::SHELL_ARGV, enc(|w| {
                w.varint(self.shell_argv.len() as u64);
                for a in &self.shell_argv {
                    w.str(a);
                }
            })));
        }
        if !self.env_extras.is_empty() {
            out.push((tags::ENV_EXTRAS, enc(|w| {
                w.varint(self.env_extras.len() as u64);
                for (k, v) in &self.env_extras {
                    w.str(k);
                    w.str(v);
                }
            })));
        }
        if let Some(v) = self.show_titlebar {
            out.push((tags::SHOW_TITLEBAR, enc(|w| w.u8(v as u8))));
        }
        if let Some(pal) = &self.palette {
            out.push((tags::PALETTE, enc(|w| {
                for (r, g, b) in pal {
                    w.u8(*r);
                    w.u8(*g);
                    w.u8(*b);
                }
            })));
        }
        out.push((tags::STYLE_TABLE, style::write_table(&self.style_table)));
        out.push((tags::SCREEN_PRIMARY, self.screen_primary.write()));
        if let Some(alt) = &self.screen_alt {
            out.push((tags::SCREEN_ALT, alt.write()));
        }

        // tag_names names every tag in the message, itself included, so an old
        // receiver can print a name for a field it does not understand (R6).
        let mut names: Vec<(u64, &'static str)> = out
            .iter()
            .filter_map(|(t, _)| name_of_tag(*t).map(|n| (*t, n)))
            .collect();
        names.push((tags::TAG_NAMES, "tag_names"));
        names.sort_by_key(|(t, _)| *t);
        out.push((tags::TAG_NAMES, enc(|w| {
            w.varint(names.len() as u64);
            for (t, n) in &names {
                w.varint(*t);
                w.str(n);
            }
        })));

        out.sort_by_key(|(tag, _)| *tag);
        out
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut fw = FieldWriter::new();
        for (tag, val) in &self.fields() {
            fw.field(*tag, val);
        }
        fw.into_vec()
    }
}
```

- [ ] **Step 5: Write the decoder**

Append to `crates/rt-handoff/src/pane.rs`:

```rust
impl PaneWire {
    pub fn decode(body: &[u8]) -> Result<PaneWire> {
        let mut p = PaneWire::default();
        let mut seen: Vec<u64> = Vec::new();

        let unknown = tlv::walk(body, |tag, val| {
            let mut r = crate::buf::Reader::new(val);
            match tag {
                tags::PANE_UID => p.pane_uid = r.varint()?,
                tags::TITLE => p.title = r.str()?,
                tags::CWD => p.cwd = Some(r.str()?),
                tags::COLS => p.cols = r.varint()? as u32,
                tags::ROWS => p.rows = r.varint()? as u32,
                tags::SCROLLBACK_LIMIT => p.scrollback_limit = Some(r.varint()? as u32),
                tags::COLUMNS_COUNT => p.columns_count = Some(r.varint()? as u32),
                tags::GROUP => p.group = Some(r.varint()? as u32),
                tags::BROADCAST => p.broadcast = Some(r.u8()? != 0),
                tags::CHILD_PID => p.child_pid = r.varint()? as u32,
                tags::SHELL_ARGV => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        p.shell_argv.push(r.str()?);
                    }
                }
                tags::ENV_EXTRAS => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        let k = r.str()?;
                        let v = r.str()?;
                        p.env_extras.push((k, v));
                    }
                }
                tags::SHOW_TITLEBAR => p.show_titlebar = Some(r.u8()? != 0),
                tags::PALETTE => {
                    if val.len() != 256 * 3 {
                        return Err(WireError::BadValue { tag, why: "palette must hold exactly 256 entries" });
                    }
                    let mut pal = Vec::with_capacity(256);
                    for _ in 0..256 {
                        pal.push((r.u8()?, r.u8()?, r.u8()?));
                    }
                    p.palette = Some(pal);
                }
                tags::TAG_NAMES => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        let t = r.varint()?;
                        let name = r.str()?;
                        p.tag_names.push((t, name));
                    }
                }
                tags::STYLE_TABLE => {
                    p.style_table = style::read_table(val)?;
                    seen.push(tag);
                    return Ok(true); // read_table consumed the whole value
                }
                tags::SCREEN_PRIMARY => {
                    p.screen_primary = Grid::read(val, tag)?;
                    seen.push(tag);
                    return Ok(true);
                }
                tags::SCREEN_ALT => {
                    p.screen_alt = Some(Grid::read(val, tag)?);
                    seen.push(tag);
                    return Ok(true);
                }
                _ => return Ok(false),
            }
            r.finish()?;
            seen.push(tag);
            Ok(true)
        })?;

        for required in [
            tags::PANE_UID,
            tags::TITLE,
            tags::COLS,
            tags::ROWS,
            tags::CHILD_PID,
            tags::TAG_NAMES,
            tags::STYLE_TABLE,
            tags::SCREEN_PRIMARY,
        ] {
            if !seen.contains(&required) {
                return Err(WireError::MissingField { tag: required });
            }
        }

        p.unknown_tags = unknown;
        Ok(p)
    }
}
```

- [ ] **Step 6: Wire the module into the crate root**

In `crates/rt-handoff/src/lib.rs`, add:

```rust
pub mod pane;
```

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test -p rt-handoff`
Expected: PASS, 51 tests.

- [ ] **Step 8: Commit**

```bash
git add crates/rt-handoff/src/pane.rs crates/rt-handoff/src/lib.rs
git commit -m "feat(handoff): PaneWire metadata, screens and the tag-name table"
```

---

### Task 7: Terminal state — modes, cursor, charsets, tabs, margins, links, images

Extends `PaneWire` with tags 0x20–0x2A, 0x50 and 0x51, and adds `MODES`,
`CURSOR` and `PEN` to the required set.

Modes travel by their **DEC/ANSI mode number**, not by an rt enum. That is the
single most important version-independence decision in the format: the
namespace belongs to the VT spec, so mode 2004 means bracketed paste in every
build that ever existed, and a build that does not implement a mode skips it by
rule R2 without needing to know what it was.

**Files:**
- Modify: `crates/rt-handoff/src/pane.rs`

**Interfaces:**
- Consumes: everything from Task 6.
- Produces: `pane::{ModeEntry, CursorState, SavedCursor, Charsets, Margins, KittyKbd, ImageEntry}`; new `PaneWire` fields `modes`, `cursor`, `saved_cursor`, `charsets`, `tab_stops`, `margins`, `title_stack`, `pen`, `active_screen`, `kitty_kbd`, `pending_raw`, `uri_table`, `image_table`; new `tags::*` constants.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/rt-handoff/src/pane.rs`:

```rust
    #[test]
    fn modes_travel_by_their_dec_number() {
        let p = PaneWire {
            modes: vec![
                ModeEntry { kind: 1, number: 1, value: 1 },    // DECCKM on
                ModeEntry { kind: 1, number: 2004, value: 1 }, // bracketed paste on
                ModeEntry { kind: 1, number: 1049, value: 0 }, // alt screen off
                ModeEntry { kind: 0, number: 20, value: 1 },   // LNM on (ANSI, not DEC)
            ],
            ..minimal()
        };
        let back = PaneWire::decode(&p.encode()).unwrap();
        assert_eq!(back.modes, p.modes);
        // The ANSI/DEC namespaces are distinct: mode 20 in each is a different mode.
        assert_eq!(back.modes[3].kind, 0);
    }

    #[test]
    fn a_mode_number_this_build_never_heard_of_survives_the_trip() {
        // The decoder does not filter modes: it is not the transport's job to
        // decide which modes an engine supports.
        let p = PaneWire { modes: vec![ModeEntry { kind: 1, number: 65000, value: 1 }], ..minimal() };
        assert_eq!(PaneWire::decode(&p.encode()).unwrap().modes, p.modes);
    }

    #[test]
    fn cursor_state_round_trips_including_pending_wrap() {
        let p = PaneWire {
            cursor: CursorState { col: 79, row: 3, shape: 2, visible: true, blink: false, pending_wrap: true },
            ..minimal()
        };
        let back = PaneWire::decode(&p.encode()).unwrap();
        assert_eq!(back.cursor, p.cursor);
        assert!(back.cursor.pending_wrap, "deferred wrap is part of the cursor, not a detail");
    }

    #[test]
    fn the_whole_terminal_state_round_trips() {
        let p = PaneWire {
            modes: vec![ModeEntry { kind: 1, number: 7, value: 1 }],
            cursor: CursorState { col: 1, row: 2, shape: 1, visible: true, blink: true, pending_wrap: false },
            saved_cursor: Some(SavedCursor {
                col: 5,
                row: 6,
                pen: Style { fg: Colour::Indexed(3), ..Style::default() },
                charsets: Charsets { g: [b'B', b'0', b'B', b'B'], gl: 0, gr: 2 },
                origin: true,
            }),
            charsets: Some(Charsets { g: [b'B', b'B', b'B', b'B'], gl: 0, gr: 0 }),
            tab_stops: Some((0..80).map(|i| i % 8 == 0).collect()),
            margins: Some(Margins { top: 1, bottom: 22, left: 0, right: 79 }),
            title_stack: vec!["one".into(), "two".into()],
            pen: Style { attrs: crate::style::attrs::BOLD, ..Style::default() },
            active_screen: 1,
            kitty_kbd: Some(KittyKbd { stack: vec![1, 5], modify_other_keys: 2 }),
            pending_raw: vec![0x1b, b'[', b'3'],
            uri_table: vec![(1, "https://example.invalid/a".into())],
            image_table: vec![ImageEntry { id: 9, format: 1, w: 4, h: 2, data: vec![1, 2, 3, 4] }],
            ..minimal()
        };
        assert_eq!(PaneWire::decode(&p.encode()).unwrap(), p);
    }

    #[test]
    fn pending_raw_carries_an_incomplete_sequence_verbatim() {
        // The freeze captures a half-parsed escape; the receiver replays the
        // bytes into its own parser. Raw bytes mean the same thing in every
        // build, which is why the format ships them rather than parser state.
        let p = PaneWire { pending_raw: vec![0x1b, b'[', b'3', b'8', b';', b'5'], ..minimal() };
        assert_eq!(PaneWire::decode(&p.encode()).unwrap().pending_raw, p.pending_raw);
    }

    #[test]
    fn the_new_required_fields_are_enforced() {
        for missing in [tags::MODES, tags::CURSOR, tags::PEN] {
            let mut fw = crate::tlv::FieldWriter::new();
            for (tag, val) in minimal().fields() {
                if tag != missing {
                    fw.field(tag, &val);
                }
            }
            let err = PaneWire::decode(&fw.into_vec()).unwrap_err();
            assert_eq!(err, WireError::MissingField { tag: missing }, "tag 0x{missing:02x}");
        }
    }

    #[test]
    fn active_screen_must_be_zero_or_one() {
        let mut fields = minimal().fields();
        fields.retain(|(t, _)| *t != tags::ACTIVE_SCREEN);
        fields.push((tags::ACTIVE_SCREEN, vec![7]));
        fields.sort_by_key(|(t, _)| *t);
        let mut fw = crate::tlv::FieldWriter::new();
        for (tag, val) in &fields {
            fw.field(*tag, val);
        }
        let err = PaneWire::decode(&fw.into_vec()).unwrap_err();
        assert_eq!(err, WireError::BadValue { tag: tags::ACTIVE_SCREEN, why: "active_screen must be 0 or 1" });
    }
```

Also update `minimal()` in the same test module so it satisfies the new
required fields — replace its body with:

```rust
    fn minimal() -> PaneWire {
        PaneWire {
            pane_uid: 42,
            title: "zsh".to_string(),
            cols: 80,
            rows: 24,
            child_pid: 1234,
            modes: vec![ModeEntry { kind: 1, number: 7, value: 1 }],
            cursor: CursorState { col: 0, row: 0, shape: 0, visible: true, blink: true, pending_wrap: false },
            pen: Style::default(),
            style_table: vec![Style::default()],
            screen_primary: Grid { lines: vec![Line { flags: 0, runs: vec![Run::blank(0, 80)] }] },
            ..PaneWire::default()
        }
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt-handoff pane`
Expected: FAIL — `struct PaneWire has no field named modes`.

- [ ] **Step 3: Add the tag constants and their names**

In `crates/rt-handoff/src/pane.rs`, add to `mod tags`:

```rust
    pub const MODES: u64 = 0x20;
    pub const CURSOR: u64 = 0x21;
    pub const SAVED_CURSOR: u64 = 0x22;
    pub const CHARSETS: u64 = 0x23;
    pub const TAB_STOPS: u64 = 0x24;
    pub const MARGINS: u64 = 0x25;
    pub const TITLE_STACK: u64 = 0x26;
    pub const PEN: u64 = 0x27;
    pub const ACTIVE_SCREEN: u64 = 0x28;
    pub const KITTY_KBD: u64 = 0x29;
    pub const PENDING_RAW: u64 = 0x2A;
    pub const URI_TABLE: u64 = 0x50;
    pub const IMAGE_TABLE: u64 = 0x51;
```

and to `name_of_tag`, before the `_ => return None` arm:

```rust
        tags::MODES => "modes",
        tags::CURSOR => "cursor",
        tags::SAVED_CURSOR => "saved_cursor",
        tags::CHARSETS => "charsets",
        tags::TAB_STOPS => "tab_stops",
        tags::MARGINS => "margins",
        tags::TITLE_STACK => "title_stack",
        tags::PEN => "pen",
        tags::ACTIVE_SCREEN => "active_screen",
        tags::KITTY_KBD => "kitty_kbd",
        tags::PENDING_RAW => "pending_raw",
        tags::URI_TABLE => "uri_table",
        tags::IMAGE_TABLE => "image_table",
```

- [ ] **Step 4: Add the state types**

In `crates/rt-handoff/src/pane.rs`, above `PaneWire`:

```rust
/// One terminal mode, identified the way the VT spec identifies it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModeEntry {
    /// 0 = ANSI mode, 1 = DEC private mode. Two distinct namespaces.
    pub kind: u8,
    /// The mode number itself: 1 DECCKM, 7 DECAWM, 25 DECTCEM, 2004 bracketed
    /// paste, 1049 alt screen, 2026 synchronised update, and so on.
    pub number: u32,
    pub value: u8,
}

/// Where the cursor is and how it looks.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct CursorState {
    pub col: u32,
    pub row: u32,
    /// 0 block, 1 underline, 2 bar.
    pub shape: u8,
    pub visible: bool,
    pub blink: bool,
    /// The deferred-wrap flag: the cursor sits past the last column and the
    /// next printable character wraps first. Dropping it misplaces output.
    pub pending_wrap: bool,
}

/// DECSC state.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct SavedCursor {
    pub col: u32,
    pub row: u32,
    pub pen: Style,
    pub charsets: Charsets,
    pub origin: bool,
}

/// G0..G3 designators plus the GL/GR locking shifts.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Charsets {
    /// The final character of each designation sequence: b'B' ASCII, b'0' DEC
    /// graphics, and so on.
    pub g: [u8; 4],
    pub gl: u8,
    pub gr: u8,
}

impl Default for Charsets {
    fn default() -> Self {
        Charsets { g: [b'B'; 4], gl: 0, gr: 0 }
    }
}

/// Scrolling and horizontal margins.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Margins {
    pub top: u32,
    pub bottom: u32,
    pub left: u32,
    pub right: u32,
}

/// Kitty keyboard protocol flag stack plus the xterm modifyOtherKeys level.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct KittyKbd {
    pub stack: Vec<u32>,
    pub modify_other_keys: u8,
}

/// One inline image. `format` is 1 for PNG; other values are reserved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageEntry {
    pub id: u32,
    pub format: u8,
    pub w: u32,
    pub h: u32,
    pub data: Vec<u8>,
}
```

- [ ] **Step 5: Extend `PaneWire`**

Add these fields to the `PaneWire` struct, between `palette` and `style_table`:

```rust
    pub modes: Vec<ModeEntry>,
    pub cursor: CursorState,
    pub saved_cursor: Option<SavedCursor>,
    pub charsets: Option<Charsets>,
    /// One entry per column.
    pub tab_stops: Option<Vec<bool>>,
    pub margins: Option<Margins>,
    pub title_stack: Vec<String>,
    /// The current SGR state, so output after the move continues in the same
    /// attributes.
    pub pen: Style,
    /// 0 primary, 1 alt.
    pub active_screen: u8,
    pub kitty_kbd: Option<KittyKbd>,
    /// Bytes of an escape sequence the donor's parser had not finished, to be
    /// replayed into the receiver's parser before anything else.
    pub pending_raw: Vec<u8>,
    pub uri_table: Vec<(u32, String)>,
    pub image_table: Vec<ImageEntry>,
```

- [ ] **Step 6: Extend the encoder**

In `PaneWire::fields`, insert after the `PALETTE` block and before the
`STYLE_TABLE` push:

```rust
        out.push((tags::MODES, enc(|w| {
            w.varint(self.modes.len() as u64);
            for m in &self.modes {
                w.u8(m.kind);
                w.varint(m.number as u64);
                w.u8(m.value);
            }
        })));
        out.push((tags::CURSOR, enc(|w| {
            w.varint(self.cursor.col as u64);
            w.varint(self.cursor.row as u64);
            w.u8(self.cursor.shape);
            w.u8(self.cursor.visible as u8);
            w.u8(self.cursor.blink as u8);
            w.u8(self.cursor.pending_wrap as u8);
        })));
        if let Some(sc) = &self.saved_cursor {
            out.push((tags::SAVED_CURSOR, enc(|w| {
                w.varint(sc.col as u64);
                w.varint(sc.row as u64);
                sc.pen.write(w);
                w.raw(&sc.charsets.g);
                w.u8(sc.charsets.gl);
                w.u8(sc.charsets.gr);
                w.u8(sc.origin as u8);
            })));
        }
        if let Some(cs) = &self.charsets {
            out.push((tags::CHARSETS, enc(|w| {
                w.raw(&cs.g);
                w.u8(cs.gl);
                w.u8(cs.gr);
            })));
        }
        if let Some(stops) = &self.tab_stops {
            out.push((tags::TAB_STOPS, enc(|w| {
                w.varint(stops.len() as u64);
                for chunk in stops.chunks(8) {
                    let mut byte = 0u8;
                    for (i, on) in chunk.iter().enumerate() {
                        if *on {
                            byte |= 1 << i;
                        }
                    }
                    w.u8(byte);
                }
            })));
        }
        if let Some(m) = &self.margins {
            out.push((tags::MARGINS, enc(|w| {
                w.varint(m.top as u64);
                w.varint(m.bottom as u64);
                w.varint(m.left as u64);
                w.varint(m.right as u64);
            })));
        }
        if !self.title_stack.is_empty() {
            out.push((tags::TITLE_STACK, enc(|w| {
                w.varint(self.title_stack.len() as u64);
                for t in &self.title_stack {
                    w.str(t);
                }
            })));
        }
        out.push((tags::PEN, enc(|w| self.pen.write(w))));
        out.push((tags::ACTIVE_SCREEN, enc(|w| w.u8(self.active_screen))));
        if let Some(k) = &self.kitty_kbd {
            out.push((tags::KITTY_KBD, enc(|w| {
                w.varint(k.stack.len() as u64);
                for f in &k.stack {
                    w.varint(*f as u64);
                }
                w.u8(k.modify_other_keys);
            })));
        }
        if !self.pending_raw.is_empty() {
            out.push((tags::PENDING_RAW, enc(|w| w.bytes(&self.pending_raw))));
        }
        if !self.uri_table.is_empty() {
            out.push((tags::URI_TABLE, enc(|w| {
                w.varint(self.uri_table.len() as u64);
                for (id, uri) in &self.uri_table {
                    w.varint(*id as u64);
                    w.str(uri);
                }
            })));
        }
        if !self.image_table.is_empty() {
            out.push((tags::IMAGE_TABLE, enc(|w| {
                w.varint(self.image_table.len() as u64);
                for img in &self.image_table {
                    w.varint(img.id as u64);
                    w.u8(img.format);
                    w.varint(img.w as u64);
                    w.varint(img.h as u64);
                    w.bytes(&img.data);
                }
            })));
        }
```

- [ ] **Step 7: Extend the decoder**

In `PaneWire::decode`'s `match tag`, add these arms before `_ => return Ok(false)`:

```rust
                tags::MODES => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        let kind = r.u8()?;
                        let number = r.varint()? as u32;
                        let value = r.u8()?;
                        p.modes.push(ModeEntry { kind, number, value });
                    }
                }
                tags::CURSOR => {
                    p.cursor = CursorState {
                        col: r.varint()? as u32,
                        row: r.varint()? as u32,
                        shape: r.u8()?,
                        visible: r.u8()? != 0,
                        blink: r.u8()? != 0,
                        pending_wrap: r.u8()? != 0,
                    };
                }
                tags::SAVED_CURSOR => {
                    let col = r.varint()? as u32;
                    let row = r.varint()? as u32;
                    let pen = Style::read(&mut r, tag)?;
                    let gb = r.take(4)?;
                    let charsets = Charsets { g: [gb[0], gb[1], gb[2], gb[3]], gl: r.u8()?, gr: r.u8()? };
                    p.saved_cursor = Some(SavedCursor { col, row, pen, charsets, origin: r.u8()? != 0 });
                }
                tags::CHARSETS => {
                    let gb = r.take(4)?;
                    p.charsets = Some(Charsets { g: [gb[0], gb[1], gb[2], gb[3]], gl: r.u8()?, gr: r.u8()? });
                }
                tags::TAB_STOPS => {
                    let n = r.varint()? as usize;
                    let bytes = r.take(n.div_ceil(8))?; // one byte per eight columns, LSB first
                    let mut stops = Vec::with_capacity(n.min(65536));
                    for i in 0..n {
                        stops.push(bytes[i / 8] & (1 << (i % 8)) != 0);
                    }
                    p.tab_stops = Some(stops);
                }
                tags::MARGINS => {
                    p.margins = Some(Margins {
                        top: r.varint()? as u32,
                        bottom: r.varint()? as u32,
                        left: r.varint()? as u32,
                        right: r.varint()? as u32,
                    });
                }
                tags::TITLE_STACK => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        p.title_stack.push(r.str()?);
                    }
                }
                tags::PEN => p.pen = Style::read(&mut r, tag)?,
                tags::ACTIVE_SCREEN => {
                    let v = r.u8()?;
                    if v > 1 {
                        return Err(WireError::BadValue { tag, why: "active_screen must be 0 or 1" });
                    }
                    p.active_screen = v;
                }
                tags::KITTY_KBD => {
                    let n = r.varint()? as usize;
                    let mut stack = Vec::with_capacity(n.min(256));
                    for _ in 0..n {
                        stack.push(r.varint()? as u32);
                    }
                    p.kitty_kbd = Some(KittyKbd { stack, modify_other_keys: r.u8()? });
                }
                tags::PENDING_RAW => p.pending_raw = r.bytes()?.to_vec(),
                tags::URI_TABLE => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        let id = r.varint()? as u32;
                        let uri = r.str()?;
                        p.uri_table.push((id, uri));
                    }
                }
                tags::IMAGE_TABLE => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        let id = r.varint()? as u32;
                        let format = r.u8()?;
                        let w = r.varint()? as u32;
                        let h = r.varint()? as u32;
                        let data = r.bytes()?.to_vec();
                        p.image_table.push(ImageEntry { id, format, w, h, data });
                    }
                }
```

- [ ] **Step 8: Extend the required-field list**

In `PaneWire::decode`, add `tags::MODES`, `tags::CURSOR` and `tags::PEN` to the
required array.

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cargo test -p rt-handoff`
Expected: PASS, 58 tests.

- [ ] **Step 10: Commit**

```bash
git add crates/rt-handoff/src/pane.rs
git commit -m "feat(handoff): terminal state — modes by DEC number, cursor, charsets, links, images"
```

---

### Task 8: The layout tree

A single pane is a one-leaf tree; a tab is a subtree; a bulk migration is the
donor's whole forest. One encoding covers all three, so the receiver's adopt
path never needs to care which gesture produced the transfer.

**Files:**
- Create: `crates/rt-handoff/src/tree.rs`
- Modify: `crates/rt-handoff/src/lib.rs` (add `pub mod tree;`)

**Interfaces:**
- Consumes: `buf::{Writer, Reader}`, `error::{Result, WireError}`.
- Produces: `tree::Node` with variants `Leaf { pane_uid }`, `HSplit { ratio, children }`, `VSplit { ratio, children }`, `Tabs { active, children }` where `children: Vec<(String, Node)>` for `Tabs` and `Vec<Node>` otherwise; `Node::encode(&self) -> Vec<u8>`, `Node::decode(&[u8]) -> Result<Node>`, `Node::pane_uids(&self) -> Vec<u64>`.

`ratio` is written as raw `f32` bits so it round-trips exactly, and validated
on decode to be finite and within `0.0..=1.0` — which also keeps `PartialEq`
meaningful, since a NaN ratio would make a decoded tree unequal to itself.

- [ ] **Step 1: Write the failing tests**

`crates/rt-handoff/src/tree.rs`, tests first:

```rust
//! The layout tree that travels with a transfer.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WireError;

    fn round_trip(n: &Node) -> Node {
        Node::decode(&n.encode()).unwrap()
    }

    #[test]
    fn a_single_pane_is_a_leaf() {
        let n = Node::Leaf { pane_uid: 7 };
        assert_eq!(round_trip(&n), n);
        assert_eq!(n.pane_uids(), vec![7]);
    }

    #[test]
    fn splits_round_trip_with_exact_ratios() {
        let n = Node::HSplit {
            ratio: 0.382,
            children: vec![Node::Leaf { pane_uid: 1 }, Node::VSplit { ratio: 0.5, children: vec![Node::Leaf { pane_uid: 2 }, Node::Leaf { pane_uid: 3 }] }],
        };
        let back = round_trip(&n);
        assert_eq!(back, n);
        match back {
            Node::HSplit { ratio, .. } => assert_eq!(ratio.to_bits(), 0.382f32.to_bits(), "ratio must be bit-exact"),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn tabs_carry_titles_and_the_active_index() {
        let n = Node::Tabs {
            active: 1,
            children: vec![
                ("build".to_string(), Node::Leaf { pane_uid: 10 }),
                ("claude".to_string(), Node::HSplit { ratio: 0.5, children: vec![Node::Leaf { pane_uid: 11 }, Node::Leaf { pane_uid: 12 }] }),
            ],
        };
        assert_eq!(round_trip(&n), n);
    }

    #[test]
    fn pane_uids_walks_the_whole_forest_in_order() {
        let n = Node::Tabs {
            active: 0,
            children: vec![
                ("a".into(), Node::Leaf { pane_uid: 1 }),
                ("b".into(), Node::VSplit { ratio: 0.5, children: vec![Node::Leaf { pane_uid: 2 }, Node::Leaf { pane_uid: 3 }] }),
            ],
        };
        assert_eq!(n.pane_uids(), vec![1, 2, 3]);
    }

    #[test]
    fn an_unknown_node_kind_is_rejected() {
        let mut w = crate::buf::Writer::new();
        w.varint(9); // there is no kind 9
        assert_eq!(
            Node::decode(&w.into_vec()).unwrap_err(),
            WireError::BadValue { tag: 0x04, why: "node kind must be 0, 1, 2 or 3" }
        );
    }

    #[test]
    fn a_nan_ratio_is_rejected() {
        let mut w = crate::buf::Writer::new();
        w.varint(1); // HSplit
        w.u32le(f32::NAN.to_bits());
        w.varint(0); // no children
        assert_eq!(
            Node::decode(&w.into_vec()).unwrap_err(),
            WireError::BadValue { tag: 0x04, why: "split ratio must be finite and within 0.0..=1.0" }
        );
    }

    #[test]
    fn an_out_of_range_ratio_is_rejected() {
        let mut w = crate::buf::Writer::new();
        w.varint(2); // VSplit
        w.u32le(1.5f32.to_bits());
        w.varint(0);
        assert_eq!(
            Node::decode(&w.into_vec()).unwrap_err(),
            WireError::BadValue { tag: 0x04, why: "split ratio must be finite and within 0.0..=1.0" }
        );
    }

    #[test]
    fn a_deep_tree_round_trips() {
        let mut n = Node::Leaf { pane_uid: 0 };
        for i in 1..64 {
            n = Node::HSplit { ratio: 0.5, children: vec![n, Node::Leaf { pane_uid: i }] };
        }
        assert_eq!(round_trip(&n), n);
    }

    #[test]
    fn trailing_bytes_after_a_tree_are_rejected() {
        let mut bytes = Node::Leaf { pane_uid: 1 }.encode();
        bytes.push(0);
        assert_eq!(Node::decode(&bytes).unwrap_err(), WireError::TrailingBytes { left: 1 });
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt-handoff tree`
Expected: FAIL — `cannot find type Node in this scope`.

- [ ] **Step 3: Write the implementation**

Prepend to `crates/rt-handoff/src/tree.rs`, below the module doc comment:

```rust
use crate::buf::{Reader, Writer};
use crate::error::{Result, WireError};

/// The tag a tree-shaped `BadValue` is reported against: the Tree message.
const TREE_TAG: u64 = 0x04;

/// One node of the transferred layout.
#[derive(Debug, Clone, PartialEq)]
pub enum Node {
    Leaf { pane_uid: u64 },
    HSplit { ratio: f32, children: Vec<Node> },
    VSplit { ratio: f32, children: Vec<Node> },
    Tabs { active: u32, children: Vec<(String, Node)> },
}

impl Node {
    pub fn encode(&self) -> Vec<u8> {
        let mut w = Writer::new();
        self.write(&mut w);
        w.into_vec()
    }

    fn write(&self, w: &mut Writer) {
        match self {
            Node::Leaf { pane_uid } => {
                w.varint(0);
                w.varint(*pane_uid);
            }
            Node::HSplit { ratio, children } | Node::VSplit { ratio, children } => {
                w.varint(if matches!(self, Node::HSplit { .. }) { 1 } else { 2 });
                w.u32le(ratio.to_bits());
                w.varint(children.len() as u64);
                for c in children {
                    c.write(w);
                }
            }
            Node::Tabs { active, children } => {
                w.varint(3);
                w.varint(*active as u64);
                w.varint(children.len() as u64);
                for (title, c) in children {
                    w.str(title);
                    c.write(w);
                }
            }
        }
    }

    pub fn decode(body: &[u8]) -> Result<Node> {
        let mut r = Reader::new(body);
        let n = Node::read(&mut r)?;
        r.finish()?;
        Ok(n)
    }

    fn read(r: &mut Reader<'_>) -> Result<Node> {
        let kind = r.varint()?;
        match kind {
            0 => Ok(Node::Leaf { pane_uid: r.varint()? }),
            1 | 2 => {
                let ratio = f32::from_bits(r.u32le()?);
                if !ratio.is_finite() || !(0.0..=1.0).contains(&ratio) {
                    return Err(WireError::BadValue { tag: TREE_TAG, why: "split ratio must be finite and within 0.0..=1.0" });
                }
                let n = r.varint()? as usize;
                let mut children = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    children.push(Node::read(r)?);
                }
                Ok(if kind == 1 { Node::HSplit { ratio, children } } else { Node::VSplit { ratio, children } })
            }
            3 => {
                let active = r.varint()? as u32;
                let n = r.varint()? as usize;
                let mut children = Vec::with_capacity(n.min(1024));
                for _ in 0..n {
                    let title = r.str()?;
                    children.push((title, Node::read(r)?));
                }
                Ok(Node::Tabs { active, children })
            }
            _ => Err(WireError::BadValue { tag: TREE_TAG, why: "node kind must be 0, 1, 2 or 3" }),
        }
    }

    /// Every pane referenced by this tree, depth-first, left to right. The
    /// receiver uses it to check that every leaf got a matching `PaneState`.
    pub fn pane_uids(&self) -> Vec<u64> {
        let mut out = Vec::new();
        self.collect_uids(&mut out);
        out
    }

    fn collect_uids(&self, out: &mut Vec<u64>) {
        match self {
            Node::Leaf { pane_uid } => out.push(*pane_uid),
            Node::HSplit { children, .. } | Node::VSplit { children, .. } => {
                for c in children {
                    c.collect_uids(out);
                }
            }
            Node::Tabs { children, .. } => {
                for (_, c) in children {
                    c.collect_uids(out);
                }
            }
        }
    }
}
```

- [ ] **Step 4: Wire the module into the crate root**

In `crates/rt-handoff/src/lib.rs`, add:

```rust
pub mod tree;
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rt-handoff`
Expected: PASS, 67 tests.

Note the recursion in `Node::read`: a hostile peer could nest thousands of
splits and overflow the stack. Phase 3 adds a depth cap when the socket layer
lands and the input becomes genuinely untrusted; within phase 1 the input is
always something this crate encoded. Do not add the cap here — it would be
untested code guarding a threat that does not exist yet.

- [ ] **Step 6: Commit**

```bash
git add crates/rt-handoff/src/tree.rs crates/rt-handoff/src/lib.rs
git commit -m "feat(handoff): layout tree encoding for panes, tabs and whole forests"
```

---

### Task 9: The message set and frame dispatch

The remaining bodies: the handshake, the offer/claim exchange, the scrollback
stream, and the small control messages. `PaneFds` carries only a `pane_uid`
here — the file descriptors ride out-of-band in `SCM_RIGHTS`, which is phase 2.

**Files:**
- Create: `crates/rt-handoff/src/msg.rs`
- Modify: `crates/rt-handoff/src/lib.rs` (add `pub mod msg;`)

**Interfaces:**
- Consumes: `frame::Frame`, `buf`, `tlv`, `grid::Line`, `tree::Node`, `pane::PaneWire`, `error`.
- Produces: `msg::msg_type::*` constants; `msg::fail::*` codes; `msg::{Hello, Offer, Claim, ScrollChunk, PaneFds, Adopted, Failed, DropTargetWire}`; `msg::Message` enum with `Message::to_frame(&self) -> Result<Frame>` and `Message::from_frame(&Frame) -> Result<Message>`.

- [ ] **Step 1: Write the failing tests**

`crates/rt-handoff/src/msg.rs`, tests first:

```rust
//! The message set. Bodies are TLV like everything else, so a v1 receiver can
//! read a v2 Hello and still learn the version it needs to refuse.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WireError;
    use crate::grid::{Line, Run};

    fn round_trip(m: &Message) -> Message {
        Message::from_frame(&m.to_frame().unwrap()).unwrap()
    }

    #[test]
    fn hello_round_trips() {
        let m = Message::Hello(Hello {
            proto_min: 1,
            proto_max: 1,
            rt_version: "0.3.20".into(),
            engine: "vtterm".into(),
            boot_id: "6f1c...".into(),
            caps: vec![1, 2],
            max_scrollback_lines: 50_000,
            max_payload_bytes: 64 * 1024 * 1024,
            display: "wayland".into(),
            unknown_tags: vec![],
        });
        assert_eq!(round_trip(&m), m);
    }

    #[test]
    fn hello_carries_the_magic_so_a_wrong_peer_fails_clearly() {
        let m = Message::Hello(Hello { proto_min: 1, proto_max: 1, ..Hello::default() });
        let frame = m.to_frame().unwrap();
        assert!(frame.payload.windows(9).any(|w| w == crate::MAGIC), "the magic must appear in the body");
    }

    #[test]
    fn a_hello_without_the_magic_is_rejected() {
        let mut fw = crate::tlv::FieldWriter::new();
        fw.field(hello_tags::MAGIC, b"NOTRTHAND");
        fw.field(hello_tags::PROTO_MIN, &[1]);
        let frame = crate::frame::Frame { msg_type: msg_type::HELLO, flags: 0, payload: fw.into_vec() };
        assert_eq!(
            Message::from_frame(&frame).unwrap_err(),
            WireError::BadValue { tag: hello_tags::MAGIC, why: "not an rt handoff peer" }
        );
    }

    #[test]
    fn a_hello_from_a_future_version_still_decodes_its_version_fields() {
        // The whole point of TLV in the handshake: a v1 build must be able to
        // read a v9 Hello well enough to say "we share no protocol".
        let mut m = Hello { proto_min: 4, proto_max: 9, rt_version: "0.9.0".into(), ..Hello::default() };
        m.caps = vec![77];
        let mut fields = m.fields();
        fields.push((0x6000, b"a field from the future".to_vec()));
        fields.sort_by_key(|(t, _)| *t);
        let mut fw = crate::tlv::FieldWriter::new();
        for (tag, val) in &fields {
            fw.field(*tag, val);
        }
        let frame = crate::frame::Frame { msg_type: msg_type::HELLO, flags: 0, payload: fw.into_vec() };
        let back = match Message::from_frame(&frame).unwrap() {
            Message::Hello(h) => h,
            other => panic!("wrong message: {other:?}"),
        };
        assert_eq!((back.proto_min, back.proto_max), (4, 9));
        assert_eq!(back.unknown_tags, vec![0x6000]);
    }

    #[test]
    fn offer_and_claim_round_trip() {
        let offer = Message::Offer(Offer {
            token: [7u8; 16],
            pane_count: 3,
            titles: vec!["zsh".into(), "claude".into(), "vim".into()],
            byte_estimate: 1_500_000,
            unknown_tags: vec![],
        });
        assert_eq!(round_trip(&offer), offer);

        let claim = Message::Claim(Claim {
            token: [7u8; 16],
            target: DropTargetWire::SplitRight(99),
            accepted_budget: 50_000,
            unknown_tags: vec![],
        });
        assert_eq!(round_trip(&claim), claim);
    }

    #[test]
    fn an_offer_from_a_future_version_keeps_the_fields_we_know() {
        let o = Offer { token: [3u8; 16], pane_count: 2, titles: vec!["a".into()], byte_estimate: 9, unknown_tags: vec![] };
        let mut fields = o.fields();
        fields.push((0x50, b"a budget knob from rt 0.9".to_vec()));
        fields.sort_by_key(|(t, _)| *t);
        let mut fw = crate::tlv::FieldWriter::new();
        for (tag, val) in &fields {
            fw.field(*tag, val);
        }
        let frame = crate::frame::Frame { msg_type: msg_type::OFFER, flags: 0, payload: fw.into_vec() };
        let back = match Message::from_frame(&frame).unwrap() {
            Message::Offer(o) => o,
            other => panic!("wrong message: {other:?}"),
        };
        assert_eq!(back.pane_count, 2);
        assert_eq!(back.titles, vec!["a".to_string()]);
        assert_eq!(back.unknown_tags, vec![0x50]);
    }

    #[test]
    fn every_drop_target_round_trips() {
        for t in [
            DropTargetWire::Root,
            DropTargetWire::SplitLeft(1),
            DropTargetWire::SplitRight(2),
            DropTargetWire::SplitAbove(3),
            DropTargetWire::SplitBelow(4),
            DropTargetWire::Swap(5),
            DropTargetWire::TabInsert { first_pane: 6, index: 2 },
        ] {
            let m = Message::Claim(Claim { token: [0; 16], target: t.clone(), accepted_budget: 1, unknown_tags: vec![] });
            assert_eq!(round_trip(&m), m, "target {t:?}");
        }
    }

    #[test]
    fn scroll_chunks_round_trip_and_carry_the_more_flag() {
        let m = Message::ScrollChunk(ScrollChunk {
            pane_uid: 5,
            more: true,
            lines: vec![Line { flags: 0, runs: vec![Run::text(0, "an older line")] }],
        });
        assert_eq!(round_trip(&m), m);
    }

    #[test]
    fn the_small_control_messages_round_trip() {
        for m in [
            Message::PaneFds(PaneFds { pane_uid: 3 }),
            Message::Adopted(Adopted { pane_uids: vec![1, 2, 3] }),
            Message::Failed(Failed { code: fail::BAD_TOKEN, text: "token expired".into() }),
            Message::Cancel,
            Message::Ping,
            Message::Pong,
            Message::Bye,
        ] {
            assert_eq!(round_trip(&m), m, "message {m:?}");
        }
    }

    #[test]
    fn failure_codes_are_frozen() {
        assert_eq!(fail::PROTOCOL_MISMATCH, 1);
        assert_eq!(fail::PEER_REJECTED, 2);
        assert_eq!(fail::BAD_TOKEN, 3);
        assert_eq!(fail::OVER_BUDGET, 4);
        assert_eq!(fail::MALFORMED_FRAME, 5);
        assert_eq!(fail::FD_PASSING_FAILED, 6);
        assert_eq!(fail::CANNOT_PLACE, 7);
        assert_eq!(fail::INTERNAL, 8);
    }

    #[test]
    fn an_unknown_message_type_is_named_in_the_error() {
        let frame = crate::frame::Frame { msg_type: 0x7777, flags: 0, payload: vec![] };
        assert_eq!(Message::from_frame(&frame).unwrap_err(), WireError::UnknownMsgType(0x7777));
    }

    #[test]
    fn a_pane_state_message_round_trips_through_a_frame() {
        let p = crate::pane::PaneWire {
            pane_uid: 1,
            title: "zsh".into(),
            cols: 80,
            rows: 24,
            child_pid: 2,
            modes: vec![],
            cursor: Default::default(),
            pen: Default::default(),
            style_table: vec![Default::default()],
            screen_primary: Default::default(),
            ..Default::default()
        };
        let m = Message::PaneState(Box::new(p));
        assert_eq!(round_trip(&m), m);
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt-handoff msg`
Expected: FAIL — `cannot find type Message in this scope`.

- [ ] **Step 3: Write the constants and the small types**

Prepend to `crates/rt-handoff/src/msg.rs`, below the module doc comment:

```rust
use crate::buf::{Reader, Writer};
use crate::error::{Result, WireError};
use crate::frame::Frame;
use crate::grid::Line;
use crate::pane::PaneWire;
use crate::tlv::{self, FieldWriter};
use crate::tree::Node;

/// Frame message types. Frozen.
pub mod msg_type {
    pub const HELLO: u16 = 0x01;
    pub const OFFER: u16 = 0x02;
    pub const CLAIM: u16 = 0x03;
    pub const TREE: u16 = 0x04;
    pub const PANE_STATE: u16 = 0x05;
    pub const PANE_FDS: u16 = 0x06;
    pub const SCROLL_CHUNK: u16 = 0x07;
    pub const ADOPTED: u16 = 0x08;
    pub const FAILED: u16 = 0x09;
    pub const CANCEL: u16 = 0x0A;
    pub const PING: u16 = 0x0B;
    pub const PONG: u16 = 0x0C;
    pub const BYE: u16 = 0x0D;
}

/// `Failed.code` values. Frozen. An unknown code is treated as `INTERNAL`.
pub mod fail {
    pub const PROTOCOL_MISMATCH: u32 = 1;
    pub const PEER_REJECTED: u32 = 2;
    pub const BAD_TOKEN: u32 = 3;
    pub const OVER_BUDGET: u32 = 4;
    pub const MALFORMED_FRAME: u32 = 5;
    pub const FD_PASSING_FAILED: u32 = 6;
    pub const CANNOT_PLACE: u32 = 7;
    pub const INTERNAL: u32 = 8;
}

/// `Hello` field tags.
pub mod hello_tags {
    pub const MAGIC: u64 = 0x01;
    pub const PROTO_MIN: u64 = 0x02;
    pub const PROTO_MAX: u64 = 0x03;
    pub const RT_VERSION: u64 = 0x04;
    pub const ENGINE: u64 = 0x05;
    pub const BOOT_ID: u64 = 0x06;
    pub const CAPS: u64 = 0x07;
    pub const MAX_SCROLLBACK_LINES: u64 = 0x08;
    pub const MAX_PAYLOAD_BYTES: u64 = 0x09;
    pub const DISPLAY: u64 = 0x0A;
}

/// `Offer` field tags.
pub mod offer_tags {
    pub const TOKEN: u64 = 0x01;
    pub const PANE_COUNT: u64 = 0x02;
    pub const TITLES: u64 = 0x03;
    pub const BYTE_ESTIMATE: u64 = 0x04;
}

/// `Claim` field tags.
pub mod claim_tags {
    pub const TOKEN: u64 = 0x01;
    pub const TARGET: u64 = 0x02;
    pub const ACCEPTED_BUDGET: u64 = 0x03;
}

/// Where a claimed payload should land in the receiver's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropTargetWire {
    Root,
    SplitLeft(u64),
    SplitRight(u64),
    SplitAbove(u64),
    SplitBelow(u64),
    Swap(u64),
    TabInsert { first_pane: u64, index: u32 },
}

impl DropTargetWire {
    fn write(&self, w: &mut Writer) {
        match self {
            DropTargetWire::Root => w.varint(0),
            DropTargetWire::SplitLeft(p) => {
                w.varint(1);
                w.varint(*p);
            }
            DropTargetWire::SplitRight(p) => {
                w.varint(2);
                w.varint(*p);
            }
            DropTargetWire::SplitAbove(p) => {
                w.varint(3);
                w.varint(*p);
            }
            DropTargetWire::SplitBelow(p) => {
                w.varint(4);
                w.varint(*p);
            }
            DropTargetWire::Swap(p) => {
                w.varint(5);
                w.varint(*p);
            }
            DropTargetWire::TabInsert { first_pane, index } => {
                w.varint(6);
                w.varint(*first_pane);
                w.varint(*index as u64);
            }
        }
    }

    fn read(r: &mut Reader<'_>, tag: u64) -> Result<DropTargetWire> {
        Ok(match r.varint()? {
            0 => DropTargetWire::Root,
            1 => DropTargetWire::SplitLeft(r.varint()?),
            2 => DropTargetWire::SplitRight(r.varint()?),
            3 => DropTargetWire::SplitAbove(r.varint()?),
            4 => DropTargetWire::SplitBelow(r.varint()?),
            5 => DropTargetWire::Swap(r.varint()?),
            6 => DropTargetWire::TabInsert { first_pane: r.varint()?, index: r.varint()? as u32 },
            _ => return Err(WireError::BadValue { tag, why: "unknown drop target kind" }),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hello {
    pub proto_min: u32,
    pub proto_max: u32,
    pub rt_version: String,
    pub engine: String,
    pub boot_id: String,
    pub caps: Vec<u64>,
    pub max_scrollback_lines: u32,
    pub max_payload_bytes: u64,
    pub display: String,
    pub unknown_tags: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Offer {
    pub token: [u8; 16],
    pub pane_count: u32,
    pub titles: Vec<String>,
    pub byte_estimate: u64,
    pub unknown_tags: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub token: [u8; 16],
    pub target: DropTargetWire,
    pub accepted_budget: u32,
    pub unknown_tags: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollChunk {
    pub pane_uid: u64,
    /// True when more chunks follow for this pane.
    pub more: bool,
    /// Newest-first, so a cancelled transfer keeps the history that matters.
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneFds {
    pub pane_uid: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adopted {
    pub pane_uids: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    pub code: u32,
    pub text: String,
}
```

- [ ] **Step 4: Write `Hello`'s TLV body**

Append to `crates/rt-handoff/src/msg.rs`:

```rust
impl Hello {
    /// `(tag, bytes)` in ascending order. Public so a test can splice in a
    /// field from an imagined future version.
    pub fn fields(&self) -> Vec<(u64, Vec<u8>)> {
        fn enc(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
            let mut w = Writer::new();
            f(&mut w);
            w.into_vec()
        }
        let mut out = vec![
            (hello_tags::MAGIC, crate::MAGIC.to_vec()),
            (hello_tags::PROTO_MIN, enc(|w| w.varint(self.proto_min as u64))),
            (hello_tags::PROTO_MAX, enc(|w| w.varint(self.proto_max as u64))),
            (hello_tags::RT_VERSION, enc(|w| w.str(&self.rt_version))),
            (hello_tags::ENGINE, enc(|w| w.str(&self.engine))),
            (hello_tags::BOOT_ID, enc(|w| w.str(&self.boot_id))),
            (hello_tags::CAPS, enc(|w| {
                w.varint(self.caps.len() as u64);
                for c in &self.caps {
                    w.varint(*c);
                }
            })),
            (hello_tags::MAX_SCROLLBACK_LINES, enc(|w| w.varint(self.max_scrollback_lines as u64))),
            (hello_tags::MAX_PAYLOAD_BYTES, enc(|w| w.varint(self.max_payload_bytes))),
            (hello_tags::DISPLAY, enc(|w| w.str(&self.display))),
        ];
        out.sort_by_key(|(t, _)| *t);
        out
    }

    fn encode(&self) -> Vec<u8> {
        let mut fw = FieldWriter::new();
        for (tag, val) in &self.fields() {
            fw.field(*tag, val);
        }
        fw.into_vec()
    }

    fn decode(body: &[u8]) -> Result<Hello> {
        let mut h = Hello::default();
        let unknown = tlv::walk(body, |tag, val| {
            let mut r = Reader::new(val);
            match tag {
                hello_tags::MAGIC => {
                    if val != crate::MAGIC {
                        return Err(WireError::BadValue { tag, why: "not an rt handoff peer" });
                    }
                    return Ok(true);
                }
                hello_tags::PROTO_MIN => h.proto_min = r.varint()? as u32,
                hello_tags::PROTO_MAX => h.proto_max = r.varint()? as u32,
                hello_tags::RT_VERSION => h.rt_version = r.str()?,
                hello_tags::ENGINE => h.engine = r.str()?,
                hello_tags::BOOT_ID => h.boot_id = r.str()?,
                hello_tags::CAPS => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        h.caps.push(r.varint()?);
                    }
                }
                hello_tags::MAX_SCROLLBACK_LINES => h.max_scrollback_lines = r.varint()? as u32,
                hello_tags::MAX_PAYLOAD_BYTES => h.max_payload_bytes = r.varint()?,
                hello_tags::DISPLAY => h.display = r.str()?,
                _ => return Ok(false),
            }
            r.finish()?;
            Ok(true)
        })?;
        h.unknown_tags = unknown;
        Ok(h)
    }
}
```

- [ ] **Step 5: Write `Offer`'s and `Claim`'s TLV bodies**

Append to `crates/rt-handoff/src/msg.rs`:

```rust
impl Offer {
    pub fn fields(&self) -> Vec<(u64, Vec<u8>)> {
        fn enc(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
            let mut w = Writer::new();
            f(&mut w);
            w.into_vec()
        }
        let mut out = vec![
            (offer_tags::TOKEN, self.token.to_vec()),
            (offer_tags::PANE_COUNT, enc(|w| w.varint(self.pane_count as u64))),
            (offer_tags::TITLES, enc(|w| {
                w.varint(self.titles.len() as u64);
                for t in &self.titles {
                    w.str(t);
                }
            })),
            (offer_tags::BYTE_ESTIMATE, enc(|w| w.varint(self.byte_estimate))),
        ];
        out.sort_by_key(|(t, _)| *t);
        out
    }

    fn encode(&self) -> Vec<u8> {
        let mut fw = FieldWriter::new();
        for (tag, val) in &self.fields() {
            fw.field(*tag, val);
        }
        fw.into_vec()
    }

    fn decode(body: &[u8]) -> Result<Offer> {
        let mut o = Offer::default();
        o.unknown_tags = tlv::walk(body, |tag, val| {
            let mut r = Reader::new(val);
            match tag {
                offer_tags::TOKEN => {
                    if val.len() != 16 {
                        return Err(WireError::BadValue { tag, why: "token must be 16 bytes" });
                    }
                    o.token.copy_from_slice(val);
                    return Ok(true);
                }
                offer_tags::PANE_COUNT => o.pane_count = r.varint()? as u32,
                offer_tags::TITLES => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        o.titles.push(r.str()?);
                    }
                }
                offer_tags::BYTE_ESTIMATE => o.byte_estimate = r.varint()?,
                _ => return Ok(false),
            }
            r.finish()?;
            Ok(true)
        })?;
        Ok(o)
    }
}

impl Claim {
    pub fn fields(&self) -> Vec<(u64, Vec<u8>)> {
        fn enc(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
            let mut w = Writer::new();
            f(&mut w);
            w.into_vec()
        }
        let mut out = vec![
            (claim_tags::TOKEN, self.token.to_vec()),
            (claim_tags::TARGET, enc(|w| self.target.write(w))),
            (claim_tags::ACCEPTED_BUDGET, enc(|w| w.varint(self.accepted_budget as u64))),
        ];
        out.sort_by_key(|(t, _)| *t);
        out
    }

    fn encode(&self) -> Vec<u8> {
        let mut fw = FieldWriter::new();
        for (tag, val) in &self.fields() {
            fw.field(*tag, val);
        }
        fw.into_vec()
    }

    fn decode(body: &[u8]) -> Result<Claim> {
        let mut token = [0u8; 16];
        let mut target = DropTargetWire::Root;
        let mut accepted_budget = 0u32;
        let unknown_tags = tlv::walk(body, |tag, val| {
            let mut r = Reader::new(val);
            match tag {
                claim_tags::TOKEN => {
                    if val.len() != 16 {
                        return Err(WireError::BadValue { tag, why: "token must be 16 bytes" });
                    }
                    token.copy_from_slice(val);
                    return Ok(true);
                }
                claim_tags::TARGET => target = DropTargetWire::read(&mut r, tag)?,
                claim_tags::ACCEPTED_BUDGET => accepted_budget = r.varint()? as u32,
                _ => return Ok(false),
            }
            r.finish()?;
            Ok(true)
        })?;
        Ok(Claim { token, target, accepted_budget, unknown_tags })
    }
}
```

- [ ] **Step 6: Write the `Message` enum and frame dispatch**

Append to `crates/rt-handoff/src/msg.rs`:

```rust
/// One decoded message. `PaneState` is boxed because it dwarfs the others.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Hello(Hello),
    Offer(Offer),
    Claim(Claim),
    Tree(Node),
    PaneState(Box<PaneWire>),
    PaneFds(PaneFds),
    ScrollChunk(ScrollChunk),
    Adopted(Adopted),
    Failed(Failed),
    Cancel,
    Ping,
    Pong,
    Bye,
}

impl Message {
    pub fn to_frame(&self) -> Result<Frame> {
        let (msg_type, payload) = match self {
            Message::Hello(h) => (msg_type::HELLO, h.encode()),
            Message::Offer(o) => (msg_type::OFFER, o.encode()),
            Message::Claim(c) => (msg_type::CLAIM, c.encode()),
            Message::Tree(n) => (msg_type::TREE, n.encode()),
            Message::PaneState(p) => (msg_type::PANE_STATE, p.encode()),
            Message::PaneFds(f) => {
                let mut w = Writer::new();
                w.varint(f.pane_uid);
                (msg_type::PANE_FDS, w.into_vec())
            }
            Message::ScrollChunk(s) => {
                let mut w = Writer::new();
                w.varint(s.pane_uid);
                w.u8(s.more as u8);
                w.varint(s.lines.len() as u64);
                for l in &s.lines {
                    l.write(&mut w);
                }
                (msg_type::SCROLL_CHUNK, w.into_vec())
            }
            Message::Adopted(a) => {
                let mut w = Writer::new();
                w.varint(a.pane_uids.len() as u64);
                for u in &a.pane_uids {
                    w.varint(*u);
                }
                (msg_type::ADOPTED, w.into_vec())
            }
            Message::Failed(f) => {
                let mut w = Writer::new();
                w.varint(f.code as u64);
                w.str(&f.text);
                (msg_type::FAILED, w.into_vec())
            }
            Message::Cancel => (msg_type::CANCEL, Vec::new()),
            Message::Ping => (msg_type::PING, Vec::new()),
            Message::Pong => (msg_type::PONG, Vec::new()),
            Message::Bye => (msg_type::BYE, Vec::new()),
        };
        let frame = Frame { msg_type, flags: 0, payload };
        if frame.payload.len() > crate::frame::MAX_PAYLOAD {
            return Err(WireError::PayloadTooLarge { len: frame.payload.len() as u64 });
        }
        Ok(frame)
    }

    pub fn from_frame(frame: &Frame) -> Result<Message> {
        let body = &frame.payload[..];
        Ok(match frame.msg_type {
            msg_type::HELLO => Message::Hello(Hello::decode(body)?),
            msg_type::OFFER => Message::Offer(Offer::decode(body)?),
            msg_type::CLAIM => Message::Claim(Claim::decode(body)?),
            msg_type::TREE => Message::Tree(Node::decode(body)?),
            msg_type::PANE_STATE => Message::PaneState(Box::new(PaneWire::decode(body)?)),
            msg_type::PANE_FDS => {
                let mut r = Reader::new(body);
                let pane_uid = r.varint()?;
                r.finish()?;
                Message::PaneFds(PaneFds { pane_uid })
            }
            msg_type::SCROLL_CHUNK => {
                let mut r = Reader::new(body);
                let pane_uid = r.varint()?;
                let more = r.u8()? != 0;
                let n = r.varint()? as usize;
                let mut lines = Vec::with_capacity(n.min(65536));
                for _ in 0..n {
                    lines.push(Line::read(&mut r, msg_type::SCROLL_CHUNK as u64)?);
                }
                r.finish()?;
                Message::ScrollChunk(ScrollChunk { pane_uid, more, lines })
            }
            msg_type::ADOPTED => {
                let mut r = Reader::new(body);
                let n = r.varint()? as usize;
                let mut pane_uids = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    pane_uids.push(r.varint()?);
                }
                r.finish()?;
                Message::Adopted(Adopted { pane_uids })
            }
            msg_type::FAILED => {
                let mut r = Reader::new(body);
                let code = r.varint()? as u32;
                let text = r.str()?;
                r.finish()?;
                Message::Failed(Failed { code, text })
            }
            msg_type::CANCEL => Message::Cancel,
            msg_type::PING => Message::Ping,
            msg_type::PONG => Message::Pong,
            msg_type::BYE => Message::Bye,
            other => return Err(WireError::UnknownMsgType(other)),
        })
    }
}
```

- [ ] **Step 7: Make `Line::write`/`Line::read` reachable**

`ScrollChunk` needs them. They are already `pub` on `Line` from Task 5 — if the
compiler disagrees, make them `pub` rather than duplicating the logic here.

- [ ] **Step 8: Wire the module into the crate root**

In `crates/rt-handoff/src/lib.rs`, add:

```rust
pub mod msg;
```

- [ ] **Step 9: Run the tests to verify they pass**

Run: `cargo test -p rt-handoff`
Expected: PASS, 79 tests.

- [ ] **Step 10: Commit**

```bash
git add crates/rt-handoff/src/msg.rs crates/rt-handoff/src/lib.rs
git commit -m "feat(handoff): message set, drop targets and frame dispatch"
```

---

### Task 10: Deterministic generator and the round-trip property test

**Files:**
- Create: `crates/rt-handoff/src/testgen.rs`
- Create: `crates/rt-handoff/tests/roundtrip.rs`
- Modify: `crates/rt-handoff/src/lib.rs` (add the gated `pub mod testgen;`)

**Interfaces:**
- Consumes: every model type from Tasks 4–9.
- Produces: `testgen::Rng` with `new(u64)` and `next_u64()`, `below(u64)`, `bool()`, `pick<T: Copy>(&[T])`; `testgen::{gen_pane, gen_tree, gen_scroll_chunk, gen_message}`, each taking `&mut Rng`.

The PRNG is a hand-rolled xorshift64 with a printed seed, matching
`crates/vt-conformance/src/lib.rs:192`. A failing case reproduces exactly from
the seed in the panic message, and the crate stays dependency-free.

- [ ] **Step 1: Write the generator**

`crates/rt-handoff/src/testgen.rs`:

```rust
//! Deterministic model generator for the round-trip tests.
//!
//! Compiled unconditionally so the integration tests under `tests/` can reach
//! it, and so phase 2's engine tests can reuse it.

use crate::grid::{line_flags, run_flags, Grid, Line, Run};
use crate::msg::{Adopted, Claim, DropTargetWire, Failed, Hello, Message, Offer, PaneFds, ScrollChunk};
use crate::pane::{Charsets, CursorState, ImageEntry, KittyKbd, Margins, ModeEntry, PaneWire, SavedCursor};
use crate::style::{attrs, Colour, Style};
use crate::tree::Node;

/// xorshift64. No dependencies, and a seed always replays exactly.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed | 1) // avoid the all-zero fixed point
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    pub fn below(&mut self, n: u64) -> u64 {
        if n == 0 {
            0
        } else {
            self.next_u64() % n
        }
    }

    pub fn bool(&mut self) -> bool {
        self.next_u64() & 1 == 1
    }

    pub fn pick<T: Copy>(&mut self, xs: &[T]) -> T {
        xs[self.below(xs.len() as u64) as usize]
    }
}

fn gen_colour(r: &mut Rng) -> Colour {
    match r.below(3) {
        0 => Colour::Default,
        1 => Colour::Indexed(r.below(256) as u8),
        _ => Colour::Rgb(r.below(256) as u8, r.below(256) as u8, r.below(256) as u8),
    }
}

fn gen_style(r: &mut Rng) -> Style {
    Style {
        fg: gen_colour(r),
        bg: gen_colour(r),
        underline: gen_colour(r),
        attrs: (r.next_u64() as u32) & attrs::KNOWN,
        link_id: r.below(4) as u32,
    }
}

fn gen_text(r: &mut Rng, cells: usize) -> String {
    const ALPHABET: [char; 12] = ['a', 'z', ' ', '0', '9', '/', '$', '~', 'é', 'ß', '→', '★'];
    (0..cells).map(|_| r.pick(&ALPHABET)).collect()
}

fn gen_run(r: &mut Rng, styles: u32) -> Run {
    let style_id = r.below(styles.max(1) as u64) as u32;
    match r.below(5) {
        0 => Run::blank(style_id, 1 + r.below(80) as u32),
        1 => {
            // Wide: two columns per character.
            let n = 1 + r.below(6) as usize;
            let text: String = (0..n).map(|_| r.pick(&['日', '本', '語', '中'])).collect();
            Run::wide(style_id, &text)
        }
        2 => {
            // Combining marks: some cells carry more than one char.
            let cells = 1 + r.below(6) as usize;
            let mut text = String::new();
            let mut char_counts = Vec::with_capacity(cells);
            for _ in 0..cells {
                text.push(r.pick(&['e', 'a', 'o']));
                if r.bool() {
                    text.push('\u{0301}');
                    char_counts.push(2);
                } else {
                    char_counts.push(1);
                }
            }
            Run { flags: run_flags::CHAR_COUNTS, style_id, cell_span: cells as u32, text, char_counts }
        }
        _ => {
            let cells = 1 + r.below(40) as usize;
            Run::text(style_id, &gen_text(r, cells))
        }
    }
}

fn gen_line(r: &mut Rng, styles: u32) -> Line {
    let flags = r.pick(&[0, line_flags::WRAPPED, line_flags::DECDWL, line_flags::DECDHL_TOP, line_flags::DECDHL_BOTTOM]);
    let runs = (0..r.below(5)).map(|_| gen_run(r, styles)).collect();
    Line { flags, runs }
}

fn gen_grid(r: &mut Rng, styles: u32, max_lines: u64) -> Grid {
    Grid { lines: (0..r.below(max_lines)).map(|_| gen_line(r, styles)).collect() }
}

/// A pane with every field exercised, valid by construction.
pub fn gen_pane(r: &mut Rng) -> PaneWire {
    let n_styles = 1 + r.below(8) as u32;
    PaneWire {
        pane_uid: r.next_u64(),
        title: gen_text(r, 1 + r.below(20) as usize),
        cwd: r.bool().then(|| format!("/home/u/{}", gen_text(r, 5))),
        cols: 1 + r.below(400) as u32,
        rows: 1 + r.below(200) as u32,
        scrollback_limit: r.bool().then(|| r.below(5_000_000) as u32),
        columns_count: r.bool().then(|| 1 + r.below(8) as u32),
        group: r.bool().then(|| r.below(16) as u32),
        broadcast: r.bool().then(|| r.bool()),
        child_pid: r.below(4_000_000) as u32,
        shell_argv: (0..r.below(4)).map(|_| gen_text(r, 6)).collect(),
        env_extras: (0..r.below(3)).map(|_| (gen_text(r, 6), gen_text(r, 20))).collect(),
        show_titlebar: r.bool().then(|| r.bool()),
        palette: r.bool().then(|| (0..256).map(|_| (r.below(256) as u8, r.below(256) as u8, r.below(256) as u8)).collect()),
        modes: (0..r.below(20))
            .map(|_| ModeEntry { kind: r.below(2) as u8, number: r.below(3000) as u32, value: r.below(2) as u8 })
            .collect(),
        cursor: CursorState {
            col: r.below(400) as u32,
            row: r.below(200) as u32,
            shape: r.below(3) as u8,
            visible: r.bool(),
            blink: r.bool(),
            pending_wrap: r.bool(),
        },
        saved_cursor: r.bool().then(|| SavedCursor {
            col: r.below(400) as u32,
            row: r.below(200) as u32,
            pen: gen_style(r),
            charsets: Charsets { g: [b'B', b'0', b'A', b'B'], gl: r.below(4) as u8, gr: r.below(4) as u8 },
            origin: r.bool(),
        }),
        charsets: r.bool().then(|| Charsets { g: [b'B'; 4], gl: r.below(4) as u8, gr: r.below(4) as u8 }),
        tab_stops: r.bool().then(|| (0..1 + r.below(400)).map(|_| r.bool()).collect()),
        margins: r.bool().then(|| Margins {
            top: r.below(100) as u32,
            bottom: r.below(100) as u32,
            left: r.below(100) as u32,
            right: r.below(100) as u32,
        }),
        title_stack: (0..r.below(4)).map(|_| gen_text(r, 8)).collect(),
        pen: gen_style(r),
        active_screen: r.below(2) as u8,
        kitty_kbd: r.bool().then(|| KittyKbd {
            stack: (0..r.below(4)).map(|_| r.below(32) as u32).collect(),
            modify_other_keys: r.below(3) as u8,
        }),
        pending_raw: (0..r.below(8)).map(|_| r.below(256) as u8).collect(),
        style_table: (0..n_styles).map(|_| gen_style(r)).collect(),
        screen_primary: gen_grid(r, n_styles, 60),
        screen_alt: r.bool().then(|| gen_grid(r, n_styles, 30)),
        uri_table: (0..r.below(4)).map(|_| (r.below(1000) as u32, format!("https://h.invalid/{}", gen_text(r, 6)))).collect(),
        image_table: (0..r.below(3))
            .map(|_| ImageEntry {
                id: r.below(1000) as u32,
                format: 1,
                w: 1 + r.below(64) as u32,
                h: 1 + r.below(64) as u32,
                data: (0..r.below(64)).map(|_| r.below(256) as u8).collect(),
            })
            .collect(),
        tag_names: Vec::new(),   // filled by the decoder, never by a producer
        unknown_tags: Vec::new(), // ditto
    }
}

/// A layout tree of bounded depth.
pub fn gen_tree(r: &mut Rng, depth: u32) -> Node {
    if depth == 0 || r.below(3) == 0 {
        return Node::Leaf { pane_uid: r.next_u64() };
    }
    let ratio = (r.below(1001) as f32) / 1000.0;
    let n = 2 + r.below(2) as usize;
    match r.below(3) {
        0 => Node::HSplit { ratio, children: (0..n).map(|_| gen_tree(r, depth - 1)).collect() },
        1 => Node::VSplit { ratio, children: (0..n).map(|_| gen_tree(r, depth - 1)).collect() },
        _ => Node::Tabs {
            active: r.below(n as u64) as u32,
            children: (0..n).map(|_| (gen_text(r, 6), gen_tree(r, depth - 1))).collect(),
        },
    }
}

pub fn gen_scroll_chunk(r: &mut Rng) -> ScrollChunk {
    ScrollChunk {
        pane_uid: r.next_u64(),
        more: r.bool(),
        lines: (0..r.below(200)).map(|_| gen_line(r, 4)).collect(),
    }
}

/// Any message, so the frame dispatch is exercised too.
pub fn gen_message(r: &mut Rng) -> Message {
    match r.below(10) {
        0 => Message::Hello(Hello {
            proto_min: 1,
            proto_max: 1 + r.below(4) as u32,
            rt_version: format!("0.{}.{}", r.below(10), r.below(40)),
            engine: if r.bool() { "vtterm".into() } else { "alacritty".into() },
            boot_id: gen_text(r, 16),
            caps: (0..r.below(5)).map(|_| r.below(64)).collect(),
            max_scrollback_lines: r.below(5_000_000) as u32,
            max_payload_bytes: r.below(64 * 1024 * 1024),
            display: if r.bool() { "wayland".into() } else { "x11".into() },
            unknown_tags: vec![],
        }),
        1 => {
            let mut token = [0u8; 16];
            for b in token.iter_mut() {
                *b = r.below(256) as u8;
            }
            Message::Offer(Offer {
                token,
                pane_count: r.below(20) as u32,
                titles: (0..r.below(6)).map(|_| gen_text(r, 8)).collect(),
                byte_estimate: r.next_u64() % 1_000_000_000,
                unknown_tags: vec![],
            })
        }
        2 => {
            let mut token = [0u8; 16];
            for b in token.iter_mut() {
                *b = r.below(256) as u8;
            }
            let target = match r.below(7) {
                0 => DropTargetWire::Root,
                1 => DropTargetWire::SplitLeft(r.next_u64()),
                2 => DropTargetWire::SplitRight(r.next_u64()),
                3 => DropTargetWire::SplitAbove(r.next_u64()),
                4 => DropTargetWire::SplitBelow(r.next_u64()),
                5 => DropTargetWire::Swap(r.next_u64()),
                _ => DropTargetWire::TabInsert { first_pane: r.next_u64(), index: r.below(10) as u32 },
            };
            Message::Claim(Claim { token, target, accepted_budget: r.below(5_000_000) as u32, unknown_tags: vec![] })
        }
        3 => Message::Tree(gen_tree(r, 4)),
        4 => Message::PaneState(Box::new(gen_pane(r))),
        5 => Message::PaneFds(PaneFds { pane_uid: r.next_u64() }),
        6 => Message::ScrollChunk(gen_scroll_chunk(r)),
        7 => Message::Adopted(Adopted { pane_uids: (0..r.below(8)).map(|_| r.next_u64()).collect() }),
        8 => Message::Failed(Failed { code: 1 + r.below(8) as u32, text: gen_text(r, 20) }),
        _ => match r.below(4) {
            0 => Message::Cancel,
            1 => Message::Ping,
            2 => Message::Pong,
            _ => Message::Bye,
        },
    }
}
```

- [ ] **Step 2: Wire the module into the crate root**

In `crates/rt-handoff/src/lib.rs`, add:

```rust
// Unconditional, NOT `#[cfg(test)]`: integration tests under `tests/` link the
// library built WITHOUT `cfg(test)`, so a gated module would be invisible to
// them. Phase 2's engine tests reuse it too. It costs a few hundred bytes.
#[doc(hidden)]
pub mod testgen;
```

- [ ] **Step 3: Write the round-trip property test**

`crates/rt-handoff/tests/roundtrip.rs`:

```rust
//! Encode → decode → compare, over ten thousand generated models.
//!
//! This is the same standard the terminal engine is held to: not "it works on
//! the case I thought of" but "it works on ten thousand cases nobody thought
//! of, and a failure replays from its seed".

use rt_handoff::frame::Frame;
use rt_handoff::msg::Message;
use rt_handoff::pane::PaneWire;
use rt_handoff::testgen::{gen_message, gen_pane, gen_tree, Rng};
use rt_handoff::tree::Node;

#[test]
fn panes_round_trip_over_ten_thousand_seeds() {
    for seed in 1..=10_000u64 {
        let mut r = Rng::new(seed);
        let pane = gen_pane(&mut r);
        let bytes = pane.encode();
        let back = match PaneWire::decode(&bytes) {
            Ok(p) => p,
            Err(e) => panic!("seed {seed}: decode failed: {e}"),
        };
        // tag_names is produced by the encoder and read back by the decoder, so
        // compare against the pane as it comes back rather than as it went in.
        let mut expected = pane.clone();
        expected.tag_names = back.tag_names.clone();
        assert_eq!(back, expected, "seed {seed}");
        assert!(back.unknown_tags.is_empty(), "seed {seed}: our own encoding had unknown tags");
    }
}

#[test]
fn pane_encoding_is_canonical() {
    // Encoding twice must give identical bytes, and re-encoding a decoded pane
    // must reproduce the original. Without this the golden corpus is worthless.
    for seed in 1..=2_000u64 {
        let mut r = Rng::new(seed);
        let pane = gen_pane(&mut r);
        let once = pane.encode();
        assert_eq!(once, pane.encode(), "seed {seed}: encoding is not deterministic");
        let back = PaneWire::decode(&once).unwrap();
        assert_eq!(back.encode(), once, "seed {seed}: re-encoding a decoded pane changed the bytes");
    }
}

#[test]
fn trees_round_trip() {
    for seed in 1..=5_000u64 {
        let mut r = Rng::new(seed);
        let tree = gen_tree(&mut r, 5);
        let back = Node::decode(&tree.encode()).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(back, tree, "seed {seed}");
    }
}

#[test]
fn every_message_round_trips_through_a_frame() {
    for seed in 1..=10_000u64 {
        let mut r = Rng::new(seed);
        let msg = gen_message(&mut r);
        let frame = msg.to_frame().unwrap_or_else(|e| panic!("seed {seed}: {e}"));

        // Also exercise the frame envelope itself, including the byte count.
        let mut wire = Vec::new();
        frame.encode(&mut wire).unwrap();
        let (decoded_frame, used) = Frame::decode(&wire).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(used, wire.len(), "seed {seed}");

        let back = Message::from_frame(&decoded_frame).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let expected = match (&msg, back.clone()) {
            // tag_names round-trips through the encoder, as above.
            (Message::PaneState(orig), Message::PaneState(b)) => {
                let mut e = orig.clone();
                e.tag_names = b.tag_names.clone();
                Message::PaneState(e)
            }
            _ => msg.clone(),
        };
        assert_eq!(back, expected, "seed {seed}");
    }
}

#[test]
fn truncating_any_encoded_pane_errors_rather_than_panics() {
    // A short read must never be a panic: on a socket, truncation is normal.
    for seed in 1..=200u64 {
        let mut r = Rng::new(seed);
        let bytes = gen_pane(&mut r).encode();
        for cut in [1, bytes.len() / 3, bytes.len() / 2, bytes.len() - 1] {
            let _ = PaneWire::decode(&bytes[..cut]); // must return, not panic
        }
    }
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p rt-handoff --test roundtrip`
Expected: PASS, 5 tests. If `panes_round_trip_over_ten_thousand_seeds` is slow
in a debug build, run it with `--release` while iterating; do not reduce the
seed count.

- [ ] **Step 5: Commit**

```bash
git add crates/rt-handoff/src/testgen.rs crates/rt-handoff/tests/roundtrip.rs crates/rt-handoff/src/lib.rs
git commit -m "test(handoff): deterministic generator and ten-thousand-seed round-trip"
```

---

### Task 11: Forward-compatibility — unknown tags at every nesting level

The test that proves the format survives a version it has never seen. It
simulates a future donor by splicing tags this build does not know into a valid
message, then asserts the decoder skips them, reports them, and produces
byte-identical known state.

**Files:**
- Create: `crates/rt-handoff/tests/forward_compat.rs`

**Interfaces:**
- Consumes: `tlv::{FieldWriter, walk}`, `pane::{PaneWire, tags, name_of_tag}`, `msg::{Message, Hello, hello_tags, msg_type}`, `frame::Frame`, `testgen::{Rng, gen_pane}`.

- [ ] **Step 1: Write the test**

`crates/rt-handoff/tests/forward_compat.rs`:

```rust
//! What happens when a NEWER rt talks to this one.
//!
//! Every assertion here is a promise to a build that does not exist yet: it may
//! add fields, and we will ignore them without losing anything we did
//! understand, and we will be able to tell the user what we ignored.

use rt_handoff::frame::Frame;
use rt_handoff::msg::{hello_tags, msg_type, Hello, Message};
use rt_handoff::pane::{name_of_tag, tags, PaneWire};
use rt_handoff::testgen::{gen_pane, Rng};
use rt_handoff::tlv::FieldWriter;

/// Rebuild a body from `(tag, bytes)` pairs, keeping ascending order.
fn body_from(mut fields: Vec<(u64, Vec<u8>)>) -> Vec<u8> {
    fields.sort_by_key(|(t, _)| *t);
    let mut fw = FieldWriter::new();
    for (tag, val) in &fields {
        fw.field(*tag, val);
    }
    fw.into_vec()
}

#[test]
fn a_pane_from_a_future_version_decodes_with_every_known_field_intact() {
    for seed in 1..=500u64 {
        let mut r = Rng::new(seed);
        let pane = gen_pane(&mut r);
        let plain = PaneWire::decode(&pane.encode()).unwrap();

        // A future rt adds fields below, between and above ours.
        let mut fields = pane.fields();
        fields.push((0x00, b"a tag below every tag we know".to_vec()));
        fields.push((0x30, b"between the state block and the style table".to_vec()));
        fields.push((0x52, b"just past the image table".to_vec()));
        fields.push((0xDEAD_BEEF, vec![0xAB; 300]));
        let future = PaneWire::decode(&body_from(fields)).unwrap();

        // Everything we understood is identical...
        let mut expected = plain.clone();
        expected.unknown_tags = future.unknown_tags.clone();
        assert_eq!(future, expected, "seed {seed}");

        // ...and we can say exactly what we skipped, in wire order.
        assert_eq!(future.unknown_tags, vec![0x00, 0x30, 0x52, 0xDEAD_BEEF], "seed {seed}");
    }
}

#[test]
fn skipped_tags_can_be_named_from_the_donors_table() {
    // Rule R6: a receiver cannot name a tag it has never heard of, so the
    // donor ships names. Simulate a future field that the donor DID name.
    let mut r = Rng::new(99);
    let pane = gen_pane(&mut r);

    let mut fields = pane.fields();
    // Replace tag_names with one that also names the future field.
    fields.retain(|(t, _)| *t != tags::TAG_NAMES);
    let mut names: Vec<(u64, String)> = pane
        .fields()
        .iter()
        .filter_map(|(t, _)| name_of_tag(*t).map(|n| (*t, n.to_string())))
        .collect();
    names.push((tags::TAG_NAMES, "tag_names".to_string()));
    names.push((0x60, "holographic_cursor".to_string()));
    names.sort_by_key(|(t, _)| *t);

    let mut nw = Vec::new();
    {
        // varint count, then (tag, name) pairs — the same shape the encoder writes.
        let mut w = rt_handoff::buf::Writer::new();
        w.varint(names.len() as u64);
        for (t, n) in &names {
            w.varint(*t);
            w.str(n);
        }
        nw = w.into_vec();
    }
    fields.push((tags::TAG_NAMES, nw));
    fields.push((0x60, b"from rt 0.9".to_vec()));

    let back = PaneWire::decode(&body_from(fields)).unwrap();
    assert_eq!(back.unknown_tags, vec![0x60]);

    let named: Vec<&str> = back
        .unknown_tags
        .iter()
        .map(|t| back.tag_names.iter().find(|(nt, _)| nt == t).map(|(_, n)| n.as_str()).unwrap_or("?"))
        .collect();
    assert_eq!(named, vec!["holographic_cursor"], "the donor's table must let us name what we skipped");
}

#[test]
fn a_future_hello_still_yields_its_version_numbers() {
    // The most important forward-compatibility case in the whole protocol: if
    // we cannot read a future Hello, we cannot even print a useful refusal.
    let future = Hello { proto_min: 7, proto_max: 12, rt_version: "1.4.0".into(), ..Hello::default() };
    let mut fields = future.fields();
    fields.push((0x40, b"negotiation extension".to_vec()));
    fields.push((0x99, vec![0; 64]));

    let frame = Frame { msg_type: msg_type::HELLO, flags: 0, payload: body_from(fields) };
    let back = match Message::from_frame(&frame).unwrap() {
        Message::Hello(h) => h,
        other => panic!("wrong message: {other:?}"),
    };
    assert_eq!((back.proto_min, back.proto_max), (7, 12));
    assert_eq!(back.rt_version, "1.4.0");
    assert_eq!(back.unknown_tags, vec![0x40, 0x99]);
}

#[test]
fn unknown_attribute_bits_do_not_leak_into_the_model() {
    // A future version adds attribute bit 20. We must render what we know and
    // silently drop what we do not — never re-emit a bit we cannot describe.
    use rt_handoff::style::{attrs, Style};
    let s = Style { attrs: attrs::BOLD | attrs::STRIKEOUT | (1 << 20) | (1 << 31), ..Style::default() };
    let table = rt_handoff::style::write_table(&[s]);
    let back = rt_handoff::style::read_table(&table).unwrap();
    assert_eq!(back[0].attrs, attrs::BOLD | attrs::STRIKEOUT);
}

#[test]
fn an_unknown_message_type_does_not_poison_the_stream() {
    // A future message type must be skippable: the frame header carries its
    // length, so a receiver can step over it and keep reading.
    let mut wire = Vec::new();
    Frame { msg_type: 0x4242, flags: 0, payload: vec![1; 100] }.encode(&mut wire).unwrap();
    Message::Ping.to_frame().unwrap().encode(&mut wire).unwrap();

    let (unknown, used) = Frame::decode(&wire).unwrap();
    assert!(Message::from_frame(&unknown).is_err(), "we do not know this type");

    let (next, _) = Frame::decode(&wire[used..]).unwrap();
    assert_eq!(Message::from_frame(&next).unwrap(), Message::Ping, "the stream survived");
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rt-handoff --test forward_compat`
Expected: FAIL — `rt_handoff::buf::Writer` is private, or `PaneWire::fields` is
not public, depending on what Tasks 6 and 2 exposed.

- [ ] **Step 3: Make the needed items public**

The test needs `buf::Writer` (already `pub` from Task 2), `PaneWire::fields`
(already `pub` from Task 6) and `pane::name_of_tag` (already `pub` from Task 6).
If any is not reachable from an integration test, make it `pub` — these are
part of the crate's contract with phase 2, not internals.

Also fix the awkward `let mut nw = Vec::new();` shadow in the second test while
you are there:

```rust
    let nw = {
        let mut w = rt_handoff::buf::Writer::new();
        w.varint(names.len() as u64);
        for (t, n) in &names {
            w.varint(*t);
            w.str(n);
        }
        w.into_vec()
    };
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p rt-handoff --test forward_compat`
Expected: PASS, 5 tests.

- [ ] **Step 5: Commit**

```bash
git add crates/rt-handoff/tests/forward_compat.rs
git commit -m "test(handoff): unknown tags, bits and message types survive a future peer"
```

---

### Task 12: The golden corpus

The cross-version guarantee, made testable today with a single build: committed
v1 payloads that every future version must still decode. When rt 0.9 changes
this crate, this test is what tells it that it broke rt 0.3.20.

**Files:**
- Create: `crates/rt-handoff/src/bin/regen_fixtures.rs`
- Create: `crates/rt-handoff/tests/golden.rs`
- Create: `crates/rt-handoff/tests/fixtures/wire-v1/README.md`
- Create: `crates/rt-handoff/tests/fixtures/wire-v1/*.bin` (generated in Step 3)
- Create: `crates/rt-handoff/tests/fixtures/wire-v1/MANIFEST.txt` (generated in Step 3)

**Interfaces:**
- Consumes: `testgen::{Rng, gen_pane, gen_tree, gen_message}`, `pane::PaneWire`, `tree::Node`, `msg::Message`, `frame::Frame`.
- Produces: the corpus. No library API.

Fixtures are named by what they hold, because the test dispatches on the prefix:
`pane_*.bin` is a bare `PaneWire` body, `tree_*.bin` a `Node` body, and
`msg_*.bin` a complete encoded `Frame`.

- [ ] **Step 1: Write the regeneration binary**

`crates/rt-handoff/src/bin/regen_fixtures.rs`:

```rust
//! Regenerates the golden corpus. Run by hand, never by a test:
//!
//!     cargo run -p rt-handoff --bin regen_fixtures
//!
//! Regenerating after a format change DEFEATS the corpus. The corpus exists to
//! fail when the format changes. If this binary's output differs from what is
//! committed, that difference is the finding — investigate it before you
//! overwrite anything.

use std::io::Write;
use std::path::PathBuf;

use rt_handoff::frame::Frame;
use rt_handoff::testgen::{gen_message, gen_pane, gen_tree, Rng};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wire-v1")
}

fn write(name: &str, bytes: &[u8], manifest: &mut Vec<String>) {
    let path = dir().join(name);
    std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    manifest.push(format!("{name} {}", bytes.len()));
    println!("{name}: {} bytes", bytes.len());
}

fn main() {
    std::fs::create_dir_all(dir()).unwrap();
    let mut manifest = Vec::new();

    // Panes across a spread of shapes, from fixed seeds so the corpus is stable.
    for seed in [1u64, 2, 3, 7, 11, 42, 1234, 99991] {
        let mut r = Rng::new(seed);
        write(&format!("pane_seed{seed:05}.bin"), &gen_pane(&mut r).encode(), &mut manifest);
    }

    // Trees, including a tab group and a deep split chain.
    for seed in [5u64, 50, 500] {
        let mut r = Rng::new(seed);
        write(&format!("tree_seed{seed:05}.bin"), &gen_tree(&mut r, 5).encode(), &mut manifest);
    }

    // One framed message of each kind we can reach from the generator.
    for seed in 1..=20u64 {
        let mut r = Rng::new(seed * 7919);
        let msg = gen_message(&mut r);
        let mut wire = Vec::new();
        msg.to_frame().unwrap().encode(&mut wire).unwrap();
        write(&format!("msg_seed{:05}.bin", seed * 7919), &wire, &mut manifest);
    }

    // A frame at an awkward boundary: an empty payload.
    let mut wire = Vec::new();
    Frame { msg_type: rt_handoff::msg::msg_type::BYE, flags: 0, payload: vec![] }
        .encode(&mut wire)
        .unwrap();
    write("msg_empty_bye.bin", &wire, &mut manifest);

    manifest.sort();
    let mut f = std::fs::File::create(dir().join("MANIFEST.txt")).unwrap();
    for line in &manifest {
        writeln!(f, "{line}").unwrap();
    }
    println!("{} fixtures", manifest.len());
}
```

- [ ] **Step 2: Write the fixtures README**

`crates/rt-handoff/tests/fixtures/wire-v1/README.md`:

```markdown
# Golden corpus — wire protocol v1

Committed payloads produced by the first build that shipped the format. Every
future version of `rt-handoff` must decode all of them, and must re-encode each
one to byte-identical output.

**Do not regenerate these to make a test pass.** The test failing is the
corpus doing its job: it means the format changed, which means a newer rt can
no longer read an older rt's panes. Either revert the change or add a new tag
instead of altering an existing one (rule R1).

Adding fixtures is fine and welcome. Changing or deleting one is a protocol
break and needs the same scrutiny as changing the spec.
```

- [ ] **Step 3: Generate the fixtures**

Run: `cargo run -p rt-handoff --bin regen_fixtures`
Expected: `32 fixtures` and a `MANIFEST.txt` listing each name and byte length.

- [ ] **Step 4: Write the golden test**

`crates/rt-handoff/tests/golden.rs`:

```rust
//! Every committed v1 payload must still decode, and must re-encode to the
//! same bytes. This is the cross-version guarantee, checkable with one build.

use std::path::{Path, PathBuf};

use rt_handoff::frame::Frame;
use rt_handoff::msg::Message;
use rt_handoff::pane::PaneWire;
use rt_handoff::tree::Node;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wire-v1")
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir()).expect("the fixture directory must exist") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        out.push((name, std::fs::read(&path).unwrap()));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn the_corpus_is_present_and_complete() {
    // MANIFEST.txt pins every fixture's name and length, so a fixture that is
    // deleted, truncated or quietly rewritten fails here rather than silently
    // shrinking the guarantee.
    let manifest = std::fs::read_to_string(dir().join("MANIFEST.txt")).expect("MANIFEST.txt must exist");
    let mut expected: Vec<(String, usize)> = manifest
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (name, len) = l.rsplit_once(' ').expect("MANIFEST line is `<name> <length>`");
            (name.to_string(), len.parse().unwrap())
        })
        .collect();
    expected.sort();

    let actual: Vec<(String, usize)> = fixtures().into_iter().map(|(n, b)| (n, b.len())).collect();
    assert_eq!(actual, expected, "the corpus does not match MANIFEST.txt");
    assert!(actual.len() >= 32, "the corpus must not shrink: {} fixtures", actual.len());
}

#[test]
fn every_fixture_decodes_and_re_encodes_byte_for_byte() {
    for (name, bytes) in fixtures() {
        let re_encoded = round_trip(&name, &bytes);
        assert_eq!(
            re_encoded,
            bytes,
            "{name}: re-encoding changed the bytes. The format is frozen — if this is \
             a deliberate protocol change, it is a BREAKING one; read the fixtures README."
        );
    }
}

/// Decode a fixture by its filename prefix and encode it straight back.
fn round_trip(name: &str, bytes: &[u8]) -> Vec<u8> {
    if name.starts_with("pane_") {
        PaneWire::decode(bytes).unwrap_or_else(|e| panic!("{name}: {e}")).encode()
    } else if name.starts_with("tree_") {
        Node::decode(bytes).unwrap_or_else(|e| panic!("{name}: {e}")).encode()
    } else if name.starts_with("msg_") {
        let (frame, used) = Frame::decode(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(used, bytes.len(), "{name}: trailing bytes after the frame");
        let msg = Message::from_frame(&frame).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut out = Vec::new();
        msg.to_frame().unwrap().encode(&mut out).unwrap();
        out
    } else {
        panic!("{name}: fixture name must start with pane_, tree_ or msg_");
    }
}

#[test]
fn a_known_fixture_decodes_to_known_values() {
    // One fixture checked semantically, not just structurally, so a decoder
    // that round-trips garbage consistently still fails.
    let bytes = std::fs::read(dir().join("pane_seed00042.bin")).expect("pane_seed00042.bin must exist");
    let pane = PaneWire::decode(&bytes).unwrap();
    assert!(pane.cols >= 1 && pane.cols <= 400, "cols out of range: {}", pane.cols);
    assert!(pane.rows >= 1 && pane.rows <= 200, "rows out of range: {}", pane.rows);
    assert!(!pane.tag_names.is_empty(), "tag_names must be present (R6)");
    assert!(pane.unknown_tags.is_empty(), "a v1 fixture must have no unknown tags in a v1 build");
    assert!(!pane.style_table.is_empty(), "style_table is required");
}

#[test]
fn fixture_paths_are_relative_to_the_crate_not_the_cwd() {
    // Guards against a fixture loader that only works when run from the repo
    // root — the failure mode that makes a corpus silently stop running.
    assert!(Path::new(&dir()).is_absolute());
    assert!(dir().ends_with("tests/fixtures/wire-v1"));
}
```

- [ ] **Step 5: Run the test to verify it passes**

Run: `cargo test -p rt-handoff --test golden`
Expected: PASS, 4 tests.

- [ ] **Step 6: Prove the corpus actually bites**

Temporarily change one attribute constant in `style.rs` (say `OVERLINE` to
16384), run `cargo test -p rt-handoff --test golden`, and confirm it FAILS with
the "re-encoding changed the bytes" message. Then revert the constant and
confirm it passes again. A corpus that cannot fail is decoration.

- [ ] **Step 7: Run the whole battery**

Run: `cargo test -p rt-handoff`
Expected: PASS, all tests.

Run: `ci/verify.sh` (checks x86-64 and riscv64 — varint and little-endian
handling is exactly what differs between architectures).
Expected: ALL GREEN, with a `test result:` line from every host. An empty or
error-only section from a remote is a FAILURE, not a pass — see the
`rt-verify-milkv-silent-green` memory.

- [ ] **Step 8: Commit**

```bash
git add crates/rt-handoff/src/bin/regen_fixtures.rs crates/rt-handoff/tests/golden.rs crates/rt-handoff/tests/fixtures
git commit -m "test(handoff): golden corpus pinning wire v1 against every future build"
```

---

## Definition of done for phase 1

- `cargo test -p rt-handoff` is green, and `ci/verify.sh` is green on x86-64 and riscv64.
- `crates/rt-handoff/Cargo.toml` still has empty `[dependencies]` and `[dev-dependencies]`.
- The golden corpus has been demonstrated to fail when the format changes (Task 12, Step 6).
- No socket, fd, engine or GUI code exists in the crate. Those are phase 2 and 3.
- `project-map.js` gains an `rt-handoff` node with status `active` and `project.updated` set to the day phase 1 lands — per the standing order in `CLAUDE.md`. Do this in the final commit of the phase, not before: the map tracks what shipped.
