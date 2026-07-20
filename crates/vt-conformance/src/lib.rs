//! # vt-conformance — the terminal-engine differential-testing harness
//!
//! The verification backbone of the own-engine program (`docs/own-engine-plan.md`,
//! Phase 1). Its job is to answer one question mechanically, for millions of inputs:
//! *does engine X produce the same observable terminal state as the oracle?*
//!
//! It has three parts:
//!
//! 1. [`ScreenState`] — a **neutral**, fully-materialised, comparable snapshot of a
//!    terminal (grid of cells + cursor + a few modes). Neutral means it names no
//!    engine's types, so the vendored oracle and a future in-house engine translate
//!    *into* it and are compared apples-to-apples.
//! 2. [`VtEngine`] — the tiny interface every engine-under-test implements: spawn at a
//!    size, [`feed`](VtEngine::feed) bytes, [`resize`](VtEngine::resize),
//!    [`observe`](VtEngine::observe). The vendored oracle ([`vendored::Vendored`]) is
//!    the first implementation; `vt-term` becomes the second.
//! 3. The **comparators + generator** ([`feed_whole`], [`feed_chunks`], [`gen_script`],
//!    [`split`]) that drive two engines on identical input and diff them.
//!
//! Until the in-house engine exists, the "system under test" slot is filled by the
//! oracle itself, so the battery is green from day one — and it already finds real
//! bugs via the **chunk-invariance** property (feeding the same bytes in any split
//! must yield identical state), which any correct engine — vendored or ours — must
//! satisfy.
//!
//! The oracle is driven **synchronously** (no PTY, no shell): a `Term` plus the ANSI
//! `Processor`, fed straight from a byte slice — deterministic and reproducible,
//! exactly how alacritty's own tests drive it.

/// Neutral attribute bits (stable across engines). A cell's `attrs` is an OR of these.
pub mod attr {
    pub const BOLD: u16 = 1 << 0;
    pub const ITALIC: u16 = 1 << 1;
    pub const UNDERLINE: u16 = 1 << 2;
    pub const INVERSE: u16 = 1 << 3;
    pub const DIM: u16 = 1 << 4;
    pub const HIDDEN: u16 = 1 << 5;
    pub const STRIKEOUT: u16 = 1 << 6;
    pub const WIDE: u16 = 1 << 7;
}

/// A colour in engine-neutral terms: a named/palette index or a direct RGB triple.
/// (`Named` carries the engine's named-colour discriminant; identical engines agree
/// on it, which is all differential testing needs.)
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NColor {
    Named(u16),
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// One materialised grid cell: the character, its resolved fg/bg, and its attributes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NCell {
    pub ch: char,
    pub fg: NColor,
    pub bg: NColor,
    pub attrs: u16,
}

/// The cursor as observed: position, a neutral shape code, and whether it is shown.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct NCursor {
    pub col: usize,
    pub line: usize,
    pub shape: u8, // 0 block, 1 underline, 2 beam, 3 hollow, 4 hidden
    pub visible: bool,
}

/// A complete, comparable snapshot of a terminal's observable state after some input.
/// Two engines fed the same bytes must produce equal `ScreenState`s; [`diff`] reports
/// the first difference in human-readable form for test output.
///
/// [`diff`]: ScreenState::diff
#[derive(Clone, PartialEq, Eq)]
pub struct ScreenState {
    pub cols: usize,
    pub rows: usize,
    pub grid: Vec<Vec<NCell>>,
    pub cursor: Option<NCursor>,
    pub alt_screen: bool,
    pub app_cursor: bool,
    pub display_offset: usize,
    pub history: usize,
}

impl ScreenState {
    /// The first way `self` differs from `other`, as a readable string, or `None` if
    /// they are identical. Ordered coarse→fine (dims, then cells row-major, then
    /// cursor/modes) so the report points at the earliest meaningful divergence.
    pub fn diff(&self, other: &Self) -> Option<String> {
        if (self.cols, self.rows) != (other.cols, other.rows) {
            return Some(format!(
                "dimensions {}x{} vs {}x{}",
                self.cols, self.rows, other.cols, other.rows
            ));
        }
        for r in 0..self.rows {
            for c in 0..self.cols {
                let (a, b) = (self.grid[r][c], other.grid[r][c]);
                if a != b {
                    return Some(format!("cell (row {r}, col {c}): {a:?} vs {b:?}"));
                }
            }
        }
        if self.cursor != other.cursor {
            return Some(format!("cursor {:?} vs {:?}", self.cursor, other.cursor));
        }
        if self.alt_screen != other.alt_screen {
            return Some(format!("alt_screen {} vs {}", self.alt_screen, other.alt_screen));
        }
        if self.app_cursor != other.app_cursor {
            return Some(format!("app_cursor {} vs {}", self.app_cursor, other.app_cursor));
        }
        if self.display_offset != other.display_offset {
            return Some(format!("display_offset {} vs {}", self.display_offset, other.display_offset));
        }
        if self.history != other.history {
            return Some(format!("history {} vs {}", self.history, other.history));
        }
        None
    }

    /// The visible grid as text (one string per row, trailing blanks trimmed), for
    /// eyeballing a failing case.
    pub fn to_text(&self) -> String {
        self.grid
            .iter()
            .map(|row| row.iter().map(|c| c.ch).collect::<String>().trim_end().to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }
}

/// The interface every engine-under-test implements. Deliberately tiny: the whole
/// point is that the oracle and the in-house engine present the *same* surface so the
/// harness can drive them interchangeably.
pub trait VtEngine {
    /// A fresh terminal of `cols`×`rows`, empty, scrolled to the bottom.
    fn spawn(cols: usize, rows: usize) -> Self
    where
        Self: Sized;
    /// Feed a chunk of raw bytes (may split a sequence mid-way; the engine must resume).
    fn feed(&mut self, bytes: &[u8]);
    /// Resize the grid.
    fn resize(&mut self, cols: usize, rows: usize);
    /// Materialise the current observable state.
    fn observe(&self) -> ScreenState;
    /// Human name, for diagnostics.
    fn name() -> &'static str
    where
        Self: Sized;
}

/// Spawn engine `E`, feed the whole script at once, and observe.
pub fn feed_whole<E: VtEngine>(cols: usize, rows: usize, script: &[u8]) -> ScreenState {
    let mut e = E::spawn(cols, rows);
    e.feed(script);
    e.observe()
}

/// Spawn engine `E`, feed the script as the given chunks, and observe. Used to prove
/// chunk-invariance (identical to [`feed_whole`] for a correct engine).
pub fn feed_chunks<E: VtEngine>(cols: usize, rows: usize, chunks: &[&[u8]]) -> ScreenState {
    let mut e = E::spawn(cols, rows);
    for c in chunks {
        e.feed(c);
    }
    e.observe()
}

/// A tiny deterministic xorshift64 PRNG — no external deps, so a failing seed always
/// reproduces exactly. Seed it, print the seed on failure, replay.
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
    /// A value in `0..n` (n > 0).
    pub fn below(&mut self, n: u32) -> u32 {
        (self.next_u64() % n as u64) as u32
    }
}

/// Generate a pseudo-random but structurally-plausible terminal byte script from a
/// seed: runs of printable text interleaved with common control/CSI sequences (cursor
/// moves, SGR, erase, CUP, alt-screen toggles). `tokens` is roughly how many pieces to
/// emit. Deterministic in `seed`.
pub fn gen_script(seed: u64, tokens: usize) -> Vec<u8> {
    let mut r = Rng::new(seed);
    let mut out = Vec::new();
    for _ in 0..tokens {
        match r.below(10) {
            0..=4 => {
                // a run of printable letters
                let n = 1 + r.below(8);
                for _ in 0..n {
                    out.push(b'A' + r.below(26) as u8);
                }
            }
            5 => out.push(b'\n'),
            6 => out.push(b'\r'),
            7 => {
                // relative cursor move
                out.extend_from_slice(b"\x1b[");
                out.extend_from_slice(format!("{}", 1 + r.below(20)).as_bytes());
                out.push(match r.below(4) {
                    0 => b'A',
                    1 => b'B',
                    2 => b'C',
                    _ => b'D',
                });
            }
            8 => {
                // SGR attribute
                out.extend_from_slice(b"\x1b[");
                out.extend_from_slice(format!("{}", r.below(8)).as_bytes());
                out.push(b'm');
            }
            _ => match r.below(5) {
                0 => out.extend_from_slice(b"\x1b[2J"), // erase screen
                1 => out.extend_from_slice(b"\x1b[H"),  // home
                2 => out.extend_from_slice(b"\x1b[K"),  // erase line
                3 => out.extend_from_slice(if r.below(2) == 0 { b"\x1b[?1049h" } else { b"\x1b[?1049l" }),
                _ => {
                    // absolute cursor position
                    out.extend_from_slice(b"\x1b[");
                    out.extend_from_slice(format!("{};{}", 1 + r.below(24), 1 + r.below(80)).as_bytes());
                    out.push(b'H');
                }
            },
        }
    }
    out
}

/// Split a script into a deterministic sequence of small chunks (1..=7 bytes), so a
/// test can feed the same bytes in a different framing than [`feed_whole`] and check
/// the engine's state is unchanged (the chunk-invariance property).
pub fn split(script: &[u8], seed: u64) -> Vec<&[u8]> {
    let mut r = Rng::new(seed ^ 0xA5A5_5A5A);
    let mut chunks = Vec::new();
    let mut i = 0;
    while i < script.len() {
        let take = 1 + r.below(7) as usize;
        let end = (i + take).min(script.len());
        chunks.push(&script[i..end]);
        i = end;
    }
    chunks
}

pub mod vendored;
