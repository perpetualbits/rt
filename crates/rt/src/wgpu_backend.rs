//! The macOS rendering backend: wgpu on Metal.
//!
//! A third `Backend` beside `GlBackend` (local Linux GL) and `XRenderBackend`
//! (remote `ssh -X`). Neither of those is touched by this file's existence —
//! they are `cfg`'d out on macOS and this one is `cfg`'d out everywhere else.
//!
//! ## Why this backend reports no damage support
//!
//! wgpu exposes neither buffer age nor swap-with-damage, and `CAMetalLayer` has
//! no damage-rect concept, so `partial_present_available()` is `false` and
//! `buffer_age()` is 0 ("unknown → redraw all"). `main.rs` therefore marks damage
//! full every frame here. That is deliberate: damage tracking exists for slow
//! boards and `ssh -X`, and a full terminal-grid redraw on an Apple GPU is
//! negligible. Scissored *drawing* still works (`begin_frame_scissored` maps onto
//! a render-pass scissor rect); only damage-limited *presenting* is unavailable.
use std::num::NonZeroU32;
use std::sync::Arc;

use winit::window::Window;

use crate::backend::Backend;
use crate::damage::PxRect;
use crate::render::{Color, FontBlobs};

pub struct WgpuBackend {
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) surface: wgpu::Surface<'static>,
    pub(crate) config: wgpu::SurfaceConfiguration,
    /// The frame being built between `begin_frame` and `end_frame`.
    pub(crate) frame: Option<wgpu::SurfaceTexture>,
    pub(crate) encoder: Option<wgpu::CommandEncoder>,
    pub(crate) clear: Color,
    pub(crate) scissor: Option<PxRect>,
    pub(crate) cell_w: f32,
    pub(crate) cell_h: f32,
    text: crate::wgpu_text::TextPipeline,
}

impl WgpuBackend {
    pub fn new(window: Arc<dyn Window>, font_blobs: &FontBlobs, font_px: f32) -> Result<Self, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("wgpu: create_surface failed: {e}"))?;
        // wgpu 27's request_adapter/request_device already return `Result`
        // (older wgpu returned `Option`), so no extra `.ok_or_else` is needed
        // around the `pollster::block_on` — just `.map_err`.
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower, // a terminal is not a game
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|e| format!("wgpu: no Metal adapter: {e}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("rt"),
            ..Default::default()
        }))
        .map_err(|e| format!("wgpu: request_device failed: {e}"))?;

        let size = window.surface_size();
        let caps = surface.get_capabilities(&adapter);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: caps.formats[0],
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            // PostMultiplied is what lets the NSVisualEffectView installed in
            // Task 8 show through. With Opaque you get a black window and the
            // frosted glass never appears.
            alpha_mode: if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
                wgpu::CompositeAlphaMode::PostMultiplied
            } else {
                caps.alpha_modes[0]
            },
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        let mut text = crate::wgpu_text::TextPipeline::new(&device, &queue, config.format, font_blobs, font_px)?;
        let (cell_w, cell_h) = text.cell_size();
        // Seed the uniform buffer's screen size from the surface's configured
        // (physical-pixel) size now. `Backend::resize` (Task 6) keeps it in sync
        // on later window resizes, but nothing calls `resize` on macOS between
        // construction and the first frame -- without this, `screen` stays at
        // `TextPipeline::new`'s default [1.0, 1.0] and every glyph's NDC position
        // divides by that, putting text far off-screen on the very first draw.
        text.set_screen(config.width as f32, config.height as f32);

        Ok(Self {
            device,
            queue,
            surface,
            config,
            frame: None,
            encoder: None,
            clear: Color(0.0, 0.0, 0.0, 1.0),
            scissor: None,
            cell_w,
            cell_h,
            text,
        })
    }

    /// Begin a frame, clearing to `bg`. `scissor` limits later draws.
    fn start(&mut self, bg: Color, scissor: Option<PxRect>) {
        self.clear = bg;
        self.scissor = scissor;
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            // Lost/outdated surface: reconfigure and skip this frame rather than
            // panicking. The next redraw picks it up.
            Err(_) => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    Ok(f) => f,
                    // Still failing after a reconfigure: log it. Silently
                    // skipping every frame here is otherwise indistinguishable
                    // from "the clear colour is wrong" — the worst failure mode
                    // during bring-up, when there's no other diagnostic signal.
                    Err(e) => {
                        log::warn!("wgpu: get_current_texture failed after reconfigure: {e}");
                        return;
                    }
                }
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // render.rs's Color is already normalised 0..1 AND
                        // carries alpha, so this is a straight widen. The alpha
                        // matters in Task 8: it is what lets the vibrancy show
                        // through, and at 1.0 the frosted glass is invisible.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg.0 as f64, g: bg.1 as f64, b: bg.2 as f64, a: bg.3 as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        self.frame = Some(frame);
        self.encoder = Some(encoder);
    }
}

impl Backend for WgpuBackend {
    fn cell_size(&self) -> (f32, f32) { (self.cell_w, self.cell_h) }

    fn resize(&mut self, _w: f32, _h: f32) {}

    fn reload_fonts(&mut self, blobs: &FontBlobs, font_px: f32) -> Result<(), String> {
        self.text = crate::wgpu_text::TextPipeline::new(&self.device, &self.queue, self.config.format, blobs, font_px)?;
        let (w, h) = self.text.cell_size();
        self.cell_w = w;
        self.cell_h = h;
        Ok(())
    }

    fn begin_frame(&mut self, bg: Color) { self.start(bg, None); }
    fn begin_frame_scissored(&mut self, bg: Color, bbox: PxRect) { self.start(bg, Some(bbox)); }
    fn clear_scissor(&mut self) { self.scissor = None; }

    // Drawing primitives arrive in Tasks 5-7. Empty, never `todo!()`: a stub that
    // panics would take the whole window down mid-frame during bring-up.
    fn fill_rect(&mut self, _x: f32, _y: f32, _w: f32, _h: f32, _c: Color) {}
    fn fill_cell(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _color: Color) {}
    fn draw_char(&mut self, ox: f32, oy: f32, col: usize, row: usize, ch: char, fg: Color, bold: bool, italic: bool) {
        let x = ox + col as f32 * self.cell_w;
        let y = oy + row as f32 * self.cell_h;
        self.text.push_glyph(&self.queue, x, y, ch, fg, bold, italic);
    }
    fn draw_underline(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _color: Color) {}
    fn draw_strikeout(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _color: Color) {}
    fn cursor_hollow(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _color: Color) {}
    fn cursor_underline(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _color: Color) {}
    fn cursor_beam(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _color: Color) {}
    fn bell_stripe(&mut self, _x: f32, _y: f32, _w: f32, _h: f32) {}

    fn end_frame(&mut self) {
        let (Some(frame), Some(mut encoder)) = (self.frame.as_ref(), self.encoder.take()) else { return };
        let view = frame.texture.create_view(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("text"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    // Load, NOT Clear: begin_frame already cleared, and clearing
                    // again here would erase it.
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            if let Some(r) = self.scissor {
                pass.set_scissor_rect(r.x as u32, r.y as u32, r.w as u32, r.h as u32);
            }
            self.text.flush(&self.queue, &mut pass);
        }
        self.queue.submit(Some(encoder.finish()));
    }

    fn resize_surface(&mut self, w: NonZeroU32, h: NonZeroU32) {
        self.config.width = w.get();
        self.config.height = h.get();
        self.surface.configure(&self.device, &self.config);
    }

    /// `damage` is ignored — see the module header. Always returns `false`
    /// (no full-redraw fallback needed), and is effectively unreachable with
    /// `Some(..)` because the scissored path is gated on
    /// `partial_present_available()`.
    fn present(&mut self, _window: &dyn Window, _damage: Option<(PxRect, &[PxRect])>) -> bool {
        if let Some(frame) = self.frame.take() {
            frame.present();
        }
        false
    }

    fn full_swap(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.present();
        }
    }

    fn is_software(&self) -> bool { false }
    fn buffer_age(&self) -> u32 { 0 }
    fn partial_present_available(&self) -> bool { false }
    fn x11_present_active(&self) -> bool { false }

    /// `true` despite the name. `is_gl()` is documented as distinguishing ONLY
    /// how instruments are drawn — GL repaints them inline every frame, XRender
    /// keeps a 6fps persistent layer to avoid re-shipping geometry over `ssh -X`.
    /// wgpu wants the inline behaviour. Renaming this to
    /// `repaints_instruments_inline()` would touch both Linux backends and
    /// `main.rs`, breaking this port's "no Linux code path changes" guarantee;
    /// buying naming clarity with Linux churn is a bad trade.
    fn is_gl(&self) -> bool { true }
}
