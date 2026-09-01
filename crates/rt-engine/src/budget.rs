//! Process-wide scrollback memory budget, shared **proportionally** across live panes.
//!
//! `vtpane.rs`'s per-pane byte cap (`SCROLLBACK_MEMORY_BUDGET`, 1 GiB) bounds one pane; it
//! does not bound the process. At 60 panes that's a 60 GiB ceiling with nothing above it —
//! it has never actually bitten only because the user's line cap reaches its own limit
//! first, at a far smaller byte count. Raise the line slider and nothing catches it.
//!
//! This module adds the missing ceiling: [`Budget`], an owned value holding a registry of
//! every live pane's `Term` (held `Weak`, so a dropped pane leaks no slot) plus the two
//! policy constants. A periodic [`Budget::rebalance`] sums every pane's
//! [`vt_term::Term::history_bytes`] and, only once the total exceeds the budget, tightens
//! each pane's byte cap in proportion to how much of that total it holds — never equally.
//! `rebalance` does none of the eviction itself: it just moves each pane's cap via the
//! existing [`vt_term::Term::set_scrollback`], whose `trim_history` already evicts
//! oldest-first under that pane's own lock and keeps `history_bytes` in step. Reusing that
//! is deliberate — this module's whole job is bookkeeping across panes, not per-pane
//! eviction logic, which already exists and is already tested.
//!
//! **Ownership, deliberately not a global.** `Budget` is a plain value a host constructs
//! and hands to every pane it spawns (`VtPane::spawn_env` takes `&Arc<Budget>`) — there is
//! no `static` anywhere in this module. "Process-wide" is a property of the host creating
//! exactly one `Arc<Budget>` and sharing it, not of the type system forcing a single
//! instance to exist. That keeps every test able to build its own `Budget` with zero shared
//! state (no serializing lock needed between tests), and leaves room for a host that wants
//! a different scope — per-window, say — without this module standing in the way.
//!
//! **Locking discipline:** `rebalance` and `total_usage` each visit panes one at a time —
//! never two `Term` locks held at once — and use `try_lock` throughout, so a pane whose
//! lock its reader thread currently holds is simply skipped for this pass rather than
//! blocked on. This runs on the GUI thread (via a ~1s timer, added in Task 2); a lock skip
//! costs nothing but staleness until the next pass, which is always imminent.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError, Weak};

use vt_term::Term;

/// Generous per-pane byte cap used while the process is under budget. This is the same
/// value `vtpane.rs` used before this module existed (moved here so the "normal case" cap
/// and the coordinator that may tighten it below this live beside each other); its meaning
/// is unchanged: 1 GiB "bounds the worst case" **for one pane**. It's `GLOBAL_SCROLLBACK_
/// BUDGET` below that bounds the worst case across every pane at once — this constant is
/// only ever *tightened*, never loosened, by `rebalance`.
pub(crate) const SCROLLBACK_MEMORY_BUDGET: usize = 1 << 30;

/// The process-wide scrollback ceiling, shared **proportionally** (not equally) across
/// every live pane. One busy pane holding a huge build log should keep its history while
/// dozens of idle panes hold almost nothing — equal division would make a deep-scrollback
/// setting meaningless at exactly the pane counts where memory matters.
///
/// Chosen as 4x the single-pane generous cap (`SCROLLBACK_MEMORY_BUDGET`, 1 GiB): a hard
/// multiple of "one busy pane's worth", not "one busy pane's worth times however many
/// panes happen to be open" (which is what today's uncapped per-pane budget amounts to).
///
/// What it means in practice, by pane count:
/// - **1 pane:** never binds. A lone pane is already capped at `SCROLLBACK_MEMORY_BUDGET`
///   (1 GiB), which is below this 4 GiB budget, so `rebalance` always finds a single pane
///   under budget and leaves its cap alone — behaviour identical to before this module
///   existed.
/// - **10 panes:** `PANE_FLOOR` reserves `10 * PANE_FLOOR` = 20 MiB as an unconditional
///   floor; the rest is shared by usage, so one pane running a build log can still claim
///   most of the ~4 GiB while nine idle neighbours hold a couple MiB each.
/// - **60 panes:** the same 4 GiB now has to cover what used to be an unbounded 60 GiB
///   worst case. The floor alone reserves `60 * PANE_FLOOR` = 120 MiB, guaranteed to every
///   pane regardless of what its neighbours are doing; a single busy pane among 59 idle
///   ones can still grow to nearly the ~3.9 GiB remaining, but 60 simultaneously busy
///   panes divide it far more thinly than they would with no process-wide cap at all.
///
/// **The trade this accepts, and it IS a trade, not a bug:** because the budget is
/// shared, a quiet pane's history can shrink when a busy neighbour grows — not because of
/// anything the quiet pane's user did, purely because another pane's share of a fixed pool
/// grew. That's inherent to sharing a budget at all; the alternative (equal division)
/// defeats a deep scrollback setting at exactly the pane counts where memory matters, which
/// is why the user chose proportional anyway. It will occasionally look like a bug —
/// scrollback that was there yesterday is gone today with no eviction the user asked for.
/// It's the accepted cost of bounding the process instead of each pane in isolation.
pub const GLOBAL_SCROLLBACK_BUDGET: usize = 4 * (1 << 30); // 4 GiB

/// The minimum byte cap `rebalance` will ever assign a live pane, however far over budget
/// the process is. Without a floor, strict proportionality could push a quiet pane's share
/// toward zero purely because a neighbour is loud — "has some history" to "has none" with
/// the quiet pane's user having done nothing. The floor makes that impossible: every live
/// pane keeps at least this many bytes of scrollback no matter how busy its neighbours are.
///
/// 2 MiB holds several thousand lines of typical terminal output (a scrollback `Line` runs
/// from a couple hundred bytes to a couple KB depending on pane width) — enough to scroll
/// back through recent output even on the most starved pane in the room. At the pane
/// counts this module exists for, the floor's total cost stays a small slice of the
/// budget: 10 panes reserve 20 MiB of the 4 GiB budget; 60 panes reserve 120 MiB — both
/// well under `GLOBAL_SCROLLBACK_BUDGET`, leaving the rest to be shared by actual usage.
pub const PANE_FLOOR: usize = 2 << 20; // 2 MiB

/// An owned, process-wide-*by-convention* scrollback budget coordinator. A host constructs
/// exactly one (typically `Arc::new(Budget::default())`), hands `&Arc<Budget>` to every
/// pane it spawns (see `VtPane::spawn_env`), and calls [`rebalance`](Budget::rebalance) or
/// [`rebalance_default`](Budget::rebalance_default) periodically (Task 2: a ~1s timer on
/// the GUI thread). Nothing here reaches for a `static` — sharing is the host's choice, made
/// by sharing the `Arc`, not something this type imposes.
pub struct Budget {
    /// Every registered pane's `Term`, held weakly: registering never keeps a pane alive,
    /// and a dropped pane's slot is reclaimed (not merely ignored) the next time anything
    /// here walks the registry — see `prune_and_upgrade`.
    registry: Mutex<Vec<Weak<Mutex<Term>>>>,
    /// This instance's process-wide ceiling — normally `GLOBAL_SCROLLBACK_BUDGET`, but a
    /// test (or a future non-default host) may pick any value.
    global_budget: usize,
    /// This instance's per-pane floor — normally `PANE_FLOOR`.
    pane_floor: usize,
    /// Set once `rebalance` has tightened at least one pane's cap below the generous
    /// default, cleared once a later call has restored every pane back to it. Lets
    /// `rebalance` skip its apply pass ENTIRELY in the steady state where the process has
    /// never gone over budget (or has fully recovered from doing so) — the common case
    /// under a periodic timer, where touching every pane's lock for a guaranteed no-op
    /// would just be contention against every reader thread for nothing.
    squeezed: AtomicBool,
}

impl Default for Budget {
    /// A `Budget` using the real production constants — what a host actually wants.
    fn default() -> Self {
        Self::new(GLOBAL_SCROLLBACK_BUDGET, PANE_FLOOR)
    }
}

impl Budget {
    /// Build a `Budget` against an explicit process budget and per-pane floor. Production
    /// callers should prefer `Budget::default()`; this exists so tests (and any future
    /// caller with a reason to differ) can exercise the policy against any numbers without
    /// touching the constants.
    pub fn new(global_budget: usize, pane_floor: usize) -> Self {
        Budget {
            registry: Mutex::new(Vec::new()),
            global_budget,
            pane_floor,
            squeezed: AtomicBool::new(false),
        }
    }

    /// Register a pane's `Term` with this coordinator. Call once per pane, right after
    /// it's created (`VtPane::spawn_env` does this with the `&Arc<Budget>` it's given).
    /// Cheap and idempotent-ish: it always appends, so registering the same `Arc` twice
    /// double-counts it — callers must register each pane exactly once.
    pub fn register(&self, term: &Arc<Mutex<Term>>) {
        self.registry.lock().unwrap().push(Arc::downgrade(term));
    }

    /// Count of currently-live registered panes. Walking it also prunes dead slots as a
    /// side effect (see `prune_and_upgrade`), so this is also how a dropped pane's slot
    /// gets reclaimed if nothing else has rebalanced since it dropped. Exposed mainly so
    /// tests can assert a dropped pane doesn't leak a slot forever.
    pub fn live_panes(&self) -> usize {
        self.prune_and_upgrade().len()
    }

    /// Drop every registry slot whose pane no longer exists (`Weak::upgrade` failing is
    /// the only signal needed — no separate liveness bookkeeping), and return the
    /// survivors upgraded to strong references for this call's use. Holds the registry
    /// lock only long enough to filter the `Vec`; never a `Term` lock.
    fn prune_and_upgrade(&self) -> Vec<Arc<Mutex<Term>>> {
        let mut reg = self.registry.lock().unwrap();
        let mut live = Vec::with_capacity(reg.len());
        reg.retain(|weak| match weak.upgrade() {
            Some(strong) => {
                live.push(strong);
                true
            }
            None => false, // the pane is gone; its slot is not kept around
        });
        live
    }

    /// Sum of `history_bytes` over every live pane this pass could read without blocking.
    /// A pane whose lock is contended at this instant is simply left out of this sum —
    /// it's re-read next call, never blocked on.
    pub fn total_usage(&self) -> usize {
        self.prune_and_upgrade()
            .iter()
            .filter_map(|t| try_with_term(t, |term| term.history_bytes()))
            .sum()
    }

    /// The core policy (see the module doc): sum every live pane's usage. If it's within
    /// `budget`, the normal case, leave every pane on the generous per-pane cap — and if
    /// nothing has been squeezed since the last time this was true, don't even touch a
    /// pane's lock to re-assert it; there's nothing to do. Otherwise tighten each pane's
    /// byte cap to its proportional share of `budget`, floored at this `Budget`'s
    /// `pane_floor`, and apply it via `set_scrollback` (which evicts, via its own
    /// `trim_history`, entirely under that pane's own lock).
    ///
    /// Two passes when there's anything to apply, never two `Term` locks held at once: the
    /// first reads usage from each live pane in turn; the second applies the cap computed
    /// from that snapshot, one pane at a time. A pane whose lock is contended in either
    /// pass is skipped for that pass — its contribution to `total`, or its new cap, is
    /// simply stale until the next call (on the host's ~1s timer in Task 2, that's never
    /// long).
    ///
    /// Note the feedback loop and don't try to defeat it: a newly busy pane starts at the
    /// floor and its share grows only as its usage grows, converging pane-by-pane across
    /// successive calls rather than needing one instant of perfect foresight.
    ///
    /// Takes `budget` explicitly (rather than reading this `Budget`'s own `global_budget`)
    /// so it's directly testable against any budget a test wants to exercise; production
    /// callers use [`rebalance_default`](Self::rebalance_default), the thin wrapper that
    /// supplies this instance's own constant.
    pub fn rebalance(&self, budget: usize) {
        // Always prune first: a dropped pane's slot must be reclaimed on every pass,
        // whether or not anything below needs applying.
        let panes = self.prune_and_upgrade();

        // Pass 1: read usage. Locks one pane at a time; never blocks.
        let usages: Vec<(Arc<Mutex<Term>>, usize)> = panes
            .into_iter()
            .filter_map(|t| {
                let usage = try_with_term(&t, |term| term.history_bytes())?;
                Some((t, usage))
            })
            .collect();

        let total: usize = usages.iter().map(|(_, usage)| *usage).sum();

        if total <= budget {
            if !self.squeezed.load(Ordering::SeqCst) {
                // Steady state: every pane is already sitting on `SCROLLBACK_MEMORY_
                // BUDGET` from spawn (or from a previous restore below) and nothing has
                // squeezed anything since. Skip the apply pass entirely — no lock touched.
                return;
            }
            // Something WAS squeezed on an earlier call; the process has since come back
            // under budget (a busy pane's output slowed, or a pane closed). Restore every
            // pane to the generous cap once, then clear the flag so later calls
            // short-circuit again above.
            for (term, _usage) in &usages {
                try_with_term(term, |t| {
                    let lines = t.scrollback_lines(); // preserve the line cap — only the
                    t.set_scrollback(lines, SCROLLBACK_MEMORY_BUDGET); // byte cap moves.
                });
            }
            self.squeezed.store(false, Ordering::SeqCst);
            return;
        }

        // Over budget: tighten. Pass 2, locking one pane at a time; never blocks.
        for (term, usage) in &usages {
            // usage * budget can overflow a usize well before either operand is huge;
            // widen to u128 for the multiply, matching `total`'s own scale.
            let share = (*usage as u128 * budget as u128 / (total.max(1)) as u128) as usize;
            let cap = share.max(self.pane_floor);
            try_with_term(term, |t| {
                let lines = t.scrollback_lines();
                t.set_scrollback(lines, cap);
            });
        }
        self.squeezed.store(true, Ordering::SeqCst);
    }

    /// Rebalance against this instance's own process-wide budget (`GLOBAL_SCROLLBACK_
    /// BUDGET` for `Budget::default()`) — what a host's periodic timer actually calls.
    /// [`rebalance`](Self::rebalance) itself takes the budget as a parameter so tests can
    /// exercise the policy against any budget without needing a second `Budget` instance.
    pub fn rebalance_default(&self) {
        self.rebalance(self.global_budget);
    }
}

/// Read or mutate one pane's `Term` without ever blocking the caller: `WouldBlock` (the
/// pane's reader thread is mid-feed) simply returns `None` rather than waiting — the
/// caller is expected to catch a skipped pane on the next periodic pass rather than stall
/// the GUI thread behind a busy pane. A poisoned lock (something else panicked while
/// holding it) is still read/written via `into_inner` rather than propagating the panic
/// here: this module's job is bookkeeping, not correctness-critical state, so one
/// crashed/poisoned pane must not stop every other pane's rebalance.
fn try_with_term<T>(term: &Arc<Mutex<Term>>, f: impl FnOnce(&mut Term) -> T) -> Option<T> {
    match term.try_lock() {
        Ok(mut guard) => Some(f(&mut guard)),
        Err(TryLockError::Poisoned(poisoned)) => Some(f(&mut poisoned.into_inner())),
        Err(TryLockError::WouldBlock) => None,
    }
}

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
        // Each test builds its own `Budget` — no shared state, no serializing lock.
        let budget = Budget::default();
        let a = term_with_history(80, 10, 200);
        let b = term_with_history(80, 10, 200);
        budget.register(&a);
        budget.register(&b);
        let before = (a.lock().unwrap().history_bytes(), b.lock().unwrap().history_bytes());
        budget.rebalance(usize::MAX); // an unreachable budget
        let after = (a.lock().unwrap().history_bytes(), b.lock().unwrap().history_bytes());
        assert_eq!(before, after, "nothing may be evicted while under budget");
    }

    #[test]
    fn over_budget_the_total_comes_down_under_the_cap() {
        let coordinator = Budget::default();
        let a = term_with_history(80, 10, 4000);
        let b = term_with_history(80, 10, 4000);
        coordinator.register(&a);
        coordinator.register(&b);
        let total = coordinator.total_usage();
        let cap = total / 2;
        coordinator.rebalance(cap);
        assert!(
            coordinator.total_usage() <= cap,
            "total {} still over budget {cap}",
            coordinator.total_usage()
        );
    }

    #[test]
    fn the_busy_pane_keeps_more_than_the_quiet_one() {
        // The whole point of proportional: a pane with a big log keeps its
        // history while an idle neighbour holds little.
        let coordinator = Budget::default();
        let busy = term_with_history(80, 10, 8000);
        let quiet = term_with_history(80, 10, 200);
        coordinator.register(&busy);
        coordinator.register(&quiet);
        let cap = coordinator.total_usage() / 2;
        coordinator.rebalance(cap);
        let (bu, qu) = (busy.lock().unwrap().history_bytes(), quiet.lock().unwrap().history_bytes());
        assert!(bu > qu, "busy {bu} should keep more than quiet {qu}");
    }

    #[test]
    fn a_quiet_pane_is_never_starved_to_nothing() {
        let coordinator = Budget::default();
        let busy = term_with_history(80, 10, 20000);
        let quiet = term_with_history(80, 10, 50);
        coordinator.register(&busy);
        coordinator.register(&quiet);
        coordinator.rebalance(PANE_FLOOR * 2); // brutally tight
        assert!(quiet.lock().unwrap().history_bytes() > 0 || PANE_FLOOR == 0,
                "the floor must leave a quiet pane something");
    }

    #[test]
    fn dropped_panes_are_pruned_and_do_not_leak_slots() {
        let coordinator = Budget::default();
        let live = term_with_history(80, 10, 100);
        coordinator.register(&live);
        {
            let temp = term_with_history(80, 10, 100);
            coordinator.register(&temp);
        } // temp dropped here
        coordinator.rebalance(usize::MAX);
        assert_eq!(coordinator.live_panes(), 1, "the dropped pane's slot must be reclaimed");
    }

    #[test]
    fn a_pane_squeezed_earlier_is_restored_once_the_process_is_back_under_budget() {
        // The apply-pass short-circuit (skip entirely when under budget) must not skip the
        // ONE pass needed to undo a previous squeeze — otherwise a pane tightened while the
        // process was briefly over budget would stay tightened forever, even long after
        // its neighbour's usage (or its own) drops back down. That would violate "under
        // budget, behaviour is byte-identical to today." Eviction is one-way (evicted lines
        // don't come back), so the only way to observe whether the BYTE CAP itself was
        // really loosened back up is to feed more output afterward and see whether it's
        // allowed to grow past what the tightened cap would have allowed.
        let coordinator = Budget::default();
        let busy = term_with_history(80, 10, 8000);
        let quiet = term_with_history(80, 10, 200);
        coordinator.register(&busy);
        coordinator.register(&quiet);

        // Force a squeeze tight enough that `quiet` is pinned at the floor.
        coordinator.rebalance(PANE_FLOOR * 2);
        let squeezed_quiet = quiet.lock().unwrap().history_bytes();
        assert!(squeezed_quiet <= PANE_FLOOR * 2, "test setup: quiet should have been squeezed");

        // Now rebalance against a budget nothing could exceed; the squeeze must lift.
        coordinator.rebalance(usize::MAX);

        // Feed far more than the squeezed cap ever allowed. If the byte cap is still
        // pinned near the floor, this stays capped near `squeezed_quiet`; if it was
        // correctly restored to the generous per-pane cap, it grows well past it.
        {
            let mut t = quiet.lock().unwrap();
            for i in 0..8000 {
                t.feed(format!("more{i} {}\r\n", "x".repeat(40)).as_bytes());
            }
        }
        let grown_quiet = quiet.lock().unwrap().history_bytes();
        assert!(
            grown_quiet > squeezed_quiet * 4,
            "quiet pane's cap must be restored after the process is back under budget \
             (squeezed at {squeezed_quiet}, only grew to {grown_quiet} after feeding much more)"
        );
    }
}
