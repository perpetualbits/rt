# vt-term design

> Status: **matches the oracle on all fuzzed input** (Phase 3). Common sequences +
> scrollback are in; the full differential (grid, cursor, modes, history) is 0/10000 vs
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
- **Wide characters.** CJK/emoji occupy two cells (glyph + `spacer`), with a leading
  spacer + wrap at the right edge, and the WIDE flag derived from char width — matching
  alacritty cell-for-cell (a `spacer` flag distinguishes a real spacer from an erased
  blank for the clear-wide and emptiness rules). Combining marks attach to the base
  (observably ignored) except the pending-wrap boundary edge (ledgered).
- **Scrollback.** A ring (cap 10 000) of lines scrolled off the top of the *primary*
  screen. Grows only on a top-anchored scroll and on `\x1b[2J` (which scrolls the
  viewport into history, not a plain blank); the alt screen has none. `history_size`
  tracks the oracle exactly. `display_offset` is observed at 0 (bottom); viewport
  scrolling to read the ring is future work.

## Verification

`vt-conformance` drives vt-term through the same battery as the oracle: `vtterm_spec.rs`
runs the 32 spec cases; `vtterm_diff.rs` diffs curated scripts against the oracle. The
random-fuzz and replay-corpus differential sweeps grow next, tracked by the divergence
ledger. All differential comparisons go through the neutral `ScreenState` so vt-term and
alacritty compare apples-to-apples.

## Open (see `docs/engine-divergence.md`)

The full differential (grid, cursor, modes, history, wide chars) is **0/8000** vs the
oracle. What remains: **reflow on resize** (the hard part, last), OSC/DCS semantics,
charsets, colon sub-param SGR, and the one obscure combining-mark-at-pending-wrap edge.

## Performance

Same discipline as the parser: benchmark vs alacritty on x86_64 AND riscv64 (milkv) via
`ci/verify.sh` once the Term is feature-complete enough for a fair comparison; the grid
representation and damage tracking are where the speed work lands. Not yet started.
