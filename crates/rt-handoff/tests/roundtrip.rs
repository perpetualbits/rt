//! Encode → decode → compare, over ten thousand generated models.
//!
//! This is the same standard the terminal engine is held to: not "it works on
//! the case I thought of" but "it works on ten thousand cases nobody thought
//! of, and a failure replays from its seed".

use rt_handoff::frame::Frame;
use rt_handoff::msg::Message;
use rt_handoff::pane::PaneWire;
use rt_handoff::testgen::{gen_message, gen_pane, gen_tree, Rng};
use rt_handoff::tree::Node;

#[test]
fn panes_round_trip_over_ten_thousand_seeds() {
    for seed in 1..=10_000u64 {
        let mut r = Rng::new(seed);
        let pane = gen_pane(&mut r);
        let bytes = pane.encode();
        let back = match PaneWire::decode(&bytes) {
            Ok(p) => p,
            Err(e) => panic!("seed {seed}: decode failed: {e}"),
        };
        // tag_names is produced by the encoder and read back by the decoder, so
        // compare against the pane as it comes back rather than as it went in.
        let mut expected = pane.clone();
        expected.tag_names = back.tag_names.clone();
        assert_eq!(back, expected, "seed {seed}");
        assert!(back.unknown_tags.is_empty(), "seed {seed}: our own encoding had unknown tags");
    }
}

#[test]
fn pane_encoding_is_canonical() {
    // Encoding twice must give identical bytes, and re-encoding a decoded pane
    // must reproduce the original. Without this the golden corpus is worthless.
    for seed in 1..=2_000u64 {
        let mut r = Rng::new(seed);
        let pane = gen_pane(&mut r);
        let once = pane.encode();
        assert_eq!(once, pane.encode(), "seed {seed}: encoding is not deterministic");
        let back = PaneWire::decode(&once).unwrap();
        assert_eq!(back.encode(), once, "seed {seed}: re-encoding a decoded pane changed the bytes");
    }
}

#[test]
fn trees_round_trip() {
    for seed in 1..=5_000u64 {
        let mut r = Rng::new(seed);
        let tree = gen_tree(&mut r, 5);
        let back = Node::decode(&tree.encode()).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(back, tree, "seed {seed}");
    }
}

#[test]
fn every_message_round_trips_through_a_frame() {
    for seed in 1..=10_000u64 {
        let mut r = Rng::new(seed);
        let msg = gen_message(&mut r);
        let frame = msg.to_frame().unwrap_or_else(|e| panic!("seed {seed}: {e}"));

        // Also exercise the frame envelope itself, including the byte count.
        let mut wire = Vec::new();
        frame.encode(&mut wire).unwrap();
        let (decoded_frame, used) = Frame::decode(&wire).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        assert_eq!(used, wire.len(), "seed {seed}");

        let back = Message::from_frame(&decoded_frame).unwrap_or_else(|e| panic!("seed {seed}: {e}"));
        let expected = match (&msg, back.clone()) {
            // tag_names round-trips through the encoder, as above.
            (Message::PaneState(orig), Message::PaneState(b)) => {
                let mut e = orig.clone();
                e.tag_names = b.tag_names.clone();
                Message::PaneState(e)
            }
            _ => msg.clone(),
        };
        assert_eq!(back, expected, "seed {seed}");
    }
}

#[test]
fn truncating_any_encoded_pane_errors_rather_than_panics() {
    // A short read must never be a panic: on a socket, truncation is normal.
    for seed in 1..=200u64 {
        let mut r = Rng::new(seed);
        let bytes = gen_pane(&mut r).encode();
        for cut in [1, bytes.len() / 3, bytes.len() / 2, bytes.len() - 1] {
            let _ = PaneWire::decode(&bytes[..cut]); // must return, not panic
        }
    }
}
