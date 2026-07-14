//! Native manual overlay: a centered panel scrolling `manual::MANUAL` by cell
//! rows. Scroll position lives in `Active.manual_scroll`.
use crate::backend::Backend;
use crate::chrome::Recti;
use crate::manual::MANUAL;
use crate::render::Color;

/// Manual panel geometry: the centered box, visible cell rows, and total lines.
pub struct Geom {
    pub panel: Recti,
    pub rows: usize,
    pub total: usize,
}

const PAD: f32 = 12.0;

/// A panel ~80% of the window, centered.
pub fn layout(win_w: f32, win_h: f32, _cell_w: f32, cell_h: f32) -> Geom {
    let w = (win_w * 0.8).min(720.0);
    let h = win_h * 0.85;
    let panel = Recti { x: (win_w - w) / 2.0, y: (win_h - h) / 2.0, w, h };
    let rows = (((h - PAD * 2.0) / cell_h).floor() as usize).max(1);
    let total = MANUAL.lines().count();
    Geom { panel, rows, total }
}

/// Clamp a scroll offset so the last page stays on-screen.
pub fn clamp_scroll(scroll: usize, g: &Geom) -> usize {
    let max = g.total.saturating_sub(g.rows);
    scroll.min(max)
}

/// How many character columns fit inside the panel's padded interior. Lines are
/// truncated to this so text stays within the grey box (the egui manual clipped
/// inside a ScrollArea; the native draw must clip explicitly). Saturates at 0.
pub fn visible_cols(panel_w: f32, cell_w: f32) -> usize {
    let inner = panel_w - PAD * 2.0;
    if inner <= 0.0 || cell_w <= 0.0 {
        return 0;
    }
    (inner / cell_w).floor() as usize
}

/// Draw the panel, the visible line slice, and a scrollbar thumb.
pub fn draw(be: &mut dyn Backend, g: &Geom, scroll: usize, cell_w: f32, _cell_h: f32) {
    let bg = Color::rgb(0x18, 0x1a, 0x1f);
    let border = Color::rgb(0x50, 0x54, 0x60);
    let fg = Color::rgb(0xd0, 0xd2, 0xda);
    let thumb = Color::rgb(0x45, 0x48, 0x54);
    let p = g.panel;
    be.fill_rect(p.x, p.y, p.w, p.h, bg);
    be.fill_rect(p.x, p.y, p.w, 1.0, border);
    be.fill_rect(p.x, p.y + p.h - 1.0, p.w, 1.0, border);
    be.fill_rect(p.x, p.y, 1.0, p.h, border);
    be.fill_rect(p.x + p.w - 1.0, p.y, 1.0, p.h, border);
    let ox = p.x + PAD;
    let oy = p.y + PAD;
    let scroll = clamp_scroll(scroll, g);
    // Clip each line to the panel's interior so long lines don't spill past the
    // grey box (the visible manual text stays inside its panel).
    let cols = visible_cols(p.w, cell_w);
    for (r, line) in MANUAL.lines().skip(scroll).take(g.rows).enumerate() {
        for (c, ch) in line.chars().take(cols).enumerate() {
            be.draw_char(ox, oy, c, r, ch, fg, false, false);
        }
    }
    // Scrollbar thumb on the right edge, sized to the visible fraction.
    if g.total > g.rows {
        let track_h = p.h - 2.0;
        let th = (track_h * g.rows as f32 / g.total as f32).max(12.0);
        let ty = p.y + 1.0 + (track_h - th) * scroll as f32 / (g.total - g.rows) as f32;
        be.fill_rect(p.x + p.w - 4.0, ty, 3.0, th, thumb);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_last_page_visible() {
        let g = layout(1000.0, 700.0, 8.0, 18.0);
        assert!(g.total > g.rows, "manual is longer than one page");
        let max = g.total - g.rows;
        assert_eq!(clamp_scroll(usize::MAX, &g), max, "cannot scroll past the end");
        assert_eq!(clamp_scroll(0, &g), 0);
    }

    #[test]
    fn visible_cols_fits_inside_the_padded_panel() {
        // 640px panel, 8px cells, 12px padding each side → (640-24)/8 = 77 cols.
        assert_eq!(visible_cols(640.0, 8.0), 77);
        // Degenerate: a panel narrower than the padding yields 0, never underflows.
        assert_eq!(visible_cols(10.0, 8.0), 0);
    }

    #[test]
    fn manual_has_lines_wider_than_a_typical_panel() {
        // Guards the bug this fixes: MANUAL genuinely contains lines longer than
        // the visible width of an 80%-of-800px panel, so truncation must engage.
        let cols = visible_cols((800.0f32 * 0.8).min(720.0), 8.0);
        assert!(
            MANUAL.lines().any(|l| l.chars().count() > cols),
            "expected some manual line wider than {cols} cols to exercise clipping"
        );
    }
}
