// Like `damage_pixel_identity.rs`, this needs a real GL context via EGL, which
// macOS does not provide (no EGL, no pkg-config). Its dev-dependencies are
// target-gated to match, so on macOS this file must vanish entirely.
#![cfg(not(target_os = "macos"))]

//! Gate for the OpenGL **ES** shader path (`render::ShaderDialect::Es300`).
//!
//! rt is meant to run on the tiniest Linux systems, and a good number of those
//! have a GPU whose vendor driver offers OpenGL ES *only* — the StarFive JH7110's
//! PowerVR on `pvrsrvkm` is the case that prompted this, where rt's old
//! `#version 330 core` shaders died with "Syntax error, version 330 not
//! supported". No CI machine has PowerVR, so the stand-in is Mesa's software
//! GLES: an EGL context created with `eglBindAPI(EGL_OPENGL_ES_API)` and
//! `EGL_OPENGL_ES3_BIT`, which llvmpipe serves as a genuine GLES 3.x
//! implementation — a different GLSL front-end from the desktop one, which is
//! exactly the part that has to be proven.
//!
//! Two things are asserted:
//!
//!   1. `Renderer::new` succeeds on a GLES 3.0 context at all — i.e. the runtime
//!      dialect detection picked `#version 300 es`, the precision declarations
//!      satisfy GLSL ES, and every GL call in `render.rs` (VAOs, sized `R8`
//!      storage, `texture()`, a user-declared fragment output) is inside ES 3.0.
//!   2. The frame it draws is **byte-identical** to the one the desktop GL 3.3
//!      path draws from the same calls. The two dialects share one shader body,
//!      so this is what proves the headers alone did not change the arithmetic —
//!      and it is what would catch a `mediump` slipping in and quantising the
//!      atlas UVs.
//!
//! Needs a live GL driver, so it is `#[ignore]`d by default:
//!
//!   cargo test -p rt --test gles_shader_path -- --ignored
//!
//! (`LIBGL_ALWAYS_SOFTWARE=1` is a reasonable belt-and-braces on a box with a
//! real GPU, but the surfaceless platform already lands on llvmpipe here.)

use glow::HasContext; // `version()` — the runtime dialect signal under test
use rt_app::render::{Color, FontBlobs, Renderer, ShaderDialect};

const W: i32 = 320;
const H: i32 = 200;

// ---------------------------------------------------------------------------
// Headless EGL, for either client API.
// ---------------------------------------------------------------------------
mod egl_headless {
    use glow::HasContext;
    use khronos_egl as egl;
    use std::sync::Arc;

    // Mesa's surfaceless platform enum (EGL_PLATFORM_SURFACELESS_MESA). Lets us
    // obtain an EGL display with no window-system connection at all.
    const PLATFORM_SURFACELESS_MESA: egl::Enum = 0x31DD;

    type Egl = egl::Instance<egl::Static>;

    /// Which client API to build the context for. The whole point of this test
    /// file is that these two take different paths through the driver's GLSL
    /// compiler, so they are chosen explicitly rather than left to a default.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub enum Api {
        /// Desktop OpenGL 3.3 core — rt's long-standing path.
        Desktop,
        /// OpenGL ES 3.0 — the path a PowerVR/Mali/Adreno board gives you.
        Gles3,
    }

    /// A live headless GL context plus everything that must outlive it. Dropping
    /// this releases the context and terminates the display, so a second `make`
    /// for the other API starts from a clean slate in the same process.
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

    /// Build a surfaceless context for `api` at `w×h` and a matching offscreen
    /// RGBA8 framebuffer bound as the render target.
    pub fn make(api: Api, w: u32, h: u32) -> Result<Ctx, String> {
        let egl = egl::Instance::new(egl::Static);

        // Surfaceless display: no X11/Wayland connection needed.
        let display = unsafe {
            egl.get_platform_display(PLATFORM_SURFACELESS_MESA, egl::DEFAULT_DISPLAY, &[egl::ATTRIB_NONE])
        }
        .map_err(|e| format!("eglGetPlatformDisplay(surfaceless) failed: {e:?}"))?;

        egl.initialize(display).map_err(|e| format!("eglInitialize failed: {e:?}"))?;

        let (client_api, renderable_bit) = match api {
            Api::Desktop => (egl::OPENGL_API, egl::OPENGL_BIT),
            Api::Gles3 => (egl::OPENGL_ES_API, egl::OPENGL_ES3_BIT),
        };
        egl.bind_api(client_api).map_err(|e| format!("eglBindAPI({api:?}) failed: {e:?}"))?;

        // Pick a renderable RGBA8 config. Try a pbuffer-capable config first,
        // then relax the surface-type constraint (we render to an FBO with no EGL
        // surface, so the surface type is not actually load-bearing).
        let config = choose_config(&egl, display, renderable_bit, true)
            .or_else(|| choose_config(&egl, display, renderable_bit, false))
            .ok_or_else(|| format!("no {api:?}-renderable EGL config found"))?;

        // GLES takes no profile mask — that attribute is desktop-GL-only and an
        // ES context creation would reject it.
        let ctx_attribs: Vec<egl::Int> = match api {
            Api::Desktop => vec![
                egl::CONTEXT_MAJOR_VERSION,
                3,
                egl::CONTEXT_MINOR_VERSION,
                3,
                egl::CONTEXT_OPENGL_PROFILE_MASK,
                egl::CONTEXT_OPENGL_CORE_PROFILE_BIT,
                egl::NONE,
            ],
            Api::Gles3 => vec![egl::CONTEXT_MAJOR_VERSION, 3, egl::CONTEXT_MINOR_VERSION, 0, egl::NONE],
        };
        let context = egl
            .create_context(display, config, None, &ctx_attribs)
            .map_err(|e| format!("eglCreateContext({api:?}) failed: {e:?}"))?;

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

        // Offscreen RGBA8 framebuffer as the render target. RGBA8 renderbuffer
        // storage and RGBA/UNSIGNED_BYTE readback are both core in ES 3.0, so
        // this is the same code for either API.
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

    /// Choose a config renderable by `renderable_bit` with RGBA8 colour,
    /// optionally constrained to pbuffer-capable surface types.
    fn choose_config(
        egl: &Egl,
        display: egl::Display,
        renderable_bit: egl::Int,
        want_pbuffer: bool,
    ) -> Option<egl::Config> {
        let mut attribs = vec![
            egl::RENDERABLE_TYPE,
            renderable_bit,
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

    /// Read the whole framebuffer back as RGBA8.
    pub fn read_pixels(gl: &glow::Context, w: i32, h: i32) -> Vec<u8> {
        let mut buf = vec![0u8; (w * h * 4) as usize];
        unsafe {
            gl.read_pixels(0, 0, w, h, glow::RGBA, glow::UNSIGNED_BYTE, glow::PixelPackData::Slice(Some(&mut buf)));
        }
        buf
    }
}

use egl_headless::Api;

/// Draw one representative frame: a background clear, a solid rect (which
/// samples the atlas's opaque seed texel) and a line of real glyphs (which
/// samples rasterised coverage — the precision-sensitive part). Returns the
/// framebuffer.
fn render_reference_frame(gl: std::sync::Arc<glow::Context>) -> Result<Vec<u8>, String> {
    let mut r = Renderer::new(gl.clone(), &test_fonts(), 16.0)?;
    r.resize(W as f32, H as f32);

    let bg = Color::rgb(0x10, 0x10, 0x18).with_alpha(1.0);
    let fg = Color::rgb(0xd0, 0xd0, 0xc8);

    r.begin_frame(bg);
    // A solid quad: exercises the seed-texel path (flat colour through the same
    // shader), including the premultiplied output.
    r.fill_rect(8.0, 8.0, 64.0, 24.0, Color::rgb(0x40, 0x80, 0xc0));
    // Real glyphs: atlas UVs into a 1024² texture, sampled with NEAREST. This is
    // the draw that a `mediump` varying would quantise into the wrong texel.
    for (i, ch) in "rt on GLES 300 es — 0123".chars().enumerate() {
        r.draw_char(0.0, 48.0, i, 0, ch, fg, false, false);
    }
    // A partly-transparent fill over the glyphs, so the blend path is in too.
    r.fill_rect(4.0, 100.0, 200.0, 20.0, Color::rgb(0xa0, 0x20, 0x20).with_alpha(0.5));
    r.end_frame();

    Ok(egl_headless::read_pixels(&gl, W, H))
}

/// The whole point: a GLES-3.0-only driver must be able to build and drive the
/// renderer, and must produce the same pixels as desktop GL.
#[test]
#[ignore = "needs a live GL driver; run with --ignored on a GL-capable box"]
fn gles3_renders_identically_to_desktop_gl() {
    // GLES first, so a failure here is reported as the GLES failure it is rather
    // than being masked by an unrelated desktop-GL problem.
    let gles_px = {
        let ctx = match egl_headless::make(Api::Gles3, W as u32, H as u32) {
            Ok(c) => c,
            Err(e) => panic!(
                "could not create a headless GLES 3.0 context: {e}\n\
                 This test needs EGL + a GLES-capable driver (Mesa/llvmpipe is fine)."
            ),
        };
        let gl = ctx.glow();
        // Prove we are actually testing the ES front-end and not silently
        // getting a desktop context back — the assertion the whole file rests on.
        let v = gl.version().clone();
        assert!(v.is_embedded, "expected an OpenGL ES context, got desktop GL {}.{}", v.major, v.minor);
        assert!(v.major >= 3, "expected GLES 3.0+, got GLES {}.{}", v.major, v.minor);
        // The detection half of the fix: from THIS context's reported version,
        // the renderer must pick the ES dialect. (Asserted separately from the
        // compile because Mesa is lenient about `#version 330` in an ES context —
        // see `es_dialect_is_what_this_context_selects` below — so a successful
        // compile alone would not prove the right source was used.)
        assert_eq!(
            ShaderDialect::pick(v.major, v.minor, v.is_embedded),
            Some(ShaderDialect::Es300),
            "GLES {}.{} must select the `#version 300 es` shaders",
            v.major,
            v.minor
        );
        // `Renderer::new` compiles the shaders; on a `#version 330 core` source
        // this is exactly where a PowerVR board died.
        render_reference_frame(gl).expect("renderer on GLES 3.0")
    };

    let desktop_px = {
        let ctx = egl_headless::make(Api::Desktop, W as u32, H as u32)
            .expect("headless desktop OpenGL 3.3 context");
        render_reference_frame(ctx.glow()).expect("renderer on desktop GL")
    };

    // Non-vacuity: the frame must contain something other than the clear colour,
    // or two blank buffers would compare equal and prove nothing.
    let bg_bytes = [0x10u8, 0x10, 0x18, 0xff];
    let non_bg = desktop_px.chunks_exact(4).filter(|px| px != &bg_bytes).count();
    assert!(non_bg > 0, "the reference frame drew nothing: GL rendering appears to be a no-op");

    assert_eq!(gles_px.len(), desktop_px.len(), "framebuffer sizes differ");
    let diffs = gles_px.iter().zip(&desktop_px).filter(|(a, b)| a != b).count();
    assert_eq!(diffs, 0, "{diffs} of {} bytes differ between the GLES and desktop-GL frames", desktop_px.len());
}

/// Compile one shader stage in the current context, returning the driver's info
/// log on failure.
fn try_compile(gl: &glow::Context, kind: u32, src: &str) -> Result<(), String> {
    unsafe {
        let sh = gl.create_shader(kind).map_err(|e| format!("create_shader: {e}"))?;
        gl.shader_source(sh, src);
        gl.compile_shader(sh);
        let ok = gl.get_shader_compile_status(sh);
        let log = gl.get_shader_info_log(sh);
        gl.delete_shader(sh);
        if ok {
            Ok(())
        } else {
            Err(log)
        }
    }
}

/// The `#version 300 es` sources must compile on a real GLES front-end, stage by
/// stage — the direct analogue of what the PowerVR driver refused to do with
/// `#version 330 core`.
///
/// This also records how lenient the *stand-in* driver is. Mesa/llvmpipe happens
/// to accept desktop `#version 330 core` in an ES context; the vendor drivers
/// this fix exists for do not. So a "the desktop source fails here" assertion
/// would be asserting a Mesa quirk, and instead the desktop result is only
/// printed — the load-bearing assertion is that the ES source compiles.
#[test]
#[ignore = "needs a live GL driver; run with --ignored on a GL-capable box"]
fn es_dialect_is_what_this_context_selects() {
    let ctx = match egl_headless::make(Api::Gles3, 16, 16) {
        Ok(c) => c,
        Err(e) => panic!("could not create a headless GLES 3.0 context: {e}"),
    };
    let gl = ctx.glow();
    let v = gl.version().clone();
    assert!(v.is_embedded, "expected an OpenGL ES context, got desktop GL {}.{}", v.major, v.minor);
    eprintln!("GLES context: {}.{} ({})", v.major, v.minor, v.vendor_info);

    let es = ShaderDialect::Es300;
    try_compile(&gl, glow::VERTEX_SHADER, &es.vertex_source()).expect("`#version 300 es` vertex shader");
    try_compile(&gl, glow::FRAGMENT_SHADER, &es.fragment_source()).expect("`#version 300 es` fragment shader");

    // Informational: what this driver does with the desktop source.
    let desktop = ShaderDialect::Core330;
    match try_compile(&gl, glow::FRAGMENT_SHADER, &desktop.fragment_source()) {
        Ok(()) => eprintln!(
            "note: this driver also accepts `#version 330 core` in an ES context \
             (Mesa is lenient here; PowerVR/pvrsrvkm is not — that rejection is the bug this fixes)"
        ),
        Err(log) => eprintln!("this driver rejects `#version 330 core` in an ES context, as expected: {}", log.trim()),
    }
}

/// Both dialects must pass a **strict** GLSL front-end.
///
/// This is the one check here that does not need a GPU at all, so it is NOT
/// `#[ignore]`d — it runs in the ordinary `cargo test` whenever `glslangValidator`
/// is on PATH, and prints why it skipped when it is not. It matters because the
/// two drivers available to this project (Mesa and NVIDIA) both *accept*
/// desktop `#version 330 core` inside an ES context, so neither can tell us
/// whether the ES source is really valid ESSL 3.00. glslang enforces the ESSL
/// rules from the `#version 300 es` directive itself, which is exactly the
/// strictness a PowerVR/Mali/Adreno compiler applies.
#[test]
fn both_dialects_pass_a_strict_glsl_front_end() {
    let Some(tool) = which_glslang() else {
        eprintln!("skipping: glslangValidator not on PATH (install glslang-tools for this check)");
        return;
    };
    let dir = std::env::temp_dir().join(format!("rt-glsl-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");

    for (dialect, tag) in [(ShaderDialect::Core330, "core330"), (ShaderDialect::Es300, "es300")] {
        // glslang picks the stage from the file extension.
        let vert = dir.join(format!("{tag}.vert"));
        let frag = dir.join(format!("{tag}.frag"));
        std::fs::write(&vert, dialect.vertex_source()).expect("write vert");
        std::fs::write(&frag, dialect.fragment_source()).expect("write frag");
        let out = std::process::Command::new(&tool).arg(&vert).arg(&frag).output().expect("run glslangValidator");
        assert!(
            out.status.success(),
            "{tag} shaders rejected by glslang:\n{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}

/// `glslangValidator` (or its newer `glslang` alias) on PATH, if present.
fn which_glslang() -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        for name in ["glslangValidator", "glslang"] {
            let p = dir.join(name);
            if p.is_file() {
                return Some(p);
            }
        }
    }
    None
}

/// Build a `FontBlobs` from the first readable common monospace TTF. Only the
/// regular chain must be non-empty for `Renderer::new` (it measures the cell
/// from the primary face).
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
            return FontBlobs { regular: vec![bytes], ..Default::default() };
        }
    }
    panic!("no test font found; install DejaVu Sans Mono or adjust CANDIDATES");
}
