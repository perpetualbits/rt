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
//! **Reasoned about what is on screen, not what is configured.** A window with
//! `background_opacity = 0.05` is not showing the user's background colour; it
//! is showing 5% of it over a desktop rt cannot see. [`Palette::derive`]
//! therefore takes the opacity and works from [`surround`] — the composite that
//! actually reaches the eye — and holds every text floor against the panel *as
//! composited*, not against the panel's own RGB. Two consequences fall out of
//! that one input: a sheer window gets a nearly solid panel (there is no point
//! blending in a colour nobody can predict — see `panel_alpha`), and it gets a
//! stronger border ([`FLOOR_EDGE_SHEER`]), because when the surroundings are
//! unknowable the border is the only thing that reliably says where the panel
//! ends.
//!
//! **Three text colours, used the same way everywhere.** `text` is reading
//! matter, `dim` is metadata, and `accent` marks *the thing you press* — key
//! names in the manual's left column, accelerators in the menu — with bold
//! `accent` reserved for section headings. The manual's key column is coloured,
//! so [`FLOOR_ROLE_SPLIT`] makes `accent` being distinguishable from `text` a
//! guarantee rather than a coincidence of the user's scheme.
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

/// `body`, moved away from `around` until the panel **as composited at `alpha`**
/// clears `floor` against it — the panel's own separation from the terminal.
///
/// Two things changed here when the theme started reasoning about the effective
/// background:
///
/// * The test is on `mix(around, body, alpha)`, not on `body`. A panel that is
///   96% opaque is 4% of whatever it sits on, which pulls it back *toward* the
///   thing it is trying to stand off from; solving on the raw body overshoots
///   into a panel that misses the floor by exactly that 4%. A white terminal is
///   the case that catches it.
/// * The direction comes from the two *luminances*, not from the theme's
///   light/dark flag. That is what keeps this monotone — and hence bisectable —
///   now that `around` is the effective background and may sit on either side of
///   a dark panel. The flag was a safe proxy only while `around` was the user's
///   own background, which a dark panel is always lighter than; `dark` now only
///   breaks the exact tie.
fn ensure_separation(body: [f32; 3], around: [f32; 3], floor: f32, alpha: f32, dark: bool) -> [f32; 3] {
    let composited = |b: [f32; 3]| mix(around, b, alpha);
    if contrast(composited(body), around) >= floor {
        return body;
    }
    // Preferred direction: straight away from `around`, which is the smallest
    // move and the one that preserves what the panel band already decided. It is
    // not always *reachable*, though — a near-white terminal at high opacity puts
    // `around` above a light panel with less than the floor's worth of room left
    // above it — so the opposite pole is tried second.
    let preferred = match lum(composited(body)).partial_cmp(&lum(around)) {
        Some(std::cmp::Ordering::Greater) => WHITE,
        Some(std::cmp::Ordering::Less) => BLACK,
        _ => {
            if dark {
                WHITE
            } else {
                BLACK
            }
        }
    };
    let other = if preferred == WHITE { BLACK } else { WHITE };
    // A forward scan, not a bisection: crossing `around` on the way to the far
    // pole makes contrast dip to 1.0 and rise again, so the predicate is not
    // monotone in `t` and bisection can settle on the wrong side of the dip.
    // The first `t` that clears the floor is the least the panel has to move.
    const STEPS: usize = 256;
    let mut best = (contrast(composited(body), around), body);
    for toward in [preferred, other] {
        for i in 1..=STEPS {
            let c = mix(body, toward, i as f32 / STEPS as f32);
            let got = contrast(composited(c), around);
            if got >= floor {
                return c;
            }
            if got > best.0 {
                best = (got, c);
            }
        }
    }
    // Nothing reaches the floor (a panel wedged against a backdrop it cannot
    // escape): take the most separated colour there was.
    best.1
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
/// A panel's border against its own body, over an **opaque** window — enough to
/// find the edge, not enough to draw the eye. The body's own difference from a
/// known terminal colour is doing most of the separating here.
pub const FLOOR_EDGE: f32 = 1.25;
/// The same floor over a **fully see-through** window. When the terminal is not
/// really there, the colour behind the panel is the user's desktop, which rt
/// cannot know — so body-versus-terminal contrast guarantees nothing and the
/// border becomes the only thing that reliably says where the panel ends. It has
/// to be *seen* rather than merely found, so the floor roughly doubles.
pub const FLOOR_EDGE_SHEER: f32 = 3.2;
/// A panel body against the terminal background it floats over — measured
/// against the EFFECTIVE background (see [`surround`]), which is what is
/// actually on screen, not against the configured colour.
pub const FLOOR_PANEL: f32 = 1.15;
/// The alpha floor for a panel body. The contrast floors above are computed
/// against the panel's own RGB as if it were opaque; at 0.95 and above the
/// worst case a composite can shift the effective luminance by is 5%, which is
/// well inside the margin every floor is met with.
pub const PANEL_ALPHA_MIN: f32 = 0.95;
/// A recessed field (the search bar's query area) against the panel it is sunk
/// into. *"[the search bar] background grey also too light, so it does not
/// contrast enough"* — the well was a fixed mix off the panel and nothing
/// checked that the mix actually produced a visible recess.
/// It is deliberately shallow: on a near-white panel the whole range left
/// between the panel and paper white is 1.16, and deepening the well the *other*
/// way would sink it toward the text colour — a recess that costs legibility,
/// which is the trade this well must never make.
pub const FLOOR_FIELD: f32 = 1.12;
/// Accent text against primary text. The manual sets a two-column reference in
/// two colours — key names in [`Palette::accent`], their descriptions in
/// [`Palette::text`] — so the *difference* between those two roles carries
/// meaning. A scheme whose accent happens to land on top of its foreground
/// would silently turn that back into one undifferentiated slab.
pub const FLOOR_ROLE_SPLIT: f32 = 1.25;

/// The luminance rt assumes for whatever is behind its window.
///
/// A see-through window shows the desktop — or, on macOS, a frosted-glass
/// version of it. rt cannot sample either, so it has to assume. Mid grey is the
/// honest assumption on two counts: it is roughly where arbitrary screen content
/// averages out (the photographer's 18% card), and it is the luminance from
/// which the maximum contrast reachable in *either* direction is lowest — so
/// assuming it is also the conservative choice.
const BACKDROP_L: f32 = 0.20;

/// The largest fraction of a chrome pixel that may be unknown backdrop.
///
/// A translucent panel is only safe while what shows through it is *known* to be
/// close to the panel's own colour — which is true of a terminal painted in the
/// user's background colour, and false of an arbitrary desktop. See
/// [`panel_alpha`].
const UNKNOWN_MAX: f32 = 0.01;

/// The neutral mid-grey rt assumes is behind the window.
fn backdrop() -> [f32; 3] {
    with_lum(WHITE, BACKDROP_L)
}

/// **What the terminal area actually looks like on screen**: the user's
/// background colour composited at `opacity` over the assumed [`backdrop`].
///
/// This is the colour a floating panel really sits next to, and it is the thing
/// the theme reasons about now. The difference is not academic: at
/// `background_opacity = 0.05` — a perfectly ordinary setting on a Mac with
/// glass behind it — a near-black configured background of luminance 0.0008
/// reaches the eye at luminance 0.18. Every guarantee stated against the
/// configured colour was being stated about something that is 95% not there.
pub fn surround(bg: [f32; 3], opacity: f32) -> [f32; 3] {
    mix(backdrop(), bg, opacity.clamp(0.0, 1.0))
}

/// How opaque a panel has to be, given how see-through the window is.
///
/// A chrome pixel is `alpha` of the panel plus `1 - alpha` of the terminal
/// behind it, and that terminal is itself only `opacity` of the user's
/// background plus `1 - opacity` of unknown desktop. So the unknown fraction of
/// a chrome pixel is `(1 - alpha) * (1 - opacity)`, and holding it under
/// [`UNKNOWN_MAX`] is what keeps "the floors are computed against the panel's own
/// RGB as if it were opaque" a true statement rather than a hopeful one.
///
/// On an opaque window this asks for nothing and the theme's own alpha stands
/// (rt's Linux default at `opacity = 0.9` still gets its 0.96 panel). As the
/// window goes sheer the panel closes up to nearly solid, because the only
/// alternative is blending in a colour nobody can predict.
fn panel_alpha(theme_alpha: f32, opacity: f32) -> f32 {
    let unknown = (1.0 - opacity.clamp(0.0, 1.0)).max(1e-6);
    let need = 1.0 - UNKNOWN_MAX / unknown;
    theme_alpha.max(PANEL_ALPHA_MIN).max(need).clamp(0.0, 1.0)
}

/// The border floor for a window of this opacity — [`FLOOR_EDGE`] when the
/// window is opaque, rising to [`FLOOR_EDGE_SHEER`] as it goes see-through.
fn edge_floor(opacity: f32) -> f32 {
    FLOOR_EDGE + (FLOOR_EDGE_SHEER - FLOOR_EDGE) * (1.0 - opacity.clamp(0.0, 1.0))
}

/// What a chrome pixel of colour `body` at `alpha` actually looks like on
/// screen, over a terminal of this background and opacity. Every text floor is
/// held against THIS, not against `body`.
fn seen(body: [f32; 3], alpha: f32, bg: [f32; 3], opacity: f32) -> [f32; 3] {
    mix(surround(bg, opacity), body, alpha.clamp(0.0, 1.0))
}

// --- the pane titlebar strip ------------------------------------------------
//
// Not a floating panel, so not a `Palette` role — the per-pane titlebar is part
// of the pane, and the whole point of what follows is that it stays part of it.
// It lives here anyway because it is the same defect `surround`/`seen` exist
// for, one level down: the strip's colour was derived from the CONFIGURED
// background and then painted at alpha 1.0, so on a see-through window it was an
// opaque slab of a colour that is mostly not on screen — a different material
// sitting on top of the pane instead of the pane's own surface, lifted.

/// How far the pane titlebar strip is lifted from the pane's background toward
/// its foreground. A focused pane takes more tint, an unfocused one a whisper.
/// Interpolating between two real colours can never clip, so even a
/// black-on-white scheme yields a valid, scheme-native tint.
pub const BAR_LIFT_FOCUSED: f32 = 0.20;
/// See [`BAR_LIFT_FOCUSED`].
pub const BAR_LIFT_UNFOCUSED: f32 = 0.10;
/// The hairline under the strip: lifted further still, so the boundary between
/// the strip and the terminal body reads as an edge.
pub const BAR_LIFT_SEP: f32 = 0.40;
/// The vertical rule between newspaper columns — the fg/bg midpoint: visible,
/// but not text-weight. The same defect and the same fix as the titlebar, one
/// hairline further in: it is `mix(bg, fg, ..)` drawn straight onto the pane
/// body, so at alpha 1.0 it was an opaque rule in a colour the window is only
/// partly showing.
pub const COLUMN_RULE_LIFT: f32 = 0.5;

/// The alpha to paint the pane's **foreground** at, over a surface already
/// lifted `from` of the way from background toward foreground, so that the
/// result reads as that surface lifted to `to` — in a window of this `opacity`.
///
/// # Why paint the foreground rather than the mixed colour
///
/// The strip should be the pane body's own material, lifted — not a second
/// material laid over it. Composited over an unknown desktop `D`, the body is
/// `opacity * bg + (1 - opacity) * D`, so the strip *wants* to be
/// `opacity * mix(bg, fg, to) + (1 - opacity) * D`: the same `1 - opacity` of
/// desktop showing through.
///
/// Source-over cannot hit that exactly — every pass over the body *reduces* the
/// desktop's share, by exactly the alpha painted. So the rule is to spend as
/// little alpha as possible: painting pure `fg` reaches the tint `to` at alpha
/// `opacity * (to - from) / (1 - from)`, where painting the already-mixed colour
/// would need alpha `opacity` — five times more at the tints rt uses, and five
/// times more of the backdrop closed off. The residual error works out to
/// `to * opacity * (1 - opacity) * (bg - D)`, which is at most
/// `0.05 * (bg - D)` at [`BAR_LIFT_FOCUSED`] and vanishes at both ends of the
/// opacity range. [`tests::the_pane_titlebar_is_the_body_lifted_at_the_bodys_own_alpha`]
/// pins it.
///
/// At `opacity == 1.0` this is exactly the old opaque behaviour, to the bit:
/// `lift_alpha(0, t, 1) == t`, and `mix(bg, fg, t)` is what a fill of `fg` at
/// alpha `t` over `bg` produces. An opaque window sees no change at all.
pub fn lift_alpha(from: f32, to: f32, opacity: f32) -> f32 {
    let from = from.clamp(0.0, 1.0);
    let to = to.clamp(0.0, 1.0);
    if to <= from {
        return 0.0; // nothing to add (and the `1 - from` divisor cannot be zero below)
    }
    (opacity.clamp(0.0, 1.0) * (to - from) / (1.0 - from)).clamp(0.0, 1.0)
}

/// The fill for one band of the pane titlebar: the strip itself (`from = 0.0`,
/// `to` = one of [`BAR_LIFT_FOCUSED`]/[`BAR_LIFT_UNFOCUSED`]) or the hairline
/// drawn on top of it (`from` = the strip's lift, `to` = [`BAR_LIFT_SEP`]).
///
/// `blends` is [`crate::backend::Backend::is_gl`] — whether this backend's
/// `fill_rect` composites with what is already in the frame. The GL and wgpu
/// backends do. The XRender backend does **not**: its content fills are
/// `PictOp::SRC`, a replace rather than a composite, with a straight (not
/// premultiplied) colour. Handing it a translucent foreground would paint a
/// full-strength `fg` slab, which is worse than what it has today — so it keeps
/// exactly what it has today, the pre-mixed colour at alpha 1.0. That is the
/// honest answer for a backend that cannot composite, and it costs nothing:
/// XRender is the `ssh -X` path, where there is no blur to show through anyway.
pub fn lift_fill(bg: [u8; 3], fg: [u8; 3], from: f32, to: f32, opacity: f32, blends: bool) -> Color {
    let (bgf, fgf) = (to_f(bg), to_f(fg));
    if blends {
        col(fgf, lift_alpha(from, to, opacity))
    } else {
        col(mix(bgf, fgf, to), 1.0)
    }
}

/// The luminance that hits exactly `ratio` against a background of luminance
/// `bg_l`, on the side a `dark` panel's text lives on (brighter for a dark
/// panel, darker for a light one). Straight from the WCAG definition.
fn floor_lum(bg_l: f32, ratio: f32, dark: bool) -> f32 {
    let l = if dark { ratio * (bg_l + 0.05) - 0.05 } else { (bg_l + 0.05) / ratio - 0.05 };
    l.clamp(0.0, 1.0)
}

impl Palette {
    /// Derive the whole palette from the user's colours.
    ///
    /// `fg`/`bg` are `Settings::foreground`/`background`; `accent` is the
    /// user's own bright-blue palette entry, so a selection bar is in the
    /// family of colours the terminal already uses.
    ///
    /// `opacity` is `Settings::background_opacity` — how much of the window is
    /// really the user's background and how much is desktop showing through.
    /// Everything the palette claims about *separation* is claimed against
    /// [`surround`], the colour that composite actually produces, and every text
    /// floor is held against [`seen`], the panel colour that composite actually
    /// produces. Passing the configured background alone (which is what this
    /// used to do) states those guarantees about a colour that, on a sheer
    /// window, is almost entirely not on screen.
    ///
    /// The shape of it: decide dark vs light from the background's luminance;
    /// pin the panel body into the legible band for that side (keeping the
    /// background's hue, except under [`ChromeTheme::Graphite`], which
    /// deliberately drops it); then derive every other role from the panel and
    /// hold each to its floor with [`ensure_contrast`].
    pub fn derive(fg: [u8; 3], bg: [u8; 3], accent: [u8; 3], theme: ChromeTheme, opacity: f32) -> Palette {
        let bg = to_f(bg);
        let fg = to_f(fg);
        let accent = to_f(accent);
        let opacity = if opacity.is_finite() { opacity.clamp(0.0, 1.0) } else { 1.0 };
        // Light-vs-dark stays the USER's decision, taken from the colour they
        // configured. It deliberately does NOT follow `surround`: an assumed
        // backdrop is a guess, and a guess must not be allowed to hand somebody
        // who chose a light terminal a black menu. The guess only ever informs
        // how far apart things have to be, never which side they are on.
        let dark = lum(bg) < 0.5;
        // What the panel will really be sitting next to.
        let around = surround(bg, opacity);

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
        // How solid the panel has to be for the floors below to mean anything.
        let alpha = panel_alpha(alpha, opacity);
        // The panel must be visibly a different surface from the terminal behind
        // it. A pure-white terminal is the case that forces this: the light-mode
        // band alone lands the panel within 1.06:1 of the background, which is
        // no edge at all. Push it further from what is ACTUALLY behind it — the
        // composited `around`, not the configured colour.
        let body = ensure_separation(body, around, FLOOR_PANEL, alpha, dark);
        // From here on, floors are held against the colour the panel resolves to
        // on screen. With `alpha` set as above the two are within 1% of each
        // other, so this is a small correction — but it is the correction that
        // makes "text clears 7:1 on the panel" a statement about the panel the
        // user is looking at.
        let panel_seen = seen(body, alpha, bg, opacity);
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
        let edge = ensure_contrast(mix(body, away, edge_mix), panel_seen, edge_floor(opacity));
        let sep = ensure_contrast(mix(body, away, edge_mix * 0.6), panel_seen, 1.18);
        let hover = mix(body, away, if dark { 0.13 } else { 0.10 });
        let thumb = mix(body, away, 0.34);
        // A well is recessed: darker than the panel in a dark theme, brighter
        // (toward paper white) in a light one.
        let recess = if dark { BLACK } else { WHITE };
        let field = mix(body, recess, if dark { 0.45 } else { 0.55 });
        // …and it has to be a recess you can SEE, not just a different number.
        // Deepened only ever AWAY from the text: `ensure_separation` would be
        // free to pick the other pole when this one runs out of room, and on a
        // near-white panel that means a well DARKER than the panel — which is a
        // visible recess bought with the legibility of the text sitting in it.
        let field = {
            const STEPS: usize = 64;
            let mut deepened = mix(field, recess, 1.0);
            for i in 0..=STEPS {
                let c = mix(field, recess, i as f32 / STEPS as f32);
                if contrast(c, panel_seen) >= FLOOR_FIELD {
                    deepened = c;
                    break;
                }
            }
            deepened
        };

        // 3. Text, each held to its floor, each starting from the user's own colour.
        let text = ensure_contrast(fg, panel_seen, floor_text);
        let dim = ensure_contrast(mix(text, panel_seen, 0.40), panel_seen, FLOOR_DIM);
        let off = ensure_contrast(mix(text, panel_seen, 0.62), panel_seen, FLOOR_OFF);
        let accent_text = ensure_contrast(accent, panel_seen, FLOOR_ACCENT);
        // The manual sets its key column in `accent` and the descriptions beside
        // it in `text`, so those two roles have to be TELLABLE APART or the
        // colour coding says nothing. A scheme whose accent already sits on top
        // of its foreground gets its accent restated at exactly the dimmest
        // luminance FLOOR_ACCENT allows. That is guaranteed to be a clear step
        // below `text`, because `text` is held to a strictly higher floor
        // against the same panel — the gap is at least FLOOR_TEXT/FLOOR_ACCENT,
        // which is 1.56, comfortably past FLOOR_ROLE_SPLIT.
        let accent_text = if contrast(accent_text, text) >= FLOOR_ROLE_SPLIT {
            accent_text
        } else {
            let at_floor = floor_lum(lum(panel_seen), FLOOR_ACCENT, dark);
            ensure_contrast(with_lum(accent_text, at_floor), panel_seen, FLOOR_ACCENT)
        };

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
            // `alpha` came out of `panel_alpha`, which is what keeps the floors
            // above — all computed against `panel_seen` — true of the pixels
            // that actually reach the screen.
            panel: col(body, alpha),
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
        Palette::derive(s.foreground, s.background, s.palette[12], s.chrome_theme, s.background_opacity)
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

    /// Straight-alpha source over, the one operation both blending backends
    /// perform: GL's shader premultiplies and blends `ONE, ONE_MINUS_SRC_ALPHA`,
    /// wgpu's does the same (`wgpu_frame::premultiplied`). Returns the RGB the
    /// eye sees and the alpha the *window* now carries at that pixel, which is
    /// what the compositor uses to let the desktop through.
    fn over(src: [f32; 3], a: f32, dst: [f32; 3], dst_a: f32) -> ([f32; 3], f32) {
        (mix(dst, src, a), a + dst_a * (1.0 - a))
    }

    /// **The pane titlebar guarantee.** The strip is the pane body's own
    /// material lifted toward the foreground, carrying the body's own alpha —
    /// not an opaque slab of a colour that, on a see-through window, is mostly
    /// not on screen. Pinned against the exact composite the backends perform.
    #[test]
    fn the_pane_titlebar_is_the_body_lifted_at_the_bodys_own_alpha() {
        // The residual of painting `fg` instead of the mixed colour, derived in
        // `lift_alpha`: `to * opacity * (1 - opacity) * (bg - D)`, maximised at
        // `opacity = 0.5` where `opacity * (1 - opacity) = 0.25`, and with
        // `|bg - D| <= 1`.
        let bound = BAR_LIFT_SEP * 0.25 + 1e-6;
        // How much MORE opaque than the pane body each band leaves the window.
        // One pass costs `to * opacity * (1 - opacity) <= to / 4`. The hairline
        // is a second pass on top of the strip, so it compounds a little past
        // that: solving `d/dp [a2 (1 - A1) + a1 (1 - p)] = 0` puts its true
        // maximum at 0.1064 (opacity 0.485, focused), rounded up here.
        let strip_alpha_bound = BAR_LIFT_FOCUSED * 0.25 + 1e-6;
        let hair_alpha_bound = 0.11;
        for (fg, bg, _) in schemes() {
            let (fgf, bgf) = (to_f(fg), to_f(bg));
            for desktop_l in [0.0_f32, 0.5, 1.0] {
                let desktop = [desktop_l; 3];
                for opacity in [0.0_f32, 0.25, 0.5, 0.65, 0.9, 1.0] {
                    for lift in [BAR_LIFT_FOCUSED, BAR_LIFT_UNFOCUSED] {
                        // The pane body as it reaches the eye, and the window
                        // alpha it carries.
                        let body = mix(desktop, bgf, opacity);

                        // The strip, painted the way `lift_fill` says to.
                        let c = lift_fill(bg, fg, 0.0, lift, opacity, true);
                        let (strip, strip_a) = over(rgb(c), c.3, body, opacity);

                        // What it is *supposed* to look like: the body's colour
                        // lifted toward fg, composited at the body's opacity.
                        let ideal = mix(desktop, mix(bgf, fgf, lift), opacity);
                        for i in 0..3 {
                            assert!(
                                (strip[i] - ideal[i]).abs() <= bound,
                                "strip {strip:?} vs ideal {ideal:?} at opacity {opacity} lift {lift}"
                            );
                        }
                        // And it lets the backdrop through like the body does,
                        // instead of sealing it off the way an opaque slab did.
                        assert!(
                            strip_a - opacity <= strip_alpha_bound,
                            "strip alpha {strip_a} is not in the body's world ({opacity})"
                        );
                        assert!(strip_a >= opacity - 1e-6, "a strip never makes the window MORE sheer");

                        // The hairline, drawn on top of the strip, lands in the
                        // same place relative to the body.
                        let h = lift_fill(bg, fg, lift, BAR_LIFT_SEP, opacity, true);
                        let (hair, hair_a) = over(rgb(h), h.3, strip, strip_a);
                        let hair_ideal = mix(desktop, mix(bgf, fgf, BAR_LIFT_SEP), opacity);
                        for i in 0..3 {
                            assert!(
                                (hair[i] - hair_ideal[i]).abs() <= bound,
                                "hairline {hair:?} vs ideal {hair_ideal:?} at opacity {opacity}"
                            );
                        }
                        assert!(
                            hair_a - opacity <= hair_alpha_bound,
                            "hairline alpha {hair_a} is not in the body's world ({opacity})"
                        );
                        assert!(hair_a >= strip_a - 1e-6, "a hairline never makes the strip MORE sheer");
                    }
                }
            }
        }
    }

    /// An OPAQUE window must be bit-identical to the slab rt drew before this
    /// change — that is what makes the Linux delta a function of opacity alone,
    /// and it is the only reason this is safe to ship to both platforms at once.
    #[test]
    fn an_opaque_window_gets_exactly_the_colours_it_always_did() {
        for (fg, bg, _) in schemes() {
            let (fgf, bgf) = (to_f(fg), to_f(bg));
            for lift in [BAR_LIFT_FOCUSED, BAR_LIFT_UNFOCUSED] {
                let c = lift_fill(bg, fg, 0.0, lift, 1.0, true);
                assert_eq!(c.3, lift, "opaque: the alpha IS the lift");
                let (strip, strip_a) = over(rgb(c), c.3, bgf, 1.0);
                assert_eq!(strip_a, 1.0, "an opaque window stays opaque");
                for i in 0..3 {
                    assert!((strip[i] - mix(bgf, fgf, lift)[i]).abs() < 1e-6, "the old mix(bg, fg, t)");
                }
                // The hairline compounds correctly on top of it: lifted from the
                // strip's tint to BAR_LIFT_SEP lands on mix(bg, fg, BAR_LIFT_SEP).
                let h = lift_fill(bg, fg, lift, BAR_LIFT_SEP, 1.0, true);
                let (hair, _) = over(rgb(h), h.3, strip, 1.0);
                for i in 0..3 {
                    assert!((hair[i] - mix(bgf, fgf, BAR_LIFT_SEP)[i]).abs() < 1e-6);
                }
            }
        }
    }

    /// A backend that cannot composite gets what it has today rather than a
    /// full-strength foreground slab. XRender's content fills are `PictOp::SRC`
    /// — a replace, not a blend — so alpha there is not a tint, it is a hole.
    #[test]
    fn a_non_blending_backend_keeps_the_opaque_premixed_colour() {
        for (fg, bg, _) in schemes() {
            for opacity in [0.0_f32, 0.65, 1.0] {
                for lift in [BAR_LIFT_FOCUSED, BAR_LIFT_UNFOCUSED, BAR_LIFT_SEP] {
                    let c = lift_fill(bg, fg, 0.0, lift, opacity, false);
                    assert_eq!(c.3, 1.0, "no alpha ever reaches a SRC fill");
                    let want = mix(to_f(bg), to_f(fg), lift);
                    for i in 0..3 {
                        assert!((rgb(c)[i] - want[i]).abs() < 1e-6, "exactly the pre-change colour");
                    }
                }
            }
        }
    }

    /// A fully transparent window contributes no chrome either — which is the
    /// point of allowing `background_opacity = 0` with blur on: at that setting
    /// rt adds nothing to the frosted backdrop but its text.
    #[test]
    fn a_fully_transparent_window_paints_no_titlebar_slab() {
        assert_eq!(lift_alpha(0.0, BAR_LIFT_FOCUSED, 0.0), 0.0);
        assert_eq!(lift_alpha(BAR_LIFT_FOCUSED, BAR_LIFT_SEP, 0.0), 0.0);
        // And the degenerate lifts are inert rather than a division by zero.
        assert_eq!(lift_alpha(BAR_LIFT_SEP, BAR_LIFT_SEP, 1.0), 0.0, "no lift asked for");
        assert_eq!(lift_alpha(BAR_LIFT_SEP, BAR_LIFT_FOCUSED, 1.0), 0.0, "a downward lift is not a fill");
        assert_eq!(lift_alpha(1.0, 1.0, 1.0), 0.0, "from == 1.0 must not divide by zero");
    }

    /// THE guarantee. Whatever the user's colours, and whichever chrome theme is
    /// chosen, every text role clears its stated floor against the panel it is
    /// drawn on, the border is visible against its own panel, and the panel is
    /// visible against the terminal behind it.
    #[test]
    fn every_theme_is_legible_over_every_scheme() {
        for theme in rt_config::ChromeTheme::ALL {
            for (fg, bg, accent) in schemes() {
                // Every opacity a user can dial in, not just the opaque default:
                // 0.05 is what a Mac with glass behind it actually runs at, and
                // it is the setting under which the old derivation was reasoning
                // about a background colour that was 95% not on screen.
                for &op in &[1.0_f32, 0.9, 0.5, 0.2, 0.05, 0.0] {
                    let p = Palette::derive(fg, bg, accent, *theme, op);
                    // The floors are claims about the SCREEN, so they are tested
                    // against what the composite really produces.
                    let body = seen(rgb(p.panel), p.panel.3, to_f(bg), op);
                    let around = surround(to_f(bg), op);
                    let name = theme.name();
                    let ctx = format!("{name} over bg {bg:?} at opacity {op}");
                    assert!(p.panel.3 >= PANEL_ALPHA_MIN, "{ctx}: panel alpha {}", p.panel.3);
                    // A chrome pixel is almost entirely a colour rt chose, never
                    // mostly a desktop rt cannot see.
                    let unknown = (1.0 - p.panel.3) * (1.0 - op);
                    assert!(unknown <= UNKNOWN_MAX + 1e-6, "{ctx}: {unknown} of the panel is unknown backdrop");
                    assert!(contrast(rgb(p.text), body) >= FLOOR_TEXT - 0.01, "{ctx}: text");
                    assert!(contrast(rgb(p.dim), body) >= FLOOR_DIM - 0.01, "{ctx}: dim");
                    assert!(contrast(rgb(p.off), body) >= FLOOR_OFF - 0.01, "{ctx}: off");
                    assert!(contrast(rgb(p.accent), body) >= FLOOR_ACCENT - 0.01, "{ctx}: accent");
                    assert!(contrast(rgb(p.sel_text), rgb(p.sel)) >= FLOOR_SEL_TEXT - 0.01, "{ctx}: sel text");
                    // The border carries more of the separation the sheerer the
                    // window gets, because nothing else can be relied on.
                    assert!(
                        contrast(rgb(p.edge), body) >= edge_floor(op) - 0.01,
                        "{ctx}: edge {} < {}",
                        contrast(rgb(p.edge), body),
                        edge_floor(op)
                    );
                    // The panel stands off the terminal AS SEEN, not as configured.
                    assert!(contrast(body, around) >= FLOOR_PANEL - 0.01, "{ctx}: panel vs terminal");
                    // Key names and their descriptions are two different colours
                    // in the manual; they have to look it.
                    assert!(
                        contrast(rgb(p.accent), rgb(p.text)) >= FLOOR_ROLE_SPLIT - 0.01,
                        "{ctx}: accent is indistinguishable from text ({})",
                        contrast(rgb(p.accent), rgb(p.text))
                    );
                }
                let p = Palette::derive(fg, bg, accent, *theme, 1.0);
                let body = rgb(p.panel);
                let name = theme.name();
                let ctx = format!("{name} over bg {bg:?}");
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
        let t = Palette::derive([0xd0, 0xd0, 0xd8], bg, [0x8b, 0x7c, 0xff], ChromeTheme::Tinted, 1.0);
        let g = Palette::derive([0xd0, 0xd0, 0xd8], bg, [0x8b, 0x7c, 0xff], ChromeTheme::Graphite, 1.0);
        assert!(hue_spread(t.panel) > 0.02, "tinted must carry the terminal's hue: {:?}", t.panel);
        assert!(hue_spread(g.panel) < 0.005, "graphite must be neutral: {:?}", g.panel);
        // And the blue channel leads in the tinted panel, as it does in the bg.
        assert!(t.panel.2 > t.panel.1, "the tint follows the background");
    }

    /// Light and dark terminals both get chrome, and they are not the same
    /// chrome: a white terminal must not be handed a black menu.
    #[test]
    fn a_light_terminal_gets_light_chrome() {
        let dark = Palette::derive([0xd0; 3], [0x10, 0x10, 0x14], [0x5c, 0x5c, 0xff], ChromeTheme::Tinted, 1.0);
        let light = Palette::derive([0x20; 3], [0xfd, 0xf6, 0xe3], [0x26, 0x8b, 0xd2], ChromeTheme::Tinted, 1.0);
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
        let t = Palette::derive(fg, bg, ac, ChromeTheme::Tinted, 1.0);
        let g = Palette::derive(fg, bg, ac, ChromeTheme::Graphite, 1.0);
        let c = Palette::derive(fg, bg, ac, ChromeTheme::Contrast, 1.0);
        assert_eq!(c.panel.3, 1.0, "contrast mode is fully opaque");
        assert!(t.panel.3 < 1.0 && g.panel.3 < 1.0, "the softer themes let the terminal through");
        assert!(contrast(rgb(c.text), rgb(c.panel)) > contrast(rgb(t.text), rgb(t.panel)));
        assert!(contrast(rgb(c.edge), rgb(c.panel)) > contrast(rgb(t.edge), rgb(t.panel)));
        // All three are distinguishable panels.
        assert!(contrast(rgb(t.panel), rgb(g.panel)) > 1.05, "tinted vs graphite");
    }

    /// THE opacity defect. *"The grey in the preferences and the manual is ugly
    /// compared to the rest in the OSX GUI. On Linux it is not ugly."* Same code,
    /// same panel colour — the difference between the two machines is that one
    /// runs at `background_opacity = 0.90` and the other at `0.05`, and
    /// `derive` used to take no opacity at all.
    ///
    /// At 0.05 a near-black background reaches the eye at roughly the luminance
    /// of the desktop behind it. Every "the panel stands off the terminal"
    /// guarantee was being made about a colour that is 95% not on screen.
    #[test]
    fn the_theme_reasons_about_what_is_on_screen_not_what_is_configured() {
        // Roland's two machines, verbatim from their config.toml files.
        let linux = ([248, 194, 0], [13, 0, 28], 0.90_f32);
        let macos = ([5, 255, 12], [2, 2, 9], 0.05_f32);
        for (fg, bg, op) in [linux, macos] {
            let cfg = to_f(bg);
            let on_screen = surround(cfg, op);
            let p = Palette::derive(fg, bg, [0x5c, 0x5c, 0xff], ChromeTheme::Tinted, op);
            let body = seen(rgb(p.panel), p.panel.3, cfg, op);
            assert!(contrast(body, on_screen) >= FLOOR_PANEL - 0.01, "panel must stand off what is THERE");
            assert!(contrast(rgb(p.text), body) >= FLOOR_TEXT - 0.01, "text on the panel as composited");
        }
        // The two are not the same situation, which is the whole point: the
        // configured backgrounds are both near-black and within a hair of each
        // other in luminance, but what reaches the eye is an order of magnitude
        // apart. A derivation blind to opacity cannot tell them apart at all.
        let l_seen = lum(surround(to_f(linux.1), linux.2));
        let m_seen = lum(surround(to_f(macos.1), macos.2));
        assert!((lum(to_f(linux.1)) - lum(to_f(macos.1))).abs() < 0.002, "configured: indistinguishable");
        assert!(m_seen > l_seen * 10.0, "on screen: {m_seen} vs {l_seen} — not remotely the same surface");
    }

    /// A panel may only be see-through while what shows through it is a colour
    /// rt chose. The sheerer the window, the more of that is unknowable desktop,
    /// so the panel closes up — and the border, which is then the only reliable
    /// separator, is held to a higher floor.
    #[test]
    fn a_sheer_window_gets_a_solid_panel_and_a_stronger_border() {
        let (fg, bg, ac) = ([0xd0, 0xd0, 0xd8], [0x14, 0x10, 0x22], [0x8b, 0x7c, 0xff]);
        let opaque = Palette::derive(fg, bg, ac, ChromeTheme::Tinted, 1.0);
        let linuxish = Palette::derive(fg, bg, ac, ChromeTheme::Tinted, 0.9);
        let sheer = Palette::derive(fg, bg, ac, ChromeTheme::Tinted, 0.05);
        // rt's own Linux default is opaque enough that the theme's soft 0.96
        // stands: this change must not quietly solidify chrome that was fine.
        assert_eq!(opaque.panel.3, linuxish.panel.3, "0.9 is opaque enough to leave alone");
        assert!(sheer.panel.3 > linuxish.panel.3, "a sheer window gets a more solid panel");
        assert!(sheer.panel.3 > 0.98, "…and nearly solid at that: {}", sheer.panel.3);
        let edge_c = |p: &Palette| contrast(rgb(p.edge), rgb(p.panel));
        assert!(edge_c(&sheer) >= edge_floor(0.05) - 0.01, "sheer edge {} < floor", edge_c(&sheer));
        assert!(edge_c(&sheer) > edge_c(&linuxish) * 1.25, "a sheer window needs a border you can see");
        assert!(edge_floor(1.0) < edge_floor(0.0), "the border floor rises as the window goes sheer");
        // Nothing about the panel's own colour is thrown away to get there.
        assert!(contrast(rgb(sheer.text), rgb(sheer.panel)) >= FLOOR_TEXT - 0.01);
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

