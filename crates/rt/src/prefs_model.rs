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
/// Only one row is ever disabled, and for a reason worth encoding: the 6fps
/// instrument tick is gated on BOTH flags (`instruments_animating = anim &&
/// inst_remote && inst_animate`), so `inst_animate` alone does nothing while
/// `inst_remote` is off. Setting `inst_remote = true` and seeing no animation —
/// because `inst_animate` defaulted false — is exactly the trap this avoids.
pub fn enabled(s: &Settings, row: PrefRow) -> bool {
    match row {
        PrefRow::InstAnimate => s.inst_remote,
        PrefRow::ArrowAccelMax => s.arrow_accel, // the cap is moot when acceleration is off
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
pub fn step(s: &mut Settings, row: PrefRow, dir: i32, families: &[String]) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use rt_config::Settings;

    fn fams() -> Vec<String> {
        vec!["Alpha Mono".to_string(), "Beta Mono".to_string(), "Gamma Mono".to_string()]
    }

    #[test]
    fn font_size_steps_by_one_and_clamps_at_both_ends() {
        let mut s = Settings::default();
        s.font_size = 18.0;
        step(&mut s, PrefRow::FontSize, 1, &fams());
        assert_eq!(s.font_size, 19.0);
        step(&mut s, PrefRow::FontSize, -1, &fams());
        assert_eq!(s.font_size, 18.0);
        // Clamps at 48 (an unbounded slider could pick an unrenderable size).
        s.font_size = 48.0;
        step(&mut s, PrefRow::FontSize, 1, &fams());
        assert_eq!(s.font_size, 48.0);
        // Clamps at 8.
        s.font_size = 8.0;
        step(&mut s, PrefRow::FontSize, -1, &fams());
        assert_eq!(s.font_size, 8.0);
    }

    #[test]
    fn opacity_clamps_via_the_existing_settings_rule() {
        let mut s = Settings::default();
        s.background_opacity = 1.0;
        step(&mut s, PrefRow::Opacity, 1, &fams());
        assert_eq!(s.background_opacity, 1.0, "must not exceed 1.0");
        // Down to the floor: MIN_OPACITY, never 0 (the window would vanish).
        for _ in 0..100 {
            step(&mut s, PrefRow::Opacity, -1, &fams());
        }
        assert_eq!(s.background_opacity, Settings::MIN_OPACITY);
    }

    #[test]
    fn scrollback_doubles_and_halves_within_bounds() {
        let mut s = Settings::default();
        s.scrollback = 10_000;
        step(&mut s, PrefRow::Scrollback, 1, &fams());
        assert_eq!(s.scrollback, 20_000, "logarithmic: x2 per step");
        step(&mut s, PrefRow::Scrollback, -1, &fams());
        assert_eq!(s.scrollback, 10_000);
        // Clamps at the 1000 floor.
        s.scrollback = 1000;
        step(&mut s, PrefRow::Scrollback, -1, &fams());
        assert_eq!(s.scrollback, 1000);
        // Clamps at MAX_SCROLLBACK (a full buffer no machine can hold).
        s.scrollback = Settings::MAX_SCROLLBACK;
        step(&mut s, PrefRow::Scrollback, 1, &fams());
        assert_eq!(s.scrollback, Settings::MAX_SCROLLBACK);
    }

    #[test]
    fn family_cycles_and_wraps_both_ways() {
        let mut s = Settings::default();
        s.font_family = "Beta Mono".to_string();
        step(&mut s, PrefRow::FontFamily, 1, &fams());
        assert_eq!(s.font_family, "Gamma Mono");
        step(&mut s, PrefRow::FontFamily, 1, &fams());
        assert_eq!(s.font_family, "Alpha Mono", "wraps forward");
        step(&mut s, PrefRow::FontFamily, -1, &fams());
        assert_eq!(s.font_family, "Gamma Mono", "wraps backward");
    }

    #[test]
    fn family_step_is_a_noop_when_no_families_are_installed() {
        let mut s = Settings::default();
        s.font_family = "Whatever".to_string();
        step(&mut s, PrefRow::FontFamily, 1, &[]);
        assert_eq!(s.font_family, "Whatever", "must not panic or blank the family");
    }

    #[test]
    fn a_toggle_flips_on_either_direction() {
        let mut s = Settings::default();
        s.show_titlebar = true;
        step(&mut s, PrefRow::Titlebar, 1, &fams());
        assert!(!s.show_titlebar);
        step(&mut s, PrefRow::Titlebar, -1, &fams());
        assert!(s.show_titlebar, "Left and Right both toggle");
    }

    #[test]
    fn preset_applies_a_whole_scheme_and_reports_its_name() {
        let mut s = Settings::default();
        step(&mut s, PrefRow::Preset, 1, &fams());
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
        step(&mut s, PrefRow::Preset, 1, &fams());
        let first = &rt_config::SCHEMES[0];
        assert_eq!(s.foreground, first.foreground, "Right from custom -> first scheme");
        assert_eq!(s.background, first.background);
        assert_eq!(s.palette, first.palette);
        // And from custom, a first Left lands on the LAST scheme.
        let mut s = Settings::default();
        s.foreground = [248, 194, 0];
        s.background = [1, 2, 3];
        step(&mut s, PrefRow::Preset, -1, &fams());
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
        step(&mut s, PrefRow::FontFamily, 1, &fams());
        assert_eq!(s.font_family, "Alpha Mono", "Right from unmatched -> first family");
        let mut s = Settings::default();
        s.font_family = "Not Installed".to_string();
        step(&mut s, PrefRow::FontFamily, -1, &fams());
        assert_eq!(s.font_family, "Gamma Mono", "Left from unmatched -> last family");
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
        step(&mut s, PrefRow::InstAnimate, 1, &fams());
        assert!(!s.inst_animate, "a greyed row must not be steppable");
    }
}
