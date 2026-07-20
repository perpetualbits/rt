# vt-term design

> Status: **foundation implemented** (Phase 3 in progress). The common sequences are in
> and pass the spec cases + curated differential; scrollback, reflow, wide chars, and
> the last-column wrap edge are open (see `docs/engine-divergence.md`). Code:
> `crates/vt-term/src/lib.rs`; doc and code stay in lockstep.

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

## Verification

`vt-conformance` drives vt-term through the same battery as the oracle: `vtterm_spec.rs`
runs the 32 spec cases; `vtterm_diff.rs` diffs curated scripts against the oracle. The
random-fuzz and replay-corpus differential sweeps grow next, tracked by the divergence
ledger. All differential comparisons go through the neutral `ScreenState` so vt-term and
alacritty compare apples-to-apples.

## Open (see `docs/engine-divergence.md`)

Scrollback (history), reflow on resize (the hard part, last), wide characters, colon
sub-param SGR, OSC/DCS semantics, charsets — and the last-column wrap × scroll edge that
accounts for most of the current ~3.5% grid-fuzz divergence.

## Performance

Same discipline as the parser: benchmark vs alacritty on x86_64 AND riscv64 (milkv) via
`ci/verify.sh` once the Term is feature-complete enough for a fair comparison; the grid
representation and damage tracking are where the speed work lands.

## Sections to write (outline)

1. **Data model** — the cell (glyph + attributes + colour), the grid, and the
   scrollback ring. Memory layout and *why* (cache-friendliness on the hot path; the
   measurement). Compare alacritty's and foot's representations.
2. **Cursor & printing** — advance, wrap (pending-wrap / deferred wrap semantics),
   insert vs replace, wide characters + combining/zero-width, tab stops.
3. **SGR & colour** — attributes, 16/256/truecolour, palette resolution.
4. **Modes** — DECSET/DECRST private modes (the big compatibility surface), scroll
   regions (DECSTBM), origin mode, autowrap, charsets (G0–G3), insert mode.
5. **Screens** — primary vs alternate screen; save/restore cursor.
6. **Scrollback** — the ring buffer, display offset, scroll-region interaction with
   history, limits.
7. **Damage tracking** — what changed since the last snapshot, kept tight (never
   over-report — that was rt's whole ssh-X performance story). The snapshot the
   renderer consumes.
8. **Reflow** — THE hard part. Rewrapping wrapped lines on resize while preserving
   scrollback, selection, and cursor. Study alacritty's `grid/resize.rs` and foot.
   Ships LAST; interim behaviour is clear/redraw-on-resize with reflow tests on the
   divergence ledger (`docs/engine-divergence.md`).
9. **OSC handlers** — title, clipboard (OSC 52), hyperlinks (OSC 8), colour queries.
10. **Mouse & bracketed paste** — reporting modes (1000/1002/1003/1006), the query
    methods (`wants_mouse`, `mouse_sgr`, …) the engine contract exposes.
11. **Performance** — per-trick, each with its measurement; comparison vs alacritty and
    foot.
12. **Verification** — differential fuzzing against the oracle; esctest/vttest; property
    invariants (cursor in bounds, scrollback ≤ limit, reflow preserves content, resize
    round-trip); real-app replay corpora; the divergence ledger.
