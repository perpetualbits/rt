#![cfg(feature = "x11")]

//! Route 1 present path: read the damage rectangle back from the GL back buffer
//! and push it to the X11 window with `XPutImage`, so only the changed pixels
//! cross the wire (fast over X11-over-ssh). No buffer preservation needed — the
//! X window keeps its other pixels server-side. X11/GLX only; `try_new` returns
//! `None` on Wayland or an unsupported visual depth.

use glow::HasContext;
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;
use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, CreateGCAux, Gcontext, ImageFormat};
use x11rb::rust_connection::RustConnection;

/// Reverse the row order of a tightly-packed `w*h*4` buffer. `glReadPixels` is
/// bottom-up; `XPutImage` `ZPixmap` is top-down.
pub fn flip_rows(buf: &[u8], w: usize, h: usize) -> Vec<u8> {
    let stride = w * 4;
    let mut out = vec![0u8; buf.len()];
    for row in 0..h {
        let src = (h - 1 - row) * stride;
        let dst = row * stride;
        out[dst..dst + stride].copy_from_slice(&buf[src..src + stride]);
    }
    out
}

/// An X11 present handle: the connection, the window, a GC, and the window depth.
pub struct X11Present {
    conn: RustConnection,
    window: u32,
    gc: Gcontext,
    depth: u8,
}

impl X11Present {
    /// Build from rt's window. `None` on Wayland, an unsupported depth (not 24/32),
    /// or if X setup fails — the caller then keeps the normal `swap_buffers` path.
    pub fn try_new(window: &Window) -> Option<Self> {
        let win = match window.window_handle().ok()?.as_raw() {
            RawWindowHandle::Xlib(h) => h.window as u32,
            RawWindowHandle::Xcb(h) => h.window.get(),
            _ => return None, // Wayland: no X present path
        };
        let (conn, screen_num) = x11rb::connect(None).ok()?; // honours $DISPLAY
        let depth = conn.setup().roots[screen_num].root_depth;
        if depth != 24 && depth != 32 {
            log::info!("x11_present: depth {depth} unsupported; using swap_buffers");
            return None; // unfamiliar visual → fall back
        }
        let gc = conn.generate_id().ok()?;
        conn.create_gc(gc, win, &CreateGCAux::new()).ok()?;
        conn.flush().ok()?;
        log::info!("x11_present: ready (window={win:#x} depth={depth})");
        Some(Self { conn, window: win, gc, depth })
    }

    /// Read back `(x,y,w,h)` (top-left origin) from `GL_BACK` as BGRA and
    /// `XPutImage` it to the window. `true` on success; `false` on any error so
    /// the caller can fall back to a full present.
    pub fn present_rect(&self, gl: &glow::Context, x: i32, y: i32, w: i32, h: i32, screen_h: i32) -> bool {
        if w <= 0 || h <= 0 {
            return false;
        }
        let mut buf = vec![0u8; (w * h * 4) as usize];
        let gl_y = screen_h - (y + h); // glReadPixels is bottom-left origin
        unsafe {
            gl.read_buffer(glow::BACK);
            gl.read_pixels(
                x, gl_y, w, h, glow::BGRA, glow::UNSIGNED_BYTE,
                glow::PixelPackData::Slice(Some(&mut buf)),
            );
            gl.finish(); // force the scissored render + readback to complete
        }
        let data = flip_rows(&buf, w as usize, h as usize);
        let put = match self.conn.put_image(
            ImageFormat::Z_PIXMAP, self.window, self.gc,
            w as u16, h as u16, x as i16, y as i16, 0, self.depth, &data,
        ) {
            Ok(cookie) => cookie,
            Err(e) => {
                log::warn!("x11_present put_image request failed ({e:?}); full present next frame");
                return false;
            }
        };
        match put.check() {
            Ok(()) => true,
            Err(e) => {
                log::warn!("x11_present put_image failed ({e:?}); full present next frame");
                false
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn flip_rows_reverses_row_order() {
        // 2 rows × 1 px × 4 bytes. Row 0 = [1,2,3,4], row 1 = [5,6,7,8].
        let buf = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let out = flip_rows(&buf, 1, 2);
        assert_eq!(out, vec![5, 6, 7, 8, 1, 2, 3, 4]); // rows swapped
    }

    #[test]
    fn flip_rows_single_row_unchanged() {
        let buf = [1u8, 2, 3, 4, 9, 9, 9, 9]; // 2px × 1 row
        assert_eq!(flip_rows(&buf, 2, 1), buf.to_vec());
    }
}
