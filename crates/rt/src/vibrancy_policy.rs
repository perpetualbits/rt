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
//! [`glass_action`] what to do and does it.
// Off macOS the only caller is this file's own test module, which `cargo build` does not
// compile -- that is the whole point of the module being platform-independent, so the
// resulting dead_code warning is noise, not a finding.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use rt_config::GlassMaterial;

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
    match (want, installed) {
        (true, false) => GlassAction::Install(material),
        (true, true) => GlassAction::Retarget(material),
        (false, true) => GlassAction::Remove,
        (false, false) => GlassAction::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const M: GlassMaterial = GlassMaterial::UnderWindowBackground;

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
        for m in GlassMaterial::ALL {
            assert_eq!(glass_action(false, true, *m), GlassAction::Install(*m));
            assert_eq!(glass_action(true, true, *m), GlassAction::Retarget(*m));
        }
    }

    /// End-to-end over the settings that produce the decision, so the gate and
    /// the material are pinned together against the real `Settings` type.
    #[test]
    fn the_settings_gate_drives_the_action() {
        let mut s = rt_config::Settings::default();
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
