# Upstreaming the throughput forks

rt's ASCII-throughput win (~1.5× in the engine, ~1.4× end-to-end) currently rides
on **two uncommitted local forks**:

| Fork | Path | Upstream repo | Change |
|------|------|---------------|--------|
| `vte` | `~/git/vte` (off v0.15.0) | `alacritty/vte` | Batch printable runs into `print_str` / `input_run`, run-length-gated by `BATCH_MIN`. |
| `alacritty_terminal` | `~/git/alacritty/alacritty_terminal` (0.26.1-dev) | `alacritty/alacritty` | `Term::input_run`: write a whole ASCII run to the grid in one pass; per-char fallback for wide/zero-width/INSERT/non-ASCII. |

rt is wired to both via **uncommitted** edits it cannot ship as-is:
- `crates/rt-engine/Cargo.toml` → path-dep on `~/git/alacritty/alacritty_terminal`
- root `Cargo.toml` → `[patch.crates-io] vte = { path = "~/git/vte" }`

A clean checkout does not build the fast path. The goal below is to get rt back to
plain crates.io dependencies with the win intact.

## The key enabler: every addition is backward-compatible

All four new API surfaces have default impls that reproduce the old per-char behavior:

- `vte::Perform::print_str` (lib.rs:809) → loops `self.print(c)`
- `vte::ansi::Handler::input_run` (ansi.rs:515) → loops `self.input(c)`
- The parser only calls `print_str` when `run.len() >= BATCH_MIN` (lib.rs `flush_run`);
  control bytes still dispatch individually via `execute`, preserving call order.
- `preceding_char` (REP `\x1b[b`) is set to the run's last char, matching per-char `print`.

Correctness is proven by `input_run_matches_input` (alacritty_terminal
`src/term/mod.rs:2670`), a differential test asserting the batched grid is
byte-identical to the per-char grid.

**Consequence:** vte can be released as a normal additive minor bump (0.15 → 0.16),
and downstreams that never override the new methods are completely unaffected.

## Ordering

```
vte PR (alacritty/vte)                    ← must land + be published to crates.io first
        │
        ▼
alacritty_terminal PR (alacritty/alacritty)  ← bumps vte dep to 0.16, adds input_run override
        │
        ▼
rt cutover                                ← drop the [patch] + path-dep, bump versions
```

The alacritty_terminal override is *harmless* on old vte (its `input_run` simply never
gets called with a batch), so the two PRs can be reviewed in parallel — but the **benefit**
only appears once vte 0.16 is released and alacritty_terminal depends on it.

---

## Track A — Upstream properly (correct, but on the maintainers' timeline)

### A1. vte → `alacritty/vte`
- [ ] Commit the working-tree changes in `~/git/vte` (`src/lib.rs`, `src/ansi.rs`,
      `Cargo.toml`, `CHANGELOG.md`) on a topic branch off the latest `alacritty/vte` main.
- [ ] Keep `BATCH_MIN = 4` (the shipping value validated in the A/B).
- [ ] Ensure the CHANGELOG entry describes the additive `print_str` / `input_run` methods.
- [ ] Add/keep a parser-level test that `ground_dispatch` preserves exact `print`/`execute`
      ordering and the `preceding_char`/REP semantics (the risky invariant).
- [ ] `cargo test` + `cargo bench` (or point at rt's `throughput.rs` numbers) in the PR body;
      lead with "additive, default-impl, no behavior change unless overridden."
- [ ] Open PR. Propose the version bump to **0.16.0**.
- [ ] After merge, wait for the crates.io release (maintainer-controlled).

### A2. alacritty_terminal → `alacritty/alacritty`
- [ ] Rebase the `Term::input_run` change (`src/term/mod.rs:1080`) + the differential test
      (`:2670`) onto current alacritty main.
- [ ] Bump `alacritty_terminal/Cargo.toml` `vte = "0.16"` once A1 is published.
- [ ] Run the full `alacritty_terminal` test suite; include `throughput.rs` A/B numbers
      (batched vs `BATCH_MIN=usize::MAX`) in the PR body — this repo's example is the
      reference harness, so it is persuasive.
- [ ] Open PR referencing the merged vte PR.

### A3. rt cutover (after both land on crates.io)
- [ ] `crates/rt-engine/Cargo.toml`: replace the path-dep with
      `alacritty_terminal = "0.27"` (whatever release carries `input_run`).
- [ ] root `Cargo.toml`: **delete** the `[patch.crates-io] vte = { path = ... }` block.
- [ ] `cargo update -p vte -p alacritty_terminal`; commit the refreshed `Cargo.lock`.
- [ ] `cargo build --workspace --locked` on a clean clone (or CI) to prove no fork is needed.
- [ ] Re-run `bench/term-render-bench.sh` to confirm the win survived the version bump.
- [ ] Normal workflow: branch → commit → merge --no-ff → push → `cargo install`.

---

## Track B — Vendor now (self-contained, unblocks immediately)

Use this if you want a permanent, cloneable build **before** upstream review completes.
It removes the dependency on `~/git/vte` and `~/git/alacritty` existing on disk.

- [ ] Copy both forks into the rt tree, e.g. `vendor/vte/` and `vendor/alacritty_terminal/`
      (source + `Cargo.toml` + `LICENSE`; drop each crate's `.git`, `examples`, `tests` you
      don't need — but **keep** `input_run_matches_input`).
- [ ] Preserve the upstream licenses (vte: Apache-2.0/MIT; alacritty_terminal: Apache-2.0/MIT)
      and note the fork + the specific patch in each vendored `README`/`NOTICE`.
- [ ] root `Cargo.toml`: `[patch.crates-io] vte = { path = "vendor/vte" }`.
- [ ] `crates/rt-engine/Cargo.toml`: `alacritty_terminal = { path = "vendor/alacritty_terminal" }`.
- [ ] Update `.github/workflows/ci.yml` — no change needed if paths are in-repo (they build
      like any workspace member); confirm the `--locked` builds still pass.
- [ ] Add a one-paragraph `docs/vendored-engine.md` recording *why* (the throughput fork),
      *what* (the exact patch), and *the plan to de-vendor* once Track A lands.
- [ ] Commit `Cargo.lock`.

**Trade-off:** Track B works today and for everyone, but you now maintain a vendored
snapshot and must manually pull upstream alacritty_terminal fixes until you de-vendor.

---

## Recommendation

Do **B then A in parallel**: vendor now so rt is permanently buildable and the win is banked,
and open the vte + alacritty_terminal PRs on their own clock. When (if) they land on
crates.io, delete `vendor/` and switch to Track A3's plain version deps. If upstream declines,
you keep the vendored path indefinitely at low cost.

## Risks / watch-items
- **Maintainer appetite:** alacritty is conservative about `alacritty_terminal` API and perf
  claims. The additive-default framing + the byte-identical differential test are the
  strongest arguments; lead with them.
- **`BATCH_MIN` tuning:** 4 was validated here; upstream may want a benchmark sweep. The knob
  is a single const, easy to defend.
- **REP / `preceding_char`:** the one semantic subtlety. Already handled and commented — call
  it out explicitly in the PR so reviewers don't have to find it.
- **vte release cadence:** the alacritty_terminal PR is blocked on a *published* vte 0.16,
  not just a merged one. Vendoring (Track B) removes this blocker for rt specifically.
