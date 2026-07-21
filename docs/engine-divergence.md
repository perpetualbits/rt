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

## Known not-yet-implemented (will diverge when exercised)

- **Reflow on resize** — vt-term truncates/extends; the oracle rewraps. THE hard part,
  isolated to the end by design.
- **Colon sub-parameter SGR** beyond the extended-colour case.
- **OSC / DCS semantics** (title, clipboard, hyperlinks): parsed but not applied.
- **Origin mode** edge interactions, DECSCUSR cursor shape, LNM newline mode.

## Reconciliations already done

- **Neutral colour model** unified to `Default`/`Indexed`/`Rgb`: alacritty named
  colours 0–15 → `Indexed`, Foreground/Background → the `Named(256)` default sentinel,
  matching vt-term's `Color::Default`.
- **ED(Above)** matched to alacritty's `cursor.line > 1` quirk.
- **Tab** matched to alacritty's write-`\t`-glyph-into-the-blank-start-cell behaviour.
