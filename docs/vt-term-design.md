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

1. **No `occ` (occupied-length) tracking — the dominant lever.** A `\x1b[2J`-heavy TUI
   workload runs at only ~0.22×: we reset all 80 cols × 24 rows every clear, while
   alacritty's `Row::reset` only touches the *written* cells (a near-empty screen → ~40×
   less work). Tracking a per-row `occ` and clearing/scrolling only up to it is the single
   biggest win. It wants a `Row { cells, occ }` type (occ travels with the row on rotate).
2. **Grid layout.** `Vec<Vec<Cell>>` is a double indirection with a heap allocation per
   row and poor locality; alacritty uses one contiguous buffer. A general ~1.3× penalty.
3. **`Cell` size.** Seven separate `bool`s for attributes; packing them into bitflags
   shrinks the per-cell memcpy on fills/scrolls.

Already landed: scroll/erase **blank rows in place** (reuse the row's allocation instead
of allocating a fresh `Vec`), which lifted the scroll-heavy workloads. The `occ` + `Row`
refactor is the next focused pass; correctness stays pinned by the 0/10000 differential.
Not yet benchmarked on riscv64.
