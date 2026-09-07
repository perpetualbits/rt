//! Drag-and-drop cues: the "put new tile here?" highlight, the tab-insert
//! caret, the ghost chip riding the cursor, and the dim over the dragged pane.
//! Pure Backend primitives so GL and XRender render identically.
//!
//! XRender alpha note: outside an instrument layer, `Backend::fill_rect` on
//! `XRenderBackend` writes straight into the opaque content buffer with
//! `PictOp::SRC` (see `xrender_backend.rs`'s `fill`) — the destination has no
//! alpha channel there, so a translucent colour would come out fully opaque
//! instead of blended. `begin_instrument_layer`/`end_instrument_layer` switch
//! `fill_rect`/`draw_char` to `PictOp::OVER` with premultiplied alpha, baking
//! straight into the back buffer — the same mechanism the native instruments
//! already use to blend over content. Bracketing this whole painter in that
//! layer gets correct translucency on XRender; both calls are default no-ops
//! on `GlBackend` (which always blends via `glBlendFunc`), so no backend
//! branch is needed here.
use crate::backend::Backend;
use crate::chrome_scale::logical;
use crate::dragdrop::ResolvedDrop;
use crate::render::Color;

/// Everything one frame's drag cues consist of, gathered so the painter takes a
/// cue set rather than a growing list of parallel `Option`s.
#[derive(Default)]
pub struct Cues<'a> {
    /// rt's own pane/tab drag: where a release would land, plus its rect.
    pub drop: Option<&'a ResolvedDrop>,
    /// Text dragged in from another application: the whole receiving pane.
    pub text: Option<rt_core::Rect>,
    /// The chip riding the cursor: (position, label). Shared by both gestures —
    /// they can never be live at once.
    pub ghost: Option<&'a ((f32, f32), String)>,
    /// The pane being dragged, drawn dimmed at the place it came from.
    pub dim: Option<rt_core::Rect>,
}

/// Draw every active drag cue: the dim over the pane being dragged, the
/// drop-target highlight/caret, and the ghost chip riding the cursor.
///
/// `win` is the window's content size in pixels (window width/height is fine
/// too — only used to clamp the ghost chip on-screen). `cell` is the current
/// glyph cell size. Never panics: an empty `label` just draws a padding-only
/// chip, and a not-yet-measured (zero) `cell` skips the chip rather than
/// drawing degenerate geometry.
///
/// `sc` is the display's backing factor. The cue RECTANGLES arrive already
/// scaled — `dragdrop::resolve_drop` computes them from scaled bounds and a
/// scaled `EDGE_STRIP` — so what `sc` is for here is the flat decoration drawn
/// ON them: the caret's wings, the zone border, the ghost chip's padding and
/// offset. Both halves take the same factor, so the cue the user sees still
/// marks exactly the region the drop resolver accepted.
///
/// `text_cue` is the SAME cue for a different gesture: text dragged in from
/// another application (`crate::textdrop`), where the payload is bytes rather
/// than a pane and the target is always a whole pane. It is drawn with the
/// zone fill + border, identical to a pane drop zone, on purpose — a drop cue
/// should look like a drop cue whatever is being dropped. The two can never be
/// live at once (`App::chrome_busy` refuses a foreign drop while rt's own drag
/// is running), so they share the `ghost` chip rather than stacking two.
pub fn draw(backend: &mut dyn Backend, cues: Cues<'_>, cell: (f32, f32), win: (f32, f32), sc: f32) {
    let Cues { drop: cue, text: text_cue, ghost, dim } = cues;
    if cue.is_none() && text_cue.is_none() && ghost.is_none() && dim.is_none() {
        return;
    }
    // See the module doc: this whole painter runs inside an instrument layer
    // so XRender blends it translucently instead of overwriting the content
    // buffer opaque. No-op on GL.
    backend.begin_instrument_layer();

    let cue_fill = Color::rgb(0x4a, 0x7a, 0xc8).with_alpha(0.30);
    let cue_edge = Color::rgb(0x4a, 0x7a, 0xc8).with_alpha(0.90);
    let dim_col = Color::rgb(0x00, 0x00, 0x00).with_alpha(0.35);

    if let Some(r) = dim {
        backend.fill_rect(r.x, r.y, r.w, r.h, dim_col); // the pane being dragged
    }
    if let Some(c) = cue {
        if c.caret {
            // A thin caret between tabs, full strip height, plus little wings.
            let wing = sc * logical::DROP_CARET_WING;
            backend.fill_rect(c.cue.x, c.cue.y, c.cue.w, c.cue.h, cue_edge);
            backend.fill_rect(c.cue.x - wing, c.cue.y, c.cue.w + 2.0 * wing, wing, cue_edge);
        } else {
            backend.fill_rect(c.cue.x, c.cue.y, c.cue.w, c.cue.h, cue_fill);
            // A border so the zone reads even over busy content.
            let e = sc * logical::DROP_ZONE_EDGE;
            backend.fill_rect(c.cue.x, c.cue.y, c.cue.w, e, cue_edge);
            backend.fill_rect(c.cue.x, c.cue.y + c.cue.h - e, c.cue.w, e, cue_edge);
            backend.fill_rect(c.cue.x, c.cue.y, e, c.cue.h, cue_edge);
            backend.fill_rect(c.cue.x + c.cue.w - e, c.cue.y, e, c.cue.h, cue_edge);
        }
    }
    if let Some(r) = text_cue {
        // A whole-pane zone, drawn exactly like the pane-drop zone above.
        let e = sc * logical::DROP_ZONE_EDGE;
        backend.fill_rect(r.x, r.y, r.w, r.h, cue_fill);
        backend.fill_rect(r.x, r.y, r.w, e, cue_edge);
        backend.fill_rect(r.x, r.y + r.h - e, r.w, e, cue_edge);
        backend.fill_rect(r.x, r.y, e, r.h, cue_edge);
        backend.fill_rect(r.x + r.w - e, r.y, e, r.h, cue_edge);
    }
    if let Some(((x, y), label)) = ghost {
        // A small chip to the lower-right of the cursor with the payload
        // label. Guard against a not-yet-measured cell: skip rather than
        // paint a zero-sized/garbage chip.
        if cell.0 > 0.0 && cell.1 > 0.0 {
            let pad = sc * logical::GHOST_PAD;
            let w = label.chars().count() as f32 * cell.0 + 2.0 * pad;
            let h = cell.1 + 2.0 * pad;
            // Clamp fully inside the window: an un-clamped chip at the
            // cursor's lower-right would spill off-screen (and, on XRender,
            // draw into unallocated backbuffer pixels) near the right/bottom
            // edge.
            let off = sc * logical::GHOST_OFFSET;
            let cx = (x + off).min((win.0 - w).max(0.0)).max(0.0);
            let cy = (y + off).min((win.1 - h).max(0.0)).max(0.0);
            backend.fill_rect(cx, cy, w, h, Color::rgb(0x10, 0x10, 0x14).with_alpha(0.85));
            backend.fill_rect(cx, cy, w, sc * logical::HAIRLINE, cue_edge);
            let text = Color::rgb(0xd0, 0xd0, 0xd8);
            for (i, ch) in label.chars().enumerate() {
                backend.draw_char(cx + pad, cy + pad, i, 0, ch, text, false, false);
            }
        }
    }

    backend.end_instrument_layer();
}
