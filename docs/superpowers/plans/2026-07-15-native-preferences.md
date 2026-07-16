# Native Preferences Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace rt's egui-only preferences dialog with a keyboard-driven native one that works on BOTH backends — so the milkv (riscv64, `ssh -X`), which today has no preferences at all, gets them.

**Architecture:** Two new pure modules — `prefs_model.rs` (which setting a row edits, and how a step clamps) and `chrome/prefs.rs` (geometry + draw + hit) — following the existing `chrome/menu.rs` split of a testable `layout() -> Geom` from `draw(be, &geom)`. `main.rs` holds the dialog's state, a keyboard shim beside the manual's, and a 150ms commit-on-settle that reuses the diff-and-apply block lifted out of `paint_egui`. `preferences.rs` (egui) is deleted.

**Tech Stack:** Rust, winit 0.30 (`WindowEvent`, `Key`/`NamedKey`), rt's `Backend` trait (`fill_rect` / `draw_char` / `cell_size`), `rt_config::Settings`, Xvfb + xtrace + ImageMagick for the integration gates.

## Global Constraints

Copied verbatim from `docs/superpowers/specs/2026-07-15-native-preferences-design.md`. Every task's requirements implicitly include these.

- **No drags, no continuous adjustment.** Every edit is ONE discrete step = ONE frame. A draggable slider is a full window repaint per pointer sample — the cost removed from the divider and the wire this session.
- **Commit on settle: `PREFS_SETTLE = 150ms`.** The dialog shows a new value instantly (text is free); apply+persist happens once the value stops changing. Stepping font size 18 → 24 costs ONE glyph re-rasterisation + reflow (~700ms on a milkv) and ONE `config.toml` write, not six of each.
- **`preferences.rs` is DELETED.** Native everywhere; the native dialog serves both backends. Two dialogs that must agree about every setting forever is the drift this session paid for twice.
- **No per-colour editing.** Colours are chosen by preset scheme (`rt_config::SCHEMES`); individual colours stay in `config.toml`. The 16 palette + fg/bg render as read-only swatches.
- **Chrome carries no engine cell-damage.** An open overlay already forces full frames (`overlay_open → mark_full`) and suspends input, so frames only happen on a keypress. Add no new damage machinery.
- **`inst_animate` greys out while `inst_remote` is off** — the 6fps tick is gated on both (`instruments_animating = anim && inst_remote && inst_animate`), and `inst_remote = true` alone doing nothing visible is exactly the trap the user hit.
- **Reuse `Settings::adjust_opacity(delta)`** — it already clamps to `MIN_OPACITY..=1.0`. Do not write a second clamp rule for opacity.
- **Verify with `cargo build`, not the analyser.** rust-analyzer reported phantom errors repeatedly in this codebase on code rustc compiles cleanly.

---

## File Structure

| File | Responsibility |
|---|---|
| `crates/rt/src/prefs_model.rs` (create) | Which setting each row edits (`PrefRow`), and how one step mutates `Settings` with clamping/cycling. Pure: no UI, no backend, no X. |
| `crates/rt/src/chrome/prefs.rs` (create) | Turning `Settings` into display rows; panel/row/step-zone geometry; draw; hit-test. Mirrors `chrome/menu.rs`. |
| `crates/rt/src/chrome/mod.rs` (modify) | `pub mod prefs;` |
| `crates/rt/src/main.rs` (modify) | Dialog state, keyboard/mouse shim, `PREFS_SETTLE` commit, `commit_settings()` lifted from `paint_egui`, `RT_OPEN_PREFS` hook, deletion of the force-close hack and `paint_egui`. |
| `crates/rt/src/preferences.rs` (delete) | The egui dialog. |
| `crates/rt/tests/prefs_native.rs` (create) | Xvfb+xtrace gate: the dialog renders on XRender, `PutImage == 0`, and N steps commit once. |

---

### Task 1: `prefs_model.rs` — which setting a row edits, and how a step clamps

**Files:**
- Create: `crates/rt/src/prefs_model.rs`
- Modify: `crates/rt/src/main.rs` (add `mod prefs_model;` beside the other `mod` lines near the top, e.g. next to `mod preferences;`)

**Interfaces:**
- Consumes: `rt_config::{Settings, SCHEMES}`, `Settings::{MIN_OPACITY, MAX_SCROLLBACK, adjust_opacity}`.
- Produces (Tasks 2-4 rely on these exact names):
  - `pub enum PrefRow { FontSize, FontFamily, Opacity, Blur, Preset, Ffm, Titlebar, Scrollback, InstOutput, InstHeat, InstLatency, Jacks, InstRemote, InstAnimate, Close }` (derives `Clone, Copy, Debug, PartialEq, Eq`)
  - `pub fn step(s: &mut Settings, row: PrefRow, dir: i32, families: &[String])`
  - `pub fn enabled(s: &Settings, row: PrefRow) -> bool`
  - `pub fn preset_name(s: &Settings) -> &'static str`

- [ ] **Step 1: Write the failing tests**

Create `crates/rt/src/prefs_model.rs` containing ONLY this test module for now (the impl comes in Step 3):

```rust
//! Which setting each preferences row edits, and what one step does to it.
//!
//! Pure: no UI, no backend, no X server. Split out of `chrome::prefs` so the
//! clamping rules can be tested exhaustively without constructing a `Geom`.

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
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt --bins prefs_model 2>&1 | tail -20`
Expected: FAIL to compile — `cannot find function 'step' in this scope`, `cannot find type 'PrefRow'`.

- [ ] **Step 3: Write the implementation**

Insert ABOVE the `#[cfg(test)] mod tests` block in `crates/rt/src/prefs_model.rs`:

```rust
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
            let cur = families.iter().position(|f| *f == s.font_family).unwrap_or(0);
            let n = families.len() as i32;
            // rem_euclid keeps the index positive when stepping left off zero.
            let next = ((cur as i32 + dir).rem_euclid(n)) as usize;
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
            let n = rt_config::SCHEMES.len() as i32;
            // Start from the scheme we currently match; "custom" starts at 0 so
            // a first press lands on a real scheme rather than jumping about.
            let cur = rt_config::SCHEMES.iter().position(|c| c.name == preset_name(s)).unwrap_or(0);
            let next = ((cur as i32 + dir).rem_euclid(n)) as usize;
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
        PrefRow::Close => {} // handled by the caller; edits nothing
    }
}
```

Then add the module declaration in `crates/rt/src/main.rs`, immediately after the existing `mod preferences;` line:

```rust
mod prefs_model; // which setting each preferences row edits, and how a step clamps
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p rt --bins prefs_model 2>&1 | grep -E "test result|^test "`
Expected: PASS — 10 tests, `test result: ok. 10 passed`.

- [ ] **Step 5: Verify with rustc, not the analyser**

Run: `cargo build -p rt --release 2>&1 | tail -2`
Expected: `Finished` with zero warnings. (rust-analyzer has reported phantom errors on this codebase repeatedly; rustc is the source of truth.)

- [ ] **Step 6: Commit**

```bash
git add crates/rt/src/prefs_model.rs crates/rt/src/main.rs
git commit -m "feat(prefs): pure step/clamp semantics for the preferences rows

PrefRow names each editable setting; step() applies one discrete step with
clamping (font 8..48, scrollback x2/÷2 within 1000..MAX) or cycling (family,
preset). Opacity reuses Settings::adjust_opacity rather than a second clamp.

enabled() encodes the one real dependency: inst_animate is dead while
inst_remote is off, because the 6fps tick needs both -- the trap that made
inst_remote=true look like it did nothing."
```

---

### Task 2: `chrome/prefs.rs` — rows, geometry, hit-testing

**Files:**
- Create: `crates/rt/src/chrome/prefs.rs`
- Modify: `crates/rt/src/chrome/mod.rs` (add `pub mod prefs;` beside `pub mod menu;`)

**Interfaces:**
- Consumes: `crate::prefs_model::{PrefRow, enabled, preset_name}` (Task 1); `crate::chrome::Recti`; `rt_config::Settings`; `rt_engine::CELL_BYTES`.
- Produces (Tasks 3-4 rely on these exact names):
  - `pub enum RowKind { Section, Toggle, Step, Display, Swatches, Action }`
  - `pub struct Row { pub kind: RowKind, pub label: String, pub value: String, pub pref: Option<PrefRow>, pub enabled: bool }`
  - `pub struct Geom { pub panel: Recti, pub rows: Vec<Recti>, pub left: Vec<Option<Recti>>, pub right: Vec<Option<Recti>>, pub scroll: usize, pub visible: usize }`
  - `pub enum Hit { Row(usize), Step(usize, i32), Close }`
  - `pub fn rows(s: &Settings, mem_total: u64, cols: usize) -> Vec<Row>`
  - `pub fn selectable(rows: &[Row]) -> Vec<usize>`
  - `pub fn next_sel(rows: &[Row], sel: usize, dir: i32) -> usize`
  - `pub fn scroll_for(rows: &[Row], sel: usize, scroll: usize, visible: usize) -> usize`
  - `pub fn layout(rows: &[Row], scroll: usize, cell_w: f32, cell_h: f32, win_w: f32, win_h: f32) -> Geom`
  - `pub fn hit(g: &Geom, p: (f32, f32)) -> Option<Hit>`

- [ ] **Step 1: Write the failing tests**

Create `crates/rt/src/chrome/prefs.rs` containing ONLY this test module for now:

```rust
//! Native preferences dialog: rows built from `Settings`, laid out as rects,
//! drawn as fills + glyphs. Mirrors `chrome/menu.rs` — a pure `layout()` split
//! from `draw()` so the geometry is unit-testable with no X server.

#[cfg(test)]
mod tests {
    use super::*;
    use rt_config::Settings;

    fn fams() -> Vec<String> {
        vec!["DejaVu Sans Mono".to_string()]
    }
    fn rs(s: &Settings) -> Vec<Row> {
        rows(s, 16 * 1024 * 1024 * 1024, 80) // 16 GB of RAM, an 80-column pane
    }

    #[test]
    fn every_setting_row_carries_the_pref_it_edits() {
        let rows = rs(&Settings::default());
        let want = [
            PrefRow::FontSize, PrefRow::FontFamily, PrefRow::Opacity, PrefRow::Blur,
            PrefRow::Preset, PrefRow::Ffm, PrefRow::Titlebar, PrefRow::Scrollback,
            PrefRow::InstOutput, PrefRow::InstHeat, PrefRow::InstLatency, PrefRow::Jacks,
            PrefRow::InstRemote, PrefRow::InstAnimate, PrefRow::Close,
        ];
        let got: Vec<PrefRow> = rows.iter().filter_map(|r| r.pref).collect();
        assert_eq!(got, want, "every PrefRow appears once, in order");
    }

    #[test]
    fn headers_and_readouts_are_not_selectable() {
        let rows = rs(&Settings::default());
        for r in &rows {
            match r.kind {
                RowKind::Section | RowKind::Display | RowKind::Swatches => {
                    assert!(r.pref.is_none(), "{:?} must not be selectable", r.kind)
                }
                _ => assert!(r.pref.is_some(), "{} must map to a setting", r.label),
            }
        }
    }

    #[test]
    fn selection_skips_headers_and_wraps() {
        let rows = rs(&Settings::default());
        let sel = selectable(&rows);
        assert!(sel.len() >= 15);
        // Down from the last selectable wraps to the first.
        let last = *sel.last().unwrap();
        assert_eq!(next_sel(&rows, last, 1), sel[0], "wraps forward");
        assert_eq!(next_sel(&rows, sel[0], -1), last, "wraps backward");
        // Every hop lands on a selectable row, never a header.
        let mut at = sel[0];
        for _ in 0..sel.len() * 2 {
            at = next_sel(&rows, at, 1);
            assert!(rows[at].pref.is_some(), "landed on a non-selectable row");
        }
    }

    #[test]
    fn toggle_rows_render_a_checkbox_reflecting_the_setting() {
        let mut s = Settings::default();
        s.show_titlebar = true;
        s.focus_follows_mouse = false;
        let rows = rs(&s);
        let tb = rows.iter().find(|r| r.pref == Some(PrefRow::Titlebar)).unwrap();
        let ffm = rows.iter().find(|r| r.pref == Some(PrefRow::Ffm)).unwrap();
        assert_eq!(tb.value, "[x]");
        assert_eq!(ffm.value, "[ ]");
    }

    #[test]
    fn inst_animate_row_is_disabled_while_inst_remote_is_off() {
        let mut s = Settings::default();
        s.inst_remote = false;
        let rows = rs(&s);
        let anim = rows.iter().find(|r| r.pref == Some(PrefRow::InstAnimate)).unwrap();
        assert!(!anim.enabled, "must grey out: the 6fps tick needs inst_remote too");
        s.inst_remote = true;
        let rows = rs(&s);
        let anim = rows.iter().find(|r| r.pref == Some(PrefRow::InstAnimate)).unwrap();
        assert!(anim.enabled);
    }

    #[test]
    fn scrollback_readout_states_the_memory_cost_and_its_share_of_ram() {
        let mut s = Settings::default();
        s.scrollback = 1_000_000;
        // 16 GB of RAM, 80 columns.
        let rows = rows(&s, 16 * 1024 * 1024 * 1024, 80);
        let readout = rows.iter().find(|r| matches!(r.kind, RowKind::Display)).unwrap();
        assert!(readout.value.contains("per pane"), "got {:?}", readout.value);
        assert!(readout.value.contains('%'), "must state its share of RAM: {:?}", readout.value);
        assert!(readout.value.contains("80 cols"), "must state the width it assumed");
    }

    #[test]
    fn layout_keeps_the_panel_on_screen_and_rows_inside_it() {
        let rows = rs(&Settings::default());
        let g = layout(&rows, 0, 11.0, 21.0, 900.0, 700.0);
        assert!(g.panel.x >= 0.0 && g.panel.y >= 0.0);
        assert!(g.panel.x + g.panel.w <= 900.0);
        assert!(g.panel.y + g.panel.h <= 700.0);
        for r in g.rows.iter().take(g.visible) {
            assert!(r.x >= g.panel.x && r.x + r.w <= g.panel.x + g.panel.w + 0.01);
        }
    }

    #[test]
    fn a_selection_below_the_fold_scrolls_into_view() {
        let rows = rs(&Settings::default());
        // A window too short to hold every row.
        let g = layout(&rows, 0, 11.0, 21.0, 900.0, 200.0);
        assert!(g.visible < rows.len(), "this window must not fit them all");
        let last = *selectable(&rows).last().unwrap();
        let sc = scroll_for(&rows, last, 0, g.visible);
        assert!(sc > 0, "must scroll to reach the last row");
        assert!(last >= sc && last < sc + g.visible, "selection must be visible");
        // Selecting the first row scrolls back to the top.
        assert_eq!(scroll_for(&rows, selectable(&rows)[0], sc, g.visible), 0);
    }

    #[test]
    fn clicking_a_step_arrow_returns_the_direction() {
        let rows = rs(&Settings::default());
        let g = layout(&rows, 0, 11.0, 21.0, 900.0, 700.0);
        let i = rows.iter().position(|r| r.pref == Some(PrefRow::FontSize)).unwrap();
        let l = g.left[i].expect("a Step row has a left arrow");
        let r = g.right[i].expect("a Step row has a right arrow");
        assert!(matches!(hit(&g, (l.x + 1.0, l.y + 1.0)), Some(Hit::Step(n, -1)) if n == i));
        assert!(matches!(hit(&g, (r.x + 1.0, r.y + 1.0)), Some(Hit::Step(n, 1)) if n == i));
    }

    #[test]
    fn clicking_a_row_selects_it_and_clicking_outside_hits_nothing() {
        let rows = rs(&Settings::default());
        let g = layout(&rows, 0, 11.0, 21.0, 900.0, 700.0);
        let i = rows.iter().position(|r| r.pref == Some(PrefRow::Ffm)).unwrap();
        let row = g.rows[i];
        // A click on the label (left of the arrows) selects the row.
        assert!(matches!(hit(&g, (row.x + 2.0, row.y + 2.0)), Some(Hit::Row(n)) if n == i));
        assert!(hit(&g, (g.panel.x - 5.0, g.panel.y - 5.0)).is_none(), "outside the panel");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt --bins chrome::prefs 2>&1 | tail -20`
Expected: FAIL to compile — `cannot find function 'rows'`, `cannot find type 'Row'`.

- [ ] **Step 3: Write the implementation**

Insert ABOVE the `#[cfg(test)] mod tests` block in `crates/rt/src/chrome/prefs.rs`:

```rust
use crate::chrome::Recti;
use crate::prefs_model::{enabled, preset_name, PrefRow};
use rt_config::Settings;

// NOTE: line counts use the EXISTING `crate::fmt_lines` (main.rs), which already
// renders the titlebar's "buf 100k/100k" meter. Do not write a second one: two
// copies of a formatting rule drift, and then the dialog and the titlebar
// disagree about what "10k" means. (Private at the crate root is still visible
// to descendant modules via `crate::`.)

/// What a row is. Only `Toggle`, `Step` and `Action` are selectable.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RowKind {
    Section,  // a heading ("Font")
    Toggle,   // "[x]" / "[ ]"
    Step,     // "◄ value ►"
    Display,  // a readout (the scrollback memory guardrail)
    Swatches, // the colour preview; `draw` paints rects, not text
    Action,   // "Close"
}

/// One line of the dialog, already rendered to strings. This module draws text
/// and rects; it never reads `Settings` after `rows()` has run.
pub struct Row {
    pub kind: RowKind,
    pub label: String,
    pub value: String,
    /// Which setting this row edits — `Some` exactly for selectable rows.
    /// Carried IN the row (rather than a parallel vec, as `chrome/menu.rs` does
    /// with `clickable`) so the two cannot fall out of step.
    pub pref: Option<PrefRow>,
    /// Live? A disabled row draws dimmed and refuses steps (see
    /// `prefs_model::enabled`).
    pub enabled: bool,
}

/// Panel and per-row geometry in window px.
pub struct Geom {
    pub panel: Recti,
    /// One rect per row in `rows` (indices line up 1:1), positioned for the
    /// current `scroll`. Rows outside the visible window get an off-panel rect
    /// that `hit` rejects.
    pub rows: Vec<Recti>,
    /// The `◄` zone per row — `Some` only for enabled `Step` rows.
    pub left: Vec<Option<Recti>>,
    /// The `►` zone per row.
    pub right: Vec<Option<Recti>>,
    pub scroll: usize,
    /// How many rows fit in the panel.
    pub visible: usize,
}

/// What a click landed on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Hit {
    Row(usize),
    Step(usize, i32),
    Close,
}

const PAD_X: f32 = 10.0; // inner horizontal padding
const ARROW_W: f32 = 2.0; // "◄ " / " ►" width, in cells
const VALUE_COLS: usize = 22; // room for the widest value ("DejaVu Sans Mono")
const LABEL_COLS: usize = 26; // room for the widest label

fn sec(label: &str) -> Row {
    Row { kind: RowKind::Section, label: label.into(), value: String::new(), pref: None, enabled: true }
}
fn toggle(label: &str, on: bool, pref: PrefRow, enabled: bool) -> Row {
    Row {
        kind: RowKind::Toggle,
        label: label.into(),
        value: if on { "[x]".into() } else { "[ ]".into() },
        pref: Some(pref),
        enabled,
    }
}
fn stepper(label: &str, value: String, pref: PrefRow) -> Row {
    Row { kind: RowKind::Step, label: label.into(), value, pref: Some(pref), enabled: true }
}

/// Build every row from `settings`. The ONLY place settings become display text.
///
/// `mem_total` is the machine's RAM in bytes and `cols` the focused pane's
/// width — both feed the scrollback guardrail, which is the one piece of real
/// logic here: it states what a FULL buffer would cost per pane, so sliding the
/// ceiling up cannot silently pick a size no machine can hold.
pub fn rows(s: &Settings, mem_total: u64, cols: usize) -> Vec<Row> {
    // No `families` param: the family VALUE shown is `s.font_family`. Only
    // `prefs_model::step` needs the installed list, to cycle through it.
    let mut v = Vec::new();

    v.push(sec("Font"));
    v.push(stepper("Size (px)", format!("{:.0}", s.font_size), PrefRow::FontSize));
    v.push(stepper("Family", s.font_family.clone(), PrefRow::FontFamily));

    v.push(sec("Appearance"));
    v.push(stepper("Background opacity", format!("{:.2}", s.background_opacity), PrefRow::Opacity));
    v.push(toggle("Background blur", s.background_blur, PrefRow::Blur, true));

    v.push(sec("Colours"));
    v.push(stepper("Preset", preset_name(s).to_string(), PrefRow::Preset));
    v.push(Row {
        kind: RowKind::Swatches,
        label: "Palette".into(),
        value: String::new(),
        pref: None,
        enabled: true,
    });

    v.push(sec("Behaviour"));
    v.push(toggle("Focus follows mouse", s.focus_follows_mouse, PrefRow::Ffm, true));
    v.push(toggle("Show per-pane titlebars", s.show_titlebar, PrefRow::Titlebar, true));
    v.push(stepper("Scrollback (lines)", crate::fmt_lines(s.scrollback), PrefRow::Scrollback));
    // The guardrail, carried over from the egui dialog intact.
    let per_line = cols.max(1) as u64 * rt_engine::CELL_BYTES as u64 + 32; // + row overhead
    let full = (s.scrollback as u64).saturating_mul(per_line);
    let (val, unit) = if full >= 1_000_000_000 { (full as f64 / 1e9, "GB") } else { (full as f64 / 1e6, "MB") };
    let frac = if mem_total > 0 { full as f64 / mem_total as f64 } else { 0.0 };
    v.push(Row {
        kind: RowKind::Display,
        label: String::new(),
        value: if mem_total > 0 {
            format!("≈ {val:.1} {unit} per pane if full — {:.0}% of RAM, at {cols} cols", frac * 100.0)
        } else {
            format!("≈ {val:.1} {unit} per pane if full, at {cols} cols")
        },
        pref: None,
        enabled: true,
    });

    v.push(sec("Border instruments"));
    v.push(toggle("Output activity", s.inst_output, PrefRow::InstOutput, true));
    v.push(toggle("CPU heat", s.inst_heat, PrefRow::InstHeat, true));
    v.push(toggle("Latency", s.inst_latency, PrefRow::InstLatency, true));
    v.push(toggle("Patch-bay jacks", s.show_jacks, PrefRow::Jacks, true));
    v.push(toggle("Show over ssh -X", s.inst_remote, PrefRow::InstRemote, true));
    v.push(toggle("Animate at 6fps", s.inst_animate, PrefRow::InstAnimate, enabled(s, PrefRow::InstAnimate)));

    v.push(Row {
        kind: RowKind::Action,
        label: "Close".into(),
        value: "(Esc)".into(),
        pref: Some(PrefRow::Close),
        enabled: true,
    });
    v
}

/// Indices of the rows the selection may land on.
pub fn selectable(rows: &[Row]) -> Vec<usize> {
    rows.iter().enumerate().filter(|(_, r)| r.pref.is_some()).map(|(i, _)| i).collect()
}

/// Move the selection `dir` steps, skipping headers/readouts and wrapping.
pub fn next_sel(rows: &[Row], sel: usize, dir: i32) -> usize {
    let sels = selectable(rows);
    if sels.is_empty() {
        return sel;
    }
    let at = sels.iter().position(|i| *i == sel).unwrap_or(0) as i32;
    let n = sels.len() as i32;
    sels[((at + dir).rem_euclid(n)) as usize]
}

/// The scroll offset that keeps `sel` visible, moving as little as possible.
pub fn scroll_for(rows: &[Row], sel: usize, scroll: usize, visible: usize) -> usize {
    if visible == 0 || rows.len() <= visible {
        return 0;
    }
    let max = rows.len() - visible;
    if sel < scroll {
        sel // scrolled off the top: bring it to the top edge
    } else if sel >= scroll + visible {
        (sel + 1 - visible).min(max) // off the bottom: bring it to the bottom edge
    } else {
        scroll.min(max)
    }
}

/// Lay the dialog out centred, clamped fully on-screen.
pub fn layout(rows: &[Row], scroll: usize, cell_w: f32, cell_h: f32, win_w: f32, win_h: f32) -> Geom {
    let row_h = cell_h + 4.0;
    let w = ((LABEL_COLS + VALUE_COLS) as f32 + ARROW_W * 2.0) * cell_w + PAD_X * 2.0;
    let w = w.min(win_w); // never wider than the window
    // How many rows fit, leaving the padding at top and bottom.
    let visible = (((win_h - PAD_X * 2.0) / row_h).floor() as usize).clamp(1, rows.len());
    let h = visible as f32 * row_h + PAD_X * 2.0;
    let x = ((win_w - w) * 0.5).max(0.0);
    let y = ((win_h - h) * 0.5).max(0.0);
    let scroll = scroll.min(rows.len().saturating_sub(visible));

    let mut rrects = Vec::with_capacity(rows.len());
    let mut left = Vec::with_capacity(rows.len());
    let mut right = Vec::with_capacity(rows.len());
    for (i, r) in rows.iter().enumerate() {
        // Rows outside the scroll window get an off-panel rect: indices stay 1:1
        // with `rows` (as menu.rs does) and `hit` rejects them by position.
        if i < scroll || i >= scroll + visible {
            rrects.push(Recti { x: -1.0, y: -1.0, w: 0.0, h: 0.0 });
            left.push(None);
            right.push(None);
            continue;
        }
        let ry = y + PAD_X + (i - scroll) as f32 * row_h;
        rrects.push(Recti { x, y: ry, w, h: row_h });
        // Arrow zones sit at the right edge, either side of the value.
        if matches!(r.kind, RowKind::Step) && r.enabled {
            let aw = ARROW_W * cell_w;
            let vx = x + w - PAD_X - VALUE_COLS as f32 * cell_w;
            left.push(Some(Recti { x: vx - aw, y: ry, w: aw, h: row_h }));
            right.push(Some(Recti { x: x + w - PAD_X - aw, y: ry, w: aw, h: row_h }));
        } else {
            left.push(None);
            right.push(None);
        }
    }
    Geom { panel: Recti { x, y, w, h }, rows: rrects, left, right, scroll, visible }
}

/// What is under `p`. Arrows win over the row they sit in.
pub fn hit(g: &Geom, p: (f32, f32)) -> Option<Hit> {
    if !g.panel.contains(p) {
        return None;
    }
    for i in 0..g.rows.len() {
        if let Some(l) = g.left[i] {
            if l.contains(p) {
                return Some(Hit::Step(i, -1));
            }
        }
        if let Some(r) = g.right[i] {
            if r.contains(p) {
                return Some(Hit::Step(i, 1));
            }
        }
    }
    for (i, r) in g.rows.iter().enumerate() {
        if r.contains(p) {
            return Some(Hit::Row(i));
        }
    }
    None
}
```

Then add to `crates/rt/src/chrome/mod.rs`, beside the other `pub mod` lines:

```rust
pub mod prefs;
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p rt --bins chrome::prefs 2>&1 | grep -E "test result|^test "`
Expected: PASS — 10 tests, `test result: ok`.

- [ ] **Step 5: Verify the whole crate still builds clean**

Run: `cargo build -p rt --release 2>&1 | tail -2`
Expected: `Finished`, zero warnings.

- [ ] **Step 6: Commit**

```bash
git add crates/rt/src/chrome/prefs.rs crates/rt/src/chrome/mod.rs
git commit -m "feat(prefs): native preferences rows, geometry and hit-testing

rows() is the only place Settings becomes display text (including the
scrollback memory guardrail, carried over from the egui dialog intact).
layout() is pure and unit-tested with no X server, as chrome/menu.rs does.

Row carries the PrefRow it edits rather than a parallel vec, so selection and
setting cannot fall out of step -- the drift that produced the jack cursor
disagreeing with the press path."
```

---

### Task 3: `chrome/prefs.rs` — draw

**Files:**
- Modify: `crates/rt/src/chrome/prefs.rs` (add `draw`)

**Interfaces:**
- Consumes: `Geom`, `Row`, `RowKind` (Task 2); `crate::backend::Backend`; `crate::render::Color`.
- Produces: `pub fn draw(be: &mut dyn Backend, g: &Geom, rows: &[Row], sel: usize, swatches: &[Color], cell_w: f32, cell_h: f32)`

There is no unit test for `draw` — it needs a live `Backend`. Task 5's Xvfb gate is what proves it renders; that is the same division `chrome/menu.rs` uses.

- [ ] **Step 1: Write the implementation**

Add to `crates/rt/src/chrome/prefs.rs`, after `hit`:

```rust
use crate::backend::Backend;
use crate::render::Color;

const PANEL_BG: Color = Color(0.10, 0.10, 0.12, 0.97);
const PANEL_EDGE: Color = Color(0.35, 0.35, 0.42, 1.0);
const SEL_BG: Color = Color(0.18, 0.20, 0.28, 1.0);
const TEXT: Color = Color(0.82, 0.82, 0.86, 1.0);
const TEXT_DIM: Color = Color(0.45, 0.45, 0.50, 1.0);
const SECTION: Color = Color(0.55, 0.72, 0.90, 1.0);

/// Paint the dialog. `swatches` is `[fg, bg, palette…]`, painted into the
/// `Swatches` row; `sel` is the selected row index.
pub fn draw(be: &mut dyn Backend, g: &Geom, rows: &[Row], sel: usize, swatches: &[Color], cell_w: f32, cell_h: f32) {
    // Panel: a 1px edge drawn as four thin fills around an opaque body.
    let p = g.panel;
    be.fill_rect(p.x, p.y, p.w, p.h, PANEL_BG);
    be.fill_rect(p.x, p.y, p.w, 1.0, PANEL_EDGE);
    be.fill_rect(p.x, p.y + p.h - 1.0, p.w, 1.0, PANEL_EDGE);
    be.fill_rect(p.x, p.y, 1.0, p.h, PANEL_EDGE);
    be.fill_rect(p.x + p.w - 1.0, p.y, 1.0, p.h, PANEL_EDGE);

    for (i, row) in rows.iter().enumerate() {
        // Skip rows scrolled out of view (layout parked them off-panel).
        if i < g.scroll || i >= g.scroll + g.visible {
            continue;
        }
        let r = g.rows[i];
        if i == sel {
            be.fill_rect(r.x + 1.0, r.y, r.w - 2.0, r.h, SEL_BG);
        }
        let ty = r.y + 2.0;
        let colour = if !row.enabled {
            TEXT_DIM
        } else {
            match row.kind {
                RowKind::Section => SECTION,
                RowKind::Display => TEXT_DIM,
                _ => TEXT,
            }
        };
        // Label: sections sit flush, everything else indents one cell.
        let lx = if matches!(row.kind, RowKind::Section) { r.x + PAD_X } else { r.x + PAD_X + cell_w };
        for (c, ch) in row.label.chars().enumerate() {
            be.draw_char(lx, ty, c, 0, ch, colour, matches!(row.kind, RowKind::Section), false);
        }
        // A Display row's text IS its value, and it can be long: start at the
        // label column rather than the value column so it is not clipped.
        if matches!(row.kind, RowKind::Display) {
            for (c, ch) in row.value.chars().enumerate() {
                be.draw_char(lx, ty, c, 0, ch, colour, false, false);
            }
            continue;
        }
        // Swatches: fg, bg, then the 16 palette colours, as small squares.
        if matches!(row.kind, RowKind::Swatches) {
            let s = cell_h * 0.6;
            let mut sx = r.x + r.w - PAD_X - swatches.len() as f32 * (s + 2.0);
            for c in swatches {
                be.fill_rect(sx, r.y + (r.h - s) * 0.5, s, s, *c);
                sx += s + 2.0;
            }
            continue;
        }
        // Value, right-aligned in the value column.
        let vx = r.x + r.w - PAD_X - VALUE_COLS as f32 * cell_w;
        for (c, ch) in row.value.chars().enumerate() {
            be.draw_char(vx, ty, c, 0, ch, colour, false, false);
        }
        // Arrows, only where a step is possible.
        if let (Some(l), Some(rt)) = (g.left[i], g.right[i]) {
            be.draw_char(l.x, ty, 0, 0, '◄', colour, false, false);
            be.draw_char(rt.x, ty, 0, 0, '►', colour, false, false);
        }
    }
}
```

- [ ] **Step 2: Verify it builds**

Run: `cargo build -p rt --release 2>&1 | tail -2`
Expected: `Finished`, zero warnings.

- [ ] **Step 3: Run the existing unit tests (draw must not have broken layout)**

Run: `cargo test -p rt --bins 2>&1 | grep "test result"`
Expected: PASS, all of them.

- [ ] **Step 4: Commit**

```bash
git add crates/rt/src/chrome/prefs.rs
git commit -m "feat(prefs): draw the native preferences dialog

Fills + glyphs through Backend, as the sibling chrome modules do. Disabled rows
draw dimmed; the Swatches row paints colours instead of text. No unit test --
it needs a live Backend; Task 5's Xvfb gate proves it renders."
```

---

### Task 4: `main.rs` — state, input, commit-on-settle; delete the egui dialog

**Files:**
- Modify: `crates/rt/src/main.rs`
- Delete: `crates/rt/src/preferences.rs`

**Interfaces:**
- Consumes: `chrome::prefs::{rows, layout, hit, next_sel, scroll_for, Hit, RowKind}` (Tasks 2-3); `prefs_model::{step, PrefRow}` (Task 1).
- Produces: `RT_OPEN_PREFS=1` startup hook (Task 5 depends on it); the log line `prefs settled: … N edit(s) coalesced into 1 commit` (Task 5 asserts on it).

- [ ] **Step 1: Add the dialog's state**

In `struct Active` (near `prefs_open: bool`), add:

```rust
    prefs_sel: usize,                     // selected row index into chrome::prefs::rows()
    prefs_scroll: usize,                  // first visible row (the panel scrolls when it doesn't fit)
    prefs_pending: Option<rt_config::Settings>, // edits not yet committed (see PREFS_SETTLE)
    prefs_edits: u64,                     // edits folded into the pending commit (diagnostics)
    last_prefs_edit: Instant,             // when the last edit landed
```

In the `Active { … }` initialiser (beside `prefs_open: false`):

```rust
            prefs_sel: 0,
            prefs_scroll: 0,
            prefs_pending: None,
            prefs_edits: 0,
            last_prefs_edit: Instant::now(),
```

- [ ] **Step 2: Add the settle constant**

Beside `RESIZE_SETTLE`:

```rust
/// How long a preferences value must hold still before it is applied+persisted.
///
/// Applying is expensive — a font change re-rasterises every glyph and reflows
/// every pane (~700ms on a milkv) — and `persist()` writes `config.toml`. So
/// stepping 18 → 24 must cost ONE of each, not six. Long enough to swallow a run
/// of key-repeat presses (~30ms apart), short enough that a single toggle feels
/// immediate. Same rule as RESIZE_SETTLE, same reason: never pay for an
/// intermediate value nobody keeps.
const PREFS_SETTLE: Duration = Duration::from_millis(150);
```

- [ ] **Step 3: Lift the diff-and-apply block out of `paint_egui` into `commit_settings`**

`paint_egui` currently clones `active.settings`, runs `preferences::ui`, then diffs and applies. Move everything from `if settings != active.settings {` to the end of that block into a new method, unchanged apart from its signature:

```rust
    /// Apply and persist `new`, doing only the work each change actually needs.
    ///
    /// Lifted verbatim out of `paint_egui`: it was always backend-agnostic, and
    /// deleting the egui dialog leaves it one caller. Called ONCE per settle
    /// (see PREFS_SETTLE), never per keystroke.
    fn commit_settings(active: &mut Active, new: rt_config::Settings) {
        if new == active.settings {
            return; // nothing to do
        }
        // …the existing body from paint_egui, verbatim: colours_changed /
        // family_changed / fonts_changed / titlebar_changed / blur_changed,
        // then `active.settings = new; Self::persist(&active.settings);` and the
        // per-change applies (scrollback.set, apply_blur, set_show_titlebar,
        // palette rebuild, font re-rasterisation)…
        active.force_full = true; // chrome + metrics changed: repaint the lot
    }
```

- [ ] **Step 4: Delete the egui dialog**

- Delete the file: `crates/rt/src/preferences.rs`
- Remove `mod preferences;` from `main.rs`
- Delete the whole `fn paint_egui` method (its diff-and-apply body now lives in `commit_settings`; its egui UI call is gone)
- Remove the `paint_egui` call from `paint_overlays_or_instruments` (the `if active.prefs_open { Self::paint_egui(active); }` arm) and replace it with the native draw:

```rust
            if active.prefs_open {
                Self::paint_prefs(active);
            } else if active.menu.is_some() {
```

- Delete the force-close hack — the dialog now exists on this backend:

```rust
        // DELETE this block:
        if !active.backend.supports_egui() && active.prefs_open {
            active.prefs_open = false;
            …
        }
```

- [ ] **Step 5: Add the native paint**

```rust
    /// Draw the preferences dialog from the PENDING settings, so the value you
    /// just stepped is on screen immediately — the terminal behind it changes
    /// once, on settle.
    fn paint_prefs(active: &mut Active) {
        let size = active.window.inner_size();
        let (cw, ch) = active.backend.cell_size();
        let s = active.prefs_pending.clone().unwrap_or_else(|| active.settings.clone());
        // Exactly how paint_egui derived it: a full-width pane at the current
        // font size. There is no `pane.cols()`.
        let cols = (content_bounds(size).w / cw).max(1.0) as usize;
        let rows = chrome::prefs::rows(&s, total_ram_bytes(), cols);
        let g = chrome::prefs::layout(&rows, active.prefs_scroll, cw, ch, size.width as f32, size.height as f32);
        let mut sw = vec![Color::rgb(s.foreground[0], s.foreground[1], s.foreground[2])];
        sw.push(Color::rgb(s.background[0], s.background[1], s.background[2]));
        sw.extend(s.palette.iter().map(|c| Color::rgb(c[0], c[1], c[2])));
        chrome::prefs::draw(&mut *active.backend, &g, &rows, active.prefs_sel, &sw, cw, ch);
    }
```

Both helpers already exist and must be reused, not reinvented: `total_ram_bytes()` (reads `MemTotal` from `/proc/meminfo`; deliberately excludes swap, since spilling a terminal buffer to swap is already a failure) and `content_bounds(size)`. `paint_egui` computed its two arguments exactly this way:

```rust
let cols = (content_bounds(active.window.inner_size()).w / active.backend.cell_size().0).max(1.0) as usize;
let ram = total_ram_bytes();
```

- [ ] **Step 6: Add the keyboard shim**

Beside the manual's shim (`if active.manual_open { … }`), add — BEFORE it, so prefs wins when both are somehow set:

```rust
        // Preferences: keys drive the dialog and never reach the PTY. Every edit
        // is one discrete step (no drags — see PREFS_SETTLE and the design doc).
        if active.prefs_open {
            match &event {
                WindowEvent::KeyboardInput { event: ke, .. } if ke.state == ElementState::Pressed => {
                    let size = active.window.inner_size();
                    let (cw, ch) = active.backend.cell_size();
                    let s = active.prefs_pending.clone().unwrap_or_else(|| active.settings.clone());
                    let cols = (content_bounds(size).w / cw).max(1.0) as usize;
                    let rws = chrome::prefs::rows(&s, total_ram_bytes(), cols);
                    let g = chrome::prefs::layout(&rws, active.prefs_scroll, cw, ch, size.width as f32, size.height as f32);
                    match &ke.logical_key {
                        Key::Named(NamedKey::Escape) => active.prefs_open = false,
                        Key::Named(NamedKey::ArrowDown) => {
                            active.prefs_sel = chrome::prefs::next_sel(&rws, active.prefs_sel, 1);
                            active.prefs_scroll = chrome::prefs::scroll_for(&rws, active.prefs_sel, active.prefs_scroll, g.visible);
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            active.prefs_sel = chrome::prefs::next_sel(&rws, active.prefs_sel, -1);
                            active.prefs_scroll = chrome::prefs::scroll_for(&rws, active.prefs_sel, active.prefs_scroll, g.visible);
                        }
                        Key::Named(NamedKey::ArrowLeft) => Self::prefs_step(active, &rws, -1),
                        Key::Named(NamedKey::ArrowRight) => Self::prefs_step(active, &rws, 1),
                        Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Space) => {
                            if rws.get(active.prefs_sel).and_then(|r| r.pref) == Some(prefs_model::PrefRow::Close) {
                                active.prefs_open = false;
                            } else {
                                Self::prefs_step(active, &rws, 1);
                            }
                        }
                        _ => {}
                    }
                    active.window.request_redraw();
                    return;
                }
                // Swallow all other input so it cannot reach the PTY.
                WindowEvent::KeyboardInput { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::Ime(_)
                | WindowEvent::ModifiersChanged(_) => return,
                _ => {} // Close / resize / redraw fall through
            }
        }
```

And the step helper, beside `paint_prefs`:

```rust
    /// Apply one step to the selected row: mutate the PENDING settings and arm
    /// the settle. Never applies or persists — that is `commit_settings`, once,
    /// after PREFS_SETTLE.
    fn prefs_step(active: &mut Active, rows: &[chrome::prefs::Row], dir: i32) {
        let Some(pref) = rows.get(active.prefs_sel).and_then(|r| r.pref) else { return };
        if pref == prefs_model::PrefRow::Close {
            return;
        }
        let mut s = active.prefs_pending.clone().unwrap_or_else(|| active.settings.clone());
        prefs_model::step(&mut s, pref, dir, &active.mono_families);
        active.prefs_pending = Some(s);
        active.prefs_edits += 1;
        active.last_prefs_edit = Instant::now();
    }
```

- [ ] **Step 7: Add the settle to `about_to_wait`**

Immediately after the `resize_pending` settle block:

```rust
        // Preferences: one commit per run of edits, not one per keystroke.
        if active.prefs_pending.is_some() && now.duration_since(active.last_prefs_edit) >= PREFS_SETTLE {
            let new = active.prefs_pending.take().unwrap();
            let n = std::mem::take(&mut active.prefs_edits);
            let t0 = Instant::now();
            Self::commit_settings(active, new);
            log::info!(
                "prefs settled: commit={:.1}ms, {} edit(s) coalesced into 1 commit",
                t0.elapsed().as_secs_f32() * 1e3, n,
            );
            dirty = true;
        }
```

And so a pending commit cannot wait on `IDLE_POLL`, add to the interval selection beside `resize_pending`:

```rust
        } else if active.prefs_pending.is_some() {
            PREFS_SETTLE // a commit is owed; wake to pay it
```

- [ ] **Step 8: Close must commit immediately**

A user who steps a value then presses Esc within 150ms must not lose the edit. In the `Escape` arm above, replace `active.prefs_open = false;` with:

```rust
                        Key::Named(NamedKey::Escape) => {
                            active.prefs_open = false;
                            // Do not strand a pending edit: commit it now rather
                            // than wait out PREFS_SETTLE on a closed dialog.
                            if let Some(new) = active.prefs_pending.take() {
                                active.prefs_edits = 0;
                                Self::commit_settings(active, new);
                            }
                        }
```

Do the same in the `Close` action arm.

- [ ] **Step 9: Add the mouse shim**

In the same `if active.prefs_open` block, before the swallow arm:

```rust
                WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                    let size = active.window.inner_size();
                    let (cw, ch) = active.backend.cell_size();
                    let s = active.prefs_pending.clone().unwrap_or_else(|| active.settings.clone());
                    let cols = (content_bounds(size).w / cw).max(1.0) as usize;
                    let rws = chrome::prefs::rows(&s, total_ram_bytes(), cols);
                    let g = chrome::prefs::layout(&rws, active.prefs_scroll, cw, ch, size.width as f32, size.height as f32);
                    match chrome::prefs::hit(&g, active.mouse) {
                        Some(chrome::prefs::Hit::Step(i, dir)) => {
                            active.prefs_sel = i;
                            Self::prefs_step(active, &rws, dir);
                        }
                        Some(chrome::prefs::Hit::Row(i)) => {
                            if rws[i].pref.is_some() {
                                active.prefs_sel = i;
                                // A click on a Toggle toggles it; on a Step row it only selects.
                                if matches!(rws[i].kind, chrome::prefs::RowKind::Toggle) {
                                    Self::prefs_step(active, &rws, 1);
                                } else if rws[i].pref == Some(prefs_model::PrefRow::Close) {
                                    active.prefs_open = false;
                                    if let Some(new) = active.prefs_pending.take() {
                                        active.prefs_edits = 0;
                                        Self::commit_settings(active, new);
                                    }
                                }
                            }
                        }
                        _ => {}
                    }
                    active.window.request_redraw();
                    return;
                }
```

- [ ] **Step 10: Add the `RT_OPEN_PREFS` startup hook**

Beside the existing `RT_OPEN_MANUAL` hook:

```rust
        // Debug/verification hook: open preferences at startup so the Xvfb gate
        // can screenshot it without synthetic input (mirrors RT_OPEN_MANUAL).
        if std::env::var_os("RT_OPEN_PREFS").is_some() {
            if let Some(active) = self.active.as_mut() {
                active.prefs_open = true;
            }
        }
```

- [ ] **Step 11: Build and run every test**

Run: `cargo build -p rt --release 2>&1 | tail -2 && cargo test -p rt --bins 2>&1 | grep "test result"`
Expected: `Finished` with zero warnings; all unit tests pass.

- [ ] **Step 12: Commit**

```bash
git add -A crates/rt/src
git commit -m "feat(prefs): wire the native dialog; delete the egui one

The XRender backend force-closed the egui dialog (an invisible dialog would
swallow all input), so the milkv had no preferences at all -- including
inst_remote/inst_animate, which the egui dialog never exposed anywhere.

Keys drive the dialog and never reach the PTY. Every edit mutates PENDING
settings and arms PREFS_SETTLE; commit_settings (lifted verbatim out of
paint_egui, always backend-agnostic) applies and persists ONCE per run of edits.
Esc/Close commit immediately so a fast Esc cannot strand an edit.

preferences.rs deleted: one dialog on both backends, because two that must agree
forever is the drift this session paid for twice."
```

---

### Task 5: Gates — it renders where it could not, and N steps commit once

**Files:**
- Create: `crates/rt/tests/prefs_native.rs`

**Interfaces:**
- Consumes: `common::{start_xvfb_scan, stop_xvfb, release_display_name, wait_for_trace, x_test_lock, free_display_name, have}`; the `RT_OPEN_PREFS=1` hook and the `prefs settled:` log line (Task 4).

**Harness rules (learned the hard way this session — violate these and the test lies):**
- Take `x_test_lock()` first: these spawn a real Xvfb + rt on llvmpipe, and running them concurrently pushes cold start past the window.
- Never `Path::exists()` a socket to decide a display is free, and always tear down through `stop_xvfb`/`release_display_name` — a leaked name poisons that display for every later run.
- Never bound rt with a fixed timeout: wait for the CONDITION (`wait_for_trace` for `CompositeGlyphs`). Cold start is ~1.5s idle but ~3.5s in a debug build under load.
- Assert something POSITIVE rendered. `PutImage == 0` passes vacuously on a run that drew nothing.

- [ ] **Step 1: Write the failing test**

Create `crates/rt/tests/prefs_native.rs`:

```rust
//! The native preferences dialog on the XRender backend.
//!
//! This is the feature made falsifiable: on this backend the dialog previously
//! could not open AT ALL (`main.rs` force-closed it, because an invisible egui
//! dialog would swallow every keystroke). So the gate is simply: it renders, as
//! commands, and a run of edits commits once.
//!
//! Needs `Xvfb` + `xtrace` on PATH. Run explicitly:
//!   cargo test -p rt --test prefs_native -- --ignored --nocapture

#![cfg(feature = "x11")]

use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;
use common::{free_display_name, have, release_display_name, start_xvfb_scan, stop_xvfb, wait_for_trace, x_test_lock};

/// A private config so the run does not depend on the host's ~/.config/rt.
fn write_config(tag: &str) -> PathBuf {
    let mut base = std::env::temp_dir();
    base.push(format!("rt_prefs_xdg_{tag}_{}", std::process::id()));
    let rt_dir = base.join("rt");
    std::fs::create_dir_all(&rt_dir).expect("create private XDG_CONFIG_HOME/rt");
    std::fs::write(rt_dir.join("config.toml"), "[settings]\ninst_remote = false\nfont_size = 18.0\n")
        .expect("write config.toml");
    base
}

fn write_shell(tag: &str) -> PathBuf {
    let mut shell = std::env::temp_dir();
    shell.push(format!("rt_prefs_shell_{tag}_{}.sh", std::process::id()));
    let mut f = std::fs::File::create(&shell).expect("create temp shell");
    f.write_all(b"#!/bin/sh\nprintf 'hello\\n'; sleep 120\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    shell
}

#[test]
#[ignore = "needs Xvfb + xtrace; run with --ignored"]
fn prefs_dialog_renders_on_xrender_as_commands() {
    let _serial = x_test_lock(); // Xvfb+rt is heavy: never run these concurrently
    if !have("Xvfb") || !have("xtrace") {
        eprintln!("SKIP prefs_native: needs both `Xvfb` and `xtrace` on PATH");
        return;
    }
    let Some((disp, xvfb)) = start_xvfb_scan(410) else {
        panic!("no Xvfb came up at or after :410");
    };
    let shell = write_shell("render");
    let cfg = write_config("render");
    let trace = std::env::temp_dir().join(format!("rt_prefs_trace_{}.txt", std::process::id()));
    let fake = free_display_name(disp + 1).expect("no free proxy display");
    let rt_bin = env!("CARGO_BIN_EXE_rt");

    let mut child = Command::new("xtrace")
        .arg("-n")
        .args(["-d", &format!(":{disp}")])
        .args(["-D", &format!(":{fake}")])
        .args(["-o", trace.to_str().unwrap(), "--"])
        .arg("timeout")
        .arg("60") // backstop only; we stop it ourselves
        .arg(rt_bin)
        .env_remove("WAYLAND_DISPLAY")
        .env("RT_BACKEND", "xrender") // the backend that had NO preferences at all
        .env("RT_OPEN_PREFS", "1") // test-only: open the dialog at startup
        .env("XDG_CONFIG_HOME", &cfg)
        .env("SHELL", &shell)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn xtrace");

    // Wait for a real paint; never a fixed sleep (cold start is ~3.5s under load).
    let drew = wait_for_trace(&trace, "CompositeGlyphs", Duration::from_secs(30)).is_some();
    if drew {
        std::thread::sleep(Duration::from_millis(400)); // let the frame settle
    }
    let _ = child.kill();
    let _ = child.wait();
    release_display_name(fake);
    stop_xvfb(xvfb, disp);
    let _ = std::fs::remove_file(&shell);
    let _ = std::fs::remove_dir_all(&cfg);

    let dump = std::fs::read_to_string(&trace).unwrap_or_default();
    let _ = std::fs::remove_file(&trace);
    assert!(!dump.is_empty(), "xtrace produced no output — did rt connect to :{disp}?");
    assert!(drew, "rt never rendered text within 30s — a zero-count trace satisfies PutImage==0 vacuously");

    let glyphs = dump.matches("CompositeGlyphs").count();
    let fills = dump.matches("FillRectangles").count();
    let put_image = dump.matches("PutImage").count();
    eprintln!("prefs wire profile: CompositeGlyphs={glyphs} FillRectangles={fills} PutImage={put_image}");

    // The dialog is text + rects. It draws MANY more of both than a bare
    // terminal: ~20 rows of label+value, a panel, a selection bar, swatches.
    assert!(glyphs > 30, "expected the dialog's rows as glyph commands, got {glyphs}");
    assert!(fills > 10, "expected the panel/selection/swatches as fill commands, got {fills}");
    // Mechanism C's invariant still holds for this new surface.
    assert_eq!(put_image, 0, "the dialog must ship commands, not pixels");
}

#[test]
#[ignore = "needs Xvfb + xtrace; run with --ignored"]
fn a_run_of_steps_commits_once() {
    let _serial = x_test_lock();
    if !have("Xvfb") || !have("xtrace") || !have("xdotool") {
        eprintln!("SKIP prefs_native settle: needs Xvfb, xtrace and xdotool on PATH");
        return;
    }
    let Some((disp, xvfb)) = start_xvfb_scan(440) else {
        panic!("no Xvfb came up at or after :440");
    };
    let shell = write_shell("settle");
    let cfg = write_config("settle");
    let log = std::env::temp_dir().join(format!("rt_prefs_settle_{}.log", std::process::id()));
    let rt_bin = env!("CARGO_BIN_EXE_rt");

    let mut child = Command::new(rt_bin)
        .env_remove("WAYLAND_DISPLAY")
        .env("DISPLAY", format!(":{disp}"))
        .env("RUST_LOG", "rt=info") // the settle line we assert on
        .env("RT_BACKEND", "xrender")
        .env("RT_OPEN_PREFS", "1")
        .env("XDG_CONFIG_HOME", &cfg)
        .env("SHELL", &shell)
        .stdout(Stdio::from(std::fs::File::create(&log).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&log).unwrap()))
        .spawn()
        .expect("spawn rt");

    // Wait for the window, then focus it: with no WM, XTEST goes to the focused
    // window and `xdotool type --window` (XSendEvent) is ignored by winit.
    let mut win = String::new();
    for _ in 0..60 {
        let out = Command::new("xdotool")
            .args(["search", "--onlyvisible", "--name", "^rt$"])
            .env("DISPLAY", format!(":{disp}"))
            .output();
        if let Ok(o) = out {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() {
                win = s.lines().next().unwrap().to_string();
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(!win.is_empty(), "rt's window never appeared on :{disp}");
    std::thread::sleep(Duration::from_millis(3000)); // cold start + first paint
    let dpy = format!(":{disp}");
    let run = |args: &[&str]| {
        let _ = Command::new("xdotool").args(args).env("DISPLAY", &dpy).status();
    };
    run(&["windowfocus", &win]);
    std::thread::sleep(Duration::from_millis(300));
    // Select "Size (px)" (the first selectable row) and step it SIX times fast.
    for _ in 0..6 {
        run(&["key", "Right"]);
        std::thread::sleep(Duration::from_millis(30)); // key-repeat speed: inside PREFS_SETTLE
    }
    std::thread::sleep(Duration::from_millis(1200)); // let it settle and log

    let _ = child.kill();
    let _ = child.wait();
    stop_xvfb(xvfb, disp);
    let _ = std::fs::remove_file(&shell);
    let _ = std::fs::remove_dir_all(&cfg);

    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    let settles: Vec<&str> = text.lines().filter(|l| l.contains("prefs settled")).collect();
    eprintln!("settle lines:\n{}", settles.join("\n"));
    assert_eq!(
        settles.len(),
        1,
        "6 steps must produce exactly ONE commit (a font commit re-rasterises every \
         glyph, reflows every pane and writes config.toml); got {}:\n{}",
        settles.len(),
        text
    );
    assert!(
        settles[0].contains("6 edit(s) coalesced"),
        "all six steps must fold into the one commit: {}",
        settles[0]
    );
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt --test prefs_native -- --ignored --nocapture 2>&1 | tail -20`
Expected: FAIL — before Task 4's hook exists, `RT_OPEN_PREFS` does nothing and the dialog never renders, so `glyphs > 30` fails (a bare "hello" terminal draws far fewer). If Tasks 1-4 are already merged, they PASS; run this step against `git stash` of Task 4 only if you want to see the red.

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p rt --test prefs_native -- --ignored --nocapture 2>&1 | grep -E "test result|wire profile|settle lines"`
Expected: PASS — 2 tests; a wire profile with `PutImage=0` and glyphs well above 30; exactly one settle line reading `6 edit(s) coalesced into 1 commit`.

- [ ] **Step 4: Verify the harness does not leak display names**

Run:
```bash
B=$(ls /tmp/.X11-unix/ | wc -l)
cargo test -p rt --test prefs_native -- --ignored >/dev/null 2>&1
A=$(ls /tmp/.X11-unix/ | wc -l)
echo "sockets $B -> $A"
```
Expected: unchanged. A growing count means a teardown path is missing `stop_xvfb`/`release_display_name`, which starves the display scan for every later run.

- [ ] **Step 5: Run the whole suite**

Run:
```bash
cargo test -p rt --bins 2>&1 | grep "test result"
cargo test -p rt --test xrender_commands -- --ignored 2>&1 | grep "test result"
cargo test -p rt --test instrument_compositing -- --ignored 2>&1 | grep "test result"
cargo test -p rt --test cli_headless 2>&1 | grep "test result"
```
Expected: all green. `xrender_commands` matters most here: it pins `PutImage == 0` for the whole backend.

- [ ] **Step 6: Commit**

```bash
git add crates/rt/tests/prefs_native.rs
git commit -m "test(prefs): the dialog renders on XRender, as commands, and settles once

Two gates. First: with RT_BACKEND=xrender and RT_OPEN_PREFS=1 the dialog's rows
appear as CompositeGlyphs/FillRectangles and PutImage stays 0 -- on a backend
where it previously could not open at all. Asserts something POSITIVE rendered,
because PutImage==0 passes vacuously on a run that drew nothing.

Second: six rapid Right presses produce exactly ONE 'prefs settled' line reading
'6 edit(s) coalesced' -- the falsifiable form of commit-on-settle. Without it a
font step costs six re-rasterisations, six reflows and six config writes."
```

---

## Verification on real hardware

After Task 5, before calling this done — the milkv is the machine this feature exists for, and every remaining bug this session was found there, not here:

```bash
# from the repo, with main built:
git bundle create /tmp/prefs.bundle <last-shipped>..HEAD
scp -o ControlPath=none /tmp/prefs.bundle milkv:/tmp/
ssh -S none milkv 'cd ~/git/rt && git fetch -q /tmp/prefs.bundle HEAD && git checkout -q -B main FETCH_HEAD \
  && ~/.cargo/bin/cargo build --release && ./target/release/rt --version'
```

Then ask the user to open preferences over `ssh -X` and check:
1. It opens at all (it never could before).
2. Arrow keys move and step; nothing leaks to the shell.
3. Stepping font size feels like ONE pause, not six.
4. `inst_animate` is greyed until `inst_remote` is on.
5. Esc closes and the change survives a restart (it persisted).

`ssh -S none` is mandatory: `ControlMaster auto` is set for `Host *`, and reusing the user's live `ssh -X` master inherits THEIR X forwarding.

---

## Self-review

**Spec coverage.** Every section of the design maps to a task: the settings surface and memory guardrail → Task 2 `rows()`; interaction keys → Task 4 Step 6; mouse → Task 4 Step 9; scrolling → Task 2 `scroll_for` + Task 4; `PREFS_SETTLE` commit → Task 4 Steps 2/7 + Task 5's settle gate; `commit_settings` lift → Task 4 Step 3; `preferences.rs` deletion → Task 4 Step 4; `inst_animate` greying → Task 1 `enabled` + Task 2 row + tests in both; swatches → Task 2 `RowKind::Swatches` + Task 3 draw; the four gates → Tasks 1, 2 and 5.

**Placeholders.** One deliberate ellipsis remains, in Task 4 Step 3: the body of `commit_settings` is described as "the existing body from `paint_egui`, verbatim" rather than transcribed. That is intentional — it is ~40 lines of existing, working code, and re-typing it into the plan invites transcription errors; the instruction is to MOVE it unchanged. Every other step carries its real code.

**Type consistency.** `PrefRow` (Task 1) is used by name in Tasks 2 and 4. `Row`/`RowKind`/`Geom`/`Hit` (Task 2) are used in Tasks 3 and 4 with matching fields. `rows(s, mem_total, cols)`, `layout(rows, scroll, cell_w, cell_h, win_w, win_h)`, `draw(be, g, rows, sel, swatches, cell_w, cell_h)`, `hit(g, p)`, `next_sel(rows, sel, dir)`, `scroll_for(rows, sel, scroll, visible)` and `step(s, row, dir, families)` are spelled identically everywhere they appear. The `prefs settled: … N edit(s) coalesced into 1 commit` log line is emitted in Task 4 Step 7 and asserted in Task 5 with the same wording.
