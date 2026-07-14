//! Command-not-pixels regression — the on-point guard for mechanism C.
//!
//! This is the exact `xtrace` measurement that proved rt's remote slowness: on the
//! same "hello" text, rt shipped 2.51 MB (38 `PutImage` pixel blits) while Terminator
//! shipped 48 KB of drawing *commands*. The XRender backend must emit
//! `RenderCompositeGlyphs`/`RenderFillRectangles` and **zero** `PutImage`. If a future
//! change reintroduces pixel transfer on the XRender path, this test fails.
//!
//! Needs `Xvfb` + `xtrace` on PATH. Run it explicitly:
//!   cargo test -p rt --test xrender_commands -- --ignored --nocapture
//! It skips (prints why, passes) if either tool is missing, so `cargo test` is unaffected.
//!
//! Process hygiene: every helper process is spawned as an owned [`Child`] and stopped
//! by that exact handle (`child.kill()`) — never by name/pattern — and the traced rt
//! is bounded by `timeout(1)` so it exits on its own.

#![cfg(feature = "x11")]

use std::io::Write;
use std::path::Path;
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

#[test]
#[ignore = "needs Xvfb + xtrace; run with --ignored"]
fn xrender_emits_commands_not_pixels() {
    if !have("Xvfb") || !have("xtrace") {
        eprintln!("SKIP xrender_commands: needs both `Xvfb` and `xtrace` on PATH");
        return;
    }

    // A display number unlikely to collide with a desktop or a parallel test.
    let disp: u32 = 71 + (std::process::id() % 20);
    let Some(mut xvfb) = start_xvfb(disp) else {
        eprintln!("SKIP xrender_commands: Xvfb :{disp} did not come up");
        return;
    };

    // Give rt a shell that prints known text then idles, so the very first XRender
    // frame contains real glyph runs (no synthetic input needed).
    let mut shell = std::env::temp_dir();
    shell.push(format!("rt_xrender_shell_{}.sh", std::process::id()));
    {
        let mut f = std::fs::File::create(&shell).expect("create temp shell");
        // Print known text, then idle well past rt's startup + first-render time
        // (llvmpipe cold start + GL context + XRender init is ~1.5 s), so the traced
        // frame always contains real glyph runs before `timeout` stops rt.
        f.write_all(b"#!/bin/sh\nprintf 'hello world\\n'; printf 'second line\\n'; sleep 5\n")
            .unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let trace = std::env::temp_dir().join(format!("rt_xrender_trace_{}.txt", std::process::id()));
    let rt_bin = env!("CARGO_BIN_EXE_rt");

    // xtrace connects upstream to Xvfb (`-d :disp`), creates a proxy/fake display
    // (`-D :fake`) whose DISPLAY it hands the child, and dumps every request the
    // child sends. `-n` skips xauth copying (Xvfb here runs without auth). We bound
    // rt with `timeout` so it exits on its own; xtrace exits when the child does.
    let fake = disp + 50;
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
        .env("SHELL", &shell)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    // Tear down the Xvfb we own, and the temp shell, regardless of outcome.
    let _ = xvfb.kill();
    let _ = xvfb.wait();
    let _ = std::fs::remove_file(&shell);

    let status = status.expect("run xtrace");
    // timeout(1) kills rt with SIGTERM → exit 124; that's the normal path here.
    eprintln!("xtrace/rt exited: {status:?}");

    let dump = std::fs::read_to_string(&trace).unwrap_or_default();
    let _ = std::fs::remove_file(&trace);
    assert!(!dump.is_empty(), "xtrace produced no output — did rt connect to :{disp}?");

    // Count the request kinds that matter. xtrace prints RENDER requests by name
    // (e.g. "RenderCompositeGlyphs32", "RenderFillRectangles") and core pixel blits
    // as "PutImage". Substring matching is robust across xtrace naming variants.
    let composite = dump.matches("CompositeGlyphs").count();
    let fills = dump.matches("FillRectangles").count();
    let put_image = dump.matches("PutImage").count();
    let bytes = dump.len();
    eprintln!(
        "xrender wire profile: CompositeGlyphs={composite} FillRectangles={fills} \
         PutImage={put_image} trace_bytes={bytes}"
    );

    // The thesis, made falsifiable:
    assert!(composite > 0, "expected RenderCompositeGlyphs (text as glyph commands), got 0");
    assert!(fills > 0, "expected RenderFillRectangles (backgrounds as fill commands), got 0");
    assert_eq!(
        put_image, 0,
        "XRender path must ship ZERO PutImage pixel blits (rt's old cost was 38 = 2.5 MB)"
    );
}

/// Chrome regression: with the manual overlay open (via the test-only
/// `RT_OPEN_MANUAL=1` startup hook), the native XRender path must still ship
/// zero `PutImage` pixel blits, and the instruments drawing underneath the
/// overlay must still emit RENDER `Triangles` (their AA primitives). This is
/// the on-point guard that chrome + instruments stay commands-only even when
/// an overlay is showing, not just on the bare "hello world" frame.
#[test]
#[ignore = "needs Xvfb + xtrace; run with --ignored"]
fn xrender_chrome_is_commands_not_pixels() {
    if !have("Xvfb") || !have("xtrace") {
        eprintln!("SKIP xrender_chrome_is_commands_not_pixels: needs both `Xvfb` and `xtrace` on PATH");
        return;
    }

    // A different display number than the other test, so the two can run
    // concurrently (or back-to-back with a lingering socket) without colliding.
    let disp: u32 = 111 + (std::process::id() % 20);
    let Some(mut xvfb) = start_xvfb(disp) else {
        eprintln!("SKIP xrender_chrome_is_commands_not_pixels: Xvfb :{disp} did not come up");
        return;
    };

    // Give rt a shell that prints known text then idles, matching the sibling
    // test's timing budget (cold-start + first-render is ~1.5 s).
    let mut shell = std::env::temp_dir();
    shell.push(format!("rt_xrender_chrome_shell_{}.sh", std::process::id()));
    {
        let mut f = std::fs::File::create(&shell).expect("create temp shell");
        f.write_all(b"#!/bin/sh\nprintf 'hello world\\n'; printf 'second line\\n'; sleep 5\n")
            .unwrap();
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    let trace =
        std::env::temp_dir().join(format!("rt_xrender_chrome_trace_{}.txt", std::process::id()));
    let rt_bin = env!("CARGO_BIN_EXE_rt");

    // With the manual open, the instruments animate continuously underneath it,
    // so this trace is far busier than the sibling test's idle-terminal one
    // (tens of MB vs hundreds of KB). Measured directly (owned processes,
    // watched with `ps` mid-run): the inner `timeout` reliably kills `rt`
    // right on schedule, and by then every byte of interest is already
    // written — but `xtrace` itself then does *not* exit on its own here (near
    // -zero CPU afterward), unlike the sibling test's low-volume case where it
    // does. So the outer `timeout` below isn't just a safety net for this
    // test: measured over repeated runs, it is the thing that actually ends
    // `xtrace`, every time, regardless of how large the outer bound is (5 s
    // and 90 s runs produced the same fully-formed trace) — so keep it
    // deliberate rather than open-ended, just comfortably larger than the
    // inner bound so xtrace is reliably reaped after rt exits. `-b` batches
    // xtrace's writes instead of flushing every line, keeping that window's
    // work cheap.
    //
    // The inner bound matches the sibling test's `timeout 3` headroom: rt's
    // cold-start + first-render on llvmpipe is ~1.5 s, and under the default
    // *parallel* `cargo test` invocation (both xrender_commands tests run
    // their Xvfb+xtrace+rt chains concurrently, contending for CPU), that
    // cold start can run measurably slower than in isolation. `1.8` s left
    // only ~0.3 s margin and was observed to flip `triangles` to 0 or
    // truncate the trace under contention; `3` s restores the ~1.5 s margin
    // the sibling test relies on.
    let fake = disp + 50;
    let status = Command::new("timeout")
        .arg("7") // comfortably larger than the inner 3s bound; ends xtrace deliberately
        .arg("xtrace")
        .arg("-n")
        .arg("-b")
        .args(["-d", &format!(":{disp}")])
        .args(["-D", &format!(":{fake}")])
        .args(["-o", trace.to_str().unwrap(), "--"])
        .arg("timeout")
        .arg("3") // > rt's cold-start + first-render even under parallel-test CPU contention
        .arg(rt_bin)
        .env_remove("WAYLAND_DISPLAY") // force winit onto X11, not the host's Wayland
        .env("RT_BACKEND", "xrender") // override detection: exercise the XRender path
        .env("RT_OPEN_MANUAL", "1") // test-only hook: open the manual overlay at startup
        .env("SHELL", &shell)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();

    // Tear down the Xvfb we own, and the temp shell, regardless of outcome.
    let _ = xvfb.kill();
    let _ = xvfb.wait();
    let _ = std::fs::remove_file(&shell);

    let status = status.expect("run xtrace");
    eprintln!("xtrace/rt (chrome) exited: {status:?}");

    let dump = std::fs::read_to_string(&trace).unwrap_or_default();
    let _ = std::fs::remove_file(&trace);
    assert!(!dump.is_empty(), "xtrace produced no output — did rt connect to :{disp}?");

    let put_image = dump.matches("PutImage").count();
    let triangles = dump.matches("Triangles").count();
    let bytes = dump.len();
    eprintln!("chrome wire profile: Triangles={triangles} PutImage={put_image} bytes={bytes}");

    assert!(triangles > 0, "native chrome must emit RENDER Triangles (AA primitives), got 0");
    assert_eq!(put_image, 0, "native chrome must ship ZERO PutImage pixel blits");
}
