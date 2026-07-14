//! Native context menu: laid out from `menu::rows`, drawn as fills + glyphs.
use crate::backend::Backend;
use crate::chrome::{hit, Recti};
use crate::menu::Row;
use crate::render::Color;

/// Menu geometry in window px: the panel box and each row's rect (row rects
/// share the panel width; separators get a short rect so indices line up 1:1
/// with `rows`).
pub struct Geom {
    pub panel: Recti,
    pub rows: Vec<Recti>,
    // Parallel to `rows`: true where the row is a clickable action (not a
    // separator). Kept alongside the rects so `hit_row` can filter separators
    // out without needing the original `&[Row]` slice again.
    clickable: Vec<bool>,
}

const PAD_X: f32 = 8.0; // inner horizontal padding
const SEP_H: f32 = 7.0; // separator row height

/// Lay the menu out anchored at `anchor`, clamped fully on-screen.
pub fn layout(rows: &[Row], anchor: (f32, f32), cell_w: f32, cell_h: f32, win_w: f32, win_h: f32) -> Geom {
    let row_h = cell_h + 4.0;
    // Width = widest "label   accel" in cells, plus padding.
    let cols = rows.iter().map(|r| {
        let a = r.accel.as_deref().map(|s| s.chars().count() + 3).unwrap_or(0);
        r.label.chars().count() + a
    }).max().unwrap_or(8);
    let w = cols as f32 * cell_w + PAD_X * 2.0;
    let h: f32 = rows.iter().map(|r| if r.action.is_none() { SEP_H } else { row_h }).sum::<f32>() + PAD_X;
    // Clamp so the whole panel stays visible.
    let x = anchor.0.min(win_w - w).max(0.0);
    let y = anchor.1.min(win_h - h).max(0.0);
    let mut rrects = Vec::with_capacity(rows.len());
    let mut cy = y + PAD_X * 0.5;
    for r in rows {
        let rh = if r.action.is_none() { SEP_H } else { row_h };
        rrects.push(Recti { x, y: cy, w, h: rh });
        cy += rh;
    }
    let clickable = rows.iter().map(|r| r.action.is_some()).collect();
    Geom { panel: Recti { x, y, w, h }, rows: rrects, clickable }
}

/// The clickable row at `p`, or `None` for separators / outside the panel.
pub fn hit_row(g: &Geom, p: (f32, f32)) -> Option<usize> {
    let i = hit(&g.rows, p)?;
    // Separator rects exist only so indices line up 1:1 with `rows`; they are
    // never clickable, so re-filter them out here.
    if g.clickable[i] {
        Some(i)
    } else {
        None
    }
}

/// Draw the panel, hovered highlight, labels, accelerators, and separators.
pub fn draw(be: &mut dyn Backend, g: &Geom, rows: &[Row], hover: Option<usize>, cell_w: f32, cell_h: f32) {
    let bg = Color::rgb(0x20, 0x22, 0x28);
    let border = Color::rgb(0x50, 0x54, 0x60);
    let fg = Color::rgb(0xe0, 0xe0, 0xe6);
    let fg_dim = Color::rgb(0x90, 0x94, 0xa0);
    let fg_off = Color::rgb(0x60, 0x62, 0x6a);
    let hl = Color::rgb(0x35, 0x5a, 0x9a);
    let sep = Color::rgb(0x40, 0x43, 0x4d);
    // Panel + 1px border.
    be.fill_rect(g.panel.x, g.panel.y, g.panel.w, g.panel.h, bg);
    be.fill_rect(g.panel.x, g.panel.y, g.panel.w, 1.0, border);
    be.fill_rect(g.panel.x, g.panel.y + g.panel.h - 1.0, g.panel.w, 1.0, border);
    be.fill_rect(g.panel.x, g.panel.y, 1.0, g.panel.h, border);
    be.fill_rect(g.panel.x + g.panel.w - 1.0, g.panel.y, 1.0, g.panel.h, border);
    for (i, (row, rect)) in rows.iter().zip(&g.rows).enumerate() {
        if row.action.is_none() {
            // Separator: a thin line centred in its rect.
            be.fill_rect(rect.x + PAD_X, rect.y + rect.h / 2.0, rect.w - PAD_X * 2.0, 1.0, sep);
            continue;
        }
        if hover == Some(i) && row.enabled {
            be.fill_rect(rect.x + 1.0, rect.y, rect.w - 2.0, rect.h, hl);
        }
        let colr = if !row.enabled { fg_off } else { fg };
        // Label at the left; draw_char places glyphs on the cell grid, so map the
        // row's pixel origin to (col,row) = (0,0) with the origin as the offset.
        let ox = rect.x + PAD_X;
        let oy = rect.y + (rect.h - cell_h) / 2.0;
        for (c, ch) in row.label.chars().enumerate() {
            be.draw_char(ox, oy, c, 0, ch, colr, false, false);
        }
        // Accelerator, right-aligned in a dim colour.
        if let Some(acc) = &row.accel {
            let n = acc.chars().count();
            let ax = rect.x + rect.w - PAD_X - n as f32 * cell_w;
            for (c, ch) in acc.chars().enumerate() {
                be.draw_char(ax, oy, c, 0, ch, fg_dim, false, false);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu;
    use rt_config::Keymap;

    fn sample() -> Vec<Row> {
        menu::rows(&Keymap::default(), true, None)
    }

    #[test]
    fn panel_clamps_onto_screen() {
        let rows = sample();
        // Anchor near the bottom-right corner: the panel must shift fully on-screen.
        let g = layout(&rows, (795.0, 595.0), 8.0, 18.0, 800.0, 600.0);
        assert!(g.panel.x + g.panel.w <= 800.0 + 0.01);
        assert!(g.panel.y + g.panel.h <= 600.0 + 0.01);
    }

    #[test]
    fn hit_row_skips_separators() {
        let rows = sample();
        let g = layout(&rows, (10.0, 10.0), 8.0, 18.0, 800.0, 600.0);
        // The 3rd row in a no-url menu is the separator after Copy/Paste.
        let sep_idx = rows.iter().position(|r| r.action.is_none()).unwrap();
        let mid = (g.rows[sep_idx].x + 2.0, g.rows[sep_idx].y + g.rows[sep_idx].h / 2.0);
        assert_eq!(hit_row(&g, mid), None, "clicking a separator selects nothing");
        // A real row hits.
        let copy_idx = rows.iter().position(|r| r.label == "Copy").unwrap();
        let cm = (g.rows[copy_idx].x + 2.0, g.rows[copy_idx].y + g.rows[copy_idx].h / 2.0);
        assert_eq!(hit_row(&g, cm), Some(copy_idx));
    }
}
