//! Headless throughput micro-benchmark for the PTY-parse -> grid hot path.
//!
//! Feeds large, deterministic byte payloads straight into a `Term` through the
//! real `vte` `Processor`, with no window or compositor involved, so the numbers
//! isolate parser + grid-mutation cost and are reproducible run to run.
//!
//! Run with: `cargo run --release -p alacritty_terminal --example throughput`

use std::time::Instant;

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi::{Handler, Processor, StdSyncHandler};
use alacritty_terminal::vte::{Params, Perform};

/// Minimal viewport size for the benchmark terminal.
struct BenchDims {
    columns: usize,
    screen_lines: usize,
}

impl Dimensions for BenchDims {
    // Scrollback is supplied separately via `Config::scrolling_history`, so the
    // dimensions only need to describe the visible viewport.
    fn total_lines(&self) -> usize {
        self.screen_lines
    }

    fn screen_lines(&self) -> usize {
        self.screen_lines
    }

    fn columns(&self) -> usize {
        self.columns
    }
}

/// Build a fresh terminal for each payload so runs do not interfere.
fn make_term() -> Term<VoidListener> {
    let dims = BenchDims { columns: 200, screen_lines: 50 };
    Term::new(Config::default(), &dims, VoidListener)
}

/// Deterministic block of printable ASCII arranged in newline-terminated lines.
///
/// This is the primary signal for the ASCII fast-path: it is almost entirely
/// `print`/`input` calls plus periodic line feeds, with no escape sequences.
fn payload_ascii(total: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(total);
    let mut col = 0usize;
    // Cycle through the printable ASCII range so every byte is a real glyph.
    let mut ch = b'!';
    while out.len() < total {
        out.push(ch);
        ch = if ch >= b'~' { b'!' } else { ch + 1 };
        col += 1;
        if col == 180 {
            out.push(b'\n');
            col = 0;
        }
    }
    out
}

/// Printable ASCII with no control bytes at all; wrapping is driven entirely by
/// the terminal. This is the cleanest before/after signal for the batched
/// `input_run` path versus per-character `input`.
fn payload_ascii_flat(total: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(total);
    let mut ch = b'!';
    while out.len() < total {
        out.push(ch);
        ch = if ch >= b'~' { b'!' } else { ch + 1 };
    }
    out
}

/// `"y\n"` repeated: the pathological short-run case (single-character print
/// runs separated by line feeds), mirroring vtebench's `scrolling` benchmark.
fn payload_yn(total: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(total);
    while out.len() < total {
        out.push(b'y');
        out.push(b'\n');
    }
    out
}

/// Printable ASCII with an SGR color change before every cell.
///
/// Exercises the attribute path (`terminal_attribute`) interleaved with prints,
/// closer to colored program output (e.g. `ls`, build logs).
fn payload_sgr(total: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(total);
    let mut col = 0usize;
    let mut n = 0u8;
    while out.len() < total {
        // 24-bit SGR foreground escape, then one printable char.
        out.extend_from_slice(b"\x1b[38;2;");
        out.extend_from_slice(format!("{};{};{}m", n, n.wrapping_add(85), n.wrapping_add(170)).as_bytes());
        out.push(b'A' + (n % 26));
        n = n.wrapping_add(1);
        col += 1;
        if col == 60 {
            out.push(b'\n');
            col = 0;
        }
    }
    out
}

/// Wide (CJK) characters, to confirm the fast-path change does not regress the
/// double-width path that it must skip.
fn payload_unicode(total: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(total);
    let mut col = 0usize;
    // A handful of wide CJK codepoints, cycled.
    let chars = ['你', '好', '世', '界', '安', '寧', '文', '字'];
    let mut i = 0usize;
    while out.len() < total {
        let mut buf = [0u8; 4];
        out.extend_from_slice(chars[i % chars.len()].encode_utf8(&mut buf).as_bytes());
        i += 1;
        col += 1;
        if col == 90 {
            out.push(b'\n');
            col = 0;
        }
    }
    out
}

/// A `Perform` that does nothing but tally printed chars, to measure the raw
/// `vte` parser + per-char dispatch cost with no grid mutation. The gap between
/// this and the real `Term` numbers is the ceiling any grid-write batching could
/// recover.
#[derive(Default)]
struct NullPerform {
    count: u64,
}

impl Perform for NullPerform {
    fn print(&mut self, c: char) {
        self.count = self.count.wrapping_add(c as u64);
    }
    fn execute(&mut self, _byte: u8) {}
    fn hook(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}
    fn put(&mut self, _: u8) {}
    fn unhook(&mut self) {}
    fn osc_dispatch(&mut self, _: &[&[u8]], _: bool) {}
    fn csi_dispatch(&mut self, _: &Params, _: &[u8], _: bool, _: char) {}
    fn esc_dispatch(&mut self, _: &[u8], _: bool, _: u8) {}
}

/// Measure raw vte dispatch cost (no grid) for a payload.
fn bench_null(name: &str, data: &[u8], iters: usize) {
    let mut samples: Vec<f64> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let mut perform = NullPerform::default();
        let mut parser = alacritty_terminal::vte::Parser::new();
        let start = Instant::now();
        parser.advance(&mut perform, data);
        let secs = start.elapsed().as_secs_f64();
        std::hint::black_box(perform.count);
        samples.push(data.len() as f64 / 1_000_000.0 / secs);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[samples.len() / 2];
    println!("{name:<16} median {median:8.1} MB/s   (vte-only, no grid)");
}

/// Time the pre-optimization per-character path: feed each char through
/// `Handler::input` directly, bypassing both the parser and `input_run`.
fn bench_perchar(name: &str, data: &str, iters: usize) {
    let mut samples: Vec<f64> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let mut term = make_term();
        let start = Instant::now();
        for c in data.chars() {
            Handler::input(&mut term, c);
        }
        let secs = start.elapsed().as_secs_f64();
        std::hint::black_box(term.grid()[Line(0)][Column(0)].c);
        samples.push(data.len() as f64 / 1_000_000.0 / secs);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[samples.len() / 2];
    println!("{name:<16} median {median:8.1} MB/s   (per-char input)");
}

/// Time the batched `input_run` path directly on a UTF-8 payload.
fn bench_run(name: &str, data: &str, iters: usize) {
    let mut samples: Vec<f64> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let mut term = make_term();
        let start = Instant::now();
        term.input_run(data);
        let secs = start.elapsed().as_secs_f64();
        std::hint::black_box(term.grid()[Line(0)][Column(0)].c);
        samples.push(data.len() as f64 / 1_000_000.0 / secs);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[samples.len() / 2];
    let best = samples[samples.len() - 1];
    println!("{name:<16} median {median:8.1} MB/s   best {best:8.1} MB/s   (batched input_run)");
}

/// Time a single payload: parse the whole buffer `iters` times, report MB/s.
fn bench(name: &str, data: &[u8], iters: usize) {
    let mut samples: Vec<f64> = Vec::with_capacity(iters);
    for _ in 0..iters {
        let mut term = make_term();
        let mut parser: Processor<StdSyncHandler> = Processor::new();
        let start = Instant::now();
        parser.advance(&mut term, data);
        let secs = start.elapsed().as_secs_f64();
        // Touch the grid so the optimizer cannot elide the work.
        std::hint::black_box(term.grid()[Line(0)][Column(0)].c);
        samples.push(data.len() as f64 / 1_000_000.0 / secs);
    }
    samples.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let median = samples[samples.len() / 2];
    let best = samples[samples.len() - 1];
    println!("{name:<16} median {median:8.1} MB/s   best {best:8.1} MB/s   ({iters} iters, {} MB each)", data.len() / 1_000_000);
}

fn main() {
    // ~48 MB per payload keeps each sample well above timer noise while staying
    // fast enough to take many iterations.
    let size = 48 * 1_000_000;
    let iters = 9;
    println!("=== alacritty_terminal throughput ({} MB payloads, {iters} iters) ===", size / 1_000_000);
    let ascii = payload_ascii(size);
    let sgr = payload_sgr(size);
    let unicode = payload_unicode(size);
    let yn = payload_yn(size);
    bench("ascii", &ascii, iters);
    bench("sgr_colored", &sgr, iters);
    bench("unicode_wide", &unicode, iters);
    bench("yn_scroll", &yn, iters);
    println!("--- raw vte dispatch ceiling (no grid mutation) ---");
    bench_null("ascii", &ascii, iters);
    bench_null("sgr_colored", &sgr, iters);
    bench_null("unicode_wide", &unicode, iters);

    // Same-run A/B of the optimization itself (both paths bypass the parser, so
    // system load affects them equally). Flat ASCII is the fast-path target;
    // wide CJK is the fallback case, which must not regress.
    let flat = payload_ascii_flat(size);
    let flat_str = std::str::from_utf8(&flat).unwrap();
    let uni_str = std::str::from_utf8(&unicode).unwrap();
    println!("--- flat ASCII: per-char input vs batched input_run ---");
    bench_perchar("ascii_flat", flat_str, iters);
    bench_run("ascii_flat", flat_str, iters);
    println!("--- wide CJK (fallback path must not regress) ---");
    bench_perchar("unicode_wide", uni_str, iters);
    bench_run("unicode_wide", uni_str, iters);
}
