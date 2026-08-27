//! The "held pane" card: a pure-RGBA image of the dragged/carried pane —
//! accent outline, opaque titlebar band, translucent body — used as a custom
//! cursor so the user visibly holds what they picked up. Pure pixel math:
//! no winit, no Backend, unit-tested by pixel.

pub const CARD_MAX_DIM: u16 = 128;
pub const CARD_MIN_ASPECT: f32 = 0.4;
pub const CARD_MAX_ASPECT: f32 = 2.5;
pub const CARD_HOTSPOT: (u16, u16) = (8, 8);

const OUTLINE: [u8; 4] = [0x4a, 0x7a, 0xc8, 0xff];
const BAND: [u8; 4] = [0x2e, 0x2e, 0x38, 230];
const BODY_ALPHA: u8 = 140;

pub fn build_card_rgba(aspect: f32, bg: [u8; 3]) -> (Vec<u8>, u16, u16) {
    let aspect = if aspect.is_finite() && aspect > 0.0 {
        aspect.clamp(CARD_MIN_ASPECT, CARD_MAX_ASPECT)
    } else {
        CARD_MIN_ASPECT // 0/NaN input: pick the clamp floor, never divide by it
    };
    let (w, h) = if aspect >= 1.0 {
        (CARD_MAX_DIM, ((CARD_MAX_DIM as f32 / aspect) as u16).max(24))
    } else {
        (((CARD_MAX_DIM as f32 * aspect) as u16).max(24), CARD_MAX_DIM)
    };
    let band_h = ((h as usize * 14) / 100).max(8) as u16;
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            let outline = x < 2 || y < 2 || x >= w - 2 || y >= h - 2;
            let p: [u8; 4] = if outline {
                OUTLINE
            } else if y < 2 + band_h {
                BAND
            } else {
                [bg[0], bg[1], bg[2], BODY_ALPHA]
            };
            rgba.extend_from_slice(&p);
        }
    }
    (rgba, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn px(rgba: &[u8], w: u16, x: u16, y: u16) -> [u8; 4] {
        let i = (y as usize * w as usize + x as usize) * 4;
        [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
    }

    /// A wide pane (2:1) gets a 128x64 card; outline, band and body pixels land
    /// where the layout says, with the exact colours and alphas.
    #[test]
    fn card_layout_wide_pane() {
        let (rgba, w, h) = build_card_rgba(2.0, [0x10, 0x10, 0x14]);
        assert_eq!((w, h), (128, 64));
        assert_eq!(rgba.len(), w as usize * h as usize * 4);
        assert_eq!(px(&rgba, w, 0, 0), [0x4a, 0x7a, 0xc8, 0xff], "corner = outline");
        assert_eq!(px(&rgba, w, 64, 1), [0x4a, 0x7a, 0xc8, 0xff], "2px top edge");
        // band_h = max(8, 64*14/100 = 8) = 8 → rows 2..10 are band.
        assert_eq!(px(&rgba, w, 64, 5), [0x2e, 0x2e, 0x38, 230], "titlebar band");
        assert_eq!(px(&rgba, w, 64, 32), [0x10, 0x10, 0x14, 140], "translucent body");
        assert_eq!(px(&rgba, w, 127, 63), [0x4a, 0x7a, 0xc8, 0xff], "far corner = outline");
    }

    /// A tall pane makes a tall card; extreme aspects clamp.
    #[test]
    fn card_layout_tall_and_clamped() {
        let (_, w, h) = build_card_rgba(0.5, [0, 0, 0]);
        assert_eq!((w, h), (64, 128));
        let (_, w2, h2) = build_card_rgba(100.0, [0, 0, 0]);
        assert_eq!((w2, h2), (128, (128.0 / CARD_MAX_ASPECT) as u16), "aspect clamps high");
        let (_, w3, h3) = build_card_rgba(0.0, [0, 0, 0]);
        assert_eq!((w3, h3), (((128.0 * CARD_MIN_ASPECT) as u16), 128), "aspect clamps low");
        assert!(w3 >= 24 && h3 >= 24, "minimum dimensions");
    }

    /// The hotspot must lie inside every possible card.
    #[test]
    fn hotspot_always_inside() {
        for aspect in [0.0f32, 0.4, 1.0, 2.5, 100.0] {
            let (_, w, h) = build_card_rgba(aspect, [9, 9, 9]);
            assert!(CARD_HOTSPOT.0 < w && CARD_HOTSPOT.1 < h, "aspect {aspect}");
        }
    }
}
