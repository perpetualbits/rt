//! The frame envelope: an 8-byte little-endian header followed by a payload.
//!
//! Framing is deliberately dumber than the payload: a fixed header a receiver
//! can read without knowing anything about the protocol version, so a version
//! mismatch is diagnosed from a decoded `Hello` rather than from a parse crash.

use crate::error::{Result, WireError};

/// The fixed header: u32 payload length, u16 message type, u16 flags.
pub const HEADER_LEN: usize = 8;

/// The largest payload a single frame may carry. A pane's screen and one
/// scrollback chunk both fit far inside this; the cap exists so a corrupt or
/// hostile length cannot make a receiver allocate the world.
pub const MAX_PAYLOAD: usize = 64 * 1024 * 1024;

/// One length-prefixed message on the wire.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub msg_type: u16,
    pub flags: u16,
    pub payload: Vec<u8>,
}

impl Frame {
    /// Append this frame's bytes to `out`.
    pub fn encode(&self, out: &mut Vec<u8>) -> Result<()> {
        if self.payload.len() > MAX_PAYLOAD {
            return Err(WireError::PayloadTooLarge { len: self.payload.len() as u64 });
        }
        out.extend_from_slice(&(self.payload.len() as u32).to_le_bytes());
        out.extend_from_slice(&self.msg_type.to_le_bytes());
        out.extend_from_slice(&self.flags.to_le_bytes());
        out.extend_from_slice(&self.payload);
        Ok(())
    }

    /// Decode one frame from the front of `buf`, returning it and the number of
    /// bytes consumed. The length is validated against `MAX_PAYLOAD` BEFORE any
    /// allocation, so a bogus header costs nothing.
    pub fn decode(buf: &[u8]) -> Result<(Frame, usize)> {
        if buf.len() < HEADER_LEN {
            return Err(WireError::Truncated { need: HEADER_LEN, had: buf.len() });
        }
        let len = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
        if len > MAX_PAYLOAD {
            return Err(WireError::PayloadTooLarge { len: len as u64 });
        }
        let total = HEADER_LEN + len;
        if buf.len() < total {
            return Err(WireError::Truncated { need: total, had: buf.len() });
        }
        Ok((
            Frame {
                msg_type: u16::from_le_bytes([buf[4], buf[5]]),
                flags: u16::from_le_bytes([buf[6], buf[7]]),
                payload: buf[HEADER_LEN..total].to_vec(),
            },
            total,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_round_trips() {
        let f = Frame { msg_type: 0x05, flags: 0, payload: vec![1, 2, 3] };
        let mut out = Vec::new();
        f.encode(&mut out).unwrap();
        assert_eq!(out.len(), HEADER_LEN + 3);
        let (back, used) = Frame::decode(&out).unwrap();
        assert_eq!(back, f);
        assert_eq!(used, out.len());
    }

    #[test]
    fn two_frames_decode_back_to_back() {
        let a = Frame { msg_type: 1, flags: 0, payload: vec![9] };
        let b = Frame { msg_type: 2, flags: 3, payload: vec![] };
        let mut out = Vec::new();
        a.encode(&mut out).unwrap();
        b.encode(&mut out).unwrap();
        let (fa, used_a) = Frame::decode(&out).unwrap();
        let (fb, used_b) = Frame::decode(&out[used_a..]).unwrap();
        assert_eq!(fa, a);
        assert_eq!(fb, b);
        assert_eq!(used_a + used_b, out.len());
    }

    #[test]
    fn truncated_header_is_truncated_not_panic() {
        let err = Frame::decode(&[0, 0, 0]).unwrap_err();
        assert_eq!(err, WireError::Truncated { need: HEADER_LEN, had: 3 });
    }

    #[test]
    fn truncated_payload_reports_the_full_need() {
        let f = Frame { msg_type: 7, flags: 0, payload: vec![1, 2, 3, 4] };
        let mut out = Vec::new();
        f.encode(&mut out).unwrap();
        out.truncate(HEADER_LEN + 2);
        let err = Frame::decode(&out).unwrap_err();
        assert_eq!(err, WireError::Truncated { need: HEADER_LEN + 4, had: HEADER_LEN + 2 });
    }

    #[test]
    fn oversize_declared_length_is_rejected_without_allocating() {
        // A hostile or corrupt header claiming 4 GiB must not become a Vec.
        let mut buf = Vec::new();
        buf.extend_from_slice(&u32::MAX.to_le_bytes());
        buf.extend_from_slice(&1u16.to_le_bytes());
        buf.extend_from_slice(&0u16.to_le_bytes());
        let err = Frame::decode(&buf).unwrap_err();
        assert_eq!(err, WireError::PayloadTooLarge { len: u32::MAX as u64 });
    }

    #[test]
    fn oversize_payload_is_rejected_on_encode() {
        let f = Frame { msg_type: 1, flags: 0, payload: vec![0; MAX_PAYLOAD + 1] };
        let err = f.encode(&mut Vec::new()).unwrap_err();
        assert_eq!(err, WireError::PayloadTooLarge { len: MAX_PAYLOAD as u64 + 1 });
    }
}
