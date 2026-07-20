//! The Phase-1 battery, run with the vendored engine in the system-under-test slot.
//! It is green today AND already exercises real properties (determinism, chunk-
//! invariance, spec conformance, invariants). When `vt-term` lands, the same tests
//! run with it as the SUT and diff it against the vendored oracle.

use vt_conformance::vendored::Vendored;
use vt_conformance::{attr, feed_chunks, feed_whole, gen_script, split, NCursor, VtEngine};

const COLS: usize = 80;
const ROWS: usize = 24;

/// Feeding a fixed script produces the same state every time (the harness and
/// `observe()` are deterministic — the precondition for everything else).
#[test]
fn vendored_is_deterministic() {
    let script = gen_script(1234, 400);
    let a = feed_whole::<Vendored>(COLS, ROWS, &script);
    let b = feed_whole::<Vendored>(COLS, ROWS, &script);
    assert!(a.diff(&b).is_none(), "same input, different state: {:?}", a.diff(&b));
}

/// Chunk-invariance: the SAME bytes fed whole vs. split into arbitrary small chunks
/// must yield identical state. This is a genuine correctness property (the parser
/// must resume across read boundaries) that any engine — vendored or ours — must
/// satisfy, so the harness earns its keep before the in-house engine exists.
#[test]
fn chunk_invariance_fuzz() {
    for seed in 0..2_000u64 {
        let script = gen_script(seed, 200);
        let whole = feed_whole::<Vendored>(COLS, ROWS, &script);
        let chunks = split(&script, seed);
        let chunked = feed_chunks::<Vendored>(COLS, ROWS, &chunks);
        if let Some(d) = whole.diff(&chunked) {
            panic!("chunk-invariance broke on seed {seed}: {d}\n--- whole ---\n{}", whole.to_text());
        }
    }
}

/// Hand-written conformance cases — the seed of an esctest-style suite, and readable
/// documentation of expected behaviour. Each asserts observed state after a sequence.
#[test]
fn conformance_basic_sequences() {
    // Erase-screen + home: screen blank, cursor at origin.
    let s = feed_whole::<Vendored>(COLS, ROWS, b"hello\r\nworld\x1b[2J\x1b[H");
    assert_eq!(s.cursor, Some(NCursor { col: 0, line: 0, shape: 0, visible: true }));
    assert!(s.grid.iter().all(|row| row.iter().all(|c| c.ch == ' ')), "2J should blank the screen");

    // SGR bold applies to the next printed cell.
    let s = feed_whole::<Vendored>(COLS, ROWS, b"\x1b[1mX");
    assert_eq!(s.grid[0][0].ch, 'X');
    assert!(s.grid[0][0].attrs & attr::BOLD != 0, "\\e[1m should set BOLD");

    // Autowrap: printing past the right edge wraps to the next row.
    let s = feed_whole::<Vendored>(5, ROWS, b"ABCDEFG");
    assert_eq!(row_text(&s, 0), "ABCDE");
    assert_eq!(row_text(&s, 1), "FG");

    // Absolute cursor position (CUP is 1-based; we observe 0-based).
    let s = feed_whole::<Vendored>(COLS, ROWS, b"\x1b[3;4H");
    assert_eq!(s.cursor, Some(NCursor { col: 3, line: 2, shape: 0, visible: true }));

    // Erase-line (2K) clears the whole line; the cursor does not move.
    let s = feed_whole::<Vendored>(COLS, ROWS, b"ABC\x1b[2K");
    assert_eq!(row_text(&s, 0), "");
    assert_eq!(s.cursor.map(|c| c.col), Some(3));

    // Alternate-screen toggle is observable.
    let on = feed_whole::<Vendored>(COLS, ROWS, b"\x1b[?1049h");
    assert!(on.alt_screen, "?1049h enters the alt screen");
    let off = feed_whole::<Vendored>(COLS, ROWS, b"\x1b[?1049h\x1b[?1049l");
    assert!(!off.alt_screen, "?1049l leaves the alt screen");

    // Application-cursor-keys mode (DECCKM) is observable.
    let app = feed_whole::<Vendored>(COLS, ROWS, b"\x1b[?1h");
    assert!(app.app_cursor, "?1h sets application cursor keys");
}

/// Invariants that must hold for ANY input, checked over a fuzz sweep.
#[test]
fn property_invariants_hold_under_fuzz() {
    for seed in 0..2_000u64 {
        let script = gen_script(seed.wrapping_mul(2_654_435_761), 200);
        let s = feed_whole::<Vendored>(COLS, ROWS, &script);
        assert_eq!(s.grid.len(), ROWS, "grid row count == rows (seed {seed})");
        assert!(s.grid.iter().all(|r| r.len() == COLS), "every row has cols cells (seed {seed})");
        if let Some(cur) = s.cursor {
            assert!(cur.col < COLS && cur.line < ROWS, "cursor in bounds (seed {seed}): {cur:?}");
        }
        assert!(s.display_offset <= s.history, "display_offset <= history (seed {seed})");
    }
}

/// Resize round-trip does not corrupt observable dimensions.
#[test]
fn resize_updates_dimensions() {
    let mut e = Vendored::spawn(COLS, ROWS);
    e.feed(b"the quick brown fox\r\njumps over the lazy dog");
    e.resize(40, 12);
    let s = e.observe();
    assert_eq!((s.cols, s.rows), (40, 12));
    assert_eq!(s.grid.len(), 12);
    assert!(s.grid.iter().all(|r| r.len() == 40));
}

fn row_text(s: &vt_conformance::ScreenState, row: usize) -> String {
    s.grid[row].iter().map(|c| c.ch).collect::<String>().trim_end().to_string()
}
