//! Task 2 integration test for `rt_engine::budget`: a dozen-plus REAL, PTY-backed panes
//! (real `/bin/sh` children, real output through the actual reader-thread path — nothing
//! poked directly into a `Term`), driven until they hold substantial scrollback, then
//! rebalanced against a `Budget` this test constructs and owns outright.
//!
//! Deliberately never `Budget::default()`'s production constants (4 GiB never binds
//! against what a handful of real shells can produce inside a test's lifetime) and never
//! anything shared across tests — see `budget.rs`'s own module doc on why a shared/global
//! instance was removed; each test building its own `Budget` is what keeps these isolated
//! under cargo's parallel test threads.
//!
//! Proves the two properties this whole design exists for:
//!   1. the process-wide total comes under budget once `rebalance` runs, and
//!   2. a busy pane still keeps substantially more history than an idle one — the
//!      proportional split the user chose over equal division.

use rt_engine::budget::Budget;
use rt_engine::TermPane;
use std::sync::Arc;
use std::time::{Duration, Instant};

const COLS: usize = 80;
const ROWS: usize = 24;
// Both line counts must clear ROWS before anything lands in scrollback at all; BUSY_LINES
// clears it by two orders of magnitude (deep history), IDLE_LINES by a little (some
// history, not none — a pane that produced literally nothing wouldn't test much).
const BUSY_LINES: usize = 2000;
const IDLE_LINES: usize = 40;
const N_BUSY: usize = 4;
const N_IDLE: usize = 12; // 16 real panes total — "a dozen or more" per the plan.

/// Poll a pane's visible screen for `sentinel`, up to `timeout`. Used to know a spawned
/// shell has actually finished writing before we measure anything — these are real PTYs
/// with a real reader thread, not a synchronous feed.
fn wait_for_sentinel(pane: &TermPane, sentinel: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if pane.snapshot().to_text().contains(sentinel) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn spawn_shell(script: String, budget: &Arc<Budget>) -> TermPane {
    TermPane::spawn_vt_env(
        Some(("/bin/sh".to_string(), vec!["-c".to_string(), script])),
        None,
        COLS,
        ROWS,
        &[],
        1_000_000, // line cap: generous, so this test's byte budget governs, not the line cap
        budget,
    )
    .expect("spawn real pty pane")
}

#[test]
fn process_wide_total_stays_under_budget_and_busy_pane_keeps_more_than_idle() {
    // This test's own budget. `pane_floor` is picked small relative to what these panes
    // actually accumulate (idle panes land in the tens of KB; see the module doc) so the
    // floor never binds here and can't inflate the post-rebalance total above the target —
    // Task 1's own tests already cover the floor's own guarantee in isolation. The
    // constructor's `global_budget` param is irrelevant below: we call `rebalance(target)`
    // directly with a target computed from what these real panes actually produced, rather
    // than `rebalance_default()`, since the real byte counts aren't worth hardcoding.
    const PANE_FLOOR_TEST: usize = 1024;
    let budget = Arc::new(Budget::new(usize::MAX, PANE_FLOOR_TEST));

    let mut busy_panes = Vec::new();
    for _ in 0..N_BUSY {
        let script = format!(
            "for i in $(seq 1 {BUSY_LINES}); do echo BUSY line $i; done; echo BUSY_DONE; sleep 5"
        );
        busy_panes.push(spawn_shell(script, &budget));
    }

    let mut idle_panes = Vec::new();
    for _ in 0..N_IDLE {
        let script = format!(
            "for i in $(seq 1 {IDLE_LINES}); do echo IDLE line $i; done; echo IDLE_DONE; sleep 5"
        );
        idle_panes.push(spawn_shell(script, &budget));
    }

    // Wait for every real shell's output to actually land before measuring anything.
    for p in &busy_panes {
        assert!(
            wait_for_sentinel(p, "BUSY_DONE", Duration::from_secs(30)),
            "a busy pane never finished producing its output"
        );
    }
    for p in &idle_panes {
        assert!(
            wait_for_sentinel(p, "IDLE_DONE", Duration::from_secs(30)),
            "an idle pane never finished producing its output"
        );
    }

    assert_eq!(
        budget.live_panes(),
        N_BUSY + N_IDLE,
        "every spawned pane must be registered with this test's budget"
    );

    let total_before = budget.total_usage();
    assert!(total_before > 0, "real scrolled output must have accounted for something");

    // Snapshot the busy:idle split BEFORE the squeeze. The assertion at the end compares
    // the ratio after against this one; capturing it here is what makes that comparison
    // independent of BUSY_LINES/IDLE_LINES/N_BUSY/N_IDLE (see the comment there).
    // History lines are a valid proxy for bytes at a fixed column width — vt-term's
    // per-history-line cost is uniform.
    let busy_lines_before: usize = busy_panes.iter().map(|p| p.scroll_info().1).sum();
    let idle_lines_before: usize = idle_panes.iter().map(|p| p.scroll_info().1).sum();
    // Setup precondition, not the property under test: ratio-preservation only
    // discriminates on a LOPSIDED population. If busy and idle held the same amount,
    // proportional and equal division would both leave the ratio at 1:1 and the assertion
    // below would be vacuous. Fail loudly here if the constants ever stop producing a
    // lopsided mix, rather than silently proving nothing.
    assert!(
        busy_lines_before > idle_lines_before * 2,
        "test setup: the pane mix must be lopsided for a ratio test to mean anything \
         (busy {busy_lines_before} vs idle {idle_lines_before} history lines)"
    );

    // Squeeze hard — a quarter of what's actually accumulated — so the over-budget path
    // definitely runs (not the under-budget short-circuit), regardless of the exact byte
    // counts real shells/PTYs produce (not worth hardcoding; see `total_before` above).
    let target = total_before / 4;
    budget.rebalance(target);

    let total_after = budget.total_usage();
    assert!(
        total_after <= target,
        "process-wide total {total_after} still over the {target}-byte target after rebalance \
         (started at {total_before})"
    );

    // The proportional property, asserted so the pane mix can't decide the outcome.
    //
    // A bare `busy_after > idle_after` does NOT distinguish this design from flat equal
    // division (`cap_i = budget / pane_count`): both pass it. An absolute threshold — the
    // 8x this test used to assert — does distinguish it for THESE constants, but only for
    // these: at IDLE_LINES = 25 equal division also clears 8x, and at IDLE_LINES = 200 the
    // real proportional policy would fail it. That makes the test a property of the pane
    // mix, so editing a constant two dozen lines above silently guts it.
    //
    // What is mix-independent is what proportional actually guarantees: every pane's cap
    // is scaled by the SAME factor (`budget / total`), so the busy:idle ratio survives the
    // squeeze. Equal division does the opposite by construction — it collapses every pane
    // toward one flat cap, so the ratio falls toward 1 (or below, when the idle panes sit
    // under the flat cap and are not trimmed at all). So: compare the ratio after against
    // the ratio before, and require it to have held to within `RATIO_TOLERANCE`.
    //
    // Cross-multiplied to stay in integers (u128, since these are sums over panes):
    //   busy_after / idle_after  >=  (busy_before / idle_before) / RATIO_TOLERANCE
    // ⇔ busy_after * idle_before * RATIO_TOLERANCE  >=  busy_before * idle_after
    //
    // The tolerance absorbs line-granularity rounding on the small idle panes (a quarter
    // of 16 lines does not divide evenly), which nudges the post ratio slightly down. It
    // is nowhere near enough to absorb the collapse equal division causes: measured, the
    // ratio goes from ~41:1 to ~2.35:1 here, a factor of ~17.
    const RATIO_TOLERANCE: u128 = 2;
    let busy_lines_after: usize = busy_panes.iter().map(|p| p.scroll_info().1).sum();
    let idle_lines_after: usize = idle_panes.iter().map(|p| p.scroll_info().1).sum();
    assert!(
        busy_lines_after as u128 * idle_lines_before as u128 * RATIO_TOLERANCE
            >= busy_lines_before as u128 * idle_lines_after as u128,
        "the busy:idle ratio must survive a proportional squeeze (every pane's cap scales \
         by the same budget/total factor): before {busy_lines_before}:{idle_lines_before}, \
         after {busy_lines_after}:{idle_lines_after}. A collapse toward 1:1 like this is \
         what flat equal division produces, not proportional sharing"
    );
}
