# Freeze/thaw + `pending_raw` (phase 2b-i) — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Be able to stop a live pane's reader thread at a parse boundary, without losing a byte, and read back the raw bytes of any half-consumed escape sequence.

**Architecture:** `vt-parser` gains a raw-echo buffer that accumulates only while the state machine is OFF `Ground`, plus a `pending_raw()` accessor. `VtPane`'s reader thread is restructured to `poll()` the PTY master alongside a wakeup pipe, so it can be woken on a quiet pane; `freeze()` wakes it, drains what is pending, applies it under the `Term` lock, and parks it, and `thaw()` releases it. No `adopt()`, no descriptor handover — those are 2b-ii and 2b-iii.

**Tech Stack:** Rust 2021. `vt-parser` gains NO dependencies. `rt-engine` already has `libc`.

**Spec:** `docs/superpowers/specs/2026-08-29-cross-instance-pane-transfer-design.md` — read "The transfer protocol" (the FREEZE step) and "The child after the move".

**Prior slices:** phase 1 (`rt-handoff`, the frozen wire format) and phase 2a (`TermPane::export`) are on `feat/handoff-wire-v1` and `feat/handoff-engine-export`, both unmerged.

## Global Constraints

- **`vt-parser` gains NO dependencies**, and its throughput must not regress. It is benchmarked on a riscv64 board (a MilkV Mars) as a co-equal target, and the differential harness holds it at 0 divergences against the vendored `vte` oracle. Both must still hold at the end of this slice.
- **The raw-echo buffer must not touch the ground-state fast path.** `advance_ground` bulk-scans printable runs with `memchr` and is the overwhelming majority of real throughput. Bytes are echoed only while `state != Ground` — i.e. only inside an escape sequence, which is a tiny fraction of any real stream. A design that appends every byte is wrong even if it passes the tests.
- **Freeze must lose nothing.** The reader currently blocks in `read()` into a 64 KiB stack buffer; bytes returned by `read()` but not yet handed to `Term::feed` exist nowhere else — the kernel has already discarded them. Any freeze that can drop those is a failed freeze.
- **Freeze must work on an IDLE pane.** A shell sitting at a prompt produces nothing, so a design that only notices a freeze request when the next bytes arrive would hang. That is why the reader polls a wakeup pipe as well as the PTY.
- **Thaw must fully restore.** A frozen-then-thawed pane must be indistinguishable from one that was never frozen — same output, same fd count, same thread count.
- **Commits** follow the repo's conventional-commit style with the session trailer.
- **Every task ends green:** `cargo test -p vt-parser -p vt-conformance -p vt-term -p rt-engine` passes. Do NOT run the whole workspace suite (it builds GUI crates needing system libraries). Do NOT run `ci/verify.sh` — it ssh's to remote machines and is the controller's to run.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/vt-parser/src/lib.rs` | The raw-echo buffer, its clearing at every return-to-Ground site, and `pending_raw()`. |
| `crates/rt-engine/src/vtpane.rs` | The wakeup pipe, the poll-based reader loop, and `freeze()`/`thaw()`. |
| `crates/rt-engine/tests/freeze.rs` | NEW. Real-PTY proof that a freeze loses nothing and an idle pane freezes promptly. |

---

### Task 1: `vt-parser` raw-echo buffer and `pending_raw()`

**Files:**
- Modify: `crates/vt-parser/src/lib.rs`

**Interfaces:**
- Produces: `Parser::pending_raw(&self) -> &[u8]`.

**The ten sites that return to `Ground`**, all of which must clear the buffer (verified by exploration, with line numbers as of `6979ab1` — re-verify before editing, the file may have moved):

| ~line | function | context |
|---|---|---|
| 418 | `anywhere` | `0x18 \| 0x1A` — CAN/SUB abort, reachable from any state |
| 438 | `advance_esc` | final byte of a two-character ESC sequence |
| 456 | `advance_esc` | `0x18 \| 0x1A` inline |
| 469 | `advance_esc_intermediate` | final byte after intermediates |
| 540 | `advance_csi_ignore` | final byte of an overflowed/invalid CSI |
| 614 | `advance_dcs_passthrough` | `0x18 \| 0x1A` — `unhook()` then Ground |
| 624 | `advance_dcs_passthrough` | `0x9C` (ST) — `unhook()` then Ground |
| 637 | `advance_osc_string` | `0x07` (BEL) — `osc_end()` then Ground |
| 642 | `advance_osc_string` | `0x18 \| 0x1A` — `osc_end()` + execute |
| 673 | `csi_dispatch` | after every completed CSI final byte — the common case |

`hook()` transitions to `DcsPassthrough` rather than Ground: a DCS data phase is itself resumable and its bytes must keep accumulating.

**Synchronized updates are a second buffer, not a complication of the first.** While `sync_active`, `feed()` routes bytes into `sync_buffer` and they never reach the state machine at all. Entering a sync always leaves the machine in `Ground` (`csi_dispatch` sets it unconditionally right after). So mid-sync the pending raw bytes are exactly `sync_buffer`, and the echo buffer is empty. `pending_raw` must return the concatenation in the right order: any echo-buffer bytes first (there will be none mid-sync), then `sync_buffer`.

- [ ] **Step 1: Write the failing tests**

Add to `crates/vt-parser/src/lib.rs`'s test module:

```rust
    /// Feed `bytes` to a fresh parser with a no-op performer and return what it
    /// considers still in flight.
    fn pending_after(bytes: &[u8]) -> Vec<u8> {
        let mut p = Parser::new();
        let mut sink = crate::tests::NullPerformer::default();
        p.advance(&mut sink, bytes);
        p.pending_raw().to_vec()
    }

    #[test]
    fn a_complete_stream_leaves_nothing_pending() {
        assert!(pending_after(b"hello \x1b[1mworld\x1b[m done").is_empty());
    }

    #[test]
    fn ground_text_is_never_echoed() {
        // The hot path must not accumulate. A megabyte of plain text with no
        // escape at all must leave the buffer empty AND untouched.
        let text = vec![b'x'; 1024 * 1024];
        assert!(pending_after(&text).is_empty());
    }

    #[test]
    fn a_half_finished_csi_is_pending_verbatim() {
        assert_eq!(pending_after(b"\x1b[38;5"), b"\x1b[38;5");
    }

    #[test]
    fn a_half_finished_osc_is_pending_verbatim() {
        assert_eq!(pending_after(b"\x1b]0;a tit"), b"\x1b]0;a tit");
    }

    #[test]
    fn a_lone_escape_is_pending() {
        assert_eq!(pending_after(b"\x1b"), b"\x1b");
    }

    #[test]
    fn a_split_utf8_codepoint_is_pending() {
        // The first two bytes of a three-byte character.
        let s = "日".as_bytes();
        assert_eq!(pending_after(&s[..2]), &s[..2]);
    }

    #[test]
    fn completing_a_sequence_clears_the_buffer() {
        let mut p = Parser::new();
        let mut sink = crate::tests::NullPerformer::default();
        p.advance(&mut sink, b"\x1b[38;5");
        assert!(!p.pending_raw().is_empty(), "mid-sequence");
        p.advance(&mut sink, b";200m");
        assert!(p.pending_raw().is_empty(), "the CSI completed");
    }

    #[test]
    fn an_aborted_sequence_clears_the_buffer() {
        // CAN (0x18) aborts from anywhere.
        let mut p = Parser::new();
        let mut sink = crate::tests::NullPerformer::default();
        p.advance(&mut sink, b"\x1b[38;5\x18");
        assert!(p.pending_raw().is_empty(), "CAN returned us to Ground");
    }

    #[test]
    fn replaying_pending_bytes_into_a_fresh_parser_reproduces_the_state() {
        // This is the property the whole accessor exists for: the receiver
        // replays these bytes and lands in the same place the donor was.
        let mut donor = Parser::new();
        let mut sink = crate::tests::NullPerformer::default();
        donor.advance(&mut sink, b"text \x1b[1;38;5");
        let pending = donor.pending_raw().to_vec();

        let mut receiver = Parser::new();
        let mut sink2 = crate::tests::NullPerformer::default();
        receiver.advance(&mut sink2, &pending);

        // Finishing the sequence must dispatch identically on both.
        let mut a = crate::tests::RecordingPerformer::default();
        let mut b = crate::tests::RecordingPerformer::default();
        donor.advance(&mut a, b";200m");
        receiver.advance(&mut b, b";200m");
        assert_eq!(a.actions, b.actions, "replayed parser dispatches identically");
    }
```

Use whatever performer types the existing test module already provides rather than the invented `NullPerformer`/`RecordingPerformer` names above — read the module first and adapt. The ASSERTIONS are the requirement; the helper names are not.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p vt-parser pending`
Expected: FAIL — no method named `pending_raw`.

- [ ] **Step 3: Add the buffer, echoing only off Ground**

Add a field to `Parser` and echo in `change_state` — which is the single dispatch point for every non-Ground byte — NOT in `advance_ground`:

```rust
    /// Raw bytes of the escape sequence currently in flight, if any.
    ///
    /// Accumulates ONLY while the machine is off `Ground`: `advance_ground`'s
    /// bulk `memchr` scan of printable text never touches it, so the hot path
    /// is unaffected. Cleared at every site that returns to `Ground`.
    ///
    /// This exists so a pane frozen mid-sequence can hand the bytes to another
    /// process, which replays them into its own parser. Raw bytes are the right
    /// currency: everything else the parser holds — state, intermediates,
    /// params, partial UTF-8 — is itself derived from replaying them.
    pending_raw: Vec<u8>,
```

Clear it at each of the ten sites in the table above, and add the accessor:

```rust
    /// The bytes of a partially-consumed sequence, in order, ready to be
    /// replayed into a fresh parser. Empty when the machine is at a boundary.
    ///
    /// Mid-synchronized-update the machine is always at `Ground` and the bytes
    /// live in `sync_buffer` instead, so both are returned in stream order.
    pub fn pending_raw(&self) -> &[u8] { /* echo buffer, then sync_buffer */ }
```

Returning a single `&[u8]` across two buffers needs one of them materialised. Prefer keeping ONE buffer: have the sync path append to `pending_raw` as well as `sync_buffer`, or have `pending_raw()` return a `Cow`/`Vec`. Choose, and say in your report which and why — the accessor's SHAPE is yours to decide, its CONTENTS are not.

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p vt-parser`
Expected: PASS.

- [ ] **Step 5: Prove the differential harness is untouched**

Run: `cargo test -p vt-conformance`
Expected: PASS — 0 divergences against the vendored oracle. This slice must not change what the parser DOES, only expose what it is holding. A divergence here means the echo buffer altered behaviour.

- [ ] **Step 6: Prove throughput did not regress**

Run: `cargo run --release --example parser_bench -p vt-conformance`
Compare against the same command on `HEAD~1` (stash, run, restore). Report BOTH numbers. A ground-state-text regression above a few percent means the buffer is being touched on the hot path — go back to Step 3 rather than accepting it. The riscv board is the real gate and the controller runs it; your job is to show x86-64 is clean first.

- [ ] **Step 7: Commit**

```bash
git add crates/vt-parser/src/lib.rs
git commit -m "feat(vt-parser): expose the bytes of a sequence still in flight"
```

---

### Task 2: A wakeup pipe and a poll-based reader loop

A behaviour-preserving refactor. The reader must still do exactly what it does
today; it just becomes interruptible. Freeze itself is Task 3 — keeping them
apart means that if the pane misbehaves afterwards, the cause is one change or
the other, not both at once.

**Files:**
- Modify: `crates/rt-engine/src/vtpane.rs`

**Interfaces:**
- Produces: a wakeup pipe owned by `VtPane` (write end) and its reader thread (read end); the reader loop blocks in `poll()` on both descriptors rather than in `read()` on one.

Today (verified by exploration, line numbers as of `6979ab1` — re-verify):

- `read_fd` is a `dup` of the master (~line 140), made blocking by clearing `O_NONBLOCK` (~148-153), then owned by the reader thread as a `File` (~line 204).
- The loop blocks in `file.read(&mut buf)` with a 64 KiB stack buffer (~205), and on `Ok(n)` takes the `Term` lock ONCE (~220) to `feed`, `take_title` and `take_output`.
- It exits only on EOF, a hard error, or a caught panic. There is no way in.

- [ ] **Step 1: Write the failing test**

Add to `crates/rt-engine/src/vtpane.rs`'s test module — this pins the property the refactor must not break, and fails today only because the constructor does not yet make a pipe:

```rust
    #[test]
    fn the_reader_still_delivers_output_after_the_poll_refactor() {
        let pane = VtPane::spawn_env(
            Some(("/bin/sh".into(), vec!["-c".into(), "printf 'AFTER_REFACTOR'; sleep 5".into()])),
            None, 40, 6, &[], 1000,
        )
        .expect("spawn");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let t = pane.lock_term();
            let row: String = (0..14).map(|c| t.cell(0, c).c).collect();
            if row.starts_with("AFTER_REFACTOR") {
                return;
            }
        }
        panic!("the reader stopped delivering output");
    }

    #[test]
    fn the_pane_owns_a_wakeup_pipe() {
        let pane = VtPane::spawn_env(
            Some(("/bin/sh".into(), vec!["-c".into(), "sleep 5".into()])),
            None, 20, 4, &[], 1000,
        )
        .expect("spawn");
        assert!(pane.has_wakeup_pipe(), "freeze needs a way to wake an idle reader");
    }
```

`has_wakeup_pipe()` is a small `#[cfg(test)]`-visible predicate; if you prefer to assert the invariant another way that does not add public surface, do that and say so.

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p rt-engine wakeup`
Expected: FAIL — no method `has_wakeup_pipe`.

- [ ] **Step 3: Create the pipe and restructure the loop**

In `spawn_env`, create a pipe (`libc::pipe2` with `O_CLOEXEC`). The write end is owned by `VtPane`; the read end moves into the reader thread. In the reader loop, replace the blocking `file.read(..)` with a `poll()` over both descriptors:

- `POLLIN` on the master → read and apply exactly as today, including taking the `Term` lock once for `feed` + `take_title` + `take_output`.
- `POLLIN` on the wakeup pipe → drain the pipe byte(s). In THIS task that is all it does; Task 3 gives it meaning.
- `poll()` returning `EINTR` → retry, do not treat as an error.
- Master `POLLHUP`/EOF → break, exactly as today. Do NOT emit `Exited` here; `next_child_event` remains the sole exit authority (the existing comment explains why, and there is a regression test for a backgrounded grandchild holding the slave open).

Keep the master fd blocking or make it non-blocking as the loop requires, but be explicit in a comment about which and why — the current code deliberately clears `O_NONBLOCK`, and silently reversing that would change behaviour under a partial read.

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p vt-term -p rt-engine`
Expected: PASS, including the existing PTY tests (`echo.rs`, `soak.rs`, `history.rs`, `export.rs`) — those are what prove the refactor is behaviour-preserving.

- [ ] **Step 5: Prove no descriptor leak**

`soak.rs` already hammers spawn/drop asserting the open-fd count does not climb. A pipe adds two descriptors per pane, so confirm the soak still passes and say in your report what the per-pane fd count is before and after this change.

- [ ] **Step 6: Commit**

```bash
git add crates/rt-engine/src/vtpane.rs
git commit -m "refactor(rt-engine): poll the pty alongside a wakeup pipe, so the reader is interruptible"
```

---

### Task 3: `freeze()` and `thaw()`

**Files:**
- Modify: `crates/rt-engine/src/vtpane.rs`
- Modify: `crates/rt-engine/src/lib.rs` (dispatch on `TermPane`, refusing on the vendored arm by name, as `export` does)

**Interfaces:**
- Produces: `VtPane::freeze(&self) -> FreezeGuard` (or equivalent), `VtPane::thaw(&self)`, and `TermPane::freeze`/`thaw` returning `Result<_, ExportError>` with the alacritty arm refusing.

What freeze must guarantee, in order:

1. Wake the reader even if the pane is idle — write to the wakeup pipe.
2. The reader drains everything currently readable from the master and applies it under the `Term` lock. Nothing that `read()` returned may go unapplied.
3. The reader acknowledges that it is parked, and freeze does not return until it has. A freeze that returns while the reader is still running has not frozen anything.
4. While parked, the reader touches neither the master nor the `Term`.
5. `thaw()` releases it and normal operation resumes.

- [ ] **Step 1: Write the failing tests**

```rust
    #[test]
    fn freezing_an_idle_pane_returns_promptly() {
        // The case a cooperative flag cannot serve: a shell at a prompt emits
        // nothing, so a freeze that waits for output would hang here.
        let pane = VtPane::spawn_env(
            Some(("/bin/sh".into(), vec!["-c".into(), "sleep 30".into()])),
            None, 20, 4, &[], 1000,
        ).expect("spawn");
        std::thread::sleep(std::time::Duration::from_millis(300)); // settle
        let t0 = std::time::Instant::now();
        pane.freeze();
        assert!(t0.elapsed() < std::time::Duration::from_secs(1), "freeze took {:?}", t0.elapsed());
        pane.thaw();
    }

    #[test]
    fn a_freeze_loses_no_output() {
        // Write a known amount, freeze, and assert every byte reached the Term.
        // Bytes read() returned but never fed exist nowhere else — the kernel
        // has already discarded them — so this is the property that matters.
        let pane = VtPane::spawn_env(
            Some(("/bin/sh".into(), vec!["-c".into(),
                  "for i in $(seq 1 500); do echo LINE$i; done; sleep 30".into()])),
            None, 40, 10, &[], 100_000,
        ).expect("spawn");
        std::thread::sleep(std::time::Duration::from_millis(600));
        pane.freeze();
        let t = pane.lock_term();
        let mut seen = 0;
        for abs in t.topmost()..=t.bottommost() {
            let s: String = (0..40).map(|c| t.cell_at(abs, c).c).collect();
            if s.trim_end().starts_with("LINE") { seen += 1; }
        }
        drop(t);
        pane.thaw();
        assert!(seen >= 490, "only {seen} of 500 lines survived the freeze");
    }

    #[test]
    fn a_thawed_pane_is_indistinguishable_from_one_never_frozen() {
        let pane = VtPane::spawn_env(
            Some(("/bin/sh".into(), vec!["-c".into(), "cat".into()])),
            None, 30, 6, &[], 1000,
        ).expect("spawn");
        pane.freeze();
        pane.thaw();
        pane.write(b"still alive\n");
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            std::thread::sleep(std::time::Duration::from_millis(20));
            let t = pane.lock_term();
            for r in 0..6 {
                let s: String = (0..30).map(|c| t.cell(r, c).c).collect();
                if s.contains("still alive") { return; }
            }
        }
        panic!("a thawed pane stopped echoing");
    }

    #[test]
    fn freeze_is_idempotent_and_thaw_without_freeze_is_harmless() {
        let pane = VtPane::spawn_env(
            Some(("/bin/sh".into(), vec!["-c".into(), "sleep 30".into()])),
            None, 20, 4, &[], 1000,
        ).expect("spawn");
        pane.thaw();            // never frozen
        pane.freeze();
        pane.freeze();          // twice
        pane.thaw();
        pane.thaw();            // twice
    }
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p rt-engine freeze`
Expected: FAIL — no method `freeze`.

- [ ] **Step 3: Implement**

Shape is yours, but these are requirements, not suggestions:

- The acknowledgement must be a real handshake (a channel, or a condvar plus a state enum). A `sleep` is not an acknowledgement, and a test that passes because of a sleep proves nothing.
- The drain step must loop until the master reports no more data, not read once. A single `read()` returns at most 64 KiB and a busy pane can have more queued.
- A pane whose child has already exited must still freeze and thaw without hanging — do not wait forever for a reader that has broken out of its loop.
- Do not emit `Exited` from any new path; `next_child_event` stays the sole exit authority.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p vt-term -p rt-engine`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/rt-engine/src/vtpane.rs crates/rt-engine/src/lib.rs
git commit -m "feat(rt-engine): freeze a pane at a parse boundary, and thaw it again"
```

---

### Task 4: Real-PTY freeze test, and `pending_raw` reaching the wire

**Files:**
- Create: `crates/rt-engine/tests/freeze.rs`
- Modify: `crates/rt-engine/src/handoff.rs` (populate `PaneWire::pending_raw`)

Phase 2a left `pending_raw` (wire tag 0x2A) empty and the spec records it under
what does not survive a move. This task closes that: with a freeze, the donor
can now capture it, so `export_term` should carry it.

- [ ] **Step 1: Write the tests**

`crates/rt-engine/tests/freeze.rs`, driving a real shell through a real PTY:

```rust
//! Freeze a live pane and prove nothing is lost — through the whole path:
//! fork, pty, poll loop, parser, grid.

use rt_engine::TermPane;

#[test]
fn a_pane_frozen_mid_sequence_reports_the_pending_bytes() {
    // printf a deliberately truncated CSI, then stop. The parser is left
    // mid-sequence with nothing following, which is exactly the state a
    // handoff has to carry across.
    let pane = TermPane::spawn_vt_env(
        Some(("/bin/sh".into(), vec!["-c".into(), "printf 'x\\033[38;5'; sleep 30".into()])),
        None, 30, 6, &[], 1000,
    ).expect("spawn");
    std::thread::sleep(std::time::Duration::from_millis(500));
    pane.freeze().expect("in-house engine freezes");
    let (wire, _) = pane.export(1, 0).expect("export");
    pane.thaw().expect("thaw");
    assert_eq!(wire.pending_raw, b"\x1b[38;5", "the half-written CSI must travel");
}

#[test]
fn a_pane_frozen_at_a_boundary_reports_nothing_pending() {
    let pane = TermPane::spawn_vt_env(
        Some(("/bin/sh".into(), vec!["-c".into(), "printf 'clean\\n'; sleep 30".into()])),
        None, 30, 6, &[], 1000,
    ).expect("spawn");
    std::thread::sleep(std::time::Duration::from_millis(500));
    pane.freeze().expect("freeze");
    let (wire, _) = pane.export(1, 0).expect("export");
    pane.thaw().expect("thaw");
    assert!(wire.pending_raw.is_empty(), "nothing was in flight");
}
```

- [ ] **Step 2: Wire `pending_raw` into `export_term`**

Read it from the parser via whatever accessor `VtPane` exposes, and set
`PaneWire::pending_raw`. Update `export_term`'s doc comment, which currently
lists `pending_raw` among the fields this engine cannot source — it can now,
when frozen. Note in the comment that an UNFROZEN export may still see an empty
value, since the reader may be mid-`feed`.

- [ ] **Step 3: Update the spec**

In `docs/superpowers/specs/2026-08-29-cross-instance-pane-transfer-design.md`,
remove `pending_raw` from "What does not survive a move" and say instead that it
is captured at freeze time. Leave the held-aside screen's cursor/pen/charsets
entry alone — that is still lost.

- [ ] **Step 4: Run everything**

Run: `cargo test -p vt-parser -p vt-conformance -p vt-term -p rt-engine`
Expected: PASS, warning-free.

- [ ] **Step 5: Commit**

```bash
git add crates/rt-engine/tests/freeze.rs crates/rt-engine/src/handoff.rs docs/superpowers/specs/2026-08-29-cross-instance-pane-transfer-design.md
git commit -m "feat(handoff): carry the half-parsed sequence across a freeze"
```

---

## Definition of done for 2b-i

- `cargo test -p vt-parser -p vt-conformance -p vt-term -p rt-engine` green and warning-free.
- `vt-conformance` still reports 0 divergences: this slice changed what the parser EXPOSES, never what it does.
- The parser benchmark shows no meaningful regression on x86-64, and the controller confirms riscv64 via `ci/verify.sh`.
- Freezing an idle pane returns in well under a second, proven by a test that would hang under a cooperative flag.
- A freeze provably loses no output, and a thawed pane still echoes.
- `PaneWire::pending_raw` carries a real half-consumed sequence off a live pane.

## What 2b-i deliberately does NOT do

- **No `adopt()`.** Rebuilding a `Term` from a `PaneWire` is 2b-ii.
- **No descriptor handover, no `Pty` disarm.** 2b-iii.
- **No alacritty support.** The vendored engine refuses by name, as with `export`.
