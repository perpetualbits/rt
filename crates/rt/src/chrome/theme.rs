//! **The chrome design system.** One palette, one spacing scale, one row
//! rhythm, one panel shape — for every floating panel rt draws over the
//! terminal: the context menu, the manual, preferences, the colour picker, the
//! clipboard history and the search bar.
//!
//! # The defect this exists for
//!
//! *"The rt manual and menu system in linux and the in-window manual and menu in
//! the mac is very ugly. Text does not flow, colors are ugly, the bottom of the
//! menu is under the edge."*
//!
//! Before this module there were forty hardcoded colour literals across
//! `chrome/*.rs` and no two panels agreed: `menu.rs` painted itself
//! `rgb(0x20,0x22,0x28)`, `prefs.rs` `Color(0.10,0.10,0.12,0.97)`, the colour
//! picker the same thing at `0.98`. None of them related to the user's own
//! theme, so a dark blue-purple terminal sat behind grey-blue chrome that
//! clashed with it. There is now exactly one place a chrome colour comes from:
//! [`Palette::derive`].
//!
//! # The system
//!
//! **Palette roles.** Never "a grey" — always the role the colour plays. See
//! [`Palette`]: `panel`, `edge`, `sep`, `hover`, `sel`/`sel_text`, `text`,
//! `dim`, `off`, `accent`, `thumb`, `field`. Add a panel, and you reach for a
//! role; you do not invent a literal.
//!
//! **Derived from the terminal, not from nowhere.** Every role is computed from
//! the user's configured foreground, background and one palette accent, so the
//! chrome belongs to the terminal it floats over. What the derivation
//! guarantees, at every user colour scheme from near-black to near-white, is
//! *contrast*: see [`Palette::derive`]'s floors and
//! [`tests::every_theme_is_legible_over_every_scheme`].
//!
//! **Spacing scale.** A 4 px logical grid, registered in
//! [`crate::chrome_scale::logical`] and multiplied by the display's backing
//! factor at every use: `PANEL_PAD_X` (14) across, `PANEL_PAD_Y` (8) down,
//! `PANEL_GAP` (8) between control groups, `PANEL_RADIUS` (7) on every corner,
//! `PANEL_SEL_INSET` (5) for a highlight's standoff from the panel edge.
//!
//! **Row rhythm.** Every list row in every panel is `cell_h + PANEL_ROW_PAD`
//! tall (8 logical px of air, up from 4 — the old rows were typographically
//! cramped), and its text is *vertically centred* in it by [`text_y`], never
//! top-aligned with a magic offset. A separator row is `PANEL_SEP_H` (11) tall
//! with the rule centred in it.
//!
//! **Panel shape.** [`panel`] paints every one of them: a rounded body at
//! `PANEL_RADIUS` with a hairline `edge` outline that follows the curve. Corner
//! radius is the single cheapest thing that makes a panel read as designed
//! rather than as a debug rectangle, and rt has no rounded-rect primitive — so
//! it is built here out of `fill_rect` slices, which both backends have.
//!
//! # Adding a panel
//!
//! Take a `&Palette`. Call [`panel`] for the frame, [`row_highlight`] for a
//! hover/selection fill, [`separator`] for a rule, [`text_y`] to place a row's
//! text. Do not write a `Color` literal, and do not write a bare pixel number —
//! it goes in `chrome_scale::logical` or it is a hairline on a Retina display.

use crate::backend::Backend;
use crate::chrome::Recti;
use crate::chrome_scale::logical;
use crate::render::Color;
use rt_config::ChromeTheme;

// --- colour maths -----------------------------------------------------------

/// WCAG 2.x relative luminance of an sRGB colour in 0..1 components.
fn lum(c: [f32; 3]) -> f32 {
    let lin = |v: f32| {
        let v = v.clamp(0.0, 1.0);
        if v <= 0.04045 {
            v / 12.92
        } else {
            ((v + 0.055) / 1.055).powf(2.4)
        }
    };
    0.2126 * lin(c[0]) + 0.7152 * lin(c[1]) + 0.0722 * lin(c[2])
}

/// WCAG contrast ratio between two sRGB colours — 1.0 (identical) to 21.0
/// (black on white). Order-independent.
pub fn contrast(a: [f32; 3], b: [f32; 3]) -> f32 {
    let (la, lb) = (lum(a), lum(b));
    let (hi, lo) = if la >= lb { (la, lb) } else { (lb, la) };
    (hi + 0.05) / (lo + 0.05)
}

const BLACK: [f32; 3] = [0.0, 0.0, 0.0];
const WHITE: [f32; 3] = [1.0, 1.0, 1.0];

/// Linear interpolation between two sRGB colours (`t = 0` is `a`).
fn mix(a: [f32; 3], b: [f32; 3], t: f32) -> [f32; 3] {
    let t = t.clamp(0.0, 1.0);
    [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t, a[2] + (b[2] - a[2]) * t]
}

/// `c` restated at relative luminance `target`, keeping its hue: mixed toward
/// white or black, whichever direction the target lies in, by bisection.
///
/// Bisection rather than algebra because luminance is not linear in the sRGB
/// components — and because a closed form would have to be re-derived the next
/// time somebody changes the transfer function. Twenty-four halvings put the
/// result inside 1e-7 of the requested luminance, far below one 8-bit step.
fn with_lum(c: [f32; 3], target: f32) -> [f32; 3] {
    let target = target.clamp(0.0, 1.0);
    let l = lum(c);
    if (l - target).abs() < 1e-4 {
        return c;
    }
    let toward = if target > l { WHITE } else { BLACK };
    let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
    for _ in 0..24 {
        let m = 0.5 * (lo + hi);
        // lum(mix(c, toward, t)) is monotone in t, so a plain bisection converges.
        if (lum(mix(c, toward, m)) > target) == (toward == WHITE) {
            hi = m;
        } else {
            lo = m;
        }
    }
    mix(c, toward, 0.5 * (lo + hi))
}

/// `c`, pushed toward whichever pole the panel is NOT, until it clears `ratio`
/// against `bg`. Returns `c` untouched when it already does.
///
/// This is the guarantee that makes a user-derived palette safe: the starting
/// colour is the user's own foreground (or accent), so chrome text matches
/// terminal text wherever it legibly can, and is nudged only as far as the
/// contrast floor demands. `bg` is always a panel colour, whose luminance
/// [`Palette::derive`] has already forced into a band where the floors below are
/// reachable — see `PANEL_L_*`.
fn ensure_contrast(c: [f32; 3], bg: [f32; 3], ratio: f32) -> [f32; 3] {
    if contrast(c, bg) >= ratio {
        return c;
    }
    let toward = if lum(bg) < 0.5 { WHITE } else { BLACK };
    let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
    for _ in 0..24 {
        let m = 0.5 * (lo + hi);
        if contrast(mix(c, toward, m), bg) >= ratio {
            hi = m;
        } else {
            lo = m;
        }
    }
    mix(c, toward, hi)
}

/// `body`, moved away from `bg` until it clears `floor` — the panel's own
/// separation from the terminal behind it.
///
/// Monotone, hence bisectable: a dark panel only ever moves toward white and a
/// light one only ever toward black, and the background is by construction on
/// the far side of that move.
fn ensure_separation(body: [f32; 3], bg: [f32; 3], floor: f32, dark: bool) -> [f32; 3] {
    if contrast(body, bg) >= floor {
        return body;
    }
    let toward = if dark { WHITE } else { BLACK };
    let (mut lo, mut hi) = (0.0_f32, 1.0_f32);
    for _ in 0..24 {
        let m = 0.5 * (lo + hi);
        if contrast(mix(body, toward, m), bg) >= floor {
            hi = m;
        } else {
            lo = m;
        }
    }
    mix(body, toward, hi)
}

fn to_f(rgb: [u8; 3]) -> [f32; 3] {
    [rgb[0] as f32 / 255.0, rgb[1] as f32 / 255.0, rgb[2] as f32 / 255.0]
}

fn col(c: [f32; 3], a: f32) -> Color {
    Color(c[0], c[1], c[2], a)
}

// --- the palette ------------------------------------------------------------

/// Every colour rt's chrome is allowed to use, by the role it plays.
///
/// Built once per frame by [`Palette::derive`] from the user's settings; panels
/// take it by reference and never construct a `Color` of their own.
#[derive(Clone, Copy, Debug)]
pub struct Palette {
    /// Panel body. Slightly translucent, so the terminal shows through.
    pub panel: Color,
    /// The hairline outline around a panel.
    pub edge: Color,
    /// A separator rule inside a panel.
    pub sep: Color,
    /// Fill under the row the pointer is over (neutral, unobtrusive).
    pub hover: Color,
    /// Fill under the row the keyboard has selected (the accent).
    pub sel: Color,
    /// Text on top of [`Self::sel`].
    pub sel_text: Color,
    /// Primary text.
    pub text: Color,
    /// Secondary text: accelerators, readouts, badges, the version line.
    pub dim: Color,
    /// Disabled text.
    pub off: Color,
    /// Headings and other accented text.
    pub accent: Color,
    /// A scrollbar thumb.
    pub thumb: Color,
    /// A recessed field or well (the search bar's query area).
    pub field: Color,
}

// Luminance bands the panel body is forced into. These are what make the text
// contrast floors below REACHABLE: against a mid-grey panel (L ≈ 0.21) the best
// contrast any colour can reach is white's 4.0:1, so a 7:1 floor would be
// unsatisfiable. Pinning the panel near one end of the range keeps every floor
// achievable at every user colour scheme.
//
// Dark: L ≤ 0.085 → white reaches (1.05)/(0.135) = 7.8:1.
// Light: L ≥ 0.78 → black reaches (0.83)/(0.05) = 16.6:1.
const PANEL_L_DARK: f32 = 0.085;
const PANEL_L_LIGHT: f32 = 0.78;
/// How far a dark panel is lifted off the terminal background, so the panel
/// reads as a separate surface even before its border is drawn.
const DARK_LIFT: f32 = 0.030;
/// The same, downward, for a light one.
const LIGHT_SETTLE: f32 = 0.060;

/// Contrast floors, asserted by [`tests::every_theme_is_legible_over_every_scheme`].
/// Named so a future reader can see what "legible" was taken to mean rather
/// than reverse-engineering it from four bisections.
pub const FLOOR_TEXT: f32 = 7.0;
/// Secondary text — WCAG AA for body copy.
pub const FLOOR_DIM: f32 = 4.5;
/// Disabled text: deliberately BELOW AA (that is what "disabled" looks like),
/// but never so low it disappears.
pub const FLOOR_OFF: f32 = 2.4;
/// Accented text (section headings).
pub const FLOOR_ACCENT: f32 = 4.5;
/// Text on a selected row.
pub const FLOOR_SEL_TEXT: f32 = 4.5;
/// A panel's border against its own body — enough to see the edge, not enough
/// to draw the eye.
pub const FLOOR_EDGE: f32 = 1.25;
/// A panel body against the terminal background it floats over.
pub const FLOOR_PANEL: f32 = 1.15;
/// The alpha floor for a panel body. The contrast floors above are computed
/// against the panel's own RGB as if it were opaque; at 0.95 and above the
/// worst case a composite can shift the effective luminance by is 5%, which is
/// well inside the margin every floor is met with.
pub const PANEL_ALPHA_MIN: f32 = 0.95;

impl Palette {
    /// Derive the whole palette from the user's colours.
    ///
    /// `fg`/`bg` are `Settings::foreground`/`background`; `accent` is the
    /// user's own bright-blue palette entry, so a selection bar is in the
    /// family of colours the terminal already uses.
    ///
    /// The shape of it: decide dark vs light from the background's luminance;
    /// pin the panel body into the legible band for that side (keeping the
    /// background's hue, except under [`ChromeTheme::Graphite`], which
    /// deliberately drops it); then derive every other role from the panel and
    /// hold each to its floor with [`ensure_contrast`].
    pub fn derive(fg: [u8; 3], bg: [u8; 3], accent: [u8; 3], theme: ChromeTheme) -> Palette {
        let bg = to_f(bg);
        let fg = to_f(fg);
        let accent = to_f(accent);
        let dark = lum(bg) < 0.5;

        // 1. The panel body.
        let (body, alpha, floor_text): ([f32; 3], f32, f32) = match theme {
            ChromeTheme::Tinted => {
                let l = if dark {
                    (lum(bg) + DARK_LIFT).clamp(0.020, PANEL_L_DARK)
                } else {
                    (lum(bg) - LIGHT_SETTLE).clamp(PANEL_L_LIGHT, 0.97)
                };
                (with_lum(bg, l), 0.96, FLOOR_TEXT)
            }
            // Neutral by construction: the hue is dropped, only light-vs-dark
            // survives. The two luminances are the weights macOS uses for a menu
            // (a light graphite in dark mode, an off-white in light mode).
            ChromeTheme::Graphite => {
                let l = if dark { 0.055 } else { 0.86 };
                (with_lum(if dark { WHITE } else { BLACK }, l), 0.97, FLOOR_TEXT)
            }
            // Tinted, pushed out to the ends, fully opaque, higher text floor.
            ChromeTheme::Contrast => {
                let l = if dark { 0.012 } else { 0.94 };
                (with_lum(bg, l), 1.0, 9.0)
            }
        };
        // The panel must be visibly a different surface from the terminal behind
        // it. A pure-white terminal is the case that forces this: the light-mode
        // band alone lands the panel within 1.06:1 of the background, which is
        // no edge at all. Push it further from the background — away from the
        // pole it already sits near, which is the direction that also RAISES the
        // text contrast, so nothing below is spent to pay for it.
        let body = ensure_separation(body, bg, FLOOR_PANEL, dark);
        // Against the pole the panel is not: the direction every contrast-
        // increasing nudge below travels in.
        let away = if dark { WHITE } else { BLACK };

        // 2. Structure, as fractions of the way from the body toward that pole.
        // Contrast-driven, not eyeballed: the edge is nudged until it clears
        // FLOOR_EDGE, which is what keeps it visible on a near-black panel where
        // a fixed mix would vanish.
        let edge_mix = match theme {
            ChromeTheme::Contrast => 0.45,
            _ => 0.26,
        };
        let edge = ensure_contrast(mix(body, away, edge_mix), body, FLOOR_EDGE);
        let sep = ensure_contrast(mix(body, away, edge_mix * 0.6), body, 1.18);
        let hover = mix(body, away, if dark { 0.13 } else { 0.10 });
        let thumb = mix(body, away, 0.34);
        // A well is recessed: darker than the panel in a dark theme, brighter
        // (toward paper white) in a light one.
        let field = mix(body, if dark { BLACK } else { WHITE }, if dark { 0.45 } else { 0.55 });

        // 3. Text, each held to its floor, each starting from the user's own colour.
        let text = ensure_contrast(fg, body, floor_text);
        let dim = ensure_contrast(mix(text, body, 0.40), body, FLOOR_DIM);
        let off = ensure_contrast(mix(text, body, 0.62), body, FLOOR_OFF);
        let accent_text = ensure_contrast(accent, body, FLOOR_ACCENT);

        // 4. The selection bar. A saturated accent at a fixed dark luminance in
        // BOTH modes — that is what a macOS menu highlight is, and it is the one
        // place white text is right whatever the rest of the theme does.
        let sel = with_lum(mix(body, accent, 0.90), 0.155);
        let sel_text = ensure_contrast(
            if contrast(WHITE, sel) >= contrast(BLACK, sel) { WHITE } else { BLACK },
            sel,
            FLOOR_SEL_TEXT,
        );

        Palette {
            // The floors below are computed against the panel's own RGB as if it
            // were opaque, so the alpha is held at or above the level that makes
            // that approximation safe.
            panel: col(body, alpha.max(PANEL_ALPHA_MIN)),
            edge: col(edge, 1.0),
            sep: col(sep, 1.0),
            hover: col(hover, 1.0),
            sel: col(sel, 1.0),
            sel_text: col(sel_text, 1.0),
            text: col(text, 1.0),
            dim: col(dim, 1.0),
            off: col(off, 1.0),
            accent: col(accent_text, 1.0),
            thumb: col(thumb, 1.0),
            field: col(field, 1.0),
        }
    }

    /// The palette for a `Settings`. The one call site every panel's caller uses.
    pub fn of(s: &rt_config::Settings) -> Palette {
        // Palette entry 12 is bright blue in every scheme rt ships — the nearest
        // thing a terminal palette has to a system accent colour.
        Palette::derive(s.foreground, s.background, s.palette[12], s.chrome_theme)
    }
}

// --- shared drawing ---------------------------------------------------------

/// A rounded rectangle, built from horizontal `fill_rect` slices.
///
/// rt has no rounded-rect primitive and the two backends share only
/// `fill_rect`, so the corner is approximated by insetting each of the `r`
/// topmost and bottommost slices by the circle's own `r - sqrt(r² - dy²)`. At
/// the sizes chrome uses this is indistinguishable from a real arc, and it
/// costs `2r + 1` fills for a whole panel.
pub fn rounded(be: &mut dyn Backend, x: f32, y: f32, w: f32, h: f32, r: f32, c: Color) {
    let r = r.min(w * 0.5).min(h * 0.5).max(0.0);
    if r < 1.0 || w <= 0.0 || h <= 0.0 {
        if w > 0.0 && h > 0.0 {
            be.fill_rect(x, y, w, h, c);
        }
        return;
    }
    let n = r.ceil() as usize;
    let step = r / n as f32;
    for i in 0..n {
        let dy = r - (i as f32 + 0.5) * step; // vertical distance from the arc centre
        let dx = r - (r * r - dy * dy).max(0.0).sqrt();
        let iw = w - 2.0 * dx;
        if iw <= 0.0 {
            continue;
        }
        be.fill_rect(x + dx, y + i as f32 * step, iw, step, c);
        be.fill_rect(x + dx, y + h - (i as f32 + 1.0) * step, iw, step, c);
    }
    if h - 2.0 * r > 0.0 {
        be.fill_rect(x, y + r, w, h - 2.0 * r, c);
    }
}

/// The hairline outline of the same rounded rectangle.
///
/// Drawn as an outline rather than as a second, inset rounded fill, so a
/// translucent panel body is composited over the terminal ONCE. (Filling the
/// edge shape and then the body over it would blend 4% of the border colour
/// through the entire panel.)
fn rounded_outline(be: &mut dyn Backend, x: f32, y: f32, w: f32, h: f32, r: f32, t: f32, c: Color) {
    let r = r.min(w * 0.5).min(h * 0.5).max(0.0);
    if r < 1.0 || w <= 0.0 || h <= 0.0 {
        be.fill_rect(x, y, w, t, c);
        be.fill_rect(x, y + h - t, w, t, c);
        be.fill_rect(x, y, t, h, c);
        be.fill_rect(x + w - t, y, t, h, c);
        return;
    }
    let n = r.ceil() as usize;
    let step = r / n as f32;
    let mut cap = 0.0_f32;
    for i in 0..n {
        let dy = r - (i as f32 + 0.5) * step;
        let dx = r - (r * r - dy * dy).max(0.0).sqrt();
        if i == 0 {
            cap = dx;
        }
        if w - 2.0 * dx <= 0.0 {
            continue;
        }
        // The two vertical marks that trace the arc, top and bottom.
        for sy in [y + i as f32 * step, y + h - (i as f32 + 1.0) * step] {
            be.fill_rect(x + dx, sy, t, step, c);
            be.fill_rect(x + w - dx - t, sy, t, step, c);
        }
    }
    // Straight runs between the arcs, and the caps that close the top/bottom.
    if h - 2.0 * r > 0.0 {
        be.fill_rect(x, y + r, t, h - 2.0 * r, c);
        be.fill_rect(x + w - t, y + r, t, h - 2.0 * r, c);
    }
    let cw = w - 2.0 * cap;
    if cw > 0.0 {
        be.fill_rect(x + cap, y, cw, t, c);
        be.fill_rect(x + cap, y + h - t, cw, t, c);
    }
}

/// Paint a panel: rounded body plus its hairline edge. **Every** chrome panel
/// goes through this — that is what makes them look like one family.
pub fn panel(be: &mut dyn Backend, r: Recti, p: &Palette, sc: f32) {
    let rad = sc * logical::PANEL_RADIUS;
    let hair = sc * logical::HAIRLINE;
    rounded(be, r.x, r.y, r.w, r.h, rad, p.panel);
    rounded_outline(be, r.x, r.y, r.w, r.h, rad, hair, p.edge);
}

/// Paint a row highlight: a rounded fill inset from the panel's edges, the way
/// a macOS menu insets its highlight rather than running it wall to wall.
pub fn row_highlight(be: &mut dyn Backend, row: Recti, c: Color, sc: f32) {
    let inset = sc * logical::PANEL_SEL_INSET;
    let w = row.w - 2.0 * inset;
    if w <= 0.0 || row.h <= 0.0 {
        return;
    }
    rounded(be, row.x + inset, row.y, w, row.h, sc * logical::PANEL_SEL_RADIUS, c);
}

/// Paint a separator: a hairline rule centred in its row, inset from the panel
/// edges by the same standoff a highlight uses.
pub fn separator(be: &mut dyn Backend, row: Recti, p: &Palette, sc: f32) {
    let inset = sc * logical::PANEL_PAD_X;
    let hair = sc * logical::HAIRLINE;
    let w = row.w - 2.0 * inset;
    if w <= 0.0 {
        return;
    }
    be.fill_rect(row.x + inset, (row.y + (row.h - hair) * 0.5).round(), w, hair, p.sep);
}

/// The y a row's text is drawn at: vertically centred in the row.
///
/// The single rule for text placement in every panel. It replaces the old
/// `PANEL_TEXT_TOP` magic offset, which top-aligned text and left the extra
/// row height as a gap underneath — the reason rows read as sitting too high
/// in their own highlight.
pub fn text_y(row: Recti, cell_h: f32) -> f32 {
    row.y + ((row.h - cell_h) * 0.5).max(0.0)
}

/// Draw a string as a run of cells from `(ox, oy)`. Every panel draws text this
/// way; having one helper keeps the `enumerate`/`draw_char` incantation (and its
/// column indexing) in a single place.
pub fn text(be: &mut dyn Backend, ox: f32, oy: f32, s: &str, c: Color, bold: bool) {
    for (i, ch) in s.chars().enumerate() {
        be.draw_char(ox, oy, i, 0, ch, c, bold, false);
    }
}

/// Draw a string right-aligned so it ENDS at `right`.
pub fn text_right(be: &mut dyn Backend, right: f32, oy: f32, s: &str, cell_w: f32, c: Color, bold: bool) {
    let n = s.chars().count() as f32;
    text(be, right - n * cell_w, oy, s, c, bold);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every colour scheme rt ships, plus the two extremes the derivation has to
    /// survive: a pure-black and a pure-white terminal.
    fn schemes() -> Vec<([u8; 3], [u8; 3], [u8; 3])> {
        let mut v: Vec<([u8; 3], [u8; 3], [u8; 3])> = rt_config::SCHEMES
            .iter()
            .map(|s| (s.foreground, s.background, s.palette[12]))
            .collect();
        v.push(([0xff, 0xff, 0xff], [0x00, 0x00, 0x00], [0x00, 0x00, 0xff])); // near-black
        v.push(([0x00, 0x00, 0x00], [0xff, 0xff, 0xff], [0x00, 0x00, 0xff])); // near-white
        v.push(([0x80, 0x80, 0x80], [0x80, 0x80, 0x80], [0x80, 0x80, 0x80])); // the pathological mid-grey
        v.push(([0xd0, 0xd0, 0xd8], [0x14, 0x10, 0x22], [0x8b, 0x7c, 0xff])); // a dark blue-purple
        v
    }

    fn rgb(c: Color) -> [f32; 3] {
        [c.0, c.1, c.2]
    }

    /// THE guarantee. Whatever the user's colours, and whichever chrome theme is
    /// chosen, every text role clears its stated floor against the panel it is
    /// drawn on, the border is visible against its own panel, and the panel is
    /// visible against the terminal behind it.
    #[test]
    fn every_theme_is_legible_over_every_scheme() {
        for theme in rt_config::ChromeTheme::ALL {
            for (fg, bg, accent) in schemes() {
                let p = Palette::derive(fg, bg, accent, *theme);
                let body = rgb(p.panel);
                let name = theme.name();
                let ctx = format!("{name} over bg {bg:?}");
                assert!(p.panel.3 >= PANEL_ALPHA_MIN, "{ctx}: panel alpha {}", p.panel.3);
                assert!(contrast(rgb(p.text), body) >= FLOOR_TEXT - 0.01, "{ctx}: text");
                assert!(contrast(rgb(p.dim), body) >= FLOOR_DIM - 0.01, "{ctx}: dim");
                assert!(contrast(rgb(p.off), body) >= FLOOR_OFF - 0.01, "{ctx}: off");
                assert!(contrast(rgb(p.accent), body) >= FLOOR_ACCENT - 0.01, "{ctx}: accent");
                assert!(contrast(rgb(p.sel_text), rgb(p.sel)) >= FLOOR_SEL_TEXT - 0.01, "{ctx}: sel text");
                assert!(contrast(rgb(p.edge), body) >= FLOOR_EDGE - 0.01, "{ctx}: edge");
                assert!(contrast(body, to_f(bg)) >= FLOOR_PANEL - 0.01, "{ctx}: panel vs terminal");
                // Dimmed text really is dimmer than primary, and disabled dimmer
                // still — the ordering is what makes the hierarchy readable.
                assert!(
                    contrast(rgb(p.text), body) > contrast(rgb(p.dim), body),
                    "{ctx}: dim must be dimmer than text"
                );
                assert!(
                    contrast(rgb(p.dim), body) > contrast(rgb(p.off), body),
                    "{ctx}: disabled must be dimmer than dim"
                );
                // A hover fill has to be seen, without shouting; a SELECTION has
                // to read as louder than a hover, or the two say the same thing.
                let hc = contrast(rgb(p.hover), body);
                assert!(hc > 1.05 && hc < 2.2, "{ctx}: hover contrast {hc}");
                assert!(
                    contrast(rgb(p.sel), body) > hc,
                    "{ctx}: a selection must stand out more than a hover"
                );
                // Text keeps its footing on a hover fill too (the fill is neutral
                // and slight, so this is a floor, not a coincidence).
                assert!(contrast(rgb(p.text), rgb(p.hover)) >= FLOOR_DIM - 0.01, "{ctx}: text on hover");
                // A recessed field only ever moves AWAY from the text, so text
                // drawn in one is at least as legible as text on the panel.
                assert!(
                    contrast(rgb(p.text), rgb(p.field)) >= contrast(rgb(p.text), body) - 0.01,
                    "{ctx}: the search field must not cost legibility"
                );
            }
        }
    }

    /// The chrome is DERIVED, not fixed: a blue terminal gets blue-tinted
    /// panels, a green one green-tinted. That is the whole point of `Tinted`,
    /// and the thing `Graphite` deliberately gives up.
    #[test]
    fn tinted_chrome_keeps_the_terminals_hue_and_graphite_drops_it() {
        let hue_spread = |c: Color| {
            let m = [c.0, c.1, c.2];
            m.iter().cloned().fold(f32::MIN, f32::max) - m.iter().cloned().fold(f32::MAX, f32::min)
        };
        // A strongly blue-purple background.
        let bg = [0x18, 0x10, 0x30];
        let t = Palette::derive([0xd0, 0xd0, 0xd8], bg, [0x8b, 0x7c, 0xff], ChromeTheme::Tinted);
        let g = Palette::derive([0xd0, 0xd0, 0xd8], bg, [0x8b, 0x7c, 0xff], ChromeTheme::Graphite);
        assert!(hue_spread(t.panel) > 0.02, "tinted must carry the terminal's hue: {:?}", t.panel);
        assert!(hue_spread(g.panel) < 0.005, "graphite must be neutral: {:?}", g.panel);
        // And the blue channel leads in the tinted panel, as it does in the bg.
        assert!(t.panel.2 > t.panel.1, "the tint follows the background");
    }

    /// Light and dark terminals both get chrome, and they are not the same
    /// chrome: a white terminal must not be handed a black menu.
    #[test]
    fn a_light_terminal_gets_light_chrome() {
        let dark = Palette::derive([0xd0; 3], [0x10, 0x10, 0x14], [0x5c, 0x5c, 0xff], ChromeTheme::Tinted);
        let light = Palette::derive([0x20; 3], [0xfd, 0xf6, 0xe3], [0x26, 0x8b, 0xd2], ChromeTheme::Tinted);
        assert!(lum(rgb(dark.panel)) < 0.1, "dark chrome stays dark");
        assert!(lum(rgb(light.panel)) > 0.6, "light chrome stays light");
        // Text flips with it.
        assert!(lum(rgb(dark.text)) > lum(rgb(dark.panel)));
        assert!(lum(rgb(light.text)) < lum(rgb(light.panel)));
    }

    /// Contrast mode is genuinely harder than tinted: opaque, and further from
    /// the terminal background. If it were not, there would be nothing to
    /// compare.
    #[test]
    fn the_three_themes_are_actually_different() {
        let (fg, bg, ac) = ([0xd0, 0xd0, 0xd8], [0x14, 0x10, 0x22], [0x8b, 0x7c, 0xff]);
        let t = Palette::derive(fg, bg, ac, ChromeTheme::Tinted);
        let g = Palette::derive(fg, bg, ac, ChromeTheme::Graphite);
        let c = Palette::derive(fg, bg, ac, ChromeTheme::Contrast);
        assert_eq!(c.panel.3, 1.0, "contrast mode is fully opaque");
        assert!(t.panel.3 < 1.0 && g.panel.3 < 1.0, "the softer themes let the terminal through");
        assert!(contrast(rgb(c.text), rgb(c.panel)) > contrast(rgb(t.text), rgb(t.panel)));
        assert!(contrast(rgb(c.edge), rgb(c.panel)) > contrast(rgb(t.edge), rgb(t.panel)));
        // All three are distinguishable panels.
        assert!(contrast(rgb(t.panel), rgb(g.panel)) > 1.05, "tinted vs graphite");
    }

    /// `with_lum` hits the luminance it is asked for and keeps the hue's sign.
    #[test]
    fn with_lum_reaches_its_target() {
        for target in [0.01_f32, 0.05, 0.2, 0.5, 0.85, 0.97] {
            for c in [[0.1, 0.2, 0.6], [0.9, 0.4, 0.1], [0.5, 0.5, 0.5]] {
                let got = with_lum(c, target);
                assert!((lum(got) - target).abs() < 1e-3, "{c:?} -> {target}: got {}", lum(got));
            }
        }
    }

    /// Known contrast anchors, so a change to the luminance maths is caught.
    #[test]
    fn contrast_matches_the_wcag_anchors() {
        assert!((contrast(WHITE, BLACK) - 21.0).abs() < 0.01);
        assert!((contrast(BLACK, BLACK) - 1.0).abs() < 1e-6);
        assert!(contrast(WHITE, BLACK) == contrast(BLACK, WHITE), "order-independent");
    }
}
