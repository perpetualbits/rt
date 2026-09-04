# macOS Port Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run rt on Apple Silicon macOS with a wgpu/Metal `Backend` and native `NSVisualEffectView` frosted glass, without changing any Linux rendering code path.

**Architecture:** A third `Backend` implementation beside `GlBackend` and `XRenderBackend`, selected on macOS. Platform dependencies move behind target `cfg`s; the Wayland/X11 modules are gated out on macOS and three new modules are gated in. `render.rs`, `gl_backend.rs`, `xrender_backend.rs`, `damage.rs` and `raster.rs` are never edited.

**Tech Stack:** Rust 1.98, winit 0.31.0-beta.2 (appkit backend), wgpu (Metal), objc2 + objc2-app-kit, arboard, WGSL.

**Spec:** `docs/superpowers/specs/2026-09-03-macos-port-design.md`

## Global Constraints

- **Never edit** `crates/rt/src/render.rs`, `gl_backend.rs`, `xrender_backend.rs`, `damage.rs`, `raster.rs`. If a task seems to need it, stop and report.
- The Linux dependency graph must be byte-identical before and after every task (Task 1 adds the check).
- Target `cfg`, never a Cargo feature, for platform selection. macOS is a platform, not a choice.
- Target triple: `aarch64-apple-darwin`. Intel macOS is unclaimed and untested.
- Test host: `ssh kiku` (Apple M5, macOS 26.4, repo at `~/git/rt`). Its IP is location-dependent — `10.13.0.231` at ASTRON only.
- **macOS has no `timeout`.** Never wrap a remote command in it: it exits 127 with no output, which looks exactly like success.
- kiku has no CI. Tasks 4–8 are verified by hand on kiku; Tasks 1–3 are verified in Linux CI.
- Commit after every task. Use the existing commit style (imperative subject, body explaining *why*).

## File Structure

| File | Responsibility | Status |
|---|---|---|
| `crates/rt/Cargo.toml` | target-gated dependency sets | modify |
| `crates/rt/src/main.rs` | `mod` gating + one backend-construction arm | modify (cfg only) |
| `crates/rt/src/backend.rs` | `BackendKind::Wgpu`, platform-parameterised selection | modify |
| `crates/rt/src/clipboard.rs` | `Clipboard::Mac` variant | modify |
| `ci/check-target-deps.sh` | asserts macOS graph is Wayland-free, Linux graph unchanged | create |
| `crates/rt/src/wgpu_backend.rs` | `Backend` impl: surface, present, primitives | create |
| `crates/rt/src/wgpu_text.rs` | glyph atlas, pipeline, WGSL shader | create |
| `crates/rt/src/vibrancy.rs` | `NSVisualEffectView` install + fallback chain | create |

---

### Task 1: Target-gate platform dependencies

Gets `cargo check -p rt` on macOS past its current `smithay-client-toolkit` failure, and adds the guard that keeps it that way.

**Files:**
- Modify: `crates/rt/Cargo.toml`
- Modify: `crates/rt/src/main.rs:13-23` (module declarations)
- Create: `ci/check-target-deps.sh`
- Modify: `.woodpecker/gate.yaml`

**Interfaces:**
- Consumes: nothing.
- Produces: `cfg(target_os = "macos")` is the platform switch every later task keys off. Modules `blur`, `bg_effect`, `x11_blur`, `gl_backend`, `x11_present`, `xrender_backend` are absent on macOS.

- [ ] **Step 1: Write the failing check**

Create `ci/check-target-deps.sh`:

```bash
#!/bin/bash
# Two assertions that need no Mac and no macOS SDK:
#
#   1. The macOS dependency graph contains NO Wayland/X11 crates. This is the
#      regression that will recur -- someone adds an unconditional dep and
#      silently breaks macOS, which nothing else here would notice.
#   2. The Linux graph is unchanged against a committed baseline. This is what
#      makes "the macOS port cannot regress Linux" an assertion rather than a
#      promise.
#
# `cargo tree --target` only RESOLVES the graph, so both run on Linux.
set -uo pipefail
cd "$(dirname "$0")/.."
FAIL=0
BASE=ci/linux-dep-baseline.txt

echo "########## macOS graph: must be free of Wayland/X11 ##########"
mac=$(cargo tree --target aarch64-apple-darwin -p rt --edges normal 2>/dev/null \
      | grep -oE '(wayland|smithay|x11rb|glutin|khronos-egl)[a-z0-9_-]*' | sort -u)
if [ -n "$mac" ]; then
  echo "FAIL: these must not be in the macOS graph:"; echo "$mac" | sed 's/^/  /'
  FAIL=1
else
  echo "OK: no Wayland/X11/glutin crates on macOS"
fi

echo "########## Linux graph: must match the baseline ##########"
lin=$(cargo tree --target x86_64-unknown-linux-gnu -p rt --edges normal 2>/dev/null | sort -u)
if [ ! -f "$BASE" ]; then
  printf '%s\n' "$lin" > "$BASE"
  echo "baseline created at $BASE -- commit it"
elif ! diff -q <(printf '%s\n' "$lin") "$BASE" >/dev/null; then
  echo "FAIL: the Linux dependency graph changed:"
  diff <(printf '%s\n' "$lin") "$BASE" | head -20
  echo "If this change is intended, update $BASE in the same commit."
  FAIL=1
else
  echo "OK: Linux graph unchanged"
fi

[ $FAIL = 0 ] && echo "ALL GREEN" || echo "FAILURES ABOVE"
exit $FAIL
```

- [ ] **Step 2: Run it to confirm it fails**

```bash
chmod +x ci/check-target-deps.sh && ./ci/check-target-deps.sh
```

Expected: the Linux baseline is created (that half passes), and the macOS half FAILS listing 14 crates — `smithay-client-toolkit`, `smithay-clipboard`, `wayland-backend`, `wayland-client`, `wayland-csd-frame`, `wayland-cursor`, `wayland-protocols`, `wayland-protocols-experimental`, `wayland-protocols-misc`, `wayland-protocols-plasma`, `wayland-protocols-wlr`, `wayland-scanner`, `wayland-source`, `wayland-sys` — plus `glutin`, `khronos-egl`, `x11rb`.

- [ ] **Step 3: Move the dependencies behind target cfgs**

In `crates/rt/Cargo.toml`, take `wayland-client`, `wayland-protocols`, `wayland-protocols-plasma`, `smithay-clipboard`, `glutin`, `khronos-egl`, `x11rb` out of `[dependencies]` and add:

```toml
# Wayland/X11 stack: Linux-only by construction. These were unconditional until
# the macOS port; smithay-client-toolkit uses rustix::pipe::pipe_with, which is
# #[cfg(not(apple))] upstream, so merely RESOLVING them on Darwin broke the build.
#
# Target cfg rather than a feature: the `x11` feature is a genuine choice
# (universal vs lean binary), but macOS is not a choice, it is the platform.
# winit needs no gating here -- it already target-gates winit-x11/winit-wayland
# to all(unix, not(target_vendor = "apple")) and selects winit-appkit itself.
[target.'cfg(not(target_os = "macos"))'.dependencies]
glutin = { version = "0.32.2", default-features = false, features = ["egl", "wayland"] }
khronos-egl = { version = "6", features = ["static"] }
wayland-client = "0.31"
wayland-protocols = { version = "0.32", features = ["client", "staging"] }
wayland-protocols-plasma = { version = "0.3", features = ["client"] }
smithay-clipboard = "0.7"
x11rb = { version = "0.13", optional = true }

[target.'cfg(target_os = "macos")'.dependencies]
wgpu = "27"
objc2 = "0.6"
objc2-app-kit = { version = "0.3", features = ["NSView", "NSWindow", "NSVisualEffectView", "NSResponder", "NSColor"] }
objc2-foundation = "0.3"
```

Move `arboard` from optional/`x11`-gated into plain `[dependencies]` — it already supports macOS and is the pasteboard backend there:

```toml
arboard = "3.6"   # X11 AND macOS clipboard; no longer optional
```

Leave the `x11` feature list alone. `glutin/glx` and `dep:x11rb` referencing a target-gated dependency is valid — Cargo resolves features per target, so on macOS they are inert.

- [ ] **Step 4: Gate the modules**

In `crates/rt/src/main.rs`, replace lines 14–23 with:

```rust
#[cfg(not(target_os = "macos"))]
mod blur; // best-effort KDE/KWin background-blur request (no-op elsewhere)
#[cfg(not(target_os = "macos"))]
mod bg_effect; // cross-compositor blur via ext-background-effect-v1 (no-op elsewhere)
mod chrome; // native (XRender) chrome: menu/search/manual/instruments draw + hit-test
#[cfg(not(target_os = "macos"))]
mod gl_backend; // the default GL backend: wraps render.rs's Renderer + present resources
#[cfg(not(target_os = "macos"))]
mod x11_blur; // X11 background blur via _KDE_NET_WM_BLUR_BEHIND_REGION (no-op elsewhere)
#[cfg(all(feature = "x11", not(target_os = "macos")))]
mod x11_present; // Route 1: X11 damage-rect present (glReadPixels + XPutImage)
#[cfg(all(feature = "x11", not(target_os = "macos")))]
mod xrender_backend; // mechanism C: XRender backend
#[cfg(target_os = "macos")]
mod vibrancy; // NSVisualEffectView frosted glass (best-effort, like blur.rs)
#[cfg(target_os = "macos")]
mod wgpu_backend; // the macOS backend: wgpu/Metal
#[cfg(target_os = "macos")]
mod wgpu_text; // glyph atlas + pipeline for wgpu_backend
mod clipboard; // cross-backend clipboard (Wayland smithay / X11 arboard / macOS arboard)
```

Create the three new modules as empty placeholders so this compiles — Tasks 4, 5 and 8 fill them:

```bash
printf '//! Filled in by Task 8.\n' > crates/rt/src/vibrancy.rs
printf '//! Filled in by Task 4.\n' > crates/rt/src/wgpu_backend.rs
printf '//! Filled in by Task 5.\n' > crates/rt/src/wgpu_text.rs
```

Then guard the remaining glutin/wayland use-sites in `main.rs` (6 `use` lines, 22 references) with `#[cfg(not(target_os = "macos"))]`. `main.rs` already has 11 `#[cfg(feature = "x11")]` sites — follow that pattern. The backend-construction block near `main.rs:1190` gets its macOS arm in Task 4; until then wrap the existing block in `#[cfg(not(target_os = "macos"))]` and add `#[cfg(target_os = "macos")] unimplemented!("wgpu backend arrives in Task 4");`.

- [ ] **Step 5: Run the check to verify it passes**

```bash
./ci/check-target-deps.sh
```

Expected: `ALL GREEN` — no Wayland/X11 crates on macOS, Linux graph unchanged.

- [ ] **Step 6: Verify Linux still builds and tests green**

```bash
cargo build -p rt && cargo test -q -p rt-core -p rt-engine -p rt-config -p vt-parser -p vt-term
```

Expected: builds; all suites pass. If the Linux graph check failed in Step 5, you edited a dependency you should not have.

- [ ] **Step 7: Wire the check into CI**

Add to `.woodpecker/gate.yaml`, after the `clippy` step:

```yaml
  - name: target-deps
    image: bash
    environment: *rust
    # Guards the macOS port from Linux CI: no Mac, no macOS SDK needed, because
    # `cargo tree --target` only resolves the graph. Blocking on purpose -- this
    # is the only automated macOS coverage that exists (see the port spec).
    commands:
      - ./ci/check-target-deps.sh
```

- [ ] **Step 8: Commit**

```bash
git add crates/rt/Cargo.toml crates/rt/src/main.rs crates/rt/src/vibrancy.rs \
        crates/rt/src/wgpu_backend.rs crates/rt/src/wgpu_text.rs \
        ci/check-target-deps.sh ci/linux-dep-baseline.txt .woodpecker/gate.yaml
git commit -m "port(macos): target-gate the Wayland/X11 dependency stack

cargo check -p rt died on Darwin inside smithay-client-toolkit, which uses
rustix::pipe::pipe_with -- gated #[cfg(not(apple))] upstream. Every Wayland crate
was an unconditional dependency, so 14 of them were resolved on a platform that
cannot build them.

Target cfg rather than a Cargo feature: the x11 feature is a real choice
(universal vs lean binary); macOS is not a choice, it is the platform, and a
feature would be one more thing to forget to pass.

ci/check-target-deps.sh guards both directions from Linux CI, needing no Mac and
no SDK because cargo tree --target only resolves: the macOS graph must stay
Wayland-free, and the Linux graph must match a committed baseline -- which is
what makes 'this port cannot regress Linux' checkable instead of merely stated."
```

---

### Task 2: `BackendKind::Wgpu` and platform-aware selection

**Files:**
- Modify: `crates/rt/src/backend.rs:126-172`
- Test: `crates/rt/src/backend.rs` (existing `mod tests`)

**Interfaces:**
- Consumes: Task 1's `cfg(target_os = "macos")`.
- Produces: `BackendKind::Wgpu`; `choose_backend(display: Option<&str>, is_x11: bool, override_env: Option<&str>) -> BackendKind` keeps its existing signature, so `main.rs`'s call site is untouched; `choose_backend_on(display, is_x11, override_env, is_macos: bool) -> BackendKind` is the pure core.

- [ ] **Step 1: Write the failing tests**

Add to the `mod tests` block at the end of `crates/rt/src/backend.rs`:

```rust
    #[test]
    fn macos_always_selects_wgpu() {
        // Wayland-shaped and X11-shaped inputs alike: on macOS neither exists.
        assert!(matches!(choose_backend_on(None, false, None, true), BackendKind::Wgpu));
        assert!(matches!(choose_backend_on(Some(":0"), true, None, true), BackendKind::Wgpu));
    }

    #[test]
    fn macos_ignores_the_override() {
        // macOS has exactly one backend, so there is nothing to select between.
        // Honouring "xrender" here would name a module that is cfg'd out.
        assert!(matches!(choose_backend_on(Some(":0"), true, Some("xrender"), true), BackendKind::Wgpu));
        assert!(matches!(choose_backend_on(None, false, Some("gl"), true), BackendKind::Wgpu));
    }

    #[test]
    fn linux_selection_is_unaffected_by_the_macos_arm() {
        assert!(matches!(choose_backend_on(Some(":0"), true, None, false), BackendKind::Gl));
        assert!(matches!(choose_backend_on(Some("localhost:10.0"), true, None, false), BackendKind::XRender));
        assert!(matches!(choose_backend_on(None, false, None, false), BackendKind::Gl));
        assert!(matches!(choose_backend_on(Some(":0"), true, Some("xrender"), false), BackendKind::XRender));
    }
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p rt --bin rt backend:: -- --nocapture`
Expected: FAIL — `cannot find function choose_backend_on`, `no variant named Wgpu`.

- [ ] **Step 3: Implement**

Replace the `BackendKind` enum and `choose_backend` in `crates/rt/src/backend.rs`:

```rust
/// Which [`Backend`] implementation to use. `Gl` is the local-rendering path,
/// `XRender` the remote-friendly one (`ssh -X`), `Wgpu` the macOS Metal path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind {
    Gl,
    XRender,
    Wgpu,
}

/// Pick the backend. See [`choose_backend_on`]; this reads the platform from
/// `cfg!` so callers do not have to. The signature is unchanged from before the
/// macOS port, so `main.rs`'s call site did not move.
pub fn choose_backend(display: Option<&str>, is_x11: bool, override_env: Option<&str>) -> BackendKind {
    choose_backend_on(display, is_x11, override_env, cfg!(target_os = "macos"))
}

/// The pure core, with the platform passed in.
///
/// Split out from [`choose_backend`] so the macOS arm is unit-testable ON LINUX
/// — the Mac is not in CI, and a `cfg!` buried in the body would make this the
/// one selection rule nothing could check.
///
/// Override wins on Linux; else Wayland/non-X11 → Gl; else a DISPLAY with a host
/// part before `:` (TCP / ssh -X forward) → XRender; a bare `:N` (local unix
/// socket) → Gl.
pub fn choose_backend_on(
    display: Option<&str>,
    is_x11: bool,
    override_env: Option<&str>,
    is_macos: bool,
) -> BackendKind {
    // Checked BEFORE the override: macOS builds contain exactly one backend, so
    // there is nothing to switch to, and `RT_BACKEND=xrender` there would select
    // a module that `cfg` removed.
    if is_macos {
        return BackendKind::Wgpu;
    }
    if let Some(o) = override_env {
        return if o.eq_ignore_ascii_case("xrender") { BackendKind::XRender } else { BackendKind::Gl };
    }
    if !is_x11 {
        return BackendKind::Gl;
    }
    match display {
        // "host:N" (host non-empty) is TCP/forwarded; ":N" is a local unix socket.
        Some(d) if d.split(':').next().map_or(false, |h| !h.is_empty()) => BackendKind::XRender,
        _ => BackendKind::Gl,
    }
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p rt --bin rt backend:: -- --nocapture`
Expected: PASS — all seven tests, including the four pre-existing ones.

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/backend.rs
git commit -m "port(macos): add BackendKind::Wgpu and a testable selection core

choose_backend gains a macOS arm, but the platform is passed in rather than read
from cfg! inside the body: choose_backend_on(.., is_macos) is pure, so the macOS
selection rule is unit-tested ON LINUX. kiku is not a CI agent, so a cfg! in the
body would have made this the one selection rule nothing could check.

macOS is decided before the override is consulted. That build contains exactly
one backend, so there is nothing to select between, and RT_BACKEND=xrender would
otherwise name a module that target-gating removed."
```

---

### Task 3: macOS clipboard

**Files:**
- Modify: `crates/rt/src/clipboard.rs`

**Interfaces:**
- Consumes: `arboard` now unconditional (Task 1).
- Produces: `Clipboard::Mac` variant; `Clipboard::from_display` returns it for `RawDisplayHandle::AppKit`.

- [ ] **Step 1: Write the failing test**

Add at the end of `crates/rt/src/clipboard.rs`:

```rust
#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use raw_window_handle::{AppKitDisplayHandle, RawDisplayHandle};

    // Runs on kiku only: constructing an arboard Clipboard needs a real
    // pasteboard, so this cannot be exercised in Linux CI.
    #[test]
    fn appkit_display_selects_the_mac_backend() {
        let h = RawDisplayHandle::AppKit(AppKitDisplayHandle::new());
        assert!(matches!(Clipboard::from_display(h), Some(Clipboard::Mac(_))));
    }

    #[test]
    fn primary_is_a_no_op_and_never_panics() {
        let h = RawDisplayHandle::AppKit(AppKitDisplayHandle::new());
        let c = Clipboard::from_display(h).expect("mac clipboard");
        c.store_primary("ignored".to_string()); // must not panic
        assert!(c.load_primary().is_err());     // macOS has no PRIMARY
    }
}
```

- [ ] **Step 2: Run on kiku to verify it fails**

```bash
ssh kiku 'cd ~/git/rt && cargo test -p rt --bin rt clipboard:: 2>&1 | tail -20'
```

Expected: FAIL — `no variant named Mac`.

- [ ] **Step 3: Implement**

In `crates/rt/src/clipboard.rs`, add the variant, gate the Wayland one, and add the arms:

```rust
pub enum Clipboard {
    /// Wayland: smithay-clipboard, tied to the window's `wl_display`.
    #[cfg(not(target_os = "macos"))]
    Wayland(smithay_clipboard::Clipboard),
    /// X11: arboard (only compiled with the `x11` feature).
    #[cfg(all(feature = "x11", not(target_os = "macos")))]
    X11(X11Clipboard),
    /// macOS: arboard over NSPasteboard.
    #[cfg(target_os = "macos")]
    Mac(MacClipboard),
}
```

In `from_display`, add:

```rust
            #[cfg(target_os = "macos")]
            RawDisplayHandle::AppKit(_) => {
                arboard::Clipboard::new().ok().map(|c| Clipboard::Mac(MacClipboard { inner: std::cell::RefCell::new(c) }))
            }
```

And the backend type, mirroring `X11Clipboard`:

```rust
/// The macOS clipboard backend: an arboard `Clipboard` behind a `RefCell` so its
/// `&mut self` methods are reachable from `&self` (the event loop is
/// single-threaded, so no locking is needed — matching smithay's `&self` shape).
#[cfg(target_os = "macos")]
pub struct MacClipboard {
    inner: std::cell::RefCell<arboard::Clipboard>,
}

#[cfg(target_os = "macos")]
impl MacClipboard {
    fn store(&self, text: String) {
        let _ = self.inner.borrow_mut().set_text(text);
    }
    fn load(&self) -> Result<String, ()> {
        self.inner.borrow_mut().get_text().map_err(|_| ())
    }
}
```

Add the `Mac` arms to `store` and `load`. For `store_primary` and `load_primary`:

```rust
            // macOS has NO PRIMARY selection. This is not a gap to fill later:
            // PRIMARY is an X11 concept, so middle-click paste does not exist on
            // macOS and `load_primary` correctly reports nothing.
            #[cfg(target_os = "macos")]
            Clipboard::Mac(_) => {}            // in store_primary
            #[cfg(target_os = "macos")]
            Clipboard::Mac(_) => Err(()),      // in load_primary
```

- [ ] **Step 4: Run on kiku to verify it passes**

```bash
ssh kiku 'cd ~/git/rt && cargo test -p rt --bin rt clipboard:: 2>&1 | tail -20'
```

Expected: PASS, 2 tests.

- [ ] **Step 5: Verify Linux is unaffected**

```bash
cargo test -q -p rt --bin rt && ./ci/check-target-deps.sh
```

Expected: pass; `ALL GREEN`.

- [ ] **Step 6: Commit**

```bash
git add crates/rt/src/clipboard.rs
git commit -m "port(macos): add the NSPasteboard clipboard variant

Clipboard was already an enum dispatched on RawDisplayHandle, so macOS is a third
variant over arboard, which supports the pasteboard natively.

store_primary/load_primary are deliberate no-ops there. macOS has no PRIMARY
selection -- it is an X11 concept -- so middle-click paste does not exist on the
Mac. Recorded as a platform property rather than a gap, so nobody later 'fixes'
it into existence."
```

---

### Task 4: wgpu backend skeleton — first pixels

Gets an rt window open on macOS painting its background colour. Drawing primitives are stubs until Tasks 5–7.

**Files:**
- Modify: `crates/rt/src/wgpu_backend.rs`
- Modify: `crates/rt/src/main.rs` (backend-construction arm near line 1190)

**Interfaces:**
- Consumes: `BackendKind::Wgpu` (Task 2); `crate::backend::Backend`, `crate::damage::PxRect`, `crate::render::{Color, FontBlobs}`.
- Produces: `WgpuBackend::new(window: Arc<dyn Window>, font_blobs: &FontBlobs, font_px: f32) -> Result<WgpuBackend, String>`; fields `device: wgpu::Device`, `queue: wgpu::Queue`, `surface: wgpu::Surface<'static>`, `config: wgpu::SurfaceConfiguration` used by Task 5.

- [ ] **Step 1: Write the backend skeleton**

Replace `crates/rt/src/wgpu_backend.rs`:

```rust
//! The macOS rendering backend: wgpu on Metal.
//!
//! A third `Backend` beside `GlBackend` (local Linux GL) and `XRenderBackend`
//! (remote `ssh -X`). Neither of those is touched by this file's existence —
//! they are `cfg`'d out on macOS and this one is `cfg`'d out everywhere else.
//!
//! ## Why this backend reports no damage support
//!
//! wgpu exposes neither buffer age nor swap-with-damage, and `CAMetalLayer` has
//! no damage-rect concept, so `partial_present_available()` is `false` and
//! `buffer_age()` is 0 ("unknown → redraw all"). `main.rs` therefore marks damage
//! full every frame here. That is deliberate: damage tracking exists for slow
//! boards and `ssh -X`, and a full terminal-grid redraw on an Apple GPU is
//! negligible. Scissored *drawing* still works (`begin_frame_scissored` maps onto
//! a render-pass scissor rect); only damage-limited *presenting* is unavailable.
use std::num::NonZeroU32;
use std::sync::Arc;

use winit::window::Window;

use crate::backend::Backend;
use crate::damage::PxRect;
use crate::render::{Color, FontBlobs};

pub struct WgpuBackend {
    pub(crate) device: wgpu::Device,
    pub(crate) queue: wgpu::Queue,
    pub(crate) surface: wgpu::Surface<'static>,
    pub(crate) config: wgpu::SurfaceConfiguration,
    /// The frame being built between `begin_frame` and `end_frame`.
    pub(crate) frame: Option<wgpu::SurfaceTexture>,
    pub(crate) encoder: Option<wgpu::CommandEncoder>,
    pub(crate) clear: Color,
    pub(crate) scissor: Option<PxRect>,
    pub(crate) cell_w: f32,
    pub(crate) cell_h: f32,
}

impl WgpuBackend {
    pub fn new(window: Arc<dyn Window>, _font_blobs: &FontBlobs, font_px: f32) -> Result<Self, String> {
        let instance = wgpu::Instance::new(&wgpu::InstanceDescriptor {
            backends: wgpu::Backends::METAL,
            ..Default::default()
        });
        let surface = instance
            .create_surface(window.clone())
            .map_err(|e| format!("wgpu: create_surface failed: {e}"))?;
        let adapter = pollster::block_on(instance.request_adapter(&wgpu::RequestAdapterOptions {
            power_preference: wgpu::PowerPreference::LowPower, // a terminal is not a game
            compatible_surface: Some(&surface),
            force_fallback_adapter: false,
        }))
        .map_err(|e| format!("wgpu: no Metal adapter: {e}"))?;
        let (device, queue) = pollster::block_on(adapter.request_device(&wgpu::DeviceDescriptor {
            label: Some("rt"),
            ..Default::default()
        }))
        .map_err(|e| format!("wgpu: request_device failed: {e}"))?;

        let size = window.surface_size();
        let caps = surface.get_capabilities(&adapter);
        let config = wgpu::SurfaceConfiguration {
            usage: wgpu::TextureUsages::RENDER_ATTACHMENT,
            format: caps.formats[0],
            width: size.width.max(1),
            height: size.height.max(1),
            present_mode: wgpu::PresentMode::AutoVsync,
            // PostMultiplied is what lets the NSVisualEffectView installed in
            // Task 8 show through. With Opaque you get a black window and the
            // frosted glass never appears.
            alpha_mode: if caps.alpha_modes.contains(&wgpu::CompositeAlphaMode::PostMultiplied) {
                wgpu::CompositeAlphaMode::PostMultiplied
            } else {
                caps.alpha_modes[0]
            },
            view_formats: vec![],
            desired_maximum_frame_latency: 2,
        };
        surface.configure(&device, &config);

        Ok(Self {
            device,
            queue,
            surface,
            config,
            frame: None,
            encoder: None,
            clear: Color(0.0, 0.0, 0.0, 1.0),
            scissor: None,
            // Replaced by the real font metrics in Task 5.
            cell_w: font_px * 0.6,
            cell_h: font_px * 1.2,
        })
    }

    /// Begin a frame, clearing to `bg`. `scissor` limits later draws.
    fn start(&mut self, bg: Color, scissor: Option<PxRect>) {
        self.clear = bg;
        self.scissor = scissor;
        let frame = match self.surface.get_current_texture() {
            Ok(f) => f,
            // Lost/outdated surface: reconfigure and skip this frame rather than
            // panicking. The next redraw picks it up.
            Err(_) => {
                self.surface.configure(&self.device, &self.config);
                match self.surface.get_current_texture() {
                    Ok(f) => f,
                    Err(_) => return,
                }
            }
        };
        let view = frame.texture.create_view(&Default::default());
        let mut encoder = self.device.create_command_encoder(&Default::default());
        {
            let _pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("clear"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        // render.rs's Color is already normalised 0..1 AND
                        // carries alpha, so this is a straight widen. The alpha
                        // matters in Task 8: it is what lets the vibrancy show
                        // through, and at 1.0 the frosted glass is invisible.
                        load: wgpu::LoadOp::Clear(wgpu::Color {
                            r: bg.0 as f64, g: bg.1 as f64, b: bg.2 as f64, a: bg.3 as f64,
                        }),
                        store: wgpu::StoreOp::Store,
                    },
                })],
                ..Default::default()
            });
        }
        self.frame = Some(frame);
        self.encoder = Some(encoder);
    }
}

impl Backend for WgpuBackend {
    fn cell_size(&self) -> (f32, f32) { (self.cell_w, self.cell_h) }

    fn resize(&mut self, _w: f32, _h: f32) {}

    fn reload_fonts(&mut self, _blobs: &FontBlobs, _font_px: f32) -> Result<(), String> { Ok(()) }

    fn begin_frame(&mut self, bg: Color) { self.start(bg, None); }
    fn begin_frame_scissored(&mut self, bg: Color, bbox: PxRect) { self.start(bg, Some(bbox)); }
    fn clear_scissor(&mut self) { self.scissor = None; }

    // Drawing primitives arrive in Tasks 5-7. Empty, never `todo!()`: a stub that
    // panics would take the whole window down mid-frame during bring-up.
    fn fill_rect(&mut self, _x: f32, _y: f32, _w: f32, _h: f32, _c: Color) {}
    fn fill_cell(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _color: Color) {}
    fn draw_char(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _ch: char, _fg: Color, _bold: bool, _italic: bool) {}
    fn draw_underline(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _color: Color) {}
    fn draw_strikeout(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _color: Color) {}
    fn cursor_hollow(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _color: Color) {}
    fn cursor_underline(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _color: Color) {}
    fn cursor_beam(&mut self, _ox: f32, _oy: f32, _col: usize, _row: usize, _color: Color) {}
    fn bell_stripe(&mut self, _x: f32, _y: f32, _w: f32, _h: f32) {}

    fn end_frame(&mut self) {
        if let Some(encoder) = self.encoder.take() {
            self.queue.submit(Some(encoder.finish()));
        }
    }

    fn resize_surface(&mut self, w: NonZeroU32, h: NonZeroU32) {
        self.config.width = w.get();
        self.config.height = h.get();
        self.surface.configure(&self.device, &self.config);
    }

    /// `damage` is ignored — see the module header. Always returns `false`
    /// (no full-redraw fallback needed), and is effectively unreachable with
    /// `Some(..)` because the scissored path is gated on
    /// `partial_present_available()`.
    fn present(&mut self, _window: &dyn Window, _damage: Option<(PxRect, &[PxRect])>) -> bool {
        if let Some(frame) = self.frame.take() {
            frame.present();
        }
        false
    }

    fn full_swap(&mut self) {
        if let Some(frame) = self.frame.take() {
            frame.present();
        }
    }

    fn is_software(&self) -> bool { false }
    fn buffer_age(&self) -> u32 { 0 }
    fn partial_present_available(&self) -> bool { false }
    fn x11_present_active(&self) -> bool { false }

    /// `true` despite the name. `is_gl()` is documented as distinguishing ONLY
    /// how instruments are drawn — GL repaints them inline every frame, XRender
    /// keeps a 6fps persistent layer to avoid re-shipping geometry over `ssh -X`.
    /// wgpu wants the inline behaviour. Renaming this to
    /// `repaints_instruments_inline()` would touch both Linux backends and
    /// `main.rs`, breaking this port's "no Linux code path changes" guarantee;
    /// buying naming clarity with Linux churn is a bad trade.
    fn is_gl(&self) -> bool { true }
}
```

- [ ] **Step 2: Add `pollster` and wire the construction arm**

Add to the macOS dependency block in `crates/rt/Cargo.toml`:

```toml
pollster = "0.4"   # block_on for wgpu's async adapter/device request
```

In `crates/rt/src/main.rs`, replace the Task 1 `unimplemented!` with:

```rust
        #[cfg(target_os = "macos")]
        let backend: Box<dyn backend::Backend> = Box::new(
            wgpu_backend::WgpuBackend::new(window.clone(), &font_blobs, settings.font_size)
                .expect("wgpu backend"),
        );
```

- [ ] **Step 3: Build on kiku**

```bash
ssh kiku 'cd ~/git/rt && cargo build -p rt 2>&1 | tail -20'
```

Expected: builds clean. This is the first time `cargo build -p rt` has ever succeeded on macOS.

- [ ] **Step 4: Verify first pixels by hand**

On the Mac itself (not over ssh — this needs a real display), run `~/git/rt/target/debug/rt`.

Expected: a window opens, painted the configured background colour, no text. It will not respond usefully yet. Confirm it does not crash and that resizing does not panic.

- [ ] **Step 5: Verify Linux is untouched**

```bash
cargo build -p rt && ./ci/check-target-deps.sh
```

Expected: builds; `ALL GREEN`.

- [ ] **Step 6: Commit**

```bash
git add crates/rt/src/wgpu_backend.rs crates/rt/src/main.rs crates/rt/Cargo.toml
git commit -m "port(macos): wgpu/Metal backend skeleton -- a window that clears

Surface creation, configuration, clear, and present. Drawing primitives are empty
stubs, deliberately not todo!(): a panicking stub would take the window down
mid-frame during bring-up, when the whole point is watching it come up.

alpha_mode is PostMultiplied where the surface offers it. That is what lets the
NSVisualEffectView from a later task show through; with Opaque the window is
black and the frosted glass never appears.

get_current_texture reconfigures and skips the frame on Lost/Outdated rather than
unwrapping -- a resize must not be fatal."
```

---

### Task 5: Glyph atlas and text pipeline

**Files:**
- Modify: `crates/rt/src/wgpu_text.rs`
- Modify: `crates/rt/src/wgpu_backend.rs` (`draw_char`, `reload_fonts`, `cell_size`)

**Interfaces:**
- Consumes: `WgpuBackend`'s `device`, `queue`, `config` (Task 4).
- Produces: `TextPipeline::new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat, blobs: &FontBlobs, font_px: f32) -> Result<TextPipeline, String>`; `TextPipeline::cell_size(&self) -> (f32, f32)`; `TextPipeline::push_glyph(&mut self, queue: &wgpu::Queue, x: f32, y: f32, ch: char, fg: Color, bold: bool, italic: bool)`; `TextPipeline::push_quad(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color)`; `TextPipeline::flush(&mut self, queue: &wgpu::Queue, pass: &mut wgpu::RenderPass)`; `TextPipeline::set_screen(&mut self, w: f32, h: f32)` (updates the uniform buffer; used by `Backend::resize` in Task 6). Task 5 also adds a `text: TextPipeline` field to `WgpuBackend`, which Tasks 6 and 7 draw through.

`push_quad` is used by Task 6 for solid fills — a quad with a UV pointing at a fully-opaque texel in the atlas, so rects and glyphs share one pipeline and one draw call.

- [ ] **Step 1: Write `wgpu_text.rs`**

The design mirrors `render.rs`, whose header reads: *"One shader. Vertices carry position (pixels), a UV into a single coverage-only (`R8`) atlas texture, and an RGBA colour."* Keep that exactly — same atlas semantics, so output matches the GL backend.

```rust
//! Glyph atlas and text pipeline for the macOS wgpu backend.
//!
//! A direct translation of `render.rs`'s design to wgpu, deliberately: one
//! shader, one coverage-only (`R8Unorm`) atlas texture, vertices carrying
//! position in pixels, a UV into the atlas, and an RGBA colour. Keeping the same
//! model means the Mac draws the same shapes as the GL backend rather than
//! subtly different ones.
//!
//! Texel (0,0) of the atlas is forced to full coverage so solid fills can go
//! through the SAME pipeline as glyphs — one pipeline, one draw call per frame.
use crate::render::{Color, FontBlobs};

pub const ATLAS_SIZE: u32 = 2048;

#[repr(C)]
#[derive(Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Vertex {
    pub pos: [f32; 2],   // pixels, origin top-left
    pub uv: [f32; 2],    // 0..1 into the atlas
    pub color: [f32; 4], // straight RGBA
}

pub struct TextPipeline {
    pipeline: wgpu::RenderPipeline,
    bind_group: wgpu::BindGroup,
    vbuf: wgpu::Buffer,
    ubuf: wgpu::Buffer,
    verts: Vec<Vertex>,
    atlas: wgpu::Texture,
    /// Next free column/row and the current row's height, for the shelf packer.
    shelf_x: u32,
    shelf_y: u32,
    shelf_h: u32,
    /// Cached glyph placements: (char, bold, italic) -> uv rect + pixel offsets.
    glyphs: std::collections::HashMap<(char, bool, bool), Glyph>,
    /// Cached instrument coverage masks (Task 7), sharing the same atlas.
    masks: std::collections::HashMap<MaskKey, [f32; 4]>,
    /// [width, height] in pixels, written into `ubuf` on flush.
    screen: [f32; 2],
    /// The four faces, indexed by `font_for(bold, italic)`. Bold/italic/bold-italic
    /// are `Option` because a font set may not supply them; `font_for` falls back
    /// to regular, matching what render.rs does.
    fonts: (Font, Option<Font>, Option<Font>, Option<Font>),
    font_px: f32,
    cell_w: f32,
    cell_h: f32,
    ascent: f32,
}

#[derive(Clone, Copy)]
struct Glyph {
    u0: f32, v0: f32, u1: f32, v1: f32,
    w: f32, h: f32,
    bearing_x: f32, bearing_y: f32,
}
```

The WGSL shader — one pipeline, alpha-blended, coverage from the atlas modulating the vertex colour:

```rust
const SHADER: &str = r#"
struct Uniforms { screen: vec2<f32> };
@group(0) @binding(0) var<uniform> u: Uniforms;
@group(0) @binding(1) var atlas: texture_2d<f32>;
@group(0) @binding(2) var samp: sampler;

struct VsOut {
  @builtin(position) pos: vec4<f32>,
  @location(0) uv: vec2<f32>,
  @location(1) color: vec4<f32>,
};

@vertex
fn vs(@location(0) pos: vec2<f32>,
      @location(1) uv: vec2<f32>,
      @location(2) color: vec4<f32>) -> VsOut {
  var o: VsOut;
  // Pixels (origin top-left) -> clip space (origin centre, +y up).
  let ndc = vec2<f32>(pos.x / u.screen.x * 2.0 - 1.0,
                      1.0 - pos.y / u.screen.y * 2.0);
  o.pos = vec4<f32>(ndc, 0.0, 1.0);
  o.uv = uv;
  o.color = color;
  return o;
}

@fragment
fn fs(in: VsOut) -> @location(0) vec4<f32> {
  // R8 coverage modulates alpha, exactly as render.rs's GL shader does.
  let cov = textureSample(atlas, samp, in.uv).r;
  return vec4<f32>(in.color.rgb, in.color.a * cov);
}
"#;
```

`render.rs` rasterises with **`fontdue`** (`use fontdue::Font;`, "CPU glyph
rasteriser"), which is pure Rust and has no GL coupling — so this backend uses the
same crate and the same glyph shapes fall out.

```rust
use fontdue::Font;

impl TextPipeline {
    pub fn new(device: &wgpu::Device, queue: &wgpu::Queue, format: wgpu::TextureFormat,
               blobs: &FontBlobs, font_px: f32) -> Result<Self, String> {
        // Mirrors render.rs: take the first blob in each set that fontdue can
        // actually read (some CFF/OTF files it cannot), regular being required.
        let pick = |set: &Vec<Vec<u8>>| -> Option<Font> {
            set.iter().find_map(|b| Font::from_bytes(b.as_slice(), fontdue::FontSettings::default()).ok())
        };
        let regular = pick(&blobs.regular).ok_or("wgpu_text: no usable regular font")?;
        let bold = pick(&blobs.bold);
        let italic = pick(&blobs.italic);
        let bold_italic = pick(&blobs.bold_italic);

        let lm = regular.horizontal_line_metrics(font_px)
            .ok_or("wgpu_text: font has no horizontal line metrics")?;
        let cell_h = (lm.ascent - lm.descent + lm.line_gap).ceil();
        // Monospace: every advance is the same, so 'M' is representative.
        let cell_w = regular.metrics('M', font_px).advance_width.ceil();

        let atlas = device.create_texture(&wgpu::TextureDescriptor {
            label: Some("rt glyph atlas"),
            size: wgpu::Extent3d { width: ATLAS_SIZE, height: ATLAS_SIZE, depth_or_array_layers: 1 },
            mip_level_count: 1, sample_count: 1,
            dimension: wgpu::TextureDimension::D2,
            format: wgpu::TextureFormat::R8Unorm, // coverage only, exactly as render.rs
            usage: wgpu::TextureUsages::TEXTURE_BINDING | wgpu::TextureUsages::COPY_DST,
            view_formats: &[],
        });
        // Texel (0,0) = full coverage, so solid quads ride the glyph pipeline.
        queue.write_texture(
            wgpu::TexelCopyTextureInfo { texture: &atlas, mip_level: 0,
                origin: wgpu::Origin3d::ZERO, aspect: wgpu::TextureAspect::All },
            &[255u8],
            wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(1), rows_per_image: Some(1) },
            wgpu::Extent3d { width: 1, height: 1, depth_or_array_layers: 1 },
        );

        // Pipeline: alpha blending, triangle list, no depth. Vertex layout matches
        // `Vertex` above: vec2 pos, vec2 uv, vec4 colour.
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("rt text"), source: wgpu::ShaderSource::Wgsl(SHADER.into()),
        });
        // ... bind group layout (uniform, texture, sampler), pipeline layout,
        // create_render_pipeline with:
        //     blend: Some(wgpu::BlendState::ALPHA_BLENDING),
        //     topology: wgpu::PrimitiveTopology::TriangleList,
        //     targets: &[Some(wgpu::ColorTargetState { format, .. })]

        Ok(Self { /* pipeline, bind_group, vbuf, ubuf, atlas, ... */
                  verts: Vec::new(), shelf_x: 1, shelf_y: 0, shelf_h: 1,
                  glyphs: Default::default(), masks: Default::default(),
                  screen: [1.0, 1.0], font_px, cell_w, cell_h, ascent: lm.ascent,
                  fonts: (regular, bold, italic, bold_italic) })
    }

    pub fn cell_size(&self) -> (f32, f32) { (self.cell_w, self.cell_h) }

    /// The face for this style, falling back to regular when a set is absent —
    /// the same fallback render.rs uses, so a font pack missing an italic face
    /// renders upright rather than blank.
    fn font_for(&self, bold: bool, italic: bool) -> &Font {
        let (r, b, i, bi) = &self.fonts;
        match (bold, italic) {
            (true, true) => bi.as_ref().or(b.as_ref()).or(i.as_ref()).unwrap_or(r),
            (true, false) => b.as_ref().unwrap_or(r),
            (false, true) => i.as_ref().unwrap_or(r),
            (false, false) => r,
        }
    }

    pub fn set_screen(&mut self, w: f32, h: f32) { self.screen = [w, h]; /* written in flush */ }

    /// Shelf packer: fill the current row left-to-right, start a new row when it
    /// no longer fits. Same scheme render.rs uses; an atlas this size never fills
    /// for a terminal's glyph repertoire.
    fn pack(&mut self, queue: &wgpu::Queue, w: u32, h: u32, data: &[u8]) -> [f32; 4] {
        if self.shelf_x + w > ATLAS_SIZE {
            self.shelf_x = 0;
            self.shelf_y += self.shelf_h;
            self.shelf_h = 0;
        }
        let (x, y) = (self.shelf_x, self.shelf_y);
        self.shelf_x += w;
        self.shelf_h = self.shelf_h.max(h);
        if w > 0 && h > 0 {
            queue.write_texture(
                wgpu::TexelCopyTextureInfo { texture: &self.atlas, mip_level: 0,
                    origin: wgpu::Origin3d { x, y, z: 0 }, aspect: wgpu::TextureAspect::All },
                data,
                wgpu::TexelCopyBufferLayout { offset: 0, bytes_per_row: Some(w), rows_per_image: Some(h) },
                wgpu::Extent3d { width: w, height: h, depth_or_array_layers: 1 },
            );
        }
        let s = ATLAS_SIZE as f32;
        [x as f32 / s, y as f32 / s, (x + w) as f32 / s, (y + h) as f32 / s]
    }

    pub fn push_glyph(&mut self, queue: &wgpu::Queue, x: f32, y: f32, ch: char,
                      fg: Color, bold: bool, italic: bool) {
        let key = (ch, bold, italic);
        let g = match self.glyphs.get(&key) {
            Some(g) => *g,
            None => {
                let font = self.font_for(bold, italic);
                let (m, cov) = font.rasterize(ch, self.font_px);
                let uv = self.pack(queue, m.width as u32, m.height as u32, &cov);
                let g = Glyph { u0: uv[0], v0: uv[1], u1: uv[2], v1: uv[3],
                                w: m.width as f32, h: m.height as f32,
                                bearing_x: m.xmin as f32, bearing_y: m.ymin as f32 };
                self.glyphs.insert(key, g);
                g
            }
        };
        // fontdue's ymin is the offset of the bitmap's BOTTOM from the baseline,
        // and our y grows downward, so the top edge is baseline - (h + ymin).
        let px = x + g.bearing_x;
        let py = y + self.ascent - (g.h + g.bearing_y);
        self.push_uv_quad(px, py, g.w, g.h, [g.u0, g.v0, g.u1, g.v1], fg);
    }

    /// A solid rectangle: same pipeline, UV pinned to the full-coverage texel.
    pub fn push_quad(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        let t = 0.5 / ATLAS_SIZE as f32; // centre of texel (0,0)
        self.push_uv_quad(x, y, w, h, [t, t, t, t], c);
    }

    fn push_uv_quad(&mut self, x: f32, y: f32, w: f32, h: f32, uv: [f32; 4], c: Color) {
        let col = [c.0, c.1, c.2, c.3]; // render.rs Color is already 0..1 RGBA
        let (x0, y0, x1, y1) = (x, y, x + w, y + h);
        let v = |px, py, u, vv| Vertex { pos: [px, py], uv: [u, vv], color: col };
        self.verts.extend_from_slice(&[
            v(x0, y0, uv[0], uv[1]), v(x1, y0, uv[2], uv[1]), v(x1, y1, uv[2], uv[3]),
            v(x0, y0, uv[0], uv[1]), v(x1, y1, uv[2], uv[3]), v(x0, y1, uv[0], uv[3]),
        ]);
    }

    /// Upload this frame's vertices and issue ONE draw call, then reset.
    pub fn flush(&mut self, queue: &wgpu::Queue, pass: &mut wgpu::RenderPass) {
        if self.verts.is_empty() { return; }
        queue.write_buffer(&self.ubuf, 0, bytemuck::cast_slice(&self.screen));
        queue.write_buffer(&self.vbuf, 0, bytemuck::cast_slice(&self.verts));
        pass.set_pipeline(&self.pipeline);
        pass.set_bind_group(0, &self.bind_group, &[]);
        pass.set_vertex_buffer(0, self.vbuf.slice(..));
        pass.draw(0..self.verts.len() as u32, 0..1);
        self.verts.clear();
    }
}
```

`vbuf` needs a capacity; size it for a full screen of cells (`cols * rows * 6`
vertices, times a small factor for chrome) and grow by reallocation if
`verts.len()` ever exceeds it.

- [ ] **Step 2: Add `bytemuck`**

```toml
bytemuck = { version = "1", features = ["derive"] }   # macOS dependency block
```

- [ ] **Step 3: Wire it into the backend**

In `wgpu_backend.rs`, add a `text: TextPipeline` field, build it in `new`, and:

```rust
    fn cell_size(&self) -> (f32, f32) { self.text.cell_size() }

    fn reload_fonts(&mut self, blobs: &FontBlobs, font_px: f32) -> Result<(), String> {
        self.text = crate::wgpu_text::TextPipeline::new(
            &self.device, &self.queue, self.config.format, blobs, font_px)?;
        let (w, h) = self.text.cell_size();
        self.cell_w = w;
        self.cell_h = h;
        Ok(())
    }

    fn draw_char(&mut self, ox: f32, oy: f32, col: usize, row: usize, ch: char,
                 fg: Color, bold: bool, italic: bool) {
        let x = ox + col as f32 * self.cell_w;
        let y = oy + row as f32 * self.cell_h;
        self.text.push_glyph(&self.queue, x, y, ch, fg, bold, italic);
    }

    fn end_frame(&mut self) {
        let (Some(frame), Some(mut encoder)) = (self.frame.as_ref(), self.encoder.take()) else { return };
        let view = frame.texture.create_view(&Default::default());
        {
            let mut pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("text"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view: &view,
                    depth_slice: None,
                    resolve_target: None,
                    // Load, NOT Clear: begin_frame already cleared, and clearing
                    // again here would erase it.
                    ops: wgpu::Operations { load: wgpu::LoadOp::Load, store: wgpu::StoreOp::Store },
                })],
                ..Default::default()
            });
            if let Some(r) = self.scissor {
                pass.set_scissor_rect(r.x as u32, r.y as u32, r.w as u32, r.h as u32);
            }
            self.text.flush(&self.queue, &mut pass);
        }
        self.queue.submit(Some(encoder.finish()));
    }
```

Check `PxRect`'s actual field names in `damage.rs` before writing the
`set_scissor_rect` call — read it, do not edit it.

- [ ] **Step 4: Build and verify text on kiku**

```bash
ssh kiku 'cd ~/git/rt && cargo build -p rt 2>&1 | tail -10'
```

Then on the Mac, run `~/git/rt/target/debug/rt`.

Expected: a shell prompt renders, typing echoes, characters are correctly positioned and legible. Compare glyph shapes against the Linux GL build — they should match, since both drive the same coverage-mask model.

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/wgpu_text.rs crates/rt/src/wgpu_backend.rs crates/rt/Cargo.toml
git commit -m "port(macos): glyph atlas and text pipeline on wgpu

A deliberate translation of render.rs's model rather than a fresh design: one
shader, one coverage-only R8 atlas, vertices carrying pixel position, a UV and an
RGBA colour. Same model means the Mac draws the same shapes as the GL backend
instead of subtly different ones.

Texel (0,0) is forced to full coverage so solid fills share the glyph pipeline --
one pipeline and one draw call for text and rectangles both."
```

---

### Task 6: Remaining drawing primitives

**Files:**
- Modify: `crates/rt/src/wgpu_backend.rs`

**Interfaces:**
- Consumes: `TextPipeline::push_quad` (Task 5).
- Produces: a `Backend` with every non-instrument drawing method implemented.

- [ ] **Step 1: Implement the rect-shaped primitives**

All of these are quads through `push_quad`, so they need no new pipeline:

```rust
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        self.text.push_quad(x, y, w, h, c);
    }

    fn fill_cell(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let (x, y) = (ox + col as f32 * self.cell_w, oy + row as f32 * self.cell_h);
        self.text.push_quad(x, y, self.cell_w, self.cell_h, color);
    }

    fn draw_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let (x, y) = (ox + col as f32 * self.cell_w, oy + row as f32 * self.cell_h);
        let t = (self.cell_h * 0.06).max(1.0);
        self.text.push_quad(x, y + self.cell_h - t * 2.0, self.cell_w, t, color);
    }

    fn draw_strikeout(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let (x, y) = (ox + col as f32 * self.cell_w, oy + row as f32 * self.cell_h);
        let t = (self.cell_h * 0.06).max(1.0);
        self.text.push_quad(x, y + self.cell_h * 0.5, self.cell_w, t, color);
    }

    fn cursor_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let (x, y) = (ox + col as f32 * self.cell_w, oy + row as f32 * self.cell_h);
        let t = (self.cell_h * 0.12).max(2.0);
        self.text.push_quad(x, y + self.cell_h - t, self.cell_w, t, color);
    }

    fn cursor_beam(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let (x, y) = (ox + col as f32 * self.cell_w, oy + row as f32 * self.cell_h);
        let t = (self.cell_w * 0.15).max(2.0);
        self.text.push_quad(x, y, t, self.cell_h, color);
    }

    /// Four thin quads, not a filled cell: the glyph underneath must stay visible.
    fn cursor_hollow(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color) {
        let (x, y) = (ox + col as f32 * self.cell_w, oy + row as f32 * self.cell_h);
        let (w, h) = (self.cell_w, self.cell_h);
        let t = 1.0_f32.max((h * 0.05).floor());
        self.text.push_quad(x, y, w, t, color);             // top
        self.text.push_quad(x, y + h - t, w, t, color);     // bottom
        self.text.push_quad(x, y, t, h, color);             // left
        self.text.push_quad(x + w - t, y, t, h, color);     // right
    }

    fn bell_stripe(&mut self, x: f32, y: f32, w: f32, h: f32) {
        self.text.push_quad(x, y, w, h, Color::rgb(255, 200, 0));
    }
```

- [ ] **Step 2: Implement `resize`**

```rust
    fn resize(&mut self, w: f32, h: f32) {
        self.text.set_screen(w, h);   // updates the uniform buffer's screen size
    }
```

- [ ] **Step 3: Build and verify on kiku**

```bash
ssh kiku 'cd ~/git/rt && cargo build -p rt 2>&1 | tail -10'
```

On the Mac, run rt and check: block/underline/beam cursors all render (cycle them in preferences), selection highlights, the menu and preferences overlays draw, search highlights, split borders appear, and resizing the window reflows without artefacts.

- [ ] **Step 4: Commit**

```bash
git add crates/rt/src/wgpu_backend.rs
git commit -m "port(macos): implement the rect-shaped drawing primitives

Fills, cell backgrounds, underline, strikeout, all three cursor shapes and the
bell stripe. Every one is a quad through the glyph pipeline's push_quad, so they
add no pipeline and no extra draw call.

cursor_hollow is four thin quads rather than a filled cell, so the glyph beneath
the cursor stays readable."
```

---

### Task 7: Instrument layer

**Files:**
- Modify: `crates/rt/src/wgpu_backend.rs`

**Interfaces:**
- Consumes: `crate::raster::{disc, ring, bar}` — the existing CPU coverage-mask rasterisers, each returning `(w, h, Vec<u8>)`; `TextPipeline`'s atlas.
- Produces: `fill_circle`, `stroke_circle`, `stroke_line`, `begin_instrument_layer`, `end_instrument_layer` implemented.

- [ ] **Step 1: Add mask upload to the atlas**

`raster.rs` exists precisely so "both paths draw byte-identical shapes" — the GL backend uploads its masks into the coverage atlas, and this does the same. Add to `TextPipeline`:

```rust
/// Identifies a cached coverage mask. `f32::to_bits` because f32 is not `Hash`
/// and the radii/widths come from layout maths, so they repeat exactly.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub enum MaskKey { Disc(u32), Ring(u32, u32), Bar(u32) }
```

Then on `TextPipeline`:

```rust
    /// Upload an A8/R8 coverage mask from `raster.rs` into the glyph atlas and
    /// draw it as one quad. Cached by `key`, so a dial repainted every frame
    /// uploads once and thereafter costs four vertices.
    pub fn push_mask(&mut self, key: MaskKey, x: f32, y: f32,
                     mask: (u16, u16, Vec<u8>), c: Color) {
        let (w, h, data) = mask;
        let uv = match self.masks.get(&key) {
            Some(uv) => *uv,
            None => {
                let uv = self.pack(w as u32, h as u32, &data); // same shelf packer as glyphs
                self.masks.insert(key, uv);
                uv
            }
        };
        self.push_uv_quad(x, y, w as f32, h as f32, uv, c);
    }

    /// As `push_mask`, but stretched to `len` along +x and rotated by `angle`
    /// about `(x, y)`. Used for connection lines, whose cross-section mask is
    /// width-only.
    pub fn push_mask_rotated(&mut self, key: MaskKey, x: f32, y: f32, len: f32,
                             angle: f32, mask: (u16, u16, Vec<u8>), c: Color) { /* rotate the 4 corners, then push */ }
```

- [ ] **Step 2: Implement the instrument methods**

```rust
    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, c: Color) {
        let m = crate::raster::rasterize_disc(r); // -> (u16, u16, Vec<u8>)
        self.text.push_mask(MaskKey::Disc(r.to_bits()), cx - r, cy - r, m, c);
    }

    fn stroke_circle(&mut self, cx: f32, cy: f32, r: f32, width: f32, c: Color) {
        let m = crate::raster::rasterize_ring(r, width);
        self.text.push_mask(MaskKey::Ring(r.to_bits(), width.to_bits()), cx - r, cy - r, m, c);
    }

    fn stroke_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Color) {
        // rasterize_bar takes ONLY the width -- it is the line's cross-section,
        // not the whole line. Stretch that cross-section along the segment and
        // rotate it; length and angle come from the endpoints.
        let (dx, dy) = (x1 - x0, y1 - y0);
        let len = (dx * dx + dy * dy).sqrt();
        let angle = dy.atan2(dx);
        let m = crate::raster::rasterize_bar(width);
        self.text.push_mask_rotated(MaskKey::Bar(width.to_bits()), x0, y0, len, angle, m, c);
    }

    /// No-ops on purpose. The layer split exists so XRender can keep instruments
    /// on a persistent surface redrawn at 6fps, avoiding re-shipping geometry
    /// over `ssh -X`. wgpu repaints inline every frame like the GL backend, which
    /// is also why `is_gl()` returns true here.
    fn begin_instrument_layer(&mut self) {}
    fn end_instrument_layer(&mut self) {}
```

- [ ] **Step 3: Build and verify on kiku**

```bash
ssh kiku 'cd ~/git/rt && cargo build -p rt 2>&1 | tail -10'
```

On the Mac, open the patchbay/instruments view. Expected: dials, gauges and connection lines render with smooth anti-aliased edges matching the Linux build — same masks, so they should be identical.

- [ ] **Step 4: Commit**

```bash
git add crates/rt/src/wgpu_backend.rs
git commit -m "port(macos): instrument layer via the shared raster.rs masks

fill_circle, stroke_circle and stroke_line upload raster.rs's CPU coverage masks
into the same atlas the glyphs use -- which is exactly what the GL backend does,
and why raster.rs exists: 'both paths draw byte-identical shapes'. Now three
paths do.

begin/end_instrument_layer stay no-ops. That split exists so XRender can hold
instruments on a persistent 6fps surface and avoid re-shipping geometry over
ssh -X; wgpu repaints inline every frame like GL."
```

---

### Task 8: Frosted glass

**Files:**
- Modify: `crates/rt/src/vibrancy.rs`
- Modify: `crates/rt/src/wgpu_backend.rs` (clear alpha)
- Modify: `crates/rt/src/main.rs` (call site)

**Interfaces:**
- Consumes: winit's `Window` and its `AppKitWindowHandle` via `raw_window_handle`.
- Produces: `vibrancy::try_enable(window: &dyn Window)` — best-effort, never panics, mirroring `blur::try_enable_kwin_blur`.

- [ ] **Step 1: Write `vibrancy.rs`**

```rust
//! Best-effort macOS frosted glass via `NSVisualEffectView`.
//!
//! Same model as `blur.rs`, which notes: "A Wayland client cannot blur what is
//! behind its window itself — the compositor must do it." macOS is identical:
//! the window server blurs behind a transparent window. So this is a third
//! sibling of `blur.rs` (KDE `org_kde_kwin_blur`) and `bg_effect.rs`
//! (`ext-background-effect-v1`), and like both it degrades to a quiet no-op.
//!
//! ## Why not winit's `with_blur(true)`
//!
//! winit implements macOS blur as `CGSSetWindowBackgroundBlurRadius(.., 80)` — a
//! PRIVATE CoreGraphics/SkyLight call at a hardcoded radius. It is a plain
//! gaussian backdrop blur: no vibrancy, no material, and no automatic light/dark
//! or desktop-tint adaptation. `NSVisualEffectView` is public API and adapts on
//! its own, which is the part a Mac user actually notices. We keep the private
//! path only as fallback 2.
//!
//! ## Material
//!
//! Deliberately NOT set: take the system default for now. The look can only be
//! judged on screen, so picking a material up front would be guessing. If the
//! default looks flat, note that `NSVisualEffectView`'s historical default is
//! `.appearanceBased` (deprecated since 10.14) and `.underWindowBackground` is
//! the closest match to how rt looks on KDE.
use objc2::rc::Retained;
use objc2_app_kit::{NSVisualEffectBlendingMode, NSVisualEffectState, NSVisualEffectView, NSWindow};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// Install an `NSVisualEffectView` behind the window's content. Every failure
/// path returns quietly; nothing here can panic.
pub fn try_enable(window: &dyn Window) {
    let Ok(handle) = window.window_handle() else {
        log::debug!("vibrancy: no window handle; skipping");
        return;
    };
    let RawWindowHandle::AppKit(h) = handle.as_raw() else {
        log::debug!("vibrancy: not an AppKit window; skipping");
        return;
    };
    unsafe {
        let view: &objc2_app_kit::NSView = &*(h.ns_view.as_ptr() as *const objc2_app_kit::NSView);
        let Some(ns_window): Option<Retained<NSWindow>> = view.window() else {
            log::debug!("vibrancy: view has no window; skipping");
            return;
        };
        let Some(content) = ns_window.contentView() else {
            log::debug!("vibrancy: window has no content view; skipping");
            return;
        };

        let effect = NSVisualEffectView::new(objc2_foundation::MainThreadMarker::new_unchecked());
        // behindWindow is the whole point: blur what is BEHIND the window, not
        // its own contents.
        effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        // Keep the glass alive when the window is not focused; the default dims
        // it on deactivate, which reads as a rendering bug in a terminal.
        effect.setState(NSVisualEffectState::Active);
        effect.setFrame(content.bounds());
        effect.setAutoresizingMask(
            objc2_app_kit::NSAutoresizingMaskOptions::ViewWidthSizable
                | objc2_app_kit::NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        // Index 0 = beneath winit's content, so we blur behind it rather than
        // over it.
        content.addSubview_positioned_relativeTo(
            &effect,
            objc2_app_kit::NSWindowOrderingMode::Below,
            None,
        );
        ns_window.setOpaque(false);
        log::debug!("vibrancy: NSVisualEffectView installed");
    }
}
```

- [ ] **Step 2: Confirm the clear alpha already flows (no code change expected)**

Task 4 already clears with `a: bg.3 as f64`, because `render.rs`'s `Color` carries
alpha. So there is most likely nothing to change here — the opacity the user sets
should already reach the surface exactly as it does on the GL path.

Verify rather than assume. Check that `main.rs` folds `settings.background_opacity`
into the `Color` it hands `begin_frame`, as it must for Linux transparency to work:

```bash
grep -n "background_opacity" crates/rt/src/main.rs
```

If it does, this step is a no-op — say so and move on. If the opacity is applied
somewhere the wgpu path bypasses, thread it into the clear colour's alpha, and say
in the commit which of the two it turned out to be.

- [ ] **Step 3: Call it, with the fallback chain**

In `main.rs`, after the window is created (near the existing `blur::try_enable_kwin_blur` call site):

```rust
        #[cfg(target_os = "macos")]
        {
            vibrancy::try_enable(window.as_ref());
            // Fallback 2: winit's private-API gaussian blur. Harmless if the
            // effect view above already installed — it just adds a backdrop blur
            // the effect view mostly hides. Fallback 3 is plain transparency,
            // which already works and needs no call.
            window.set_blur(true);
        }
```

- [ ] **Step 4: Build and verify on kiku**

```bash
ssh kiku 'cd ~/git/rt && cargo build -p rt 2>&1 | tail -10'
```

On the Mac: run rt over a colourful background (a photo wallpaper, or a browser window). Expected: the terminal background is frosted, the desktop behind is visibly blurred rather than merely translucent, and the effect persists when rt loses focus. Then drag the opacity slider in preferences — lower opacity should reveal *more* frosted glass, not more raw desktop. Finally, switch macOS between light and dark appearance: the glass should follow without restarting rt.

- [ ] **Step 5: Verify Linux is untouched**

```bash
cargo build -p rt && cargo test -q -p rt --bin rt && ./ci/check-target-deps.sh
```

Expected: builds; tests pass; `ALL GREEN`.

- [ ] **Step 6: Commit**

```bash
git add crates/rt/src/vibrancy.rs crates/rt/src/wgpu_backend.rs crates/rt/src/main.rs
git commit -m "port(macos): native NSVisualEffectView frosted glass

A third sibling of blur.rs and bg_effect.rs, and the same model both document:
the client cannot blur what is behind it, the compositor must -- on macOS, the
window server. Best-effort, quiet no-op on every failure path.

Public NSVisualEffectView rather than winit's with_blur(), which calls the
private CGSSetWindowBackgroundBlurRadius at a hardcoded radius 80: a plain
gaussian blur with no vibrancy, no material and no light/dark adaptation. That
stays as fallback 2; plain transparency is fallback 3.

Material is deliberately unset -- take the system default until the look can be
judged on screen.

The frame clear now carries background_opacity as its alpha. At a: 1.0 the effect
view is installed but completely hidden, which looks like the vibrancy failed."
```

---

## Post-plan verification

After Task 8, confirm the whole thing on both platforms:

```bash
# Linux, from dop561
cargo test --all && ./ci/check-target-deps.sh
git diff --stat 8ca3533..HEAD -- crates/rt/src/render.rs crates/rt/src/gl_backend.rs \
    crates/rt/src/xrender_backend.rs crates/rt/src/damage.rs crates/rt/src/raster.rs
# ^ MUST be empty. Those five files are the no-regression guarantee.

# macOS, on kiku
ssh kiku 'cd ~/git/rt && cargo test -q -p rt-engine -p vt-term -p vt-parser 2>&1 | tail -5'
```

Then update `project-map.js`: add a `wgpu-backend` node in the `frontend` layer with `deps: ["rt-app"]`, and set `project.updated`. A new rendering backend is a component landing, which the repo's `CLAUDE.md` names as a status change.
