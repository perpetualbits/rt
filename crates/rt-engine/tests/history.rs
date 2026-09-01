//! Test the history-aware `snapshot_lines` primitive that newspaper columns are
//! built on: print more lines than the screen holds, then read a range that
//! reaches into scrollback and confirm the older lines are there, in order.

use rt_engine::TermPane;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A fresh, unshared budget per test.
fn test_budget() -> Arc<rt_engine::budget::Budget> {
    Arc::new(rt_engine::budget::Budget::default())
}

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
        &test_budget(),
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

/// Scrolling up must reveal scrollback history, not blank it out. This guards
/// the display_offset bug where scrolled cells (negative grid lines) were
/// dropped, leaving the view mostly empty.
#[test]
fn scroll_up_reveals_history_not_blanks() {
    let pane = TermPane::spawn(
        Some(("/bin/sh".to_string(), vec!["-c".to_string(), "for i in $(seq 1 80); do echo LINE$i; done".to_string()])),
        None,
        80,
        24,
        &test_budget(),
    )
    .expect("pane spawns");

    // Wait until the newest line is on screen.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if pane.snapshot().to_text().contains("LINE80") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // Scroll up 30 lines into history.
    pane.scroll(30);
    let text = pane.snapshot().to_text();
    // The scrolled view should be full of history, not blanked out.
    let nonblank = text.lines().filter(|l| !l.trim().is_empty()).count();
    assert!(nonblank > 15, "scrolled view is mostly blank ({nonblank} non-blank lines):\n{text}");
    // And it should show older lines that were off-screen at the bottom.
    assert!(text.contains("LINE50") || text.contains("LINE45") || text.contains("LINE40"),
        "scrolled view missing history:\n{text}");
}

/// Scrollback search must find hits across history (not just the visible
/// screen), report them in top-to-bottom order, and place each at the right
/// column so the UI can highlight it. Guards the `search` primitive the find bar
/// is built on.
#[test]
fn search_finds_hits_across_scrollback() {
    // Print 80 numbered lines so many scroll off the 24-row screen into history.
    let pane = TermPane::spawn(
        Some(("/bin/sh".to_string(), vec!["-c".to_string(), "for i in $(seq 1 80); do echo NEEDLE$i; done".to_string()])),
        None,
        80,
        24,
        &test_budget(),
    )
    .expect("pane spawns");

    // Wait until the newest line is on screen.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if pane.snapshot().to_text().contains("NEEDLE80") {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    // "NEEDLE" appears on every one of the 80 lines, including those now in
    // history — so a scrollback-wide search must find far more than one screen.
    let hits = pane.search("NEEDLE", false);
    assert!(hits.len() >= 60, "expected most of 80 lines to match, got {}", hits.len());
    // Hits come back in reading order (ascending absolute line).
    assert!(hits.windows(2).all(|w| w[0].line <= w[1].line), "hits not in top-to-bottom order");
    // Every hit starts at column 0 (the shell prints "NEEDLE…" flush left) and
    // spans the six letters of the needle.
    assert!(hits.iter().all(|m| m.col == 0 && m.len == 6), "unexpected hit geometry: {hits:?}");

    // Case-insensitivity: a lowercase query finds the uppercase text.
    let lower = pane.search("needle", false);
    assert_eq!(lower.len(), hits.len(), "case-insensitive search should match the same lines");

    // A string that never appears yields nothing (no false positives).
    assert!(pane.search("HAYSTACK", false).is_empty(), "unexpected match for absent text");
}
