//! Integration tests for the layout tree. These run headless (no display, no
//! PTY) and are the safety net that lets us refactor the tree with confidence.

use rt_core::{Direction, Orientation, Rect, Tree};

/// A convenient full-window rectangle used by the geometry-dependent tests.
fn window() -> Rect {
    Rect::new(0.0, 0.0, 1000.0, 800.0) // 1000x800 logical pixels
}

#[test]
fn starts_with_one_pane() {
    // A new tree must contain exactly the single pane it reported.
    let (tree, first) = Tree::new();
    let rects = tree.rects(window());
    assert_eq!(rects.len(), 1); // only one visible pane
    assert_eq!(rects[0].0, first); // and it is the id we were handed
    // That pane fills the whole window.
    assert_eq!(rects[0].1, window());
}

#[test]
fn split_left_right_divides_width() {
    // Splitting left/right should produce two panes sharing the width, minus
    // the divider gutter, at equal heights.
    let (mut tree, first) = Tree::new();
    let second = tree.split(first, Orientation::LeftRight).expect("split ok");
    let rects = tree.rects(window());
    assert_eq!(rects.len(), 2); // two visible panes now
    let a = rects.iter().find(|(id, _)| *id == first).unwrap().1;
    let b = rects.iter().find(|(id, _)| *id == second).unwrap().1;
    // Both keep full height.
    assert_eq!(a.h, 800.0);
    assert_eq!(b.h, 800.0);
    // Widths are equal and sum (with the 6px gutter) to the window width.
    assert!((a.w - b.w).abs() < 0.001); // equal split
    assert!((a.w + b.w + 6.0 - 1000.0).abs() < 0.001); // gutter accounted for
    // The original pane is on the left, the new one on the right.
    assert!(a.x < b.x);
}

#[test]
fn split_top_bottom_divides_height() {
    // The vertical analogue of the previous test.
    let (mut tree, first) = Tree::new();
    let second = tree.split(first, Orientation::TopBottom).unwrap();
    let rects = tree.rects(window());
    let a = rects.iter().find(|(id, _)| *id == first).unwrap().1;
    let b = rects.iter().find(|(id, _)| *id == second).unwrap().1;
    assert_eq!(a.w, 1000.0); // full width retained
    assert!((a.h + b.h + 6.0 - 800.0).abs() < 0.001); // heights + gutter = window
    assert!(a.y < b.y); // original on top
}

#[test]
fn split_unknown_pane_returns_none() {
    // Passing a stale/foreign id must not panic — it returns None (rt's
    // no-crash policy; contrast Terminator's unguarded lookups).
    let (mut tree, _first) = Tree::new();
    assert!(tree.split(rt_core::PaneId(999), Orientation::LeftRight).is_none());
}

#[test]
fn close_collapses_split() {
    // After splitting then closing one side, the survivor should fill the whole
    // window again (the redundant split node collapses away).
    let (mut tree, first) = Tree::new();
    let second = tree.split(first, Orientation::LeftRight).unwrap();
    assert!(tree.close(second)); // close the right pane
    let rects = tree.rects(window());
    assert_eq!(rects.len(), 1); // back to one pane
    assert_eq!(rects[0].0, first); // and it's the survivor
    assert_eq!(rects[0].1, window()); // filling everything
}

#[test]
fn close_last_pane_empties_tree() {
    // Closing the final pane marks the tree empty so the window layer knows to
    // close the window.
    let (mut tree, first) = Tree::new();
    assert!(!tree.is_empty()); // starts non-empty
    assert!(tree.close(first)); // close the only pane
    assert!(tree.is_empty()); // now empty
    assert_eq!(tree.rects(window()).len(), 0); // nothing to draw
}

#[test]
fn nested_splits_and_close() {
    // Build a 3-pane layout (split, then split one child), then close the
    // middle and confirm the tree stays consistent and panic-free.
    let (mut tree, a) = Tree::new();
    let b = tree.split(a, Orientation::LeftRight).unwrap(); // a | b
    let c = tree.split(b, Orientation::TopBottom).unwrap(); // a | (b / c)
    assert_eq!(tree.rects(window()).len(), 3); // three visible panes
    assert!(tree.close(b)); // remove b; the b/c split collapses to c
    let ids: Vec<_> = tree.rects(window()).into_iter().map(|(id, _)| id).collect();
    assert_eq!(ids.len(), 2); // a and c remain
    assert!(ids.contains(&a) && ids.contains(&c)); // exactly those two
}

#[test]
fn tabs_hide_inactive_pages() {
    // A new tab makes a Tabs node; only the active page is laid out/visible.
    let (mut tree, a) = Tree::new();
    let b = tree.new_tab(a).unwrap(); // a and b as tabs, b active
    let rects = tree.rects(window());
    assert_eq!(rects.len(), 1); // only the active tab is visible
    assert_eq!(rects[0].0, b); // and new_tab focuses the new page
    // Both panes still exist in the full enumeration though.
    let all = tree.all_panes();
    assert!(all.contains(&a) && all.contains(&b));
}

#[test]
fn directional_navigation_is_spatial() {
    // Layout: a | b horizontally. From a, Right → b; from b, Left → a; and
    // vertical moves find nothing (there is no pane above/below).
    let (mut tree, a) = Tree::new();
    let b = tree.split(a, Orientation::LeftRight).unwrap();
    assert_eq!(tree.neighbor(a, Direction::Right, window()), Some(b));
    assert_eq!(tree.neighbor(b, Direction::Left, window()), Some(a));
    assert_eq!(tree.neighbor(a, Direction::Up, window()), None); // window edge
    assert_eq!(tree.neighbor(a, Direction::Left, window()), None); // leftmost pane
}

#[test]
fn rotate_flips_the_enclosing_split() {
    // a|b side by side; rotating flips the parent split so they stack instead.
    let (mut tree, a) = Tree::new();
    let b = tree.split(a, Orientation::LeftRight).unwrap();
    // Before: a is left of b (same row).
    let before = tree.rects(window());
    let ra = before.iter().find(|(id, _)| *id == a).unwrap().1;
    let rb = before.iter().find(|(id, _)| *id == b).unwrap().1;
    assert!(ra.x < rb.x && (ra.y - rb.y).abs() < 0.001, "expected side-by-side to start");
    // Rotate the split containing a.
    assert!(tree.rotate(a));
    let after = tree.rects(window());
    let ra = after.iter().find(|(id, _)| *id == a).unwrap().1;
    let rb = after.iter().find(|(id, _)| *id == b).unwrap().1;
    // Now they stack: a above b, sharing the x column.
    assert!(ra.y < rb.y && (ra.x - rb.x).abs() < 0.001, "expected stacked after rotate");
    // Rotating a lone pane does nothing (no parent split).
    let (mut solo, s) = Tree::new();
    assert!(!solo.rotate(s));
}

#[test]
fn resize_grows_the_focused_pane() {
    // a|b at 50/50; growing a to the right must widen a and narrow b.
    let (mut tree, a) = Tree::new();
    let b = tree.split(a, Orientation::LeftRight).unwrap();
    let w0 = tree.rects(window()).iter().find(|(id, _)| *id == a).unwrap().1.w;
    assert!(tree.resize(a, Direction::Right, 0.1)); // push the boundary right
    let rects = tree.rects(window());
    let ra = rects.iter().find(|(id, _)| *id == a).unwrap().1;
    let rb = rects.iter().find(|(id, _)| *id == b).unwrap().1;
    assert!(ra.w > w0, "a should be wider after growing right");
    assert!(ra.w > rb.w, "a should now be wider than b");
    // Resizing along an axis with no matching split is a no-op (a|b has no
    // top/bottom split, so a vertical resize changes nothing).
    assert!(!tree.resize(a, Direction::Up, 0.1));
}

#[test]
fn divider_drag_resizes_binary_split() {
    // Split left|right (50/50), grab the divider, and resize to ~25/75.
    let (mut tree, a) = Tree::new();
    let b = tree.split(a, Orientation::LeftRight).unwrap();
    let bounds = Rect::new(0.0, 0.0, 1000.0, 800.0);
    // The divider sits near the middle (~497 for 1000px minus the 6px gutter).
    let h = tree.divider_at(497.0, 400.0, bounds).expect("divider found at the split boundary");
    assert!(h.horizontal); // a left/right split drags along x
    tree.set_split_ratio(&h.path, 0.25); // left pane -> ~25%
    let rects = tree.rects(bounds);
    let ra = rects.iter().find(|(id, _)| *id == a).unwrap().1;
    let rb = rects.iter().find(|(id, _)| *id == b).unwrap().1;
    assert!(ra.w < rb.w); // left is now narrower
    let frac = ra.w / (ra.w + rb.w);
    assert!((frac - 0.25).abs() < 0.05, "left fraction was {frac}");
    // A point in open pane area yields no divider handle.
    assert!(tree.divider_at(100.0, 400.0, bounds).is_none());
}
