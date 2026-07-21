# vt-term design

> Status: **matches the oracle on all fuzzed input** (Phase 3). Common sequences +
> scrollback are in; the full differential (grid, cursor, modes, history, wide chars, charsets) is 0/10000 vs
> the vendored oracle on x86_64 and riscv64. Reflow and OSC/DCS semantics
> remain (see `docs/engine-divergence.md`). Code: `crates/vt-term/src/lib.rs`; doc and
> code stay in lockstep.

The in-house Term: consumes [`vt_parser`]'s action stream (it implements
`vt_parser::Perform`) and maintains the terminal grid — cells, cursor, pen, scroll
region, and modes — exposing the observable state the harness reads. Correctness is
"produce the same state as the vendored `alacritty_terminal`", verified by differential
testing against that oracle (spec cases + fuzz + replay corpus). Where alacritty has a
quirk, we match the quirk (it is the reference, not the abstract spec).

## What's implemented

- **Data model.** `Cell { c, fg, bg, attrs }`; `Color = Default | Indexed(u8) |
  Rgb(u8,u8,u8)`; `Attrs` (bold/dim/italic/underline/inverse/hidden/strikeout). The
  grid is `Vec<Vec<Cell>>` (rows × cols); a saved copy backs the alternate screen.
- **Printing + autowrap.** Printable chars land at the cursor with the current pen.
  DECAWM uses xterm's *deferred wrap*: printing on the last column places the glyph and
  sets `pending_wrap` (cursor stays); the NEXT printable first wraps (col 0 + line
  feed). Autowrap off overwrites the last column instead.
- **Cursor motion.** CUU/CUD/CUF/CUB, CHA/HPA, VPA, CUP/HVP (origin-aware), with the
  region-aware clamping alacritty uses. Any move clears `pending_wrap`.
- **Erase.** ED (0/1/2) and EL (0/1/2), ECH. ED(Above) matches alacritty's
  `cursor.line > 1` quirk; erased cells take the current background.
- **Scroll.** Line feed / reverse line feed within the scroll region (DECSTBM); IND/RI/
  NEL; SU/SD; IL/DL and ICH/DCH; region scrolling via slice rotation.
- **SGR.** Attributes + reset, ANSI 30–37/90–97 and 40–47/100–107 (→ `Indexed`),
  `38;2;r;g;b`/`48;2` (→ `Rgb`), `38;5;n`/`48;5` (→ `Indexed`), 39/49 (→ `Default`).
- **Modes.** DECAWM (?7), DECTCEM (?25), DECCKM (?1), DECOM (?6), alternate screen
  (?47/?1047/?1049 with cursor+screen save/restore). DECSC/DECRC, RIS.
- **Tabs.** Every-8 stops, matching alacritty's write-`\t`-into-the-start-cell quirk.
- **Charsets.** G0–G3 designations + DEC special graphics (line-drawing) mapping, SI/SO
  invocation. Designations are per-cursor (saved by alt/DECSC); the active charset `gl`
  is Term-global.
- **Wide characters.** CJK/emoji occupy two cells (glyph + `spacer`), with a leading
  spacer + wrap at the right edge, and the WIDE flag derived from char width — matching
  alacritty cell-for-cell (a `spacer` flag distinguishes a real spacer from an erased
  blank for the clear-wide and emptiness rules). Combining marks attach to the base
  (observably ignored) except the pending-wrap boundary edge (ledgered).
- **Scrollback + viewport.** A ring (cap 10 000) of lines scrolled off the top of the
  *primary* screen. Grows only on a top-anchored scroll and on `\x1b[2J` (which scrolls the
  viewport into history, not a plain blank); the alt screen has none. `history_size` tracks
  the oracle exactly. A `display_offset` scrolls the view up into history
  (`scroll_display`/`scroll_to_bottom_view`); `cell_at(abs, col)` reads any absolute line
  (`topmost..=bottommost`, history negative), so a host can render scrollback, extract
  selections, and search. New output while scrolled keeps the view anchored.
- **Reporting modes.** Mouse reporting (DECSET 1000/1002/1003 → `wants_mouse`/`wants_motion`,
  1006 → `mouse_sgr`, mutually exclusive like alacritty), DECSCUSR cursor shape
  (`cursor_shape`), and the OSC 0/2 window title (`take_title`) are tracked and exposed for
  a host to act on.
- **Reflow on resize.** Resize does lines first (a pure row move that scrolls to keep the
  cursor placed — into scrollback on the primary, discarded on the alt), then columns.
  Column reflow is a **faithful port of alacritty's `grow_columns`/`shrink_columns`**: a
  row-by-row rewrap over the whole buffer (history + visible, height-indexed from the
  bottom), carrying the cursor through the exact split arithmetic. `occ` is never read by
  that logic (physical `len()` + content-based `is_clear` suffice), so no row refactor was
  needed. The alt screen doesn't reflow (truncate/extend + clamp), matching alacritty.
  ~95% of random resizes match the oracle exactly; the deepest wide-glyph edges remain —
  see the ledger.

## Verification

`vt-conformance` drives vt-term through the same battery as the oracle: `vtterm_spec.rs`
runs the 32 spec cases; `vtterm_diff.rs` diffs curated scripts against the oracle. The
random-fuzz and replay-corpus differential sweeps grow next, tracked by the divergence
ledger. All differential comparisons go through the neutral `ScreenState` so vt-term and
alacritty compare apples-to-apples.

## Open (see `docs/engine-divergence.md`)

The full differential (grid, cursor, modes, history, wide chars, charsets) is **0/10000** vs the
oracle. What remains: **reflow on resize** (the hard part, last), OSC/DCS semantics,
colon sub-param SGR, and the one obscure combining-mark-at-pending-wrap edge.

## Performance

Benchmarked vs `alacritty_terminal` (the full Term — parse **and** build the grid) with
`examples/term_bench.rs` (`cargo run --release --example term_bench -p vt-conformance`),
80×24, 4 MiB workloads. First measurement (x86_64, 2026-07-21): **geomean ~0.7× alacritty**
— faster on some workloads (unicode, control-heavy, real spiral capture at ~1.1–1.35×),
slower on others. Note the parser *beats* vte; the gap is the **grid layer**.

Where the time goes, and the optimisation path (mirrors how the parser went from 0.65× to
1.17× vs vte):

1. **`occ` (occupied-length) tracking — LANDED (2026-07-21).** A `\x1b[2J`-heavy TUI
   workload ran at only ~0.22×: we reset all 80 cols × 24 rows every clear, while
   alacritty's `Row::reset` only touches the *written* cells. Now each row is a `Line
   { cells, occ }`: writes extend `occ` (via `IndexMut`), and clears/scrolls reset only the
   `[0, occ)` prefix (with alacritty's bg-discriminant check for a changed erase colour).
   Because `occ` lives in the row, it travels through scroll rotations and scrollback for
   free. **Result: the clear-heavy workload went 0.22× → ~0.85×, geomean ~0.73× → ~0.85×,
   real spiral ~0.79× → ~0.87×** — with a small (~5%) regression on write-heavy plain text
   from the per-write `occ` bump. Correctness pinned: non-resize fuzz still 0/10000,
   chunk-invariance 0/2000, reflow unchanged (`occ` is never observed).
2. **Packed `Cell` — LANDED (2026-07-21).** The seven attribute `bool`s plus the
   `spacer`/`wrapline` markers are now one packed `u16` (`Cell { c, fg, bg, flags }`),
   shrinking `Cell` **24 → 16 bytes** — *smaller* than alacritty's `Cell`, which also
   carries an `Option<Arc<CellExtra>>`. Read via accessor methods; module-internal code
   touches the bits. **Result: geomean ~0.85× → ~0.91×, with mixed-tui at parity (~1.0×),
   real spiral ~0.97×, unicode/sgr ~1.0×.** Correctness pinned: fuzz 0/10000,
   chunk-invariance 0/2000, reflow unchanged.
3. **"Contiguous grid" — was a MISDIAGNOSIS, dropped.** The earlier note claimed alacritty
   uses one contiguous buffer; it does **not** — `Storage` is `Vec<Row<T>>` with `Row =
   Vec<T> + occ` in a ring (zero-offset for O(1) scroll), i.e. exactly our `Vec<Line>`. A
   flat cell buffer would *hurt* scroll (copying cells instead of rotating row pointers).
   Our `Vec<Line>` already rotates pointers on scroll (O(rows), not O(rows·cols)). No win
   here; not pursued.

Remaining gap is the **write-heavy** workloads (plain text ~0.77×, control ~0.79×), where
the cost is per-char `put_char` dispatch (autowrap/width checks per glyph). The lever there
is a batched `print_str` fast path for runs of simple narrow ASCII (bump `occ` once, skip
the per-char wrap machinery until the run hits the edge). That, plus a possible ring for
truly O(1) scroll, is the next pass. Also landed earlier: scroll/erase blank rows in place.
Correctness stays pinned by the 0/10000 differential.
