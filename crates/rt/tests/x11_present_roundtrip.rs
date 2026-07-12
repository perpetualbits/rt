//! X11 put/get round-trip: put a known image to an off-screen pixmap and read it
//! back, asserting byte-identity for the depth-24 ZPixmap path Route 1 uses.
//! Needs an X server; run under Xvfb:
//!   Xvfb :99 & DISPLAY=:99 cargo test -p rt --test x11_present_roundtrip -- --ignored

#![cfg(feature = "x11")]

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{ConnectionExt, CreateGCAux, ImageFormat};

#[test]
#[ignore = "needs an X server; run under Xvfb with --ignored"]
fn put_get_roundtrip_depth24() {
    let (conn, screen_num) = x11rb::connect(None).expect("connect to $DISPLAY");
    let screen = &conn.setup().roots[screen_num];
    let depth = screen.root_depth;
    assert!(depth == 24 || depth == 32, "test expects a 24/32-bit visual, got {depth}");

    let (w, h) = (16u16, 8u16);
    // A distinct BGRA pattern per pixel so a wrong row/byte order would show.
    let mut data = vec![0u8; (w as usize) * (h as usize) * 4];
    for (i, px) in data.chunks_mut(4).enumerate() {
        px[0] = (i & 0xff) as u8; // B
        px[1] = ((i >> 1) & 0xff) as u8; // G
        px[2] = ((i >> 2) & 0xff) as u8; // R
        px[3] = 0; // X (pad)
    }

    // Off-screen pixmap of the screen depth as the drawable (no window needed).
    let pixmap = conn.generate_id().unwrap();
    conn.create_pixmap(depth, pixmap, screen.root, w, h).unwrap();
    let gc = conn.generate_id().unwrap();
    conn.create_gc(gc, pixmap, &CreateGCAux::new()).unwrap();

    conn.put_image(ImageFormat::Z_PIXMAP, pixmap, gc, w, h, 0, 0, 0, depth, &data)
        .unwrap()
        .check()
        .expect("put_image");

    let got = conn
        .get_image(ImageFormat::Z_PIXMAP, pixmap, 0, 0, w, h, !0)
        .unwrap()
        .reply()
        .expect("get_image");

    // Compare the RGB bytes (the pad byte may be forced to 0xff by the server on
    // a 24-bit visual, so ignore byte 3 of each pixel).
    assert_eq!(got.data.len(), data.len());
    for (a, b) in got.data.chunks(4).zip(data.chunks(4)) {
        assert_eq!(&a[0..3], &b[0..3], "pixel RGB round-trips");
    }

    conn.free_gc(gc).ok();
    conn.free_pixmap(pixmap).ok();
}
