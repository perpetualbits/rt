//! # vt-parser — rt's in-house VT/ANSI parser
//!
//! A clean-room implementation of [Paul Williams' ANSI parser state machine], the
//! byte→action layer of the in-house engine (`docs/own-engine-plan.md`, Phase 2, and
//! `docs/vt-parser-design.md`). It turns a raw byte stream into a sequence of
//! [`Perform`] actions (`print`/`execute`/CSI/ESC/OSC/DCS) and assigns them no
//! meaning — that is the Term's job.
//!
//! **Correctness contract:** it must produce the *same action stream* as the vendored
//! `vte` for any input, verified by differential testing (`vt-conformance`'s
//! `parser.rs`). Where the spec leaves a choice, we make vte's choice deliberately so
//! the streams match: UTF-8 input support, OSC terminated by BEL (0x07) as well as ST,
//! C1 controls executed on lone-high-byte input, and printable runs batched into
//! [`Perform::print_str`] once they reach [`BATCH_MIN`](Parser::BATCH_MIN).
//!
//! **Speed:** the ground state is left only by ESC, so it uses a `memchr` SIMD scan to
//! the next ESC and hands the whole printable run over in one `print_str` — the two
//! wins a naive per-byte parser would lose. See `docs/vt-parser-design.md`.
//!
//! [Paul Williams' ANSI parser state machine]: https://vt100.net/emu/dec_ansi_parser

use core::str;

const MAX_INTERMEDIATES: usize = 2;
const MAX_PARAMS: usize = 32;
const MAX_OSC_PARAMS: usize = 16;
/// Max raw bytes retained for one OSC string. Without a cap, a child that starts an OSC
/// (`ESC ] …`) and never terminates it grows `osc_raw` until memory exhaustion — a trivial
/// DoS over any untrusted stream (ssh, `cat` of a hostile file). 64 KiB is far more than any
/// real title or (ignored) OSC-52 clipboard payload; overflow bytes are dropped until the
/// terminator. The vendored vte oracle is capped to the same value so the differential still
/// agrees byte-for-byte. [review RT-SEC-001]
const OSC_RAW_MAX: usize = 64 * 1024;

/// The CSI/DCS parameter list: a sequence of parameters, each of which may carry
/// colon-separated sub-parameters (e.g. `38:2:255:0:0`). Mirrors vte's iteration
/// exactly — `iter()` yields one `&[u16]` per parameter — so the two agree on the
/// nested structure a `csi_dispatch` handler sees.
///
/// Storage is **allocation-free**: a flat `[u16; 32]` of values plus a `[u8; 32]` that,
/// at each parameter's start index, records how many values belong to that parameter
/// (itself + its sub-parameters). The alternative — a `Vec<Vec<u16>>` — heap-allocated
/// on every escape sequence and made the parser ~30–45% slower than vte on CSI-heavy
/// workloads (measured on riscv64/milkv; see `examples/parser_bench.rs`). This layout
/// is what closed that gap.
#[derive(Clone, Debug)]
pub struct Params {
    /// Number of values in each parameter, stored at that parameter's start index
    /// (0 at sub-parameter positions).
    subparams: [u8; MAX_PARAMS],
    /// All parameter and sub-parameter values, packed.
    params: [u16; MAX_PARAMS],
    /// Values in the parameter currently being built.
    current_subparams: u8,
    /// Total values stored (parameters + sub-parameters).
    len: usize,
}

impl Default for Params {
    fn default() -> Self {
        Params { subparams: [0; MAX_PARAMS], params: [0; MAX_PARAMS], current_subparams: 0, len: 0 }
    }
}

impl Params {
    #[inline]
    fn clear(&mut self) {
        self.current_subparams = 0;
        self.len = 0;
    }
    #[inline]
    fn is_full(&self) -> bool {
        self.len == MAX_PARAMS
    }
    /// Finalise `item` as a new parameter (the `;` separator, and the final byte).
    #[inline]
    fn push(&mut self, item: u16) {
        self.subparams[self.len - self.current_subparams as usize] = self.current_subparams + 1;
        self.params[self.len] = item;
        self.current_subparams = 0;
        self.len += 1;
    }
    /// Add `item` as a sub-parameter of the current parameter (the `:` separator).
    #[inline]
    fn extend(&mut self, item: u16) {
        self.subparams[self.len - self.current_subparams as usize] = self.current_subparams + 1;
        self.params[self.len] = item;
        self.current_subparams += 1;
        self.len += 1;
    }
    /// Number of values (parameters plus sub-parameters). Matches vte's `len`.
    pub fn len(&self) -> usize {
        self.len
    }
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }
    /// Iterate the parameters; each item is that parameter's sub-parameter slice.
    pub fn iter(&self) -> ParamsIter<'_> {
        ParamsIter { params: self, index: 0 }
    }
}

/// Iterator over [`Params`]: yields one `&[u16]` per parameter (its sub-parameters).
pub struct ParamsIter<'a> {
    params: &'a Params,
    index: usize,
}

impl<'a> Iterator for ParamsIter<'a> {
    type Item = &'a [u16];
    #[inline]
    fn next(&mut self) -> Option<&'a [u16]> {
        if self.index >= self.params.len {
            return None;
        }
        let n = self.params.subparams[self.index] as usize;
        let slice = &self.params.params[self.index..self.index + n];
        self.index += n;
        Some(slice)
    }
}

impl<'a> IntoIterator for &'a Params {
    type Item = &'a [u16];
    type IntoIter = ParamsIter<'a>;
    fn into_iter(self) -> ParamsIter<'a> {
        self.iter()
    }
}

/// The action sink. Mirrors vte's `Perform` so the Term can consume either engine's
/// parser identically. Every method has an empty default so an implementation only
/// overrides the actions it cares about.
pub trait Perform {
    /// A single printable character.
    fn print(&mut self, _c: char) {}
    /// A run of consecutive printable characters (never any control char) — the
    /// batched form of [`print`](Self::print). The default forwards char-by-char, so
    /// overriding is optional and changes nothing observable.
    fn print_str(&mut self, s: &str) {
        for c in s.chars() {
            self.print(c);
        }
    }
    /// Execute a C0 or C1 control byte.
    fn execute(&mut self, _byte: u8) {}
    /// A DCS sequence's final byte arrived; subsequent `put`s carry its data string.
    #[inline]
    fn hook(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    /// A byte of a DCS data string.
    fn put(&mut self, _byte: u8) {}
    /// A DCS string terminated.
    fn unhook(&mut self) {}
    /// An OSC command: its `;`-separated parameters, and whether BEL (not ST) ended it.
    fn osc_dispatch(&mut self, _params: &[&[u8]], _bell_terminated: bool) {}
    /// A CSI sequence's final byte arrived.
    #[inline]
    fn csi_dispatch(&mut self, _params: &Params, _intermediates: &[u8], _ignore: bool, _action: char) {}
    /// An ESC sequence's final byte arrived.
    fn esc_dispatch(&mut self, _intermediates: &[u8], _ignore: bool, _byte: u8) {}
}

#[derive(PartialEq, Eq, Debug, Default, Copy, Clone)]
enum State {
    #[default]
    Ground,
    Escape,
    EscapeIntermediate,
    CsiEntry,
    CsiParam,
    CsiIntermediate,
    CsiIgnore,
    DcsEntry,
    DcsParam,
    DcsIntermediate,
    DcsPassthrough,
    DcsIgnore,
    OscString,
    SosPmApcString,
}

/// The parser. Feed bytes with [`advance`](Parser::advance); actions are delivered to
/// your [`Perform`]. Holds the partial state (current sequence's params/intermediates,
/// any half-received UTF-8 codepoint) so it resumes correctly across calls.
#[derive(Default)]
pub struct Parser {
    state: State,
    intermediates: [u8; MAX_INTERMEDIATES],
    intermediate_idx: usize,
    params: Params,
    param: u16, // the number currently being accumulated digit-by-digit
    ignoring: bool,
    osc_raw: Vec<u8>,
    osc_params: [(usize, usize); MAX_OSC_PARAMS],
    osc_num_params: usize,
    partial_utf8: [u8; 4],
    partial_utf8_len: usize,
    /// Synchronized-update (DECSET 2026) state. While `sync_active`, raw bytes are held in
    /// `sync_buffer` instead of being dispatched, and applied atomically when the update
    /// ends (ESU `\x1b[?2026l`, or the 2 MiB cap). `flushing` guards the buffer-replay so a
    /// buffered BSU doesn't re-enter sync. Mirrors the vendored `vte` fork so the action
    /// streams agree; a stream ending mid-update leaves its tail unapplied, exactly as the
    /// oracle does. See `docs/vt-parser-design.md`.
    sync_active: bool,
    /// True only while the sync-aware [`feed`](Parser::feed) drives the machine, so
    /// `csi_dispatch` enters sync on a live BSU there but never on the raw [`advance`]
    /// path (which stays sync-free for the parser-vs-`vte` differential and the bench).
    sync_driver: bool,
    flushing: bool,
    sync_buffer: Vec<u8>,
}

/// Begin-synchronized-update CSI (`\x1b[?2026h`).
const BSU_CSI: &[u8] = b"\x1b[?2026h";
/// End-synchronized-update CSI (`\x1b[?2026l`).
const ESU_CSI: &[u8] = b"\x1b[?2026l";
/// Max bytes buffered in one synchronized update before it is force-flushed (2 MiB),
/// matching the vendored `vte`.
const SYNC_BUFFER_SIZE: usize = 0x20_0000;

impl Parser {
    /// Minimum printable-run length dispatched via the batched [`Perform::print_str`]
    /// rather than one `print` per char. Below this the batched path's setup is not
    /// worth it, so short (control-heavy) output keeps per-char dispatch — matched to
    /// vte so the action streams agree.
    pub const BATCH_MIN: usize = 4;

    pub fn new() -> Self {
        Self::default()
    }

    #[inline]
    fn intermediates(&self) -> &[u8] {
        &self.intermediates[..self.intermediate_idx]
    }

    /// Feed `bytes` to the parser, dispatching actions to `performer`. This is the raw
    /// state machine — **sync-unaware** — so it stays the hot path measured by the bench
    /// and differentially tested against `vte`'s low-level parser. Drive a terminal
    /// through [`feed`](Self::feed) instead, which layers on synchronized updates.
    pub fn advance<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) {
        let mut i = 0;
        // Finish any codepoint split across the previous call first.
        if self.partial_utf8_len != 0 {
            i += self.advance_partial_utf8(performer, bytes);
        }
        while i != bytes.len() {
            match self.state {
                State::Ground => i += self.advance_ground(performer, &bytes[i..]),
                _ => {
                    self.change_state(performer, bytes[i]);
                    i += 1;
                }
            }
        }
    }

    /// Feed `bytes` with synchronized-update (DECSET 2026) support — the entry point a
    /// terminal uses. Identical to [`advance`](Self::advance) except that bytes arriving
    /// while an update is open (`\x1b[?2026h` … `\x1b[?2026l`) are buffered and applied
    /// atomically at the end, matching the vendored `vte` `Processor`. A stream that ends
    /// mid-update leaves its tail unapplied — exactly what the oracle does.
    pub fn feed<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) {
        self.sync_driver = true;
        let mut i = 0;
        while i != bytes.len() {
            if self.sync_active {
                i += self.advance_sync(performer, &bytes[i..]);
            } else {
                i += self.advance_until_sync(performer, &bytes[i..]);
            }
        }
    }

    /// Drive the machine like [`advance`](Self::advance) but stop as soon as a live BSU is
    /// dispatched (which sets `sync_active`), so [`feed`](Self::feed) can begin buffering.
    fn advance_until_sync<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) -> usize {
        let mut i = 0;
        if self.partial_utf8_len != 0 {
            i += self.advance_partial_utf8(performer, &bytes[i..]);
        }
        while i != bytes.len() {
            match self.state {
                State::Ground => i += self.advance_ground(performer, &bytes[i..]),
                _ => {
                    self.change_state(performer, bytes[i]);
                    i += 1;
                }
            }
            if self.sync_active {
                break; // a live \x1b[?2026h was just dispatched
            }
        }
        i
    }

    /// Dispatch `bytes` straight through the state machine with no sync interception —
    /// the buffer-replay path. `flushing` is set so a buffered BSU can't re-enter sync.
    fn dispatch_raw<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) {
        let mut i = 0;
        if self.partial_utf8_len != 0 {
            i += self.advance_partial_utf8(performer, bytes);
        }
        while i != bytes.len() {
            match self.state {
                State::Ground => i += self.advance_ground(performer, &bytes[i..]),
                _ => {
                    self.change_state(performer, bytes[i]);
                    i += 1;
                }
            }
        }
    }

    /// Buffer `bytes` during an open synchronized update, scanning for the terminating or
    /// extending escape. Returns bytes consumed. On overflow the update is force-flushed
    /// and the caller reprocesses `bytes` normally.
    #[cold]
    fn advance_sync<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) -> usize {
        if self.sync_buffer.len() + bytes.len() >= SYNC_BUFFER_SIZE - 1 {
            self.stop_sync(performer, None); // sync_active := false; caller reprocesses
            return 0;
        }
        self.sync_buffer.extend_from_slice(bytes);
        self.scan_sync_csi(performer, bytes.len());
        bytes.len()
    }

    /// Search the just-added region (plus the 7-byte overlap for split escapes) for the
    /// BSU/ESU escapes, in reverse. A later BSU extends the update (its tail is kept
    /// buffered); the first ESU found terminates it, flushing everything before that BSU.
    fn scan_sync_csi<P: Perform>(&mut self, performer: &mut P, new_bytes: usize) {
        let len = self.sync_buffer.len();
        let start = (len - new_bytes).saturating_sub(BSU_CSI.len() - 1);
        let end = len.saturating_sub(BSU_CSI.len() - 1);
        let mut bsu_offset = None;
        let mut off = end;
        while off > start {
            off -= 1;
            if self.sync_buffer[off] != 0x1B {
                continue;
            }
            let escape = &self.sync_buffer[off..off + BSU_CSI.len()];
            if escape == BSU_CSI {
                bsu_offset = Some(off);
            } else if escape == ESU_CSI {
                self.stop_sync(performer, bsu_offset);
                break;
            }
        }
    }

    /// End (or trim) the synchronized update: replay the buffered bytes up to `bsu_offset`
    /// (or the whole buffer on a plain ESU) through the parser, then either exit sync or,
    /// if a later BSU extended it, keep that BSU's tail buffered and stay in sync.
    fn stop_sync<P: Perform>(&mut self, performer: &mut P, bsu_offset: Option<usize>) {
        let buffer = std::mem::take(&mut self.sync_buffer);
        let offset = bsu_offset.unwrap_or(buffer.len());
        self.flushing = true;
        self.dispatch_raw(performer, &buffer[..offset]);
        self.flushing = false;
        self.sync_buffer = buffer;
        match bsu_offset {
            Some(off) => {
                let new_len = self.sync_buffer.len() - off;
                self.sync_buffer.copy_within(off.., 0);
                self.sync_buffer.truncate(new_len);
            }
            None => {
                self.sync_active = false;
                self.sync_buffer.clear();
            }
        }
    }

    /// Is the CSI currently being dispatched exactly `\x1b[?2026h`/`l`? Returns `Some(true)`
    /// for BSU (begin), `Some(false)` for ESU (end), else `None`.
    fn sync_csi_kind(&self, action: u8) -> Option<bool> {
        if self.intermediates() != b"?" {
            return None;
        }
        let mut it = self.params.iter();
        match (it.next(), it.next()) {
            (Some([2026]), None) => match action {
                b'h' => Some(true),
                b'l' => Some(false),
                _ => None,
            },
            _ => None,
        }
    }

    #[inline]
    fn change_state<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match self.state {
            State::Ground => unreachable!("ground is handled by advance_ground"),
            State::Escape => self.advance_esc(performer, byte),
            State::EscapeIntermediate => self.advance_esc_intermediate(performer, byte),
            State::CsiEntry => self.advance_csi_entry(performer, byte),
            State::CsiParam => self.advance_csi_param(performer, byte),
            State::CsiIntermediate => self.advance_csi_intermediate(performer, byte),
            State::CsiIgnore => self.advance_csi_ignore(performer, byte),
            State::DcsEntry => self.advance_dcs_entry(performer, byte),
            State::DcsParam => self.advance_dcs_param(performer, byte),
            State::DcsIntermediate => self.advance_dcs_intermediate(performer, byte),
            State::DcsPassthrough => self.advance_dcs_passthrough(performer, byte),
            State::DcsIgnore => self.anywhere(byte),
            State::OscString => self.advance_osc_string(performer, byte),
            State::SosPmApcString => self.anywhere(byte),
        }
    }

    // ── C0/C1 "anywhere" transitions shared by many states ────────────────────
    #[inline]
    fn anywhere(&mut self, byte: u8) {
        match byte {
            0x18 | 0x1A => self.state = State::Ground,
            0x1B => {
                self.reset_params();
                self.state = State::Escape;
            }
            _ => {}
        }
    }

    // ── Escape ────────────────────────────────────────────────────────────────
    #[inline]
    fn advance_esc<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => {
                self.collect(byte);
                self.state = State::EscapeIntermediate;
            }
            0x30..=0x4F | 0x51..=0x57 | 0x59..=0x5A | 0x5C | 0x60..=0x7E => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground;
            }
            0x50 => {
                self.reset_params();
                self.state = State::DcsEntry;
            }
            0x58 | 0x5E..=0x5F => self.state = State::SosPmApcString,
            0x5B => {
                self.reset_params();
                self.state = State::CsiEntry;
            }
            0x5D => {
                self.osc_raw.clear();
                self.osc_num_params = 0;
                self.state = State::OscString;
            }
            0x18 | 0x1A => {
                performer.execute(byte);
                self.state = State::Ground;
            }
            _ => {}
        }
    }

    #[inline]
    fn advance_esc_intermediate<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => self.collect(byte),
            0x30..=0x7E => {
                performer.esc_dispatch(self.intermediates(), self.ignoring, byte);
                self.state = State::Ground;
            }
            0x7F => {}
            _ => self.anywhere(byte),
        }
    }

    // ── CSI ───────────────────────────────────────────────────────────────────
    #[inline]
    fn advance_csi_entry<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => {
                self.collect(byte);
                self.state = State::CsiIntermediate;
            }
            0x30..=0x39 => {
                self.paramnext(byte);
                self.state = State::CsiParam;
            }
            0x3A => {
                self.subparam();
                self.state = State::CsiParam;
            }
            0x3B => {
                self.param();
                self.state = State::CsiParam;
            }
            0x3C..=0x3F => {
                self.collect(byte);
                self.state = State::CsiParam;
            }
            0x40..=0x7E => self.csi_dispatch(performer, byte),
            _ => self.anywhere(byte),
        }
    }

    #[inline]
    fn advance_csi_param<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => {
                self.collect(byte);
                self.state = State::CsiIntermediate;
            }
            0x30..=0x39 => self.paramnext(byte),
            0x3A => self.subparam(),
            0x3B => self.param(),
            0x3C..=0x3F => self.state = State::CsiIgnore,
            0x40..=0x7E => self.csi_dispatch(performer, byte),
            0x7F => {}
            _ => self.anywhere(byte),
        }
    }

    #[inline]
    fn advance_csi_intermediate<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x2F => self.collect(byte),
            0x30..=0x3F => self.state = State::CsiIgnore,
            0x40..=0x7E => self.csi_dispatch(performer, byte),
            _ => self.anywhere(byte),
        }
    }

    #[inline]
    fn advance_csi_ignore<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => performer.execute(byte),
            0x20..=0x3F => {}
            0x40..=0x7E => self.state = State::Ground,
            0x7F => {}
            _ => self.anywhere(byte),
        }
    }

    // ── DCS ───────────────────────────────────────────────────────────────────
    #[inline]
    fn advance_dcs_entry<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => {}
            0x20..=0x2F => {
                self.collect(byte);
                self.state = State::DcsIntermediate;
            }
            0x30..=0x39 => {
                self.paramnext(byte);
                self.state = State::DcsParam;
            }
            0x3A => {
                self.subparam();
                self.state = State::DcsParam;
            }
            0x3B => {
                self.param();
                self.state = State::DcsParam;
            }
            0x3C..=0x3F => {
                self.collect(byte);
                self.state = State::DcsParam;
            }
            0x40..=0x7E => self.hook(performer, byte),
            0x7F => {}
            _ => self.anywhere(byte),
        }
    }

    #[inline]
    fn advance_dcs_param<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => {}
            0x20..=0x2F => {
                self.collect(byte);
                self.state = State::DcsIntermediate;
            }
            0x30..=0x39 => self.paramnext(byte),
            0x3A => self.subparam(),
            0x3B => self.param(),
            0x3C..=0x3F => self.state = State::DcsIgnore,
            0x40..=0x7E => self.hook(performer, byte),
            0x7F => {}
            _ => self.anywhere(byte),
        }
    }

    #[inline]
    fn advance_dcs_intermediate<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x1F => {}
            0x20..=0x2F => self.collect(byte),
            0x30..=0x3F => self.state = State::DcsIgnore,
            0x40..=0x7E => self.hook(performer, byte),
            0x7F => {}
            _ => self.anywhere(byte),
        }
    }

    #[inline]
    fn advance_dcs_passthrough<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x17 | 0x19 | 0x1C..=0x7E => performer.put(byte),
            0x18 | 0x1A => {
                performer.unhook();
                performer.execute(byte);
                self.state = State::Ground;
            }
            0x1B => {
                performer.unhook();
                self.reset_params();
                self.state = State::Escape;
            }
            0x7F => {}
            0x9C => {
                performer.unhook();
                self.state = State::Ground;
            }
            _ => {}
        }
    }

    // ── OSC ───────────────────────────────────────────────────────────────────
    #[inline]
    fn advance_osc_string<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        match byte {
            0x00..=0x06 | 0x08..=0x17 | 0x19 | 0x1C..=0x1F => {}
            0x07 => {
                self.osc_end(performer, byte);
                self.state = State::Ground;
            }
            0x18 | 0x1A => {
                self.osc_end(performer, byte);
                performer.execute(byte);
                self.state = State::Ground;
            }
            0x1B => {
                self.osc_end(performer, byte);
                self.reset_params();
                self.state = State::Escape;
            }
            0x3B => self.osc_put_param(),
            // Drop overflow bytes past the cap (keep parsing so the terminator still fires
            // and the state machine recovers) — bounds an unterminated OSC. [RT-SEC-001]
            _ if self.osc_raw.len() >= OSC_RAW_MAX => {}
            _ => self.osc_raw.push(byte),
        }
    }

    // ── Actions ───────────────────────────────────────────────────────────────
    #[inline]
    fn csi_dispatch<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            self.params.push(self.param);
        }
        performer.csi_dispatch(&self.params, self.intermediates(), self.ignoring, byte as char);
        // Enter a synchronized update on a live `\x1b[?2026h` — only on the sync-aware
        // `feed` path (`sync_driver`), never while replaying a buffered update
        // (`flushing`) or on the raw `advance` path. The feed loop then buffers the rest.
        if self.sync_driver && !self.flushing && self.sync_csi_kind(byte) == Some(true) {
            self.sync_active = true;
            self.sync_buffer.clear();
        }
        self.state = State::Ground;
    }

    #[inline]
    fn hook<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            self.params.push(self.param);
        }
        performer.hook(&self.params, self.intermediates(), self.ignoring, byte as char);
        self.state = State::DcsPassthrough;
    }

    #[inline]
    fn collect(&mut self, byte: u8) {
        if self.intermediate_idx == MAX_INTERMEDIATES {
            self.ignoring = true;
        } else {
            self.intermediates[self.intermediate_idx] = byte;
            self.intermediate_idx += 1;
        }
    }

    #[inline]
    fn subparam(&mut self) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            self.params.extend(self.param);
            self.param = 0;
        }
    }

    #[inline]
    fn param(&mut self) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            self.params.push(self.param);
            self.param = 0;
        }
    }

    #[inline]
    fn paramnext(&mut self, byte: u8) {
        if self.params.is_full() {
            self.ignoring = true;
        } else {
            self.param = self.param.saturating_mul(10).saturating_add((byte - b'0') as u16);
        }
    }

    #[inline]
    fn osc_put_param(&mut self) {
        let idx = self.osc_raw.len();
        let n = self.osc_num_params;
        match n {
            0 => self.osc_params[0] = (0, idx),
            MAX_OSC_PARAMS => return,
            _ => {
                let begin = self.osc_params[n - 1].1;
                self.osc_params[n] = (begin, idx);
            }
        }
        self.osc_num_params += 1;
    }

    #[inline]
    fn osc_end<P: Perform>(&mut self, performer: &mut P, byte: u8) {
        self.osc_put_param();
        // Build the parameter slices into the raw buffer on the stack (no allocation)
        // and dispatch.
        let mut slices: [&[u8]; MAX_OSC_PARAMS] = [&[]; MAX_OSC_PARAMS];
        for i in 0..self.osc_num_params {
            let (a, b) = self.osc_params[i];
            slices[i] = &self.osc_raw[a..b];
        }
        performer.osc_dispatch(&slices[..self.osc_num_params], byte == 0x07);
        self.osc_raw.clear();
        self.osc_num_params = 0;
    }

    #[inline]
    fn reset_params(&mut self) {
        self.intermediate_idx = 0;
        self.ignoring = false;
        self.param = 0;
        self.params.clear();
    }

    // ── Ground: the SIMD fast path + UTF-8 ────────────────────────────────────
    #[inline]
    fn advance_ground<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) -> usize {
        let num_bytes = bytes.len();
        let plain = memchr::memchr(0x1B, bytes).unwrap_or(num_bytes);
        if plain == 0 {
            // The very next byte is ESC: switch and consume it.
            self.reset_params();
            self.state = State::Escape;
            return 1;
        }
        match str::from_utf8(&bytes[..plain]) {
            Ok(text) => {
                Self::ground_dispatch(performer, text);
                let mut processed = plain;
                if processed < num_bytes {
                    // The byte at `plain` is ESC.
                    self.reset_params();
                    self.state = State::Escape;
                    processed += 1;
                }
                processed
            }
            Err(err) => {
                let valid = err.valid_up_to();
                let text = unsafe { str::from_utf8_unchecked(&bytes[..valid]) };
                Self::ground_dispatch(performer, text);
                match err.error_len() {
                    Some(len) => {
                        // Invalid sequence: a lone C1 byte executes, else replacement char.
                        if len == 1 && bytes[valid] <= 0x9F {
                            performer.execute(bytes[valid]);
                        } else {
                            performer.print('\u{FFFD}');
                        }
                        valid + len
                    }
                    None => {
                        if plain < num_bytes {
                            // Truncated by an escape mid-codepoint: emit replacement, take ESC.
                            performer.print('\u{FFFD}');
                            self.reset_params();
                            self.state = State::Escape;
                            plain + 1
                        } else {
                            // Truncated by the buffer end: stash the partial codepoint.
                            let extra = num_bytes - valid;
                            let end = self.partial_utf8_len + extra;
                            self.partial_utf8[self.partial_utf8_len..end]
                                .copy_from_slice(&bytes[valid..valid + extra]);
                            self.partial_utf8_len = end;
                            num_bytes
                        }
                    }
                }
            }
        }
    }

    #[inline]
    fn advance_partial_utf8<P: Perform>(&mut self, performer: &mut P, bytes: &[u8]) -> usize {
        let old = self.partial_utf8_len;
        let to_copy = bytes.len().min(self.partial_utf8.len() - old);
        self.partial_utf8[old..old + to_copy].copy_from_slice(&bytes[..to_copy]);
        self.partial_utf8_len += to_copy;

        match str::from_utf8(&self.partial_utf8[..self.partial_utf8_len]) {
            Ok(parsed) => {
                let c = parsed.chars().next().unwrap();
                performer.print(c);
                self.partial_utf8_len = 0;
                c.len_utf8() - old
            }
            Err(err) => {
                let valid = err.valid_up_to();
                if valid > 0 {
                    let c = {
                        let s = unsafe { str::from_utf8_unchecked(&self.partial_utf8[..valid]) };
                        s.chars().next().unwrap()
                    };
                    performer.print(c);
                    self.partial_utf8_len = 0;
                    return valid - old;
                }
                match err.error_len() {
                    Some(invalid_len) => {
                        performer.print('\u{FFFD}');
                        self.partial_utf8_len = 0;
                        invalid_len - old
                    }
                    None => to_copy,
                }
            }
        }
    }

    /// Split a printable run at control characters (C0 and C1), executing each and
    /// batching the printable stretches. Fast path: printable ASCII bytes
    /// (`0x20..=0x7F`) advance with a plain byte compare — no per-char UTF-8 decode,
    /// since a validated `str` cannot hold a stray byte there. Only `< 0x20` (a C0
    /// control) or `>= 0x80` (a multibyte codepoint, possibly a C1 control) needs a
    /// closer look. Produces the exact print/execute order and run boundaries vte does
    /// (the differential test proves it), while skipping char construction for the
    /// overwhelmingly common printable-ASCII byte — the plain-text throughput win.
    #[inline]
    fn ground_dispatch<P: Perform>(performer: &mut P, text: &str) {
        let bytes = text.as_bytes();
        let mut run_start = 0;
        let mut i = 0;
        while i < bytes.len() {
            let b = bytes[i];
            if b < 0x20 {
                // C0 control: a single byte.
                Self::flush_run(performer, &text[run_start..i]);
                performer.execute(b);
                i += 1;
                run_start = i;
            } else if b < 0x80 {
                // Printable ASCII: stays in the run, no decode.
                i += 1;
            } else {
                // Multibyte codepoint: decode to distinguish a printable char from a
                // C1 control (U+0080..=U+009F).
                let c = text[i..].chars().next().unwrap();
                let len = c.len_utf8();
                if ('\u{80}'..='\u{9f}').contains(&c) {
                    Self::flush_run(performer, &text[run_start..i]);
                    performer.execute(c as u8);
                    i += len;
                    run_start = i;
                } else {
                    i += len;
                }
            }
        }
        Self::flush_run(performer, &text[run_start..]);
    }

    #[inline]
    fn flush_run<P: Perform>(performer: &mut P, run: &str) {
        if run.len() >= Self::BATCH_MIN {
            performer.print_str(run);
        } else {
            for c in run.chars() {
                performer.print(c);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Default, Debug, PartialEq)]
    enum Ev {
        #[default]
        None,
        Print(char),
        PrintStr(String),
        Execute(u8),
        Csi(Vec<Vec<u16>>, Vec<u8>, char),
        Esc(Vec<u8>, u8),
        Osc(Vec<Vec<u8>>, bool),
    }

    #[derive(Default)]
    struct Log(Vec<Ev>);
    impl Perform for Log {
        fn print(&mut self, c: char) {
            self.0.push(Ev::Print(c));
        }
        fn print_str(&mut self, s: &str) {
            self.0.push(Ev::PrintStr(s.to_string()));
        }
        fn execute(&mut self, b: u8) {
            self.0.push(Ev::Execute(b));
        }
        fn csi_dispatch(&mut self, p: &Params, i: &[u8], _ig: bool, a: char) {
            self.0.push(Ev::Csi(p.iter().map(|s| s.to_vec()).collect(), i.to_vec(), a));
        }
        fn esc_dispatch(&mut self, i: &[u8], _ig: bool, b: u8) {
            self.0.push(Ev::Esc(i.to_vec(), b));
        }
        fn osc_dispatch(&mut self, p: &[&[u8]], bell: bool) {
            self.0.push(Ev::Osc(p.iter().map(|s| s.to_vec()).collect(), bell));
        }
    }

    fn run(bytes: &[u8]) -> Vec<Ev> {
        let mut p = Parser::new();
        let mut l = Log::default();
        p.advance(&mut l, bytes);
        l.0
    }

    #[test]
    fn printable_run_batches_over_threshold() {
        // 5 chars ≥ BATCH_MIN → one print_str; a 2-char run stays per-char.
        assert_eq!(run(b"hello"), vec![Ev::PrintStr("hello".into())]);
        assert_eq!(run(b"hi"), vec![Ev::Print('h'), Ev::Print('i')]);
    }

    #[test]
    fn control_splits_runs_and_executes() {
        assert_eq!(
            run(b"ab\ncd"),
            vec![Ev::Print('a'), Ev::Print('b'), Ev::Execute(b'\n'), Ev::Print('c'), Ev::Print('d')]
        );
    }

    #[test]
    fn csi_params_subparams_and_intermediates() {
        assert_eq!(run(b"\x1b[1;2m"), vec![Ev::Csi(vec![vec![1], vec![2]], vec![], 'm')]);
        assert_eq!(run(b"\x1b[38:2:1:2:3m"), vec![Ev::Csi(vec![vec![38, 2, 1, 2, 3]], vec![], 'm')]);
        assert_eq!(run(b"\x1b[?25h"), vec![Ev::Csi(vec![vec![25]], vec![b'?'], 'h')]);
        // Empty CSI has one implicit zero parameter.
        assert_eq!(run(b"\x1b[m"), vec![Ev::Csi(vec![vec![0]], vec![], 'm')]);
    }

    #[test]
    fn esc_and_osc() {
        assert_eq!(run(b"\x1b(B"), vec![Ev::Esc(vec![b'('], b'B')]);
        assert_eq!(run(b"\x1b]0;title\x07"),
                   vec![Ev::Osc(vec![b"0".to_vec(), b"title".to_vec()], true)]);
    }

    #[test]
    fn utf8_split_across_advance_calls() {
        // "🦀" is 4 bytes; feeding it one byte at a time must still print exactly once.
        let crab = "🦀".as_bytes();
        let mut p = Parser::new();
        let mut l = Log::default();
        for b in crab {
            p.advance(&mut l, &[*b]);
        }
        assert_eq!(l.0, vec![Ev::Print('🦀')]);
    }

    #[test]
    fn oversized_osc_is_capped_and_the_parser_recovers() {
        // ESC ] 2 ; then well past the cap with no terminator, then BEL, then a normal char.
        let mut bytes = b"\x1b]2;".to_vec();
        bytes.extend(std::iter::repeat(b'x').take(OSC_RAW_MAX + 50_000));
        bytes.push(0x07); // BEL terminates the OSC
        bytes.push(b'A'); // must parse normally afterward
        let evs = run(&bytes);
        // The OSC still dispatched, with a payload bounded by the cap (not the 64K+50K sent).
        let osc = evs
            .iter()
            .find_map(|e| if let Ev::Osc(p, _) = e { Some(p) } else { None })
            .expect("OSC should still dispatch after overflow");
        assert!(
            osc.iter().map(|p| p.len()).sum::<usize>() <= OSC_RAW_MAX,
            "OSC payload exceeds the {OSC_RAW_MAX}-byte cap"
        );
        // The trailing 'A' parsed normally — the state machine fully recovered.
        assert!(
            evs.iter().any(|e| matches!(e, Ev::Print('A'))),
            "parser did not recover after an oversized OSC"
        );
    }
}

