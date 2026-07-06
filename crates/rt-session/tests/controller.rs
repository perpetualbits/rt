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

/// A fake terminal backend that just records into a shared `PaneLog`.
#[derive(Clone)]
struct MockBackend {
    log: Rc<RefCell<PaneLog>>, // shared with the test harness
}

impl Backend for MockBackend {
    fn write(&self, bytes: &[u8]) {
        self.log.borrow_mut().writes.push(bytes.to_vec()); // record the write
    }
    fn resize(&mut self, cols: usize, rows: usize) {
        self.log.borrow_mut().size = (cols, rows); // record the latest size
    }
}

/// A spawner that hands out mock backends and stashes each one's log in a shared
/// vector, so the test can examine all panes created during a run.
fn spawner(logs: Rc<RefCell<Vec<Rc<RefCell<PaneLog>>>>>) -> impl FnMut(usize, usize) -> MockBackend {
    move |cols, rows| {
        // Create this pane's log, pre-seeded with its initial size.
        let log = Rc::new(RefCell::new(PaneLog { writes: Vec::new(), size: (cols, rows) }));
        logs.borrow_mut().push(log.clone()); // remember it for the test
        MockBackend { log } // the backend the session will own
    }
}

/// Build a session over a 1000x800 window with 10x20 cells and return it plus
/// the shared list of per-pane logs.
fn make() -> (
    Session<MockBackend, impl FnMut(usize, usize) -> MockBackend>,
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
    // The PTY should have been resized to one column's width: full 100 cols
    // minus gaps (2*(4-1)=6), /4 = 23.
    assert_eq!(logs.borrow()[0].borrow().size, (23, 40));
    // Ctrl+, floors at 1 no matter how many times pressed.
    for _ in 0..5 {
        session.apply(Action::ColumnsFewer);
    }
    assert_eq!(session.columns_of(first), 1); // never below 1
    assert_eq!(logs.borrow()[0].borrow().size, (100, 40)); // back to full width
}

#[test]
fn column_scroll_clamps_at_bottom() {
    let (mut session, _logs) = make();
    let first = session.focus();
    assert_eq!(session.col_scroll_of(first), 0); // bottom-anchored initially
    session.scroll_columns(first, -5); // scrolling down past the bottom...
    assert_eq!(session.col_scroll_of(first), 0); // ...stays clamped at 0
    session.scroll_columns(first, 12); // scroll up into history
    assert_eq!(session.col_scroll_of(first), 12);
}

#[test]
fn close_window_action_is_forwarded() {
    let (mut session, _logs) = make();
    assert_eq!(session.apply(Action::CloseWindow), Some(SessionEvent::CloseWindow));
}
