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

/// Rasterise a filled disc of radius `r` into an A8 coverage bitmap, centred in a
/// `side`×`side` square (`side = 2*ceil(r)+1`). Edge coverage ramps over ~1px
/// (analytic AA). Returns `(side, side, coverage)`. Rasterised ONCE, then cached.
fn rasterize_disc(r: f32) -> (u16, u16, Vec<u8>) {
    let rad = r.ceil().max(1.0) as i32;
    let side = (rad * 2 + 1) as usize;
    let mut data = vec![0u8; side * side];
    for py in 0..side {
        for px in 0..side {
            let dx = px as f32 - rad as f32;
            let dy = py as f32 - rad as f32;
            let d = (dx * dx + dy * dy).sqrt();
            let cov = (r + 0.5 - d).clamp(0.0, 1.0);
            data[py * side + px] = (cov * 255.0) as u8;
        }
    }
    (side as u16, side as u16, data)
}

/// Rasterise a ring (outer radius `r`, stroke `width` inward) into an A8 mask:
/// coverage = inside the outer edge AND outside the inner edge, both AA.
fn rasterize_ring(r: f32, width: f32) -> (u16, u16, Vec<u8>) {
    let rad = r.ceil().max(1.0) as i32;
    let side = (rad * 2 + 1) as usize;
    let ri = (r - width).max(0.0);
    let mut data = vec![0u8; side * side];
    for py in 0..side {
        for px in 0..side {
            let dx = px as f32 - rad as f32;
            let dy = py as f32 - rad as f32;
            let d = (dx * dx + dy * dy).sqrt();
            let outer = (r + 0.5 - d).clamp(0.0, 1.0); // inside outer edge
            let inner = (d - ri + 0.5).clamp(0.0, 1.0); // outside inner edge
            data[py * side + px] = (outer.min(inner) * 255.0) as u8;
        }
    }
    (side as u16, side as u16, data)
}

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

pub struct XRenderBackend {
    conn: RustConnection,
    window: u32,                // X window id (CopyArea destination)
    win_pic: render::Picture,   // the on-screen window, as an XRender Picture
    // Server-side back buffer: all drawing targets `back_pic`; `present` copies the
    // damaged region `back_pixmap`->window with a single server-side `CopyArea`, so
    // a full repaint never blanks the window (no flash) and still ships zero pixels.
    back_pixmap: xproto::Pixmap,
    back_pic: render::Picture,
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
        // Respect the damage clip: skip fills that don't touch it.
        if let Some(b) = self.clip {
            if !rect_intersects(x, y, w, h, b) {
                return;
            }
        }
        let rect = xproto::Rectangle { x: x as i16, y: y as i16, width: w.max(0.0) as u16, height: h.max(0.0) as u16 };
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.back_pic, to_render_color(c), &[rect]);
    }

    /// Set the 1x1 repeating ARGB source to `c` (straight alpha), premultiplied as
    /// OVER expects, ready to be modulated by a shape mask or a triangle mesh.
    fn set_argb_src(&self, c: Color) {
        let s = |v: f32| (v.clamp(0.0, 1.0) * 65535.0) as u16;
        let premult = render::Color { red: s(c.0 * c.3), green: s(c.1 * c.3), blue: s(c.2 * c.3), alpha: s(c.3) };
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
            &self.conn, render::PictOp::OVER, self.src_pic_argb, self.back_pic,
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
            &self.conn, render::PictOp::OVER, self.src_pic_argb, self.back_pic,
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
    }
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
        self.clip = None;
        // Clear the whole BACK buffer (off-screen) — the window is untouched until
        // `present` copies the finished frame, so a full repaint never flashes.
        let rect = xproto::Rectangle { x: 0, y: 0, width: self.win_w, height: self.win_h };
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.back_pic, to_render_color(bg), &[rect]);
    }
    fn begin_frame_scissored(&mut self, bg: Color, bbox: PxRect) {
        self.clip = Some(bbox);
        let rect = xproto::Rectangle { x: bbox.x as i16, y: bbox.y as i16, width: bbox.w as u16, height: bbox.h as u16 };
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.back_pic, to_render_color(bg), &[rect]);
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

    fn resize_surface(&mut self, w: std::num::NonZeroU32, h: std::num::NonZeroU32) {
        self.win_w = w.get() as u16;
        self.win_h = h.get() as u16;
        self.recreate_back(); // back buffer must match the new window size
    }
    fn present(&mut self, _window: &Window, _damage: Option<(PxRect, &[PxRect])>) -> bool {
        // ALWAYS copy the whole back buffer to the window, ignoring the damage
        // bbox. A CopyArea is server-side (zero wire pixels), so a full-window
        // copy costs the same as a partial one — but it guarantees the complete,
        // consistent back buffer is what's on screen. Presenting only the damage
        // bbox was the source of the "half-drawn / grows-as-you-type" borders:
        // `fill()` draws a whole rect (focus border, divider) into the back buffer
        // whenever it merely intersects the bbox, but a bbox-only present showed
        // just the sliver inside the bbox, revealing the rest only as later
        // frames' bboxes swept over it. The DRAW stays minimal (scissored to the
        // changed cells — that is what keeps the *wire* small); only the present
        // is full. Still zero PutImage, so mechanism C's invariant holds.
        let _ = self.conn.copy_area(self.back_pixmap, self.window, self.gc, 0, 0, 0, 0, self.win_w, self.win_h);
        let _ = self.conn.flush();
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
    fn supports_egui(&self) -> bool {
        false // no GL context → egui_glow chrome cannot render (Slice 1 degrade)
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
