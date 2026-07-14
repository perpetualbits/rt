//! Instrument-layer compositing over content — the on-point guard for Task 4.
//!
//! `present()` now does two things on the XRender path: (1) `CopyArea` the
//! content back-buffer onto the window (as before), then (2) — when
//! `instr_visible` — a RENDER `Composite(OVER)` of the ARGB instrument layer
//! onto the window, on top of that content. Both steps are server-side RENDER
//! requests; neither ships client pixels, so the commands-not-pixels invariant
//! (`PutImage == 0`) established by `xrender_commands.rs` must still hold.
//!
//! Full assertion (instrument-green pixels appear on screen when instruments
//! are forced on, and are absent when they're off) needs Task 5:
//! `begin_instrument_layer`/`end_instrument_layer` are now wired into the
//! frame loop (`main.rs`'s `paint_overlays_or_instruments`, on a 6fps tick
//! decoupled from content frames), so real jack/wire/border geometry lands on
//! the layer and this now passes for real — see
//! `instrument_green_appears_when_visible` below. Task 4's own gate — this
//! file's first test — asserts only the mechanical invariants: both
//! configurations render without error, and `PutImage == 0` in both traces.
//!
//! Needs `Xvfb` + `xtrace` + ImageMagick's `convert` on PATH. Run explicitly:
//!   cargo test -p rt --features x11 --test instrument_compositing -- --ignored --nocapture
//! Skips (prints why, passes) if any tool is missing, so `cargo test` is unaffected.
//!
//! Process hygiene: every helper process is spawned as an owned [`Child`] and
//! stopped by that exact handle (`child.kill()`) — never by name/pattern —
//! and the traced rt is bounded by `timeout(N)` so it exits on its own.

#![cfg(feature = "x11")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// True if `prog` is runnable (resolves on PATH / is executable).
fn have(prog: &str) -> bool {
    Command::new(prog)
        .arg("--help")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| true)
        .unwrap_or(false)
}

/// Spawn `Xvfb :disp` and wait until its unix socket appears (up to ~3 s).
/// Returns the owned child so the caller kills exactly this PID.
fn start_xvfb(disp: u32) -> Option<Child> {
    let child = Command::new("Xvfb")
        .arg(format!(":{disp}"))
        .args(["-screen", "0", "800x600x24", "-nolisten", "tcp"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let sock = format!("/tmp/.X11-unix/X{disp}");
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if Path::new(&sock).exists() {
            return Some(child);
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    // never came up — kill the one we started and give up
    let mut c = child;
    let _ = c.kill();
    let _ = c.wait();
    None
}

/// Write a private `$XDG_CONFIG_HOME/rt/config.toml` forcing the instrument
/// visuals on (`inst_remote`/`inst_animate`) or off, so the run is deterministic
/// regardless of the host's real `~/.config/rt/config.toml`. Returns the
/// `XDG_CONFIG_HOME` directory to point rt at via env.
fn write_config(tag: &str, inst_remote: bool, inst_animate: bool) -> PathBuf {
    let mut base = std::env::temp_dir();
    base.push(format!("rt_instr_xdg_{tag}_{}", std::process::id()));
    let rt_dir = base.join("rt");
    std::fs::create_dir_all(&rt_dir).expect("create private XDG_CONFIG_HOME/rt");
    let toml = format!(
        "[settings]\ninst_remote = {inst_remote}\ninst_animate = {inst_animate}\n\
         inst_output = true\ninst_heat = true\ninst_latency = true\nshow_jacks = true\n"
    );
    std::fs::write(rt_dir.join("config.toml"), toml).expect("write config.toml");
    base
}

/// Give rt a shell that prints known text then idles, so the first XRender
/// frame contains real content (matches the sibling `xrender_commands.rs`
/// timing budget: cold-start + first-render on llvmpipe is ~1.5 s).
fn write_shell(tag: &str) -> PathBuf {
    let mut shell = std::env::temp_dir();
    shell.push(format!("rt_instr_shell_{tag}_{}.sh", std::process::id()));
    let mut f = std::fs::File::create(&shell).expect("create temp shell");
    f.write_all(b"#!/bin/sh\nprintf 'hello world\\n'; printf 'second line\\n'; sleep 5\n")
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    shell
}

/// Run rt once under Xvfb+xtrace with the given XDG_CONFIG_HOME, `RT_SPLIT=v`
/// so there's more than one pane (jacks/borders to draw), capture the window
/// with `xwd` into a PNG, and return `(trace_dump, png_path)`. `disp` must be
/// free; `tag` disambiguates temp file names across the two runs.
fn run_and_capture(tag: &str, disp: u32, xdg_config_home: &Path) -> (String, PathBuf) {
    let Some(mut xvfb) = start_xvfb(disp) else {
        panic!("Xvfb :{disp} did not come up for run '{tag}'");
    };

    let shell = write_shell(tag);
    let trace = std::env::temp_dir().join(format!("rt_instr_trace_{tag}_{}.txt", std::process::id()));
    let png = std::env::temp_dir().join(format!("rt_instr_shot_{tag}_{}.png", std::process::id()));
    let rt_bin = env!("CARGO_BIN_EXE_rt");
    let fake = disp + 50;

    // xtrace connects upstream to Xvfb (`-d :disp`), creates a proxy display
    // (`-D :fake`) whose DISPLAY it hands the child, and dumps every request the
    // child sends. `-n` skips xauth copying (Xvfb here runs without auth). We
    // bound rt with `timeout` as a backstop, but explicitly kill our own spawned
    // child below well before it fires (see the screenshot-timing note).
    let mut child = Command::new("xtrace")
        .arg("-n")
        .args(["-d", &format!(":{disp}")])
        .args(["-D", &format!(":{fake}")])
        .args(["-o", trace.to_str().unwrap(), "--"])
        .arg("timeout")
        .arg("5")
        .arg(rt_bin)
        .env_remove("WAYLAND_DISPLAY") // force winit onto X11, not the host's Wayland
        .env("RT_BACKEND", "xrender") // override detection: exercise the XRender path
        .env("RT_SPLIT", "v") // side-by-side split: more than one pane's borders/jacks to draw
        .env("XDG_CONFIG_HOME", xdg_config_home) // private config: force inst_remote/inst_animate
        .env("SHELL", &shell)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn xtrace");

    // Screenshot the real display (:disp, not the xtrace proxy) WHILE rt is
    // still alive. A post-exit capture is unusable: an X server destroys all
    // resources (including the window) owned by a client's connection the
    // moment that connection closes, so the screen area it occupied reverts to
    // the root background before a post-hoc `xwd -root` can see it (verified:
    // capturing after `wait` on the child produces an all-black PNG). Sleeping
    // past rt's cold-start + first-render (~1.5s) plus a couple of
    // `INSTRUMENT_TICK`s (166ms each) before grabbing the shot guarantees the
    // instrument layer has been drawn at least once.
    std::thread::sleep(Duration::from_millis(2200));
    let xwd_status = Command::new("xwd")
        .args(["-root", "-display", &format!(":{disp}")])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .and_then(|mut xwd| {
            let out = xwd.stdout.take().unwrap();
            let convert = Command::new("convert")
                .arg("xwd:-")
                .arg(&png)
                .stdin(out)
                .stderr(Stdio::null())
                .status();
            let _ = xwd.wait();
            convert
        });

    // Stop exactly the process we spawned (never by name/pattern), then reap it.
    let _ = child.kill();
    let status = child.wait();

    // Tear down the Xvfb we own, and the temp shell, regardless of outcome.
    let _ = xvfb.kill();
    let _ = xvfb.wait();
    let _ = std::fs::remove_file(&shell);

    let status = status.expect("run xtrace");
    eprintln!("[{tag}] xtrace/rt exited: {status:?}; xwd/convert: {xwd_status:?}");

    let dump = std::fs::read_to_string(&trace).unwrap_or_default();
    let _ = std::fs::remove_file(&trace);
    assert!(!dump.is_empty(), "[{tag}] xtrace produced no output — did rt connect to :{disp}?");

    (dump, png)
}

/// Mean brightness (0.0..=1.0) of an ImageMagick "opaque -> white, else black"
/// mask built from `png`, matching color `#rrggbb` within `fuzz_pct`%. A mean
/// of 0.0 means the color is entirely absent; > 0.0 means at least some pixels
/// matched. Returns `None` if `convert` isn't usable or the PNG is missing
/// (e.g. `xwd`/`convert` failed upstream) rather than panicking, so a caller
/// can treat "couldn't measure" distinctly from "measured zero".
fn green_mean(png: &Path, hex: &str, fuzz_pct: u32) -> Option<f64> {
    if !png.exists() {
        return None;
    }
    // NOTE: `-fuzz` stays in effect for every later color op until reset, so
    // the second `+opaque white` (paint everything NOT white to black) must
    // reset it to 0% first — otherwise near-white anti-aliased text survives
    // as light grey ("close enough to white" under the still-active fuzz)
    // instead of being blackened, contaminating the mean with non-green
    // pixels. Verified against a real screenshot: without the reset, an
    // instruments-OFF render (no green drawn at all) still read mean > 0
    // purely from light-grey text edges; with the reset it reads exactly 0.
    let out = Command::new("convert")
        .arg(png)
        .args(["-fuzz", &format!("{fuzz_pct}%")])
        .args(["-fill", "white", "-opaque", hex])
        .args(["-fuzz", "0%"])
        .args(["-fill", "black", "+opaque", "white"])
        .args(["-format", "%[fx:mean]"])
        .arg("info:")
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse::<f64>().ok()
}

/// Task 4's own gate: `present()` composites the instrument layer without
/// error in both configurations, and the XRender path still ships ZERO
/// `PutImage` pixel blits (the layer composite is a RENDER `Composite`
/// request, not a client pixel upload). This does NOT assert instrument-green
/// pixels appear/disappear — nothing draws onto the instrument layer until
/// Task 5 wires `begin_instrument_layer`/`end_instrument_layer` into the frame
/// loop, so the layer is transparent regardless of config this task. That
/// assertion is `instrument_green_appears_when_visible` below, ignored and
/// expected to fail until Task 5 lands.
#[test]
#[ignore = "needs Xvfb + xtrace + ImageMagick; run with --ignored"]
fn instrument_layer_composites_without_pixel_upload() {
    if !have("Xvfb") || !have("xtrace") || !have("convert") || !have("xwd") {
        eprintln!(
            "SKIP instrument_layer_composites_without_pixel_upload: needs Xvfb, xtrace, xwd and convert (ImageMagick) on PATH"
        );
        return;
    }

    let disp_on: u32 = 151 + (std::process::id() % 20);
    let disp_off: u32 = 181 + (std::process::id() % 20);

    let xdg_on = write_config("on", true, true);
    let xdg_off = write_config("off", false, false);

    let (dump_on, _png_on) = run_and_capture("on", disp_on, &xdg_on);
    let (dump_off, _png_off) = run_and_capture("off", disp_off, &xdg_off);

    let put_image_on = dump_on.matches("PutImage").count();
    let put_image_off = dump_off.matches("PutImage").count();
    let composite_on = dump_on.matches("RenderComposite").count() + dump_on.matches("Composite ").count();
    eprintln!(
        "instrument compositing wire profile: PutImage(on)={put_image_on} PutImage(off)={put_image_off} \
         composite-ish-requests(on)={composite_on}"
    );

    // Mechanical invariant #1: rendering both configurations produced real
    // trace output (rt actually connected, drew, and exited cleanly under
    // `timeout`) — i.e. present() with the new composite call didn't error out
    // or hang differently than the plain-CopyArea path.
    assert!(!dump_on.is_empty(), "instruments-ON run produced no trace output");
    assert!(!dump_off.is_empty(), "instruments-OFF run produced no trace output");

    // Mechanical invariant #2 (Task 4's actual gate): the XRender path must
    // still ship ZERO PutImage pixel blits. The instrument-layer composite in
    // present() is a RENDER `Composite(OVER)` — a server-side request — never
    // a client pixel upload, in either instrument configuration.
    assert_eq!(put_image_on, 0, "instruments-ON: XRender path must ship ZERO PutImage pixel blits");
    assert_eq!(put_image_off, 0, "instruments-OFF: XRender path must ship ZERO PutImage pixel blits");

    std::fs::remove_dir_all(xdg_on.parent().unwrap_or(&xdg_on)).ok();
    std::fs::remove_dir_all(xdg_off.parent().unwrap_or(&xdg_off)).ok();
}

/// The REAL end-to-end assertion for the instrument layer — instrument-green
/// pixels appear on screen when `inst_remote` + `inst_animate` are forced on,
/// and are absent when they're off. Now that Task 5 wires
/// `begin_instrument_layer`/`end_instrument_layer` into the frame loop (a
/// 6fps tick in `paint_overlays_or_instruments`, decoupled from content
/// frames), real jack/wire/border geometry lands on the layer and this
/// passes for real.
#[test]
#[ignore = "needs Xvfb + xtrace + ImageMagick; run with --ignored"]
fn instrument_green_appears_when_visible() {
    if !have("Xvfb") || !have("xtrace") || !have("convert") || !have("xwd") {
        eprintln!("SKIP instrument_green_appears_when_visible: needs Xvfb, xtrace, xwd and convert (ImageMagick) on PATH");
        return;
    }

    let disp_on: u32 = 211 + (std::process::id() % 20);
    let disp_off: u32 = 241 + (std::process::id() % 20);

    let xdg_on = write_config("green_on", true, true);
    let xdg_off = write_config("green_off", false, false);

    let (_dump_on, png_on) = run_and_capture("green_on", disp_on, &xdg_on);
    let (_dump_off, png_off) = run_and_capture("green_off", disp_off, &xdg_off);

    // rt's border-instrument "stdout activity" green, as drawn by the GL/egui
    // path today (crates/rt/src/main.rs: `(0x40, 0xc0, 0x54)` a.k.a. `#40c054`).
    let mean_on = green_mean(&png_on, "#40c054", 18).unwrap_or(0.0);
    let mean_off = green_mean(&png_off, "#40c054", 18).unwrap_or(0.0);
    eprintln!("green mean: on={mean_on} off={mean_off}");

    std::fs::remove_dir_all(xdg_on.parent().unwrap_or(&xdg_on)).ok();
    std::fs::remove_dir_all(xdg_off.parent().unwrap_or(&xdg_off)).ok();

    assert!(mean_on > 0.0, "green mean: expected instrument-green pixels with inst_remote=true, found none (mean={mean_on})");
    assert_eq!(mean_off, 0.0, "green mean: expected NO instrument-green pixels with inst_remote=false, found some (mean={mean_off})");
}

/// A shell that keeps one CPU core spinning forever and prints NOTHING. Feeds
/// the heat instrument (`/proc` CPU sampling, sampled independently of any
/// pane output) without ever touching stdout, so the pane's on-screen text
/// stays static while rt still has a reason to keep the instrument layer
/// ticking (a live heat border). This is the "silent" side of the decoupling
/// guard below: visually quiet, but not so inert that the animation state
/// starves — see the big comment on `instrument_ticks_decoupled_from_output`
/// for why literal silence (a `sleep`-only shell) doesn't exercise anything.
fn write_busy_silent_shell(tag: &str) -> PathBuf {
    let mut shell = std::env::temp_dir();
    shell.push(format!("rt_instr_busy_{tag}_{}.sh", std::process::id()));
    let mut f = std::fs::File::create(&shell).expect("create busy shell");
    f.write_all(b"#!/bin/sh\ni=0\nwhile true; do i=$((i+1)); done\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    shell
}

/// A shell that floods stdout as fast as possible, forever: the "content
/// frames" side of the decoupling guard below — the pane scrolls continuously,
/// generating far more `CompositeGlyphs` than the busy-silent shell above.
fn write_flood_shell(tag: &str) -> PathBuf {
    let mut shell = std::env::temp_dir();
    shell.push(format!("rt_instr_flood_{tag}_{}.sh", std::process::id()));
    let mut f = std::fs::File::create(&shell).expect("create flood shell");
    f.write_all(b"#!/bin/sh\nyes 'flood flood flood flood flood filler filler filler filler filler filler filler'\n")
        .unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    shell
}

/// Run rt for exactly `run_secs` under Xvfb+xtrace with the given shell and
/// instruments forced on, returning the raw trace dump. No screenshot here —
/// this guard only counts wire requests, so it skips `xwd`/`convert` entirely.
fn run_traced(tag: &str, disp: u32, xdg_config_home: &Path, shell: &Path, run_secs: u64) -> String {
    let Some(mut xvfb) = start_xvfb(disp) else {
        panic!("Xvfb :{disp} did not come up for run '{tag}'");
    };

    let trace = std::env::temp_dir().join(format!("rt_instr_decouple_trace_{tag}_{}.txt", std::process::id()));
    let rt_bin = env!("CARGO_BIN_EXE_rt");
    let fake = disp + 50;

    let status = Command::new("xtrace")
        .arg("-n")
        .args(["-d", &format!(":{disp}")])
        .args(["-D", &format!(":{fake}")])
        .args(["-o", trace.to_str().unwrap(), "--"])
        .arg("timeout")
        .arg(run_secs.to_string()) // bounds rt; both runs use the same duration
        .arg(rt_bin)
        .env_remove("WAYLAND_DISPLAY") // force winit onto X11, not the host's Wayland
        .env("RT_BACKEND", "xrender") // override detection: exercise the XRender path
        .env("RT_SPLIT", "v") // side-by-side split: more than one pane's borders/jacks to draw
        .env("XDG_CONFIG_HOME", xdg_config_home) // private config: force inst_remote/inst_animate
        .env("SHELL", shell)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    let _ = xvfb.kill();
    let _ = xvfb.wait();

    let status = status.expect("run xtrace");
    eprintln!("[{tag}] xtrace/rt exited: {status:?}");

    let dump = std::fs::read_to_string(&trace).unwrap_or_default();
    let _ = std::fs::remove_file(&trace);
    assert!(!dump.is_empty(), "[{tag}] xtrace produced no output — did rt connect to :{disp}?");
    dump
}

/// The falsifiable decoupling guard: instrument geometry volume tracks
/// wall-clock ticks, not keystroke/output volume. Two equal-length traces,
/// both with instruments forced on (`inst_remote` + `inst_animate`):
///
/// - "silent": a shell that burns CPU forever but prints nothing. The pane's
///   text never changes, but the heat border still has a reason to animate
///   (`/proc` CPU sampling — sampled independently of any pane output, see
///   `sample_heat`), so the instrument layer keeps redrawing at rt's throttled
///   animation cadence (`INSTRUMENT_TICK` = 166ms, further capped to ~2fps by
///   `anim_min` under a software/llvmpipe GL context, which Xvfb always is —
///   see `about_to_wait`). A truly inert shell (just `sleep`) was tried first
///   and rejected: with zero CPU/output/focus/bell activity at all, `anim`
///   never becomes true even once past the first frame (in EITHER the old or
///   new code — this is a pre-existing, unrelated property of the animation
///   gate, not something this task changes), so it only ever measures the
///   single first-show draw and can't distinguish "ticking at a bounded rate"
///   from "frozen" — not a useful baseline for THIS guard.
/// - "flood": a shell that floods stdout as fast as possible forever (`yes`).
///   Massive content-frame volume (`CompositeGlyphs`), on top of the same
///   heat-driven ticking.
///
/// The claim under test: `Triangles` (the instrument-layer geometry — jack
/// circles are glyph-stamped, but the wire/latency-frame strokes are RENDER
/// `Triangles`, and nothing else in this codebase emits `Triangles` — see
/// `chrome/instruments.rs`' `stroke_line`) stays within the same order of
/// magnitude regardless of the output flood, because it is paced by
/// `INSTRUMENT_TICK`'s wall-clock cadence, not by how much content flows.
/// Meanwhile `CompositeGlyphs` (content text) scales hugely with the flood.
/// Before Task 5, the native path redrew instruments on every content-forced
/// full frame — so heavier output (more frequent `dirty`/full-frame activity)
/// would have re-shipped instrument geometry more often, not at a fixed rate.
#[test]
#[ignore = "needs Xvfb + xtrace; run with --ignored"]
fn instrument_ticks_decoupled_from_output() {
    if !have("Xvfb") || !have("xtrace") {
        eprintln!("SKIP instrument_ticks_decoupled_from_output: needs Xvfb and xtrace on PATH");
        return;
    }

    let disp_silent: u32 = 331 + (std::process::id() % 20);
    let disp_flood: u32 = 361 + (std::process::id() % 20);
    const RUN_SECS: u64 = 4; // > cold-start + first-render (~1.5s), several ticks after

    let xdg_silent = write_config("decouple_silent", true, true);
    let xdg_flood = write_config("decouple_flood", true, true);
    let silent_shell = write_busy_silent_shell("decouple");
    let flood_shell = write_flood_shell("decouple");

    let dump_silent = run_traced("decouple_silent", disp_silent, &xdg_silent, &silent_shell, RUN_SECS);
    let dump_flood = run_traced("decouple_flood", disp_flood, &xdg_flood, &flood_shell, RUN_SECS);

    std::fs::remove_file(&silent_shell).ok();
    std::fs::remove_file(&flood_shell).ok();
    std::fs::remove_dir_all(xdg_silent.parent().unwrap_or(&xdg_silent)).ok();
    std::fs::remove_dir_all(xdg_flood.parent().unwrap_or(&xdg_flood)).ok();

    let silent_triangles = dump_silent.matches("Triangles").count();
    let flood_triangles = dump_flood.matches("Triangles").count();
    let silent_glyphs = dump_silent.matches("CompositeGlyphs").count();
    let flood_glyphs = dump_flood.matches("CompositeGlyphs").count();
    eprintln!(
        "decoupling counts: silent_triangles={silent_triangles} flood_triangles={flood_triangles} \
         silent_glyphs={silent_glyphs} flood_glyphs={flood_glyphs}"
    );

    // silent_triangles ≈ flood_triangles (both ~ the same throttled tick rate
    // over the same wall-clock duration), but flood_glyphs >> silent_glyphs.
    assert!(flood_triangles as f64 <= 2.0 * silent_triangles as f64 + 50.0,
        "instrument geometry rode on output: silent={silent_triangles} flood={flood_triangles}");
    assert!(flood_glyphs > silent_glyphs * 3,
        "expected far more text glyphs under output flood: silent={silent_glyphs} flood={flood_glyphs}");
}
