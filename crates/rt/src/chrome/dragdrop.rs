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
use crate::dragdrop::ResolvedDrop;
use crate::render::Color;

/// Draw every active drag cue: the dim over the pane being dragged, the
/// drop-target highlight/caret, and the ghost chip riding the cursor.
///
/// `win` is the window's content size in pixels (window width/height is fine
/// too — only used to clamp the ghost chip on-screen). `cell` is the current
/// glyph cell size. Never panics: an empty `label` just draws a padding-only
/// chip, and a not-yet-measured (zero) `cell` skips the chip rather than
/// drawing degenerate geometry.
pub fn draw(
    backend: &mut dyn Backend,
    cue: Option<&ResolvedDrop>,
    ghost: Option<&((f32, f32), String)>,
    dim: Option<rt_core::Rect>,
    cell: (f32, f32),
    win: (f32, f32),
) {
    if cue.is_none() && ghost.is_none() && dim.is_none() {
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
            // A 3px caret between tabs, full strip height, plus little wings.
            backend.fill_rect(c.cue.x, c.cue.y, c.cue.w, c.cue.h, cue_edge);
            backend.fill_rect(c.cue.x - 3.0, c.cue.y, c.cue.w + 6.0, 3.0, cue_edge);
        } else {
            backend.fill_rect(c.cue.x, c.cue.y, c.cue.w, c.cue.h, cue_fill);
            // A 2px border so the zone reads even over busy content.
            backend.fill_rect(c.cue.x, c.cue.y, c.cue.w, 2.0, cue_edge);
            backend.fill_rect(c.cue.x, c.cue.y + c.cue.h - 2.0, c.cue.w, 2.0, cue_edge);
            backend.fill_rect(c.cue.x, c.cue.y, 2.0, c.cue.h, cue_edge);
            backend.fill_rect(c.cue.x + c.cue.w - 2.0, c.cue.y, 2.0, c.cue.h, cue_edge);
        }
    }
    if let Some(((x, y), label)) = ghost {
        // A small chip to the lower-right of the cursor with the payload
        // label. Guard against a not-yet-measured cell: skip rather than
        // paint a zero-sized/garbage chip.
        if cell.0 > 0.0 && cell.1 > 0.0 {
            let pad = 6.0;
            let w = label.chars().count() as f32 * cell.0 + 2.0 * pad;
            let h = cell.1 + 2.0 * pad;
            // Clamp fully inside the window: an un-clamped chip at the
            // cursor's lower-right would spill off-screen (and, on XRender,
            // draw into unallocated backbuffer pixels) near the right/bottom
            // edge.
            let cx = (x + 12.0).min((win.0 - w).max(0.0)).max(0.0);
            let cy = (y + 12.0).min((win.1 - h).max(0.0)).max(0.0);
            backend.fill_rect(cx, cy, w, h, Color::rgb(0x10, 0x10, 0x14).with_alpha(0.85));
            backend.fill_rect(cx, cy, w, 1.0, cue_edge);
            let text = Color::rgb(0xd0, 0xd0, 0xd8);
            for (i, ch) in label.chars().enumerate() {
                backend.draw_char(cx + pad, cy + pad, i, 0, ch, text, false, false);
            }
        }
    }

    backend.end_instrument_layer();
}
