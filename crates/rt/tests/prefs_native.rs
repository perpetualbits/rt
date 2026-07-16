//! The native preferences dialog — it renders on BOTH backends, and a run of
//! edits commits once.
//!
//! This is the feature made falsifiable twice over:
//!
//! 1. On XRender, the dialog previously could not open AT ALL (`main.rs` force-
//!    closed it, because an invisible egui dialog would swallow every
//!    keystroke). `prefs_dialog_renders_on_xrender_as_commands` gates that: run
//!    with `RT_BACKEND=xrender` + `RT_OPEN_PREFS=1`, the dialog's rows must
//!    appear as `CompositeGlyphs`/`FillRectangles` wire commands, with
//!    `PutImage` staying zero (mechanism C's invariant).
//!
//! 2. A Critical bug shipped and was caught only in human review, not by any
//!    automated check: the dialog rendered fine on XRender but was INVISIBLE on
//!    the GL (default/local) backend, because GL batches draw commands and the
//!    flush ordering differed from the XRender path. Every gate at the time was
//!    XRender-only (`RT_BACKEND=xrender` explicitly set), so nothing exercised
//!    the batched `Renderer`/egui path at all — the bug shipped clean.
//!    `prefs_dialog_renders_on_gl_backend` closes that hole: it runs rt on
//!    Xvfb/llvmpipe with NO `RT_BACKEND` set (so `choose_backend` picks `Gl` for
//!    a local `:N` display, `supports_egui() == true`), screenshots the window,
//!    and counts the dialog's own section-header-blue pixels
//!    (`crate::chrome::prefs::SECTION` = `Color(0.55, 0.72, 0.90, 1.0)` ≈
//!    rgb(140,184,230)). A negative control (same run, no `RT_OPEN_PREFS`)
//!    proves the count actually discriminates rather than matching incidental
//!    pixels — a bare terminal has no blue headers, so it must read ~0.
//!
//! 3. `a_run_of_steps_commits_once` gates commit-on-settle: six rapid `Right`
//!    presses must coalesce into exactly one `prefs settled: … 6 edit(s)
//!    coalesced into 1 commit` log line, not six separate commits (each of
//!    which would re-rasterise every glyph, reflow every pane and write
//!    config.toml).
//!
//! Needs `Xvfb` (+ `xtrace` for the XRender gates; + `xdotool`/`xwd`/`convert`/
//! `python3` with Pillow+numpy for the GL gate) on PATH. Run explicitly:
//!   cargo test -p rt --test prefs_native -- --ignored --nocapture
//!
//! Process hygiene: every helper process is spawned as an owned `Child` and
//! stopped by that exact handle — never by name/pattern — and every display
//! name this file claims (Xvfb's own, xtrace's proxy) is returned via
//! `stop_xvfb`/`release_display_name` so it doesn't poison later runs.
//! `WAYLAND_DISPLAY` is removed from every rt invocation below: if inherited,
//! winit connects to the host's real Wayland compositor and silently bypasses
//! Xvfb entirely, so the test would measure the wrong display (or pop a window
//! on the user's actual screen).

#![cfg(feature = "x11")]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::Duration;

mod common;
use common::{free_display_name, have, release_display_name, start_xvfb_scan, stop_xvfb, wait_for_trace, x_test_lock};

/// A private config so the run does not depend on the host's ~/.config/rt.
fn write_config(tag: &str) -> PathBuf {
    let mut base = std::env::temp_dir();
    base.push(format!("rt_prefs_xdg_{tag}_{}", std::process::id()));
    let rt_dir = base.join("rt");
    std::fs::create_dir_all(&rt_dir).expect("create private XDG_CONFIG_HOME/rt");
    std::fs::write(rt_dir.join("config.toml"), "[settings]\ninst_remote = false\nfont_size = 18.0\n")
        .expect("write config.toml");
    base
}

fn write_shell(tag: &str) -> PathBuf {
    let mut shell = std::env::temp_dir();
    shell.push(format!("rt_prefs_shell_{tag}_{}.sh", std::process::id()));
    let mut f = std::fs::File::create(&shell).expect("create temp shell");
    f.write_all(b"#!/bin/sh\nprintf 'hello\\n'; sleep 120\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    shell
}

#[test]
#[ignore = "needs Xvfb + xtrace; run with --ignored"]
fn prefs_dialog_renders_on_xrender_as_commands() {
    let _serial = x_test_lock(); // Xvfb+rt is heavy: never run these concurrently
    if !have("Xvfb") || !have("xtrace") {
        eprintln!("SKIP prefs_native: needs both `Xvfb` and `xtrace` on PATH");
        return;
    }
    let Some((disp, xvfb)) = start_xvfb_scan(410) else {
        panic!("no Xvfb came up at or after :410");
    };
    let shell = write_shell("render");
    let cfg = write_config("render");
    let trace = std::env::temp_dir().join(format!("rt_prefs_trace_{}.txt", std::process::id()));
    let fake = free_display_name(disp + 1).expect("no free proxy display");
    let rt_bin = env!("CARGO_BIN_EXE_rt");

    let mut child = Command::new("xtrace")
        .arg("-n")
        .args(["-d", &format!(":{disp}")])
        .args(["-D", &format!(":{fake}")])
        .args(["-o", trace.to_str().unwrap(), "--"])
        .arg("timeout")
        .arg("60") // backstop only; we stop it ourselves
        .arg(rt_bin)
        .env_remove("WAYLAND_DISPLAY")
        .env("RT_BACKEND", "xrender") // the backend that had NO preferences at all
        .env("RT_OPEN_PREFS", "1") // test-only: open the dialog at startup
        .env("XDG_CONFIG_HOME", &cfg)
        .env("SHELL", &shell)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn xtrace");

    // Wait for a real paint; never a fixed sleep (cold start is ~3.5s under load).
    let drew = wait_for_trace(&trace, "CompositeGlyphs", Duration::from_secs(30)).is_some();
    if drew {
        std::thread::sleep(Duration::from_millis(400)); // let the frame settle
    }
    let _ = child.kill();
    let _ = child.wait();
    release_display_name(fake);
    stop_xvfb(xvfb, disp);
    let _ = std::fs::remove_file(&shell);
    let _ = std::fs::remove_dir_all(&cfg);

    let dump = std::fs::read_to_string(&trace).unwrap_or_default();
    let _ = std::fs::remove_file(&trace);
    assert!(!dump.is_empty(), "xtrace produced no output — did rt connect to :{disp}?");
    assert!(drew, "rt never rendered text within 30s — a zero-count trace satisfies PutImage==0 vacuously");

    let glyphs = dump.matches("CompositeGlyphs").count();
    let fills = dump.matches("FillRectangles").count();
    let put_image = dump.matches("PutImage").count();
    eprintln!("prefs wire profile: CompositeGlyphs={glyphs} FillRectangles={fills} PutImage={put_image}");

    // The dialog is text + rects. It draws MANY more of both than a bare
    // terminal: ~20 rows of label+value, a panel, a selection bar, swatches.
    assert!(glyphs > 30, "expected the dialog's rows as glyph commands, got {glyphs}");
    assert!(fills > 10, "expected the panel/selection/swatches as fill commands, got {fills}");
    // Mechanism C's invariant still holds for this new surface.
    assert_eq!(put_image, 0, "the dialog must ship commands, not pixels");
}

#[test]
#[ignore = "needs Xvfb + xtrace; run with --ignored"]
fn a_run_of_steps_commits_once() {
    let _serial = x_test_lock();
    if !have("Xvfb") || !have("xtrace") || !have("xdotool") {
        eprintln!("SKIP prefs_native settle: needs Xvfb, xtrace and xdotool on PATH");
        return;
    }
    let Some((disp, xvfb)) = start_xvfb_scan(440) else {
        panic!("no Xvfb came up at or after :440");
    };
    let shell = write_shell("settle");
    let cfg = write_config("settle");
    let log = std::env::temp_dir().join(format!("rt_prefs_settle_{}.log", std::process::id()));
    let rt_bin = env!("CARGO_BIN_EXE_rt");

    let mut child = Command::new(rt_bin)
        .env_remove("WAYLAND_DISPLAY")
        .env("DISPLAY", format!(":{disp}"))
        .env("RUST_LOG", "rt=info") // the settle line we assert on
        .env("RT_BACKEND", "xrender")
        .env("RT_OPEN_PREFS", "1")
        .env("XDG_CONFIG_HOME", &cfg)
        .env("SHELL", &shell)
        .stdout(Stdio::from(std::fs::File::create(&log).unwrap()))
        .stderr(Stdio::from(std::fs::File::create(&log).unwrap()))
        .spawn()
        .expect("spawn rt");

    // Wait for the window, then focus it: with no WM, XTEST goes to the focused
    // window and `xdotool type --window` (XSendEvent) is ignored by winit.
    let mut win = String::new();
    for _ in 0..60 {
        let out = Command::new("xdotool")
            .args(["search", "--onlyvisible", "--name", "^rt$"])
            .env("DISPLAY", format!(":{disp}"))
            .output();
        if let Ok(o) = out {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() {
                win = s.lines().next().unwrap().to_string();
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    assert!(!win.is_empty(), "rt's window never appeared on :{disp}");
    std::thread::sleep(Duration::from_millis(3000)); // cold start + first paint
    let dpy = format!(":{disp}");
    let run = |args: &[&str]| {
        let _ = Command::new("xdotool").args(args).env("DISPLAY", &dpy).status();
    };
    run(&["windowfocus", &win]);
    std::thread::sleep(Duration::from_millis(300));
    // Select "Size (px)" (the first selectable row) and step it SIX times fast.
    for _ in 0..6 {
        run(&["key", "Right"]);
        std::thread::sleep(Duration::from_millis(30)); // key-repeat speed: inside PREFS_SETTLE
    }
    std::thread::sleep(Duration::from_millis(1200)); // let it settle and log

    let _ = child.kill();
    let _ = child.wait();
    stop_xvfb(xvfb, disp);
    let _ = std::fs::remove_file(&shell);
    let _ = std::fs::remove_dir_all(&cfg);

    let text = std::fs::read_to_string(&log).unwrap_or_default();
    let _ = std::fs::remove_file(&log);
    let settles: Vec<&str> = text.lines().filter(|l| l.contains("prefs settled")).collect();
    eprintln!("settle lines:\n{}", settles.join("\n"));
    assert_eq!(
        settles.len(),
        1,
        "6 steps must produce exactly ONE commit (a font commit re-rasterises every \
         glyph, reflows every pane and writes config.toml); got {}:\n{}",
        settles.len(),
        text
    );
    assert!(
        settles[0].contains("6 edit(s) coalesced"),
        "all six steps must fold into the one commit: {}",
        settles[0]
    );
}

/// Count pixels in `png` within `tol` of `target` (r,g,b), via a throwaway
/// python3 + Pillow/numpy one-liner. `None` means the tool chain isn't usable
/// (missing Pillow/numpy, or the PNG doesn't exist) — distinct from "measured
/// zero", so a caller can tell "couldn't measure" from "measured none".
fn count_color_pixels(png: &Path, target: (u8, u8, u8), tol: i32) -> Option<usize> {
    if !png.exists() {
        return None;
    }
    let script = format!(
        "import sys\n\
         import numpy as np\n\
         from PIL import Image\n\
         im = np.asarray(Image.open(sys.argv[1]).convert('RGB'), dtype=np.int16)\n\
         target = np.array([{}, {}, {}], dtype=np.int16)\n\
         mask = np.all(np.abs(im - target) <= {}, axis=-1)\n\
         print(int(mask.sum()))\n",
        target.0, target.1, target.2, tol
    );
    let out = Command::new("python3").arg("-c").arg(&script).arg(png).output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse::<usize>().ok()
}

/// True when `python3 -c "import PIL, numpy"` actually succeeds — distinct from
/// `have("python3")`, which only checks the interpreter itself is on PATH.
fn have_pil_numpy() -> bool {
    Command::new("python3")
        .args(["-c", "import PIL, numpy"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run rt directly on `:disp` (no xtrace — this gate only screenshots), wait
/// for its window, let it paint, `xwd`+`convert` the whole root into a PNG, and
/// tear everything down. `open_prefs` toggles `RT_OPEN_PREFS`; deliberately NO
/// `RT_BACKEND` is set (and any inherited from the test process's own
/// environment is stripped), so `choose_backend` picks `Gl` for this local
/// `:N` display — exactly the path the Critical bug shipped on.
fn run_gl_and_capture(tag: &str, disp_base: u32, open_prefs: bool) -> PathBuf {
    let Some((disp, xvfb)) = start_xvfb_scan(disp_base) else {
        panic!("no Xvfb came up at or after :{disp_base} for run '{tag}'");
    };

    let shell = write_shell(tag);
    let cfg = write_config(tag);
    let png = std::env::temp_dir().join(format!("rt_prefs_gl_shot_{tag}_{}.png", std::process::id()));
    let rt_bin = env!("CARGO_BIN_EXE_rt");

    let mut cmd = Command::new(rt_bin);
    cmd.env_remove("WAYLAND_DISPLAY") // force winit onto Xvfb's X11, never the host's Wayland
        .env_remove("RT_BACKEND") // the whole point: exercise the DEFAULT Gl choice, not an override
        .env("DISPLAY", format!(":{disp}"))
        .env("XDG_CONFIG_HOME", &cfg)
        .env("SHELL", &shell)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if open_prefs {
        cmd.env("RT_OPEN_PREFS", "1");
    } else {
        cmd.env_remove("RT_OPEN_PREFS");
    }
    let mut child = cmd.spawn().expect("spawn rt");

    // Wait for the window (no xtrace here to poll a trace file against), then
    // give it a healthy margin for first paint. Cold start is ~1.5s idle but
    // ~3.5s in a debug build under load — a fixed short sleep here previously
    // caught a blank screen on a loaded machine.
    let mut win_seen = false;
    for _ in 0..60 {
        let out = Command::new("xdotool")
            .args(["search", "--onlyvisible", "--name", "^rt$"])
            .env("DISPLAY", format!(":{disp}"))
            .output();
        if let Ok(o) = out {
            if !String::from_utf8_lossy(&o.stdout).trim().is_empty() {
                win_seen = true;
                break;
            }
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    if !win_seen {
        let _ = child.kill();
        let _ = child.wait();
        stop_xvfb(xvfb, disp);
        let _ = std::fs::remove_file(&shell);
        let _ = std::fs::remove_dir_all(&cfg);
        panic!("[{tag}] rt's window never appeared on :{disp}");
    }
    std::thread::sleep(Duration::from_millis(4000)); // first paint under load

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
    let _ = child.wait();
    stop_xvfb(xvfb, disp);
    let _ = std::fs::remove_file(&shell);
    let _ = std::fs::remove_dir_all(&cfg);

    match xwd_status {
        Ok(s) if s.success() => {}
        other => eprintln!("[{tag}] xwd/convert did not report success: {other:?}"),
    }
    png
}

/// **The gate that would have caught the Critical.** A dialog that renders on
/// XRender proves nothing about the GL/local path: GL batches draw commands,
/// and a flush-ordering bug there shipped a dialog that was invisible on the
/// default backend while every XRender-only check stayed green. This runs rt
/// on Xvfb with llvmpipe software GL, no `RT_BACKEND` override (so
/// `choose_backend` picks `Gl`, `supports_egui() == true`, the batched
/// `Renderer`/egui path Tasks 1-4 actually changed), screenshots the window,
/// and counts pixels matching the dialog's own section-header colour
/// (`crate::chrome::prefs::SECTION` = `Color(0.55, 0.72, 0.90, 1.0)` ≈
/// rgb(140,184,230); the panel background is `Color(0.10, 0.10, 0.12, 0.97)`).
/// With ~5 section headers ("Behaviour", "Colours", ...) rendered as bold
/// glyphs, a healthy run reads hundreds of matching pixels.
///
/// The negative control — the identical run WITHOUT `RT_OPEN_PREFS` — proves
/// the count actually discriminates: a bare terminal showing shell output has
/// no blue headers at all, so it must read close to zero. Without this half,
/// a test that just asserts "count > 0" could be fooled by an unrelated blue
/// pixel somewhere in the window chrome.
#[test]
#[ignore = "needs Xvfb + xdotool + xwd + ImageMagick + python3(Pillow,numpy); run with --ignored"]
fn prefs_dialog_renders_on_gl_backend() {
    let _serial = x_test_lock(); // Xvfb+rt is heavy: never run these concurrently
    if !have("Xvfb") || !have("xdotool") || !have("xwd") || !have("convert") || !have("python3") {
        eprintln!("SKIP prefs_dialog_renders_on_gl_backend: needs Xvfb, xdotool, xwd, convert and python3 on PATH");
        return;
    }
    if !have_pil_numpy() {
        eprintln!("SKIP prefs_dialog_renders_on_gl_backend: needs python3 with Pillow and numpy installed");
        return;
    }

    // Section-header blue, as drawn by `crate::chrome::prefs::SECTION`
    // (`Color(0.55, 0.72, 0.90, 1.0)`): 0.55*255≈140, 0.72*255≈184, 0.90*255≈230.
    const SECTION_BLUE: (u8, u8, u8) = (140, 184, 230);
    const TOL: i32 = 24; // generous enough for AA edges, far from every other chrome colour

    let png_on = run_gl_and_capture("gl_prefs_on", 470, true);
    let png_off = run_gl_and_capture("gl_prefs_off", 520, false);

    let count_on = count_color_pixels(&png_on, SECTION_BLUE, TOL);
    let count_off = count_color_pixels(&png_off, SECTION_BLUE, TOL);
    eprintln!(
        "GL backend section-header-blue pixel count: prefs_open={:?} prefs_closed={:?}",
        count_on, count_off
    );

    let _ = std::fs::remove_file(&png_on);
    let _ = std::fs::remove_file(&png_off);

    let count_on = count_on.expect("could not count pixels for the prefs-open GL screenshot");
    let count_off = count_off.expect("could not count pixels for the prefs-closed GL screenshot");

    // THE gate: with the dialog open on the GL backend, its section headers
    // must actually be on screen. This is the exact assertion that would have
    // failed under the Critical bug (dialog invisible on GL) while every
    // XRender-only check above stayed green.
    assert!(
        count_on > 200,
        "expected hundreds of section-header-blue pixels with the prefs dialog open on GL \
         (~5 headers of bold glyphs), got {count_on} — this is the exact failure mode of the \
         Critical bug: the dialog rendered on XRender but was invisible on GL"
    );
    // The discriminating half: prove the count isn't matching some incidental
    // pixel elsewhere in the window by checking it vanishes with the dialog
    // closed. A bare terminal showing shell output has no blue headers.
    assert!(
        count_off < 50,
        "expected ~0 section-header-blue pixels with the prefs dialog CLOSED (bare terminal), \
         got {count_off} — the colour match isn't discriminating, so `count_on` above proves nothing"
    );
}
