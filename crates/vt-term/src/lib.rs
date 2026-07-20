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

use vt_parser::{Params, Parser, Perform};

/// A cell colour: the terminal default, a 0–255 palette index (named colours 0–15
/// included), or a direct RGB triple.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Color {
    Default,
    Indexed(u8),
    Rgb(u8, u8, u8),
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
}

impl Default for Cell {
    fn default() -> Self {
        Cell { c: ' ', fg: Color::Default, bg: Color::Default, attrs: Attrs::default() }
    }
}

/// The terminal. Feed bytes with [`feed`](Term::feed); read state via the accessors.
pub struct Term {
    cols: usize,
    rows: usize,
    grid: Vec<Vec<Cell>>,
    // Primary-screen state saved while the alternate screen is active.
    saved_screen: Option<(Vec<Vec<Cell>>, usize, usize, Cell)>,
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
    saved_cursor: (usize, usize, Cell, bool), // DECSC: row, col, pen, origin
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
            saved_cursor: (0, 0, Cell::default(), false),
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

    // ── Grid helpers ──────────────────────────────────────────────────────────
    fn blank(&self) -> Cell {
        // Erased cells keep the current background (xterm/alacritty behaviour), default
        // foreground, no attributes.
        Cell { c: ' ', fg: Color::Default, bg: self.pen.bg, attrs: Attrs::default() }
    }
    fn blank_row(&self) -> Vec<Cell> {
        vec![self.blank(); self.cols]
    }

    /// Scroll the scroll region up by `n` lines (content moves up; blanks fill in at
    /// the bottom of the region).
    fn scroll_up(&mut self, n: usize) {
        let (t, b) = (self.scroll_top, self.scroll_bottom);
        let n = n.min(b - t + 1);
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
    fn line_feed(&mut self) {
        self.pending_wrap = false;
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
    fn put_char(&mut self, c: char) {
        if self.pending_wrap {
            self.col = 0;
            self.line_feed();
            self.pending_wrap = false;
        }
        let (fg, bg, attrs) = (self.pen.fg, self.pen.bg, self.pen.attrs);
        self.grid[self.row][self.col] = Cell { c, fg, bg, attrs };
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
            2 | 3 => {
                for r in 0..self.rows {
                    self.grid[r] = vec![blank; self.cols];
                }
            }
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
    fn delete_chars(&mut self, n: usize) {
        let blank = self.blank();
        let row = &mut self.grid[self.row];
        let n = n.min(self.cols - self.col);
        for c in self.col..self.cols {
            row[c] = if c + n < self.cols { row[c + n] } else { blank };
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
            self.saved_screen = Some((saved, self.row, self.col, self.pen));
            self.alt = true;
        } else if !to_alt && self.alt {
            if let Some((grid, row, col, pen)) = self.saved_screen.take() {
                self.grid = grid;
                self.row = row.min(self.rows - 1);
                self.col = col.min(self.cols - 1);
                self.pen = pen;
            }
            self.alt = false;
        }
    }

    fn reset(&mut self) {
        let (cols, rows) = (self.cols, self.rows);
        *self = Term::new(cols, rows);
    }
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
            0x09 => self.put_tab(), // HT
            _ => {} // BEL and others: no grid effect
        }
    }

    fn csi_dispatch(&mut self, params: &Params, intermediates: &[u8], _ignore: bool, action: char) {
        let p = flat(params);
        let private = intermediates.first() == Some(&b'?');
        match action {
            'A' => self.cursor_up(count(&p, 0)),
            'B' | 'e' => self.cursor_down(count(&p, 0)),
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
            'h' if private => self.set_mode(&p, true),
            'l' if private => self.set_mode(&p, false),
            _ => {}
        }
    }

    fn esc_dispatch(&mut self, intermediates: &[u8], _ignore: bool, byte: u8) {
        if !intermediates.is_empty() {
            return; // charset designations etc. — ignored (ASCII assumed) for now
        }
        match byte {
            b'7' => self.saved_cursor = (self.row, self.col, self.pen, self.origin), // DECSC
            b'8' => {
                // DECRC
                let (r, c, pen, origin) = self.saved_cursor;
                self.row = r.min(self.rows - 1);
                self.col = c.min(self.cols - 1);
                self.pen = pen;
                self.origin = origin;
                self.pending_wrap = false;
            }
            b'D' => self.line_feed(),         // IND
            b'M' => self.reverse_line_feed(),  // RI
            b'E' => {
                // NEL
                self.col = 0;
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
