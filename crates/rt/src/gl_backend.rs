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
    // `RT_FRAME_SYNC=1` diagnostic: glFinish after clear / draw / present and log the
    // split under the `rt::frame` target. Off (no extra GL calls) unless the env var is set.
    sync: bool,
    sync_clear_ms: f64,
    sync_draw_ms: f64,
    sync_ticks: [(u64, u64); 4], // (llvmpipe, main) tick deltas: clear, draw, swap, finish
}

/// `RT_FRAME_SYNC` helper: CPU ticks (utime+stime) consumed so far by this process's
/// llvmpipe worker threads and by the calling (main) thread. Linux `/proc` only;
/// returns zeros elsewhere.
fn proc_ticks() -> (u64, u64) {
    #[cfg(target_os = "linux")]
    {
        let mut lp = 0u64;
        let mut main = 0u64;
        let me = unsafe { libc_gettid() };
        if let Ok(rd) = std::fs::read_dir("/proc/self/task") {
            for e in rd.flatten() {
                let Ok(s) = std::fs::read_to_string(e.path().join("stat")) else { continue };
                // comm is in parens and may hold spaces; split after the closing paren
                let Some(close) = s.rfind(')') else { continue };
                let comm = &s[s.find('(').map_or(0, |i| i + 1)..close];
                let f: Vec<&str> = s[close + 1..].split_whitespace().collect();
                // fields after ')' : state(0) ppid(1) ... utime(11) stime(12)
                let t = f.get(11).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0)
                    + f.get(12).and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                if comm.starts_with("llvmpipe") {
                    lp += t;
                } else if e.file_name().to_string_lossy() == me.to_string() {
                    main = t;
                }
            }
        }
        (lp, main)
    }
    #[cfg(not(target_os = "linux"))]
    {
        (0, 0)
    }
}

#[cfg(target_os = "linux")]
unsafe fn libc_gettid() -> i64 {
    extern "C" {
        fn gettid() -> i32;
    }
    gettid() as i64
}

impl GlBackend {
    /// Wrap a freshly-built `Renderer` and its GL surface/context as a `Backend`.
    /// Probes for the X11 Route-1 present handle exactly as the old `Active`
    /// construction did (`None` on Wayland / a no-x11 build).
    pub fn new(
        renderer: Renderer,
        surface: Surface<WindowSurface>,
        context: PossiblyCurrentContext,
        #[cfg_attr(not(feature = "x11"), allow(unused_variables))] window: &dyn Window,
    ) -> Self {
        GlBackend {
            renderer,
            surface,
            context,
            sync: std::env::var_os("RT_FRAME_SYNC").is_some(),
            sync_clear_ms: 0.0,
            sync_draw_ms: 0.0,
            sync_ticks: [(0, 0); 4],
            #[cfg(feature = "x11")]
            x11_present: crate::x11_present::X11Present::try_new(window),
        }
    }

    /// Mutable access to the underlying GL renderer for any GL-only path.
    #[allow(dead_code)] // provided for GL-only use / tests; not all builds call it
    pub fn renderer_mut(&mut self) -> &mut Renderer {
        &mut self.renderer
    }

    /// Make OUR context current on OUR surface unless it already is. With one
    /// window this is a cheap `is_current()` check and nothing else — identical
    /// behaviour to before multi-window. With several windows it re-targets the
    /// thread's GL state at this window before any GL/EGL work; every
    /// context-dependent `Backend` entry point calls it first (see the trait
    /// doc), and all other GL calls happen strictly between a `begin_frame*`
    /// and the following `present` within one synchronous redraw, so nothing
    /// can slip through on another window's context.
    fn ensure_current(&self) {
        if !self.context.is_current() {
            if let Err(e) = self.context.make_current(&self.surface) {
                log::error!("make_current failed: {e}"); // GL calls will mistarget; keep running
            }
        }
    }

    /// Present a scissored frame via EGL `swap_buffers_with_damage`. Moved verbatim
    /// from the old `App::present_with_damage`. Returns `true` on a successful
    /// partial-damage swap; `false` if the surface is not EGL or the swap errored
    /// (the caller then runs a full redraw + [`full_swap`](Self::full_swap)).
    /// `rects` are physical px, top-left origin; converted to EGL's bottom-left
    /// origin here.
    fn present_with_damage(&mut self, window: &dyn Window, rects: &[PxRect]) -> bool {
        let screen_h = window.surface_size().height as i32;
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
    fn make_current(&self) {
        self.ensure_current()
    }
    fn cell_size(&self) -> (f32, f32) {
        self.renderer.cell_size()
    }
    fn resize(&mut self, w: f32, h: f32) {
        self.ensure_current(); // glViewport targets the current context
        self.renderer.resize(w, h)
    }
    fn reload_fonts(&mut self, blobs: &FontBlobs, font_px: f32) -> Result<(), String> {
        self.ensure_current(); // atlas textures are rebuilt in THIS context
        self.renderer.reload_fonts(blobs, font_px)
    }

    fn begin_frame(&mut self, bg: Color) {
        self.ensure_current(); // frame chokepoint: draw/end_frame/present follow synchronously
        let t0 = self.sync.then(|| (std::time::Instant::now(), proc_ticks()));
        self.renderer.begin_frame(bg);
        if let Some((t0, k0)) = t0 {
            self.renderer.finish();
            self.sync_clear_ms = t0.elapsed().as_secs_f64() * 1000.0;
            self.sync_draw_ms = 0.0;
            let k1 = proc_ticks();
            self.sync_ticks = [(k1.0 - k0.0, k1.1 - k0.1), (0, 0), (0, 0), (0, 0)];
        }
    }
    fn begin_frame_scissored(&mut self, bg: Color, bbox: PxRect) {
        self.ensure_current(); // frame chokepoint (partial path)
        let t0 = self.sync.then(|| (std::time::Instant::now(), proc_ticks()));
        self.renderer.begin_frame_scissored(bg, bbox);
        if let Some((t0, k0)) = t0 {
            self.renderer.finish();
            self.sync_clear_ms = t0.elapsed().as_secs_f64() * 1000.0;
            self.sync_draw_ms = 0.0;
            let k1 = proc_ticks();
            self.sync_ticks = [(k1.0 - k0.0, k1.1 - k0.1), (0, 0), (0, 0), (0, 0)];
        }
    }
    fn begin_frame_scissored_rects(&mut self, bg: Color, bbox: PxRect, rects: &[PxRect]) {
        // Too many rects → the bbox: each rect is a scissored clear + a draw call,
        // and past a point the per-draw overhead beats the fragment savings.
        const MAX_SCISSOR_RECTS: usize = 24;
        if rects.is_empty() || rects.len() > MAX_SCISSOR_RECTS {
            return self.begin_frame_scissored(bg, bbox);
        }
        self.ensure_current(); // frame chokepoint (partial path)
        let t0 = self.sync.then(|| (std::time::Instant::now(), proc_ticks()));
        self.renderer.begin_frame_scissored_rects(bg, rects);
        if let Some((t0, k0)) = t0 {
            self.renderer.finish();
            self.sync_clear_ms = t0.elapsed().as_secs_f64() * 1000.0;
            self.sync_draw_ms = 0.0;
            let k1 = proc_ticks();
            self.sync_ticks = [(k1.0 - k0.0, k1.1 - k0.1), (0, 0), (0, 0), (0, 0)];
        }
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
    // AA chrome primitives for the native instruments (previously egui-only on GL).
    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, c: Color) {
        self.renderer.fill_circle(cx, cy, r, c)
    }
    fn stroke_circle(&mut self, cx: f32, cy: f32, r: f32, width: f32, c: Color) {
        self.renderer.stroke_circle(cx, cy, r, width, c)
    }
    fn stroke_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Color) {
        self.renderer.stroke_line(x0, y0, x1, y1, width, c)
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
        let t0 = self.sync.then(|| (std::time::Instant::now(), proc_ticks()));
        self.renderer.end_frame();
        if let Some((t0, k0)) = t0 {
            self.renderer.finish();
            self.sync_draw_ms += t0.elapsed().as_secs_f64() * 1000.0;
            let k1 = proc_ticks();
            self.sync_ticks[1].0 += k1.0 - k0.0;
            self.sync_ticks[1].1 += k1.1 - k0.1;
            // RT_FRAME_SYNC=2: is glFinish a real barrier? Sleep and count worker ticks.
            if std::env::var_os("RT_FRAME_SYNC").is_some_and(|v| v == "2") {
                std::thread::sleep(std::time::Duration::from_millis(400));
                let k2 = proc_ticks();
                log::debug!(target: "rt::frame", "gl-sync gap-after-draw-finish 400ms [lp{} m{}]", k2.0 - k1.0, k2.1 - k1.1);
            }
        }
    }
    fn frame_verts(&self) -> Option<usize> {
        Some(self.renderer.frame_verts())
    }

    fn resize_surface(&mut self, w: NonZeroU32, h: NonZeroU32) {
        self.ensure_current(); // resize OUR surface, not whichever context is current
        self.surface.resize(&self.context, w, h); // resize GL surface
    }

    fn present(&mut self, window: &dyn Window, damage: Option<(PxRect, &[PxRect])>) -> bool {
        let t0 = self.sync.then(|| (std::time::Instant::now(), proc_ticks()));
        let r = self.present_inner(window, damage);
        if let Some((t0, k0)) = t0 {
            let swap_ms = t0.elapsed().as_secs_f64() * 1000.0;
            let k1 = proc_ticks();
            let t1 = std::time::Instant::now();
            self.renderer.finish();
            let finish_ms = t1.elapsed().as_secs_f64() * 1000.0;
            let k2 = proc_ticks();
            self.sync_ticks[2] = (k1.0 - k0.0, k1.1 - k0.1);
            self.sync_ticks[3] = (k2.0 - k1.0, k2.1 - k1.1);
            let t = self.sync_ticks;
            log::debug!(
                target: "rt::frame",
                "gl-sync clear={:.1}ms[lp{} m{}] draw={:.1}ms[lp{} m{}] swap={:.1}ms[lp{} m{}] finish={:.1}ms[lp{} m{}]",
                self.sync_clear_ms, t[0].0, t[0].1,
                self.sync_draw_ms, t[1].0, t[1].1,
                swap_ms, t[2].0, t[2].1,
                finish_ms, t[3].0, t[3].1
            );
        }
        r
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
        // EGL_BUFFER_AGE can only be queried while the surface is current on
        // the calling thread — and redraw() plans damage (calling this) BEFORE
        // the begin_frame chokepoint runs.
        self.ensure_current();
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

impl GlBackend {
    /// The real `present` (timing wrapper above). Full path: Route-1 X11 present or
    /// `swap_buffers`; partial: Route-1 bbox present or EGL partial swap.
    fn present_inner(&mut self, window: &dyn Window, damage: Option<(PxRect, &[PxRect])>) -> bool {
        match damage {
            // --- full path (was redraw_full's tail), verbatim ---------------
            None => {
                #[cfg(feature = "x11")]
                if let Some(p) = self.x11_present.as_ref() {
                    let sz = window.surface_size();
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
                    let sh = window.surface_size().height as i32;
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
}
