# Colon-subparam SGR — design

**Date:** 2026-07-26
**Status:** approved (design), pending implementation plan
**Scope:** engine hardening — Thread 2 (expand verified coverage). Follows the
observable-state-edges slice (shipped v0.3.16). See `docs/own-engine-plan.md`
(Phase 3) and `docs/engine-divergence.md`.

## Problem

vt-term's `sgr()` (`crates/vt-term/src/lib.rs:1429`) consumes a **flattened**
`&[u16]` param list (`flat(params)`), and `flat()` keeps only the **first
subparam of each parameter** (`g.first()`, `lib.rs:1930-1934`) — so every colon
sub-parameter after the first is silently **dropped**. Colon-delimited SGR is
therefore degraded (not corrupted into other attributes — the subparams simply
vanish):

- **`38:2:r:g:b`** (colon truecolour) → `flat` keeps only `[38]`; with no
  following params, `sgr_color` finds nothing and applies **no colour** — colon
  truecolour is a silent no-op. Same for `38:5:n`, `48:2:…`, `48:5:n`. (The
  colorspace-id form `38:2:cs:r:g:b` is dropped identically.)
- **`4:3`** (curly underline) → `flat` keeps only `[4]`; the `3` style subparam
  is lost, so `4:3` degrades to a plain single underline. Every `4:n` collapses to
  bare `4` (or, for `4:0`, to bare `4` = underline-on — the opposite of intended).
- **`58:2:…` / `58:5:…` / `59`** (underline colour) → `flat` keeps only `[58]`/
  `[59]`, which `sgr()` does not handle → a no-op; the colour is dropped.

The **semicolon** forms (`38;2;r;g;b`, `38;5;n`, `48;…`) work today, because each
value is its own single-subparam parameter and survives flattening. The fix must
preserve them.

The parser is not at fault: `vt_parser::Params::iter()` already yields one
`&[u16]` per parameter (its colon subparams). The loss is entirely in vt-term's
`flat()`. The differential never caught this because `gen_script` emits only bare
single-code SGR (`\x1b[{0..8}m`) — no extended colour, no colon.

## Goal

Make `sgr()` consume the **structured** params so colon, semicolon, and mixed
forms all parse correctly, implement the three colon-SGR families to match the
vendored oracle, and extend the grid differential to cover them at 0 divergences
on both architectures. No new verification machinery beyond the neutral-model
additions below.

## Decisions (from brainstorming)

1. **Structured `sgr()`.** Change `sgr()` from `&[u16]` to consume the parser's
   `&Params` via `iter()`. Per parameter: a multi-subparam param is a colon group
   (`4:n`, `38:…`, `48:…`, `58:…`); a single-value param is a normal SGR code,
   and `38`/`48`/`58` consume following *parameters* for the semicolon form via
   lookahead. Mirrors how vte/alacritty parse SGR.
2. **Underline styles: full fidelity.** Single/double/curly/dotted/dashed, stored
   as distinct vt-term flag bits and verified via new neutral attr bits.
3. **Underline colour (58/59): parse-correct, pen-tracked, NOT per-cell.** The
   codes are parsed correctly (so they no longer corrupt other attributes) and the
   current underline colour is tracked in the pen, but it is **not** stored on
   cells nor exposed in the neutral model — an intentional, documented boundary.
   Rationale: rt's renderer draws no underline colour, and per-cell colour storage
   would either grow the deliberately-16-byte `Copy` `Cell` (+25% grid/scrollback
   memory, slower fills/scrolls on the riscv perf canary) or force a
   `Cell: Copy`-breaking boxed-extra refactor — cost with zero rt-visible payoff.
   True per-cell fidelity (a sparse per-line side-map) is a deferred follow-up.

## Changes in vt-term (`crates/vt-term/src/lib.rs`)

### Structured `sgr(&mut self, params: &Params)`
The `'m'` arm in `csi_dispatch` calls `self.sgr(params)` (the structured
`Params`) instead of `self.sgr(&p)` (flattened). New loop over `params.iter()`:

- **Empty** (`CSI m`): reset pen (as today).
- **Param with ≥2 subparams** (colon group `p = &[code, sub1, …]`):
  - `4` → underline style from `sub1` (see below).
  - `38` / `48` / `58` → extended colour / underline colour from the subparams,
    including the ISO colorspace-id form (`38:2:cs:r:g:b`, where a 6-subparam `2`
    carries a colorspace id that is ignored, matching alacritty).
- **Single-value param** `[code]`:
  - The existing attribute codes (0,1,2,3,7,8,9,22,23,24,27,28,29,30–37,39,40–47,
    49,90–97,100–107) unchanged.
  - `4` → single underline.
  - `21` → double underline.
  - `38`/`48`/`58` → consume following *params* (semicolon form) via a small
    lookahead helper that reads `2;r;g;b` / `5;n` from the subsequent single-value
    params, returning how many to skip.

A shared colour-decode helper handles both a subparam slice (colon) and a
following-params slice (semicolon), so `38:2:r:g:b` and `38;2;r;g;b` resolve
identically. The colorspace-id variant is recognised only in the colon form (it
never occurs in the semicolon form), matching the oracle.

### Underline styles
Add four flag bits at the free positions (vt-term's `flags: u16` uses bits 0–9;
10–15 are free): `DOUBLE_UNDERLINE`, `UNDERCURL`, `DOTTED_UNDERLINE`,
`DASHED_UNDERLINE` (bits 10–13), alongside the existing `UNDERLINE` (bit 2). Add
them to `ATTR_MASK` so they are carried by the pen and written to cells. A
`clear_all_underlines` helper removes all five (mirroring alacritty's
`ALL_UNDERLINES` mask). SGR mapping (matching alacritty's `Attr` handling,
`vendor/alacritty_terminal/src/term/mod.rs:2012-2032`):
- `4` or `4:1` → clear-all + `UNDERLINE`
- `4:2` or `21` → clear-all + `DOUBLE_UNDERLINE`
- `4:3` → clear-all + `UNDERCURL`
- `4:4` → clear-all + `DOTTED_UNDERLINE`
- `4:5` → clear-all + `DASHED_UNDERLINE`
- `4:0` or `24` → clear-all (no underline)

### Underline colour (58/59) — pen only
Add `underline_color: Color` to the **pen** (the `Cell`-typed pen template's
companion state — a Term field, NOT a `Cell` field, so `Cell` stays 16 bytes).
`58:2[:cs]:r:g:b` / `58:5:n` set it; `59` resets to `Color::Default`. It is not
written to cells and not observed. (It exists so the pen is correct and 58/59 are
consumed without leaking into the SGR stream; per-cell storage is deferred.)

## Neutral model (`crates/vt-conformance`)

- `attr` bits: add `DOUBLE_UNDERLINE`, `UNDERCURL`, `DOTTED_UNDERLINE`,
  `DASHED_UNDERLINE` (`lib.rs`).
- `vendored.rs` `neutral_attrs`: map alacritty's `Flags::DOUBLE_UNDERLINE`/
  `UNDERCURL`/`DOTTED_UNDERLINE`/`DASHED_UNDERLINE` to the new neutral bits (the
  existing `UNDERLINE` mapping stays).
- `vtterm.rs` `nattrs`: map vt-term's four new flags to the new neutral bits.
- Underline colour is deliberately NOT added to the neutral cell.

## Verification (reuse the grid differential)

Extend `gen_script` (`crates/vt-conformance/src/lib.rs:218`) to emit, interleaved
with existing tokens, an "extended SGR" case that randomly produces:
- extended colour, both forms and both channels: `38;2;r;g;b`, `48;2;r;g;b`,
  `38;5;n`, `38:2:r:g:b`, `38:2::r:g:b` (colorspace-id), `38:5:n`, `48:5:n`;
- underline styles: `4`, `4:0`…`4:5`, `21`, `24`;
- underline colour: `58:2:r:g:b`, `58:5:n`, `59` (parsed; no observable effect,
  but exercises that they don't corrupt other state).

Drive `vtterm_fuzz` and `vtterm_reflow` to **0** (extend the sweep; ceiling stays
0). Any divergence → fix vt-term to match the oracle (never narrow the generator
or loosen the comparator). Run on x86-64 (local + apollo) and riscv-64 (milkv) via
the hardened `ci/verify.sh`.

## Testing (TDD)

Pure unit tests, watched to fail first:
- **Extended colour:** `38;2;r;g;b` and `38:2:r:g:b` produce the same `Rgb`;
  `38:2::r:g:b` (colorspace-id) skips the id and reads the right triple; `38:5:n`
  and `38;5;n` → `Indexed(n)`; same for `48`.
- **Underline styles:** each of `4`/`4:1`…`4:5`/`21` sets exactly its flag and
  clears the others; `4:0` and `24` clear all. In particular `4:3` now sets
  `UNDERCURL` specifically (today it degrades to a plain single underline because
  the style subparam is dropped), and it touches no non-underline attr.
- **Underline colour:** `58:2:r:g:b`/`58:5:n` set the pen's `underline_color`;
  `59` resets it; the numeric subparams do NOT appear as spurious attrs.
- **Mixed/semicolon still work:** existing single-code SGR unchanged.

Then the differential to 0 on both arches.

## Documentation

- `docs/vt-term-design.md` — rewrite the SGR section: structured-param parsing,
  colon vs semicolon vs colorspace-id extended colour, the underline-style flags,
  and the pen-only underline colour with its documented boundary.
- `docs/engine-divergence.md` — record colon-SGR now handled; the underline-colour
  **intentional boundary** (parsed + pen-tracked, not per-cell stored/verified —
  rt renders none); note the extended `gen_script` coverage (still 0-ceiling, both
  arches).
- `docs/own-engine-plan.md` — Phase-3 SGR coverage progressed.
- `project-map.js` — the `vt-term` node reflects colon-SGR done; `project.updated`.

## What is reused vs new

**Reused:** `Params::iter()` (already colon-aware); the `Cell`/`Color` model
(unchanged — no growth); `sgr_color`'s decode logic (generalised); the grid
differential + `gen_script`; `ci/verify.sh`.

**New:** the structured `sgr(&Params)` loop + colour-decode helper handling both
subparam and following-param forms; four underline-style flags + `ATTR_MASK`
inclusion + `clear_all_underlines`; the pen `underline_color`; four neutral attr
bits + their mappings in both engines; the `gen_script` extended-SGR case.

## Out of scope (deferred)

- **Per-cell underline colour** (sparse per-line side-map) — deliberately deferred;
  the boundary is documented in the ledger.
- **OSC side-effects** (clipboard/hyperlink/palette), **DCS**, **esctest hookup** —
  separate slices.

## Risk notes

- **Signature change of `sgr()`** ripples only to its single `csi_dispatch` call
  site; `flat()` remains for the other CSI handlers that legitimately want a flat
  list. Confirm no other caller of `sgr` exists.
- The colour-decode helper must treat the colon and semicolon forms identically
  for `2`/`5` while recognising the colorspace-id only in the colon form — the one
  place a subtle divergence could hide; the fuzz emits both forms to pin it.
