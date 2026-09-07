//! Pure drop-target resolver for cross-window pane/tab drag-and-drop.
//!
//! Turns a cursor position over a window's content (panes + tab bars +
//! window bounds) into "where would releasing here land the payload", plus
//! the cue rectangle the renderer highlights while hovering. No `Active`, no
//! backend, no winit types — just geometry, so it is unit-testable with
//! synthetic rects (see `mod tests` below) and reusable from any window's
//! event loop once wired up in a later task.
//!
//! Resolution priority (see `resolve_drop`): tab strip first (a caret always
//! wins inside its band, even where a strip lives inside the top edge's
//! `EDGE_STRIP`), then window-edge strips, then pane zones (centre = swap,
//! else nearest edge = split), else `None` for a gutter/dead zone.

/// What is being dragged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DragPayload {
    Pane(rt_core::PaneId),
    Tab { first_pane: rt_core::PaneId },
}

/// A resolved hover: where a release would put the payload, plus the cue
/// rectangle the renderer highlights.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedDrop {
    pub target: rt_session::DropTarget,
    pub cue: rt_core::Rect, // the highlight (half-pane / whole pane / edge band)
    pub caret: bool,        // true → draw as a thin insert caret, not a fill
}

/// Width of the window-edge root-split strips, in LOGICAL pixels: multiplied by
/// the display's backing factor (`resolve_drop`'s `sc`) so the band stays the
/// same apparent width on a Retina panel. Registered in
/// `chrome_scale::logical::EDGE_STRIP`.
pub const EDGE_STRIP: f32 = 24.0;
/// Fraction of a pane's width/height forming the centre (swap) box. A ratio, not
/// a length — deliberately NOT scaled, and not in the chrome register.
pub const CENTRE_FRAC: f32 = 0.4;
/// Width of the tab-insert caret cue, in LOGICAL pixels. The DRAW side
/// (`chrome::dragdrop`, which paints wings on it) takes the same factor.
/// Registered in `chrome_scale::logical::DROP_CARET_W`.
pub const CARET_W: f32 = 3.0;
/// Movement beyond which an armed press becomes a drag, in LOGICAL pixels.
///
/// This one is a GESTURE distance rather than a drawn size, and it still scales.
/// The reason is that both sides of the comparison are physical: winit reports
/// the pointer in physical pixels, so the same 1mm twitch of the same mouse
/// produces twice the delta on a 2x display. Leaving the threshold flat would
/// make rt twice as trigger-happy about starting a pane drag there — the hand
/// has not changed, so the number it is compared against must scale with the
/// units it is measured in. (A threshold expressed in millimetres would be a
/// better model still, but rt has no physical-size information; the backing
/// factor is the closest thing available, and it is exactly right for the common
/// case of a 2x panel at a normal viewing distance.) Registered in
/// `chrome_scale::logical::DRAG_THRESHOLD`.
pub const DRAG_THRESHOLD: f32 = 4.0;

/// Resolve a cursor position over one window into a drop target + cue.
///
/// `payload_panes` lists every pane inside the thing being dragged (so a
/// subtree never drops onto itself); `panes`/`tab_bars` are the target
/// window's current visible geometry; `bounds` is its content area (used for
/// the edge-strip test); `cursor` is in the same coordinate space as all the
/// rects. Never panics — an empty `panes`/`tab_bars` slice, or a zero-size
/// rect, simply fails to match and the function falls through toward `None`.
pub fn resolve_drop(
    payload: DragPayload,
    payload_panes: &[rt_core::PaneId],
    panes: &[(rt_core::PaneId, rt_core::Rect)],
    tab_bars: &[rt_core::TabBar],
    bounds: rt_core::Rect,
    cursor: (f32, f32),
    sc: f32,
) -> Option<ResolvedDrop> {
    let (cx, cy) = cursor;

    // --- 1. Tab strips take priority over everything else. ---
    if let Some(r) = resolve_tab_strip(payload, payload_panes, tab_bars, cx, cy, sc) {
        return Some(r);
    }

    // --- 2. Window-edge strips (root split). ---
    if let Some(r) = resolve_edge_strip(bounds, cx, cy, sc) {
        return Some(r);
    }

    // --- 3. Pane zones: centre swap, else nearest-edge split. ---
    resolve_pane_zone(payload, payload_panes, panes, cx, cy)
}

/// Turn a caret insertion slot into the index [`rt_session::Session::reorder_tab`]
/// wants, for a tab dropped on the strip it already lives in.
///
/// `TabAt.index` counts insertion slots in the CURRENT strip — including the
/// dragged tab's own position. `reorder_tab` moves within a strip the tab has
/// already been removed from conceptually, so every slot to the right of the
/// tab's current index has shifted one place left: subtract one there. Slots at
/// or left of it are unaffected. Dropping onto its own two adjacent slots
/// (`current` and `current + 1`) is the identity, as it should be.
pub fn index_for_reorder(current: usize, insertion: usize) -> usize {
    if insertion > current {
        insertion - 1
    } else {
        insertion
    }
}

fn resolve_tab_strip(
    payload: DragPayload,
    payload_panes: &[rt_core::PaneId],
    tab_bars: &[rt_core::TabBar],
    cx: f32,
    cy: f32,
    sc: f32,
) -> Option<ResolvedDrop> {
    for bar in tab_bars {
        let Some(first) = bar.tabs.first() else {
            continue; // empty bar: nothing to hit-test
        };
        // The strip's vertical band spans the tabs' own y..y+h (all tabs in
        // a bar share the same band; use the first tab's as representative).
        let (top, height) = (first.rect.y, first.rect.h);
        if height <= 0.0 || cy < top || cy >= top + height {
            continue; // cursor not in this bar's vertical band
        }

        let anchor = first.first_pane;

        // Exclusion: a bar whose first tab is part of the dragged subtree is
        // off-limits UNLESS the payload is a Tab belonging to this same bar
        // (in which case it's a reorder within its own strip, which is
        // allowed).
        let is_own_bar = matches!(payload, DragPayload::Tab { first_pane } if first_pane == anchor);
        if !is_own_bar && payload_panes.contains(&anchor) {
            continue;
        }

        // Insertion index = count of tabs whose horizontal midpoint lies
        // left of the cursor.
        let index = bar
            .tabs
            .iter()
            .filter(|t| t.rect.x + t.rect.w * 0.5 < cx)
            .count();

        let caret_x = bar.tabs.get(index).map(|t| t.rect.x).unwrap_or_else(|| {
            // Past the last tab: caret sits just after the last tab's right
            // edge (or at the bar's own left edge if somehow empty, though
            // we already bailed out above for that case).
            bar.tabs.last().map(|t| t.rect.right()).unwrap_or(0.0)
        });

        return Some(ResolvedDrop {
            target: rt_session::DropTarget::TabAt { anchor, index },
            cue: rt_core::Rect::new(caret_x, top, sc * CARET_W, height),
            caret: true,
        });
    }
    None
}

fn resolve_edge_strip(bounds: rt_core::Rect, cx: f32, cy: f32, sc: f32) -> Option<ResolvedDrop> {
    if bounds.w <= 0.0 || bounds.h <= 0.0 {
        return None;
    }
    // Must be within the content bounds at all (a cursor off-window entirely
    // shouldn't hit the edge strip).
    if !bounds.contains(cx, cy) {
        return None;
    }

    let dist_top = cy - bounds.y;
    let dist_bottom = bounds.bottom() - cy;
    let dist_left = cx - bounds.x;
    let dist_right = bounds.right() - cx;

    // Pick the nearest edge, but only if it's within the strip width.
    let min = dist_top.min(dist_bottom).min(dist_left).min(dist_right);
    if min > sc * EDGE_STRIP {
        return None;
    }

    let (orient, before, cue) = if min == dist_top {
        (
            rt_core::Orientation::TopBottom,
            true,
            rt_core::Rect::new(bounds.x, bounds.y, bounds.w, bounds.h * 0.5),
        )
    } else if min == dist_bottom {
        (
            rt_core::Orientation::TopBottom,
            false,
            rt_core::Rect::new(bounds.x, bounds.y + bounds.h * 0.5, bounds.w, bounds.h * 0.5),
        )
    } else if min == dist_left {
        (
            rt_core::Orientation::LeftRight,
            true,
            rt_core::Rect::new(bounds.x, bounds.y, bounds.w * 0.5, bounds.h),
        )
    } else {
        (
            rt_core::Orientation::LeftRight,
            false,
            rt_core::Rect::new(bounds.x + bounds.w * 0.5, bounds.y, bounds.w * 0.5, bounds.h),
        )
    };

    Some(ResolvedDrop {
        target: rt_session::DropTarget::RootEdge { orient, before },
        cue,
        caret: false,
    })
}

fn resolve_pane_zone(
    payload: DragPayload,
    payload_panes: &[rt_core::PaneId],
    panes: &[(rt_core::PaneId, rt_core::Rect)],
    cx: f32,
    cy: f32,
) -> Option<ResolvedDrop> {
    let (pane, rect) = panes.iter().find(|(_, r)| r.contains(cx, cy))?;
    if payload_panes.contains(pane) {
        return None; // never drop a pane onto itself/its own subtree
    }
    if rect.w <= 0.0 || rect.h <= 0.0 {
        return None; // degenerate geometry: nothing sane to resolve
    }

    // Centre (swap) box: CENTRE_FRAC of w/h, centred in the pane.
    let (ccx, ccy) = rect.center();
    let half_w = rect.w * CENTRE_FRAC * 0.5;
    let half_h = rect.h * CENTRE_FRAC * 0.5;
    let in_centre = cx >= ccx - half_w && cx < ccx + half_w && cy >= ccy - half_h && cy < ccy + half_h;

    if in_centre {
        if let DragPayload::Pane(_) = payload {
            return Some(ResolvedDrop {
                target: rt_session::DropTarget::Swap { pane: *pane },
                cue: *rect,
                caret: false,
            });
        }
        // Tab payload: a tab never swaps — fall through to nearest-edge.
    }

    // Nearest edge by normalized distance.
    let dl = (cx - rect.x) / rect.w;
    let dr = (rect.right() - cx) / rect.w;
    let dt = (cy - rect.y) / rect.h;
    let db = (rect.bottom() - cy) / rect.h;

    let min = dl.min(dr).min(dt).min(db);
    let (orient, before, cue) = if min == dl {
        (
            rt_core::Orientation::LeftRight,
            true,
            rt_core::Rect::new(rect.x, rect.y, rect.w * 0.5, rect.h),
        )
    } else if min == dr {
        (
            rt_core::Orientation::LeftRight,
            false,
            rt_core::Rect::new(rect.x + rect.w * 0.5, rect.y, rect.w * 0.5, rect.h),
        )
    } else if min == dt {
        (
            rt_core::Orientation::TopBottom,
            true,
            rt_core::Rect::new(rect.x, rect.y, rect.w, rect.h * 0.5),
        )
    } else {
        (
            rt_core::Orientation::TopBottom,
            false,
            rt_core::Rect::new(rect.x, rect.y + rect.h * 0.5, rect.w, rect.h * 0.5),
        )
    };

    Some(ResolvedDrop {
        target: rt_session::DropTarget::SplitBeside { pane: *pane, orient, before },
        cue,
        caret: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rt_core::{PaneId, Rect};
    use rt_session::DropTarget;

    fn two_panes() -> Vec<(PaneId, Rect)> {
        vec![
            (PaneId(1), Rect::new(0.0, 0.0, 400.0, 600.0)),
            (PaneId(2), Rect::new(406.0, 0.0, 400.0, 600.0)),
        ]
    }
    fn bounds() -> Rect {
        Rect::new(0.0, 0.0, 806.0, 600.0)
    }

    fn make_bar(tabs: &[(PaneId, f32)]) -> rt_core::TabBar {
        rt_core::TabBar {
            tabs: tabs
                .iter()
                .enumerate()
                .map(|(i, (id, x))| rt_core::Tab {
                    rect: Rect::new(*x, 0.0, 100.0, 24.0),
                    first_pane: *id,
                    active: i == 0,
                    number: i + 1,
                })
                .collect(),
        }
    }

    #[test]
    fn centre_of_a_pane_swaps_for_a_pane_payload() {
        let r = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &two_panes(), &[], bounds(), (200.0, 300.0), 1.0).unwrap();
        assert_eq!(r.target, DropTarget::Swap { pane: PaneId(1) });
        assert!(!r.caret);
        assert_eq!((r.cue.w, r.cue.h), (400.0, 600.0), "whole-pane cue");
    }

    #[test]
    fn near_an_edge_splits_on_that_side() {
        // 30px from pane 2's left edge, vertically centred → LeftRight before=true.
        let r = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &two_panes(), &[], bounds(), (436.0, 300.0), 1.0).unwrap();
        assert_eq!(r.target, DropTarget::SplitBeside { pane: PaneId(2), orient: rt_core::Orientation::LeftRight, before: true });
        assert!(r.cue.w < 401.0 * 0.6, "cue is (about) the left half of pane 2");
        // Near pane 1's bottom → TopBottom before=false. y=550 keeps this
        // outside the window's own bottom EDGE_STRIP band (24px, so y >=
        // 576 would coincide with pane 1's bottom == the window's bottom in
        // this fixture and hit `window_edge_strip_beats_pane_zones` instead
        // — see the deviation note in task-8-report.md) while still being
        // nearest to pane 1's bottom edge (50px away vs 550px from the top).
        let r2 = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &two_panes(), &[], bounds(), (200.0, 550.0), 1.0).unwrap();
        assert_eq!(r2.target, DropTarget::SplitBeside { pane: PaneId(1), orient: rt_core::Orientation::TopBottom, before: false });
    }

    #[test]
    fn window_edge_strip_beats_pane_zones() {
        let r = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &two_panes(), &[], bounds(), (200.0, 10.0), 1.0).unwrap();
        assert_eq!(r.target, DropTarget::RootEdge { orient: rt_core::Orientation::TopBottom, before: true });
    }

    #[test]
    fn a_pane_never_drops_on_itself() {
        assert!(resolve_drop(DragPayload::Pane(PaneId(1)), &[PaneId(1)], &two_panes(), &[], bounds(), (200.0, 300.0), 1.0).is_none());
    }

    #[test]
    fn tab_strip_yields_an_insert_caret() {
        // One bar, two 100px tabs at y 0..24.
        let bar = make_bar(&[(PaneId(1), 0.0), (PaneId(2), 100.0)]); // helper below
        let panes = vec![(PaneId(1), Rect::new(0.0, 24.0, 806.0, 576.0))];
        // Cursor past tab 1's midpoint and tab 2's midpoint → index 2 (the end).
        let r = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &panes, &[bar], bounds(), (190.0, 12.0), 1.0).unwrap();
        assert_eq!(r.target, DropTarget::TabAt { anchor: PaneId(1), index: 2 });
        assert!(r.caret);
    }

    #[test]
    fn a_tab_reorders_on_its_own_strip() {
        let bar = make_bar(&[(PaneId(1), 0.0), (PaneId(2), 100.0)]);
        let panes = vec![(PaneId(2), Rect::new(0.0, 24.0, 806.0, 576.0))];
        let r = resolve_drop(DragPayload::Tab { first_pane: PaneId(2) }, &[PaneId(2)], &panes, &[bar], bounds(), (10.0, 12.0), 1.0).unwrap();
        assert_eq!(r.target, DropTarget::TabAt { anchor: PaneId(1), index: 0 });
    }

    #[test]
    fn dropping_a_tab_right_on_its_own_strip_subtracts_the_gap_it_leaves() {
        // Strip [a, b, c]; drag `a` (current index 0) to the caret AFTER b —
        // the insertion slot between b and c is 2. Removing `a` first shifts b
        // and c left, so the resulting order [b, a, c] is reorder_tab(a, 1).
        assert_eq!(index_for_reorder(0, 2), 1);
        // Its own two adjacent slots are no-ops.
        assert_eq!(index_for_reorder(1, 1), 1);
        assert_eq!(index_for_reorder(1, 2), 1);
        // Moving left keeps the slot as-is (nothing before it shifted).
        assert_eq!(index_for_reorder(2, 0), 0);
        // Past the end of a 3-tab strip → the last position.
        assert_eq!(index_for_reorder(0, 3), 2);
    }

    // --- extra edge-case coverage beyond the brief's tests ---

    #[test]
    fn empty_panes_and_bars_never_panic_and_yield_none() {
        assert!(resolve_drop(DragPayload::Pane(PaneId(1)), &[], &[], &[], bounds(), (200.0, 300.0), 1.0).is_none());
    }

    #[test]
    fn cursor_in_a_gutter_between_panes_is_none() {
        // x = 402 sits in the 6px gap between pane 1 (ends at 400) and pane 2
        // (starts at 406) — not inside either pane's rect, no tab bar, and
        // outside the edge strip (it's centred vertically & horizontally).
        assert!(resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &two_panes(), &[], bounds(), (402.0, 300.0), 1.0).is_none());
    }

    #[test]
    fn empty_tab_bar_is_skipped_without_panicking() {
        let bar = rt_core::TabBar { tabs: vec![] };
        let r = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &two_panes(), &[bar], bounds(), (200.0, 300.0), 1.0).unwrap();
        // Falls through to pane-zone resolution (centre swap of pane 1).
        assert_eq!(r.target, DropTarget::Swap { pane: PaneId(1) });
    }

    #[test]
    fn tab_payload_over_centre_falls_through_to_nearest_edge_not_swap() {
        // A Tab payload dropped in the centre of another pane must NOT swap;
        // it should resolve to a SplitBeside via nearest-edge instead.
        let r = resolve_drop(
            DragPayload::Tab { first_pane: PaneId(9) },
            &[PaneId(9)],
            &two_panes(),
            &[],
            bounds(),
            (200.0, 300.0),
            1.0,
        )
        .unwrap();
        match r.target {
            DropTarget::SplitBeside { pane, .. } => assert_eq!(pane, PaneId(1)),
            other => panic!("expected SplitBeside for a Tab payload in the centre, got {other:?}"),
        }
        assert!(!r.caret);
    }

    #[test]
    fn tab_bar_excludes_dragged_pane_subtree_but_not_a_reorder_of_itself() {
        // A bar anchored at PaneId(1), dragging PaneId(1) itself (a Pane
        // payload, not a same-bar Tab reorder) — the strip must be excluded.
        // The cursor's y also happens to fall within the top window-edge
        // strip (EDGE_STRIP == the tab band height here), so once the tab
        // strip is excluded, resolution falls through to that edge band
        // rather than yielding TabAt. x is centred so top is unambiguously
        // the nearest bounds edge (not left/right).
        let bar = make_bar(&[(PaneId(1), 0.0), (PaneId(2), 100.0)]);
        let panes = vec![(PaneId(1), Rect::new(0.0, 24.0, 806.0, 576.0))];
        let r = resolve_drop(DragPayload::Pane(PaneId(1)), &[PaneId(1)], &panes, &[bar], bounds(), (403.0, 12.0), 1.0).unwrap();
        assert_eq!(r.target, DropTarget::RootEdge { orient: rt_core::Orientation::TopBottom, before: true });
    }

    #[test]
    fn zero_size_pane_rect_does_not_panic() {
        let panes = vec![(PaneId(1), Rect::new(0.0, 0.0, 0.0, 0.0))];
        // Cursor can never be "contained" by a zero-size rect (half-open),
        // so this simply falls through to None (no edge strip match either,
        // since bounds here has zero size too).
        let r = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &panes, &[], Rect::new(0.0, 0.0, 0.0, 0.0), (0.0, 0.0), 1.0);
        assert!(r.is_none());
    }

    // ----- HiDPI: the strips and cues scale with the display ---------------

    /// The window-edge strip is a flat 24px band. On a 2x panel it must be 48
    /// physical px — the same apparent reach — or dropping a pane at the window
    /// edge means aiming at a band half the size it looks.
    #[test]
    fn the_edge_strip_widens_on_a_hidpi_display() {
        // 30px in from the top: outside the 1x strip, inside the 2x one.
        let at = |sc: f32| {
            resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &two_panes(), &[], bounds(), (200.0, 30.0), sc)
        };
        // At 1x, 30 > 24, so this falls through to the pane zone (a split, not a
        // root edge).
        assert!(
            !matches!(at(1.0).unwrap().target, DropTarget::RootEdge { .. }),
            "30px in is outside the 1x edge strip",
        );
        // At 2x the strip reaches 48px, so the same point IS the root edge.
        assert!(
            matches!(at(2.0).unwrap().target, DropTarget::RootEdge { .. }),
            "30px in must be inside the 2x edge strip",
        );
    }

    /// The tab-insert caret is drawn from the rect resolved here (`chrome::
    /// dragdrop` paints wings on it, with the same factor), so its width scales
    /// with everything around it.
    #[test]
    fn the_tab_caret_cue_scales() {
        let panes = vec![(PaneId(1), Rect::new(0.0, 24.0, 806.0, 576.0))];
        let bar = make_bar(&[(PaneId(1), 0.0), (PaneId(2), 100.0)]);
        for &sc in &[1.0_f32, 2.0, 3.0] {
            let r = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &panes, &[bar.clone()], bounds(), (190.0, 12.0), sc).unwrap();
            assert!(r.caret);
            assert_eq!(r.cue.w, sc * CARET_W, "caret width at {sc}x");
        }
    }

    /// And at 1.0 every cue is bit-for-bit what it was: the whole resolver is
    /// unchanged on a normal display.
    #[test]
    fn scale_one_leaves_every_cue_bit_identical() {
        let r = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &two_panes(), &[], bounds(), (200.0, 10.0), 1.0).unwrap();
        assert!(matches!(r.target, DropTarget::RootEdge { .. }), "10px in is inside the 24px strip");
        let panes = vec![(PaneId(1), Rect::new(0.0, 24.0, 806.0, 576.0))];
        let bar = make_bar(&[(PaneId(1), 0.0), (PaneId(2), 100.0)]);
        let c = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &panes, &[bar], bounds(), (190.0, 12.0), 1.0).unwrap();
        assert_eq!(c.cue.w.to_bits(), 3.0_f32.to_bits(), "the caret was, and stays, 3px at 1x");
    }

    /// `DRAG_THRESHOLD` is a GESTURE distance, not a drawn size — but winit
    /// reports the pointer in PHYSICAL pixels, so the same hand movement yields
    /// twice the delta on a 2x panel. Scaling it is what keeps "how far I have to
    /// move before this becomes a drag" constant to the hand rather than to the
    /// pixel grid. (The comparison itself lives in `main.rs`'s pointer handler;
    /// this pins the relationship the two sides depend on.)
    #[test]
    fn the_drag_threshold_is_a_hand_distance_not_a_pixel_count() {
        // A twitch that produced 5 physical px on a 1x display produces 10 on a
        // 2x one. Flat, only the 1x case is a drag; scaled, both are — which is
        // the point: the hand did the same thing.
        let twitch_1x = 5.0_f32;
        let twitch_2x = 10.0_f32;
        assert!(twitch_1x > 1.0 * DRAG_THRESHOLD);
        assert!(twitch_2x > 2.0 * DRAG_THRESHOLD);
        // And a HALF-twitch stays below the threshold at both scales, rather
        // than crossing it on the denser display only.
        assert!(0.5 * twitch_1x < 1.0 * DRAG_THRESHOLD);
        assert!(0.5 * twitch_2x < 2.0 * DRAG_THRESHOLD);
    }

}
