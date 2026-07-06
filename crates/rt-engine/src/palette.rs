//! The colour palette and cell-colour resolution.
//!
//! `alacritty_terminal` stores each cell's colour abstractly as
//! `Color::{Named, Spec, Indexed}` and never resolves it to RGB — that is the
//! front-end's job (it lives in the `alacritty` binary, not the reusable
//! engine). So rt builds a standard xterm-style 256-colour palette here and
//! resolves every cell to concrete RGB, honouring the attribute flags
//! (bold→bright, dim, inverse, hidden). This is what lets full-colour programs
//! (e.g. `spiral_stress`) render as intended instead of monochrome.

/// An 8-bit-per-channel RGB colour, the concrete form the renderer draws.
pub type Rgb = [u8; 3];

/// Default foreground: light grey (matches the window chrome).
pub const DEFAULT_FG: Rgb = [0xd0, 0xd0, 0xd8];
/// Default background: near-black (matches the window clear colour, so
/// default-background cells stay translucent instead of drawing an opaque quad).
pub const DEFAULT_BG: Rgb = [0x10, 0x10, 0x14];
/// Cursor block colour.
pub const CURSOR: Rgb = [0xd0, 0xd0, 0xd8];

/// The 16 base ANSI colours (classic xterm values): 0–7 normal, 8–15 bright.
const ANSI16: [Rgb; 16] = [
    [0x00, 0x00, 0x00], // 0 black
    [0xcd, 0x00, 0x00], // 1 red
    [0x00, 0xcd, 0x00], // 2 green
    [0xcd, 0xcd, 0x00], // 3 yellow
    [0x00, 0x00, 0xee], // 4 blue
    [0xcd, 0x00, 0xcd], // 5 magenta
    [0x00, 0xcd, 0xcd], // 6 cyan
    [0xe5, 0xe5, 0xe5], // 7 white
    [0x7f, 0x7f, 0x7f], // 8 bright black (grey)
    [0xff, 0x00, 0x00], // 9 bright red
    [0x00, 0xff, 0x00], // 10 bright green
    [0xff, 0xff, 0x00], // 11 bright yellow
    [0x5c, 0x5c, 0xff], // 12 bright blue
    [0xff, 0x00, 0xff], // 13 bright magenta
    [0x00, 0xff, 0xff], // 14 bright cyan
    [0xff, 0xff, 0xff], // 15 bright white
];

/// The full 256-colour palette used to resolve `Indexed(i)` and named 0–15.
///
/// Layout (standard xterm): 0–15 the ANSI colours above; 16–231 a 6×6×6 RGB
/// cube; 232–255 a 24-step grayscale ramp.
#[derive(Clone)]
pub struct Palette {
    colors: [Rgb; 256], // fully materialised 256-colour table
    pub fg: Rgb,         // default foreground (Named::Foreground resolves here)
    pub bg: Rgb,         // default background (Named::Background resolves here)
    pub cursor: Rgb,     // cursor colour (Named::Cursor)
}

impl Palette {
    /// Build the standard xterm 256-colour palette with rt's default fg/bg.
    pub fn xterm() -> Self {
        Self::new(DEFAULT_FG, DEFAULT_BG, ANSI16)
    }

    /// Build a palette from a foreground, background, and the 16 ANSI base
    /// colours; the 216-colour cube and 24-step greyscale ramp are derived. This
    /// is what makes rt's colours configurable (fed from `rt_config::Settings`).
    /// The cursor colour defaults to the foreground.
    pub fn new(fg: Rgb, bg: Rgb, ansi16: [Rgb; 16]) -> Self {
        let mut colors = [[0u8; 3]; 256]; // start black
        // 0–15: the ANSI base colours.
        colors[..16].copy_from_slice(&ansi16);
        // 16–231: the 6×6×6 colour cube. Each axis level 0..5 maps to a byte:
        // 0 → 0, otherwise 55 + 40·level (the xterm convention).
        for i in 0..216usize {
            let r = i / 36; // red level 0..5
            let g = (i / 6) % 6; // green level 0..5
            let b = i % 6; // blue level 0..5
            let comp = |v: usize| -> u8 {
                if v == 0 { 0 } else { (55 + 40 * v) as u8 } // xterm cube ramp
            };
            colors[16 + i] = [comp(r), comp(g), comp(b)];
        }
        // 232–255: 24-step grayscale, value = 8 + 10·step.
        for i in 0..24usize {
            let v = (8 + 10 * i) as u8; // grey level
            colors[232 + i] = [v, v, v];
        }
        Palette { colors, fg, bg, cursor: fg }
    }

    /// The RGB for a 0–255 palette index.
    pub fn indexed(&self, i: u8) -> Rgb {
        self.colors[i as usize]
    }
}

/// Darken an RGB colour to ~2/3 brightness, for the DIM attribute.
pub fn dim(c: Rgb) -> Rgb {
    [
        ((c[0] as u16 * 2) / 3) as u8, // scale each channel down
        ((c[1] as u16 * 2) / 3) as u8,
        ((c[2] as u16 * 2) / 3) as u8,
    ]
}
