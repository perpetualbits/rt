//! What happens when a NEWER rt talks to this one.
//!
//! Every assertion here is a promise to a build that does not exist yet: it may
//! add fields, and we will ignore them without losing anything we did
//! understand, and we will be able to tell the user what we ignored.

use rt_handoff::frame::Frame;
use rt_handoff::msg::{msg_type, Hello, Message};
use rt_handoff::pane::{name_of_tag, tags, PaneWire};
use rt_handoff::testgen::{gen_pane, Rng};
use rt_handoff::tlv::FieldWriter;

/// Rebuild a body from `(tag, bytes)` pairs, keeping ascending order.
fn body_from(mut fields: Vec<(u64, Vec<u8>)>) -> Vec<u8> {
    fields.sort_by_key(|(t, _)| *t);
    let mut fw = FieldWriter::new();
    for (tag, val) in &fields {
        fw.field(*tag, val);
    }
    fw.into_vec()
}

#[test]
fn a_pane_from_a_future_version_decodes_with_every_known_field_intact() {
    for seed in 1..=500u64 {
        let mut r = Rng::new(seed);
        let pane = gen_pane(&mut r);
        let plain = PaneWire::decode(&pane.encode()).unwrap();

        // A future rt adds fields below, between and above ours.
        let mut fields = pane.fields();
        fields.push((0x00, b"a tag below every tag we know".to_vec()));
        fields.push((0x30, b"between the state block and the style table".to_vec()));
        fields.push((0x52, b"just past the image table".to_vec()));
        fields.push((0xDEAD_BEEF, vec![0xAB; 300]));
        let future = PaneWire::decode(&body_from(fields)).unwrap();

        // Everything we understood is identical...
        let mut expected = plain.clone();
        expected.unknown_tags = future.unknown_tags.clone();
        assert_eq!(future, expected, "seed {seed}");

        // ...and we can say exactly what we skipped, in wire order.
        assert_eq!(future.unknown_tags, vec![0x00, 0x30, 0x52, 0xDEAD_BEEF], "seed {seed}");
    }
}

#[test]
fn skipped_tags_can_be_named_from_the_donors_table() {
    // Rule R6: a receiver cannot name a tag it has never heard of, so the
    // donor ships names. Simulate a future field that the donor DID name.
    let mut r = Rng::new(99);
    let pane = gen_pane(&mut r);

    let mut fields = pane.fields();
    // Replace tag_names with one that also names the future field.
    fields.retain(|(t, _)| *t != tags::TAG_NAMES);
    let mut names: Vec<(u64, String)> = pane
        .fields()
        .iter()
        .filter_map(|(t, _)| name_of_tag(*t).map(|n| (*t, n.to_string())))
        .collect();
    names.push((tags::TAG_NAMES, "tag_names".to_string()));
    names.push((0x60, "holographic_cursor".to_string()));
    names.sort_by_key(|(t, _)| *t);

    let nw = {
        // varint count, then (tag, name) pairs — the same shape the encoder writes.
        let mut w = rt_handoff::buf::Writer::new();
        w.varint(names.len() as u64);
        for (t, n) in &names {
            w.varint(*t);
            w.str(n);
        }
        w.into_vec()
    };
    fields.push((tags::TAG_NAMES, nw));
    fields.push((0x60, b"from rt 0.9".to_vec()));

    let back = PaneWire::decode(&body_from(fields)).unwrap();
    assert_eq!(back.unknown_tags, vec![0x60]);

    let named: Vec<&str> = back
        .unknown_tags
        .iter()
        .map(|t| back.tag_names.iter().find(|(nt, _)| nt == t).map(|(_, n)| n.as_str()).unwrap_or("?"))
        .collect();
    assert_eq!(named, vec!["holographic_cursor"], "the donor's table must let us name what we skipped");
}

#[test]
fn a_future_hello_still_yields_its_version_numbers() {
    // The most important forward-compatibility case in the whole protocol: if
    // we cannot read a future Hello, we cannot even print a useful refusal.
    let future = Hello { proto_min: 7, proto_max: 12, rt_version: "1.4.0".into(), ..Hello::default() };
    let mut fields = future.fields();
    fields.push((0x40, b"negotiation extension".to_vec()));
    fields.push((0x99, vec![0; 64]));

    let frame = Frame { msg_type: msg_type::HELLO, flags: 0, payload: body_from(fields) };
    let back = match Message::from_frame(&frame).unwrap() {
        Message::Hello(h) => h,
        other => panic!("wrong message: {other:?}"),
    };
    assert_eq!((back.proto_min, back.proto_max), (7, 12));
    assert_eq!(back.rt_version, "1.4.0");
    assert_eq!(back.unknown_tags, vec![0x40, 0x99]);
}

#[test]
fn unknown_attribute_bits_do_not_leak_into_the_model() {
    // A future version adds attribute bit 20. We must render what we know and
    // silently drop what we do not — never re-emit a bit we cannot describe.
    use rt_handoff::style::{attrs, Style};
    let s = Style { attrs: attrs::BOLD | attrs::STRIKEOUT | (1 << 20) | (1 << 31), ..Style::default() };
    let table = rt_handoff::style::write_table(&[s]);
    let back = rt_handoff::style::read_table(&table).unwrap();
    assert_eq!(back[0].attrs, attrs::BOLD | attrs::STRIKEOUT);
}

#[test]
fn an_unknown_message_type_does_not_poison_the_stream() {
    // A future message type must be skippable: the frame header carries its
    // length, so a receiver can step over it and keep reading.
    let mut wire = Vec::new();
    Frame { msg_type: 0x4242, flags: 0, payload: vec![1; 100] }.encode(&mut wire).unwrap();
    Message::Ping.to_frame().unwrap().encode(&mut wire).unwrap();

    let (unknown, used) = Frame::decode(&wire).unwrap();
    assert!(Message::from_frame(&unknown).is_err(), "we do not know this type");

    let (next, _) = Frame::decode(&wire[used..]).unwrap();
    assert_eq!(Message::from_frame(&next).unwrap(), Message::Ping, "the stream survived");
}
