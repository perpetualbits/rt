//! Query/report differential: interleave terminal queries (DSR/CPR, DA1/DA2, DECRQM)
//! with normal grid-mutating input, feed BOTH engines the identical stream, and assert
//! their host-bound reply streams match byte-for-byte (DA2 version masked). Ceiling: 0
//! divergences — a strict regression guard, run on x86_64 and riscv64 via ci/verify.sh.
//!
//! Scope note: the query pool is the set of modes vt-term and the oracle represent
//! identically (see docs/superpowers/plans Task 3 design note). Modes where they
//! legitimately differ (ANSI IRM/LNM, private 12/1005/1042, and the 1000/1002/1003 mouse
//! trio) are deliberately excluded until their own coverage slice.

use vt_conformance::{reports_match, VtEngine};

// Queries both engines answer identically. Setters (below) exercise Set/Reset states.
const QUERIES: &[&[u8]] = &[
    b"\x1b[6n",       // CPR
    b"\x1b[5n",       // DSR status
    b"\x1b[c",        // DA1
    b"\x1b[0c",       // DA1 (explicit 0)
    b"\x1b[>c",       // DA2 (version masked)
    b"\x1b[?1$p",     // DECRQM DECCKM
    b"\x1b[?6$p",     // DECRQM DECOM
    b"\x1b[?7$p",     // DECRQM DECAWM
    b"\x1b[?25$p",    // DECRQM DECTCEM
    b"\x1b[?1004$p",  // DECRQM focus events
    b"\x1b[?1006$p",  // DECRQM SGR mouse
    b"\x1b[?1007$p",  // DECRQM alt scroll
    b"\x1b[?2004$p",  // DECRQM bracketed paste
    b"\x1b[?1049$p",  // DECRQM alt screen
    b"\x1b[?2026$p",  // DECRQM sync update (always reset)
    b"\x1b[?3$p",     // DECRQM column mode (not supported)
    b"\x1b[?9999$p",  // DECRQM unknown private
    b"\x1b[99$p",     // DECRQM unknown ANSI
];

// Mode-changing / cursor-moving input, to vary the state the queries observe.
const MUTATORS: &[&[u8]] = &[
    b"\x1b[?6h", b"\x1b[?6l",          // DECOM on/off
    b"\x1b[?7l", b"\x1b[?7h",          // DECAWM off/on
    b"\x1b[?25l", b"\x1b[?25h",        // DECTCEM off/on
    b"\x1b[?1004h", b"\x1b[?1006h",    // focus / sgr mouse on
    b"\x1b[?2004h", b"\x1b[?1049h",    // bracketed paste / alt screen on
    b"\x1b[?1049l",                    // alt screen off
    b"\x1b[5;10H", b"\x1b[H", b"hello", b"\r\n", b"\x1b[2J",
];

// Tiny dependency-free xorshift, seeded per-iteration (matches the crate's RNG style;
// Date/rand are unavailable — determinism is required for reproducibility).
fn next(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13; x ^= x >> 7; x ^= x << 17;
    *state = x; x
}

#[test]
fn query_report_differential_matches_oracle() {
    use vt_conformance::vendored::Vendored;
    let iters: u64 = 4000;
    for seed in 0..iters {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        // Build one script mixing mutators and queries.
        let mut script: Vec<u8> = Vec::new();
        let steps = 4 + (next(&mut rng) % 20) as usize;
        for _ in 0..steps {
            if next(&mut rng) % 2 == 0 {
                script.extend_from_slice(MUTATORS[(next(&mut rng) as usize) % MUTATORS.len()]);
            } else {
                script.extend_from_slice(QUERIES[(next(&mut rng) as usize) % QUERIES.len()]);
            }
        }
        let mut o = Vendored::spawn(80, 24);
        let mut v = <vt_term::Term as VtEngine>::spawn(80, 24);
        o.feed(&script);
        v.feed(&script);
        let (ro, rv) = (o.take_output(), v.take_output());
        assert!(
            reports_match(&ro, &rv),
            "seed {seed}: reply divergence\n script: {:?}\n oracle: {:?}\n vtterm: {:?}",
            String::from_utf8_lossy(&script), ro, rv,
        );
    }
}
