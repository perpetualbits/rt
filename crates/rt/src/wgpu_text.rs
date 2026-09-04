//! Glyph atlas and text pipeline for the macOS wgpu backend.
//!
//! A direct translation of `render.rs`'s design to wgpu, deliberately: one
//! shader, one coverage-only (`R8Unorm`) atlas texture, vertices carrying
//! position in pixels, a UV into the atlas, and an RGBA colour. Keeping the same
//! model means the Mac draws the same shapes as the GL backend rather than
//! subtly different ones.
//!
//! Texel (0,0) of the atlas is forced to full coverage so solid fills can go
//! through the SAME pipeline as glyphs -- one pipeline, one draw call per frame.
//!
//! ## Deviations from the task-5 brief (see task-5-report.md for the full list)
//!
//! * The brief's `TextPipeline` carries a `masks: HashMap<MaskKey, [f32; 4]>`
//!   field for Task 7's instrument coverage masks. `MaskKey` isn't defined by
//!   any task landed so far, so that field is omitted here; Task 7 adds it
//!   back alongside the type when it needs it.
//! * The brief's `flush(&mut self, queue, pass)` has no `&Device`, but "grow
//!   [`vbuf`] by reallocation if `verts.len()` ever exceeds it" needs one to
//!   create a bigger buffer. `TextPipeline` keeps its own `wgpu::Device` clone
//!   (a cheap handle, not a second GPU device) for exactly that.
use std::collections::HashMap;

use fontdue::Font;

use crate::render::{Color, FontBlobs};

pub const ATLAS_SIZE: u32 = 2048;

/// Parse every blob in a fallback chain that fontdue can read, skipping (not
/// failing on) any it can't -- e.g. CFF/OTF, which fontdue doesn't support.
/// Mirrors `render.rs`'s `parse_chain`; unlike it, this returns `Vec<Font>`
/// unconditionally (never a `Result`) because the ONE fatal case -- an empty
/// `regular` chain -- is checked by the caller against the parsed result,
/// exactly as render.rs checks `fonts.is_empty()` after calling its version.
fn parse_chain(blobs: &[Vec<u8>]) -> Vec<Font> {
    let mut out = Vec::new();
    for (i, blob) in blobs.iter().enumerate() {
        match Font::from_bytes(blob.as_slice(), fontdue::FontSettings::default()) {
            Ok(f) => out.push(f),
            Err(e) => log::warn!("wgpu_text: skipping unparseable font #{i}: {e}"),
        }
    }
    out
}

/// Resolve the face that should draw `c` at `(bold, italic)`, exactly as
/// render.rs's `Renderer::glyph()` does: build preference-ordered chains
/// (exact style first, progressively looser, ending at `fonts` -- the
/// regular chain, which carries the widest coverage), then within each chain
/// take the first font that actually covers the character
/// (`lookup_glyph_index(c) != 0`). If nothing covers it, fall back to
/// `fonts[0]` (the primary), which draws notdef. Free-standing (not a method)
/// so the coverage fallback chain is unit-testable without a `wgpu::Device`
/// -- see the `tests` module below.
fn resolve_face<'a>(
    fonts: &'a [Font],
    bold_fonts: &'a [Font],
    italic_fonts: &'a [Font],
    bold_italic_fonts: &'a [Font],
    c: char,
    bold: bool,
    italic: bool,
) -> &'a Font {
    let prefs: &[&[Font]] = match (bold, italic) {
        (true, true) => &[bold_italic_fonts, bold_fonts, italic_fonts, fonts],
        (true, false) => &[bold_fonts, fonts],
        (false, true) => &[italic_fonts, fonts],
        (false, false) => &[fonts],
    };
    for chain in prefs {
        if let Some(i) = chain.iter().position(|f| f.lookup_glyph_index(c) != 0) {
            return &chain[i];
        }
    }
    // Nobody covers it: fall back to the primary (draws notdef/blank).
    &fonts[0]
}

/// Initial vertex-buffer capacity, in vertices (6 per quad). Generous for a
/// terminal grid plus chrome; `flush` grows it by doubling if a frame ever
/// needs more.
const INITIAL_VERT_CAP: usize = 6 * 4096;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub pos: [f32; 2],   // pixels, origin top-left
    pub uv: [f32; 2],    // 0..1 into the atlas
    pub color: [f32; 4], // straight RGBA
}

impl Vertex {
    const ATTRS: [wgpu::VertexAttribute; 3] =
        wgpu::vertex_attr_array![0 => Float32x2, 1 => Float32x2, 2 => Float32x4];

    fn layout() -> wgpu::VertexBufferLayout<'static> {
        wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<Vertex>() as wgpu::BufferAddress,
            step_mode: wgpu::VertexStepMode::Vertex,
            attributes: &Self::ATTRS,
        }
    }
}

const SHADER: &str = r#"
struct Uniforms { screen: vec2<f32> };
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VsOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) color: vec4<f32>,
};

@vertex
fn vs(@location(0) pos: vec2<f32>,
      @location(1) uv: vec2<f32>,
      @location(2) color: vec4<f32>) -> VsOut {
  var o: VsOut;
  // Pixels (origin top-left) -> clip space (origin centre, +y up).
  let ndc = vec2<f32>(pos.x / u.screen.x * 2.0 - 1.0,
                      1.0 - pos.y / u.screen.y * 2.0);
  o.pos = vec4<f32>(ndc, 0.0, 1.0);
  o.uv = uv;
  o.color = color;
  return o;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
  // R8 coverage modulates alpha, exactly as render.rs's GL shader does.
  let cov = textureSample(atlas, samp, in.uv).r;
  return vec4<f32>(in.color.rgb, in.color.a * cov);
}
"#;

/// Where a rasterised glyph lives in the atlas plus how to place it on the
/// baseline. Mirrors `render.rs`'s `Glyph`.
#[derive(Clone, Copy)]
struct Glyph {
    u0: f32,
    v0: f32,
    u1: f32,
    v1: f32,
    w: f32,
    h: f32,
    bearing_x: f32,
    bearing_y: f32,
}

pub struct TextPipeline {
    device: wgpu::Device, // cheap handle clone; needed to grow `vbuf` in `flush`
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    vbuf: wgpu::Buffer,
    vbuf_cap: usize, // vertices `vbuf` can currently hold
    ubuf: wgpu::Buffer,
    verts: Vec<Vertex>,
    atlas: wgpu::Texture,
    /// Next free column/row and the current row's height, for the shelf packer.
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
    /// Cached glyph placements: (char, bold, italic) -> uv rect + pixel offsets.
    glyphs: HashMap<(char, bool, bool), Glyph>,
    /// [width, height] in pixels, written into `ubuf` on flush.
    screen: [f32; 2],
    /// Per-style fallback chains, mirroring render.rs's `Renderer::fonts` /
    /// `bold_fonts` / `italic_fonts` / `bold_italic_fonts`: `fonts[0]` is the
    /// primary and defines the cell metrics; the rest (and every entry of the
    /// other three chains) are coverage fallbacks consulted per glyph in
    /// `push_glyph` so a character the primary lacks (braille, box-drawing,
    /// CJK, …) still renders instead of notdef. Bold/italic/bold_italic may be
    /// empty; `fonts` (regular) must be non-empty.
    fonts: Vec<Font>,
    bold_fonts: Vec<Font>,
    italic_fonts: Vec<Font>,
    bold_italic_fonts: Vec<Font>,
    font_px: f32,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
}

impl TextPipeline {
    pub fn new(
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        format: wgpu::TextureFormat,
        blobs: &FontBlobs,
        font_px: f32,
    ) -> Result<Self, String> {
        // Mirrors render.rs's `parse_chain`: parse EVERY blob in a chain that
        // fontdue can actually read (some CFF/OTF files it cannot skip rather
        // than fail the whole chain), regular's primary (index 0) being the
        // one required entry.
        let fonts = parse_chain(&blobs.regular);
        let regular = fonts.first().ok_or("wgpu_text: no usable regular font")?;

        let lm = regular
            .horizontal_line_metrics(font_px)
            .ok_or("wgpu_text: font has no horizontal line metrics")?;
        let cell_h = (lm.ascent - lm.descent + lm.line_gap).ceil();
        // Monospace: every advance is the same, so 'M' is representative.
        let cell_w = regular.metrics('M', font_px).advance_width.ceil();

        let bold_fonts = parse_chain(&blobs.bold);
        let italic_fonts = parse_chain(&blobs.italic);
        let bold_italic_fonts = parse_chain(&blobs.bold_italic);

        let atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rt glyph atlas"),
            size: wgpu::Extent3d { width: ATLAS_SIZE, height: ATLAS_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm, // coverage only, exactly as render.rs
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // Texel (0,0) = full coverage, so solid quads ride the glyph pipeline.
        queue.write_texture(
            wgpu::TexelCopyTextureInfo {
                texture: &atlas,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            &[255u8],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(1), rows_per_image: Some(1) },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );
        let atlas_view = atlas.create_view(&Default::default());

        // NEAREST filtering, matching render.rs's GL atlas exactly (glow.rs sets
        // TEXTURE_MIN/MAG_FILTER to NEAREST) so glyph edges look identical.
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("rt text atlas sampler"),
            address_mode_u: wgpu::AddressMode::ClampToEdge,
            address_mode_v: wgpu::AddressMode::ClampToEdge,
            address_mode_w: wgpu::AddressMode::ClampToEdge,
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });

        let ubuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rt text uniforms"),
            size: std::mem::size_of::<[f32; 2]>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let vbuf = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rt text vbuf"),
            size: (INITIAL_VERT_CAP * std::mem::size_of::<Vertex>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("rt text bind group layout"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::VERTEX,
                    ty: wgpu::BindingType::Buffer {
                        ty: wgpu::BufferBindingType::Uniform,
                        has_dynamic_offset: false,
                        min_binding_size: None,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 2,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
            ],
        });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("rt text bind group"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: ubuf.as_entire_binding() },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::TextureView(&atlas_view) },
                wgpu::BindGroupEntry { binding: 2, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });

        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rt text"),
            source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("rt text pipeline layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("rt text pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs"),
                compilation_options: Default::default(),
                buffers: &[Vertex::layout()],
            },
            primitive: wgpu::PrimitiveState {
                topology: wgpu::PrimitiveTopology::TriangleList,
                ..Default::default()
            },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState {
                    format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
            }),
            multiview: None,
            cache: None,
        });

        Ok(Self {
            device: device.clone(),
            pipeline,
            bind_group,
            vbuf,
            vbuf_cap: INITIAL_VERT_CAP,
            ubuf,
            verts: Vec::new(),
            atlas,
            // Matches render.rs's initial shelf state: leave column 0/1 near the
            // opaque seed texel, first shelf sits below the seed row.
            shelf_x: 2,
            shelf_y: 2,
            shelf_h: 0,
            glyphs: HashMap::new(),
            screen: [1.0, 1.0],
            font_px,
            cell_w,
            cell_h,
            ascent: lm.ascent,
            fonts,
            bold_fonts,
            italic_fonts,
            bold_italic_fonts,
        })
    }

    pub fn cell_size(&self) -> (f32, f32) { (self.cell_w, self.cell_h) }

    /// The regular face's ascent in pixels, matching render.rs's use of
    /// `self.ascent` to place underline/strikeout bars relative to the
    /// text baseline rather than an arbitrary fraction of the cell.
    pub fn ascent(&self) -> f32 { self.ascent }

    /// Resolve the face that should draw `c` at this style. Thin wrapper around
    /// the free function [`resolve_face`] (kept free-standing so the coverage
    /// fallback chain is unit-testable without a `wgpu::Device`).
    fn face_for(&self, c: char, bold: bool, italic: bool) -> &Font {
        resolve_face(&self.fonts, &self.bold_fonts, &self.italic_fonts, &self.bold_italic_fonts, c, bold, italic)
    }

    pub fn set_screen(&mut self, w: f32, h: f32) { self.screen = [w, h]; }

    /// Shelf packer: fill the current row left-to-right, start a new row when it
    /// no longer fits. Same scheme (and same 1px gutter, to keep a NEAREST
    /// sampler from bleeding into a neighbouring glyph's texels) render.rs's
    /// `pack_coverage` uses. Returns `None` when the atlas is full, same as
    /// render.rs -- the caller must refuse the glyph rather than pack it, since
    /// an out-of-bounds `write_texture` origin panics.
    fn pack(&mut self, queue: &wgpu::Queue, w: u32, h: u32, data: &[u8]) -> Option<[f32; 4]> {
        // Advance to a new shelf if this won't fit on the current one.
        if self.shelf_x + w + 1 >= ATLAS_SIZE {
            self.shelf_y += self.shelf_h + 1; // move down past the current shelf, +1 gutter
            self.shelf_x = 2; // back to the left margin
            self.shelf_h = 0; // new shelf starts empty
        }
        if self.shelf_y + h + 1 >= ATLAS_SIZE {
            return None; // atlas full
        }
        let (x, y) = (self.shelf_x, self.shelf_y);
        if w > 0 && h > 0 {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo {
                    texture: &self.atlas,
                    mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 },
                    aspect: wgpu::TextureAspect::All,
                },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }
        self.shelf_x += w + 1; // 1px gutter
        self.shelf_h = self.shelf_h.max(h);
        let s = ATLAS_SIZE as f32;
        Some([x as f32 / s, y as f32 / s, (x + w) as f32 / s, (y + h) as f32 / s])
    }

    pub fn push_glyph(&mut self, queue: &wgpu::Queue, x: f32, y: f32, ch: char, fg: Color, bold: bool, italic: bool) {
        let key = (ch, bold, italic);
        let g = match self.glyphs.get(&key) {
            Some(g) => *g,
            None => {
                let font = self.face_for(ch, bold, italic);
                let (m, cov) = font.rasterize(ch, self.font_px);
                if m.width == 0 || m.height == 0 {
                    // Empty glyph (space etc): cache a blank placement so we
                    // don't retry, and skip drawing -- matches render.rs.
                    let g = Glyph { u0: 0.0, v0: 0.0, u1: 0.0, v1: 0.0, w: 0.0, h: 0.0, bearing_x: 0.0, bearing_y: 0.0 };
                    self.glyphs.insert(key, g);
                    return;
                }
                // Atlas full: refuse the glyph rather than pack it (an
                // out-of-bounds write_texture origin would panic mid-frame) --
                // matches render.rs's `pack_coverage(..)?` early return. Not
                // cached, so a later frame may retry (harmless: atlas
                // exhaustion is not expected for a terminal's glyph
                // repertoire, per ATLAS_SIZE's doc comment).
                let Some(uv) = self.pack(queue, m.width as u32, m.height as u32, &cov) else {
                    return;
                };
                let g = Glyph {
                    u0: uv[0],
                    v0: uv[1],
                    u1: uv[2],
                    v1: uv[3],
                    w: m.width as f32,
                    h: m.height as f32,
                    bearing_x: m.xmin as f32,
                    bearing_y: m.ymin as f32,
                };
                self.glyphs.insert(key, g);
                g
            }
        };
        if g.w == 0.0 || g.h == 0.0 {
            return; // blank glyph (space), cached above -- nothing to draw
        }
        // fontdue's ymin is the offset of the bitmap's BOTTOM from the baseline,
        // and our y grows downward, so the top edge is baseline - (h + ymin).
        // Both rounded to whole pixels, matching render.rs:762-763 exactly: with
        // a NEAREST sampler, fractional placement makes edge fragments sample
        // the neighbouring atlas texel (the "faint per-glyph outline" render.rs
        // warns about at 754-761).
        let px = (x + g.bearing_x).round();
        let py = (y + self.ascent - (g.h + g.bearing_y)).round();
        self.push_uv_quad(px, py, g.w, g.h, [g.u0, g.v0, g.u1, g.v1], fg);
    }

    /// A solid rectangle: same pipeline, UV pinned to the full-coverage texel.
    pub fn push_quad(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        let t = 0.5 / ATLAS_SIZE as f32; // centre of texel (0,0)
        self.push_uv_quad(x, y, w, h, [t, t, t, t], c);
    }

    fn push_uv_quad(&mut self, x: f32, y: f32, w: f32, h: f32, uv: [f32; 4], c: Color) {
        let col = [c.0, c.1, c.2, c.3]; // render.rs Color is already 0..1 RGBA
        let (x0, y0, x1, y1) = (x, y, x + w, y + h);
        let v = |px, py, u, vv| Vertex { pos: [px, py], uv: [u, vv], color: col };
        self.verts.extend_from_slice(&[
            v(x0, y0, uv[0], uv[1]),
            v(x1, y0, uv[2], uv[1]),
            v(x1, y1, uv[2], uv[3]),
            v(x0, y0, uv[0], uv[1]),
            v(x1, y1, uv[2], uv[3]),
            v(x0, y1, uv[0], uv[3]),
        ]);
    }

    /// Upload this frame's vertices and issue ONE draw call, then reset.
    pub fn flush(&mut self, queue: &wgpu::Queue, pass: &mut wgpu::RenderPass) {
        if self.verts.is_empty() {
            return;
        }
        if self.verts.len() > self.vbuf_cap {
            let mut cap = self.vbuf_cap.max(1);
            while cap < self.verts.len() {
                cap *= 2;
            }
            self.vbuf = self.device.create_buffer(&wgpu::BufferDescriptor {
                label: Some("rt text vbuf"),
                size: (cap * std::mem::size_of::<Vertex>()) as u64,
                usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            });
            self.vbuf_cap = cap;
        }
        queue.write_buffer(&self.ubuf, 0, bytemuck::cast_slice(&self.screen));
        queue.write_buffer(&self.vbuf, 0, bytemuck::cast_slice(&self.verts));
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.vbuf.slice(..));
        pass.draw(0..self.verts.len() as u32, 0..1);
        self.verts.clear();
    }
}

// Runs on kiku only (`cargo test -p rt --bin rt`): needs the real macOS system
// fonts on disk, which Linux CI does not have. Proves the coverage fallback
// chain actually falls through -- not just that it builds -- by picking a
// character the primary face (Courier New) genuinely lacks and asserting
// `resolve_face` returns a DIFFERENT, covering face rather than silently
// drawing notdef from index 0.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn coverage_fallback_resolves_a_character_the_primary_lacks() {
        let blobs = crate::load_fonts().expect("macOS system fonts (see REGULAR_FONTS in main.rs)");
        let regular = parse_chain(&blobs.regular);
        let bold = parse_chain(&blobs.bold);
        let italic = parse_chain(&blobs.italic);
        let bold_italic = parse_chain(&blobs.bold_italic);
        assert!(!regular.is_empty(), "no usable primary font parsed from FontBlobs.regular");

        // U+28FF, FULL BRAILLE PATTERN (also probed: box-drawing U+2500, which
        // Courier New/SF Mono/Andale Mono in fact DO cover, so it wouldn't make
        // the test capable of failing). Courier New (the primary, chosen so
        // regular/bold/italic/bold-italic share one advance width -- see
        // REGULAR_FONTS's doc comment in main.rs) has no braille glyphs; Apple
        // Braille.ttf, appended to the regular chain as a coverage fallback,
        // does.
        let probe = '⣿';

        let primary_covers = regular[0].lookup_glyph_index(probe) != 0;
        assert!(
            !primary_covers,
            "primary face unexpectedly covers U+28FF -- pick a probe character it genuinely lacks"
        );

        let covering_idx = regular
            .iter()
            .position(|f| f.lookup_glyph_index(probe) != 0)
            .expect(
                "no loaded macOS face covers U+28FF -- is Apple Braille.ttf missing from this machine?",
            );

        let resolved = resolve_face(&regular, &bold, &italic, &bold_italic, probe, false, false);
        assert!(
            resolved.lookup_glyph_index(probe) != 0,
            "resolve_face returned a face that does not cover the probe character"
        );
        assert!(
            std::ptr::eq(resolved, &regular[covering_idx]),
            "chain fell through to the wrong face instead of the one that actually covers U+28FF"
        );
        // Specifically must not be index 0 (the notdef fallback path).
        assert_ne!(covering_idx, 0, "test setup bug: primary already covers the probe character");
    }
}
