//! Reflow-on-resize differential: feed a script, resize, and compare vt-term's rewrapped
//! grid against the vendored oracle. Reflow is the hardest Phase-3 milestone (see
//! `docs/vt-term-design.md`), so this file has two layers:
//!
//! 1. **Curated cases** that must match EXACTLY — the common reflow shapes (grow/shrink
//!    columns with soft-wrap rejoin/split, grow/shrink rows, scrollback interaction, wide
//!    glyphs at the wrap boundary, cursor-at-top). These are locked green.
//! 2. **A fuzz-rate regression guard** — a random-resize sweep whose divergence rate must
//!    stay at or below a ceiling. The residual divergences are the deepest edges (exact
//!    cursor position through reflow, wide-glyph reflow boundaries in reflowed history),
//!    tracked on the divergence ledger; the guard stops them from regressing while they
//!    are driven down.

use vt_conformance::{feed_resize, gen_script, vendored::Vendored, Rng};

/// The curated cases: `(name, start_cols, start_rows, script, end_cols, end_rows)`.
const CASES: &[(&str, usize, usize, &[u8], usize, usize)] = &[
    ("wrap-shrink-cols", 10, 4, b"ABCDEFGHIJKLMNOP", 6, 4),
    ("wrap-grow-cols", 6, 4, b"ABCDEFGHIJKLMNOP", 12, 4),
    ("shrink-rows-drop-bottom", 8, 6, b"AAA\r\nBBB\r\nCCC", 8, 3),
    ("grow-rows-append", 8, 3, b"AAA\r\nBBB\r\nCCC", 8, 6),
    ("full-plus-hist-shrink-cols", 8, 3, b"11111111222222223333333344444444", 5, 3),
    ("both-dims", 20, 6, b"The quick brown fox jumps over the lazy dog and runs", 12, 4),
    ("wide-wrap-shrink", 10, 4, "AB你好CD中EF".as_bytes(), 6, 5),
    ("cursor-at-top-shrink", 8, 4, b"AAAAAAAAAAAA\r\nBBBB\x1b[H", 4, 4),
    ("grow-cols-rejoin-hist", 8, 3, b"111222333444555666", 16, 3),
    ("shrink-both-scrollback", 12, 5, b"one two three four five six seven eight nine ten", 6, 3),
];

#[test]
fn curated_reflow_matches_oracle() {
    for &(name, sc, sr, script, nc, nr) in CASES {
        let a = feed_resize::<Vendored>(sc, sr, script, nc, nr);
        let b = feed_resize::<vt_term::Term>(sc, sr, script, nc, nr);
        assert!(a.diff(&b).is_none(), "{name}: {}", a.diff(&b).unwrap());
    }
}

/// Random-resize sweep: feed a generated script at a random start size, resize to a random
/// end size, diff. The divergence rate must not exceed the ceiling. Lowering this ceiling
/// as the ledgered reflow edges are closed is the metric for reflow's remaining work.
#[test]
fn reflow_fuzz_rate_within_ceiling() {
    const N: u64 = 3000;
    // Current rate ~0.8% (24/3000) after porting alacritty's wide-glyph overwrite cleanup
    // (leading + trailing spacer clears) and adding CHT/CBT (was 5.2%). Guard a little above
    // it so noise doesn't flake, but tight enough to catch a real regression. Drive this
    // DOWN as edges are fixed; never raise it.
    const CEILING: usize = 30; // ~1.0% of N; residual is grow-path wide-glyph + cursor edges (ledger)
    let mut div = 0;
    for seed in 0..N {
        let s = gen_script(seed, 100);
        let mut r = Rng::new(seed ^ 0x9e37);
        let (c2, r2) = (8 + r.below(30) as usize, 4 + r.below(16) as usize);
        let a = feed_resize::<Vendored>(24, 8, &s, c2, r2);
        let b = feed_resize::<vt_term::Term>(24, 8, &s, c2, r2);
        if a.diff(&b).is_some() {
            div += 1;
        }
    }
    assert!(div <= CEILING, "reflow divergence {div}/{N} exceeds ceiling {CEILING} — regression");
}
