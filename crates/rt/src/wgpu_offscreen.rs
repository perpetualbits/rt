//! Headless Metal harness for the macOS backend: a real `wgpu::Device`, a real
//! [`TextPipeline`](crate::wgpu_text::TextPipeline), and an offscreen colour
//! target that can be read back pixel for pixel — with no window, no drawable
//! and nothing on anybody's screen.
//!
//! It exists for two jobs that cannot be done any other way:
//!
//!  * **the scissored-clear pixel gate** below, the macOS counterpart of Linux's
//!    `tests/damage_pixel_identity.rs`. `wgpu_frame`'s unit tests prove rt asks
//!    for the right load op; only real pixels prove the load op does what the
//!    reasoning says, and that an unblended background quad lands the same RGBA
//!    a clear would.
//!  * **the damage cost measurement** in [`bench`], which is what decided
//!    whether partial redraw should be wired up at all. See that module.
//!
//! Test-only, and macOS-only twice over: `wgpu` is only a dependency there, and
//! a Metal device is the thing being measured.
#![cfg(all(test, target_os = "macos"))]

use crate::render::Color;
use crate::wgpu_text::TextPipeline;

/// BGRA8, matching what a `CAMetalLayer` surface actually hands rt
/// (`caps.formats[0]` on Metal), so the pipelines under test are built for the
/// same format they are built for in production.
pub const FORMAT: wgpu::TextureFormat = wgpu::TextureFormat::Bgra8Unorm;

/// A live headless Metal device plus an offscreen colour target.
pub struct Harness {
    pub device: wgpu::Device,
    pub queue: wgpu::Queue,
    pub target: wgpu::Texture,
    pub view: wgpu::TextureView,
    pub w: u32,
    pub h: u32,
}

impl Harness {
    pub fn new(w: u32, h: u32) -> Harness {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });
        // No `compatible_surface`: there is no window, and Metal exposes the
        // one adapter regardless.
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower,
            compatible_surface: None,
            force_fallback_adapter: false,
        }))
        .expect("no Metal adapter");
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("rt offscreen"),
            ..Default::default()
        }))
        .expect("request_device");
        let target = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rt offscreen target"),
            size: wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            mip_level_count: 1,
            sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: FORMAT,
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT
                | wgpu::TextureUsages::COPY_SRC
                | wgpu::TextureUsages::TEXTURE_BINDING,
            view_formats: &[],
        });
        let view = target.create_view(&Default::default());
        Harness { device, queue, target, view, w, h }
    }

    /// A `TextPipeline` on this device, with its screen size already seeded —
    /// exactly as `WgpuBackend::new` seeds it from the surface configuration.
    pub fn text(&self) -> TextPipeline {
        self.text_at(14.0)
    }

    /// As [`text`](Self::text), at a given rasterisation size. rt multiplies the
    /// user's font size by the window's backing scale factor before it gets
    /// here, so a 14pt font on a Retina display is `font_px = 28.0` — which
    /// halves the columns and rows a surface of a given pixel size holds, and
    /// therefore quarters the cell count. Getting that wrong makes a benchmark
    /// four times as pessimistic as the machine it claims to describe.
    pub fn text_at(&self, font_px: f32) -> TextPipeline {
        let blobs = crate::load_fonts().expect("macOS system fonts (see REGULAR_FONTS in main.rs)");
        let mut t = TextPipeline::new(&self.device, &self.queue, FORMAT, &blobs, font_px)
            .expect("TextPipeline::new");
        t.set_screen(self.w as f32, self.h as f32);
        t
    }

    /// Fill the whole target with one colour, standing in for "what the previous
    /// frame left on the drawable".
    pub fn prefill(&self, c: Color) {
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("prefill"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &self.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: wgpu::LoadOp::Clear(wgpu::Color {
                        r: c.0 as f64,
                        g: c.1 as f64,
                        b: c.2 as f64,
                        a: c.3 as f64,
                    }),
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        self.queue.submit(Some(enc.finish()));
        wait(&self.device);
    }

    /// Copy the target back to the CPU as `(r, g, b, a)` per pixel, row-major.
    ///
    /// The width must be a multiple of 64 so that `w * 4` is a multiple of
    /// wgpu's 256-byte `COPY_BYTES_PER_ROW_ALIGNMENT` and the image comes out in
    /// one copy with no row padding to unpick.
    pub fn read_pixels(&self) -> Vec<(u8, u8, u8, u8)> {
        assert_eq!(self.w % 64, 0, "width must keep the readback rows 256-byte aligned");
        let bytes = (self.w * self.h * 4) as u64;
        let buf = self.device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("rt readback"),
            size: bytes,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        let mut enc = self.device.create_command_encoder(&Default::default());
        enc.copy_texture_to_buffer(
            wgpu::TexelCopyTextureInfo {
                texture: &self.target,
                mip_level: 0,
                origin: wgpu::Origin3d::ZERO,
                aspect: wgpu::TextureAspect::All,
            },
            wgpu::TexelCopyBufferInfo {
                buffer: &buf,
                layout: wgpu::TexelCopyBufferLayout {
                    offset: 0,
                    bytes_per_row: Some(self.w * 4),
                    rows_per_image: Some(self.h),
                },
            },
            wgpu::Extent3d { width: self.w, height: self.h, depth_or_array_layers: 1 },
        );
        self.queue.submit(Some(enc.finish()));
        let slice = buf.slice(..);
        slice.map_async(wgpu::MapMode::Read, |r| r.expect("map_async"));
        wait(&self.device);
        let data = slice.get_mapped_range();
        // BGRA8 on the wire; hand callers RGBA so assertions read naturally.
        let out = data.chunks_exact(4).map(|p| (p[2], p[1], p[0], p[3])).collect();
        drop(data);
        buf.unmap();
        out
    }

    pub fn px(&self, pixels: &[(u8, u8, u8, u8)], x: u32, y: u32) -> (u8, u8, u8, u8) {
        pixels[(y * self.w + x) as usize]
    }
}

/// Block until the GPU has finished everything submitted so far.
pub fn wait(device: &wgpu::Device) {
    device
        .poll(wgpu::PollType::Wait { submission_index: None, timeout: None })
        .expect("device.poll");
}

/// Run one render pass over `h.view` the way `WgpuBackend::end_frame` does,
/// including the background decision — so this harness exercises the real
/// `wgpu_frame::background_op` and `wgpu_frame::scissor_rect`, not a copy of
/// them. `draw` pushes the frame's geometry into the pipeline.
pub fn frame(
    h: &Harness,
    text: &mut TextPipeline,
    bg: Color,
    scissor: Option<crate::damage::PxRect>,
    owns_background: bool,
    draw: impl FnOnce(&mut TextPipeline),
) {
    use crate::wgpu_frame::{background_op, scissor_rect, BackgroundOp};
    draw(text);
    let op = background_op(owns_background, scissor.is_some());
    let mut enc = h.device.create_command_encoder(&Default::default());
    {
        let mut pass = enc.begin_render_pass(&wgpu::RenderPassDescriptor {
            label: Some("frame"),
            color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                view: &h.view,
                depth_slice: None,
                resolve_target: None,
                ops: wgpu::Operations {
                    load: match op {
                        BackgroundOp::ClearAll => wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg.0 as f64,
                            g: bg.1 as f64,
                            b: bg.2 as f64,
                            a: bg.3 as f64,
                        }),
                        BackgroundOp::LoadAndPaint | BackgroundOp::LoadOnly => wgpu::LoadOp::Load,
                    },
                    store: wgpu::StoreOp::Store,
                },
            })],
            ..Default::default()
        });
        if let Some(r) = scissor {
            let (x, y, w, hh) = scissor_rect(r, h.w, h.h);
            pass.set_scissor_rect(x, y, w, hh);
        }
        if op == BackgroundOp::LoadAndPaint {
            text.paint_background(&h.queue, &mut pass, bg);
        }
        text.flush(&h.queue, &mut pass);
    }
    h.queue.submit(Some(enc.finish()));
    wait(&h.device);
}

// ---------------------------------------------------------------------------
// The pixel gate for the scissored clear.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod pixel_tests {
    use super::*;
    use crate::damage::PxRect;

    const W: u32 = 128;
    const H: u32 = 64;

    const RED: Color = Color(1.0, 0.0, 0.0, 1.0); // "the previous frame"
    const BG: Color = Color(0.0, 0.25, 0.5, 0.75); // rt's translucent background
    const WHITE: Color = Color(1.0, 1.0, 1.0, 1.0);

    #[test]
    fn a_scissored_frame_leaves_every_pixel_outside_the_damage_rect_alone() {
        // The bug this gate exists for: `begin_frame_scissored` used to record a
        // full-surface `LoadOp::Clear`, and a scissor cannot clip a load op. If
        // `wgpu_frame::background_op` ever goes back to answering `ClearAll` for
        // a scissored frame, the corner pixels below stop being red.
        let h = Harness::new(W, H);
        let mut text = h.text();
        h.prefill(RED);

        let damage = PxRect { x: 32, y: 16, w: 40, h: 20 };
        frame(&h, &mut text, BG, Some(damage), true, |t| {
            t.push_quad(36.0, 20.0, 8.0, 8.0, WHITE);
        });

        let px = h.read_pixels();
        for &(x, y) in &[(0u32, 0u32), (W - 1, 0), (0, H - 1), (W - 1, H - 1), (31, 16), (72, 16), (32, 15), (32, 36)] {
            assert_eq!(
                h.px(&px, x, y),
                (255, 0, 0, 255),
                "pixel ({x},{y}) is outside the damage rect and must still hold the previous frame"
            );
        }
    }

    #[test]
    fn the_scissored_background_replaces_rather_than_blends_with_the_old_frame() {
        // The subtler half. rt's background is deliberately translucent (that is
        // what the macOS vibrancy layer shows through), so painting it as a quad
        // through the normal ALPHA_BLENDING pipeline would COMPOSITE it over the
        // previous frame instead of replacing it -- the damage rect would drift a
        // little redder here, and darker in real use, every time it was
        // repainted. `paint_background` uses the unblended pipeline, so the
        // damage rect must come back as exactly BG and nothing of RED.
        let h = Harness::new(W, H);
        let mut text = h.text();
        h.prefill(RED);

        let damage = PxRect { x: 32, y: 16, w: 40, h: 20 };
        frame(&h, &mut text, BG, Some(damage), true, |_| {});

        let px = h.read_pixels();
        // 0.25 -> 64, 0.5 -> 128, 0.75 -> 191 (Bgra8Unorm rounds to nearest).
        let want = (0u8, 64u8, 128u8, 191u8);
        for &(x, y) in &[(32u32, 16u32), (50, 25), (71, 35)] {
            let got = h.px(&px, x, y);
            assert!(
                got.0 == 0 && (got.1 as i32 - want.1 as i32).abs() <= 1 && (got.2 as i32 - want.2 as i32).abs() <= 1 && (got.3 as i32 - want.3 as i32).abs() <= 1,
                "pixel ({x},{y}) = {got:?}, want ~{want:?} -- any red left in it means the background BLENDED over the previous frame instead of replacing it"
            );
        }
    }

    #[test]
    fn an_unscissored_frame_still_clears_the_whole_surface() {
        // The path every macOS frame actually takes. Nothing about the fix may
        // change it: a full redraw must still wipe the previous frame entirely.
        let h = Harness::new(W, H);
        let mut text = h.text();
        h.prefill(RED);
        frame(&h, &mut text, BG, None, true, |_| {});
        let px = h.read_pixels();
        for &(x, y) in &[(0u32, 0u32), (W - 1, H - 1), (64, 32)] {
            let got = h.px(&px, x, y);
            assert_eq!(got.0, 0, "pixel ({x},{y}) = {got:?} still has red in it after a full clear");
        }
    }

    #[test]
    fn a_negative_damage_origin_draws_instead_of_panicking() {
        // The second latent bug, end to end. `r.x as u32` on a negative origin
        // is 4294967286, which fails wgpu's `x + width <= attachment width`
        // validation -- and wgpu's default error handler panics, so the frame
        // takes the process with it. Clamped, the same rect simply draws its
        // on-screen part.
        let h = Harness::new(W, H);
        let mut text = h.text();
        h.prefill(RED);
        let damage = PxRect { x: -20, y: -10, w: 60, h: 40 };
        frame(&h, &mut text, BG, Some(damage), true, |_| {});
        let px = h.read_pixels();
        assert_eq!(h.px(&px, 0, 0).0, 0, "the on-screen part of the rect was painted");
        assert_eq!(h.px(&px, 45, 10), (255, 0, 0, 255), "and nothing beyond its clamped right edge was");
    }
}

pub mod bench;
