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

/// Draw the panel, the visible line slice, and a scrollbar thumb.
pub fn draw(be: &mut dyn Backend, g: &Geom, scroll: usize, _cell_w: f32, _cell_h: f32) {
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
    for (r, line) in MANUAL.lines().skip(scroll).take(g.rows).enumerate() {
        for (c, ch) in line.chars().enumerate() {
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
}
