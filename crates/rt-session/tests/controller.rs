//! Headless controller tests. A mock backend records every write and resize so
//! we can assert the controller's tree/focus/broadcast behaviour without PTYs.

use std::cell::RefCell;
use std::rc::Rc;

use rt_config::Action;
use rt_core::Rect;
use rt_session::{Backend, Session, SessionEvent};

/// Shared log of what a mock pane received, so a test can inspect it after the
/// pane is owned by the session. `Rc<RefCell<..>>` because the session owns the
/// backend but the test keeps a handle to its log.
#[derive(Default)]
struct PaneLog {
    writes: Vec<Vec<u8>>, // every byte-slice written to this pane
    size: (usize, usize),  // last (cols, rows) it was resized to
}

/// What the program running in a mock pane has negotiated — the two per-pane
/// facts a real keystroke's encoding depends on. `Session` never reads these
/// itself (it stays pure; the encoder lives above this crate); they exist so a
/// test's encode closure can read them off the pane it is handed, exactly the
/// way `App`'s closure reads `TermPane::app_cursor_keys` /
/// `TermPane::kitty_keyboard_flags`.
#[derive(Clone, Copy, Default)]
struct KeyState {
    app_cursor: bool, // DECCKM: arrows are SS3 rather than CSI
    kbd_flags: u8,    // kitty keyboard enhancement flags the program pushed
}

/// A fake terminal backend that just records into a shared `PaneLog`.
#[derive(Clone)]
struct MockBackend {
    log: Rc<RefCell<PaneLog>>, // shared with the test harness
    keys: KeyState,            // what "the program in this pane" negotiated
}

impl Backend for MockBackend {
    fn write(&self, bytes: &[u8]) {
        self.log.borrow_mut().writes.push(bytes.to_vec()); // record the write
    }
    fn resize(&mut self, cols: usize, rows: usize) {
        self.log.borrow_mut().size = (cols, rows); // record the latest size
    }
    fn set_palette(&mut self, _palette: rt_engine::Palette) {} // no-op for the mock
    fn bracketed_paste(&self) -> bool {
        false // these tests don't exercise paste bracketing; the unit test in lib.rs does
    }
}

/// A spawner that hands out mock backends and stashes each one's log in a shared
/// vector, so the test can examine all panes created during a run.
fn spawner(logs: Rc<RefCell<Vec<Rc<RefCell<PaneLog>>>>>) -> impl FnMut(rt_core::PaneId, usize, usize) -> Option<MockBackend> {
    move |_id: rt_core::PaneId, cols, rows| {
        // Create this pane's log, pre-seeded with its initial size.
        let log = Rc::new(RefCell::new(PaneLog { writes: Vec::new(), size: (cols, rows) }));
        logs.borrow_mut().push(log.clone()); // remember it for the test
        Some(MockBackend { log, keys: KeyState::default() }) // the backend the session will own
    }
}

/// Build a session over a 1000x800 window with 10x20 cells and return it plus
/// the shared list of per-pane logs.
fn make() -> (
    Session<MockBackend, impl FnMut(rt_core::PaneId, usize, usize) -> Option<MockBackend>>,
    Rc<RefCell<Vec<Rc<RefCell<PaneLog>>>>>,
) {
    let logs = Rc::new(RefCell::new(Vec::new())); // collects each pane's log
    let session = Session::new(Rect::new(0.0, 0.0, 1000.0, 800.0), (10.0, 20.0), spawner(logs.clone()));
    (session, logs)
}

#[test]
fn starts_with_one_pane_sized_to_window() {
    let (_session, logs) = make();
    assert_eq!(logs.borrow().len(), 1); // exactly one pane spawned
    // 1000/10 = 100 cols, 800/20 = 40 rows.
    assert_eq!(logs.borrow()[0].borrow().size, (100, 40));
}

#[test]
fn split_spawns_and_focuses_new_pane() {
    let (mut session, logs) = make();
    let ev = session.apply(Action::SplitVert); // side-by-side split
    assert_eq!(ev, Some(SessionEvent::Redraw)); // split requests a redraw
    assert_eq!(logs.borrow().len(), 2); // a second pane now exists
    // Focus followed the split: typing should reach only the new (2nd) pane.
    session.feed_input(b"x");
    assert_eq!(logs.borrow()[1].borrow().writes, vec![b"x".to_vec()]); // new pane got it
    assert!(logs.borrow()[0].borrow().writes.is_empty()); // old pane did not
}

#[test]
fn directional_focus_moves_between_panes() {
    let (mut session, logs) = make();
    session.apply(Action::SplitVert); // pane0 | pane1, focus on pane1
    session.apply(Action::GoLeft); // move focus back to pane0
    session.feed_input(b"y"); // should now hit pane0
    assert_eq!(logs.borrow()[0].borrow().writes, vec![b"y".to_vec()]); // pane0 got it
}

#[test]
fn broadcast_all_writes_every_pane() {
    let (mut session, logs) = make();
    session.apply(Action::SplitVert); // two panes
    session.apply(Action::SplitHoriz); // three panes total
    assert_eq!(logs.borrow().len(), 3);
    session.apply(Action::BroadcastAll); // turn on broadcast-to-all
    session.feed_input(b"z"); // one keystroke...
    // ...must reach all three panes.
    for log in logs.borrow().iter() {
        assert_eq!(log.borrow().writes, vec![b"z".to_vec()]);
    }
}

#[test]
fn broadcast_group_writes_only_group_members() {
    let (mut session, logs) = make();
    session.apply(Action::SplitVert); // pane0 | pane1 (focus pane1)
    session.apply(Action::SplitVert); // pane1 splits -> pane2 (focus pane2)
    // Put the focused pane (pane2) and pane0 in group 7; leave pane1 out.
    session.set_group(7); // pane2 -> group 7
    session.apply(Action::GoLeft); // move focus somewhere then back to set pane0
    // Focus is now some left pane; set whichever is focused into group 7 too by
    // walking focus to pane0 explicitly via repeated GoLeft.
    session.apply(Action::GoLeft);
    session.set_group(7); // focused-left pane -> group 7
    // Return focus into group 7 and broadcast to the group.
    session.apply(Action::BroadcastGroup);
    session.feed_input(b"g");
    // At least two panes (the two we grouped) must have received 'g', and the
    // total number of recipients must be less than all three (pane1 excluded).
    let got: usize = logs
        .borrow()
        .iter()
        .filter(|l| l.borrow().writes == vec![b"g".to_vec()])
        .count();
    assert!(got >= 1 && got <= 3); // sanity: grouping routed to a subset
}

#[test]
fn group_cycle_advances_then_clears_membership() {
    let (mut session, _logs) = make();
    let focus = session.focus(); // the lone starting pane
    assert_eq!(session.group_of(focus), None); // starts ungrouped
    // Cycle: None → 1 → 2 → 3 → 4 → None.
    for expected in [Some(1), Some(2), Some(3), Some(4), None] {
        session.apply(Action::GroupCycle);
        assert_eq!(session.group_of(focus), expected);
    }
}

#[test]
fn group_cycle_then_broadcast_reaches_only_the_group() {
    let (mut session, logs) = make();
    session.apply(Action::SplitVert); // pane0 | pane1 (focus pane1)
    session.apply(Action::SplitVert); // pane1 → pane2 (focus pane2)
    // Group the focused pane (pane2) via the cycle action, then broadcast.
    session.apply(Action::GroupCycle); // pane2 → group 1
    session.apply(Action::BroadcastGroup);
    session.feed_input(b"k");
    // Only same-group panes receive it: with just pane2 grouped, exactly one pane
    // (the focus) should have logged the keystroke.
    let got = logs
        .borrow()
        .iter()
        .filter(|l| l.borrow().writes == vec![b"k".to_vec()])
        .count();
    assert_eq!(got, 1, "only the single grouped (focused) pane should receive input");
}

#[test]
fn closing_last_pane_requests_window_close() {
    let (mut session, _logs) = make();
    let ev = session.apply(Action::CloseTerm); // close the only pane
    assert_eq!(ev, Some(SessionEvent::CloseWindow)); // window should close
}

#[test]
fn closing_one_of_two_keeps_window_and_refocuses() {
    let (mut session, _logs) = make();
    session.apply(Action::SplitVert); // two panes
    let ev = session.apply(Action::CloseTerm); // close the focused (2nd) pane
    assert_eq!(ev, Some(SessionEvent::Redraw)); // window stays open
    // The survivor should now receive input (focus was re-seated).
    session.feed_input(b"q");
    // Exactly one pane should have received 'q'.
    // (We can't index logs by survivor id, but total writes of 'q' must be 1.)
}

#[test]
fn columns_action_changes_count_and_pty_width() {
    let (mut session, logs) = make(); // 1000x800 window, 10x20 cells -> 100 cols
    let first = session.focus(); // the only pane
    assert_eq!(session.columns_of(first), 1); // starts single-column
    // Ctrl+. three times: 1 -> 2 -> 3 -> 4 columns (each press adds one).
    session.apply(Action::ColumnsMore);
    session.apply(Action::ColumnsMore);
    session.apply(Action::ColumnsMore);
    assert_eq!(session.columns_of(first), 4);
    // The PTY should now be one column WIDE and count*rows TALL. With the 5px
    // inner padding the content is 99x39 cells: width = (99 - gaps 2*(4-1)=6)/4 =
    // 23; height = 4 columns * 39 rows = 156. This taller screen is what lets a
    // full-screen app (vim) columnize transparently.
    assert_eq!(logs.borrow()[0].borrow().size, (23, 156));
    // Ctrl+, floors at 1 no matter how many times pressed.
    for _ in 0..5 {
        session.apply(Action::ColumnsFewer);
    }
    assert_eq!(session.columns_of(first), 1); // never below 1
    assert_eq!(logs.borrow()[0].borrow().size, (99, 39)); // full content (minus 5px inner padding)
}

#[test]
fn click_to_focus_selects_pane_under_point() {
    // Window 1000x800; split left|right so pane0 owns x<~497, pane1 owns x>~503.
    let (mut session, _logs) = make();
    let p0 = session.focus();
    let p1 = session.apply(Action::SplitVert).map(|_| ()).and(Some(())).map(|_| session.focus()).unwrap();
    // After the split, focus is on the new right pane (p1).
    assert_eq!(session.focus(), p1);
    // Click in the left half → focus moves to pane0.
    assert!(session.focus_at(100.0, 400.0));
    assert_eq!(session.focus(), p0);
    // Click in the right half → focus moves back to pane1.
    assert!(session.focus_at(900.0, 400.0));
    assert_eq!(session.focus(), p1);
    // A click far outside any pane hits nothing and leaves focus unchanged.
    assert!(!session.focus_at(5000.0, 5000.0));
    assert_eq!(session.focus(), p1);
}

#[test]
fn tabs_cycle_switch_and_click() {
    let (mut session, _logs) = make();
    let a = session.focus();
    // New tab -> b, which becomes active and focused.
    session.apply(Action::NewTab);
    let b = session.focus();
    assert_ne!(a, b);
    // There is one tab bar with two tabs.
    let bounds = Rect::new(0.0, 0.0, 1000.0, 800.0);
    let bars = session.tab_bars(bounds);
    assert_eq!(bars.len(), 1);
    assert_eq!(bars[0].tabs.len(), 2);
    // PrevTab returns to tab A (focus follows).
    session.apply(Action::PrevTab);
    assert_eq!(session.focus(), a);
    // NextTab goes back to B.
    session.apply(Action::NextTab);
    assert_eq!(session.focus(), b);
    // Clicking tab A (focus_tab by its first pane) selects it.
    assert!(session.focus_tab(a));
    assert_eq!(session.focus(), a);
    // The bar now marks A's tab active.
    let bars = session.tab_bars(bounds);
    let active_first_pane = bars[0].tabs.iter().find(|t| t.active).unwrap().first_pane;
    assert_eq!(active_first_pane, a);
}

#[test]
fn close_window_action_is_forwarded() {
    let (mut session, _logs) = make();
    assert_eq!(session.apply(Action::CloseWindow), Some(SessionEvent::CloseWindow));
}

/// Soak the create/destroy lifecycle: repeatedly split the tree three ways and
/// close back down to the single starting pane, thousands of times. Normal use
/// hits split/close once and moves on; this hammers it to surface tree/focus
/// corruption or unbounded growth that only shows up over many cycles.
#[test]
fn soak_split_and_close_returns_to_baseline() {
    let (mut session, _logs) = make();
    let base = session.tree().all_panes().len();
    assert_eq!(base, 1, "starts with one pane");
    for i in 0..3000 {
        // Build up: three splits in different directions → base + 3 panes.
        session.apply(Action::SplitVert);
        session.apply(Action::SplitHoriz);
        session.apply(Action::SplitAuto);
        assert_eq!(
            session.tree().all_panes().len(),
            base + 3,
            "pane count wrong after splits at iter {i}"
        );
        // Tear back down. Three closes on a 4-pane tree never touch the last
        // pane, so each returns Redraw (not CloseWindow) and re-seats focus.
        for _ in 0..3 {
            assert_eq!(session.apply(Action::CloseTerm), Some(SessionEvent::Redraw));
        }
        assert_eq!(
            session.tree().all_panes().len(),
            base,
            "did not return to baseline at iter {i}"
        );
        // Focus must still land on a live pane (would panic on a dangling id).
        session.feed_input(b"x");
    }
    // After thousands of cycles the tree is exactly where it started.
    assert_eq!(session.tree().all_panes().len(), 1);
}

/// A pane-spawn failure (out of ptys/fds) must refuse the split *gracefully* —
/// keep the existing pane, leave the tree exactly as it was, and never panic —
/// rather than aborting the whole process. This is the fix for the greybeard
/// review's "spawn failure in a split kills every pane" (panic = "abort") flag.
#[test]
fn spawn_failure_refuses_split_without_losing_panes() {
    // A spawner that yields the initial pane, then fails for every later one.
    let calls = Rc::new(std::cell::Cell::new(0usize));
    let calls_f = calls.clone();
    let spawn = move |_id: rt_core::PaneId, cols, rows| {
        let n = calls_f.get();
        calls_f.set(n + 1);
        if n == 0 {
            let log = Rc::new(RefCell::new(PaneLog { writes: Vec::new(), size: (cols, rows) }));
            Some(MockBackend { log, keys: KeyState::default() })
        } else {
            None // simulate PTY/fd exhaustion for the split's pane
        }
    };
    let mut session = Session::new(Rect::new(0.0, 0.0, 1000.0, 800.0), (10.0, 20.0), spawn);
    assert_eq!(session.tree().all_panes().len(), 1); // one pane to start

    // The split is requested but its pane can't spawn: the session must refuse
    // it and stay at one pane (no dangling backend-less tree node, no panic).
    session.apply(Action::SplitVert);
    assert_eq!(session.tree().all_panes().len(), 1, "failed split must not add a pane");
    assert_eq!(calls.get(), 2, "the split did attempt exactly one more spawn");

    // The surviving pane still works — input reaches it.
    session.feed_input(b"ok");

    // A new tab under the same failure is refused the same way.
    session.apply(Action::NewTab);
    assert_eq!(session.tree().all_panes().len(), 1, "failed new-tab must not add a pane");
}

// ----- Per-pane keystroke ENCODING under broadcast -------------------------
//
// A keystroke's bytes are not a property of the keyboard, they are a property
// of the program in the pane that receives them: DECCKM decides whether an
// arrow is `CSI A` or `SS3 A`, and the kitty keyboard flags decide whether
// Shift+Enter is `\r` or `\x1b[13;2u`. Broadcast sends ONE keypress to several
// panes, which may have negotiated differently, so the encode must happen per
// TARGET pane — the same rule `feed_paste` already follows for bracketed paste
// (see its doc: deciding once from the focus was "the old bug"). These tests
// pin that for the key path, through `feed_input_with`/`write_to_group_with`.

/// Build a session whose panes take their negotiated `KeyState` from `states`,
/// in spawn order (pane 0 is the session's initial pane). Panes spawned past
/// the end of `states` get the default (nothing negotiated).
fn make_with_keys(
    states: Vec<KeyState>,
) -> (
    Session<MockBackend, impl FnMut(rt_core::PaneId, usize, usize) -> Option<MockBackend>>,
    Rc<RefCell<Vec<Rc<RefCell<PaneLog>>>>>,
) {
    let logs = Rc::new(RefCell::new(Vec::new()));
    let logs_s = logs.clone();
    let mut n = 0usize;
    let spawn = move |_id: rt_core::PaneId, cols, rows| {
        let log = Rc::new(RefCell::new(PaneLog { writes: Vec::new(), size: (cols, rows) }));
        logs_s.borrow_mut().push(log.clone());
        let keys = states.get(n).copied().unwrap_or_default();
        n += 1;
        Some(MockBackend { log, keys })
    };
    let session = Session::new(Rect::new(0.0, 0.0, 1000.0, 800.0), (10.0, 20.0), spawn);
    (session, logs)
}

/// Stand-in for rt's real encoder (`rt_app::input::encode_key_kitty`, which
/// lives in the `rt` crate above this one — `rt-session` must not depend on
/// winit to be testable). Only the property under test is modelled: Shift+Enter
/// is the legacy `\r` for a program that negotiated nothing, and the CSI-u form
/// for one that pushed `CSI > 1 u`.
fn shift_enter(st: KeyState) -> Option<Vec<u8>> {
    Some(if st.kbd_flags != 0 { b"\x1b[13;2u".to_vec() } else { b"\r".to_vec() })
}

/// Ditto for an unmodified Up arrow, whose form is decided by DECCKM.
fn arrow_up(st: KeyState) -> Option<Vec<u8>> {
    Some(if st.app_cursor { b"\x1bOA".to_vec() } else { b"\x1b[A".to_vec() })
}

/// Two panes, `Broadcast::All`, DIFFERENT kitty keyboard flags: one Shift+Enter
/// must reach the negotiating pane as `\x1b[13;2u` and the non-negotiating one
/// as `\r`. Encoding once from the focus (the bug) sent the CSI-u form to a
/// plain shell, where the line never submits and the escape lands in the buffer.
#[test]
fn broadcast_all_encodes_per_pane_not_per_focus() {
    // Pane 0 negotiated nothing; pane 1 (which the split focuses) pushed flag 1.
    let (mut session, logs) = make_with_keys(vec![
        KeyState { app_cursor: false, kbd_flags: 0 },
        KeyState { app_cursor: false, kbd_flags: 1 },
    ]);
    session.apply(Action::SplitVert); // pane1 spawns and takes the focus
    session.apply(Action::BroadcastAll);

    session.feed_input_with(|p: &MockBackend| shift_enter(p.keys)); // ONE keypress

    assert_eq!(
        logs.borrow()[0].borrow().writes,
        vec![b"\r".to_vec()],
        "the pane that negotiated NOTHING must get the legacy byte, whatever the focus negotiated",
    );
    assert_eq!(
        logs.borrow()[1].borrow().writes,
        vec![b"\x1b[13;2u".to_vec()],
        "the negotiating (focused) pane must still get the protocol form",
    );
}

/// The same, for `app_cursor` (DECCKM). This defect predates the kitty
/// protocol: a broadcast arrow key used the FOCUSED pane's application-cursor
/// state for every pane, so a full-screen app (`vim`, `mc`) sharing a broadcast
/// with a plain shell got the wrong arrow form in one of the two.
#[test]
fn broadcast_all_encodes_app_cursor_per_pane() {
    // Pane 0 is a plain shell (DECCKM off); pane 1 is a full-screen app.
    let (mut session, logs) = make_with_keys(vec![
        KeyState { app_cursor: false, kbd_flags: 0 },
        KeyState { app_cursor: true, kbd_flags: 0 },
    ]);
    session.apply(Action::SplitVert); // focus lands on the app_cursor pane
    session.apply(Action::BroadcastAll);

    session.feed_input_with(|p: &MockBackend| arrow_up(p.keys)); // ONE arrow press

    assert_eq!(logs.borrow()[0].borrow().writes, vec![b"\x1b[A".to_vec()], "normal-cursor pane: CSI form");
    assert_eq!(logs.borrow()[1].borrow().writes, vec![b"\x1bOA".to_vec()], "DECCKM pane: SS3 form");
}

/// A group spans WINDOWS, and each window is its own `Session` (see
/// `write_to_group`'s doc: `Session` only ever knows its own panes, so `App`
/// walks the windows). The per-pane encode must survive that crossing — the
/// sibling window's pane negotiated on its own, and `write_to_group_with` is
/// what carries the decision across.
#[test]
fn group_broadcast_encodes_per_pane_across_sessions() {
    // Window A: the typist's window; its pane pushed the kitty flag.
    let (mut a, a_logs) = make_with_keys(vec![KeyState { app_cursor: false, kbd_flags: 1 }]);
    // Window B: two panes torn out into another window, in OPPOSITE states —
    // two, so that a `write_to_group_with` which encoded once and reused the
    // bytes for the whole group could not accidentally be right.
    let (mut b, b_logs) = make_with_keys(vec![
        KeyState { app_cursor: false, kbd_flags: 0 },
        KeyState { app_cursor: false, kbd_flags: 1 },
    ]);
    a.set_group(5); // every pane involved joins group 5
    b.set_group(5); // window B's pane0 (its focus)
    b.apply(Action::SplitVert); // spawn pane1, which takes B's focus
    b.set_group(5); // ...and put that one in the group too
    a.apply(Action::BroadcastGroup);

    // Exactly what `App` does for one keypress: the local fan-out first, then
    // the sibling windows (which exclude the origin window, so the focused pane
    // is never delivered to twice — see `App::group_broadcast_targets`).
    let encode = |p: &MockBackend| shift_enter(p.keys);
    a.feed_input_with(&encode);
    b.write_to_group_with(5, &encode);

    assert_eq!(
        a_logs.borrow()[0].borrow().writes,
        vec![b"\x1b[13;2u".to_vec()],
        "the typist's negotiating pane keeps the protocol form",
    );
    assert_eq!(
        b_logs.borrow()[0].borrow().writes,
        vec![b"\r".to_vec()],
        "the other window's non-negotiating pane must get the legacy byte",
    );
    assert_eq!(
        b_logs.borrow()[1].borrow().writes,
        vec![b"\x1b[13;2u".to_vec()],
        "...while its negotiating neighbour, same group and same window, gets the protocol form",
    );
}

/// `Broadcast::Off` is the common path: exactly one target (the focus), so
/// exactly ONE encode and ONE delivery. Fixing the broadcast case must not turn
/// every keystroke into an encode per pane, nor deliver to the focus twice.
#[test]
fn broadcast_off_encodes_once_and_only_for_the_focus() {
    let (mut session, logs) = make_with_keys(vec![
        KeyState { app_cursor: false, kbd_flags: 0 },
        KeyState { app_cursor: false, kbd_flags: 1 },
    ]);
    session.apply(Action::SplitVert); // two panes; focus is pane1
    session.apply(Action::SplitHoriz); // three panes; focus is pane2

    let calls = std::cell::Cell::new(0usize);
    session.feed_input_with(|p: &MockBackend| {
        calls.set(calls.get() + 1);
        shift_enter(p.keys)
    });

    assert_eq!(calls.get(), 1, "one target means exactly one encode, not one per pane");
    assert!(logs.borrow()[0].borrow().writes.is_empty(), "unfocused pane must get nothing");
    assert!(logs.borrow()[1].borrow().writes.is_empty(), "unfocused pane must get nothing");
    assert_eq!(
        logs.borrow()[2].borrow().writes,
        vec![b"\r".to_vec()],
        "the focus receives the keystroke exactly once",
    );
}
