# vt-term design

> Status: **stub** (Phase 3 not yet started). A first-class deliverable — see the
> documentation mandate in `docs/own-engine-plan.md`. It must teach the Term well
> enough that a reader could reimplement it, including the hard parts (reflow, damage).
> Code and this doc stay in lockstep.

The in-house Term: consumes the parser's action stream and maintains the terminal grid
— cells, cursor, scrollback, modes, and damage — exposing the engine contract in
`docs/engine-seam.md`. Correctness is verified by differential testing against vendored
`alacritty_terminal` (the oracle) plus esctest/vttest and real-app replay; speed must
meet or beat alacritty and foot.

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
