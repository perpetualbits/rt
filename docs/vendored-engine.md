# Vendored terminal engine (throughput fork)

`vendor/vte` and `vendor/alacritty_terminal` are **vendored forks** carrying a single
optimization: batching runs of printable ASCII into one grid write
(`print_str` / `input_run`) instead of one call per character. It is worth **~1.5× on
ASCII-dominated engine throughput** (source, logs, `ls`, `cat`) and a clean no-op on
escape-heavy, wide-char, and short-run output.

## Why these are vendored, not crates.io deps

The optimization is not yet released upstream. Rather than depend on two forks existing
on a developer's disk (the previous state: absolute path-deps to `~/git/vte` and
`~/git/alacritty`), the crates are vendored in-tree so **a clean checkout builds the fast
path with no external setup**.

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

The `alacritty_terminal` Cargo.toml had `edition`/`rust-version` inlined from the
alacritty workspace root (they were `.workspace = true` in the monorepo).

## De-vendor plan

The upstreaming path and ready-to-apply patches live in
[`upstreaming-forks.md`](upstreaming-forks.md) and [`upstreaming/`](upstreaming/).
Once vte 0.16 (with the batch hook) and an `alacritty_terminal` release (with
`input_run`) are published to crates.io:

1. Delete `vendor/`.
2. `crates/rt-engine/Cargo.toml`: `alacritty_terminal = "<release>"`.
3. Root `Cargo.toml`: remove the `[patch.crates-io] vte` block.
4. `cargo update`, rebuild `--locked` on a clean clone, re-run `bench/term-render-bench.sh`.

Until then, pulling upstream `alacritty_terminal` fixes means re-applying the `input_run`
patch onto a fresh crate copy.
