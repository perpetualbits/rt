# Query/report differential + runtime wiring — design

**Date:** 2026-07-25
**Status:** approved (design), pending implementation plan
**Scope:** engine hardening — Thread 2 (expand verified coverage), first slice.
Roadmap node `vt-term` "OSC/DCS & query-report edges" (`active`); the query/report
half. See `docs/own-engine-plan.md` (Phase 3/5) and `docs/engine-divergence.md`.

## Problem

The in-house engine's differential vs the vendored oracle is at **0 divergences**
over everything the fuzz reaches — but it reaches only *screen state* (grid,
cursor, modes, scrollback). The **query/report path is a total gap**:

- The neutral `ScreenState` the harness compares carries no engine→host output;
  the `VtEngine` trait (`spawn/feed/resize/observe`) has no reply channel.
- vt-term generates **no reply bytes** for DSR/DA/DECRQM. There is no output
  buffer on `Term`.
- The in-house `vtpane` path (the DEFAULT engine at runtime) has **no reply route
  to the PTY**. The vendored path answers queries (`AlacEvent::PtyWrite` →
  written straight back to the PTY, `crates/rt-engine/src/lib.rs:300`); the
  in-house path drops them silently.

So an application that probes cursor position (DSR 6 / CPR), negotiates device
attributes (DA1/DA2), or queries a mode (DECRQM) gets **no answer** under the
default engine. This is both a verification gap and a latent runtime robustness
gap.

`esctest` (the real Python conformance suite) depends on these replies; this
slice builds exactly the machinery it will later plug into, but does **not** wire
esctest itself (a separate slice — see Out of scope).

## Decisions (from brainstorming)

1. **Reply surfacing = drainable buffer on `Term`.** Mirror the existing
   `take_title()` pattern: `Term` accumulates reply bytes into an internal
   `Vec<u8>`; a new `take_output(&mut self) -> Vec<u8>` drains it. Rejected
   alternatives: a `&mut dyn Write` sink (changes `feed`'s hot-path signature and
   every caller) and returning bytes from `feed` (same signature churn). The
   buffer is least-invasive and keeps the parser hot path untouched.
2. **Only differential what the oracle answers.** Every in-scope sequence maps to
   a real alacritty reply, so the differential is meaningful. Sequences the
   oracle cannot answer are explicitly deferred (below), not faked.
3. **Include runtime wiring.** vtpane drains `take_output()` and forwards to the
   existing writer thread, closing the real gap — not just harness parity.
4. **DA2 version is an intentional divergence.** vt-term keeps its own version
   number; the comparator masks DA2's version field rather than forcing equality.

## Sequence set (slice 1)

All verified against the oracle's handlers in
`vendor/alacritty_terminal/src/term/mod.rs`:

| Query | Trigger | Oracle reply | Comparison |
|---|---|---|---|
| DA1 | `ESC[c`, `ESC[0c` | `\x1b[?6c` | exact |
| DA2 | `ESC[>c` | `\x1b[>0;{ver};1c` | version field masked |
| DSR status | `ESC[5n` | `\x1b[0n` | exact |
| CPR (DSR 6) | `ESC[6n` | `\x1b[{line};{col}R` (1-based, absolute) | exact |
| DECRQM ANSI | `ESC[{Ps}$p` | `\x1b[{Ps};{val}$y` | exact |
| DECRQM private | `ESC[?{Ps}$p` | `\x1b[?{Ps};{val}$y` | exact |

Notes:

- **CPR is absolute** (alacritty uses `grid.cursor.point` directly, not
  DECOM-relative). Match the oracle: absolute, 1-based, `line;col`.
- **DECRQM** reports each mode's current state as one of DEC's five values:
  `0` not recognised, `1` set, `2` reset, `3` permanently set, `4` permanently
  reset. vt-term must map its known modes (ANSI + private/DEC) to the correct
  value and return `0` for anything it does not recognise — matching
  `report_mode`/`report_private_mode` in the oracle. This is the substantive
  piece of the slice.
- **DA1 = `\x1b[?6c`** (VT102) — matched exactly.

### Explicitly out of scope for this slice

The oracle is silent or environment-dependent, so no valid differential exists:

- **DECRQCRA / rectangular checksum** — alacritty does not implement it. It is
  `esctest`'s key dependency and belongs to the later esctest slice.
- **Pixel-size window ops** (`ESC[14t`) — need real font/cell metrics absent in
  the headless harness. (`ESC[18t` char-size is deterministic and *may* be added
  if cheap, but is not required.)
- **Kitty keyboard-mode reports** — gated off by config in the oracle.
- **DCS-based queries** (DECRQSS etc.) — a separate family.

## Architecture

### vt-term (`crates/vt-term`)

- New field: an output buffer `Vec<u8>` on `Term` (or its state), written by the
  reply handlers.
- New method: `take_output(&mut self) -> Vec<u8>` — drains and returns the buffer
  (empty `Vec` when nothing pending), exactly parallel to `take_title`.
- Reply handlers in the CSI dispatch:
  - `device_status(n)` → DSR 5 / CPR 6.
  - `identify_terminal(intermediate)` → DA1 / DA2.
  - `report_mode` / `report_private_mode` (DECRQM `$p`) → `$y` reply, driven by a
    mode→state lookup over vt-term's existing mode flags.
- Replies are pushed while the parser lock is held (inside `feed`); no reply is
  generated on the render/observe path.

### Harness (`crates/vt-conformance`)

- `VtEngine` trait gains `fn take_output(&mut self) -> Vec<u8>`.
  - `vtterm::…` drains `Term::take_output`.
  - `vendored::…` drains a buffer its event proxy fills from `Event::PtyWrite`
    (today the conformance proxy drops PtyWrite; it must capture it instead).
- Reply comparator: byte-equality with a **DA2 version mask** — a small helper
  that, when both streams contain a `\x1b[>0;<n>;1c` DA2 reply, normalises the
  `<n>` field before comparing. All other bytes compared verbatim.
- New fuzz strand `tests/vtterm_report.rs`: the script generator interleaves query
  sequences (the set above) with normal grid-mutating input at varied cursor
  positions and modes; after each script both engines are drained and their reply
  streams compared. **Ceiling: 0 divergences**, a strict regression guard, run on
  x86_64 + riscv64 via `ci/verify.sh` like `vtterm_fuzz`/`vtterm_reflow`.
- Optionally extend the spec-case table (`spec.rs`) with a few hand-written
  `(query → expected reply)` cases for readability, using the same drain.

### Runtime (`crates/rt-engine/src/vtpane.rs`)

- After the reader thread feeds bytes into `Term`, drain `take_output()` and, if
  non-empty, hand the bytes to the existing writer thread via the same channel
  keystrokes use (`input_tx` / the writer `_writer` path). This mirrors the
  vendored `Proxy::PtyWrite` handler (`lib.rs:300`) one-for-one.
- Draining happens right after `feed`, off the render hot path. No new thread, no
  new channel — reuse the writer that already exists.

## Data flow

```
app writes  ESC[6n  ──► PTY master ──► reader thread ──► Term::feed
                                                            │ (parser lock)
                                                            ▼
                                              device_status(6) pushes "\x1b[..R"
                                                            │
reader thread: let out = term.take_output();  ◄────────────┘
   if !out.is_empty() { input_tx.send(out) }  ──► writer thread ──► PTY master ──► app
```

Harness path is the same minus the PTY: `feed(script)` then
`take_output()` on both engines, compare.

## Error handling / edge cases

- **Empty drain is the common case** — `take_output` returns an empty `Vec`;
  vtpane skips the send. No allocation churn concern (buffer reused/cleared).
- **Multiple queries per feed** accumulate in order; the buffer preserves emission
  order so the differential is order-sensitive (correct — apps rely on order).
- **CPR after cursor moves** must reflect the cursor at the moment the query is
  parsed, not end-of-feed — because replies are generated inline during `feed`,
  this is automatic.
- **Crashed/frozen pane**: `take_output` is only called on the live feed path; a
  frozen pane feeds nothing, so nothing drains. No special case.
- **Unknown DSR/DECRQM arg**: match the oracle — DSR unknown = no reply; DECRQM
  unknown mode = `0` ("not recognised") reply. (DSR is silent; DECRQM answers.)

## Testing

TDD throughout:

1. **Pure unit tests (vt-term)** for each reply format, watched to fail first:
   DA1, DA2 (shape), DSR 5, CPR at several cursor positions, and a DECRQM
   state-table sweep (set / reset / unknown → `1`/`2`/`0`) across a representative
   mode selection. These are the RED tests; implement handlers to green.
2. **Differential fuzz** (`vtterm_report.rs`) driven to **0** vs the oracle on
   x86_64 and riscv64. Locked as the ceiling.
3. **Real-app smoke (dop651, manual):** under the default engine,
   `printf '\e[6n'; read -rsN 10 x; printf '%q\n' "$x"` returns a plausible CPR;
   `vim` and `tmux` start and redraw cleanly (they issue DA/DSR handshakes);
   confirm no regression with `RT_ENGINE=alacritty`.

## Documentation

- `docs/vt-term-design.md` — add a "Query / report" section: the output buffer,
  each reply's format, the DECRQM state mapping, and the inline-during-feed
  timing rationale.
- `docs/engine-divergence.md` — record the DA2-version **intentional** divergence;
  move query/report out of "Known not-yet-implemented"; note the new
  `vtterm_report` 0-ceiling.
- `docs/own-engine-plan.md` — mark the query/report forcing-function step
  progressed (esctest still pending).
- Update `project-map.js`: the `vt-term` node's "OSC / DCS & query-report edges"
  part reflects query/report done (OSC/DCS still active); bump `project.updated`.

## Out of scope

- **esctest hookup** — the real Python suite. This slice builds the reply
  machinery it needs (`take_output`, PTY wiring) but does not drive esctest.
- **DECRQCRA / checksum**, pixel window ops, kitty keyboard reports, DCS queries
  (see "Explicitly out of scope" above).
- **OSC side-effects** (clipboard 52, hyperlink 8, palette 4/104) — a different
  Thread-2 slice.

## What is reused vs new

**Reused:**
- The `take_title()` drain pattern (vt-term).
- The vtpane writer thread + `input_tx` channel (runtime reply path).
- `ci/verify.sh`, the fuzz script generator, and the ledger/ceiling discipline.
- The vendored `Event::PtyWrite` the oracle already emits.

**New:**
- `Term` output buffer + `take_output()` and the DSR/DA/DECRQM reply handlers.
- `VtEngine::take_output`, the oracle's PtyWrite capture, the DA2-mask comparator,
  and the `vtterm_report` differential strand.
- The vtpane drain-and-forward call.
