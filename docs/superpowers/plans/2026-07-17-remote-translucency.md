# Remote Translucency over `ssh -X` Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make rt's window translucent over `ssh -X` (the XRender backend), honouring the existing `background_opacity`, matching what Terminator already does on the same Xwayland/cosmic-comp path.

**Architecture:** Two small edits, no new files. (1) In the glutin config-selection closure, prefer a config whose X11 *visual* supports transparency, so the window is created 32-bit ARGB over Xwayland (the back-buffer, created at the window depth, then carries alpha end-to-end). (2) In the XRender clear, premultiply the background colour when the window is 32-bit (correct Wayland-style blending), stay opaque otherwise. Only the clear is translucent; glyphs and chrome remain opaque on top.

**Tech Stack:** Rust, glutin 0.32.3 (`GlConfig::supports_transparency`, already in scope via `glutin::prelude::*`), x11rb RENDER, winit 0.30.

## Global Constraints

Copied from `docs/superpowers/specs/2026-07-17-remote-translucency-design.md`. Every task's requirements implicitly include these.

- **No new config, no prefs change, no new files.** Reuse the existing `background_opacity` (and its prefs slider). The two edits are the entire change.
- **Only the clear is translucent.** Cells, glyphs, cursor, and chrome keep drawing opaque (`alpha = 1.0`) on top — exactly as the GL path does.
- **Premultiply only when `depth == 32`.** On a 24-bit drawable, premultiplying would darken the RGB (no alpha channel to carry it) — a visible wrong result. The depth branch is mandatory.
- **Degrade cleanly.** Where no 32-bit-visual config exists (bare X server, no compositor), the config closure falls through to today's preference and the window stays 24-bit — no regression, no crash.
- **Default `background_opacity` is `1.0`** — so `premultiply(bg)` at alpha 1.0 equals the opaque clear; the Xvfb gates (which never set opacity) must stay green.
- **Verify with `cargo build`, never the analyser** — rust-analyzer has reported phantom errors on this codebase repeatedly.

---

## File Structure

| File | Change |
|---|---|
| `crates/rt/src/main.rs` | The glutin config-selection closure (Task 1). One closure body. |
| `crates/rt/src/xrender_backend.rs` | A pure `bg_clear_color` helper + both `begin_frame` / `begin_frame_scissored` clears use it (Task 2). |

No new files. `premultiply`, `to_render_color`, and the `self.depth` field already exist in `xrender_backend.rs`.

---

### Task 1: Prefer a transparency-capable GL config

**Files:**
- Modify: `crates/rt/src/main.rs` — the config-selection closure inside `display_builder.build(event_loop, template, |configs| { … })` (currently around lines 670-681).

**Interfaces:**
- Consumes: `glutin::config::GlConfig` (in scope via `use glutin::prelude::*;`) — the methods `supports_transparency(&self) -> Option<bool>`, `alpha_size(&self) -> u8`, `num_samples(&self) -> u8`.
- Produces: nothing other tasks consume. Its observable effect is the window depth at runtime.

**This task has no unit test by nature** — it depends on live glutin/EGL/GLX config enumeration, which cannot be constructed in a unit test. Its falsifiable gate is the runtime window depth (Step 4), checked on real hardware in the Verification section. Do not fabricate a mock.

- [ ] **Step 1: Read the current closure**

Run: `sed -n '670,681p' crates/rt/src/main.rs`
Expected: the closure that does `configs.reduce(|a, b| { … better_alpha … num_samples … })`.

- [ ] **Step 2: Replace the closure body to prefer `supports_transparency` first**

Replace exactly this block:

```rust
        let (window, gl_config) = match display_builder.build(event_loop, template, |configs| {
            // Pick a config that HAS an alpha channel first (needed for a
            // transparent window), then, among equal alpha, the most samples.
            configs
                .reduce(|a, b| {
                    let better_alpha = b.alpha_size() > a.alpha_size(); // prefer any alpha
                    let same_more_samples = b.alpha_size() == a.alpha_size() && b.num_samples() > a.num_samples();
                    if better_alpha || same_more_samples { b } else { a }
                })
                .expect("at least one GL config")
        }) {
```

with:

```rust
        let (window, gl_config) = match display_builder.build(event_loop, template, |configs| {
            // Prefer a config whose X11 VISUAL supports transparency, before any
            // other criterion. On X11 the WINDOW's visual — not the GL drawable's
            // alpha_size — decides transparency: a config can report alpha_size 8
            // yet have a 24-bit visual, giving an OPAQUE window (background_opacity
            // is then silently dropped over ssh -X). supports_transparency() is
            // Some(true) only when the config's native visual is 32-bit ARGB, which
            // is what a translucent window over Xwayland needs. Native Wayland
            // already reports transparency-capable configs, so this is a no-op
            // there; and where no such config exists (bare X, no compositor) we
            // fall through to the alpha/sample preference and stay 24-bit.
            configs
                .reduce(|a, b| {
                    let (at, bt) = (
                        a.supports_transparency().unwrap_or(false),
                        b.supports_transparency().unwrap_or(false),
                    );
                    if at != bt {
                        return if bt { b } else { a }; // the transparency-capable one wins
                    }
                    // Tie on transparency: prefer more alpha, then more samples.
                    let better_alpha = b.alpha_size() > a.alpha_size();
                    let same_more_samples = b.alpha_size() == a.alpha_size() && b.num_samples() > a.num_samples();
                    if better_alpha || same_more_samples { b } else { a }
                })
                .expect("at least one GL config")
        }) {
```

- [ ] **Step 3: Build clean**

Run: `cargo build -p rt --release 2>&1 | tail -3`
Expected: `Finished`, zero warnings. (`supports_transparency` resolves via the already-imported `glutin::prelude::*`; if the analyser flags it as unknown, trust the build.)

- [ ] **Step 4: Confirm the local Wayland window still starts (no depth check here)**

Run: `cargo test -p rt --bins 2>&1 | grep "test result"`
Expected: all existing unit tests pass (this change touches only window-config selection, no unit-tested code).

The runtime depth gate (24→32 over `ssh -X`) is verified on the milkv in the Verification section — it cannot be checked from a unit test or a headless Xvfb (Xvfb usually has no 32-bit visual).

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/main.rs
git commit -m "fix(x11): pick a transparency-capable GL config so the window is 32-bit ARGB

On X11 the window's VISUAL decides transparency, not the GL drawable's
alpha_size: a config can report alpha_size 8 yet have a 24-bit visual, giving an
opaque window that silently drops background_opacity over ssh -X. Prefer
GlConfig::supports_transparency()==Some(true) first. No-op on native Wayland
(already transparency-capable); falls through to the old preference where no
32-bit visual exists (bare X, no compositor)."
```

---

### Task 2: Premultiplied translucent clear on a 32-bit window

**Files:**
- Modify: `crates/rt/src/xrender_backend.rs` — add a pure `bg_clear_color` helper; use it in `begin_frame` (around line 697) and `begin_frame_scissored` (around line 705).
- Test: unit tests in the existing `#[cfg(test)]` area of `crates/rt/src/xrender_backend.rs`.

**Interfaces:**
- Consumes: existing free functions `premultiply(c: Color) -> render::Color` and `to_render_color(c: Color) -> render::Color`; the `Color` type from `crate::render`.
- Produces: `fn bg_clear_color(depth: u8, bg: Color) -> render::Color` (module-private free function), used by both `begin_frame` variants.

- [ ] **Step 1: Write the failing test**

Add this test module next to the existing `mod premult_tests` in `crates/rt/src/xrender_backend.rs`:

```rust
#[cfg(test)]
mod bg_clear_tests {
    use super::*;
    use crate::render::Color;

    // NOTE: x11rb's `render::Color` only derives PartialEq/Debug under the
    // `extra-traits` feature (not enabled here), so assert on FIELDS, not the
    // whole struct — matching the existing `premult_tests` in this file.

    #[test]
    fn depth_32_premultiplies_the_translucent_background() {
        // 35% opaque pure-red bg on a 32-bit ARGB window: the clear must be
        // PREMULTIPLIED (red scaled by alpha) so the compositor blends it
        // correctly, and must carry the alpha.
        let c = bg_clear_color(32, Color(1.0, 0.0, 0.0, 0.35));
        let want = premultiply(Color(1.0, 0.0, 0.0, 0.35));
        assert_eq!((c.red, c.green, c.blue, c.alpha), (want.red, want.green, want.blue, want.alpha));
        assert_eq!(c.alpha, (0.35 * 65535.0) as u16, "alpha must be carried");
        assert_eq!(c.red, (1.0 * 0.35 * 65535.0) as u16, "red must be premultiplied");
    }

    #[test]
    fn depth_24_is_opaque_straight_colour_not_premultiplied() {
        // Same bg on a 24-bit window: the drawable has no alpha channel, so the
        // clear must NOT premultiply (that would darken the RGB to a wrong dark
        // opaque background). It uses the straight colour, exactly as before.
        let c = bg_clear_color(24, Color(1.0, 0.0, 0.0, 0.35));
        let want = to_render_color(Color(1.0, 0.0, 0.0, 0.35));
        assert_eq!((c.red, c.green, c.blue, c.alpha), (want.red, want.green, want.blue, want.alpha));
        assert_eq!(c.red, 65535, "red must stay full, not scaled by alpha");
    }

    #[test]
    fn opaque_background_is_identical_on_both_depths() {
        // The default background_opacity is 1.0. premultiply at alpha 1.0 equals
        // the straight colour, so a 32-bit and 24-bit clear are identical then —
        // which is why the opacity-less Xvfb gates are unaffected.
        let opaque = Color(0.1, 0.1, 0.12, 1.0);
        let a = bg_clear_color(32, opaque);
        let b = bg_clear_color(24, opaque);
        assert_eq!((a.red, a.green, a.blue, a.alpha), (b.red, b.green, b.blue, b.alpha));
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rt --bins bg_clear 2>&1 | tail -15`
Expected: FAIL to compile — `cannot find function 'bg_clear_color' in this scope`.

- [ ] **Step 3: Add the `bg_clear_color` helper**

Add this free function immediately after `premultiply` (which ends around line 111) in `crates/rt/src/xrender_backend.rs`:

```rust
/// The RENDER colour to clear the background with, given the window `depth`.
///
/// On a 32-bit ARGB window the clear is PREMULTIPLIED (Wayland/Xwayland surfaces
/// expect premultiplied alpha, like the GL path's premultiplied `clear_color`) so
/// the compositor blends `background_opacity` correctly. On a 24-bit window there
/// is no alpha channel, so premultiplying would just darken the RGB — use the
/// straight colour, which the opaque drawable renders exactly as today. At the
/// default opacity 1.0 the two are identical.
fn bg_clear_color(depth: u8, bg: Color) -> render::Color {
    if depth == 32 {
        premultiply(bg)
    } else {
        to_render_color(bg)
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p rt --bins bg_clear 2>&1 | grep -E "test result|^test "`
Expected: PASS — 3 tests.

- [ ] **Step 5: Use the helper in both clear sites**

In `begin_frame` (around line 697), replace:

```rust
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.back_pic, to_render_color(bg), &[rect]);
```

with:

```rust
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.back_pic, bg_clear_color(self.depth, bg), &[rect]);
```

In `begin_frame_scissored` (around line 705), replace the identical line:

```rust
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.back_pic, to_render_color(bg), &[rect]);
```

with:

```rust
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.back_pic, bg_clear_color(self.depth, bg), &[rect]);
```

The two lines are **byte-identical**, so an `Edit` with `replace_all: true` on `to_render_color(bg), &[rect]` → `bg_clear_color(self.depth, bg), &[rect]` changes both at once (a non-`replace_all` edit would fail as non-unique). Confirm afterward that both `begin_frame` and `begin_frame_scissored` now call `bg_clear_color`.

Both clear sites must use the helper — otherwise a scissored (partial) redraw would clear opaque while a full redraw clears translucent, giving inconsistent translucency between the two paths on the persistent back-buffer.

- [ ] **Step 6: Build and run the whole unit suite**

Run: `cargo build -p rt --release 2>&1 | tail -2 && cargo test -p rt --bins 2>&1 | grep "test result"`
Expected: `Finished` zero warnings; all unit tests pass.

- [ ] **Step 7: Confirm the Xvfb command/pixel gates still pass (opaque-by-default)**

Run:
```bash
cargo test -p rt --test xrender_commands -- --ignored 2>&1 | grep "test result"
cargo test -p rt --test instrument_compositing -- --ignored 2>&1 | grep "test result"
```
Expected: both green. These never set `background_opacity`, so the clear takes the opaque-equivalent path (premultiply at alpha 1.0 == straight colour) regardless of the depth Xvfb provides — `PutImage == 0` and the pixel/command assertions are unchanged.

- [ ] **Step 8: Commit**

```bash
git add crates/rt/src/xrender_backend.rs
git commit -m "fix(xrender): premultiplied translucent clear on a 32-bit window

The XRender clear wrote straight alpha; Wayland/Xwayland surfaces expect
premultiplied (the GL path already premultiplies its clear_color), so translucency
blended too bright. Clear with premultiply(bg) when the window is depth 32, and
the straight opaque colour otherwise (premultiplying a 24-bit drawable would
darken the RGB with no alpha to carry it). Both begin_frame and
begin_frame_scissored go through the shared bg_clear_color helper so full and
partial redraws stay consistent. Unit-tested on both depths; identical at the
default opacity 1.0, so the Xvfb gates are unaffected."
```

---

## Verification on real hardware

After both tasks, before calling this done — the milkv over `ssh -X` is the machine this feature exists for, and the depth gate can only be checked against a real Xwayland (a headless Xvfb usually has no 32-bit visual).

```bash
# from ~/git/rt, main built with both tasks:
BASE=$(ssh -S none milkv 'cd ~/git/rt && git rev-parse --short HEAD')
git bundle create /tmp/translucency.bundle ${BASE}..HEAD
scp -o ControlPath=none /tmp/translucency.bundle milkv:/tmp/
ssh -S none milkv 'cd ~/git/rt && git fetch -q /tmp/translucency.bundle HEAD && git checkout -q -B main FETCH_HEAD \
  && ~/.cargo/bin/cargo build --release && ./target/release/rt --version'
```

**Gate 1 — depth flips 24→32 (falsifiable, no window needed beyond a brief startup):**
```bash
ssh -X -S none milkv 'cd ~/git/rt && RUST_LOG=rt::xrender_backend=info RT_BACKEND=xrender timeout 4 ./target/release/rt 2>&1 | grep -oE "depth=[0-9]+"'
```
Expected: `depth=32` (was `depth=24` before this change). This is the mechanical proof the ARGB visual was selected. `ssh -S none` is mandatory — `ControlMaster auto` otherwise reuses the user's master and can inherit the wrong X forwarding.

**Gate 2 — the user's feel-test (translucency visible, matches Terminator):** ask the user to open rt over `ssh -X milkv` and confirm the desktop shows through the background at their `background_opacity=0.35`, comparable side-by-side with Terminator, and that glyphs/chrome stay fully opaque and legible. (Do not open windows on the user's desktop yourself — this is theirs to run.)

**Gate 3 — no local Wayland regression:** on the laptop's native Wayland session, translucency and blur look exactly as before (Task 1 is a no-op there; Task 2's `depth == 32` branch matches the GL path's premultiplied clear).

`ssh` notes: `~/.cargo/bin/cargo` on the milkv (cargo is not on the non-interactive PATH); always `ssh -S none` to bypass `ControlMaster`.

---

## Self-Review

**Spec coverage.** Both spec fixes map to tasks: config selection → Task 1; premultiplied depth-branched clear → Task 2 (both `begin_frame` variants, via the shared helper the spec's "the clear" section implies). All four spec gates map to the Verification section (gate 1 depth log, gate 2 feel-test, gate 3 local Wayland, gate 4 Xvfb tests → Task 2 Step 7). Non-goals (remote blur, local unchanged, no new config) are respected — no task touches blur, Wayland, or config.

**Placeholder scan.** No TBD/TODO/"handle edge cases"/"similar to". Every code step carries the exact before/after. Task 1's "no unit test" is stated with its reason (live glutin enumeration) and a concrete runtime gate instead, not a hand-wave.

**Type consistency.** `bg_clear_color(depth: u8, bg: Color) -> render::Color` is defined in Task 2 Step 3 and called with `self.depth` (a `u8` field) and `bg` (a `Color`) in Step 5 — matching. `supports_transparency`/`alpha_size`/`num_samples` are used per the glutin 0.32.3 `GlConfig` signatures verified against the source. `premultiply` and `to_render_color` signatures match their existing definitions.
