# Observable-state edges (modes + cursor shape) — design

**Date:** 2026-07-26
**Status:** approved (design), pending implementation plan
**Scope:** engine hardening — Thread 2 (expand verified coverage), second slice.
Follows the query/report slice (shipped v0.3.15). Closes the mode/cursor-shape
gaps that slice deliberately excluded. See `docs/own-engine-plan.md` (Phase 3)
and `docs/engine-divergence.md`.

## Problem

The query/report differential (`vtterm_report`, 0/4000) and the grid differential
(`vtterm_fuzz`, 0/10000) are at zero only over what they reach. Several mode and
cursor-shape edges are **not implemented in vt-term** or **not observed by the
harness**, so they sit outside both differentials:

- **ANSI `SM`/`RM` (`CSI Ps h` / `CSI Ps l`, no `?`) is entirely unhandled** — it
  falls through to a no-op in `csi_dispatch`'s main match. So **IRM (insert mode,
  4)** and **LNM (newline mode, 20)** do nothing, and DECRQM reports `0` for them
  (which happens to be correct only because they're unimplemented).
- **DECSCUSR cursor shape is implemented but unverified.** vt-term has the full
  `CursorShape` model (Block/Underline/Beam/HollowBlock/Hidden), `set_cursor_shape`,
  and `cursor_shape()`, but the conformance `observe()` (`vtterm.rs`) hardcodes
  `shape: 0`, so the differential never compares it — while the vendored oracle
  reports the real shape.
- **Private flags the query/report slice excluded** — BlinkingCursor (`12`),
  Utf8Mouse (`1005`), UrgencyHints (`1042`), and the mouse trio
  (`1000`/`1002`/`1003`). vt-term doesn't track 12/1042, tracks 1005 only as a
  side effect, and models the three mouse-report modes as one mutually-exclusive
  enum rather than the oracle's three independent bits — which is exactly why the
  `vtterm_report` `QUERIES` pool excluded them (they'd diverge when more than one
  is set).

## Goal

Implement these edges in vt-term to match the vendored oracle, wire cursor shape
into the harness, and **extend the two existing differentials to cover them,
driving each to 0 on x86-64 and riscv-64**. No new verification machinery — reuse
`vtterm_fuzz` (grid/cursor/shape observable) and `vtterm_report` (DECRQM
observable).

## Changes in vt-term (`crates/vt-term/src/lib.rs`)

### ANSI SM/RM plumbing
Add to `csi_dispatch`'s main (non-intermediate) match: `'h' => self.set_ansi_mode(&p, true)`
and `'l' => self.set_ansi_mode(&p, false)`. `set_ansi_mode` handles the ANSI modes
vt-term supports (`4` IRM, `20` LNM); unknown ANSI modes are ignored, matching the
oracle. (Private `?h`/`?l` stays in the intermediate block, unchanged.)

### IRM — insert mode (4)
`insert_mode: bool` field. When set, printing a character **inserts** at the cursor:
the cells from the cursor to the right edge shift one column right, the last cell
falls off, and the new glyph lands at the cursor. Matches alacritty's IRM
(`Term::input` with `Mode::INSERT`): the shift is within the current line only (no
wrap of the shifted-off cell), wide-glyph handling follows the same rules the print
path already uses. Touches the print path (`put_char`; `print_str`'s batched fast
path must fall back to per-char insert when `insert_mode` is on, mirroring how it
already defers wide-glyph-structure cells). Cleared/set only via SM/RM; not part of
the saved cursor.

### LNM — newline mode (20)
`newline_mode: bool` field. When set, LF / VT / FF also carriage-return (column →
0), matching alacritty's `Mode::LINE_FEED_NEW_LINE` handling in `linefeed`. Small,
localized to the linefeed path. (This changes only output translation; input/return
encoding is a host concern and out of scope.)

### DECSCUSR cursor-shape observability + blink
- **Observe wiring:** `crates/vt-conformance/src/vtterm.rs::observe()` reports the
  real shape by mapping vt-term `CursorShape` → neutral `0..=3`
  (Block→0, Underline→1, Beam→2, HollowBlock→3), instead of hardcoded `0`. When the
  cursor is hidden (DECTCEM reset) it returns `None`, exactly as it does today and
  as the oracle's `observe()` does — so "Hidden" (neutral 4) never needs to appear.
  Confirm vt-term's `set_cursor_shape` maps DECSCUSR `Ps` (0/1 blink block, 2 steady
  block, 3 blink underline, 4 steady underline, 5 blink bar, 6 steady bar) to the
  same shapes alacritty's `CursorShape` uses.
- **Blink:** add `cursor_blink: bool`. DECSCUSR sets it from the `Ps` parity
  (odd = blink, even = steady, matching alacritty), and DECSET/DECRST `12` sets it
  directly. Blink is observable only via DECRQM `12` (the neutral shape carries no
  blink bit), so it needs no `observe()` change — only the DECRQM reply.

### Mouse-trio bit refactor (1000/1002/1003)
Replace the `MouseMode` enum + `mouse_mode` field with three independent bits:
`mouse_click`, `mouse_drag`, `mouse_motion`. `set_ansi_mode`/`set_mode` insert/remove
each bit independently on its own DECSET/DECRST (matching alacritty's
`TermMode` bits — setting 1003 does **not** clear 1000). Public accessors are
preserved so **no rt-engine/rt change is needed**:
- `wants_mouse()` = `mouse_click || mouse_drag || mouse_motion`
- `any_motion()` = `mouse_motion`
DECRQM `1000`/`1002`/`1003` report each bit directly.

### Utf8Mouse (1005) and UrgencyHints (1042)
- `utf8_mouse: bool` set by DECSET/DECRST `1005`, reported by DECRQM `1005`.
  Reconcile the encoding interaction to match the oracle: alacritty tracks
  `UTF8_MOUSE` and `SGR_MOUSE` as **independent** bits — setting 1005 does **not**
  clear 1006. vt-term today has `1005 => if set { self.mouse_sgr = false }`, which
  diverges; **remove that coupling** so 1005 sets only `utf8_mouse` and leaves
  `mouse_sgr` alone. This is a behavior reconciliation the `vtterm_report`
  differential will confirm (analogous to the 1007/DECOM fixes in the prior slice).
  Runtime encoding selection is unchanged — rt reads only `mouse_sgr()` (1006).
- `urgency_hints: bool` set by DECSET/DECRST `1042`, reported by DECRQM `1042`.
  Pure flag, no other effect.

### DECRQM updates (`report_mode`)
Extend so it reports the true state of the newly-tracked modes:
- **ANSI:** `4` (insert_mode), `20` (newline_mode); other ANSI modes still `0`.
- **Private:** `12` (cursor_blink), `1000`/`1002`/`1003` (the mouse bits), `1005`
  (utf8_mouse), `1042` (urgency_hints), in addition to the modes it already
  reports. Everything untracked still returns `0`.

## Verification (reuse both differentials)

### Grid differential (`vtterm_fuzz`)
Extend the crate's script generator (`gen_script` in `crates/vt-conformance/src/lib.rs`)
to emit, interleaved with the existing printing/mode/cursor operations:
- ANSI SM/RM for the supported modes: `\x1b[4h`/`\x1b[4l` (IRM), `\x1b[20h`/`\x1b[20l` (LNM).
- DECSCUSR: `\x1b[{Ps} q` for `Ps` in 0..=6.
IRM insert-shifts, LNM CR-on-LF, and cursor-shape changes then surface in the
grid/cursor/shape `ScreenState` diff. Drive to **0** (extend the sweep; keep the
ceiling at 0 as a strict guard).

### DECRQM differential (`vtterm_report`)
Add the now-supported modes to the strand's `QUERIES` and `MUTATORS` pools
(`crates/vt-conformance/tests/vtterm_report.rs`): ANSI `4`/`20`; private `12`,
`1000`, `1002`, `1003`, `1005`, `1042`. Remove them from the documented
"deliberately excluded" set. Drive to **0**.

Both strands run on x86-64 (local + apollo) and riscv-64 (milkv) via the hardened
`ci/verify.sh`.

## Testing (TDD)

Pure unit tests, watched to fail first:
- **IRM:** print into a populated row with insert mode on → cells shift right, last
  drops; wide-glyph at the shift boundary matches the oracle's rule.
- **LNM:** LF with newline mode on moves to column 0 of the next row; off → column
  preserved.
- **DECSCUSR:** each `Ps` 0..=6 maps to the expected shape (and blink) and is
  reported by `observe()` (via the harness) and DECRQM `12`.
- **Mouse bits:** set 1000 then 1003, query both → both report set (the case the
  old enum got wrong); reset one leaves the other.
- **1005 / 1042 / 4 / 20 DECRQM:** set/reset/query returns the right `$y` state.

Then the two differentials to 0 (integration), on both arches.

## Documentation

- `docs/vt-term-design.md` — add IRM, LNM, DECSCUSR (shape + blink), and the
  mouse-bit model to the relevant sections; note ANSI SM/RM is now handled.
- `docs/engine-divergence.md` — move IRM/LNM/12/1005/1042/mouse-trio out of the
  "deliberately-excluded DECRQM modes" list; note the extended `vtterm_fuzz` and
  `vtterm_report` coverage (still 0-ceiling, both arches).
- `docs/own-engine-plan.md` — Phase-3 mode coverage progressed.
- `project-map.js` — the `vt-term` "OSC / DCS & query-report edges" part reflects
  the widened mode coverage; bump `project.updated`.

## What is reused vs new

**Reused:** `csi_dispatch` structure, the `reply`/`report_mode` DECRQM path, both
differential strands and their generators, `ci/verify.sh`, the `take_output` seam,
the `wants_mouse()`/`any_motion()` public accessors (preserved), the print path's
existing wide-glyph deferral in `print_str`.

**New:** `set_ansi_mode` + the `'h'`/`'l'` arms; `insert_mode`/`newline_mode`/
`cursor_blink`/`utf8_mouse`/`urgency_hints` fields and the mouse bits; the
insert-on-print logic; the LNM linefeed change; the `observe()` shape mapping; the
generator additions for SM/RM and DECSCUSR; the expanded `vtterm_report` pools.

## Out of scope

- **Colon-subparam SGR** (`38:2:…`, underline styles `4:3`) — a parser→Term param
  interface change; its own slice.
- **esctest hookup**, **OSC side-effects** (clipboard/hyperlink/palette), **DCS** —
  separate slices.
- Any change to rt's runtime input handling (mouse forwarding, return-key
  encoding) — the accessors vt-term exposes are unchanged.

## Risk notes

- **IRM is the highest-risk change** (hot print path + `print_str` fast path). It is
  gated behind `insert_mode`; when off, the print path is byte-for-byte unchanged.
- **DECSET 12 (blink)** is the fiddliest reconciliation (blink is set by both
  DECSET 12 and DECSCUSR parity). If it entangles, it is the one item to split into
  a follow-up — the rest of the slice does not depend on it.
