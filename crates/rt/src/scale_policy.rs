//! What a HiDPI scale-factor change must do, as plain data.
//!
//! The decision itself is three booleans and a float comparison; the work it
//! authorises (reload the fonts at the new physical pixel size, re-measure the
//! cell, resize the surface, reflow every pane) all lives in `main.rs` against
//! a live window and a live backend, and none of it can be constructed in a
//! test. So the decision is lifted out here — no winit types, no backend, no
//! `cfg` — for the same reason [`crate::wgpu_frame`], [`crate::vibrancy_policy`]
//! and [`crate::menubar_model`] are: it is the only automated coverage a
//! Retina-only code path can have, and Linux CI runs it on every commit.
//!
//! # The bug this exists for
//!
//! `WindowEvent::ScaleFactorChanged` used to be logged and dropped. Dragging a
//! window from a Retina panel to a 1x monitor left every glyph rasterised at
//! the OLD scale — text at half or double size — until the next font reload
//! (a zoom step, or a Preferences commit) happened to re-read
//! `Window::scale_factor()` and self-heal. On Linux the same event fires for a
//! HiDPI Wayland/X11 output, so this was never macOS-only.
//!
//! # Why it routes through the resize settle
//!
//! Re-measuring the cell on the spot is exactly what the old comment warned
//! against. A scale change arrives WITH a new surface size — winit's AppKit
//! backend queues `ScaleFactorChanged` and then immediately queues
//! `SurfaceResized(physical_size)` for the same window
//! (`winit-appkit`'s `handle_scale_factor_changed`), and the X11 and Wayland
//! backends both re-request a surface size off the back of it too. That second
//! event lands in `WindowEvent::SurfaceResized`, which defers EVERYTHING to
//! `RESIZE_SETTLE` (see its handler for the measurements: a reflow is ~676ms
//! median on a weak box, and a drag emits ~20 of them). Doing the font reload
//! in the scale handler would therefore reflow once at the new scale and then
//! reflow AGAIN at the settle, mid-drag, at a size that is already stale.
//!
//! So a scale change arms the SAME settle instead: it records that a font
//! reload is owed, suspends painting (`Active::surface_pending`), and lets the
//! one existing quiet-moment code path pay for surface + fonts + reflow once,
//! reading `Window::scale_factor()` fresh at that point rather than trusting
//! the value the event carried.
//!
//! During a slow drag between two displays this means: each crossing arms the
//! settle and re-arms it on every following resize event, the window holds its
//! last painted frame (exactly as it does during a resize drag), and when the
//! pointer stops for `RESIZE_SETTLE` the window repaints once at the resting
//! size and the resting scale.

/// Scale factors closer together than this are the same factor.
///
/// Guards against a backend that recomputes and re-reports a factor it derived
/// from a float division (X11 reports a DPI-derived factor, Wayland a
/// fractional one), which would otherwise arm a full font reload + reflow for a
/// change of nothing. Far tighter than any real difference: the smallest step
/// any platform reports is 0.25.
pub const SCALE_EPSILON: f64 = 1e-6;

/// What `WindowEvent::ScaleFactorChanged` must do to [`crate::Active`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ScaleArm {
    /// Set `resize_pending` and stamp `last_resize_at`, so the settle in
    /// `tick` fires `RESIZE_SETTLE` after the last event of the run.
    pub arm_settle: bool,
    /// Set `scale_pending`: the settle owes a font reload at the new scale,
    /// not just the plain reflow a size change owes.
    pub reload_owed: bool,
    /// Set `surface_pending`, which suspends painting until the settle. A frame
    /// drawn between here and the settle would use the OLD cell metrics against
    /// the NEW surface size — the visibly-wrong half/double-size text this whole
    /// module exists to remove — so it is skipped rather than shown.
    pub suspend_painting: bool,
}

/// The event changed nothing: log it and carry on.
pub const IGNORE: ScaleArm =
    ScaleArm { arm_settle: false, reload_owed: false, suspend_painting: false };

/// Decide what a `ScaleFactorChanged` carrying `reported` must do, given that
/// the cell metrics currently on screen were measured at `loaded_at`.
///
/// `loaded_at` is `Active::font_scale` — the factor the live font raster was
/// built with, updated by every successful `refresh_fonts` — and NOT
/// `Window::scale_factor()`, which by this point already reads the new value on
/// every backend and so could never differ from `reported`.
///
/// A `reported` that is not a usable factor (zero, negative, NaN, infinite —
/// none of which a sane compositor sends, but all of which would poison
/// `physical_font_px` into a zero-sized or non-finite raster) is refused
/// outright: keeping the last known-good metrics is strictly better than
/// reloading fonts at 0px.
pub fn on_scale_factor_changed(loaded_at: f64, reported: f64) -> ScaleArm {
    if !reported.is_finite() || reported <= 0.0 {
        return IGNORE;
    }
    if (reported - loaded_at).abs() <= SCALE_EPSILON {
        return IGNORE; // same factor re-reported: nothing to re-measure
    }
    ScaleArm { arm_settle: true, reload_owed: true, suspend_painting: true }
}

/// The work the deferred-resize settle owes, once the size (and scale) have
/// held still for `RESIZE_SETTLE`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SettleWork {
    /// Recreate the back buffer / instrument layer and move the viewport to the
    /// current surface size. Owed whenever painting was suspended.
    pub resize_surface: bool,
    /// Re-rasterise the fonts at `physical_font_px(font_size, scale_factor)`
    /// read fresh, re-measure the cell, and push it at the session. This is
    /// `refresh_fonts`, which relayouts as its last step.
    pub reload_fonts: bool,
    /// Reflow the panes to the current bounds *directly*. Owed only when the
    /// fonts are NOT being reloaded, because `refresh_fonts` already ends in a
    /// relayout against the same bounds — doing both would pay the single most
    /// expensive operation in the settle twice for one settled state.
    pub relayout: bool,
}

/// Decide the settle's work. `surface_owed` is `Active::surface_pending.is_some()`
/// (painting was suspended, so the backend surface is still the old one);
/// `scale_owed` is `Active::scale_pending`.
///
/// The invariant worth keeping in mind: exactly one reflow comes out of every
/// settle, whichever route pays for it. See [`SettleWork::relayout`].
pub fn settle_work(surface_owed: bool, scale_owed: bool) -> SettleWork {
    SettleWork {
        resize_surface: surface_owed,
        reload_fonts: scale_owed,
        relayout: !scale_owed,
    }
}

impl SettleWork {
    /// How many times this settle reflows the session — via `relayout`
    /// directly, or inside `refresh_fonts`. Must always be exactly 1.
    ///
    /// Test-only: it states an invariant about the two fields rather than
    /// doing work, and `tick_active` reads the fields themselves.
    #[cfg(test)]
    pub fn reflow_count(&self) -> usize {
        usize::from(self.relayout) + usize::from(self.reload_fonts)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The defect, stated as a test: a window dragged from a 1x monitor onto a
    /// Retina panel MUST arm a font reload. The old handler logged and returned,
    /// which is `IGNORE` — so this is the assertion that fails before the fix.
    #[test]
    fn moving_to_a_retina_display_arms_a_reload() {
        let arm = on_scale_factor_changed(1.0, 2.0);
        assert!(arm.reload_owed, "1x -> 2x must re-measure the cell at the new scale");
        assert!(arm.arm_settle, "the re-measure must be paid by the resize settle, not inline");
        assert!(arm.suspend_painting, "no frame may be drawn with stale cell metrics");
    }

    /// And the way back: Retina -> 1x is the same defect with the sign flipped
    /// (text twice the size it should be, rather than half).
    #[test]
    fn moving_to_a_1x_display_arms_a_reload() {
        assert_eq!(
            on_scale_factor_changed(2.0, 1.0),
            ScaleArm { arm_settle: true, reload_owed: true, suspend_painting: true },
        );
    }

    /// Fractional factors are the common case on Wayland and on Windows, and
    /// every one of them must arm — nothing here may special-case 2.0.
    #[test]
    fn every_real_scale_step_arms() {
        for &(from, to) in &[
            (1.0_f64, 1.25_f64),
            (1.25, 1.5),
            (1.5, 1.0),
            (2.0, 3.0),
            (3.0, 1.75),
            (1.0, 1.0 + SCALE_EPSILON * 10.0), // just above the epsilon still counts
        ] {
            assert!(
                on_scale_factor_changed(from, to).reload_owed,
                "{from} -> {to} must arm a reload",
            );
        }
    }

    /// A factor re-reported unchanged must cost nothing. macOS emits
    /// `ScaleFactorChanged` around monitor moves and window drags, and both X11
    /// and Wayland recompute the factor from a float division, so an
    /// arm-on-every-event implementation would reflow the whole session for no
    /// reason — the exact cost the settle machinery exists to avoid.
    #[test]
    fn the_same_factor_re_reported_does_nothing() {
        for &f in &[1.0_f64, 1.25, 2.0, 3.0] {
            assert_eq!(on_scale_factor_changed(f, f), IGNORE, "{f} -> {f}");
        }
        // And a difference below the epsilon is the same factor.
        assert_eq!(on_scale_factor_changed(2.0, 2.0 + SCALE_EPSILON / 2.0), IGNORE);
    }

    /// A nonsense factor must never reach `physical_font_px`: a 0px raster has
    /// no cell size, and the session would divide the window by it.
    #[test]
    fn an_unusable_factor_is_refused() {
        for &bad in &[0.0_f64, -1.0, -2.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert_eq!(on_scale_factor_changed(1.0, bad), IGNORE, "scale {bad}");
        }
    }

    /// Linux at 1.0 must be bit-identical: the factor never changes, so no
    /// scale event can ever arm anything, and the settle keeps doing exactly
    /// what it did before this module existed.
    #[test]
    fn linux_at_scale_1_is_untouched() {
        assert_eq!(on_scale_factor_changed(1.0, 1.0), IGNORE);
        assert_eq!(
            settle_work(true, false),
            SettleWork { resize_surface: true, reload_fonts: false, relayout: true },
            "a plain resize settle must still be surface + one relayout",
        );
    }

    /// The invariant that keeps the settle affordable: one reflow per settle,
    /// no matter which combination of work is owed. A reflow is the 676ms
    /// operation the whole deferred-resize design is built around.
    #[test]
    fn every_settle_reflows_exactly_once() {
        for &surface in &[false, true] {
            for &scale in &[false, true] {
                let w = settle_work(surface, scale);
                assert_eq!(w.reflow_count(), 1, "surface={surface} scale={scale}");
            }
        }
    }

    /// A scale settle reloads the fonts and does NOT also relayout directly —
    /// `refresh_fonts` ends in a relayout of its own, so the direct call would
    /// be the second one.
    #[test]
    fn a_scale_settle_reflows_through_the_font_reload() {
        let w = settle_work(true, true);
        assert!(w.reload_fonts);
        assert!(!w.relayout, "refresh_fonts already relayouts; a second one is pure waste");
        assert!(w.resize_surface, "the surface is still the pre-scale one");
    }

    /// A slow drag across the boundary between two displays. macOS re-reports
    /// the factor repeatedly while the window straddles the edge, and
    /// `font_scale` does not move until the settle actually reloads — so every
    /// one of those events must keep arming, which is what re-stamps
    /// `last_resize_at` and pushes the settle out until the drag stops. The
    /// window holds its last painted frame throughout (`suspend_painting`), and
    /// exactly one reload is paid, at the resting scale.
    #[test]
    fn a_slow_drag_between_displays_keeps_arming_until_it_settles() {
        let mut font_scale = 1.0_f64; // fonts on screen were built at 1x
        // The window crosses onto the Retina panel and wobbles back and forth
        // over the boundary. `font_scale` does not move for any of this — only
        // a settle reload updates it.
        for reported in [2.0_f64, 2.0, 2.0] {
            let arm = on_scale_factor_changed(font_scale, reported);
            assert!(arm.arm_settle, "event {reported} must push the settle out");
            assert!(arm.suspend_painting, "the window must not paint mid-crossing");
            assert_eq!(font_scale, 1.0);
        }
        // A wobble back onto the 1x display mid-drag arms NOTHING, because the
        // fonts on screen are still the 1x ones and so already correct for it.
        // The settle armed by the events above is untouched and still stands —
        // and because it reads the factor fresh when it fires, a drag that ends
        // back where it started costs one reload that changes nothing, rather
        // than leaving the window mismatched.
        assert_eq!(on_scale_factor_changed(font_scale, 1.0), IGNORE);
        for reported in [2.0_f64, 2.0] {
            assert!(on_scale_factor_changed(font_scale, reported).arm_settle);
        }
        // The drag stops on the Retina panel. The settle reloads at the factor
        // read FRESH from the window then, not at whatever the last event said.
        let w = settle_work(true, true);
        assert_eq!(w.reflow_count(), 1, "one reflow for the whole drag");
        assert!(w.reload_fonts);
        font_scale = 2.0; // what refresh_fonts records on success

        // And now the window is at rest and in agreement: a redundant
        // re-report of the same factor costs nothing.
        assert_eq!(on_scale_factor_changed(font_scale, 2.0), IGNORE);
    }

    /// `surface_owed` and `scale_owed` are independent: a scale change that
    /// somehow settled with no surface work still reloads the fonts.
    #[test]
    fn the_two_kinds_of_owed_work_are_independent() {
        assert_eq!(
            settle_work(false, true),
            SettleWork { resize_surface: false, reload_fonts: true, relayout: false },
        );
        assert_eq!(
            settle_work(false, false),
            SettleWork { resize_surface: false, reload_fonts: false, relayout: true },
        );
    }
}
