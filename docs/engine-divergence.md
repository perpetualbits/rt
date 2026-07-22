# Engine divergence ledger

Where the in-house `vt-term` does NOT yet match the vendored `alacritty_terminal`
oracle. The Phase-3 process (see `docs/own-engine-plan.md`) is to drive this list to
empty (or to *intentional*, documented differences) under the `vt-conformance` harness.
Each entry: what diverges, the measured impact, and the plan.

Status snapshot (2026-07-21):
- Spec cases (`spec.rs`, 32 cases): **PASS** against vt-term.
- Curated differential (`vtterm_diff.rs`, 16 scripts): **PASS**.
- Random-fuzz FULL differential (`vtterm_fuzz.rs`, 8000 scripts) — grid, cursor, modes,
  AND scrollback history: **0 divergences** (verified 0/10000 in a wider sweep). Locked
  in as a test, green on x86_64 and riscv64.

**vt-term now matches the vendored oracle exactly on every fuzzed input.** The open
items below are not-yet-exercised features (nothing in the fuzz reaches them yet).

### Fixed under the harness (2026-07-21)
Four alacritty behaviours the differential fuzz surfaced, each traced to a minimal
reproducer via delta-debugging and matched:
- **LF keeps `pending_wrap`** — linefeed/newline do NOT clear the deferred-wrap flag
  (they did in the first draft), so a char after a bare LF wraps one more line. This
  one fix took grid divergence 3.2%→0.36%.
- **EL-Right is a no-op while a wrap is pending** (`clear_line … if input_needs_wrap`).
- **Private-marker CSI** (`?…H` etc.) is ignored; only `?…h/l` (DECSET/DECRST) act.
- **`pending_wrap` is part of the cursor** — saved/restored by the alternate screen and
  DECSC/DECRC.

Scrollback (the ring buffer) reconciled to the oracle:
- **History grows only on a top-anchored scroll** (`scroll_up` when the region starts at
  row 0), never inside a DECSTBM region that starts below the top, never on the alt
  screen.
- **`\x1b[2J` scrolls the viewport into history** (alacritty's `clear_viewport`), not a
  plain blank. `positions` = last-non-empty-row + 1; on an all-empty screen it is 1 when
  history is empty (the scan stops at line 0) and 0 otherwise (it descends to line −1) —
  a genuine iterator edge, matched exactly.
- **The alt screen reports `history_size` 0** (no scrollback); the primary's history is
  preserved and returns on exit. `\x1b[3J` clears scrollback.

Wide characters (CJK/emoji) reconciled to the oracle:
- Wide glyph + trailing spacer placement, the right-edge leading spacer + wrap, and the
  WIDE flag (derived from char width) all match. A `spacer` flag distinguishes a real
  trailing spacer from an erase-left blank, so overwriting a spacer clears the glyph
  (alacritty's clear_wide) but overwriting an EL'd blank does not.
- `clear_viewport`'s emptiness scan treats a spacer as non-empty (matching alacritty's
  is_empty, which also ignores bold/dim/italic).
- DCH clamps its count to the FULL width (not cols−col), so a large count also clears
  cells left of the cursor.
- CNL (`ESC [ E`) / CPL (`ESC [ F`) added; private-marker CSI (`ESC [ ? … H`) ignored.

Charsets (DEC line drawing) reconciled to the oracle:
- G0–G3 designations (`ESC ( ) * + <final>`, `0` = Special) are part of the CURSOR:
  saved/restored by the alt screen and DECSC. The active charset `gl` (SI/SO) is
  Term-GLOBAL and is NOT swapped by the alt screen — matching alacritty exactly. The
  DEC special-graphics map matches `StandardCharset::map` character-for-character.

## Open divergences

- **Combining mark exactly at a pending-wrap boundary.** A zero-width mark arriving when
  the cursor is in the deferred-wrap state resolves the wrap in the oracle but not in
  vt-term (`pending_wrap` is not observable, so the harness only catches it via a later
  op). Obscure — combining marks rarely land on the last column — and parked out of the
  fuzz generator. Everywhere else combining marks match (attach to the base, ignored).

The scrollback ring is implemented; the full differential (grid, cursor, modes, history,
wide chars AND charsets) is **0/10000**. `display_offset` is always observed at 0 (bottom of the
view); reading scrolled-back lines / viewport scrolling are future.

## Reflow on resize — implemented, common cases matched (2026-07-21)

vt-term now reflows (was: truncate/extend). Algorithm mirrors alacritty: **lines first**
(a pure row move — the cursor is kept in view by scrolling top rows into scrollback on the
primary screen, or discarding them on the alt screen), **then columns** (rejoin
`WRAPLINE`-marked soft-wrapped rows into logical lines, re-split at the new width with
leading spacers for wide glyphs at the boundary, re-lay-out bottom-anchored, track the
cursor). The alt screen does not reflow columns (truncate/extend + clamp).

Update (2026-07-21): `reflow_columns` is now a **faithful port** of alacritty's
`grow_columns`/`shrink_columns` (`grid/resize.rs`) — a row-by-row rewrap over the whole
buffer (history + visible, height-indexed from the bottom like `take_all()`), carrying the
cursor through the exact split arithmetic (`Point::sub`/`grid_clamp`). Investigation showed
`occ` is **never read** by that logic (it uses physical `len()` + content-based
`is_clear`), so no `occ`/`Line` refactor was needed — plain `Vec<Cell>` rows suffice.
Line-count changes (`grow_lines`/`shrink_lines`) keep the earlier empirically-derived
implementation.

Result: **non-resize fuzz unchanged (0/10000)**; the random-resize sweep matches the oracle
on **~95%** (≈156/3000 diverge, down from ~242 with the logical-line reimplementation and
~1050 with truncate/extend). Cursor-only divergences dropped 58→20 (the port's inline
cursor arithmetic). Guarded by `tests/vtterm_reflow.rs`. Remaining divergences, to drive to
zero:

- **Wide-glyph reflow edges — root-caused and largely fixed (156/3000 → 28/3000,
  2026-07-22).** The one-column shift around a wide glyph at a wrap boundary was NOT in the
  reflow code at all (the earlier "leading-spacer inference" diagnosis was wrong — the
  inference is provably always correct). It was in the **cell-overwrite path**:
  `clear_wide_left` ported only part of alacritty's `write_at_cursor` cleanup and skipped
  the *"remove leading spacers"* step. When a narrow glyph overwrites a wide glyph that had
  autowrapped to the start of a continuation row, alacritty clears the leading spacer in the
  *previous* row's last column; vt-term left it, so column reflow later misclassified the
  stray spacer (repro `"VXEKSWNANACVKWRm\x1b[5C界世\rX"` 24×8→21×18: oracle `界··X`, vt-term
  `界·X`). Fixed by porting that case, guarded so our single `SPACER` bit only clears a
  *leading* spacer (predecessor at `cols-2` not wide) and never orphans a trailing one.
  Then the sibling *trailing* `WIDE_CHAR_SPACER` clear (alacritty `term/mod.rs:998`) and
  CHT/CBT (`ESC[I`/`ESC[Z`, which were unimplemented — a real cursor gap) were added:
  **156/3000 → 24/3000 (0.8%)**, non-resize fuzz still 0. **Residual ~24** (20 pure-cell +
  2 cursor + 2 history): a *distinct, deeper* set — a one-cell wide-glyph shift in the
  **grow-columns** path (repro reduces to a wide glyph near the right edge on a 24→25 grow),
  plus the deepest cursor/overflow arithmetic and two history-count edges. Each is its own
  investigation; the overwrite-clear family is now closed.
- **Residual cursor edges (~20).** A few cursor positions still off, in the deepest
  split/overflow interactions.

### Synchronized updates (DECSET/DECRST 2026) — implemented (2026-07-21)

The vendored `vte` buffers all bytes between `\x1b[?2026h` and `\x1b[?2026l`, applying
them atomically at the end (or on a 2 MiB cap); vt-parser now does the same, in a layer
above the raw state machine (`Parser::feed`; see `docs/vt-parser-design.md` §6a). Only
*observable* when a feed ends mid-sync (the oracle holds the buffered tail unapplied) —
exactly how a captured stream ends. **Surfaced by the `spiral_stress` replay corpus**, not
the fuzz (the generator emits no 2026). All four corpus fixtures now match the oracle
whole-feed and chunk-split (`tests/replay.rs::replay_corpus_matches_oracle`). The raw
`Parser::advance` path is unchanged, so the parser-vs-`vte` differential and the throughput
bench are unaffected.

## Known not-yet-implemented (will diverge when exercised)

- **Colon sub-parameter SGR** beyond the extended-colour case.
- **OSC / DCS semantics** (title, clipboard, hyperlinks): parsed but not applied.
- **Origin mode** edge interactions, DECSCUSR cursor shape, LNM newline mode.

## Reconciliations already done

- **Neutral colour model** unified to `Default`/`Indexed`/`Rgb`: alacritty named
  colours 0–15 → `Indexed`, Foreground/Background → the `Named(256)` default sentinel,
  matching vt-term's `Color::Default`.
- **ED(Above)** matched to alacritty's `cursor.line > 1` quirk.
- **Tab** matched to alacritty's write-`\t`-glyph-into-the-blank-start-cell behaviour.
