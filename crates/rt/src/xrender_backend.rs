//! Mechanism C: a command-based X11/XRender rendering backend for the remote
//! (`ssh -X`) case. Instead of rendering glyphs to GL pixels and shipping the
//! bitmaps, it uploads each glyph to an XRender glyph set ONCE and draws text by
//! glyph-index reference (`CompositeGlyphs`), and fills backgrounds/cursor with
//! `FillRectangles` — so only tiny drawing *commands* cross the wire (like
//! Terminator). Draws directly into winit's existing X11 window via `x11rb`; no
//! GL context. X11 only; `try_new` returns `None` otherwise (caller keeps GL).
#![cfg(feature = "x11")]

use std::collections::HashMap;

use fontdue::Font;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;
use x11rb::connection::Connection;
use x11rb::protocol::render::{self, ConnectionExt as _, PictType, Pictformat, Pointfix, Triangle};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use crate::backend::Backend;
use crate::damage::PxRect;
use crate::render::{Color, FontBlobs};

/// A triangle in f32 window pixels (converted to XRender 16.16 fixed point at draw).
/// Used for the thin instrument LINES (wires/latency): a line is 2 triangles, so
/// this stays cheap AND anti-aliased. The round shapes (packets, jacks), by
/// contrast, are drawn as cached A8 masks (see `rasterise_*` + `shape_glyph`) —
/// re-tessellating a disc into ~30 inline triangles every frame was far too
/// expensive over ssh -X on a slow board (CPU + wire volume). A mask uploads
/// once and is stamped by a ~20-byte reference, like a font glyph.
type TriF = [(f32, f32); 3];

// Disc/ring coverage masks are shared with the GL backend (see `crate::raster`).
use crate::raster::{rasterize_disc, rasterize_ring};

/// Thick segment as a quad (2 triangles), butt caps, `width` centred on the line.
fn line_tris(x0: f32, y0: f32, x1: f32, y1: f32, width: f32) -> Vec<TriF> {
    let (dx, dy) = (x1 - x0, y1 - y0);
    let len = (dx * dx + dy * dy).sqrt().max(1e-6);
    // Unit normal, scaled to the half-width.
    let (nx, ny) = (-dy / len * width * 0.5, dx / len * width * 0.5);
    let a = (x0 + nx, y0 + ny);
    let b = (x1 + nx, y1 + ny);
    let c = (x1 - nx, y1 - ny);
    let d = (x0 - nx, y0 - ny);
    vec![[a, b, c], [a, c, d]]
}

/// Axis-aligned bounds `(x, y, w, h)` of a triangle list (for clip rejection).
fn tris_bbox(tris: &[TriF]) -> (f32, f32, f32, f32) {
    let mut lo = (f32::MAX, f32::MAX);
    let mut hi = (f32::MIN, f32::MIN);
    for t in tris {
        for &(x, y) in t {
            lo = (lo.0.min(x), lo.1.min(y));
            hi = (hi.0.max(x), hi.1.max(y));
        }
    }
    (lo.0, lo.1, hi.0 - lo.0, hi.1 - lo.1)
}

/// Convert rt's 0..1 float colour to XRender's 16-bit-per-channel colour.
fn to_render_color(c: Color) -> render::Color {
    let s = |v: f32| (v.clamp(0.0, 1.0) * 65535.0) as u16;
    render::Color { red: s(c.0), green: s(c.1), blue: s(c.2), alpha: s(c.3) }
}

/// Premultiply a straight-alpha colour into the 16-bit RENDER colour that
/// `Composite(OVER)` expects: each channel scaled by alpha, alpha preserved.
fn premultiply(c: Color) -> render::Color {
    let s = |v: f32| (v.clamp(0.0, 1.0) * 65535.0) as u16;
    render::Color { red: s(c.0 * c.3), green: s(c.1 * c.3), blue: s(c.2 * c.3), alpha: s(c.3) }
}

/// The RENDER colour to clear the background with, given the window `depth`.
///
/// On a 32-bit ARGB window the clear is PREMULTIPLIED (Wayland/Xwayland surfaces
/// expect premultiplied alpha, like the GL path's premultiplied `clear_color`) so
/// the compositor blends `background_opacity` correctly. On a 24-bit window there
/// is no alpha channel, so premultiplying would just darken the RGB — use the
/// straight colour, which the opaque drawable renders exactly as today. At the
/// default opacity 1.0 the two are identical.
fn bg_clear_color(depth: u8, bg: Color) -> render::Color {
    if depth == 32 {
        premultiply(bg)
    } else {
        to_render_color(bg)
    }
}

pub struct XRenderBackend {
    conn: RustConnection,
    window: u32,                // X window id (CopyArea destination)
    win_pic: render::Picture,   // the on-screen window, as an XRender Picture
    // Server-side back buffer: all drawing targets `back_pic`; `present` copies the
    // damaged region `back_pixmap`->window with a single server-side `CopyArea`, so
    // a full repaint never blanks the window (no flash) and still ships zero pixels.
    back_pixmap: xproto::Pixmap,
    back_pic: render::Picture,
    // The Picture that drawing primitives (`fill`, `stamp_shape`, `draw_tris`)
    // currently target. Normally `back_pic` (the content buffer); temporarily
    // switched to the instrument layer between `begin/end_instrument_layer`.
    dst_pic: render::Picture,
    // Instruments are drawn directly into the back buffer (`back_pic`) between
    // `begin/end_instrument_layer`, clipped to the frame scissor — NOT into a
    // separate ARGB layer that present() would composite. An earlier design kept
    // an offscreen instrument pixmap and composited it OVER the content on every
    // present; under Xwayland that per-frame software RENDER Composite cost the X
    // server ~73% CPU with instruments on (invisible in any rt-side profile) and
    // made typing over `ssh -X` unusable. Baking into the back buffer removed it.
    drawing_instruments: bool, // true between begin/end_instrument_layer: fill() uses OVER+premultiplied
    gc: xproto::Gcontext,       // GC for the pixmap->window CopyArea
    depth: u8,                  // window/pixmap depth (for back-buffer recreation)
    win_format: Pictformat,     // the window's Pictformat (for back-buffer recreation)
    a8_format: Pictformat,      // the A8 glyph mask format
    glyphset: render::Glyphset, // one shared glyph set (all styles)
    src_pixmap: xproto::Pixmap, // 1x1 repeating solid-colour source
    src_pic: render::Picture,   // the source Picture over `src_pixmap`
    src_pixmap_argb: xproto::Pixmap,// 1x1 repeating alpha-capable source pixmap
    src_pic_argb: render::Picture,  // the ARGB source Picture (AA primitives)
    // AA round shapes (packets/jacks) as cached A8 masks, keyed by (kind, r, w).
    // Uploaded once, stamped by reference (composite_glyphs) — cheap over ssh -X.
    shape_glyphset: render::Glyphset,
    shapes: HashMap<(u8, u32, u32), u32>, // (0=disc/1=ring, r*4, width*4) -> glyph id
    next_shape_id: u32,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
    fonts: Vec<Font>,      // regular chain (Slice 1: regular only; bold/italic reuse regular)
    glyph_px: f32,         // rasterisation size
    // glyph_id per (char) — Slice 1 keys on char only (regular face)
    glyphs: HashMap<char, u32>,
    next_glyph_id: u32,
    clip: Option<PxRect>,  // damage clip; None = whole window
    debug_fill: bool,      // RT_DEBUG_FILL: log every fill's float in / clip / int rect out
    // RT_XDIAG: per-second present / instrument-redraw / flush-blocking counters,
    // to diagnose ssh -X flooding on a fast client (present rate vs the link's
    // drain rate). flush() blocking time is the saturation signal.
    xdiag: bool,
    xd_last: std::time::Instant,
    xd_present: u32,
    xd_geom: u32,
    xd_flush: std::time::Duration,
    xd_dmg: u64,   // window pixels copied this second = the recomposite damage rt asks of the compositor
    win_w: u16,
    win_h: u16,
}

impl XRenderBackend {
    pub fn try_new(window: &Window, blobs: &FontBlobs, font_px: f32) -> Option<Self> {
        let win = match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Xlib(h) => h.window as u32,
            RawWindowHandle::Xcb(h) => h.window.get(),
            _ => return None, // Wayland: no X path
        };
        let (conn, _screen) = x11rb::connect(None).ok()?;

        // RENDER must be present.
        let ver = conn.render_query_version(0, 11).ok()?.reply().ok()?;
        let formats = render::query_pict_formats(&conn).ok()?.reply().ok()?;
        log::info!("xrender: RENDER {}.{}, {} formats, {} screens", ver.major_version, ver.minor_version, formats.formats.len(), formats.screens.len());

        // The window's visual → its Pictformat.
        let visual = conn.get_window_attributes(win).ok()?.reply().ok()?.visual;
        let win_format = match pictformat_for_visual(&formats, visual) {
            Some(f) => f,
            None => { log::warn!("xrender: no Pictformat for window visual {visual:#x}; falling back to GL"); return None; }
        };
        // An A8 (alpha-only, depth 8) format for glyphs.
        let a8_format = match a8_format(&formats) {
            Some(f) => f,
            None => { log::warn!("xrender: no A8 glyph format found; falling back to GL"); return None; }
        };
        let argb_format = match argb32_format(&formats) {
            Some(f) => f,
            None => { log::warn!("xrender: no 32-bit ARGB format; falling back to GL"); return None; }
        };

        // The window Picture.
        let win_pic = conn.generate_id().ok()?;
        render::create_picture(&conn, win_pic, win, win_format, &render::CreatePictureAux::new()).ok()?;

        // A 1x1 repeating solid source Picture (re-filled with the run colour).
        let geo = conn.get_geometry(win).ok()?.reply().ok()?;
        let depth = geo.depth;
        let src_pixmap = conn.generate_id().ok()?;
        conn.create_pixmap(depth, src_pixmap, win, 1, 1).ok()?;
        let src_pic = conn.generate_id().ok()?;
        let aux = render::CreatePictureAux::new().repeat(render::Repeat::NORMAL);
        render::create_picture(&conn, src_pic, src_pixmap, win_format, &aux).ok()?;

        // A 1x1 repeating 32-bit ARGB source for the alpha-blended AA primitives.
        let src_pixmap_argb = conn.generate_id().ok()?;
        conn.create_pixmap(32, src_pixmap_argb, win, 1, 1).ok()?;
        let src_pic_argb = conn.generate_id().ok()?;
        let aux_argb = render::CreatePictureAux::new().repeat(render::Repeat::NORMAL);
        render::create_picture(&conn, src_pic_argb, src_pixmap_argb, argb_format, &aux_argb).ok()?;

        let glyphset = conn.generate_id().ok()?;
        render::create_glyph_set(&conn, glyphset, a8_format).ok()?;
        // A second glyph set for the AA round-shape masks (discs/rings).
        let shape_glyphset = conn.generate_id().ok()?;
        render::create_glyph_set(&conn, shape_glyphset, a8_format).ok()?;

        // Server-side back buffer at the window's size + depth, and a GC for the
        // pixmap->window copy. Drawing goes here; `present` copies it to the window.
        let (win_w, win_h) = (geo.width.max(1), geo.height.max(1));
        let back_pixmap = conn.generate_id().ok()?;
        conn.create_pixmap(depth, back_pixmap, win, win_w, win_h).ok()?;
        let back_pic = conn.generate_id().ok()?;
        render::create_picture(&conn, back_pic, back_pixmap, win_format, &render::CreatePictureAux::new()).ok()?;
        let gc = conn.generate_id().ok()?;
        conn.create_gc(gc, win, &xproto::CreateGCAux::new()).ok()?;

        let fonts = parse_fonts(blobs)?;
        let (cell_w, cell_h, ascent) = measure_cell(&fonts[0], font_px);

        conn.flush().ok()?;
        log::info!(
            "xrender: ready (window={win:#x} depth={depth} cell={cell_w:.0}x{cell_h:.0})"
        );
        Some(Self {
            conn,
            window: win,
            win_pic,
            back_pixmap,
            back_pic,
            dst_pic: back_pic, // starts as the content buffer
            drawing_instruments: false,
            gc,
            depth,
            win_format,
            a8_format,
            glyphset,
            src_pixmap,
            src_pic,
            src_pixmap_argb,
            src_pic_argb,
            shape_glyphset,
            shapes: HashMap::new(),
            next_shape_id: 1,
            cell_w,
            cell_h,
            ascent,
            fonts,
            glyph_px: font_px,
            glyphs: HashMap::new(),
            next_glyph_id: 1,
            clip: None,
            debug_fill: std::env::var_os("RT_DEBUG_FILL").is_some(),
            xdiag: std::env::var_os("RT_XDIAG").is_some(),
            xd_last: std::time::Instant::now(),
            xd_present: 0,
            xd_geom: 0,
            xd_flush: std::time::Duration::ZERO,
            xd_dmg: 0,
            win_w,
            win_h,
        })
    }

    /// Glyph id for `ch`, rasterising + uploading it to the glyph set on first use.
    /// `None` if the glyph has no bitmap or can't be rasterised.
    fn glyph_id(&mut self, ch: char) -> Option<u32> {
        if let Some(&g) = self.glyphs.get(&ch) {
            return Some(g);
        }
        // Font fallback (mirrors the GL renderer): rasterise with the first font in
        // the regular chain that actually covers this glyph (`lookup_glyph_index != 0`),
        // so braille/box-drawing/etc. from fallback fonts render instead of the
        // primary font's notdef box. Fall back to the primary for a true notdef.
        let idx = self
            .fonts
            .iter()
            .position(|f| f.lookup_glyph_index(ch) != 0)
            .unwrap_or(0);
        let (m, bitmap) = self.fonts[idx].rasterize(ch, self.glyph_px);
        if m.width == 0 || m.height == 0 {
            // No pixels (e.g. control char): cache a "blank" so we don't retry, but
            // don't upload — return None so draw_char skips it.
            return None;
        }
        // XRender wants each A8 scanline padded to a 4-byte boundary.
        let stride = (m.width + 3) & !3;
        let mut data = vec![0u8; stride * m.height];
        for r in 0..m.height {
            data[r * stride..r * stride + m.width].copy_from_slice(&bitmap[r * m.width..(r + 1) * m.width]);
        }
        let info = render::Glyphinfo {
            width: m.width as u16,
            height: m.height as u16,
            x: (-m.xmin) as i16,                       // origin ← bitmap left
            y: (m.ymin + m.height as i32) as i16,      // origin ← bitmap top (ascent)
            x_off: m.advance_width.round() as i16,
            y_off: 0,
        };
        let gid = self.next_glyph_id;
        render::add_glyphs(&self.conn, self.glyphset, &[gid], &[info], &data).ok()?;
        self.next_glyph_id += 1;
        self.glyphs.insert(ch, gid);
        Some(gid)
    }

    fn fill(&self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        // TRIM to the damage clip — do not merely skip fills that miss it.
        //
        // Rejecting-but-not-trimming was a real bug: a fill that so much as
        // touched the scissor was issued at FULL size, painting over pixels
        // outside the damaged region. Nothing redraws those (that is the whole
        // point of scissoring), so they were destroyed. It ate pane titles: a
        // frame scissored to the right end of a titlebar (where the `buf` meter
        // lives) repainted the WHOLE titlebar background, while the title's own
        // character cells — outside the clip — were rejected by `draw_char` and
        // never redrawn. Hence "the title disappears, sometimes one character at
        // a time": `fill` clobbers by whole rects, `draw_char` restores by cells.
        //
        // Same family as the half-drawn borders that made `present()` full-window.
        // That fix made the WINDOW show the whole back buffer; it could not help
        // when the back buffer itself had been overwritten. This is the other half.
        let Some(rect) = trim_to_clip(x, y, w, h, self.clip) else {
            if self.debug_fill {
                log::info!("FILL in=({x:.3},{y:.3},{w:.3},{h:.3}) clip={:?} -> SKIP", self.clip);
            }
            return;
        };
        if self.debug_fill {
            log::info!(
                "FILL in=({x:.3},{y:.3},{w:.3},{h:.3}) clip={:?} -> rect=(x={} y={} w={} h={}) instr={}",
                self.clip, rect.x, rect.y, rect.width, rect.height, self.drawing_instruments,
            );
        }
        if self.drawing_instruments {
            // Instruments bake into the content buffer with OVER (premultiplied so
            // alpha blends correctly), trimmed to the frame scissor by `rect` above.
            let _ = render::fill_rectangles(&self.conn, render::PictOp::OVER, self.dst_pic, premultiply(c), &[rect]);
        } else {
            // Content buffer: opaque SRC, exactly as before.
            let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.dst_pic, to_render_color(c), &[rect]);
        }
    }

    /// Set the 1x1 repeating ARGB source to `c` (straight alpha), premultiplied as
    /// OVER expects, ready to be modulated by a shape mask or a triangle mesh.
    fn set_argb_src(&self, c: Color) {
        let premult = premultiply(c);
        let one = xproto::Rectangle { x: 0, y: 0, width: 1, height: 1 };
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.src_pic_argb, premult, &[one]);
    }

    /// Glyph id for a disc (`kind` 0) or ring (`kind` 1) of the given radius/width,
    /// rasterising + uploading its A8 mask on first use (then cached). The glyph
    /// origin is the shape centre, so it stamps centred on the pen position.
    fn shape_glyph(&mut self, kind: u8, r: f32, width: f32) -> Option<u32> {
        // Quantise to 0.25px so a handful of masks cover every instrument shape.
        let key = (kind, (r * 4.0).round() as u32, (width * 4.0).round() as u32);
        if let Some(&g) = self.shapes.get(&key) {
            return Some(g);
        }
        let (w, h, cov) = if kind == 1 { rasterize_ring(r, width) } else { rasterize_disc(r) };
        if w == 0 || h == 0 {
            return None;
        }
        let rad = (r.ceil().max(1.0)) as i16; // origin offset from top-left = centre
        // A8 scanlines padded to a 4-byte boundary (as add_glyphs requires).
        let stride = (w as usize + 3) & !3;
        let mut data = vec![0u8; stride * h as usize];
        for row in 0..h as usize {
            data[row * stride..row * stride + w as usize]
                .copy_from_slice(&cov[row * w as usize..(row + 1) * w as usize]);
        }
        let info = render::Glyphinfo { width: w, height: h, x: rad, y: rad, x_off: 0, y_off: 0 };
        let gid = self.next_shape_id;
        render::add_glyphs(&self.conn, self.shape_glyphset, &[gid], &[info], &data).ok()?;
        self.next_shape_id += 1;
        self.shapes.insert(key, gid);
        Some(gid)
    }

    /// Stamp shape glyph `gid` centred at `(dx, dy)` using the current ARGB source.
    fn stamp_shape(&self, gid: u32, dx: i16, dy: i16) {
        let mut cmd = Vec::with_capacity(12);
        cmd.push(1u8); // one glyph
        cmd.extend_from_slice(&[0u8, 0, 0]); // pad
        cmd.extend_from_slice(&dx.to_ne_bytes());
        cmd.extend_from_slice(&dy.to_ne_bytes());
        cmd.extend_from_slice(&gid.to_ne_bytes());
        let _ = render::composite_glyphs32(
            &self.conn, render::PictOp::OVER, self.src_pic_argb, self.dst_pic,
            self.a8_format, self.shape_glyphset, 0, 0, &cmd,
        );
    }

    /// Composite a triangle mesh in colour `c` (straight alpha) onto the back
    /// buffer with anti-aliasing — used for the thin instrument LINES (2 triangles
    /// each). `render::triangles` OVER through the A8 mask format gives per-edge
    /// coverage. Server-side geometry — zero wire pixels.
    fn draw_tris(&self, tris: &[TriF], c: Color) {
        if tris.is_empty() { return; }
        // Clip rejection: skip meshes wholly outside the damage clip.
        if let Some(b) = self.clip {
            let (x, y, w, h) = tris_bbox(tris);
            if !rect_intersects(x, y, w, h, b) { return; }
        }
        self.set_argb_src(c);
        // f32 window px → 16.16 fixed point.
        let fx = |v: f32| (v * 65536.0).round() as i32;
        let mk = |(x, y): (f32, f32)| Pointfix { x: fx(x), y: fx(y) };
        let hw: Vec<Triangle> = tris.iter().map(|t| Triangle { p1: mk(t[0]), p2: mk(t[1]), p3: mk(t[2]) }).collect();
        let _ = render::triangles(
            &self.conn, render::PictOp::OVER, self.src_pic_argb, self.dst_pic,
            self.a8_format, 0, 0, &hw,
        );
    }

    /// Recreate the back buffer at the current window size (after a resize). The
    /// new pixmap's contents are undefined, but the resize path arms a full redraw,
    /// so `begin_frame` clears and repaints it before `present` copies it out.
    fn recreate_back(&mut self) {
        let (w, h) = (self.win_w.max(1), self.win_h.max(1));
        let _ = render::free_picture(&self.conn, self.back_pic);
        let _ = self.conn.free_pixmap(self.back_pixmap);
        if let Ok(pm) = self.conn.generate_id() {
            if self.conn.create_pixmap(self.depth, pm, self.window, w, h).is_ok() {
                self.back_pixmap = pm;
                if let Ok(pic) = self.conn.generate_id() {
                    let _ = render::create_picture(&self.conn, pic, pm, self.win_format, &render::CreatePictureAux::new());
                    self.back_pic = pic;
                }
            }
        }
        // keep the draw target pointing at the new back buffer (invalidated on resize)
        self.dst_pic = self.back_pic;
    }
}

/// Trim a float fill rect to `clip` (if any) and convert it to an integer pixel
/// `Rectangle`, or `None` when nothing lands inside the clip.
///
/// The subtle part is FLOORING the trimmed origin before deriving the width and
/// height. A pane whose split is not cell-aligned has a FRACTIONAL origin, so its
/// cells sit at `x.5` positions. The damage clip is integer pixels, so its far
/// edge cuts the cell's trailing sub-pixel; then, without flooring, the fractional
/// origin loses the LEADING sub-pixel to the `as u16` width truncation as well —
/// two half-pixels gone, a whole pixel column dropped. That produced the 1px black
/// gaps at cell boundaries in right/nested panes' titlebars, and the missing
/// cursor right-edge, on scissored XRender frames. Confirmed with RT_DEBUG_FILL on
/// the milkv: a cell `x=892.5 w=11` under `clip.right=903` came out `width=10`
/// (lost px 902), and the cursor's 1px right edge `x=902.5 w=1` came out `width=0`
/// (not drawn). Flooring the origin restores both to their full width.
///
/// Full-frame fills (`clip == None`) are unaffected — they already tile, because
/// the truncation of consistently-fractional cell origins is itself consistent.
fn trim_to_clip(x: f32, y: f32, w: f32, h: f32, clip: Option<PxRect>) -> Option<xproto::Rectangle> {
    let (mut x, mut y, mut w, mut h) = (x, y, w, h);
    if let Some(b) = clip {
        if !rect_intersects(x, y, w, h, b) {
            return None; // wholly outside the damage
        }
        let (x0, y0) = (x.max(b.x as f32), y.max(b.y as f32));
        let (x1, y1) = ((x + w).min(b.right() as f32), (y + h).min(b.bottom() as f32));
        if x1 <= x0 || y1 <= y0 {
            return None; // degenerate after trimming
        }
        // Floor the origin FIRST, then derive size to the (clip-clamped) far edge,
        // so the leading sub-pixel is not also lost. See the doc comment.
        x = x0.floor();
        y = y0.floor();
        w = x1 - x;
        h = y1 - y;
    }
    let width = w.max(0.0) as u16;
    let height = h.max(0.0) as u16;
    if width == 0 || height == 0 {
        return None; // nothing to paint
    }
    Some(xproto::Rectangle { x: x as i16, y: y as i16, width, height })
}

fn rect_intersects(x: f32, y: f32, w: f32, h: f32, b: PxRect) -> bool {
    let (x0, y0, x1, y1) = (x, y, x + w, y + h);
    let (bx0, by0, bx1, by1) = (b.x as f32, b.y as f32, (b.x + b.w) as f32, (b.y + b.h) as f32);
    x0 < bx1 && bx0 < x1 && y0 < by1 && by0 < y1
}

/// Find the Pictformat that backs `visual` in the screens table.
fn pictformat_for_visual(formats: &render::QueryPictFormatsReply, visual: u32) -> Option<Pictformat> {
    for screen in &formats.screens {
        for depth in &screen.depths {
            for v in &depth.visuals {
                if v.visual == visual {
                    return Some(v.format);
                }
            }
        }
    }
    None
}

/// Find an alpha-only depth-8 (A8) Direct format for glyphs.
fn a8_format(formats: &render::QueryPictFormatsReply) -> Option<Pictformat> {
    formats
        .formats
        .iter()
        .find(|f| f.type_ == PictType::DIRECT && f.depth == 8 && f.direct.alpha_mask == 0xff && f.direct.red_mask == 0)
        .map(|f| f.id)
}

/// Find a 32-bit DIRECT ARGB format for the alpha-blended solid source (packet
/// glow needs true alpha, unlike the opaque 24-bit `src_pic`).
fn argb32_format(formats: &render::QueryPictFormatsReply) -> Option<Pictformat> {
    formats
        .formats
        .iter()
        .find(|f| f.type_ == PictType::DIRECT && f.depth == 32 && f.direct.alpha_mask == 0xff)
        .map(|f| f.id)
}

fn parse_fonts(blobs: &FontBlobs) -> Option<Vec<Font>> {
    let mut out = Vec::new();
    for b in &blobs.regular {
        if let Ok(f) = Font::from_bytes(b.as_slice(), fontdue::FontSettings::default()) {
            out.push(f);
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn measure_cell(font: &Font, font_px: f32) -> (f32, f32, f32) {
    let (m, _) = font.rasterize('M', font_px);
    let line = font.horizontal_line_metrics(font_px);
    let cell_w = m.advance_width.ceil().max(1.0);
    match line {
        Some(l) => (cell_w, l.new_line_size.ceil().max(1.0), l.ascent),
        None => (cell_w, font_px.ceil().max(1.0), font_px * 0.8),
    }
}

impl Backend for XRenderBackend {
    fn cell_size(&self) -> (f32, f32) {
        (self.cell_w, self.cell_h)
    }
    fn resize(&mut self, w: f32, h: f32) {
        self.win_w = w as u16;
        self.win_h = h as u16;
    }
    fn reload_fonts(&mut self, blobs: &FontBlobs, font_px: f32) -> Result<(), String> {
        self.fonts = parse_fonts(blobs).ok_or("no usable font")?;
        let (cw, ch, asc) = measure_cell(&self.fonts[0], font_px);
        self.cell_w = cw;
        self.cell_h = ch;
        self.ascent = asc;
        self.glyph_px = font_px;
        // Old glyph ids are stale (different rasterisation): drop the cache and
        // rebuild the server-side GlyphSet from scratch so no glyphs are orphaned.
        self.glyphs.clear();
        let new_set = self.conn.generate_id().map_err(|e| e.to_string())?;
        render::create_glyph_set(&self.conn, new_set, self.a8_format).map_err(|e| e.to_string())?;
        let _ = render::free_glyph_set(&self.conn, self.glyphset);
        self.glyphset = new_set;
        self.next_glyph_id = 1;
        Ok(())
    }

    fn begin_frame(&mut self, bg: Color) {
        if self.debug_fill {
            log::info!("--- FRAME begin_frame (FULL) ---");
        }
        self.clip = None;
        // Clear the whole BACK buffer (off-screen) — the window is untouched until
        // `present` copies the finished frame, so a full repaint never flashes.
        let rect = xproto::Rectangle { x: 0, y: 0, width: self.win_w, height: self.win_h };
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.back_pic, bg_clear_color(self.depth, bg), &[rect]);
    }
    fn begin_frame_scissored(&mut self, bg: Color, bbox: PxRect) {
        if self.debug_fill {
            log::info!("--- FRAME begin_frame_scissored bbox={bbox:?} ---");
        }
        self.clip = Some(bbox);
        let rect = xproto::Rectangle { x: bbox.x as i16, y: bbox.y as i16, width: bbox.w as u16, height: bbox.h as u16 };
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.back_pic, bg_clear_color(self.depth, bg), &[rect]);
    }
    fn clear_scissor(&mut self) {
        self.clip = None;
    }
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        self.fill(x, y, w, h, c);
    }
    fn fill_cell(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        self.fill(ox + col as f32 * self.cell_w, oy + row as f32 * self.cell_h, self.cell_w, self.cell_h, color);
    }
    fn draw_char(&mut self, ox: f32, oy: f32, col: usize, row: usize, ch: char, fg: Color, _bold: bool, _italic: bool) {
        if ch == ' ' {
            return; // space: no glyph
        }
        let x = ox + col as f32 * self.cell_w;
        let y = oy + row as f32 * self.cell_h;
        if let Some(b) = self.clip {
            if !rect_intersects(x, y, self.cell_w, self.cell_h, b) {
                return;
            }
        }
        let gid = match self.glyph_id(ch) {
            Some(g) => g,
            None => return, // unrasterisable → skip
        };
        // Set the 1x1 solid source to the fg colour.
        let one = xproto::Rectangle { x: 0, y: 0, width: 1, height: 1 };
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.src_pic, to_render_color(fg), &[one]);
        // Composite the glyph at the cell's pen baseline.
        let dx = x.round() as i16;
        let dy = (y + self.ascent).round() as i16;
        let mut cmd = Vec::with_capacity(12);
        cmd.push(1u8); // one glyph in this element
        cmd.extend_from_slice(&[0u8, 0, 0]); // pad
        cmd.extend_from_slice(&dx.to_ne_bytes());
        cmd.extend_from_slice(&dy.to_ne_bytes());
        cmd.extend_from_slice(&gid.to_ne_bytes()); // u32 glyph id
        let _ = render::composite_glyphs32(
            &self.conn, render::PictOp::OVER, self.src_pic, self.back_pic,
            self.a8_format, self.glyphset, 0, 0, &cmd,
        );
    }
    fn draw_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let t = (self.cell_h / 16.0).max(1.0);
        self.fill(ox + col as f32 * self.cell_w, oy + row as f32 * self.cell_h + self.cell_h - t, self.cell_w, t, color);
    }
    fn draw_strikeout(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let t = (self.cell_h / 16.0).max(1.0);
        self.fill(ox + col as f32 * self.cell_w, oy + row as f32 * self.cell_h + self.cell_h * 0.5, self.cell_w, t, color);
    }
    fn cursor_hollow(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let (x, y) = (ox + col as f32 * self.cell_w, oy + row as f32 * self.cell_h);
        let t = 1.0;
        self.fill(x, y, self.cell_w, t, color);
        self.fill(x, y + self.cell_h - t, self.cell_w, t, color);
        self.fill(x, y, t, self.cell_h, color);
        self.fill(x + self.cell_w - t, y, t, self.cell_h, color);
    }
    fn cursor_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let t = 2.0;
        self.fill(ox + col as f32 * self.cell_w, oy + row as f32 * self.cell_h + self.cell_h - t, self.cell_w, t, color);
    }
    fn cursor_beam(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        self.fill(ox + col as f32 * self.cell_w, oy + row as f32 * self.cell_h, 2.0, self.cell_h, color);
    }
    fn bell_stripe(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let c = Color::rgb(0xff, 0xcc, 0x00);
        let t = 3.0;
        self.fill(x, y, w, t, c);
        self.fill(x, y + h - t, w, t, c);
    }
    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, c: Color) {
        // Cached A8 mask stamped by reference — cheap over ssh -X (no per-frame
        // tessellation, no inline geometry). AA + alpha preserved.
        if r <= 0.0 {
            return;
        }
        let Some(gid) = self.shape_glyph(0, r, 0.0) else { return };
        self.set_argb_src(c);
        self.stamp_shape(gid, cx.round() as i16, cy.round() as i16);
    }
    fn stroke_circle(&mut self, cx: f32, cy: f32, r: f32, width: f32, c: Color) {
        if r <= 0.0 || width <= 0.0 {
            return;
        }
        let Some(gid) = self.shape_glyph(1, r, width) else { return };
        self.set_argb_src(c);
        self.stamp_shape(gid, cx.round() as i16, cy.round() as i16);
    }
    fn stroke_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Color) {
        // Thin lines: 2 anti-aliased triangles — cheap AND smooth (no staircase).
        self.draw_tris(&line_tris(x0, y0, x1, y1, width), c);
    }
    fn end_frame(&mut self) {}

    fn begin_instrument_layer(&mut self) {
        // BAKE instruments straight into the content buffer (`back_pic`) with OVER,
        // instead of a separate ARGB layer composited over the window every
        // present. That per-present RENDER `Composite` is software in Xwayland and
        // pegged it (0%→73% CPU) over `ssh -X`; drawing into `back_pic` means the
        // instruments ride the normal content `CopyArea` — no composite at all.
        // fill_circle/stroke_line don't self-trim, so clip `back_pic` to the frame
        // scissor here (fill() already trims; this covers the mask/triangle prims)
        // so a scissored frame only touches its bbox and the wire stays small.
        self.drawing_instruments = true; // fill() switches to OVER + premultiplied
        if let Some(b) = self.clip {
            let r = xproto::Rectangle {
                x: b.x as i16,
                y: b.y as i16,
                width: b.w.max(0) as u16,
                height: b.h.max(0) as u16,
            };
            let _ = render::set_picture_clip_rectangles(&self.conn, self.back_pic, 0, 0, &[r]);
        }
    }
    fn end_instrument_layer(&mut self) {
        if self.xdiag {
            self.xd_geom += 1;
        }
        self.drawing_instruments = false;
        // Restore back_pic to a full-drawable clip so the next frame's content
        // draws aren't scissored to this frame's instrument bbox.
        let full = xproto::Rectangle { x: 0, y: 0, width: self.win_w, height: self.win_h };
        let _ = render::set_picture_clip_rectangles(&self.conn, self.back_pic, 0, 0, &[full]);
    }

    fn resize_surface(&mut self, w: std::num::NonZeroU32, h: std::num::NonZeroU32) {
        self.win_w = w.get() as u16;
        self.win_h = h.get() as u16;
        self.recreate_back(); // back buffer must match the new window size
    }
    fn present(&mut self, _window: &Window, damage: Option<(PxRect, &[PxRect])>) -> bool {
        // Instruments are BAKED into the back buffer (see begin_instrument_layer),
        // so present is just a CopyArea — NO RENDER Composite (that per-present
        // software composite was what pegged Xwayland over ssh -X). Full frames
        // (chrome moved) copy the whole window; scissored frames copy only the
        // damage bbox — the back buffer is a valid full-content buffer, so any
        // sub-region is correct, and it keeps both the wire and cosmic-comp's
        // recomposite small.
        match damage {
            None => {
                let _ = self.conn.copy_area(self.back_pixmap, self.window, self.gc, 0, 0, 0, 0, self.win_w, self.win_h);
                if self.xdiag {
                    self.xd_dmg += self.win_w as u64 * self.win_h as u64;
                }
            }
            Some((b, _)) => {
                let x = b.x.clamp(0, self.win_w as i32) as i16;
                let y = b.y.clamp(0, self.win_h as i32) as i16;
                let w = (b.w.min(self.win_w as i32 - x as i32)).max(0) as u16;
                let h = (b.h.min(self.win_h as i32 - y as i32)).max(0) as u16;
                if w > 0 && h > 0 {
                    let _ = self.conn.copy_area(self.back_pixmap, self.window, self.gc, x, y, x, y, w, h);
                    if self.xdiag {
                        self.xd_dmg += w as u64 * h as u64;
                    }
                }
            }
        }
        let ft = std::time::Instant::now();
        let _ = self.conn.flush();
        if self.xdiag {
            // flush() blocks when the X connection's send buffer is full (server
            // not draining fast enough) — the ssh -X saturation signal.
            self.xd_flush += ft.elapsed();
            self.xd_present += 1;
            if self.xd_last.elapsed() >= std::time::Duration::from_secs(1) {
                eprintln!(
                    "xdiag: {} present/s, {:.1} Mpx/s damage, flush {} ms/s, {} instr-redraw/s",
                    self.xd_present, self.xd_dmg as f64 / 1e6, self.xd_flush.as_millis(), self.xd_geom
                );
                self.xd_last = std::time::Instant::now();
                self.xd_present = 0;
                self.xd_geom = 0;
                self.xd_flush = std::time::Duration::ZERO;
                self.xd_dmg = 0;
            }
        }
        false // never needs the GL fallback
    }
    fn full_swap(&mut self) {
        // Copy the whole back buffer to the window.
        let _ = self.conn.copy_area(self.back_pixmap, self.window, self.gc, 0, 0, 0, 0, self.win_w, self.win_h);
        let _ = self.conn.flush();
    }
    fn is_software(&self) -> bool {
        true
    }
    fn buffer_age(&self) -> u32 {
        1 // the X window preserves undamaged pixels server-side
    }
    fn partial_present_available(&self) -> bool {
        true // XRender draws only the damaged cells directly into the window
    }
    fn x11_present_active(&self) -> bool {
        // Semantically "an X present that preserves the window server-side": the
        // planner uses this to force age=1 AND to skip the border-band damage
        // inflation (a GL-buffer artifact), so a keystroke's damage stays the
        // changed cells — exactly what the clip filter in fill/draw_char honours.
        true
    }
    fn is_gl(&self) -> bool {
        false // the XRender backend: instruments live on the persistent layer
    }
}

impl Drop for XRenderBackend {
    fn drop(&mut self) {
        let _ = render::free_picture(&self.conn, self.win_pic);
        let _ = render::free_picture(&self.conn, self.back_pic);
        let _ = render::free_picture(&self.conn, self.src_pic);
        let _ = render::free_picture(&self.conn, self.src_pic_argb);
        let _ = xproto::free_pixmap(&self.conn, self.src_pixmap_argb);
        let _ = render::free_glyph_set(&self.conn, self.glyphset);
        let _ = render::free_glyph_set(&self.conn, self.shape_glyphset);
        let _ = xproto::free_pixmap(&self.conn, self.back_pixmap);
        let _ = xproto::free_pixmap(&self.conn, self.src_pixmap);
        let _ = xproto::free_gc(&self.conn, self.gc);
        let _ = self.conn.flush();
    }
}

#[cfg(test)]
mod trim_fractional_tests {
    use super::*;

    // The exact numbers RT_DEBUG_FILL captured on the milkv for the 1px-gap bug.
    // clip = PxRect{x:189, y:38, w:714, h:21} => right edge = 189+714 = 903.
    fn milkv_clip() -> PxRect {
        PxRect { x: 189, y: 38, w: 714, h: 21 }
    }

    #[test]
    fn fractional_cell_keeps_full_width_under_an_integer_clip() {
        // A cell at x=892.5 spanning to 903.5, clip cuts the far edge at 903.
        // Before the fix this came out width=10 (dropped px 902 -> the black gap).
        let r = trim_to_clip(892.5, 38.0, 11.0, 21.0, Some(milkv_clip())).expect("inside clip");
        assert_eq!(r.x, 892);
        assert_eq!(r.width, 11, "must not lose the rightmost pixel column");
    }

    #[test]
    fn fractional_cursor_right_edge_is_still_drawn() {
        // The hollow cursor's 1px right edge at x=902.5, clip.right=903.
        // Before the fix width=0 -> the cursor's right side vanished.
        let r = trim_to_clip(902.5, 38.0, 1.0, 21.0, Some(milkv_clip())).expect("inside clip");
        assert_eq!(r.width, 1, "the 1px edge must survive trimming, not collapse to 0");
    }

    #[test]
    fn adjacent_fractional_cells_tile_with_no_gap() {
        // Two neighbouring cells (cell_w=11) at fractional origins, both fully
        // inside a wide clip. Their integer rects must abut (no gap, no overlap
        // beyond 1px) so no black slit appears between them.
        let clip = Some(PxRect { x: 0, y: 0, w: 2000, h: 100 });
        let a = trim_to_clip(892.5, 0.0, 11.0, 21.0, clip).unwrap();
        let b = trim_to_clip(903.5, 0.0, 11.0, 21.0, clip).unwrap();
        let a_right = a.x as i32 + a.width as i32;
        assert!(a_right >= b.x as i32, "gap between adjacent cells: a ends {a_right}, b starts {}", b.x);
        assert!(a_right <= b.x as i32 + 1, "excessive overlap of adjacent cells");
    }

    #[test]
    fn full_frame_fills_are_unchanged_and_tile() {
        // clip=None: consistently-fractional origins truncate consistently, so
        // adjacent cells already tile. The fix must not disturb this.
        let a = trim_to_clip(892.5, 0.0, 11.0, 21.0, None).unwrap();
        let b = trim_to_clip(903.5, 0.0, 11.0, 21.0, None).unwrap();
        assert_eq!((a.x, a.width), (892, 11));
        assert_eq!(a.x as i32 + a.width as i32, b.x as i32, "full-frame cells abut exactly");
    }

    #[test]
    fn fully_outside_or_degenerate_yields_none() {
        let clip = Some(PxRect { x: 100, y: 100, w: 50, h: 50 });
        assert!(trim_to_clip(10.0, 10.0, 5.0, 5.0, clip).is_none(), "outside the clip");
        assert!(trim_to_clip(50.0, 50.0, 0.0, 0.0, None).is_none(), "zero area");
    }
}

#[cfg(test)]
mod fill_clip_tests {
    use super::*;

    /// `fill` trims to the clip; this is the pure geometry it trims with.
    /// Kept honest against the real thing by mirroring its arithmetic exactly.
    fn trim(x: f32, y: f32, w: f32, h: f32, b: PxRect) -> Option<(f32, f32, f32, f32)> {
        if !rect_intersects(x, y, w, h, b) {
            return None;
        }
        let (x0, y0) = (x.max(b.x as f32), y.max(b.y as f32));
        let (x1, y1) = ((x + w).min(b.right() as f32), (y + h).min(b.bottom() as f32));
        if x1 <= x0 || y1 <= y0 {
            return None;
        }
        Some((x0, y0, x1 - x0, y1 - y0))
    }

    /// THE bug: a titlebar-wide background fill, on a frame scissored to the far
    /// RIGHT of that titlebar (where the `buf` meter changes), must not repaint
    /// the left end — the title's glyphs live there, `draw_char` rejects them as
    /// outside the clip, and nothing else would ever restore them.
    #[test]
    fn fill_does_not_paint_outside_the_clip() {
        // titlebar background: full pane width, 25px tall, at y=303
        let (x, y, w, h) = (8.0, 303.0, 944.0, 25.0);
        // frame scissored to the meter at the right end only
        let clip = PxRect { x: 700, y: 303, w: 244, h: 25 };
        let (tx, ty, tw, th) = trim(x, y, w, h, clip).expect("touches the clip");
        assert_eq!(tx, 700.0, "fill must start at the clip, not at the rect's own x");
        assert!(tx + tw <= 944.0 + 8.0);
        assert_eq!((ty, th), (303.0, 25.0), "vertically inside the clip already");
        // The title's glyph cells (x=14..270) must be untouched by this fill.
        assert!(tx >= 270.0, "the fill reached the title glyphs and would erase them");
    }

    #[test]
    fn fill_is_unchanged_when_wholly_inside_the_clip() {
        let clip = PxRect { x: 0, y: 0, w: 900, h: 700 };
        assert_eq!(trim(10.0, 20.0, 30.0, 40.0, clip), Some((10.0, 20.0, 30.0, 40.0)));
    }

    #[test]
    fn fill_outside_the_clip_is_dropped() {
        let clip = PxRect { x: 700, y: 303, w: 244, h: 25 };
        assert_eq!(trim(8.0, 10.0, 100.0, 20.0, clip), None, "wholly above the clip");
        assert_eq!(trim(8.0, 303.0, 100.0, 25.0, clip), None, "wholly left of the clip");
    }
}


#[cfg(test)]
mod premult_tests {
    use super::*;
    use crate::render::Color;

    #[test]
    fn premultiply_scales_rgb_by_alpha_and_keeps_alpha() {
        // Half-alpha pure red → premultiplied red = 0.5, alpha = 0.5.
        let c = premultiply(Color(1.0, 0.0, 0.0, 0.5));
        assert_eq!(c.alpha, 32767); // 0.5 * 65535, truncated
        assert_eq!(c.red, 32767);   // 1.0 * 0.5 * 65535
        assert_eq!(c.green, 0);
        assert_eq!(c.blue, 0);
    }

    #[test]
    fn premultiply_opaque_is_unchanged_rgb() {
        let c = premultiply(Color(1.0, 0.5, 0.25, 1.0));
        assert_eq!(c.alpha, 65535);
        assert_eq!(c.red, 65535);
        assert_eq!(c.green, 32767);
        assert_eq!(c.blue, 16383);
    }

    #[test]
    fn premultiply_fully_transparent_is_zero() {
        let c = premultiply(Color(1.0, 1.0, 1.0, 0.0));
        assert_eq!((c.red, c.green, c.blue, c.alpha), (0, 0, 0, 0));
    }
}

#[cfg(test)]
mod bg_clear_tests {
    use super::*;
    use crate::render::Color;

    // NOTE: x11rb's `render::Color` only derives PartialEq/Debug under the
    // `extra-traits` feature (not enabled here), so assert on FIELDS, not the
    // whole struct — matching the existing `premult_tests` in this file.

    #[test]
    fn depth_32_premultiplies_the_translucent_background() {
        // 35% opaque pure-red bg on a 32-bit ARGB window: the clear must be
        // PREMULTIPLIED (red scaled by alpha) so the compositor blends it
        // correctly, and must carry the alpha.
        let c = bg_clear_color(32, Color(1.0, 0.0, 0.0, 0.35));
        let want = premultiply(Color(1.0, 0.0, 0.0, 0.35));
        assert_eq!((c.red, c.green, c.blue, c.alpha), (want.red, want.green, want.blue, want.alpha));
        assert_eq!(c.alpha, (0.35 * 65535.0) as u16, "alpha must be carried");
        assert_eq!(c.red, (1.0 * 0.35 * 65535.0) as u16, "red must be premultiplied");
    }

    #[test]
    fn depth_24_is_opaque_straight_colour_not_premultiplied() {
        // Same bg on a 24-bit window: the drawable has no alpha channel, so the
        // clear must NOT premultiply (that would darken the RGB to a wrong dark
        // opaque background). It uses the straight colour, exactly as before.
        let c = bg_clear_color(24, Color(1.0, 0.0, 0.0, 0.35));
        let want = to_render_color(Color(1.0, 0.0, 0.0, 0.35));
        assert_eq!((c.red, c.green, c.blue, c.alpha), (want.red, want.green, want.blue, want.alpha));
        assert_eq!(c.red, 65535, "red must stay full, not scaled by alpha");
    }

    #[test]
    fn opaque_background_is_identical_on_both_depths() {
        // The default background_opacity is 1.0. premultiply at alpha 1.0 equals
        // the straight colour, so a 32-bit and 24-bit clear are identical then —
        // which is why the opacity-less Xvfb gates are unaffected.
        let opaque = Color(0.1, 0.1, 0.12, 1.0);
        let a = bg_clear_color(32, opaque);
        let b = bg_clear_color(24, opaque);
        assert_eq!((a.red, a.green, a.blue, a.alpha), (b.red, b.green, b.blue, b.alpha));
    }
}

#[cfg(test)]
mod geom_tests {
    use super::*;

    #[test]
    fn disc_mask_is_opaque_centre_clear_corner() {
        let (w, h, d) = rasterize_disc(9.0);
        let rad = 9usize; // ceil(9)
        assert!(w as usize >= 2 * rad + 1 && h == w, "square mask around the disc");
        let centre = d[rad * w as usize + rad];
        assert!(centre > 250, "disc centre should be ~opaque, got {centre}");
        assert_eq!(d[0], 0, "top-left corner is outside the disc");
        assert!(d.iter().any(|&v| v > 0 && v < 255), "expected AA edge coverage");
    }

    #[test]
    fn ring_mask_is_hollow_and_filled_on_the_band() {
        let (w, _h, d) = rasterize_ring(6.0, 1.4);
        let rad = 6usize;
        assert_eq!(d[rad * w as usize + rad], 0, "ring centre must be hollow");
        let band = d[rad * w as usize + (rad + 6).min(w as usize - 1)];
        assert!(band > 100, "ring band should be filled, got {band}");
    }

    #[test]
    fn line_is_two_triangles_of_correct_width() {
        // A horizontal segment, width 2 → a 10x2 quad centred on y=20.
        let tris = line_tris(10.0, 20.0, 20.0, 20.0, 2.0);
        assert_eq!(tris.len(), 2, "a quad is two triangles");
        let (_, y, _, h) = tris_bbox(&tris);
        assert!((y - 19.0).abs() < 0.01 && (h - 2.0).abs() < 0.01, "half-width each side");
    }
}
