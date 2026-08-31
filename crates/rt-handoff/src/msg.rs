//! The message set. Bodies are TLV like everything else, so a v1 receiver can
//! read a v2 Hello and still learn the version it needs to refuse.

use crate::buf::{Reader, Writer};
use crate::error::{Result, WireError};
use crate::frame::Frame;
use crate::grid::Line;
use crate::pane::PaneWire;
use crate::tlv::{self, FieldWriter};
use crate::tree::Node;

/// Frame message types. Frozen.
pub mod msg_type {
    pub const HELLO: u16 = 0x01;
    pub const OFFER: u16 = 0x02;
    pub const CLAIM: u16 = 0x03;
    pub const TREE: u16 = 0x04;
    pub const PANE_STATE: u16 = 0x05;
    pub const PANE_FDS: u16 = 0x06;
    pub const SCROLL_CHUNK: u16 = 0x07;
    pub const ADOPTED: u16 = 0x08;
    pub const FAILED: u16 = 0x09;
    pub const CANCEL: u16 = 0x0A;
    pub const PING: u16 = 0x0B;
    pub const PONG: u16 = 0x0C;
    pub const BYE: u16 = 0x0D;
}

/// `Failed.code` values. Frozen. An unknown code is treated as `INTERNAL`.
pub mod fail {
    pub const PROTOCOL_MISMATCH: u32 = 1;
    pub const PEER_REJECTED: u32 = 2;
    pub const BAD_TOKEN: u32 = 3;
    pub const OVER_BUDGET: u32 = 4;
    pub const MALFORMED_FRAME: u32 = 5;
    pub const FD_PASSING_FAILED: u32 = 6;
    pub const CANNOT_PLACE: u32 = 7;
    pub const INTERNAL: u32 = 8;
}

/// `Hello` field tags.
pub mod hello_tags {
    pub const MAGIC: u64 = 0x01;
    pub const PROTO_MIN: u64 = 0x02;
    pub const PROTO_MAX: u64 = 0x03;
    pub const RT_VERSION: u64 = 0x04;
    pub const ENGINE: u64 = 0x05;
    pub const BOOT_ID: u64 = 0x06;
    pub const CAPS: u64 = 0x07;
    pub const MAX_SCROLLBACK_LINES: u64 = 0x08;
    pub const MAX_PAYLOAD_BYTES: u64 = 0x09;
    pub const DISPLAY: u64 = 0x0A;
}

/// `Offer` field tags.
pub mod offer_tags {
    pub const TOKEN: u64 = 0x01;
    pub const PANE_COUNT: u64 = 0x02;
    pub const TITLES: u64 = 0x03;
    pub const BYTE_ESTIMATE: u64 = 0x04;
}

/// `Claim` field tags.
pub mod claim_tags {
    pub const TOKEN: u64 = 0x01;
    pub const TARGET: u64 = 0x02;
    pub const ACCEPTED_BUDGET: u64 = 0x03;
}

/// Where a claimed payload should land in the receiver's tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DropTargetWire {
    Root,
    SplitLeft(u64),
    SplitRight(u64),
    SplitAbove(u64),
    SplitBelow(u64),
    Swap(u64),
    TabInsert { first_pane: u64, index: u32 },
}

impl DropTargetWire {
    fn write(&self, w: &mut Writer) {
        match self {
            DropTargetWire::Root => w.varint(0),
            DropTargetWire::SplitLeft(p) => {
                w.varint(1);
                w.varint(*p);
            }
            DropTargetWire::SplitRight(p) => {
                w.varint(2);
                w.varint(*p);
            }
            DropTargetWire::SplitAbove(p) => {
                w.varint(3);
                w.varint(*p);
            }
            DropTargetWire::SplitBelow(p) => {
                w.varint(4);
                w.varint(*p);
            }
            DropTargetWire::Swap(p) => {
                w.varint(5);
                w.varint(*p);
            }
            DropTargetWire::TabInsert { first_pane, index } => {
                w.varint(6);
                w.varint(*first_pane);
                w.varint(*index as u64);
            }
        }
    }

    fn read(r: &mut Reader<'_>, tag: u64) -> Result<DropTargetWire> {
        Ok(match r.varint()? {
            0 => DropTargetWire::Root,
            1 => DropTargetWire::SplitLeft(r.varint()?),
            2 => DropTargetWire::SplitRight(r.varint()?),
            3 => DropTargetWire::SplitAbove(r.varint()?),
            4 => DropTargetWire::SplitBelow(r.varint()?),
            5 => DropTargetWire::Swap(r.varint()?),
            6 => DropTargetWire::TabInsert { first_pane: r.varint()?, index: r.varint()? as u32 },
            _ => return Err(WireError::BadValue { tag, why: "unknown drop target kind" }),
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Hello {
    pub proto_min: u32,
    pub proto_max: u32,
    pub rt_version: String,
    pub engine: String,
    pub boot_id: String,
    pub caps: Vec<u64>,
    pub max_scrollback_lines: u32,
    pub max_payload_bytes: u64,
    pub display: String,
    pub unknown_tags: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Offer {
    pub token: [u8; 16],
    pub pane_count: u32,
    pub titles: Vec<String>,
    pub byte_estimate: u64,
    pub unknown_tags: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Claim {
    pub token: [u8; 16],
    pub target: DropTargetWire,
    pub accepted_budget: u32,
    pub unknown_tags: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollChunk {
    pub pane_uid: u64,
    /// True when more chunks follow for this pane.
    pub more: bool,
    /// Newest-first, so a cancelled transfer keeps the history that matters.
    pub lines: Vec<Line>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PaneFds {
    pub pane_uid: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Adopted {
    pub pane_uids: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failed {
    pub code: u32,
    pub text: String,
}

/// Encode one field's value with a fresh writer.
fn enc(f: impl FnOnce(&mut Writer)) -> Vec<u8> {
    let mut w = Writer::new();
    f(&mut w);
    w.into_vec()
}

/// Write already-sorted `(tag, bytes)` pairs as a TLV body. `FieldWriter`
/// asserts ascending order, so the sort in each `fields()` is what keeps this
/// honest.
fn encode_fields(fields: &[(u64, Vec<u8>)]) -> Vec<u8> {
    let mut fw = FieldWriter::new();
    for (tag, val) in fields {
        fw.field(*tag, val);
    }
    fw.into_vec()
}

impl Hello {
    /// `(tag, bytes)` in ascending order. Public so a test can splice in a
    /// field from an imagined future version.
    pub fn fields(&self) -> Vec<(u64, Vec<u8>)> {
        let mut out = vec![
            (hello_tags::MAGIC, crate::MAGIC.to_vec()),
            (hello_tags::PROTO_MIN, enc(|w| w.varint(self.proto_min as u64))),
            (hello_tags::PROTO_MAX, enc(|w| w.varint(self.proto_max as u64))),
            (hello_tags::RT_VERSION, enc(|w| w.str(&self.rt_version))),
            (hello_tags::ENGINE, enc(|w| w.str(&self.engine))),
            (hello_tags::BOOT_ID, enc(|w| w.str(&self.boot_id))),
            (hello_tags::CAPS, enc(|w| {
                w.varint(self.caps.len() as u64);
                for c in &self.caps {
                    w.varint(*c);
                }
            })),
            (hello_tags::MAX_SCROLLBACK_LINES, enc(|w| w.varint(self.max_scrollback_lines as u64))),
            (hello_tags::MAX_PAYLOAD_BYTES, enc(|w| w.varint(self.max_payload_bytes))),
            (hello_tags::DISPLAY, enc(|w| w.str(&self.display))),
        ];
        out.sort_by_key(|(t, _)| *t);
        out
    }

    fn encode(&self) -> Vec<u8> {
        encode_fields(&self.fields())
    }

    fn decode(body: &[u8]) -> Result<Hello> {
        let mut h = Hello::default();
        // The magic must be PRESENT, not merely correct-when-present: a body
        // that omits it entirely is not from an rt peer, and saying so here is
        // the whole reason the field exists. Without this it would surface far
        // downstream as a baffling version mismatch against proto 0/0.
        let mut saw_magic = false;
        let unknown = tlv::walk(body, |tag, val| {
            let mut r = Reader::new(val);
            match tag {
                hello_tags::MAGIC => {
                    if val != crate::MAGIC {
                        return Err(WireError::BadValue { tag, why: "not an rt handoff peer" });
                    }
                    saw_magic = true;
                    return Ok(true);
                }
                hello_tags::PROTO_MIN => h.proto_min = r.varint()? as u32,
                hello_tags::PROTO_MAX => h.proto_max = r.varint()? as u32,
                hello_tags::RT_VERSION => h.rt_version = r.str()?,
                hello_tags::ENGINE => h.engine = r.str()?,
                hello_tags::BOOT_ID => h.boot_id = r.str()?,
                hello_tags::CAPS => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        h.caps.push(r.varint()?);
                    }
                }
                hello_tags::MAX_SCROLLBACK_LINES => h.max_scrollback_lines = r.varint()? as u32,
                hello_tags::MAX_PAYLOAD_BYTES => h.max_payload_bytes = r.varint()?,
                hello_tags::DISPLAY => h.display = r.str()?,
                _ => return Ok(false),
            }
            r.finish()?;
            Ok(true)
        })?;
        if !saw_magic {
            return Err(WireError::BadValue { tag: hello_tags::MAGIC, why: "not an rt handoff peer" });
        }
        h.unknown_tags = unknown;
        Ok(h)
    }
}

impl Offer {
    pub fn fields(&self) -> Vec<(u64, Vec<u8>)> {
        let mut out = vec![
            (offer_tags::TOKEN, self.token.to_vec()),
            (offer_tags::PANE_COUNT, enc(|w| w.varint(self.pane_count as u64))),
            (offer_tags::TITLES, enc(|w| {
                w.varint(self.titles.len() as u64);
                for t in &self.titles {
                    w.str(t);
                }
            })),
            (offer_tags::BYTE_ESTIMATE, enc(|w| w.varint(self.byte_estimate))),
        ];
        out.sort_by_key(|(t, _)| *t);
        out
    }

    fn encode(&self) -> Vec<u8> {
        encode_fields(&self.fields())
    }

    fn decode(body: &[u8]) -> Result<Offer> {
        let mut o = Offer::default();
        o.unknown_tags = tlv::walk(body, |tag, val| {
            let mut r = Reader::new(val);
            match tag {
                offer_tags::TOKEN => {
                    if val.len() != 16 {
                        return Err(WireError::BadValue { tag, why: "token must be 16 bytes" });
                    }
                    o.token.copy_from_slice(val);
                    return Ok(true);
                }
                offer_tags::PANE_COUNT => o.pane_count = r.varint()? as u32,
                offer_tags::TITLES => {
                    let n = r.varint()? as usize;
                    for _ in 0..n {
                        o.titles.push(r.str()?);
                    }
                }
                offer_tags::BYTE_ESTIMATE => o.byte_estimate = r.varint()?,
                _ => return Ok(false),
            }
            r.finish()?;
            Ok(true)
        })?;
        Ok(o)
    }
}

impl Claim {
    pub fn fields(&self) -> Vec<(u64, Vec<u8>)> {
        let mut out = vec![
            (claim_tags::TOKEN, self.token.to_vec()),
            (claim_tags::TARGET, enc(|w| self.target.write(w))),
            (claim_tags::ACCEPTED_BUDGET, enc(|w| w.varint(self.accepted_budget as u64))),
        ];
        out.sort_by_key(|(t, _)| *t);
        out
    }

    fn encode(&self) -> Vec<u8> {
        encode_fields(&self.fields())
    }

    fn decode(body: &[u8]) -> Result<Claim> {
        let mut token = [0u8; 16];
        let mut target = DropTargetWire::Root;
        let mut accepted_budget = 0u32;
        let unknown_tags = tlv::walk(body, |tag, val| {
            let mut r = Reader::new(val);
            match tag {
                claim_tags::TOKEN => {
                    if val.len() != 16 {
                        return Err(WireError::BadValue { tag, why: "token must be 16 bytes" });
                    }
                    token.copy_from_slice(val);
                    return Ok(true);
                }
                claim_tags::TARGET => target = DropTargetWire::read(&mut r, tag)?,
                claim_tags::ACCEPTED_BUDGET => accepted_budget = r.varint()? as u32,
                _ => return Ok(false),
            }
            r.finish()?;
            Ok(true)
        })?;
        Ok(Claim { token, target, accepted_budget, unknown_tags })
    }
}

/// One decoded message. `PaneState` is boxed because it dwarfs the others.
#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    Hello(Hello),
    Offer(Offer),
    Claim(Claim),
    Tree(Node),
    PaneState(Box<PaneWire>),
    PaneFds(PaneFds),
    ScrollChunk(ScrollChunk),
    Adopted(Adopted),
    Failed(Failed),
    Cancel,
    Ping,
    Pong,
    Bye,
}

impl Message {
    pub fn to_frame(&self) -> Result<Frame> {
        let (msg_type, payload) = match self {
            Message::Hello(h) => (msg_type::HELLO, h.encode()),
            Message::Offer(o) => (msg_type::OFFER, o.encode()),
            Message::Claim(c) => (msg_type::CLAIM, c.encode()),
            Message::Tree(n) => (msg_type::TREE, n.encode()),
            Message::PaneState(p) => (msg_type::PANE_STATE, p.encode()),
            Message::PaneFds(f) => {
                let mut w = Writer::new();
                w.varint(f.pane_uid);
                (msg_type::PANE_FDS, w.into_vec())
            }
            Message::ScrollChunk(s) => {
                let mut w = Writer::new();
                w.varint(s.pane_uid);
                w.u8(s.more as u8);
                w.varint(s.lines.len() as u64);
                for l in &s.lines {
                    l.write(&mut w);
                }
                (msg_type::SCROLL_CHUNK, w.into_vec())
            }
            Message::Adopted(a) => {
                let mut w = Writer::new();
                w.varint(a.pane_uids.len() as u64);
                for u in &a.pane_uids {
                    w.varint(*u);
                }
                (msg_type::ADOPTED, w.into_vec())
            }
            Message::Failed(f) => {
                let mut w = Writer::new();
                w.varint(f.code as u64);
                w.str(&f.text);
                (msg_type::FAILED, w.into_vec())
            }
            Message::Cancel => (msg_type::CANCEL, Vec::new()),
            Message::Ping => (msg_type::PING, Vec::new()),
            Message::Pong => (msg_type::PONG, Vec::new()),
            Message::Bye => (msg_type::BYE, Vec::new()),
        };
        let frame = Frame { msg_type, flags: 0, payload };
        if frame.payload.len() > crate::frame::MAX_PAYLOAD {
            return Err(WireError::PayloadTooLarge { len: frame.payload.len() as u64 });
        }
        Ok(frame)
    }

    pub fn from_frame(frame: &Frame) -> Result<Message> {
        let body = &frame.payload[..];
        Ok(match frame.msg_type {
            msg_type::HELLO => Message::Hello(Hello::decode(body)?),
            msg_type::OFFER => Message::Offer(Offer::decode(body)?),
            msg_type::CLAIM => Message::Claim(Claim::decode(body)?),
            msg_type::TREE => Message::Tree(Node::decode(body)?),
            msg_type::PANE_STATE => Message::PaneState(Box::new(PaneWire::decode(body)?)),
            msg_type::PANE_FDS => {
                let mut r = Reader::new(body);
                let pane_uid = r.varint()?;
                r.finish()?;
                Message::PaneFds(PaneFds { pane_uid })
            }
            msg_type::SCROLL_CHUNK => {
                let mut r = Reader::new(body);
                let pane_uid = r.varint()?;
                let more = r.u8()? != 0;
                let n = r.varint()? as usize;
                let mut lines = Vec::with_capacity(n.min(65536));
                for _ in 0..n {
                    lines.push(Line::read(&mut r, msg_type::SCROLL_CHUNK as u64)?);
                }
                r.finish()?;
                Message::ScrollChunk(ScrollChunk { pane_uid, more, lines })
            }
            msg_type::ADOPTED => {
                let mut r = Reader::new(body);
                let n = r.varint()? as usize;
                let mut pane_uids = Vec::with_capacity(n.min(4096));
                for _ in 0..n {
                    pane_uids.push(r.varint()?);
                }
                r.finish()?;
                Message::Adopted(Adopted { pane_uids })
            }
            msg_type::FAILED => {
                let mut r = Reader::new(body);
                let code = r.varint()? as u32;
                let text = r.str()?;
                r.finish()?;
                Message::Failed(Failed { code, text })
            }
            msg_type::CANCEL => Message::Cancel,
            msg_type::PING => Message::Ping,
            msg_type::PONG => Message::Pong,
            msg_type::BYE => Message::Bye,
            other => return Err(WireError::UnknownMsgType(other)),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::WireError;
    use crate::grid::{Line, Run};

    fn round_trip(m: &Message) -> Message {
        Message::from_frame(&m.to_frame().unwrap()).unwrap()
    }

    #[test]
    fn hello_round_trips() {
        let m = Message::Hello(Hello {
            proto_min: 1,
            proto_max: 1,
            rt_version: "0.3.20".into(),
            engine: "vtterm".into(),
            boot_id: "6f1c...".into(),
            caps: vec![1, 2],
            max_scrollback_lines: 50_000,
            max_payload_bytes: 64 * 1024 * 1024,
            display: "wayland".into(),
            unknown_tags: vec![],
        });
        assert_eq!(round_trip(&m), m);
    }

    #[test]
    fn hello_carries_the_magic_so_a_wrong_peer_fails_clearly() {
        let m = Message::Hello(Hello { proto_min: 1, proto_max: 1, ..Hello::default() });
        let frame = m.to_frame().unwrap();
        assert!(frame.payload.windows(9).any(|w| w == crate::MAGIC), "the magic must appear in the body");
    }

    #[test]
    fn a_hello_without_the_magic_is_rejected() {
        let mut fw = crate::tlv::FieldWriter::new();
        fw.field(hello_tags::MAGIC, b"NOTRTHAND");
        fw.field(hello_tags::PROTO_MIN, &[1]);
        let frame = crate::frame::Frame { msg_type: msg_type::HELLO, flags: 0, payload: fw.into_vec() };
        assert_eq!(
            Message::from_frame(&frame).unwrap_err(),
            WireError::BadValue { tag: hello_tags::MAGIC, why: "not an rt handoff peer" }
        );
    }

    #[test]
    fn a_hello_that_omits_the_magic_entirely_is_rejected() {
        // Not the same case as a WRONG magic: tlv::walk has no notion of a
        // required tag, so an omitted one would otherwise decode to a default
        // Hello and be mistaken for a protocol-0 peer.
        let mut fw = crate::tlv::FieldWriter::new();
        fw.field(hello_tags::PROTO_MIN, &[1]);
        fw.field(hello_tags::PROTO_MAX, &[1]);
        let frame = crate::frame::Frame { msg_type: msg_type::HELLO, flags: 0, payload: fw.into_vec() };
        assert_eq!(
            Message::from_frame(&frame).unwrap_err(),
            WireError::BadValue { tag: hello_tags::MAGIC, why: "not an rt handoff peer" }
        );
    }

    #[test]
    fn a_hello_from_a_future_version_still_decodes_its_version_fields() {
        // The whole point of TLV in the handshake: a v1 build must be able to
        // read a v9 Hello well enough to say "we share no protocol".
        let mut m = Hello { proto_min: 4, proto_max: 9, rt_version: "0.9.0".into(), ..Hello::default() };
        m.caps = vec![77];
        let mut fields = m.fields();
        fields.push((0x6000, b"a field from the future".to_vec()));
        fields.sort_by_key(|(t, _)| *t);
        let mut fw = crate::tlv::FieldWriter::new();
        for (tag, val) in &fields {
            fw.field(*tag, val);
        }
        let frame = crate::frame::Frame { msg_type: msg_type::HELLO, flags: 0, payload: fw.into_vec() };
        let back = match Message::from_frame(&frame).unwrap() {
            Message::Hello(h) => h,
            other => panic!("wrong message: {other:?}"),
        };
        assert_eq!((back.proto_min, back.proto_max), (4, 9));
        assert_eq!(back.unknown_tags, vec![0x6000]);
    }

    #[test]
    fn offer_and_claim_round_trip() {
        let offer = Message::Offer(Offer {
            token: [7u8; 16],
            pane_count: 3,
            titles: vec!["zsh".into(), "claude".into(), "vim".into()],
            byte_estimate: 1_500_000,
            unknown_tags: vec![],
        });
        assert_eq!(round_trip(&offer), offer);

        let claim = Message::Claim(Claim {
            token: [7u8; 16],
            target: DropTargetWire::SplitRight(99),
            accepted_budget: 50_000,
            unknown_tags: vec![],
        });
        assert_eq!(round_trip(&claim), claim);
    }

    #[test]
    fn an_offer_from_a_future_version_keeps_the_fields_we_know() {
        let o = Offer { token: [3u8; 16], pane_count: 2, titles: vec!["a".into()], byte_estimate: 9, unknown_tags: vec![] };
        let mut fields = o.fields();
        fields.push((0x50, b"a budget knob from rt 0.9".to_vec()));
        fields.sort_by_key(|(t, _)| *t);
        let mut fw = crate::tlv::FieldWriter::new();
        for (tag, val) in &fields {
            fw.field(*tag, val);
        }
        let frame = crate::frame::Frame { msg_type: msg_type::OFFER, flags: 0, payload: fw.into_vec() };
        let back = match Message::from_frame(&frame).unwrap() {
            Message::Offer(o) => o,
            other => panic!("wrong message: {other:?}"),
        };
        assert_eq!(back.pane_count, 2);
        assert_eq!(back.titles, vec!["a".to_string()]);
        assert_eq!(back.unknown_tags, vec![0x50]);
    }

    #[test]
    fn every_drop_target_round_trips() {
        for t in [
            DropTargetWire::Root,
            DropTargetWire::SplitLeft(1),
            DropTargetWire::SplitRight(2),
            DropTargetWire::SplitAbove(3),
            DropTargetWire::SplitBelow(4),
            DropTargetWire::Swap(5),
            DropTargetWire::TabInsert { first_pane: 6, index: 2 },
        ] {
            let m = Message::Claim(Claim { token: [0; 16], target: t.clone(), accepted_budget: 1, unknown_tags: vec![] });
            assert_eq!(round_trip(&m), m, "target {t:?}");
        }
    }

    #[test]
    fn scroll_chunks_round_trip_and_carry_the_more_flag() {
        let m = Message::ScrollChunk(ScrollChunk {
            pane_uid: 5,
            more: true,
            lines: vec![Line { flags: 0, runs: vec![Run::text(0, "an older line")] }],
        });
        assert_eq!(round_trip(&m), m);
    }

    #[test]
    fn the_small_control_messages_round_trip() {
        for m in [
            Message::PaneFds(PaneFds { pane_uid: 3 }),
            Message::Adopted(Adopted { pane_uids: vec![1, 2, 3] }),
            Message::Failed(Failed { code: fail::BAD_TOKEN, text: "token expired".into() }),
            Message::Cancel,
            Message::Ping,
            Message::Pong,
            Message::Bye,
        ] {
            assert_eq!(round_trip(&m), m, "message {m:?}");
        }
    }

    #[test]
    fn failure_codes_are_frozen() {
        assert_eq!(fail::PROTOCOL_MISMATCH, 1);
        assert_eq!(fail::PEER_REJECTED, 2);
        assert_eq!(fail::BAD_TOKEN, 3);
        assert_eq!(fail::OVER_BUDGET, 4);
        assert_eq!(fail::MALFORMED_FRAME, 5);
        assert_eq!(fail::FD_PASSING_FAILED, 6);
        assert_eq!(fail::CANNOT_PLACE, 7);
        assert_eq!(fail::INTERNAL, 8);
    }

    #[test]
    fn an_unknown_message_type_is_named_in_the_error() {
        let frame = crate::frame::Frame { msg_type: 0x7777, flags: 0, payload: vec![] };
        assert_eq!(Message::from_frame(&frame).unwrap_err(), WireError::UnknownMsgType(0x7777));
    }

    #[test]
    fn a_pane_state_message_round_trips_through_a_frame() {
        let p = crate::pane::PaneWire {
            pane_uid: 1,
            title: "zsh".into(),
            cols: 80,
            rows: 24,
            child_pid: 2,
            modes: vec![],
            cursor: Default::default(),
            pen: Default::default(),
            style_table: vec![Default::default()],
            screen_primary: Default::default(),
            ..Default::default()
        };
        let m = Message::PaneState(Box::new(p));
        // `tag_names` is populated by the DECODER and left empty by a
        // freshly-built PaneWire, so it must be normalised before comparing —
        // the same convention `assert_round_trips` uses in pane.rs.
        let back = round_trip(&m);
        let mut expected = m;
        if let (Message::PaneState(back_p), Message::PaneState(expected_p)) = (&back, &mut expected) {
            expected_p.tag_names = back_p.tag_names.clone();
        }
        assert_eq!(back, expected);
    }
}
