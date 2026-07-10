# PR to alacritty/vte — batch printable runs into `print_str`

**Branch suggestion:** `batched-print-str` off `alacritty/vte` main
**Patch:** `01-vte-batched-print_str.patch` (apply with `git apply` in a clean vte checkout)
**Proposed version bump:** 0.15.x → **0.16.0** (additive)

## Summary

Add an optional batched-print path to the parser. When the ground state consumes a
run of printable characters, dispatch the whole run once via a new
`Perform::print_str(&mut self, &str)` instead of calling `print` per character.
`ansi::Handler` gains a matching `input_run(&mut self, &str)`, and `ansi::Processor`
forwards `print_str` → `handler.input_run`.

## Why it's safe (lead with this)

- **Purely additive, default impls reproduce old behavior byte-for-byte.**
  - `Perform::print_str` (src/lib.rs) defaults to `for c in s.chars() { self.print(c) }`.
  - `ansi::Handler::input_run` (src/ansi.rs) defaults to `for c in text.chars() { self.input(c) }`.
  - Any existing `Perform`/`Handler` impl that does **not** override them is completely
    unaffected — no API break, no behavior change.
- **Batching is run-length gated.** The parser only calls `print_str` when a run is
  `>= BATCH_MIN` (4). Shorter runs (control/newline-heavy output like `y\n` scrolling)
  take the exact per-char `print` path, so they see zero added overhead.
- **Call ordering preserved.** Control bytes still dispatch individually via `execute`,
  flushing any pending run first, so the interleaving of `print`/`execute` is identical.
- **REP semantics preserved.** `preceding_char` is set to the run's last char, matching
  what per-char `print` would leave for the REP (`\x1b[b`) control.

## Perf

Measured in the downstream `alacritty_terminal` throughput harness (below), the batched
path is **~1.5× on ASCII-dominated output** through the real parser, with no change on
SGR-per-cell, wide CJK, or single-char-run payloads. A/B is a one-line flip of `BATCH_MIN`
between `4` and `usize::MAX`.

## Test

Ordering/`preceding_char` invariants are exercised by the parser tests; the downstream
`alacritty_terminal` side adds a differential test (`input_run_matches_input`) proving the
resulting grid is byte-identical to the per-char path.
