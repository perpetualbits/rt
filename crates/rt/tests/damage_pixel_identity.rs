// The offscreen pixel-identity gate needs a real GL context via EGL, which macOS
// does not provide (no EGL, no pkg-config). Its dev-dependencies are target-gated
// to match, so on macOS this file must vanish entirely rather than fail to link.
#![cfg(not(target_os = "macos"))]

//! Correctness gate for damage-based rendering: a scissored single-cell redraw
//! must produce a byte-identical framebuffer to a full redraw. Damage may change
//! only HOW MUCH is drawn, never WHAT ends up on screen.
//!
//! This needs a live GL context, so it is `#[ignore]`d by default:
//!
//!   cargo test -p rt --test damage_pixel_identity -- --ignored
//!
//! Run it on any box with EGL + a GL driver (llvmpipe is fine — that is exactly
//! the software path we optimise). It builds a *headless* OpenGL context via the
//! `EGL_MESA_platform_surfaceless` platform (no window opens) and renders into an
//! offscreen RGBA8 framebuffer object, then reads it back with `glReadPixels`.
//!
//! We drive the renderer's public `bell_stripe` to fill a cell-sized rect rather
//! than a glyph: the property under test (scissored redraw == full redraw) is
//! independent of *what* is drawn in the cell, and a filled rect avoids all the
//! glyph/atlas/font plumbing while still exercising the same clear + draw path.

use rt_app::damage::PxRect;
use rt_app::render::{Color, FontBlobs, Renderer};

const W: i32 = 320;
const H: i32 = 200;

// ---------------------------------------------------------------------------
// Headless EGL + offscreen framebuffer helper.
// ---------------------------------------------------------------------------
mod egl_headless {
    use glow::HasContext;
    use khronos_egl as egl;
    use std::sync::Arc;

    // Mesa's surfaceless platform enum (EGL_PLATFORM_SURFACELESS_MESA). Lets us
    // obtain an EGL display with no window-system connection at all.
    const PLATFORM_SURFACELESS_MESA: egl::Enum = 0x31DD;

    type Egl = egl::Instance<egl::Static>;

    /// A live headless GL context plus everything that must outlive it (the EGL
    /// instance, display and context, and the offscreen framebuffer objects).
    /// Dropping this in the wrong order would tear the context down under GL, so
    /// callers keep it alive for the whole test.
    pub struct Ctx {
        gl: Arc<glow::Context>,
        egl: Egl,
        display: egl::Display,
        context: egl::Context,
        _fbo: glow::Framebuffer,
        _rbo: glow::Renderbuffer,
    }

    impl Ctx {
        pub fn glow(&self) -> Arc<glow::Context> {
            self.gl.clone()
        }
    }

    impl Drop for Ctx {
        fn drop(&mut self) {
            // Release the context before terminating the display.
            let _ = self.egl.make_current(self.display, None, None, None);
            let _ = self.egl.destroy_context(self.display, self.context);
            let _ = self.egl.terminate(self.display);
        }
    }

    /// Build a surfaceless OpenGL 3.3-core context at `w×h` and a matching
    /// offscreen RGBA8 framebuffer bound as the render target.
    pub fn make(w: u32, h: u32) -> Result<Ctx, String> {
        let egl = egl::Instance::new(egl::Static);

        // Surfaceless display: no X11/Wayland connection needed.
        let display = unsafe {
            egl.get_platform_display(PLATFORM_SURFACELESS_MESA, egl::DEFAULT_DISPLAY, &[egl::ATTRIB_NONE])
        }
        .map_err(|e| format!("eglGetPlatformDisplay(surfaceless) failed: {e:?}"))?;

        egl.initialize(display).map_err(|e| format!("eglInitialize failed: {e:?}"))?;
        // The renderer's shader is `#version 330 core` desktop GL, so bind the
        // desktop OpenGL API (not GLES).
        egl.bind_api(egl::OPENGL_API).map_err(|e| format!("eglBindAPI(OpenGL) failed: {e:?}"))?;

        // Pick an OpenGL-renderable RGBA8 config. Try a pbuffer-capable config
        // first, then relax the surface-type constraint (we render to an FBO with
        // no EGL surface, so the surface type is not actually load-bearing).
        let config = choose_config(&egl, display, true)
            .or_else(|| choose_config(&egl, display, false))
            .ok_or("no OpenGL-renderable EGL config found")?;

        let ctx_attribs = [
            egl::CONTEXT_MAJOR_VERSION,
            3,
            egl::CONTEXT_MINOR_VERSION,
            3,
            egl::CONTEXT_OPENGL_PROFILE_MASK,
            egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
            egl::NONE,
        ];
        let context = egl
            .create_context(display, config, None, &ctx_attribs)
            .map_err(|e| format!("eglCreateContext failed: {e:?}"))?;

        // Surfaceless make-current (EGL_KHR_surfaceless_context): no draw/read
        // surface; we render into an FBO instead.
        egl.make_current(display, None, None, Some(context))
            .map_err(|e| format!("eglMakeCurrent(surfaceless) failed: {e:?}"))?;

        let gl = unsafe {
            glow::Context::from_loader_function(|s| match egl.get_proc_address(s) {
                Some(f) => f as *const std::ffi::c_void,
                None => std::ptr::null(),
            })
        };

        // Offscreen RGBA8 framebuffer as the render target.
        let (fbo, rbo) = unsafe {
            let rbo = gl.create_renderbuffer().map_err(|e| format!("create_renderbuffer: {e}"))?;
            gl.bind_renderbuffer(glow::RENDERBUFFER, Some(rbo));
            gl.renderbuffer_storage(glow::RENDERBUFFER, glow::RGBA8, w as i32, h as i32);
            let fbo = gl.create_framebuffer().map_err(|e| format!("create_framebuffer: {e}"))?;
            gl.bind_framebuffer(glow::FRAMEBUFFER, Some(fbo));
            gl.framebuffer_renderbuffer(glow::FRAMEBUFFER, glow::COLOR_ATTACHMENT0, glow::RENDERBUFFER, Some(rbo));
            let status = gl.check_framebuffer_status(glow::FRAMEBUFFER);
            if status != glow::FRAMEBUFFER_COMPLETE {
                return Err(format!("offscreen FBO incomplete: 0x{status:x}"));
            }
            gl.viewport(0, 0, w as i32, h as i32);
            (fbo, rbo)
        };

        Ok(Ctx { gl: Arc::new(gl), egl, display, context, _fbo: fbo, _rbo: rbo })
    }

    /// Choose an OpenGL-renderable RGBA8 config, optionally constrained to
    /// pbuffer-capable surface types.
    fn choose_config(egl: &Egl, display: egl::Display, want_pbuffer: bool) -> Option<egl::Config> {
        let mut attribs = vec![
            egl::RENDERABLE_TYPE,
            egl::OPENGL_BIT,
            egl::RED_SIZE,
            8,
            egl::GREEN_SIZE,
            8,
            egl::BLUE_SIZE,
            8,
            egl::ALPHA_SIZE,
            8,
        ];
        if want_pbuffer {
            attribs.push(egl::SURFACE_TYPE);
            attribs.push(egl::PBUFFER_BIT);
        }
        attribs.push(egl::NONE);
        egl.choose_first_config(display, &attribs).ok().flatten()
    }

    /// Read the whole framebuffer back as RGBA8 (top-left → bottom-right row
    /// order is GL's bottom-up, but that is irrelevant here: we compare two
    /// buffers read the identical way).
    pub fn read_pixels(gl: &glow::Context, w: i32, h: i32) -> Vec<u8> {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        unsafe {
            gl.read_pixels(0, 0, w, h, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut buf)));
        }
        buf
    }
}

#[test]
#[ignore = "needs a live GL context; run with --ignored on a GL-capable box"]
fn scissored_single_cell_equals_full_redraw() {
    let ctx = match egl_headless::make(W as u32, H as u32) {
        Ok(c) => c,
        Err(e) => panic!("could not create a headless GL context: {e}\n\
             This test needs EGL + a GL driver (llvmpipe is fine). Run it on a \
             GL-capable box (desktop with Mesa, or the milkv)."),
    };
    let gl = ctx.glow();

    // A minimal FontBlobs from a common system TTF: `Renderer::new` needs a
    // parseable primary font to measure the cell, even though `bell_stripe`
    // draws no glyphs. (Mirrors what `main.rs`'s fontdb path yields, without
    // pulling fontdb into the test.)
    let blobs = test_fonts();
    let mut r = Renderer::new(gl.clone(), &blobs, 16.0).expect("renderer");
    r.resize(W as f32, H as f32);

    let bg = Color::rgb(0x10, 0x10, 0x18).with_alpha(1.0);

    let (cw, ch) = r.cell_size();
    let (cw, ch) = (cw as i32, ch as i32);
    // Target cell: col 2, row 1. Its physical-pixel bbox (top-left origin).
    let (col, row) = (2, 1);
    let cell = PxRect { x: col * cw, y: row * ch, w: cw, h: ch };
    let (fx, fy) = (cell.x as f32, cell.y as f32);
    let (fw, fh) = (cw as f32, ch as f32);

    // --- Baseline: a bg-only frame, so we can prove the stripe actually draws
    // (otherwise an all-bg-vs-all-bg comparison would pass vacuously even if GL
    // rendered nothing). ---
    r.begin_frame(bg);
    r.end_frame();
    let bg_px = egl_headless::read_pixels(&gl, W, H);

    // --- Reference (full redraw): clear whole buffer to bg, draw the stripe. ---
    r.begin_frame(bg);
    r.bell_stripe(fx, fy, fw, fh);
    r.end_frame();
    let full_px = egl_headless::read_pixels(&gl, W, H);

    // Non-vacuity: the stripe must have changed pixels vs the bg-only baseline,
    // confirming the GL path really rendered.
    let drawn = bg_px.iter().zip(&full_px).filter(|(a, b)| a != b).count();
    assert!(drawn > 0, "stripe drew nothing: GL rendering appears to be a no-op");

    // --- Partial (scissored on top of a prior frame). ---
    // 1. The "before" frame: whole buffer bg, nothing at the cell.
    r.begin_frame(bg);
    r.end_frame();
    // 2. Scissor to just the cell, clear it to bg and draw the same stripe.
    r.begin_frame_scissored(bg, cell);
    r.bell_stripe(fx, fy, fw, fh);
    r.end_frame();
    r.clear_scissor();
    let partial_px = egl_headless::read_pixels(&gl, W, H);

    assert_eq!(full_px.len(), partial_px.len(), "framebuffer sizes differ");
    let diffs = full_px.iter().zip(&partial_px).filter(|(a, b)| a != b).count();
    assert_eq!(diffs, 0, "{diffs} bytes differ between full and scissored redraw");
}

/// Multi-rect partial redraw: two disjoint cells scissored as TWO rects (not
/// their bounding box) must equal a full redraw, and the untouched pixels
/// between them must keep the prior frame's content. This is the property the
/// software-GL path relies on: a keystroke plus the pane's border bands must
/// clear and draw only those rects, never the whole bounding box.
#[test]
#[ignore = "needs a live GL context; run with --ignored on a GL-capable box"]
fn scissored_multi_rect_equals_full_redraw() {
    let ctx = match egl_headless::make(W as u32, H as u32) {
        Ok(c) => c,
        Err(e) => panic!("could not create a headless GL context: {e}"),
    };
    let gl = ctx.glow();
    let blobs = test_fonts();
    let mut r = Renderer::new(gl.clone(), &blobs, 16.0).expect("renderer");
    r.resize(W as f32, H as f32);
    let bg = Color::rgb(0x10, 0x10, 0x18).with_alpha(1.0);
    let other = Color::rgb(0x40, 0x20, 0x20).with_alpha(1.0); // a distinct "stale" colour
    let (cw, ch) = r.cell_size();
    let (cw, ch) = (cw as i32, ch as i32);
    let a = PxRect { x: cw, y: ch, w: cw, h: ch };
    let b = PxRect { x: 6 * cw, y: 3 * ch, w: cw, h: ch };
    let stripe = |r: &mut Renderer, c: PxRect| r.bell_stripe(c.x as f32, c.y as f32, c.w as f32, c.h as f32);

    // Reference: full redraw over `bg` with both stripes.
    r.begin_frame(bg);
    stripe(&mut r, a);
    stripe(&mut r, b);
    r.end_frame();
    let full_px = egl_headless::read_pixels(&gl, W, H);

    // Prior frame: whole buffer bg, nothing at a or b.
    r.begin_frame(bg);
    r.end_frame();
    // Partial: clear + draw ONLY a and b (two scissor rects), same geometry.
    r.begin_frame_scissored_rects(bg, &[a, b]);
    stripe(&mut r, a);
    stripe(&mut r, b);
    r.end_frame();
    r.clear_scissor();
    let partial_px = egl_headless::read_pixels(&gl, W, H);
    let diffs = full_px.iter().zip(&partial_px).filter(|(x, y)| x != y).count();
    assert_eq!(diffs, 0, "{diffs} bytes differ between full and multi-rect scissored redraw");

    // Preservation: paint the whole buffer `other`, then a two-rect partial over
    // it. Pixels outside a and b must still be `other` — i.e. the bbox between the
    // rects was NOT cleared. (A bbox scissor would clear it to bg.)
    r.begin_frame(other);
    r.end_frame();
    r.begin_frame_scissored_rects(bg, &[a, b]);
    stripe(&mut r, a);
    stripe(&mut r, b);
    r.end_frame();
    r.clear_scissor();
    let px = egl_headless::read_pixels(&gl, W, H);
    // A pixel strictly between the two rects (inside their bounding box).
    let (mx, my) = (3 * cw + cw / 2, 2 * ch + ch / 2);
    let gl_row = H - 1 - my; // read_pixels is bottom-up
    let i = ((gl_row * W + mx) * 4) as usize;
    let want = [0x40u8, 0x20, 0x20];
    assert_eq!(&px[i..i + 3], &want, "pixel inside the bbox but outside the rects was clobbered");
}

/// Real glyphs through both cache paths (ASCII table and the non-ASCII map):
/// a per-cell partial redraw of "Hé" + bold H must equal the full redraw, and
/// repeating the draw (now cache hits) must produce the same pixels.
#[test]
#[ignore = "needs a live GL context; run with --ignored on a GL-capable box"]
fn glyphs_via_both_cache_paths_are_pixel_identical() {
    let ctx = match egl_headless::make(W as u32, H as u32) {
        Ok(c) => c,
        Err(e) => panic!("could not create a headless GL context: {e}"),
    };
    let gl = ctx.glow();
    let blobs = test_fonts();
    let mut r = Renderer::new(gl.clone(), &blobs, 16.0).expect("renderer");
    r.resize(W as f32, H as f32);
    let bg = Color::rgb(0x10, 0x10, 0x18).with_alpha(1.0);
    let fg = Color::rgb(0xe0, 0xe0, 0xe0).with_alpha(1.0);
    let (cw, ch) = r.cell_size();
    let (cwi, chi) = (cw as i32, ch as i32);
    let text = |r: &mut Renderer| {
        r.draw_char(0.0, 0.0, 2, 1, 'H', fg, false, false); // ASCII path
        r.draw_char(0.0, 0.0, 3, 1, 'é', fg, false, false); // map path
        r.draw_char(0.0, 0.0, 4, 1, 'H', fg, true, false); // ASCII, bold slot
    };
    r.begin_frame(bg);
    text(&mut r);
    r.end_frame();
    let first = egl_headless::read_pixels(&gl, W, H);
    r.begin_frame(bg);
    text(&mut r); // all cache hits now
    r.end_frame();
    let second = egl_headless::read_pixels(&gl, W, H);
    assert_eq!(first, second, "cache hits drew different pixels than the first rasterisation");
    r.begin_frame(bg);
    r.end_frame();
    let bg_only = egl_headless::read_pixels(&gl, W, H);
    assert!(first.iter().zip(&bg_only).any(|(a, b)| a != b), "no glyph pixels were drawn");
    // Partial: prior frame bg, then only the three cells as three rects.
    r.begin_frame(bg);
    r.end_frame();
    let cells: Vec<PxRect> = (2..5).map(|c| PxRect { x: c * cwi, y: chi, w: cwi, h: chi }).collect();
    r.begin_frame_scissored_rects(bg, &cells);
    text(&mut r);
    r.end_frame();
    r.clear_scissor();
    let partial = egl_headless::read_pixels(&gl, W, H);
    let diffs = first.iter().zip(&partial).filter(|(a, b)| a != b).count();
    assert_eq!(diffs, 0, "{diffs} bytes differ between full and per-cell scissored glyph redraw");
}

/// Build a `FontBlobs` from the first readable common monospace TTF. Only the
/// regular chain must be non-empty for `Renderer::new` (it measures the cell
/// from the primary face); `bell_stripe` itself rasterises no glyphs.
fn test_fonts() -> FontBlobs {
    const CANDIDATES: &[&str] = &[
        "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
        "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
        "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
        "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
        "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
    ];
    for p in CANDIDATES {
        if let Ok(bytes) = std::fs::read(p) {
            return FontBlobs { regular: vec![bytes.into()], ..Default::default() };
        }
    }
    panic!("no test font found; install DejaVu Sans Mono or adjust CANDIDATES");
}
