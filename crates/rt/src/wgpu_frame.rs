//! The frame-sequencing decisions of the macOS wgpu backend, as plain data.
//!
//! `wgpu_backend.rs` is `cfg(target_os = "macos")` and needs a live Metal device to
//! construct, so nothing in it can be exercised by Linux CI — and the bug this module
//! exists for (geometry queued into a frame that was never presented, then drawn at the
//! front of the NEXT frame, underneath everything painted since) has now been introduced
//! twice through two different exits from `end_frame`. This module is deliberately NOT
//! `cfg`'d: it holds no wgpu types, so it compiles and its tests run everywhere, which
//! is the only automated coverage the decision can have.
//!
//! `wgpu_backend.rs` keeps the wgpu calls and nothing else: it asks [`end_frame_action`]
//! what to do and does it.
// Off macOS the only caller is this file's own test module, which `cargo build` does not
// compile -- that is the whole point of the module being platform-independent, so the
// resulting dead_code warning is noise, not a finding.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

/// What `WgpuBackend::end_frame` must do this call.
///
/// The three inputs are: whether `begin_frame` managed to acquire a drawable
/// (`has_frame`), whether this frame's clear has been applied yet (`needs_clear` — true
/// only for the FIRST `end_frame` of a frame), and how many vertices `TextPipeline` has
/// queued since the last flush.
///
/// `main.rs`'s `redraw_full` calls `end_frame` TWICE per frame by design: once after
/// `draw_panes` and again after `paint_overlays_or_instruments`, which batches the
/// menu/preferences geometry on top. Every call must flush what accumulated since the
/// previous one, in painter's order — that is the contract `GlBackend::end_frame` meets
/// by simply re-flushing its vertex buffer both times.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EndFrameAction {
    /// There is no drawable to paint into: `begin_frame` could not acquire one
    /// (`nextDrawable` returns nil when the layer is occluded or the window
    /// miniaturised, and after a GPU reset). The queued geometry belongs to a frame
    /// that will never be presented and must be **dropped**, not kept: `TextPipeline`'s
    /// vertex buffer is a single painter's-order list with no frame boundary in it, so
    /// anything left behind is drawn FIRST in the next frame that does acquire — one
    /// frame late and underneath everything painted since, double-blending through rt's
    /// translucent background. Dropping it costs nothing: this backend reports no damage
    /// support, so `main.rs` repaints the whole window every frame anyway.
    ///
    /// It is also the only bound on the vertex buffer across repeated skips. A skipped
    /// frame has no vsync backpressure (`get_current_texture` returns immediately rather
    /// than blocking), so an occluded window would otherwise accumulate a grid of
    /// vertices per frame until `flush`'s doubling passed `max_buffer_size` (256 MB),
    /// where `create_buffer` raises a wgpu validation error and the default handler
    /// panics.
    DiscardGeometry,
    /// A drawable exists, the clear is already on it, and nothing new has been queued.
    /// Opening a pass would draw nothing, so skip the whole command buffer.
    Skip,
    /// Open one render pass and submit. `clear` is true for the first pass of the frame
    /// — it carries `begin_frame`'s clear as its `LoadOp` — and false afterwards, where
    /// the attachment must be LOADED so the clear (and any earlier pass's geometry) is
    /// preserved rather than erased.
    Submit { clear: bool },
}

/// Decide what `end_frame` does. See [`EndFrameAction`].
///
/// The first call of a frame always submits even with nothing queued: its pass is what
/// applies the clear, and returning early would leave the window showing the previous
/// frame. A later call submits only if there is new geometry.
pub fn end_frame_action(has_frame: bool, needs_clear: bool, pending_verts: usize) -> EndFrameAction {
    if !has_frame {
        // Nothing can be drawn into this frame, so nothing may be carried out of it.
        return EndFrameAction::DiscardGeometry;
    }
    if needs_clear || pending_verts > 0 {
        EndFrameAction::Submit { clear: needs_clear }
    } else {
        EndFrameAction::Skip
    }
}

/// How a pass puts rt's background colour onto the attachment.
///
/// This is a separate decision from [`end_frame_action`] because it depends on
/// something `end_frame_action` cannot see: whether the frame is scissored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackgroundOp {
    /// `LoadOp::Clear(bg)`. Correct only for a frame that owns the whole
    /// surface, because a load-op clear covers the entire attachment.
    ClearAll,
    /// `LoadOp::Load`, then paint `bg` as one screen-covering quad with blending
    /// OFF, so the scissor rect clips it. Same pixels as a clear, only inside
    /// the damage rect.
    LoadAndPaint,
    /// `LoadOp::Load` and nothing else: a later pass of a frame whose background
    /// is already on the attachment.
    LoadOnly,
}

/// Decide how this pass lays down the background. `owns_background` is
/// [`EndFrameAction::Submit`]'s `clear` (true only for the first pass of a
/// frame); `scissored` is whether `begin_frame_scissored` set a damage rect.
///
/// The rule is one sentence: **a scissor cannot clip a load-op clear.** A
/// `LoadOp` is the tile initialiser — it runs before any fragment exists, over
/// the whole attachment — while `set_scissor_rect` only ever discards fragments.
/// So a scissored frame that asks for `LoadOp::Clear` blanks the entire drawable
/// and then repaints only the damage rect, wiping every pane outside it.
///
/// The scissored answer therefore loads, and paints the background as a quad
/// instead, which the scissor *does* clip. It has to be painted with blending
/// **off** rather than through rt's normal alpha-blended pipeline: rt's
/// background is deliberately translucent (that is what lets the macOS vibrancy
/// layer show through), and blending a translucent colour over the previous
/// frame composites with it instead of replacing it — the damage rect would
/// darken a little more on every frame it was redrawn. With blending off the
/// fragment's RGBA is written straight to the attachment, which is exactly what
/// a clear does.
///
/// The unscissored case keeps the load-op clear: on a tile-based deferred GPU
/// that initialises tile memory instead of reading the previous drawable in, and
/// it is the path every macOS frame actually takes today.
pub fn background_op(owns_background: bool, scissored: bool) -> BackgroundOp {
    match (owns_background, scissored) {
        (false, _) => BackgroundOp::LoadOnly,
        (true, false) => BackgroundOp::ClearAll,
        (true, true) => BackgroundOp::LoadAndPaint,
    }
}

/// Turn a damage rectangle into the `(x, y, w, h)` quadruple
/// [`wgpu::RenderPass::set_scissor_rect`] takes, clamped to a `surface_w` x
/// `surface_h` attachment.
///
/// `PxRect` is signed and rt's damage rects legitimately go negative or overhang
/// the surface — border bands inset by `BORDER_PX`, a scroll-blit shifts a
/// content rect up by whole lines, and damage recorded before a window shrank is
/// unioned into the next frame's plan by `plan_frame`'s history ring. The GL
/// backend never had to care: `glScissor` takes signed ints and silently clamps
/// to the drawable. wgpu's `set_scissor_rect` takes **u32** and validates
/// `x + width <= attachment width`, so a straight `r.x as u32` turns -10 into
/// 4294967286 and the frame dies in a validation panic rather than drawing a
/// slightly wrong rectangle.
///
/// So the clamp is not cosmetic — it is the whole difference between the two
/// APIs, and it is done here in i64 because `r.x + r.w` overflows i32 for a
/// sufficiently silly rect (which panics in a debug build before wgpu ever sees
/// it). An empty result is a legitimate answer, not an error: the pass still
/// runs and simply draws nothing, which is correct for damage that no longer
/// intersects the surface.
pub fn scissor_rect(r: crate::damage::PxRect, surface_w: u32, surface_h: u32) -> (u32, u32, u32, u32) {
    let clamp = |lo: i64, len: i64, limit: u32| -> (u32, u32) {
        let limit = limit as i64;
        let x0 = lo.clamp(0, limit);
        // `lo + len` in i64: the i32 sum can overflow, and `len` may be negative
        // (a degenerate rect), in which case `x1 < x0` and the max() floors the
        // extent at zero rather than letting the subtraction go negative.
        let x1 = lo.saturating_add(len).clamp(0, limit);
        (x0 as u32, (x1 - x0).max(0) as u32)
    };
    let (x, w) = clamp(r.x as i64, r.w as i64, surface_w);
    let (y, h) = clamp(r.y as i64, r.h as i64, surface_h);
    (x, y, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::damage::PxRect;

    #[test]
    fn a_frame_that_never_acquired_a_drawable_drops_its_geometry() {
        // THE regression. `start()` failed to acquire (occluded layer, miniaturised
        // window, GPU reset), so `frame` is None -- but `redraw_full` cannot know that:
        // it has already run `draw_panes`, pushing a whole grid of vertices. The old
        // `end_frame` began `let Some(frame) = self.frame.as_ref() else { return }`,
        // bailing BEFORE `TextPipeline::flush` -- and `flush` is the only thing that
        // ever clears the vertex buffer. Those vertices then sat at the FRONT of the
        // buffer and were drawn first by the next frame that did acquire: one frame
        // late, underneath everything painted since. Asserting `Skip` here (the old
        // "just return" behaviour) is what fails.
        assert_eq!(end_frame_action(false, true, 4096), EndFrameAction::DiscardGeometry);
        // Same with nothing queued, and same on a later call of the same frame: the
        // answer never depends on how the frame was lost.
        assert_eq!(end_frame_action(false, true, 0), EndFrameAction::DiscardGeometry);
        assert_eq!(end_frame_action(false, false, 900), EndFrameAction::DiscardGeometry);
    }

    #[test]
    fn the_first_call_of_a_frame_carries_the_clear_even_with_no_geometry() {
        // Nothing was drawn this frame, but the clear still has to reach the drawable
        // -- otherwise the window shows the previous frame's content.
        assert_eq!(end_frame_action(true, true, 0), EndFrameAction::Submit { clear: true });
    }

    #[test]
    fn a_later_call_with_pending_geometry_still_flushes_it_into_this_frame() {
        // `redraw_full`'s second `end_frame`, after `paint_overlays_or_instruments`
        // batched the menu/preferences geometry. It must flush into the CURRENT frame,
        // and it must LOAD rather than clear -- clearing here would erase the panes
        // that the first call just drew.
        assert_eq!(end_frame_action(true, false, 3), EndFrameAction::Submit { clear: false });
    }

    #[test]
    fn a_later_call_with_nothing_pending_costs_no_command_buffer() {
        // The common case on the second call: egui self-flushes, so nothing was
        // batched. Opening a pass would draw nothing.
        assert_eq!(end_frame_action(true, false, 0), EndFrameAction::Skip);
    }

    #[test]
    fn exactly_one_pass_per_frame_clears() {
        // Finding 2: the clear must ride on the first pass that actually runs, not on a
        // pass of its own. Walk a whole frame the way `redraw_full` does and check the
        // load ops in order -- one Clear, then Load for the rest. A second Clear would
        // erase the panes; a first Load would leave the previous frame showing through.
        let mut needs_clear = true;
        let mut loads = Vec::new();
        for pending in [12_000_usize, 240, 0] {
            match end_frame_action(true, needs_clear, pending) {
                EndFrameAction::Submit { clear } => {
                    loads.push(clear);
                    needs_clear = false;
                }
                EndFrameAction::Skip => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(loads, vec![true, false], "one clear, then loads");
    }

    // ---------------------------------------------------------------------
    // `background_op` — a scissor cannot clip a load-op clear.
    // ---------------------------------------------------------------------

    #[test]
    fn a_scissored_frame_must_not_reach_for_a_load_op_clear() {
        // THE bug. `begin_frame_scissored` records the same `needs_clear` an
        // unscissored `begin_frame` does, and `end_frame` turns it into
        // `LoadOp::Clear` on the pass's colour attachment. A `LoadOp` is not a
        // draw: it is the tile initialiser, it runs before any fragment exists,
        // and `set_scissor_rect` — which only ever discards fragments — cannot
        // touch it. So a partial redraw of a 200x20 damage rect would clear the
        // WHOLE 3024x1964 drawable to the background and then repaint 200x20 of
        // it: every pane outside the damage rect blanked, every frame.
        //
        // Asserting `ClearAll` here (today's behaviour) is what fails.
        assert_eq!(background_op(true, true), BackgroundOp::LoadAndPaint);
    }

    #[test]
    fn an_unscissored_frame_still_clears_the_cheap_way() {
        // The full-redraw path must be untouched: a load-op clear is free on a
        // tile-based GPU (it initialises tile memory instead of reading the
        // previous contents in), and rt takes this path on every macOS frame.
        // Replacing it with a painted quad would add a full-surface blend for
        // no reason.
        assert_eq!(background_op(true, false), BackgroundOp::ClearAll);
    }

    #[test]
    fn a_later_pass_never_lays_the_background_down_twice() {
        // `redraw_full`/`redraw_scissored` call `end_frame` twice. The second
        // pass must LOAD whatever the first drew — scissored or not. Clearing
        // again erases the panes; painting the background quad again would too.
        assert_eq!(background_op(false, false), BackgroundOp::LoadOnly);
        assert_eq!(background_op(false, true), BackgroundOp::LoadOnly);
    }

    #[test]
    fn a_whole_scissored_frame_lays_the_background_down_exactly_once() {
        // Walk a scissored frame the way `redraw_scissored` does and check the
        // background ops in order. The pairing with `end_frame_action` is the
        // part that matters: `clear` means "this pass owns the frame's
        // background", and only the first pass may act on it.
        let mut needs_clear = true;
        let mut ops = Vec::new();
        for pending in [900_usize, 40, 0] {
            match end_frame_action(true, needs_clear, pending) {
                EndFrameAction::Submit { clear } => {
                    ops.push(background_op(clear, true));
                    needs_clear = false;
                }
                EndFrameAction::Skip => {}
                other => panic!("unexpected {other:?}"),
            }
        }
        assert_eq!(ops, vec![BackgroundOp::LoadAndPaint, BackgroundOp::LoadOnly]);
    }

    // ---------------------------------------------------------------------
    // `scissor_rect` — the i32 -> u32 cast that panics instead of clamping.
    // ---------------------------------------------------------------------

    #[test]
    fn a_negative_scissor_origin_clamps_to_the_attachment_instead_of_wrapping() {
        // THE bug. `PxRect` is i32 and rt's damage rects legitimately go
        // negative: `pane_border_rects` insets by BORDER_PX, the scroll-blit
        // path shifts a content rect up by whole lines, and a window shrink
        // leaves history rects hanging off the top-left. `glScissor` takes
        // signed ints and clamps, so the GL backend never noticed; wgpu's
        // `set_scissor_rect` takes u32 and VALIDATES `x + width <= attachment
        // width`, so `-10 as u32` == 4294967286 is not a stale pixel, it is a
        // validation error and a panic in the middle of a frame.
        assert_eq!(
            scissor_rect(PxRect { x: -10, y: -5, w: 100, h: 50 }, 800, 600),
            (0, 0, 90, 45),
            "a rect that starts off the top-left must be trimmed, not wrapped"
        );
    }

    #[test]
    fn a_scissor_past_the_right_or_bottom_edge_is_trimmed() {
        // The other half of the same validation rule: `x + width` must not
        // exceed the attachment. A window that shrank between the damage being
        // recorded and the frame being drawn produces exactly this.
        assert_eq!(scissor_rect(PxRect { x: 700, y: 500, w: 400, h: 400 }, 800, 600), (700, 500, 100, 100));
        assert_eq!(scissor_rect(PxRect { x: 0, y: 0, w: 800, h: 600 }, 800, 600), (0, 0, 800, 600), "an exact fit is left alone");
    }

    #[test]
    fn a_scissor_entirely_outside_the_attachment_is_empty_not_negative() {
        // Trimming must never produce a negative width (which would wrap to ~4
        // billion all over again). A rect wholly off any edge scissors to zero
        // area: the pass runs and draws nothing, which is the correct answer
        // for damage that no longer intersects the surface.
        assert_eq!(scissor_rect(PxRect { x: 900, y: 10, w: 50, h: 50 }, 800, 600), (800, 10, 0, 50));
        assert_eq!(scissor_rect(PxRect { x: -200, y: -200, w: 100, h: 100 }, 800, 600), (0, 0, 0, 0));
    }

    #[test]
    fn a_degenerate_or_absurd_rect_cannot_wrap_or_overflow() {
        // `w`/`h` are i32 and `DamageAccumulator` drops empty rects, but the
        // scissor is the last line of defence and must be total: a negative
        // extent is zero area, and `x + w` must not be computed in i32 (it
        // overflows, which panics in a debug build).
        assert_eq!(scissor_rect(PxRect { x: 10, y: 10, w: -5, h: -5 }, 800, 600), (10, 10, 0, 0));
        assert_eq!(scissor_rect(PxRect { x: i32::MAX, y: i32::MAX, w: i32::MAX, h: i32::MAX }, 800, 600), (800, 600, 0, 0));
        assert_eq!(scissor_rect(PxRect { x: i32::MIN, y: i32::MIN, w: i32::MAX, h: i32::MAX }, 800, 600), (0, 0, 0, 0));
    }
}
