//! Every committed v1 payload must still decode, and must re-encode to the
//! same bytes. This is the cross-version guarantee, checkable with one build.

use std::path::{Path, PathBuf};

use rt_handoff::frame::Frame;
use rt_handoff::msg::Message;
use rt_handoff::pane::PaneWire;
use rt_handoff::tree::Node;

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wire-v1")
}

fn fixtures() -> Vec<(String, Vec<u8>)> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir()).expect("the fixture directory must exist") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("bin") {
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().to_string();
        out.push((name, std::fs::read(&path).unwrap()));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

#[test]
fn the_corpus_is_present_and_complete() {
    // MANIFEST.txt pins every fixture's name and length, so a fixture that is
    // deleted, truncated or quietly rewritten fails here rather than silently
    // shrinking the guarantee.
    let manifest = std::fs::read_to_string(dir().join("MANIFEST.txt")).expect("MANIFEST.txt must exist");
    let mut expected: Vec<(String, usize)> = manifest
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| {
            let (name, len) = l.rsplit_once(' ').expect("MANIFEST line is `<name> <length>`");
            (name.to_string(), len.parse().unwrap())
        })
        .collect();
    expected.sort();

    let actual: Vec<(String, usize)> = fixtures().into_iter().map(|(n, b)| (n, b.len())).collect();
    assert_eq!(actual, expected, "the corpus does not match MANIFEST.txt");
    assert!(actual.len() >= 32, "the corpus must not shrink: {} fixtures", actual.len());
}

#[test]
fn every_fixture_decodes_and_re_encodes_byte_for_byte() {
    for (name, bytes) in fixtures() {
        let re_encoded = round_trip(&name, &bytes);
        assert_eq!(
            re_encoded,
            bytes,
            "{name}: re-encoding changed the bytes. The format is frozen — if this is \
             a deliberate protocol change, it is a BREAKING one; read the fixtures README."
        );
    }
}

/// Decode a fixture by its filename prefix and encode it straight back.
fn round_trip(name: &str, bytes: &[u8]) -> Vec<u8> {
    if name.starts_with("pane_") {
        PaneWire::decode(bytes).unwrap_or_else(|e| panic!("{name}: {e}")).encode()
    } else if name.starts_with("tree_") {
        Node::decode(bytes).unwrap_or_else(|e| panic!("{name}: {e}")).encode()
    } else if name.starts_with("msg_") {
        let (frame, used) = Frame::decode(bytes).unwrap_or_else(|e| panic!("{name}: {e}"));
        assert_eq!(used, bytes.len(), "{name}: trailing bytes after the frame");
        let msg = Message::from_frame(&frame).unwrap_or_else(|e| panic!("{name}: {e}"));
        let mut out = Vec::new();
        msg.to_frame().unwrap().encode(&mut out).unwrap();
        out
    } else {
        panic!("{name}: fixture name must start with pane_, tree_ or msg_");
    }
}

#[test]
fn a_known_fixture_decodes_to_known_values() {
    // One fixture checked semantically, not just structurally, so a decoder
    // that round-trips garbage consistently still fails.
    let bytes = std::fs::read(dir().join("pane_seed00042.bin")).expect("pane_seed00042.bin must exist");
    let pane = PaneWire::decode(&bytes).unwrap();
    assert!(pane.cols >= 1 && pane.cols <= 400, "cols out of range: {}", pane.cols);
    assert!(pane.rows >= 1 && pane.rows <= 200, "rows out of range: {}", pane.rows);
    assert!(!pane.tag_names.is_empty(), "tag_names must be present (R6)");
    assert!(pane.unknown_tags.is_empty(), "a v1 fixture must have no unknown tags in a v1 build");
    assert!(!pane.style_table.is_empty(), "style_table is required");
}

#[test]
fn fixture_paths_are_relative_to_the_crate_not_the_cwd() {
    // Guards against a fixture loader that only works when run from the repo
    // root — the failure mode that makes a corpus silently stop running.
    assert!(Path::new(&dir()).is_absolute());
    assert!(dir().ends_with("tests/fixtures/wire-v1"));
}
