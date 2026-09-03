//! A child that writes and exits immediately must still have its output land in
//! the grid, even when the render thread is holding the `Term` lock at the moment
//! the PTY reports EOF.
//!
//! Regression test for bytes discarded by the vendored engine's `pty_read`: it
//! reads into a staging buffer, and if the terminal lock is contended it loops
//! back to read AGAIN before parsing. The next read on a PTY whose child has
//! exited returns EIO, and the error path returned without ever parsing the bytes
//! it had already taken out of the kernel -- so a fast `printf` vanished.
//!
//! Two things this test learned the hard way (2026-09-03):
//!
//! 1. It must NOT inherit the ambient `RT_ENGINE`. The first version called
//!    `TermPane::spawn`, so in a shell exporting `RT_ENGINE=vtterm` it silently
//!    exercised the in-house engine and never touched the vendored code it was
//!    written to guard. Each engine is now spawned explicitly.
//!
//! 2. It must distinguish DISCARDED bytes from merely LATE ones. Hammering
//!    `snapshot()` from four threads starves the reader on a non-fair mutex, so a
//!    fixed deadline measures scheduling luck, not correctness -- the in-house
//!    engine "failed" that way while provably losing nothing. So we contend for a
//!    bounded window, then STOP contending and give the bytes time to arrive.
//!    Data the old bug threw away is gone forever and can never arrive; data that
//!    was merely delayed shows up as soon as the lock frees.

use rt_engine::{AlacPane, TermPane, DEFAULT_SCROLLBACK};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

const MARKER: &str = "hello-rt-9137";
const ATTEMPTS: usize = 40;
/// How long the lock stays contended after spawn. The child writes and exits in
/// microseconds; this window only has to cover the reader reaching the dead-PTY
/// read while the lock is busy, which is where the discard happened.
const CONTEND: Duration = Duration::from_millis(300);
/// Once contention stops, how long the bytes have to appear. Deliberately
/// generous: this is not a latency assertion, it only has to outlast a starved
/// reader catching up.
const QUIESCE: Duration = Duration::from_secs(5);

fn shell() -> Option<(String, Vec<String>)> {
    Some(("/bin/sh".to_string(), vec!["-c".to_string(), format!("printf '{MARKER}'")]))
}

fn budget() -> Arc<rt_engine::budget::Budget> {
    Arc::new(rt_engine::budget::Budget::default())
}

/// Count attempts whose output never arrived AT ALL, for one explicitly chosen engine.
fn lost_attempts(spawn: impl Fn() -> TermPane) -> usize {
    let mut lost = 0;
    for _ in 0..ATTEMPTS {
        let pane = Arc::new(spawn());

        // Contend the terminal lock while the child writes, exits, and the reader
        // hits EOF/EIO. Several hammers, not one: a single thread leaves gaps
        // between acquisitions.
        let stop = Arc::new(AtomicBool::new(false));
        let hammers: Vec<_> = (0..4)
            .map(|_| {
                let pane = Arc::clone(&pane);
                let stop = Arc::clone(&stop);
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        std::hint::black_box(pane.snapshot());
                    }
                })
            })
            .collect();
        std::thread::sleep(CONTEND);
        stop.store(true, Ordering::Relaxed);
        for h in hammers {
            h.join().unwrap();
        }

        // Uncontended: anything still missing was thrown away, not delayed.
        let deadline = Instant::now() + QUIESCE;
        let mut seen = false;
        while Instant::now() < deadline {
            if pane.snapshot().to_text().contains(MARKER) {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        if !seen {
            lost += 1;
        }
    }
    lost
}

#[test]
fn vendored_engine_keeps_fast_exiting_child_output() {
    let lost = lost_attempts(|| {
        TermPane::Alac(
            AlacPane::spawn_env(shell(), None, 80, 24, &[], DEFAULT_SCROLLBACK)
                .expect("vendored pane spawns"),
        )
    });
    assert_eq!(lost, 0, "{lost}/{ATTEMPTS} fast-exiting children lost their output (vendored engine)");
}

#[test]
fn in_house_engine_keeps_fast_exiting_child_output() {
    let lost = lost_attempts(|| {
        TermPane::spawn_vt_env(shell(), None, 80, 24, &[], DEFAULT_SCROLLBACK, &budget())
            .expect("in-house pane spawns")
    });
    assert_eq!(lost, 0, "{lost}/{ATTEMPTS} fast-exiting children lost their output (in-house engine)");
}
