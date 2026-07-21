//! Throughput benchmark: the in-house `vt_term::Term` vs the vendored
//! `alacritty_terminal::Term`, on representative terminal workloads. Unlike
//! `parser_bench` (which uses a no-op sink to isolate the parser), this drives the FULL
//! Term — parse **and** build the grid: printing, wrapping, scrolling into scrollback,
//! SGR pen, erases, cursor motion. It answers "how fast is our Term vs alacritty's?".
//!
//! Both feed the SAME bytes into a persistent 80×24 Term (created once, reused across the
//! timing loop, so we measure steady-state throughput including scrollback churn — not
//! per-call allocation). A checksum (cursor column) is returned each round so the
//! optimiser can't elide the work.
//!
//! Run (RELEASE is essential):
//!   cargo run --release --example term_bench -p vt-conformance
//!
//! Reports MB/s for each engine and the ratio (own ÷ alacritty; >1.0 = we are faster).
//! Run on x86_64 AND riscv64 (milkv) — the slow board magnifies differences.

use std::hint::black_box;
use std::time::{Duration, Instant};

use alacritty_terminal::event::EventListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi;

const COLS: usize = 80;
const ROWS: usize = 24;

/// No-op event listener (title/bell/query events dropped — none affect grid throughput).
struct Noop;
impl EventListener for Noop {}

/// Dimensions for `Term::new`.
struct Dims;
impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        ROWS
    }
    fn screen_lines(&self) -> usize {
        ROWS
    }
    fn columns(&self) -> usize {
        COLS
    }
}

/// Time-budgeted throughput in MB/s: run until ~`budget` elapsed, counting bytes fed.
fn mbps(buf: &[u8], mut feed: impl FnMut(&[u8]) -> u64) -> f64 {
    black_box(feed(buf)); // warm up
    let budget = Duration::from_millis(700);
    let start = Instant::now();
    let mut bytes = 0u64;
    while start.elapsed() < budget {
        black_box(feed(black_box(buf)));
        bytes += buf.len() as u64;
    }
    bytes as f64 / start.elapsed().as_secs_f64() / 1e6
}

/// Repeat `pattern` until the buffer is at least `target` bytes.
fn grow(pattern: &[u8], target: usize) -> Vec<u8> {
    let mut v = Vec::with_capacity(target + pattern.len());
    while v.len() < target {
        v.extend_from_slice(pattern);
    }
    v
}

/// Drive the vendored alacritty Term (a persistent Term + ANSI Processor).
fn bench_alac(buf: &[u8]) -> f64 {
    let config = Config { scrolling_history: 10_000, ..Config::default() };
    let mut term = Term::new(config, &Dims, Noop);
    let mut proc: ansi::Processor = ansi::Processor::new();
    mbps(buf, move |b| {
        proc.advance(&mut term, b);
        term.grid().cursor.point.column.0 as u64
    })
}

/// Drive the in-house vt-term Term (a persistent Term).
fn bench_own(buf: &[u8]) -> f64 {
    let mut term = vt_term::Term::new(COLS, ROWS);
    mbps(buf, move |b| {
        term.feed(b);
        term.cursor().0 as u64
    })
}

fn main() {
    const SIZE: usize = 4 << 20; // 4 MiB per workload
    let mut workloads: Vec<(&str, Vec<u8>)> = vec![
        ("plain ascii", grow(b"The quick brown fox jumps over the lazy dog. 0123456789\n", SIZE)),
        ("unicode text", grow("héllo 你好世界 🦀 wörld café ™ ελληνικά — dash\n".as_bytes(), SIZE)),
        ("sgr heavy", grow(b"\x1b[38;5;196mERR\x1b[0m \x1b[1;32mOK\x1b[0m \x1b[3;4;33mwarn\x1b[0m \x1b[38;2;10;20;30mrgb\x1b[0m\n", SIZE)),
        ("control heavy", grow(b"\x1b[2;5Hx\x1b[3;1H\r\ty\x1b[H\x1b[Kz\x08\x08w\n", SIZE)),
        ("mixed tui", grow(b"\x1b[?1049h\x1b[2J\x1b[1;1H\x1b[44;37m top \x1b[0m\x1b[3;3H\xe2\x94\x8c\xe2\x94\x80\xe2\x94\x90 data \x1b[10;1Hmore text here 123\n", SIZE)),
    ];
    let spiral = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("corpus/spiral_stress.bytes");
    if let Ok(bytes) = std::fs::read(&spiral) {
        workloads.push(("spiral (real)", grow(&bytes, SIZE)));
    }

    let arch = std::env::consts::ARCH;
    println!("Term throughput — {arch}  (80x24, 4 MiB workloads, higher MB/s is better)\n");
    println!("{:<16} {:>12} {:>12} {:>10}", "workload", "alac MB/s", "own MB/s", "own/alac");
    println!("{}", "-".repeat(54));
    let mut ratios = Vec::new();
    for (name, buf) in &workloads {
        let a = bench_alac(buf);
        let o = bench_own(buf);
        let r = o / a;
        ratios.push(r);
        println!("{name:<16} {a:>12.0} {o:>12.0} {r:>9.2}x");
    }
    let geo = ratios.iter().map(|r| r.ln()).sum::<f64>() / ratios.len() as f64;
    println!("{}", "-".repeat(54));
    println!("geomean own/alac: {:.2}x", geo.exp());
}
