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
    // --- context ownership (multi-window) ---------------------------------
    /// Make this backend's GL context current on ITS surface. With several
    /// windows alive there are several GL contexts, and GL/EGL calls target
    /// whatever context is current on the thread — without this, every window
    /// would render into the most recently created window's context/surface.
    /// `GlBackend` arms itself by calling this at the start of every
    /// context-dependent entry point (`begin_frame*`, `resize`,
    /// `resize_surface`, `reload_fonts`, `buffer_age`); the default is a no-op
    /// for backends with no GL context (XRender issues X requests only).
    fn make_current(&self) {}

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

    // --- anti-aliased chrome primitives (native XRender chrome, Slice 2) ---
    // Default no-ops: the GL backend never draws native chrome (it uses egui),
    // so only XRenderBackend overrides these. Coords are window pixels.
    /// Filled anti-aliased disc, alpha-composited (OVER).
    fn fill_circle(&mut self, _cx: f32, _cy: f32, _r: f32, _c: Color) {}
    /// Anti-aliased ring of the given stroke width (outer radius `r`).
    fn stroke_circle(&mut self, _cx: f32, _cy: f32, _r: f32, _width: f32, _c: Color) {}
    /// Anti-aliased thick line segment (butt caps).
    fn stroke_line(&mut self, _x0: f32, _y0: f32, _x1: f32, _y1: f32, _width: f32, _c: Color) {}

    /// Begin drawing instruments (XRender only). Between this and
    /// `end_instrument_layer`, `fill`/`fill_circle`/`stroke_*` are drawn OVER
    /// (blended into) the content back buffer, clipped to the frame scissor —
    /// there is no separate composited layer. No-op on GL (which draws
    /// instruments through its own overlay pass).
    fn begin_instrument_layer(&mut self) {}
    /// Stop drawing instruments; restore the normal (SRC) content draw mode.
    fn end_instrument_layer(&mut self) {}

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
    fn present(&mut self, window: &dyn Window, damage: Option<(PxRect, &[PxRect])>) -> bool;

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
    /// Whether this is the GL backend (`true`) or the XRender backend (`false`).
    /// All chrome is native on both now; this only distinguishes how instruments
    /// are drawn: GL repaints them inline every frame (cheap), while XRender keeps
    /// them on a persistent layer redrawn on a 6fps tick to avoid re-shipping
    /// geometry over `ssh -X`.
    fn is_gl(&self) -> bool {
        true
    }

    /// Whether this backend can cheaply scroll a rectangle of already-rendered pixels in
    /// place (a server-side blit) — true only for the XRender backend. When true, the frame
    /// planner may take the scroll-blit fast path instead of re-rendering a scrolled pane.
    fn supports_scroll_blit(&self) -> bool {
        false
    }

    /// Scroll the pixels of `rect` (in the back buffer) UP by `dy` pixels: move the sub-rect
    /// `rect` minus its top `dy` rows up to `rect`'s top, leaving the bottom `dy` rows for the
    /// caller to repaint. A no-op for backends that don't `supports_scroll_blit`. Does NOT
    /// present — the caller redraws the exposed rows and then presents `rect`.
    fn scroll_blit(&mut self, _rect: PxRect, _dy: i32) {}
}

/// Which [`Backend`] implementation to use. `Gl` is the local-rendering path,
/// `XRender` the remote-friendly one (`ssh -X`), `Wgpu` the macOS Metal path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Gl,
    XRender,
    Wgpu,
}

/// Pick the backend. See [`choose_backend_on`]; this reads the platform from
/// `cfg!` so callers do not have to. The signature is unchanged from before the
/// macOS port, so `main.rs`'s call site did not move.
pub fn choose_backend(display: Option<&str>, is_x11: bool, override_env: Option<&str>) -> BackendKind {
    choose_backend_on(display, is_x11, override_env, cfg!(target_os = "macos"))
}

/// The pure core, with the platform passed in.
///
/// Split out from [`choose_backend`] so the macOS arm is unit-testable ON LINUX
/// — the Mac is not in CI, and a `cfg!` buried in the body would make this the
/// one selection rule nothing could check.
///
/// Override wins on Linux; else Wayland/non-X11 → Gl; else a DISPLAY with a host
/// part before `:` (TCP / ssh -X forward) → XRender; a bare `:N` (local unix
/// socket) → Gl.
pub fn choose_backend_on(
    display: Option<&str>,
    is_x11: bool,
    override_env: Option<&str>,
    is_macos: bool,
) -> BackendKind {
    // Checked BEFORE the override: macOS builds contain exactly one backend, so
    // there is nothing to switch to, and `RT_BACKEND=xrender` there would select
    // a module that `cfg` removed.
    if is_macos {
        return BackendKind::Wgpu;
    }
    if let Some(o) = override_env {
        return if o.eq_ignore_ascii_case("xrender") { BackendKind::XRender } else { BackendKind::Gl };
    }
    if !is_x11 {
        return BackendKind::Gl;
    }
    match display {
        // "host:N" (host non-empty) is TCP/forwarded; ":N" is a local unix socket.
        Some(d) if d.split(':').next().map_or(false, |h| !h.is_empty()) => BackendKind::XRender,
        _ => BackendKind::Gl,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unix_socket_selects_gl() {
        assert!(matches!(choose_backend(Some(":0"), true, None), BackendKind::Gl));
        assert!(matches!(choose_backend(Some(":1.0"), true, None), BackendKind::Gl));
    }
    #[test]
    fn tcp_forwarded_selects_xrender() {
        assert!(matches!(choose_backend(Some("localhost:10.0"), true, None), BackendKind::XRender));
        assert!(matches!(choose_backend(Some("192.168.1.5:0"), true, None), BackendKind::XRender));
    }
    #[test]
    fn wayland_selects_gl() {
        assert!(matches!(choose_backend(None, false, None), BackendKind::Gl));
    }
    #[test]
    fn override_wins() {
        assert!(matches!(choose_backend(Some(":0"), true, Some("xrender")), BackendKind::XRender));
        assert!(matches!(choose_backend(Some("localhost:10.0"), true, Some("gl")), BackendKind::Gl));
    }

    #[test]
    fn macos_always_selects_wgpu() {
        // Wayland-shaped and X11-shaped inputs alike: on macOS neither exists.
        assert!(matches!(choose_backend_on(None, false, None, true), BackendKind::Wgpu));
        assert!(matches!(choose_backend_on(Some(":0"), true, None, true), BackendKind::Wgpu));
    }

    #[test]
    fn macos_ignores_the_override() {
        // macOS has exactly one backend, so there is nothing to select between.
        // Honouring "xrender" here would name a module that is cfg'd out.
        assert!(matches!(choose_backend_on(Some(":0"), true, Some("xrender"), true), BackendKind::Wgpu));
        assert!(matches!(choose_backend_on(None, false, Some("gl"), true), BackendKind::Wgpu));
    }

    #[test]
    fn linux_selection_is_unaffected_by_the_macos_arm() {
        assert!(matches!(choose_backend_on(Some(":0"), true, None, false), BackendKind::Gl));
        assert!(matches!(choose_backend_on(Some("localhost:10.0"), true, None, false), BackendKind::XRender));
        assert!(matches!(choose_backend_on(None, false, None, false), BackendKind::Gl));
        assert!(matches!(choose_backend_on(Some(":0"), true, Some("xrender"), false), BackendKind::XRender));
    }
}
