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
use x11rb::protocol::render::{self, ConnectionExt as _, PictType, Pictformat};
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use x11rb::rust_connection::RustConnection;

use crate::backend::Backend;
use crate::damage::PxRect;
use crate::render::{Color, FontBlobs};

/// Convert rt's 0..1 float colour to XRender's 16-bit-per-channel colour.
fn to_render_color(c: Color) -> render::Color {
    let s = |v: f32| (v.clamp(0.0, 1.0) * 65535.0) as u16;
    render::Color { red: s(c.0), green: s(c.1), blue: s(c.2), alpha: s(c.3) }
}

pub struct XRenderBackend {
    conn: RustConnection,
    window: u32,
    win_pic: render::Picture,   // the on-screen window, as an XRender Picture
    a8_format: Pictformat,      // the A8 glyph mask format
    glyphset: render::Glyphset, // one shared glyph set (all styles)
    src_pixmap: xproto::Pixmap, // 1x1 repeating solid-colour source
    src_pic: render::Picture,   // the source Picture over `src_pixmap`
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

        let glyphset = conn.generate_id().ok()?;
        render::create_glyph_set(&conn, glyphset, a8_format).ok()?;

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
            a8_format,
            glyphset,
            src_pixmap,
            src_pic,
            cell_w,
            cell_h,
            ascent,
            fonts,
            glyph_px: font_px,
            glyphs: HashMap::new(),
            next_glyph_id: 1,
            clip: None,
            win_w: geo.width,
            win_h: geo.height,
        })
    }

    fn fill(&self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        // Respect the damage clip: skip fills that don't touch it.
        if let Some(b) = self.clip {
            if !rect_intersects(x, y, w, h, b) {
                return;
            }
        }
        let rect = xproto::Rectangle { x: x as i16, y: y as i16, width: w.max(0.0) as u16, height: h.max(0.0) as u16 };
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.win_pic, to_render_color(c), &[rect]);
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
        // stale glyph ids: recreate the glyph set (Task 7 does this properly)
        self.glyphs.clear();
        Ok(())
    }

    fn begin_frame(&mut self, bg: Color) {
        self.clip = None;
        let rect = xproto::Rectangle { x: 0, y: 0, width: self.win_w, height: self.win_h };
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.win_pic, to_render_color(bg), &[rect]);
    }
    fn begin_frame_scissored(&mut self, bg: Color, bbox: PxRect) {
        self.clip = Some(bbox);
        let rect = xproto::Rectangle { x: bbox.x as i16, y: bbox.y as i16, width: bbox.w as u16, height: bbox.h as u16 };
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.win_pic, to_render_color(bg), &[rect]);
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
    fn draw_char(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _ch: char, _fg: Color, _bold: bool, _italic: bool) {
        // Glyphs: Task 5.
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
    fn end_frame(&mut self) {}

    fn resize_surface(&mut self, w: std::num::NonZeroU32, h: std::num::NonZeroU32) {
        self.win_w = w.get() as u16;
        self.win_h = h.get() as u16;
    }
    fn present(&mut self, _window: &Window, _damage: Option<(PxRect, &[PxRect])>) -> bool {
        let _ = self.conn.flush();
        false // XRender drew the damage region directly; never needs the GL fallback
    }
    fn full_swap(&mut self) {
        let _ = self.conn.flush();
    }
    fn is_software(&self) -> bool {
        true
    }
    fn buffer_age(&self) -> u32 {
        1 // the X window preserves undamaged pixels server-side
    }
    fn partial_present_available(&self) -> bool {
        false // Slice 1 Task 3-5 run the full path; Task 6 flips this on
    }
    fn x11_present_active(&self) -> bool {
        false
    }
}

impl Drop for XRenderBackend {
    fn drop(&mut self) {
        let _ = render::free_picture(&self.conn, self.win_pic);
        let _ = render::free_picture(&self.conn, self.src_pic);
        let _ = render::free_glyph_set(&self.conn, self.glyphset);
        let _ = xproto::free_pixmap(&self.conn, self.src_pixmap);
        let _ = self.conn.flush();
    }
}
