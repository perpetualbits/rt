//! Lifecycle/teardown soak: spawn a real shell-backed pane and immediately drop
//! it, many times, asserting that resources are actually released.
//!
//! `TermPane::drop` sends `Shutdown` and joins the I/O thread, then the PTY
//! closes. If any of that regressed — a leaked master fd, an unjoined reader
//! thread holding its pipe — the process's open-fd count would climb across
//! iterations. Normal tests spawn once; this hammers the destroy path, which is
//! exactly where the close-time races hide (cf. the window-close teardown bug).

use rt_engine::TermPane;
use std::sync::Arc;

/// Count this process's open file descriptors (Linux `/proc/self/fd`).
fn open_fds() -> usize {
    std::fs::read_dir("/proc/self/fd").map(|d| d.count()).unwrap_or(0)
}

#[test]
fn soak_spawn_drop_releases_fds() {
    // A shell that exits immediately keeps each iteration cheap; we're exercising
    // spawn + Drop, not the shell.
    let shell = Some(("/bin/sh".to_string(), vec!["-c".to_string(), "exit 0".to_string()]));
    // One budget for the whole soak run, shared across every iteration below — this
    // test is a single-threaded stand-in for one host's process lifetime (spawn,
    // drop, repeat), which is exactly what one long-lived `Arc<Budget>` models.
    let budget = Arc::new(rt_engine::budget::Budget::default());

    // Warm up once so first-time allocations (thread-locals, etc.) don't count
    // against the baseline.
    drop(TermPane::spawn(shell.clone(), None, 80, 24, &budget).expect("initial spawn"));

    let before = open_fds();
    for i in 0..40 {
        let pane = TermPane::spawn(shell.clone(), None, 80, 24, &budget).expect("spawn");
        drop(pane); // Shutdown → join the I/O thread → close the PTY
        let now = open_fds();
        // A little slack for transient fds; what we're guarding against is
        // *unbounded* growth, i.e. one leaked fd per iteration.
        assert!(
            now <= before + 8,
            "fd count climbing (leak) by iter {i}: {before} -> {now}"
        );
    }
    let after = open_fds();
    assert!(after <= before + 8, "net fd leak across the soak: {before} -> {after}");
}
