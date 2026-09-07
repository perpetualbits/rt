//! Native context menu: laid out from `menu::rows`, drawn as fills + glyphs.
use crate::backend::Backend;
use crate::chrome::{hit, Recti};
use crate::chrome_scale::logical;
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

/// Lay the menu out anchored at `anchor`, clamped fully on-screen.
///
/// `sc` is the display's backing factor. `hit_row` tests the rects this
/// produces, so scaling the layout scales the hit-test with it — there is no
/// separate copy of the geometry that could be left behind.
pub fn layout(rows: &[Row], anchor: (f32, f32), cell_w: f32, cell_h: f32, win_w: f32, win_h: f32, sc: f32) -> Geom {
    let pad_x = sc * logical::MENU_PAD; // inner padding (used across AND down)
    let sep_h = sc * logical::MENU_SEP_H; // separator row height
    let row_h = cell_h + sc * logical::PANEL_ROW_PAD;
    // Width = widest "label   accel" in cells, plus padding.
    let cols = rows.iter().map(|r| {
        let a = r.accel.as_deref().map(|s| s.chars().count() + 3).unwrap_or(0);
        r.label.chars().count() + a
    }).max().unwrap_or(8);
    let w = cols as f32 * cell_w + pad_x * 2.0;
    // A separator is the only row with an empty label; info rows (version footer)
    // carry a label but no action, so they get a full-height row like any other.
    let h: f32 = rows.iter().map(|r| if r.label.is_empty() { sep_h } else { row_h }).sum::<f32>() + pad_x;
    // Clamp so the whole panel stays visible.
    let x = anchor.0.min(win_w - w).max(0.0);
    let y = anchor.1.min(win_h - h).max(0.0);
    let mut rrects = Vec::with_capacity(rows.len());
    let mut cy = y + pad_x * 0.5;
    for r in rows {
        let rh = if r.label.is_empty() { sep_h } else { row_h };
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
pub fn draw(be: &mut dyn Backend, g: &Geom, rows: &[Row], hover: Option<usize>, cell_w: f32, cell_h: f32, sc: f32) {
    let pad_x = sc * logical::MENU_PAD;
    let hair = sc * logical::HAIRLINE;
    let bg = Color::rgb(0x20, 0x22, 0x28);
    let border = Color::rgb(0x50, 0x54, 0x60);
    let fg = Color::rgb(0xe0, 0xe0, 0xe6);
    let fg_dim = Color::rgb(0x90, 0x94, 0xa0);
    let fg_off = Color::rgb(0x60, 0x62, 0x6a);
    let hl = Color::rgb(0x35, 0x5a, 0x9a);
    let sep = Color::rgb(0x40, 0x43, 0x4d);
    // Panel + 1px border.
    be.fill_rect(g.panel.x, g.panel.y, g.panel.w, g.panel.h, bg);
    be.fill_rect(g.panel.x, g.panel.y, g.panel.w, hair, border);
    be.fill_rect(g.panel.x, g.panel.y + g.panel.h - hair, g.panel.w, hair, border);
    be.fill_rect(g.panel.x, g.panel.y, hair, g.panel.h, border);
    be.fill_rect(g.panel.x + g.panel.w - hair, g.panel.y, hair, g.panel.h, border);
    for (i, (row, rect)) in rows.iter().zip(&g.rows).enumerate() {
        // A separator is the only row with an empty label (matches `layout`); an
        // info row like the version footer has a label but no action and draws as
        // dimmed text, not a rule.
        if row.label.is_empty() {
            // Separator: a thin line centred in its rect.
            be.fill_rect(rect.x + pad_x, rect.y + rect.h / 2.0, rect.w - pad_x * 2.0, hair, sep);
            continue;
        }
        if hover == Some(i) && row.enabled {
            be.fill_rect(rect.x + hair, rect.y, rect.w - 2.0 * hair, rect.h, hl);
        }
        let colr = if !row.enabled { fg_off } else { fg };
        // Label at the left; draw_char places glyphs on the cell grid, so map the
        // row's pixel origin to (col,row) = (0,0) with the origin as the offset.
        let ox = rect.x + pad_x;
        let oy = rect.y + (rect.h - cell_h) / 2.0;
        for (c, ch) in row.label.chars().enumerate() {
            be.draw_char(ox, oy, c, 0, ch, colr, false, false);
        }
        // Accelerator, right-aligned in a dim colour.
        if let Some(acc) = &row.accel {
            let n = acc.chars().count();
            let ax = rect.x + rect.w - pad_x - n as f32 * cell_w;
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
        menu::rows(&Keymap::default(), true, None, &[])
    }

    #[test]
    fn panel_clamps_onto_screen() {
        let rows = sample();
        // Anchor near the bottom-right corner: the panel must shift fully on-screen.
        let g = layout(&rows, (795.0, 795.0), 8.0, 18.0, 800.0, 800.0, 1.0);
        assert!(g.panel.x + g.panel.w <= 800.0 + 0.01);
        assert!(g.panel.y + g.panel.h <= 800.0 + 0.01);
    }

    /// A menu TALLER than the window can't fit however it is placed (the full
    /// row list in a short window; likewise any window once several "Move Pane
    /// to N" rows are added). The clamp then pins it to the top so the panel
    /// STARTS on-screen and its first rows stay reachable, rather than letting
    /// the anchor push the top off the screen too.
    #[test]
    fn panel_taller_than_the_window_pins_to_the_top() {
        let rows = sample();
        let g = layout(&rows, (795.0, 595.0), 8.0, 18.0, 800.0, 300.0, 1.0);
        assert!(g.panel.h > 300.0, "this case only means anything when the menu overflows");
        assert_eq!(g.panel.y, 0.0, "pinned to the top edge");
        assert!(g.panel.x + g.panel.w <= 800.0 + 0.01, "still clamped horizontally");
    }

    #[test]
    fn hit_row_skips_separators() {
        let rows = sample();
        let g = layout(&rows, (10.0, 10.0), 8.0, 18.0, 800.0, 600.0, 1.0);
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
