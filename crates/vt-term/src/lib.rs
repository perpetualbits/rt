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
/// Default scrollback cap (lines), matching the vendored oracle's `scrolling_history`.
/// A live `Term` raises this (and adds a memory budget) via [`Term::set_scrollback`];
/// the differential harness uses `Term::new`, so it keeps this default and tracks the
/// oracle exactly.
const DEFAULT_SCROLLBACK: usize = 10_000;

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

/// Display width of `c` with an inline fast path for printable ASCII (`0x20..=0x7e`, always
/// width 1) that skips the unicode-width table lookup — the dominant per-glyph cost in the
/// print hot path, and especially on in-order cores (riscv).
#[inline]
fn char_width(c: char) -> usize {
    if matches!(c, ' '..='~') {
        1
    } else {
        c.width().unwrap_or(0)
    }
}

// Packed [`Cell`] flag bits: the seven SGR attributes, plus the wide-glyph `SPACER` and
// soft-wrap `WRAPLINE` structural markers. All live in one `u16` word.
const BOLD: u16 = 1 << 0;
const ITALIC: u16 = 1 << 1;
const UNDERLINE: u16 = 1 << 2;
const INVERSE: u16 = 1 << 3;
const DIM: u16 = 1 << 4;
const HIDDEN: u16 = 1 << 5;
const STRIKEOUT: u16 = 1 << 6;
/// Trailing/leading spacer of a wide glyph — invisible (`c == ' '`) but NOT empty;
/// overwriting it clears the wide glyph beside it (alacritty's WIDE_CHAR_SPACER /
/// LEADING_WIDE_CHAR_SPACER).
const SPACER: u16 = 1 << 7;
/// Last cell of a soft-wrapped (autowrapped) row, as opposed to a hard break; reflow joins
/// rows across it, and it makes the cell non-empty (alacritty's WRAPLINE).
const WRAPLINE: u16 = 1 << 8;
/// The SGR-attribute bits (bold…strikeout) a printed cell inherits from the pen; the
/// structural SPACER/WRAPLINE bits are per-cell and never copied from the pen.
const ATTR_MASK: u16 = BOLD | ITALIC | UNDERLINE | INVERSE | DIM | HIDDEN | STRIKEOUT;

/// One grid cell: a character, its resolved fg/bg, and packed rendition/structural flags.
/// 16 bytes (`char` + two `Color`s + a `u16`) — smaller than alacritty's `Cell` (which also
/// carries an `Option<Arc<..>>`), keeping the memcpy on fills/scrolls/clones cheap. Read the
/// flags via the accessor methods; module-internal code touches the `flags` bits directly.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Cell {
    pub c: char,
    pub fg: Color,
    pub bg: Color,
    flags: u16,
}

impl Default for Cell {
    fn default() -> Self {
        Cell { c: ' ', fg: Color::Default, bg: Color::Default, flags: 0 }
    }
}

impl Cell {
    pub fn bold(&self) -> bool {
        self.flags & BOLD != 0
    }
    pub fn italic(&self) -> bool {
        self.flags & ITALIC != 0
    }
    pub fn underline(&self) -> bool {
        self.flags & UNDERLINE != 0
    }
    pub fn inverse(&self) -> bool {
        self.flags & INVERSE != 0
    }
    pub fn dim(&self) -> bool {
        self.flags & DIM != 0
    }
    pub fn hidden(&self) -> bool {
        self.flags & HIDDEN != 0
    }
    pub fn strikeout(&self) -> bool {
        self.flags & STRIKEOUT != 0
    }
    pub fn spacer(&self) -> bool {
        self.flags & SPACER != 0
    }
    pub fn wrapline(&self) -> bool {
        self.flags & WRAPLINE != 0
    }
}

/// One grid row: its cells plus `occ`, the number of cells written since the last reset.
/// Cells at or beyond `occ` are guaranteed to equal the last reset template, so a clear
/// only has to touch the `[0, occ)` prefix — alacritty's `Row` trick, the dominant cost on
/// clear-heavy TUIs. Because `occ` lives *in* the row, it travels through scrolls and
/// scrollback for free. `occ` is never observed (the grid is compared cell-by-cell); it is
/// purely a clear-cost optimisation, so a too-large `occ` is merely slower, and the only
/// invariant that matters is `occ >= (highest written column + 1)`.
#[derive(Clone)]
struct Line {
    cells: Vec<Cell>,
    occ: usize,
}

impl Line {
    /// A fresh blank row (nothing written yet).
    fn blank(cols: usize) -> Line {
        Line { cells: vec![Cell::default(); cols], occ: 0 }
    }
    /// Wrap an existing cell vector, treating all of it as occupied — used at the reflow /
    /// alt-restore boundary where the precise write history is unknown (a safe upper bound).
    fn from_cells(cells: Vec<Cell>) -> Line {
        let occ = cells.len();
        Line { cells, occ }
    }
    #[inline]
    fn iter(&self) -> std::slice::Iter<'_, Cell> {
        self.cells.iter()
    }
    /// Estimated bytes this row occupies (heap cells + inline struct), for the scrollback
    /// memory budget. Uses `capacity` so the estimate reflects what is actually held.
    #[inline]
    fn byte_size(&self) -> usize {
        self.cells.capacity() * std::mem::size_of::<Cell>() + std::mem::size_of::<Line>()
    }
    /// Reset the row to `template`, touching only the occupied prefix. If the trailing
    /// (beyond-`occ`) cells' background differs from the template they are stale, so the
    /// whole row is reset (alacritty's discriminant check; the blank template only ever
    /// varies in `bg`, so comparing `bg` identifies the last template exactly).
    #[inline]
    fn reset(&mut self, template: Cell) {
        if self.cells.last().map_or(false, |c| c.bg != template.bg) {
            self.occ = self.cells.len();
        }
        for cell in &mut self.cells[..self.occ] {
            *cell = template;
        }
        self.occ = 0;
    }
    /// Grow/shrink to `cols`, keeping content (new cells are default).
    fn resize(&mut self, cols: usize) {
        self.cells.resize(cols, Cell::default());
        self.occ = self.occ.min(cols);
    }
}

impl std::ops::Index<usize> for Line {
    type Output = Cell;
    #[inline]
    fn index(&self, i: usize) -> &Cell {
        &self.cells[i]
    }
}
impl std::ops::IndexMut<usize> for Line {
    #[inline]
    fn index_mut(&mut self, i: usize) -> &mut Cell {
        // Any mutable access is treated as a write, extending the occupied prefix.
        self.occ = self.occ.max(i + 1);
        &mut self.cells[i]
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
    grid: Vec<Line>,
    // Primary-screen state saved while the alternate screen is active.
    saved_screen: Option<(Vec<Line>, usize, usize, Cell, bool, [Charset; 4])>, // + designations
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
    /// Lines scrolled off the top of the primary screen (oldest at the front), capped by
    /// `scrollback_lines` and `scrollback_bytes`. Only its length is observed by the
    /// differential harness; it exists so `history_size` tracks the oracle and scrolling
    /// can read it.
    history: VecDeque<Line>,
    /// Scrollback caps: evict the oldest history when EITHER the line count exceeds
    /// `scrollback_lines` OR the running byte estimate exceeds `scrollback_bytes`. The
    /// defaults (`DEFAULT_SCROLLBACK` lines, `usize::MAX` bytes) cap purely by line count
    /// like the oracle, so the harness is unaffected; a host raises both via
    /// [`set_scrollback`](Term::set_scrollback) so a huge line setting can't exhaust RAM.
    scrollback_lines: usize,
    scrollback_bytes: usize,
    /// Running estimate of `history`'s memory, kept in step with every push/pop so the
    /// byte budget is O(1) to enforce (recomputed wholesale after a reflow rebuilds it).
    history_bytes: usize,
    /// Recycled blank rows (all-default, `occ` 0), fed by scrollback eviction and drained
    /// by scrolling — so a top-anchored scroll MOVES the scrolled-off row into history
    /// (no clone) and takes a ready blank from here instead of allocating one. This kills
    /// the per-line malloc+copy that dominated scroll-heavy workloads on in-order cores.
    blank_pool: Vec<Line>,
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
            grid: (0..rows).map(|_| Line::blank(cols)).collect(),
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
            scrollback_lines: DEFAULT_SCROLLBACK,
            scrollback_bytes: usize::MAX,
            history_bytes: 0,
            blank_pool: Vec::new(),
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
        self.blank_pool.clear(); // pooled blanks are the old width
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
            row.resize(new_cols);
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
                self.history_bytes += line.byte_size();
                self.history.push_back(line); // primary: into scrollback; alt: discard
            }
        }
        self.trim_history(); // enforce both caps, recycling evicted rows
        self.row -= scroll;
        self.grid.truncate(target);
        while self.grid.len() < target {
            self.grid.push(Line::blank(self.cols));
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
            self.history_bytes = self.history_bytes.saturating_sub(line.byte_size());
            self.grid.insert(0, line);
        }
        self.row += from_history;
        for _ in 0..(added - from_history) {
            self.grid.push(Line::blank(self.cols));
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
        let rows: Vec<Vec<Cell>> = self
            .grid
            .iter()
            .rev()
            .map(|l| l.cells.clone())
            .chain(self.history.iter().rev().map(|l| l.cells.clone()))
            .collect();
        let mut cur = RCursor { line: self.row as i32, col: self.col, wrap: self.pending_wrap };

        let nb = if new_cols > old_cols {
            grow_columns_impl(rows, new_cols, lines, &mut cur)
        } else {
            shrink_columns_impl(rows, new_cols, lines, self.scrollback_lines, &mut cur)
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
        while history.len() > self.scrollback_lines {
            history.pop_front();
        }

        // Final cursor reflow (resize.rs 374-388): pending-wrap at the width, else clamp.
        let cline = cur.line.clamp(0, lines as i32 - 1) as usize;
        let at_wrap = grid[cline].get(new_cols - 1).map(|c| c.wrapline()).unwrap_or(false);
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

        self.history = history.into_iter().map(Line::from_cells).collect();
        // The rebuilt history was capped by line count above; recompute its byte estimate
        // and enforce the memory budget too (a host may have a tighter one).
        self.history_bytes = self.history.iter().map(Line::byte_size).sum();
        self.trim_history();
        self.grid = grid.into_iter().map(Line::from_cells).collect();
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
        row.cells.get(col).copied().unwrap_or_default()
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
        Cell { c: ' ', fg: Color::Default, bg: self.pen.bg, flags: 0 }
    }
    /// Reset row `r` to `blank` in place, keeping its heap allocation — the scroll/erase
    /// hot path. Replacing the `Vec` (`grid[r] = blank_row()`) allocated a fresh row every
    /// time; reusing the buffer is what alacritty's ring-buffer reset does.
    #[inline]
    fn fill_row(&mut self, r: usize, blank: Cell) {
        self.grid[r].reset(blank); // occ-bounded: only touches written cells
    }

    /// Scroll the scroll region up by `n` lines (content moves up; blanks fill in at
    /// the bottom of the region).
    /// Push a line into scrollback (capped), and if the view is scrolled up, bump the
    /// offset so it stays anchored on the same content (alacritty's `increase_scroll_limit`).
    fn push_history(&mut self, line: Line) {
        self.history_bytes += line.byte_size();
        self.history.push_back(line);
        let before = self.history.len();
        self.trim_history(); // evict oldest until within both caps (recycling rows)
        let evicted = before - self.history.len();
        // Keep a scrolled-up view anchored on the same content: the new bottom line pushes
        // the view up by one, each front eviction pulls it back down (steady state: +1-1=0).
        if self.display_offset > 0 {
            self.display_offset = (self.display_offset + 1).saturating_sub(evicted).min(self.history.len());
        }
    }

    /// Configure this pane's scrollback caps: keep at most `lines` history lines and at
    /// most `bytes` estimated bytes, whichever is tighter. A host calls this with the
    /// user's configured limit and a memory budget, so a large line setting degrades
    /// (oldest-first eviction) instead of exhausting RAM. Trims immediately if over.
    pub fn set_scrollback(&mut self, lines: usize, bytes: usize) {
        self.scrollback_lines = lines;
        self.scrollback_bytes = bytes;
        self.trim_history();
        self.display_offset = self.display_offset.min(self.history.len());
    }

    /// Evict the oldest history until within BOTH caps, recycling each evicted row's
    /// allocation and keeping `history_bytes` in step. The byte cap never empties a
    /// non-empty history below its last line (a single oversized row is still kept).
    fn trim_history(&mut self) {
        while self.history.len() > self.scrollback_lines
            || (self.history.len() > 1 && self.history_bytes > self.scrollback_bytes)
        {
            match self.history.pop_front() {
                Some(popped) => {
                    self.history_bytes = self.history_bytes.saturating_sub(popped.byte_size());
                    self.recycle_blank(popped); // reuse the evicted row's allocation
                }
                None => break,
            }
        }
    }

    /// A ready blank row (`occ` 0, all-default) from the recycle pool, or a fresh one.
    fn take_blank(&mut self) -> Line {
        match self.blank_pool.pop() {
            Some(row) if row.cells.len() == self.cols => row,
            _ => Line::blank(self.cols),
        }
    }

    /// Return a no-longer-needed row to the recycle pool, cleared to a clean default blank
    /// (occ-bounded, so cheap). Dropped if it is the wrong width or the pool is full.
    fn recycle_blank(&mut self, mut row: Line) {
        if row.cells.len() == self.cols && self.blank_pool.len() < self.rows + 2 {
            row.reset(Cell::default());
            self.blank_pool.push(row);
        }
    }

    fn scroll_up(&mut self, n: usize) {
        let (t, b) = (self.scroll_top, self.scroll_bottom);
        let n = n.min(b - t + 1);
        // History grows only for a top-anchored scroll on the primary screen (matching
        // alacritty's `scroll_up`: history increases iff `region.start == 0`; the alt
        // screen has no scrollback).
        if t == 0 && !self.alt && self.pen.bg == Color::Default {
            // Fast path: default erase colour → the scrolled-in blanks are plain defaults,
            // so MOVE each scrolled-off row into history (no clone) and swap in a pooled
            // blank. After the rotate, those blanks land at the bottom. No per-line malloc.
            for r in 0..n {
                let blank = self.take_blank();
                let row = std::mem::replace(&mut self.grid[r], blank);
                self.push_history(row);
            }
            self.grid[t..=b].rotate_left(n);
            return;
        }
        if t == 0 && !self.alt {
            // Coloured erase background: the blanks carry `pen.bg`, so keep the clone path.
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
        let (fg, bg, flags) = (self.pen.fg, self.pen.bg, self.pen.flags & ATTR_MASK);
        self.grid[self.row][self.col] = Cell { c, fg, bg, flags };
    }

    /// Write a wide-glyph spacer at the cursor: a blank that carries the pen colours/
    /// attrs and the `spacer` flag.
    fn write_spacer(&mut self) {
        let (fg, bg, flags) = (self.pen.fg, self.pen.bg, (self.pen.flags & ATTR_MASK) | SPACER);
        self.grid[self.row][self.col] = Cell { c: ' ', fg, bg, flags };
    }

    /// Clear the cells related to a wide glyph before overwriting the cursor cell — a
    /// faithful port of alacritty's `write_at_cursor` cleanup (`term/mod.rs`), which only
    /// runs when the cell being overwritten is a wide glyph or one of its spacers. Three
    /// cases: overwriting the glyph drops its trailing spacer to the right; overwriting a
    /// trailing spacer blanks the glyph to the left; and overwriting a wrapped wide glyph
    /// (now at column 0/1) clears the leading spacer it left in the previous row's last
    /// column. Missing the last case left a stray spacer that reflow misclassified — the
    /// wide-glyph column-shift divergence.
    fn clear_wide_left(&mut self) {
        let cur = self.grid[self.row][self.col];
        let wide = char_width(cur.c) == 2;
        if wide && self.col + 1 < self.cols {
            // Overwriting the wide glyph: drop its trailing spacer to the right.
            self.grid[self.row][self.col + 1].flags &= !SPACER;
        } else if self.col > 0 && cur.spacer() && char_width(self.grid[self.row][self.col - 1].c) == 2 {
            // Overwriting a trailing spacer: blank the wide glyph to its left.
            self.grid[self.row][self.col - 1].c = ' ';
        }
        // Overwriting a wrapped wide glyph (now at column 0/1): clear the leading spacer it
        // left in the previous row's last column. Missing this left a stray spacer that
        // column reflow misclassified — the wide-glyph one-column-shift divergence. Our one
        // SPACER bit can't distinguish a leading spacer from a trailing one (alacritty has
        // two flags), so only clear it when it is a *leading* spacer: its predecessor is
        // never a wide glyph (a trailing spacer's is), so `cols-2` not being wide identifies
        // it — else we would orphan a legitimate wide glyph at `cols-2`.
        if self.col <= 1
            && self.row > 0
            && (char_width(self.grid[self.row][self.col].c) == 2 || self.grid[self.row][self.col].spacer())
            && self.grid[self.row - 1][self.cols - 1].spacer()
            && (self.cols < 2 || char_width(self.grid[self.row - 1][self.cols - 2].c) != 2)
        {
            self.grid[self.row - 1][self.cols - 1].flags &= !SPACER;
        }
    }

    /// Soft (autowrap) line break: mark the current row's last cell WRAPLINE so reflow
    /// can rejoin it, then move to column 0 of the next line and clear the pending wrap.
    fn soft_wrap(&mut self) {
        self.grid[self.row][self.cols - 1].flags |= WRAPLINE;
        self.col = 0;
        self.line_feed();
        self.pending_wrap = false;
    }

    fn put_char(&mut self, c: char) {
        // Character display width (unicode-width): 0 = combining/zero-width, 1 = normal,
        // 2 = wide (CJK/emoji). Matches alacritty's `input`.
        let width = char_width(c);
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
            3 => {
                // ED-Saved: clear scrollback. Keep the byte accounting and viewport in step
                // — otherwise history_bytes counts bytes that no longer exist and the next
                // pushes get evicted prematurely. [review RT-TERM-002]
                self.history.clear();
                self.history_bytes = 0;
                self.display_offset = 0;
            }
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
                1 => self.pen.flags |= BOLD,
                2 => self.pen.flags |= DIM,
                3 => self.pen.flags |= ITALIC,
                4 => self.pen.flags |= UNDERLINE,
                7 => self.pen.flags |= INVERSE,
                8 => self.pen.flags |= HIDDEN,
                9 => self.pen.flags |= STRIKEOUT,
                22 => self.pen.flags &= !(BOLD | DIM),
                23 => self.pen.flags &= !ITALIC,
                24 => self.pen.flags &= !UNDERLINE,
                27 => self.pen.flags &= !INVERSE,
                28 => self.pen.flags &= !HIDDEN,
                29 => self.pen.flags &= !STRIKEOUT,
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
            let saved = std::mem::replace(&mut self.grid, (0..self.rows).map(|_| Line::blank(self.cols)).collect());
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
        // Preserve the host-configured scrollback policy across RIS — it is a property of
        // the pane, not terminal state, so a child printing `ESC c` must not silently reset
        // the line/byte caps back to the library defaults (matches alacritty, whose Config
        // is separate from the reset Term). [review RT-TERM-001]
        let (lines, bytes) = (self.scrollback_lines, self.scrollback_bytes);
        *self = Term::new(cols, rows);
        self.scrollback_lines = lines;
        self.scrollback_bytes = bytes;
    }
}

/// A cell alacritty treats as "empty" for the clear_viewport scan: a space or tab
/// glyph with the default foreground/background and no attributes.
fn cell_is_empty(c: &Cell) -> bool {
    (c.c == ' ' || c.c == '\t')
        && (c.flags & (SPACER | WRAPLINE)) == 0
        && c.fg == Color::Default
        && c.bg == Color::Default
        // alacritty's is_empty ignores bold/dim/italic; only these mark a blank non-empty.
        && (c.flags & (INVERSE | UNDERLINE | STRIKEOUT)) == 0
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
    char_width(c.c) == 2
}
/// A *leading* wide-char spacer (alacritty's `LEADING_WIDE_CHAR_SPACER`): a spacer with no
/// wide glyph immediately before it (as opposed to a wide glyph's trailing spacer).
fn reflow_is_leading_spacer(row: &[Cell], i: usize) -> bool {
    row[i].flags & SPACER != 0 && (i == 0 || char_width(row[i - 1].c) != 2)
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
            l > 0 && l < columns && last[l - 1].wrapline()
        });
        if !should_reflow {
            reversed.push(row);
            continue;
        }

        {
            let last_row = reversed.last_mut().unwrap();
            if let Some(cell) = last_row.last_mut() {
                cell.flags &= !WRAPLINE;
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
                spacer.flags |= SPACER;
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
                cell.flags |= WRAPLINE;
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
    max_history: usize,
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
                spacer.flags |= SPACER;
                let wide = std::mem::replace(&mut row[columns - 1], spacer);
                wrapped.insert(0, wide);
            }

            let len = wrapped.len();
            if len > 0 && reflow_is_leading_spacer(&wrapped, len - 1) {
                if len == 1 {
                    row[columns - 1].flags |= WRAPLINE;
                    new_raw.push(row);
                    break;
                } else {
                    wrapped[len - 2].flags |= WRAPLINE;
                    wrapped.truncate(len - 1);
                }
            }

            new_raw.push(row);
            if let Some(cell) = new_raw.last_mut().and_then(|r| r.last_mut()) {
                cell.flags |= WRAPLINE;
            }

            if wrapped.last().map_or(false, |c| c.wrapline() && i >= 1) && wrapped.len() < columns {
                if let Some(cell) = wrapped.last_mut() {
                    cell.flags &= !WRAPLINE;
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
    reversed.truncate(max_history + lines);
    reversed
}

/// First value of each parameter as a flat `Vec` (drops sub-parameters — the common
/// semicolon form; colon sub-parameters beyond the SGR extended-colour case are on the
/// ledger).
/// Flattened CSI parameters — the first sub-parameter of each — held in a fixed stack
/// array, so a CSI dispatch costs **no heap allocation** (the old `Vec` collect dominated
/// control-heavy workloads, one alloc per sequence). `Params` holds at most `FLAT_MAX`, and
/// `Deref` to `[u16]` keeps every call site (`count`, `sgr`, `set_mode`, …) unchanged.
const FLAT_MAX: usize = 32;
struct Flat {
    buf: [u16; FLAT_MAX],
    len: usize,
}
impl std::ops::Deref for Flat {
    type Target = [u16];
    #[inline]
    fn deref(&self) -> &[u16] {
        &self.buf[..self.len]
    }
}
#[inline]
fn flat(params: &Params) -> Flat {
    let mut buf = [0u16; FLAT_MAX];
    let mut len = 0;
    for g in params.iter() {
        if len >= FLAT_MAX {
            break;
        }
        buf[len] = g.first().copied().unwrap_or(0);
        len += 1;
    }
    Flat { buf, len }
}
/// Parameter `i` treated as a count: absent or 0 means 1 (the cursor-motion default).
fn count(p: &[u16], i: usize) -> usize {
    p.get(i).copied().unwrap_or(0).max(1) as usize
}

impl Perform for Term {
    fn print(&mut self, c: char) {
        self.put_char(c);
    }

    /// Batched print of a run of printable characters — the hot path for plain text. It
    /// produces cell-for-cell the same result as feeding each char through [`put_char`],
    /// but for the common case (ASCII charset, narrow width-1 glyphs) it writes a whole
    /// row-segment in a tight loop: no per-char charset map, no per-char `clear_wide_left`
    /// (it can't fire once we're overwriting our own just-written narrow cells), and one
    /// `occ` bump per segment instead of per cell. Wide/zero-width glyphs and the last-
    /// column autowrap boundary defer to `put_char`, so all the delicate edge behaviour
    /// stays in exactly one place.
    fn print_str(&mut self, s: &str) {
        // Only ASCII G0/GL is fast-pathed; a designated special-graphics set needs the
        // per-char mapping, so fall back wholesale (charsets can't change mid-run).
        if self.charsets[self.gl] != Charset::Ascii {
            for c in s.chars() {
                self.put_char(c);
            }
            return;
        }
        let cols = self.cols;
        let (fg, bg, flags) = (self.pen.fg, self.pen.bg, self.pen.flags & ATTR_MASK);
        let mut it = s.chars().peekable();
        while let Some(&c) = it.peek() {
            // Wide (2) or zero-width/combining (0) glyphs take the exact slow path.
            if char_width(c) != 1 {
                self.put_char(c);
                it.next();
                continue;
            }
            // Start of a width-1 segment: resolve a pending wrap and the one place a
            // wide-char spacer could sit under the cursor, exactly as put_char does.
            if self.pending_wrap {
                self.soft_wrap();
            }
            self.clear_wide_left();
            // Fill the row up to (but not including) the last column — none of these hit
            // the autowrap boundary, and each cell's left neighbour is a narrow glyph we
            // just wrote, so `clear_wide_left` would be a no-op.
            let mut col = self.col;
            {
                let row = &mut self.grid[self.row];
                while col < cols - 1 {
                    match it.peek() {
                        Some(&c) if char_width(c) == 1 => {
                            row.cells[col] = Cell { c, fg, bg, flags };
                            col += 1;
                            it.next();
                        }
                        _ => break,
                    }
                }
                row.occ = row.occ.max(col); // one bump for the whole segment
            }
            self.col = col;
            // At the last column, the next width-1 glyph needs put_char's edge handling
            // (write here, then set/defer the pending wrap per DECAWM).
            if self.col == cols - 1 {
                if let Some(&c) = it.peek() {
                    if char_width(c) == 1 {
                        self.put_char(c);
                        it.next();
                    }
                }
            }
        }
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
            'I' => self.cursor_forward_tabs(count(&p, 0)), // CHT
            'Z' => self.cursor_backward_tabs(count(&p, 0)), // CBT
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

    /// CHT — advance the cursor to the next tab stop, `count` times (every-8 stops, matching
    /// `put_tab`), clamped to the last column. Cursor-only (no `\t` glyph). alacritty's
    /// `move_forward_tabs`.
    fn cursor_forward_tabs(&mut self, count: usize) {
        for _ in 0..count {
            if self.col + 1 >= self.cols {
                break;
            }
            self.col = (((self.col / 8) + 1) * 8).min(self.cols - 1);
        }
    }

    /// CBT — move the cursor back to the previous tab stop, `count` times. alacritty's
    /// `move_backward_tabs`.
    fn cursor_backward_tabs(&mut self, count: usize) {
        for _ in 0..count {
            if self.col == 0 {
                break;
            }
            self.col = ((self.col - 1) / 8) * 8;
        }
    }
}

#[cfg(test)]
mod scrollback_tests {
    use super::*;

    fn feed_lines(t: &mut Term, n: usize) {
        for _ in 0..n {
            t.feed(b"line of text\r\n"); // each LF past the bottom scrolls one row into history
        }
    }

    #[test]
    fn line_cap_bounds_history() {
        let mut t = Term::new(80, 24);
        t.set_scrollback(50, usize::MAX); // no byte budget: pure line cap
        feed_lines(&mut t, 500);
        assert_eq!(t.history_size(), 50, "history is capped at the configured line count");
    }

    #[test]
    fn default_cap_matches_oracle_default() {
        // No set_scrollback: the differential-harness path. Must stay at the oracle default.
        let mut t = Term::new(80, 24);
        feed_lines(&mut t, DEFAULT_SCROLLBACK + 24 + 100);
        assert_eq!(t.history_size(), DEFAULT_SCROLLBACK);
    }

    #[test]
    fn byte_budget_evicts_before_line_cap() {
        let mut t = Term::new(80, 24);
        let budget = 64 * 1024; // 64 KiB: a tiny memory budget under a huge line cap
        t.set_scrollback(1_000_000, budget);
        feed_lines(&mut t, 5_000);
        assert!(t.history_size() < 1_000_000, "the memory budget bounds history below the line cap");
        assert!(
            t.history_bytes <= budget || t.history_size() <= 1,
            "history memory ({}) stays within the {}-byte budget",
            t.history_bytes, budget
        );
    }

    #[test]
    fn ris_preserves_the_configured_scrollback_policy() {
        // RIS (ESC c) is terminal state, not pane policy — a child must not reset the caps.
        let mut t = Term::new(80, 24);
        t.set_scrollback(500, 1_234_567);
        t.feed(b"\x1bc"); // RIS
        assert_eq!(t.scrollback_lines, 500, "RIS must keep the configured line cap");
        assert_eq!(t.scrollback_bytes, 1_234_567, "RIS must keep the configured byte budget");
    }

    #[test]
    fn ed3_clears_history_and_its_byte_accounting() {
        let mut t = Term::new(80, 24);
        feed_lines(&mut t, 100);
        assert!(t.history_size() > 0 && t.history_bytes > 0, "history should have built up");
        t.feed(b"\x1b[3J"); // ED-Saved: clear scrollback
        assert_eq!(t.history_size(), 0, "ED 3 clears the history");
        assert_eq!(t.history_bytes, 0, "ED 3 must reset the byte accounting with it");
        assert_eq!(t.display_offset, 0, "ED 3 must snap the viewport to the (now empty) bottom");
    }
}
