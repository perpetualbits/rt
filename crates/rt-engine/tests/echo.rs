//! Headless smoke test for the engine: spawn a shell that prints known text,
//! then confirm the parsed grid actually contains it. This exercises the whole
//! PTY → EventLoop → Term → snapshot path without a display.

use rt_engine::TermPane;
use std::time::{Duration, Instant};

/// Spawn `sh -c "printf ..."`, poll the snapshot until the text appears (or a
/// timeout elapses), and assert we saw it. Polling (rather than a fixed sleep)
/// keeps the test fast when the PTY is quick and robust when the machine is
/// slow — the I/O thread parses asynchronously.
#[test]
fn shell_output_reaches_the_grid() {
    // A distinctive marker unlikely to collide with any shell noise.
    let marker = "hello-rt-9137";
    // Run a non-interactive shell that prints the marker and exits.
    let shell = Some((
        "/bin/sh".to_string(),                                   // POSIX shell, present everywhere
        vec!["-c".to_string(), format!("printf '{marker}'")],     // print marker, no newline needed
    ));
    // 80x24 is the classic default terminal size; plenty for our marker.
    let pane = TermPane::spawn(shell, None, 80, 24).expect("pane spawns");

    // Poll for up to 5 seconds for the marker to show up in the grid.
    let deadline = Instant::now() + Duration::from_secs(5); // hard cap
    let mut seen = false; // did we observe the marker?
    while Instant::now() < deadline {
        let text = pane.snapshot().to_text(); // current screen as text
        if text.contains(marker) {
            seen = true; // success: the engine parsed our output
            break;
        }
        std::thread::sleep(Duration::from_millis(20)); // brief backoff between polls
    }
    assert!(seen, "engine grid never showed the marker text");
}

/// Writing input to the shell should round-trip: send `echo` output and read it
/// back from the grid. Confirms the write() path (host → PTY) works too.
#[test]
fn input_round_trips_through_the_shell() {
    let marker = "roundtrip-4421";
    // An interactive-ish shell reading commands from its PTY stdin.
    let pane = TermPane::spawn(
        Some(("/bin/sh".to_string(), vec![])), // bare shell, reads stdin
        None,
        80,
        24,
    )
    .expect("pane spawns");

    // Give the shell a moment to start before typing at it.
    std::thread::sleep(Duration::from_millis(200));
    // Type: echo <marker><Enter>. The shell echoes the command AND its output,
    // so the marker will appear on the grid regardless.
    pane.write(format!("echo {marker}\n").as_bytes());

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = false;
    while Instant::now() < deadline {
        if pane.snapshot().to_text().contains(marker) {
            seen = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(seen, "typed command never appeared on the grid");
}
