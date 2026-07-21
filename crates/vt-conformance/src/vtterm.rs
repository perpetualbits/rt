//! Adapt the in-house `vt_term::Term` to the harness [`VtEngine`] interface, so the
//! SAME spec cases, differential fuzz, and replay corpus that validate the vendored
//! oracle run against our Term too — and diff it against that oracle.

use crate::{attr, NCell, NColor, NCursor, ScreenState, VtEngine};

fn ncolor(c: vt_term::Color) -> NColor {
    match c {
        // The terminal default. `Named(256)` is the neutral sentinel for "default" —
        // it lines up with alacritty's Foreground/Background (index 256/257), which
        // the oracle maps the same way once colour reconciliation lands (Phase-3 fuzz).
        vt_term::Color::Default => NColor::Named(256),
        vt_term::Color::Indexed(i) => NColor::Indexed(i),
        vt_term::Color::Rgb(r, g, b) => NColor::Rgb(r, g, b),
    }
}

fn nattrs(c: &vt_term::Cell) -> u16 {
    let mut m = 0;
    if c.bold() { m |= attr::BOLD; }
    if c.italic() { m |= attr::ITALIC; }
    if c.underline() { m |= attr::UNDERLINE; }
    if c.inverse() { m |= attr::INVERSE; }
    if c.dim() { m |= attr::DIM; }
    if c.hidden() { m |= attr::HIDDEN; }
    if c.strikeout() { m |= attr::STRIKEOUT; }
    m
}

impl VtEngine for vt_term::Term {
    fn spawn(cols: usize, rows: usize) -> Self {
        vt_term::Term::new(cols, rows)
    }
    fn feed(&mut self, bytes: &[u8]) {
        vt_term::Term::feed(self, bytes)
    }
    fn resize(&mut self, cols: usize, rows: usize) {
        vt_term::Term::resize(self, cols, rows)
    }
    fn observe(&self) -> ScreenState {
        let (cols, rows) = (self.cols(), self.rows());
        let mut grid = vec![vec![NCell { ch: ' ', fg: NColor::Named(256), bg: NColor::Named(256), attrs: 0 }; cols]; rows];
        for r in 0..rows {
            for c in 0..cols {
                let cell = self.cell(r, c);
                let mut attrs = nattrs(&cell);
                // A double-width glyph carries the WIDE flag (alacritty's Flags::WIDE_CHAR),
                // derived here from the char so vt-term needn't store it separately.
                if unicode_width::UnicodeWidthChar::width(cell.c) == Some(2) {
                    attrs |= attr::WIDE;
                }
                grid[r][c] = NCell { ch: cell.c, fg: ncolor(cell.fg), bg: ncolor(cell.bg), attrs };
            }
        }
        let (col, line) = self.cursor();
        let cursor = if self.cursor_visible() {
            Some(NCursor { col, line, shape: 0, visible: true })
        } else {
            None
        };
        ScreenState {
            cols,
            rows,
            grid,
            cursor,
            alt_screen: self.alt_screen(),
            app_cursor: self.app_cursor(),
            display_offset: 0, // we always observe at the bottom of the view
            history: self.history_size(),
        }
    }
    fn name() -> &'static str {
        "vt-term"
    }
}
