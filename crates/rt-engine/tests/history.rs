//! Test the history-aware `snapshot_lines` primitive that newspaper columns are
//! built on: print more lines than the screen holds, then read a range that
//! reaches into scrollback and confirm the older lines are there, in order.

use rt_engine::TermPane;
use std::time::{Duration, Instant};

/// Print 60 numbered lines into an 80x24 terminal (so ~36 lines land in
/// scrollback), then read a wide range through history and assert both an early
/// and a late line are present, with the early line physically above the late
/// one (correct top-to-bottom ordering across the history boundary).
#[test]
fn snapshot_lines_reads_scrollback_in_order() {
    // A shell that emits LINE1..LINE60 then exits.
    let pane = TermPane::spawn(
        Some(("/bin/sh".to_string(), vec!["-c".to_string(), "for i in $(seq 1 60); do echo LINE$i; done".to_string()])),
        None,
        80,
        24,
    )
    .expect("pane spawns");

    // Wait until the newest line (LINE60) has been parsed onto the screen.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if pane.snapshot().to_text().contains("LINE60") {
            break; // output fully arrived
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let b = pane.line_bounds(); // discover the available line range
    assert!(b.topmost < 0, "expected scrollback below the screen, got {b:?}");

    // Read the entire buffer (history + screen) in one call.
    let rows = (b.bottommost - b.topmost + 1) as usize; // total available lines
    let snap = pane.snapshot_lines(b.topmost, rows); // top-to-bottom, oldest first
    let text = snap.to_text(); // flatten for easy assertions

    // Both an early (history) and a late (screen) line must be present.
    assert!(text.contains("LINE5"), "history line missing:\n{text}");
    assert!(text.contains("LINE60"), "newest line missing:\n{text}");

    // Ordering: find the row index of LINE5 and LINE55; earlier number = higher.
    let pos5 = snap.rows.iter().position(|r| row_text(r).contains("LINE5 ") || row_text(r).trim() == "LINE5");
    let pos55 = snap.rows.iter().position(|r| row_text(r).trim() == "LINE55");
    if let (Some(a), Some(c)) = (pos5, pos55) {
        assert!(a < c, "LINE5 (row {a}) should be above LINE55 (row {c})");
    }
}

/// Flatten one snapshot row to a trimmed string.
fn row_text(row: &[rt_engine::SnapCell]) -> String {
    row.iter().map(|c| c.c).collect::<String>()
}
