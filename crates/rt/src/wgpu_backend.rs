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

    /// One edge of `bell_stripe`: alternating yellow/black segments along the
    /// band, giving the classic caution-tape look. Mirrors render.rs's private
    /// `striped_edge` (render.rs:720-739) exactly.
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
                self.text.push_quad(x + o, y, seg, h, c);
            } else {
                self.text.push_quad(x, y + o, w, seg, c);
            }
            o += seg;
            i += 1;
        }
    }
}

/// Whether `end_frame` needs to open a render pass and submit, given whether
/// this call still owns `begin_frame`'s encoder (`had_encoder` -- `true`
/// only for the FIRST `end_frame` in a frame) and how many vertices
/// `self.text` has queued. Extracted as a free function -- mirroring
/// `wgpu_text.rs`'s `mask_key`/`line_corners`/`corners_to_verts` pattern of
/// pulling logic out of `wgpu::Device`-touching methods -- so the decision
/// that was the actual bug is unit-testable without a live GPU:
/// `WgpuBackend` itself can't be constructed in a test (`new` needs a real
/// `Arc<dyn Window>` and Metal adapter), so this is the strongest testable
/// surface for that decision.
///
/// The first call must always submit: its encoder carries `begin_frame`'s
/// clear, and returning early would mean the clear itself is never
/// submitted (a black/stale window), even if nothing has been drawn yet. A
/// LATER call has no clear riding along -- only submit it if there is new
/// geometry to flush; an empty later call would cost a wasted command
/// buffer for a pass that draws nothing.
fn end_frame_should_submit(had_encoder: bool, pending_verts: usize) -> bool {
    had_encoder || pending_verts > 0
}

impl Backend for WgpuBackend {
    fn cell_size(&self) -> (f32, f32) { (self.cell_w, self.cell_h) }

    fn resize(&mut self, w: f32, h: f32) {
        self.text.set_screen(w, h); // updates the uniform buffer's screen size
    }

    fn reload_fonts(&mut self, blobs: &FontBlobs, font_px: f32) -> Result<(), String> {
        self.text = crate::wgpu_text::TextPipeline::new(&self.device, &self.queue, self.config.format, blobs, font_px)?;
        let (w, h) = self.text.cell_size();
        self.cell_w = w;
        self.cell_h = h;
        // Rebuilding TextPipeline resets `screen` to its [1.0, 1.0] placeholder
        // (see `new`'s comment). `Backend::resize` is still a no-op (Task 6), so
        // without re-seeding here a font-size change leaves text permanently
        // off-screen: nothing else will ever call `set_screen` again.
        self.text.set_screen(self.config.width as f32, self.config.height as f32);
        Ok(())
    }

    fn begin_frame(&mut self, bg: Color) { self.start(bg, None); }
    fn begin_frame_scissored(&mut self, bg: Color, bbox: PxRect) { self.start(bg, Some(bbox)); }
    fn clear_scissor(&mut self) { self.scissor = None; }

    // Drawing primitives arrive in Tasks 5-7. Every rect-shaped one here is a
    // quad through the glyph pipeline's `push_quad` -- no new pipeline, no
    // extra draw call. Geometry mirrors render.rs's GL reference exactly (same
    // thickness formulas, same offsets) so a cursor or underline is pixel-for-
    // pixel identical to Linux; see render.rs:665-783 for the originals.
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        self.text.push_quad(x, y, w, h, c);
    }
    fn fill_cell(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let (x, y) = (ox + col as f32 * self.cell_w, oy + row as f32 * self.cell_h);
        self.text.push_quad(x, y, self.cell_w, self.cell_h, color);
    }
    fn draw_char(&mut self, ox: f32, oy: f32, col: usize, row: usize, ch: char, fg: Color, bold: bool, italic: bool) {
        let x = ox + col as f32 * self.cell_w;
        let y = oy + row as f32 * self.cell_h;
        self.text.push_glyph(&self.queue, x, y, ch, fg, bold, italic);
    }
    // render.rs:769-774: a thin bar just under the text baseline, not a fixed
    // fraction of the cell -- ties the underline to where the glyphs actually
    // sit.
    fn draw_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let x = ox + col as f32 * self.cell_w;
        let y = oy + row as f32 * self.cell_h + self.text.ascent() + 1.0;
        let thick = (self.cell_h / 16.0).max(1.0);
        self.text.push_quad(x, y, self.cell_w, thick, color);
    }
    // render.rs:778-783: through the x-height, ~60% of the ascent down from the
    // cell top.
    fn draw_strikeout(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let x = ox + col as f32 * self.cell_w;
        let y = oy + row as f32 * self.cell_h + self.text.ascent() * 0.6;
        let thick = (self.cell_h / 16.0).max(1.0);
        self.text.push_quad(x, y, self.cell_w, thick, color);
    }
    // render.rs:682-690: four thin quads, not a filled cell -- the glyph
    // underneath must stay visible.
    fn cursor_hollow(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let x = ox + col as f32 * self.cell_w;
        let y = oy + row as f32 * self.cell_h;
        let (w, h) = (self.cell_w, self.cell_h);
        let t = (h / 16.0).max(1.0);
        self.text.push_quad(x, y, w, t, color); // top
        self.text.push_quad(x, y + h - t, w, t, color); // bottom
        self.text.push_quad(x, y, t, h, color); // left
        self.text.push_quad(x + w - t, y, t, h, color); // right
    }
    // render.rs:694-699: a chunky bar on the cell bottom, distinct from a text
    // underline.
    fn cursor_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let x = ox + col as f32 * self.cell_w;
        let th = (self.cell_h / 8.0).max(2.0);
        let y = oy + row as f32 * self.cell_h + self.cell_h - th;
        self.text.push_quad(x, y, self.cell_w, th, color);
    }
    // render.rs:702-707: a thin vertical bar at the cell's left.
    fn cursor_beam(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let x = ox + col as f32 * self.cell_w;
        let y = oy + row as f32 * self.cell_h;
        let bw = (self.cell_w / 8.0).max(2.0);
        self.text.push_quad(x, y, bw, self.cell_h, color);
    }
    // render.rs:712-739: a yellow/black hazard-stripe frame just inside
    // `(x,y,w,h)`, not a solid fill -- see `striped_edge` below.
    fn bell_stripe(&mut self, x: f32, y: f32, w: f32, h: f32) {
        const T: f32 = 5.0; // band thickness
        self.striped_edge(x, y, w, T, true); // top
        self.striped_edge(x, y + h - T, w, T, true); // bottom
        self.striped_edge(x, y, T, h, false); // left
        self.striped_edge(x + w - T, y, T, h, false); // right
    }

    // Instrument shapes (Task 7): anti-aliased circles/lines for the
    // patchbay/gauges, via `TextPipeline`'s cached coverage masks -- same
    // atlas, same pipeline, same draw call as glyphs and solid quads. Geometry
    // mirrors render.rs's GL reference exactly; see wgpu_text.rs's `mask`,
    // `fill_circle`, `stroke_circle`, `stroke_line` for the formulas.
    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, c: Color) {
        self.text.fill_circle(&self.queue, cx, cy, r, c);
    }
    fn stroke_circle(&mut self, cx: f32, cy: f32, r: f32, width: f32, c: Color) {
        self.text.stroke_circle(&self.queue, cx, cy, r, width, c);
    }
    fn stroke_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Color) {
        self.text.stroke_line(&self.queue, x0, y0, x1, y1, width, c);
    }

    // Left as the trait's no-op defaults, deliberately: that split exists so
    // XRenderBackend can keep instruments on a persistent surface redrawn at
    // 6fps, avoiding re-shipping geometry over `ssh -X` (xrender_backend.rs:839
    // / :859). wgpu repaints instruments inline every frame like GlBackend does
    // (see `is_gl()` below), so it has nothing to do between them.

    // `main.rs`'s `redraw_full` calls `end_frame` TWICE per frame by design:
    // once after `draw_panes` to flush pane geometry, and again after
    // `paint_overlays_or_instruments` to flush menu/preferences-dialog
    // geometry that pass batches on top. `GlBackend::end_frame` handles this
    // by simply re-flushing its GL vertex buffer both times (it has no
    // encoder to consume) -- see gl_backend.rs. This backend must produce the
    // same result: EVERY call flushes whatever has accumulated in `self.text`
    // since the previous call, into the CURRENT frame, in painter's order.
    //
    // The wrinkle wgpu adds is the encoder. `begin_frame` creates one and
    // records the clear into it, but does not submit -- submission is this
    // function's job, so the clear rides along with the first flush. Only
    // ONE encoder can hold that clear, and it must be consumed on the FIRST
    // call (`self.encoder.take()` is `Some` then, `None` on every call after).
    // The old code returned immediately when `encoder` was `None` -- i.e. on
    // every call after the first -- which silently dropped any geometry
    // pushed since: it stayed in `self.text`'s vertex buffer (nothing ever
    // called `flush`, so nothing cleared it) and was drawn at the START of
    // the NEXT frame's first `end_frame`, ahead of that frame's own content.
    // That's the reported bug: a pane's menu/title-bar geometry, batched
    // after the content flush, appearing one frame late and underneath
    // everything painted since.
    fn end_frame(&mut self) {
        let Some(frame) = self.frame.as_ref() else { return };
        // `Some` only for the call that still owns `begin_frame`'s encoder
        // (and therefore its unsubmitted clear) -- see `end_frame_should_submit`.
        let had_encoder = self.encoder.is_some();
        let pending = self.text.pending_vertex_count();
        if !end_frame_should_submit(had_encoder, pending) {
            return;
        }
        // First call this frame: take the encoder `begin_frame` built (it
        // carries the clear). Any later call: that encoder is already gone
        // (submitted by the first call), so open a fresh one -- there is new
        // geometry to flush (`end_frame_should_submit` guarantees that for a
        // `None` encoder) but nothing else queued on it.
        let mut encoder = self.encoder.take().unwrap_or_else(|| self.device.create_command_encoder(&Default::default()));
        let view = frame.texture.create_view(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("text"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    // Load, NOT Clear, on every call including this fresh
                    // encoder's: begin_frame's clear (or an earlier end_frame's
                    // geometry) is already on the texture, and clearing again
                    // would erase it.
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

// Runs on kiku only (`cargo test -p rt --bin rt`): this whole module is
// `cfg(target_os = "macos")`'d out of the tree everywhere else (see main.rs),
// so there is no way to compile, let alone run, these tests on Linux CI.
//
// `end_frame_should_submit` is a pure function precisely so this doesn't need
// a `wgpu::Device` -- see its doc comment for why `WgpuBackend` itself can't
// be built in a test. This test module is what would have caught the
// double-`end_frame`-per-frame regression: the OLD `end_frame` decided
// whether to proceed with `self.encoder.take().is_some()` alone, i.e.
// `had_encoder` with `pending_verts` never even consulted. Reproduce that on
// `end_frame_should_submit`'s inputs and
// `end_frame_must_flush_a_later_call_with_pending_geometry` below fails: a
// later call (no encoder left -- the first call already took and submitted
// it) with geometry queued (the overlay/menu batch) returns `false`, so
// `end_frame` bails out without ever calling `TextPipeline::flush`. That
// geometry then sits in `self.text`'s vertex buffer, undrawn and
// un-cleared, until the NEXT frame's first `end_frame` finally flushes it --
// ahead of that next frame's own content. That's the reported bug: a pane's
// menu/title-bar drawn one frame late and underneath everything painted
// since.
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;

    #[test]
    fn end_frame_must_submit_the_first_call_even_with_no_geometry() {
        // The first call's encoder carries begin_frame's clear. Even if
        // nothing has been pushed since (a blank frame), it must still be
        // submitted -- otherwise the clear itself is silently dropped.
        assert!(end_frame_should_submit(true, 0), "the clear-carrying first call must always submit");
    }

    #[test]
    fn end_frame_must_flush_a_later_call_with_pending_geometry() {
        // The actual bug. A second end_frame in the same frame (main.rs
        // calls it after paint_overlays_or_instruments) has already had its
        // encoder taken and submitted by the first call -- had_encoder is
        // false here -- but DOES have geometry queued (the menu/title-bar
        // batch). It must still flush. Under the OLD logic (`had_encoder`
        // alone, pending_verts never consulted) this input returns `false`:
        // this assertion is RED against that logic and GREEN against the fix.
        assert!(
            end_frame_should_submit(false, 3),
            "a later call with pending geometry must still flush it into the current frame"
        );
    }

    #[test]
    fn end_frame_may_skip_a_later_call_with_nothing_pending() {
        // No encoder left to carry a clear, and nothing new was pushed since
        // the previous flush -- opening a pass and submitting would draw
        // nothing. Not required for correctness, just avoids a wasted
        // command buffer every such call.
        assert!(!end_frame_should_submit(false, 0));
    }
}
