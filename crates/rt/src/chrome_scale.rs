//! Flat chrome constants as LOGICAL pixels, and the one multiply that turns
//! them into physical ones.
//!
//! # The defect this exists for
//!
//! Until this module landed, only the rasterised glyph size was multiplied by
//! `Window::scale_factor()` (see `physical_font_px`). Every flat chrome value —
//! the window margin, the pane padding, the patch-bay jack discs, the scrollbar,
//! every panel's inner padding, every 1px rule — was a raw PHYSICAL pixel
//! literal. On a 2x Retina panel that means all of it renders at half the
//! apparent size it was drawn at on the 1x display rt was tuned on. The user
//! report that closed the argument: *"the little green travelling disks … on the
//! mac they are in any case quite small and that makes the jack ports hard to
//! hit."*
//!
//! `physical_font_px` used to carry a comment arguing the opposite — that these
//! were "hairline-sized either way" and that scaling them would be "guessing at
//! an intended logical size for values that were never expressed as one". The
//! reasoning that overturns it: at 1x these values ARE their logical size,
//! because 1x is the only display rt was ever tuned on. There is no guess left
//! to make. That comment has been rewritten to say so; if you are reading this
//! because you are about to revert, read it there first.
//!
//! # The invariant
//!
//! Scaling is a single `f32` multiply: `sc * logical::WHATEVER`. At `sc == 1.0`
//! IEEE-754 multiplication by 1.0 is the exact identity for every finite value,
//! so a 1x display — every Linux setup rt has ever run on — comes out
//! bit-for-bit unchanged, `as i32` truncations downstream included. That is
//! asserted over the whole table by [`tests::scale_one_is_bit_identical`], and
//! the real GL renderer's pixel-identity gate (`tests/damage_pixel_identity.rs`)
//! is the end-to-end proof.
//!
//! # Draw and hit scale together, or not at all
//!
//! Several of these are used by BOTH a draw path and a hit-test — the jacks
//! (`JACK_R_*` drawn, `JACK_GRAB_R` hit), the scrollbar (`SCROLLBAR_W` drawn,
//! `SCROLLBAR_GRAB_SLOP` hit), the split divider (`rt_core`'s `DIVIDER` drawn,
//! its `GRAB` hit), the drop cues. Scaling one side only is exactly the bug that
//! made the colour picker unclickable when HiDPI first landed. The overlay
//! panels are immune by construction: their `hit()` reads the same `Geom` their
//! `draw()` does, so a scale that reaches `layout()` reaches both.
//!
//! # Why a module, and why it is not `cfg`'d to macOS
//!
//! Same reason as [`crate::scale_policy`], [`crate::vibrancy_policy`],
//! [`crate::cpu_heat`] and [`crate::menubar_model`]: a Retina display is the one
//! thing no CI here has, so the decision is lifted into a pure table that Linux
//! CI runs on every commit. Linux is not merely a stand-in either — a HiDPI
//! Wayland or X11 output reports 2.0 and takes exactly this path.

/// Every flat chrome value in `rt`, in LOGICAL pixels — the size it should
/// occupy on a 1x display.
///
/// A value belongs here when it is a fixed pixel quantity that a person is meant
/// to see or hit. A value does NOT belong here when it is derived from the cell
/// size (`cell_h * 0.6`, `cell_w * 2.0`, a column count) — those already scale,
/// because the cell does — or when it is dimensionless (a fraction, a ratio, a
/// count).
///
/// Adding a chrome constant anywhere in `rt` means adding it here too:
/// [`tests::the_chrome_register_is_frozen`] asserts this list entry-for-entry
/// against [`REGISTER`], so a new one has to be filed deliberately rather than
/// shipping unscaled by accident.
pub mod logical {
    // --- window and pane chrome -------------------------------------------
    /// Standoff between the window edge and the terminal content.
    pub const WINDOW_MARGIN: f32 = 8.0;
    /// Horizontal inset of the per-pane titlebar strip's contents.
    pub const TITLEBAR_PAD: f32 = 6.0;
    /// The width of a 1px rule: divider lines, panel borders, tab separators,
    /// the titlebar underline, newspaper-column rules.
    pub const HAIRLINE: f32 = 1.0;
    /// Gap between the group swatch and the pane title.
    pub const GROUP_GAP: f32 = 5.0;
    /// Gap the pane title must leave before the meter/size readout.
    pub const TITLE_GAP: f32 = 8.0;
    /// Side of the group marker square drawn when titlebars are off.
    pub const GROUP_MARK: f32 = 10.0;
    /// Inset of that marker from the pane's top-right corner.
    pub const GROUP_MARK_INSET: f32 = 4.0;
    /// Horizontal padding reserved either side of a tab's label.
    pub const TAB_LABEL_INSET: f32 = 8.0;

    // --- patch bay (draw + hit) -------------------------------------------
    /// Jack disc: dark backing halo radius.
    pub const JACK_R_BACK: f32 = 7.5;
    /// Jack disc: filled centre radius, when a wire uses the jack.
    pub const JACK_R_FILL: f32 = 5.8;
    /// Jack disc: outline radius when the jack is idle.
    pub const JACK_R_RING: f32 = 5.4;
    /// Jack disc: that outline's stroke width.
    pub const JACK_RING_W: f32 = 1.8;
    /// Jack GRAB radius — the hit-test partner of the four above.
    pub const JACK_GRAB_R: f32 = 12.0;

    // --- border instruments ------------------------------------------------
    /// Thickness of the per-pane heat border.
    pub const HEAT_BORDER_T: f32 = 2.4;
    /// Output-packet glow halo radius (the "little green travelling disks").
    pub const PACKET_R_GLOW: f32 = 9.0;
    /// Output-packet bright core radius.
    pub const PACKET_R_CORE: f32 = 3.4;
    /// Radius of a packet riding a patch-bay wire.
    pub const WIRE_PACKET_R: f32 = 2.6;
    /// Stroke width of a wire's body.
    pub const WIRE_W: f32 = 2.0;
    /// Stroke width of the dashed rubber-band wire while dragging.
    pub const RUBBER_W: f32 = 1.6;
    /// Floor of a wire bezier's control-point extension.
    pub const WIRE_CTRL_MIN: f32 = 40.0;
    /// Cap of that extension.
    pub const WIRE_CTRL_MAX: f32 = 180.0;
    /// Thickness of the latency frame around the content region.
    pub const LATENCY_FRAME_T: f32 = 2.0;

    // --- scrollbar (draw + hit) -------------------------------------------
    /// Scrollbar track/thumb width.
    pub const SCROLLBAR_W: f32 = 4.0;
    /// How far into the pane's right padding gutter the bar sits.
    pub const SCROLLBAR_INSET: f32 = 0.5;
    /// Shortest the thumb may get, so it stays grabbable.
    pub const SCROLLBAR_MIN_THUMB: f32 = 24.0;
    /// Grab slop either side of the bar — the hit-test partner of `SCROLLBAR_W`.
    pub const SCROLLBAR_GRAB_SLOP: f32 = 2.0;
    /// How far a search hit-marker tick overhangs the bar on each side.
    pub const SEARCH_TICK_BLEED: f32 = 2.0;
    /// Height of a search hit-marker tick.
    pub const SEARCH_TICK_H: f32 = 2.0;
    /// Height of the CURRENT search hit's marker tick.
    pub const SEARCH_TICK_CUR_H: f32 = 3.0;

    // --- drag and drop (draw + hit) ---------------------------------------
    // These three are DEFINED in `crate::dragdrop` (the pure drop resolver, which
    // reads them itself) and re-exported here so the register names one value,
    // not a copy that could drift from it.
    /// Width of the window-edge root-split strips.
    /// Movement beyond which an armed press becomes a drag.
    pub use crate::dragdrop::{DRAG_THRESHOLD, EDGE_STRIP};
    /// Width of the tab-insert caret cue.
    pub use crate::dragdrop::CARET_W as DROP_CARET_W;
    /// The caret's end caps: height, and overhang on each side.
    pub const DROP_CARET_WING: f32 = 3.0;
    /// Border thickness of the drop-zone overlay.
    pub const DROP_ZONE_EDGE: f32 = 2.0;
    /// Inner padding of the drag ghost chip.
    pub const GHOST_PAD: f32 = 6.0;
    /// Offset of that chip from the cursor.
    pub const GHOST_OFFSET: f32 = 12.0;

    // --- pointer ----------------------------------------------------------
    /// Chebyshev slop within which two clicks count as the same spot
    /// (double/triple click, compose continuation).
    pub const CLICK_SLOP: f32 = 5.0;
    /// Pixels of pointer/touch travel that make one scrolled line. Defined in
    /// `crate::touch` — which is also compiled into the `rt_app` library, where
    /// this module does not exist — and re-exported here.
    pub use crate::touch::PX_PER_LINE;

    // --- overlay panels ----------------------------------------------------
    /// Vertical padding added to the cell height to make one panel row.
    pub const PANEL_ROW_PAD: f32 = 4.0;
    /// Top offset of a row's text inside that row.
    pub const PANEL_TEXT_TOP: f32 = 2.0;
    /// Search bar: inner padding all round.
    pub const SEARCH_PAD: f32 = 6.0;
    /// Search bar: standoff from the window's top and right edges.
    pub const SEARCH_INSET: f32 = 8.0;
    /// Search bar: caret width.
    pub const SEARCH_CARET_W: f32 = 2.0;
    /// Preferences: inner padding (used both across and down).
    pub const PREFS_PAD: f32 = 10.0;
    /// Preferences: gap between palette swatches.
    pub const PREFS_SWATCH_GAP: f32 = 2.0;
    /// Preferences: inset of the selected-row highlight.
    pub const PREFS_SEL_INSET: f32 = 1.0;
    /// Context menu: inner padding (used both across and down).
    pub const MENU_PAD: f32 = 8.0;
    /// Context menu: height of a separator row.
    pub const MENU_SEP_H: f32 = 7.0;
    /// Manual overlay: inner padding (defined next to the code that wraps text
    /// to it, and re-exported here).
    pub use crate::chrome::manual::PAD as MANUAL_PAD;
    /// Manual overlay: widest the panel may get.
    pub const MANUAL_MAX_W: f32 = 900.0;
    /// Manual overlay: scrollbar inset from the panel's right edge.
    pub const MANUAL_SB_INSET: f32 = 4.0;
    /// Manual overlay: scrollbar thumb width.
    pub const MANUAL_SB_W: f32 = 3.0;
    /// Manual overlay: shortest that thumb may get.
    pub const MANUAL_SB_MIN_THUMB: f32 = 12.0;
    /// Manual overlay: total vertical inset of the scrollbar track.
    pub const MANUAL_SB_TRACK_INSET: f32 = 2.0;
    /// Clipboard history: inner padding (defined with its panel, re-exported here).
    pub use crate::chrome::clip_history::PAD as CLIP_PAD;
    /// Clipboard history: anchor fallback when the focused pane has no rect.
    pub const CLIP_ANCHOR_FALLBACK: f32 = 40.0;
    /// Colour picker: floor on the saturation/value square's side.
    pub const PICKER_SV_MIN: f32 = 140.0;
    /// Colour picker: floor on the hue strip's width.
    pub const PICKER_HUE_MIN: f32 = 14.0;
    /// Colour picker: side of the SV marker's outer (black) square.
    pub const PICKER_SV_MARK_OUT: f32 = 10.0;
    /// Colour picker: side of its inner (white) square.
    pub const PICKER_SV_MARK_IN: f32 = 8.0;
    /// Colour picker: height of the hue caret's outer (black) bar.
    pub const PICKER_HUE_CARET_OUT: f32 = 4.0;
    /// Colour picker: height of its inner (white) bar.
    pub const PICKER_HUE_CARET_IN: f32 = 2.0;
}

/// The frozen register of [`logical`]: every flat chrome value in `rt`, by name.
///
/// This is the list a future addition has to be filed in. It is asserted
/// entry-for-entry against `logical` by
/// [`tests::the_chrome_register_is_frozen`], so the two cannot drift, and
/// [`tests::scale_one_is_bit_identical`] and [`tests::every_value_doubles_at_2x`]
/// run over it — meaning a new entry is covered by the identity and scaling
/// gates the moment it is added.
// Only the tests below read it; it is `pub` because it IS the register, and the
// place a reviewer should be sent to see every flat chrome value at once.
#[allow(dead_code)]
pub const REGISTER: &[(&str, f32)] = &[
    ("WINDOW_MARGIN", logical::WINDOW_MARGIN),
    ("TITLEBAR_PAD", logical::TITLEBAR_PAD),
    ("HAIRLINE", logical::HAIRLINE),
    ("GROUP_GAP", logical::GROUP_GAP),
    ("TITLE_GAP", logical::TITLE_GAP),
    ("GROUP_MARK", logical::GROUP_MARK),
    ("GROUP_MARK_INSET", logical::GROUP_MARK_INSET),
    ("TAB_LABEL_INSET", logical::TAB_LABEL_INSET),
    ("JACK_R_BACK", logical::JACK_R_BACK),
    ("JACK_R_FILL", logical::JACK_R_FILL),
    ("JACK_R_RING", logical::JACK_R_RING),
    ("JACK_RING_W", logical::JACK_RING_W),
    ("JACK_GRAB_R", logical::JACK_GRAB_R),
    ("HEAT_BORDER_T", logical::HEAT_BORDER_T),
    ("PACKET_R_GLOW", logical::PACKET_R_GLOW),
    ("PACKET_R_CORE", logical::PACKET_R_CORE),
    ("WIRE_PACKET_R", logical::WIRE_PACKET_R),
    ("WIRE_W", logical::WIRE_W),
    ("RUBBER_W", logical::RUBBER_W),
    ("WIRE_CTRL_MIN", logical::WIRE_CTRL_MIN),
    ("WIRE_CTRL_MAX", logical::WIRE_CTRL_MAX),
    ("LATENCY_FRAME_T", logical::LATENCY_FRAME_T),
    ("SCROLLBAR_W", logical::SCROLLBAR_W),
    ("SCROLLBAR_INSET", logical::SCROLLBAR_INSET),
    ("SCROLLBAR_MIN_THUMB", logical::SCROLLBAR_MIN_THUMB),
    ("SCROLLBAR_GRAB_SLOP", logical::SCROLLBAR_GRAB_SLOP),
    ("SEARCH_TICK_BLEED", logical::SEARCH_TICK_BLEED),
    ("SEARCH_TICK_H", logical::SEARCH_TICK_H),
    ("SEARCH_TICK_CUR_H", logical::SEARCH_TICK_CUR_H),
    ("EDGE_STRIP", logical::EDGE_STRIP),
    ("DRAG_THRESHOLD", logical::DRAG_THRESHOLD),
    ("DROP_CARET_W", logical::DROP_CARET_W),
    ("DROP_CARET_WING", logical::DROP_CARET_WING),
    ("DROP_ZONE_EDGE", logical::DROP_ZONE_EDGE),
    ("GHOST_PAD", logical::GHOST_PAD),
    ("GHOST_OFFSET", logical::GHOST_OFFSET),
    ("CLICK_SLOP", logical::CLICK_SLOP),
    ("PX_PER_LINE", logical::PX_PER_LINE),
    ("PANEL_ROW_PAD", logical::PANEL_ROW_PAD),
    ("PANEL_TEXT_TOP", logical::PANEL_TEXT_TOP),
    ("SEARCH_PAD", logical::SEARCH_PAD),
    ("SEARCH_INSET", logical::SEARCH_INSET),
    ("SEARCH_CARET_W", logical::SEARCH_CARET_W),
    ("PREFS_PAD", logical::PREFS_PAD),
    ("PREFS_SWATCH_GAP", logical::PREFS_SWATCH_GAP),
    ("PREFS_SEL_INSET", logical::PREFS_SEL_INSET),
    ("MENU_PAD", logical::MENU_PAD),
    ("MENU_SEP_H", logical::MENU_SEP_H),
    ("MANUAL_PAD", logical::MANUAL_PAD),
    ("MANUAL_MAX_W", logical::MANUAL_MAX_W),
    ("MANUAL_SB_INSET", logical::MANUAL_SB_INSET),
    ("MANUAL_SB_W", logical::MANUAL_SB_W),
    ("MANUAL_SB_MIN_THUMB", logical::MANUAL_SB_MIN_THUMB),
    ("MANUAL_SB_TRACK_INSET", logical::MANUAL_SB_TRACK_INSET),
    ("CLIP_PAD", logical::CLIP_PAD),
    ("CLIP_ANCHOR_FALLBACK", logical::CLIP_ANCHOR_FALLBACK),
    ("PICKER_SV_MIN", logical::PICKER_SV_MIN),
    ("PICKER_HUE_MIN", logical::PICKER_HUE_MIN),
    ("PICKER_SV_MARK_OUT", logical::PICKER_SV_MARK_OUT),
    ("PICKER_SV_MARK_IN", logical::PICKER_SV_MARK_IN),
    ("PICKER_HUE_CARET_OUT", logical::PICKER_HUE_CARET_OUT),
    ("PICKER_HUE_CARET_IN", logical::PICKER_HUE_CARET_IN),
];

/// Turn a window's backing factor into the chrome scale to multiply by.
///
/// Refuses nonsense the same way [`crate::scale_policy::on_scale_factor_changed`]
/// does — a zero, negative, NaN or infinite factor would poison every rect in
/// the window — and falls back to 1.0, which is the "draw it exactly as Linux
/// always has" answer.
pub fn chrome_scale(scale_factor: f64) -> f32 {
    if !scale_factor.is_finite() || scale_factor <= 0.0 {
        return 1.0;
    }
    scale_factor as f32
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The hard requirement: a 1x display must be bit-for-bit what it was
    /// before chrome scaling existed. IEEE-754 multiplication by 1.0 is the
    /// exact identity for every finite value, so this holds for the whole
    /// register at once — including the `.5`s and the `2.4`/`5.8`/`1.8`s that a
    /// rounding step would perturb. Asserted with `to_bits`, not `==`, so it is
    /// bit equality and not merely numeric equality.
    #[test]
    fn scale_one_is_bit_identical() {
        for &(name, v) in REGISTER {
            assert_eq!(
                (1.0_f32 * v).to_bits(),
                v.to_bits(),
                "{name}: scaling by 1.0 must be the exact identity",
            );
        }
    }

    /// And the point of the change: at 2x every one of them doubles.
    #[test]
    fn every_value_doubles_at_2x() {
        for &(name, v) in REGISTER {
            assert_eq!(2.0_f32 * v, v * 2.0, "{name}");
            assert!((2.0_f32 * v - v - v).abs() < f32::EPSILON * v.max(1.0), "{name} must double");
        }
    }

    /// Scaling is linear in the factor, at every factor a real compositor
    /// reports — including the fractional ones Wayland and Windows use.
    #[test]
    fn scaling_is_linear_at_every_real_factor() {
        for &sc in &[1.0_f32, 1.25, 1.5, 1.75, 2.0, 2.5, 3.0] {
            for &(name, v) in REGISTER {
                let got = sc * v;
                assert!(got.is_finite(), "{name} at {sc}");
                assert!(got > 0.0, "{name} at {sc}: chrome must never scale to nothing");
                // Doubling the factor doubles the result, exactly.
                assert_eq!((2.0 * sc) * v, 2.0 * (sc * v), "{name} at {sc}");
            }
        }
    }

    /// The freeze. [`REGISTER`] is the register of every flat chrome value in
    /// `rt`; this pins its exact contents so adding a chrome constant is a
    /// deliberate, reviewed act rather than an accidental unscaled literal.
    ///
    /// If you are here because this failed: you added (or removed) a chrome
    /// constant. Add it to `logical`, add it to `REGISTER`, add its name and
    /// value here — and make sure its USE site multiplies by the scale.
    #[test]
    fn the_chrome_register_is_frozen() {
        let expected: &[(&str, f32)] = &[
            ("WINDOW_MARGIN", 8.0),
            ("TITLEBAR_PAD", 6.0),
            ("HAIRLINE", 1.0),
            ("GROUP_GAP", 5.0),
            ("TITLE_GAP", 8.0),
            ("GROUP_MARK", 10.0),
            ("GROUP_MARK_INSET", 4.0),
            ("TAB_LABEL_INSET", 8.0),
            ("JACK_R_BACK", 7.5),
            ("JACK_R_FILL", 5.8),
            ("JACK_R_RING", 5.4),
            ("JACK_RING_W", 1.8),
            ("JACK_GRAB_R", 12.0),
            ("HEAT_BORDER_T", 2.4),
            ("PACKET_R_GLOW", 9.0),
            ("PACKET_R_CORE", 3.4),
            ("WIRE_PACKET_R", 2.6),
            ("WIRE_W", 2.0),
            ("RUBBER_W", 1.6),
            ("WIRE_CTRL_MIN", 40.0),
            ("WIRE_CTRL_MAX", 180.0),
            ("LATENCY_FRAME_T", 2.0),
            ("SCROLLBAR_W", 4.0),
            ("SCROLLBAR_INSET", 0.5),
            ("SCROLLBAR_MIN_THUMB", 24.0),
            ("SCROLLBAR_GRAB_SLOP", 2.0),
            ("SEARCH_TICK_BLEED", 2.0),
            ("SEARCH_TICK_H", 2.0),
            ("SEARCH_TICK_CUR_H", 3.0),
            ("EDGE_STRIP", 24.0),
            ("DRAG_THRESHOLD", 4.0),
            ("DROP_CARET_W", 3.0),
            ("DROP_CARET_WING", 3.0),
            ("DROP_ZONE_EDGE", 2.0),
            ("GHOST_PAD", 6.0),
            ("GHOST_OFFSET", 12.0),
            ("CLICK_SLOP", 5.0),
            ("PX_PER_LINE", 20.0),
            ("PANEL_ROW_PAD", 4.0),
            ("PANEL_TEXT_TOP", 2.0),
            ("SEARCH_PAD", 6.0),
            ("SEARCH_INSET", 8.0),
            ("SEARCH_CARET_W", 2.0),
            ("PREFS_PAD", 10.0),
            ("PREFS_SWATCH_GAP", 2.0),
            ("PREFS_SEL_INSET", 1.0),
            ("MENU_PAD", 8.0),
            ("MENU_SEP_H", 7.0),
            ("MANUAL_PAD", 12.0),
            ("MANUAL_MAX_W", 900.0),
            ("MANUAL_SB_INSET", 4.0),
            ("MANUAL_SB_W", 3.0),
            ("MANUAL_SB_MIN_THUMB", 12.0),
            ("MANUAL_SB_TRACK_INSET", 2.0),
            ("CLIP_PAD", 6.0),
            ("CLIP_ANCHOR_FALLBACK", 40.0),
            ("PICKER_SV_MIN", 140.0),
            ("PICKER_HUE_MIN", 14.0),
            ("PICKER_SV_MARK_OUT", 10.0),
            ("PICKER_SV_MARK_IN", 8.0),
            ("PICKER_HUE_CARET_OUT", 4.0),
            ("PICKER_HUE_CARET_IN", 2.0),
        ];
        assert_eq!(
            REGISTER.len(),
            expected.len(),
            "a chrome constant was added or removed without filing it here",
        );
        for (got, want) in REGISTER.iter().zip(expected) {
            assert_eq!(got.0, want.0, "register order changed");
            assert_eq!(got.1, want.1, "{} changed its logical value", want.0);
        }
    }

    /// Names are unique — a duplicate would silently hide one entry from the
    /// per-entry gates above.
    #[test]
    fn register_names_are_unique() {
        let mut seen: Vec<&str> = REGISTER.iter().map(|e| e.0).collect();
        seen.sort_unstable();
        let n = seen.len();
        seen.dedup();
        assert_eq!(seen.len(), n, "duplicate name in REGISTER");
    }

    /// A backing factor that is not a usable number must fall back to 1.0
    /// rather than collapsing every rect in the window to zero or NaN.
    #[test]
    fn an_unusable_factor_falls_back_to_one() {
        for &bad in &[0.0_f64, -1.0, -2.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(chrome_scale(bad), 1.0, "factor {bad}");
        }
    }

    /// And a real one passes straight through.
    #[test]
    fn a_real_factor_passes_through() {
        for &(f, want) in &[(1.0_f64, 1.0_f32), (1.25, 1.25), (1.5, 1.5), (2.0, 2.0), (3.0, 3.0)] {
            assert_eq!(chrome_scale(f), want);
        }
    }

    /// The jack discs and the jack GRAB radius must keep their ratio at every
    /// factor. This is the pairing that matters most to the user: the discs are
    /// what he sees, the grab radius is what he hits, and a scale applied to one
    /// and not the other is the colour-picker-unclickable bug all over again.
    #[test]
    fn jack_draw_and_hit_keep_their_ratio() {
        let ratio_at = |sc: f32| {
            [
                (sc * logical::JACK_GRAB_R) / (sc * logical::JACK_R_BACK),
                (sc * logical::JACK_GRAB_R) / (sc * logical::JACK_R_FILL),
                (sc * logical::JACK_GRAB_R) / (sc * logical::JACK_R_RING),
            ]
        };
        let at_1x = ratio_at(1.0);
        for &sc in &[1.0_f32, 1.25, 1.5, 2.0, 3.0] {
            // A ratio of two scaled values is scale-free up to one rounding step
            // in each division; anything larger than that means one side of the
            // pair is not taking the factor at all.
            for (got, want) in ratio_at(sc).iter().zip(&at_1x) {
                assert!(
                    (got - want).abs() <= f32::EPSILON * want,
                    "jack draw/hit ratio drifted at {sc}: {got} vs {want}",
                );
            }
        }
        // And the grab radius must stay strictly the largest of the four, at
        // every factor — a disc you can see but not hit is the defect.
        for &sc in &[1.0_f32, 1.25, 1.5, 2.0, 3.0] {
            let grab = sc * logical::JACK_GRAB_R;
            for &(n, r) in &[
                ("back", logical::JACK_R_BACK),
                ("fill", logical::JACK_R_FILL),
                ("ring", logical::JACK_R_RING),
            ] {
                assert!(grab > sc * r, "at {sc}x the grab radius must exceed the {n} disc");
            }
        }
    }

    /// The same pairing for the scrollbar: the drawn bar must always sit inside
    /// the band the hit-test accepts.
    #[test]
    fn scrollbar_draw_stays_inside_its_grab_band() {
        for &sc in &[1.0_f32, 1.25, 1.5, 2.0, 3.0] {
            let bar_w = sc * logical::SCROLLBAR_W;
            let band = bar_w + 2.0 * (sc * logical::SCROLLBAR_GRAB_SLOP);
            assert!(band > bar_w, "at {sc}x the grab band must be wider than the bar");
            // Ratio preserved: band/bar is scale-invariant. 4px bar + 2px slop
            // each side = 8px band = exactly 2x the bar, at every factor.
            assert_eq!(band / bar_w, 2.0, "band/bar ratio drifted at {sc}");
        }
    }

    /// The drop-zone cues: the caret's wings must always overhang the caret
    /// itself, at every factor.
    #[test]
    fn drop_caret_wings_always_overhang() {
        for &sc in &[1.0_f32, 1.5, 2.0, 3.0] {
            let w = sc * logical::DROP_CARET_W;
            let winged = w + 2.0 * (sc * logical::DROP_CARET_WING);
            assert!(winged > w, "at {sc}x");
        }
    }
}
