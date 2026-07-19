//! CPU anti-aliased coverage-mask rasterisers for the instrument shapes: a
//! filled disc, a ring, and a bar (a thick line's cross-section). Each returns
//! an A8/R8 coverage bitmap `(w, h, data)` — the same masks the XRender backend
//! stamps as glyphs and the GL backend uploads into its coverage atlas, so both
//! paths draw byte-identical shapes. Pure and unit-testable; no backend, no X.

/// Filled disc of radius `r`. Origin (mask centre) is at `(r.ceil(), r.ceil())`.
/// Coverage AA at the edge: `clamp(r + 0.5 - dist, 0, 1)`.
pub fn rasterize_disc(r: f32) -> (u16, u16, Vec<u8>) {
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

/// Ring: outer radius `r`, stroke `width` inward. Coverage = inside the outer
/// edge AND outside the inner edge, both anti-aliased.
pub fn rasterize_ring(r: f32, width: f32) -> (u16, u16, Vec<u8>) {
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

/// A thick line's cross-section: a horizontal bar of thickness `width`, anti-
/// aliased on its two long edges and solid across its (short) length. The GL
/// backend maps the `height` (AA) axis across the line's width and stretches the
/// `width` (solid) axis along the segment on a rotated quad, so a single mask per
/// thickness draws a segment of any length. Butt caps (matching the XRender
/// `line_tris` path). The solid rows sit centred; total height carries a 1px AA
/// margin each side.
pub fn rasterize_bar(width: f32) -> (u16, u16, Vec<u8>) {
    let h = (width.ceil() as i32 + 2).max(3) as usize; // width axis + AA margin
    let w = 4usize; // length axis (solid; stretched by the quad)
    let mid = (h as f32 - 1.0) / 2.0;
    let half = width / 2.0;
    let mut data = vec![0u8; w * h];
    for py in 0..h {
        let d = (py as f32 - mid).abs();
        let cov = (half + 0.5 - d).clamp(0.0, 1.0);
        let v = (cov * 255.0) as u8;
        for px in 0..w {
            data[py * w + px] = v;
        }
    }
    (w as u16, h as u16, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn disc_is_opaque_at_centre_and_clear_outside() {
        let (w, h, d) = rasterize_disc(6.0);
        assert_eq!((w, h), (13, 13)); // 2*ceil(6)+1
        let c = 6 * w as usize + 6; // centre index
        assert_eq!(d[c], 255, "opaque core");
        assert_eq!(d[0], 0, "clear corner");
        // A soft edge exists somewhere (some partial-coverage texel).
        assert!(d.iter().any(|&v| v > 0 && v < 255), "anti-aliased edge");
    }

    #[test]
    fn ring_is_hollow() {
        let (w, h, d) = rasterize_ring(6.0, 1.6);
        let c = (h as usize / 2) * w as usize + w as usize / 2;
        assert_eq!(d[c], 0, "ring centre is hollow");
        // The band near the outer radius is (near-)opaque somewhere on the mid row.
        let midrow = (h as usize / 2) * w as usize;
        assert!(d[midrow..midrow + w as usize].iter().any(|&v| v > 200), "the stroke band is drawn");
    }

    #[test]
    fn bar_is_solid_across_its_length_and_aa_across_width() {
        let (w, h, d) = rasterize_bar(3.0);
        assert_eq!(w, 4);
        // Every column in a given row is identical (solid along the length).
        for py in 0..h as usize {
            let row = &d[py * w as usize..(py + 1) * w as usize];
            assert!(row.iter().all(|&v| v == row[0]), "row {py} is uniform along length");
        }
        // The centre row is opaque; the outer rows fade (AA).
        let mid = h as usize / 2;
        assert_eq!(d[mid * w as usize], 255, "solid core");
        assert!(d[0] < 255, "top edge fades");
    }
}
