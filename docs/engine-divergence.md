# Engine divergence ledger

Where the in-house `vt-term` does NOT yet match the vendored `alacritty_terminal`
oracle. The Phase-3 process (see `docs/own-engine-plan.md`) is to drive this list to
empty (or to *intentional*, documented differences) under the `vt-conformance` harness.
Each entry: what diverges, the measured impact, and the plan.

Status snapshot (2026-07-21, foundation landed):
- Spec cases (`spec.rs`, 32 cases): **PASS** against vt-term.
- Curated differential (`vtterm_diff.rs`, 16 scripts): **PASS**.
- Random-fuzz grid divergence (scrollback ignored): **~3.5%** (104/3000 scripts).
- Random-fuzz including scrollback: ~52% (dominated by the missing history counter).

## Open divergences

### 1. Scrollback / history (deferred feature) — HIGH volume, LOW risk
vt-term has no scrollback: content scrolled off the top is dropped, so `history` stays
0 and `display_offset` stays 0 while the oracle accumulates them. This is the single
biggest source of raw fuzz divergence (~half of all scripts), but it is a *missing
feature*, not a grid bug — with scrollback counters neutralised, grid divergence is
~3.5%. **Plan:** implement a scrollback ring buffer (Phase-3 milestone), which also
unlocks `snapshot_lines`/scroll for the eventual rt wiring.

### 2. Last-column autowrap edge — ~most of the 3.5% grid divergence
On some scripts a single cell at the last column differs (vt-term keeps a glyph the
oracle wrapped/cleared). Everything else — including the cursor — matches. It is a
pending-wrap × scroll/newline interaction. **Plan:** align the wrapline semantics with
alacritty's `wrapline`/`input_needs_wrap` handling precisely.

### 3. A few cursor-position edge cases
A handful of scripts end with the cursor one row/column off (e.g. after a wrap-then-
scroll). Same family as #2. **Plan:** fold into the wrapline fix.

## Known not-yet-implemented (will diverge when exercised)

- **Reflow on resize** — vt-term truncates/extends; the oracle rewraps. THE hard part,
  isolated to the end by design.
- **Wide characters** (CJK/emoji): treated as width 1; the oracle places a spacer cell.
- **Colon sub-parameter SGR** beyond the extended-colour case.
- **OSC / DCS semantics** (title, clipboard, hyperlinks): parsed but not applied.
- **Charsets** (G0–G3 designations): ignored (ASCII assumed).
- **Origin mode** edge interactions, DECSCUSR cursor shape, LNM newline mode.

## Reconciliations already done

- **Neutral colour model** unified to `Default`/`Indexed`/`Rgb`: alacritty named
  colours 0–15 → `Indexed`, Foreground/Background → the `Named(256)` default sentinel,
  matching vt-term's `Color::Default`.
- **ED(Above)** matched to alacritty's `cursor.line > 1` quirk.
- **Tab** matched to alacritty's write-`\t`-glyph-into-the-blank-start-cell behaviour.
