# Server-Side Instrument Compositing Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Draw the remote (XRender) instruments/patch-bay onto a separate server-side ARGB layer composited over the terminal content, redrawn only on a fixed 6fps tick, so instrument geometry never rides on a keystroke — making instruments cheap enough to default on over `ssh -X`.

**Architecture:** `XRenderBackend` gains a second offscreen pixmap `instr_pix` (32-bit premultiplied ARGB) beside the existing 24-bit `back_pixmap` (renamed in intent to "content"). Drawing primitives route through a mutable `dst_pic` target; `begin_instrument_layer`/`end_instrument_layer` point it at `instr_pix` and back. `present()` copies content to the window then composites `instr_pix` OVER it (both server-side, zero wire pixels). `main.rs` schedules a 6fps instrument tick that redraws only the instrument layer, decoupled from content frames.

**Tech Stack:** Rust, x11rb (RENDER extension), winit, existing rt `Backend` trait. Tests use the repo's Xvfb + xtrace integration harness (`crates/rt/tests/xrender_commands.rs`) and plain `cargo test` unit tests.

## Global Constraints

- **XRender-only.** All new drawing behaviour is on `XRenderBackend`. `GlBackend` gets no-op trait impls and is byte-identical; it animates instruments through egui every frame ("A") and must not change.
- **Zero `PutImage`.** Every XRender frame ships drawing commands, never pixel blits. `present()` uses server-side `CopyArea` + `RENDER Composite` only.
- **`present()` stays always-full-window.** A full-window copy/composite costs the same over the wire as a partial one (server-side); do not reintroduce damage-bbox-only present (it caused the half-drawn-border class of bugs).
- **Premultiplied ARGB.** Anything drawn onto `instr_pix` must be premultiplied alpha so `RENDER Composite(OVER)` blends correctly.
- **6fps fixed.** The instrument tick interval is a named `const INSTRUMENT_TICK: Duration = Duration::from_millis(166)` in `main.rs`. Not a config knob.
- **Defaults flip on (last task):** `inst_remote` and `inst_animate` both default `true`. Integration tests must set config explicitly (private `XDG_CONFIG_HOME`) rather than depend on the default, so they are deterministic regardless.
- **License headers / style:** follow the existing file conventions (no new headers needed; match surrounding comment density — a doc line per function, intent comments per non-obvious line).

---

## File Structure

- `crates/rt/src/xrender_backend.rs` — the two-pixmap layer, `dst_pic` indirection, `premultiply` helper, the three new `Backend` methods' real impls, the `present()` composite step, and recreation on resize. (Primary file.)
- `crates/rt/src/backend.rs` — three new `Backend` trait methods with default no-op bodies.
- `crates/rt/src/main.rs` — 6fps instrument-tick scheduling, wrapping the native instrument draw in `begin/end_instrument_layer` on ticks, and `set_instrument_layer_visible(...)` each frame.
- `crates/rt-config/src/lib.rs` — `inst_remote` and `inst_animate` defaults flip to `true`.
- `crates/rt/tests/instrument_compositing.rs` — new integration test file (Xvfb): composite-identity + decoupling guard.
- `crates/rt/tests/xrender_commands.rs` — existing guard; keep green (config-explicit).

---

## Task 1: Route drawing primitives through a mutable target Picture

Pure refactor: introduce `dst_pic` (initially always `back_pic`) so later tasks can retarget drawing at the instrument layer. No behaviour change — byte-identical output.

**Files:**
- Modify: `crates/rt/src/xrender_backend.rs` (struct fields ~104-140; `fill` ~286-295; `stamp_shape` ~336-347; `draw_tris` ~353-368; constructor `try_new` field init ~215-240)

**Interfaces:**
- Consumes: existing `back_pic: render::Picture`.
- Produces: field `dst_pic: render::Picture` on `XRenderBackend`, equal to `back_pic` at all times after this task. `fill`, `stamp_shape`, `draw_tris` composite onto `self.dst_pic`.

- [ ] **Step 1: Add the `dst_pic` field**

In the `XRenderBackend` struct (after `back_pic: render::Picture,` ~line 112) add:

```rust
    // The Picture that drawing primitives (`fill`, `stamp_shape`, `draw_tris`)
    // currently target. Normally `back_pic` (the content buffer); temporarily
    // switched to the instrument layer between `begin/end_instrument_layer`.
    dst_pic: render::Picture,
```

- [ ] **Step 2: Initialise it in `try_new`**

In the struct literal returned by `try_new` (near `back_pic,` ~line 218) add:

```rust
            dst_pic: back_pic, // starts as the content buffer
```

- [ ] **Step 3: Retarget the three primitives**

In `fill` (line ~294) change `self.back_pic` → `self.dst_pic`:

```rust
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.dst_pic, to_render_color(c), &[rect]);
```

In `stamp_shape` (line ~344) change the destination `self.back_pic` → `self.dst_pic`:

```rust
        let _ = render::composite_glyphs32(
            &self.conn, render::PictOp::OVER, self.src_pic_argb, self.dst_pic,
            self.a8_format, self.shape_glyphset, 0, 0, &cmd,
```

In `draw_tris` (line ~366) change the destination `self.back_pic` → `self.dst_pic`:

```rust
        let _ = render::triangles(
            &self.conn, render::PictOp::OVER, self.src_pic_argb, self.dst_pic,
```

Do NOT change the `back_pic` uses in `begin_frame`/`begin_frame_scissored` (lines ~484, ~489), `draw_char` glyph composite (line ~527), `recreate_back` (lines ~376, ~383, ~635) — those legitimately target the content buffer directly.

- [ ] **Step 4: Build and run the suite (regression — output must be identical)**

Run: `cargo build -p rt --features x11 && cargo test -p rt --features x11`
Expected: builds clean; all tests pass (this is a no-op refactor).

- [ ] **Step 5: Run the xtrace command guard (still commands, still zero pixels)**

Run: `cargo test -p rt --features x11 --test xrender_commands -- --ignored --nocapture 2>&1 | grep "wire profile"`
Expected: both lines print `PutImage=0` and `CompositeGlyphs=<nonzero>` (unchanged from before).

- [ ] **Step 6: Commit**

```bash
git add crates/rt/src/xrender_backend.rs
git commit -m "refactor(xrender): route draw primitives through dst_pic target"
```

---

## Task 2: Allocate the ARGB instrument pixmap, cleared transparent, resized with the back buffer

Add the offscreen 32-bit ARGB layer and keep it sized to the window. Nothing draws to it yet; `dst_pic` still stays `back_pic`.

**Files:**
- Modify: `crates/rt/src/xrender_backend.rs` (fields; `try_new` ~165-240; `recreate_back` ~372-390)
- Modify: `crates/rt/tests/xrender_commands.rs` (add a depth-32 CreatePixmap assertion to the existing idle test)

**Interfaces:**
- Consumes: local `argb_format` computed in `try_new` (~line 165), `win_w`/`win_h`, `depth`.
- Produces: fields `argb_format: Pictformat`, `instr_pixmap: xproto::Pixmap`, `instr_pic: render::Picture` on `XRenderBackend`. `instr_pix` is full-window, 32-bit, and cleared to transparent whenever (re)created. A private method `clear_instr_layer(&self)` fills `instr_pic` with transparent `(0,0,0,0)` via `PictOp::SRC`.

- [ ] **Step 1: Store `argb_format` and add the instrument-layer fields**

The struct currently has `a8_format: Pictformat,` (~line 116). After it add:

```rust
    argb_format: Pictformat,        // 32-bit premultiplied ARGB format (instrument layer)
    // Offscreen instrument/patch-bay layer: 32-bit ARGB, full-window, transparent
    // except where instruments are drawn. Composited OVER the content at present.
    instr_pixmap: xproto::Pixmap,
    instr_pic: render::Picture,
```

- [ ] **Step 2: Create the instrument pixmap in `try_new`**

`argb_format` is already computed at ~line 165 (`let argb_format = match argb32_format(&formats) { ... }`). After the `back_pic`/`gc` creation block (~line 204, before the struct literal) add:

```rust
        // The instrument layer: a full-window 32-bit ARGB pixmap + Picture,
        // composited OVER the content at present time.
        let instr_pixmap = conn.generate_id().ok()?;
        conn.create_pixmap(32, instr_pixmap, win, win_w, win_h).ok()?;
        let instr_pic = conn.generate_id().ok()?;
        render::create_picture(&conn, instr_pic, instr_pixmap, argb_format, &render::CreatePictureAux::new()).ok()?;
        // Freshly-created pixmap contents are undefined — start fully transparent.
        let _ = render::fill_rectangles(
            &conn, render::PictOp::SRC, instr_pic,
            render::Color { red: 0, green: 0, blue: 0, alpha: 0 },
            &[xproto::Rectangle { x: 0, y: 0, width: win_w, height: win_h }],
        );
```

- [ ] **Step 3: Add the fields to the struct literal**

Near `back_pic,` / `gc,` in the returned literal (~line 218) add:

```rust
            argb_format,
            instr_pixmap,
            instr_pic,
```

- [ ] **Step 4: Add `clear_instr_layer` and recreate the layer on resize**

Add this method near `fill` (an inherent method, not trait):

```rust
    /// Reset the instrument layer to fully transparent (whole window). Called
    /// at creation and at the start of every instrument tick.
    fn clear_instr_layer(&self) {
        let _ = render::fill_rectangles(
            &self.conn, render::PictOp::SRC, self.instr_pic,
            render::Color { red: 0, green: 0, blue: 0, alpha: 0 },
            &[xproto::Rectangle { x: 0, y: 0, width: self.win_w, height: self.win_h }],
        );
    }
```

In `recreate_back` (~line 372, where `back_pixmap`/`back_pic` are freed and remade), free and recreate the instrument layer alongside them. After the existing back-buffer recreation and before the method returns, add:

```rust
        // Recreate the instrument layer at the new size, transparent.
        let _ = render::free_picture(&self.conn, self.instr_pic);
        let _ = self.conn.free_pixmap(self.instr_pixmap);
        let instr_pixmap = self.conn.generate_id().expect("xid");
        let _ = self.conn.create_pixmap(32, instr_pixmap, self.window, self.win_w, self.win_h);
        let instr_pic = self.conn.generate_id().expect("xid");
        let _ = render::create_picture(&self.conn, instr_pic, instr_pixmap, self.argb_format, &render::CreatePictureAux::new());
        self.instr_pixmap = instr_pixmap;
        self.instr_pic = instr_pic;
        self.clear_instr_layer();
```

(Match the exact error-handling idiom already used in `recreate_back` — if it uses `.ok()?`/`expect`/ignored results, mirror that. The block above assumes `recreate_back` returns `()` and ignores results; adapt to the real signature you see.)

- [ ] **Step 5: Add a depth-32 CreatePixmap assertion to the existing idle xtrace test**

In `crates/rt/tests/xrender_commands.rs`, inside `xrender_emits_commands_not_pixels`, after the existing `put_image`/`composite` counts (~line 134) add:

```rust
    // The instrument layer is a 32-bit ARGB pixmap — its creation appears as a
    // CreatePixmap with depth 32 in the trace.
    let argb_pixmap = dump.matches("CreatePixmap").filter(|_| dump.contains("depth=32") || dump.contains("depth: 32")).count();
    eprintln!("depth-32 CreatePixmap present: {}", argb_pixmap > 0);
    assert!(dump.contains("CreatePixmap"), "expected the instrument-layer CreatePixmap in the trace");
```

(xtrace formats depth differently across versions; the robust assertion is that a `CreatePixmap` request is present — the ARGB layer guarantees at least one. Keep the assertion tolerant.)

- [ ] **Step 6: Build and test**

Run: `cargo build -p rt --features x11 && cargo test -p rt --features x11`
Expected: builds clean; unit/integration suite passes.

Run: `cargo test -p rt --features x11 --test xrender_commands -- --ignored --nocapture 2>&1 | grep -E "wire profile|CreatePixmap"`
Expected: `PutImage=0` still; CreatePixmap present.

- [ ] **Step 7: Commit**

```bash
git add crates/rt/src/xrender_backend.rs crates/rt/tests/xrender_commands.rs
git commit -m "feat(xrender): allocate transparent ARGB instrument layer, resized with content"
```

---

## Task 3: `premultiply` helper (unit-tested) + the three `Backend` layer methods

Extract the premultiply math into a pure, unit-tested function, and add the trait methods that retarget drawing at the instrument layer. This is the delicate-alpha task; the unit test is its correctness gate.

**Files:**
- Modify: `crates/rt/src/backend.rs` (trait: 3 new methods with no-op defaults)
- Modify: `crates/rt/src/xrender_backend.rs` (`premultiply` fn + unit test; `set_argb_src` reuse; real trait impls; `fill` alpha path; new state fields)

**Interfaces:**
- Consumes: `dst_pic` (Task 1), `instr_pic`/`clear_instr_layer` (Task 2), `Color(f32,f32,f32,f32)`.
- Produces:
  - `fn premultiply(c: Color) -> render::Color` (free fn in `xrender_backend.rs`) — premultiplied 16-bit RENDER color.
  - `Backend::begin_instrument_layer(&mut self)`, `Backend::end_instrument_layer(&mut self)`, `Backend::set_instrument_layer_visible(&mut self, visible: bool)` — no-op defaults; real on XRender.
  - XRender state: `drawing_instruments: bool` (fill uses OVER+premult when true), `instr_visible: bool` (read by `present()` in Task 4). `begin_instrument_layer` clears the layer, sets `dst_pic = instr_pic`, `clip = None`, `drawing_instruments = true`. `end_instrument_layer` sets `dst_pic = back_pic`, `drawing_instruments = false`.

- [ ] **Step 1: Write the failing unit test for `premultiply`**

Add to the bottom of `crates/rt/src/xrender_backend.rs` (inside an existing `#[cfg(test)] mod tests` if present, else create one):

```rust
#[cfg(test)]
mod premult_tests {
    use super::*;
    use crate::render::Color;

    #[test]
    fn premultiply_scales_rgb_by_alpha_and_keeps_alpha() {
        // Half-alpha pure red → premultiplied red = 0.5, alpha = 0.5.
        let c = premultiply(Color(1.0, 0.0, 0.0, 0.5));
        assert_eq!(c.alpha, 32767); // 0.5 * 65535, truncated
        assert_eq!(c.red, 32767);   // 1.0 * 0.5 * 65535
        assert_eq!(c.green, 0);
        assert_eq!(c.blue, 0);
    }

    #[test]
    fn premultiply_opaque_is_unchanged_rgb() {
        let c = premultiply(Color(1.0, 0.5, 0.25, 1.0));
        assert_eq!(c.alpha, 65535);
        assert_eq!(c.red, 65535);
        assert_eq!(c.green, 32767);
        assert_eq!(c.blue, 16383);
    }

    #[test]
    fn premultiply_fully_transparent_is_zero() {
        let c = premultiply(Color(1.0, 1.0, 1.0, 0.0));
        assert_eq!((c.red, c.green, c.blue, c.alpha), (0, 0, 0, 0));
    }
}
```

(Use the correct import path for `Color` — match how the rest of `xrender_backend.rs` refers to it, e.g. `crate::render::Color`.)

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rt --features x11 premultiply 2>&1 | tail -5`
Expected: FAIL — `cannot find function premultiply`.

- [ ] **Step 3: Implement `premultiply`**

Add near `set_argb_src` (top-level free fn, or a `pub(crate)` fn — the test uses `super::premultiply`):

```rust
/// Premultiply a straight-alpha colour into the 16-bit RENDER colour that
/// `Composite(OVER)` expects: each channel scaled by alpha, alpha preserved.
fn premultiply(c: Color) -> render::Color {
    let s = |v: f32| (v.clamp(0.0, 1.0) * 65535.0) as u16;
    render::Color { red: s(c.0 * c.3), green: s(c.1 * c.3), blue: s(c.2 * c.3), alpha: s(c.3) }
}
```

Refactor `set_argb_src` to reuse it:

```rust
    fn set_argb_src(&self, c: Color) {
        let premult = premultiply(c);
        let one = xproto::Rectangle { x: 0, y: 0, width: 1, height: 1 };
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.src_pic_argb, premult, &[one]);
    }
```

- [ ] **Step 4: Run the unit test to verify it passes**

Run: `cargo test -p rt --features x11 premultiply 2>&1 | tail -5`
Expected: PASS (3 tests).

- [ ] **Step 5: Add the trait methods with no-op defaults**

In `crates/rt/src/backend.rs`, inside the `Backend` trait (near the other default-bodied methods ~45-49) add:

```rust
    /// Begin drawing onto the separate instrument layer (XRender only). Between
    /// this and `end_instrument_layer`, `fill`/`fill_circle`/`stroke_*` land on
    /// the ARGB instrument pixmap, not the content buffer. No-op on GL (which
    /// draws instruments through egui).
    fn begin_instrument_layer(&mut self) {}
    /// Stop drawing onto the instrument layer; restore the content buffer target.
    fn end_instrument_layer(&mut self) {}
    /// Whether `present` composites the instrument layer over the content this
    /// frame. No-op on GL.
    fn set_instrument_layer_visible(&mut self, _visible: bool) {}
```

- [ ] **Step 6: Add XRender state fields + real impls**

Add fields to `XRenderBackend` (near `dst_pic`):

```rust
    drawing_instruments: bool, // true between begin/end_instrument_layer: fill() uses OVER+premultiplied
    instr_visible: bool,       // whether present() composites the instrument layer this frame
```

Init both `false` in the `try_new` struct literal.

Add the real trait impls in the `impl Backend for XRenderBackend` block (near `end_frame`):

```rust
    fn begin_instrument_layer(&mut self) {
        self.clear_instr_layer();          // fresh transparent layer each tick
        self.dst_pic = self.instr_pic;     // retarget drawing at the layer
        self.clip = None;                  // the whole layer is in play (no scissor)
        self.drawing_instruments = true;   // fill() switches to OVER + premultiplied
    }
    fn end_instrument_layer(&mut self) {
        self.dst_pic = self.back_pic;      // back to the content buffer
        self.drawing_instruments = false;
    }
    fn set_instrument_layer_visible(&mut self, visible: bool) {
        self.instr_visible = visible;
    }
```

- [ ] **Step 7: Make `fill` premultiply onto the instrument layer**

Change `fill` (the solid-rect path) so that when drawing the instrument layer it composites OVER with a premultiplied colour (so overlapping/edge cases blend), and otherwise keeps today's opaque SRC path:

```rust
    fn fill(&self, x: f32, y: f32, w: f32, h: f32, c: Color) {
        if let Some(b) = self.clip {
            if !rect_intersects(x, y, w, h, b) {
                return;
            }
        }
        let rect = xproto::Rectangle { x: x as i16, y: y as i16, width: w.max(0.0) as u16, height: h.max(0.0) as u16 };
        if self.drawing_instruments {
            // ARGB layer: OVER with premultiplied colour so alpha blends correctly.
            let _ = render::fill_rectangles(&self.conn, render::PictOp::OVER, self.dst_pic, premultiply(c), &[rect]);
        } else {
            // Content buffer: opaque SRC, exactly as before.
            let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.dst_pic, to_render_color(c), &[rect]);
        }
    }
```

- [ ] **Step 8: Build and run the full suite**

Run: `cargo build -p rt --features x11 && cargo test -p rt --features x11 2>&1 | grep -E "test result|error"`
Expected: builds clean; all tests pass (nothing calls the new methods yet, so runtime behaviour is unchanged).

- [ ] **Step 9: Commit**

```bash
git add crates/rt/src/backend.rs crates/rt/src/xrender_backend.rs
git commit -m "feat(xrender): instrument-layer draw methods + unit-tested premultiply"
```

---

## Task 4: Composite the instrument layer over the content in `present()`

Make `present()` copy the content buffer, then composite the instrument layer over it when visible. Verified by a new Xvfb integration test: instrument pixels appear over content, content-only pixels are untouched, and `PutImage` stays zero.

**Files:**
- Modify: `crates/rt/src/xrender_backend.rs` (`present` ~590-600)
- Create: `crates/rt/tests/instrument_compositing.rs`

**Interfaces:**
- Consumes: `instr_visible` (Task 3), `instr_pic`, `win_pic`, `back_pixmap`, `gc`, `win_w`/`win_h`.
- Produces: `present()` composites `instr_pic` OVER the window when `instr_visible` is true.

- [ ] **Step 1: Write the failing integration test (compositing occurs, content preserved, zero PutImage)**

Create `crates/rt/tests/instrument_compositing.rs`. It reuses the same Xvfb+xtrace helper shape as `xrender_commands.rs` (copy the `have`/`start_xvfb` helpers verbatim — they are small and self-contained). The test drives rt on the XRender path under Xvfb with instruments forced on via a private config, screenshots the window with `xwd`, and asserts an instrument-coloured pixel is present (green jack/packet `#40c0` / `#66ff7a` family) that is not part of the text/background — i.e. the layer composited. A second run with instruments off (config) must NOT show those pixels, proving the layer is what put them there. Both runs assert `PutImage=0` in the xtrace.

```rust
#![cfg(feature = "x11")]
// Full harness omitted here for brevity in the plan — the implementer copies the
// `have`, `start_xvfb`, temp-shell, and xtrace-invocation scaffolding from
// crates/rt/tests/xrender_commands.rs (they are identical in shape) and adds:
//
//  - a private XDG_CONFIG_HOME with `inst_remote = true` + `inst_animate = true`
//    (instruments ON) for the positive run, and `inst_remote = false` for the
//    control run;
//  - RT_SPLIT=v so there are two panes and at least jacks/heat borders to draw;
//  - after rt renders, capture the window: `xwd -root` piped through
//    `convert xwd:- <png>` on the test DISPLAY;
//  - count "instrument-green" pixels (R<0x60, G>0xa0, B<0x70) via a tiny inline
//    scan of the decoded image, OR shell out to ImageMagick like the repro
//    scripts: `convert png -fuzz 18% -fill white -opaque "#40c054" -fill black
//    +opaque white mask.png` then read its mean;
//  - assert: instruments-ON mean > 0 (green present) AND instruments-OFF mean == 0
//    (green absent) AND both traces have PutImage == 0.
```

Concretely, model the ImageMagick green-detection on the working repro (`convert ... -fuzz 18% -opaque "#40c054" ... -format "%[fx:mean]"`). Keep the test `#[ignore]`d like the others (needs Xvfb + xtrace + ImageMagick) and print a SKIP if any tool is missing.

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p rt --features x11 --test instrument_compositing -- --ignored --nocapture 2>&1 | tail -15`
Expected: FAIL — instruments-ON shows no green (present does not composite the layer yet), so the `mean > 0` assertion fails.

- [ ] **Step 3: Composite the layer in `present`**

Replace the body of `present` (~line 590) with:

```rust
    fn present(&mut self, _window: &Window, _damage: Option<(PxRect, &[PxRect])>) -> bool {
        // Content buffer → window (server-side, zero wire pixels), always full-window.
        let _ = self.conn.copy_area(self.back_pixmap, self.window, self.gc, 0, 0, 0, 0, self.win_w, self.win_h);
        // Instrument layer OVER the content, when visible this frame.
        if self.instr_visible {
            let _ = render::composite(
                &self.conn, render::PictOp::OVER,
                self.instr_pic, 0 /* no mask */, self.win_pic,
                0, 0, 0, 0, 0, 0, self.win_w, self.win_h,
            );
        }
        let _ = self.conn.flush();
        false
    }
```

(Confirm the `render::composite` argument order against the x11rb signature in scope — `(conn, op, src, mask, dst, src_x, src_y, mask_x, mask_y, dst_x, dst_y, width, height)`. `mask` of `0` means Picture::NONE.)

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p rt --features x11 --test instrument_compositing -- --ignored --nocapture 2>&1 | tail -15`
Expected: PASS — instruments-ON green mean > 0, instruments-OFF mean == 0, both `PutImage=0`.

Note: this test needs Task 5's wiring to actually *draw* the instruments onto the layer. If Task 5 is not yet done, the instruments-ON run draws nothing to `instr_pix` (the layer stays transparent) and green mean is 0. **Therefore: implement Step 3 here, but the passing assertion is verified at the end of Task 5.** For Task 4's own gate, assert only the mechanical invariant now: `PutImage == 0` on both runs and rt renders without error. Move the green-present/absent assertion into Task 5's verification. Adjust Step 1's test to `#[ignore]` and split its asserts accordingly: Task 4 keeps the `PutImage==0` + renders-clean asserts; Task 5 adds the green presence/absence asserts.

- [ ] **Step 5: Build + mechanical gate**

Run: `cargo build -p rt --features x11 && cargo test -p rt --features x11 --test instrument_compositing -- --ignored --nocapture 2>&1 | grep -E "PutImage|result"`
Expected: builds clean; `PutImage=0`; test passes on the mechanical asserts.

- [ ] **Step 6: Commit**

```bash
git add crates/rt/src/xrender_backend.rs crates/rt/tests/instrument_compositing.rs
git commit -m "feat(xrender): composite instrument layer over content in present"
```

---

## Task 5: Wire the 6fps instrument tick in `main.rs` (decoupled from content)

Replace the current "instrument animation forces a content full frame at ~2fps" mechanism with: a 6fps tick that redraws ONLY the instrument layer (via `begin/end_instrument_layer`), and `set_instrument_layer_visible(...)` set every frame. Content frames (keystrokes) never redraw instruments.

**Files:**
- Modify: `crates/rt/src/main.rs` — `Active` fields; `about_to_wait` anim block (~1873-1916); `paint_overlays_or_instruments` native path (~3033-3095); the frame builder where the native instrument draw is invoked; a `const INSTRUMENT_TICK`.

**Interfaces:**
- Consumes: `Backend::begin_instrument_layer`/`end_instrument_layer`/`set_instrument_layer_visible` (Task 3/4); `settings.inst_remote`/`inst_animate`; `chrome::instruments::draw` (unchanged).
- Produces: an `Active.instr_tick: bool` (this frame should redraw the instrument layer) and `Active.last_instr_tick: Instant`. The native instrument draw is wrapped in `begin/end_instrument_layer` and only runs when `instr_tick` is set.

- [ ] **Step 1: Add the tick constant and `Active` state**

Near `const ACTIVE_POLL` (~line 3882) add:

```rust
/// Remote instrument layer redraw cadence: 6fps, decoupled from content frames.
const INSTRUMENT_TICK: Duration = Duration::from_millis(166);
```

In `Active` (near `force_full`/`last_focus` ~298) add:

```rust
    instr_tick: bool,          // redraw the instrument layer this frame (6fps, native path)
    last_instr_tick: Instant,  // when the instrument layer was last redrawn
```

Init in the constructor (near `last_focus: init_focus,`): `instr_tick: false,` and `last_instr_tick: std::time::Instant::now(),` — use the same `Instant::now()` the constructor already has if available, else `Instant::now()`.

- [ ] **Step 2: Replace the anim-forces-full-frame block with a 6fps instrument tick**

In `about_to_wait`, the block at ~1900-1916 currently does, on the native path, `active.force_full = true` when `anim`. Replace the native branch so that instead of forcing a *content* full frame, it flags an *instrument* tick at 6fps:

```rust
        let anim_min = if active.low_power { Duration::from_millis(500) } else { Duration::ZERO };
        if anim && now.duration_since(active.last_anim) >= anim_min {
            active.last_anim = now;
            dirty = true;
            if active.backend.supports_egui() {
                // GL: egui redraws instruments inline every frame (target "A").
            } else if active.settings.inst_remote
                && active.settings.inst_animate
                && now.duration_since(active.last_instr_tick) >= INSTRUMENT_TICK
            {
                // Native: redraw ONLY the instrument layer at 6fps; the content
                // buffer stays on the scissored path. No content full frame.
                active.instr_tick = true;
                active.last_instr_tick = now;
                dirty = true;
            }
        }
```

(Remove the old `if !active.backend.supports_egui() { active.force_full = true; }`. Keep `pump_wires`/`sample_heat` calls above it unchanged — they feed the animation state.)

- [ ] **Step 3: Set instrument-layer visibility every frame + draw only on a tick**

In `paint_overlays_or_instruments` native path (~3070), the current guard is `if active.settings.inst_remote && active.scroll_drag.is_none()`. Change it so that (a) visibility is always set, and (b) the layer is redrawn only on a tick (or the first time), wrapped in begin/end:

```rust
        // Native (XRender): the instrument layer is composited over the content
        // by present(). Show it when instruments are on and no overlay is up and
        // we're not mid scroll-drag.
        let overlay_up = active.prefs_open || active.menu.is_some() || active.manual_open || active.search_open;
        let show = active.settings.inst_remote && !overlay_up && active.scroll_drag.is_none();
        active.backend.set_instrument_layer_visible(show);
        // Redraw the layer's geometry only on a 6fps tick (or the first show),
        // so typing/output never re-ships instrument geometry.
        if show && (active.instr_tick || !active.instr_layer_drawn) {
            active.backend.begin_instrument_layer();
            let ctx = chrome::instruments::InstrCtx {
                // ... unchanged: copy the existing InstrCtx { ... } construction verbatim ...
            };
            chrome::instruments::draw(&mut *active.backend, &ctx);
            active.backend.end_instrument_layer();
            active.instr_layer_drawn = true;
        }
        active.instr_tick = false; // consume the tick
```

Add `instr_layer_drawn: bool` to `Active` (init `false`) so the layer is drawn once on first show even between ticks (e.g. when `inst_animate = false`: static layer, drawn once, held). When `show` goes false (overlay opens), leave `instr_layer_drawn` as-is; when instruments are re-enabled after being off, reset it — simplest: set `instr_layer_drawn = false` whenever `show` is false, so the next show redraws:

```rust
        if !show { active.instr_layer_drawn = false; }
```

(Place that right after computing `show`.)

- [ ] **Step 4: Write the failing decoupling guard (xtrace)**

Add a test to `crates/rt/tests/instrument_compositing.rs`: with instruments ON (config), drive rt and feed a burst of keystrokes (via `xdotool type` against the Xvfb DISPLAY, or an output-producing shell) within a trace window shorter than one `INSTRUMENT_TICK` after the first tick, and assert the trace over that window contains **zero** new RENDER `Triangles` (wire/latency geometry) — i.e. typing did not re-ship instruments. A separate longer window that spans a tick DOES contain `Triangles`.

Because precisely bounding "between ticks" from a test is fiddly, use this robust proxy instead: run two fixed-duration traces of equal length — one where the shell is *silent* (only instrument ticks happen) and one where the shell *floods output* (content frames + ticks). Assert the flood trace's `CompositeGlyphs` count is much higher (content) while its `Triangles` count is within a small multiple of the silent trace's (instruments ticked at the same 6fps regardless of the flood). The falsifiable claim: instrument geometry volume tracks wall-clock ticks, not keystroke/output volume.

```rust
// silent_triangles ≈ flood_triangles (both ~ 6fps * duration),
// but flood_glyphs >> silent_glyphs.
assert!(flood_triangles as f64 <= 2.0 * silent_triangles as f64 + 50.0,
    "instrument geometry rode on output: silent={silent_triangles} flood={flood_triangles}");
assert!(flood_glyphs > silent_glyphs * 3,
    "expected far more text glyphs under output flood");
```

- [ ] **Step 5: Run it to verify it fails, then passes**

Run: `cargo test -p rt --features x11 --test instrument_compositing -- --ignored --nocapture 2>&1 | tail -20`
Expected before Steps 2-3 are complete: FAIL (old code re-ships instruments on every full frame → flood_triangles ≫ silent_triangles). After Steps 2-3: PASS. Also confirm the Task 4 green-present/absent asserts now pass (instruments actually draw onto the layer).

- [ ] **Step 6: Build + full suite + xtrace guards**

Run: `cargo build -p rt --features x11 && cargo test -p rt --features x11 2>&1 | grep -E "test result|error"`
Run: `cargo test -p rt --features x11 --test xrender_commands --test instrument_compositing -- --ignored --nocapture 2>&1 | grep -E "wire profile|PutImage|result"`
Expected: all green; `PutImage=0` everywhere.

- [ ] **Step 7: Commit**

```bash
git add crates/rt/src/main.rs crates/rt/tests/instrument_compositing.rs
git commit -m "feat: 6fps instrument tick, decoupled from content frames"
```

---

## Task 6: Flip the remote defaults on

Make instruments visible + animated out of the box remotely, and confirm the existing guards stay deterministic (they set config explicitly).

**Files:**
- Modify: `crates/rt-config/src/lib.rs` (`inst_remote` default ~206; `inst_animate` default ~207)
- Modify: `crates/rt/tests/xrender_commands.rs` (make it config-explicit so the default flip can't change its meaning)

**Interfaces:**
- Consumes: nothing new.
- Produces: `Settings::default()` has `inst_remote: true, inst_animate: true`.

- [ ] **Step 1: Make the existing idle guard config-explicit (so the default flip doesn't change it)**

The `xrender_commands.rs` tests drive rt with no private config, so they currently inherit `inst_remote = false`. To keep them testing *content* geometry deterministically, give both tests a private `XDG_CONFIG_HOME` with `inst_remote = false` (so the "hello world" trace has no instrument geometry, as today). Add to each test's `Command` env (near the other `.env(...)` calls):

```rust
        // Pin instruments OFF so this guard measures content geometry only,
        // independent of the shipped default.
        .env("XDG_CONFIG_HOME", &cfg_home) // a temp dir containing rt/config.toml: "inst_remote = false\n"
```

Create `cfg_home` earlier in each test (a temp dir with `rt/config.toml`), mirroring how `instrument_compositing.rs` sets up config. Keep the existing assertions unchanged.

- [ ] **Step 2: Flip the defaults**

In `crates/rt-config/src/lib.rs` `Default for Settings` (~206-207):

```rust
            inst_remote: true,             // instruments compositing is cheap now — show the patch-bay over ssh -X by default
            inst_animate: true,            // 6fps decoupled instrument tick (see INSTRUMENT_TICK); the point of this slice
```

Update the field doc comments (~144, ~152) to match (they currently say remote = plain fast terminal / static over ssh).

- [ ] **Step 3: Build and run the full suite**

Run: `cargo build -p rt --features x11 && cargo test -p rt --features x11 2>&1 | grep -E "test result|error"`
Expected: all green (config-explicit tests unaffected by the default flip).

- [ ] **Step 4: Run all xtrace guards**

Run: `cargo test -p rt --features x11 --test xrender_commands --test instrument_compositing -- --ignored --nocapture 2>&1 | grep -E "wire profile|PutImage|result"`
Expected: `PutImage=0` on every frame; all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/rt-config/src/lib.rs crates/rt/tests/xrender_commands.rs
git commit -m "feat(config): default instruments on + animated over ssh -X"
```

---

## Final verification (before the whole-branch review)

- [ ] Full suite: `cargo test -p rt --features x11` — green.
- [ ] All ignored xtrace/Xvfb integration tests: green, `PutImage=0` everywhere.
- [ ] Local GL unchanged: `cargo build -p rt` (default features) clean; a quick manual/visual check that local instruments still animate smoothly via egui.
- [ ] **milkv feel-test** (the real gate, ship via bundle as in prior slices): instruments visible + ticking at ~6fps over `ssh -X`; typing / `ls -alR ~` stay as fast as with instruments off; opening an overlay hides the instruments, closing restores them; resizing the window keeps the layer aligned.
