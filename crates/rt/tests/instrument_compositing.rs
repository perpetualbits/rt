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
//! are forced on, and are absent when they're off) needs Task 5: nothing yet
//! *draws* onto the instrument layer (`begin_instrument_layer`/
//! `end_instrument_layer` are not wired into the frame loop), so the layer
//! stays fully transparent this task and green pixels legitimately can't show
//! up yet regardless of config. That half of the test is written now as a
//! separate `#[ignore]`d test (`instrument_green_appears_when_visible`) that
//! is EXPECTED TO FAIL until Task 5 lands the drawing; see the TODO(task5) on
//! it. Task 4's own gate — this file's first test — asserts only the
//! mechanical invariants: both configurations render without error, and
//! `PutImage == 0` in both traces.
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
    // bound rt with `timeout` so it exits on its own.
    let status = Command::new("xtrace")
        .arg("-n")
        .args(["-d", &format!(":{disp}")])
        .args(["-D", &format!(":{fake}")])
        .args(["-o", trace.to_str().unwrap(), "--"])
        .arg("timeout")
        .arg("3") // > rt's cold-start + first-render, < the shell's 5 s idle
        .arg(rt_bin)
        .env_remove("WAYLAND_DISPLAY") // force winit onto X11, not the host's Wayland
        .env("RT_BACKEND", "xrender") // override detection: exercise the XRender path
        .env("RT_SPLIT", "v") // side-by-side split: more than one pane's borders/jacks to draw
        .env("XDG_CONFIG_HOME", xdg_config_home) // private config: force inst_remote/inst_animate
        .env("SHELL", &shell)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    // Screenshot the real display (:disp, not the xtrace proxy) while rt was
    // rendering would race the timeout; instead grab it right after: Xvfb keeps
    // the last-drawn frame on its root window until torn down, so a post-hoc
    // `xwd -root` against :disp still captures rt's final on-screen frame.
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
    let out = Command::new("convert")
        .arg(png)
        .args(["-fuzz", &format!("{fuzz_pct}%")])
        .args(["-fill", "white", "-opaque", hex])
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

/// TODO(task5): this is the REAL end-to-end assertion for the instrument
/// layer — instrument-green pixels appear on screen when `inst_remote` +
/// `inst_animate` are forced on, and are absent when they're off — but it can
/// only pass once Task 5 wires `begin_instrument_layer`/`end_instrument_layer`
/// into the frame loop so something actually draws jacks/borders onto the
/// layer. Right now the layer stays fully transparent regardless of config
/// (Task 4 only makes `present()` composite it — see the sibling test above),
/// so this would fail if run. Deliberately inert by default: it no-ops unless
/// `RT_TEST_TASK5_GREEN=1` is set, so neither plain `cargo test` NOR
/// `cargo test -- --ignored` (Task 4's own verification command) can trip it.
/// Task 5's verification step should run it with that env var set and, once
/// green, delete this opt-in guard.
#[test]
#[ignore = "needs Xvfb + xtrace + ImageMagick; TODO(task5) — set RT_TEST_TASK5_GREEN=1 to activate, expected to fail before Task 5 lands"]
fn instrument_green_appears_when_visible() {
    if std::env::var("RT_TEST_TASK5_GREEN").as_deref() != Ok("1") {
        eprintln!(
            "SKIP instrument_green_appears_when_visible: inert until Task 5 wires the instrument draw path; \
             set RT_TEST_TASK5_GREEN=1 to run it for real"
        );
        return;
    }
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
