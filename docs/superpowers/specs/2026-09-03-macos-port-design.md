# macOS port — design

Run rt on macOS (Apple Silicon) with a wgpu/Metal rendering backend and native
`NSVisualEffectView` frosted glass, **without changing a single Linux code path**.

Status: design. Nothing implemented yet.
Date: 2026-09-03.

## Goal

rt is a Wayland-native tiling terminal for Linux, and Linux stays the primary
target. This design adds macOS as a second platform because a Mac user wants to
run rt — it is not a strategic pivot, and nothing here may cost Linux
performance.

Three properties, in priority order:

1. **Linux cannot regress.** Not "we benchmarked it and it looked fine" —
   no Linux code path changes at all. This is a structural guarantee, and
   [Testing](#testing) makes it machine-checkable.
2. **Full feature parity on macOS.** Grid, chrome (menus, prefs, search,
   tabs/splits), selection, clipboard, and the instrument layer.
3. **Native frosted glass**, using the public `NSVisualEffectView` API rather
   than the private blur winit ships.

Non-goal: automated macOS CI. See [Non-goals](#non-goals).

## Why not wgpu everywhere

The obvious tidier option — one wgpu backend for all platforms — was rejected on
evidence, not taste.

**wgpu cannot express rt's damage model.** `Backend` requires:

```rust
fn present(&mut self, window: &dyn Window, damage: Option<(PxRect, &[PxRect])>) -> bool;
fn buffer_age(&self) -> u32;
fn partial_present_available(&self) -> bool;
```

These map onto `EGL_EXT_buffer_age` and `eglSwapBuffersWithDamage`. wgpu exposes
neither: its surface present takes no damage rectangles and reports no buffer
age. Adopting it on Linux would forfeit the native damage tracking shipped in
v0.3.7 — measured performance work on exactly the slow hardware rt targets.

**It would not even simplify anything.** wgpu also cannot replace
`xrender_backend.rs`, which exists because GL over `ssh -X` is unusable: it ships
glyph *indices* over the wire rather than pixels. wgpu has no XRender path, so
remote X11 would fall back to software or fail. "wgpu everywhere" therefore means
wgpu **plus** XRender on Linux — still two backends, minus a tuned GL path. Cost
paid, simplification not collected.

Raw Metal was also considered and rejected as more work for no practical gain
over wgpu-on-Metal: it would mean Objective-C interop and hand-written MSL for a
result wgpu already produces. wgpu additionally leaves the door open to
evaluating wgpu-everywhere later without betting Linux on it first.

## Architecture

A **third `Backend` implementation**, selected on macOS, beside the two that
exist. The trait is already the right seam — `xrender_backend.rs` proves a
from-scratch backend is a ~1200-line job, and this one is smaller.

### Files that do not change

`render.rs`, `gl_backend.rs`, `xrender_backend.rs`, `damage.rs`, `raster.rs`.
`backend.rs` gains one enum variant and one match arm; `main.rs` gains one arm at
the backend construction (near `main.rs:1190`). Nothing is subtracted or
rewritten.

### Dependency gating — the actual first blocker

Nothing in `crates/rt/Cargo.toml` is target-gated today, so every Wayland crate
is compiled on Darwin. `cargo check -p rt` on macOS currently fails at
`smithay-client-toolkit`, which uses `rustix::pipe::pipe_with` — gated
`#[cfg(not(apple))]` upstream. There are **14 distinct** wayland/smithay
crates in the macOS dependency graph today (42 entries in `cargo tree`, which
repeats shared dependencies).

This is a Cargo.toml problem before it is a graphics problem, and it must be
fixed under any renderer choice.

Use **target `cfg`, not a feature flag.** The existing `x11` feature is a genuine
choice (universal binary vs. lean Wayland-only build). macOS is not a choice —
it is the platform — so `[target.'cfg(...)'.dependencies]` resolves it
automatically with no `--features` incantation to forget.

| dependency | moves to |
|---|---|
| `wayland-client`, `wayland-protocols`, `wayland-protocols-plasma`, `smithay-clipboard`, `glutin`, `khronos-egl`, `x11rb` | non-macOS target cfg |
| `wgpu`, `objc2`, `objc2-app-kit` | `cfg(target_os = "macos")` |
| `arboard` | unconditional (it already supports macOS; today it is gated behind `x11`) |

### New files

Each mirrors an existing file's role, so the structure stays legible:

| new file | mirrors | role |
|---|---|---|
| `wgpu_text.rs` | `render.rs` (847 lines) | glyph atlas + shader, WGSL |
| `wgpu_backend.rs` | `gl_backend.rs` (253 lines) | `Backend` impl delegating to the above |
| `vibrancy.rs` | `blur.rs` / `bg_effect.rs` | `NSVisualEffectView`, best-effort |

### Backend selection

`backend.rs` already has `choose_backend(display, is_x11, override_env) ->
BackendKind`, a **pure function with unit tests that need no display**. Add
`BackendKind::Wgpu` and a macOS arm. Backend-selection logic is therefore
testable in Linux CI; the Mac is needed only for pixels.

`RT_BACKEND` and `--backend` keep working, so the macOS build can still be forced
to a different backend for debugging.

## The wgpu backend

### Capability answers

| method | value | reasoning |
|---|---|---|
| `partial_present_available()` | `false` | wgpu has no `swap_buffers_with_damage`; `CAMetalLayer` has no damage-rect concept |
| `buffer_age()` | `0` | the trait documents 0 as "unknown/fresh → must redraw all", which is the truth here |
| `present(_, damage)` | ignores `damage`, full present, returns `false` | see below |
| `is_software()` | `false` | real GPU — do not throttle animated chrome |
| `is_gl()` | `true` | see [naming wart](#a-naming-wart-left-alone-deliberately) |
| `supports_scroll_blit()` | `false` (trait default) | that is XRender's `ssh -X` optimisation |

**Accepted consequence:** with `partial_present_available()` false, `main.rs:5440`
calls `damage.mark_full()`, so macOS redraws the whole window every frame. This
is deliberate. Damage tracking exists for slow boards and `ssh -X`; on a local
Apple Silicon GPU a full terminal-grid redraw per frame is negligible.
`damage.rs` still computes damage — it is pure and untouched — macOS simply does
not act on it.

That makes `present(Some(..))` effectively unreachable, since the scissored path
is gated behind `partial_present_available()`. Implement it as a full present
returning `false` anyway, rather than `todo!()`, so a future caller cannot trip a
panic.

**Scissored *drawing* still works.** `begin_frame_scissored` maps onto a wgpu
render-pass scissor rect. Only damage-limited *presenting* is unavailable, so the
method saves GPU work rather than bandwidth.

### Text rendering

`render.rs` describes itself as: *"One shader. Vertices carry position (pixels), a
UV into a single coverage-only (`R8`) atlas texture, and an RGBA colour."* That
maps onto wgpu nearly one-to-one — one render pipeline, one `R8Unorm` atlas
texture, one vertex buffer, GLSL becoming WGSL.

Instruments reuse `raster.rs`, which already produces CPU coverage masks
explicitly so that "both paths draw byte-identical shapes". The third backend
uploads those same masks into the same atlas, exactly as the GL backend does — so
full instrument parity is mostly upload-and-draw, not new geometry.

### A naming wart, left alone deliberately

`is_gl()` is documented as distinguishing *only* how instruments are drawn: GL
repaints inline every frame, XRender uses a 6fps persistent layer. wgpu wants the
GL behaviour, so it returns `true` — which reads as a lie.

The honest fix is renaming it to something like `repaints_instruments_inline()`.
That would touch `backend.rs`, both Linux backends and `main.rs`, breaking the
"no Linux code path changes" guarantee that is the entire safety argument for
this approach. **Buying naming clarity with Linux churn is a bad trade.** Return
`true`, document why at the implementation site, and do the rename separately if
it is ever wanted.

## Transparency and vibrancy

### The plumbing already exists

rt already creates transparent windows and already has a user-facing opacity
setting:

```rust
main.rs:878:  .with_transparent(true)  // REQUIRED for the compositor to honour our alpha
main.rs:912:  ConfigTemplateBuilder::new().with_alpha_size(8)
```

`docs/APPEARANCE.md` documents the whole translucency-and-blur model. macOS wires
a new blur *provider* into a working system rather than building one.

The conceptual fit is exact. `blur.rs` states: *"A Wayland client cannot blur what
is behind its window itself — the compositor must do it."* `NSVisualEffectView`
works the same way: the window server does the blur behind a transparent window.
rt already has two implementations of "request blur behind our surface, no-op if
unavailable" (`blur.rs` for KDE's `org_kde_kwin_blur`, `bg_effect.rs` for
`ext-background-effect-v1`). macOS is a third sibling.

### Two different blurs — use the good one

winit already implements `set_blur()` for macOS, reachable via `with_blur(true)`:

```rust
pub fn set_blur(&self, blur: bool) {
    let radius = if blur { 80 } else { 0 };
    unsafe { ffi::CGSSetWindowBackgroundBlurRadius(
        ffi::CGSMainConnectionID(), window_number, radius) }
}
```

That is `CGSSetWindowBackgroundBlurRadius` — a **private** CoreGraphics/SkyLight
API at a hardcoded radius. It is a plain gaussian backdrop blur: no vibrancy, no
material, no automatic light/dark or desktop-tint adaptation.

Real frosted glass is `NSVisualEffectView` with an `NSVisualEffectMaterial`,
`blendingMode = .behindWindow`, and `state = .followsWindowActiveState`. Public
API, and it adapts to appearance changes on its own — the part a Mac user
actually notices.

### Implementation and fallback chain

Take the `NSWindow` from the `AppKitWindowHandle` winit exposes, create an
`NSVisualEffectView`, set material/blending/state, and install it beneath winit's
content view with a full autoresizing mask. `objc2` and `objc2-app-kit` are
already in the macOS dependency tree via winit-appkit, so this adds no new
ecosystem.

Degrade quietly, exactly as `blur.rs` does when the KWin global is absent:

1. `NSVisualEffectView` — real frosted glass.
2. winit's `with_blur(true)` — private-API gaussian.
3. Plain transparency — already works.

### Composition with the existing scrim

`APPEARANCE.md` calls the scrim "the portable slider that works everywhere". On
macOS the scrim sits *over* the vibrancy and `background_opacity` keeps its
current meaning: lower opacity lets more frosted glass through. No new settings
semantics — which is what parity should mean.

**Take the system default material for v1** — do not set `material` explicitly,
and let `NSVisualEffectView` use whatever the system picks. The look can only be
judged on screen, so choosing a specific material now would be guessing; refine
it once rt actually runs on the Mac.

One thing to check visually when it does: `NSVisualEffectView`'s historical
default is `.appearanceBased`, deprecated since 10.14. If the default turns out
to look wrong or flat, `.underWindowBackground` is the closest match to how rt
looks on KDE and is the first thing to try. A macOS-only material *picker*
remains out of scope; see [Non-goals](#non-goals).

### Surface alpha

Configure the wgpu surface with `CompositeAlphaMode::PostMultiplied` and a
non-opaque `CAMetalLayer`, so the terminal's alpha reaches the effect view
underneath. This is the one place the renderer and the vibrancy interact — wrong,
and you get either an opaque black window or double-darkened glass.

## Clipboard

`Clipboard` is already an enum dispatched on `RawDisplayHandle`, so macOS is a
third variant:

```rust
pub enum Clipboard {
    Wayland(smithay_clipboard::Clipboard),
    #[cfg(feature = "x11")] X11(X11Clipboard),
    Mac(MacClipboard),   // arboard, selected from RawDisplayHandle::AppKit
}
```

`arboard` handles the macOS pasteboard, so `store`/`load` work unchanged.

**Known capability gap, not a defect:** macOS has no PRIMARY selection.
`store_primary`/`load_primary` are no-ops there, so middle-click paste does not
exist on macOS. PRIMARY is an X11 concept macOS does not have. Documented here so
nobody later "fixes" it.

## Testing

### Tier 1 — automated, existing Linux CI, no Mac required

`cargo tree --target aarch64-apple-darwin` resolves the macOS dependency graph on
Linux **with no macOS SDK and no cross-linker** (verified: exit 0). That gives
two strong checks:

1. **Zero wayland/smithay crates in the macOS graph.** Today: 14 distinct
   (`smithay-client-toolkit`, `smithay-clipboard`, `wayland-backend`,
   `wayland-client`, `wayland-csd-frame`, `wayland-cursor`, `wayland-protocols`,
   `-experimental`, `-misc`, `-plasma`, `-wlr`, `wayland-scanner`,
   `wayland-source`, `wayland-sys`). This guards the
   failure mode that will certainly recur — someone adds an unconditional Wayland
   dependency and silently breaks macOS.
2. **The Linux dependency graph is byte-identical before and after.** This turns
   "Linux cannot regress" from a promise into an assertion.

Plus `choose_backend`'s macOS arm (pure, existing test module) and the unchanged
`damage.rs` / `raster.rs` suites.

### Tier 2 — manual, on kiku

3. `cargo check -p rt` on kiku: today's `smithay-client-toolkit` failure → clean.
4. Pixels: grid, fonts, cursor, selection, menus, prefs, search, tabs/splits,
   instruments, and vibrancy actually looking like frosted glass.
5. Engine suites (already passing there).

### Tier 3 — macOS CI, deferred

kiku cannot be a Woodpecker agent today: the server's gRPC listener is on apollo
inside Station Oost, and kiku is on the ASTRON network. A concrete route exists —
the `stationoost` repo's WireGuard work ("Phase 8: WireGuard working end to end
from off-LAN") would let kiku dial out to apollo exactly as milkv does across
VLANs. Not part of this port.

**Until Tier 3 exists, macOS has no automated coverage and is manually verified
only.** Stated explicitly because on 2026-09-03 the riscv64 cell reported
`success` for seven consecutive pipelines while contributing nothing — its
`build-and-test` step is `failure: ignore`, so a full disk and a skipped clone
both looked green. A green badge must not be read as macOS coverage.

## Operational notes

- **kiku's address is location-dependent.** `10.13.0.231` is its ASTRON address;
  at Station Oost it differs, and the `Host kiku` entry in
  `~/.ssh/config.d/adhoc` needs updating. `is-astron` cannot gate it — that
  detector tests for the `control.lofar` zone, which is not visible from ASTRON
  office wifi.
- **macOS has no `timeout`.** A remote command wrapped in `timeout …` exits 127
  and produces no output, which is indistinguishable from a clean run in a
  script.
- kiku is **not** in the three-machine deploy rotation (dop561, apollo, milkv).
  It is a port target, not a deploy target.

## Non-goals

- **wgpu on Linux.** See [Why not wgpu everywhere](#why-not-wgpu-everywhere).
- **Automated macOS CI.** Deferred to the WireGuard route above.
- **A macOS-only blur-material picker.** `APPEARANCE.md` records that a
  client-controlled blur-strength slider is impossible on Wayland (KWin owns
  strength globally); on macOS the client *can* choose the material, so the Mac
  build could exceed Linux here. That is a real opportunity — and its own slice,
  not smuggled into a port whose goal is parity.
- **Renaming `is_gl()`.** See [the naming wart](#a-naming-wart-left-alone-deliberately).
- **Intel Macs.** Target is Apple Silicon (kiku is an M5). Nothing here
  forbids x86_64 macOS, but it is untested and unclaimed.
