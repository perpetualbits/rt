//! Native preferences dialog: rows built from `Settings`, laid out as rects,
//! drawn as fills + glyphs. Mirrors `chrome/menu.rs` — a pure `layout()` split
//! from `draw()` so the geometry is unit-testable with no X server.

use crate::backend::Backend;
use crate::chrome_scale::logical;
use crate::chrome::Recti;
use crate::prefs_model::{enabled, family_advisory, preset_name, FamilyStatus, PrefRow};
use crate::render::Color;
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
    // `Close` is redundant with `Row`: the Close action row carries
    // `pref: Some(PrefRow::Close)`, so `hit()` already reports a click on it as
    // `Hit::Row(i)` and the caller matches `rows[i].pref` to find Close. Never
    // constructed by `hit()` — kept as a documented, intentional dead variant
    // rather than removed, in case a future caller wants to distinguish it
    // without re-deriving the row's `pref`.
    #[allow(dead_code)]
    Close,
}

const ARROW_W: f32 = 2.0; // "◄ " / " ►" width, in CELLS — derived, never scaled
// Room for the widest value. The right "►" arrow is drawn in the LAST `ARROW_W`
// cells of this field, so the usable text width is `VALUE_COLS - ARROW_W`.
#[cfg(not(target_os = "macos"))]
const VALUE_COLS: usize = 22; // 20 usable: "DejaVu Sans Mono" (16) is the widest
// macOS carries one more row, "Glass material", whose widest value is
// "under-window-background" — 23 characters, three past what the Linux panel
// leaves for text. Widened HERE rather than everywhere, so the Linux dialog keeps
// the exact width it has always had.
#[cfg(target_os = "macos")]
const VALUE_COLS: usize = 26; // 24 usable: "under-window-background" (23) + slack
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
///
/// `family` says whether `s.font_family` is the font actually on screen. It is
/// passed in, not computed, for the same reason `prefs_model::step` takes an
/// oracle: answering it costs a font parse, and this function runs on every
/// frame the dialog is painted. The caller memoises (see `Active::font_status`).
pub fn rows(s: &Settings, mem_total: u64, cols: usize, family: FamilyStatus) -> Vec<Row> {
    // No `families` param: the family VALUE shown is `s.font_family`. Only
    // `prefs_model::step` needs the installed list, to cycle through it.
    let mut v = Vec::new();

    // The running build, as a dim readout at the very top (a Display row: its
    // value IS its text, drawn from the label column).
    v.push(Row {
        kind: RowKind::Display,
        label: String::new(),
        value: crate::version_string(),
        pref: None,
        enabled: true,
    });

    v.push(sec("Font"));
    v.push(stepper("Size (px)", format!("{:.0}", s.font_size), PrefRow::FontSize));
    v.push(stepper("Family", s.font_family.clone(), PrefRow::FontFamily));
    // The Family row keeps showing the CONFIGURED name — that is what
    // `config.toml` holds and what the arrows edit — so when rt cannot draw it,
    // a line beneath has to say so, exactly as the `TERM` row's advisory does.
    // Without it the dialog reads as "this family is in use" while the old font
    // is still on screen, which is the bug this row exists to close.
    //
    // Pushed only when there IS something to say, so the common case does not
    // spend a line of a scrolling panel on silence. That cannot shift the
    // selection out from under the user: the advisory sits BELOW the Family row,
    // and the only thing that makes it come or go is stepping that row, which
    // requires the selection to be on it. `prefs_model::step` never lands on an
    // unusable family, so from here the line can only disappear, never appear.
    if let Some(line) = family_advisory(family) {
        v.push(Row {
            kind: RowKind::Display,
            label: String::new(),
            value: line.to_string(),
            pref: None,
            enabled: true,
        });
    }

    v.push(sec("Appearance"));
    v.push(stepper("Background opacity", format!("{:.2}", s.background_opacity), PrefRow::Opacity));
    v.push(toggle("Background blur", s.background_blur, PrefRow::Blur, true));
    // macOS only, and BUILT only there: on Linux the compositor owns the blur and
    // the protocols offer nothing but on/off, so a material row would be a control
    // that visibly does nothing. The setting itself is cross-platform (it is
    // parsed and preserved everywhere, so one config.toml stays portable) — it is
    // only the UI for it that is target-gated. Dimmed unless there is actually
    // glass on screen; see `prefs_model::enabled`.
    #[cfg(target_os = "macos")]
    v.push(Row {
        kind: RowKind::Step,
        label: "Glass material".into(),
        value: s.macos_glass_material.name().to_string(),
        pref: Some(PrefRow::GlassMaterial),
        enabled: enabled(s, PrefRow::GlassMaterial),
    });

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
    v.push(toggle("Hold-arrow acceleration", s.arrow_accel, PrefRow::ArrowAccel, true));
    v.push(Row {
        kind: RowKind::Step,
        label: "Max arrow speed (x)".into(),
        value: s.arrow_accel_max.to_string(),
        pref: Some(PrefRow::ArrowAccelMax),
        enabled: enabled(s, PrefRow::ArrowAccelMax), // dimmed while acceleration is off
    });
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

    // Terminal type. Cycles only names this machine has terminfo for (see
    // `rt_config::term_candidates`), because a `TERM` with no entry breaks every ncurses
    // application in every pane opened afterwards — `vim`, `less`, `htop`, `mc`.
    v.push(sec("Terminal type"));
    v.push(stepper("TERM", s.term.clone(), PrefRow::Term));
    // What the next pane will ACTUALLY get, and why. `RT_TERM` in rt's own environment
    // overrides the setting above, and a user who exported it for an experiment needs to
    // see that the dialog is not in charge — otherwise the row reads as a lie.
    let effective = rt_config::term_name(Some(&s.term));
    v.push(Row {
        kind: RowKind::Display,
        label: String::new(),
        value: if effective != s.term {
            format!("next pane gets {effective} — $RT_TERM overrides this")
        } else if s.term == rt_config::DEFAULT_TERM {
            "rt's own sequences are a superset of this; safe everywhere".to_string()
        } else {
            format!("needs `infocmp {effective}` to work on EVERY host you ssh to")
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
pub fn layout(rows: &[Row], scroll: usize, cell_w: f32, cell_h: f32, win_w: f32, win_h: f32, sc: f32) -> Geom {
    // `sc` is the display's backing factor, applied to the flat padding and row
    // pad. `hit` and `swatch_rects` both work off the rects produced here, so the
    // whole click surface follows the drawing — the colour picker's own
    // unclickable-at-2x bug is exactly what that arrangement prevents.
    let pad_x = sc * logical::PREFS_PAD;
    let row_h = cell_h + sc * logical::PANEL_ROW_PAD;
    let w = ((LABEL_COLS + VALUE_COLS) as f32 + ARROW_W * 2.0) * cell_w + pad_x * 2.0;
    let w = w.min(win_w); // never wider than the window
    // How many rows fit, leaving the padding at top and bottom.
    let visible = (((win_h - pad_x * 2.0) / row_h).floor() as usize).clamp(1, rows.len());
    let h = visible as f32 * row_h + pad_x * 2.0;
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
        let ry = y + pad_x + (i - scroll) as f32 * row_h;
        rrects.push(Recti { x, y: ry, w, h: row_h });
        // Arrow zones sit at the right edge, either side of the value.
        if matches!(r.kind, RowKind::Step) && r.enabled {
            let aw = ARROW_W * cell_w;
            let vx = x + w - pad_x - VALUE_COLS as f32 * cell_w;
            left.push(Some(Recti { x: vx - aw, y: ry, w: aw, h: row_h }));
            right.push(Some(Recti { x: x + w - pad_x - aw, y: ry, w: aw, h: row_h }));
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

/// The per-swatch rects in a `Swatches` row (`[fg, bg, palette…]`), laid out
/// right-aligned. Shared by `draw` (to paint them) and the click handler (to
/// open the colour picker on the one clicked), so the two never drift.
pub fn swatch_rects(row: Recti, count: usize, cell_h: f32, sc: f32) -> Vec<Recti> {
    let s = cell_h * 0.6; // derived from the cell: already scales with the glyph
    let gap = sc * logical::PREFS_SWATCH_GAP;
    let pad_x = sc * logical::PREFS_PAD;
    let y = row.y + (row.h - s) * 0.5;
    let mut sx = row.x + row.w - pad_x - count as f32 * (s + gap);
    let mut out = Vec::with_capacity(count);
    for _ in 0..count {
        out.push(Recti { x: sx, y, w: s, h: s });
        sx += s + gap;
    }
    out
}

const PANEL_BG: Color = Color(0.10, 0.10, 0.12, 0.97);
const PANEL_EDGE: Color = Color(0.35, 0.35, 0.42, 1.0);
const SEL_BG: Color = Color(0.18, 0.20, 0.28, 1.0);
const TEXT: Color = Color(0.82, 0.82, 0.86, 1.0);
const TEXT_DIM: Color = Color(0.45, 0.45, 0.50, 1.0);
const SECTION: Color = Color(0.55, 0.72, 0.90, 1.0);

/// Paint the dialog. `swatches` is `[fg, bg, palette…]`, painted into the
/// `Swatches` row; `sel` is the selected row index.
pub fn draw(be: &mut dyn Backend, g: &Geom, rows: &[Row], sel: usize, swatches: &[Color], cell_w: f32, cell_h: f32, sc: f32) {
    let pad_x = sc * logical::PREFS_PAD;
    let hair = sc * logical::HAIRLINE;
    // Panel: a hairline edge drawn as four thin fills around an opaque body.
    let p = g.panel;
    be.fill_rect(p.x, p.y, p.w, p.h, PANEL_BG);
    be.fill_rect(p.x, p.y, p.w, hair, PANEL_EDGE);
    be.fill_rect(p.x, p.y + p.h - hair, p.w, hair, PANEL_EDGE);
    be.fill_rect(p.x, p.y, hair, p.h, PANEL_EDGE);
    be.fill_rect(p.x + p.w - hair, p.y, hair, p.h, PANEL_EDGE);

    for (i, row) in rows.iter().enumerate() {
        // Skip rows scrolled out of view (layout parked them off-panel).
        if i < g.scroll || i >= g.scroll + g.visible {
            continue;
        }
        let r = g.rows[i];
        if i == sel {
            let inset = sc * logical::PREFS_SEL_INSET;
            be.fill_rect(r.x + inset, r.y, r.w - 2.0 * inset, r.h, SEL_BG);
        }
        let ty = r.y + sc * logical::PANEL_TEXT_TOP;
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
        let lx = if matches!(row.kind, RowKind::Section) { r.x + pad_x } else { r.x + pad_x + cell_w };
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
            for (rect, col) in swatch_rects(r, swatches.len(), cell_h, sc).into_iter().zip(swatches) {
                be.fill_rect(rect.x, rect.y, rect.w, rect.h, *col);
            }
            continue;
        }
        // Value, in the value column (right side of the row, sized to the
        // widest value so it lines up across every row and never collides
        // with the arrow zones `layout` reserved at the same `vx`).
        let vx = r.x + r.w - pad_x - VALUE_COLS as f32 * cell_w;
        for (c, ch) in row.value.chars().enumerate() {
            be.draw_char(vx, ty, c, 0, ch, colour, false, false);
        }
        // Arrows, only where a step is possible.
        if let (Some(l), Some(right)) = (g.left[i], g.right[i]) {
            be.draw_char(l.x, ty, 0, 0, '◄', colour, false, false);
            be.draw_char(right.x, ty, 0, 0, '►', colour, false, false);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rt_config::Settings;

    fn rs(s: &Settings) -> Vec<Row> {
        // 16 GB of RAM, an 80-column pane, and a font rt can actually draw —
        // the ordinary case every test below the family ones is about.
        rows(s, 16 * 1024 * 1024 * 1024, 80, FamilyStatus::Usable)
    }

    /// The index of the Family row, found by scanning (never hardcoded).
    fn family_row(rows: &[Row]) -> usize {
        rows.iter().position(|r| r.pref == Some(PrefRow::FontFamily)).expect("a Family row")
    }

    #[test]
    fn every_setting_row_carries_the_pref_it_edits() {
        let rows = rs(&Settings::default());
        let want = [
            PrefRow::FontSize, PrefRow::FontFamily, PrefRow::Opacity, PrefRow::Blur,
            // The glass material row exists only on macOS (see `rows`), so the
            // frozen order differs by target rather than pretending it doesn't.
            #[cfg(target_os = "macos")]
            PrefRow::GlassMaterial,
            PrefRow::Preset, PrefRow::Ffm, PrefRow::Titlebar, PrefRow::Scrollback,
            PrefRow::ArrowAccel, PrefRow::ArrowAccelMax, PrefRow::Term,
            PrefRow::InstOutput, PrefRow::InstHeat, PrefRow::InstLatency, PrefRow::Jacks,
            PrefRow::InstRemote, PrefRow::InstAnimate, PrefRow::Close,
        ];
        let got: Vec<PrefRow> = rows.iter().filter_map(|r| r.pref).collect();
        assert_eq!(got, want, "every PrefRow appears once, in order");
    }

    /// The TERM row shows the configured name, and the readout under it says what the
    /// next pane will really get. A `TERM` that is not installed here is the one setting
    /// in this dialog that can stop `vim` from starting, so the row must not read as a
    /// bare cosmetic choice.
    #[test]
    fn the_terminal_type_row_shows_the_setting_and_warns_about_a_borrowed_one() {
        let rows = rs(&Settings::default());
        let i = rows.iter().position(|r| r.pref == Some(PrefRow::Term)).expect("a TERM row");
        assert_eq!(rows[i].value, "xterm-256color", "the default is shown as-is");
        assert_eq!(rows[i + 1].kind, RowKind::Display, "an advisory line follows it");
        assert!(
            rows[i + 1].value.contains("safe everywhere"),
            "the default must read as safe: {:?}",
            rows[i + 1].value
        );

        // A borrowed name turns the advisory into the check the user has to make on every
        // machine — rt cannot make it for them.
        let borrowed = Settings { term: "xterm-kitty".to_string(), ..Settings::default() };
        let rows = rs(&borrowed);
        let i = rows.iter().position(|r| r.pref == Some(PrefRow::Term)).expect("a TERM row");
        assert_eq!(rows[i].value, "xterm-kitty");
        assert!(
            rows[i + 1].value.contains("infocmp xterm-kitty"),
            "the advisory must name the check to run: {:?}",
            rows[i + 1].value
        );
    }

    /// The value column is sized to the widest value a row can hold, and the
    /// step arrows are drawn in its last `ARROW_W` cells — so a value longer than
    /// `VALUE_COLS - ARROW_W` silently draws over the "►" and out of the panel.
    /// Adding a setting whose value is a NAME (a font family, a colour preset, a
    /// glass material) is exactly how that happens, so pin it.
    #[test]
    fn every_value_a_row_can_show_fits_the_value_column() {
        let budget = VALUE_COLS - ARROW_W as usize;
        let mut states = vec![Settings::default()];
        // The widest value of each name-valued row, not just the default one.
        for name in rt_config::SCHEMES.iter() {
            let mut s = Settings::default();
            s.foreground = name.foreground;
            s.background = name.background;
            s.palette = name.palette;
            states.push(s);
        }
        for m in rt_config::GlassMaterial::ALL {
            let mut s = Settings::default();
            s.background_blur = true;
            s.background_opacity = 0.5;
            s.macos_glass_material = *m;
            states.push(s);
        }
        // Font family is user-supplied and unbounded, so it is deliberately not
        // covered here — the panel clamps to the window width and a long family
        // name has always been allowed to run wide.
        for s in &states {
            for row in rs(s) {
                if matches!(row.kind, RowKind::Display | RowKind::Section | RowKind::Swatches) {
                    continue; // drawn from the label column, not the value column
                }
                if row.pref == Some(PrefRow::FontFamily) {
                    continue;
                }
                let n = row.value.chars().count();
                assert!(n <= budget, "{:?} value {:?} is {n} cells, budget {budget}", row.pref, row.value);
            }
        }
    }

    /// The display half of the bug: a configured family rt cannot draw must not
    /// be shown as though it were the font on screen. The row still carries the
    /// name (that IS the setting, and the arrows edit it), and the line beneath
    /// says a fallback is what is rendering — the same shape as the TERM row.
    #[test]
    fn a_family_rt_cannot_draw_is_shown_as_not_in_use() {
        let s = Settings { font_family: "GB18030 Bitmap".to_string(), ..Settings::default() };
        for status in [FamilyStatus::Missing, FamilyStatus::Unrasterisable] {
            let rows = rows(&s, 16 * 1024 * 1024 * 1024, 80, status);
            let i = family_row(&rows);
            assert_eq!(rows[i].value, "GB18030 Bitmap", "the configured name stays visible");
            assert_eq!(rows[i + 1].kind, RowKind::Display, "{status:?}: an advisory must follow it");
            assert_eq!(
                rows[i + 1].value,
                family_advisory(status).unwrap(),
                "{status:?}: the row must carry the model's advisory, not a second copy"
            );
            assert!(rows[i + 1].pref.is_none(), "an advisory is a readout, not a setting");
        }
        // And when rt IS drawing the configured family, the dialog says nothing:
        // no line of a scrolling panel spent on "everything is fine".
        let rows = rows(&s, 16 * 1024 * 1024 * 1024, 80, FamilyStatus::Usable);
        let i = family_row(&rows);
        assert_eq!(rows[i + 1].kind, RowKind::Section, "the usable case goes straight on");
    }

    /// The advisory must not move the row the user is standing on. It sits below
    /// the Family row, so the Family row's own index — and every index above it
    /// — is the same whether or not the advisory is there.
    #[test]
    fn the_advisory_never_shifts_the_row_it_explains() {
        let s = Settings { font_family: "GB18030 Bitmap".to_string(), ..Settings::default() };
        let usable = rows(&s, 1 << 34, 80, FamilyStatus::Usable);
        for status in [FamilyStatus::Missing, FamilyStatus::Unrasterisable] {
            let bad = rows(&s, 1 << 34, 80, status);
            assert_eq!(family_row(&bad), family_row(&usable), "{status:?}: Family row moved");
            assert_eq!(bad.len(), usable.len() + 1, "{status:?}: exactly one extra row");
        }
    }

    /// A `Display` row is drawn from the label column, so it has the panel's
    /// inner width minus the one-cell indent to fit in. The advisories are
    /// fixed strings, so this can be pinned exactly rather than hoped for.
    #[test]
    fn every_family_advisory_fits_the_panel() {
        let budget = LABEL_COLS + VALUE_COLS + ARROW_W as usize * 2 - 1;
        for status in [FamilyStatus::Usable, FamilyStatus::Missing, FamilyStatus::Unrasterisable] {
            let Some(line) = family_advisory(status) else { continue };
            let n = line.chars().count();
            assert!(n <= budget, "{status:?} advisory is {n} cells, budget {budget}: {line:?}");
        }
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
        let rows = rows(&s, 16 * 1024 * 1024 * 1024, 80, FamilyStatus::Usable);
        // Find the scrollback readout specifically — there is also a version
        // Display row at the top now.
        let readout = rows
            .iter()
            .find(|r| matches!(r.kind, RowKind::Display) && r.value.contains("per pane"))
            .unwrap();
        assert!(readout.value.contains("per pane"), "got {:?}", readout.value);
        assert!(readout.value.contains('%'), "must state its share of RAM: {:?}", readout.value);
        assert!(readout.value.contains("80 cols"), "must state the width it assumed");
    }

    #[test]
    fn layout_keeps_the_panel_on_screen_and_rows_inside_it() {
        let rows = rs(&Settings::default());
        let g = layout(&rows, 0, 11.0, 21.0, 900.0, 700.0, 1.0);
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
        let g = layout(&rows, 0, 11.0, 21.0, 900.0, 200.0, 1.0);
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
        let g = layout(&rows, 0, 11.0, 21.0, 900.0, 700.0, 1.0);
        let i = rows.iter().position(|r| r.pref == Some(PrefRow::FontSize)).unwrap();
        let l = g.left[i].expect("a Step row has a left arrow");
        let r = g.right[i].expect("a Step row has a right arrow");
        assert!(matches!(hit(&g, (l.x + 1.0, l.y + 1.0)), Some(Hit::Step(n, -1)) if n == i));
        assert!(matches!(hit(&g, (r.x + 1.0, r.y + 1.0)), Some(Hit::Step(n, 1)) if n == i));
    }

    #[test]
    fn clicking_a_row_selects_it_and_clicking_outside_hits_nothing() {
        let rows = rs(&Settings::default());
        let g = layout(&rows, 0, 11.0, 21.0, 900.0, 700.0, 1.0);
        let i = rows.iter().position(|r| r.pref == Some(PrefRow::Ffm)).unwrap();
        let row = g.rows[i];
        // A click on the label (left of the arrows) selects the row.
        assert!(matches!(hit(&g, (row.x + 2.0, row.y + 2.0)), Some(Hit::Row(n)) if n == i));
        assert!(hit(&g, (g.panel.x - 5.0, g.panel.y - 5.0)).is_none(), "outside the panel");
    }
}
