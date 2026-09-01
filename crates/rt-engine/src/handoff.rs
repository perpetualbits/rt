//! Converting a live `vt_term::Term` into the frozen `rt_handoff` wire model.
//!
//! Export is a pure read: it takes the terminal by shared reference, mutates
//! nothing, and consumes nothing. The renderer's `Snapshot` type is NOT used
//! here — it resolves colours to RGB, and the wire must keep an indexed colour
//! indexed so a moved pane still follows the receiving window's palette.

use rt_handoff::grid::{line_flags, run_flags, Line, Run};
use rt_handoff::style::{attrs, Colour, Style};
use vt_term::{Cell, Color};

/// Interns styles so identical cells share one table entry, which is what
/// makes a run-length encoding of a screen small.
#[derive(Debug, Default)]
pub struct StyleTable {
    styles: Vec<Style>,
}

impl StyleTable {
    pub fn new() -> Self {
        StyleTable { styles: Vec::new() }
    }

    /// The id for this cell's style, adding it to the table if new.
    pub fn intern(&mut self, cell: &Cell) -> u32 {
        let s = style_of(cell);
        if let Some(i) = self.styles.iter().position(|e| *e == s) {
            return i as u32;
        }
        self.styles.push(s);
        (self.styles.len() - 1) as u32
    }

    pub fn into_vec(self) -> Vec<Style> {
        self.styles
    }
}

/// vt-term's colour to the wire's. An index stays an index — resolving it to
/// RGB here would stop a moved pane following the receiver's palette, and the
/// index could never be recovered.
fn colour_of(c: Color) -> Colour {
    match c {
        Color::Default => Colour::Default,
        Color::Indexed(i) => Colour::Indexed(i),
        Color::Rgb(r, g, b) => Colour::Rgb(r, g, b),
    }
}

/// The seven attributes this engine tracks. It has no blink, overline, or
/// underline styles beyond plain, and no underline colour or hyperlink id.
fn style_of(cell: &Cell) -> Style {
    let mut a = 0u32;
    if cell.bold() {
        a |= attrs::BOLD;
    }
    if cell.dim() {
        a |= attrs::DIM;
    }
    if cell.italic() {
        a |= attrs::ITALIC;
    }
    if cell.underline() {
        a |= attrs::UNDERLINE;
    }
    if cell.inverse() {
        a |= attrs::REVERSE;
    }
    if cell.hidden() {
        a |= attrs::HIDDEN;
    }
    if cell.strikeout() {
        a |= attrs::STRIKEOUT;
    }
    Style {
        fg: colour_of(cell.fg),
        bg: colour_of(cell.bg),
        underline: Colour::Default,
        attrs: a,
        link_id: 0,
    }
}

/// True for the second half of a wide glyph — and NOT for the before-wrap
/// placeholder `put_char` writes when a double-width glyph will not fit the
/// last column. Both set `spacer()`; only the placeholder sets `leading`.
/// Confusing them tags a narrow glyph as double-width, and the wire's own
/// invariants (text length vs. cell count) cannot detect it: both readings
/// still balance.
fn is_trailing_spacer(cell: &Cell) -> bool {
    cell.spacer() && !cell.leading_spacer()
}

/// True when `cells[i]` is the second half of the wide glyph immediately to its
/// left — the only spacer a run may absorb.
///
/// Both halves of the test matter. A spacer whose left neighbour is NOT a
/// double-width glyph is an ORPHAN: vt-term's `delete_chars`, `erase_chars` and
/// insert-mode shift move cells raw, with no wide-glyph cleanup (deliberate —
/// alacritty does the same, `vt-term/src/lib.rs`), so `日x` + `CSI 1P` leaves
/// `[spacer][x]` with the glyph gone. An orphan still occupies its column, so
/// absorbing it would shorten the row and shift everything to its right one
/// column left.
fn is_glyph_spacer(cells: &[Cell], i: usize) -> bool {
    is_trailing_spacer(&cells[i]) && i > 0 && cells[i - 1].is_wide()
}

/// True for a cell that contributes nothing but its column: a space in a style
/// that renders identically to the default.
///
/// Called only for cells that are not a glyph's own spacer (see
/// [`is_glyph_spacer`]), so the spacers that reach it — the before-wrap
/// placeholder, and orphans left behind by a raw cell shift — are judged
/// exactly as the invisible one-column blanks they are. If such a spacer
/// carries a non-default background it is not blank and travels as a real
/// space, which is what it paints.
fn is_blank(cell: &Cell) -> bool {
    cell.c == ' ' && style_of(cell) == Style::default()
}

/// One row of live cells to one wire line.
///
/// `cells` is the whole row, `cells.len()` columns wide. A cell's WIDTH comes
/// from its own character (`Cell::is_wide`), never from whether its neighbour
/// happens to carry the spacer flag: the grid can hold a glyph with no spacer
/// and a spacer with no glyph, and reading the neighbour turns both into wrong
/// content — a narrow glyph tagged double-width, or a row a column short.
///
/// A well-formed wide glyph's trailing spacer is absorbed into that glyph's run
/// and never emitted on its own; every other cell, spacer or not, contributes
/// exactly one column.
pub fn row_to_line(cells: &[Cell], styles: &mut StyleTable) -> Line {
    let wrapped = cells.last().map(|c| c.wrapline()).unwrap_or(false);
    let flags = if wrapped { line_flags::WRAPPED } else { 0 };

    // Which cells belong to the glyph on their left; computed once, because the
    // trim below and both loops must agree on it.
    let absorbed: Vec<bool> = (0..cells.len()).map(|i| is_glyph_spacer(cells, i)).collect();

    // Drop the tail of blank default cells; the receiver pads the line back out.
    // An absorbed spacer at the end goes with them: its glyph's run still spans
    // both columns, so nothing is lost.
    let mut end = cells.len();
    while end > 0 && (absorbed[end - 1] || is_blank(&cells[end - 1])) {
        end -= 1;
    }

    let mut runs: Vec<Run> = Vec::new();
    let mut i = 0usize;
    while i < end {
        if absorbed[i] {
            i += 1; // its glyph's run already covered this column
            continue;
        }
        let cell = &cells[i];
        let style_id = styles.intern(cell);
        let wide = cell.is_wide();

        if is_blank(cell) {
            // A stretch of blanks in one style: no text, just a span. A blank is
            // never wide (its character is a space), so no glyph is swallowed.
            let start = i;
            while i < end && !absorbed[i] && is_blank(&cells[i]) {
                i += 1;
            }
            runs.push(Run::blank(style_id, (i - start) as u32));
            continue;
        }

        // A run of same-style, same-width, non-blank cells.
        let mut text = String::new();
        let mut span = 0u32;
        while i < end {
            if absorbed[i] {
                i += 1; // the spacer of the wide glyph this run just took
                continue;
            }
            let c = &cells[i];
            if styles.intern(c) != style_id || c.is_wide() != wide || is_blank(c) {
                break;
            }
            text.push(c.c);
            span += if wide { 2 } else { 1 };
            i += 1;
        }
        runs.push(Run {
            flags: if wide { run_flags::WIDE } else { 0 },
            style_id,
            cell_span: span,
            text,
            char_counts: Vec::new(),
        });
    }

    Line { flags, runs }
}

use rt_handoff::grid::Grid;
use vt_term::Term;

/// The currently visible screen, one wire line per row.
pub fn screen_to_grid(term: &Term, styles: &mut StyleTable) -> Grid {
    let cols = term.cols();
    let lines = (0..term.rows())
        .map(|r| {
            let cells: Vec<Cell> = (0..cols).map(|c| term.cell(r, c)).collect();
            row_to_line(&cells, styles)
        })
        .collect();
    Grid { lines }
}

/// The screen held aside while the other one is displayed, if any.
pub fn inactive_to_grid(term: &Term, styles: &mut StyleTable) -> Option<Grid> {
    if !term.has_inactive_screen() {
        return None;
    }
    let rows = term.inactive_rows()?;
    let cols = term.cols();
    let lines = (0..rows)
        .map(|r| {
            let cells: Vec<Cell> =
                (0..cols).map(|c| term.inactive_cell(r, c).unwrap_or_default()).collect();
            row_to_line(&cells, styles)
        })
        .collect();
    Some(Grid { lines })
}

/// Scrollback above the visible screen, NEWEST FIRST and capped at `budget`.
///
/// Newest-first is deliberate: a transfer that is cancelled or runs into the
/// receiver's budget keeps the history nearest the prompt, which is the part
/// anyone would miss.
///
/// Absolute line numbers (see `Term::cell_at`): `0..rows` is the visible
/// screen (row 0 is its top), and scrollback is negative — `-1` is the line
/// immediately above the visible screen's top row, down to `-retained_history()`,
/// the oldest retained line. So the newest scrollback line is always `-1`,
/// never a function of `rows()`/`bottommost()`: the brief's
/// `bottommost() - rows() + 1` collapses to `0`, which is the visible top
/// row, not scrollback — it would duplicate that row into the scrollback
/// output (and return one bogus line even with zero history).
///
/// The walk is bounded by `retained_history()`, NOT by `topmost()`. `topmost()`
/// is `-history_size()`, and `history_size()` is a VIEWPORT answer: it reports 0
/// while the alt screen is active, because the alt screen cannot be scrolled
/// back. The lines are still retained and `cell_at(-1, ..)` still returns them,
/// so bounding by `topmost()` exported ZERO scrollback for any pane on the alt
/// screen — vim, less, htop, the panes most worth moving — with no error and no
/// wire invariant able to notice.
pub fn scrollback_newest_first(term: &Term, budget: usize, styles: &mut StyleTable) -> Vec<Line> {
    if budget == 0 {
        return Vec::new();
    }
    let cols = term.cols();
    let oldest = -(term.retained_history() as i32);
    let mut out = Vec::new();
    let mut abs = -1i32;
    while abs >= oldest && out.len() < budget {
        let cells: Vec<Cell> = (0..cols).map(|c| term.cell_at(abs, c)).collect();
        out.push(row_to_line(&cells, styles));
        abs -= 1;
    }
    out
}

use rt_handoff::pane::{Charsets, CursorState, Margins, ModeEntry, PaneWire, SavedCursor};

/// DEC private mode numbers this engine can report. Kept as literals so the
/// namespace is the VT spec's, not rt's — that is what lets a build years from
/// now understand a mode it does not implement, or skip one it has never heard
/// of, without any shared enum.
///
/// The three mouse-tracking modes each get their own entry, from their own flag.
/// They are mutually exclusive in the engine (setting one clears the other two,
/// as xterm does), so mapping 1000 to `wants_mouse()` — the OR of all three —
/// exported a 1002 pane as `1000=1, 1003=0`, and the receiver came up
/// click-only with drag reporting gone. 1002 is what vim, tmux, htop and less
/// actually set. Reporting each flag separately also removes a replay-order
/// dependency: previously a 1003 pane said both `1000=1` and `1003=1`, and only
/// the emission order stopped a naive replay landing on click-only.
fn modes_of(term: &Term) -> Vec<ModeEntry> {
    let dec = [
        (1u32, term.app_cursor()),
        (6, term.origin()),
        (7, term.autowrap()),
        (25, term.cursor_visible()),
        (1000, term.mouse_click()),
        (1002, term.mouse_drag()),
        // `wants_motion()` IS the 1003 flag — it is the renderer-facing name for
        // `mouse_motion`, keyed on 1003 alone (see its doc comment). No separate
        // accessor is added for it.
        (1003, term.wants_motion()),
        (1004, term.focus_events()),
        (1005, term.utf8_mouse()),
        (1006, term.mouse_sgr()),
        (1007, term.alt_scroll()),
        (1042, term.urgency_hints()),
        (1049, term.alt_screen()),
        (2004, term.bracketed_paste()),
    ];
    let ansi = [(4u32, term.insert_mode()), (20, term.newline_mode())];

    ansi.iter()
        .map(|(n, v)| ModeEntry { kind: 0, number: *n, value: *v as u8 })
        .chain(dec.iter().map(|(n, v)| ModeEntry { kind: 1, number: *n, value: *v as u8 }))
        .collect()
}

fn charsets_of(term: &Term) -> Charsets {
    let g = term.charsets();
    Charsets {
        // The final character of each designation sequence.
        g: [designator(g[0]), designator(g[1]), designator(g[2]), designator(g[3])],
        gl: term.gl() as u8,
        // vt-term has no GR locking shift; the wire's default is G0.
        gr: 0,
    }
}

/// A `vt_term::Charset` as the final byte of its designation sequence.
fn designator(c: vt_term::Charset) -> u8 {
    match c {
        vt_term::Charset::Ascii => b'B',
        // vt-term calls the DEC line-drawing set `Special`; its designation
        // sequence is ESC ( 0, so the final byte on the wire is '0'.
        vt_term::Charset::Special => b'0',
    }
}

/// Read a live terminal into the wire model.
///
/// Returns the pane plus its scrollback, newest-first, which the caller streams
/// as separate `ScrollChunk` messages rather than inlining.
///
/// ENGINE-KNOWN FIELDS ONLY. `title`, `cwd`, `group`, `broadcast`,
/// `columns_count`, `show_titlebar`, `scrollback_limit`, `shell_argv` and
/// `env_extras` belong to the host and are overlaid by `rt-session`.
pub fn export_term(
    term: &Term,
    pane_uid: u64,
    child_pid: u32,
    scrollback_budget: usize,
) -> (PaneWire, Vec<Line>) {
    let mut styles = StyleTable::new();
    let screen_primary = screen_to_grid(term, &mut styles);
    let screen_alt = inactive_to_grid(term, &mut styles);
    let scrollback = scrollback_newest_first(term, scrollback_budget, &mut styles);

    // `Term::cursor()` returns `(col, row)` — confirmed against every other call
    // site in this codebase (rt-engine's vtpane.rs, vt-term's own tests) and by
    // its source (`(self.col, self.row)`). Destructuring it as `(row, col)`
    // silently swaps the wire's cursor position; there is no compile error and
    // no existing test here caught it, so this is called out explicitly.
    let (ccol, crow) = term.cursor();
    let sc = term.saved_cursor();
    let (top, bottom) = term.margins();

    let pane = PaneWire {
        pane_uid,
        cols: term.cols() as u32,
        rows: term.rows() as u32,
        child_pid,
        modes: modes_of(term),
        cursor: CursorState {
            col: ccol as u32,
            row: crow as u32,
            shape: shape_code(term.cursor_shape()),
            visible: term.cursor_visible(),
            blink: term.cursor_blink(),
            pending_wrap: term.pending_wrap(),
        },
        saved_cursor: Some(SavedCursor {
            col: sc.col as u32,
            row: sc.row as u32,
            pen: style_of(&sc.pen),
            charsets: Charsets {
                g: [
                    designator(sc.charsets[0]),
                    designator(sc.charsets[1]),
                    designator(sc.charsets[2]),
                    designator(sc.charsets[3]),
                ],
                gl: 0,
                gr: 0,
            },
            origin: sc.origin,
        }),
        charsets: Some(charsets_of(term)),
        margins: Some(Margins {
            top: top as u32,
            bottom: bottom as u32,
            left: 0,   // vt-term has no DECSLRM
            right: term.cols().saturating_sub(1) as u32,
        }),
        pen: style_of(&term.pen()),
        active_screen: term.alt_screen() as u8,
        style_table: styles.into_vec(),
        screen_primary,
        screen_alt,
        // Left for the host, and the four this engine cannot source.
        ..PaneWire::default()
    };

    (pane, scrollback)
}

/// The wire's cursor shape code: 0 block, 1 underline, 2 bar (vt-term's `Beam`).
fn shape_code(s: vt_term::CursorShape) -> u8 {
    match s {
        vt_term::CursorShape::Block => 0,
        vt_term::CursorShape::Underline => 1,
        vt_term::CursorShape::Beam => 2,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rt_handoff::style::{attrs, Colour};
    use vt_term::{Cell, Color};

    /// A cell carrying `c` in the default style.
    fn plain(c: char) -> Cell {
        // `Cell { c, ..Cell::default() }` doesn't compile from outside vt-term:
        // its `flags` field is private, and functional-update syntax needs every
        // field visible at the call site even when it's coming from `default()`.
        let mut cell = Cell::default();
        cell.c = c;
        cell
    }

    /// A row of `n` default blank cells, with `text` written from column 0.
    fn row(text: &str, n: usize) -> Vec<Cell> {
        let mut cells = vec![Cell::default(); n];
        for (i, ch) in text.chars().enumerate() {
            cells[i] = plain(ch);
        }
        cells
    }

    #[test]
    fn an_empty_row_becomes_an_empty_line() {
        let mut st = StyleTable::new();
        let line = row_to_line(&row("", 80), &mut st);
        assert!(line.runs.is_empty(), "trailing default blanks are omitted entirely");
    }

    #[test]
    fn plain_text_becomes_one_run_and_trailing_blanks_vanish() {
        let mut st = StyleTable::new();
        let line = row_to_line(&row("hello", 80), &mut st);
        assert_eq!(line.runs.len(), 1, "one run, and no trailing blank run");
        assert_eq!(line.runs[0].text, "hello");
        assert_eq!(line.runs[0].cell_span, 5);
    }

    #[test]
    fn an_interior_gap_stays_as_a_blank_run() {
        let mut cells = row("ab", 10);
        cells[5] = plain('z');
        let mut st = StyleTable::new();
        let line = row_to_line(&cells, &mut st);
        let spans: Vec<(u32, &str)> =
            line.runs.iter().map(|r| (r.cell_span, r.text.as_str())).collect();
        assert_eq!(spans, vec![(2, "ab"), (3, ""), (1, "z")], "gap kept, tail dropped");
    }

    #[test]
    fn indexed_colours_reach_the_wire_as_indexed() {
        let mut cells = row("x", 4);
        cells[0].fg = Color::Indexed(200);
        let mut st = StyleTable::new();
        let line = row_to_line(&cells, &mut st);
        let table = st.into_vec();
        assert_eq!(table[line.runs[0].style_id as usize].fg, Colour::Indexed(200));
    }

    #[test]
    fn rgb_colours_reach_the_wire_as_rgb() {
        let mut cells = row("x", 4);
        cells[0].bg = Color::Rgb(10, 20, 30);
        let mut st = StyleTable::new();
        let line = row_to_line(&cells, &mut st);
        let table = st.into_vec();
        assert_eq!(table[line.runs[0].style_id as usize].bg, Colour::Rgb(10, 20, 30));
    }

    #[test]
    fn a_style_change_breaks_the_run() {
        let mut cells = row("abcd", 8);
        cells[2].fg = Color::Indexed(1);
        cells[3].fg = Color::Indexed(1);
        let mut st = StyleTable::new();
        let line = row_to_line(&cells, &mut st);
        assert_eq!(line.runs.len(), 2);
        assert_eq!(line.runs[0].text, "ab");
        assert_eq!(line.runs[1].text, "cd");
        assert_ne!(line.runs[0].style_id, line.runs[1].style_id);
    }

    #[test]
    fn identical_styles_intern_to_one_table_entry() {
        let mut cells = row("a b", 8);
        cells[0].fg = Color::Indexed(5);
        cells[2].fg = Color::Indexed(5);
        let mut st = StyleTable::new();
        let line = row_to_line(&cells, &mut st);
        assert_eq!(line.runs[0].style_id, line.runs[2].style_id, "same style, same id");
        assert_eq!(st.into_vec().len(), 2, "one for the coloured style, one default");
    }

    #[test]
    fn the_seven_attributes_this_engine_has_are_mapped() {
        // Build a cell with every flag vt-term supports, via a real Term so the
        // flag bits are set the way the engine actually sets them.
        let mut t = vt_term::Term::new(20, 2);
        t.feed(b"\x1b[1;2;3;4;7;8;9mX");
        let cell = t.cell(0, 0);
        let mut st = StyleTable::new();
        let _ = row_to_line(&[cell], &mut st);
        let a = st.into_vec()[0].attrs;
        for (bit, name) in [
            (attrs::BOLD, "bold"),
            (attrs::DIM, "dim"),
            (attrs::ITALIC, "italic"),
            (attrs::UNDERLINE, "underline"),
            (attrs::REVERSE, "reverse"),
            (attrs::HIDDEN, "hidden"),
            (attrs::STRIKEOUT, "strikeout"),
        ] {
            assert!(a & bit != 0, "{name} did not reach the wire");
        }
        // The engine has no blink/overline/underline-style, so those stay clear.
        assert_eq!(a & (attrs::BLINK | attrs::OVERLINE | attrs::CURLY_UNDERLINE), 0);
    }

    #[test]
    fn a_wide_glyph_is_one_run_cell_spanning_two_columns() {
        let mut t = vt_term::Term::new(20, 2);
        t.feed("日本".as_bytes());
        let cells: Vec<Cell> = (0..20).map(|c| t.cell(0, c)).collect();
        let mut st = StyleTable::new();
        let line = row_to_line(&cells, &mut st);
        assert_eq!(line.runs.len(), 1);
        let r = &line.runs[0];
        assert_eq!(r.flags & run_flags::WIDE, run_flags::WIDE);
        assert_eq!(r.text, "日本");
        assert_eq!(r.cell_span, 4, "two wide glyphs occupy four columns");
        assert!(r.char_counts.is_empty(), "vt-term is one char per cell");
    }

    #[test]
    fn wide_and_narrow_do_not_share_a_run() {
        let mut t = vt_term::Term::new(20, 2);
        t.feed("ab日".as_bytes());
        let cells: Vec<Cell> = (0..20).map(|c| t.cell(0, c)).collect();
        let mut st = StyleTable::new();
        let line = row_to_line(&cells, &mut st);
        assert_eq!(line.runs.len(), 2, "the run breaks at the width change");
        assert_eq!(line.runs[0].text, "ab");
        assert_eq!(line.runs[1].text, "日");
    }

    /// Every run's `(cell_span, WIDE?, text)`, plus the columns they cover in
    /// total — the number the receiver lays out, and the one a neighbour-based
    /// width guess gets wrong.
    fn shape(line: &Line) -> (Vec<(u32, bool, String)>, u32) {
        let runs: Vec<(u32, bool, String)> = line
            .runs
            .iter()
            .map(|r| (r.cell_span, r.flags & run_flags::WIDE != 0, r.text.clone()))
            .collect();
        let cols = runs.iter().map(|(s, _, _)| *s).sum();
        (runs, cols)
    }

    /// The row of a live `Term`, as the exporter reads it.
    fn live_row(t: &vt_term::Term, row: usize) -> Vec<Cell> {
        (0..t.cols()).map(|c| t.cell(row, c)).collect()
    }

    // ── Raw cell shifts leave the grid inconsistent; width must come from the
    // glyph, not the neighbour. vt-term's `delete_chars`/`erase_chars`/insert
    // shift do no wide-glyph cleanup (deliberately, matching alacritty), and
    // ncurses reaches for `dch`/`ech` whenever terminfo has them, so a CJK
    // pane hits all three of these routinely.

    #[test]
    fn a_deleted_wide_glyph_leaves_its_spacer_as_one_blank_column() {
        // `日x` then DCH 1 at column 0 shifts the row left over the glyph,
        // leaving `[orphan spacer][x]`. The spacer still occupies a column:
        // absorbing it (there is no glyph left to absorb it INTO) would make
        // the line one column short and slide everything right of it left.
        let mut t = vt_term::Term::new(20, 2);
        t.feed("日x".as_bytes());
        t.feed(b"\x1b[H\x1b[1P");
        assert!(t.cell(0, 0).spacer() && !t.cell(0, 0).leading_spacer(), "an orphaned spacer");
        assert_eq!(t.cell(0, 1).c, 'x');

        let mut st = StyleTable::new();
        let line = row_to_line(&live_row(&t, 0), &mut st);
        let (runs, cols) = shape(&line);
        assert_eq!(cols, 2, "the row still occupies two columns");
        assert_eq!(runs, vec![(1, false, String::new()), (1, false, "x".into())]);
    }

    #[test]
    fn a_shifted_spacer_does_not_make_a_narrow_glyph_look_wide() {
        // `ab日c` then DCH 2 at column 1 leaves `[a][orphan spacer][c]`. Reading
        // the neighbour's flag calls `a` double-width — the same CRITICAL the
        // LEADING flag fixed for the wrap placeholder, reached another way.
        let mut t = vt_term::Term::new(20, 2);
        t.feed("ab日c".as_bytes());
        t.feed(b"\x1b[2G\x1b[2P");
        assert_eq!(t.cell(0, 0).c, 'a');
        assert!(t.cell(0, 1).spacer() && !t.cell(0, 1).leading_spacer(), "an orphaned spacer");

        let mut st = StyleTable::new();
        let line = row_to_line(&live_row(&t, 0), &mut st);
        let (runs, cols) = shape(&line);
        for (_, wide, text) in &runs {
            assert!(!wide, "no run here is double-width: {text:?}");
        }
        assert_eq!(cols, 3);
        assert_eq!(
            runs,
            vec![(1, false, "a".into()), (1, false, String::new()), (1, false, "c".into())]
        );
    }

    #[test]
    fn an_erased_spacer_leaves_its_glyph_two_columns_wide() {
        // `A日B` then ECH 1 over the spacer leaves the wide glyph with none.
        // It still paints two columns, so calling it narrow makes the next
        // run start a column early and overlap it.
        let mut t = vt_term::Term::new(20, 2);
        t.feed("A日B".as_bytes());
        t.feed(b"\x1b[3G\x1b[1X");
        assert!(t.cell(0, 1).is_wide(), "the glyph is still there");
        assert!(!t.cell(0, 2).spacer(), "but its spacer was erased");

        let mut st = StyleTable::new();
        let line = row_to_line(&live_row(&t, 0), &mut st);
        let (runs, cols) = shape(&line);
        assert_eq!(cols, 5, "A + a two-column glyph + the erased column + B");
        assert_eq!(
            runs,
            vec![
                (1, false, "A".into()),
                (2, true, "日".into()),
                (1, false, String::new()),
                (1, false, "B".into()),
            ]
        );
    }

    #[test]
    fn a_wrap_placeholder_does_not_make_the_previous_glyph_look_wide() {
        // Three columns: 'a', 'b', then a wide glyph that cannot fit, so the
        // engine writes a leading placeholder and wraps the glyph to row 1.
        let mut t = vt_term::Term::new(3, 2);
        t.feed("ab日".as_bytes());
        let cells: Vec<Cell> = (0..3).map(|c| t.cell(0, c)).collect();
        let mut st = StyleTable::new();
        let line = row_to_line(&cells, &mut st);
        for r in &line.runs {
            assert_eq!(r.flags & run_flags::WIDE, 0, "no run on this row is wide: {r:?}");
        }
        let text: String = line.runs.iter().map(|r| r.text.as_str()).collect();
        assert_eq!(text, "ab", "the placeholder is not content");
    }

    #[test]
    fn a_soft_wrapped_row_carries_the_wrapped_flag() {
        let mut t = vt_term::Term::new(4, 3);
        t.feed(b"abcdef"); // wraps after four columns
        let cells: Vec<Cell> = (0..4).map(|c| t.cell(0, c)).collect();
        let mut st = StyleTable::new();
        let line = row_to_line(&cells, &mut st);
        assert_eq!(line.flags & line_flags::WRAPPED, line_flags::WRAPPED);
    }

    #[test]
    fn every_run_this_builds_survives_the_wire_decoder() {
        // The encoder validates on decode; a run we build that the decoder
        // rejects is a bug here, not there.
        let mut t = vt_term::Term::new(40, 4);
        t.feed("plain \x1b[38;5;9mred\x1b[m 日本 \x1b[1mbold".as_bytes());
        let mut st = StyleTable::new();
        let lines: Vec<_> = (0..4)
            .map(|r| {
                let cells: Vec<Cell> = (0..40).map(|c| t.cell(r, c)).collect();
                row_to_line(&cells, &mut st)
            })
            .collect();
        let grid = rt_handoff::grid::Grid { lines };
        let bytes = grid.write();
        assert_eq!(rt_handoff::grid::Grid::read(&bytes, 0x40).unwrap(), grid);
    }

    #[test]
    fn the_visible_screen_becomes_the_primary_grid() {
        let mut t = vt_term::Term::new(20, 3);
        t.feed(b"one\r\ntwo\r\nthree");
        let mut st = StyleTable::new();
        let g = screen_to_grid(&t, &mut st);
        assert_eq!(g.lines.len(), 3, "one wire line per visible row");
        assert_eq!(g.lines[0].runs[0].text, "one");
        assert_eq!(g.lines[2].runs[0].text, "three");
    }

    #[test]
    fn there_is_no_inactive_grid_on_the_primary_screen() {
        let t = vt_term::Term::new(20, 3);
        let mut st = StyleTable::new();
        assert!(inactive_to_grid(&t, &mut st).is_none());
    }

    #[test]
    fn the_alt_screen_puts_the_primary_content_in_the_inactive_grid() {
        let mut t = vt_term::Term::new(20, 3);
        t.feed(b"PRIMARY");
        t.feed(b"\x1b[?1049h");
        // Entering the alt screen clears its content but, matching real xterm
        // 1049 semantics, leaves the cursor exactly where it was on the
        // primary screen (row 0, col 7, right after "PRIMARY") — it does not
        // home it. A real full-screen app (vim, less) always homes the
        // cursor itself before drawing; do the same here, or "ALT" lands at
        // columns 7..10 and `runs[0]` is a leading blank run, not "ALT".
        t.feed(b"\x1b[H");
        t.feed(b"ALT");
        let mut st = StyleTable::new();
        let visible = screen_to_grid(&t, &mut st);
        let inactive = inactive_to_grid(&t, &mut st).expect("primary is held aside");
        assert_eq!(visible.lines[0].runs[0].text, "ALT", "primary grid = what is on screen");
        assert_eq!(inactive.lines[0].runs[0].text, "PRIMARY");
        assert!(t.alt_screen(), "and active_screen will be 1");
    }

    #[test]
    fn scrollback_comes_back_newest_first() {
        let mut t = vt_term::Term::new(20, 2);
        for i in 0..6 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        let mut st = StyleTable::new();
        let lines = scrollback_newest_first(&t, 100, &mut st);
        assert!(!lines.is_empty(), "six lines through a two-row screen leaves history");
        let first_text = &lines[0].runs[0].text;
        let last_text = &lines[lines.len() - 1].runs[0].text;
        let first_n: usize = first_text.trim_start_matches("line").parse().unwrap();
        let last_n: usize = last_text.trim_start_matches("line").parse().unwrap();
        assert!(first_n > last_n, "newest first: {first_text} must precede {last_text}");
    }

    #[test]
    fn the_scrollback_budget_keeps_the_newest_lines() {
        let mut t = vt_term::Term::new(20, 2);
        for i in 0..30 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        let mut st = StyleTable::new();
        let all = scrollback_newest_first(&t, 1000, &mut st);
        let mut st2 = StyleTable::new();
        let capped = scrollback_newest_first(&t, 5, &mut st2);
        assert_eq!(capped.len(), 5, "the budget is a hard cap");
        assert_eq!(capped[0].runs[0].text, all[0].runs[0].text, "and it keeps the NEWEST");
    }

    #[test]
    fn a_zero_budget_yields_no_scrollback() {
        let mut t = vt_term::Term::new(20, 2);
        for i in 0..10 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        let mut st = StyleTable::new();
        assert!(scrollback_newest_first(&t, 0, &mut st).is_empty());
    }

    #[test]
    fn a_pane_with_no_history_yields_no_scrollback() {
        let mut t = vt_term::Term::new(20, 10);
        t.feed(b"just one line");
        let mut st = StyleTable::new();
        assert!(scrollback_newest_first(&t, 100, &mut st).is_empty());
    }

    #[test]
    fn the_alt_screen_still_exports_the_primarys_scrollback() {
        // A pane running vim/less/htop is exactly the pane worth moving, and it
        // is on the alt screen. `history_size()` reports 0 there (the VIEWPORT
        // cannot scroll back), but the lines are retained and `cell_at(-1, ..)`
        // returns them — so bounding the walk by `topmost()` dropped the whole
        // history silently, with nothing on the wire able to tell.
        let mut t = vt_term::Term::new(20, 3);
        for i in 0..20 {
            t.feed(format!("line{i}\r\n").as_bytes());
        }
        let mut st = StyleTable::new();
        let on_primary = scrollback_newest_first(&t, 1000, &mut st);
        assert!(on_primary.len() > 10, "20 lines through a 3-row screen leaves history");

        t.feed(b"\x1b[?1049h");
        assert!(t.alt_screen());
        assert_eq!(t.history_size(), 0, "the viewport really does report no scrollback here");

        let mut st2 = StyleTable::new();
        let on_alt = scrollback_newest_first(&t, 1000, &mut st2);
        assert_eq!(on_alt, on_primary, "entering the alt screen must not drop the history");
    }

    fn mode_value(p: &rt_handoff::pane::PaneWire, kind: u8, number: u32) -> Option<u8> {
        p.modes.iter().find(|m| m.kind == kind && m.number == number).map(|m| m.value)
    }

    #[test]
    fn modes_travel_by_their_dec_number() {
        let mut t = vt_term::Term::new(20, 4);
        t.feed(b"\x1b[?1h\x1b[?2004h\x1b[?7l"); // DECCKM on, bracketed paste on, DECAWM off
        let (p, _) = export_term(&t, 1, 99, 0);
        assert_eq!(mode_value(&p, 1, 1), Some(1), "DECCKM");
        assert_eq!(mode_value(&p, 1, 2004), Some(1), "bracketed paste");
        assert_eq!(mode_value(&p, 1, 7), Some(0), "DECAWM off");
    }

    #[test]
    fn each_mouse_tracking_mode_travels_as_itself() {
        // The three are mutually exclusive in the engine, so exactly one of
        // 1000/1002/1003 may be set at a time — and 1002 (button-event
        // tracking) is what vim, tmux, htop and less actually enable. Folding
        // them into `wants_mouse()` exported a 1002 pane as click-only.
        for (seq, want) in [
            (&b"\x1b[?1000h"[..], 1000u32),
            (&b"\x1b[?1002h"[..], 1002),
            (&b"\x1b[?1003h"[..], 1003),
        ] {
            let mut t = vt_term::Term::new(20, 4);
            t.feed(seq);
            let (p, _) = export_term(&t, 1, 99, 0);
            for n in [1000u32, 1002, 1003] {
                let expect = u8::from(n == want);
                assert_eq!(
                    mode_value(&p, 1, n),
                    Some(expect),
                    "with DECSET {want} set, mode {n} must export as {expect}"
                );
            }
        }
    }

    #[test]
    fn urgency_hints_travel_as_dec_1042() {
        let mut t = vt_term::Term::new(20, 4);
        t.feed(b"\x1b[?1042l");
        let (p, _) = export_term(&t, 1, 99, 0);
        assert_eq!(mode_value(&p, 1, 1042), Some(0), "DECRST 1042 must reach the wire");

        t.feed(b"\x1b[?1042h");
        let (p, _) = export_term(&t, 1, 99, 0);
        assert_eq!(mode_value(&p, 1, 1042), Some(1));
    }

    #[test]
    fn ansi_and_dec_modes_are_separate_namespaces() {
        let mut t = vt_term::Term::new(20, 4);
        t.feed(b"\x1b[4h\x1b[20h"); // IRM, LNM — ANSI, kind 0
        let (p, _) = export_term(&t, 1, 99, 0);
        assert_eq!(mode_value(&p, 0, 4), Some(1), "IRM is ANSI mode 4");
        assert_eq!(mode_value(&p, 0, 20), Some(1), "LNM is ANSI mode 20");
        assert_eq!(mode_value(&p, 1, 4), None, "there is no DEC PRIVATE mode 4 here");
    }

    #[test]
    fn the_cursor_carries_position_shape_and_pending_wrap() {
        // 4 columns; CUP to row 2 col 3 (0-based row 1, col 2) leaves exactly two
        // columns — "ab" fills col 2 then col 3 (the last column), which is what
        // sets the deferred-wrap flag. Feeding a third/fourth character (the
        // brief's original "abcd") would trigger the wrap itself and clear the
        // flag before export — verified against vt-term directly: after "abcd"
        // `pending_wrap()` is false and `cursor()` is `(2, 2)`, not the wire-edge
        // state this test means to capture. "ab" is the correct fixture.
        let mut t = vt_term::Term::new(4, 4);
        t.feed(b"\x1b[2;3Hab");
        let (p, _) = export_term(&t, 1, 99, 0);
        assert!(p.cursor.visible);
        assert!(p.cursor.pending_wrap, "the deferred wrap must survive");
        // `Term::cursor()` returns `(col, row)` (confirmed at every other call
        // site in this codebase, e.g. rt-engine's vtpane.rs and vt-term's own
        // tests) — position must reach the wire uninverted.
        assert_eq!((p.cursor.col, p.cursor.row), (3, 1), "position must not be col/row swapped");
    }

    #[test]
    fn cursor_blink_reaches_the_wire() {
        let mut t = vt_term::Term::new(20, 4);
        t.feed(b"\x1b[?12h"); // DECSET 12: blink on
        let (p, _) = export_term(&t, 1, 99, 0);
        assert!(p.cursor.blink, "DECSET 12 must reach the wire");

        t.feed(b"\x1b[?12l"); // and off again
        let (p, _) = export_term(&t, 1, 99, 0);
        assert!(!p.cursor.blink);
    }

    #[test]
    fn decscusr_blink_reaches_the_wire() {
        // DECSCUSR (`CSI Ps SP q`): 0/1/2 = block, 3/4 = underline, 5/6 = bar;
        // odd Ps blinks, even is steady. Verified against
        // `set_cursor_shape` in vt-term/src/lib.rs: `cursor_blink =
        // matches!(ps, 1 | 3 | 5)`, and the CSI dispatch site that reaches it
        // on `(Some(&b' '), 'q')` — confirming `\x1b[1 q` / `\x1b[2 q` really
        // do parse as Ps=1/2 with a space intermediate before `q`.
        let mut t = vt_term::Term::new(20, 4);
        t.feed(b"\x1b[1 q"); // blinking block
        let (p, _) = export_term(&t, 1, 99, 0);
        assert!(p.cursor.blink, "DECSCUSR 1 is a blinking cursor");
        assert_eq!(p.cursor.shape, 0, "DECSCUSR 1 is still block-shaped");

        t.feed(b"\x1b[2 q"); // steady block
        let (p, _) = export_term(&t, 1, 99, 0);
        assert!(!p.cursor.blink, "DECSCUSR 2 is steady");
        assert_eq!(p.cursor.shape, 0, "DECSCUSR 2 is still block-shaped");
    }

    #[test]
    fn margins_and_the_pen_are_exported() {
        let mut t = vt_term::Term::new(20, 24);
        t.feed(b"\x1b[5;20r\x1b[1;38;5;42m");
        let (p, _) = export_term(&t, 1, 99, 0);
        let m = p.margins.expect("DECSTBM was set");
        assert_eq!((m.top, m.bottom), (4, 19));
        assert_eq!(p.pen.fg, rt_handoff::style::Colour::Indexed(42));
        assert!(p.pen.attrs & rt_handoff::style::attrs::BOLD != 0);
    }

    #[test]
    fn active_screen_flags_which_grid_is_showing() {
        let mut t = vt_term::Term::new(20, 4);
        let (p, _) = export_term(&t, 1, 99, 0);
        assert_eq!(p.active_screen, 0);
        assert!(p.screen_alt.is_none());

        t.feed(b"\x1b[?1049h");
        let (p, _) = export_term(&t, 1, 99, 0);
        assert_eq!(p.active_screen, 1);
        assert!(p.screen_alt.is_some(), "the held-aside primary rides along");
    }

    #[test]
    fn the_four_unsupported_fields_ship_absent() {
        // vt-term tracks no tab stops, title stack, hyperlinks or images.
        // Rule R3 means the receiver applies documented defaults.
        let mut t = vt_term::Term::new(20, 4);
        t.feed(b"hello");
        let (p, _) = export_term(&t, 1, 99, 0);
        assert!(p.tab_stops.is_none());
        assert!(p.title_stack.is_empty());
        assert!(p.uri_table.is_empty());
        assert!(p.image_table.is_empty());
    }

    #[test]
    fn host_level_fields_are_left_for_the_session_to_fill() {
        // export() knows the engine, not the window. Title, cwd, group and the
        // rest belong to rt-session and are overlaid later.
        let mut t = vt_term::Term::new(20, 4);
        t.feed(b"\x1b]0;a title\x07");
        let (p, _) = export_term(&t, 7, 4242, 0);
        assert_eq!(p.pane_uid, 7);
        assert_eq!(p.child_pid, 4242);
        assert_eq!(p.title, "", "the host owns the title, not the engine");
        assert!(p.cwd.is_none());
        assert!(p.shell_argv.is_empty());
    }

    #[test]
    fn an_exported_pane_encodes_and_decodes_unchanged() {
        let mut t = vt_term::Term::new(40, 6);
        t.feed("\x1b[1;38;5;9mred\x1b[m normal 日本\r\nsecond".as_bytes());
        let (p, _) = export_term(&t, 3, 111, 0);
        let back = rt_handoff::pane::PaneWire::decode(&p.encode()).unwrap();
        let mut expected = p.clone();
        expected.tag_names = back.tag_names.clone();
        assert_eq!(back, expected, "a real exported pane must survive the wire");
    }
}
