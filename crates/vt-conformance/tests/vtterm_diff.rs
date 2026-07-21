//! Differential test: the in-house `vt_term::Term` vs the vendored oracle. Feed both
//! the same bytes, compare the neutral `ScreenState`. This is the Phase-3 correctness
//! engine — every divergence is a bug in the Term (or an intentional, ledgered one).
//!
//! While the Term is young this test uses a curated set of feature-focused scripts
//! (not the full random fuzz yet) so failures point cleanly at a missing behaviour;
//! the random-fuzz sweep and the divergence ledger grow from here.

use vt_conformance::vendored::Vendored;
use vt_conformance::feed_whole;

fn diff(name: &str, input: &[u8]) -> Option<String> {
    let a = feed_whole::<Vendored>(24, 8, input);
    let b = feed_whole::<vt_term::Term>(24, 8, input);
    a.diff(&b).map(|d| format!("[{name}] {d}"))
}

#[test]
fn vt_term_matches_oracle_on_curated_scripts() {
    let scripts: &[(&str, &[u8])] = &[
        ("plain text", b"hello world"),
        ("wrap+newline", b"line one\r\nline two\r\nthird"),
        ("cursor + overwrite", b"abcdef\x1b[1;3HXY"),
        ("erase display 0", b"AAAA\r\nBBBB\r\nCCCC\x1b[2;2H\x1b[0J"),
        ("erase display 1", b"AAAA\r\nBBBB\r\nCCCC\x1b[2;2H\x1b[1J"),
        ("erase line variants", b"ABCDEFGH\x1b[1;4H\x1b[0K\x1b[2;1Hxyz\x1b[2;2H\x1b[1K"),
        ("sgr truecolor + attrs", b"\x1b[1;38;2;10;20;30mA\x1b[0mB\x1b[4;48;5;42mC"),
        ("sgr named colours", b"\x1b[31mR\x1b[32mG\x1b[33mY\x1b[0m\x1b[91mr\x1b[39mD"),
        ("scroll region", b"\x1b[2;4r\x1b[1;1Ha\r\nb\r\nc\r\nd\r\ne\r\nf"),
        ("insert/delete lines", b"1\r\n2\r\n3\r\n4\x1b[2;1H\x1b[L\x1b[3;1H\x1b[M"),
        ("insert/delete chars", b"ABCDEFGH\x1b[1;3H\x1b[2@\x1b[1;6H\x1b[2P"),
        ("index/reverse/nel", b"top\x1bE\x1bDmid\x1bMx"),
        ("autowrap fill", &[b'x'; 40]),
        ("tabs", b"a\tb\tc\td"),
        ("alt screen", b"primary\x1b[?1049h\x1b[2JalT\x1b[?1049l"),
        ("save/restore cursor", b"\x1b[3;3H\x1b7\x1b[8;8Hzz\x1b8Q"),
        // wide chars: placement + spacer, overwrite-spacer clears the glyph, right-edge
        // leading spacer, and a combining mark attached to a base (common, non-edge).
        ("wide glyphs", "hi \u{4f60}\u{597d} \u{1f980}x".as_bytes()),
        ("overwrite wide spacer", "AB\u{4e2d}CD\u{1b}[1;3H\u{1b}[4CZ".as_bytes()),
        ("wide at right edge", "\u{1b}[1;23HAB\u{754c}".as_bytes()),
        ("combining mark", "cafe\u{0301} out".as_bytes()),
    ];
    let mut fails = Vec::new();
    for (name, input) in scripts {
        if let Some(d) = diff(name, input) {
            fails.push(d);
        }
    }
    assert!(fails.is_empty(), "vt-term vs oracle divergences ({}):\n{}", fails.len(), fails.join("\n"));
}
