//! Native clipboard-history overlay: a list of recent-clip previews plus a
//! trailing "Clear history" row. Geometry + hit-testing are pure (this module);
//! `main.rs` supplies the preview strings and draws. Mirrors `chrome/menu.rs`.

use crate::backend::Backend;
use crate::chrome_scale::logical;
use crate::chrome::{hit, Recti};
use crate::render::Color;

/// Panel inner padding, in LOGICAL px (registered as
/// `chrome_scale::logical::CLIP_PAD`); scaled at every use.
pub const PAD: f32 = 6.0;

/// Overlay geometry in window px. `rows` has one rect per clip row plus a final
/// rect for the Clear row (index `clear_row`), so indices line up with the
/// caller's clip list.
pub struct Geom {
    pub panel: Recti,
    pub rows: Vec<Recti>,
    pub clear_row: usize,
}

/// Lay the overlay out anchored at `anchor`, clamped fully on-screen. `row_count`
/// is the number of clip rows; a Clear row is appended at `clear_row`.
pub fn layout(row_count: usize, anchor: (f32, f32), cell_w: f32, cell_h: f32, win_w: f32, win_h: f32, width_cols: usize, sc: f32) -> Geom {
    // `hit_row` tests the rects built here, so the whole click surface follows
    // the same `sc` the drawing does.
    let pad = sc * PAD;
    let row_h = cell_h + sc * logical::PANEL_ROW_PAD;
    let total = row_count + 1; // + Clear row
    let w = width_cols as f32 * cell_w + pad * 2.0;
    let h = total as f32 * row_h + pad;
    let x = anchor.0.min(win_w - w).max(0.0);
    let y = anchor.1.min(win_h - h).max(0.0);
    let mut rows = Vec::with_capacity(total);
    let mut cy = y + pad * 0.5;
    for _ in 0..total {
        rows.push(Recti { x, y: cy, w, h: row_h });
        cy += row_h;
    }
    Geom { panel: Recti { x, y, w, h }, rows, clear_row: row_count }
}

/// The row at `p` (a clip index `0..clear_row`, or `clear_row` for Clear), or
/// `None` outside the panel.
pub fn hit_row(g: &Geom, p: (f32, f32)) -> Option<usize> {
    hit(&g.rows, p)
}

/// Draw the panel, hover highlight, each preview + its size badge, and the Clear
/// row. `previews[i]`/`badges[i]` are the clip rows; `hover`/`selected` mark the
/// highlighted row (either may be the Clear row).
pub fn draw(
    be: &mut dyn Backend,
    g: &Geom,
    previews: &[String],
    badges: &[String],
    hover: Option<usize>,
    selected: usize,
    cell_w: f32,
    cell_h: f32,
    sc: f32,
) {
    let pad = sc * PAD;
    let bg = Color::rgb(0x20, 0x22, 0x28);
    let fg = Color::rgb(0xd6, 0xde, 0xe8);
    let dim = Color::rgb(0x8b, 0x98, 0xa9);
    let hl = Color::rgb(0x33, 0x3a, 0x46);
    be.fill_rect(g.panel.x, g.panel.y, g.panel.w, g.panel.h, bg);
    for (i, r) in g.rows.iter().enumerate() {
        if hover == Some(i) || selected == i {
            be.fill_rect(r.x, r.y, r.w, r.h, hl);
        }
        let ty = r.y + sc * logical::PANEL_TEXT_TOP;
        if i == g.clear_row {
            let label = "Clear history";
            for (c, ch) in label.chars().enumerate() {
                be.draw_char(r.x + pad, ty, c, 0, ch, dim, false, false);
            }
        } else {
            let p = previews.get(i).map(String::as_str).unwrap_or("");
            for (c, ch) in p.chars().enumerate() {
                be.draw_char(r.x + pad, ty, c, 0, ch, fg, false, false);
            }
            if let Some(b) = badges.get(i) {
                let bw = b.chars().count();
                let bx = r.x + r.w - pad - bw as f32 * cell_w;
                for (c, ch) in b.chars().enumerate() {
                    be.draw_char(bx, ty, c, 0, ch, dim, false, false);
                }
            }
        }
    }
    let _ = cell_h;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn layout_has_one_row_per_clip_plus_a_clear_row_on_screen() {
        let g = layout(3, (10.0, 10.0), 8.0, 16.0, 800.0, 600.0, 24, 1.0);
        assert_eq!(g.rows.len(), 4); // 3 clips + Clear
        assert_eq!(g.clear_row, 3);
        // Fully on screen.
        assert!(g.panel.x >= 0.0 && g.panel.y >= 0.0);
        assert!(g.panel.x + g.panel.w <= 800.0);
        assert!(g.panel.y + g.panel.h <= 600.0);
    }

    #[test]
    fn hit_row_maps_points_to_clip_and_clear_rows() {
        let g = layout(2, (0.0, 0.0), 8.0, 16.0, 800.0, 600.0, 24, 1.0);
        let mid = |r: &Recti| (r.x + r.w / 2.0, r.y + r.h / 2.0);
        assert_eq!(hit_row(&g, mid(&g.rows[0])), Some(0));
        assert_eq!(hit_row(&g, mid(&g.rows[1])), Some(1));
        assert_eq!(hit_row(&g, mid(&g.rows[2])), Some(2)); // Clear row
        assert_eq!(hit_row(&g, (5000.0, 5000.0)), None); // outside
    }

    #[test]
    fn anchor_clamps_so_the_panel_stays_visible() {
        let g = layout(5, (790.0, 590.0), 8.0, 16.0, 800.0, 600.0, 24, 1.0);
        assert!(g.panel.x + g.panel.w <= 800.0);
        assert!(g.panel.y + g.panel.h <= 600.0);
    }
}
