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
    /// The four faces, indexed by `font_for(bold, italic)`. Bold/italic/bold-italic
    /// are `Option` because a font set may not supply them; `font_for` falls back
    /// to regular, matching what render.rs does.
    fonts: (Font, Option<Font>, Option<Font>, Option<Font>),
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
        // Mirrors render.rs: take the first blob in each set that fontdue can
        // actually read (some CFF/OTF files it cannot), regular being required.
        let pick = |set: &Vec<Vec<u8>>| -> Option<Font> {
            set.iter()
                .find_map(|b| Font::from_bytes(b.as_slice(), fontdue::FontSettings::default()).ok())
        };
        let regular = pick(&blobs.regular).ok_or("wgpu_text: no usable regular font")?;
        let bold = pick(&blobs.bold);
        let italic = pick(&blobs.italic);
        let bold_italic = pick(&blobs.bold_italic);

        let lm = regular
            .horizontal_line_metrics(font_px)
            .ok_or("wgpu_text: font has no horizontal line metrics")?;
        let cell_h = (lm.ascent - lm.descent + lm.line_gap).ceil();
        // Monospace: every advance is the same, so 'M' is representative.
        let cell_w = regular.metrics('M', font_px).advance_width.ceil();

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
            fonts: (regular, bold, italic, bold_italic),
        })
    }

    pub fn cell_size(&self) -> (f32, f32) { (self.cell_w, self.cell_h) }

    /// The face for this style, falling back to regular when a set is absent --
    /// the same fallback render.rs uses, so a font pack missing an italic face
    /// renders upright rather than blank.
    fn font_for(&self, bold: bool, italic: bool) -> &Font {
        let (r, b, i, bi) = &self.fonts;
        match (bold, italic) {
            (true, true) => bi.as_ref().or(b.as_ref()).or(i.as_ref()).unwrap_or(r),
            (true, false) => b.as_ref().unwrap_or(r),
            (false, true) => i.as_ref().unwrap_or(r),
            (false, false) => r,
        }
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
                let font = self.font_for(bold, italic);
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
