# Newspaper columns

A feature unique to rt (Terminator has none): a single pane can display its
output as **N newspaper columns**. Text that reaches the bottom of column 1
continues at the top of column 2, and so on — like a newspaper page. Screenshots:
- `docs/screenshots/newspaper-columns.png` — a shell running `seq 1 200` in 3
  columns (col 1 ends 139, col 2 begins 140; col 2 ends 170, col 3 begins 171).
- `docs/screenshots/vim-columns.png` — **vim** editing a 300-line file in 3
  columns (col 1 = lines 1–101, col 2 = 102–202, col 3 = 203–300 + status line).

## The model (transparent to the app)

The key idea: **the app is told it has one tall, narrow screen; we re-tile that
screen into columns purely at display time.** The application — shell, `vim`,
`vi`, `neovim`, anything — never knows.

1. **The PTY is `col_cells` wide × `N × rows` tall.** If a pane is 100 cells
   wide and 40 tall and shows 3 columns (2-cell gaps), each column is
   `(100 − 2·2)/3 = 32` cells wide, and the screen handed to the app is
   `32 × 120` (3·40). The app draws into that whole tall screen as usual.
2. **We slice the visible screen back into columns column-major.** Row `r` of
   the tall screen is drawn in column `r / rows` at line `r % rows`. So the
   first `rows` lines are column 1, the next `rows` are column 2, etc.
3. **Scrolling shifts the whole tall viewport by whole lines**, driven by the
   terminal's own scrollback (mouse wheel → `Term::scroll_display`). Because the
   viewport is laid out column-major, a line leaving the bottom of column *n*
   reappears at the top of column *n+1* when scrolling up, and the mirror when
   scrolling down — the flow the feature promises — with the app none the wiser.

### Why this beats the first attempt
The first implementation kept the PTY at pane height and pulled the *extra*
lines a multi-column view needs out of **scrollback**. That only works for a
shell whose history is the content; a full-screen app (`vim`) draws only into
its screen and uses no scrollback, so it would fill just one column. Making the
screen itself taller (this model) means full-screen apps columnize transparently
— see `vim-columns.png`. There is **no alt-screen special-case** any more.

## Controls

| Key | Action |
|-----|--------|
| `Ctrl+.` | add a column (`ColumnsMore`, up to `MAX_COLUMNS = 8`) |
| `Ctrl+,` | remove a column (`ColumnsFewer`, floor 1 = normal pane) |
| mouse wheel | scroll the focused pane's viewport through scrollback |

rt **starts every pane single-column**; columns are opt-in via `Ctrl+.`.
(`Ctrl+symbol` without Shift is used deliberately: winit reports *shifted*
symbols as different characters, which would break a `Ctrl+Shift+.` binding.)

## Where it lives (the seams that avoided an overhaul)

- **`rt-engine::TermPane`** — `scroll(delta)` drives the terminal scrollback;
  `snapshot()` returns the visible screen (which *is* `N × rows` tall in column
  mode). `snapshot_lines(top,rows)`/`line_bounds()`/`is_alt_screen()` remain as
  history-aware primitives (tested in `crates/rt-engine/tests/history.rs`).
- **`rt-session`** — per-pane column count + `column_layout(id,rect) →
  {count, col_cells, rows, gap}`, shared by `relayout` (which sizes the PTY to
  `col_cells × count·rows`) and the renderer. Tested in
  `crates/rt-session/tests/controller.rs`.
- **Renderer** (`crates/rt/src/main.rs::redraw`) tiles the tall screen into
  columns and draws separators in the gaps.

## On full-screen apps

`vim`/`vi`/`neovim` work great columnized — they lay out text top-to-bottom in a
narrow screen, which is exactly newspaper flow. Programs that assume a roughly
normal aspect ratio and paint fixed regions (e.g. `btop`/`htop`) will look wrong
in columns; that is the user's call — use a single column (`Ctrl+,` to 1) for
those. rt does not try to detect and override the user's choice.

## Not yet done

- A scroll-position indicator for the column view.
- Selection/copy across column boundaries.
- Per-pane column count persisted in saved layouts.
