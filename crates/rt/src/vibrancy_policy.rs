//! What the macOS frosted glass must DO on a given settings state, as plain data.
//!
//! `vibrancy.rs` is `cfg(target_os = "macos")` and every line of it is an AppKit
//! call, so no Linux build can compile it and no Linux CI can test it. The
//! decision it makes, though, is not AppKit at all — it is three booleans and an
//! enum — and getting that decision wrong is exactly the bug the user hit: the
//! effect view was installed unconditionally, so `background_blur = false` did
//! nothing and the glass could not be turned off.
//!
//! So this module is deliberately NOT `cfg`'d, for the same reason
//! [`crate::wgpu_frame`] is not: it holds no objc2 types, it compiles and its
//! tests run everywhere, and it is the only automated coverage the decision can
//! have. `vibrancy.rs` keeps the AppKit calls and nothing else: it asks
//! [`glass_plan`] what to do and does it.
//!
//! ## Two mechanisms
//!
//! There are two ways to blur what is behind a macOS window, and rt can use
//! either — but never both at once, which is what makes this a state machine and
//! not a pair of booleans:
//!
//! * an **`NSVisualEffectView`** with a named `NSVisualEffectMaterial`: public
//!   API, light/dark adaptive, but every material carries its own tint and none
//!   of them lets you set the blur radius;
//! * the window's own **backdrop blur radius**, untinted and variable — what
//!   Terminal.app's Profiles → Window "Blur" slider drives, and what
//!   [`rt_config::GlassMaterial::WindowBlur`] selects. rt's default.
//!
//! [`glass_plan`] decides both together so a switch takes the old one down in
//! the same pass it brings the new one up.
// Off macOS the only caller is this file's own test module, which `cargo build` does not
// compile -- that is the whole point of the module being platform-independent, so the
// resulting dead_code warning is noise, not a finding.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use rt_config::GlassMaterial;

/// Everything `vibrancy::set_enabled` must do for one settings state.
///
/// TWO mechanisms, decided together and applied together, because they are
/// mutually exclusive and switching between them at runtime has to take the old
/// one down in the same pass that brings the new one up. Leaving that to two
/// independent decisions is how a user ends up with an `NSVisualEffectView`'s
/// tint sitting on top of a plain window blur, or with the blur radius still set
/// after switching to a material.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GlassPlan {
    /// What to do with the `NSVisualEffectView` (which the plain-blur mode does
    /// not use at all).
    pub view: GlassAction,
    /// The radius to hand `CGSSetWindowBackgroundBlurRadius`. `0` means "no
    /// window blur", which is both the off state and what clears a radius set by
    /// a previous call — so this is always applied, never skipped.
    pub blur_radius: u32,
}

/// What `vibrancy::set_enabled` must do to the window's view hierarchy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GlassAction {
    /// No effect view is installed and one is wanted: build an
    /// `NSVisualEffectView`, give it this material, and slide it in below the
    /// content view.
    Install(GlassMaterial),
    /// One is already installed and still wanted: leave it exactly where it is
    /// and only push the material at it.
    ///
    /// This is the case that makes repeated calls safe. `set_enabled` runs on
    /// every opacity step and every settings commit, and an `Install` each time
    /// would stack effect views — each one retained by the frame view, each one
    /// blurring the one below, which is precisely the "there may be two layers
    /// somehow" the user described.
    Retarget(GlassMaterial),
    /// One is installed and is no longer wanted: take it out of the hierarchy.
    ///
    /// Removal, not `isHidden = true`. Two reasons, both about ownership:
    /// `addSubview:` is what retains the effect view (we keep no handle of our
    /// own), so `removeFromSuperview` drops the last strong reference and the
    /// view — and the window-server backdrop it asked for — actually goes away,
    /// where a hidden view stays allocated and keeps participating in layout and
    /// autoresizing forever. And it keeps "is it installed?" answerable from the
    /// view hierarchy alone, so there is no second flag in `Active` to drift out
    /// of sync with what the window is really showing.
    Remove,
    /// Nothing is installed and nothing is wanted. The overwhelmingly common
    /// case: rt is opaque, or the user turned blur off and then changed some
    /// unrelated preference.
    Nothing,
}

/// Decide what the frosted glass must do. See [`GlassAction`].
///
/// `installed` is whether rt's effect view is currently in the window's view
/// hierarchy (answered by looking, not by remembering), `want` is
/// `Settings::wants_background_blur`, and `material` is the user's chosen
/// [`GlassMaterial`].
pub fn glass_action(installed: bool, want: bool, material: GlassMaterial) -> GlassAction {
    // The plain-blur mode installs no view at all, so from this function's point
    // of view it is indistinguishable from "no glass wanted": whatever effect
    // view is there has to come out. That is what makes switching Preferences
    // from `hud-window` to `window-blur` actually drop the material's tint,
    // rather than blurring twice.
    let want_view = want && material.is_effect_view();
    match (want_view, installed) {
        (true, false) => GlassAction::Install(material),
        (true, true) => GlassAction::Retarget(material),
        (false, true) => GlassAction::Remove,
        (false, false) => GlassAction::Nothing,
    }
}

/// The whole decision: the effect view AND the window's backdrop blur radius.
///
/// `radius` is `settings.macos_blur_radius`; it is only reachable through
/// [`GlassMaterial::WindowBlur`], because `NSVisualEffectView` has no radius to
/// set — AppKit picks one per material. So the returned `blur_radius` is
/// `radius` for exactly one combination (blur wanted, and the plain mode chosen)
/// and `0` for every other, including "a material is chosen": switching from
/// `window-blur` to a material has to CLEAR the radius, or the material's own
/// blur stacks on the window server's.
pub fn glass_plan(installed: bool, want: bool, material: GlassMaterial, radius: u32) -> GlassPlan {
    GlassPlan {
        view: glass_action(installed, want, material),
        blur_radius: if want && !material.is_effect_view() { radius } else { 0 },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: GlassMaterial = GlassMaterial::UnderWindowBackground;
    /// Every variant that installs an effect view — i.e. all but `WindowBlur`.
    fn materials() -> impl Iterator<Item = GlassMaterial> {
        GlassMaterial::ALL.iter().copied().filter(|m| m.is_effect_view())
    }

    #[test]
    fn glass_is_installed_only_when_wanted() {
        assert_eq!(glass_action(false, true, M), GlassAction::Install(M));
        // The defect: `background_blur = false` (or an opaque background) must
        // NOT put an effect view on the window.
        assert_eq!(glass_action(false, false, M), GlassAction::Nothing);
    }

    #[test]
    fn turning_blur_off_removes_the_glass_rather_than_leaving_it() {
        // The other half of the defect: the glass could not be turned off once
        // installed. Toggling the preference (or sliding opacity back to 1.0)
        // has to take the view out again.
        assert_eq!(glass_action(true, false, M), GlassAction::Remove);
    }

    #[test]
    fn a_repeated_call_retargets_instead_of_stacking_a_second_view() {
        // `set_enabled` is called on every opacity step and every settings
        // commit. Installing again each time would stack effect views.
        assert_eq!(glass_action(true, true, M), GlassAction::Retarget(M));
    }

    #[test]
    fn the_chosen_material_is_carried_to_whichever_action_applies_it() {
        for m in materials() {
            assert_eq!(glass_action(false, true, m), GlassAction::Install(m));
            assert_eq!(glass_action(true, true, m), GlassAction::Retarget(m));
        }
    }

    /// The default mode installs NO effect view. It is the absence of a material,
    /// not a material — an `NSVisualEffectView` would put its own tint under the
    /// user's background colour, which is the complaint this mode answers.
    #[test]
    fn the_plain_window_blur_mode_never_installs_an_effect_view() {
        const W: GlassMaterial = GlassMaterial::WindowBlur;
        assert_eq!(glass_action(false, true, W), GlassAction::Nothing, "nothing to install");
        assert_eq!(glass_action(false, false, W), GlassAction::Nothing);
        // And it is the default, so this is what a fresh config does.
        assert_eq!(GlassMaterial::default(), W);
    }

    /// The switch the user makes in Preferences: `hud-window` → `window-blur`.
    /// The material's view must come OUT in the same pass the plain blur goes on,
    /// or its tint stays on screen over the new blur.
    #[test]
    fn switching_from_a_material_to_plain_blur_removes_the_installed_view() {
        for m in materials() {
            // Standing on a material, with its view installed.
            let plan = glass_plan(true, true, m, 24);
            assert_eq!(plan.view, GlassAction::Retarget(m));
            assert_eq!(plan.blur_radius, 0, "{}: a material has no radius of its own", m.name());
            // One step to the plain mode.
            let plan = glass_plan(true, true, GlassMaterial::WindowBlur, 24);
            assert_eq!(plan.view, GlassAction::Remove, "the material's view must come out");
            assert_eq!(plan.blur_radius, 24, "and the window blur must come on");
        }
    }

    /// ...and back the other way: choosing a material has to clear the radius the
    /// plain mode asked the window server for, or the two blurs stack.
    #[test]
    fn switching_from_plain_blur_to_a_material_clears_the_radius() {
        let plan = glass_plan(false, true, GlassMaterial::WindowBlur, 40);
        assert_eq!(plan.view, GlassAction::Nothing);
        assert_eq!(plan.blur_radius, 40);
        let plan = glass_plan(false, true, GlassMaterial::HudWindow, 40);
        assert_eq!(plan.view, GlassAction::Install(GlassMaterial::HudWindow));
        assert_eq!(plan.blur_radius, 0);
    }

    /// Turning blur off retires BOTH mechanisms, whichever one was live.
    #[test]
    fn no_blur_wanted_means_no_view_and_no_radius() {
        for m in GlassMaterial::ALL {
            let plan = glass_plan(true, false, *m, 40);
            assert_eq!(plan.view, GlassAction::Remove, "{}", m.name());
            assert_eq!(plan.blur_radius, 0, "{}", m.name());
            let plan = glass_plan(false, false, *m, 40);
            assert_eq!(plan.view, GlassAction::Nothing, "{}", m.name());
            assert_eq!(plan.blur_radius, 0, "{}", m.name());
        }
    }

    /// The radius the user set is the radius that is asked for — not winit's
    /// hardcoded 80, which is the "too blurry" this whole mode exists to fix.
    #[test]
    fn the_configured_radius_reaches_the_window_server_unchanged() {
        for r in [rt_config::Settings::MIN_BLUR_RADIUS, 8, 24, rt_config::Settings::MAX_BLUR_RADIUS]
        {
            assert_eq!(glass_plan(false, true, GlassMaterial::WindowBlur, r).blur_radius, r);
        }
    }

    /// End-to-end from `Settings`, through the same fields `apply_blur` reads.
    #[test]
    fn the_default_settings_ask_for_an_untinted_blur_and_no_effect_view() {
        let mut s = rt_config::Settings::default();
        s.background_opacity = 0.05; // the user's own setting
        assert!(s.background_blur, "blur is on by default");
        let plan =
            glass_plan(false, s.wants_background_blur(), s.macos_glass_material, s.macos_blur_radius);
        assert_eq!(plan.view, GlassAction::Nothing, "no NSVisualEffectView, so no tint");
        assert_eq!(plan.blur_radius, rt_config::Settings::DEFAULT_BLUR_RADIUS);
    }

    /// End-to-end over the settings that produce the decision, so the gate and
    /// the material are pinned together against the real `Settings` type.
    #[test]
    fn the_settings_gate_drives_the_action() {
        let mut s = rt_config::Settings::default();
        // A material, not the default plain blur: this test is about the GATE,
        // and only a material has a view for the gate to install or remove.
        s.macos_glass_material = GlassMaterial::UnderWindowBackground;
        s.background_blur = true;
        s.background_opacity = 0.4;
        assert_eq!(
            glass_action(false, s.wants_background_blur(), s.macos_glass_material),
            GlassAction::Install(GlassMaterial::UnderWindowBackground),
        );
        // The preference the user could not make stick.
        s.background_blur = false;
        assert_eq!(
            glass_action(true, s.wants_background_blur(), s.macos_glass_material),
            GlassAction::Remove,
        );
        // And an opaque background retires the glass just the same.
        s.background_blur = true;
        s.background_opacity = 1.0;
        assert_eq!(
            glass_action(true, s.wants_background_blur(), s.macos_glass_material),
            GlassAction::Remove,
        );
    }
}
