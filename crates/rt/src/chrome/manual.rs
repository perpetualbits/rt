//! Native manual overlay: a centered panel that **word-wraps** `manual::MANUAL`
//! to the panel width and scrolls by (wrapped) rows. A version line heads the
//! text. Scroll position lives in `Active.manual_scroll`.
use crate::backend::Backend;
use crate::chrome::Recti;
use crate::manual::manual_lines;
use crate::render::Color;

/// Manual panel geometry plus the wrapped lines to draw (version header first).
pub struct Geom {
    pub panel: Recti,
    pub rows: usize,        // visible wrapped rows in the panel
    pub lines: Vec<String>, // the whole manual, wrapped to the panel width
    pub total: usize,       // lines.len()
}

const PAD: f32 = 12.0;

/// How many character columns fit inside the panel's padded interior.
pub fn visible_cols(panel_w: f32, cell_w: f32) -> usize {
    let inner = panel_w - PAD * 2.0;
    if inner <= 0.0 || cell_w <= 0.0 {
        return 0;
    }
    (inner / cell_w).floor() as usize
}

/// Word-wrap the manual to `cols` columns, headed by the running version. Lines
/// that already fit are emitted unchanged (so the aligned key/description columns
/// and code examples stay aligned); only longer lines are wrapped, greedily by
/// word, hard-breaking any single word wider than `cols`.
pub fn wrapped(cols: usize) -> Vec<String> {
    let mut out = vec![crate::version_string(), String::new()];
    if cols == 0 {
        return out;
    }
    let hard_break = |out: &mut Vec<String>, word: &str| -> String {
        // Split an over-long word into full-width chunks; return the remainder.
        let mut chars: Vec<char> = word.chars().collect();
        while chars.len() > cols {
            out.push(chars[..cols].iter().collect());
            chars.drain(..cols);
        }
        chars.into_iter().collect()
    };
    for line in manual_lines() {
        if line.chars().count() <= cols {
            out.push(line.to_string());
            continue;
        }
        let mut cur = String::new();
        for word in line.split_whitespace() {
            let wl = word.chars().count();
            if cur.is_empty() {
                cur = if wl <= cols { word.to_string() } else { hard_break(&mut out, word) };
            } else if cur.chars().count() + 1 + wl <= cols {
                cur.push(' ');
                cur.push_str(word);
            } else {
                out.push(std::mem::take(&mut cur));
                cur = if wl <= cols { word.to_string() } else { hard_break(&mut out, word) };
            }
        }
        if !cur.is_empty() {
            out.push(cur);
        }
    }
    out
}

/// A panel ~85% of the window (a touch wider than before so fewer lines wrap).
pub fn layout(win_w: f32, win_h: f32, cell_w: f32, cell_h: f32) -> Geom {
    let w = (win_w * 0.85).min(900.0);
    let h = win_h * 0.85;
    let panel = Recti { x: (win_w - w) / 2.0, y: (win_h - h) / 2.0, w, h };
    let rows = (((h - PAD * 2.0) / cell_h).floor() as usize).max(1);
    let lines = wrapped(visible_cols(w, cell_w));
    let total = lines.len();
    Geom { panel, rows, lines, total }
}

/// Clamp a scroll offset so the last page stays on-screen.
pub fn clamp_scroll(scroll: usize, g: &Geom) -> usize {
    let max = g.total.saturating_sub(g.rows);
    scroll.min(max)
}

/// Draw the panel, the visible wrapped-line slice, and a scrollbar thumb.
pub fn draw(be: &mut dyn Backend, g: &Geom, scroll: usize, _cell_w: f32, _cell_h: f32) {
    let bg = Color::rgb(0x18, 0x1a, 0x1f);
    let border = Color::rgb(0x50, 0x54, 0x60);
    let fg = Color::rgb(0xd0, 0xd2, 0xda);
    let dim = Color::rgb(0x80, 0x84, 0x90);
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
    for (r, line) in g.lines.iter().skip(scroll).take(g.rows).enumerate() {
        // The version header (line 0) is dimmed; everything else is body text.
        let colr = if scroll + r == 0 { dim } else { fg };
        for (c, ch) in line.chars().enumerate() {
            be.draw_char(ox, oy, c, r, ch, colr, false, false);
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
        assert_eq!(visible_cols(10.0, 8.0), 0); // narrower than padding → 0, no underflow
    }

    #[test]
    fn wrapping_keeps_every_line_within_the_width() {
        // At a deliberately narrow width, NO wrapped line exceeds it — the bug was
        // long lines running off the panel instead of wrapping.
        let cols = 50;
        let lines = wrapped(cols);
        for l in &lines {
            assert!(l.chars().count() <= cols, "line wider than {cols}: {l:?}");
        }
        // And the manual genuinely has lines that needed wrapping.
        assert!(manual_lines().any(|l| l.chars().count() > cols));
    }

    #[test]
    fn version_heads_the_manual() {
        let lines = wrapped(80);
        assert!(lines[0].starts_with("rt "), "first line names the build: {:?}", lines[0]);
        assert!(lines[0].contains(env!("CARGO_PKG_VERSION")));
    }
}
