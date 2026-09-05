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

#[cfg(test)]
mod tests {
    use super::*;

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
}
