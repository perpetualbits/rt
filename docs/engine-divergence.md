# Engine divergence ledger

Where the in-house `vt-term` does NOT yet match the vendored `alacritty_terminal`
oracle. The Phase-3 process (see `docs/own-engine-plan.md`) is to drive this list to
empty (or to *intentional*, documented differences) under the `vt-conformance` harness.
Each entry: what diverges, the measured impact, and the plan.

Status snapshot (2026-07-26):
- Spec cases (`spec.rs`, 32 cases): **PASS** against vt-term.
- Curated differential (`vtterm_diff.rs`, 16 scripts): **PASS**.
- Random-fuzz FULL differential (`vtterm_fuzz.rs`, 8000 scripts) — grid, cursor, modes,
  scrollback history, ANSI SM/RM (IRM/LNM), AND DECSCUSR (widened 2026-07-26): **0
  divergences**. Locked in as a test, green on x86_64 and riscv64.
- Random-**resize** differential (`vtterm_reflow.rs`, 3000 scripts) — reflow on grow/shrink
  of both dims incl. wide glyphs and scrollback: **0 divergences** (verified 0/20000 in a
  wider sweep). Ceiling locked at 0.
- Random query/report differential (`vtterm_report.rs`, 4000 scripts, added 2026-07-25,
  widened 2026-07-26 to cover IRM/LNM/12/mouse-trio/1005/1042) — DSR/CPR, DA1/DA2, and
  DECRQM reply bytes interleaved with mode/cursor mutators, compared byte-for-byte against
  the oracle's reply stream (not just observable `Term` state): **0 divergences**, green
  on x86_64 AND riscv64. See "Query / report" and "Observable-state edges" below.

**vt-term now matches the vendored oracle exactly on every fuzzed input, resize,
query/report, and observable-state-edge (IRM/LNM/DECSCUSR/mouse-mode) input.** The open
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

Result: **reflow now matches the oracle exactly — 0/3000 (verified 0/20000 in a wider
sweep), non-resize fuzz still 0/10000** (down from ≈156/3000, ~242 with the logical-line
reimplementation, ~1050 with truncate/extend). The reflow ceiling in `tests/vtterm_reflow.rs`
is **0** — a strict regression guard.

- **Wide-glyph overwrite cleanup — closed (156/3000 → 24 → 0, 2026-07-22).** The residual
  wide-glyph shifts were never in the reflow code (the earlier "leading-spacer inference"
  diagnosis was wrong — the inference is provably correct). They were all in the
  **cell-overwrite path**, `clear_wide_left`, which is vt-term's port of alacritty's
  `write_at_cursor` cleanup. Three root causes, each found by delta-debugging real fuzz
  seeds and instrumenting BOTH engines' `grow_columns` + `write_at_cursor` side by side:
  1. **Cleanup ran at the wrong position.** vt-term called `clear_wide_left` ONCE up front
     in `put_char`, at the pre-wrap cursor. Alacritty runs `write_at_cursor` at EACH actual
     write. When a wide glyph autowraps to column 0 of the next row and overwrites a wide
     glyph there, alacritty's *"remove leading spacers"* step fires at the *post-wrap*
     position and clears the leading spacer on the previous row; vt-term never reached that
     position. **Fix:** fold the cleanup into `write_cell`/`write_spacer` so it runs
     per-write, exactly like `write_at_cursor`; drop the up-front call.
  2. **The batched-print fast path skipped mid-segment cleanup.** `print_str`'s bulk loop
     wrote a run of narrow cells with only one `clear_wide_left` at the segment start, so a
     narrow char landing on a wrapped wide glyph mid-segment (e.g. `C` over `글` at col 1)
     missed the leading-spacer clear. **Fix:** the loop now defers any cell that still
     carries wide-glyph structure (a spacer, or a wide glyph) to `put_char`.
  3. **The leading spacer can live in scrollback.** When the wrapped wide glyph is at the
     *top* visible row, its leading spacer is on the previous physical row — which is the
     newest *history* line (alacritty indexes history as negative grid lines, so its
     `grid[line-1]` reaches into scrollback; vt-term separates `grid` from `history`).
     `clear_wide_left` bailed at `row == 0`. **Fix:** when on the top row, target
     `history.back()`.
  A fourth issue was a *regression* these fixes introduced and then closed: running the
  cleanup on the trailing-spacer write blanked our own just-written glyph whenever a stale
  spacer sat under it (alacritty's identical `clear_wide` is safe only because its grid is
  never stale). **Fix:** the trailing-spacer write skips the "blank the glyph to the left"
  case (`write_spacer(leading=false)`); the leading-spacer write keeps it (it legitimately
  cuts a glyph off at the wrap). The 2 cursor and 2 history residuals resolved with the same
  fixes (they were downstream of the mislaid spacer). The sibling trailing `WIDE_CHAR_SPACER`
  clear and CHT/CBT (`ESC[I`/`ESC[Z`) added earlier remain.

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

## Query / report — implemented and verified (2026-07-25)

DSR/CPR (`\x1b[5n` / `\x1b[6n`), DA1/DA2 (`\x1b[c` / `\x1b[>c`), and DECRQM
(`\x1b[Ps$p` / `\x1b[?Ps$p`) are implemented in vt-term (`Term::device_status` /
`device_attributes` / `report_mode`), surfaced through a drainable `output` buffer
(`take_output`, parallel to `take_title`), and wired all the way to the real PTY from the
`vtpane` reader loop — mirroring how the vendored engine answers `Event::PtyWrite`. See
`docs/vt-term-design.md`'s "Query / report" section for the reply formats and the
tracked-mode table.

A new differential strand, `tests/vtterm_report.rs` in `vt-conformance`, interleaves the
query set with mode- and cursor-mutating input and asserts the two engines' reply BYTE
STREAMS match exactly (via `reports_match`) — the first strand that compares wire replies
rather than the neutral `ScreenState`. **0/4000, on both x86_64 and riscv64** via
`ci/verify.sh`; locked in as a regression ceiling like the other strands.

**Intentional divergence: the DA2 version field.** `\x1b[>0;{version};1c` embeds the
emulator's own version; apps use it only for feature sniffing, so vt-term legitimately
reports its OWN crate version rather than the oracle's alacritty version. `reports_match`
masks this one numeric field (`mask_da2` in `vt-conformance/src/lib.rs`) before comparing —
every other byte of every reply must match exactly. This is a deliberate, documented
difference, not a bug, and is the only masked field in the comparator.

**Two reconciliations the differential forced into vt-term** (state that the earlier
fuzz/reflow strands never observed, because nothing before read it back over the wire):
- **DECSET 1007 (alternate-scroll mode) now defaults ON.** vt-term previously defaulted it
  off; alacritty's `TermMode::default()` has it on. In xterm/alacritty this mode makes the
  alt-screen mouse wheel emit arrow keys, but rt does not yet act on `alt_scroll` for input —
  so this flip is currently observable only as a corrected DECRQM `?1007$p` reply (now
  matching the oracle). Fixed by flipping vt-term's default to match.
- **DECOM (DECSET/DECRST 6, origin mode) now homes the cursor (`goto(0, 0)`) on SET
  only, not on RESET.** Matches alacritty's `Origin` handler, which calls `goto(0,0)` only
  when the mode is being turned ON — even if it was already on — and leaves the cursor
  alone when the mode is turned off. The `goto` itself is origin-aware, consistent with the
  rest of cursor motion.

**Previously excluded from the differential — closed by the observable-state-edges
slice (2026-07-26).** ANSI IRM (mode 4)/LNM (mode 20), private BlinkingCursor (12),
Utf8Mouse (1005), UrgencyHints (1042), and the mouse-report trio (1000/1002/1003) are
all now implemented, tracked, and covered by both `vtterm_fuzz.rs` (SM/RM + DECSCUSR
generation) and `vtterm_report.rs` (DECRQM queries/mutators for all of the above). No
mode remains excluded from either pool. See "Observable-state edges" below.

## Observable-state edges — implemented and verified (2026-07-26)

ANSI SM/RM (IRM mode 4, LNM mode 20), DECSCUSR cursor shape + blink (incl. private mode
12), and the mouse-mode reconciliation (tracking trio 1000/1002/1003, encoding pair
1005/1006, urgency hints 1042) are implemented in vt-term and differentially verified.
Two of `vt-conformance`'s generators were widened to reach them: `gen_script` (the grid
fuzzer, `vtterm_fuzz.rs`) now emits ANSI SM/RM and DECSCUSR sequences, and
`vtterm_report.rs`'s `QUERIES`/`MUTATORS` pools now cover all of the modes above. **Both
strands hold at 0 divergences (`vtterm_fuzz`: 0/8000+; `vtterm_report`: 0/4000) on
x86_64 AND riscv64.**

Widening the generators surfaced real vt-term/oracle divergences — each was root-caused
against the vendored `alacritty_terminal` source and fixed in vt-term, never masked in
the harness. **Two of these corrected the original design premises for this slice** (the
initial plan/spec assumed the opposite of what alacritty actually does):

- **LNM (mode 20) is a print-stream NO-OP — corrected from the original design.** The
  original design had LNM make LF carriage-return. The oracle's `vte::ansi` parser calls
  `Handler::linefeed()` directly for C0 LF/VT/FF and for ESC D (IND) — never the
  `Handler::newline()` default method that consults `TermMode::LINE_FEED_NEW_LINE` — so
  LNM has zero effect on any linefeed's cursor behaviour in alacritty; it is tracked only
  so DECRQM (`CSI 20 $p`) reports it accurately. Confirmed empirically: `\x1b[20hAB\n`
  against the vendored oracle leaves the cursor at column 2, not 0. vt-term's
  `line_feed()` had the CR-on-LF branch removed to match.
- **Mouse tracking (1000/1002/1003) and encodings (1005/1006) are mutually exclusive ON
  SET, not independent bits — corrected from the original design.** The original design
  modeled all five as fully independent bits. The oracle's `set_private_mode`
  (`alacritty_terminal-0.26.0/src/term/mod.rs:2063-2141`) clears the other two tracking
  bits when one of 1000/1002/1003 is set, and clears the sibling encoding when 1005 or
  1006 is set — its own comments read "Mouse protocols are mutually exclusive" / "Mouse
  encodings are mutually exclusive" (real xterm semantics). RESET only ever clears the
  mode's own bit. vt-term still uses per-bit fields (so DECRQM reports each mode
  individually), but `set_mode`'s SET arms for these five modes now enforce the same
  exclusion.
- **UrgencyHints (mode 1042) defaults ON**, matching alacritty's `TermMode::default()`
  (vt-term previously defaulted it off, the same class of miss as the DECSET 1007 default
  fixed earlier in the query/report work).
- **IRM (mode 4)** is insert-on-print: printing shifts the row right by the glyph's width
  before writing, guarded by alacritty's line-fit check (`col + width < cols`,
  `term/mod.rs:1204`) so a glyph that can't fit isn't shifted (it falls through to the
  normal wrap path instead).
- **DECSCUSR** shape (0/1/2 = block/underline/bar) is now wired into
  `vt-conformance`'s `observe()` for differential comparison, and blink (odd `Ps`, or
  DECSET/DECRST 12) is tracked and reported by DECRQM 12. The `Ps 0` (DECSCUSR default)
  blink state is provisional-steady — the fuzz's DECSCUSR generator never emits a bare
  `\x1b[0 q`, so this default was never pinned by a divergence.

**Two pre-existing bugs, unrelated to IRM/LNM/DECSCUSR/mouse, were also reconciled**
once the widened `gen_script` reached code paths the earlier fuzz never touched:
- **DL (`CSI Ps M`, delete-line) now pushes evicted rows into scrollback** when the
  cursor is at absolute row 0 on the primary screen (not the alt screen) — matching
  alacritty's `delete_lines`/`scroll_up_relative`, whose history push is keyed on the
  cursor's row being the absolute top, not on the scroll-region's top margin. vt-term's
  `delete_lines` previously never touched history at all.
- **ED/EL out-of-range parameters are now a no-op**, not silently aliased to mode 0. ED
  is valid for `Ps` `0..=3`, EL for `0..=2`; anything else is validated at the CSI
  dispatch site before calling `erase_in_display`/`erase_in_line`, matching `vte`'s own
  `ansi.rs` (which never reaches the handler for an invalid `Ps`).

See `docs/vt-term-design.md` for the design writeup of each of these.

## Kitty keyboard protocol — implemented, deliberately narrower (2026-09-05)

vt-term implements the protocol's negotiation (`CSI ? u` query, `CSI > flags u` push,
`CSI < n u` pop, `CSI = flags ; mode u` set) with per-screen mode stacks, matching the
oracle's `keyboard_mode_stack` / `inactive_keyboard_mode_stack`. Four **intentional**
divergences, all of them narrowing:

- **Only flag 1 (`DISAMBIGUATE_ESC_CODES`) is stored.** Every pushed or set flag word is
  masked to `KITTY_KBD_SUPPORTED` (`0b1`) before it is stored, so `CSI > 31 u` followed
  by `CSI ? u` answers `CSI ? 1 u`. The oracle stores all five faithfully because it
  implements all five. rt's key encoder (`rt::input::encode_key_kitty`) honours flag 1
  alone, and reporting a flag it would not act on is the worse failure: an application
  told it has flag 8 ("report all keys as escape codes") waits for sequences that never
  arrive, whereas one told it has only flag 1 downgrades correctly. This is the one
  divergence an application can observe directly, and it is the point of the design.
  The vendored backend (`RT_ENGINE=alacritty`) reaches the same answer from the other
  end: its `Term` keeps storing all five faithfully, and rt masks the `CSI ? u` reply on
  its way out of the engine (`mask_kitty_keyboard_reply`). So the two engines agree on
  the observable contract — one masks on the way in, the other on the way out — while
  their internal state legitimately differs.
- **`CSI ? u` reports the ACTIVE flags, the oracle reports the top of the stack.** They
  differ only after a bare `CSI = flags u`, which changes the active flags without
  touching the stack: the oracle then answers with the stale stack top. The protocol
  says the reply is the flags in effect, so vt-term answers with those.
- **The mode stack is bounded at 4096 and drops its oldest entry on overflow.** Same
  depth as the oracle's `KEYBOARD_MODE_STACK_MAX_DEPTH`, but the oracle's overflow branch
  removes an entry from `title_stack` (an upstream slip) and lets the keyboard stack grow
  without limit. vt-term drops from the keyboard stack, which is what the bound is for.
- **The vendored engine as rt drives it now has `Config::kitty_keyboard = true`.** It
  defaults to `false`, which silently discards every keyboard-protocol sequence; with it
  off no application could ever negotiate, and the two engines would have disagreed about
  whether the protocol exists at all.

Not covered by the differential harness: `vt-conformance`'s `ScreenState` captures the
grid, cursor, `alt_screen`, `app_cursor`, `display_offset` and `history` — it has no
keyboard-mode field, and the report differential's reply stream does not generate `CSI u`
sequences. So none of the above is fuzz-visible; the unit tests in `vt-term`
(`kitty_keyboard_*`) and `rt` (`crates/rt/tests/input.rs`) are the whole coverage. Adding
a `kitty_kbd` field to `ScreenState` would immediately fail the differential on the
masking divergence above, which is why it was left out rather than added and masked.

## Known not-yet-implemented (will diverge when exercised)

- **Colon sub-parameter SGR** beyond the extended-colour case.
- **OSC / DCS semantics** (title, clipboard, hyperlinks): parsed but not applied.

## Reconciliations already done

- **Neutral colour model** unified to `Default`/`Indexed`/`Rgb`: alacritty named
  colours 0–15 → `Indexed`, Foreground/Background → the `Named(256)` default sentinel,
  matching vt-term's `Color::Default`.
- **ED(Above)** matched to alacritty's `cursor.line > 1` quirk.
- **Tab** matched to alacritty's write-`\t`-glyph-into-the-blank-start-cell behaviour.
