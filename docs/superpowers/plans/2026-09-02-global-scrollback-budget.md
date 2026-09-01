# A process-wide scrollback budget — Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans. Steps use checkbox (`- [ ]`) syntax.

**Goal:** Bound rt's total scrollback memory across all panes, sharing it proportionally rather than dividing it equally.

**Why:** `SCROLLBACK_MEMORY_BUDGET` is **1 GiB per pane** (`crates/rt-engine/src/vtpane.rs:39`) and its comment claims it "bounds the worst case". It bounds it per pane: with 60 panes that is a 60 GiB ceiling and no process-wide limit at all. It has never actually bitten only because the configured line cap (100 000 in the user's config) reaches its limit first at roughly 12 MB/pane. Raise the line slider and nothing catches it.

**Architecture:** A process-wide registry of live panes in `rt-engine`, holding `Weak<Mutex<Term>>`. A `rebalance()` call sums each pane's `history_bytes`; when the total exceeds the global budget it recomputes each pane's BYTE cap proportional to that pane's current usage (with a floor) and calls the existing `Term::set_scrollback`, whose `trim_history` does the eviction under the pane's own lock. No cross-pane locking during eviction, and no new eviction logic — the coordinator only moves the caps.

**Tech Stack:** Rust 2021, no new dependencies.

## Global Constraints

- **No new dependencies** in `vt-term` or `rt-engine`.
- **Reuse the existing eviction.** `Term::set_scrollback(lines, bytes)` already calls `trim_history()`, which evicts oldest-first under that Term's own lock and keeps `history_bytes` in step. The coordinator must not evict anything itself.
- **Proportional, not equal.** The user chose this explicitly: one pane holding a huge build log should keep its history while 59 idle panes hold almost nothing. Equal division would make a deep-scrollback setting meaningless at high pane counts.
- **A floor per pane**, so a busy neighbour cannot starve a quiet pane to zero.
- **Locks are held briefly.** Rebalance touches every live Term in turn; each visit reads one `usize` and writes two fields. It must never hold two Term locks at once, and must never block on a pane whose lock is held by its reader thread — skip and catch it next pass.
- **A dead pane must not leak a registry slot.** `Weak` upgrade failure is the signal; prune on every pass.
- **Every task ends green:** `cargo test -p vt-term -p rt-engine`. Do NOT run the whole workspace suite; do NOT run `ci/verify.sh`.

---

### Task 1: `history_bytes()` and the budget coordinator

**Files:**
- Modify: `crates/vt-term/src/state.rs` (the read-only accessor)
- Create: `crates/rt-engine/src/budget.rs`
- Modify: `crates/rt-engine/src/lib.rs`, `crates/rt-engine/src/vtpane.rs`

**Interfaces produced:**
- `vt_term::Term::history_bytes(&self) -> usize` — a pure read of the running estimate the engine already maintains.
- `rt_engine::budget::{GLOBAL_SCROLLBACK_BUDGET, PANE_FLOOR, register, rebalance, total_usage}`.

**The policy, precisely:**

1. Sum `history_bytes` over live panes → `total`.
2. If `total <= GLOBAL_SCROLLBACK_BUDGET`: every pane's byte cap is set generously (the existing per-pane 1 GiB), so nothing evicts and the line cap governs as it does today. **This is the normal case and must cost nothing observable.**
3. Otherwise, for each pane: `cap_i = max(PANE_FLOOR, GLOBAL * usage_i / total)`. Apply with `set_scrollback(existing_lines, cap_i)`.

Note the feedback loop and do not try to defeat it: a pane's share follows its usage, so a newly busy pane starts at the floor and grows its share as it fills. That converges, and it is the behaviour the proportional choice asked for.

- [ ] **Step 1: Write the failing tests**

In `crates/rt-engine/src/budget.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    /// A pane-shaped Term with `n` bytes of history already accumulated.
    /// Build it by feeding real output, not by poking fields — the point is to
    /// exercise the same accounting the engine keeps in production.
    fn term_with_history(cols: usize, rows: usize, lines: usize) -> std::sync::Arc<std::sync::Mutex<vt_term::Term>> {
        let mut t = vt_term::Term::new(cols, rows);
        t.set_scrollback(1_000_000, usize::MAX);
        for i in 0..lines {
            t.feed(format!("line{i} {}\r\n", "x".repeat(cols / 2)).as_bytes());
        }
        std::sync::Arc::new(std::sync::Mutex::new(t))
    }

    #[test]
    fn history_bytes_grows_with_real_output() {
        let t = term_with_history(80, 10, 500);
        let used = t.lock().unwrap().history_bytes();
        assert!(used > 0, "500 scrolled lines must account for something");
    }

    #[test]
    fn under_budget_nothing_is_squeezed() {
        // The normal case: total below the global budget leaves every pane on
        // the generous per-pane cap, so the line cap governs exactly as before.
        let a = term_with_history(80, 10, 200);
        let b = term_with_history(80, 10, 200);
        register(&a); register(&b);
        let before = (a.lock().unwrap().history_bytes(), b.lock().unwrap().history_bytes());
        rebalance(usize::MAX); // an unreachable budget
        let after = (a.lock().unwrap().history_bytes(), b.lock().unwrap().history_bytes());
        assert_eq!(before, after, "nothing may be evicted while under budget");
    }

    #[test]
    fn over_budget_the_total_comes_down_under_the_cap() {
        let a = term_with_history(80, 10, 4000);
        let b = term_with_history(80, 10, 4000);
        register(&a); register(&b);
        let total = total_usage();
        let budget = total / 2;
        rebalance(budget);
        assert!(total_usage() <= budget, "total {} still over budget {budget}", total_usage());
    }

    #[test]
    fn the_busy_pane_keeps_more_than_the_quiet_one() {
        // The whole point of proportional: a pane with a big log keeps its
        // history while an idle neighbour holds little.
        let busy = term_with_history(80, 10, 8000);
        let quiet = term_with_history(80, 10, 200);
        register(&busy); register(&quiet);
        let budget = total_usage() / 2;
        rebalance(budget);
        let (bu, qu) = (busy.lock().unwrap().history_bytes(), quiet.lock().unwrap().history_bytes());
        assert!(bu > qu, "busy {bu} should keep more than quiet {qu}");
    }

    #[test]
    fn a_quiet_pane_is_never_starved_to_nothing() {
        let busy = term_with_history(80, 10, 20000);
        let quiet = term_with_history(80, 10, 50);
        register(&busy); register(&quiet);
        rebalance(PANE_FLOOR * 2); // brutally tight
        assert!(quiet.lock().unwrap().history_bytes() > 0 || PANE_FLOOR == 0,
                "the floor must leave a quiet pane something");
    }

    #[test]
    fn dropped_panes_are_pruned_and_do_not_leak_slots() {
        let live = term_with_history(80, 10, 100);
        register(&live);
        {
            let temp = term_with_history(80, 10, 100);
            register(&temp);
        } // temp dropped here
        rebalance(usize::MAX);
        assert_eq!(live_panes(), 1, "the dropped pane's slot must be reclaimed");
    }
}
```

Adapt names to what you actually build (`live_panes()` is a test-visible count; expose it however fits). The ASSERTIONS are the requirement.

- [ ] **Step 2: Run to verify they fail.** `cargo test -p rt-engine budget`

- [ ] **Step 3: Implement.** Add `Term::history_bytes()` to `crates/vt-term/src/state.rs` beside the other read-only accessors. Build the registry in `budget.rs`; have `VtPane::spawn_env` register its `Arc<Mutex<Term>>`. Keep `SCROLLBACK_MEMORY_BUDGET` as the generous per-pane cap used when under budget.

Pick `GLOBAL_SCROLLBACK_BUDGET` and `PANE_FLOOR` and DOCUMENT the reasoning at the constants, including what they mean at 1, 10 and 60 panes. State in the comment that a quiet pane's history can shrink when a busy one grows — that is inherent to a shared budget and will occasionally look like a bug.

- [ ] **Step 4: Run to verify they pass**, then `cargo test -p vt-term -p rt-engine`.

- [ ] **Step 5: Commit.**

---

### Task 2: Call it from the host, and prove it under load

**Files:** `crates/rt/src/main.rs`, `crates/rt-engine/tests/budget.rs`

- [ ] **Step 1:** An integration test spawning many real panes, filling them, and asserting the process-wide total stays under the budget while a busy pane still holds more than an idle one.
- [ ] **Step 2:** Call `rebalance` from rt's frame loop on a timer (about once a second — this is not per-frame work). Rebalancing must never block the frame: skip a pane whose lock is contended and catch it next pass.
- [ ] **Step 3:** Confirm no measurable frame-time cost with 60 panes.
- [ ] **Step 4:** Commit, and update `project-map.js` if a node's status changes.

## Definition of done

- Total scrollback across all panes is bounded regardless of pane count.
- Under budget, behaviour is byte-identical to today.
- A busy pane keeps more history than an idle one; no pane is starved to zero.
- Dropped panes leak no registry slots.
- `cargo test -p vt-term -p rt-engine` green; the controller runs the multi-arch gate.
