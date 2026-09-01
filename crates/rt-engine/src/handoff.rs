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

/// True for a cell that contributes nothing to the wire: a space in a style
/// that renders identically to the default, OR the before-wrap placeholder —
/// it renders as nothing and exists only to hold the last column while its
/// glyph wraps to the next row, so it is exactly as droppable as a blank.
///
/// A wide glyph's ordinary trailing `spacer()` cell is NOT blank: it is the
/// second half of the glyph before it, and dropping it independently (e.g.
/// via the tail-trim below) would desync the wide/narrow width check for
/// that glyph.
fn is_blank(cell: &Cell) -> bool {
    if cell.spacer() {
        cell.leading_spacer()
    } else {
        cell.c == ' ' && style_of(cell) == Style::default()
    }
}

/// One row of live cells to one wire line.
///
/// `cells` is the whole row, `cells.len()` columns wide. A wide glyph appears
/// as a leading cell followed by a trailing `spacer()`; the spacer is
/// consumed into the leading cell's run and never emitted on its own. A
/// *leading* spacer — the before-wrap placeholder at the last column when a
/// wide glyph doesn't fit and wraps instead — is not part of any glyph here;
/// it is blank (see [`is_blank`]) and normally vanishes in the tail-trim.
pub fn row_to_line(cells: &[Cell], styles: &mut StyleTable) -> Line {
    let wrapped = cells.last().map(|c| c.wrapline()).unwrap_or(false);
    let flags = if wrapped { line_flags::WRAPPED } else { 0 };

    // Drop the tail of blank default cells; the receiver pads the line back out.
    let end = cells.iter().rposition(|c| !is_blank(c)).map(|i| i + 1).unwrap_or(0);
    let cells = &cells[..end];

    let mut runs: Vec<Run> = Vec::new();
    let mut i = 0usize;
    while i < cells.len() {
        let cell = &cells[i];
        let style_id = styles.intern(cell);
        let wide = i + 1 < cells.len() && is_trailing_spacer(&cells[i + 1]);

        if is_blank(cell) && !wide {
            // A stretch of blanks in one style: no text, just a span.
            let start = i;
            while i < cells.len() && is_blank(&cells[i]) && !(i + 1 < cells.len() && is_trailing_spacer(&cells[i + 1])) {
                i += 1;
            }
            runs.push(Run::blank(style_id, (i - start) as u32));
            continue;
        }

        // A run of same-style, same-width, non-blank cells.
        let mut text = String::new();
        let mut span = 0u32;
        while i < cells.len() {
            let c = &cells[i];
            if c.spacer() {
                // Consumed by the leading cell before it (a trailing spacer's
                // own leading glyph), or blank and already trimmed away (a
                // wrap placeholder) — either way, not this run's content.
                i += 1;
                continue;
            }
            let c_wide = i + 1 < cells.len() && is_trailing_spacer(&cells[i + 1]);
            if styles.intern(c) != style_id || c_wide != wide || is_blank(c) {
                break;
            }
            text.push(c.c);
            span += if c_wide { 2 } else { 1 };
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
}
