# PR to alacritty/alacritty — `Term::input_run` fast grid write

**Branch suggestion:** `term-input-run` off `alacritty/alacritty` master
**Patch:** `02-alacritty_terminal-input_run.patch` (`git apply` in a clean alacritty checkout)
**Plus:** add `alacritty_terminal/examples/throughput.rs` (copy from
`rt/vendor/alacritty_terminal/examples/throughput.rs` — the A/B harness)
**Depends on:** vte 0.16 published to crates.io (PR-1). Bump
`alacritty_terminal/Cargo.toml` `vte = "0.16"` in this PR.

## Summary

Override `vte::ansi::Handler::input_run` on `Term` to write a whole run of printable
ASCII into the grid in a single pass, instead of the default per-char loop. The fast
path handles the common case (single-width printable ASCII, no INSERT mode); anything
else — wide/zero-width chars, non-ASCII, INSERT mode, wrap boundary conditions — falls
back to the existing per-char `input()`.

## Why it's safe (lead with this)

- **Opt-in override; the default is unchanged.** With vte < 0.16 (or without this
  override) `input_run` is simply never called with a batch — no behavior change.
- **Byte-identical to the old path — proven, not asserted.** The included differential
  test `input_run_matches_input` feeds representative payloads through both `input_run`
  and the per-char `input`, then asserts the resulting grids are identical cell-for-cell.
- **Conservative fast-path guard.** Any cell that isn't plain single-width printable
  ASCII, or any active INSERT mode / wrap edge case, drops to `input()` for that char.

## Perf

`cargo run --release -p alacritty_terminal --example throughput`, then A/B by flipping
vte's `BATCH_MIN` between `4` and `usize::MAX`:

| payload | batched | per-char baseline | win |
|---|---|---|---|
| ascii | 123.0 MB/s | 82.7 MB/s | **1.49×** |
| sgr_colored | 182.1 | 183.7 | 1.00× (escape breaks runs) |
| unicode_wide | 107.2 | 104.0 | 1.00× (fallback, no regression) |
| yn_scroll | 5.9 | 5.9 | 1.00× (1-char runs, below threshold) |

(48 MB payloads, median of 9, controlled run.) The win is confined to ASCII-dominated
output — source, logs, `ls`, `cat` — and is a clean no-op elsewhere.

## Notes for reviewers

- The one subtlety is `preceding_char`/REP, handled on the vte side (PR-1): the batched
  run leaves the same `preceding_char` a per-char print would.
- `examples/throughput.rs` also includes a `NullPerform` ceiling and a direct
  `input` vs `input_run` micro-A/B, so the grid-write cost is separable from parser cost.
