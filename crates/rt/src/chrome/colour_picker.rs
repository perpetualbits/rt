//! Native HSV colour picker: a saturation/value square + a hue strip, drawn as
//! `fill_rect`s and glyphs so it works on both backends (no AA primitives).
//! Pure `layout`/`hit`/`draw` split like `chrome::prefs`, plus the HSV↔RGB maths,
//! all unit-testable with no X server.
//!
//! State (`PickerState`) keeps H, S and V — not RGB — so dragging value or
//! saturation to an edge never loses the hue (the classic picker bug).

use crate::backend::Backend;
use crate::chrome::theme::{self, Palette};
use crate::chrome::Recti;
use crate::chrome_scale::logical;
use crate::render::Color;

/// Which colour the picker edits. Swatch index in the prefs row: `0 = Fg`,
/// `1 = Bg`, `2 + i = Palette(i)`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Slot {
    Fg,
    Bg,
    Palette(usize),
}

impl Slot {
    /// The swatch at index `i` in the prefs `[fg, bg, palette…]` row.
    pub fn from_swatch_index(i: usize) -> Slot {
        match i {
            0 => Slot::Fg,
            1 => Slot::Bg,
            n => Slot::Palette(n - 2),
        }
    }

    /// Title shown above the picker.
    pub fn label(self) -> String {
        match self {
            Slot::Fg => "Foreground".into(),
            Slot::Bg => "Background".into(),
            Slot::Palette(i) => format!("Palette {i}"),
        }
    }
}

/// Which control a drag is tracking.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Drag {
    Sv,
    Hue,
}

/// Live picker state held on `Active`. `h ∈ [0,360)`, `s,v ∈ [0,1]`.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PickerState {
    pub slot: Slot,
    pub h: f32,
    pub s: f32,
    pub v: f32,
    pub drag: Option<Drag>,
}

impl PickerState {
    /// Open the picker on `slot`, seeding H/S/V from that slot's current colour.
    pub fn new(slot: Slot, rgb: [u8; 3]) -> PickerState {
        let (h, s, v) = rgb_to_hsv(rgb);
        PickerState { slot, h, s, v, drag: None }
    }

    /// The current colour as RGB.
    pub fn rgb(&self) -> [u8; 3] {
        hsv_to_rgb(self.h, self.s, self.v)
    }
}

// --- HSV ↔ RGB ------------------------------------------------------------

/// HSV (`h ∈ [0,360)`, `s,v ∈ [0,1]`) to 8-bit sRGB. Standard hexcone mapping.
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> [u8; 3] {
    let h = h.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s; // chroma
    let hp = h / 60.0; // 0..6 sextant
    let x = c * (1.0 - (hp % 2.0 - 1.0).abs());
    let (r1, g1, b1) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x), // 5 and the h→360 edge
    };
    let m = v - c;
    let to = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
    [to(r1), to(g1), to(b1)]
}

/// 8-bit sRGB to HSV (`h ∈ [0,360)`, `s,v ∈ [0,1]`). Hue is 0 for greys.
pub fn rgb_to_hsv(rgb: [u8; 3]) -> (f32, f32, f32) {
    let r = rgb[0] as f32 / 255.0;
    let g = rgb[1] as f32 / 255.0;
    let b = rgb[2] as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let d = max - min;
    let v = max;
    let s = if max <= 0.0 { 0.0 } else { d / max };
    let h = if d <= 0.0 {
        0.0
    } else if max == r {
        60.0 * ((g - b) / d).rem_euclid(6.0)
    } else if max == g {
        60.0 * ((b - r) / d + 2.0)
    } else {
        60.0 * ((r - g) / d + 4.0)
    };
    (h.rem_euclid(360.0), s, v)
}

// --- geometry -------------------------------------------------------------

/// Panel and control geometry in window px.
pub struct Geom {
    pub panel: Recti,
    pub sv: Recti,      // saturation (x) / value (y) square
    pub hue: Recti,     // vertical hue strip
    pub preview: Recti, // current-colour swatch
    pub hex: (f32, f32), // hex readout text origin
    pub close: Recti,   // "Done (Esc)" button
}

const SV_CELLS: f32 = 8.0; // SV square side, in cell-heights (DPI-aware)

/// Lay the picker out centred and **fitted** to the window.
///
/// The old version clamped only the panel's own height (`.min(win_h)`) while
/// laying its contents out at full size, so in a short window the SV square, the
/// hex readout and the Done button ran out through the bottom edge — the same
/// class of fault as the context menu's. Here the SV square is the elastic part:
/// every other element has a fixed height, so the square absorbs whatever space
/// is left, in BOTH axes, and the panel is sized from the result rather than
/// truncated to fit.
///
/// `hit`, `sv_at` and `hue_at` all read the `Geom` this returns, so the click
/// surface can never fall behind the drawing.
pub fn layout(cell_w: f32, cell_h: f32, win_w: f32, win_h: f32, sc: f32) -> Geom {
    // Padding is a fraction of the window before it is anything else: at 3x on a
    // tiny window the scaled padding alone can exceed the window, and a panel
    // whose padding does not fit has nowhere to put its contents.
    let pad_x = (sc * logical::PANEL_PAD_X).min(win_w * 0.2).max(0.0);
    let pad_y = (sc * logical::PANEL_PAD_Y).min(win_h * 0.15).max(0.0);
    let inner_w_avail = (win_w - pad_x * 2.0).max(1.0);
    let hue_w = ((cell_w * 2.0).max(sc * logical::PICKER_HUE_MIN)).min(inner_w_avail * 0.25).max(1.0);
    let title_h = cell_h + sc * logical::PANEL_ROW_PAD;
    let sw_h = cell_h; // preview swatch
    let hex_h = cell_h;
    let close_h = cell_h + sc * logical::PANEL_ROW_PAD;
    // The stack, squeezed to fit in three stages. The SV square is elastic and
    // gives way first; then the gaps between the groups close; then, only in a
    // window too small to show a picker at all, the fixed rows themselves shrink
    // in proportion. Nothing is ever laid out beyond the panel, which is the
    // fault this replaced: the old version clamped the panel's height and left
    // the hex readout and the Done button outside it.
    let avail = (win_h - pad_y * 2.0).max(1.0);
    let rows_h = title_h + sw_h + hex_h + close_h;
    let mut gap = sc * logical::PANEL_GAP;
    if rows_h + gap * 4.0 + 1.0 > avail {
        gap = ((avail - rows_h - 1.0) / 4.0).max(0.0);
    }
    let squeeze = if rows_h + gap * 4.0 + 1.0 > avail { ((avail - 1.0) / rows_h).max(0.0) } else { 1.0 };
    let (title_h, sw_h, hex_h, close_h) =
        (title_h * squeeze, sw_h * squeeze, hex_h * squeeze, close_h * squeeze);

    let want = (cell_h * SV_CELLS).max(sc * logical::PICKER_SV_MIN);
    let sv_side = want
        .min(inner_w_avail - gap - hue_w)
        .min(avail - rows_h * squeeze - gap * 4.0)
        .max(1.0);
    let fixed = pad_y * 2.0 + title_h + gap * 4.0 + sw_h + hex_h + close_h;

    let inner_w = sv_side + gap + hue_w;
    let panel_w = (inner_w + pad_x * 2.0).min(win_w);
    let panel_h = (fixed + sv_side).min(win_h);
    let px = ((win_w - panel_w) * 0.5).max(0.0);
    let py = ((win_h - panel_h) * 0.5).max(0.0);
    let top = py + pad_y + title_h + gap; // top of the SV square
    let sv = Recti { x: px + pad_x, y: top, w: sv_side, h: sv_side };
    let hue = Recti { x: px + pad_x + sv_side + gap, y: top, w: hue_w, h: sv_side };
    let preview = Recti { x: px + pad_x, y: top + sv_side + gap, w: inner_w, h: sw_h };
    let hex = (px + pad_x, preview.y + sw_h + gap);
    let close = Recti { x: px + pad_x, y: hex.1 + hex_h + gap, w: inner_w, h: close_h };
    Geom { panel: Recti { x: px, y: py, w: panel_w, h: panel_h }, sv, hue, preview, hex, close }
}

/// What a point landed on. `None` inside the panel means "swallow, do nothing";
/// the caller treats a press *outside* the panel as a dismiss.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Sv,
    Hue,
    Close,
    None,
}

/// Which control is under `p`.
pub fn hit(g: &Geom, p: (f32, f32)) -> Hit {
    if g.close.contains(p) {
        Hit::Close
    } else if g.sv.contains(p) {
        Hit::Sv
    } else if g.hue.contains(p) {
        Hit::Hue
    } else {
        Hit::None
    }
}

/// Saturation/value from a point clamped into the SV square.
pub fn sv_at(g: &Geom, p: (f32, f32)) -> (f32, f32) {
    let s = ((p.0 - g.sv.x) / g.sv.w).clamp(0.0, 1.0);
    let v = 1.0 - ((p.1 - g.sv.y) / g.sv.h).clamp(0.0, 1.0);
    (s, v)
}

/// Hue (degrees) from a point clamped into the hue strip.
pub fn hue_at(g: &Geom, p: (f32, f32)) -> f32 {
    ((p.1 - g.hue.y) / g.hue.h).clamp(0.0, 1.0) * 360.0
}

// --- drawing --------------------------------------------------------------

// The two marker colours are deliberately NOT palette roles. A marker sits on
// top of an arbitrary user-chosen hue — every colour in the SV square and the
// hue strip — so it cannot be derived from the chrome palette and stay visible.
// A black square inside a white one is the standard answer, and it works over
// anything.
const BLACK: Color = Color(0.0, 0.0, 0.0, 0.9);
const WHITE: Color = Color(1.0, 1.0, 1.0, 0.95);

/// SV-square resolution: an N×N grid of solid cells. The single `ssh -X` cost
/// knob — N² `fill_rect`s per repaint (256 at N=16). Coarser = cheaper.
const SV_GRID: usize = 16;
/// Hue-strip segments (vertical gradient).
const HUE_SEG: usize = 32;

fn c(rgb: [u8; 3]) -> Color {
    Color::rgb(rgb[0], rgb[1], rgb[2])
}

/// A thin rectangular outline (four fills) — markers, since there is no circle.
fn outline(be: &mut dyn Backend, x: f32, y: f32, w: f32, h: f32, t: f32, col: Color) {
    be.fill_rect(x, y, w, t, col); // top
    be.fill_rect(x, y + h - t, w, t, col); // bottom
    be.fill_rect(x, y, t, h, col); // left
    be.fill_rect(x + w - t, y, t, h, col); // right
}

/// Paint the picker for state `(h,s,v)`, titled `title`.
pub fn draw(
    be: &mut dyn Backend,
    g: &Geom,
    h: f32,
    s: f32,
    v: f32,
    title: &str,
    pal: &Palette,
    cell_w: f32,
    cell_h: f32,
    sc: f32,
) {
    let hair = sc * logical::HAIRLINE;
    let p = g.panel;
    theme::panel(be, p, pal, sc);

    // Title, top-left inside the padding, on the same measure as the controls.
    let ty = p.y + sc * logical::PANEL_PAD_Y + ((sc * logical::PANEL_ROW_PAD) * 0.5);
    theme::text(be, g.sv.x, ty, title, pal.text, true);

    // SV square: an N×N grid at the current hue (saturation →x, value →y↑).
    let cw = g.sv.w / SV_GRID as f32;
    let ch = g.sv.h / SV_GRID as f32;
    for j in 0..SV_GRID {
        for i in 0..SV_GRID {
            let sat = i as f32 / (SV_GRID - 1) as f32;
            let val = 1.0 - j as f32 / (SV_GRID - 1) as f32;
            // +1px overlap so neighbouring cells leave no seam.
            be.fill_rect(g.sv.x + i as f32 * cw, g.sv.y + j as f32 * ch, cw + 1.0, ch + 1.0, c(hsv_to_rgb(h, sat, val)));
        }
    }
    // Hue strip: a vertical gradient of fully-saturated hues.
    let seg = g.hue.h / HUE_SEG as f32;
    for k in 0..HUE_SEG {
        let hh = k as f32 / HUE_SEG as f32 * 360.0;
        be.fill_rect(g.hue.x, g.hue.y + k as f32 * seg, g.hue.w, seg + 1.0, c(hsv_to_rgb(hh, 1.0, 1.0)));
    }

    // SV marker: a small black-then-white hollow square at (s, v).
    let mx = g.sv.x + s.clamp(0.0, 1.0) * g.sv.w;
    let my = g.sv.y + (1.0 - v.clamp(0.0, 1.0)) * g.sv.h;
    let mo = sc * logical::PICKER_SV_MARK_OUT;
    let mi = sc * logical::PICKER_SV_MARK_IN;
    outline(be, mx - mo * 0.5, my - mo * 0.5, mo, mo, 2.0 * hair, BLACK);
    outline(be, mx - mi * 0.5, my - mi * 0.5, mi, mi, hair, WHITE);
    // Hue caret: a horizontal bar across the strip at hue h.
    let hy = g.hue.y + (h.rem_euclid(360.0) / 360.0) * g.hue.h;
    let co = sc * logical::PICKER_HUE_CARET_OUT;
    let ci = sc * logical::PICKER_HUE_CARET_IN;
    be.fill_rect(g.hue.x - co * 0.5, hy - co * 0.5, g.hue.w + co, co, BLACK);
    be.fill_rect(g.hue.x - co * 0.5, hy - ci * 0.5, g.hue.w + co, ci, WHITE);

    // Preview swatch + edge.
    let rgb = hsv_to_rgb(h, s, v);
    be.fill_rect(g.preview.x, g.preview.y, g.preview.w, g.preview.h, c(rgb));
    outline(be, g.preview.x, g.preview.y, g.preview.w, g.preview.h, hair, pal.edge);

    // Hex readout, and the same value again as the panel's quiet caption.
    let hex = format!("#{:02x}{:02x}{:02x}", rgb[0], rgb[1], rgb[2]);
    theme::text(be, g.hex.0, g.hex.1, &hex, pal.dim, false);

    // Done: the panel's default action, so it wears the selection colour the
    // rest of the chrome uses for "this is what Return does".
    theme::rounded(
        be,
        g.close.x,
        g.close.y,
        g.close.w,
        g.close.h,
        sc * logical::PANEL_SEL_RADIUS,
        pal.sel,
    );
    let label = "Done  (Esc)";
    let lw = label.chars().count() as f32 * cell_w;
    theme::text(be, g.close.x + (g.close.w - lw) * 0.5, theme::text_y(g.close, cell_h), label, pal.sel_text, false);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hsv_rgb_round_trips_within_one_step() {
        // A spread of colours: primaries, greys, and a few arbitrary tones.
        let cases = [
            [217, 144, 74], [0, 0, 0], [255, 255, 255], [128, 128, 128],
            [255, 0, 0], [0, 255, 0], [0, 0, 255], [10, 200, 130], [3, 7, 250],
        ];
        for rgb in cases {
            let (h, s, v) = rgb_to_hsv(rgb);
            let back = hsv_to_rgb(h, s, v);
            for k in 0..3 {
                assert!(
                    (back[k] as i32 - rgb[k] as i32).abs() <= 1,
                    "round-trip {rgb:?} -> ({h},{s},{v}) -> {back:?}"
                );
            }
        }
    }

    #[test]
    fn known_hsv_values() {
        assert_eq!(hsv_to_rgb(0.0, 1.0, 1.0), [255, 0, 0]);
        assert_eq!(hsv_to_rgb(120.0, 1.0, 1.0), [0, 255, 0]);
        assert_eq!(hsv_to_rgb(240.0, 1.0, 1.0), [0, 0, 255]);
        assert_eq!(hsv_to_rgb(0.0, 0.0, 1.0), [255, 255, 255]); // no sat = white
        assert_eq!(hsv_to_rgb(0.0, 0.0, 0.0), [0, 0, 0]);
        // The h→360 edge must not fall off the sextant match.
        assert_eq!(hsv_to_rgb(360.0, 1.0, 1.0), [255, 0, 0]);
    }

    #[test]
    fn grey_has_zero_saturation() {
        let (_, s, v) = rgb_to_hsv([128, 128, 128]);
        assert_eq!(s, 0.0);
        assert!((v - 128.0 / 255.0).abs() < 1e-6);
    }

    #[test]
    fn swatch_index_maps_to_the_right_slot() {
        assert_eq!(Slot::from_swatch_index(0), Slot::Fg);
        assert_eq!(Slot::from_swatch_index(1), Slot::Bg);
        assert_eq!(Slot::from_swatch_index(2), Slot::Palette(0));
        assert_eq!(Slot::from_swatch_index(17), Slot::Palette(15));
    }

    #[test]
    fn layout_stays_on_screen() {
        let g = layout(11.0, 21.0, 900.0, 700.0, 1.0);
        assert!(g.panel.x >= 0.0 && g.panel.y >= 0.0);
        assert!(g.panel.x + g.panel.w <= 900.0 + 0.01);
        assert!(g.panel.y + g.panel.h <= 700.0 + 0.01);
        // Controls sit inside the panel.
        for r in [g.sv, g.hue, g.preview, g.close] {
            assert!(r.x >= g.panel.x - 0.01 && r.x + r.w <= g.panel.x + g.panel.w + 0.01);
            assert!(r.y >= g.panel.y - 0.01 && r.y + r.h <= g.panel.y + g.panel.h + 0.01);
        }
    }

    /// The picker had the same hole the context menu did: in a window shorter
    /// than the panel's natural height it laid its contents out full size and
    /// let the hex readout and the Done button run out through the bottom. Every
    /// control must stay inside the panel, and the panel inside the window, at
    /// every size and backing factor.
    #[test]
    fn every_control_stays_inside_the_window_at_every_size_and_scale() {
        for &sc in &[1.0_f32, 2.0, 3.0] {
            for &(w, h) in &[(900.0_f32, 700.0_f32), (400.0, 200.0), (300.0, 120.0), (200.0, 900.0), (60.0, 60.0)] {
                let g = layout(11.0 * sc, 21.0 * sc, w, h, sc);
                let ctx = format!("{w}x{h} @{sc}x");
                assert!(g.panel.x >= -0.01 && g.panel.y >= -0.01, "{ctx}");
                assert!(g.panel.x + g.panel.w <= w + 0.01, "{ctx}: panel width");
                assert!(g.panel.y + g.panel.h <= h + 0.01, "{ctx}: panel height");
                for (name, r) in [("sv", g.sv), ("hue", g.hue), ("preview", g.preview), ("close", g.close)] {
                    assert!(r.w > 0.0 && r.h > 0.0, "{ctx}: {name} collapsed");
                    assert!(r.x >= g.panel.x - 0.01, "{ctx}: {name} left of the panel");
                    assert!(r.x + r.w <= g.panel.x + g.panel.w + 0.01, "{ctx}: {name} right of the panel");
                    assert!(r.y >= g.panel.y - 0.01, "{ctx}: {name} above the panel");
                    assert!(r.y + r.h <= g.panel.y + g.panel.h + 0.01, "{ctx}: {name} below the panel");
                }
                // The controls stack without overlapping, in the order drawn.
                assert!(g.sv.y + g.sv.h <= g.preview.y + 0.01, "{ctx}: square over the preview");
                assert!(g.preview.y + g.preview.h <= g.hex.1 + 0.01, "{ctx}: preview over the hex");
                assert!(g.hex.1 <= g.close.y + 0.01, "{ctx}: hex over the button");
                // And the hit-test still resolves each of them.
                assert_eq!(hit(&g, (g.close.x + 1.0, g.close.y + 1.0)), Hit::Close, "{ctx}");
                assert_eq!(hit(&g, (g.sv.x + 0.5, g.sv.y + 0.5)), Hit::Sv, "{ctx}");
            }
        }
    }

    #[test]
    fn hit_picks_the_control_under_the_point() {
        let g = layout(11.0, 21.0, 900.0, 700.0, 1.0);
        assert_eq!(hit(&g, (g.sv.x + 5.0, g.sv.y + 5.0)), Hit::Sv);
        assert_eq!(hit(&g, (g.hue.x + 2.0, g.hue.y + 5.0)), Hit::Hue);
        assert_eq!(hit(&g, (g.close.x + 5.0, g.close.y + 2.0)), Hit::Close);
        assert_eq!(hit(&g, (g.panel.x - 20.0, g.panel.y)), Hit::None);
    }

    #[test]
    fn sv_and_hue_map_corners_and_ends() {
        let g = layout(11.0, 21.0, 900.0, 700.0, 1.0);
        // Top-left of the SV square = zero saturation, full value.
        let (s, v) = sv_at(&g, (g.sv.x, g.sv.y));
        assert!(s.abs() < 1e-6 && (v - 1.0).abs() < 1e-6);
        // Bottom-right = full saturation, zero value.
        let (s, v) = sv_at(&g, (g.sv.x + g.sv.w, g.sv.y + g.sv.h));
        assert!((s - 1.0).abs() < 1e-6 && v.abs() < 1e-6);
        // Points outside clamp in.
        let (s, v) = sv_at(&g, (g.sv.x - 100.0, g.sv.y - 100.0));
        assert!(s == 0.0 && v == 1.0);
        // Hue: top = 0°, bottom → 360°.
        assert!(hue_at(&g, (g.hue.x, g.hue.y)).abs() < 1e-6);
        assert!((hue_at(&g, (g.hue.x, g.hue.y + g.hue.h)) - 360.0).abs() < 1e-3);
    }

    #[test]
    fn picker_state_seeds_from_rgb_and_reproduces_it() {
        let st = PickerState::new(Slot::Fg, [217, 144, 74]);
        let back = st.rgb();
        for k in 0..3 {
            assert!((back[k] as i32 - [217, 144, 74][k] as i32).abs() <= 1);
        }
    }
}
