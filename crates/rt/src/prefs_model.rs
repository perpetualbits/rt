//! Which setting each preferences row edits, and what one step does to it.
//!
//! Pure: no UI, no backend, no X server. Split out of `chrome::prefs` so the
//! clamping rules can be tested exhaustively without constructing a `Geom`.

use rt_config::Settings;

/// Which setting a preferences row edits. `Close` is the dismiss action and
/// edits nothing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PrefRow {
    FontSize,
    FontFamily,
    Opacity,
    Blur,
    /// The macOS `NSVisualEffectMaterial` the frosted glass is made of. The row
    /// is only BUILT on macOS (see `chrome::prefs::rows`), but the rule lives
    /// here unguarded so Linux CI still tests it — the same reason
    /// `vibrancy_policy` is not `cfg`'d.
    // Off macOS nothing constructs it outside this file's tests, which is the
    // point of keeping the rule cross-platform; the resulting dead-code warning
    // is noise, not a finding.
    #[cfg_attr(not(target_os = "macos"), allow(dead_code))]
    GlassMaterial,
    Preset,
    Ffm,
    Titlebar,
    Scrollback,
    InstOutput,
    InstHeat,
    InstLatency,
    Jacks,
    InstRemote,
    InstAnimate,
    ArrowAccel,
    ArrowAccelMax,
    Term,
    Close,
}

/// Font size bounds. The upper bound is a guardrail: the renderer must
/// rasterise every glyph at this size, and an unbounded value can pick one no
/// machine will draw.
const FONT_MIN: f32 = 8.0;
const FONT_MAX: f32 = 48.0;
/// Scrollback floor. The ceiling is `Settings::MAX_SCROLLBACK`.
const SCROLLBACK_MIN: usize = 1000;
/// One opacity step. Matches the granularity the egui slider offered.
const OPACITY_STEP: f32 = 0.05;

/// Is this row live? A disabled row draws dimmed and refuses steps.
///
/// Rows are disabled only where a setting is genuinely inert given another one,
/// and each case is worth encoding. The 6fps instrument tick is gated on BOTH
/// flags (`instruments_animating = anim && inst_remote && inst_animate`), so
/// `inst_animate` alone does nothing while `inst_remote` is off — setting
/// `inst_remote = true` and seeing no animation, because `inst_animate` defaulted
/// false, is exactly the trap that avoids. The glass material is the same shape
/// of trap: it names the LOOK of a frosted glass that `background_blur` (and a
/// translucent background) decide the existence of, so with no glass on screen
/// stepping it would change nothing visible.
pub fn enabled(s: &Settings, row: PrefRow) -> bool {
    match row {
        PrefRow::InstAnimate => s.inst_remote,
        PrefRow::ArrowAccelMax => s.arrow_accel, // the cap is moot when acceleration is off
        // The one predicate every blur/glass backend shares; never a second copy.
        PrefRow::GlassMaterial => s.wants_background_blur(),
        _ => true,
    }
}

/// The name of the scheme whose colours `s` currently carries, or `"custom"`.
///
/// Colours are edited in `config.toml`, so "custom" is the normal state for
/// anyone who has done that — it is a readout, not a warning.
pub fn preset_name(s: &Settings) -> &'static str {
    rt_config::SCHEMES
        .iter()
        .find(|c| c.foreground == s.foreground && c.background == s.background && c.palette == s.palette)
        .map(|c| c.name)
        .unwrap_or("custom")
}

/// Apply ONE step of `dir` (+1 = Right, -1 = Left) to `row`'s setting.
///
/// Every rule clamps or wraps; nothing here can leave `Settings` invalid. A
/// toggle flips on either direction — Left/Right on a checkbox has no natural
/// "increase", and users press both.
///
/// `families` is the installed monospace font list and `terms` the terminal types this
/// machine actually has terminfo for (`rt_config::term_candidates`). Both are passed in
/// rather than looked up here so this module stays pure and its tests stay independent of
/// what happens to be installed on the machine running them.
pub fn step(s: &mut Settings, row: PrefRow, dir: i32, families: &[String], terms: &[String]) {
    if !enabled(s, row) {
        return; // greyed rows refuse input; see `enabled`
    }
    match row {
        PrefRow::FontSize => {
            s.font_size = (s.font_size + dir as f32).clamp(FONT_MIN, FONT_MAX);
        }
        PrefRow::FontFamily => {
            if families.is_empty() {
                return; // nothing installed to cycle through; leave the name alone
            }
            let n = families.len();
            // When the current family isn't installed, land on the natural end
            // for the direction rather than falling back to 0-then-step (which
            // would skip index 0 on a first Right / jump to n-1 on a first Left).
            let next = match families.iter().position(|f| *f == s.font_family) {
                Some(cur) => (cur as i32 + dir).rem_euclid(n as i32) as usize,
                None => if dir > 0 { 0 } else { n - 1 },
            };
            s.font_family = families[next].clone();
        }
        // Same cycle as Family, over the terminal types this machine has terminfo for.
        // `term_candidates` always includes the configured value, so `position` finds it
        // and the "not in the list" arm only fires for an empty list.
        PrefRow::Term => {
            if terms.is_empty() {
                return; // nothing to offer; leave the name alone rather than blank it
            }
            let n = terms.len();
            let next = match terms.iter().position(|t| *t == s.term) {
                Some(cur) => (cur as i32 + dir).rem_euclid(n as i32) as usize,
                None => if dir > 0 { 0 } else { n - 1 },
            };
            s.term = terms[next].clone();
        }
        // Reuse the existing rule rather than write a second one: `Settings`
        // already clamps opacity to MIN_OPACITY..=1.0 for the OpacityUp/Down
        // key actions, and two copies of a clamp drift.
        PrefRow::Opacity => {
            s.adjust_opacity(dir as f32 * OPACITY_STEP);
        }
        PrefRow::Scrollback => {
            let next = if dir > 0 { s.scrollback.saturating_mul(2) } else { s.scrollback / 2 };
            s.scrollback = next.clamp(SCROLLBACK_MIN, Settings::MAX_SCROLLBACK);
        }
        PrefRow::Preset => {
            let n = rt_config::SCHEMES.len();
            // Start from the scheme we currently match. When the colours match
            // no scheme ("custom"), land on the natural end for the direction
            // rather than 0-then-step: a first Right must hit SCHEMES[0], not
            // skip it to SCHEMES[1], and a first Left must hit the last scheme.
            let next = match rt_config::SCHEMES.iter().position(|c| c.name == preset_name(s)) {
                Some(cur) => (cur as i32 + dir).rem_euclid(n as i32) as usize,
                None => if dir > 0 { 0 } else { n - 1 },
            };
            let c = &rt_config::SCHEMES[next];
            s.foreground = c.foreground;
            s.background = c.background;
            s.palette = c.palette;
        }
        // Cycles rather than toggles: 13 materials, wrapping at both ends, so a
        // user can walk the whole list with one arrow key and watch each one land.
        PrefRow::GlassMaterial => s.macos_glass_material = s.macos_glass_material.step(dir),
        PrefRow::Blur => s.background_blur = !s.background_blur,
        PrefRow::Ffm => s.focus_follows_mouse = !s.focus_follows_mouse,
        PrefRow::Titlebar => s.show_titlebar = !s.show_titlebar,
        PrefRow::InstOutput => s.inst_output = !s.inst_output,
        PrefRow::InstHeat => s.inst_heat = !s.inst_heat,
        PrefRow::InstLatency => s.inst_latency = !s.inst_latency,
        PrefRow::Jacks => s.show_jacks = !s.show_jacks,
        PrefRow::InstRemote => s.inst_remote = !s.inst_remote,
        PrefRow::InstAnimate => s.inst_animate = !s.inst_animate,
        PrefRow::ArrowAccel => s.arrow_accel = !s.arrow_accel,
        PrefRow::ArrowAccelMax => {
            s.arrow_accel_max =
                (s.arrow_accel_max as i32 + dir).clamp(1, Settings::MAX_ARROW_ACCEL as i32) as u32;
        }
        PrefRow::Close => {} // handled by the caller; edits nothing
    }
}

// --- dismissal semantics --------------------------------------------------

/// How the colour picker was dismissed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PickerDismiss {
    /// The "Done (Esc)" button.
    Done,
    /// The Escape key.
    Escape,
    /// A press anywhere outside the picker panel.
    ClickedOutside,
}

/// What a dismissal does with the colour edited since the picker opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dismiss {
    /// Apply and persist it.
    Commit,
    /// Throw it away and restore the colour the slot had before.
    // Nothing constructs this today — `picker_dismiss` always says Commit. It is
    // kept so `close_picker`'s match is exhaustive over a real choice rather than
    // a single-variant rubber stamp: changing the decision above is then a
    // one-line edit that the compiler forces the caller to handle.
    #[allow(dead_code)]
    Discard,
}

/// What a dismissal does with the colour edited since the picker opened.
///
/// **All three ways out commit**, and this function exists to say so on purpose
/// rather than by accident — a review flagged the outside-click path as an
/// oversight, and it is worth writing down why it is not.
///
/// 1. **rt's preferences are live-apply with no Cancel.** There is no such row
///    ([`PrefRow::Close`] is "Close", not "Cancel"), no snapshot of the settings
///    as they were when the dialog opened, and every other exit — Escape, the
///    Close row, Enter/Space on it — commits. An outside click that discarded
///    would be the only destructive exit in the dialog, and the least announced
///    one.
/// 2. **`prefs_pending` is a debounce buffer, not a transaction.** Its whole
///    purpose (see `PREFS_SETTLE`) is to pay for a font reflow or a palette
///    rebuild once per run of edits instead of once per pointer-move. It holds
///    the *newest* value, never the *original* one, so there is nothing to roll
///    back to.
/// 3. **The settle already committed it.** The `PREFS_SETTLE` tick fires 150 ms
///    after the last edit whether or not the picker is still up, so by the time a
///    hand has moved the pointer out of the panel and pressed, the colour is
///    applied and written to `config.toml`. Discarding here could therefore only
///    abandon the last sub-150 ms of a drag while keeping everything before it —
///    not a cancel, an arbitrary partial rewind that leaves the user with a
///    colour they did not stop on. Making Discard *mean* anything would take a
///    real modal transaction: snapshot on open, suppress the settle while the
///    picker is up (re-introducing the per-move recolour the settle exists to
///    avoid), and un-persist. That is a different dialog.
///
/// So: dismissing keeps what you were looking at. That also matches what the
/// picker shows — the terminal behind it has been wearing the new colour for the
/// whole drag, and taking it away on the way out would be the surprise.
pub fn picker_dismiss(way: PickerDismiss) -> Dismiss {
    match way {
        PickerDismiss::Done | PickerDismiss::Escape | PickerDismiss::ClickedOutside => {
            Dismiss::Commit
        }
    }
}

/// Is a pending preferences edit due to be applied and persisted now?
///
/// `since_last_edit` is how long the pending value has held still and `settle` is
/// `PREFS_SETTLE`. Extracted from the run-loop tick so the rule above can be
/// stated as a test rather than only as prose.
///
/// Note what this function is NOT given: whether the preferences dialog or the
/// colour picker is still open. That is the point — the settle is unconditional,
/// which is what makes [`Dismiss::Discard`] incoherent for any edit older than
/// `settle`.
pub fn settle_due(has_pending: bool, since_last_edit: std::time::Duration, settle: std::time::Duration) -> bool {
    has_pending && since_last_edit >= settle
}

#[cfg(test)]
mod dismiss_tests {
    use super::*;
    use std::time::Duration;

    /// The decision under review: every way out of the picker keeps the colour.
    /// Exhaustive over [`PickerDismiss`], so adding a fourth way to dismiss
    /// without deciding what it does will not compile past this loop unnoticed.
    #[test]
    fn every_way_out_of_the_picker_commits() {
        for way in [PickerDismiss::Done, PickerDismiss::Escape, PickerDismiss::ClickedOutside] {
            assert_eq!(picker_dismiss(way), Dismiss::Commit, "{way:?} must commit");
        }
    }

    /// The invariant that makes a discard-on-dismiss impossible to implement
    /// honestly: the settle commits a pending edit once it has held still, with
    /// no knowledge of any overlay still being on screen. A user who drags the
    /// picker, pauses, then clicks outside has already had the colour applied and
    /// persisted before the click happened.
    #[test]
    fn the_settle_commits_regardless_of_what_is_still_on_screen() {
        let settle = Duration::from_millis(150);
        assert!(settle_due(true, settle, settle), "an edit that has held still commits");
        assert!(settle_due(true, Duration::from_millis(400), settle));
    }

    /// The two ways it must not fire: nothing pending, or the value still moving.
    #[test]
    fn the_settle_waits_for_a_pending_edit_to_hold_still() {
        let settle = Duration::from_millis(150);
        assert!(!settle_due(false, Duration::from_secs(10), settle), "nothing to commit");
        assert!(!settle_due(true, Duration::from_millis(149), settle), "still being dragged");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rt_config::Settings;

    fn fams() -> Vec<String> {
        vec!["Alpha Mono".to_string(), "Beta Mono".to_string(), "Gamma Mono".to_string()]
    }

    /// A fixed candidate list, NOT `rt_config::term_candidates()` — what terminfo the test
    /// machine happens to have installed must not decide whether these tests pass.
    fn terms() -> Vec<String> {
        vec!["xterm-256color".to_string(), "xterm-kitty".to_string(), "rt".to_string()]
    }

    #[test]
    fn font_size_steps_by_one_and_clamps_at_both_ends() {
        let mut s = Settings::default();
        s.font_size = 18.0;
        step(&mut s, PrefRow::FontSize, 1, &fams(), &terms());
        assert_eq!(s.font_size, 19.0);
        step(&mut s, PrefRow::FontSize, -1, &fams(), &terms());
        assert_eq!(s.font_size, 18.0);
        // Clamps at 48 (an unbounded slider could pick an unrenderable size).
        s.font_size = 48.0;
        step(&mut s, PrefRow::FontSize, 1, &fams(), &terms());
        assert_eq!(s.font_size, 48.0);
        // Clamps at 8.
        s.font_size = 8.0;
        step(&mut s, PrefRow::FontSize, -1, &fams(), &terms());
        assert_eq!(s.font_size, 8.0);
    }

    #[test]
    fn opacity_clamps_via_the_existing_settings_rule() {
        let mut s = Settings::default();
        s.background_opacity = 1.0;
        step(&mut s, PrefRow::Opacity, 1, &fams(), &terms());
        assert_eq!(s.background_opacity, 1.0, "must not exceed 1.0");
        // Down to the floor: MIN_OPACITY, never 0 (the window would vanish).
        for _ in 0..100 {
            step(&mut s, PrefRow::Opacity, -1, &fams(), &terms());
        }
        assert_eq!(s.background_opacity, Settings::MIN_OPACITY);
    }

    #[test]
    fn scrollback_doubles_and_halves_within_bounds() {
        let mut s = Settings::default();
        s.scrollback = 10_000;
        step(&mut s, PrefRow::Scrollback, 1, &fams(), &terms());
        assert_eq!(s.scrollback, 20_000, "logarithmic: x2 per step");
        step(&mut s, PrefRow::Scrollback, -1, &fams(), &terms());
        assert_eq!(s.scrollback, 10_000);
        // Clamps at the 1000 floor.
        s.scrollback = 1000;
        step(&mut s, PrefRow::Scrollback, -1, &fams(), &terms());
        assert_eq!(s.scrollback, 1000);
        // Clamps at MAX_SCROLLBACK (a full buffer no machine can hold).
        s.scrollback = Settings::MAX_SCROLLBACK;
        step(&mut s, PrefRow::Scrollback, 1, &fams(), &terms());
        assert_eq!(s.scrollback, Settings::MAX_SCROLLBACK);
    }

    #[test]
    fn family_cycles_and_wraps_both_ways() {
        let mut s = Settings::default();
        s.font_family = "Beta Mono".to_string();
        step(&mut s, PrefRow::FontFamily, 1, &fams(), &terms());
        assert_eq!(s.font_family, "Gamma Mono");
        step(&mut s, PrefRow::FontFamily, 1, &fams(), &terms());
        assert_eq!(s.font_family, "Alpha Mono", "wraps forward");
        step(&mut s, PrefRow::FontFamily, -1, &fams(), &terms());
        assert_eq!(s.font_family, "Gamma Mono", "wraps backward");
    }

    #[test]
    fn family_step_is_a_noop_when_no_families_are_installed() {
        let mut s = Settings::default();
        s.font_family = "Whatever".to_string();
        step(&mut s, PrefRow::FontFamily, 1, &[], &terms());
        assert_eq!(s.font_family, "Whatever", "must not panic or blank the family");
    }

    #[test]
    fn a_toggle_flips_on_either_direction() {
        let mut s = Settings::default();
        s.show_titlebar = true;
        step(&mut s, PrefRow::Titlebar, 1, &fams(), &terms());
        assert!(!s.show_titlebar);
        step(&mut s, PrefRow::Titlebar, -1, &fams(), &terms());
        assert!(s.show_titlebar, "Left and Right both toggle");
    }

    #[test]
    fn preset_applies_a_whole_scheme_and_reports_its_name() {
        let mut s = Settings::default();
        step(&mut s, PrefRow::Preset, 1, &fams(), &terms());
        let want = &rt_config::SCHEMES[1];
        assert_eq!(s.foreground, want.foreground);
        assert_eq!(s.background, want.background);
        assert_eq!(s.palette, want.palette, "a preset sets fg, bg AND the palette");
        assert_eq!(preset_name(&s), want.name);
    }

    #[test]
    fn preset_name_says_custom_when_colours_match_no_scheme() {
        let mut s = Settings::default();
        s.foreground = [1, 2, 3]; // the user's own, hand-edited in config.toml
        assert_eq!(preset_name(&s), "custom");
    }

    #[test]
    fn preset_from_custom_lands_on_the_first_scheme_not_the_second() {
        // Hand-edited colours match no scheme (preset_name -> "custom"). A first
        // Right must land on SCHEMES[0], not skip past it to SCHEMES[1].
        let mut s = Settings::default();
        s.foreground = [248, 194, 0]; // the user's own; matches nothing
        s.background = [1, 2, 3];
        step(&mut s, PrefRow::Preset, 1, &fams(), &terms());
        let first = &rt_config::SCHEMES[0];
        assert_eq!(s.foreground, first.foreground, "Right from custom -> first scheme");
        assert_eq!(s.background, first.background);
        assert_eq!(s.palette, first.palette);
        // And from custom, a first Left lands on the LAST scheme.
        let mut s = Settings::default();
        s.foreground = [248, 194, 0];
        s.background = [1, 2, 3];
        step(&mut s, PrefRow::Preset, -1, &fams(), &terms());
        let last = &rt_config::SCHEMES[rt_config::SCHEMES.len() - 1];
        assert_eq!(s.foreground, last.foreground, "Left from custom -> last scheme");
        assert_eq!(s.background, last.background);
        assert_eq!(s.palette, last.palette);
    }

    #[test]
    fn family_from_unmatched_lands_on_the_first_family_not_the_second() {
        // Current family isn't among the installed ones. Right -> first (index 0),
        // Left -> last (index n-1); neither skips index 0.
        let mut s = Settings::default();
        s.font_family = "Not Installed".to_string();
        step(&mut s, PrefRow::FontFamily, 1, &fams(), &terms());
        assert_eq!(s.font_family, "Alpha Mono", "Right from unmatched -> first family");
        let mut s = Settings::default();
        s.font_family = "Not Installed".to_string();
        step(&mut s, PrefRow::FontFamily, -1, &fams(), &terms());
        assert_eq!(s.font_family, "Gamma Mono", "Left from unmatched -> last family");
    }

    #[test]
    fn glass_material_cycles_and_wraps() {
        let mut s = Settings::default();
        s.background_blur = true;
        s.background_opacity = 0.5; // the row is live only while there IS glass
        let all = rt_config::GlassMaterial::ALL;
        assert_eq!(s.macos_glass_material, all[0], "starts at the default");
        step(&mut s, PrefRow::GlassMaterial, 1, &fams(), &terms());
        assert_eq!(s.macos_glass_material, all[1]);
        step(&mut s, PrefRow::GlassMaterial, -1, &fams(), &terms());
        assert_eq!(s.macos_glass_material, all[0]);
        step(&mut s, PrefRow::GlassMaterial, -1, &fams(), &terms());
        assert_eq!(s.macos_glass_material, all[all.len() - 1], "wraps backward");
        // A whole lap comes home: the user can walk every material with one key.
        for _ in 0..all.len() {
            step(&mut s, PrefRow::GlassMaterial, 1, &fams(), &terms());
        }
        assert_eq!(s.macos_glass_material, all[all.len() - 1]);
    }

    #[test]
    fn glass_material_is_disabled_when_there_is_no_glass_to_shape() {
        let mut s = Settings::default();
        s.background_blur = false;
        s.background_opacity = 0.5;
        assert!(!enabled(&s, PrefRow::GlassMaterial), "no blur -> no glass to shape");
        s.background_blur = true;
        s.background_opacity = 1.0;
        assert!(!enabled(&s, PrefRow::GlassMaterial), "opaque -> the glass is invisible");
        s.background_opacity = 0.5;
        assert!(enabled(&s, PrefRow::GlassMaterial));
        // And a disabled row refuses to step, like every other disabled row.
        s.background_blur = false;
        let before = s.macos_glass_material;
        step(&mut s, PrefRow::GlassMaterial, 1, &fams(), &terms());
        assert_eq!(s.macos_glass_material, before);
    }

    #[test]
    fn inst_animate_is_disabled_until_inst_remote_is_on() {
        let mut s = Settings::default();
        s.inst_remote = false;
        assert!(!enabled(&s, PrefRow::InstAnimate), "the 6fps tick needs both");
        s.inst_remote = true;
        assert!(enabled(&s, PrefRow::InstAnimate));
        // Everything else is always live.
        assert!(enabled(&s, PrefRow::FontSize));
    }

    #[test]
    fn stepping_a_disabled_row_changes_nothing() {
        let mut s = Settings::default();
        s.inst_remote = false;
        s.inst_animate = false;
        step(&mut s, PrefRow::InstAnimate, 1, &fams(), &terms());
        assert!(!s.inst_animate, "a greyed row must not be steppable");
    }
}
