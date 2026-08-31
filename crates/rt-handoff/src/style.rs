//! Cell appearance: colours, attribute bits, and the per-pane style table that
//! deduplicates them.
//!
//! Indexed colours stay indexed. Flattening them to RGB would make a moved
//! pane stop following the receiving window's colour scheme, which is a
//! visible regression and an irreversible one — the index is gone.

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buf::{Reader, Writer};
    use crate::error::WireError;

    fn round_trip_colour(c: Colour) -> Colour {
        let mut w = Writer::new();
        c.write(&mut w);
        let bytes = w.into_vec();
        assert_eq!(bytes.len(), 4, "a colour is always four bytes");
        let mut r = Reader::new(&bytes);
        Colour::read(&mut r, 0x3F).unwrap()
    }

    #[test]
    fn every_colour_kind_round_trips() {
        for c in [Colour::Default, Colour::Indexed(0), Colour::Indexed(255), Colour::Rgb(1, 2, 3), Colour::Rgb(255, 255, 255)] {
            assert_eq!(round_trip_colour(c), c, "colour {c:?}");
        }
    }

    #[test]
    fn indexed_is_not_flattened_to_rgb() {
        // The whole point: an index survives as an index.
        assert_eq!(round_trip_colour(Colour::Indexed(4)), Colour::Indexed(4));
    }

    #[test]
    fn unknown_colour_kind_is_bad_value() {
        let mut r = Reader::new(&[9, 0, 0, 0]);
        assert_eq!(
            Colour::read(&mut r, 0x27).unwrap_err(),
            WireError::BadValue { tag: 0x27, why: "colour kind must be 0, 1 or 2" }
        );
    }

    #[test]
    fn style_round_trips() {
        let s = Style {
            fg: Colour::Indexed(12),
            bg: Colour::Rgb(10, 20, 30),
            underline: Colour::Default,
            attrs: attrs::BOLD | attrs::ITALIC | attrs::CURLY_UNDERLINE,
            link_id: 7,
        };
        let mut w = Writer::new();
        s.write(&mut w);
        let bytes = w.into_vec();
        let mut r = Reader::new(&bytes);
        assert_eq!(Style::read(&mut r, 0x3F).unwrap(), s);
    }

    #[test]
    fn unknown_attribute_bits_are_masked_off() {
        // A future version sets bit 20. We must not carry it into our model,
        // where it would be re-emitted as a bit we cannot describe.
        let s = Style { attrs: attrs::BOLD | (1 << 20), ..Style::default() };
        let mut w = Writer::new();
        s.write(&mut w);
        let bytes = w.into_vec();
        let mut r = Reader::new(&bytes);
        assert_eq!(Style::read(&mut r, 0x3F).unwrap().attrs, attrs::BOLD);
    }

    #[test]
    fn attribute_bit_values_are_frozen() {
        // These numbers are the contract. If this test needs editing, stop.
        assert_eq!(attrs::BOLD, 1);
        assert_eq!(attrs::DIM, 2);
        assert_eq!(attrs::ITALIC, 4);
        assert_eq!(attrs::UNDERLINE, 8);
        assert_eq!(attrs::DOUBLE_UNDERLINE, 16);
        assert_eq!(attrs::CURLY_UNDERLINE, 32);
        assert_eq!(attrs::DOTTED_UNDERLINE, 64);
        assert_eq!(attrs::DASHED_UNDERLINE, 128);
        assert_eq!(attrs::BLINK, 256);
        assert_eq!(attrs::RAPID_BLINK, 512);
        assert_eq!(attrs::REVERSE, 1024);
        assert_eq!(attrs::HIDDEN, 2048);
        assert_eq!(attrs::STRIKEOUT, 4096);
        assert_eq!(attrs::OVERLINE, 8192);
        assert_eq!(attrs::KNOWN, 16383);
    }

    #[test]
    fn style_table_round_trips() {
        let table = vec![
            Style::default(),
            Style { fg: Colour::Indexed(1), ..Style::default() },
            Style { attrs: attrs::REVERSE, link_id: 3, ..Style::default() },
        ];
        let bytes = write_table(&table);
        assert_eq!(read_table(&bytes).unwrap(), table);
    }

    #[test]
    fn empty_style_table_round_trips() {
        let bytes = write_table(&[]);
        assert_eq!(read_table(&bytes).unwrap(), Vec::<Style>::new());
    }

    #[test]
    fn style_table_with_trailing_junk_is_rejected() {
        let mut bytes = write_table(&[Style::default()]);
        bytes.push(0xff);
        assert_eq!(read_table(&bytes).unwrap_err(), WireError::TrailingBytes { left: 1 });
    }
}

use crate::buf::{Reader, Writer};
use crate::error::{Result, WireError};

/// Text attribute bits. Frozen at v1: these numbers are on the wire.
pub mod attrs {
    pub const BOLD: u32 = 1;
    pub const DIM: u32 = 2;
    pub const ITALIC: u32 = 4;
    pub const UNDERLINE: u32 = 8;
    pub const DOUBLE_UNDERLINE: u32 = 16;
    pub const CURLY_UNDERLINE: u32 = 32;
    pub const DOTTED_UNDERLINE: u32 = 64;
    pub const DASHED_UNDERLINE: u32 = 128;
    pub const BLINK: u32 = 256;
    pub const RAPID_BLINK: u32 = 512;
    pub const REVERSE: u32 = 1024;
    pub const HIDDEN: u32 = 2048;
    pub const STRIKEOUT: u32 = 4096;
    pub const OVERLINE: u32 = 8192;
    /// Every bit this version understands. Higher bits are reserved for later
    /// versions and are masked off on decode.
    pub const KNOWN: u32 = 16383;
}

/// A cell colour. Four bytes on the wire: a kind byte and three payload bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Colour {
    /// The receiver's default foreground/background for this role.
    #[default]
    Default,
    /// An index into the 256-colour palette. Stays an index, deliberately.
    Indexed(u8),
    Rgb(u8, u8, u8),
}

impl Colour {
    pub fn write(&self, w: &mut Writer) {
        match *self {
            Colour::Default => {
                w.u8(0);
                w.u8(0);
                w.u8(0);
                w.u8(0);
            }
            Colour::Indexed(i) => {
                w.u8(1);
                w.u8(i);
                w.u8(0);
                w.u8(0);
            }
            Colour::Rgb(r, g, b) => {
                w.u8(2);
                w.u8(r);
                w.u8(g);
                w.u8(b);
            }
        }
    }

    pub fn read(r: &mut Reader<'_>, tag: u64) -> Result<Colour> {
        let kind = r.u8()?;
        let a = r.u8()?;
        let b = r.u8()?;
        let c = r.u8()?;
        match kind {
            0 => Ok(Colour::Default),
            1 => Ok(Colour::Indexed(a)),
            2 => Ok(Colour::Rgb(a, b, c)),
            _ => Err(WireError::BadValue { tag, why: "colour kind must be 0, 1 or 2" }),
        }
    }
}

/// Everything about how a run of cells is drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Style {
    pub fg: Colour,
    pub bg: Colour,
    pub underline: Colour,
    pub attrs: u32,
    /// Index into the pane's `uri_table` (OSC 8), or 0 for "no hyperlink".
    pub link_id: u32,
}

impl Style {
    pub fn write(&self, w: &mut Writer) {
        self.fg.write(w);
        self.bg.write(w);
        self.underline.write(w);
        w.varint(self.attrs as u64);
        w.varint(self.link_id as u64);
    }

    pub fn read(r: &mut Reader<'_>, tag: u64) -> Result<Style> {
        let fg = Colour::read(r, tag)?;
        let bg = Colour::read(r, tag)?;
        let underline = Colour::read(r, tag)?;
        // Deliberately NOT `varint_u32`. The spec makes `attrs` a varint and
        // says higher bits are reserved and masked off by a receiver, so a
        // future rt using attribute bit 40 is LEGAL wire: erroring on it would
        // reject an entire pane over an attribute this build merely cannot
        // render. Masking is also what stops us re-emitting a bit we cannot
        // describe.
        let attrs = (r.varint()? as u32) & attrs::KNOWN;
        let link_id = r.varint_u32()?;
        Ok(Style { fg, bg, underline, attrs, link_id })
    }
}

/// Encode a pane's style table: a varint count followed by that many styles.
pub fn write_table(styles: &[Style]) -> Vec<u8> {
    let mut w = Writer::new();
    w.varint(styles.len() as u64);
    for s in styles {
        s.write(&mut w);
    }
    w.into_vec()
}

/// Decode a style table, rejecting trailing bytes.
pub fn read_table(body: &[u8]) -> Result<Vec<Style>> {
    let mut r = Reader::new(body);
    let n = r.varint()? as usize;
    let mut out = Vec::with_capacity(n.min(4096));
    for _ in 0..n {
        out.push(Style::read(&mut r, 0x3F)?);
    }
    r.finish()?;
    Ok(out)
}
