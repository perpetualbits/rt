//! Which native macOS control each preferences row becomes, and how that
//! control's request is turned into moves of the SHARED model.
//!
//! Pure: no UI, no AppKit, no `cfg`. Same split as `menubar_model` /
//! `menubar.rs` and `vibrancy_policy` / `vibrancy.rs` — the decisions live here
//! where Linux CI compiles and tests them, and `settings_window.rs` is only the
//! objc2 half that obeys them.
//!
//! ## The rule this module exists to enforce
//!
//! **Nothing here assigns a setting.** Every change a native control asks for is
//! delivered by calling [`crate::prefs_model::step`] — the one function the
//! Linux dialog's arrow keys call — until the model has moved to what was asked
//! for, or has refused to move any further. That is what keeps the two platforms
//! from drifting: the clamps, the wraps, the opacity floor that rides on the
//! blur toggle, the family skip that will not land on a font rt cannot draw, and
//! the `scheme_candidates` ring that makes "custom" a position rather than a
//! dead end are all written down exactly once, in `prefs_model`, and macOS gets
//! them by construction rather than by a second implementation that agrees for
//! now.
//!
//! A native control is therefore a *request*, never a write. It says "I want
//! 0.35 opacity" or "I want the `graphite` chrome"; this module walks the model
//! towards that and reports where it actually landed, and the window writes that
//! back into the control. A slider dragged below the opacity floor visibly
//! springs back to the floor, because the floor is the model's and the model
//! said no.
//!
//! ## Enumerating a cycle without writing the list down again
//!
//! [`choices`] discovers what a cycle row can hold *by stepping a throwaway
//! clone all the way round it*, rather than by naming `ChromeTheme::ALL` /
//! `GlassMaterial::ALL` / `scheme_candidates` a second time here. A ring
//! enumerated by walking it cannot disagree with the ring the arrow keys walk.
//! The one exception is the font family, which is not a ring the model owns but
//! a list the caller supplies; see [`choices`].

use crate::chrome::prefs::RowKind;
use crate::prefs_model::{self, enabled, preset_name, PrefRow};
use rt_config::Settings;

/// The AppKit control a preferences row becomes.
///
/// One variant per *shape* of row, not per row: the point of the mapping is that
/// rt's rows are already only five shapes, and macOS has a standard control for
/// each of them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// `NSStepper` beside a read-only `NSTextField`. For a quantity whose
    /// natural gesture is "one more" — a font size, a scrollback doubling, an
    /// acceleration cap. One click on the stepper is exactly one
    /// `prefs_model::step`, which is why the geometric scrollback ladder fits
    /// here and would not fit a slider: the ladder is defined by the step, not
    /// by the range.
    Stepper,
    /// `NSSlider`. For a magnitude the user sweeps and judges by eye while it
    /// moves — the background opacity and the macOS blur radius, both of which
    /// change the window under the pointer as they are dragged.
    Slider,
    /// `NSSwitch`. Every `[x]` / `[ ]` row.
    Switch,
    /// `NSPopUpButton`. For a cycle over named values, where the Linux dialog
    /// offers `◄ name ►`: a Mac user expects to see the whole list and pick from
    /// it, not to arrow through it blind.
    PopUp,
    /// A row of `NSColorWell`s, each opening the system `NSColorPanel`.
    ///
    /// This is the one row where the native control is not merely nicer but
    /// *replaces a whole subsystem*: `chrome/colour_picker.rs` exists only
    /// because a terminal drawing its own chrome has no colour picker to call.
    /// macOS has one, it is the one every other Mac application uses, and it
    /// carries eyedropper, palettes, sliders and recent colours for free.
    Swatches,
    /// No control at all. A native window is dismissed by its own close button
    /// and by ⌘W, so [`PrefRow::Close`] has nothing to draw — the one row of the
    /// frozen order that does not survive the port, and the only one.
    Dismiss,
}

/// The control [`row`](PrefRow) becomes. Total over `PrefRow`, so a row added to
/// the model does not compile until someone has decided what it looks like on a
/// Mac.
pub fn control(row: PrefRow) -> Control {
    match row {
        // Quantities with a natural "one more" gesture.
        PrefRow::FontSize => Control::Stepper,
        PrefRow::Scrollback => Control::Stepper,
        PrefRow::ArrowAccelMax => Control::Stepper,
        // Magnitudes judged by eye while they move.
        PrefRow::Opacity => Control::Slider,
        PrefRow::BlurRadius => Control::Slider,
        // Named cycles.
        PrefRow::FontFamily => Control::PopUp,
        PrefRow::Preset => Control::PopUp,
        PrefRow::Chrome => Control::PopUp,
        PrefRow::GlassMaterial => Control::PopUp,
        PrefRow::Term => Control::PopUp,
        // Booleans.
        PrefRow::Blur => Control::Switch,
        PrefRow::Ffm => Control::Switch,
        PrefRow::Titlebar => Control::Switch,
        PrefRow::InstOutput => Control::Switch,
        PrefRow::InstHeat => Control::Switch,
        PrefRow::InstLatency => Control::Switch,
        PrefRow::Jacks => Control::Switch,
        PrefRow::InstRemote => Control::Switch,
        PrefRow::InstAnimate => Control::Switch,
        PrefRow::ArrowAccel => Control::Switch,
        // The dismiss row, which a native window does not need.
        PrefRow::Close => Control::Dismiss,
    }
}

/// The control a whole preferences ROW becomes — including the rows that carry
/// no [`PrefRow`] at all.
///
/// [`control`] answers for a setting; this answers for a line of
/// `chrome::prefs::rows`, which is what a window actually has to build. The two
/// rows without a `PrefRow` are the reason it exists: the palette swatches (a
/// row of colour wells, edited through the system colour panel rather than by
/// stepping anything) and the section headings and read-only readouts, which are
/// text and get `None`.
pub fn row_control(kind: RowKind, pref: Option<PrefRow>) -> Option<Control> {
    match (kind, pref) {
        (RowKind::Swatches, _) => Some(Control::Swatches),
        (_, Some(p)) => Some(control(p)),
        _ => None, // a heading or a readout: text, not a control
    }
}

/// The boolean a [`Control::Switch`] row shows, or `None` for every other row.
pub fn flag(s: &Settings, row: PrefRow) -> Option<bool> {
    Some(match row {
        PrefRow::Blur => s.background_blur,
        PrefRow::Ffm => s.focus_follows_mouse,
        PrefRow::Titlebar => s.show_titlebar,
        PrefRow::InstOutput => s.inst_output,
        PrefRow::InstHeat => s.inst_heat,
        PrefRow::InstLatency => s.inst_latency,
        PrefRow::Jacks => s.show_jacks,
        PrefRow::InstRemote => s.inst_remote,
        PrefRow::InstAnimate => s.inst_animate,
        PrefRow::ArrowAccel => s.arrow_accel,
        _ => return None,
    })
}

/// The number a [`Control::Stepper`] or [`Control::Slider`] row shows, or `None`
/// for every other row.
pub fn number(s: &Settings, row: PrefRow) -> Option<f64> {
    Some(match row {
        PrefRow::FontSize => s.font_size as f64,
        PrefRow::Opacity => s.background_opacity as f64,
        PrefRow::Scrollback => s.scrollback as f64,
        PrefRow::BlurRadius => s.macos_blur_radius as f64,
        PrefRow::ArrowAccelMax => s.arrow_accel_max as f64,
        _ => return None,
    })
}

/// The name a [`Control::PopUp`] row currently shows, or `None` for every other
/// row. The same strings the Linux dialog puts between its `◄ ►`.
pub fn label(s: &Settings, row: PrefRow) -> Option<String> {
    Some(match row {
        PrefRow::FontFamily => s.font_family.clone(),
        PrefRow::Preset => preset_name(s).to_string(),
        PrefRow::Chrome => s.chrome_theme.name().to_string(),
        PrefRow::GlassMaterial => s.macos_glass_material.name().to_string(),
        PrefRow::Term => s.term.clone(),
        _ => return None,
    })
}

/// A hard bound on how many `prefs_model::step` calls one request may make.
///
/// Every real ladder is far shorter than this (48 font sizes, 20 opacity stops,
/// 25 blur radii, 30 acceleration caps, 13 scrollback doublings, 14 glass
/// materials); the font family list is the only one that can approach it, at one
/// entry per installed monospace family. It exists so that a bug in a `step`
/// rule can cost a slow frame rather than a hung terminal.
const MAX_STEPS: usize = 4096;

/// Unlock `row` on a THROWAWAY clone, so its cycle can be enumerated while the
/// real settings have it greyed.
///
/// `prefs_model::step` refuses a disabled row, which is right for input and
/// wrong for enumeration: a greyed "Glass material" popup must still list the
/// fourteen materials, or it would show one item and lie about the choice
/// waiting behind the blur toggle. Only the gate `prefs_model::enabled` actually
/// tests is touched, and only on a clone that is thrown away three lines later.
fn unlock(s: &mut Settings, row: PrefRow) {
    if enabled(s, row) {
        return;
    }
    match row {
        // `wants_background_blur() == background_blur && opacity < 1.0`.
        PrefRow::GlassMaterial | PrefRow::BlurRadius => {
            s.background_blur = true;
            if s.background_opacity >= 1.0 {
                s.background_opacity = 0.9;
            }
            // `BlurRadius` additionally wants a non-effect-view material; the
            // radius is not a cycle row, so nothing enumerates it and this arm
            // is only ever reached for the material.
        }
        PrefRow::InstAnimate => s.inst_remote = true,
        PrefRow::ArrowAccelMax => s.arrow_accel = true,
        _ => {}
    }
}

/// Every value a [`Control::PopUp`] row can take, in the model's own cycle
/// order.
///
/// Discovered by walking a throwaway clone once round the ring with
/// `prefs_model::step`, never by naming `ChromeTheme::ALL`,
/// `GlassMaterial::ALL`, `term_candidates` or `scheme_candidates` here. A list
/// enumerated by walking cannot disagree with the ring the arrow keys walk, and
/// a new chrome theme appears in the popup with no edit to this file.
///
/// **The font family is the exception**, and deliberately: it is not a ring the
/// model owns but the caller's installed-family list, and the model's walk over
/// it *skips every family rt cannot rasterise* — which costs a font parse per
/// family skipped. Walking all of it here to build the menu would mean parsing
/// every installed monospace face before the Settings window could open (~0.5 s
/// in release; two orders of magnitude worse in a debug build — the measurement
/// is recorded on `prefs_model::step`). So the popup lists the families as
/// given, and [`apply_choice`] lets the model land on the nearest one it can
/// actually draw; the window then re-reads [`label`] and the popup snaps to what
/// was really chosen. That is the same answer the Family row's advisory line
/// gives on Linux — the dialog never shows a font that is not on screen — paid
/// for lazily instead of up front.
pub fn choices(s: &Settings, row: PrefRow, families: &[String], terms: &[String]) -> Vec<String> {
    if control(row) != Control::PopUp {
        return Vec::new();
    }
    if row == PrefRow::FontFamily {
        return families.to_vec();
    }
    let mut probe = s.clone();
    unlock(&mut probe, row);
    let Some(first) = label(&probe, row) else { return Vec::new() };
    let mut out = vec![first.clone()];
    for _ in 0..MAX_STEPS {
        prefs_model::step(&mut probe, row, 1, families, terms, &mut |_| true);
        let Some(next) = label(&probe, row) else { break };
        if next == first {
            break; // back where we started: the ring is closed
        }
        if out.contains(&next) {
            break; // a ring that does not return to its start; stop rather than spin
        }
        out.push(next);
    }
    out
}

/// The span a [`Control::Slider`] row may be dragged over, as
/// `(min, max)` — or `None` for every other row.
///
/// Probed off the model rather than read from `FONT_MIN` / `MIN_OPACITY` /
/// `MIN_BLUR_RADIUS`: step a clone down until it stops moving, and that IS the
/// floor, including the one that moves — the opacity floor is 0.0 while blur is
/// on and 0.05 while it is off, and a slider built from a constant would offer a
/// travel the model then silently refuses.
pub fn range(s: &Settings, row: PrefRow) -> Option<(f64, f64)> {
    number(s, row)?;
    let mut lo = s.clone();
    unlock(&mut lo, row);
    let mut hi = lo.clone();
    let end = |s: &mut Settings, dir: i32| -> f64 {
        let mut last = number(s, row).unwrap_or(0.0);
        for _ in 0..MAX_STEPS {
            prefs_model::step(s, row, dir, &[], &[], &mut |_| true);
            let now = number(s, row).unwrap_or(last);
            if now == last {
                break;
            }
            last = now;
        }
        last
    };
    Some((end(&mut lo, -1), end(&mut hi, 1)))
}

/// Drive `row` to `target` by repeated [`prefs_model::step`], and report where
/// the model actually landed.
///
/// Never assigns: the only thing that touches `s` is `step`. The walk takes the
/// shorter way round the ring, stops the moment the label matches, and gives up
/// as soon as the model stops moving or steps clean over the target — which is
/// what the font-family row does when the family asked for has no outlines rt
/// can read. The caller re-reads [`label`] and re-selects the popup, so a pick
/// that the model redirected is visible rather than silent.
pub fn apply_choice(
    s: &mut Settings,
    row: PrefRow,
    target: &str,
    families: &[String],
    terms: &[String],
    usable: &mut dyn FnMut(&str) -> bool,
) -> Option<String> {
    if control(row) != Control::PopUp {
        return None;
    }
    let list = choices(s, row, families, terms);
    let n = list.len();
    let cur = label(s, row)?;
    if cur == target || n == 0 {
        return Some(cur);
    }
    let at = list.iter().position(|c| *c == cur);
    let to = list.iter().position(|c| *c == target)?;
    // The shorter way round. With no current position in the list (a family that
    // is not installed), forward from the start is the model's own convention —
    // see `prefs_model::step`'s `None` arm.
    let dir = match at {
        Some(i) => {
            let fwd = (to as i32 - i as i32).rem_euclid(n as i32);
            if fwd * 2 <= n as i32 { 1 } else { -1 }
        }
        None => 1,
    };
    for _ in 0..n.min(MAX_STEPS) {
        let before = label(s, row)?;
        prefs_model::step(s, row, dir, families, terms, usable);
        let after = label(s, row)?;
        if after == target {
            return Some(after);
        }
        if after == before {
            return Some(after); // the model refused to move; nothing more to try
        }
        // Stepped clean over the target — the family row skipping what it cannot
        // draw. Stop on the nearest reachable value in the direction of travel
        // rather than lapping the ring looking for one that is not there.
        let (Some(b), Some(a)) = (list.iter().position(|c| *c == before), list.iter().position(|c| *c == after))
        else {
            return Some(after);
        };
        if passed(b, a, to, dir, n) {
            return Some(after);
        }
    }
    label(s, row)
}

/// Did a move from index `b` to index `a` (travelling in `dir` round a ring of
/// `n`) go past `to` without landing on it?
fn passed(b: usize, a: usize, to: usize, dir: i32, n: usize) -> bool {
    let n = n as i32;
    let fwd = |from: usize, x: usize| (x as i32 - from as i32).rem_euclid(n);
    if dir > 0 {
        fwd(b, a) > fwd(b, to)
    } else {
        fwd(a, b) > fwd(to, b)
    }
}

/// Drive a [`Control::Switch`] row to `target`, and report where it landed.
///
/// `NSSwitch` reports the state it has been dragged to; the model has only a
/// flip. So this compares and flips at most once — which is the whole
/// translation, and is why the opacity floor still rides on the blur switch:
/// the flip is `prefs_model::step`, not `s.background_blur = target`.
pub fn apply_flag(
    s: &mut Settings,
    row: PrefRow,
    target: bool,
    families: &[String],
    terms: &[String],
    usable: &mut dyn FnMut(&str) -> bool,
) -> Option<bool> {
    let cur = flag(s, row)?;
    if cur != target {
        prefs_model::step(s, row, 1, families, terms, usable);
    }
    flag(s, row)
}

/// Drive `row` to `target` by repeated [`prefs_model::step`], and report the
/// number the model actually landed on.
///
/// A slider hands over a continuous value; the model has a ladder. This walks
/// the ladder towards the request and stops on the rung nearest it — never past
/// it by more than half a rung, and never past what the model will clamp to.
/// That is what makes a drag below the opacity floor spring back: the step is
/// refused, the value stops changing, and the window writes the floor into the
/// slider on the next refresh.
pub fn apply_number(
    s: &mut Settings,
    row: PrefRow,
    target: f64,
    families: &[String],
    terms: &[String],
    usable: &mut dyn FnMut(&str) -> bool,
) -> Option<f64> {
    let start = number(s, row)?;
    if !matches!(control(row), Control::Stepper | Control::Slider) {
        return Some(start);
    }
    let mut cur = start;
    for _ in 0..MAX_STEPS {
        if cur == target {
            return Some(cur);
        }
        let dir = if target > cur { 1 } else { -1 };
        let mut probe = s.clone();
        prefs_model::step(&mut probe, row, dir, families, terms, usable);
        let next = number(&probe, row)?;
        if next == cur {
            return Some(cur); // clamped, or the row is disabled: the model said no
        }
        // Overshoot: take the step only if the rung past the target is nearer to
        // it than the rung before.
        let over = if dir > 0 { next > target } else { next < target };
        if over && (next - target).abs() >= (cur - target).abs() {
            return Some(cur);
        }
        *s = probe;
        cur = next;
        if over {
            return Some(cur);
        }
    }
    Some(cur)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::chrome::prefs;
    use crate::prefs_model::FamilyStatus;

    fn fams() -> Vec<String> {
        vec!["Alpha Mono".into(), "Beta Mono".into(), "Gamma Mono".into(), "Delta Mono".into()]
    }
    fn terms() -> Vec<String> {
        vec!["xterm-256color".into(), "rt".into()]
    }
    fn yes() -> impl FnMut(&str) -> bool {
        |_| true
    }

    /// Every row the model knows about, so the totality tests cannot be fooled
    /// by a row that merely happens to be in `chrome::prefs::rows()` on Linux.
    /// Kept in the frozen order `chrome::prefs`'s own `the_row_order_is_frozen`
    /// test pins.
    const EVERY_ROW: &[PrefRow] = &[
        PrefRow::FontSize,
        PrefRow::FontFamily,
        PrefRow::Opacity,
        PrefRow::Blur,
        PrefRow::GlassMaterial,
        PrefRow::BlurRadius,
        PrefRow::Chrome,
        PrefRow::Preset,
        PrefRow::Ffm,
        PrefRow::Titlebar,
        PrefRow::Scrollback,
        PrefRow::ArrowAccel,
        PrefRow::ArrowAccelMax,
        PrefRow::Term,
        PrefRow::InstOutput,
        PrefRow::InstHeat,
        PrefRow::InstLatency,
        PrefRow::Jacks,
        PrefRow::InstRemote,
        PrefRow::InstAnimate,
        PrefRow::Close,
    ];

    /// The port's central promise: every row of the frozen order has a native
    /// control, and exactly one row — the dismiss row — does not, because a
    /// native window's close button is it.
    #[test]
    fn every_row_maps_to_a_native_control() {
        let dismissed: Vec<PrefRow> =
            EVERY_ROW.iter().copied().filter(|r| control(*r) == Control::Dismiss).collect();
        assert_eq!(dismissed, vec![PrefRow::Close], "only Close may map to nothing");
    }

    /// Every row the LINUX dialog actually builds is represented too — the check
    /// that `EVERY_ROW` above has not quietly fallen behind `chrome::prefs::rows`.
    #[test]
    fn the_linux_dialog_s_rows_are_all_covered() {
        let s = Settings::default();
        let rows = prefs::rows(&s, 16 << 30, 80, FamilyStatus::Usable);
        for r in rows.iter().filter_map(|r| r.pref) {
            assert!(EVERY_ROW.contains(&r), "{r:?} is built on Linux but unmapped here");
            let c = control(r);
            if r == PrefRow::Close {
                assert_eq!(c, Control::Dismiss);
            } else {
                assert_ne!(c, Control::Dismiss, "{r:?} needs a real control");
            }
        }
    }

    /// The value accessors line up with the control: a Switch row has a flag and
    /// nothing else, a Stepper/Slider row a number, a PopUp row a name.
    #[test]
    fn each_control_has_exactly_the_value_it_needs() {
        let s = Settings::default();
        for &r in EVERY_ROW {
            let (f, n, l) = (flag(&s, r).is_some(), number(&s, r).is_some(), label(&s, r).is_some());
            match control(r) {
                Control::Switch => assert_eq!((f, n, l), (true, false, false), "{r:?}"),
                Control::Stepper | Control::Slider => {
                    assert_eq!((f, n, l), (false, true, false), "{r:?}")
                }
                Control::PopUp => assert_eq!((f, n, l), (false, false, true), "{r:?}"),
                Control::Swatches | Control::Dismiss => {
                    assert_eq!((f, n, l), (false, false, false), "{r:?}")
                }
            }
        }
    }

    /// A popup's menu is the model's ring, enumerated by walking it. Checked
    /// against the lists the model is built from, which is the one place those
    /// lists may be named.
    #[test]
    fn a_popup_lists_the_model_s_whole_ring() {
        let s = Settings::default();
        let chrome = choices(&s, PrefRow::Chrome, &[], &[]);
        assert_eq!(chrome.len(), rt_config::ChromeTheme::ALL.len());
        assert!(chrome.contains(&"graphite".to_string()));
        let glass = choices(&s, PrefRow::GlassMaterial, &[], &[]);
        assert_eq!(glass.len(), rt_config::GlassMaterial::ALL.len(), "all fourteen materials");
        let t = choices(&s, PrefRow::Term, &[], &terms());
        assert_eq!(t.len(), rt_config::term_candidates(&s.term).len());
    }

    /// The greyed case: the glass popup still lists every material while the
    /// blur toggle has the row dimmed, instead of collapsing to one item.
    #[test]
    fn a_dimmed_popup_still_lists_its_ring() {
        let mut s = Settings::default();
        s.background_blur = false;
        assert!(!enabled(&s, PrefRow::GlassMaterial), "the row must be dimmed for this test");
        assert_eq!(
            choices(&s, PrefRow::GlassMaterial, &[], &[]).len(),
            rt_config::GlassMaterial::ALL.len()
        );
    }

    /// Picking from a popup reaches every entry, in both directions round the
    /// ring, and lands on exactly what was picked.
    #[test]
    fn picking_from_a_popup_reaches_every_entry() {
        let mut base = Settings::default();
        // The glass row is dimmed with no glass on screen, and a dimmed row
        // refuses a pick exactly as it refuses an arrow key (see
        // `a_disabled_row_refuses_a_native_request`). Give it glass to edit.
        base.background_blur = true;
        base.background_opacity = 0.85;
        for row in [PrefRow::Chrome, PrefRow::GlassMaterial] {
            let list = choices(&base, row, &[], &[]);
            for want in &list {
                let mut s = base.clone();
                let got = apply_choice(&mut s, row, want, &[], &terms(), &mut yes());
                assert_eq!(got.as_deref(), Some(want.as_str()), "{row:?} -> {want}");
                assert_eq!(label(&s, row).as_deref(), Some(want.as_str()), "{row:?} really moved");
            }
        }
    }

    /// The load-bearing one: a pick goes through `prefs_model::step`, so a rule
    /// that only `step` knows still fires. Turning the blur switch off through
    /// the native path must carry the opacity back over the floor exactly as the
    /// Linux dialog's Space key does.
    #[test]
    fn a_native_change_still_pays_the_model_s_side_effects() {
        let mut s = Settings::default();
        s.background_blur = true;
        s.background_opacity = 0.0; // legal only while blurred
        let mut native = s.clone();
        apply_flag(&mut native, PrefRow::Blur, false, &[], &[], &mut yes());
        let mut linux = s.clone();
        prefs_model::step(&mut linux, PrefRow::Blur, 1, &[], &[], &mut yes());
        assert_eq!(native, linux, "the native switch and the arrow key must agree");
        assert!(native.background_opacity >= Settings::MIN_OPACITY, "the floor was enforced");
    }

    /// A slider's travel is the model's, floor included — and the floor moves
    /// with the blur toggle, which is why it is probed and not hardcoded.
    #[test]
    fn a_slider_s_range_is_the_model_s_own() {
        let mut s = Settings::default();
        s.background_blur = false;
        assert_eq!(range(&s, PrefRow::Opacity), Some((Settings::MIN_OPACITY as f64, 1.0)));
        s.background_blur = true;
        s.background_opacity = 0.5;
        assert_eq!(range(&s, PrefRow::Opacity), Some((Settings::MIN_OPACITY_BLURRED as f64, 1.0)));
        assert_eq!(range(&s, PrefRow::FontSize), Some((8.0, 48.0)));
        assert_eq!(range(&s, PrefRow::ArrowAccelMax), Some((1.0, Settings::MAX_ARROW_ACCEL as f64)));
    }

    /// A drag past the end of the travel lands on the end, not past it.
    #[test]
    fn a_slider_dragged_past_the_end_stops_at_the_model_s_clamp() {
        let mut s = Settings::default();
        s.background_blur = false;
        let got = apply_number(&mut s, PrefRow::Opacity, -5.0, &[], &[], &mut yes());
        assert_eq!(got, Some(Settings::MIN_OPACITY as f64));
        let got = apply_number(&mut s, PrefRow::FontSize, 9999.0, &[], &[], &mut yes());
        assert_eq!(got, Some(48.0));
    }

    /// A slider lands on the nearest rung of the model's ladder, never between
    /// two of them — the value written to `config.toml` is one the arrow keys
    /// could also have produced.
    #[test]
    fn a_slider_lands_on_a_rung_the_arrow_keys_could_reach() {
        let mut s = Settings::default();
        s.background_opacity = 1.0;
        apply_number(&mut s, PrefRow::Opacity, 0.62, &[], &[], &mut yes());
        let got = s.background_opacity as f64;
        let steps = ((1.0 - got) / 0.05).round();
        assert!((1.0 - steps * 0.05 - got).abs() < 1e-4, "on the 0.05 ladder");
        assert!((got - 0.62).abs() <= 0.025 + 1e-4, "and nearest to the ask");
    }

    /// The stepper rows are the geometric one's home: one click is one doubling,
    /// and a request in between lands on a rung of that ladder.
    #[test]
    fn the_scrollback_stepper_walks_the_doubling_ladder() {
        let mut s = Settings::default();
        s.scrollback = 10_000;
        apply_number(&mut s, PrefRow::Scrollback, 70_000.0, &[], &[], &mut yes());
        assert_eq!(s.scrollback, 80_000, "10k doubled three times, nearest to 70k");
        apply_number(&mut s, PrefRow::Scrollback, 0.0, &[], &[], &mut yes());
        assert_eq!(s.scrollback, 1000, "the floor clamp is the model's, not a rung");
    }

    /// A disabled row refuses a native request exactly as it refuses an arrow
    /// key — the greying is the model's, not the window's.
    #[test]
    fn a_disabled_row_refuses_a_native_request() {
        let mut s = Settings::default();
        s.arrow_accel = false;
        let was = s.arrow_accel_max;
        assert_eq!(apply_number(&mut s, PrefRow::ArrowAccelMax, 99.0, &[], &[], &mut yes()), Some(was as f64));
        assert_eq!(s.arrow_accel_max, was);
    }

    /// The family popup: picking a family rt cannot draw lands on the nearest one
    /// it can, rather than showing a name that is not on screen.
    #[test]
    fn the_family_popup_cannot_land_on_a_font_rt_cannot_draw() {
        let f = fams();
        let mut s = Settings::default();
        s.font_family = "Alpha Mono".into();
        let mut oracle = |name: &str| name != "Beta Mono" && name != "Gamma Mono";
        let got = apply_choice(&mut s, PrefRow::FontFamily, "Beta Mono", &f, &[], &mut oracle);
        assert_eq!(got.as_deref(), Some("Delta Mono"), "skipped the two unusable families");
        assert_eq!(s.font_family, "Delta Mono");
    }

    /// And a usable one is reached exactly.
    #[test]
    fn the_family_popup_reaches_a_usable_family() {
        let f = fams();
        for want in &f {
            let mut s = Settings::default();
            s.font_family = "Alpha Mono".into();
            let got = apply_choice(&mut s, PrefRow::FontFamily, want, &f, &[], &mut yes());
            assert_eq!(got.as_deref(), Some(want.as_str()));
            assert_eq!(&s.font_family, want);
        }
    }

    /// A colour is the one row whose control is not a step at all, and it is the
    /// reason `row_control` exists: it carries no `PrefRow`, so `control` alone
    /// could never reach it.
    #[test]
    fn the_palette_row_is_a_colour_well_row() {
        let s = Settings::default();
        let rows = prefs::rows(&s, 16 << 30, 80, FamilyStatus::Usable);
        let sw = rows.iter().find(|r| r.kind == RowKind::Swatches).expect("a palette row");
        assert_eq!(row_control(sw.kind, sw.pref), Some(Control::Swatches));
        assert!(EVERY_ROW.iter().all(|r| control(*r) != Control::Swatches), "no PrefRow is a swatch");
    }

    /// Headings and readouts are text, not controls — a native window must not
    /// try to build a stepper for the scrollback guardrail line.
    #[test]
    fn headings_and_readouts_are_not_controls() {
        let s = Settings::default();
        let rows = prefs::rows(&s, 16 << 30, 80, FamilyStatus::Usable);
        for r in rows.iter().filter(|r| matches!(r.kind, RowKind::Section | RowKind::Display)) {
            assert_eq!(row_control(r.kind, r.pref), None, "{:?} {:?}", r.kind, r.label);
        }
        // And every row that IS a control is reachable through `row_control`.
        for r in rows.iter().filter(|r| r.pref.is_some() || r.kind == RowKind::Swatches) {
            assert!(row_control(r.kind, r.pref).is_some(), "{:?}", r.label);
        }
    }
}
