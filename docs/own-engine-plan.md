# Plan: an in-house VT parser and Term, verified against the vendored engine

Goal: replace the vendored `alacritty_terminal` + `vte` with our own `vt-parser`
and `vt-term`, at a conformance level we can *demonstrate*, without ever putting
the working rt at risk.

## Hard guarantees (non-negotiable)

1. **rt keeps working the whole time.** The vendored, patched `alacritty_terminal`
   + `vte` stay the DEFAULT engine and a permanent fallback. No phase below changes
   rt's runtime behaviour until we deliberately flip a switch — and the switch is
   reversible.
2. **The vendored engine is also the ORACLE.** We do not test our engine against a
   spec we wrote; we test it against the battle-tested implementation we already
   ship, in-process, across millions of inputs.
3. **Nothing is big-bang.** Every phase delivers something independently useful and
   independently tested. If the program stops halfway, what shipped still has value.

## Guiding principles

- **Clean-room.** Study alacritty and foot for *design* (grid, damage, reflow);
  let *behaviour diffs*, never copied source, tie us to them. This keeps provenance
  clean (relevant given the upstream AI-patch friction) and the implementation ours.
- **Preserve the wins we already have.** The parser hot path is already solved in
  our fork — `memchr` scan-to-control in `advance_ground` + batched
  `print_str`/`input_run`. Our parser MUST keep both; they are the speed.
- **Verify the tractable piece in isolation.** The parser is a finite state machine
  emitting an action stream. Test the action stream against `vte` before any Term
  semantics enter the picture.
- **Reflow is the wall — isolate it.** It is the one genuinely hard part. Ship an
  interim "no rewrap on resize" and carry reflow tests on a documented divergence
  ledger until they pass. Never let reflow block the rest.

## Target workspace layout

```
crates/rt-engine        # THE SEAM: engine-agnostic trait + types; selects an impl
  engine/vendored.rs     #   impl over vendored alacritty_terminal (default + fallback)
  engine/own.rs          #   impl over vt-term (feature/env gated)
crates/vt-parser        # our clean-room VTE state machine (own vte)
crates/vt-term          # our Term: grid, scrollback, semantics, damage, reflow
crates/vt-conformance   # DEV-ONLY: oracle interface, differential fuzzer, esctest
                        #   runner, replay corpora, property tests, divergence ledger
vendor/vte              # kept: parser oracle + current default
vendor/alacritty_terminal # kept: Term oracle + current default
xtask (or a test bin)   # `verify` entrypoint that runs the whole battery
```

Both rt AND rt-mux consume `rt-engine`, so the seam serves both for free.

## Phase 0 — Establish the seam (DONE 2026-07-21; see docs/engine-seam.md)

**Finding: the decoupling was already substantially achieved by prior design.** rt and
rt-mux name zero alacritty types (only doc comments match); `rt-engine`'s public API is
already engine-agnostic (`Snapshot`, `SnapCell`, `CursorShape`, `Damage`, …); and
rt-session already has a `Backend` trait with a mock, so the seam skeleton exists. rt is
coupled to the concrete engine in ONE place: `AppSession = Session<TermPane, …>` plus
the `spawn_env` call sites.

What shipped this phase (all safe; rt unchanged at runtime):
- **Build-time engine selection:** cargo features on `rt-engine` —
  `default = ["vendored"]`, `vendored = ["dep:alacritty_terminal"]` (the dep is now
  optional and gated, verified via `cargo tree`: `--no-default-features` drops
  alacritty), `own = []` marker.
- **Contract of record:** `docs/engine-seam.md` enumerates the exact engine interface
  (agnostic types + the ~26 pane methods) the in-house engine must satisfy and the
  oracle harness will drive.
- **Design-doc homes seeded:** `docs/vt-parser-design.md`, `docs/vt-term-design.md`.
- **Boundary guard** documented (rt/rt-mux stay alacritty-type-free).

**Deferred (deliberately):** the formal `Engine` trait extraction (beyond `Backend`)
and making `AppSession` generic over it wait until the first `own` skeleton exists — so
the trait is validated against a real second implementation rather than guessed, and so
working rt's generics are not churned for zero present benefit. Likewise the runtime
`RT_ENGINE` switch lands in Phase 4 when there are two impls to choose between.

## Phase 1 — Conformance & oracle harness (STARTED 2026-07-21; core landed)

Shipped in `crates/vt-conformance` (dev-only; not built into the rt binary):
- **Neutral `ScreenState`** — a materialised, comparable snapshot (grid of neutral
  cells + cursor + alt/app-cursor modes + offset/history), naming no engine's types,
  with a human-readable `diff`.
- **`VtEngine` trait** — `spawn` / `feed` / `resize` / `observe`; the interface the
  oracle and (later) `vt-term` both implement.
- **Vendored oracle** (`vendored::Vendored`) — the alacritty `Term` + ANSI `Processor`
  driven **synchronously** (no PTY/shell), configured to match rt-engine.
- **Differential comparator + seeded generator** — `feed_whole`/`feed_chunks`,
  `gen_script` (structured escape-sequence fuzz), `split` (chunk framing), a
  dependency-free reproducible xorshift RNG.
- **Green battery** (`cargo test -p vt-conformance`): determinism; **chunk-invariance
  across 2000 seeds** (a real property — parser must resume across read boundaries);
  hand-written spec cases (ED/CUP/SGR/autowrap/EL/alt-screen/DECCKM); fuzz property
  invariants (cursor in bounds, `offset ≤ history`, dims); resize round-trip.

Still to do in Phase 1 (scaffolded, not yet filled):
- **esctest/vttest runner** — the codified xterm spec (higher-leverage than any single
  oracle). `corpus/` exists with a README; the runner + fixtures are next.
- **Real-app replay corpora** — capture vim/tmux/emacs/htop/git streams into
  `corpus/*.bytes`, replay + diff.
- **(Deferred) foot** as an out-of-process tiebreak oracle — see "Deferred, do not
  forget".

### Original design notes for Phase 1

The `vt-conformance` crate, with the "system under test" slot filled by *alacritty*
so it is green from day one and ready to accept our engine later:

- **Oracle interface:** "drive these bytes at rows×cols, read back grid + cursor +
  scrollback + damage." Implemented first for vendored alacritty.
- **Differential fuzzer:** random bytes + a structured escape-sequence generator →
  two engines → diff observable state. Bounded iterations per CI run; a long nightly
  soak.
- **Codified spec:** an esctest/vttest runner (these encode xterm — the ground truth
  alacritty AND foot both chase; higher-leverage than any single oracle).
- **Property tests** (proptest): cursor always in bounds; scrollback ≤ limit; reflow
  preserves total character content; resize-then-back is identity for unwrapped text.
- **Replay corpora:** captured raw byte streams from vim, tmux, emacs, htop, git,
  cargo; replayed and diffed. Stored as fixtures.
- **(Optional) foot as an out-of-process tiebreaker:** drive real foot over a PTY,
  scrape state, diff only ambiguous sequences. When alacritty and foot *agree* and we
  differ, we are wrong; when they *disagree*, we have found an under-specified corner
  to decide deliberately (log it).
- **Exit criteria:** the full battery runs green with alacritty in the SUT slot; a
  one-line change swaps in a candidate engine.

## Phase 2 — Own VTE parser (`vt-parser`)

The bounded, tractable piece; done first for a fast, low-risk win.

- Clean-room Williams/DEC state machine (ground/esc/csi/osc/dcs) + UTF-8, keeping the
  `memchr` fast path and batched printable runs.
- **Verify the ACTION STREAM, not pixels:** feed bytes to `vt-parser` and to `vte`;
  assert identical sequences of dispatched actions. Finite-state ⇒ transitions are
  near-exhaustively testable.
- Ship as a drop-in for the parse layer while STILL using alacritty's Term, so it can
  ride in real rt (behind the switch) with zero Term risk.
- **Exit criteria:** action-stream parity with `vte` across fuzzer + corpora; rt on
  `vt-parser` + alacritty-Term is indistinguishable in real use.

## Phase 3 — Own Term (`vt-term`)

The open-ended piece, grown strictly under the harness:

1. Grid + cursor + printing + SGR → match the oracle on simple cases.
2. Modes (DECSET/DECRST), scroll regions, tab stops, charsets, alt screen, OSC.
3. Scrollback + damage tracking (rt's renderer needs damage).
4. Reflow LAST. Interim: clear/redraw on resize (no rewrap); reflow tests sit on the
   divergence ledger until implemented.

- Every step gated by differential fuzz + esctest against the oracle. The
  **divergence ledger** (`docs/engine-divergence.md`) is the honest, shrinking record
  of "where we don't yet match, and why."
- **Exit criteria:** the battery passes except the documented ledger; ledger is empty
  or only *intentional* divergences remain.

## Phase 4 — Wire into rt behind a switch; test rt itself

- `rt-engine` exposes both impls behind its trait, selected by a build feature and/or
  `RT_ENGINE={vendored|own}` (default **vendored**).
- Test rt AT THE APP LEVEL on the own engine: real vim/tmux/emacs/htop sessions, rt's
  own test suite, visual/interaction passes on all three machines.
- Flip the default only when the ledger is acceptable and real-app use is clean. Keep
  `RT_ENGINE=vendored` as a permanent escape hatch.

## Phase 5 — Automation

- `cargo xtask verify` (and CI): unit + property + bounded differential-fuzz +
  esctest + corpus replay. **Fails on any NEW divergence** (ledger is the allowlist).
- Nightly long-soak fuzz with a seed corpus that grows from any crash/divergence found
  (coverage-guided if practical).
- Corpus + ledger tracked in-repo so the conformance state is always visible.

## Decisions (confirmed 2026-07-20)

- **Engine selection: BOTH** a build feature AND a runtime `RT_ENGINE={vendored|own}`
  env var. Default **vendored**.
- **Two crates: `vt-parser` and `vt-term`.** Keeps the tractable parser tests cleanly
  separate from the hard Term tests.
- **foot integration is DEFERRED** — it is a tiebreaker + design reference, not on the
  critical path. TRACKED so we don't forget: revisit in Phase 1 (optional out-of-
  process oracle) and Phase 3 (reflow/grid design study). See "Deferred, do not
  forget" below.

## Documentation standard (a first-class deliverable)

The aim is bluntly stated: **the best-documented VT parser and Term in existence.**
Not aspirational fluff — a concrete bar every phase is held to.

- **Every function** gets a thorough doc comment: what it does, *why* it exists, the
  invariants it assumes and preserves, and any spec/reference it implements (cite the
  ECMA-48 / DEC / xterm section or the vttest/esctest case).
- **Inline comments where they aid understanding** and only there — explain the
  non-obvious (a state-machine transition's rationale, a reflow edge case, a perf
  trick and the measurement that justified it). No narration of the obvious; comments
  earn their place or they are noise.
- **Two companion documents** that actually *teach* the designs, not just list APIs:
  - `docs/vt-parser-design.md` — the state machine (states, transitions, the ground
    fast path), UTF-8 handling, the batching contract, and *why* each choice.
  - `docs/vt-term-design.md` — the grid + scrollback data structures, damage tracking,
    every sequence family's semantics, and reflow. A reader should finish it able to
    reimplement the Term.
- Design docs and code stay in lockstep; a behaviour change that isn't reflected in
  the design doc is an incomplete change.

## Performance mandate (co-equal with correctness)

The parser and Term must be **REALLY fast** — we study every trick in alacritty and
foot and aim to *beat* them, in Rust. Concretely:

- **Learn, measure, then improve.** Read how alacritty and foot represent the grid,
  track damage, scroll, and reflow; benchmark ours against both (vtebench + our own
  harness) on identical inputs. A trick is adopted only with a measurement behind it,
  and each such trick is commented with that measurement.
- **Preserve the wins we already have**, and push past them: `memchr`/SIMD scan-to-
  control on the ground state, batched printable runs (`print_str`/`input_run`) that
  mutate the grid in bulk, cache-friendly cell layout, minimal per-cell work on the
  hot path, damage that never over-reports.
- **Correctness gates speed.** A faster path ships only when the differential harness
  proves it produces identical state to the simple path (exactly the pattern
  alacritty's own `assert_input_run_matches` uses). No speed trick escapes the oracle.
- Perf is tracked with the same rigour as conformance: a benchmark suite in CI, and
  regressions are failures.

## Deferred, do not forget

- **foot integration** (decision 3): out-of-process tiebreak oracle (Phase 1) and
  grid/damage/reflow design study (Phase 3). Deferred, not dropped.

## Risk register

- **Reflow** — the hard wall. Mitigation: isolate, ship interim no-rewrap, ledger.
- **Oracle inherits bugs** — matching alacritty adopts alacritty's quirks; esctest +
  foot tiebreak catch cases where alacritty itself is wrong.
- **Oracle goes silent where we want to differ** — any place we intend to be *better*
  than alacritty has no oracle; those cases fall back to real-app judgement and must
  be flagged as intentional divergences, not bugs.
- **Scope creep** — Phase 0 and Phase 2 each stand alone; ship and benefit even if the
  program pauses.

## The through-line

The vendored engine is default, oracle, and fallback at every step. rt is never at
risk; we always have a reference; and "done" is defined as *demonstrable non-divergence
from the implementations the whole ecosystem trusts* — not the unreachable "provably
correct".
