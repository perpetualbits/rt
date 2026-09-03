//! A child that writes and exits immediately must still have its output land in
//! the grid, even when the render thread is holding the `Term` lock at the moment
//! the PTY reports EOF.
//!
//! Regression test for bytes discarded by `pty_read`: it reads into a staging
//! buffer, and if the terminal lock is contended it loops back to read AGAIN
//! before parsing. The next read on a PTY whose child has exited returns EIO,
//! and the error path returned without ever parsing the bytes it had already
//! taken out of the kernel -- so a fast `printf` vanished.

use rt_engine::TermPane;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn budget() -> Arc<rt_engine::budget::Budget> {
    Arc::new(rt_engine::budget::Budget::default())
}

/// Spawn a write-then-exit child while HAMMERING `snapshot()` from another
/// thread. The hammering is the point: it keeps the `Term` lock busy so the
/// reader hits the contended path that used to drop the data.
#[test]
fn fast_exiting_child_output_survives_lock_contention() {
    let marker = "hello-rt-9137";
    let mut lost = 0;
    let attempts = 40;

    for _ in 0..attempts {
        let pane = Arc::new(
            TermPane::spawn(
                Some(("/bin/sh".to_string(), vec!["-c".to_string(), format!("printf '{marker}'")])),
                None,
                80,
                24,
                &budget(),
            )
            .expect("pane spawns"),
        );

        // Contend the terminal lock for the whole life of the child.
        // Several hammers, not one: the reader only loses data when the lock is
        // held at the instant it retries, and a single thread leaves gaps between
        // acquisitions. On a slow board `snapshot()` alone is enough; on a fast
        // one it takes a crowd to hold the lock a comparable fraction of the time.
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

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = false;
        while Instant::now() < deadline {
            if pane.snapshot().to_text().contains(marker) {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        stop.store(true, Ordering::Relaxed);
        for h in hammers {
            h.join().unwrap();
        }
        if !seen {
            lost += 1;
        }
    }

    assert_eq!(lost, 0, "{lost}/{attempts} fast-exiting children lost their output");
}
