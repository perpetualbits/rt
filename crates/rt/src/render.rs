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

use std::collections::HashMap; // char -> packed glyph location

use fontdue::Font; // CPU glyph rasteriser
use glow::HasContext; // brings the raw GL methods into scope

/// Size of the (square) glyph atlas texture in texels. 1024² holds a few
/// thousand ASCII/Latin glyphs at typical sizes — far more than a terminal
/// shows at once. If it ever fills we simply stop caching new glyphs (they
/// render blank); a production version would grow or evict.
const ATLAS: i32 = 1024;

/// An RGBA colour in 0..=1 floats, matching the shader's vertex colour input.
#[derive(Clone, Copy)]
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

/// The renderer owns the GL objects, the font, the atlas, and the per-frame
/// vertex scratch buffer.
pub struct Renderer {
    gl: glow::Context,                 // the live GL context (made current by caller)
    program: glow::Program,            // the single shader program
    vao: glow::VertexArray,            // vertex array object describing the layout
    vbo: glow::Buffer,                 // dynamic vertex buffer, re-uploaded each frame
    atlas_tex: glow::Texture,          // the R8 coverage atlas
    u_screen: glow::UniformLocation,   // uniform: viewport size in pixels
    fonts: Vec<Font>,                  // primary monospace font + fallbacks for coverage gaps
    italic_fonts: Vec<Font>,           // italic/oblique faces (empty if none installed)
    font_px: f32,                      // pixel size glyphs are rasterised at
    cell_w: f32,                       // monospace cell width in pixels
    cell_h: f32,                       // cell height (line advance) in pixels
    ascent: f32,                       // baseline offset from the cell top
    glyphs: HashMap<(char, bool), Glyph>, // rasterised-glyph cache, keyed by (char, italic)
    shelf_x: i32,                      // next free x in the current atlas shelf
    shelf_y: i32,                      // top y of the current shelf
    shelf_h: i32,                      // height of the current shelf
    verts: Vec<f32>,                   // per-frame vertex scratch (cleared each frame)
    screen: (f32, f32),                // current viewport size in pixels
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
    pub fn new(
        gl: glow::Context,
        font_blobs: &[Vec<u8>],
        italic_blobs: &[Vec<u8>],
        font_px: f32,
    ) -> Result<Self, String> {
        // Parse a slice of font blobs into `Font`s, skipping any that fail. Used
        // for both the regular chain (primary required) and the italic chain.
        let parse_chain = |blobs: &[Vec<u8>], primary_required: bool| -> Result<Vec<Font>, String> {
            let mut out: Vec<Font> = Vec::new();
            for (i, blob) in blobs.iter().enumerate() {
                match Font::from_bytes(blob.as_slice(), fontdue::FontSettings::default()) {
                    Ok(f) => out.push(f), // usable font
                    Err(e) if i == 0 && primary_required => {
                        return Err(format!("primary font parse failed: {e}")); // fatal
                    }
                    Err(e) => log::warn!("skipping unparseable font #{i}: {e}"), // non-fatal
                }
            }
            Ok(out)
        };
        let fonts = parse_chain(font_blobs, true)?; // regular chain; primary must parse
        let italic_fonts = parse_chain(italic_blobs, false)?; // italic chain; all optional
        // The primary font (index 0) defines the monospace cell metrics.
        let font = &fonts[0];
        // Measure the monospace cell: 'M' advance for width, line metrics for
        // height/ascent. Unwraps guarded with sensible fallbacks.
        let (metrics, _) = font.rasterize('M', font_px); // reference glyph
        let line = font.horizontal_line_metrics(font_px); // vertical metrics
        let cell_w = metrics.advance_width.ceil().max(1.0); // never zero-width
        let (cell_h, ascent) = match line {
            Some(l) => (l.new_line_size.ceil().max(1.0), l.ascent), // proper metrics
            None => (font_px.ceil().max(1.0), font_px * 0.8),       // fallback estimate
        };

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

            Ok(Renderer {
                gl,
                program,
                vao,
                vbo,
                atlas_tex,
                u_screen,
                fonts,
                italic_fonts,
                font_px,
                cell_w,
                cell_h,
                ascent,
                glyphs: HashMap::new(),
                shelf_x: 2,  // leave column 0/1 near the opaque seed texel
                shelf_y: 2,  // first shelf sits below the seed row
                shelf_h: 0,
                verts: Vec::new(),
                screen: (0.0, 0.0),
            })
        }
    }

    /// The measured monospace cell size in pixels `(width, height)`. The app
    /// uses this to convert pane pixel rectangles into terminal (cols, rows).
    pub fn cell_size(&self) -> (f32, f32) {
        (self.cell_w, self.cell_h)
    }

    /// Compile and link the vertex+fragment shaders into a program, returning a
    /// descriptive error on failure. Kept private; called once from `new`.
    unsafe fn build_program(gl: &glow::Context) -> Result<glow::Program, String> {
        // Vertex shader: map pixel coordinates to clip space using the viewport
        // size uniform (origin top-left, y down — the usual GUI convention).
        let vs_src = r#"#version 330 core
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
        // Fragment shader: coverage (red channel) becomes alpha.
        let fs_src = r#"#version 330 core
            in vec2 v_uv;
            in vec4 v_color;
            uniform sampler2D u_atlas;                // the coverage atlas
            out vec4 frag;
            void main() {
                float cov = texture(u_atlas, v_uv).r; // glyph coverage / 1.0 for solids
                float a = v_color.a * cov;             // effective alpha of this fragment
                frag = vec4(v_color.rgb * a, a);       // PREMULTIPLIED output (rgb scaled by alpha)
            }"#;

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
        // Clear to a PREMULTIPLIED background so the alpha channel carries the
        // window's opacity: bg.3 < 1 makes the empty areas translucent, and the
        // rgb is pre-scaled by that alpha to match the premultiplied blend mode.
        let a = bg.3; // requested background opacity (1.0 = fully opaque)
        unsafe {
            self.gl.clear_color(bg.0 * a, bg.1 * a, bg.2 * a, a); // premultiplied clear
            self.gl.clear(glow::COLOR_BUFFER_BIT); // clear the colour buffer
        }
    }

    /// Rasterise (if needed) and cache a glyph, returning its atlas placement.
    /// Returns `None` for glyphs that don't fit the atlas or have no bitmap
    /// (e.g. space), so callers simply skip drawing them.
    fn glyph(&mut self, c: char, italic: bool) -> Option<Glyph> {
        // Fast path: already cached for this (char, italic) combination.
        if let Some(g) = self.glyphs.get(&(c, italic)) {
            return Some(*g);
        }
        // Choose the font chain: the italic faces when italic is requested and
        // any are installed, otherwise the regular chain. Then within the chosen
        // chain pick the first font that actually has this character
        // (`lookup_glyph_index != 0`) — the fallback logic that renders braille
        // etc. from a secondary font. If italic has no glyph anywhere, fall back
        // to the upright chain so we at least show the character.
        let chain: &[Font] = if italic && !self.italic_fonts.is_empty() {
            &self.italic_fonts
        } else {
            &self.fonts
        };
        let (chain, idx) = match chain.iter().position(|f| f.lookup_glyph_index(c) != 0) {
            Some(i) => (chain, i), // found in the chosen chain
            None if italic => {
                // Not in the italic chain: try the regular chain upright.
                let i = self.fonts.iter().position(|f| f.lookup_glyph_index(c) != 0).unwrap_or(0);
                (&self.fonts[..], i)
            }
            None => (chain, 0), // nobody has it; use the primary (notdef/blank)
        };
        // Rasterise the glyph to a coverage bitmap at our pixel size.
        let (metrics, bitmap) = chain[idx].rasterize(c, self.font_px);
        // Empty glyphs (space) have zero-size bitmaps; nothing to pack/draw.
        if metrics.width == 0 || metrics.height == 0 {
            let g = Glyph { u0: 0.0, v0: 0.0, u1: 0.0, v1: 0.0, w: 0.0, h: 0.0, left: 0.0, top: 0.0 };
            self.glyphs.insert((c, italic), g); // cache the "blank" so we don't retry
            return None;
        }
        let gw = metrics.width as i32; // glyph bitmap width
        let gh = metrics.height as i32; // glyph bitmap height
        // Advance to a new shelf if this glyph won't fit on the current one.
        if self.shelf_x + gw + 1 >= ATLAS {
            self.shelf_y += self.shelf_h + 1; // move down past the current shelf
            self.shelf_x = 2; // back to the left margin
            self.shelf_h = 0; // new shelf starts empty
        }
        // If we've run out of vertical room, give up caching this glyph.
        if self.shelf_y + gh + 1 >= ATLAS {
            return None; // atlas full; render nothing for this glyph
        }
        let x = self.shelf_x; // where this glyph goes, x
        let y = self.shelf_y; // where this glyph goes, y
        unsafe {
            // Upload the coverage bitmap into the atlas at (x, y).
            self.gl.bind_texture(glow::TEXTURE_2D, Some(self.atlas_tex));
            self.gl.tex_sub_image_2d(
                glow::TEXTURE_2D,
                0,
                x,
                y,
                gw,
                gh,
                glow::RED,
                glow::UNSIGNED_BYTE,
                glow::PixelUnpackData::Slice(Some(&bitmap)),
            );
        }
        // Advance the shelf cursor and grow the shelf height if needed.
        self.shelf_x += gw + 1; // 1px gutter between glyphs
        self.shelf_h = self.shelf_h.max(gh);
        // Record the glyph's UV rectangle and placement offsets.
        let g = Glyph {
            u0: x as f32 / ATLAS as f32,
            v0: y as f32 / ATLAS as f32,
            u1: (x + gw) as f32 / ATLAS as f32,
            v1: (y + gh) as f32 / ATLAS as f32,
            w: gw as f32,
            h: gh as f32,
            left: metrics.xmin as f32,   // horizontal bearing
            top: metrics.ymin as f32 + gh as f32, // top = height above baseline
        };
        self.glyphs.insert((c, italic), g); // cache for next time
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

    /// Draw a 1-cell character at cell column/row within a pane whose top-left
    /// pixel is `(ox, oy)`. Skips blanks. `fg` is the glyph colour; `italic`
    /// selects the oblique face (falling back to upright if none is installed).
    pub fn draw_char(&mut self, ox: f32, oy: f32, col: usize, row: usize, ch: char, fg: Color, italic: bool) {
        // Compute this cell's top-left pixel inside the pane.
        let cell_x = ox + col as f32 * self.cell_w; // pen x
        let cell_y = oy + row as f32 * self.cell_h; // cell top y
        // Fetch/rasterise the glyph; blanks return None and draw nothing.
        let g = match self.glyph(ch, italic) {
            Some(g) if g.w > 0.0 => g, // a real, drawable glyph
            _ => return,               // blank or un-cacheable: skip
        };
        // Place the bitmap on the baseline: baseline y = cell top + ascent.
        let gx = cell_x + g.left; // apply horizontal bearing
        let gy = cell_y + self.ascent - g.top; // baseline minus glyph top
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
            // One draw call for the whole frame's quads.
            self.gl.draw_arrays(glow::TRIANGLES, 0, vertex_count);
        }
    }
}
