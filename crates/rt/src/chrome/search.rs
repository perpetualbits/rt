//! Native scrollback-search bar: a slim top-right box with the query and a hit
//! counter. Typing/navigation are handled by main.rs via the existing engine.
//! Colours and shape come from the shared chrome system in
//! [`crate::chrome::theme`], so the bar is visibly the same family as the menu
//! and the manual rather than its own shade of grey.
use crate::backend::Backend;
use crate::chrome::theme::{self, Palette};
use crate::chrome::Recti;
use crate::chrome_scale::logical;

const BAR_COLS: usize = 32; // query field width in cells
/// Cells reserved for the " 12/34 " counter at the right.
const COUNT_COLS: usize = 8;

/// Bar rect pinned to the top-right corner.
///
/// `sc` is the display's backing factor, applied to the flat padding and the
/// corner standoff (the width/height terms are already cell-derived, so they
/// scale with the glyph). `hit`-testing of the bar goes through the rect this
/// returns, so there is no second copy of the geometry to keep in step.
pub fn layout(win_w: f32, win_h: f32, cell_w: f32, cell_h: f32, sc: f32) -> Recti {
    let pad_x = sc * logical::PANEL_PAD_X;
    let pad_y = sc * logical::PANEL_PAD_Y;
    let inset = sc * logical::SEARCH_INSET;
    let w = ((BAR_COLS + COUNT_COLS) as f32 * cell_w + pad_x * 2.0).min(win_w);
    let h = (cell_h + pad_y * 2.0).min(win_h);
    Recti { x: (win_w - w - inset).max(0.0), y: inset.min((win_h - h).max(0.0)), w, h }
}

/// Draw the bar: panel, query text, caret, and "pos/count".
pub fn draw(
    be: &mut dyn Backend,
    bar: Recti,
    query: &str,
    pos: usize,
    count: usize,
    pal: &Palette,
    cell_w: f32,
    cell_h: f32,
    sc: f32,
) {
    let pad_x = sc * logical::PANEL_PAD_X;
    let inset = sc * logical::PANEL_SEL_INSET;
    theme::panel(be, bar, pal, sc);
    // The query sits in a recessed field, so the bar reads as "type here" rather
    // than as a label that happens to be editable.
    let fw = (BAR_COLS as f32 * cell_w + (pad_x - inset) * 2.0).min(bar.w - inset * 2.0);
    if fw > 0.0 && bar.h > inset * 2.0 {
        theme::rounded(
            be,
            bar.x + inset,
            bar.y + inset,
            fw,
            bar.h - inset * 2.0,
            sc * logical::PANEL_SEL_RADIUS,
            pal.field,
        );
    }
    let ox = bar.x + pad_x;
    let oy = theme::text_y(bar, cell_h);
    let shown: String = query.chars().take(BAR_COLS).collect();
    theme::text(be, ox, oy, &shown, pal.text, false);
    // Caret after the query, in the accent so it reads as a live insertion point.
    let caret_x = ox + shown.chars().count() as f32 * cell_w;
    be.fill_rect(caret_x, oy, sc * logical::SEARCH_CARET_W, cell_h, pal.accent);
    // "pos/count" right-aligned, quiet. A search with no hits says so in the
    // disabled colour rather than showing a confident "0/0" in body text.
    let label = format!("{pos}/{count}");
    let colr = if count == 0 { pal.off } else { pal.dim };
    theme::text_right(be, bar.x + bar.w - pad_x, oy, &label, cell_w, colr, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_sits_at_the_top_right() {
        let bar = layout(800.0, 600.0, 8.0, 18.0, 1.0);
        assert!(bar.x + bar.w <= 800.0 + 0.01, "within the window");
        assert!(bar.x > 400.0, "anchored to the right half");
        assert!(bar.y >= 0.0 && bar.h > 0.0);
    }

    /// A window smaller than the bar in either axis: it must stay inside,
    /// including on a 2x display where every flat value has doubled.
    #[test]
    fn the_bar_stays_inside_a_window_of_any_size() {
        for &sc in &[1.0_f32, 2.0, 3.0] {
            for &(w, h) in &[(200.0_f32, 400.0_f32), (60.0, 30.0), (1.0, 1.0), (1920.0, 1080.0)] {
                let bar = layout(w, h, 8.0 * sc, 18.0 * sc, sc);
                let ctx = format!("{w}x{h} @{sc}x");
                assert!(bar.x >= -0.01 && bar.y >= -0.01, "{ctx}");
                assert!(bar.x + bar.w <= w + 0.01, "{ctx}: overflows the width");
                assert!(bar.y + bar.h <= h + 0.01, "{ctx}: overflows the height");
            }
        }
    }
}
