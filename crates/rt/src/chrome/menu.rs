//! Native context menu: laid out from `menu::rows`, drawn as fills + glyphs
//! through the shared chrome system in [`crate::chrome::theme`].
//!
//! # The defect this file was rewritten for
//!
//! *"the bottom of the menu is under the edge so I always have to make the
//! window higher before I can read it."*
//!
//! The old `layout` clamped with `anchor.1.min(win_h - h).max(0.0)`. When the
//! panel is TALLER than the window, `win_h - h` is negative, `max(0.0)` pins the
//! panel to the top, and every row past the bottom edge is simply gone — no
//! scroll, no indication, no way to reach it. A short window, or enough "Move
//! Pane to …" rows, and the menu is unusable.
//!
//! It now **scrolls**, which is what a macOS menu does: the panel takes the full
//! window height, a `▲`/`▼` cue appears at whichever end has more rows, and the
//! wheel, the arrow keys or a click on a cue move through it. [`plan`] is the
//! pure decision — which rows are on screen at a given scroll, and how far the
//! scroll may go — and BOTH [`layout`] (hence [`draw`]) and [`scroll_to_reveal`]
//! (hence the keyboard) read it, so the drawing and the hit-test cannot drift.
use crate::backend::Backend;
use crate::chrome::theme::{self, Palette};
use crate::chrome::{hit, Recti};
use crate::chrome_scale::logical;
use crate::menu::Row;

/// Cells of clear space between a label and its accelerator.
const ACCEL_GAP: usize = 4;

/// Menu geometry in window px: the panel box and each row's rect (row rects
/// share the panel width; separators get a short rect so indices line up 1:1
/// with `rows`). Rows scrolled out of view get an off-panel rect, exactly as
/// `chrome::prefs` does, so indices stay 1:1 and `hit` rejects them by position.
pub struct Geom {
    pub panel: Recti,
    pub rows: Vec<Recti>,
    // Parallel to `rows`: true where the row is a clickable action (not a
    // separator). Kept alongside the rects so `hit_row` can filter separators
    // out without needing the original `&[Row]` slice again.
    clickable: Vec<bool>,
    /// First row on screen, and one past the last.
    pub first: usize,
    pub end: usize,
    /// The largest scroll that still shows the final row.
    pub max_scroll: usize,
    /// The "more above" / "more below" cue strips, when there are more rows
    /// that way. Clicking one scrolls; see [`hit_cue`].
    pub up_cue: Option<Recti>,
    pub down_cue: Option<Recti>,
}

impl Geom {
    /// Does this menu have rows off-screen in either direction?
    pub fn scrolls(&self) -> bool {
        self.max_scroll > 0
    }
}

/// Which rows are on screen at `scroll`, and how far the scroll may go.
///
/// Pure, and the single source of truth for the scrolling menu: `heights` is one
/// entry per row, `content_h` the panel's usable interior, `cue_h` the height of
/// a `▲`/`▼` strip (which costs interior height whenever it is shown).
///
/// Returns `(first, end, up, down, max_scroll)`.
pub fn plan(heights: &[f32], content_h: f32, cue_h: f32, scroll: usize) -> (usize, usize, bool, bool, usize) {
    let n = heights.len();
    let total: f32 = heights.iter().sum();
    if n == 0 || total <= content_h {
        return (0, n, false, false, 0);
    }
    // How many rows fit from `s`, given `avail` px.
    let fits = |s: usize, avail: f32| {
        let mut used = 0.0;
        let mut i = s;
        while i < n && used + heights[i] <= avail {
            used += heights[i];
            i += 1;
        }
        i
    };
    // The largest scroll that still reaches the last row: walk backwards from
    // the end, taking rows while they fit (allowing for the up-cue that any
    // non-zero scroll implies).
    let mut max_scroll = n;
    let mut used = 0.0;
    for s in (0..n).rev() {
        let cue = if s > 0 { cue_h } else { 0.0 };
        if used + heights[s] + cue > content_h {
            break;
        }
        used += heights[s];
        max_scroll = s;
    }
    let max_scroll = max_scroll.min(n.saturating_sub(1));
    let scroll = scroll.min(max_scroll);
    let up = scroll > 0;
    let mut avail = content_h - if up { cue_h } else { 0.0 };
    let mut end = fits(scroll, avail);
    if end < n {
        // A down-cue is needed, and it costs interior height of its own.
        avail -= cue_h;
        end = fits(scroll, avail).max(scroll + 1);
    }
    (scroll, end, up, end < n, max_scroll)
}

/// Per-row heights: a full row for anything with a label, a short one for the
/// separators (the only rows with an empty label).
fn heights(rows: &[Row], cell_h: f32, sc: f32) -> Vec<f32> {
    let row_h = cell_h + sc * logical::PANEL_ROW_PAD;
    let sep_h = sc * logical::PANEL_SEP_H;
    rows.iter().map(|r| if r.label.is_empty() { sep_h } else { row_h }).collect()
}

/// Lay the menu out anchored at `anchor`, clamped fully on-screen — and, when it
/// cannot fit however it is placed, scrolled to `scroll`.
///
/// `sc` is the display's backing factor. `hit_row` tests the rects this
/// produces, so scaling the layout scales the hit-test with it — there is no
/// separate copy of the geometry that could be left behind.
pub fn layout(
    rows: &[Row],
    anchor: (f32, f32),
    cell_w: f32,
    cell_h: f32,
    win_w: f32,
    win_h: f32,
    scroll: usize,
    sc: f32,
) -> Geom {
    let pad_x = sc * logical::PANEL_PAD_X;
    let pad_y = sc * logical::PANEL_PAD_Y;
    let cue_h = cell_h + sc * logical::PANEL_ROW_PAD;
    // Width = widest "label   accel" in cells, plus padding.
    let cols = rows
        .iter()
        .map(|r| {
            let a = r.accel.as_deref().map(|s| s.chars().count() + ACCEL_GAP).unwrap_or(0);
            r.label.chars().count() + a
        })
        .max()
        .unwrap_or(8);
    let w = (cols as f32 * cell_w + pad_x * 2.0).min(win_w);
    let hs = heights(rows, cell_h, sc);
    let natural = hs.iter().sum::<f32>() + pad_y * 2.0;

    // Fits: the panel is its natural height and sits where the anchor asks,
    // shifted just enough to stay on screen. Does not fit: it takes the whole
    // window height and scrolls inside it.
    let (h, y) = if natural <= win_h {
        (natural, anchor.1.min(win_h - natural).max(0.0))
    } else {
        (win_h, 0.0)
    };
    let x = anchor.0.min(win_w - w).max(0.0);
    let (first, end, up, down, max_scroll) = plan(&hs, h - pad_y * 2.0, cue_h, scroll);

    let up_cue = up.then_some(Recti { x, y: y + pad_y, w, h: cue_h });
    let down_cue = down.then_some(Recti { x, y: y + h - pad_y - cue_h, w, h: cue_h });
    let mut rrects = vec![Recti { x: -1.0, y: -1.0, w: 0.0, h: 0.0 }; rows.len()];
    let mut cy = y + pad_y + if up { cue_h } else { 0.0 };
    for i in first..end {
        rrects[i] = Recti { x, y: cy, w, h: hs[i] };
        cy += hs[i];
    }
    let clickable = rows.iter().map(|r| r.action.is_some()).collect();
    Geom {
        panel: Recti { x, y, w, h },
        rows: rrects,
        clickable,
        first,
        end,
        max_scroll,
        up_cue,
        down_cue,
    }
}

/// The clickable row at `p`, or `None` for separators / scrolled-away rows /
/// outside the panel.
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

/// `-1` / `+1` when `p` is on the "more above" / "more below" cue — so the cues
/// are buttons, not just decoration, for anyone without a wheel.
pub fn hit_cue(g: &Geom, p: (f32, f32)) -> Option<i32> {
    if g.up_cue.is_some_and(|r| r.contains(p)) {
        return Some(-1);
    }
    if g.down_cue.is_some_and(|r| r.contains(p)) {
        return Some(1);
    }
    None
}

/// The next clickable row `dir` steps from `cur` (wrapping), for arrow-key
/// navigation. `None` when the menu has no clickable row at all.
pub fn next_row(rows: &[Row], cur: Option<usize>, dir: i32) -> Option<usize> {
    let live: Vec<usize> =
        rows.iter().enumerate().filter(|(_, r)| r.action.is_some() && r.enabled).map(|(i, _)| i).collect();
    if live.is_empty() {
        return None;
    }
    let at = match cur {
        Some(c) => live.iter().position(|i| *i == c).map(|p| p as i32 + dir).unwrap_or(0),
        None if dir > 0 => 0,
        None => live.len() as i32 - 1,
    };
    Some(live[at.rem_euclid(live.len() as i32) as usize])
}

/// The scroll that brings row `want` on screen, moving as little as possible.
///
/// Reads the same [`plan`] `layout` does, so what the keyboard reveals is
/// exactly what the pointer can hit.
pub fn scroll_to_reveal(
    rows: &[Row],
    want: usize,
    scroll: usize,
    cell_h: f32,
    win_h: f32,
    sc: f32,
) -> usize {
    let hs = heights(rows, cell_h, sc);
    let cue_h = cell_h + sc * logical::PANEL_ROW_PAD;
    let content_h = win_h - sc * logical::PANEL_PAD_Y * 2.0;
    let (first, end, _, _, max_scroll) = plan(&hs, content_h, cue_h, scroll);
    if want < first {
        return want.min(max_scroll);
    }
    if want < end {
        return first;
    }
    // Below the fold: advance until `want` is the last row that fits.
    let mut s = first;
    while s < max_scroll {
        s += 1;
        let (_, e, _, _, _) = plan(&hs, content_h, cue_h, s);
        if want < e {
            return s;
        }
    }
    max_scroll
}

/// Draw the panel, the scroll cues, the hovered highlight, labels, accelerators
/// and separators.
pub fn draw(
    be: &mut dyn Backend,
    g: &Geom,
    rows: &[Row],
    hover: Option<usize>,
    pal: &Palette,
    cell_w: f32,
    cell_h: f32,
    sc: f32,
) {
    let pad_x = sc * logical::PANEL_PAD_X;
    theme::panel(be, g.panel, pal, sc);
    for (i, (row, rect)) in rows.iter().zip(&g.rows).enumerate() {
        if i < g.first || i >= g.end {
            continue; // scrolled out; layout parked it off-panel
        }
        // A separator is the only row with an empty label (matches `layout`); an
        // info row like the version footer has a label but no action and draws as
        // dimmed text, not a rule.
        if row.label.is_empty() {
            theme::separator(be, *rect, pal, sc);
            continue;
        }
        let hot = hover == Some(i) && row.enabled && row.action.is_some();
        if hot {
            theme::row_highlight(be, *rect, pal.sel, sc);
        }
        let (colr, acol) = roles(row, hot, pal);
        let oy = theme::text_y(*rect, cell_h);
        theme::text(be, rect.x + pad_x, oy, &row.label, colr, false);
        if let Some(acc) = &row.accel {
            theme::text_right(be, rect.x + rect.w - pad_x, oy, acc, cell_w, acol, false);
        }
    }
    // "More this way" cues, centred — in the accent, because they are the one
    // thing in the panel you can click that is not a row.
    for (cue, glyph) in [(g.up_cue, '▲'), (g.down_cue, '▼')] {
        let Some(r) = cue else { continue };
        theme::text(be, r.x + (r.w - cell_w) * 0.5, theme::text_y(r, cell_h), &glyph.to_string(), pal.accent, false);
    }
}

/// The (label, accelerator) colours for one row. Pure, so what the menu is
/// *saying* with colour can be asserted rather than eyeballed.
///
/// Three ideas, and they are the same three the manual uses:
///
/// * `text` is the thing you are reading — a live command's name.
/// * `accent` is the thing you press. In the manual that is the key column; here
///   it is the accelerator. A menu whose labels and shortcuts were both grey (the
///   accelerator was merely `dim`) had no colour in it at all, which is what
///   *"there is also not use of color in the manual nor menu"* was about.
/// * `dim` is metadata — a row with no action, like the version footer.
///
/// A disabled row drops to `off` **wholesale**, accelerator included: "you
/// cannot press this" has to beat "this is what you would press".
fn roles(row: &Row, hot: bool, pal: &Palette) -> (crate::render::Color, crate::render::Color) {
    if hot {
        // On the selection bar there is exactly one legible colour, and the
        // palette guarantees it against the bar rather than against the panel.
        return (pal.sel_text, pal.sel_text);
    }
    if !row.enabled {
        return (pal.off, pal.off);
    }
    if row.action.is_none() {
        return (pal.dim, pal.dim); // an info row (the version footer)
    }
    (pal.text, pal.accent)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::menu;
    use rt_config::{ChromeTheme, Keymap};

    fn sample() -> Vec<Row> {
        menu::rows(&Keymap::default(), true, None, &[])
    }

    fn pal() -> Palette {
        Palette::derive([0xd0, 0xd0, 0xd8], [0x10, 0x10, 0x14], [0x5c, 0x5c, 0xff], ChromeTheme::Tinted, 1.0)
    }

    /// *"There is also not use of color in the manual nor menu."* A label, its
    /// accelerator, a disabled row and an info row must be four visibly
    /// different things — and the accelerator must be the accent, matching the
    /// manual's key column, so "accent means the thing you press" is one rule
    /// across both panels rather than two local decisions.
    #[test]
    fn a_menu_row_says_four_different_things_in_colour() {
        let p = pal();
        let rgb = |c: crate::render::Color| [c.0, c.1, c.2];
        let mk = |enabled: bool, action: bool| Row {
            label: "Split Horizontally".into(),
            accel: Some("Ctrl+Shift+O".into()),
            action: action.then_some(menu::RowAction::Do(rt_config::Action::SplitHoriz)),
            enabled,
        };
        let (label, accel) = roles(&mk(true, true), false, &p);
        assert_eq!(rgb(accel), rgb(p.accent), "an accelerator is the thing you press");
        assert_eq!(rgb(label), rgb(p.text), "a label is what you read");
        assert!(
            theme::contrast(rgb(label), rgb(accel)) >= theme::FLOOR_ROLE_SPLIT - 0.01,
            "label and accelerator must be tellable apart"
        );
        // Disabled beats "pressable": the whole row goes quiet, accelerator too.
        let (dl, da) = roles(&mk(false, true), false, &p);
        assert_eq!(rgb(dl), rgb(p.off));
        assert_eq!(rgb(da), rgb(p.off), "a shortcut you cannot use must not still shout");
        // An info row is metadata, not a command.
        assert_eq!(rgb(roles(&mk(true, false), false, &p).0), rgb(p.dim));
        // On the selection bar there is one colour, held against the bar.
        let (hl, ha) = roles(&mk(true, true), true, &p);
        assert_eq!(rgb(hl), rgb(p.sel_text));
        assert_eq!(rgb(ha), rgb(p.sel_text));
        // The four are a HIERARCHY, not just four colours: a live label reads
        // louder than a shortcut, and a disabled row quieter than either. (That
        // each one clears its own contrast floor is asserted where the floors
        // live — `theme::tests::every_theme_is_legible_over_every_scheme` — and
        // against the panel AS COMPOSITED, which is the only panel there is.)
        let body = rgb(p.panel);
        let loudness = |c| theme::contrast(rgb(c), body);
        assert!(loudness(p.text) > loudness(p.off), "a live label beats a disabled one");
        assert!(loudness(p.dim) > loudness(p.off), "an info row beats a disabled one");
    }

    #[test]
    fn panel_clamps_onto_screen() {
        let rows = sample();
        // Anchor near the bottom-right corner: the panel must shift fully on-screen.
        let g = layout(&rows, (795.0, 795.0), 8.0, 18.0, 800.0, 800.0, 0, 1.0);
        assert!(g.panel.x + g.panel.w <= 800.0 + 0.01);
        assert!(g.panel.y + g.panel.h <= 800.0 + 0.01);
        assert!(!g.scrolls(), "it fits, so there is nothing to scroll");
    }

    /// THE BUG: a menu taller than the window must still be READABLE and
    /// CLICKABLE to its last row. Pinning it to the top edge left the tail off
    /// the bottom with no way to reach it — "the bottom of the menu is under the
    /// edge so I always have to make the window higher before I can read it".
    #[test]
    fn every_row_of_a_too_tall_menu_can_be_reached() {
        let rows = sample();
        let (win_w, win_h) = (800.0_f32, 300.0_f32);
        let mut seen = vec![false; rows.len()];
        let mut scroll = 0;
        loop {
            let g = layout(&rows, (795.0, 595.0), 8.0, 18.0, win_w, win_h, scroll, 1.0);
            assert!(g.scrolls(), "this case only means anything when the menu overflows");
            // Nothing drawn this frame may fall outside the window.
            for i in g.first..g.end {
                let r = g.rows[i];
                assert!(
                    r.y >= -0.01 && r.y + r.h <= win_h + 0.01,
                    "row {i} ({:?}) is off the window at y={}..{}",
                    rows[i].label,
                    r.y,
                    r.y + r.h,
                );
                // And every clickable row on screen must actually hit-test.
                if rows[i].action.is_some() {
                    assert_eq!(hit_row(&g, (r.x + 2.0, r.y + r.h * 0.5)), Some(i), "row {i} unhittable");
                }
                seen[i] = true;
            }
            if scroll >= g.max_scroll {
                break;
            }
            scroll += 1;
        }
        for (i, ok) in seen.iter().enumerate() {
            assert!(ok, "row {i} ({:?}) can never be reached at any scroll", rows[i].label);
        }
    }

    /// The keyboard reaches the last row too, and its scroll agrees with the
    /// layout the pointer hit-tests.
    #[test]
    fn arrowing_down_reveals_the_last_row_of_a_too_tall_menu() {
        let rows = sample();
        let (win_h, cell_h, sc) = (300.0_f32, 18.0_f32, 1.0_f32);
        let last = rows.iter().rposition(|r| r.action.is_some()).unwrap();
        let scroll = scroll_to_reveal(&rows, last, 0, cell_h, win_h, sc);
        let g = layout(&rows, (10.0, 10.0), 8.0, cell_h, 800.0, win_h, scroll, sc);
        assert!(last >= g.first && last < g.end, "the last row must be on screen");
        let r = g.rows[last];
        assert!(r.y + r.h <= win_h + 0.01, "and inside the window");
        assert_eq!(hit_row(&g, (r.x + 2.0, r.y + r.h * 0.5)), Some(last));
        // Walking the whole menu with the arrow keys visits every live row.
        let mut cur = None;
        for _ in 0..rows.len() * 2 {
            cur = next_row(&rows, cur, 1);
            let i = cur.unwrap();
            assert!(rows[i].action.is_some() && rows[i].enabled, "arrows must land on a live row");
        }
    }

    /// The cues are only there when they mean something, and they are buttons.
    #[test]
    fn scroll_cues_appear_only_at_the_ends_they_belong_to() {
        let rows = sample();
        let g0 = layout(&rows, (10.0, 10.0), 8.0, 18.0, 800.0, 300.0, 0, 1.0);
        assert!(g0.up_cue.is_none(), "nothing above the top");
        let dc = g0.down_cue.expect("more below at scroll 0");
        assert_eq!(hit_cue(&g0, (dc.x + dc.w * 0.5, dc.y + dc.h * 0.5)), Some(1));
        let gmax = layout(&rows, (10.0, 10.0), 8.0, 18.0, 800.0, 300.0, g0.max_scroll, 1.0);
        assert!(gmax.down_cue.is_none(), "nothing below the end");
        let uc = gmax.up_cue.expect("more above at the end");
        assert_eq!(hit_cue(&gmax, (uc.x + uc.w * 0.5, uc.y + uc.h * 0.5)), Some(-1));
        assert_eq!(gmax.end, rows.len(), "the last row is on screen at max scroll");
        // Cues sit inside the panel, never over a row.
        for cue in [g0.down_cue, gmax.up_cue].into_iter().flatten() {
            assert!(cue.y >= gmax.panel.y - 0.01 && cue.y + cue.h <= gmax.panel.y + gmax.panel.h + 0.01);
            assert!(hit_row(&gmax, (cue.x + 2.0, cue.y + cue.h * 0.5)).is_none(), "a cue is not a row");
        }
    }

    /// Whatever the window and the backing factor, the panel is inside the
    /// window and every drawn row is inside the panel.
    #[test]
    fn the_panel_stays_inside_the_window_at_every_size_and_scale() {
        let rows = sample();
        for &sc in &[1.0_f32, 2.0, 3.0] {
            for &(w, h) in &[(200.0_f32, 120.0_f32), (400.0, 300.0), (1920.0, 1080.0), (640.0, 480.0)] {
                for &anchor in &[(0.0_f32, 0.0_f32), (w, h), (w * 0.5, h * 0.5)] {
                    let g = layout(&rows, anchor, 8.0 * sc, 18.0 * sc, w, h, 0, sc);
                    let ctx = format!("{w}x{h} @{sc}x anchor {anchor:?}");
                    assert!(g.panel.x >= -0.01 && g.panel.y >= -0.01, "{ctx}");
                    assert!(g.panel.x + g.panel.w <= w + 0.01, "{ctx}: panel right edge");
                    assert!(g.panel.y + g.panel.h <= h + 0.01, "{ctx}: panel bottom edge");
                    for i in g.first..g.end {
                        let r = g.rows[i];
                        assert!(r.x >= g.panel.x - 0.01 && r.x + r.w <= g.panel.x + g.panel.w + 0.01, "{ctx}");
                        assert!(r.y >= g.panel.y - 0.01 && r.y + r.h <= g.panel.y + g.panel.h + 0.01, "{ctx}");
                    }
                }
            }
        }
    }

    #[test]
    fn hit_row_skips_separators() {
        let rows = sample();
        let g = layout(&rows, (10.0, 10.0), 8.0, 18.0, 800.0, 600.0, 0, 1.0);
        // The 3rd row in a no-url menu is the separator after Copy/Paste.
        let sep_idx = rows.iter().position(|r| r.action.is_none()).unwrap();
        let mid = (g.rows[sep_idx].x + 2.0, g.rows[sep_idx].y + g.rows[sep_idx].h / 2.0);
        assert_eq!(hit_row(&g, mid), None, "clicking a separator selects nothing");
        // A real row hits.
        let copy_idx = rows.iter().position(|r| r.label == "Copy").unwrap();
        let cm = (g.rows[copy_idx].x + 2.0, g.rows[copy_idx].y + g.rows[copy_idx].h / 2.0);
        assert_eq!(hit_row(&g, cm), Some(copy_idx));
    }

    /// Label and accelerator never collide: the panel is wide enough for the
    /// widest pair plus the gap between the columns.
    #[test]
    fn the_accelerator_column_never_touches_the_label_column() {
        let rows = sample();
        let (cw, sc) = (8.0_f32, 1.0_f32);
        let g = layout(&rows, (10.0, 10.0), cw, 18.0, 1600.0, 900.0, 0, sc);
        let pad = sc * logical::PANEL_PAD_X;
        for (i, row) in rows.iter().enumerate() {
            let Some(acc) = &row.accel else { continue };
            let r = g.rows[i];
            let label_end = r.x + pad + row.label.chars().count() as f32 * cw;
            let accel_start = r.x + r.w - pad - acc.chars().count() as f32 * cw;
            assert!(
                accel_start - label_end >= (ACCEL_GAP as f32 - 1.0) * cw,
                "{:?} / {acc:?}: only {} px between the columns",
                row.label,
                accel_start - label_end
            );
        }
    }

    /// A drawn menu must not blow up on a degenerate window — the case a resize
    /// to nothing, or a one-row-tall window, produces.
    #[test]
    fn a_tiny_window_still_produces_a_sane_layout() {
        let rows = sample();
        for &(w, h) in &[(1.0_f32, 1.0_f32), (40.0, 20.0), (10.0, 400.0)] {
            let g = layout(&rows, (0.0, 0.0), 8.0, 18.0, w, h, 0, 1.0);
            assert!(g.panel.w >= 0.0 && g.panel.h >= 0.0);
            assert!(g.panel.x + g.panel.w <= w + 0.01, "{w}x{h}");
            assert!(g.panel.y + g.panel.h <= h + 0.01, "{w}x{h}");
            assert!(g.end <= rows.len());
        }
    }

    /// Draw and hit derive from the SAME rects — the invariant the colour
    /// picker's unclickable-on-Retina bug came from breaking. `draw` iterates
    /// `g.first..g.end` over `g.rows`; so must every hit.
    #[test]
    fn what_is_drawn_is_exactly_what_can_be_hit() {
        let rows = sample();
        for &sc in &[1.0_f32, 2.0] {
            for scroll in 0..4 {
                let g = layout(&rows, (10.0, 10.0), 8.0 * sc, 18.0 * sc, 700.0, 260.0, scroll, sc);
                for (i, r) in g.rows.iter().enumerate() {
                    let drawn = i >= g.first && i < g.end;
                    let centre = (r.x + r.w * 0.5, r.y + r.h * 0.5);
                    let hittable = hit(&g.rows, centre) == Some(i);
                    assert_eq!(drawn, hittable, "row {i} at scroll {scroll} @{sc}x: drawn={drawn}");
                }
            }
        }
    }

    /// The palette a menu draws with is the shared one — no local literals.
    #[test]
    fn the_menu_draws_from_the_shared_palette() {
        let p = pal();
        assert!(theme::contrast([p.text.0, p.text.1, p.text.2], [p.panel.0, p.panel.1, p.panel.2]) >= 7.0);
    }
}
