# Newspaper columns

A feature unique to rt (Terminator has no equivalent): a single pane can display
its output as **N newspaper columns**. Text that reaches the bottom of column 1
continues at the top of column 2, and so on — like a newspaper page. Screenshot:
`docs/screenshots/newspaper-columns.png` (3 columns showing `seq 1 200`: col 1
ends at 139, col 2 begins at 140; col 2 ends at 170, col 3 begins at 171).

## The model

A newspaper-column pane is a **view transformation over the terminal's line
buffer**, not a change to the terminal itself:

1. **The PTY runs at one column's width.** If a pane is 100 cells wide and shows
   3 columns (with a 2-cell gap between each), each column is
   `(100 − 2·2) / 3 = 32` cells wide, and the shell wraps at 32. This is what
   makes the text flow as narrow newspaper columns.
2. **The viewport shows `N × rows` lines at once**, laid out *column-major*: the
   first `rows` lines fill column 1 top-to-bottom, the next `rows` fill column 2,
   etc. The newest line sits at the bottom of the last column (bottom-anchored).
3. **Scrolling shifts the whole run by whole lines.** Scroll up (toward older
   history) and the line leaving the bottom of column *n* reappears at the top of
   column *n+1*; scroll down and the line leaving the top of column *n* reappears
   at the bottom of column *n−1*. This falls out automatically from (2): moving
   the run's first line earlier by one shifts every line one slot earlier in
   column-major order, which is exactly one position "back" across the column
   boundary.

## Controls

| Key | Action |
|-----|--------|
| `Ctrl+.` | add a column (`ColumnsMore`, up to `MAX_COLUMNS = 8`) |
| `Ctrl+,` | remove a column (`ColumnsFewer`, floor 1 = normal pane) |
| mouse wheel | scroll the focused pane's column view through history |

(`Ctrl+symbol` without Shift is used deliberately: winit reports *shifted*
symbols as different characters, which would break a `Ctrl+Shift+.` binding.)

## Where it lives (the seams that avoided an overhaul)

The feature was deliberately built on two foundations placed early, because they
are expensive to retrofit:

- **`rt-engine::TermPane::snapshot_lines(top, rows)`** — a history-aware read of
  an arbitrary line range (into scrollback), plus `line_bounds()` and
  `is_alt_screen()`. Column view needs `N × rows` lines at once; the plain
  `snapshot()` only ever returned the visible screen. Tested in
  `crates/rt-engine/tests/history.rs`.
- **Per-pane view state in `rt-session`** — `columns_of(id)`, `col_scroll_of(id)`,
  and `column_layout(id, rect) → {count, col_cells, rows, gap}`, shared by
  `relayout` (to size the PTY to one column) and the renderer (to place columns).
  Tested in `crates/rt-session/tests/controller.rs`.

The renderer (`crates/rt/src/main.rs::redraw`) then just walks the `N × rows`
lines column-major and blits them, drawing thin separators in the gaps.

## Deliberate limitation

Newspaper flow is meaningless for a full-screen TUI (`vim`, `htop`, `less`),
which owns the whole screen via the terminal's **alternate screen**. So when
`is_alt_screen()` is true the pane always renders as a single column, regardless
of the column count, and reverts to columns when the TUI exits. This mirrors how
alt-screen already suspends scrollback in normal terminals.

## Not yet done

- A scroll *indicator* / position marker for the column view.
- Preserving the column count across an alt-screen round-trip is automatic, but
  the scroll offset is not reset on new output at the bottom — a future tweak may
  auto-snap to bottom when the user is already at bottom and new output arrives.
- Selection/copy across column boundaries.
