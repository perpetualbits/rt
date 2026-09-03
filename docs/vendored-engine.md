# Vendored terminal engine (throughput fork)

`vendor/vte` and `vendor/alacritty_terminal` are **vendored forks** carrying a single
optimization: batching runs of printable ASCII into one grid write
(`print_str` / `input_run`) instead of one call per character. It is worth **~1.5× on
ASCII-dominated engine throughput** (source, logs, `ls`, `cat`) and a clean no-op on
escape-heavy, wide-char, and short-run output.

## Why these are vendored, not crates.io deps

The optimization is not in any upstream release, and none is coming (see "Permanent"
below). Rather than depend on two forks existing on a developer's disk (the previous
state: absolute path-deps to `~/git/vte` and `~/git/alacritty`), the crates are vendored
in-tree so **a clean checkout builds the fast path with no external setup**.

Wiring:
- `Cargo.toml` (root): `[patch.crates-io] vte = { path = "vendor/vte" }`
- `crates/rt-engine/Cargo.toml`: `alacritty_terminal = { path = "../../vendor/alacritty_terminal" }`

## What exactly was changed vs. upstream

- **vte** (off v0.15.0): additive `Perform::print_str` + `ansi::Handler::input_run`
  (both with per-char default impls), and a run-length-gated batch dispatch in the
  ground state (`BATCH_MIN = 4`). Non-breaking.
- **alacritty_terminal** (0.26.1-dev): `Term::input_run` overrides the default to write a
  printable-ASCII run in one grid pass, with per-char fallback for wide/zero-width/
  non-ASCII/INSERT. Correctness pinned by the `input_run_matches_input` differential test.

- **alacritty_terminal** (`event_loop.rs`, 2026-09-03): `pty_read` no longer discards
  bytes it has already read. It stages reads in `buf` and only parses once it can take
  the `Term` lock; if the lock was contended it did `continue`, looping back to read
  again, and a hard read error there hit `_ => return Err(err)` and returned with
  `unprocessed > 0` — throwing away bytes that were already out of the kernel and
  unrecoverable. On a PTY whose child has exited, `read` yields the final output and
  then `EIO` forever, so a write-then-exit child (`sh -c "printf hi"`) lost its output
  entirely. Now a hard error with bytes staged records the error, blocks for the lock
  rather than re-reading a dead fd, parses, and reports the error afterwards.
  Covered by `crates/rt-engine/tests/exit_race.rs`.

  This was first written up as "deterministic on riscv64, never reproduced on
  x86_64, where `snapshot()` is too fast to hold the lock long enough". That was
  wrong, and the correction matters because it retires an imagined architecture
  dependency. **The bug is architecture-independent** — with the fix reverted it
  loses 40/40 on x86_64. What actually differed was the engine, not the CPU:
  `TermPane::spawn` reads `RT_ENGINE`, the interactive shell on dop561 exports
  `RT_ENGINE=vtterm`, and a non-login `ssh milkv 'cargo test'` does not. So the
  x86_64 runs were exercising the in-house engine (which has no such bug — its
  reader takes a *blocking* lock and can only be delayed, never made to skip a
  parse) while the riscv64 runs exercised the vendored one. The test now pins
  each engine explicitly instead of inheriting the ambient variable.

The `alacritty_terminal` Cargo.toml had `edition`/`rust-version` inlined from the
alacritty workspace root (they were `.workspace = true` in the monorepo).

## Permanent: this fork is not going upstream

Upstreaming was attempted and declined. The batching was offered to `alacritty/vte`
as PR #154 and, after that was closed, issue #155 asked whether any form of it would be
in scope. Both were closed under the Alacritty project's contribution policy, which
prohibits LLM/AI-generated contributions — code, pull requests, issues, and comments:
<https://github.com/alacritty/alacritty/blob/master/CONTRIBUTING.md#llmai-contributions>

There is therefore no route to land this in `vte` / `alacritty_terminal`, and rt stays
on the vendored fork **permanently**.

Practical upkeep: to pull an upstream `alacritty_terminal` fix, re-apply the change onto
a fresh copy of the crate. The diff is small — `Term::input_run` plus its
`input_run_matches_input` differential test, and the additive `print_str` / `input_run`
hook in `vte` — and can be regenerated any time with `git diff` against the upstream tag
the vendored copy was cut from (`alacritty_terminal` 0.26.1-dev, `vte` v0.15.0).
