//! Native preferences dialog: rows built from `Settings`, laid out as rects,
//! drawn as fills + glyphs. Mirrors `chrome/menu.rs` — a pure `layout()` split
//! from `draw()` so the geometry is unit-testable with no X server.

// Foundation-only for now: nothing calls into this module until the drawing
// (Task 3) and event-loop wiring (Task 4) land on top of it. Remove once they
// do. (Same scaffolding Task 1 added to `prefs_model.rs`.)
#![allow(dead_code)]

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
    let want = if sel < scroll {
        // Scrolled off the top: bring it to the top edge. If the whole first
        // page already contains sel, snap to the true top (0) rather than to
        // sel, so the leading "Font" header comes back with it.
        if sel < visible {
            0
        } else {
            sel
        }
    } else if sel >= scroll + visible {
        (sel + 1 - visible).min(max) // off the bottom: bring it to the bottom edge
    } else {
        scroll.min(max)
    };
    // A header labels the row beneath it, so top-aligning ON that row hides the
    // very word that says what it means ("Behaviour", "Colours", ...): headers
    // are never selectable, so `sel` is never the header's own index and no
    // amount of stepping can bring it back. Pull the window back one row to
    // reveal it. This catches BOTH ways a row lands on the top edge: a
    // top-align (want == sel), and an already-visible selection sitting there
    // (want == scroll == sel, reachable by arrowing up from a bottom-aligned
    // selection). The bottom edge needs no such rule -- bottom-aligning shows
    // `visible - 1` rows ABOVE sel, so the header comes along for free.
    //
    // `visible > 1` is the guard that keeps the invariant `scroll <= sel <
    // scroll + visible` true in every branch: with room for a single row,
    // backing up would push the selection itself out of view, and showing the
    // selection beats labelling it.
    if visible > 1 && want == sel && sel > 0 && rows[sel - 1].kind == RowKind::Section {
        return (sel - 1).min(max);
    }
    want
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
    fn scrolling_to_a_row_reveals_the_section_header_that_labels_it() {
        let rows = rs(&Settings::default());
        let visible = 7; // far fewer than the ~22 rows: every case must scroll

        // Found by scanning, never hardcoded: adding a setting shifts every
        // index, and a hardcoded one would silently stop testing what it names.
        let sections: Vec<usize> = rows
            .iter()
            .enumerate()
            .filter(|(i, r)| r.kind == RowKind::Section && *i > 0)
            .map(|(i, _)| i)
            .collect();
        assert!(sections.len() >= 3, "expected several mid-list sections, got {sections:?}");

        for h in sections {
            let sel = h + 1; // the first selectable row under the header
            assert!(rows[sel].pref.is_some(), "the row under a header must be selectable");
            // Approach it from the top of the list and from the bottom, so the
            // top-align, bottom-align and already-visible branches all run.
            for start in [0usize, rows.len() - visible] {
                let sc = scroll_for(&rows, sel, start, visible);
                assert!(
                    sc <= sel && sel < sc + visible,
                    "selection must stay visible: sel={sel} scroll={sc} start={start}"
                );
                assert!(
                    h >= sc && h < sc + visible,
                    "the {:?} header must be revealed with its row: sel={sel} scroll={sc} start={start}",
                    rows[h].label
                );
            }
        }
    }

    #[test]
    fn a_one_row_window_keeps_the_selection_visible_rather_than_its_header() {
        let rows = rs(&Settings::default());
        // With room for a single row, backing up to show the header would push
        // the selection itself out of view. The invariant wins; the header goes.
        for i in selectable(&rows) {
            let sc = scroll_for(&rows, i, 0, 1);
            assert!(sc <= i && i < sc + 1, "selection must stay visible: sel={i} scroll={sc}");
        }
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
