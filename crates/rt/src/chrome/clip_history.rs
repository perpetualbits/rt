//! Native clipboard-history overlay: a list of recent-clip previews plus a
//! trailing "Clear history" row. Geometry + hit-testing are pure (this module);
//! `main.rs` supplies the preview strings and draws. Follows the shared chrome
//! system in [`crate::chrome::theme`].
//!
//! Like the context menu, this panel could be taller than the window — the
//! clipboard history holds more clips than a short window has rows for — and
//! the old clamp (`anchor.1.min(win_h - h).max(0.0)`) pinned it to the top and
//! left the tail unreachable. It now scrolls, but with no scroll state of its
//! own: the SELECTION drives it. Arrow keys already move the selection, so
//! keeping the selected row on screen is all the scrolling this panel needs,
//! and there is no second piece of state to fall out of step with the drawing.

use crate::backend::Backend;
use crate::chrome::theme::{self, Palette};
use crate::chrome::{hit, Recti};
use crate::chrome_scale::logical;

/// Overlay geometry in window px. `rows` has one rect per clip row plus a final
/// rect for the Clear row (index `clear_row`), so indices line up with the
/// caller's clip list. Rows scrolled out of view get an off-panel rect that
/// `hit` rejects — the same arrangement `chrome::prefs` and `chrome::menu` use.
pub struct Geom {
    pub panel: Recti,
    pub rows: Vec<Recti>,
    pub clear_row: usize,
    /// First row on screen and one past the last.
    pub first: usize,
    pub end: usize,
}

/// Lay the overlay out anchored at `anchor`, clamped fully on-screen and
/// scrolled so `selected` is visible. `row_count` is the number of clip rows; a
/// Clear row is appended at `clear_row`.
pub fn layout(
    row_count: usize,
    selected: usize,
    anchor: (f32, f32),
    cell_w: f32,
    cell_h: f32,
    win_w: f32,
    win_h: f32,
    width_cols: usize,
    sc: f32,
) -> Geom {
    // `hit_row` tests the rects built here, so the whole click surface follows
    // the same `sc` the drawing does.
    let pad_x = sc * logical::PANEL_PAD_X;
    let pad_y = sc * logical::PANEL_PAD_Y;
    let row_h = cell_h + sc * logical::PANEL_ROW_PAD;
    let total = row_count + 1; // + Clear row
    let w = (width_cols as f32 * cell_w + pad_x * 2.0).min(win_w);
    let natural = total as f32 * row_h + pad_y * 2.0;

    let (h, y) = if natural <= win_h {
        (natural, anchor.1.min(win_h - natural).max(0.0))
    } else {
        (win_h, 0.0)
    };
    let x = anchor.0.min(win_w - w).max(0.0);
    let visible = (((h - pad_y * 2.0) / row_h).floor() as usize).clamp(1, total);
    // The selection is the scroll: bring it to whichever edge is nearest.
    let max_scroll = total - visible;
    let first = if selected < visible { 0 } else { (selected + 1 - visible).min(max_scroll) };
    let end = (first + visible).min(total);

    let mut rows = vec![Recti { x: -1.0, y: -1.0, w: 0.0, h: 0.0 }; total];
    for (n, i) in (first..end).enumerate() {
        rows[i] = Recti { x, y: y + pad_y + n as f32 * row_h, w, h: row_h };
    }
    Geom { panel: Recti { x, y, w, h }, rows, clear_row: row_count, first, end }
}

/// The row at `p` (a clip index `0..clear_row`, or `clear_row` for Clear), or
/// `None` outside the panel / on a scrolled-away row.
pub fn hit_row(g: &Geom, p: (f32, f32)) -> Option<usize> {
    hit(&g.rows, p)
}

/// Draw the panel, hover/selection highlight, each preview + its size badge, and
/// the Clear row. `previews[i]`/`badges[i]` are the clip rows; `hover`/`selected`
/// mark the highlighted row (either may be the Clear row).
pub fn draw(
    be: &mut dyn Backend,
    g: &Geom,
    previews: &[String],
    badges: &[String],
    hover: Option<usize>,
    selected: usize,
    pal: &Palette,
    cell_w: f32,
    cell_h: f32,
    sc: f32,
) {
    let pad_x = sc * logical::PANEL_PAD_X;
    let hair = sc * logical::HAIRLINE;
    theme::panel(be, g.panel, pal, sc);
    for i in g.first..g.end {
        let r = g.rows[i];
        let sel = selected == i;
        if sel {
            theme::row_highlight(be, r, pal.sel, sc);
        } else if hover == Some(i) {
            theme::row_highlight(be, r, pal.hover, sc);
        }
        let oy = theme::text_y(r, cell_h);
        if i == g.clear_row {
            // A rule above the destructive row, so it reads as separate from the
            // clips rather than as one more of them.
            if i > g.first {
                let inset = sc * logical::PANEL_SEL_INSET;
                be.fill_rect(r.x + inset, r.y, r.w - 2.0 * inset, hair, pal.sep);
            }
            let colr = if sel { pal.sel_text } else { pal.dim };
            theme::text(be, r.x + pad_x, oy, "Clear history", colr, false);
            continue;
        }
        let colr = if sel { pal.sel_text } else { pal.text };
        let bcol = if sel { pal.sel_text } else { pal.dim };
        theme::text(be, r.x + pad_x, oy, previews.get(i).map(String::as_str).unwrap_or(""), colr, false);
        if let Some(b) = badges.get(i) {
            theme::text_right(be, r.x + r.w - pad_x, oy, b, cell_w, bcol, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_has_one_row_per_clip_plus_a_clear_row_on_screen() {
        let g = layout(3, 0, (10.0, 10.0), 8.0, 16.0, 800.0, 600.0, 24, 1.0);
        assert_eq!(g.rows.len(), 4); // 3 clips + Clear
        assert_eq!(g.clear_row, 3);
        assert_eq!((g.first, g.end), (0, 4), "they all fit");
        // Fully on screen.
        assert!(g.panel.x >= 0.0 && g.panel.y >= 0.0);
        assert!(g.panel.x + g.panel.w <= 800.0);
        assert!(g.panel.y + g.panel.h <= 600.0);
    }

    #[test]
    fn hit_row_maps_points_to_clip_and_clear_rows() {
        let g = layout(2, 0, (0.0, 0.0), 8.0, 16.0, 800.0, 600.0, 24, 1.0);
        let mid = |r: &Recti| (r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert_eq!(hit_row(&g, mid(&g.rows[0])), Some(0));
        assert_eq!(hit_row(&g, mid(&g.rows[1])), Some(1));
        assert_eq!(hit_row(&g, mid(&g.rows[2])), Some(2)); // Clear row
        assert_eq!(hit_row(&g, (5000.0, 5000.0)), None); // outside
    }

    #[test]
    fn anchor_clamps_so_the_panel_stays_visible() {
        let g = layout(5, 0, (790.0, 590.0), 8.0, 16.0, 800.0, 600.0, 24, 1.0);
        assert!(g.panel.x + g.panel.w <= 800.0);
        assert!(g.panel.y + g.panel.h <= 600.0);
    }

    /// The same hole the context menu had: more clips than the window is tall.
    /// Every row must be reachable by moving the selection, and every drawn row
    /// must be inside the window.
    #[test]
    fn a_history_taller_than_the_window_is_still_fully_reachable() {
        let (n, win_h) = (40_usize, 240.0_f32);
        let total = n + 1;
        let mut seen = vec![false; total];
        for sel in 0..total {
            let g = layout(n, sel, (10.0, 10.0), 8.0, 16.0, 800.0, win_h, 24, 1.0);
            assert!(g.end - g.first < total, "this case needs the panel to overflow");
            assert!(sel >= g.first && sel < g.end, "the selected row {sel} must be on screen");
            for i in g.first..g.end {
                let r = g.rows[i];
                assert!(r.y >= -0.01 && r.y + r.h <= win_h + 0.01, "row {i} off the window");
                assert_eq!(hit_row(&g, (r.x + 2.0, r.y + r.h * 0.5)), Some(i));
                seen[i] = true;
            }
        }
        assert!(seen.iter().all(|s| *s), "some row can never be selected onto the screen");
    }

    /// Draw and hit read the same rects: exactly the rows `draw` paints
    /// (`first..end`) are the rows `hit` accepts.
    #[test]
    fn what_is_drawn_is_exactly_what_can_be_hit() {
        for &sc in &[1.0_f32, 2.0] {
            for sel in [0_usize, 5, 20] {
                let g = layout(24, sel, (10.0, 10.0), 8.0 * sc, 16.0 * sc, 700.0, 300.0, 24, sc);
                for (i, r) in g.rows.iter().enumerate() {
                    let drawn = i >= g.first && i < g.end;
                    let hittable = hit_row(&g, (r.x + r.w * 0.5, r.y + r.h * 0.5)) == Some(i);
                    assert_eq!(drawn, hittable, "row {i} sel {sel} @{sc}x");
                }
            }
        }
    }

    /// Small windows and 2x displays: the panel never escapes the window.
    #[test]
    fn the_panel_stays_inside_the_window_at_every_size_and_scale() {
        for &sc in &[1.0_f32, 2.0, 3.0] {
            for &(w, h) in &[(120.0_f32, 80.0_f32), (400.0, 200.0), (1920.0, 1080.0)] {
                let g = layout(12, 3, (w, h), 8.0 * sc, 16.0 * sc, w, h, 24, sc);
                assert!(g.panel.x >= -0.01 && g.panel.y >= -0.01, "{w}x{h}@{sc}");
                assert!(g.panel.x + g.panel.w <= w + 0.01, "{w}x{h}@{sc}");
                assert!(g.panel.y + g.panel.h <= h + 0.01, "{w}x{h}@{sc}");
            }
        }
    }
}
