//! The oracle: the vendored `alacritty_terminal` engine, driven synchronously (no
//! PTY, no shell) so its behaviour is deterministic and reproducible. This is the
//! reference every candidate engine is diffed against, and — via chunk-invariance —
//! a useful test subject in its own right before the in-house engine exists.

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Term, TermMode};
use alacritty_terminal::vte::ansi::{self, Color, CursorShape};

use crate::{attr, NCell, NColor, NCursor, ScreenState, VtEngine};

/// A no-op event listener. `EventListener::send_event` has a default empty body, so
/// terminal events (title, bell, query replies) are simply dropped — none of them
/// affect grid state, which is all conformance compares.
struct Noop;
impl EventListener for Noop {}

/// Grid dimensions handed to `Term::new`/`resize`.
struct Dims {
    cols: usize,
    rows: usize,
}
impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows // no history at construction; Term grows it as content scrolls off
    }
    fn screen_lines(&self) -> usize {
        self.rows
    }
    fn columns(&self) -> usize {
        self.cols
    }
}

/// The vendored engine as a [`VtEngine`]: a `Term` plus the ANSI `Processor`.
pub struct Vendored {
    term: Term<Noop>,
    parser: ansi::Processor,
}

/// Translate an alacritty cell colour to the neutral colour. Named colours 0..=15 are
/// the ANSI palette → `Indexed`; Foreground/Background (and the other specials) are the
/// terminal default → the `Named(256)` sentinel. This makes the oracle and the in-house
/// Term — which stores `Default`/`Indexed`/`Rgb` — directly comparable.
fn neutral_color(c: Color) -> NColor {
    match c {
        Color::Named(n) => {
            let i = n as u16;
            if i <= 15 {
                NColor::Indexed(i as u8)
            } else {
                NColor::Named(256) // Foreground/Background/Cursor/Dim/Bright → "default"
            }
        }
        Color::Indexed(i) => NColor::Indexed(i),
        Color::Spec(rgb) => NColor::Rgb(rgb.r, rgb.g, rgb.b),
    }
}

/// Fold alacritty cell flags into the neutral attribute bits.
fn neutral_attrs(f: Flags) -> u16 {
    let mut a = 0;
    if f.contains(Flags::BOLD) {
        a |= attr::BOLD;
    }
    if f.contains(Flags::ITALIC) {
        a |= attr::ITALIC;
    }
    if f.contains(Flags::UNDERLINE) {
        a |= attr::UNDERLINE;
    }
    if f.contains(Flags::INVERSE) {
        a |= attr::INVERSE;
    }
    if f.contains(Flags::DIM) {
        a |= attr::DIM;
    }
    if f.contains(Flags::HIDDEN) {
        a |= attr::HIDDEN;
    }
    if f.contains(Flags::STRIKEOUT) {
        a |= attr::STRIKEOUT;
    }
    if f.contains(Flags::WIDE_CHAR) {
        a |= attr::WIDE;
    }
    a
}

impl VtEngine for Vendored {
    fn spawn(cols: usize, rows: usize) -> Self {
        // Match rt-engine's configuration so the oracle behaves exactly like the
        // engine rt actually ships (same scrollback default).
        let config = Config { scrolling_history: 10_000, ..Config::default() };
        let term = Term::new(config, &Dims { cols, rows }, Noop);
        Vendored { term, parser: ansi::Processor::new() }
    }

    fn feed(&mut self, bytes: &[u8]) {
        // The synchronous parse: exactly what the vendored event loop does per read,
        // minus the PTY. Handles chunk boundaries internally (resumable parser state).
        self.parser.advance(&mut self.term, bytes);
    }

    fn resize(&mut self, cols: usize, rows: usize) {
        self.term.resize(Dims { cols, rows });
    }

    fn observe(&self) -> ScreenState {
        let term = &self.term;
        let cols = term.columns();
        let rows = term.screen_lines();
        let offset = term.grid().display_offset() as i32;

        // Every visible cell, materialised. `display_iter` yields cells labelled with
        // their absolute grid line; `+ offset` maps them onto viewport rows 0..rows.
        let blank = NCell { ch: ' ', fg: NColor::Named(0), bg: NColor::Named(0), attrs: 0 };
        let mut grid = vec![vec![blank; cols]; rows];
        for cell in term.grid().display_iter() {
            let row = cell.point.line.0 + offset;
            let col = cell.point.column.0;
            if row >= 0 && (row as usize) < rows && col < cols {
                grid[row as usize][col] = NCell {
                    ch: cell.c,
                    fg: neutral_color(cell.fg),
                    bg: neutral_color(cell.bg),
                    attrs: neutral_attrs(cell.flags),
                };
            }
        }

        // Cursor: reported only when shown and not scrolled back into history (a
        // scrolled-back cursor is off-screen), mirroring the renderer's own rule.
        let cursor = if term.mode().contains(TermMode::SHOW_CURSOR) && term.grid().display_offset() == 0 {
            let shape = match term.cursor_style().shape {
                CursorShape::Block => 0,
                CursorShape::Underline => 1,
                CursorShape::Beam => 2,
                CursorShape::HollowBlock => 3,
                CursorShape::Hidden => 4,
            };
            let p = term.grid().cursor.point;
            let line = p.line.0.max(0) as usize;
            let col = p.column.0;
            Some(NCursor { col, line, shape, visible: shape != 4 })
        } else {
            None
        };

        ScreenState {
            cols,
            rows,
            grid,
            cursor,
            alt_screen: term.mode().contains(TermMode::ALT_SCREEN),
            app_cursor: term.mode().contains(TermMode::APP_CURSOR),
            display_offset: term.grid().display_offset(),
            history: term.history_size(),
        }
    }

    fn name() -> &'static str {
        "vendored"
    }
}
