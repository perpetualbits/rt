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
type TriF = [(f32, f32); 3];

/// Rim subdivisions for a disc/ring of radius `r`: enough that the polygon reads
/// as a circle at terminal sizes, capped so a big ring stays cheap.
fn seg_count(r: f32) -> u32 {
    ((r * 3.0) as u32).clamp(8, 64)
}

/// Filled disc as a triangle fan (apex = centre, rim = `seg_count(r)` points).
fn disc_tris(cx: f32, cy: f32, r: f32) -> Vec<TriF> {
    use std::f32::consts::TAU;
    let n = seg_count(r);
    let pt = |k: u32| {
        let a = TAU * k as f32 / n as f32;
        (cx + r * a.cos(), cy + r * a.sin())
    };
    (0..n).map(|k| [(cx, cy), pt(k), pt((k + 1) % n)]).collect()
}

/// Annulus (outer `r`, inner `r-width`) as a triangle strip of quads → 2 tris each.
fn ring_tris(cx: f32, cy: f32, r: f32, width: f32) -> Vec<TriF> {
    use std::f32::consts::TAU;
    let ri = (r - width).max(0.0);
    let n = seg_count(r);
    let outer = |k: u32| { let a = TAU * k as f32 / n as f32; (cx + r * a.cos(), cy + r * a.sin()) };
    let inner = |k: u32| { let a = TAU * k as f32 / n as f32; (cx + ri * a.cos(), cy + ri * a.sin()) };
    let mut out = Vec::with_capacity(n as usize * 2);
    for k in 0..n {
        let (o0, o1) = (outer(k), outer((k + 1) % n));
        let (i0, i1) = (inner(k), inner((k + 1) % n));
        out.push([o0, o1, i1]);
        out.push([o0, i1, i0]);
    }
    out
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
    argb_format: Pictformat,        // 32-bit ARGB format for the alpha source
    src_pixmap_argb: xproto::Pixmap,// 1x1 repeating alpha-capable source pixmap
    src_pic_argb: render::Picture,  // the ARGB source Picture (AA primitives)
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
            argb_format,
            src_pixmap_argb,
            src_pic_argb,
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

    /// Composite a triangle mesh in colour `c` (straight alpha) onto the back
    /// buffer with anti-aliasing: fill the 1x1 ARGB source with the *premultiplied*
    /// colour, then `render::triangles` OVER through the A8 mask format so the
    /// per-edge coverage is antialiased. Server-side geometry — zero wire pixels.
    fn draw_tris(&self, tris: &[TriF], c: Color) {
        if tris.is_empty() { return; }
        // Clip rejection: skip meshes wholly outside the damage clip.
        if let Some(b) = self.clip {
            let (x, y, w, h) = tris_bbox(tris);
            if !rect_intersects(x, y, w, h, b) { return; }
        }
        // Premultiplied ARGB solid source (OVER expects premultiplied alpha).
        let s = |v: f32| (v.clamp(0.0, 1.0) * 65535.0) as u16;
        let premult = render::Color { red: s(c.0 * c.3), green: s(c.1 * c.3), blue: s(c.2 * c.3), alpha: s(c.3) };
        let one = xproto::Rectangle { x: 0, y: 0, width: 1, height: 1 };
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.src_pic_argb, premult, &[one]);
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
        self.draw_tris(&disc_tris(cx, cy, r), c);
    }
    fn stroke_circle(&mut self, cx: f32, cy: f32, r: f32, width: f32, c: Color) {
        self.draw_tris(&ring_tris(cx, cy, r, width), c);
    }
    fn stroke_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Color) {
        self.draw_tris(&line_tris(x0, y0, x1, y1, width), c);
    }
    fn end_frame(&mut self) {}

    fn resize_surface(&mut self, w: std::num::NonZeroU32, h: std::num::NonZeroU32) {
        self.win_w = w.get() as u16;
        self.win_h = h.get() as u16;
        self.recreate_back(); // back buffer must match the new window size
    }
    fn present(&mut self, _window: &Window, damage: Option<(PxRect, &[PxRect])>) -> bool {
        // Copy the finished frame from the back buffer to the window in one
        // server-side CopyArea (no wire pixels): the damage bbox if partial, else
        // the whole window. This is what makes the update atomic — no flash.
        let (sx, sy, w, h) = match damage {
            Some((b, _)) => (b.x as i16, b.y as i16, b.w as u16, b.h as u16),
            None => (0, 0, self.win_w, self.win_h),
        };
        if w > 0 && h > 0 {
            let _ = self.conn.copy_area(self.back_pixmap, self.window, self.gc, sx, sy, sx, sy, w, h);
        }
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
    fn disc_is_a_fan_on_radius() {
        let n = seg_count(9.0);
        let tris = disc_tris(50.0, 40.0, 9.0);
        assert_eq!(tris.len() as u32, n, "one triangle per rim segment");
        // Every rim vertex is ~9px from the centre.
        for t in &tris {
            for &(x, y) in &[t[1], t[2]] {
                let d = ((x - 50.0).powi(2) + (y - 40.0).powi(2)).sqrt();
                assert!((d - 9.0).abs() < 0.01, "rim vertex off-circle: {d}");
            }
            assert_eq!(t[0], (50.0, 40.0), "fan apex is the centre");
        }
    }

    #[test]
    fn ring_has_inner_and_outer_radius() {
        let tris = ring_tris(30.0, 30.0, 4.0, 1.4);
        assert!(!tris.is_empty());
        let mut saw_outer = false;
        let mut saw_inner = false;
        for t in &tris {
            for &(x, y) in t {
                let d = ((x - 30.0).powi(2) + (y - 30.0).powi(2)).sqrt();
                if (d - 4.0).abs() < 0.02 { saw_outer = true; }
                if (d - 2.6).abs() < 0.02 { saw_inner = true; } // 4.0 - 1.4
            }
        }
        assert!(saw_outer && saw_inner, "ring must touch both radii");
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
