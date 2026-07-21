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
    /// Set on the last cell of a row that soft-wrapped (autowrap) into the next — as
    /// opposed to a hard line break. Reflow joins rows across this flag. Mirrors
    /// alacritty's WRAPLINE flag; also makes the cell non-empty.
    pub wrapline: bool,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { c: ' ', fg: Color::Default, bg: Color::Default, attrs: Attrs::default(), spacer: false, wrapline: false }
    }
}

/// Cursor shape requested via DECSCUSR (`CSI Ps SP q`); blink is not distinguished.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
}

/// Mouse-reporting protocol. Mutually exclusive, matching xterm/alacritty: DECSET
/// 1000 → `Click`, 1002 → `Drag` (button-motion), 1003 → `Motion` (any-motion).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum MouseMode {
    Off,
    Click,
    Drag,
    Motion,
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
    /// How many lines the view is scrolled up into scrollback (0 = at the bottom). The
    /// snapshot reads viewport row `r` from absolute line `r - display_offset`.
    display_offset: usize,
    /// Cursor shape (DECSCUSR); Term-global, not saved by the alt screen or DECSC.
    cursor_shape: CursorShape,
    /// Active mouse-reporting protocol (DECSET 1000/1002/1003), and whether SGR encoding
    /// (1006) is on. Term-global.
    mouse_mode: MouseMode,
    mouse_sgr: bool,
    /// Input-affecting modes a host encodes keys/paste against (DECSET 1004/1007/2004).
    focus_events: bool,
    alt_scroll: bool,
    bracketed_paste: bool,
    /// Pending window title set via OSC 0/2, consumed by [`take_title`](Term::take_title).
    title: Option<String>,
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
            display_offset: 0,
            cursor_shape: CursorShape::Block,
            mouse_mode: MouseMode::Off,
            mouse_sgr: false,
            focus_events: false,
            alt_scroll: false,
            bracketed_paste: false,
            title: None,
            parser: Parser::new(),
        }
    }

    /// Feed raw bytes: parse them and apply the resulting actions to the grid.
    pub fn feed(&mut self, bytes: &[u8]) {
        // Move the parser out so it can borrow `self` as the Perform sink, then back.
        // `feed` (not `advance`) so synchronized updates (DECSET 2026) are honoured.
        let mut parser = std::mem::take(&mut self.parser);
        parser.feed(self, bytes);
        self.parser = parser;
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        let (cols, rows) = (cols.max(1), rows.max(1));
        if cols == self.cols && rows == self.rows {
            return;
        }
        self.reflow(cols, rows);
        // Reflow rebuilds scrollback; snap the view to the bottom rather than track a
        // now-ambiguous scroll position (matching most terminals' resize behaviour).
        self.display_offset = 0;
    }

    /// Resize the grid. alacritty always adjusts the line count first (a pure row move
    /// that scrolls to keep the cursor in view), then the columns. On the primary screen
    /// scrolled-off lines enter scrollback and columns reflow wrapped content; the
    /// alternate screen has no scrollback (scrolled lines are discarded) and does not
    /// reflow (it just truncates/extends columns).
    fn reflow(&mut self, new_cols: usize, new_rows: usize) {
        let keep = !self.alt; // primary screen keeps scrolled lines + reflows
        match new_rows.cmp(&self.rows) {
            std::cmp::Ordering::Less => self.shrink_lines(new_rows, keep),
            std::cmp::Ordering::Greater => self.grow_lines(new_rows, keep),
            std::cmp::Ordering::Equal => {}
        }
        if new_cols != self.cols {
            if keep {
                self.reflow_columns(new_cols);
            } else {
                self.resize_columns_flat(new_cols);
            }
        }
    }

    /// Alt-screen column resize: truncate or blank-extend each row, clamp the cursor —
    /// no reflow (matching alacritty's `grow/shrink_columns` with `reflow = false`).
    fn resize_columns_flat(&mut self, new_cols: usize) {
        for row in &mut self.grid {
            row.resize(new_cols, Cell::default());
        }
        self.cols = new_cols;
        self.col = self.col.min(new_cols - 1);
        self.pending_wrap = false;
    }

    /// Remove lines from the visible area (rows shrink). The bottom rows are dropped
    /// outright; only if the cursor would fall outside the shorter viewport is content
    /// scrolled off the top into history to keep the cursor in view (Terminal.app /
    /// iTerm / alacritty behaviour).
    fn shrink_lines(&mut self, target: usize, keep: bool) {
        let scroll = (self.row + 1).saturating_sub(target);
        for _ in 0..scroll {
            let line = self.grid.remove(0);
            if keep {
                self.history.push_back(line); // primary: into scrollback; alt: discard
            }
        }
        while self.history.len() > SCROLLBACK {
            self.history.pop_front();
        }
        self.row -= scroll;
        self.grid.truncate(target);
        while self.grid.len() < target {
            self.grid.push(vec![Cell::default(); self.cols]);
        }
        self.rows = target;
        self.scroll_top = 0;
        self.scroll_bottom = target - 1;
        self.row = self.row.min(target - 1);
    }

    /// Add lines to the visible area (rows grow). Lines are pulled back from history at
    /// the top (moving the cursor down) as long as scrollback lasts; any remaining new
    /// lines are appended as blanks at the bottom.
    fn grow_lines(&mut self, target: usize, keep: bool) {
        let added = target - self.rows;
        let from_history = if keep { self.history.len().min(added) } else { 0 };
        for _ in 0..from_history {
            let line = self.history.pop_back().unwrap();
            self.grid.insert(0, line);
        }
        self.row += from_history;
        for _ in 0..(added - from_history) {
            self.grid.push(vec![Cell::default(); self.cols]);
        }
        self.rows = target;
        self.scroll_top = 0;
        self.scroll_bottom = target - 1;
    }

    /// Reflow the columns: rejoin soft-wrapped rows into logical lines, re-split them at
    /// `new_cols`, and re-lay-out bottom-anchored across the (already-correct) row count,
    /// tracking the cursor. Called after the line count is settled.
    /// Reflow the columns — a faithful port of alacritty's `grow_columns`/`shrink_columns`
    /// (`grid/resize.rs`): a row-by-row rewrap over the whole buffer (history + visible),
    /// carrying the cursor through the exact split arithmetic. The buffer is height-indexed
    /// from the bottom (bottom visible row first, up through the visible area, then history
    /// newest→oldest), matching alacritty's `take_all()` ordering; `occ` is never read by
    /// that logic (it uses physical `len()` + content-based `is_clear`), so plain
    /// `Vec<Cell>` rows suffice.
    fn reflow_columns(&mut self, new_cols: usize) {
        let old_cols = self.cols;
        let lines = self.rows;
        let rows: Vec<Vec<Cell>> =
            self.grid.iter().rev().chain(self.history.iter().rev()).cloned().collect();
        let mut cur = RCursor { line: self.row as i32, col: self.col, wrap: self.pending_wrap };

        let nb = if new_cols > old_cols {
            grow_columns_impl(rows, new_cols, lines, &mut cur)
        } else {
            shrink_columns_impl(rows, new_cols, lines, &mut cur)
        };

        // `nb` is height-indexed from the bottom: visible = bottom `lines` rows, the rest is
        // history (newest just above the viewport). Convert back to top-to-bottom.
        let mut grid: Vec<Vec<Cell>> = nb[..lines].iter().rev().cloned().collect();
        for row in &mut grid {
            if row.len() < new_cols {
                row.resize(new_cols, Cell::default());
            }
        }
        let mut history: VecDeque<Vec<Cell>> = nb[lines..].iter().rev().cloned().collect();
        while history.len() > SCROLLBACK {
            history.pop_front();
        }

        // Final cursor reflow (resize.rs 374-388): pending-wrap at the width, else clamp.
        let cline = cur.line.clamp(0, lines as i32 - 1) as usize;
        let at_wrap = grid[cline].get(new_cols - 1).map(|c| c.wrapline).unwrap_or(false);
        if cur.col == new_cols && !at_wrap {
            self.pending_wrap = true;
            self.row = cline;
            self.col = new_cols - 1;
        } else {
            let (l, c) = reflow_grid_clamp(cur.line, cur.col, new_cols, lines);
            self.row = l;
            self.col = c;
            self.pending_wrap = cur.wrap;
        }

        self.history = history;
        self.grid = grid;
        self.cols = new_cols;
        self.rows = lines;
        self.scroll_top = 0;
        self.scroll_bottom = lines - 1;
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
    /// The requested cursor shape (DECSCUSR).
    pub fn cursor_shape(&self) -> CursorShape {
        self.cursor_shape
    }
    /// Whether any mouse reporting is enabled (DECSET 1000/1002/1003).
    pub fn wants_mouse(&self) -> bool {
        self.mouse_mode != MouseMode::Off
    }
    /// Whether *any-motion* reporting is enabled (DECSET 1003) — matching rt-engine's
    /// `wants_motion`, which keys only on 1003, not 1002's button-motion.
    pub fn wants_motion(&self) -> bool {
        self.mouse_mode == MouseMode::Motion
    }
    /// Whether SGR mouse encoding (DECSET 1006) is active.
    pub fn mouse_sgr(&self) -> bool {
        self.mouse_sgr
    }
    /// Whether focus in/out reporting (DECSET 1004) is enabled.
    pub fn focus_events(&self) -> bool {
        self.focus_events
    }
    /// Whether alt-screen scroll-to-arrow-keys (DECSET 1007) is enabled.
    pub fn alt_scroll(&self) -> bool {
        self.alt_scroll
    }
    /// Whether bracketed paste (DECSET 2004) is enabled — a host should wrap pasted text
    /// in `\x1b[200~` … `\x1b[201~` so the app can tell paste from typing.
    pub fn bracketed_paste(&self) -> bool {
        self.bracketed_paste
    }
    /// Take the pending window title (OSC 0/2), clearing it. Returns `None` if unchanged
    /// since the last call.
    pub fn take_title(&mut self) -> Option<String> {
        self.title.take()
    }

    // ── Scrollback viewport ────────────────────────────────────────────────────
    /// Lines the view is scrolled up into scrollback (0 = at the bottom).
    pub fn display_offset(&self) -> usize {
        self.display_offset
    }
    /// Oldest readable absolute line (`<= 0`; the top of scrollback).
    pub fn topmost(&self) -> i32 {
        -(self.history_size() as i32)
    }
    /// Newest readable absolute line (`rows - 1`; the bottom of the screen).
    pub fn bottommost(&self) -> i32 {
        self.rows as i32 - 1
    }
    /// Scroll the view by `delta` whole lines (positive = up into history), clamped to the
    /// scrollable range. The alt screen has no scrollback, so this is a no-op there.
    pub fn scroll_display(&mut self, delta: i32) {
        let max = self.history_size() as i32;
        self.display_offset = (self.display_offset as i32 + delta).clamp(0, max) as usize;
    }
    /// Snap the view back to the bottom (live output).
    pub fn scroll_to_bottom_view(&mut self) {
        self.display_offset = 0;
    }
    /// The cell at absolute line `abs` (visible lines `0..rows`, history negative down to
    /// `topmost`) and column `col`, or a blank cell if out of range.
    pub fn cell_at(&self, abs: i32, col: usize) -> Cell {
        let row = if abs >= 0 && (abs as usize) < self.rows {
            &self.grid[abs as usize]
        } else if abs < 0 {
            let idx = self.history.len() as i32 + abs; // -history..-1 → 0..H-1
            if idx >= 0 && (idx as usize) < self.history.len() {
                &self.history[idx as usize]
            } else {
                return Cell::default();
            }
        } else {
            return Cell::default();
        };
        row.get(col).copied().unwrap_or_default()
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
        Cell { c: ' ', fg: Color::Default, bg: self.pen.bg, attrs: Attrs::default(), spacer: false, wrapline: false }
    }
    /// Reset row `r` to `blank` in place, keeping its heap allocation — the scroll/erase
    /// hot path. Replacing the `Vec` (`grid[r] = blank_row()`) allocated a fresh row every
    /// time; reusing the buffer is what alacritty's ring-buffer reset does.
    #[inline]
    fn fill_row(&mut self, r: usize, blank: Cell) {
        for cell in &mut self.grid[r] {
            *cell = blank;
        }
    }

    /// Scroll the scroll region up by `n` lines (content moves up; blanks fill in at
    /// the bottom of the region).
    /// Push a line into scrollback (capped), and if the view is scrolled up, bump the
    /// offset so it stays anchored on the same content (alacritty's `increase_scroll_limit`).
    fn push_history(&mut self, line: Vec<Cell>) {
        self.history.push_back(line);
        if self.history.len() > SCROLLBACK {
            self.history.pop_front();
        } else if self.display_offset > 0 {
            self.display_offset = (self.display_offset + 1).min(self.history.len());
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let (t, b) = (self.scroll_top, self.scroll_bottom);
        let n = n.min(b - t + 1);
        // History grows only for a top-anchored scroll on the primary screen (matching
        // alacritty's `scroll_up`: history increases iff `region.start == 0`; the alt
        // screen has no scrollback).
        if t == 0 && !self.alt {
            for r in 0..n {
                self.push_history(self.grid[r].clone());
            }
        }
        self.grid[t..=b].rotate_left(n);
        let blank = self.blank();
        for r in (b + 1 - n)..=b {
            self.fill_row(r, blank);
        }
    }
    /// Scroll the scroll region down by `n` lines (blanks fill in at the top).
    fn scroll_down(&mut self, n: usize) {
        let (t, b) = (self.scroll_top, self.scroll_bottom);
        let n = n.min(b - t + 1);
        self.grid[t..=b].rotate_right(n);
        let blank = self.blank();
        for r in t..(t + n) {
            self.fill_row(r, blank);
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
        self.grid[self.row][self.col] = Cell { c, fg, bg, attrs, spacer: false, wrapline: false };
    }

    /// Write a wide-glyph spacer at the cursor: a blank that carries the pen colours/
    /// attrs and the `spacer` flag.
    fn write_spacer(&mut self) {
        let (fg, bg, attrs) = (self.pen.fg, self.pen.bg, self.pen.attrs);
        self.grid[self.row][self.col] = Cell { c: ' ', fg, bg, attrs, spacer: true, wrapline: false };
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

    /// Soft (autowrap) line break: mark the current row's last cell WRAPLINE so reflow
    /// can rejoin it, then move to column 0 of the next line and clear the pending wrap.
    fn soft_wrap(&mut self) {
        self.grid[self.row][self.cols - 1].wrapline = true;
        self.col = 0;
        self.line_feed();
        self.pending_wrap = false;
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
            self.soft_wrap();
        }
        self.clear_wide_left(); // preserve wide-char pair integrity at the write position
        if width == 2 {
            // A wide glyph needs two columns. If it would run off the last column, place
            // a blank leading spacer and wrap first (autowrap on), else defer the wrap.
            if self.col + 1 >= self.cols {
                if self.autowrap {
                    self.write_spacer(); // leading spacer
                    self.soft_wrap();
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
                        self.fill_row(r, blank);
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
                        self.push_history(self.grid[r].clone());
                    }
                }
                for r in 0..self.rows {
                    self.fill_row(r, blank);
                }
            }
            3 => self.history.clear(), // ED-Saved: clear scrollback only
            _ => {
                for c in self.col..self.cols {
                    self.grid[self.row][c] = blank;
                }
                for r in (self.row + 1)..self.rows {
                    self.fill_row(r, blank);
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
        let blank = self.blank();
        for r in top..(top + n) {
            self.fill_row(r, blank);
        }
    }
    fn delete_lines(&mut self, n: usize) {
        if self.row < self.scroll_top || self.row > self.scroll_bottom {
            return;
        }
        let (top, b) = (self.row, self.scroll_bottom);
        let n = n.min(b - top + 1);
        self.grid[top..=b].rotate_left(n);
        let blank = self.blank();
        for r in (b + 1 - n)..=b {
            self.fill_row(r, blank);
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
                // Mouse protocols are mutually exclusive (xterm/alacritty): setting one
                // replaces any other; resetting clears only if it is the active one.
                1000 => self.mouse_mode = if set { MouseMode::Click } else { self.mouse_off_if(MouseMode::Click) },
                1002 => self.mouse_mode = if set { MouseMode::Drag } else { self.mouse_off_if(MouseMode::Drag) },
                1003 => self.mouse_mode = if set { MouseMode::Motion } else { self.mouse_off_if(MouseMode::Motion) },
                1006 => self.mouse_sgr = set,             // SGR mouse encoding
                1005 => if set { self.mouse_sgr = false }, // UTF-8 encoding clears SGR
                1004 => self.focus_events = set,          // report focus in/out
                1007 => self.alt_scroll = set,            // alt-screen wheel → arrow keys
                2004 => self.bracketed_paste = set,       // bracketed paste
                _ => {}
            }
        }
    }
    /// `Off` if `active` is the current mouse mode, else leave it unchanged — matches
    /// alacritty removing only the specific mode bit on DECRST.
    fn mouse_off_if(&self, active: MouseMode) -> MouseMode {
        if self.mouse_mode == active {
            MouseMode::Off
        } else {
            self.mouse_mode
        }
    }

    /// DECSCUSR (`CSI Ps SP q`): 0/1/2 = block, 3/4 = underline, 5/6 = bar. Blink ignored.
    fn set_cursor_shape(&mut self, ps: u16) {
        self.cursor_shape = match ps {
            3 | 4 => CursorShape::Underline,
            5 | 6 => CursorShape::Beam,
            _ => CursorShape::Block,
        };
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
        && !c.wrapline
        && c.fg == Color::Default
        && c.bg == Color::Default
        // alacritty's is_empty ignores bold/dim/italic; only these mark a blank non-empty.
        && !c.attrs.inverse
        && !c.attrs.underline
        && !c.attrs.strikeout
}

/// The cursor state carried through a column reflow: visible line (may go transiently
/// negative before clamping), column, and the deferred-wrap flag (alacritty's
/// `input_needs_wrap`). Mirrors `self.cursor.point` + `input_needs_wrap` in resize.rs.
struct RCursor {
    line: i32,
    col: usize,
    wrap: bool,
}

/// All cells empty — alacritty's content-based `Row::is_clear`.
fn reflow_is_clear(row: &[Cell]) -> bool {
    row.iter().all(cell_is_empty)
}
/// A double-width glyph (alacritty's `WIDE_CHAR`).
fn reflow_is_wide(c: &Cell) -> bool {
    c.c.width() == Some(2)
}
/// A *leading* wide-char spacer (alacritty's `LEADING_WIDE_CHAR_SPACER`): a spacer with no
/// wide glyph immediately before it (as opposed to a wide glyph's trailing spacer).
fn reflow_is_leading_spacer(row: &[Cell], i: usize) -> bool {
    row[i].spacer && (i == 0 || row[i - 1].c.width() != Some(2))
}
/// Split cells beyond `columns` off `row`, trimming trailing empties from the remainder —
/// alacritty's `Row::shrink`. Returns the (non-empty) overflow, or `None` if it all fits.
fn reflow_shrink_row(row: &mut Vec<Cell>, columns: usize) -> Option<Vec<Cell>> {
    if row.len() <= columns {
        return None;
    }
    let mut new_row = row.split_off(columns);
    let idx = new_row.iter().rposition(|c| !cell_is_empty(c)).map_or(0, |i| i + 1);
    new_row.truncate(idx);
    if new_row.is_empty() { None } else { Some(new_row) }
}
/// Remove and return the first `at` cells — alacritty's `Row::front_split_off`.
fn reflow_front_split_off(row: &mut Vec<Cell>, at: usize) -> Vec<Cell> {
    let mut split = row.split_off(at);
    std::mem::swap(&mut split, row);
    split
}
/// Clamp a cursor point to the grid — alacritty's `Point::grid_clamp(Boundary::Cursor)`:
/// column to `columns-1`; a line above the top collapses to `(0,0)`, below the bottom to
/// the bottom-right.
fn reflow_grid_clamp(line: i32, col: usize, columns: usize, lines: usize) -> (usize, usize) {
    let col = col.min(columns - 1);
    if line < 0 {
        (0, 0)
    } else if line > lines as i32 - 1 {
        (lines - 1, columns - 1)
    } else {
        (line as usize, col)
    }
}
/// Subtract `rhs` columns from a cursor point, moving to previous lines — alacritty's
/// `Point::sub(Boundary::Cursor)`. Returns the clamped `(line, col)`.
fn reflow_point_sub(line: i32, col: usize, rhs: usize, columns: usize, lines: usize) -> (i32, usize) {
    let line_changes = (rhs + columns - 1).saturating_sub(col) / columns;
    let nline = line - line_changes as i32;
    let ncol = (columns + col - rhs % columns) % columns;
    let (l, c) = reflow_grid_clamp(nline, ncol, columns, lines);
    // grid_clamp only lowers the line to 0 (never below), so `l` fits i32 losslessly.
    (l as i32, c)
}

/// Grow the column count, reflowing wrapped rows — a faithful port of alacritty's
/// `grow_columns` (resize.rs 101-242). `rows` and the return are height-indexed from the
/// bottom; `display_offset` is 0 (we always observe at the viewport bottom) so its
/// branches are inert and omitted.
fn grow_columns_impl(
    rows: Vec<Vec<Cell>>,
    columns: usize,
    lines: usize,
    cur: &mut RCursor,
) -> Vec<Vec<Cell>> {
    let mut reversed: Vec<Vec<Cell>> = Vec::with_capacity(rows.len());
    let mut cursor_line_delta: i32 = 0;
    if cur.wrap {
        cur.wrap = false;
        cur.col += 1;
    }

    for (i, mut row) in rows.into_iter().enumerate().rev() {
        let should_reflow = reversed.last().map_or(false, |last: &Vec<Cell>| {
            let l = last.len();
            l > 0 && l < columns && last[l - 1].wrapline
        });
        if !should_reflow {
            reversed.push(row);
            continue;
        }

        {
            let last_row = reversed.last_mut().unwrap();
            if let Some(cell) = last_row.last_mut() {
                cell.wrapline = false;
            }
            let mut last_len = last_row.len();
            if last_len >= 1 && reflow_is_leading_spacer(last_row, last_len - 1) {
                last_row.truncate(last_len - 1);
                last_len -= 1;
            }
            let mut num_wrapped = columns - last_len;
            let len = row.len().min(num_wrapped);
            let mut cells = if reflow_is_wide(&row[len - 1]) {
                num_wrapped -= 1;
                let mut cells = reflow_front_split_off(&mut row, len - 1);
                let mut spacer = Cell::default();
                spacer.spacer = true;
                cells.push(spacer);
                cells
            } else {
                reflow_front_split_off(&mut row, len)
            };
            last_row.append(&mut cells);

            let cursor_buffer_line = lines as i32 - cur.line - 1;
            if i as i32 == cursor_buffer_line {
                let (mut tline, mut tcol) =
                    reflow_point_sub(cur.line, cur.col, num_wrapped, columns, lines);
                if tcol == 0 && reflow_is_clear(&row) {
                    cur.wrap = true;
                    let (l2, c2) = reflow_point_sub(tline, tcol, 1, columns, lines);
                    tline = l2;
                    tcol = c2;
                }
                cur.col = tcol;
                let line_delta = cur.line - tline;
                if line_delta != 0 && reflow_is_clear(&row) {
                    continue;
                }
                cursor_line_delta += line_delta;
            } else if reflow_is_clear(&row) {
                if (i as i32) < cursor_buffer_line {
                    cur.line += 1;
                }
                continue;
            }

            if let Some(cell) = last_row.last_mut() {
                cell.wrapline = true;
            }
        }
        reversed.push(row);
    }

    if reversed.len() < lines {
        let delta = (lines - reversed.len()) as i32;
        cur.line = (cur.line - delta).max(0);
        reversed.resize_with(lines, || vec![Cell::default(); columns]);
    }

    if cursor_line_delta != 0 {
        let cursor_buffer_line = lines as i32 - cur.line - 1;
        let available = (cursor_buffer_line.max(0) as usize).min(reversed.len() - lines);
        let overflow = (cursor_line_delta as usize).saturating_sub(available);
        let new_len = reversed.len() + overflow - cursor_line_delta as usize;
        reversed.truncate(new_len);
        cur.line = (cur.line - overflow as i32).max(0);
    }

    let mut new_raw: Vec<Vec<Cell>> = Vec::with_capacity(reversed.len());
    for mut row in reversed.into_iter().rev() {
        if row.len() < columns {
            row.resize(columns, Cell::default());
        }
        new_raw.push(row);
    }
    new_raw
}

/// Shrink the column count, reflowing overflow down into new rows — a faithful port of
/// alacritty's `shrink_columns` (resize.rs 245-388). Height-indexed from the bottom;
/// `display_offset` is 0 so its branches are inert and omitted. The final cursor clamp
/// (374-388) is applied by the caller.
fn shrink_columns_impl(
    rows: Vec<Vec<Cell>>,
    columns: usize,
    lines: usize,
    cur: &mut RCursor,
) -> Vec<Vec<Cell>> {
    if cur.wrap {
        cur.wrap = false;
        cur.col += 1;
    }
    let mut new_raw: Vec<Vec<Cell>> = Vec::with_capacity(rows.len());
    let mut buffered: Option<Vec<Cell>> = None;

    for (i, mut row) in rows.into_iter().enumerate().rev() {
        if let Some(buf) = buffered.take() {
            let cursor_buffer_line = lines as i32 - cur.line - 1;
            if i as i32 == cursor_buffer_line {
                cur.col += buf.len();
            }
            let mut front = buf;
            front.extend(row);
            row = front;
        }

        loop {
            let mut wrapped = match reflow_shrink_row(&mut row, columns) {
                Some(w) => w,
                None => {
                    let cursor_buffer_line = lines as i32 - cur.line - 1;
                    if i as i32 == cursor_buffer_line && cur.col > columns {
                        Vec::new()
                    } else {
                        new_raw.push(row);
                        break;
                    }
                }
            };

            if row.len() >= columns && reflow_is_wide(&row[columns - 1]) {
                let mut spacer = Cell::default();
                spacer.spacer = true;
                let wide = std::mem::replace(&mut row[columns - 1], spacer);
                wrapped.insert(0, wide);
            }

            let len = wrapped.len();
            if len > 0 && reflow_is_leading_spacer(&wrapped, len - 1) {
                if len == 1 {
                    row[columns - 1].wrapline = true;
                    new_raw.push(row);
                    break;
                } else {
                    wrapped[len - 2].wrapline = true;
                    wrapped.truncate(len - 1);
                }
            }

            new_raw.push(row);
            if let Some(cell) = new_raw.last_mut().and_then(|r| r.last_mut()) {
                cell.wrapline = true;
            }

            if wrapped.last().map_or(false, |c| c.wrapline && i >= 1) && wrapped.len() < columns {
                if let Some(cell) = wrapped.last_mut() {
                    cell.wrapline = false;
                }
                buffered = Some(wrapped);
                break;
            } else {
                let cursor_buffer_line = lines as i32 - cur.line - 1;
                if (i as i32 == cursor_buffer_line && cur.col < columns) || (i as i32) < cursor_buffer_line {
                    cur.line = (cur.line - 1).max(0);
                }
                if i as i32 == cursor_buffer_line && cur.col >= columns {
                    cur.col -= columns;
                }
                if wrapped.len() < columns {
                    wrapped.resize(columns, Cell::default());
                }
                row = wrapped;
            }
        }
    }

    let mut reversed: Vec<Vec<Cell>> = new_raw.into_iter().rev().collect();
    reversed.truncate(SCROLLBACK + lines);
    reversed
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

    /// OSC handler. Only the window title (OSC 0 = icon+title, OSC 2 = title) is applied;
    /// other OSCs (icon name 1, colours, clipboard, hyperlinks) are parsed but ignored.
    fn osc_dispatch(&mut self, params: &[&[u8]], _bell_terminated: bool) {
        if let (Some(&num), Some(&val)) = (params.first(), params.get(1)) {
            if num == b"0" || num == b"2" {
                self.title = Some(String::from_utf8_lossy(val).into_owned());
            }
        }
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
            match (intermediates.first(), action) {
                (Some(&b'?'), 'h') => self.set_mode(&p, true),
                (Some(&b'?'), 'l') => self.set_mode(&p, false),
                // DECSCUSR: CSI Ps SP q (intermediate = space).
                (Some(&b' '), 'q') => self.set_cursor_shape(p.first().copied().unwrap_or(0)),
                _ => {}
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
            self.soft_wrap();
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
