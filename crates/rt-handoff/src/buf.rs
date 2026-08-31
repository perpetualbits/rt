//! Byte-level primitives shared by every encoder in this crate.
//!
//! `Writer` never fails: it appends to a `Vec`. `Reader` fails only by running
//! out of bytes or by meeting a varint that cannot be a u64.

use crate::error::{Result, WireError};

/// An append-only byte builder. Infallible by construction.
#[derive(Debug, Default)]
pub struct Writer {
    buf: Vec<u8>,
}

impl Writer {
    pub fn new() -> Self {
        Writer { buf: Vec::new() }
    }

    /// LEB128 unsigned. Seven bits per byte, high bit means "more follows".
    pub fn varint(&mut self, mut v: u64) {
        loop {
            let byte = (v & 0x7f) as u8;
            v >>= 7;
            if v == 0 {
                self.buf.push(byte);
                return;
            }
            self.buf.push(byte | 0x80);
        }
    }

    pub fn u8(&mut self, v: u8) {
        self.buf.push(v);
    }

    pub fn u32le(&mut self, v: u32) {
        self.buf.extend_from_slice(&v.to_le_bytes());
    }

    /// Length-prefixed raw bytes.
    pub fn bytes(&mut self, b: &[u8]) {
        self.varint(b.len() as u64);
        self.buf.extend_from_slice(b);
    }

    /// Length-prefixed UTF-8.
    pub fn str(&mut self, s: &str) {
        self.bytes(s.as_bytes());
    }

    /// Raw append, no length prefix. For nesting an already-encoded body.
    pub fn raw(&mut self, b: &[u8]) {
        self.buf.extend_from_slice(b);
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.buf
    }
}

/// A cursor over a byte slice. Every method either advances or fails.
#[derive(Debug)]
pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn is_empty(&self) -> bool {
        self.remaining() == 0
    }

    pub fn take(&mut self, n: usize) -> Result<&'a [u8]> {
        if self.remaining() < n {
            return Err(WireError::Truncated { need: n, had: self.remaining() });
        }
        let out = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(out)
    }

    pub fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    pub fn u32le(&mut self) -> Result<u32> {
        let b = self.take(4)?;
        Ok(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    /// LEB128 unsigned, capped at ten bytes — the most a u64 can occupy.
    ///
    /// The tenth byte carries only ONE meaningful bit (9 * 7 = 63 bits precede
    /// it), so a tenth byte above 1 encodes a value that cannot be a u64. It is
    /// rejected rather than silently truncated: a peer's corrupt varint must be
    /// an error, and two byte sequences must never decode to one value — that
    /// would put a hole in the canonical encoding the golden corpus rests on.
    pub fn varint(&mut self) -> Result<u64> {
        let mut out: u64 = 0;
        let mut shift = 0u32;
        for i in 0..10 {
            let byte = self.u8()?;
            let chunk = (byte & 0x7f) as u64;
            if i == 9 && chunk > 1 {
                return Err(WireError::VarintOverflow);
            }
            out |= chunk << shift;
            if byte & 0x80 == 0 {
                return Ok(out);
            }
            shift += 7;
        }
        Err(WireError::VarintOverflow)
    }

    pub fn bytes(&mut self) -> Result<&'a [u8]> {
        let n = self.varint()? as usize;
        self.take(n)
    }

    pub fn str(&mut self) -> Result<String> {
        let b = self.bytes()?;
        std::str::from_utf8(b).map(str::to_owned).map_err(|_| WireError::BadUtf8)
    }

    /// Assert the body is fully consumed. Catches an encoder that wrote more
    /// than the decoder reads, which is otherwise a silent divergence.
    pub fn finish(self) -> Result<()> {
        if self.remaining() != 0 {
            return Err(WireError::TrailingBytes { left: self.remaining() });
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::error::WireError;
    use super::{Reader, Writer};

    #[test]
    fn varints_round_trip_across_the_boundaries() {
        for v in [0u64, 1, 63, 127, 128, 129, 255, 256, 16383, 16384, 300, u32::MAX as u64, u64::MAX] {
            let mut w = Writer::new();
            w.varint(v);
            let bytes = w.into_vec();
            let mut r = Reader::new(&bytes);
            assert_eq!(r.varint().unwrap(), v, "value {v}");
            assert_eq!(r.remaining(), 0, "value {v} left bytes behind");
        }
    }

    #[test]
    fn small_varints_are_one_byte() {
        let mut w = Writer::new();
        w.varint(127);
        assert_eq!(w.into_vec().len(), 1);
    }

    #[test]
    fn truncated_varint_is_truncated() {
        // 0x80 has the continuation bit set but nothing follows.
        let mut r = Reader::new(&[0x80]);
        assert_eq!(r.varint().unwrap_err(), WireError::Truncated { need: 1, had: 0 });
    }

    #[test]
    fn overlong_varint_overflows_rather_than_wrapping() {
        let bytes = [0x80u8; 11];
        let mut r = Reader::new(&bytes);
        assert_eq!(r.varint().unwrap_err(), WireError::VarintOverflow);
    }

    #[test]
    fn a_ten_byte_varint_past_u64_is_rejected_not_truncated() {
        // Nine 0xFF bytes carry 63 bits; the tenth byte holds one meaningful
        // bit. A tenth byte of 0x03 needs bit 64, so it is not a u64 — it must
        // error rather than decode to the same value 0x01 would give.
        let mut over = [0xFFu8; 10];
        over[9] = 0x03;
        let mut r = Reader::new(&over);
        assert_eq!(r.varint().unwrap_err(), WireError::VarintOverflow);

        let mut max = [0xFFu8; 10];
        max[9] = 0x01;
        assert_eq!(Reader::new(&max).varint().unwrap(), u64::MAX);
    }

    #[test]
    fn strings_round_trip_including_non_ascii() {
        for s in ["", "hello", "héllo wörld", "日本語", "a\u{0}b"] {
            let mut w = Writer::new();
            w.str(s);
            let bytes = w.into_vec();
            let mut r = Reader::new(&bytes);
            assert_eq!(r.str().unwrap(), s);
            r.finish().unwrap();
        }
    }

    #[test]
    fn invalid_utf8_is_bad_utf8() {
        let mut w = Writer::new();
        w.bytes(&[0xff, 0xfe]);
        let bytes = w.into_vec();
        let mut r = Reader::new(&bytes);
        assert_eq!(r.str().unwrap_err(), WireError::BadUtf8);
    }

    #[test]
    fn fixed_width_is_little_endian() {
        let mut w = Writer::new();
        w.u32le(0x0102_0304);
        assert_eq!(w.into_vec(), vec![0x04, 0x03, 0x02, 0x01]);
    }

    #[test]
    fn finish_rejects_trailing_bytes() {
        let r = Reader::new(&[1, 2, 3]);
        assert_eq!(r.finish().unwrap_err(), WireError::TrailingBytes { left: 3 });
    }
}
