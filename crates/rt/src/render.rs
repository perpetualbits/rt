//! A compact OpenGL text renderer: alacritty's glyph-atlas idea in miniature.
//!
//! Design (kept deliberately small because it is not runtime-verifiable in the
//! headless dev sandbox — only on a real display):
//!   * One shader. Vertices carry position (pixels), a UV into a single
//!     coverage-only (`R8`) atlas texture, and an RGBA colour.
//!   * The fragment shader reads coverage as alpha and multiplies the vertex
//!     colour: `out = vec4(color.rgb, color.a * coverage)`.
//!   * Atlas texel (0,0) is forced opaque, so *solid* quads (backgrounds,
//!     dividers, the focus border) sample that texel and render as flat colour
//!     through the very same shader — no second pipeline.
//!   * Glyphs are rasterised on demand by `fontdue` and packed into the atlas in
//!     simple left-to-right shelves.
//!
//! Each frame the app builds a CPU vertex list (two triangles per quad) and
//! uploads it once; this is plenty fast for a terminal's glyph counts and keeps
//! the GL code tiny and auditable.

use std::collections::HashMap; // shape masks
use rustc_hash::FxHashMap; // glyph cache: SipHash on (char, bool, bool) was 24% of a full frame on a U74

use fontdue::Font; // CPU glyph rasteriser
use glow::HasContext; // brings the raw GL methods into scope

/// Size of the (square) glyph atlas texture in texels. 1024² holds a few
/// thousand ASCII/Latin glyphs at typical sizes — far more than a terminal
/// shows at once. If it ever fills we simply stop caching new glyphs (they
/// render blank); a production version would grow or evict.
const ATLAS: i32 = 1024;

/// One font FACE: the bytes of the file it lives in, plus WHICH face inside
/// those bytes it is.
///
/// A plain `.ttf` holds exactly one face, so its index is 0 and it behaves
/// exactly as a bare `Vec<u8>` did — which is nearly every face on Linux. A
/// `.ttc` (TrueType Collection) packs several faces — typically a family's
/// regular/bold/italic/bold-italic — into ONE file, and the index is the *only*
/// thing that distinguishes them: the bytes handed to the rasteriser are
/// byte-for-byte identical for all four. Carrying the bytes without the index
/// therefore silently collapses a whole family onto its face 0. Measured on
/// macOS 26.6.2, where `/System/Library/Fonts/Menlo.ttc` and
/// `…/Supplemental/PTMono.ttc` are real collections: Menlo drew bold and italic
/// as regular, and PT Mono drew *everything* bold, because face 0 of PTMono.ttc
/// IS PT Mono Bold.
///
/// A named struct rather than a `(Vec<u8>, u32)` tuple: the two members are not
/// interchangeable, and they are consumed far from where they are built — three
/// separate parse sites in three backends (`render.rs`, `wgpu_text.rs`,
/// `xrender_backend.rs`). `blob.index` says what `.1` would not.
///
/// [`FontBlob::settings`] is the ONE place `collection_index` is ever set, so a
/// future parse site cannot re-introduce the bug by reaching for
/// `FontSettings::default()` — it has no bytes to reach for without also having
/// the blob that carries the index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FontBlob {
    pub data: Vec<u8>, // the whole file: one face for `.ttf`, the collection for `.ttc`
    pub index: u32,    // which face inside `data` (0 for a single-face file)
}

impl FontBlob {
    /// A face at an explicit collection index (what fontdb's query reports).
    pub fn new(data: Vec<u8>, index: u32) -> Self {
        FontBlob { data, index }
    }

    /// The fontdue settings that select *this* face. The only construction of
    /// `collection_index` in rt.
    pub fn settings(&self) -> fontdue::FontSettings {
        fontdue::FontSettings { collection_index: self.index, ..Default::default() }
    }

    /// Rasteriser-ready face, or fontdue's parse error (CFF/OTF, a corrupt file,
    /// or an index past the end of the collection). The single choke point every
    /// backend parses through.
    pub fn parse(&self) -> Result<Font, &'static str> {
        Font::from_bytes(self.data.as_slice(), self.settings())
    }
}

/// A single-face file (`.ttf`): index 0, i.e. exactly the old `Vec<u8>`
/// behaviour. Keeps the path loader and the tests that build blobs from one
/// known file free of an index that would always be 0.
impl From<Vec<u8>> for FontBlob {
    fn from(data: Vec<u8>) -> Self {
        FontBlob { data, index: 0 }
    }
}

/// The font faces for each weight/style the renderer needs. Each is a
/// fallback chain (first entry preferred). Any but `regular` may be empty; when
/// a style has no face installed, the renderer falls back to `regular`.
#[derive(Default)]
pub struct FontBlobs {
    pub regular: Vec<FontBlob>,     // upright normal weight (must be non-empty)
    pub bold: Vec<FontBlob>,        // bold weight
    pub italic: Vec<FontBlob>,      // oblique/italic
    pub bold_italic: Vec<FontBlob>, // bold + oblique
}

/// An RGBA colour in 0..=1 floats, matching the shader's vertex colour input.
#[derive(Clone, Copy, PartialEq, Debug)]
pub struct Color(pub f32, pub f32, pub f32, pub f32);

impl Color {
    /// Convenience constructor from 8-bit sRGB components (what a terminal
    /// palette speaks), normalised to the 0..1 floats GL wants.
    pub fn rgb(r: u8, g: u8, b: u8) -> Color {
        Color(r as f32 / 255.0, g as f32 / 255.0, b as f32 / 255.0, 1.0)
    }

    /// Return a copy of this colour with its alpha replaced. Used to turn the
    /// opaque background colour into a translucent one at the requested opacity.
    pub fn with_alpha(self, a: f32) -> Color {
        Color(self.0, self.1, self.2, a) // keep rgb, set alpha
    }
}

/// Where a rasterised glyph lives in the atlas plus how to place it on the
/// baseline. Cached so each glyph is rasterised at most once.
#[derive(Clone, Copy)]
struct Glyph {
    u0: f32,      // atlas UV of the glyph's left edge (0..1)
    v0: f32,      // atlas UV of the glyph's top edge
    u1: f32,      // right edge
    v1: f32,      // bottom edge
    w: f32,       // glyph bitmap width in pixels
    h: f32,       // glyph bitmap height in pixels
    left: f32,    // x offset from the pen position to the bitmap's left
    top: f32,     // y offset from the baseline up to the bitmap's top
}

/// One vertex: 2 floats position, 2 floats UV, 4 floats colour = 8 floats.
/// Laid out flat in a `Vec<f32>` for a single glBufferData upload per frame.
const FLOATS_PER_VERTEX: usize = 8;

/// Parse a slice of font blobs into `Font`s, skipping any that fail to parse
/// (e.g. CFF/OTF that fontdue can't read). Each blob names its own face inside
/// its bytes (`FontBlob::parse`), so a `.ttc` member is loaded as the face
/// fontdb matched rather than as the collection's face 0. If `primary_required`,
/// the first blob must parse. Shared by `Renderer::new` and `reload_fonts`.
fn parse_chain(blobs: &[FontBlob], primary_required: bool) -> Result<Vec<Font>, String> {
    let mut out: Vec<Font> = Vec::new();
    for (i, blob) in blobs.iter().enumerate() {
        match blob.parse() {
            Ok(f) => out.push(f), // usable font
            Err(e) if i == 0 && primary_required => {
                return Err(format!("primary font parse failed: {e}")); // fatal
            }
            Err(e) => log::warn!("skipping unparseable font #{i}: {e}"), // non-fatal
        }
    }
    Ok(out)
}

/// Measure the monospace cell for a font at `font_px`: `(cell_w, cell_h, ascent)`.
/// Width comes from 'M''s advance; height/ascent from the font's line metrics,
/// with sensible fallbacks.
fn measure_cell(font: &Font, font_px: f32) -> (f32, f32, f32) {
    let (metrics, _) = font.rasterize('M', font_px); // reference glyph
    let line = font.horizontal_line_metrics(font_px); // vertical metrics
    let cell_w = metrics.advance_width.ceil().max(1.0); // never zero-width
    match line {
        Some(l) => (cell_w, l.new_line_size.ceil().max(1.0), l.ascent), // proper metrics
        None => (cell_w, font_px.ceil().max(1.0), font_px * 0.8),       // fallback estimate
    }
}

/// Measure the `(cell_w, cell_h)` a font family + size produces, WITHOUT a GL
/// context. `main` calls this before the window exists so it can pre-size the
/// window to an exact cols×rows grid (the `--cols`/`--rows` flags). It mirrors
/// exactly what `Renderer::new` measures internally, so the grid comes out
/// identical once the real renderer is built.
pub fn cell_size_for(blobs: &FontBlobs, font_px: f32) -> (f32, f32) {
    // Only the primary regular font (index 0) defines the monospace cell.
    match parse_chain(&blobs.regular, true) {
        Ok(fonts) if !fonts.is_empty() => {
            let (w, h, _) = measure_cell(&fonts[0], font_px); // drop the ascent
            (w, h)
        }
        // Unparseable/empty: a rough estimate keeps the window sane. The real
        // renderer will fail later with a clearer message if the font is broken.
        _ => (font_px * 0.6, (font_px * 1.2).ceil()),
    }
}

/// Convert a top-left-origin pixel rectangle to a GL scissor box, which is
/// **bottom-left origin**. `screen_h` is the window height in physical pixels.
/// Returns `(x, y, width, height)` ready for `glScissor`.
pub fn scissor_box(r: crate::damage::PxRect, screen_h: i32) -> (i32, i32, i32, i32) {
    let gl_y = screen_h - (r.y + r.h); // flip Y: top-left px → bottom-left GL
    (r.x, gl_y, r.w, r.h)
}

// ---------------------------------------------------------------------------
// Shaders: one body, two dialects.
// ---------------------------------------------------------------------------

/// Vertex shader body — map pixel coordinates to clip space using the viewport
/// size uniform (origin top-left, y down — the usual GUI convention).
///
/// Everything here is valid in BOTH desktop GLSL 3.30 core and GLSL ES 3.00:
/// `layout (location = N) in` on vertex inputs, `out` varyings and `gl_Position`
/// all exist unchanged in ES 3.00 (they are the ES 2.0 `attribute`/`varying`
/// keywords that do *not*). Only the header above it differs — see
/// [`ShaderDialect::vs_header`]. Starts with the newline that follows the
/// `#version` line, so `header + body` reproduces the original single string.
const VS_BODY: &str = r#"
            layout (location = 0) in vec2 a_pos;      // pixel position
            layout (location = 1) in vec2 a_uv;       // atlas uv
            layout (location = 2) in vec4 a_color;    // rgba
            uniform vec2 u_screen;                    // viewport in pixels
            out vec2 v_uv;                            // -> fragment
            out vec4 v_color;
            void main() {
                // Convert pixels to normalised device coords, flipping y.
                float x = (a_pos.x / u_screen.x) * 2.0 - 1.0;
                float y = 1.0 - (a_pos.y / u_screen.y) * 2.0;
                gl_Position = vec4(x, y, 0.0, 1.0);
                v_uv = a_uv;
                v_color = a_color;
            }"#;

/// Fragment shader body: coverage (red channel) becomes alpha.
///
/// Also dialect-neutral. `texture(sampler2D, vec2)` is the ES 3.00 spelling too
/// (ES 2.0's `texture2D` is what would have been needed at `#version 100`), and a
/// single user-declared `out` defaults to colour attachment 0 in both dialects,
/// so no `layout (location = 0)` is needed on `frag`. The one thing ES demands
/// that desktop GL does not is a default float precision in the fragment stage;
/// that lives in [`ShaderDialect::fs_header`].
const FS_BODY: &str = r#"
            in vec2 v_uv;
            in vec4 v_color;
            uniform sampler2D u_atlas;                // the coverage atlas
            out vec4 frag;
            void main() {
                float cov = texture(u_atlas, v_uv).r; // glyph coverage / 1.0 for solids
                float a = v_color.a * cov;             // effective alpha of this fragment
                frag = vec4(v_color.rgb * a, a);       // PREMULTIPLIED output (rgb scaled by alpha)
            }"#;

/// Which GLSL dialect the *live* context speaks. Chosen at runtime from
/// `glGetString(GL_VERSION)` (via glow's parsed [`glow::Version`]), never guessed
/// from the platform: the same rt binary meets desktop Mesa on a PC and an
/// OpenGL-ES-only vendor driver (PowerVR on a StarFive JH7110, Mali, Adreno…) on
/// a small board, and only the context can say which it is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShaderDialect {
    /// Desktop OpenGL 3.3 core profile — what rt has always emitted.
    Core330,
    /// OpenGL ES 3.0 — `#version 300 es` plus mandatory precision declarations.
    /// ES 3.0 (not 2.0) is the floor because `render.rs` uses vertex array
    /// objects, `in`/`out` varyings, `texture()`, sized `R8` internal formats and
    /// a user-declared fragment output — every one of which is ES 3.0, not ES 2.0.
    Es300,
}

impl ShaderDialect {
    /// The dialect a context of this version can compile, or `None` if it is too
    /// old for either variant. Pure and platform-free so it is unit-testable
    /// without a GL context (there is none in CI's headless sandbox).
    ///
    /// `is_embedded` is glow's flag for an OpenGL **ES** context (it parses the
    /// `OpenGL ES N.M …` form of `GL_VERSION`).
    pub fn pick(major: u32, minor: u32, is_embedded: bool) -> Option<ShaderDialect> {
        if is_embedded {
            // GLES 3.0+ → `#version 300 es`. ES 2.x/1.x cannot run this renderer.
            ((major, minor) >= (3, 0)).then_some(ShaderDialect::Es300)
        } else {
            // Desktop GL 3.3+ → `#version 330 core`. 4.x accepts 330 as well,
            // core or compatibility profile alike.
            ((major, minor) >= (3, 3)).then_some(ShaderDialect::Core330)
        }
    }

    /// The complete vertex shader source for this dialect: the dialect's header
    /// lines followed by the one shared [`VS_BODY`].
    ///
    /// Public so `tests/gles_shader_path.rs` can hand each dialect's exact source
    /// to a real driver and see it compile — the only way to check the GLES
    /// variant on a machine that has no GLES-only GPU.
    pub fn vertex_source(self) -> String {
        format!("{}{VS_BODY}", self.vs_header())
    }

    /// The complete fragment shader source for this dialect. See
    /// [`vertex_source`](Self::vertex_source).
    pub fn fragment_source(self) -> String {
        format!("{}{FS_BODY}", self.fs_header())
    }

    /// The `#version` line this dialect emits, for logs and error messages.
    pub fn version_line(self) -> &'static str {
        match self {
            ShaderDialect::Core330 => "#version 330 core",
            ShaderDialect::Es300 => "#version 300 es",
        }
    }

    /// Ask the live context which dialect to compile for. Returns a message
    /// naming *what was found* and *what is required* when neither fits, so the
    /// caller (and `main`'s XRender fallback) can report something actionable
    /// instead of a raw driver string like "version 330 not supported".
    fn of_context(gl: &glow::Context) -> Result<ShaderDialect, String> {
        let v = gl.version();
        let api = if v.is_embedded { "OpenGL ES" } else { "OpenGL" };
        match ShaderDialect::pick(v.major, v.minor, v.is_embedded) {
            Some(d) => {
                log::info!(
                    "GL context: {api} {}.{} ({}); compiling shaders as `{}`",
                    v.major,
                    v.minor,
                    v.vendor_info,
                    d.version_line()
                );
                Ok(d)
            }
            None => Err(format!(
                "unsupported GL version: this context is {api} {}.{} ({}), \
                 but rt's GL renderer needs desktop OpenGL 3.3+ or OpenGL ES 3.0+",
                v.major, v.minor, v.vendor_info
            )),
        }
    }

    /// Lines prepended to [`VS_BODY`]. Desktop is exactly the string rt has
    /// always compiled; ES adds the float precision the vertex stage already
    /// defaults to, stated explicitly so it provably matches the fragment
    /// stage's declaration for the `v_uv`/`v_color` interface.
    fn vs_header(self) -> &'static str {
        match self {
            ShaderDialect::Core330 => "#version 330 core",
            ShaderDialect::Es300 => "#version 300 es\nprecision highp float;",
        }
    }

    /// Lines prepended to [`FS_BODY`].
    ///
    /// GLSL ES has **no** default `float` precision in the fragment stage, so one
    /// must be declared or the shader will not compile. `highp` — not the more
    /// common `mediump` — is deliberate, on both declarations:
    ///
    /// * `v_uv` indexes a 1024² atlas. `mediump` is only guaranteed fp16-ish
    ///   (10-bit mantissa, ~1e-3 relative), which at a UV near 1.0 is a quarter
    ///   of a texel of error — and the atlas samples with `NEAREST`, so a quarter
    ///   texel is the difference between a glyph's edge pixel and its neighbour's.
    /// * `u_atlas` returns 8-bit coverage; the ES default sampler precision is
    ///   `lowp`, which is not required to preserve all 256 levels and would band
    ///   anti-aliased glyph edges.
    ///
    /// `highp` in the fragment stage is mandatory in ES 3.0 (unlike ES 2.0, where
    /// it was optional), so this costs nothing in portability on our 3.0 floor.
    fn fs_header(self) -> &'static str {
        match self {
            ShaderDialect::Core330 => "#version 330 core",
            ShaderDialect::Es300 => "#version 300 es\nprecision highp float;\nprecision highp sampler2D;",
        }
    }
}

/// The renderer owns the GL objects, the font, the atlas, and the per-frame
/// vertex scratch buffer.
pub struct Renderer {
    gl: std::sync::Arc<glow::Context>, // the live GL context
    program: glow::Program,            // the single shader program
    vao: glow::VertexArray,            // vertex array object describing the layout
    vbo: glow::Buffer,                 // dynamic vertex buffer, re-uploaded each frame
    atlas_tex: glow::Texture,          // the R8 coverage atlas
    u_screen: glow::UniformLocation,   // uniform: viewport size in pixels
    fonts: Vec<Font>,                  // regular chain: primary + coverage fallbacks
    bold_fonts: Vec<Font>,             // bold faces (empty if none installed)
    italic_fonts: Vec<Font>,           // italic/oblique faces (empty if none)
    bold_italic_fonts: Vec<Font>,      // bold-italic faces (empty if none)
    font_px: f32,                      // pixel size glyphs are rasterised at
    cell_w: f32,                       // monospace cell width in pixels
    cell_h: f32,                       // cell height (line advance) in pixels
    ascent: f32,                       // baseline offset from the cell top
    glyphs: FxHashMap<(char, bool, bool), Glyph>, // glyph cache (non-ASCII), keyed by (char, bold, italic)
    ascii_glyphs: Vec<Option<Glyph>>,           // ASCII fast path: index = (char << 2) | bold << 1 | italic; no hashing at all
    // AA coverage masks for the instrument primitives, keyed by (kind, r*4, w*4):
    // kind 0 = disc, 1 = ring, 2 = line bar. Packed into the same atlas as glyphs.
    shape_masks: HashMap<(u8, u32, u32), Glyph>,
    shelf_x: i32,                      // next free x in the current atlas shelf
    shelf_y: i32,                      // top y of the current shelf
    shelf_h: i32,                      // height of the current shelf
    verts: Vec<f32>,                   // per-frame vertex scratch (cleared each frame)
    frame_verts: usize,                // vertices drawn since the last begin_frame* (frame log)
    scissor_rects: Vec<crate::damage::PxRect>, // >1 entries: end_frame draws once per rect (partial frames)
    screen: (f32, f32),                // current viewport size in pixels
    software: bool,                    // GL renderer is software (llvmpipe/swrast) → repaints are CPU-expensive
}

impl Renderer {
    /// Whether the GL renderer is a software rasteriser (llvmpipe/softpipe/
    /// swrast). On such a backend every repaint costs real CPU, so the caller
    /// throttles animated chrome. Detected once from `GL_RENDERER`.
    pub fn is_software(&self) -> bool {
        self.software
    }

    /// Block until every queued GL command has executed (`glFinish`). Only the
    /// `RT_FRAME_SYNC` diagnostic uses it, to attribute frame time per phase.
    pub fn finish(&self) {
        unsafe { self.gl.finish() }
    }

    /// Vertices drawn since the last `begin_frame*` (for the `rt::frame` debug log).
    pub fn frame_verts(&self) -> usize {
        self.frame_verts
    }

    /// Borrow the GL context (for the X11 readback present path).
    #[cfg(feature = "x11")]
    pub fn gl_ctx(&self) -> &glow::Context {
        &self.gl
    }
}

impl Renderer {
    /// Build the renderer from a current GL context and a list of font byte
    /// blobs: `fonts[0]` is the primary monospace font (its metrics define the
    /// cell); the rest are fallbacks consulted, in order, for glyphs the primary
    /// lacks (e.g. DejaVu Sans Mono has no braille — see `docs/KNOWN_ISSUES.md`).
    ///
    /// Compiles the shader, creates the atlas with an opaque seed texel at (0,0)
    /// for solid fills, measures the monospace cell from the primary font, and is
    /// then ready for `begin_frame`/`draw_*`/`end_frame`. Returns an error string
    /// on any GL or font failure so `main` can report it instead of panicking.
    pub fn new(gl: std::sync::Arc<glow::Context>, blobs: &FontBlobs, font_px: f32) -> Result<Self, String> {
        // Parse the four weight/style chains (only the regular primary required).
        let fonts = parse_chain(&blobs.regular, true)?;
        if fonts.is_empty() {
            return Err("no usable primary font".to_string()); // avoid fonts[0] panic
        }
        let bold_fonts = parse_chain(&blobs.bold, false)?;
        let italic_fonts = parse_chain(&blobs.italic, false)?;
        let bold_italic_fonts = parse_chain(&blobs.bold_italic, false)?;
        // The primary font (index 0) defines the monospace cell metrics.
        let (cell_w, cell_h, ascent) = measure_cell(&fonts[0], font_px);

        unsafe {
            // --- shader program -------------------------------------------
            let program = Self::build_program(&gl)?; // compile+link vs/fs
            // Grab the two uniform locations we use.
            let u_screen = gl
                .get_uniform_location(program, "u_screen")
                .ok_or("missing u_screen uniform")?;

            // --- vertex array + buffer ------------------------------------
            let vao = gl.create_vertex_array()?; // holds the attribute layout
            let vbo = gl.create_buffer()?; // the dynamic vertex data store
            gl.bind_vertex_array(Some(vao));
            gl.bind_buffer(glow::ARRAY_BUFFER, Some(vbo));
            // Describe the interleaved layout: loc0 = pos(2f), loc1 = uv(2f),
            // loc2 = color(4f), stride = 8 floats.
            let stride = (FLOATS_PER_VERTEX * 4) as i32; // bytes per vertex
            gl.enable_vertex_attrib_array(0);
            gl.vertex_attrib_pointer_f32(0, 2, glow::FLOAT, false, stride, 0); // position
            gl.enable_vertex_attrib_array(1);
            gl.vertex_attrib_pointer_f32(1, 2, glow::FLOAT, false, stride, 8); // uv (after 2f)
            gl.enable_vertex_attrib_array(2);
            gl.vertex_attrib_pointer_f32(2, 4, glow::FLOAT, false, stride, 16); // color (after 4f)

            // --- atlas texture --------------------------------------------
            let atlas_tex = gl.create_texture()?; // the coverage atlas
            gl.bind_texture(glow::TEXTURE_2D, Some(atlas_tex));
            // Allocate an empty R8 atlas.
            gl.tex_image_2d(
                glow::TEXTURE_2D,
                0,
                glow::R8 as i32,   // single-channel 8-bit (coverage)
                ATLAS,
                ATLAS,
                0,
                glow::RED,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&vec![0u8; (ATLAS * ATLAS) as usize])),
            );
            // Nearest filtering keeps glyph edges crisp at 1:1 scale.
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MIN_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_MAG_FILTER, glow::NEAREST as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_S, glow::CLAMP_TO_EDGE as i32);
            gl.tex_parameter_i32(glow::TEXTURE_2D, glow::TEXTURE_WRAP_T, glow::CLAMP_TO_EDGE as i32);
            // Byte-aligned uploads (R8 rows aren't 4-byte aligned).
            gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            // Seed texel (0,0) opaque so solid quads have a fully-covered sample.
            gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                0,
                0,
                1,
                1,
                glow::RED,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&[255u8])),
            );

            // PREMULTIPLIED alpha blending (ONE, ONE_MINUS_SRC_ALPHA). The
            // fragment shader outputs premultiplied colour, which is what a
            // Wayland compositor expects — this is what makes a translucent
            // background composite correctly over whatever is behind the window.
            // For fully-opaque content it is identical to straight blending, so
            // normal rendering is unchanged.
            gl.enable(glow::BLEND);
            gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);

            // Is this a software rasteriser? On llvmpipe/swrast every repaint is
            // pure CPU, so the app throttles animated chrome (see main's loop).
            let renderer = gl.get_parameter_string(glow::RENDERER).to_lowercase();
            let software = ["llvmpipe", "softpipe", "swrast", "software", "llvm"]
                .iter()
                .any(|s| renderer.contains(s));
            log::info!("GL renderer: {renderer} (software={software})");

            Ok(Renderer {
                gl,
                software,
                program,
                vao,
                vbo,
                atlas_tex,
                u_screen,
                fonts,
                bold_fonts,
                italic_fonts,
                bold_italic_fonts,
                font_px,
                cell_w,
                cell_h,
                ascent,
                glyphs: FxHashMap::default(),
                ascii_glyphs: vec![None; 128 * 4],
                shape_masks: HashMap::new(),
                shelf_x: 2,  // leave column 0/1 near the opaque seed texel
                shelf_y: 2,  // first shelf sits below the seed row
                shelf_h: 0,
                verts: Vec::new(),
                screen: (0.0, 0.0),
                frame_verts: 0,
                scissor_rects: Vec::new(),
            })
        }
    }

    /// The measured monospace cell size in pixels `(width, height)`. The app
    /// uses this to convert pane pixel rectangles into terminal (cols, rows).
    pub fn cell_size(&self) -> (f32, f32) {
        (self.cell_w, self.cell_h)
    }

    /// Reload the font chains and/or pixel size (a preferences font change).
    /// Re-parses the four chains, re-measures the cell, and invalidates the
    /// glyph cache + atlas packing so glyphs re-rasterise at the new font. The
    /// caller must then recompute pane (cols, rows) from the new `cell_size`.
    /// Returns an error only if the new primary font fails to parse (in which
    /// case the old fonts are left untouched).
    pub fn reload_fonts(&mut self, blobs: &FontBlobs, font_px: f32) -> Result<(), String> {
        // Parse into locals first so a failure leaves the current fonts intact.
        let fonts = parse_chain(&blobs.regular, true)?;
        if fonts.is_empty() {
            return Err("no usable primary font".to_string()); // keep the old fonts
        }
        let bold_fonts = parse_chain(&blobs.bold, false)?;
        let italic_fonts = parse_chain(&blobs.italic, false)?;
        let bold_italic_fonts = parse_chain(&blobs.bold_italic, false)?;
        let (cell_w, cell_h, ascent) = measure_cell(&fonts[0], font_px);
        // Commit the new fonts/metrics.
        self.fonts = fonts;
        self.bold_fonts = bold_fonts;
        self.italic_fonts = italic_fonts;
        self.bold_italic_fonts = bold_italic_fonts;
        self.font_px = font_px;
        self.cell_w = cell_w;
        self.cell_h = cell_h;
        self.ascent = ascent;
        // Old cached glyphs are stale (wrong font/size); drop them and reset the
        // atlas packing cursor. Old pixels linger harmlessly until overwritten.
        // The shape masks share the atlas, so their placements are stale too —
        // drop them so they re-pack against the reset cursor.
        self.glyphs.clear();
        self.ascii_glyphs.iter_mut().for_each(|g| *g = None);
        self.shape_masks.clear();
        self.shelf_x = 2;
        self.shelf_y = 2;
        self.shelf_h = 0;
        Ok(())
    }

    /// Compile and link the vertex+fragment shaders into a program, returning a
    /// descriptive error on failure. Kept private; called once from `new`.
    ///
    /// The GLSL *body* of each stage is one source of truth ([`VS_BODY`] /
    /// [`FS_BODY`]); only the few lines above it — the `#version` line and, on
    /// GLES, the mandatory precision declarations — differ per dialect. That
    /// keeps the desktop string byte-for-byte what it always was while a second
    /// hardware class becomes reachable, and makes it impossible for the two
    /// variants to drift apart in the part that actually computes pixels.
    unsafe fn build_program(gl: &glow::Context) -> Result<glow::Program, String> {
        let dialect = ShaderDialect::of_context(gl)?; // desktop GL 3.3 vs GLES 3.0
        let (vs_src, fs_src) = (dialect.vertex_source(), dialect.fragment_source());
        let (vs_src, fs_src) = (vs_src.as_str(), fs_src.as_str());

        // Helper closure to compile one shader stage and check for errors.
        let compile = |kind: u32, src: &str| -> Result<glow::Shader, String> {
            let sh = gl.create_shader(kind)?; // create the shader object
            gl.shader_source(sh, src); // attach source
            gl.compile_shader(sh); // compile it
            if !gl.get_shader_compile_status(sh) {
                return Err(format!("shader compile error: {}", gl.get_shader_info_log(sh)));
            }
            Ok(sh)
        };
        let vs = compile(glow::VERTEX_SHADER, vs_src)?; // vertex stage
        let fs = compile(glow::FRAGMENT_SHADER, fs_src)?; // fragment stage
        let program = gl.create_program()?; // the linked program
        gl.attach_shader(program, vs);
        gl.attach_shader(program, fs);
        gl.link_program(program);
        if !gl.get_program_link_status(program) {
            return Err(format!("program link error: {}", gl.get_program_info_log(program)));
        }
        // Shaders can be detached/deleted after a successful link.
        gl.detach_shader(program, vs);
        gl.detach_shader(program, fs);
        gl.delete_shader(vs);
        gl.delete_shader(fs);
        Ok(program)
    }

    /// Handle a viewport resize: remember the new size and update the GL
    /// viewport so clip-space maps to the whole window.
    pub fn resize(&mut self, width: f32, height: f32) {
        self.screen = (width.max(1.0), height.max(1.0)); // never zero (div-by-zero in shader)
        unsafe {
            self.gl.viewport(0, 0, width as i32, height as i32); // full-window viewport
        }
    }

    /// Start a frame: clear the framebuffer to `bg` and reset the vertex buffer.
    pub fn begin_frame(&mut self, bg: Color) {
        self.verts.clear(); // drop last frame's geometry
        self.frame_verts = 0;
        // Clear to a PREMULTIPLIED background so the alpha channel carries the
        // window's opacity: bg.3 < 1 makes the empty areas translucent, and the
        // rgb is pre-scaled by that alpha to match the premultiplied blend mode.
        let a = bg.3; // requested background opacity (1.0 = fully opaque)
        unsafe {
            // Re-assert our GL state each frame: egui (painted last frame) leaves
            // its own viewport/scissor/blend behind, so restore ours before we
            // draw the terminal.
            self.gl.viewport(0, 0, self.screen.0 as i32, self.screen.1 as i32); // full window
            self.gl.disable(glow::SCISSOR_TEST); // egui uses scissor; we don't
            self.gl.enable(glow::BLEND); // premultiplied-alpha blending (translucency)
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.clear_color(bg.0 * a, bg.1 * a, bg.2 * a, a); // premultiplied clear
            self.gl.clear(glow::COLOR_BUFFER_BIT); // clear the colour buffer
        }
    }

    /// Begin a frame that only repaints the damaged bounding box `bbox`. Unlike
    /// [`begin_frame`], this does **not** clear the whole window — it enables
    /// the scissor test, clips to `bbox`, and clears only that region to the
    /// premultiplied background. Everything outside `bbox` keeps the previous
    /// frame's pixels (the present layer guarantees the back buffer is
    /// preserved). The following `end_frame()` draw is clipped to `bbox` too.
    pub fn begin_frame_scissored(&mut self, bg: Color, bbox: crate::damage::PxRect) {
        self.begin_frame_scissored_rects(bg, &[bbox])
    }

    /// Begin a partial frame that clears and redraws ONLY `rects` (each is
    /// scissored separately), not their bounding box. Everything outside every
    /// rect keeps the previous frame's pixels. `end_frame` then issues the
    /// frame's single vertex batch once per rect, so fragment work — and, on a
    /// software rasteriser, the clear, which Mesa's state tracker turns into a
    /// drawn quad whenever the scissor test is on — is proportional to the
    /// damaged AREA. With the pane's thin border bands folded into every
    /// partial frame, the bounding box was the whole pane; the rects are ~5%
    /// of it. An empty `rects` clears and draws nothing (zero-area scissor).
    pub fn begin_frame_scissored_rects(&mut self, bg: Color, rects: &[crate::damage::PxRect]) {
        self.verts.clear(); // drop last frame's geometry
        self.frame_verts = 0;
        self.scissor_rects.clear();
        self.scissor_rects.extend(rects.iter().copied().filter(|r| !r.is_empty()));
        let a = bg.3; // premultiplied clear, matching begin_frame
        let screen_h = self.screen.1 as i32;
        unsafe {
            self.gl.viewport(0, 0, self.screen.0 as i32, screen_h);
            self.gl.enable(glow::SCISSOR_TEST); // clip clears AND draws to the rects
            self.gl.enable(glow::BLEND);
            self.gl.blend_func(glow::ONE, glow::ONE_MINUS_SRC_ALPHA);
            self.gl.clear_color(bg.0 * a, bg.1 * a, bg.2 * a, a);
            if self.scissor_rects.is_empty() {
                self.gl.scissor(0, 0, 0, 0); // nothing to repaint: clip every draw away
            }
            for r in &self.scissor_rects {
                let (sx, sy, sw, sh) = scissor_box(*r, screen_h);
                self.gl.scissor(sx, sy, sw, sh);
                self.gl.clear(glow::COLOR_BUFFER_BIT); // scissor confines this to the rect
            }
            // Leave the scissor on the last rect; end_frame re-sets it per rect.
        }
    }

    /// Reset scissor state to the full window. Call after presenting a scissored
    /// frame so egui and the next `begin_frame`/`begin_frame_scissored` start
    /// from a known-clean state.
    pub fn clear_scissor(&mut self) {
        self.scissor_rects.clear();
        unsafe {
            self.gl.disable(glow::SCISSOR_TEST);
            self.gl.scissor(0, 0, self.screen.0 as i32, self.screen.1 as i32);
        }
    }

    /// Rasterise (if needed) and cache a glyph, returning its atlas placement.
    /// Returns `None` for glyphs that don't fit the atlas or have no bitmap
    /// (e.g. space), so callers simply skip drawing them.
    fn glyph(&mut self, c: char, bold: bool, italic: bool) -> Option<Glyph> {
        // ASCII (the overwhelming majority of cells): a direct table lookup.
        let ascii_idx = if (c as u32) < 128 { Some(((c as usize) << 2) | ((bold as usize) << 1) | italic as usize) } else { None };
        if let Some(i) = ascii_idx {
            if let Some(g) = self.ascii_glyphs[i] {
                return if g.w > 0.0 { Some(g) } else { None }; // a cached blank draws nothing
            }
        } else if let Some(g) = self.glyphs.get(&(c, bold, italic)) {
            return Some(*g);
        }
        let g = self.rasterise_glyph(c, bold, italic);
        if let Some(i) = ascii_idx {
            self.ascii_glyphs[i] = Some(g.unwrap_or(Glyph { u0: 0.0, v0: 0.0, u1: 0.0, v1: 0.0, w: 0.0, h: 0.0, left: 0.0, top: 0.0 }));
        }
        g
    }

    /// Resolve the face for `(c, bold, italic)` through the preference chains,
    /// rasterise, pack into the atlas and cache in the non-ASCII map. `None`
    /// for a blank (also cached, so it is not retried) or a full atlas.
    fn rasterise_glyph(&mut self, c: char, bold: bool, italic: bool) -> Option<Glyph> {
        // Preference-ordered chains for this style. Exact style first, then
        // progressively looser matches, ending at the regular chain (widest
        // coverage + braille/etc. fallbacks). Within each chain we take the first
        // font that actually has the glyph (`lookup_glyph_index != 0`).
        let prefs: &[&[Font]] = match (bold, italic) {
            (true, true) => &[
                self.bold_italic_fonts.as_slice(),
                self.bold_fonts.as_slice(),
                self.italic_fonts.as_slice(),
                self.fonts.as_slice(),
            ],
            (true, false) => &[self.bold_fonts.as_slice(), self.fonts.as_slice()],
            (false, true) => &[self.italic_fonts.as_slice(), self.fonts.as_slice()],
            (false, false) => &[self.fonts.as_slice()],
        };
        // Find the first (chain, font index) that covers this character.
        let mut chosen: Option<(&[Font], usize)> = None;
        for chain in prefs {
            if let Some(i) = chain.iter().position(|f| f.lookup_glyph_index(c) != 0) {
                chosen = Some((chain, i));
                break;
            }
        }
        // Nobody covers it: fall back to the primary (draws notdef/blank).
        let (chain, idx) = chosen.unwrap_or((self.fonts.as_slice(), 0));
        // Rasterise the glyph to a coverage bitmap at our pixel size.
        let (metrics, bitmap) = chain[idx].rasterize(c, self.font_px);
        // Empty glyphs (space) have zero-size bitmaps; nothing to pack/draw.
        if metrics.width == 0 || metrics.height == 0 {
            let g = Glyph { u0: 0.0, v0: 0.0, u1: 0.0, v1: 0.0, w: 0.0, h: 0.0, left: 0.0, top: 0.0 };
            self.glyphs.insert((c, bold, italic), g); // cache the "blank" so we don't retry
            return None;
        }
        let gw = metrics.width as i32; // glyph bitmap width
        let gh = metrics.height as i32; // glyph bitmap height
        // Pack + upload the coverage bitmap into the atlas (shared with masks).
        let (u0, v0, u1, v1) = self.pack_coverage(gw, gh, &bitmap)?;
        let g = Glyph {
            u0,
            v0,
            u1,
            v1,
            w: gw as f32,
            h: gh as f32,
            left: metrics.xmin as f32,            // horizontal bearing
            top: metrics.ymin as f32 + gh as f32, // top = height above baseline
        };
        self.glyphs.insert((c, bold, italic), g); // cache for next time
        Some(g)
    }

    /// Shelf-pack a `gw`×`gh` R8 coverage bitmap into the atlas and upload it,
    /// returning its UV rectangle `(u0,v0,u1,v1)`. Shared by glyphs and shape
    /// masks. `None` when the atlas is full.
    fn pack_coverage(&mut self, gw: i32, gh: i32, bitmap: &[u8]) -> Option<(f32, f32, f32, f32)> {
        // Advance to a new shelf if this won't fit on the current one.
        if self.shelf_x + gw + 1 >= ATLAS {
            self.shelf_y += self.shelf_h + 1; // move down past the current shelf
            self.shelf_x = 2; // back to the left margin
            self.shelf_h = 0; // new shelf starts empty
        }
        if self.shelf_y + gh + 1 >= ATLAS {
            return None; // atlas full
        }
        let x = self.shelf_x;
        let y = self.shelf_y;
        unsafe {
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.atlas_tex));
            // R8 rows are tightly packed (stride == width); force UNPACK_ALIGNMENT=1
            // so a width not a multiple of 4 doesn't shear.
            self.gl.pixel_store_i32(glow::UNPACK_ALIGNMENT, 1);
            self.gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                x,
                y,
                gw,
                gh,
                glow::RED,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(bitmap)),
            );
        }
        self.shelf_x += gw + 1; // 1px gutter
        self.shelf_h = self.shelf_h.max(gh);
        Some((
            x as f32 / ATLAS as f32,
            y as f32 / ATLAS as f32,
            (x + gw) as f32 / ATLAS as f32,
            (y + gh) as f32 / ATLAS as f32,
        ))
    }

    /// Rasterise (if needed) and cache an AA shape mask (disc/ring/bar), returning
    /// its atlas placement. `w`/`h` are the mask's pixel size; `left`/`top` unused.
    fn mask(&mut self, kind: u8, r: f32, width: f32) -> Option<Glyph> {
        let key = (kind, (r * 4.0).round() as u32, (width * 4.0).round() as u32);
        if let Some(g) = self.shape_masks.get(&key) {
            return Some(*g);
        }
        let (w, h, data) = match kind {
            0 => crate::raster::rasterize_disc(r),
            1 => crate::raster::rasterize_ring(r, width),
            _ => crate::raster::rasterize_bar(width),
        };
        if w == 0 || h == 0 {
            return None;
        }
        let (u0, v0, u1, v1) = self.pack_coverage(w as i32, h as i32, &data)?;
        let g = Glyph { u0, v0, u1, v1, w: w as f32, h: h as f32, left: 0.0, top: 0.0 };
        self.shape_masks.insert(key, g);
        Some(g)
    }

    /// Push one coloured quad (two triangles) into the per-frame vertex buffer.
    /// `(u0,v0)-(u1,v1)` selects the atlas region: a glyph's rectangle, or the
    /// opaque seed texel for a solid fill.
    fn push_quad(&mut self, x: f32, y: f32, w: f32, h: f32, uv: (f32, f32, f32, f32), c: Color) {
        let (u0, v0, u1, v1) = uv; // atlas corners
        let x1 = x + w; // right edge
        let y1 = y + h; // bottom edge
        // Two triangles: (TL, BL, BR) and (TL, BR, TR). Each vertex is 8 floats.
        let quad = [
            x, y, u0, v0, c.0, c.1, c.2, c.3,     // top-left
            x, y1, u0, v1, c.0, c.1, c.2, c.3,    // bottom-left
            x1, y1, u1, v1, c.0, c.1, c.2, c.3,   // bottom-right
            x, y, u0, v0, c.0, c.1, c.2, c.3,     // top-left
            x1, y1, u1, v1, c.0, c.1, c.2, c.3,   // bottom-right
            x1, y, u1, v0, c.0, c.1, c.2, c.3,    // top-right
        ];
        self.verts.extend_from_slice(&quad); // append to the frame's geometry
    }

    /// Push a quad from four arbitrary corner positions + their UVs (for a rotated
    /// line quad). Corners `[a,b,c,d]` wind around the quad; split into triangles
    /// `(a,b,c)` and `(a,c,d)`.
    fn push_quad_corners(&mut self, p: [(f32, f32); 4], uv: [(f32, f32); 4], c: Color) {
        let v = |pt: (f32, f32), t: (f32, f32)| [pt.0, pt.1, t.0, t.1, c.0, c.1, c.2, c.3];
        for &i in &[0usize, 1, 2, 0, 2, 3] {
            self.verts.extend_from_slice(&v(p[i], uv[i]));
        }
    }

    /// Filled anti-aliased disc of radius `r` centred at `(cx,cy)`, via a cached
    /// coverage mask (matches the XRender path's `fill_circle`).
    pub fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, c: Color) {
        if r <= 0.0 {
            return;
        }
        if let Some(g) = self.mask(0, r, 0.0) {
            let off = r.ceil(); // mask centre sits at (ceil r, ceil r)
            self.push_quad(cx - off, cy - off, g.w, g.h, (g.u0, g.v0, g.u1, g.v1), c);
        }
    }

    /// Anti-aliased ring (outer radius `r`, stroke `width`) centred at `(cx,cy)`.
    pub fn stroke_circle(&mut self, cx: f32, cy: f32, r: f32, width: f32, c: Color) {
        if r <= 0.0 || width <= 0.0 {
            return;
        }
        if let Some(g) = self.mask(1, r, width) {
            let off = r.ceil();
            self.push_quad(cx - off, cy - off, g.w, g.h, (g.u0, g.v0, g.u1, g.v1), c);
        }
    }

    /// Anti-aliased thick line from `(x0,y0)` to `(x1,y1)` (butt caps). A per-width
    /// bar mask is stretched along the segment on a rotated quad; the mask's AA
    /// height axis maps across the line's width so the long edges are smooth.
    pub fn stroke_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Color) {
        if width <= 0.0 {
            return;
        }
        let Some(g) = self.mask(2, 0.0, width) else { return };
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len = (dx * dx + dy * dy).sqrt();
        if len < 1e-6 {
            return;
        }
        // Half-width = half the mask's height (which carries the 1px AA margin each
        // side), so the quad is a touch wider than `width` and the fringe shows.
        let hw = g.h * 0.5;
        let (nx, ny) = (-dy / len * hw, dx / len * hw);
        let a = (x0 + nx, y0 + ny);
        let b = (x1 + nx, y1 + ny);
        let cc = (x1 - nx, y1 - ny);
        let d = (x0 - nx, y0 - ny);
        // +normal edge (a,b) at the mask's top row (v0); -normal edge (c,d) at v1.
        let (u0, v0, u1, v1) = (g.u0, g.v0, g.u1, g.v1);
        self.push_quad_corners([a, b, cc, d], [(u0, v0), (u1, v0), (u1, v1), (u0, v1)], c);
    }

    /// Draw a solid-colour rectangle (background fill, divider, focus border).
    /// Samples the opaque seed texel so it goes through the same shader.
    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        // A tiny UV window centred on the opaque seed texel at atlas (0,0).
        let s = 0.5 / ATLAS as f32; // half-texel offset to stay inside texel 0
        self.push_quad(x, y, w, h, (s, s, s, s), c); // solid fill
    }

    /// Fill a single character cell's background with a solid `color`. `(ox,oy)`
    /// is the containing region's top-left pixel; `col`/`row` index the cell.
    /// Used to paint per-cell background colours and the cursor block.
    pub fn fill_cell(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let x = ox + col as f32 * self.cell_w; // cell's left pixel
        let y = oy + row as f32 * self.cell_h; // cell's top pixel
        self.fill_rect(x, y, self.cell_w, self.cell_h, color); // solid fill
    }

    /// Draw a hollow (outline) cell — the cursor an *unfocused* terminal shows,
    /// regardless of its configured shape.
    pub fn cursor_hollow(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let x = ox + col as f32 * self.cell_w; // cell left
        let y = oy + row as f32 * self.cell_h; // cell top
        let t = (self.cell_h / 16.0).max(1.0); // outline thickness
        self.fill_rect(x, y, self.cell_w, t, color); // top edge
        self.fill_rect(x, y + self.cell_h - t, self.cell_w, t, color); // bottom edge
        self.fill_rect(x, y, t, self.cell_h, color); // left edge
        self.fill_rect(x + self.cell_w - t, y, t, self.cell_h, color); // right edge
    }

    /// Draw an underline cursor: a thick bar along the bottom of the cell (what
    /// editors typically show for overwrite mode).
    pub fn cursor_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let x = ox + col as f32 * self.cell_w; // cell left
        let th = (self.cell_h / 8.0).max(2.0); // a chunky bar, distinct from a text underline
        let y = oy + row as f32 * self.cell_h + self.cell_h - th; // sit on the cell bottom
        self.fill_rect(x, y, self.cell_w, th, color);
    }

    /// Draw a beam cursor: a thin vertical bar at the cell's left (insert mode).
    pub fn cursor_beam(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let x = ox + col as f32 * self.cell_w; // cell left
        let y = oy + row as f32 * self.cell_h; // cell top
        let bw = (self.cell_w / 8.0).max(2.0); // bar width
        self.fill_rect(x, y, bw, self.cell_h, color);
    }

    /// Paint a yellow/black hazard-stripe frame just inside the rect `(x,y,w,h)` —
    /// rt's visible bell, shown briefly on the border of the pane that rang (so a
    /// bell flags its own pane instead of flashing the whole window).
    pub fn bell_stripe(&mut self, x: f32, y: f32, w: f32, h: f32) {
        const T: f32 = 5.0; // band thickness
        self.striped_edge(x, y, w, T, true); // top
        self.striped_edge(x, y + h - T, w, T, true); // bottom
        self.striped_edge(x, y, T, h, false); // left
        self.striped_edge(x + w - T, y, T, h, false); // right
    }

    /// One edge of [`bell_stripe`]: alternating yellow/black segments along the
    /// band, giving the classic caution-tape look.
    fn striped_edge(&mut self, x: f32, y: f32, w: f32, h: f32, horizontal: bool) {
        const SEG: f32 = 12.0; // stripe segment length
        let yellow = Color::rgb(0xf2, 0xc9, 0x4c);
        let black = Color::rgb(0x14, 0x14, 0x14);
        let len = if horizontal { w } else { h };
        let (mut o, mut i) = (0.0f32, 0u32);
        while o < len {
            let seg = SEG.min(len - o);
            let c = if i % 2 == 0 { yellow } else { black };
            if horizontal {
                self.fill_rect(x + o, y, seg, h, c);
            } else {
                self.fill_rect(x, y + o, w, seg, c);
            }
            o += seg;
            i += 1;
        }
    }

    /// Draw a 1-cell character at cell column/row within a pane whose top-left
    /// pixel is `(ox, oy)`. Skips blanks. `fg` is the glyph colour; `bold`/
    /// `italic` select the heavier / oblique faces (each falling back to the
    /// regular face if none is installed).
    pub fn draw_char(&mut self, ox: f32, oy: f32, col: usize, row: usize, ch: char, fg: Color, bold: bool, italic: bool) {
        // Compute this cell's top-left pixel inside the pane.
        let cell_x = ox + col as f32 * self.cell_w; // pen x
        let cell_y = oy + row as f32 * self.cell_h; // cell top y
        // Fetch/rasterise the glyph; blanks return None and draw nothing.
        let g = match self.glyph(ch, bold, italic) {
            Some(g) if g.w > 0.0 => g, // a real, drawable glyph
            _ => return,               // blank or un-cacheable: skip
        };
        // Place the bitmap on the baseline: baseline y = cell top + ascent.
        // SNAP to the integer pixel grid. The quad's UVs map its gw×gh texels
        // one-to-one to pixels, but only if it lands on whole pixels; at a
        // fractional position (a pane split gives a fractional origin `ox`, and
        // `ascent` is a raw float) the NEAREST-filtered coverage atlas samples the
        // neighbouring texel at the edge — the faint per-glyph outline. Rounding
        // costs sub-pixel placement (irrelevant for a monospace grid) and makes
        // the text pixel-exact regardless of pane geometry.
        let gx = (cell_x + g.left).round(); // apply horizontal bearing, snapped
        let gy = (cell_y + self.ascent - g.top).round(); // baseline minus glyph top, snapped
        self.push_quad(gx, gy, g.w, g.h, (g.u0, g.v0, g.u1, g.v1), fg); // emit glyph quad
    }

    /// Draw an underline across a cell: a thin horizontal bar just below the
    /// text baseline, in `color`. Called for cells with any underline attribute.
    pub fn draw_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let x = ox + col as f32 * self.cell_w; // cell left
        let y = oy + row as f32 * self.cell_h + self.ascent + 1.0; // just under the baseline
        let thick = (self.cell_h / 16.0).max(1.0); // scale thickness with the font, min 1px
        self.fill_rect(x, y, self.cell_w, thick, color); // the underline bar
    }

    /// Draw a strikeout across a cell: a thin horizontal bar through the middle
    /// of the text (about 60% of the ascent), in `color`.
    pub fn draw_strikeout(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let x = ox + col as f32 * self.cell_w; // cell left
        let y = oy + row as f32 * self.cell_h + self.ascent * 0.6; // through the x-height
        let thick = (self.cell_h / 16.0).max(1.0); // same thickness as underline
        self.fill_rect(x, y, self.cell_w, thick, color); // the strikeout bar
    }

    /// Finish the frame: upload the accumulated vertices and issue one draw
    /// call. The caller swaps buffers afterwards. Does nothing if no geometry
    /// was produced.
    pub fn end_frame(&mut self) {
        if self.verts.is_empty() {
            return; // nothing to draw this frame
        }
        let vertex_count = (self.verts.len() / FLOATS_PER_VERTEX) as i32; // #vertices
        self.frame_verts += vertex_count as usize;
        unsafe {
            self.gl.use_program(Some(self.program)); // select our shader
            // Set the viewport-size uniform so the vertex shader maps pixels.
            self.gl.uniform_2_f32(Some(&self.u_screen), self.screen.0, self.screen.1);
            // Bind the atlas to texture unit 0 (the sampler defaults to unit 0).
            self.gl.active_texture(glow::TEXTURE0);
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.atlas_tex));
            // Upload this frame's vertex data (reinterpret f32 slice as bytes).
            self.gl.bind_vertex_array(Some(self.vao));
            self.gl.bind_buffer(glow::ARRAY_BUFFER, Some(self.vbo));
            let bytes: &[u8] = core::slice::from_raw_parts(
                self.verts.as_ptr() as *const u8,
                self.verts.len() * 4, // 4 bytes per f32
            );
            self.gl.buffer_data_u8_slice(glow::ARRAY_BUFFER, bytes, glow::STREAM_DRAW);
            if self.scissor_rects.len() > 1 {
                // Multi-rect partial frame: the same batch once per rect, each
                // scissored to that rect (vertex work is trivial; fragments are not).
                let screen_h = self.screen.1 as i32;
                for r in &self.scissor_rects {
                    let (sx, sy, sw, sh) = scissor_box(*r, screen_h);
                    self.gl.scissor(sx, sy, sw, sh);
                    self.gl.draw_arrays(glow::TRIANGLES, 0, vertex_count);
                }
            } else {
                // One draw call for the whole frame's quads.
                self.gl.draw_arrays(glow::TRIANGLES, 0, vertex_count);
            }
        }
        // Clear the batch so end_frame is idempotent: a caller may flush again
        // after the content pass (to emit native chrome batched during the
        // overlay pass), and with verts empty that second call early-returns
        // above. begin_frame also clears verts at the top of every frame, and
        // nothing between end_frame and the next begin_frame reads verts, so
        // clearing here changes no observable behaviour.
        self.verts.clear();
    }
}

#[cfg(test)]
mod scissor_tests {
    use super::scissor_box;
    use crate::damage::PxRect;

    #[test]
    fn flips_y_to_bottom_left_origin() {
        // 800x600 window. A 24x16 rect at top-left-origin (34, 52).
        // GL origin is bottom-left, so gl_y = screen_h - (y + h) = 600 - 68 = 532.
        let (x, y, w, h) = scissor_box(PxRect { x: 34, y: 52, w: 24, h: 16 }, 600);
        assert_eq!((x, y, w, h), (34, 532, 24, 16));
    }

    #[test]
    fn top_row_maps_to_top_of_gl_buffer() {
        // A rect flush against the top (y=0, h=16) sits at gl_y = 600 - 16 = 584.
        let (_, y, _, _) = scissor_box(PxRect { x: 0, y: 0, w: 8, h: 16 }, 600);
        assert_eq!(y, 584);
    }

    #[test]
    fn bottom_row_maps_to_gl_zero() {
        // A rect flush against the bottom (y = 584, h = 16) maps to gl_y = 0.
        let (_, y, _, _) = scissor_box(PxRect { x: 0, y: 584, w: 8, h: 16 }, 600);
        assert_eq!(y, 0);
    }
}

#[cfg(test)]
mod shader_dialect_tests {
    use super::{ShaderDialect, FS_BODY, VS_BODY};

    /// The exact strings rt compiled before the GLES path existed. If a refactor
    /// of the shader body ever changes what a desktop GL box compiles, this
    /// fails — the desktop path is the one that must not move (dop561/apollo run
    /// it daily, and `tests/damage_pixel_identity.rs` gates its pixels).
    const LEGACY_VS: &str = r#"#version 330 core
            layout (location = 0) in vec2 a_pos;      // pixel position
            layout (location = 1) in vec2 a_uv;       // atlas uv
            layout (location = 2) in vec4 a_color;    // rgba
            uniform vec2 u_screen;                    // viewport in pixels
            out vec2 v_uv;                            // -> fragment
            out vec4 v_color;
            void main() {
                // Convert pixels to normalised device coords, flipping y.
                float x = (a_pos.x / u_screen.x) * 2.0 - 1.0;
                float y = 1.0 - (a_pos.y / u_screen.y) * 2.0;
                gl_Position = vec4(x, y, 0.0, 1.0);
                v_uv = a_uv;
                v_color = a_color;
            }"#;
    const LEGACY_FS: &str = r#"#version 330 core
            in vec2 v_uv;
            in vec4 v_color;
            uniform sampler2D u_atlas;                // the coverage atlas
            out vec4 frag;
            void main() {
                float cov = texture(u_atlas, v_uv).r; // glyph coverage / 1.0 for solids
                float a = v_color.a * cov;             // effective alpha of this fragment
                frag = vec4(v_color.rgb * a, a);       // PREMULTIPLIED output (rgb scaled by alpha)
            }"#;

    #[test]
    fn desktop_source_is_byte_identical_to_the_pre_gles_shader() {
        assert_eq!(ShaderDialect::Core330.vertex_source(), LEGACY_VS);
        assert_eq!(ShaderDialect::Core330.fragment_source(), LEGACY_FS);
    }

    /// The ES variant must differ from the desktop one ONLY in the header: same
    /// body, `#version 300 es`, and the precision declarations GLSL ES needs.
    #[test]
    fn es_source_is_the_same_body_under_an_es_header() {
        let vs = ShaderDialect::Es300.vertex_source();
        let fs = ShaderDialect::Es300.fragment_source();
        assert!(vs.starts_with("#version 300 es\n"), "vs: {vs}");
        assert!(fs.starts_with("#version 300 es\n"), "fs: {fs}");
        // Fragment stage: GLSL ES has no default float precision there at all,
        // and the default sampler precision is lowp — both must be raised.
        assert!(fs.contains("precision highp float;"));
        assert!(fs.contains("precision highp sampler2D;"));
        // Vertex stage declares the float precision explicitly so the
        // v_uv/v_color interface provably matches the fragment stage.
        assert!(vs.contains("precision highp float;"));
        // The pixel-computing half is shared, not copied.
        assert!(vs.ends_with(VS_BODY) && fs.ends_with(FS_BODY));
        // No ES-2-era spellings crept in.
        assert!(!fs.contains("texture2D") && !vs.contains("attribute ") && !vs.contains("varying "));
    }

    #[test]
    fn desktop_gl_needs_3_3() {
        assert_eq!(ShaderDialect::pick(3, 3, false), Some(ShaderDialect::Core330));
        assert_eq!(ShaderDialect::pick(4, 6, false), Some(ShaderDialect::Core330));
        assert_eq!(ShaderDialect::pick(3, 2, false), None); // one minor short
        assert_eq!(ShaderDialect::pick(2, 1, false), None);
    }

    #[test]
    fn embedded_gl_needs_3_0() {
        // The failing board: PowerVR/pvrsrvkm advertises OpenGL ES only.
        assert_eq!(ShaderDialect::pick(3, 0, true), Some(ShaderDialect::Es300));
        assert_eq!(ShaderDialect::pick(3, 2, true), Some(ShaderDialect::Es300));
        // ES 2.0 has no VAOs, no `in`/`out`, no sized R8 — not a target.
        assert_eq!(ShaderDialect::pick(2, 0, true), None);
    }

    /// An ES 3.0 context must NOT be mistaken for desktop 3.3-capable, and a
    /// desktop 3.0 context must not be mistaken for ES-capable: the two ladders
    /// are independent, which is exactly what the `is_embedded` flag decides.
    #[test]
    fn the_two_version_ladders_do_not_cross() {
        assert_eq!(ShaderDialect::pick(3, 0, false), None); // desktop 3.0 < 3.3
        assert_eq!(ShaderDialect::pick(3, 0, true), Some(ShaderDialect::Es300));
    }
}
