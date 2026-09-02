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

    // The proportional property: busy panes' scrollback line counts (a valid proxy for
    // bytes here — vt-term's per-history-line cost is uniform at a fixed column width, so
    // more lines held means more bytes held) must clearly exceed idle panes' after the
    // squeeze above. That's the entire reason this design is proportional, not equal
    // division: equal division across 16 panes would have flattened this difference away.
    let busy_lines_after: usize = busy_panes.iter().map(|p| p.scroll_info().1).sum();
    let idle_lines_after: usize = idle_panes.iter().map(|p| p.scroll_info().1).sum();
    assert!(
        busy_lines_after > idle_lines_after,
        "busy panes ({busy_lines_after} history lines) should hold more than idle panes \
         ({idle_lines_after} history lines) after rebalance"
    );
}
