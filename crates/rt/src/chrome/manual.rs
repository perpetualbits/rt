//! Native manual overlay: a centered panel that **word-wraps** `manual::MANUAL`
//! to a comfortable measure and scrolls by wrapped rows. A version line heads
//! the text. Scroll position lives in `Active.manual_scroll`.
//!
//! # What "text does not flow" turned out to mean
//!
//! Three separate faults, all of them typographic:
//!
//! 1. **Continuation lines lost their indent.** The old wrap split on
//!    whitespace and emitted every continuation flush at column zero. The manual
//!    is almost entirely two-column key/description material —
//!    `  Ctrl+Shift+O    split horizontally` — so a single wrapped description
//!    threw the whole column structure away and the page dissolved into ragged
//!    prose. [`wrap_line`] now keeps a line's own indent and hangs continuations
//!    under the **description column**, found by looking for the first run of two
//!    or more spaces. That is the fix that most looks like "flow".
//! 2. **The measure was far too wide.** The panel was 85% of the window capped
//!    at 900 logical px, which at a small font is 120+ characters per line —
//!    roughly double the 60–80 that is readable. The measure is now capped in
//!    COLUMNS ([`TEXT_MEASURE`]), which is the unit that actually governs
//!    reading, and the panel is sized from it.
//! 3. **Headings were invisible.** `MANUAL`'s own doc comment says "UPPERCASE
//!    lines are section headings", and the overlay drew them in exactly the same
//!    colour and weight as body text. They are now bold, in the palette's accent
//!    ([`LineKind::Heading`]).
//!
//! Plus leading: monospace set solid is a wall, so body lines get
//! `MANUAL_LEADING` px of air between them.
//!
//! # …and what fixing (2) too enthusiastically then meant
//!
//! *"the manual is way too narrow which makes it hard to read. And although
//! wrapped, it does not re-wrap."*
//!
//! Both halves of that are one cause. The layout is recomputed from the live
//! window size every frame, so it genuinely does re-wrap — but the panel was
//! `84 * cell_w`, a hard cap, so widening the window changed nothing you could
//! see, which is indistinguishable from not re-wrapping.
//!
//! 84 columns came from applying the 60–80 prose guidance to the whole line.
//! That is the wrong line to measure: 104 of the manual's rows are
//! `KEY   description`, the key column is furniture, and measuring from column
//! zero spent a quarter of the budget on it. The measure now applies to **each
//! column of reading matter separately** ([`line_measure`]) — a description gets
//! [`TEXT_MEASURE`], and so does a paragraph, counted from where its own text
//! starts. The panel is sized from the *key* rows' need ([`measure_cap`], the
//! manual's own key column plus the measure — 99 columns as the manual stands),
//! and grows with the window until it gets there. Descriptions went from 65
//! characters to 80; prose is unchanged at 80.
//!
//! # Colour
//!
//! *"There is also not use of color in the manual nor menu."*
//!
//! Three roles, no more: `text` is what you read, `accent` marks what you press,
//! `dim` is metadata. So a key row is drawn in **two** colours — the key in the
//! accent, its description in body text ([`Line::key_end`] carries the split) —
//! section headings are bold accent, and the version line is dim. One colour for
//! keys and descriptions alike is what made a hundred rows of reference material
//! read as an undifferentiated slab. `chrome::menu` marks its accelerators the
//! same way, so "accent means the thing you press" holds across both panels.
use crate::backend::Backend;
use crate::chrome::theme::{self, Palette};
use crate::chrome::Recti;
use crate::chrome_scale::logical;
use crate::manual::manual_lines;

/// The comfortable measure for one column of **reading matter**, in characters.
///
/// The unit that governs reading is a character count, so this is capped in
/// columns rather than pixels — a pixel cap turns into a different column count
/// at every font size. What is new is *what* it is counted from. The 60–80
/// guidance is about a column of prose; rt's manual is not prose, it is 104
/// two-column `KEY   description` rows, and on those rows the key column is
/// furniture. Counting the guidance from column zero spent a quarter of it on
/// the keys and left the descriptions at 65 characters in a panel that could
/// not grow.
///
/// So: **every line is allotted at most `TEXT_MEASURE` characters of its own
/// text, counted from the column that text starts in** — the description column
/// on a key row, the paragraph indent on prose. Prose still sets at 80 + its
/// indent, exactly as before; descriptions now get 80 instead of 65.
pub const TEXT_MEASURE: usize = 80;

/// The widest a key column may be before the "two columns" reading stops being
/// credible and the line is just prose containing a double space.
///
/// The manual's real key column runs to 27 characters
/// (`Ctrl+Alt+Up / Ctrl+Alt+Down`). Everything wider that matches the
/// double-space shape is a sentence — *"Every change applies live and persists
/// to  $XDG_CONFIG_HOME/rt/config.toml"* — and colouring its first forty
/// characters as a key name would be worse than not colouring anything.
const KEY_MAX: usize = 28;

/// Fraction of the window the panel may take. Wide enough to feel like a
/// document, narrow enough to leave the terminal visible around it.
const PANEL_W_FRAC: f32 = 0.90;
const PANEL_H_FRAC: f32 = 0.86;

/// What a wrapped line IS, so `draw` can set it appropriately instead of
/// painting the whole manual as one undifferentiated block.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LineKind {
    /// The running version, at the very top.
    Version,
    /// An UPPERCASE section heading (see `manual::MANUAL`'s doc comment).
    Heading,
    /// Everything else.
    Body,
}

/// One wrapped row, ready to draw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub kind: LineKind,
    pub text: String,
    /// On the first row of a two-column `KEY   description` line, the char index
    /// the key ends at — so [`draw`] can set the key and its description in
    /// different roles. `None` on prose, headings, and continuation rows (whose
    /// text is all description, hanging under the description column).
    pub key_end: Option<usize>,
}

/// Manual panel geometry plus the wrapped lines to draw (version header first).
pub struct Geom {
    pub panel: Recti,
    /// Visible wrapped rows in the panel.
    pub rows: usize,
    /// The whole manual, wrapped to the panel's measure, each line tagged.
    pub lines: Vec<Line>,
    pub total: usize,
    /// Height of one wrapped line INCLUDING its leading — the manual's own row
    /// rhythm. `draw` and `rows` both use it, so a scroll position always means
    /// the same thing on screen.
    pub line_h: f32,
}

/// Where a source line's own text begins, and — on a two-column row — where its
/// key ends.
///
/// The description column is the first run of two or more spaces after the
/// indent. A row counts as two-column only if the key before that run is at most
/// [`KEY_MAX`] wide; see that constant for why.
fn columns(line: &str) -> (usize, Option<usize>) {
    let chars: Vec<char> = line.chars().collect();
    let lead = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(chars.len());
    // A flush-left line is a heading or a top-level paragraph, never a key row.
    if lead == 0 || lead >= chars.len() {
        return (lead.min(chars.len()), None);
    }
    let mut i = lead;
    while i + 1 < chars.len() {
        if chars[i] == ' ' && chars[i + 1] == ' ' {
            let mut j = i;
            while j < chars.len() && chars[j] == ' ' {
                j += 1;
            }
            // Trailing whitespace is not a column boundary.
            if j < chars.len() && i - lead <= KEY_MAX {
                return (j, Some(i));
            }
            return (lead, None);
        }
        i += 1;
    }
    (lead, None)
}

/// The manual's own key column: the description column that the most of its
/// two-column rows share.
///
/// Derived from the text rather than written down, so editing `MANUAL` moves the
/// measure with it and the two cannot drift apart. Computed once.
pub fn key_column() -> usize {
    static COL: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *COL.get_or_init(|| {
        let mut hist: std::collections::HashMap<usize, usize> = std::collections::HashMap::new();
        for line in manual_lines() {
            if let (c, Some(_)) = columns(line) {
                *hist.entry(c).or_default() += 1;
            }
        }
        // Most common column; the narrower one breaks a tie.
        hist.into_iter().max_by_key(|&(col, n)| (n, usize::MAX - col)).map(|(c, _)| c).unwrap_or(0)
    })
}

/// The widest measure the manual is ever set at, in characters.
///
/// The key column plus [`TEXT_MEASURE`]. A panel wider than this cannot help the
/// typical row — its description already has its full measure — and would only
/// push the manual's *prose*, which starts at column 2, past the measure prose
/// wants. Rows with a deeper-than-typical key column (the CLI and environment
/// blocks sit at 28) simply get a slightly shorter description; at this cap the
/// shortest of them is still 65 characters, so every description in the document
/// lands inside the comfortable 60–80 band.
pub fn measure_cap() -> usize {
    key_column() + TEXT_MEASURE
}

/// How many character columns the manual is set at inside a panel of `panel_w`.
pub fn visible_cols(panel_w: f32, cell_w: f32, sc: f32) -> usize {
    let inner = panel_w - sc * logical::PANEL_PAD_X * 2.0 - sc * logical::MANUAL_SB_INSET * 2.0;
    if inner <= 0.0 || cell_w <= 0.0 {
        return 0;
    }
    ((inner / cell_w).floor() as usize).min(measure_cap())
}

/// Is this an UPPERCASE section heading?
fn is_heading(line: &str) -> bool {
    !line.starts_with(char::is_whitespace)
        && line.chars().any(char::is_alphabetic)
        && line.chars().filter(|c| c.is_alphabetic()).all(|c| c.is_uppercase())
}

/// The column a wrapped continuation of `line` should hang at.
///
/// For a two-column `KEY   description` line that is the description column, so
/// a long description stays in its own column instead of running back under the
/// key. For ordinary prose it is simply the line's own indent, so a paragraph
/// keeps its block shape. Clamped to half the measure, so a pathologically wide
/// first column cannot squeeze the text to nothing.
fn continuation_indent(line: &str, cols: usize) -> usize {
    let chars: Vec<char> = line.chars().collect();
    let lead = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(0);
    // Same analysis `draw` colours from, so the hang column and the key column
    // are the same column by construction.
    let (text_col, _) = columns(line);
    text_col.min(cols / 2).max(lead.min(cols / 2))
}

/// Greedy word-wrap of one source line to `cols`, appending to `out`.
///
/// Keeps the line's own leading indent on the first row and hangs every
/// continuation at [`continuation_indent`]. A single word wider than the measure
/// is hard-broken rather than allowed to overhang.
fn wrap_line(line: &str, cols: usize, kind: LineKind, out: &mut Vec<Line>) {
    let first = out.len();
    wrap_rows(line, cols, kind, out);
    // Only the FIRST row of a two-column source line still carries its key; the
    // continuations hang under the description column and are all description.
    // (A window narrow enough to break the key itself gets no colour split —
    // there is no intact key left to mark.)
    if let (_, Some(key_end)) = columns(line) {
        if let Some(row) = out.get_mut(first) {
            if row.text.chars().count() > key_end {
                row.key_end = Some(key_end);
            }
        }
    }
}

/// The wrapping proper; [`wrap_line`] adds the key/description split on top.
fn wrap_rows(line: &str, cols: usize, kind: LineKind, out: &mut Vec<Line>) {
    let push = |out: &mut Vec<Line>, text: String| out.push(Line { kind, text, key_end: None });
    if line.chars().count() <= cols {
        push(out, line.to_string());
        return;
    }
    let chars: Vec<char> = line.chars().collect();
    let lead = chars.iter().position(|c| !c.is_whitespace()).unwrap_or(0).min(cols / 2);
    let cont = continuation_indent(line, cols);
    let mut cur = " ".repeat(lead);
    let mut has_word = false;
    for w in line.split_whitespace() {
        let wl = w.chars().count();
        if has_word && cur.chars().count() + 1 + wl > cols {
            push(out, std::mem::take(&mut cur));
            cur = " ".repeat(cont);
            has_word = false;
        }
        if !has_word && cur.chars().count() + wl > cols {
            // A word wider than the measure: chop it into full rows.
            let mut rest: Vec<char> = w.chars().collect();
            loop {
                let take = cols.saturating_sub(cur.chars().count());
                if take == 0 || rest.len() <= take {
                    break;
                }
                cur.extend(rest.drain(..take));
                push(out, std::mem::take(&mut cur));
                cur = " ".repeat(cont);
            }
            cur.extend(rest);
            has_word = true;
            continue;
        }
        if has_word {
            cur.push(' ');
        }
        cur.push_str(w);
        has_word = true;
    }
    if has_word {
        push(out, cur);
    }
}

/// The measure ONE source line is set at inside a panel of `cols` columns.
///
/// This is the rule described on [`TEXT_MEASURE`]: a line gets its own text
/// column plus the comfortable measure, never more, whatever the panel can
/// afford. It is what lets the panel be wide enough for a key row's full
/// description without setting the manual's paragraphs at the same width.
fn line_measure(line: &str, cols: usize) -> usize {
    let (text_col, _) = columns(line);
    cols.min(text_col.saturating_add(TEXT_MEASURE)).max(1)
}

/// Word-wrap the manual to `cols` columns, headed by the running version.
pub fn wrapped(cols: usize) -> Vec<Line> {
    let mut out = Vec::new();
    if cols == 0 {
        return vec![Line { kind: LineKind::Version, text: crate::version_string(), key_end: None }];
    }
    // The version line wraps like everything else: at a narrow measure it is one
    // of the longest lines in the document.
    let v = crate::version_string();
    wrap_line(&v, line_measure(&v, cols), LineKind::Version, &mut out);
    out.push(Line { kind: LineKind::Body, text: String::new(), key_end: None });
    for line in manual_lines() {
        let kind = if is_heading(line) { LineKind::Heading } else { LineKind::Body };
        wrap_line(line, line_measure(line, cols), kind, &mut out);
    }
    out
}

/// A centred panel sized to the manual's measure, never wider than the window.
pub fn layout(win_w: f32, win_h: f32, cell_w: f32, cell_h: f32, sc: f32) -> Geom {
    let pad_x = sc * logical::PANEL_PAD_X;
    let pad_y = sc * logical::PANEL_PAD_Y;
    // Width comes from the MEASURE, not from the window: a 4K window must not
    // hand the reader a 200-character line.
    let ideal = measure_cap() as f32 * cell_w + pad_x * 2.0 + sc * logical::MANUAL_SB_INSET * 2.0;
    let w = ideal.min(win_w * PANEL_W_FRAC).min(win_w).max(0.0);
    let h = (win_h * PANEL_H_FRAC).min(win_h).max(0.0);
    let panel = Recti { x: ((win_w - w) * 0.5).max(0.0), y: ((win_h - h) * 0.5).max(0.0), w, h };
    let line_h = cell_h + sc * logical::MANUAL_LEADING;
    let rows = (((h - pad_y * 2.0) / line_h).floor() as usize).max(1);
    let lines = wrapped(visible_cols(w, cell_w, sc));
    let total = lines.len();
    Geom { panel, rows, lines, total, line_h }
}

/// Clamp a scroll offset so the last page stays on-screen.
pub fn clamp_scroll(scroll: usize, g: &Geom) -> usize {
    let max = g.total.saturating_sub(g.rows);
    scroll.min(max)
}

/// Draw the panel, the visible wrapped-line slice, and a scrollbar thumb.
pub fn draw(be: &mut dyn Backend, g: &Geom, scroll: usize, pal: &Palette, cell_w: f32, cell_h: f32, sc: f32) {
    let pad_x = sc * logical::PANEL_PAD_X;
    let pad_y = sc * logical::PANEL_PAD_Y;
    let p = g.panel;
    theme::panel(be, p, pal, sc);
    let ox = p.x + pad_x;
    let scroll = clamp_scroll(scroll, g);
    for (r, line) in g.lines.iter().skip(scroll).take(g.rows).enumerate() {
        let oy = p.y + pad_y + r as f32 * g.line_h;
        let (colr, bold) = match line.kind {
            LineKind::Version => (pal.dim, false),
            LineKind::Heading => (pal.accent, true),
            LineKind::Body => (pal.text, false),
        };
        // A key row is set in TWO roles: the key in the accent, because it names
        // something you press, and its description in body text, because it is
        // what you are actually reading. One colour for both is what made 104
        // rows of reference material read as an undifferentiated slab.
        match line.key_end {
            Some(k) if line.kind == LineKind::Body => {
                let key: String = line.text.chars().take(k).collect();
                let desc: String = line.text.chars().skip(k).collect();
                theme::text(be, ox, oy, &key, pal.accent, false);
                // Same origin, advanced by the key's own width — `theme::text`
                // indexes cells from its `ox`, so the description lands exactly
                // on the column the wrap hung its continuations under.
                theme::text(be, ox + k as f32 * cell_w, oy, &desc, pal.text, false);
            }
            _ => theme::text(be, ox, oy, &line.text, colr, bold),
        }
    }
    // Scrollbar thumb on the right edge, sized to the visible fraction.
    if g.total > g.rows {
        let inset = sc * logical::MANUAL_SB_TRACK_INSET;
        let track_h = (p.h - inset).max(1.0);
        let tw = sc * logical::MANUAL_SB_W;
        let th = (track_h * g.rows as f32 / g.total as f32).max(sc * logical::MANUAL_SB_MIN_THUMB).min(track_h);
        let ty = p.y + inset * 0.5 + (track_h - th) * scroll as f32 / (g.total - g.rows) as f32;
        theme::rounded(be, p.x + p.w - sc * logical::MANUAL_SB_INSET - tw, ty, tw, th, tw * 0.5, pal.thumb);
    }
    let _ = cell_h;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_last_page_visible() {
        let g = layout(1000.0, 700.0, 8.0, 18.0, 1.0);
        assert!(g.total > g.rows, "manual is longer than one page");
        let max = g.total - g.rows;
        assert_eq!(clamp_scroll(usize::MAX, &g), max, "cannot scroll past the end");
        assert_eq!(clamp_scroll(0, &g), 0);
    }

    /// The measure is capped in columns, so a huge window does not produce an
    /// unreadable 200-character line — this is half of "text does not flow".
    #[test]
    fn the_measure_is_comfortable_at_every_window_size() {
        for &(w, h) in &[(800.0_f32, 600.0_f32), (1920.0, 1080.0), (3840.0, 2160.0), (400.0, 300.0)] {
            for &cell_w in &[6.0_f32, 8.0, 11.0, 16.0] {
                let g = layout(w, h, cell_w, cell_w * 2.0, 1.0);
                let cols = visible_cols(g.panel.w, cell_w, 1.0);
                assert!(cols <= measure_cap(), "{w}x{h} @{cell_w}: {cols} columns is too wide a measure");
                for l in g.lines.iter().map(|l| &l.text) {
                    assert!(l.chars().count() <= cols.max(1), "a line ran past the measure: {l:?}");
                }
                assert!(g.panel.w <= w + 0.01 && g.panel.h <= h + 0.01, "panel outside the window");
                assert!(g.panel.x >= -0.01 && g.panel.y >= -0.01);
            }
        }
    }

    /// THE narrowness fix. *"the manual is way too narrow … And although
    /// wrapped, it does not re-wrap."* Widening the window must widen the panel,
    /// or re-wrapping is invisible and the two complaints are the same one.
    #[test]
    fn the_panel_grows_with_the_window_until_the_measure_is_satisfied() {
        let (cell_w, cell_h) = (8.0_f32, 18.0_f32);
        let cols_at = |w: f32| {
            let g = layout(w, 900.0, cell_w, cell_h, 1.0);
            visible_cols(g.panel.w, cell_w, 1.0)
        };
        // Through the range where the window is the binding constraint, every
        // step up in window width buys measure.
        let mut last = 0;
        for w in [400.0_f32, 600.0, 700.0, 800.0] {
            let c = cols_at(w);
            assert!(c > last, "widening to {w}px must widen the measure ({last} -> {c})");
            last = c;
        }
        // And it stops at the cap rather than handing a 4K window a 200-character
        // line: the description column is satisfied, so more width is waste.
        assert_eq!(cols_at(3840.0), measure_cap(), "a huge window settles at the cap");
        assert_eq!(cols_at(1920.0), measure_cap());
        // The cap is materially wider than the 84 columns that provoked this.
        assert!(measure_cap() >= 95, "measure cap is {} — still a ribbon", measure_cap());
    }

    /// The measure that matters is the DESCRIPTION column's, not the line's.
    /// Every column of reading matter in the document gets a comfortable measure
    /// of its own — and none gets more.
    #[test]
    fn every_column_of_reading_matter_gets_a_comfortable_measure() {
        let cols = measure_cap();
        for line in manual_lines() {
            let (text_col, key) = columns(line);
            let allotted = line_measure(line, cols) - text_col.min(line_measure(line, cols));
            if key.is_some() {
                // A key row's description: at most the comfortable measure, and —
                // because the panel is sized from the modal key column — never so
                // little that the deeper columns fall out of the readable band.
                assert!(allotted <= TEXT_MEASURE, "{line:?}: description {allotted} > {TEXT_MEASURE}");
                assert!(allotted >= 55, "{line:?}: description squeezed to {allotted}");
            } else {
                assert!(allotted <= TEXT_MEASURE, "{line:?}: prose {allotted} > {TEXT_MEASURE}");
            }
        }
        // The old whole-line cap of 84 gave the typical key row this much less.
        assert_eq!(measure_cap() - key_column(), TEXT_MEASURE);
        assert!(84 - key_column() < TEXT_MEASURE, "the old cap really was the narrower one");
    }

    /// A key row carries the split `draw` colours from; prose that merely
    /// contains a double space does not.
    #[test]
    fn a_key_row_is_split_but_prose_is_not() {
        let lines = wrapped(measure_cap());
        let keyed = lines.iter().filter(|l| l.key_end.is_some()).count();
        assert!(keyed > 90, "the manual is mostly two-column material, got {keyed}");
        for l in lines.iter().filter(|l| l.key_end.is_some()) {
            let k = l.key_end.unwrap();
            assert!(k < l.text.chars().count(), "the split must be inside the row: {l:?}");
            // `key_end` is an absolute column; the KEY_MAX budget is a width.
            let indent = l.text.chars().take_while(|c| *c == ' ').count();
            assert!(k - indent <= KEY_MAX, "{} is too wide to be a key: {:?}", k - indent, l.text);
            // Body only: a heading or the version line is never split.
            assert_eq!(l.kind, LineKind::Body, "{l:?}");
        }
        // A sentence with an accidental double space in it stays one colour.
        assert_eq!(columns("  Every change applies live and persists to  $XDG_CONFIG_HOME/rt/config.toml").1, None);
        assert_eq!(columns("  Ctrl+Shift+O    split horizontally").1, Some(14));
        assert_eq!(columns("PANES").1, None, "a heading is not a key row");
        // A continuation row hangs under the description column and is ALL
        // description — it must not be re-split as if it had a key of its own.
        let mut out = Vec::new();
        wrap_line("  Ctrl+Shift+R    rotate the enclosing split, which is a very long description indeed here", 50, LineKind::Body, &mut out);
        assert!(out.len() > 1);
        assert!(out[0].key_end.is_some(), "the first row keeps its key");
        assert!(out[1..].iter().all(|l| l.key_end.is_none()), "continuations carry no key");
    }

    /// THE flow fix: a wrapped key/description line keeps its description
    /// column instead of dumping the continuation at column zero.
    #[test]
    fn a_wrapped_description_hangs_under_its_own_column() {
        let mut out = Vec::new();
        let src = "  Ctrl+Shift+R    rotate the enclosing split 90 degrees counter-clockwise, nested splits included";
        wrap_line(src, 60, LineKind::Body, &mut out);
        assert!(out.len() > 1, "this line must wrap at 60 columns");
        let indent = |s: &str| s.chars().take_while(|c| *c == ' ').count();
        // The description column: where "rotate" starts on the source line.
        let want = src.find("rotate").unwrap();
        for l in out.iter().skip(1).map(|l| &l.text) {
            assert_eq!(indent(l), want, "continuation must hang under the description column: {l:?}");
        }
        // And an ordinary paragraph keeps its own block indent, not a hang.
        let mut out = Vec::new();
        wrap_line(
            "  One rt process serves every window and closing a window just closes it, and rt exits once the last one is gone.",
            60,
            LineKind::Body,
            &mut out,
        );
        assert!(out.len() > 1);
        for l in out.iter().map(|l| &l.text) {
            assert_eq!(indent(l), 2, "a paragraph keeps its block indent: {l:?}");
        }
    }

    /// Nothing ever runs past the measure, at any width — including the
    /// hard-break path for a word wider than the whole column.
    #[test]
    fn wrapping_keeps_every_line_within_the_width() {
        for cols in [20_usize, 33, 50, 76, 84] {
            let lines = wrapped(cols);
            for l in lines.iter().map(|l| &l.text) {
                assert!(l.chars().count() <= cols, "line wider than {cols}: {l:?}");
            }
        }
        // And the manual genuinely has lines that needed wrapping.
        assert!(manual_lines().any(|l| l.chars().count() > 50));
        // A single unbreakable word wider than the measure is chopped, not left
        // to overhang.
        let mut out = Vec::new();
        wrap_line(&"x".repeat(200), 30, LineKind::Body, &mut out);
        assert!(out.len() >= 7);
        for l in out.iter().map(|l| &l.text) {
            assert!(l.chars().count() <= 30, "{l:?}");
        }
    }

    /// Headings are distinguishable. They were drawn in body colour and body
    /// weight, which is why the manual read as one undifferentiated slab.
    #[test]
    fn section_headings_are_tagged_as_headings() {
        let lines = wrapped(84);
        let heads: Vec<&String> = lines.iter().filter(|l| l.kind == LineKind::Heading).map(|l| &l.text).collect();
        assert!(heads.len() >= 5, "the manual has several sections, got {}: {heads:?}", heads.len());
        for h in &heads {
            assert!(h.chars().filter(|c| c.is_alphabetic()).all(|c| c.is_uppercase()), "{h:?}");
            assert!(!h.starts_with(' '), "a heading is flush left: {h:?}");
        }
        assert!(heads.iter().any(|h| h.contains("PANES")), "expected a PANES section: {heads:?}");
        // Indented key rows are NOT headings, even though some are uppercase-ish.
        assert!(!is_heading("  Ctrl+Shift+O    split horizontally (stacked)"));
        assert!(!is_heading(""));
        assert!(is_heading("TABS  &  COLUMNS"));
    }

    /// Not an assertion — a way to LOOK at the wrapped page without a display,
    /// which is the only way anyone working on this can check that it flows:
    /// `cargo test -p rt --bin rt dump_the_wrapped_manual -- --ignored --nocapture`
    #[test]
    #[ignore = "prints the wrapped manual for eyeballing; asserts nothing"]
    fn dump_the_wrapped_manual() {
        for l in wrapped(measure_cap()) {
            let mark = l.key_end.map(|k| format!("key@{k}")).unwrap_or_default();
            println!("{:?}\t{mark}\t|{}|", l.kind, l.text);
        }
    }

    #[test]
    fn version_heads_the_manual() {
        let lines = wrapped(80);
        assert_eq!(lines[0].kind, LineKind::Version);
        assert!(lines[0].text.starts_with("rt "), "first line names the build: {:?}", lines[0].text);
        assert!(lines[0].text.contains(env!("CARGO_PKG_VERSION")));
    }

    /// Body lines get leading, and the row count agrees with it — a scroll of
    /// one row must move the page by exactly one drawn line.
    #[test]
    fn the_line_rhythm_includes_its_leading() {
        for &sc in &[1.0_f32, 2.0] {
            let g = layout(1200.0 * sc, 800.0 * sc, 8.0 * sc, 18.0 * sc, sc);
            assert!(g.line_h > 18.0 * sc, "body lines must have leading at {sc}x");
            assert_eq!(g.line_h, 18.0 * sc + sc * logical::MANUAL_LEADING);
            // Every visible row fits inside the panel.
            let last = g.panel.y + sc * logical::PANEL_PAD_Y + (g.rows as f32 - 1.0) * g.line_h + 18.0 * sc;
            assert!(last <= g.panel.y + g.panel.h + 0.01, "the last row overflows the panel at {sc}x");
        }
    }

    /// A window too small for a page still produces something sane.
    #[test]
    fn a_tiny_window_still_lays_out() {
        for &(w, h) in &[(1.0_f32, 1.0_f32), (60.0, 40.0), (200.0, 90.0)] {
            let g = layout(w, h, 8.0, 18.0, 1.0);
            assert!(g.rows >= 1);
            assert!(g.panel.x >= -0.01 && g.panel.y >= -0.01);
            assert!(g.panel.x + g.panel.w <= w + 0.01 && g.panel.y + g.panel.h <= h + 0.01);
            assert_eq!(clamp_scroll(usize::MAX, &g), g.total.saturating_sub(g.rows));
        }
    }
}
