//! Tag-length-value fields.
//!
//! The whole cross-version story lives here. A decoder walks fields it knows
//! and skips the rest by length (R2), reporting which tags it skipped (R6). An
//! encoder emits in ascending tag order (R7), which makes the encoding
//! canonical so the golden corpus can be compared byte-for-byte.

use crate::buf::{Reader, Writer};
use crate::error::Result;

/// Builds a TLV body, enforcing R7 (ascending tag order).
#[derive(Debug, Default)]
pub struct FieldWriter {
    w: Writer,
    last_tag: Option<u64>,
}

impl FieldWriter {
    pub fn new() -> Self {
        FieldWriter { w: Writer::new(), last_tag: None }
    }

    /// Append one field. Tags must be strictly ascending — a violation is an
    /// encoder bug, not a runtime condition, so it panics in every build. A
    /// non-canonical encoding would silently break the golden corpus instead.
    pub fn field(&mut self, tag: u64, value: &[u8]) {
        if let Some(prev) = self.last_tag {
            assert!(tag > prev, "fields must be emitted in ascending tag order (R7): 0x{tag:02x} after 0x{prev:02x}");
        }
        self.last_tag = Some(tag);
        self.w.varint(tag);
        self.w.bytes(value);
    }

    pub fn into_vec(self) -> Vec<u8> {
        self.w.into_vec()
    }
}

/// Walk a TLV body. `f` is called with each field and returns whether it
/// claimed the tag; unclaimed tags are skipped and returned in order.
///
/// This is the single implementation of rules R2 and R6. Message decoders call
/// it rather than reading fields themselves, so neither rule can be forgotten
/// in one message and honoured in another.
pub fn walk<'a>(body: &'a [u8], mut f: impl FnMut(u64, &'a [u8]) -> Result<bool>) -> Result<Vec<u64>> {
    let mut r = Reader::new(body);
    let mut unknown = Vec::new();
    while !r.is_empty() {
        let tag = r.varint()?;
        let value = r.bytes()?;
        if !f(tag, value)? {
            unknown.push(tag);
        }
    }
    Ok(unknown)
}

/// Every tag present in a body, in wire order. Used by the golden corpus test
/// and by `tag_names` construction.
pub fn tags_of(body: &[u8]) -> Result<Vec<u64>> {
    let mut tags = Vec::new();
    walk(body, |tag, _| {
        tags.push(tag);
        Ok(true)
    })?;
    Ok(tags)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WireError;

    #[test]
    fn fields_round_trip() {
        let mut fw = FieldWriter::new();
        fw.field(0x01, &[1, 2, 3]);
        fw.field(0x20, b"hello");
        let body = fw.into_vec();

        let mut seen: Vec<(u64, Vec<u8>)> = Vec::new();
        let unknown = walk(&body, |tag, val| {
            seen.push((tag, val.to_vec()));
            Ok(true)
        })
        .unwrap();

        assert_eq!(seen, vec![(0x01, vec![1, 2, 3]), (0x20, b"hello".to_vec())]);
        assert!(unknown.is_empty());
    }

    #[test]
    fn unknown_tags_are_skipped_and_reported() {
        let mut fw = FieldWriter::new();
        fw.field(0x01, &[7]);
        fw.field(0x99, &[0; 40]); // a field from a future version
        fw.field(0xFA, b"also future");
        let body = fw.into_vec();

        let mut known = Vec::new();
        let unknown = walk(&body, |tag, val| {
            if tag == 0x01 {
                known.push(val.to_vec());
                Ok(true)
            } else {
                Ok(false) // "I do not know this tag"
            }
        })
        .unwrap();

        assert_eq!(known, vec![vec![7]]);
        assert_eq!(unknown, vec![0x99, 0xFA]);
    }

    #[test]
    fn decoder_accepts_descending_order_even_though_encoders_must_not_emit_it() {
        // R7 binds encoders, not decoders. Hand-build a descending body.
        let mut w = crate::buf::Writer::new();
        w.varint(0x20);
        w.bytes(b"b");
        w.varint(0x01);
        w.bytes(b"a");
        let body = w.into_vec();

        let mut seen = Vec::new();
        walk(&body, |tag, _| {
            seen.push(tag);
            Ok(true)
        })
        .unwrap();
        assert_eq!(seen, vec![0x20, 0x01]);
    }

    #[test]
    fn writer_emits_ascending_and_tags_of_agrees() {
        let mut fw = FieldWriter::new();
        fw.field(0x01, &[]);
        fw.field(0x0F, &[]);
        fw.field(0x40, &[]);
        let body = fw.into_vec();
        assert_eq!(tags_of(&body).unwrap(), vec![0x01, 0x0F, 0x40]);
    }

    #[test]
    #[should_panic(expected = "ascending")]
    fn writer_panics_on_out_of_order_tags() {
        let mut fw = FieldWriter::new();
        fw.field(0x20, &[]);
        fw.field(0x01, &[]);
    }

    #[test]
    fn truncated_field_value_is_truncated() {
        let mut w = crate::buf::Writer::new();
        w.varint(0x01);
        w.varint(10); // claims ten bytes
        w.raw(&[1, 2]); // provides two
        let body = w.into_vec();
        let err = walk(&body, |_, _| Ok(true)).unwrap_err();
        assert_eq!(err, WireError::Truncated { need: 10, had: 2 });
    }

    #[test]
    fn empty_body_walks_cleanly() {
        assert!(walk(&[], |_, _| Ok(true)).unwrap().is_empty());
    }

    #[test]
    fn a_field_may_be_empty() {
        let mut fw = FieldWriter::new();
        fw.field(0x03, &[]);
        let body = fw.into_vec();
        let mut lens = Vec::new();
        walk(&body, |_, val| {
            lens.push(val.len());
            Ok(true)
        })
        .unwrap();
        assert_eq!(lens, vec![0]);
    }
}
