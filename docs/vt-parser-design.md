# vt-parser design

> Status: **stub** (Phase 2 not yet started). This document is a first-class
> deliverable — see the documentation mandate in `docs/own-engine-plan.md`. It must
> teach the parser well enough that a reader could reimplement it. Fill each section
> as the code lands; code and this doc stay in lockstep.

The in-house VT/ANSI parser: a clean-room state machine that turns a raw byte stream
into a sequence of dispatched *actions* (print, execute, CSI, ESC, OSC, DCS, hook/put/
unhook), which the Term consumes. Correctness is verified by diffing its **action
stream** against vendored `vte`; speed must meet or beat `vte` and foot.

## Sections to write (outline)

1. **Scope & non-goals** — what the parser does (byte → action stream) vs what the Term
   does (action → grid). The action vocabulary and the `Perform`-style callback API,
   shaped for rt-engine rather than inherited.
2. **The state machine** — ground / escape / escape-intermediate / CSI (entry, param,
   intermediate, ignore) / OSC / DCS (entry, param, intermediate, passthrough). A
   state diagram. Cite the DEC/ECMA-48 / Williams parser tables (vt100.net).
3. **The ground fast path** — `memchr`/SIMD scan to the next control byte so long runs
   of printable text skip the per-byte machine; batched `print_str`/`input_run` so the
   Term mutates the grid in bulk. *This is the speed* — document it with the benchmark
   that justifies it.
4. **UTF-8 handling** — incremental decoding across buffer boundaries; invalid-sequence
   recovery (U+FFFD policy) matched to the oracle.
5. **Parameter & intermediate collection** — limits, overflow behaviour, sub-parameters
   (`:`), colon-vs-semicolon SGR.
6. **OSC/DCS** — string collection, terminators (ST / BEL), length caps.
7. **Performance** — every trick, each with the measurement behind it; comparison
   against `vte` and foot on vtebench + our harness.
8. **Verification** — action-stream differential testing against `vte`; near-exhaustive
   transition coverage (finite state machine); fuzzing.
9. **Divergences** — any deliberate difference from `vte`/xterm, and why.
