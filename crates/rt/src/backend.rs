//! Rendering backend abstraction. `draw_panes` computes WHAT to draw; a `Backend`
//! decides HOW (GL quads today via [`crate::gl_backend::GlBackend`]; XRender
//! commands in a later mechanism-C task). The drawing half of this trait mirrors
//! `render.rs`'s public API one-to-one so the GL backend can delegate verbatim.
use std::num::NonZeroU32;

use winit::window::Window;

use crate::damage::PxRect;
use crate::render::{Color, FontBlobs};

/// How to turn `draw_panes`'s per-cell/-rect calls into pixels, and how to put
/// the finished frame on screen.
///
/// The drawing methods (`begin_frame` … `end_frame`) match `render.rs`'s
/// `Renderer` signatures exactly; `GlBackend` forwards each straight through, so
/// the local GL output is byte-for-byte identical to the pre-abstraction path.
/// The present/plumbing methods below fold in the swap/Route-1 logic that used to
/// live inline in `redraw_full`/`redraw_scissored`.
pub trait Backend {
    // --- geometry / fonts -------------------------------------------------
    fn cell_size(&self) -> (f32, f32);
    fn resize(&mut self, w: f32, h: f32);
    fn reload_fonts(&mut self, blobs: &FontBlobs, font_px: f32) -> Result<(), String>;

    // --- per-frame drawing (mirrors render.rs exactly) --------------------
    fn begin_frame(&mut self, bg: Color);
    fn begin_frame_scissored(&mut self, bg: Color, bbox: PxRect);
    fn clear_scissor(&mut self);
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color);
    fn fill_cell(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color);
    fn draw_char(&mut self, ox: f32, oy: f32, col: usize, row: usize, ch: char, fg: Color, bold: bool, italic: bool);
    fn draw_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color);
    fn draw_strikeout(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color);
    fn cursor_hollow(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color);
    fn cursor_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color);
    fn cursor_beam(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color);
    fn bell_stripe(&mut self, x: f32, y: f32, w: f32, h: f32);
    fn end_frame(&mut self);

    // --- present + surface plumbing ---------------------------------------
    /// Resize the presentation surface (the GL window surface) to `w`×`h`.
    fn resize_surface(&mut self, w: NonZeroU32, h: NonZeroU32);

    /// Put the frame just drawn on screen.
    ///
    /// * `damage == None` — the full path (was `redraw_full`'s tail): X11 Route-1
    ///   full-window present if available, else `swap_buffers`. Always returns
    ///   `false`.
    /// * `damage == Some((bbox, hint_rects))` — the scissored path (was
    ///   `redraw_scissored`'s tail): X11 Route-1 `bbox` present if available, else
    ///   an EGL `swap_buffers_with_damage(hint_rects)`. Returns `true` iff the
    ///   partial present was unavailable/failed and the caller must fall back to a
    ///   full redraw followed by [`Backend::full_swap`].
    fn present(&mut self, window: &Window, damage: Option<(PxRect, &[PxRect])>) -> bool;

    /// A plain full buffer swap. Used only by the scissored path's full-redraw
    /// fallback (matching the old inline `swap_buffers` there — note this does NOT
    /// re-attempt a Route-1 present).
    fn full_swap(&mut self);

    // --- capability queries used by the frame planner ---------------------
    /// Whether the GL renderer is a software rasteriser (throttle animated chrome).
    fn is_software(&self) -> bool;
    /// Age of the back buffer in swaps (0 = unknown/fresh → must redraw all).
    fn buffer_age(&self) -> u32;
    /// Whether a partial (non-full-swap) present is available this build/surface.
    fn partial_present_available(&self) -> bool;
    /// Whether the X11 Route-1 damage-rect present path is active.
    fn x11_present_active(&self) -> bool;
}
