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
//! negligible.
//!
//! "Negligible" is measured, not assumed — see `wgpu_offscreen/bench.rs`, which
//! is the whole argument. On an M5 a full 177x61 redraw at 3024x1964 costs
//! **396us**, or 2.4% of a 60Hz frame and 4.8% of a 120Hz one. Without a buffer
//! age the only *sound* partial path is a persistent surface-sized texture
//! blitted to the drawable every frame, and that blit alone costs 118us — so the
//! design's floor is ~30% of the full redraw it would replace, for a saving of
//! ~274us on a keystroke frame, at 23.8 MB of texture per window. Not worth the
//! correctness surface. `docs/macos-port-rulings.md` §2.6 carries the table.
//!
//! Scissored *drawing* still works (`begin_frame_scissored` maps onto a
//! render-pass scissor rect) and is now correct rather than merely present —
//! §2.6a, `wgpu_frame::background_op` and `wgpu_frame::scissor_rect`. Only
//! damage-limited *presenting* is unavailable.
use std::num::NonZeroU32;
use std::sync::Arc;

use winit::window::Window;

use crate::backend::Backend;
use crate::damage::PxRect;
use crate::render::{Color, FontBlobs};

/// The device-level wgpu objects, created ONCE and shared by every window.
///
/// rt is multi-window (new window, detach pane, tear-off), and only `Surface` is
/// genuinely per-window: a `Surface` is a `CAMetalLayer` bound to one `NSView`, while
/// `Instance`/`Adapter`/`Device`/`Queue` describe the GPU, which every window shares.
/// Building them per window meant a second `MTLDevice` and `MTLCommandQueue` for the same
/// GPU (so nothing could ever be shared between windows — wgpu resources belong to the
/// device that made them) plus a blocking `request_adapter` + `request_device` round trip
/// on every window open. All four are `Arc`-backed handles, so cloning one costs a
/// refcount and the clones name the same object.
///
/// Owned by `App` (`main.rs`), like `budget` and the drag/carry state — the App is what
/// sees every window. Lazily built by the first `WgpuBackend::new` and then kept for the
/// process's life, so closing every window and opening a new one does not pay for the
/// round trip again.
///
/// What is deliberately NOT in here: the glyph atlas, pipeline, bind group and vertex
/// buffer (`TextPipeline`). Those look shareable and are not. The atlas caches glyphs
/// under `(char, bold, italic)` with no font-size in the key, and font size is per window
/// — `Active::settings.font_size` — multiplied by that window's own `scale_factor`, which
/// differs between a Retina display and an external monitor. One shared atlas would serve
/// a window the other window's pixel size for the same character. Sharing them needs the
/// cache key to carry the rasterised size first; that is a design change to
/// `wgpu_text.rs`, not a wiring change, so it is left alone here. Now that every window's
/// `TextPipeline` is built on the SAME device, that change is possible at all — with a
/// device per window it was not.
pub struct WgpuShared {
    instance: wgpu::Instance,
    adapter: wgpu::Adapter,
    device: wgpu::Device,
    queue: wgpu::Queue,
}

pub struct WgpuBackend {
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) surface: wgpu::Surface<'static>,
    pub(crate) config: wgpu::SurfaceConfiguration,
    /// The frame being built between `begin_frame` and `end_frame`.
    pub(crate) frame: Option<wgpu::SurfaceTexture>,
    /// The colour `begin_frame` asked for, applied as the FIRST `end_frame` pass's
    /// `LoadOp::Clear` — see `needs_clear`.
    pub(crate) clear: Color,
    /// Set by `begin_frame`, cleared by the first `end_frame` pass of that frame: "the
    /// clear has not reached the drawable yet". This is the whole of Metal's clear —
    /// there is no separate clear pass, because on a tile-based deferred GPU an
    /// otherwise-empty `LoadOp::Clear` + `StoreOp::Store` pass writes the entire
    /// framebuffer out to memory only for the next pass's `LoadOp::Load` to read it
    /// straight back (~23.8 MB each way at 3024×1964 — about 2.85 GB/s at 60fps) for a
    /// pass that draws nothing.
    pub(crate) needs_clear: bool,
    pub(crate) scissor: Option<PxRect>,
    pub(crate) cell_w: f32,
    pub(crate) cell_h: f32,
    text: crate::wgpu_text::TextPipeline,
}

impl WgpuBackend {
    /// Build the backend for one window. `shared` is `App`'s process-wide
    /// [`WgpuShared`]: empty on the first window (this fills it in), reused by every
    /// window after — see that type for what is shared and what deliberately is not.
    pub fn new(
        shared: &mut Option<WgpuShared>,
        window: Arc<dyn Window>,
        font_blobs: &FontBlobs,
        font_px: f32,
    ) -> Result<Self, String> {
        // The instance must exist before the surface, and the surface before the adapter
        // (`compatible_surface`), so the three are threaded in that order rather than
        // taken from `shared` in one go.
        let instance = match shared.as_ref() {
            Some(s) => s.instance.clone(),
            None => wgpu::Instance::new(&wgpu::InstanceDescriptor {
                backends: wgpu::Backends::METAL,
                ..Default::default()
            }),
        };
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("wgpu: create_surface failed: {e}"))?;
        // Clone out of `shared` FIRST (owned handles, no borrow left outstanding) so the
        // `None` arm below can write back into it.
        let existing = shared.as_ref().map(|s| (s.adapter.clone(), s.device.clone(), s.queue.clone()));
        let (adapter, device, queue) = match existing {
            // Second and later windows: no adapter/device round trip, and — because
            // every window's resources now come from one device — a future shared atlas
            // is expressible at all.
            Some(t) => t,
            None => {
                // wgpu 27's request_adapter/request_device already return `Result`
                // (older wgpu returned `Option`), so no extra `.ok_or_else` is needed
                // around the `pollster::block_on` — just `.map_err`.
                let adapter =
                    pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
                        power_preference: wgpu::PowerPreference::LowPower, // a terminal is not a game
                        // Only the FIRST window's surface picks the adapter. On macOS
                        // that is not a constraint in practice: Metal exposes one
                        // adapter per GPU and every `CAMetalLayer` on the machine is
                        // compatible with it, so a window opened later on a different
                        // display resolves to the same adapter this one did.
                        compatible_surface: Some(&surface),
                        force_fallback_adapter: false,
                    }))
                    .map_err(|e| format!("wgpu: no Metal adapter: {e}"))?;
                let (device, queue) =
                    pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
                        label: Some("rt"),
                        ..Default::default()
                    }))
                    .map_err(|e| format!("wgpu: request_device failed: {e}"))?;
                *shared = Some(WgpuShared {
                    instance,
                    adapter: adapter.clone(),
                    device: device.clone(),
                    queue: queue.clone(),
                });
                (adapter, device, queue)
            }
        };

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
            clear: Color(0.0, 0.0, 0.0, 1.0),
            needs_clear: false,
            scissor: None,
            cell_w,
            cell_h,
            text,
        })
    }

    /// Begin a frame, clearing to `bg`. `scissor` limits later draws — and, since
    /// this backend's fix for it landed, the clear too.
    ///
    /// The clear is RECORDED here, not issued: the first render pass `end_frame` opens
    /// applies it (see `needs_clear`). A pass of its own would be a full framebuffer
    /// store + load on a tile-based GPU for a pass that draws nothing.
    ///
    /// *How* it is applied depends on `scissor`, and that distinction is not
    /// cosmetic. Unscissored it is the pass's `LoadOp::Clear`, which is free on a
    /// tile-based GPU. Scissored it CANNOT be: a load op is the tile initialiser,
    /// it covers the whole attachment before any fragment exists, and
    /// `set_scissor_rect` only ever discards fragments — so a scissored frame with
    /// a load-op clear would blank every pane outside the damage rect and repaint
    /// only what is inside it. `end_frame` therefore loads and paints the
    /// background as an unblended quad instead; see `wgpu_frame::background_op`
    /// and `wgpu_text::paint_background`.
    fn start(&mut self, bg: Color, scissor: Option<PxRect>) {
        self.clear = bg;
        self.scissor = scissor;
        // Belt and braces against a frame that ended without a flush (a `present` that
        // never ran, a caller that pushed geometry after the last `end_frame`): a new
        // frame starts from an empty vertex list, never with the tail of an older one.
        // `end_frame` already discards on the path that can produce it -- see
        // `EndFrameAction::DiscardGeometry` -- so in the healthy case this is a no-op.
        self.text.discard_pending();
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
                        // No drawable: `frame` stays None. The caller (`redraw_full`)
                        // cannot know that and will draw a whole frame's geometry into
                        // `self.text` regardless; `end_frame` discards it rather than
                        // letting it survive into the next frame that does acquire.
                        return;
                    }
                }
            }
        };
        self.frame = Some(frame);
        self.needs_clear = true; // the first end_frame pass applies it
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
    // The wrinkle wgpu adds is the clear. `begin_frame` does not issue one --
    // it records `needs_clear`, and the FIRST pass this function opens carries
    // it as its `LoadOp::Clear`; every later pass in the same frame LOADS, so
    // the clear (and the earlier pass's geometry) survives. An earlier version
    // opened a clear-only pass in `begin_frame` instead: on a tile-based
    // deferred GPU that stores the whole framebuffer and the next pass loads it
    // straight back, ~23.8 MB each way per frame at 3024x1964, for a pass that
    // draws nothing.
    //
    // Two exits from this function have now shipped the same bug, so both are
    // named in `EndFrameAction` and neither is decided here:
    //
    //  * returning when the encoder was `None` -- i.e. on every call after the
    //    first -- dropped the overlay batch into the next frame;
    //  * returning when `self.frame` was `None` -- a frame whose drawable could
    //    not be acquired -- did the same with a whole grid of pane geometry.
    //
    // In both cases the geometry stayed in `self.text`'s vertex buffer (nothing
    // called `flush`, and `flush` is the only thing that clears it) and was
    // drawn at the START of the next frame that did paint, ahead of that
    // frame's own content: a pane's menu/title-bar one frame late and
    // underneath everything painted since ("menu under panes, disks under
    // titlebars"), double-blending through rt's translucent background.
    fn end_frame(&mut self) {
        use crate::wgpu_frame::{background_op, end_frame_action, scissor_rect, BackgroundOp, EndFrameAction};
        let action =
            end_frame_action(self.frame.is_some(), self.needs_clear, self.text.pending_vertex_count());
        let clear = match action {
            // No drawable: this frame will never be presented, so its geometry
            // must not survive into the one that is. See the enum's doc comment.
            EndFrameAction::DiscardGeometry => {
                self.text.discard_pending();
                self.needs_clear = false;
                return;
            }
            EndFrameAction::Skip => return,
            EndFrameAction::Submit { clear } => clear,
        };
        let Some(frame) = self.frame.as_ref() else { return }; // Submit implies Some
        // How this pass lays the background down. `LoadOp::Clear` is only legal
        // when the frame owns the WHOLE surface: a load op is the tile
        // initialiser, running before any fragment exists, so the scissor set
        // below cannot clip it. A scissored frame loads and paints the
        // background as a quad instead. See `wgpu_frame::background_op`.
        let bg = background_op(clear, self.scissor.is_some());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        let view = frame.texture.create_view(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("text"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: match bg {
                            // render.rs's Color is already normalised 0..1 AND carries
                            // alpha, so this is a straight widen. The alpha matters:
                            // it is what lets the vibrancy show through, and at 1.0 the
                            // frosted glass is invisible.
                            BackgroundOp::ClearAll => wgpu::LoadOp::Clear(wgpu::Color {
                                r: self.clear.0 as f64,
                                g: self.clear.1 as f64,
                                b: self.clear.2 as f64,
                                a: self.clear.3 as f64,
                            }),
                            // Later passes of the same frame: the background and the
                            // earlier pass's geometry are already on the texture, and
                            // clearing again would erase them. A scissored FIRST pass
                            // also loads -- it paints its background below instead.
                            BackgroundOp::LoadAndPaint | BackgroundOp::LoadOnly => wgpu::LoadOp::Load,
                        },
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
            if let Some(r) = self.scissor {
                // Clamped, not cast: `PxRect` is signed and wgpu's scissor is
                // u32-and-validated, so `r.x as u32` turns a negative origin
                // into ~4 billion and panics the frame. See
                // `wgpu_frame::scissor_rect`.
                let (x, y, w, h) = scissor_rect(r, self.config.width, self.config.height);
                pass.set_scissor_rect(x, y, w, h);
            }
            if bg == BackgroundOp::LoadAndPaint {
                // Where the load-op clear would have been, in the same place in
                // the order -- but as fragments, which the scissor above clips.
                self.text.paint_background(&self.queue, &mut pass, self.clear);
            }
            self.text.flush(&self.queue, &mut pass);
        }
        self.queue.submit(Some(encoder.finish()));
        self.needs_clear = false; // the clear is on the drawable now
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

// The `end_frame` decision that was the actual bug -- twice -- is NOT tested here.
// It lives in `wgpu_frame.rs` as a pure function over three booleans/counts, with no
// wgpu types, precisely so Linux CI runs its tests: this module is
// `cfg(target_os = "macos")`'d out of the tree everywhere else (see main.rs), so a test
// placed here would run only on a Mac someone remembered to run it on. `WgpuBackend`
// itself cannot be constructed in a test either way (`new` needs a real
// `Arc<dyn Window>` and a Metal adapter), so the pure decision is the strongest
// testable surface regardless of where it sits -- and off the Mac is strictly better.
