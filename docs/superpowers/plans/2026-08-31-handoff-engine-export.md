# Engine Export (phase 2a) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Read a live pane's terminal state out of the in-house `vt-term` engine into a `rt_handoff::pane::PaneWire` — the first half of moving a pane between processes.

**Architecture:** `vt-term` gains read-only accessors for state it already tracks but keeps private. A new `rt-engine` module converts a live `Term` into the frozen wire model: raw cells to styled runs, preserving indexed colours. `TermPane::export()` dispatches to the vt-term backend and returns a clear error for the alacritty backend. Nothing is written, nothing is torn down, no file descriptor moves — export is a pure read. Freeze/thaw, `adopt()`, fd passing and the two-process test are phases 2b and 2c.

**Tech Stack:** Rust 2021. `rt-engine` may depend on `rt-handoff` (path dep). `vt-term` gains NO new dependencies. Real-PTY integration tests follow the existing pattern in `crates/rt-engine/tests/echo.rs`.

**Spec:** `docs/superpowers/specs/2026-08-29-cross-instance-pane-transfer-design.md` — read the "Wire format v1" PaneState table and "What does not survive a move".

**Prior slice:** `crates/rt-handoff` (phase 1, branch `feat/handoff-wire-v1`) holds the frozen wire model this plan populates. Read `crates/rt-handoff/src/pane.rs` for `PaneWire` and its sub-structs before starting.

## Global Constraints

- **`vt-term` gains no dependencies.** It is the engine core and is benchmarked on a riscv64 board; keep it dependency-free and allocation-conscious.
- **`rt-handoff` still gains no dependencies.** If something seems to need one, the design is wrong — say so rather than adding it.
- **Indexed colours stay indexed.** `vt_term::Color::Indexed(n)` must reach the wire as `rt_handoff::style::Colour::Indexed(n)`, never resolved to RGB. The renderer's `Snapshot`/`SnapCell` type resolves colours and is therefore **unusable** for export — do not reach for it.
- **Export is a pure read.** It takes `&Term` (or a lock guard), mutates nothing, consumes nothing, and must never call a `take_*` method. `Term::take_title` and `Term::take_output` are consuming and are off-limits: taking the title here would steal it from the host's own title tracking.
- **Export fills ENGINE-KNOWN fields only.** `title`, `cwd`, `group`, `broadcast`, `columns_count`, `show_titlebar`, `scrollback_limit`, `shell_argv` and `env_extras` are host-level and belong to `rt-session`, which overlays them in a later slice. Leave them at their `Default`.
- **Four fields ship absent, by decision.** `tab_stops`, `title_stack`, `uri_table` and `image_table` stay empty: vt-term tracks no tab stops (they are hard-coded every 8 columns), has no title stack, parses OSC 8 but ignores it, and has no image support. Rule R3 covers this — an absent tag means the documented default. Do not invent values, and do not add engine features to fill them.
- **vt-term stores one `char` per cell**, so a run never needs the wire's `CHAR_COUNTS` flag. Emit runs without it.
- **vt-term has 7 attribute flags** (bold, italic, underline, inverse, dim, hidden, strikeout). The wire has 14. Map the 7 that exist; leave the rest clear. There is no underline colour and no hyperlink id in this engine, so `Style::underline` is always `Colour::Default` and `Style::link_id` always 0.
- **Commits** follow the repo's conventional-commit style and carry whatever session trailer the executing harness requires.
- **Every task ends green:** `cargo test -p vt-term -p rt-engine -p rt-handoff` passes before the commit. Do NOT run the whole workspace suite (it builds GUI crates needing system libraries) and do NOT run `ci/verify.sh` (it ssh's to remote machines).

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/vt-term/src/state.rs` | NEW. A second `impl Term` block holding read-only accessors for state the engine tracks privately. Kept out of `lib.rs`, which is already ~2700 lines. |
| `crates/vt-term/src/lib.rs` | Add `mod state;`. Make the handful of types the accessors return public. |
| `crates/rt-engine/src/handoff.rs` | NEW. The whole `Term` → `PaneWire` conversion: style-table interning, row-to-runs, grid and scrollback assembly, mode collection. |
| `crates/rt-engine/src/lib.rs` | Add `mod handoff;`, `TermPane::export()`, and the `ExportError` type. |
| `crates/rt-engine/Cargo.toml` | Add the `rt-handoff` path dependency. |
| `crates/rt-engine/tests/export.rs` | NEW. Real-PTY integration: drive known sequences into a live pane, export, assert. |

`handoff.rs` is one module rather than several because the pieces share the style-interning table and are meaningless apart. It should stay under ~400 lines; if it grows past that during phase 2b, split the adopt half into `handoff/adopt.rs`.

---

### Task 1: `vt-term` read-only state accessors

**Files:**
- Create: `crates/vt-term/src/state.rs`
- Modify: `crates/vt-term/src/lib.rs` (add `mod state;`; widen visibility of returned types)

**Interfaces:**
- Consumes: the private `Term` fields listed below.
- Produces: on `vt_term::Term` — `pen()`, `margins()`, `autowrap()`, `origin()`, `insert_mode()`, `newline_mode()`, `pending_wrap()`, `utf8_mouse()`, `urgency_hints()`, `charsets()`, `gl()`, `saved_cursor()`, `has_inactive_screen()`, `inactive_rows()`, `inactive_cell()`.

The private fields, with their real declarations (from `crates/vt-term/src/lib.rs:399-485`):

```rust
saved_screen: Option<(Vec<Line>, usize, usize, Cell, bool, [Charset; 4])>,
pen: Cell,
scroll_top: usize,
scroll_bottom: usize,
autowrap: bool,
origin: bool,
pending_wrap: bool,
saved_cursor: (usize, usize, Cell, bool, bool, [Charset; 4]), // DECSC
charsets: [Charset; 4],
gl: usize,
utf8_mouse: bool,
urgency_hints: bool,
newline_mode: bool,
insert_mode: bool,
```

**Both tuples are positional and undocumented beyond a trailing comment.** Before writing an accessor for `saved_cursor` or `saved_screen`, read the code that WRITES them (search for `self.saved_cursor =` and `self.saved_screen =`, and the DECSC/DECRC and alt-screen-swap handlers) and confirm what each element means. Do not guess from the type. If an element's meaning is genuinely ambiguous after reading both the write and read sites, stop and ask rather than encoding a guess into an accessor other code will trust.

- [ ] **Step 1: Write the failing tests**

`crates/vt-term/src/state.rs`, tests first. These drive real escape sequences through `Term::feed` and read the accessors back, so they pin behaviour rather than field plumbing:

```rust
//! Read-only accessors for state the terminal tracks but keeps private.
//!
//! These exist for the cross-process pane handoff, which must read a live
//! pane's complete state without disturbing it. Every method here is a pure
//! read: no `&mut self`, nothing consumed. Contrast `take_title`/`take_output`,
//! which are one-shot and would steal state from the host if used here.

#[cfg(test)]
mod tests {
    use crate::Term;

    fn term_after(seq: &[u8]) -> Term {
        let mut t = Term::new(80, 24);
        t.feed(seq);
        t
    }

    #[test]
    fn margins_report_the_decstbm_region() {
        // DECSTBM 5;20 — rows are 1-based on the wire, 0-based internally.
        let t = term_after(b"\x1b[5;20r");
        let (top, bottom) = t.margins();
        assert_eq!((top, bottom), (4, 19), "DECSTBM 5;20 -> internal rows 4..=19");
    }

    #[test]
    fn margins_default_to_the_whole_screen() {
        let t = Term::new(80, 24);
        assert_eq!(t.margins(), (0, 23));
    }

    #[test]
    fn autowrap_and_origin_follow_their_dec_modes() {
        let t = term_after(b"\x1b[?7l\x1b[?6h"); // DECAWM off, DECOM on
        assert!(!t.autowrap());
        assert!(t.origin());
        let t = term_after(b"\x1b[?7h\x1b[?6l");
        assert!(t.autowrap());
        assert!(!t.origin());
    }

    #[test]
    fn insert_and_newline_modes_follow_their_ansi_modes() {
        let t = term_after(b"\x1b[4h\x1b[20h"); // IRM on, LNM on
        assert!(t.insert_mode());
        assert!(t.newline_mode());
        let t = term_after(b"\x1b[4l\x1b[20l");
        assert!(!t.insert_mode());
        assert!(!t.newline_mode());
    }

    #[test]
    fn pending_wrap_is_set_after_writing_the_last_column() {
        let mut t = Term::new(4, 2);
        t.feed(b"abcd"); // fills the row; cursor defers the wrap
        assert!(t.pending_wrap(), "the deferred-wrap flag must be visible");
        t.feed(b"e");
        assert!(!t.pending_wrap(), "and cleared once the wrap happens");
    }

    #[test]
    fn the_pen_carries_the_current_sgr_state() {
        let t = term_after(b"\x1b[1;3;38;5;200m");
        let pen = t.pen();
        assert!(pen.bold());
        assert!(pen.italic());
        assert_eq!(pen.fg, crate::Color::Indexed(200));
    }

    #[test]
    fn charsets_and_gl_report_designations() {
        // Designate DEC graphics into G1, then lock GL to G1 with SO.
        let t = term_after(b"\x1b)0\x0e");
        assert_eq!(t.gl(), 1, "SO locks GL to G1");
        assert_eq!(t.charsets()[1], crate::Charset::Special, "G1 designated to DEC graphics");
    }

    #[test]
    fn saved_cursor_round_trips_through_decsc_and_decrc() {
        let t = term_after(b"\x1b[5;10H\x1b[1m\x1b7");
        let sc = t.saved_cursor();
        assert_eq!((sc.row, sc.col), (4, 9), "DECSC captures the cursor position");
        assert!(sc.pen.bold(), "and the pen with it");
    }

    #[test]
    fn there_is_no_inactive_screen_until_the_alt_screen_is_entered() {
        let t = Term::new(80, 24);
        assert!(!t.has_inactive_screen());
        let t = term_after(b"\x1b[?1049h"); // enter alt screen
        assert!(t.has_inactive_screen(), "the primary screen is now the inactive one");
    }

    #[test]
    fn the_inactive_screen_holds_the_content_left_behind() {
        let mut t = Term::new(80, 24);
        t.feed(b"PRIMARY");
        t.feed(b"\x1b[?1049h"); // switch to alt; primary is saved
        t.feed(b"ALT");
        assert_eq!(t.inactive_rows(), Some(24), "the saved screen is the same height");
        let first: String = (0..7).map(|c| t.inactive_cell(0, c).unwrap().c).collect();
        assert_eq!(first, "PRIMARY", "the saved screen keeps what was there");
        // And the live screen shows the alt content.
        let live: String = (0..3).map(|c| t.cell(0, c).c).collect();
        assert_eq!(live, "ALT");
    }

    #[test]
    fn every_accessor_is_a_pure_read() {
        // Calling them twice must give the same answer — nothing is consumed.
        let t = term_after(b"\x1b[1m\x1b[5;20r\x1b[?7l");
        assert_eq!(t.margins(), t.margins());
        assert_eq!(t.autowrap(), t.autowrap());
        assert_eq!(t.pen().fg, t.pen().fg);
        assert_eq!(t.charsets(), t.charsets());
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vt-term state`
Expected: FAIL — `no method named margins found for struct Term`.

- [ ] **Step 3: Read the two tuples before writing their accessors**

Find every site that assigns `self.saved_cursor` and `self.saved_screen`, and every site that reads them back (DECSC/DECRC, and the `?1049`/`?47`/`?1047` alt-screen handlers). Write down what each tuple element means. The trailing comments (`// DECSC state (charsets, not gl)` and `// + designations`) are hints, not documentation. Your accessors will be the only documentation other code gets, so they must be right.

- [ ] **Step 4: Write the accessors**

Prepend to `crates/vt-term/src/state.rs`, below the module doc comment. Return small named structs rather than the raw tuples — a positional tuple is exactly what made this state hard to read in the first place:

```rust
use crate::{Cell, Charset, Term};

/// The DECSC saved-cursor state, unpacked from its tuple.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedCursor {
    pub row: usize,
    pub col: usize,
    pub pen: Cell,
    pub origin: bool,
    pub autowrap: bool,
    pub charsets: [Charset; 4],
}

impl Term {
    /// The current SGR state — the template applied to the NEXT cell written.
    pub fn pen(&self) -> Cell {
        self.pen
    }

    /// The DECSTBM scrolling region as inclusive 0-based rows.
    pub fn margins(&self) -> (usize, usize) {
        (self.scroll_top, self.scroll_bottom)
    }

    pub fn autowrap(&self) -> bool {
        self.autowrap
    }

    pub fn origin(&self) -> bool {
        self.origin
    }

    pub fn insert_mode(&self) -> bool {
        self.insert_mode
    }

    pub fn newline_mode(&self) -> bool {
        self.newline_mode
    }

    /// The deferred-wrap flag: the cursor sits past the last column and the
    /// next printable character wraps first. Dropping it misplaces output.
    pub fn pending_wrap(&self) -> bool {
        self.pending_wrap
    }

    pub fn utf8_mouse(&self) -> bool {
        self.utf8_mouse
    }

    pub fn urgency_hints(&self) -> bool {
        self.urgency_hints
    }

    /// The G0..G3 character-set designations.
    pub fn charsets(&self) -> [Charset; 4] {
        self.charsets
    }

    /// Which G-set GL is currently locked to (0..=3).
    pub fn gl(&self) -> usize {
        self.gl
    }

    /// The DECSC saved cursor. Always present — it has a defined initial value
    /// rather than being optional, matching DECRC's behaviour before any DECSC.
    pub fn saved_cursor(&self) -> SavedCursor {
        // Element order confirmed against the DECSC/DECRC handlers in lib.rs.
        let (row, col, pen, origin, autowrap, charsets) = self.saved_cursor;
        SavedCursor { row, col, pen, origin, autowrap, charsets }
    }

    /// True when a screen is being held aside — i.e. the alt screen is active
    /// and the primary is saved, or vice versa.
    pub fn has_inactive_screen(&self) -> bool {
        self.saved_screen.is_some()
    }

    /// How many rows the held-aside screen has, if there is one.
    pub fn inactive_rows(&self) -> Option<usize> {
        self.saved_screen.as_ref().map(|s| s.0.len())
    }

    /// One cell of the screen NOT currently displayed.
    ///
    /// Cell-at-a-time rather than returning the lines themselves: `Line` is a
    /// private type and must stay that way — handing it out would freeze an
    /// internal representation into the crate's public API for the sake of one
    /// caller that only ever reads cells.
    pub fn inactive_cell(&self, row: usize, col: usize) -> Option<Cell> {
        let saved = self.saved_screen.as_ref()?;
        saved.0.get(row).and_then(|line| line.get(col)).copied()
    }
}
```

Adjust the `saved_cursor` destructuring to the real element order you established in Step 3 — the order above is the declaration's apparent order and must be verified, not assumed. Likewise check how `Line` stores its cells: `line.get(col)` above assumes a slice-like accessor, so adapt it to the real shape while keeping `inactive_cell`'s signature.

- [ ] **Step 5: Make the returned types reachable**

`Cell`, `Charset` and `Color` are already `pub`. `Line` is PRIVATE and must stay private — that is why the inactive screen is read cell-at-a-time. Add `mod state;` plus `pub use state::SavedCursor;` to `crates/vt-term/src/lib.rs`.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p vt-term`
Expected: PASS, including the existing suite.

- [ ] **Step 7: Commit**

```bash
git add crates/vt-term/src/state.rs crates/vt-term/src/lib.rs
git commit -m "feat(vt-term): read-only accessors for pen, margins, modes, charsets and the saved screen"
```

---

### Task 2: Cells to styled runs — the conversion core

The subtle part of export, and the part that runs over every cell of every line. A row of `vt_term::Cell` becomes a `rt_handoff::grid::Line` of runs sharing an interned style table.

**Files:**
- Create: `crates/rt-engine/src/handoff.rs`
- Modify: `crates/rt-engine/src/lib.rs` (add `mod handoff;`)
- Modify: `crates/rt-engine/Cargo.toml` (add `rt-handoff = { path = "../rt-handoff" }`)

**Interfaces:**
- Consumes: `vt_term::{Cell, Color}`; `rt_handoff::style::{Colour, Style, attrs}`; `rt_handoff::grid::{Line, Run, line_flags, run_flags}`.
- Produces: `handoff::StyleTable` with `new()`, `intern(&mut self, &vt_term::Cell) -> u32`, `into_vec(self) -> Vec<Style>`; and `handoff::row_to_line(cells: &[vt_term::Cell], styles: &mut StyleTable) -> Line`.

The rules, all of which the tests below pin:

- Consecutive cells with the same interned style id join one run.
- A run of cells that are all `' '` in the default style is a **blank run**: `Run::blank(style_id, span)`, empty text. This is what makes a mostly-empty 200-column line cost a handful of bytes.
- **Trailing blank cells in the default style are omitted entirely.** The line simply ends. The receiver pads.
- A wide glyph occupies two cells in vt-term: the leading cell carrying the character, and a `spacer()` cell after it. On the wire that is ONE run cell spanning two columns — emit `Run::wide`, and do not emit the spacer as its own cell.
- A run never mixes wide and narrow cells; break instead.
- vt-term stores one `char` per cell, so `run_flags::CHAR_COUNTS` is never set.
- `line_flags::WRAPPED` comes from `wrapline()` on the row's LAST cell.

- [ ] **Step 1: Write the failing tests**

`crates/rt-engine/src/handoff.rs`, tests first:

```rust
//! Converting a live `vt_term::Term` into the frozen `rt_handoff` wire model.
//!
//! Export is a pure read: it takes the terminal by shared reference, mutates
//! nothing, and consumes nothing. The renderer's `Snapshot` type is NOT used
//! here — it resolves colours to RGB, and the wire must keep an indexed colour
//! indexed so a moved pane still follows the receiving window's palette.

#[cfg(test)]
mod tests {
    use super::*;
    use rt_handoff::style::{attrs, Colour};
    use vt_term::{Cell, Color};

    /// A cell carrying `c` in the default style.
    fn plain(c: char) -> Cell {
        Cell { c, ..Cell::default() }
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt-engine handoff`
Expected: FAIL — `cannot find type StyleTable in this scope`.

- [ ] **Step 3: Add the dependency**

In `crates/rt-engine/Cargo.toml`, under `[dependencies]`:

```toml
rt-handoff = { path = "../rt-handoff" }  # the frozen cross-process wire model
```

- [ ] **Step 4: Write the conversion**

Prepend to `crates/rt-engine/src/handoff.rs`, below the module doc comment:

```rust
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

/// True for a cell that contributes nothing: a space in a style that renders
/// identically to the default. Only these are dropped from a line's tail.
fn is_blank(cell: &Cell) -> bool {
    cell.c == ' ' && style_of(cell) == Style::default()
}

/// One row of live cells to one wire line.
///
/// `cells` is the whole row, `cells.len()` columns wide. A wide glyph appears
/// as a leading cell followed by a `spacer()`; the spacer is consumed into the
/// leading cell's run and never emitted on its own.
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
        let wide = i + 1 < cells.len() && cells[i + 1].spacer();

        if is_blank(cell) && !wide {
            // A stretch of blanks in one style: no text, just a span.
            let start = i;
            while i < cells.len() && is_blank(&cells[i]) && !(i + 1 < cells.len() && cells[i + 1].spacer()) {
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
                // Consumed by the leading cell before it.
                i += 1;
                continue;
            }
            let c_wide = i + 1 < cells.len() && cells[i + 1].spacer();
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
```

- [ ] **Step 5: Wire the module in**

In `crates/rt-engine/src/lib.rs`, add `mod handoff;` beside the existing module declarations. Keep it private for now — `TermPane::export` in Task 5 is the public surface.

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test -p rt-engine handoff`
Expected: PASS.

The wide-glyph and blank-run interaction in `row_to_line` is the fiddly part. If a test fails, fix the loop rather than the test — the tests encode the format's rules, and `every_run_this_builds_survives_the_wire_decoder` is the backstop that proves the output is legal.

- [ ] **Step 7: Commit**

```bash
git add crates/rt-engine/src/handoff.rs crates/rt-engine/src/lib.rs crates/rt-engine/Cargo.toml
git commit -m "feat(handoff): convert live vt-term cells into styled wire runs"
```

---

### Task 3: Screens and scrollback

**Files:**
- Modify: `crates/rt-engine/src/handoff.rs`

**Interfaces:**
- Consumes: Task 2's `StyleTable`/`row_to_line`; `vt_term::Term`'s `cols`, `rows`, `cell`, `cell_at`, `topmost`, `bottommost`, `history_size`, `alt_screen`, `inactive_rows`, `inactive_cell`.
- Produces: `handoff::screen_to_grid(&Term, &mut StyleTable) -> Grid`, `handoff::inactive_to_grid(&Term, &mut StyleTable) -> Option<Grid>`, `handoff::scrollback_newest_first(&Term, budget: usize, &mut StyleTable) -> Vec<Line>`.

Two things the spec fixes and this task must honour:

- **Scrollback is sent newest-first**, so a transfer cancelled or truncated at the budget keeps the history the user actually cares about. `scrollback_newest_first` returns lines in that order — the receiver prepends.
- **`screen_primary` always carries the CURRENTLY VISIBLE screen**, and `screen_alt` the inactive one, with `active_screen` saying which is which. When the pane is on the alt screen, primary holds the alt content and `active_screen` is 1. Do not try to reorder them by meaning; the wire's names are positional and the flag disambiguates.

`Term::cell(row, col)` reads the visible viewport. `Term::cell_at(abs, col)` reads by absolute line number across scrollback, where `topmost()` is the oldest retained line and `bottommost()` the newest. Scrollback is everything above the visible screen: absolute lines `topmost()` up to `bottommost() - rows() + 1`.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/rt-engine/src/handoff.rs`:

```rust
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt-engine handoff`
Expected: FAIL — `cannot find function screen_to_grid in this scope`.

- [ ] **Step 3: Write the implementation**

Append to the implementation part of `crates/rt-engine/src/handoff.rs`:

```rust
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
pub fn scrollback_newest_first(term: &Term, budget: usize, styles: &mut StyleTable) -> Vec<Line> {
    if budget == 0 {
        return Vec::new();
    }
    let cols = term.cols();
    // Absolute lines: `topmost()` is the oldest retained, `bottommost()` the
    // newest. The visible screen occupies the last `rows()` of that range, so
    // scrollback ends just above it.
    let newest_scrollback = term.bottommost() - term.rows() as i32 + 1;
    let oldest = term.topmost();
    let mut out = Vec::new();
    let mut abs = newest_scrollback;
    while abs >= oldest && out.len() < budget {
        let cells: Vec<Cell> = (0..cols).map(|c| term.cell_at(abs, c)).collect();
        out.push(row_to_line(&cells, styles));
        abs -= 1;
    }
    out
}
```

`inactive_cell` returns `None` past the end of a short row; `unwrap_or_default()` pads with a blank cell, which is correct — a saved screen's rows are the width they were saved at.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p rt-engine handoff`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/rt-engine/src/handoff.rs
git commit -m "feat(handoff): export the visible screen, the inactive screen and newest-first scrollback"
```

---

### Task 4: Modes, cursor and the assembled `PaneWire`

**Files:**
- Modify: `crates/rt-engine/src/handoff.rs`

**Interfaces:**
- Consumes: Tasks 1-3.
- Produces: `handoff::export_term(&Term, pane_uid: u64, child_pid: u32, scrollback_budget: usize) -> (PaneWire, Vec<Line>)` — the pane and its newest-first scrollback chunk.

Modes travel by their **DEC/ANSI number**, never by an rt enum — that is the format's core version-independence decision, and it means this function is a table of spec numbers, not a translation layer. Emit an entry for every mode the engine can report, with its current value; a mode this engine does not track is simply absent, and rule R3 gives the receiver the documented default.

The modes vt-term can report, with the numbers to emit:

| DEC private | mode | accessor |
|---|---|---|
| 1 | DECCKM application cursor keys | `app_cursor()` |
| 6 | DECOM origin | `origin()` |
| 7 | DECAWM autowrap | `autowrap()` |
| 25 | DECTCEM cursor visible | `cursor_visible()` |
| 1000 | mouse: button events | `wants_mouse()` |
| 1003 | mouse: any motion | `wants_motion()` |
| 1004 | focus events | `focus_events()` |
| 1005 | mouse: UTF-8 encoding | `utf8_mouse()` |
| 1006 | mouse: SGR encoding | `mouse_sgr()` |
| 1007 | alternate scroll | `alt_scroll()` |
| 1049 | alt screen | `alt_screen()` |
| 2004 | bracketed paste | `bracketed_paste()` |

| ANSI | mode | accessor |
|---|---|---|
| 4 | IRM insert/replace | `insert_mode()` |
| 20 | LNM newline | `newline_mode()` |

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block in `crates/rt-engine/src/handoff.rs`:

```rust
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
        let mut t = vt_term::Term::new(4, 4);
        t.feed(b"\x1b[2;3Habcd"); // row 2 col 3, then fill to the edge
        let (p, _) = export_term(&t, 1, 99, 0);
        assert!(p.cursor.visible);
        assert!(p.cursor.pending_wrap, "the deferred wrap must survive");
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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt-engine handoff`
Expected: FAIL — `cannot find function export_term in this scope`.

- [ ] **Step 3: Write the implementation**

Append to `crates/rt-engine/src/handoff.rs`:

```rust
use rt_handoff::pane::{Charsets, CursorState, Margins, ModeEntry, PaneWire, SavedCursor};

/// DEC private mode numbers this engine can report. Kept as literals so the
/// namespace is the VT spec's, not rt's — that is what lets a build years from
/// now understand a mode it does not implement, or skip one it has never heard
/// of, without any shared enum.
fn modes_of(term: &Term) -> Vec<ModeEntry> {
    let dec = [
        (1u32, term.app_cursor()),
        (6, term.origin()),
        (7, term.autowrap()),
        (25, term.cursor_visible()),
        (1000, term.wants_mouse()),
        (1003, term.wants_motion()),
        (1004, term.focus_events()),
        (1005, term.utf8_mouse()),
        (1006, term.mouse_sgr()),
        (1007, term.alt_scroll()),
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

    let (crow, ccol) = term.cursor();
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
            blink: false, // vt-term does not track a blink flag separately
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
```

`StyleTable::into_vec` consumes the table, so build every grid BEFORE taking it — the order in `export_term` matters and the borrow checker will tell you if you get it wrong. `Charset` has exactly `Ascii` and `Special`, and `CursorShape` exactly `Block`, `Underline` and `Beam` — both matches are already exhaustive. If either enum has grown by the time you implement this, map the new variant and say so in your report rather than adding a catch-all arm that would silently pick a wrong default.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p rt-engine handoff`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/rt-engine/src/handoff.rs
git commit -m "feat(handoff): assemble a PaneWire from a live terminal, modes by DEC number"
```

---

### Task 5: `TermPane::export()` and the alacritty arm

**Files:**
- Modify: `crates/rt-engine/src/lib.rs`
- Modify: `crates/rt-engine/src/vtpane.rs`

**Interfaces:**
- Consumes: `handoff::export_term`.
- Produces: `rt_engine::ExportError`; `VtPane::export(&self, pane_uid, budget) -> (PaneWire, Vec<Line>)`; `TermPane::export(&self, pane_uid, budget) -> Result<(PaneWire, Vec<Line>), ExportError>`.

Phase 2 covers the in-house engine only, by decision. The alacritty arm is a
**named, explicit refusal**, not an omission — the seam stays shaped so adding
that engine later is filling in an arm rather than reshaping the API.

`VtPane` holds its `Term` behind a mutex (`lock_term()`). Export takes the lock
for the duration of the read so it sees one consistent state, and takes `&self`
throughout: no `&mut`, nothing consumed.

- [ ] **Step 1: Write the failing tests**

Add to the existing `vtpane_tests` module in `crates/rt-engine/src/lib.rs`:

```rust
    #[test]
    fn a_live_vt_pane_exports_its_screen() {
        let mut pane = TermPane::spawn_env(
            Some(("/bin/sh".into(), vec!["-c".into(), "printf 'EXPORTED'; sleep 5".into()])),
            None, 40, 6, &[], 1000,
        )
        .expect("spawn");
        // Give the child a moment to write, draining events as a real host would.
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = pane.drain_events();
            if pane.snapshot().rows.iter().any(|r| r.iter().any(|c| c.c == 'E')) {
                break;
            }
        }
        let (wire, _scroll) = pane.export(42, 0).expect("the in-house engine exports");
        assert_eq!(wire.pane_uid, 42);
        assert_eq!(wire.cols, 40);
        assert_eq!(wire.rows, 6);
        assert_ne!(wire.child_pid, 0, "the child pid rides along for the pidfd");
        let text: String = wire.screen_primary.lines[0]
            .runs
            .iter()
            .map(|r| r.text.as_str())
            .collect();
        assert!(text.starts_with("EXPORTED"), "got {text:?}");
    }

    #[test]
    fn export_does_not_disturb_the_pane() {
        let mut pane = TermPane::spawn_env(
            Some(("/bin/sh".into(), vec!["-c".into(), "printf 'ALIVE'; sleep 5".into()])),
            None, 20, 4, &[], 1000,
        )
        .expect("spawn");
        for _ in 0..50 {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let _ = pane.drain_events();
            if pane.snapshot().rows.iter().any(|r| r.iter().any(|c| c.c == 'A')) {
                break;
            }
        }
        let (a, _) = pane.export(1, 0).unwrap();
        let (b, _) = pane.export(1, 0).unwrap();
        assert_eq!(a.screen_primary, b.screen_primary, "export is a pure read");
        // And the pane still works afterwards.
        pane.write(b"\n");
        assert!(!pane.is_crashed());
    }

    #[test]
    fn the_alacritty_engine_refuses_to_export_by_name() {
        let pane = TermPane::spawn_env_with_engine_for_test_alac(
            Some(("/bin/sh".into(), vec!["-c".into(), "sleep 5".into()])),
            None, 20, 4, &[], 1000,
        );
        let Some(pane) = pane else {
            return; // built without the vendored engine; nothing to assert
        };
        match pane.export(1, 0) {
            Err(ExportError::EngineUnsupported { engine }) => {
                assert_eq!(engine, "alacritty");
            }
            other => panic!("expected a named refusal, got {other:?}"),
        }
    }
```

The third test needs a way to force the alacritty backend regardless of the
build's default. If no such test constructor exists, add one — a
`#[cfg(test)]`-only associated function on `TermPane` that calls
`AlacPane::spawn_env` directly and returns `None` when the `vendored` feature
is off. Do not achieve it by setting `RT_ENGINE` from the test: the engine
choice is read once per process behind a `Once`, so an env var set inside one
test would leak into every other test in the binary.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt-engine export`
Expected: FAIL — `no method named export found`.

- [ ] **Step 3: Write the error type and the vt-term arm**

In `crates/rt-engine/src/lib.rs`, beside the other public types:

```rust
/// Why a pane could not be exported for a cross-process move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// This pane runs on an engine that cannot export its state. Phase 2 covers
    /// the in-house vt-term engine; the vendored alacritty engine is the
    /// differential-testing oracle and fallback, and gains export later if it
    /// is ever wanted. Named rather than silent so the UI can say which pane
    /// and why.
    EngineUnsupported { engine: &'static str },
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::EngineUnsupported { engine } => {
                write!(f, "the {engine} engine cannot export a pane for transfer")
            }
        }
    }
}

impl std::error::Error for ExportError {}
```

In `crates/rt-engine/src/vtpane.rs`, on `impl VtPane`:

```rust
    /// Read this pane's live state into the cross-process wire model.
    ///
    /// A pure read under the term lock: nothing is written, nothing consumed,
    /// no descriptor moves. Returns the pane plus its scrollback newest-first.
    pub fn export(
        &self,
        pane_uid: u64,
        scrollback_budget: usize,
    ) -> (rt_handoff::pane::PaneWire, Vec<rt_handoff::grid::Line>) {
        let term = self.lock_term();
        crate::handoff::export_term(&term, pane_uid, self.pid().unwrap_or(0), scrollback_budget)
    }
```

- [ ] **Step 4: Write the dispatch**

In `crates/rt-engine/src/lib.rs`, on `impl TermPane`, beside the other dispatching methods:

```rust
    /// Read this pane's state for a cross-process move. See `ExportError`.
    pub fn export(
        &self,
        pane_uid: u64,
        scrollback_budget: usize,
    ) -> Result<(rt_handoff::pane::PaneWire, Vec<rt_handoff::grid::Line>), ExportError> {
        match self {
            TermPane::Vt(p) => Ok(p.export(pane_uid, scrollback_budget)),
            TermPane::Alac(_) => Err(ExportError::EngineUnsupported { engine: "alacritty" }),
        }
    }
```

Make `mod handoff;` reachable from `vtpane.rs` (`pub(crate) mod handoff;` if needed) and re-export what callers in later phases will want: `pub use handoff::StyleTable;` is not needed, but `rt_handoff` itself should be re-exported as `pub use rt_handoff;` so `rt-session` need not add its own path dependency in phase 2b.

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test -p rt-engine`
Expected: PASS. These tests spawn real PTYs and sleep; they are slower than the unit tests.

- [ ] **Step 6: Commit**

```bash
git add crates/rt-engine/src/lib.rs crates/rt-engine/src/vtpane.rs
git commit -m "feat(handoff): TermPane::export with a named refusal for the vendored engine"
```

---

### Task 6: Real-PTY integration test

The unit tests drive `Term` directly. This one drives a real shell through a
real PTY and exports what actually arrived — the only test here that would
catch a break in the path between the pty, the reader thread and the grid.

**Files:**
- Create: `crates/rt-engine/tests/export.rs`

**Interfaces:**
- Consumes: `rt_engine::TermPane`, `rt_handoff::pane::PaneWire`.
- Produces: nothing; this is the slice's acceptance test.

- [ ] **Step 1: Write the test**

`crates/rt-engine/tests/export.rs`:

```rust
//! Export a live pane driven by a real shell through a real PTY.
//!
//! The unit tests in `handoff.rs` feed a `Term` directly. This one goes through
//! the whole path — fork, pty, reader thread, parser, grid — and is what would
//! catch a break between them.

use std::time::{Duration, Instant};

use rt_engine::TermPane;

/// Spawn a shell running `script`, then poll until `probe` sees what it wants
/// or the deadline passes. Draining events is what a real host does each frame.
fn pane_running(script: &str, cols: usize, rows: usize, probe: impl Fn(&TermPane) -> bool) -> TermPane {
    let mut pane = TermPane::spawn_env(
        Some(("/bin/sh".into(), vec!["-c".into(), script.into()])),
        None,
        cols,
        rows,
        &[],
        10_000,
    )
    .expect("spawn a pane");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        let _ = pane.drain_events();
        if probe(&pane) {
            return pane;
        }
    }
    panic!("the pane never produced the expected output within 10s");
}

fn line_text(wire: &rt_handoff::pane::PaneWire, row: usize) -> String {
    wire.screen_primary.lines[row].runs.iter().map(|r| r.text.as_str()).collect()
}

fn has_char(pane: &TermPane, want: char) -> bool {
    pane.snapshot().rows.iter().any(|r| r.iter().any(|c| c.c == want))
}

#[test]
fn a_real_shells_output_survives_export_and_the_wire() {
    let pane = pane_running("printf 'hello from a real pty'; sleep 30", 60, 8, |p| has_char(p, 'h'));
    let (wire, _scroll) = pane.export(1, 0).expect("in-house engine exports");

    assert!(line_text(&wire, 0).starts_with("hello from a real pty"));

    // And the whole thing survives a round trip through the frozen format.
    let back = rt_handoff::pane::PaneWire::decode(&wire.encode()).expect("decode");
    assert_eq!(back.screen_primary, wire.screen_primary);
    assert_eq!(back.cols, 60);
    assert_eq!(back.rows, 8);
}

#[test]
fn colours_written_by_a_real_program_stay_indexed() {
    // The property the whole format hinges on: an indexed colour must not be
    // resolved to RGB on the way out, or a moved pane stops following the
    // receiving window's palette and the index can never be recovered.
    let pane = pane_running(
        "printf '\\033[38;5;200mPINK\\033[m'; sleep 30",
        20,
        4,
        |p| has_char(p, 'P'),
    );
    let (wire, _) = pane.export(1, 0).unwrap();
    let run = wire.screen_primary.lines[0]
        .runs
        .iter()
        .find(|r| r.text.starts_with("PINK"))
        .expect("the coloured run");
    assert_eq!(
        wire.style_table[run.style_id as usize].fg,
        rt_handoff::style::Colour::Indexed(200)
    );
}

#[test]
fn scrollback_from_a_real_program_comes_back_newest_first() {
    let pane = pane_running(
        "for i in $(seq 1 40); do echo line$i; done; sleep 30",
        20,
        5,
        |p| has_char(p, '4'),
    );
    let (_, scroll) = pane.export(1, 100).unwrap();
    assert!(!scroll.is_empty(), "40 lines through a 5-row screen leaves history");

    let text_of = |l: &rt_handoff::grid::Line| -> String {
        l.runs.iter().map(|r| r.text.as_str()).collect()
    };
    let first = text_of(&scroll[0]);
    let last = text_of(&scroll[scroll.len() - 1]);
    let n = |s: &str| -> usize { s.trim().trim_start_matches("line").parse().unwrap_or(0) };
    assert!(n(&first) > n(&last), "newest first: {first:?} must precede {last:?}");
}

#[test]
fn an_alt_screen_program_exports_both_screens() {
    let pane = pane_running(
        "printf 'BENEATH'; printf '\\033[?1049h'; printf 'ONTOP'; sleep 30",
        20,
        4,
        |p| has_char(p, 'O'),
    );
    let (wire, _) = pane.export(1, 0).unwrap();
    assert_eq!(wire.active_screen, 1, "the alt screen is showing");
    assert!(line_text(&wire, 0).starts_with("ONTOP"), "primary grid = what is displayed");
    let alt = wire.screen_alt.as_ref().expect("the held-aside screen rides along");
    let beneath: String = alt.lines[0].runs.iter().map(|r| r.text.as_str()).collect();
    assert!(beneath.starts_with("BENEATH"), "got {beneath:?}");
}

#[test]
fn a_wide_glyph_from_a_real_program_spans_two_columns() {
    let pane = pane_running("printf '日本語'; sleep 30", 20, 4, |p| has_char(p, '日'));
    let (wire, _) = pane.export(1, 0).unwrap();
    let run = &wire.screen_primary.lines[0].runs[0];
    assert_eq!(run.flags & rt_handoff::grid::run_flags::WIDE, rt_handoff::grid::run_flags::WIDE);
    assert_eq!(run.text, "日本語");
    assert_eq!(run.cell_span, 6);
}
```

- [ ] **Step 2: Run the tests**

Run: `cargo test -p rt-engine --test export`
Expected: PASS, 5 tests. These spawn real shells and poll; expect a few seconds.

If a test times out, the probe is likely wrong rather than the export — check
what the pane actually holds by printing the snapshot before assuming the
export path is at fault.

- [ ] **Step 3: Run the whole slice green**

Run: `cargo test -p vt-term -p rt-engine -p rt-handoff`
Expected: PASS, with no warnings.

- [ ] **Step 4: Commit**

```bash
git add crates/rt-engine/tests/export.rs
git commit -m "test(handoff): export a live pane driven by a real shell through a real pty"
```

---

## Definition of done for phase 2a

- `cargo test -p vt-term -p rt-engine -p rt-handoff` is green and warning-free.
- `TermPane::export()` returns a `PaneWire` for a vt-term pane and a named `ExportError::EngineUnsupported` for an alacritty one.
- An indexed colour written by a real program reaches the wire as `Colour::Indexed`, proven by an integration test rather than a unit test.
- Export is demonstrably a pure read: calling it twice on a live pane gives equal results and the pane still works afterwards.
- `vt-term` gained no dependencies; `rt-handoff` gained no dependencies; `rt-engine` gained exactly one path dependency on `rt-handoff`.
- `tab_stops`, `title_stack`, `uri_table` and `image_table` ship absent, and a test says so out loud rather than leaving it to be discovered.
- `project-map.js` gains an `rt-handoff` → `rt-engine` export seam note, and `project.updated` is set, in the final commit of the slice.

## What this slice deliberately does NOT do

Named so a reviewer does not read them as gaps:

- **No `adopt()`.** Reading state out is phase 2a; building a pane from it is 2b.
- **No freeze/thaw.** Export reads a live terminal under its lock; it does not stop the reader thread, and a byte arriving mid-export lands in the next frame as usual. That race is harmless for a read that nothing yet depends on, and is exactly what freeze/thaw exists to close in 2b before a real handoff relies on it.
- **No fd handling.** No dup, no consolidation of VtPane's three descriptors, no `SCM_RIGHTS`, no pidfd. Phase 2c.
- **No host-level fields.** `rt-session` overlays title, cwd, group and the rest in 2b.
- **No alacritty export.** By decision; the arm is present and refuses by name.
