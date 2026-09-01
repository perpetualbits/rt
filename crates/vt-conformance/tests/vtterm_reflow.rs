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
//!
//! A third pair of tests below (`RESIZE_FEED_CASES` / `feed_resize_feed_fuzz_rate_within_
//! ceiling`) covers a DIFFERENT shape: resize, then feed AGAIN, then observe. See the doc
//! comment on `feed_resize_feed` for why that shape exists and what `feed_resize` above
//! structurally cannot catch.

use vt_conformance::{feed_resize, feed_resize_feed, gen_script, vendored::Vendored, Rng};

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
#[allow(clippy::absurd_extreme_comparisons)] // see the CEILING note below
fn reflow_fuzz_rate_within_ceiling() {
    const N: u64 = 3000;
    // Now 0/3000 (verified 0/20000 in a wider sweep) after making the wide-glyph overwrite
    // cleanup run at the ACTUAL write position — per-write inside `write_cell`/`write_spacer`
    // like alacritty's `write_at_cursor` — with the leading-spacer clear reaching into
    // scrollback and the fast path deferring wide overwrites (was 24/3000). The ceiling is a
    // strict regression guard: any divergence now fails. Never raise it.
    const CEILING: usize = 0;
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
    // CEILING is a deliberately tunable bound that currently sits at zero, so
    // `<=` is the form we want here. Clippy only sees `x <= 0` on an unsigned
    // type and flags it as always-true; raising CEILING would make it matter.
    assert!(div <= CEILING, "reflow divergence {div}/{N} exceeds ceiling {CEILING} — regression");
}

/// Curated `feed_resize_feed` cases: `(name, start_cols, start_rows, script1, end_cols,
/// end_rows, script2)`. `script1` runs BEFORE the resize, `script2` AFTER — the resize
/// lands strictly between the two feeds, which is the whole point (see `feed_resize_feed`'s
/// doc comment).
///
/// These are the shapes the shipped fix (`Term::swap_alt` reconciling the parked primary
/// screen against current geometry on alt-screen exit) targets, and they are locked green:
/// alt screen entered, the pane resized underneath it (rows only, both directions, and the
/// exact reported 53→54 crash), alt screen exited, then MORE input arrives — which is
/// exactly the read/write that used to panic with `index out of bounds`. Also covered: a
/// pure column-count change during alt (no wrapped content parked, so nothing to rejoin),
/// both dimensions at once, and scrollback already present before the alt screen is
/// entered (row-only resize, so the parked scrollback's width is never in question). What
/// is deliberately NOT here — because it does not match, see `feed_resize_feed_fuzz_rate_
/// within_ceiling` below — is a column-count change with WRAPPED content already parked in
/// the primary (viewport or scrollback): the fix reconciles the parked grid's ROW count and
/// flat-resizes its columns, but never rejoins/resplits wrapped rows the way a live reflow
/// would, so that content stays wrapped at the OLD width. That is a real, known, narrower
/// residual — not the crash, which is fully closed by the cases below.
const RESIZE_FEED_CASES: &[(&str, usize, usize, &[u8], usize, usize, &[u8])] = &[
    // THE crash shape, both directions the row count can move.
    ("alt-exit-row-grow-crash", 20, 10, b"\x1b[?1049hTUI", 20, 14, b"\x1b[?1049l\r\nhi there"),
    ("alt-exit-row-shrink-crash", 20, 10, b"\x1b[?1049hTUI", 20, 4, b"\x1b[?1049l\r\nhi there"),
    // The exact reported dimensions (`len is 53 but the index is 53`), reproduced verbatim.
    ("reported-crash-53-to-54", 80, 53, b"\x1b[?1049h", 80, 54, b"\x1b[?1049l\r\nexit\r\n"),
    // Column-count change during alt, with nothing wrapped parked in the primary.
    ("alt-exit-col-grow", 20, 6, b"\x1b[?1049hTUI", 40, 6, b"\x1b[?1049l\r\nhi there"),
    ("alt-exit-col-shrink", 20, 6, b"\x1b[?1049hTUI", 12, 6, b"\x1b[?1049l\r\nhi there"),
    // Both dimensions at once, both directions.
    ("alt-exit-both-dims-grow", 12, 4, b"\x1b[?1049hTUI", 20, 8, b"\x1b[?1049l\r\nhi there"),
    ("alt-exit-both-dims-shrink", 20, 6, b"\x1b[?1049hTUI", 12, 4, b"\x1b[?1049l\r\nhi there"),
    // Scrollback already exists (non-wrapping lines) before the alt screen is entered.
    (
        "scrollback-before-alt-row-grow",
        20,
        6,
        b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\neight\x1b[?1049hTUI",
        20,
        9,
        b"\x1b[?1049l\r\nhi there",
    ),
    (
        "scrollback-before-alt-col-change",
        20,
        6,
        b"one\r\ntwo\r\nthree\r\nfour\r\nfive\r\nsix\r\nseven\r\neight\x1b[?1049hTUI",
        30,
        6,
        b"\x1b[?1049l\r\nhi there",
    ),
];

#[test]
fn curated_resize_feed_matches_oracle() {
    for &(name, c1, r1, s1, c2, r2, s2) in RESIZE_FEED_CASES {
        let a = feed_resize_feed::<Vendored>(c1, r1, s1, c2, r2, s2);
        let b = feed_resize_feed::<vt_term::Term>(c1, r1, s1, c2, r2, s2);
        assert!(a.diff(&b).is_none(), "{name}: {}", a.diff(&b).unwrap());
    }
}

/// Random resize-then-feed-again sweep: generate a script, spawn at a random size, feed it,
/// resize to a random size, feed a SECOND generated script, diff. `gen_script` already emits
/// `\x1b[?1049h`/`\x1b[?1049l`, so this reaches the alt-screen class on its own — no special
/// casing needed to get there.
///
/// Unlike `reflow_fuzz_rate_within_ceiling`, this is NOT at zero, and is not expected to be:
/// the shipped fix reconciles the parked primary screen's geometry at alt-screen-EXIT time
/// (row count via `shrink_lines`/`grow_lines`, columns via a flat `resize_columns_flat` — no
/// reflow). The oracle reconciles at RESIZE time, across BOTH grids, with reflow on the one
/// that is not currently active (see `alacritty_terminal::Term::resize`, which calls
/// `self.inactive_grid.resize(is_alt, ...)` with `reflow = is_alt` — i.e. reflow IS applied
/// to the parked grid while alt is active). Those are two different reconciliation
/// strategies, and they provably disagree whenever the parked primary has content that was
/// soft-wrapped BEFORE the column count changes: vt-term's flat resize leaves that content
/// wrapped at the width it had when parked (never rejoined/resplit), while the oracle
/// rewraps it live. This reaches further than the "scrollback left at the old width" note in
/// vt-term's `swap_alt` comment — it also hits wrapped rows still in the VISIBLE parked
/// viewport, not just scrollback. Confirmed with a minimized repro (see the divergence
/// report) and with the fuzz-found seed 26 below (`history 26 vs 27` at 17x13 -> 23x13, no
/// scrollback involved beyond the width mismatch itself).
///
/// Measured 282/3000 (seeds 0..3000, `tokens = 100` per script, second script's seed is
/// `seed ^ 0xD1CE_2026`), and 1974/20000 in a wider sweep (both fully deterministic — the
/// seed range is fixed, so these numbers do not vary between runs). CEILING is set to the
/// measured 3000-seed count as a regression guard: it must not climb from here. Driving it
/// DOWN — by reconciling the parked-but-inactive grid's wrapped content at resize time
/// instead of leaving it for a flat fixup at alt-exit — is real remaining reflow work with
/// its own review; it is not this harness's job to do it or to hide the number.
#[test]
fn feed_resize_feed_fuzz_rate_within_ceiling() {
    const N: u64 = 3000;
    const CEILING: usize = 282;
    let mut div = 0;
    for seed in 0..N {
        let s1 = gen_script(seed, 100);
        let s2 = gen_script(seed ^ 0xD1CE_2026, 100);
        let mut r = Rng::new(seed ^ 0x9e37);
        let (c1, r1) = (8 + r.below(30) as usize, 4 + r.below(16) as usize);
        let (c2, r2) = (8 + r.below(30) as usize, 4 + r.below(16) as usize);
        let a = feed_resize_feed::<Vendored>(c1, r1, &s1, c2, r2, &s2);
        let b = feed_resize_feed::<vt_term::Term>(c1, r1, &s1, c2, r2, &s2);
        if a.diff(&b).is_some() {
            div += 1;
        }
    }
    assert!(
        div <= CEILING,
        "resize-then-feed divergence {div}/{N} exceeds ceiling {CEILING} — regression"
    );
}
