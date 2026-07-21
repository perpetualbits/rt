//! # vt-term — rt's in-house terminal Term
//!
//! Consumes [`vt_parser`]'s action stream and maintains the terminal grid: cells,
//! cursor, pen (current SGR), scroll region, and modes. The action→grid half of the
//! in-house engine (`docs/own-engine-plan.md` Phase 3, `docs/vt-term-design.md`).
//!
//! **Correctness contract:** produce the same observable state as the vendored
//! `alacritty_terminal` for any input, verified by differential testing against the
//! oracle in `vt-conformance` (spec cases + fuzz + replay corpus). This file is the
//! foundation — the common sequences — grown under that harness; scrollback and reflow
//! land later (tracked on the divergence ledger).

use std::collections::VecDeque;

use unicode_width::UnicodeWidthChar;
use vt_parser::{Params, Parser, Perform};

/// Maximum scrollback lines, matching the vendored oracle's `scrolling_history`.
const SCROLLBACK: usize = 10_000;

/// A cell colour: the terminal default, a 0–255 palette index (named colours 0–15
/// included), or a direct RGB triple.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
}

/// The two character sets vt-term maps: plain ASCII, and DEC Special Graphics (the
/// box-drawing / line-drawing set that `ESC ( 0` selects).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Charset {
    Ascii,
    Special,
}

/// Map a char through a charset (identity for ASCII; the DEC line-drawing table for
/// Special, matching alacritty's `StandardCharset::map`).
fn map_charset(cs: Charset, c: char) -> char {
    match cs {
        Charset::Ascii => c,
        Charset::Special => match c {
            '_' => ' ', '`' => '\u{25c6}', 'a' => '\u{2592}', 'b' => '\u{2409}',
            'c' => '\u{240c}', 'd' => '\u{240d}', 'e' => '\u{240a}', 'f' => '\u{b0}',
            'g' => '\u{b1}', 'h' => '\u{2424}', 'i' => '\u{240b}', 'j' => '\u{2518}',
            'k' => '\u{2510}', 'l' => '\u{250c}', 'm' => '\u{2514}', 'n' => '\u{253c}',
            'o' => '\u{23ba}', 'p' => '\u{23bb}', 'q' => '\u{2500}', 'r' => '\u{23bc}',
            's' => '\u{23bd}', 't' => '\u{251c}', 'u' => '\u{2524}', 'v' => '\u{2534}',
            'w' => '\u{252c}', 'x' => '\u{2502}', 'y' => '\u{2264}', 'z' => '\u{2265}',
            '{' => '\u{3c0}', '|' => '\u{2260}', '}' => '\u{a3}', '~' => '\u{b7}',
            _ => c,
        },
    }
}

/// A cell's rendition attributes.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub struct Attrs {
    pub bold: bool,
    pub italic: bool,
    pub underline: bool,
    pub inverse: bool,
    pub dim: bool,
    pub hidden: bool,
    pub strikeout: bool,
}

/// One grid cell: a character and its rendition.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    pub attrs: Attrs,
    /// This cell is the blank trailing (or leading) spacer of a wide glyph — invisible
    /// (`c == ' '`) but NOT empty, and overwriting it clears the wide glyph beside it.
    /// Mirrors alacritty's WIDE_CHAR_SPACER / LEADING_WIDE_CHAR_SPACER flags.
    pub spacer: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { c: ' ', fg: Color::Default, bg: Color::Default, attrs: Attrs::default(), spacer: false }
    }
}

/// The terminal. Feed bytes with [`feed`](Term::feed); read state via the accessors.
pub struct Term {
    cols: usize,
    rows: usize,
    grid: Vec<Vec<Cell>>,
    // Primary-screen state saved while the alternate screen is active.
    saved_screen: Option<(Vec<Vec<Cell>>, usize, usize, Cell, bool, [Charset; 4])>, // + designations
    row: usize,
    col: usize,
    /// Template for printed/erased cells (fg/bg/attrs); its `c` is unused.
    pen: Cell,
    scroll_top: usize,
    scroll_bottom: usize,
    autowrap: bool,
    origin: bool,
    show_cursor: bool,
    app_cursor: bool,
    alt: bool,
    /// Deferred wrap: the cursor sits on the last column having just printed there; the
    /// next printable char wraps first. This is xterm's DECAWM behaviour.
    pending_wrap: bool,
    saved_cursor: (usize, usize, Cell, bool, bool, [Charset; 4]), // DECSC state (charsets, not gl)
    /// The four designatable character sets (G0–G3) and which is currently invoked into
    /// GL (`gl`). `ESC ( 0` designates G0 = Special; SI/SO invoke G0/G1.
    charsets: [Charset; 4],
    gl: usize,
    /// Lines scrolled off the top of the primary screen (oldest at the front), capped
    /// at SCROLLBACK. Only its length is observed today (display_offset stays 0); it
    /// exists so `history_size` tracks the oracle and future scrolling can read it.
    history: VecDeque<Vec<Cell>>,
    parser: Parser,
}

impl Term {
    pub fn new(cols: usize, rows: usize) -> Self {
        let (cols, rows) = (cols.max(1), rows.max(1));
        Term {
            cols,
            rows,
            grid: vec![vec![Cell::default(); cols]; rows],
            saved_screen: None,
            row: 0,
            col: 0,
            pen: Cell::default(),
            scroll_top: 0,
            scroll_bottom: rows - 1,
            autowrap: true,
            origin: false,
            show_cursor: true,
            app_cursor: false,
            alt: false,
            pending_wrap: false,
            saved_cursor: (0, 0, Cell::default(), false, false, [Charset::Ascii; 4]),
            charsets: [Charset::Ascii; 4],
            gl: 0,
            history: VecDeque::new(),
            parser: Parser::new(),
        }
    }

    /// Feed raw bytes: parse them and apply the resulting actions to the grid.
    pub fn feed(&mut self, bytes: &[u8]) {
        // Move the parser out so it can borrow `self` as the Perform sink, then back.
        let mut parser = std::mem::take(&mut self.parser);
        parser.advance(self, bytes);
        self.parser = parser;
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        // Simple truncate/extend, no reflow (reflow is a later Phase-3 milestone —
        // see the divergence ledger). Content anchors at the top-left.
        let (cols, rows) = (cols.max(1), rows.max(1));
        let mut grid = vec![vec![Cell::default(); cols]; rows];
        for r in 0..rows.min(self.rows) {
            for c in 0..cols.min(self.cols) {
                grid[r][c] = self.grid[r][c];
            }
        }
        self.grid = grid;
        self.cols = cols;
        self.rows = rows;
        self.scroll_top = 0;
        self.scroll_bottom = rows - 1;
        self.row = self.row.min(rows - 1);
        self.col = self.col.min(cols - 1);
        self.pending_wrap = false;
    }

    // ── Accessors (the observable state) ──────────────────────────────────────
    pub fn cols(&self) -> usize {
        self.cols
    }
    pub fn rows(&self) -> usize {
        self.rows
    }
    pub fn cell(&self, row: usize, col: usize) -> Cell {
        self.grid[row][col]
    }
    /// Cursor `(col, row)`.
    pub fn cursor(&self) -> (usize, usize) {
        (self.col, self.row)
    }
    pub fn cursor_visible(&self) -> bool {
        self.show_cursor
    }
    pub fn alt_screen(&self) -> bool {
        self.alt
    }
    pub fn app_cursor(&self) -> bool {
        self.app_cursor
    }
    /// Number of lines held in scrollback (matches the oracle's `history_size`). The
    /// alternate screen has no scrollback, so it reports 0 while active — the primary's
    /// history is preserved and returns when the alt screen is left.
    pub fn history_size(&self) -> usize {
        if self.alt {
            0
        } else {
            self.history.len()
        }
    }

    // ── Grid helpers ──────────────────────────────────────────────────────────
    fn blank(&self) -> Cell {
        // Erased cells keep the current background (xterm/alacritty behaviour), default
        // foreground, no attributes.
        Cell { c: ' ', fg: Color::Default, bg: self.pen.bg, attrs: Attrs::default(), spacer: false }
    }
    fn blank_row(&self) -> Vec<Cell> {
        vec![self.blank(); self.cols]
    }

    /// Scroll the scroll region up by `n` lines (content moves up; blanks fill in at
    /// the bottom of the region).
    fn scroll_up(&mut self, n: usize) {
        let (t, b) = (self.scroll_top, self.scroll_bottom);
        let n = n.min(b - t + 1);
        // History grows only for a top-anchored scroll on the primary screen (matching
        // alacritty's `scroll_up`: history increases iff `region.start == 0`; the alt
        // screen has no scrollback).
        if t == 0 && !self.alt {
            for r in 0..n {
                self.history.push_back(self.grid[r].clone());
                if self.history.len() > SCROLLBACK {
                    self.history.pop_front();
                }
            }
        }
        self.grid[t..=b].rotate_left(n);
        for r in (b + 1 - n)..=b {
            self.grid[r] = self.blank_row();
        }
    }
    /// Scroll the scroll region down by `n` lines (blanks fill in at the top).
    fn scroll_down(&mut self, n: usize) {
        let (t, b) = (self.scroll_top, self.scroll_bottom);
        let n = n.min(b - t + 1);
        self.grid[t..=b].rotate_right(n);
        for r in t..(t + n) {
            self.grid[r] = self.blank_row();
        }
    }

    /// Line feed: cursor down one, scrolling the region if already at its bottom.
    /// Does NOT clear `pending_wrap` — alacritty's linefeed/newline leave it set, so a
    /// char printed after a bare LF still wraps once more (matched behaviour).
    fn line_feed(&mut self) {
        if self.row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }
    /// Reverse line feed: cursor up one, scrolling down if at the region top.
    fn reverse_line_feed(&mut self) {
        self.pending_wrap = false;
        if self.row == self.scroll_top {
            self.scroll_down(1);
        } else if self.row > 0 {
            self.row -= 1;
        }
    }

    // ── Printing ──────────────────────────────────────────────────────────────
    /// Write `cell` at the cursor with the current pen colours/attrs but the given char.
    fn write_cell(&mut self, c: char) {
        let c = map_charset(self.charsets[self.gl], c);
        let (fg, bg, attrs) = (self.pen.fg, self.pen.bg, self.pen.attrs);
        self.grid[self.row][self.col] = Cell { c, fg, bg, attrs, spacer: false };
    }

    /// Write a wide-glyph spacer at the cursor: a blank that carries the pen colours/
    /// attrs and the `spacer` flag.
    fn write_spacer(&mut self) {
        let (fg, bg, attrs) = (self.pen.fg, self.pen.bg, self.pen.attrs);
        self.grid[self.row][self.col] = Cell { c: ' ', fg, bg, attrs, spacer: true };
    }

    /// If the cursor cell is the trailing spacer of a wide glyph to its left, overwriting
    /// it would orphan that glyph — so blank the glyph half (alacritty's `clear_wide`).
    fn clear_wide_left(&mut self) {
        // Only when the cursor cell is a real trailing spacer whose glyph is to its
        // left (not, e.g., a blank an erase left behind) — matching alacritty, which
        // keys this off the WIDE_CHAR_SPACER flag. clear_wide only sets c=' ' (the WIDE
        // flag, here derived from width, then clears); fg/bg/attrs are kept.
        if self.col > 0
            && self.grid[self.row][self.col].spacer
            && self.grid[self.row][self.col - 1].c.width() == Some(2)
        {
            self.grid[self.row][self.col - 1].c = ' ';
        }
    }

    fn put_char(&mut self, c: char) {
        // Character display width (unicode-width): 0 = combining/zero-width, 1 = normal,
        // 2 = wide (CJK/emoji). Matches alacritty's `input`.
        let width = c.width().unwrap_or(0);
        if width == 0 {
            // Zero-width/combining: alacritty attaches it to the previous cell's base
            // glyph without changing that cell's `.c` or the cursor — observably a no-op.
            return;
        }
        if self.pending_wrap {
            self.col = 0;
            self.line_feed();
            self.pending_wrap = false;
        }
        self.clear_wide_left(); // preserve wide-char pair integrity at the write position
        if width == 2 {
            // A wide glyph needs two columns. If it would run off the last column, place
            // a blank leading spacer and wrap first (autowrap on), else defer the wrap.
            if self.col + 1 >= self.cols {
                if self.autowrap {
                    self.write_spacer(); // leading spacer
                    self.col = 0;
                    self.line_feed();
                    self.pending_wrap = false;
                } else {
                    self.pending_wrap = true;
                    return;
                }
            }
            self.write_cell(c); // the wide glyph
            if self.col + 1 < self.cols {
                self.col += 1;
                self.write_spacer(); // trailing spacer occupying the second cell
            }
        } else {
            self.write_cell(c);
        }
        // Common cursor advance (shared with alacritty's width==1/2 tail).
        if self.col + 1 < self.cols {
            self.col += 1;
        } else if self.autowrap {
            self.pending_wrap = true; // stay on the last column; wrap on the next char
        }
        // else: autowrap off — the cursor stays put and the next char overwrites.
    }

    // ── Cursor motion ─────────────────────────────────────────────────────────
    fn cursor_up(&mut self, n: usize) {
        self.pending_wrap = false;
        let floor = if self.row >= self.scroll_top { self.scroll_top } else { 0 };
        self.row = self.row.saturating_sub(n).max(floor);
    }
    fn cursor_down(&mut self, n: usize) {
        self.pending_wrap = false;
        let ceil = if self.row <= self.scroll_bottom { self.scroll_bottom } else { self.rows - 1 };
        self.row = (self.row + n).min(ceil);
    }
    fn cursor_left(&mut self, n: usize) {
        self.pending_wrap = false;
        self.col = self.col.saturating_sub(n);
    }
    fn cursor_right(&mut self, n: usize) {
        self.pending_wrap = false;
        self.col = (self.col + n).min(self.cols - 1);
    }
    fn set_col(&mut self, col: usize) {
        self.pending_wrap = false;
        self.col = col.min(self.cols - 1);
    }
    fn set_row(&mut self, row: usize) {
        self.pending_wrap = false;
        self.row = row.min(self.rows - 1);
    }
    /// CUP/HVP: 0-based `(row, col)`, respecting origin mode.
    fn goto(&mut self, row: usize, col: usize) {
        self.pending_wrap = false;
        self.col = col.min(self.cols - 1);
        self.row = if self.origin {
            (self.scroll_top + row).min(self.scroll_bottom)
        } else {
            row.min(self.rows - 1)
        };
    }

    // ── Erase ─────────────────────────────────────────────────────────────────
    fn erase_in_line(&mut self, mode: u16) {
        // Match alacritty: EL-Right is a no-op while a wrap is pending (the cursor sits
        // logically past the last column, so there is nothing from it to the edge).
        if self.pending_wrap && mode != 1 && mode != 2 {
            return;
        }
        let blank = self.blank();
        let (lo, hi) = match mode {
            1 => (0, self.col),               // start .. cursor
            2 => (0, self.cols - 1),           // whole line
            _ => (self.col, self.cols - 1),    // cursor .. end
        };
        for c in lo..=hi {
            self.grid[self.row][c] = blank;
        }
    }
    fn erase_in_display(&mut self, mode: u16) {
        let blank = self.blank();
        match mode {
            1 => {
                // Erase-above. Match alacritty's quirk: the lines ABOVE are cleared
                // only when the cursor is past line 1 (`cursor.line > 1`); the current
                // line is always cleared from its start to the cursor.
                if self.row > 1 {
                    for r in 0..self.row {
                        self.grid[r] = vec![blank; self.cols];
                    }
                }
                for c in 0..=self.col {
                    self.grid[self.row][c] = blank;
                }
            }
            2 => {
                // ED-All = alacritty's clear_viewport: on the PRIMARY screen the used
                // lines (row 0 down to the last non-empty row) scroll into history
                // first, THEN the whole screen clears. The alt screen just clears.
                if !self.alt {
                    // `positions` = the row the oracle's backward "last non-empty" scan
                    // stops at, +1. With content: last-non-empty + 1. All empty: the scan
                    // stops at line 0 when there is no history yet (→ 1), or descends to
                    // line −1 and breaks when history exists (→ 0). This exactly matches
                    // alacritty's clear_viewport iterator, verified against the oracle.
                    let last_non_empty = (0..self.rows).rev().find(|&r| self.grid[r].iter().any(|c| !cell_is_empty(c)));
                    let positions = match last_non_empty {
                        Some(r) => r + 1,
                        None if self.history.is_empty() => 1,
                        None => 0,
                    };
                    for r in 0..positions {
                        self.history.push_back(self.grid[r].clone());
                        if self.history.len() > SCROLLBACK {
                            self.history.pop_front();
                        }
                    }
                }
                for r in 0..self.rows {
                    self.grid[r] = vec![blank; self.cols];
                }
            }
            3 => self.history.clear(), // ED-Saved: clear scrollback only
            _ => {
                for c in self.col..self.cols {
                    self.grid[self.row][c] = blank;
                }
                for r in (self.row + 1)..self.rows {
                    self.grid[r] = vec![blank; self.cols];
                }
            }
        }
    }
    fn erase_chars(&mut self, n: usize) {
        let blank = self.blank();
        for c in self.col..(self.col + n).min(self.cols) {
            self.grid[self.row][c] = blank;
        }
    }

    // ── Insert / delete ───────────────────────────────────────────────────────
    fn insert_chars(&mut self, n: usize) {
        let blank = self.blank();
        let row = &mut self.grid[self.row];
        let n = n.min(self.cols - self.col);
        for c in (self.col + n..self.cols).rev() {
            row[c] = row[c - n];
        }
        for c in self.col..self.col + n {
            row[c] = blank;
        }
    }
    fn delete_chars(&mut self, count: usize) {
        // Matches alacritty: the count is clamped to the FULL width (not cols−col), and
        // the last `count` columns are cleared — so a large count also blanks cells to
        // the LEFT of the cursor.
        let cols = self.cols;
        let count = count.min(cols);
        let start = self.col;
        let blank = self.blank();
        for c in start..cols {
            self.grid[self.row][c] = if c + count < cols { self.grid[self.row][c + count] } else { blank };
        }
        for c in (cols - count)..cols {
            self.grid[self.row][c] = blank;
        }
    }
    fn insert_lines(&mut self, n: usize) {
        if self.row < self.scroll_top || self.row > self.scroll_bottom {
            return;
        }
        let (top, b) = (self.row, self.scroll_bottom);
        let n = n.min(b - top + 1);
        self.grid[top..=b].rotate_right(n);
        for r in top..(top + n) {
            self.grid[r] = self.blank_row();
        }
    }
    fn delete_lines(&mut self, n: usize) {
        if self.row < self.scroll_top || self.row > self.scroll_bottom {
            return;
        }
        let (top, b) = (self.row, self.scroll_bottom);
        let n = n.min(b - top + 1);
        self.grid[top..=b].rotate_left(n);
        for r in (b + 1 - n)..=b {
            self.grid[r] = self.blank_row();
        }
    }

    // ── SGR ───────────────────────────────────────────────────────────────────
    fn sgr(&mut self, p: &[u16]) {
        let mut i = 0;
        if p.is_empty() {
            self.pen = Cell::default();
            return;
        }
        while i < p.len() {
            match p[i] {
                0 => self.pen = Cell::default(),
                1 => self.pen.attrs.bold = true,
                2 => self.pen.attrs.dim = true,
                3 => self.pen.attrs.italic = true,
                4 => self.pen.attrs.underline = true,
                7 => self.pen.attrs.inverse = true,
                8 => self.pen.attrs.hidden = true,
                9 => self.pen.attrs.strikeout = true,
                22 => {
                    self.pen.attrs.bold = false;
                    self.pen.attrs.dim = false;
                }
                23 => self.pen.attrs.italic = false,
                24 => self.pen.attrs.underline = false,
                27 => self.pen.attrs.inverse = false,
                28 => self.pen.attrs.hidden = false,
                29 => self.pen.attrs.strikeout = false,
                30..=37 => self.pen.fg = Color::Indexed((p[i] - 30) as u8),
                38 => i += self.sgr_color(p, i, true),
                39 => self.pen.fg = Color::Default,
                40..=47 => self.pen.bg = Color::Indexed((p[i] - 40) as u8),
                48 => i += self.sgr_color(p, i, false),
                49 => self.pen.bg = Color::Default,
                90..=97 => self.pen.fg = Color::Indexed((p[i] - 90 + 8) as u8),
                100..=107 => self.pen.bg = Color::Indexed((p[i] - 100 + 8) as u8),
                _ => {}
            }
            i += 1;
        }
    }
    /// Handle `38`/`48` extended colour (semicolon form). Returns how many EXTRA params
    /// were consumed beyond the `38`/`48` itself.
    fn sgr_color(&mut self, p: &[u16], i: usize, fg: bool) -> usize {
        match p.get(i + 1) {
            Some(2) => {
                let r = p.get(i + 2).copied().unwrap_or(0) as u8;
                let g = p.get(i + 3).copied().unwrap_or(0) as u8;
                let b = p.get(i + 4).copied().unwrap_or(0) as u8;
                let c = Color::Rgb(r, g, b);
                if fg { self.pen.fg = c } else { self.pen.bg = c }
                4
            }
            Some(5) => {
                let idx = p.get(i + 2).copied().unwrap_or(0) as u8;
                let c = Color::Indexed(idx);
                if fg { self.pen.fg = c } else { self.pen.bg = c }
                2
            }
            _ => 0,
        }
    }

    fn set_scroll_region(&mut self, p: &[u16]) {
        let top = p.first().copied().filter(|&v| v > 0).map_or(1, |v| v) as usize - 1;
        let bottom = p.get(1).copied().filter(|&v| v > 0).map_or(self.rows, |v| v as usize) - 1;
        if top < bottom && bottom < self.rows {
            self.scroll_top = top;
            self.scroll_bottom = bottom;
        } else {
            self.scroll_top = 0;
            self.scroll_bottom = self.rows - 1;
        }
        // DECSTBM homes the cursor (origin-aware).
        self.goto(0, 0);
    }

    fn set_mode(&mut self, p: &[u16], set: bool) {
        for &mode in p {
            match mode {
                1 => self.app_cursor = set,   // DECCKM
                6 => self.origin = set,        // DECOM
                7 => self.autowrap = set,      // DECAWM
                25 => self.show_cursor = set,  // DECTCEM
                47 | 1047 | 1049 => self.swap_alt(set),
                _ => {}
            }
        }
    }
    fn swap_alt(&mut self, to_alt: bool) {
        if to_alt && !self.alt {
            let saved = std::mem::replace(&mut self.grid, vec![vec![Cell::default(); self.cols]; self.rows]);
            // Designations (G0–G3) are part of the cursor → saved and restored across the
            // alt screen. The active charset `gl` is Term-global and is NOT.
            self.saved_screen = Some((saved, self.row, self.col, self.pen, self.pending_wrap, self.charsets));
            self.alt = true;
        } else if !to_alt && self.alt {
            if let Some((grid, row, col, pen, wrap, charsets)) = self.saved_screen.take() {
                self.grid = grid;
                self.row = row.min(self.rows - 1);
                self.col = col.min(self.cols - 1);
                self.pen = pen;
                self.pending_wrap = wrap;
                self.charsets = charsets;
            }
            self.alt = false;
        }
    }

    fn reset(&mut self) {
        let (cols, rows) = (self.cols, self.rows);
        *self = Term::new(cols, rows);
    }
}

/// A cell alacritty treats as "empty" for the clear_viewport scan: a space or tab
/// glyph with the default foreground/background and no attributes.
fn cell_is_empty(c: &Cell) -> bool {
    (c.c == ' ' || c.c == '\t')
        && !c.spacer
        && c.fg == Color::Default
        && c.bg == Color::Default
        // alacritty's is_empty ignores bold/dim/italic; only these mark a blank non-empty.
        && !c.attrs.inverse
        && !c.attrs.underline
        && !c.attrs.strikeout
}

/// First value of each parameter as a flat `Vec` (drops sub-parameters — the common
/// semicolon form; colon sub-parameters beyond the SGR extended-colour case are on the
/// ledger).
fn flat(params: &Params) -> Vec<u16> {
    params.iter().map(|g| g.first().copied().unwrap_or(0)).collect()
}
/// Parameter `i` treated as a count: absent or 0 means 1 (the cursor-motion default).
fn count(p: &[u16], i: usize) -> usize {
    p.get(i).copied().unwrap_or(0).max(1) as usize
}

impl Perform for Term {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    fn execute(&mut self, byte: u8) {
        match byte {
            0x0A | 0x0B | 0x0C => self.line_feed(), // LF / VT / FF
            0x0D => {
                self.pending_wrap = false;
                self.col = 0;
            }
            0x08 => {
                self.pending_wrap = false;
                self.col = self.col.saturating_sub(1);
            }
            0x0E => self.gl = 1, // SO: invoke G1 into GL
            0x0F => self.gl = 0, // SI: invoke G0 into GL
            0x09 => self.put_tab(), // HT
            _ => {} // BEL and others: no grid effect
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let p = flat(params);
        // A CSI with any intermediate byte only matches specific handlers. We implement
        // the `?`-private DECSET/DECRST (h/l); every other intermediate+action pair —
        // e.g. `?…H` — is ignored, exactly as alacritty leaves it unhandled.
        if !intermediates.is_empty() {
            if intermediates.first() == Some(&b'?') {
                match action {
                    'h' => self.set_mode(&p, true),
                    'l' => self.set_mode(&p, false),
                    _ => {}
                }
            }
            return;
        }
        match action {
            'A' => self.cursor_up(count(&p, 0)),
            'B' | 'e' => self.cursor_down(count(&p, 0)),
            'E' => {
                self.cursor_down(count(&p, 0)); // CNL: down N lines, to column 0
                self.col = 0;
            }
            'F' => {
                self.cursor_up(count(&p, 0)); // CPL: up N lines, to column 0
                self.col = 0;
            }
            'C' | 'a' => self.cursor_right(count(&p, 0)),
            'D' => self.cursor_left(count(&p, 0)),
            'G' | '`' => self.set_col(count(&p, 0) - 1), // CHA / HPA
            'd' => self.set_row(count(&p, 0) - 1),        // VPA
            'H' | 'f' => self.goto(count(&p, 0) - 1, count(&p, 1) - 1),
            'J' => self.erase_in_display(p.first().copied().unwrap_or(0)),
            'K' => self.erase_in_line(p.first().copied().unwrap_or(0)),
            'L' => self.insert_lines(count(&p, 0)),
            'M' => self.delete_lines(count(&p, 0)),
            '@' => self.insert_chars(count(&p, 0)),
            'P' => self.delete_chars(count(&p, 0)),
            'X' => self.erase_chars(count(&p, 0)),
            'S' => self.scroll_up(count(&p, 0)),
            'T' => self.scroll_down(count(&p, 0)),
            'r' => self.set_scroll_region(&p),
            'm' => self.sgr(&p),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if let Some(&i) = intermediates.first() {
            // Charset designation: ESC ( ) * + <final>. '0' = DEC special graphics; any
            // other final designates ASCII. Other intermediates (e.g. ESC # …) are ignored.
            if let Some(idx) = match i { b'(' => Some(0), b')' => Some(1), b'*' => Some(2), b'+' => Some(3), _ => None } {
                self.charsets[idx] = if byte == b'0' { Charset::Special } else { Charset::Ascii };
            }
            return;
        }
        match byte {
            b'7' => self.saved_cursor = (self.row, self.col, self.pen, self.origin, self.pending_wrap, self.charsets), // DECSC
            b'8' => {
                // DECRC
                let (r, c, pen, origin, wrap, charsets) = self.saved_cursor;
                self.row = r.min(self.rows - 1);
                self.col = c.min(self.cols - 1);
                self.pen = pen;
                self.origin = origin;
                self.pending_wrap = wrap;
                self.charsets = charsets;
            }
            b'D' => self.line_feed(),         // IND
            b'M' => self.reverse_line_feed(),  // RI
            b'E' => {
                // NEL = CR + LF; the CR half clears the pending wrap.
                self.col = 0;
                self.pending_wrap = false;
                self.line_feed();
            }
            b'c' => self.reset(), // RIS
            _ => {}
        }
    }
}

impl Term {
    /// Horizontal tab, matching alacritty: at a pending wrap it line-breaks; otherwise
    /// it writes a `\t` glyph into the (blank) starting cell, then advances to the next
    /// tab stop (every 8 columns), clamped to the last column.
    fn put_tab(&mut self) {
        if self.pending_wrap {
            self.col = 0;
            self.line_feed();
            self.pending_wrap = false;
            return;
        }
        if self.grid[self.row][self.col].c == ' ' {
            self.grid[self.row][self.col].c = '\t';
        }
        if self.col + 1 < self.cols {
            self.col = (((self.col / 8) + 1) * 8).min(self.cols - 1);
        }
    }
}
