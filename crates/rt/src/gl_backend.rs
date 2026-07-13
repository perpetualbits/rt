//! The default rendering backend: today's GL glyph-atlas [`Renderer`] plus the GL
//! present resources it needs — the drawing surface, the current context, and the
//! optional X11 Route-1 present handle (all moved out of `Active`).
//!
//! Every [`Backend`] drawing method delegates verbatim to `Renderer`, and
//! `present`/`full_swap` hold the exact `swap_buffers`/Route-1 logic that used to
//! sit inline in `redraw_full`/`redraw_scissored`. No GL call is added, dropped,
//! or reordered, so the on-screen output is byte-for-byte identical to the
//! pre-abstraction path.
use std::num::NonZeroU32;

use glutin::context::PossiblyCurrentContext;
use glutin::prelude::*; // GlSurface: swap_buffers / buffer_age / resize
use glutin::surface::Rect as GlRect; // EGL damage rect (bottom-left origin)
use glutin::surface::{Surface, WindowSurface};
use winit::window::Window;

use crate::backend::Backend;
use crate::damage::PxRect;
use crate::render::{Color, FontBlobs, Renderer};

/// The GL backend: the renderer plus everything needed to present its frames.
pub struct GlBackend {
    renderer: Renderer,               // the glyph-atlas renderer (drawing)
    surface: Surface<WindowSurface>,  // the GL drawing surface
    context: PossiblyCurrentContext,  // the current GL context
    #[cfg(feature = "x11")]
    x11_present: Option<crate::x11_present::X11Present>, // Route 1: X11 damage-rect present
}

impl GlBackend {
    /// Wrap a freshly-built `Renderer` and its GL surface/context as a `Backend`.
    /// Probes for the X11 Route-1 present handle exactly as the old `Active`
    /// construction did (`None` on Wayland / a no-x11 build).
    pub fn new(
        renderer: Renderer,
        surface: Surface<WindowSurface>,
        context: PossiblyCurrentContext,
        #[cfg_attr(not(feature = "x11"), allow(unused_variables))] window: &Window,
    ) -> Self {
        GlBackend {
            renderer,
            surface,
            context,
            #[cfg(feature = "x11")]
            x11_present: crate::x11_present::X11Present::try_new(window),
        }
    }

    /// Mutable access to the underlying GL renderer for any GL-only path.
    #[allow(dead_code)] // provided for GL-only use / tests; not all builds call it
    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// Present a scissored frame via EGL `swap_buffers_with_damage`. Moved verbatim
    /// from the old `App::present_with_damage`. Returns `true` on a successful
    /// partial-damage swap; `false` if the surface is not EGL or the swap errored
    /// (the caller then runs a full redraw + [`full_swap`](Self::full_swap)).
    /// `rects` are physical px, top-left origin; converted to EGL's bottom-left
    /// origin here.
    fn present_with_damage(&mut self, window: &Window, rects: &[PxRect]) -> bool {
        let screen_h = window.inner_size().height as i32;
        let egl_rects: Vec<GlRect> = rects
            .iter()
            .map(|r| {
                let y = screen_h - (r.y + r.h); // flip to bottom-left origin
                GlRect::new(r.x, y, r.w, r.h)
            })
            .collect();

        // swap_buffers_with_damage is only on the concrete EGL surface/context.
        match (&self.surface, &self.context) {
            (Surface::Egl(egl_surface), PossiblyCurrentContext::Egl(egl_ctx)) => {
                match egl_surface.swap_buffers_with_damage(egl_ctx, &egl_rects) {
                    Ok(()) => true,
                    Err(e) => {
                        log::warn!("swap_buffers_with_damage failed ({e}); full swap next frame");
                        false
                    }
                }
            }
            _ => false, // GLX / other backend: Phase 2 territory
        }
    }
}

impl Backend for GlBackend {
    fn cell_size(&self) -> (f32, f32) {
        self.renderer.cell_size()
    }
    fn resize(&mut self, w: f32, h: f32) {
        self.renderer.resize(w, h)
    }
    fn reload_fonts(&mut self, blobs: &FontBlobs, font_px: f32) -> Result<(), String> {
        self.renderer.reload_fonts(blobs, font_px)
    }

    fn begin_frame(&mut self, bg: Color) {
        self.renderer.begin_frame(bg)
    }
    fn begin_frame_scissored(&mut self, bg: Color, bbox: PxRect) {
        self.renderer.begin_frame_scissored(bg, bbox)
    }
    fn clear_scissor(&mut self) {
        self.renderer.clear_scissor()
    }
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        self.renderer.fill_rect(x, y, w, h, c)
    }
    fn fill_cell(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        self.renderer.fill_cell(ox, oy, col, row, color)
    }
    fn draw_char(&mut self, ox: f32, oy: f32, col: usize, row: usize, ch: char, fg: Color, bold: bool, italic: bool) {
        self.renderer.draw_char(ox, oy, col, row, ch, fg, bold, italic)
    }
    fn draw_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        self.renderer.draw_underline(ox, oy, col, row, color)
    }
    fn draw_strikeout(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        self.renderer.draw_strikeout(ox, oy, col, row, color)
    }
    fn cursor_hollow(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        self.renderer.cursor_hollow(ox, oy, col, row, color)
    }
    fn cursor_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        self.renderer.cursor_underline(ox, oy, col, row, color)
    }
    fn cursor_beam(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        self.renderer.cursor_beam(ox, oy, col, row, color)
    }
    fn bell_stripe(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.renderer.bell_stripe(x, y, w, h)
    }
    fn end_frame(&mut self) {
        self.renderer.end_frame()
    }

    fn resize_surface(&mut self, w: NonZeroU32, h: NonZeroU32) {
        self.surface.resize(&self.context, w, h); // resize GL surface
    }

    fn present(&mut self, window: &Window, damage: Option<(PxRect, &[PxRect])>) -> bool {
        match damage {
            // --- full path (was redraw_full's tail), verbatim ---------------
            None => {
                #[cfg(feature = "x11")]
                if let Some(p) = self.x11_present.as_ref() {
                    let sz = window.inner_size();
                    let (w, h) = (sz.width as i32, sz.height as i32);
                    if p.present_rect(self.renderer.gl_ctx(), 0, 0, w, h, h) {
                        return false; // presented the full window via XPutImage; no swap
                    }
                    // present failed → fall through to swap_buffers
                }
                if let Err(e) = self.surface.swap_buffers(&self.context) {
                    log::error!("swap_buffers failed: {e}"); // non-fatal; log and continue
                }
                false
            }
            // --- scissored path (was redraw_scissored's tail), verbatim -----
            Some((bbox, hint_rects)) => {
                #[cfg(not(feature = "x11"))]
                let _ = bbox; // bbox is only consumed by the (cfg'd-out) Route-1 branch
                #[cfg(feature = "x11")]
                if let Some(p) = self.x11_present.as_ref() {
                    let sh = window.inner_size().height as i32;
                    if p.present_rect(self.renderer.gl_ctx(), bbox.x, bbox.y, bbox.w, bbox.h, sh) {
                        return false; // presented the damage rect via XPutImage; no swap, no re-arm
                    }
                    // present failed → fall through to the full-redraw fallback
                }
                // EGL partial swap; if it isn't available/fails, tell the caller to
                // run a full redraw + full swap this frame (and force full next).
                !self.present_with_damage(window, hint_rects)
            }
        }
    }

    fn full_swap(&mut self) {
        if let Err(e) = self.surface.swap_buffers(&self.context) {
            log::error!("swap_buffers failed: {e}");
        }
    }

    fn is_software(&self) -> bool {
        self.renderer.is_software()
    }

    fn buffer_age(&self) -> u32 {
        self.surface.buffer_age()
    }

    fn partial_present_available(&self) -> bool {
        if matches!(self.surface, Surface::Egl(_)) {
            return true; // mechanism A (buffer_age partial swap)
        }
        #[cfg(feature = "x11")]
        if self.x11_present.is_some() {
            return true; // Route 1 (readback + XPutImage)
        }
        false
    }

    fn x11_present_active(&self) -> bool {
        #[cfg(feature = "x11")]
        {
            self.x11_present.is_some()
        }
        #[cfg(not(feature = "x11"))]
        {
            false
        }
    }
}
