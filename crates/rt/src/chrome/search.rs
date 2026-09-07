//! Native scrollback-search bar: a slim top-right box with the query and a hit
//! counter. Typing/navigation are handled by main.rs via the existing engine.
use crate::backend::Backend;
use crate::chrome::Recti;
use crate::chrome_scale::logical;
use crate::render::Color;

const BAR_COLS: usize = 32; // query field width in cells

/// Bar rect pinned to the top-right corner.
///
/// `sc` is the display's backing factor, applied to the flat padding and the
/// corner standoff (the width/height terms are already cell-derived, so they
/// scale with the glyph). `hit`-testing of the bar goes through the rect this
/// returns, so there is no second copy of the geometry to keep in step.
pub fn layout(win_w: f32, cell_w: f32, cell_h: f32, sc: f32) -> Recti {
    let pad = sc * logical::SEARCH_PAD;
    let inset = sc * logical::SEARCH_INSET;
    let w = BAR_COLS as f32 * cell_w + pad * 2.0 + 8.0 * cell_w; // query + " 12/34 "
    let w = w.min(win_w); // never wider than the window itself
    let h = cell_h + pad * 2.0;
    Recti { x: (win_w - w - inset).max(0.0), y: inset, w, h }
}

/// Draw the bar: background, border, query text, caret, and "pos/count".
pub fn draw(be: &mut dyn Backend, bar: Recti, query: &str, pos: usize, count: usize, cell_w: f32, cell_h: f32, sc: f32) {
    let pad = sc * logical::SEARCH_PAD;
    let hair = sc * logical::HAIRLINE;
    let bg = Color::rgb(0x1c, 0x1e, 0x24);
    let border = Color::rgb(0x50, 0x54, 0x60);
    let fg = Color::rgb(0xe0, 0xe0, 0xe6);
    let dim = Color::rgb(0x90, 0x94, 0xa0);
    be.fill_rect(bar.x, bar.y, bar.w, bar.h, bg);
    be.fill_rect(bar.x, bar.y, bar.w, hair, border);
    be.fill_rect(bar.x, bar.y + bar.h - hair, bar.w, hair, border);
    be.fill_rect(bar.x, bar.y, hair, bar.h, border);
    be.fill_rect(bar.x + bar.w - hair, bar.y, hair, bar.h, border);
    let ox = bar.x + pad;
    let oy = bar.y + pad;
    for (c, ch) in query.chars().take(BAR_COLS).enumerate() {
        be.draw_char(ox, oy, c, 0, ch, fg, false, false);
    }
    // Caret after the query.
    let caret_x = ox + query.chars().count().min(BAR_COLS) as f32 * cell_w;
    be.fill_rect(caret_x, oy, sc * logical::SEARCH_CARET_W, cell_h, fg);
    // "pos/count" right-aligned.
    let label = format!("{pos}/{count}");
    let lx = bar.x + bar.w - pad - label.chars().count() as f32 * cell_w;
    for (c, ch) in label.chars().enumerate() {
        be.draw_char(lx, oy, c, 0, ch, dim, false, false);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_sits_at_the_top_right() {
        let bar = layout(800.0, 8.0, 18.0, 1.0);
        assert!(bar.x + bar.w <= 800.0 + 0.01, "within the window");
        assert!(bar.x > 400.0, "anchored to the right half");
        assert!(bar.y >= 0.0 && bar.h > 0.0);
    }

    #[test]
    fn bar_fits_narrow_window() {
        // Window narrower than the bar's natural width: it must not overflow.
        let bar = layout(200.0, 8.0, 18.0, 1.0);
        assert!(bar.x >= 0.0 && bar.x + bar.w <= 200.0 + 0.01, "bar overflows a narrow window");
        assert!(bar.h > 0.0);
    }
}
