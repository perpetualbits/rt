//! What a partial redraw would actually cost on Metal, measured.
//!
//! The question this module was written to answer: rt's macOS backend repaints
//! the whole window every frame, because wgpu exposes neither `EGL_EXT_buffer_age`
//! nor `eglSwapBuffersWithDamage` and `CAMetalLayer` has no damage-rect concept.
//! Without a buffer age you do not know which back buffer you were handed, so
//! scissoring straight into the swapchain texture is unsound — you would
//! composite this frame's damage onto a buffer two frames old.
//!
//! The one shape that sidesteps that is a **persistent offscreen texture** the
//! size of the surface: draw only the damaged region into it (it is yours, so
//! its age is always 1), then blit it to the swapchain every frame. The saving
//! is not re-rasterising every glyph; the price is that every frame now touches
//! the full framebuffer more times — the offscreen pass loads and stores it (a
//! render pass's load/store covers the whole attachment; `set_scissor_rect`
//! discards fragments, it does not shrink the tile region), and the blit reads it
//! back in. On a tile-based deferred GPU that price is paid in memory bandwidth
//! whether one cell changed or all of them, which is why this had to be measured
//! rather than assumed.
//!
//! ## Reading the numbers, and why they are wall clock
//!
//! `TIMESTAMP_QUERY` is not usable here. wgpu 27 advertises the feature on this
//! adapter and `create_query_set` succeeds, but every resolved pair comes back
//! `(0, 0)`: Apple GPUs do not implement Metal's
//! `MTLCounterSamplingPointAtStageBoundary`, which is where wgpu-hal writes
//! beginning-/end-of-pass timestamps, and the failure is silent. (Measured;
//! `TIMESTAMP_QUERY_INSIDE_ENCODERS` and `..._INSIDE_PASSES` are both absent
//! too, so there is no other write point either.)
//!
//! So the GPU side is measured as **throughput**: submit `ITERS` frames back to
//! back and block once at the end, which is how a real app pipelines frames and
//! which amortises away the fixed submit-and-block latency. That latency is not
//! small — the "empty submit" line prints it, and it is over a millisecond, big
//! enough to drown every difference this benchmark is looking for if it were
//! left in a per-frame figure.
//!
//! `cpu` is the time spent building the frame's vertex list, timed on its own
//! inside the loop; it is the work damage tracking actually saves. `gpu` is
//! everything else per frame — encoding, the vertex upload, and the GPU's own
//! work. `total` is what a frame costs.
//!
//! ## The grid
//!
//! rt multiplies the user's font size by the window's backing scale factor
//! before it reaches `TextPipeline`, so 14pt on a Retina display rasterises at
//! `font_px = 28.0`. That halves the columns and the rows, and so quarters the
//! cell count, against a naive 14.0. Both are measured below because rt runs on
//! both: an external 1080p monitor at 1x is a genuinely denser grid in cells
//! than the built-in Retina panel is, even though it has a quarter the pixels.
//!
//! Run it on the Mac:
//!
//! ```text
//! cargo test --bin rt --release -- --ignored --nocapture damage_cost
//! ```
//!
//! Nothing here is compiled into rt: the whole module is `cfg(test)`.

use std::time::Instant;

use super::{wait, Harness, FORMAT};
use crate::damage::PxRect;
use crate::render::Color;
use crate::wgpu_text::TextPipeline;

const BG: Color = Color(0.05, 0.05, 0.07, 0.85); // rt's translucent background
const FG: Color = Color(0.85, 0.86, 0.88, 1.0);

const WARMUP: usize = 20;
const ITERS: usize = 60;
/// How many times the whole set of variants is re-measured. The variants are
/// interleaved and each one's median taken, because the machine drifts: an
/// early version of this benchmark ran each variant once, end to end, and
/// reported a ~20% spread between two measurements of *identical* CPU work
/// purely from clock ramping. Interleaving puts every variant through the same
/// drift.
const REPS: usize = 7;

/// One measured configuration's per-frame cost, nanosecond medians.
#[derive(Clone, Copy)]
struct Timing {
    cpu: u128,
    gpu: u128,
}

impl Timing {
    fn total(&self) -> u128 {
        self.cpu + self.gpu
    }
}

fn median(mut v: Vec<u128>) -> u128 {
    v.sort_unstable();
    v[v.len() / 2]
}

/// Push `rows` rows of terminal cells — a background fill and a glyph each —
/// exactly as `draw_panes` does through `Backend`.
fn push_rows(t: &mut TextPipeline, queue: &wgpu::Queue, rows: std::ops::Range<usize>, cols: usize, cw: f32, ch: f32) {
    for row in rows {
        for col in 0..cols {
            let (x, y) = (col as f32 * cw, row as f32 * ch);
            t.push_quad(x, y, cw, ch, BG);
            // A rotating repertoire rather than one character: all cache hits
            // after warm-up, as in a real screenful, but with a different atlas
            // UV per cell so nothing collapses into one degenerate quad.
            let g = char::from_u32(0x41 + ((row * 7 + col) % 58) as u32).unwrap_or('x');
            t.push_glyph(queue, x, y + ch * 0.75, g, FG, false, false);
        }
    }
}

/// Blit pipeline: one full-screen triangle sampling the persistent offscreen
/// texture, blending off so the offscreen's RGBA (alpha included — rt's
/// translucency is what the vibrancy layer shows through) lands verbatim.
struct Blit {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
}

const BLIT_WGSL: &str = r#"
@group(0) @binding(0) var src: texture_2d<f32>;
@group(0) @binding(1) var samp: sampler;
struct VsOut { @builtin(position) pos: vec4<f32>, @location(0) uv: vec2<f32> };
@vertex
fn vs(@builtin(vertex_index) i: u32) -> VsOut {
  var p = array<vec2<f32>, 3>(vec2<f32>(-1.0, -1.0), vec2<f32>(3.0, -1.0), vec2<f32>(-1.0, 3.0));
  var o: VsOut;
  o.pos = vec4<f32>(p[i], 0.0, 1.0);
  o.uv = vec2<f32>(p[i].x * 0.5 + 0.5, 0.5 - p[i].y * 0.5);
  return o;
}
@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> { return textureSample(src, samp, in.uv); }
"#;

impl Blit {
    fn new(device: &wgpu::Device, src: &wgpu::TextureView) -> Blit {
        let bgl = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("blit bgl"),
            entries: &[
                wgpu::BindGroupLayoutEntry {
                    binding: 0,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Texture {
                        sample_type: wgpu::TextureSampleType::Float { filterable: false },
                        view_dimension: wgpu::TextureViewDimension::D2,
                        multisampled: false,
                    },
                    count: None,
                },
                wgpu::BindGroupLayoutEntry {
                    binding: 1,
                    visibility: wgpu::ShaderStages::FRAGMENT,
                    ty: wgpu::BindingType::Sampler(wgpu::SamplerBindingType::NonFiltering),
                    count: None,
                },
            ],
        });
        let sampler = device.create_sampler(&wgpu::SamplerDescriptor {
            label: Some("blit sampler"),
            mag_filter: wgpu::FilterMode::Nearest,
            min_filter: wgpu::FilterMode::Nearest,
            mipmap_filter: wgpu::FilterMode::Nearest,
            ..Default::default()
        });
        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("blit bg"),
            layout: &bgl,
            entries: &[
                wgpu::BindGroupEntry { binding: 0, resource: wgpu::BindingResource::TextureView(src) },
                wgpu::BindGroupEntry { binding: 1, resource: wgpu::BindingResource::Sampler(&sampler) },
            ],
        });
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("blit"),
            source: wgpu::ShaderSource::Wgsl(BLIT_WGSL.into()),
        });
        let layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("blit layout"),
            bind_group_layouts: &[&bgl],
            push_constant_ranges: &[],
        });
        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("blit pipeline"),
            layout: Some(&layout),
            vertex: wgpu::VertexState { module: &shader, entry_point: Some("vs"), compilation_options: Default::default(), buffers: &[] },
            primitive: wgpu::PrimitiveState { topology: wgpu::PrimitiveTopology::TriangleList, ..Default::default() },
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs"),
                compilation_options: Default::default(),
                targets: &[Some(wgpu::ColorTargetState { format: FORMAT, blend: None, write_mask: wgpu::ColorWrites::ALL })],
            }),
            multiview: None,
            cache: None,
        });
        Blit { pipeline, bind_group }
    }

    fn draw(&self, enc: &mut wgpu::CommandEncoder, dst: &wgpu::TextureView) {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("blit pass"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: dst,
                depth_slice: None,
                resolve_target: None,
                // The triangle covers every pixel and replaces it, so the
                // drawable's previous contents are irrelevant: Clear, not Load,
                // so the tile region is never read in.
                ops: wgpu::Operations { load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT), store: wgpu::StoreOp::Store },
            })],
            ..Default::default()
        });
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.draw(0..3, 0..1);
    }
}

/// The fixed cost of submitting a command buffer and blocking until the GPU
/// reports it done, with no passes in it at all. Printed for context: it is
/// what makes a per-frame submit-and-block figure useless here.
fn bench_empty_submit(h: &Harness) -> u128 {
    let mut v = Vec::new();
    for i in 0..WARMUP + ITERS {
        let t0 = Instant::now();
        let enc = h.device.create_command_encoder(&Default::default());
        h.queue.submit(Some(enc.finish()));
        wait(&h.device);
        if i >= WARMUP {
            v.push(t0.elapsed().as_nanos());
        }
    }
    median(v)
}

/// Run `frame` for a warm-up burst and then `ITERS` times back to back, blocking
/// only once at the end. `frame` is handed a closure-scoped timer: it reports
/// the nanoseconds it spent building vertices, and everything else it does
/// (encoding, upload, GPU work) falls into `gpu`.
fn throughput(h: &Harness, mut frame: impl FnMut() -> u128) -> Timing {
    for _ in 0..WARMUP {
        frame();
    }
    wait(&h.device);
    let t0 = Instant::now();
    let mut cpu = 0u128;
    for _ in 0..ITERS {
        cpu += frame();
    }
    wait(&h.device);
    let wall = t0.elapsed().as_nanos();
    Timing { cpu: cpu / ITERS as u128, gpu: wall.saturating_sub(cpu) / ITERS as u128 }
}

/// Today's path: rebuild every cell, one pass with a `LoadOp::Clear`, present.
fn bench_full(h: &Harness, t: &mut TextPipeline, cols: usize, rows: usize, cw: f32, ch: f32) -> Timing {
    let dst = drawable(h);
    let dst_view = dst.create_view(&Default::default());
    throughput(h, || {
        let t0 = Instant::now();
        push_rows(t, &h.queue, 0..rows, cols, cw, ch);
        let cpu_ns = t0.elapsed().as_nanos();
        let mut enc = h.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("full"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &dst_view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color { r: BG.0 as f64, g: BG.1 as f64, b: BG.2 as f64, a: BG.3 as f64 }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            t.flush(&h.queue, &mut pass);
        }
        h.queue.submit(Some(enc.finish()));
        cpu_ns
    })
}

/// The proposed path: draw only `rows_dirty` rows into a persistent offscreen
/// texture, scissored, then blit the whole thing to the drawable.
fn bench_offscreen_blit(
    h: &Harness,
    t: &mut TextPipeline,
    cols: usize,
    rows_dirty: usize,
    cw: f32,
    ch: f32,
) -> Timing {
    use crate::wgpu_frame::scissor_rect;
    let dst = drawable(h);
    let dst_view = dst.create_view(&Default::default());
    let off = offscreen(h);
    let off_view = off.create_view(&Default::default());
    let blit = Blit::new(&h.device, &off_view);
    let damage = PxRect { x: 0, y: 0, w: h.w as i32, h: (rows_dirty as f32 * ch).ceil() as i32 };

    throughput(h, || {
        let t0 = Instant::now();
        push_rows(t, &h.queue, 0..rows_dirty, cols, cw, ch);
        let cpu_ns = t0.elapsed().as_nanos();
        let mut enc = h.device.create_command_encoder(&Default::default());
        {
            let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("damage into offscreen"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &off_view,
                    depth_slice: None,
                    resolve_target: None,
                    // The whole point of the persistent texture: everything
                    // outside the damage rect must survive, so the pass loads.
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            let (x, y, w, hh) = scissor_rect(damage, h.w, h.h);
            pass.set_scissor_rect(x, y, w, hh);
            t.paint_background(&h.queue, &mut pass, BG);
            t.flush(&h.queue, &mut pass);
        }
        blit.draw(&mut enc, &dst_view);
        h.queue.submit(Some(enc.finish()));
        cpu_ns
    })
}

/// The blit on its own, drawing nothing into the offscreen: the fixed toll the
/// design charges every frame no matter how small the damage.
fn bench_blit_only(h: &Harness) -> Timing {
    let dst = drawable(h);
    let dst_view = dst.create_view(&Default::default());
    let off = offscreen(h);
    let off_view = off.create_view(&Default::default());
    let blit = Blit::new(&h.device, &off_view);
    throughput(h, || {
        let mut enc = h.device.create_command_encoder(&Default::default());
        blit.draw(&mut enc, &dst_view);
        h.queue.submit(Some(enc.finish()));
        0
    })
}

/// A stand-in for the swapchain texture: `RENDER_ATTACHMENT` and nothing else,
/// exactly as `WgpuBackend`'s `SurfaceConfiguration` asks for.
///
/// The usage flags are not a detail. `Harness::target` also carries `COPY_SRC`
/// so the pixel tests can read it back, and that alone makes Metal give it a
/// layout that is measurably slower to render into — enough that an early run of
/// this benchmark had the offscreen+blit path beating the full redraw even when
/// it redrew every cell AND blitted, which is strictly more work. Benchmarking
/// against a texture the real path never uses is how you measure your own
/// harness.
fn drawable(h: &Harness) -> wgpu::Texture {
    h.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("rt drawable stand-in"),
        size: wgpu::Extent3d { width: h.w, height: h.h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
        view_formats: &[],
    })
}

/// A surface-sized persistent colour texture: what the damage design would keep
/// between frames.
fn offscreen(h: &Harness) -> wgpu::Texture {
    h.device.create_texture(&wgpu::TextureDescriptor {
        label: Some("rt persistent offscreen"),
        size: wgpu::Extent3d { width: h.w, height: h.h, depth_or_array_layers: 1 },
        mip_level_count: 1,
        sample_count: 1,
        dimension: wgpu::TextureDimension::D2,
        format: FORMAT,
        usage: wgpu::TextureUsages::RENDER_ATTACHMENT | wgpu::TextureUsages::TEXTURE_BINDING,
        view_formats: &[],
    })
}

/// Median of a set of repetitions, by total cost.
fn median_timing(mut v: Vec<Timing>) -> Timing {
    v.sort_by_key(|t| t.total());
    v[v.len() / 2]
}

fn us(ns: u128) -> String {
    format!("{:>8.1}", ns as f64 / 1000.0)
}

/// One display configuration: surface size in physical pixels, and the
/// `font_px` rt would rasterise at there.
fn run(label: &str, w: u32, h: u32, font_px: f32) {
    let harness = Harness::new(w, h);
    let mut t = harness.text_at(font_px);
    let (cw, ch) = t.cell_size();
    let cols = (w as f32 / cw) as usize;
    let rows = (h as f32 / ch) as usize;

    let dirty: Vec<(String, usize)> = vec![
        ("1 row (a keystroke / cursor blink)".to_string(), 1usize),
        ("10% of rows (a busy pane)".to_string(), (rows / 10).max(1)),
        ("50% of rows".to_string(), (rows / 2).max(1)),
        ("every row (full-screen redraw)".to_string(), rows),
    ];

    let mut full_s = Vec::new();
    let mut blit_s = Vec::new();
    let mut part_s: Vec<Vec<Timing>> = vec![Vec::new(); dirty.len()];
    for _ in 0..REPS {
        full_s.push(bench_full(&harness, &mut t, cols, rows, cw, ch));
        blit_s.push(bench_blit_only(&harness));
        for (i, (_, d)) in dirty.iter().enumerate() {
            part_s[i].push(bench_offscreen_blit(&harness, &mut t, cols, *d, cw, ch));
        }
    }

    println!("\n=== {label}: {w}x{h} px, font_px {font_px}, cell {cw}x{ch} -> {cols} x {rows} = {} cells", cols * rows);
    println!("median of {REPS} interleaved repetitions of {ITERS} back-to-back frames, per-frame microseconds");
    println!("(for scale: ONE submit + block on an empty command buffer costs {}us)", us(bench_empty_submit(&harness)).trim());
    println!("\n{:<46} {:>8} {:>8} {:>8}", "path", "cpu", "gpu", "total");

    let full = median_timing(full_s);
    println!("{:<46} {} {} {}", "FULL redraw (today's macOS path)", us(full.cpu), us(full.gpu), us(full.total()));
    let blit = median_timing(blit_s);
    println!("{:<46} {} {} {}", "  the blit alone, offscreen untouched", us(blit.cpu), us(blit.gpu), us(blit.total()));

    for (i, (name, _)) in dirty.iter().enumerate() {
        let p = median_timing(std::mem::take(&mut part_s[i]));
        let verdict = if p.total() < full.total() {
            format!("{:.2}x faster", full.total() as f64 / p.total() as f64)
        } else {
            format!("{:.2}x SLOWER", p.total() as f64 / full.total() as f64)
        };
        println!("{:<46} {} {} {}   {verdict}", format!("  offscreen+blit, {name}"), us(p.cpu), us(p.gpu), us(p.total()));
    }
}

#[test]
#[ignore = "needs a Metal device; run on the Mac with --nocapture"]
fn damage_cost_full_redraw_versus_offscreen_and_blit() {
    println!();
    // The built-in panel of a 16" MacBook Pro, where rt's font is rasterised at
    // 2x the user's point size.
    run("Retina, built-in display", 3024, 1964, 28.0);
    // An external 1080p monitor at 1x: a quarter of the pixels, but a DENSER
    // grid in cells, which is where the CPU-side vertex building bites hardest.
    run("1x external monitor", 1920, 1080, 14.0);
    println!("\na 60Hz frame budget is 16666.7us; a 120Hz ProMotion frame is 8333.3us\n");
}
