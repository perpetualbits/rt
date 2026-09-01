//! Export a live pane driven by a real shell through a real PTY.
//!
//! The unit tests in `handoff.rs` feed a `Term` directly. This one goes through
//! the whole path — fork, pty, reader thread, parser, grid — and is what would
//! catch a break between them.

use std::time::{Duration, Instant};

use rt_engine::TermPane;

/// Spawn a shell running `script`, then poll until `probe` sees what it wants
/// or the deadline passes. Draining events is what a real host does each frame.
fn pane_running(script: &str, cols: usize, rows: usize, probe: impl Fn(&TermPane) -> bool) -> TermPane {
    let pane = TermPane::spawn_vt_env(
        Some(("/bin/sh".into(), vec!["-c".into(), script.into()])),
        None,
        cols,
        rows,
        &[],
        10_000,
    )
    .expect("spawn a pane");

    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(20));
        let _ = pane.drain_events();
        if probe(&pane) {
            return pane;
        }
    }
    panic!("the pane never produced the expected output within 10s");
}

/// Reconstruct ANY wire line's text, INCLUDING blanks.
///
/// A run's `text` is empty when the run is blank (`grid.rs`: "Empty means
/// blanks") — that is how the format keeps a mostly-empty line cheap. Naively
/// joining `run.text` across a line's runs (as the brief's sample code did)
/// therefore drops every blank run's columns entirely: "hello from a real
/// pty" round-trips as "hellofromarealpty", because each inter-word space is
/// its own blank run with `text == ""`. Blanks are always narrow, so
/// `cell_span` spaces reconstructs them exactly.
///
/// Takes the `Line`, not a screen and a row: scrollback lines and the
/// held-aside grid have exactly the same hazard, and concatenating raw
/// `run.text` there happened to work only because those fixtures start at
/// column 0 with no interior blanks. One fixture with a leading indent or two
/// spaces in it and those assertions would have compared the wrong string.
fn line_text(line: &rt_handoff::grid::Line) -> String {
    let mut s = String::new();
    for run in &line.runs {
        if run.text.is_empty() {
            s.extend(std::iter::repeat(' ').take(run.cell_span as usize));
        } else {
            s.push_str(&run.text);
        }
    }
    s
}

/// Whether `want` appears anywhere in the pane's currently rendered text.
///
/// The brief's original probes checked for a single leading character
/// (`has_char(p, 'h')`, `has_char(p, 'P')`, `has_char(p, '4')`, ...). That
/// races against the reader thread: a probe on the FIRST character of a
/// multi-character write can fire before the rest of the write has been
/// applied, and for the scrollback test it is worse than a race — `'4'` is
/// satisfied by `line4`, `line14`, ... long before the `for` loop reaches
/// `line40`, so the pane could be exported with far less scrollback than the
/// test assumes. Matching the whole expected substring makes the probe track
/// what the test actually depends on.
fn has_text(pane: &TermPane, want: &str) -> bool {
    pane.snapshot().to_text().contains(want)
}

/// Whether every one of `wants` has arrived somewhere in the pane's rendered
/// text. Used instead of `has_text` for wide glyphs: the RENDER snapshot pads
/// each wide glyph's second (spacer) column with a literal space
/// (`日 本 語`), so a wide multi-glyph string never appears as one contiguous
/// substring in `to_text()` even though the WIRE text (which drops spacer
/// cells) is contiguous. Checking each glyph's presence still rules out the
/// single-character race `has_char` had.
fn has_all_chars(pane: &TermPane, wants: &[char]) -> bool {
    let text = pane.snapshot().to_text();
    wants.iter().all(|c| text.contains(*c))
}

#[test]
fn a_real_shells_output_survives_export_and_the_wire() {
    let pane = pane_running(
        "printf 'hello from a real pty'; sleep 30",
        60,
        8,
        |p| has_text(p, "hello from a real pty"),
    );
    let (wire, _scroll) = pane.export(1, 0).expect("in-house engine exports");

    assert!(line_text(&wire.screen_primary.lines[0]).starts_with("hello from a real pty"));

    // And the whole thing survives a round trip through the frozen format.
    let back = rt_handoff::pane::PaneWire::decode(&wire.encode()).expect("decode");
    assert_eq!(back.screen_primary, wire.screen_primary);
    assert_eq!(back.cols, 60);
    assert_eq!(back.rows, 8);
}

#[test]
fn colours_written_by_a_real_program_stay_indexed() {
    // The property the whole format hinges on: an indexed colour must not be
    // resolved to RGB on the way out, or a moved pane stops following the
    // receiving window's palette and the index can never be recovered.
    let pane = pane_running(
        "printf '\\033[38;5;200mPINK\\033[m'; sleep 30",
        20,
        4,
        |p| has_text(p, "PINK"),
    );
    let (wire, _) = pane.export(1, 0).unwrap();
    let run = wire.screen_primary.lines[0]
        .runs
        .iter()
        .find(|r| r.text.starts_with("PINK"))
        .expect("the coloured run");
    assert_eq!(
        wire.style_table[run.style_id as usize].fg,
        rt_handoff::style::Colour::Indexed(200)
    );
}

#[test]
fn scrollback_from_a_real_program_comes_back_newest_first() {
    let pane = pane_running(
        "for i in $(seq 1 40); do echo line$i; done; sleep 30",
        20,
        5,
        |p| has_text(p, "line40"),
    );
    let (_, scroll) = pane.export(1, 100).unwrap();
    assert!(!scroll.is_empty(), "40 lines through a 5-row screen leaves history");

    let first = line_text(&scroll[0]);
    let last = line_text(&scroll[scroll.len() - 1]);
    let n = |s: &str| -> usize { s.trim().trim_start_matches("line").parse().unwrap_or(0) };
    assert!(n(&first) > n(&last), "newest first: {first:?} must precede {last:?}");
}

#[test]
fn an_alt_screen_program_exports_both_screens() {
    // `\033[?1049h` saves the cursor and switches to (and clears) the alt
    // screen, but it does NOT reset the cursor's column — real terminal
    // behaviour, not a vt-term quirk. After `BENEATH` the cursor sits at
    // column 7, so without the `\r` here "ONTOP" lands at column 7 on the alt
    // screen, not column 0, and `starts_with` below fails. The brief's sample
    // script omitted the `\r` and asserted the column-0 case regardless.
    let pane = pane_running(
        "printf 'BENEATH'; printf '\\033[?1049h'; printf '\\rONTOP'; sleep 30",
        20,
        4,
        |p| has_text(p, "ONTOP"),
    );
    let (wire, _) = pane.export(1, 0).unwrap();
    assert_eq!(wire.active_screen, 1, "the alt screen is showing");
    assert!(
        line_text(&wire.screen_primary.lines[0]).starts_with("ONTOP"),
        "primary grid = what is displayed"
    );
    let alt = wire.screen_alt.as_ref().expect("the held-aside screen rides along");
    let beneath = line_text(&alt.lines[0]);
    assert!(beneath.starts_with("BENEATH"), "got {beneath:?}");
}

#[test]
fn an_alt_screen_pane_still_carries_its_scrollback() {
    // The end-to-end version of the trap: a real program scrolls a real pty,
    // then switches to the alt screen (vim, less, htop all do). vt-term's
    // `history_size()` reports 0 from there, so an export bounded by the
    // VIEWPORT sends an empty scrollback and the receiver silently loses every
    // line — no error, no wire invariant violated.
    let pane = pane_running(
        "for i in $(seq 1 40); do echo line$i; done; printf '\\033[?1049h'; printf 'ONTOP'; sleep 30",
        20,
        4,
        |p| has_text(p, "ONTOP"),
    );
    let (wire, scroll) = pane.export(1, 100).unwrap();
    assert_eq!(wire.active_screen, 1, "the alt screen is showing");
    assert!(!scroll.is_empty(), "the primary's history must ride along from the alt screen");
    let newest = line_text(&scroll[0]);
    assert!(newest.trim().starts_with("line"), "got {newest:?}");
}

#[test]
fn a_wide_glyph_from_a_real_program_spans_two_columns() {
    let pane = pane_running("printf '日本語'; sleep 30", 20, 4, |p| has_all_chars(p, &['日', '本', '語']));
    let (wire, _) = pane.export(1, 0).unwrap();
    let run = &wire.screen_primary.lines[0].runs[0];
    assert_eq!(run.flags & rt_handoff::grid::run_flags::WIDE, rt_handoff::grid::run_flags::WIDE);
    assert_eq!(run.text, "日本語");
    assert_eq!(run.cell_span, 6);
}
