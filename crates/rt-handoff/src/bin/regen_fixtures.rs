//! Regenerates the golden corpus. Run by hand, never by a test:
//!
//!     cargo run -p rt-handoff --bin regen_fixtures
//!
//! Regenerating after a format change DEFEATS the corpus. The corpus exists to
//! fail when the format changes. If this binary's output differs from what is
//! committed, that difference is the finding — investigate it before you
//! overwrite anything.

use std::io::Write;
use std::path::PathBuf;

use rt_handoff::frame::Frame;
use rt_handoff::testgen::{gen_message, gen_pane, gen_tree, Rng};

fn dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/wire-v1")
}

fn write(name: &str, bytes: &[u8], manifest: &mut Vec<String>) {
    let path = dir().join(name);
    std::fs::write(&path, bytes).unwrap_or_else(|e| panic!("writing {}: {e}", path.display()));
    manifest.push(format!("{name} {}", bytes.len()));
    println!("{name}: {} bytes", bytes.len());
}

fn main() {
    std::fs::create_dir_all(dir()).unwrap();
    let mut manifest = Vec::new();

    // Panes across a spread of shapes, from fixed seeds so the corpus is stable.
    for seed in [1u64, 2, 3, 7, 11, 42, 1234, 99991] {
        let mut r = Rng::new(seed);
        write(&format!("pane_seed{seed:05}.bin"), &gen_pane(&mut r).encode(), &mut manifest);
    }

    // Trees, including a tab group and a deep split chain.
    for seed in [5u64, 50, 500] {
        let mut r = Rng::new(seed);
        write(&format!("tree_seed{seed:05}.bin"), &gen_tree(&mut r, 5).encode(), &mut manifest);
    }

    // One framed message of each kind we can reach from the generator.
    for seed in 1..=20u64 {
        let mut r = Rng::new(seed * 7919);
        let msg = gen_message(&mut r);
        let mut wire = Vec::new();
        msg.to_frame().unwrap().encode(&mut wire).unwrap();
        write(&format!("msg_seed{:05}.bin", seed * 7919), &wire, &mut manifest);
    }

    // A frame at an awkward boundary: an empty payload.
    let mut wire = Vec::new();
    Frame { msg_type: rt_handoff::msg::msg_type::BYE, flags: 0, payload: vec![] }
        .encode(&mut wire)
        .unwrap();
    write("msg_empty_bye.bin", &wire, &mut manifest);

    manifest.sort();
    let mut f = std::fs::File::create(dir().join("MANIFEST.txt")).unwrap();
    for line in &manifest {
        writeln!(f, "{line}").unwrap();
    }
    println!("{} fixtures", manifest.len());
}
