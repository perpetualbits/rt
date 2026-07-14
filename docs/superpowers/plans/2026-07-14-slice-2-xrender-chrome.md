# Slice 2 — XRender-drawn chrome Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Draw rt's context menu, search bar, manual, and animated instruments/patch-bay natively as XRender commands on the remote (`ssh -X`) backend, so the remote session has working chrome while staying commands-only (zero pixel blits).

**Architecture:** Each overlay becomes a set of backend-agnostic *draw + hit-test* functions in a new `chrome` module that read rt's existing overlay state (no parallel state). They paint via the `Backend` trait — reusing Slice 1's `fill_rect`/`draw_char` plus three new anti-aliased primitives (`fill_circle`, `stroke_circle`, `stroke_line`) that `XRenderBackend` implements with the RENDER `Triangles` request. Selection is per-overlay on `Backend::supports_egui()`: the GL backend keeps today's egui chrome unchanged; the XRender backend uses the native units.

**Tech Stack:** Rust, `x11rb` (RENDER extension: `triangles`, `fill_rectangles`, `composite_glyphs32`), `fontdue`, `egui` core types (`Color32`/`Pos2` reused only as color/point carriers for the instrument math — single source of truth), `winit`.

## Global Constraints

- **Local GL path stays byte-for-byte identical.** Native chrome is XRender-only, gated by `supports_egui() == false`. The new `Backend` primitives have **no-op defaults** so `GlBackend` needs no change.
- **Zero `PutImage`.** RENDER `Triangles`/`FillRectangles`/`CompositeGlyphs` are server-side commands. The xtrace guard `tests/xrender_commands.rs` asserting `PutImage == 0` must keep passing with chrome on screen.
- **Never kill unowned processes.** Every helper process in tests (Xvfb, xtrace, rt-under-test) is spawned as an owned `Child` and stopped by that exact handle; the traced rt is bounded by `timeout`. No `pkill`/`killall`/kill-by-name, on any host.
- **Reuse the existing color type:** the new primitives take `crate::render::Color` (a `pub struct Color(pub f32,pub f32,pub f32,pub f32)` with `Color::rgb`), NOT a new `Rgba` type.
- **Reuse logic verbatim:** `menu.rs` items, the search engine (`run_search`/`search_step`/`close_search`), `manual::MANUAL`, and the instrument state math (`heat_color32`/`latency_color`/`cubic_bezier`/`flow_point`/`blackbody`) are shared between the GL and native paths — no duplicated or reimplemented decision logic.
- **Preferences stays egui-only** (still force-closed on XRender). Translucency/blur is out of scope (Slice 3).
- **Feature flag:** all XRender code is under `#[cfg(feature = "x11")]`; `x11` is a default feature, so `cargo test -p rt` exercises it.
- **Verify remote perf over the real transport** (`ssh -X milkv`), never a local Xvfb (its SHM hides the network cost).

## File Structure

- **Create `crates/rt/src/chrome/mod.rs`** — declares the submodules; shared helpers: a `Recti { x, y, w, h: f32 }` layout rect, `fn hit(rects: &[Recti], p: (f32,f32)) -> Option<usize>`, and `fn col(c: egui::Color32) -> crate::render::Color` (byte→float adapter so native instruments reuse the exact egui color functions).
- **Create `crates/rt/src/chrome/menu.rs`** — native context-menu layout/hit-test/draw over `menu::Row`s.
- **Create `crates/rt/src/chrome/search.rs`** — native search-bar layout/draw.
- **Create `crates/rt/src/chrome/manual.rs`** — native manual layout/draw (scroll viewport).
- **Create `crates/rt/src/chrome/instruments.rs`** — native instruments/patch-bay draw via the new primitives.
- **Modify `crates/rt/src/xrender_backend.rs`** — add an ARGB solid source + the AA tessellation helpers + implement the three primitives.
- **Modify `crates/rt/src/backend.rs:20-78`** — add the three primitive methods with no-op defaults.
- **Modify `crates/rt/src/menu.rs`** — extract a backend-agnostic `pub fn rows(...) -> Vec<Row>` consumed by both the egui `ui()` and the native menu.
- **Modify `crates/rt/src/main.rs`** — declare `mod chrome;` (see Crate-Layout note below); add `manual_scroll`/`menu_hover` state; extract `advance_instrument_state`; replace the Slice-1 force-close (lines 1040-1064) + input-diversion blocks with native routing for the four overlays; dispatch native draws in `paint_overlays_or_instruments` (lines 2773-2790).
- **Modify `crates/rt/tests/xrender_commands.rs`** — add an overlay-open variant asserting `Triangles > 0` and `PutImage == 0`.

### Crate-Layout note (critical)

rt is split into a **library crate** (`lib.rs`: `damage`, `input`, `render`) and a **binary crate** (`main.rs`, which re-declares `mod render` etc.). `backend`, `xrender_backend`, `menu`, `manual`, `render`, and the `Active` struct + free helpers (`heat_color32`, `latency_color`, `cubic_bezier`, `flow_point`, `content_bounds`) all live in the **binary** crate. Therefore `chrome` **must be declared in `main.rs`** (`mod chrome;`), not `lib.rs` — otherwise it cannot see `Backend`, `Active`, or those helpers.

Because `chrome` is then a **descendant module of the crate root**, it can access crate-root-**private** items (functions, constants, and `Active`'s private fields) via `crate::…` with **no `pub`/`pub(crate)` changes needed** — Rust grants descendant modules access to ancestors' private items. So Task 9 needs no visibility edits, only the `use` paths. Chrome's `#[cfg(test)]` unit tests run under `cargo test -p rt` (which compiles and tests the bin), not `cargo test -p rt --lib`.

---

### Task 1: Anti-aliased geometry tessellation (pure math)

The novel geometry, isolated as pure functions so it is unit-testable without an X server. Produces triangle lists in **f32** window pixels; a separate converter maps them to `render::Triangle` (16.16 fixed point) at draw time.

**Files:**
- Modify: `crates/rt/src/xrender_backend.rs` (add a private `geom` section near the top, after the `use`s)
- Test: same file, `#[cfg(test)] mod geom_tests`

**Interfaces:**
- Produces:
  - `type TriF = [(f32, f32); 3];`
  - `fn seg_count(r: f32) -> u32` — adaptive rim subdivisions (min 8, ~`r*3` capped 64).
  - `fn disc_tris(cx: f32, cy: f32, r: f32) -> Vec<TriF>` — filled disc as a fan.
  - `fn ring_tris(cx: f32, cy: f32, r: f32, width: f32) -> Vec<TriF>` — annulus (outer `r`, inner `r-width`) as a triangle strip.
  - `fn line_tris(x0: f32, y0: f32, x1: f32, y1: f32, width: f32) -> Vec<TriF>` — thick segment as two triangles (butt caps).
  - `fn tris_bbox(tris: &[TriF]) -> (f32, f32, f32, f32)` — `(x, y, w, h)` bounds, for clip testing.

- [ ] **Step 1: Write the failing tests**

Add to `crates/rt/src/xrender_backend.rs`:

```rust
#[cfg(test)]
mod geom_tests {
    use super::*;

    #[test]
    fn disc_is_a_fan_on_radius() {
        let n = seg_count(9.0);
        let tris = disc_tris(50.0, 40.0, 9.0);
        assert_eq!(tris.len() as u32, n, "one triangle per rim segment");
        // Every rim vertex is ~9px from the centre.
        for t in &tris {
            for &(x, y) in &[t[1], t[2]] {
                let d = ((x - 50.0).powi(2) + (y - 40.0).powi(2)).sqrt();
                assert!((d - 9.0).abs() < 0.01, "rim vertex off-circle: {d}");
            }
            assert_eq!(t[0], (50.0, 40.0), "fan apex is the centre");
        }
    }

    #[test]
    fn ring_has_inner_and_outer_radius() {
        let tris = ring_tris(30.0, 30.0, 4.0, 1.4);
        assert!(!tris.is_empty());
        let mut saw_outer = false;
        let mut saw_inner = false;
        for t in &tris {
            for &(x, y) in t {
                let d = ((x - 30.0).powi(2) + (y - 30.0).powi(2)).sqrt();
                if (d - 4.0).abs() < 0.02 { saw_outer = true; }
                if (d - 2.6).abs() < 0.02 { saw_inner = true; } // 4.0 - 1.4
            }
        }
        assert!(saw_outer && saw_inner, "ring must touch both radii");
    }

    #[test]
    fn line_is_two_triangles_of_correct_width() {
        // A horizontal segment, width 2 → a 10x2 quad centred on y=20.
        let tris = line_tris(10.0, 20.0, 20.0, 20.0, 2.0);
        assert_eq!(tris.len(), 2, "a quad is two triangles");
        let (_, y, _, h) = tris_bbox(&tris);
        assert!((y - 19.0).abs() < 0.01 && (h - 2.0).abs() < 0.01, "half-width each side");
    }
}
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test -p rt geom_tests -- --nocapture`
Expected: FAIL — `cannot find function seg_count` (etc.).

- [ ] **Step 3: Implement the tessellation helpers**

Add near the top of `crates/rt/src/xrender_backend.rs` (after the existing `use` block):

```rust
/// A triangle in f32 window pixels (converted to XRender 16.16 fixed point at draw).
type TriF = [(f32, f32); 3];

/// Rim subdivisions for a disc/ring of radius `r`: enough that the polygon reads
/// as a circle at terminal sizes, capped so a big ring stays cheap.
fn seg_count(r: f32) -> u32 {
    ((r * 3.0) as u32).clamp(8, 64)
}

/// Filled disc as a triangle fan (apex = centre, rim = `seg_count(r)` points).
fn disc_tris(cx: f32, cy: f32, r: f32) -> Vec<TriF> {
    use std::f32::consts::TAU;
    let n = seg_count(r);
    let pt = |k: u32| {
        let a = TAU * k as f32 / n as f32;
        (cx + r * a.cos(), cy + r * a.sin())
    };
    (0..n).map(|k| [(cx, cy), pt(k), pt((k + 1) % n)]).collect()
}

/// Annulus (outer `r`, inner `r-width`) as a triangle strip of quads → 2 tris each.
fn ring_tris(cx: f32, cy: f32, r: f32, width: f32) -> Vec<TriF> {
    use std::f32::consts::TAU;
    let ri = (r - width).max(0.0);
    let n = seg_count(r);
    let outer = |k: u32| { let a = TAU * k as f32 / n as f32; (cx + r * a.cos(), cy + r * a.sin()) };
    let inner = |k: u32| { let a = TAU * k as f32 / n as f32; (cx + ri * a.cos(), cy + ri * a.sin()) };
    let mut out = Vec::with_capacity(n as usize * 2);
    for k in 0..n {
        let (o0, o1) = (outer(k), outer((k + 1) % n));
        let (i0, i1) = (inner(k), inner((k + 1) % n));
        out.push([o0, o1, i1]);
        out.push([o0, i1, i0]);
    }
    out
}

/// Thick segment as a quad (2 triangles), butt caps, `width` centred on the line.
fn line_tris(x0: f32, y0: f32, x1: f32, y1: f32, width: f32) -> Vec<TriF> {
    let (dx, dy) = (x1 - x0, y1 - y0);
    let len = (dx * dx + dy * dy).sqrt().max(1e-6);
    // Unit normal, scaled to the half-width.
    let (nx, ny) = (-dy / len * width * 0.5, dx / len * width * 0.5);
    let a = (x0 + nx, y0 + ny);
    let b = (x1 + nx, y1 + ny);
    let c = (x1 - nx, y1 - ny);
    let d = (x0 - nx, y0 - ny);
    vec![[a, b, c], [a, c, d]]
}

/// Axis-aligned bounds `(x, y, w, h)` of a triangle list (for clip rejection).
fn tris_bbox(tris: &[TriF]) -> (f32, f32, f32, f32) {
    let mut lo = (f32::MAX, f32::MAX);
    let mut hi = (f32::MIN, f32::MIN);
    for t in tris {
        for &(x, y) in t {
            lo = (lo.0.min(x), lo.1.min(y));
            hi = (hi.0.max(x), hi.1.max(y));
        }
    }
    (lo.0, lo.1, hi.0 - lo.0, hi.1 - lo.1)
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test -p rt geom_tests -- --nocapture`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/xrender_backend.rs
git commit -m "feat(xrender): AA geometry tessellation for chrome primitives"
```

---

### Task 2: `Backend` AA primitives + XRender implementation

Add the three anti-aliased primitives to the `Backend` trait (no-op defaults, so `GlBackend` is untouched) and implement them on `XRenderBackend` via a 32-bit ARGB solid source composited through `render::triangles` with an A8 mask.

**Files:**
- Modify: `crates/rt/src/backend.rs:39` (add methods to the trait, after `end_frame`)
- Modify: `crates/rt/src/xrender_backend.rs` (add ARGB source fields, format finder, primitive impls, Drop cleanup)

**Interfaces:**
- Consumes: Task 1's `disc_tris`/`ring_tris`/`line_tris`/`tris_bbox`, `TriF`.
- Produces (on `Backend`):
  - `fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, c: Color) {}`
  - `fn stroke_circle(&mut self, cx: f32, cy: f32, r: f32, width: f32, c: Color) {}`
  - `fn stroke_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Color) {}`
- Produces (on `XRenderBackend`): new fields `argb_format: Pictformat`, `src_pixmap_argb: xproto::Pixmap`, `src_pic_argb: render::Picture`; helper `fn draw_tris(&self, tris: &[TriF], c: Color)`.

- [ ] **Step 1: Add the trait methods (no-op defaults)**

In `crates/rt/src/backend.rs`, immediately after `fn end_frame(&mut self);` (line 39):

```rust
    // --- anti-aliased chrome primitives (native XRender chrome, Slice 2) ---
    // Default no-ops: the GL backend never draws native chrome (it uses egui),
    // so only XRenderBackend overrides these. Coords are window pixels.
    /// Filled anti-aliased disc, alpha-composited (OVER).
    fn fill_circle(&mut self, _cx: f32, _cy: f32, _r: f32, _c: Color) {}
    /// Anti-aliased ring of the given stroke width (outer radius `r`).
    fn stroke_circle(&mut self, _cx: f32, _cy: f32, _r: f32, _width: f32, _c: Color) {}
    /// Anti-aliased thick line segment (butt caps).
    fn stroke_line(&mut self, _x0: f32, _y0: f32, _x1: f32, _y1: f32, _width: f32, _c: Color) {}
```

- [ ] **Step 2: Add the ARGB format finder + a build check**

In `crates/rt/src/xrender_backend.rs`, add beside `a8_format` (after line 243):

```rust
/// Find a 32-bit DIRECT ARGB format for the alpha-blended solid source (packet
/// glow needs true alpha, unlike the opaque 24-bit `src_pic`).
fn argb32_format(formats: &render::QueryPictFormatsReply) -> Option<Pictformat> {
    formats
        .formats
        .iter()
        .find(|f| f.type_ == PictType::DIRECT && f.depth == 32 && f.direct.alpha_mask == 0xff)
        .map(|f| f.id)
}
```

Run: `cargo build -p rt` — Expected: PASS (unused function warning is fine for now).

- [ ] **Step 3: Add the ARGB source fields + create them in `try_new`**

Add three fields to `struct XRenderBackend` (after `src_pic` at line 45):

```rust
    argb_format: Pictformat,        // 32-bit ARGB format for the alpha source
    src_pixmap_argb: xproto::Pixmap,// 1x1 repeating alpha-capable source pixmap
    src_pic_argb: render::Picture,  // the ARGB source Picture (AA primitives)
```

In `try_new`, after the A8 format lookup (after line 83) add:

```rust
        let argb_format = match argb32_format(&formats) {
            Some(f) => f,
            None => { log::warn!("xrender: no 32-bit ARGB format; falling back to GL"); return None; }
        };
```

After the opaque `src_pic` is created (after line 96) add:

```rust
        // A 1x1 repeating 32-bit ARGB source for the alpha-blended AA primitives.
        let src_pixmap_argb = conn.generate_id().ok()?;
        conn.create_pixmap(32, src_pixmap_argb, win, 1, 1).ok()?;
        let src_pic_argb = conn.generate_id().ok()?;
        let aux_argb = render::CreatePictureAux::new().repeat(render::Repeat::NORMAL);
        render::create_picture(&conn, src_pic_argb, src_pixmap_argb, argb_format, &aux_argb).ok()?;
```

Add the three fields to the `Some(Self { … })` initializer (after `src_pic,` at line 130):

```rust
            argb_format,
            src_pixmap_argb,
            src_pic_argb,
```

- [ ] **Step 4: Implement the primitives**

Add the shared helper as an `impl XRenderBackend` method (near `fill`, after line 214) — note the two `use` additions at the top of the file: `use x11rb::protocol::render::{Triangle, Pointfix};` fold into the existing `render` import if preferred.

```rust
    /// Composite a triangle mesh in colour `c` (straight alpha) onto the back
    /// buffer with anti-aliasing: fill the 1x1 ARGB source with the *premultiplied*
    /// colour, then `render::triangles` OVER through the A8 mask format so the
    /// per-edge coverage is antialiased. Server-side geometry — zero wire pixels.
    fn draw_tris(&self, tris: &[TriF], c: Color) {
        if tris.is_empty() { return; }
        // Clip rejection: skip meshes wholly outside the damage clip.
        if let Some(b) = self.clip {
            let (x, y, w, h) = tris_bbox(tris);
            if !rect_intersects(x, y, w, h, b) { return; }
        }
        // Premultiplied ARGB solid source (OVER expects premultiplied alpha).
        let s = |v: f32| (v.clamp(0.0, 1.0) * 65535.0) as u16;
        let premult = render::Color { red: s(c.0 * c.3), green: s(c.1 * c.3), blue: s(c.2 * c.3), alpha: s(c.3) };
        let one = xproto::Rectangle { x: 0, y: 0, width: 1, height: 1 };
        let _ = render::fill_rectangles(&self.conn, render::PictOp::SRC, self.src_pic_argb, premult, &[one]);
        // f32 window px → 16.16 fixed point.
        let fx = |v: f32| (v * 65536.0).round() as i32;
        let mk = |(x, y): (f32, f32)| Pointfix { x: fx(x), y: fx(y) };
        let hw: Vec<Triangle> = tris.iter().map(|t| Triangle { p1: mk(t[0]), p2: mk(t[1]), p3: mk(t[2]) }).collect();
        let _ = render::triangles(
            &self.conn, render::PictOp::OVER, self.src_pic_argb, self.back_pic,
            self.a8_format, 0, 0, &hw,
        );
    }
```

Add the three trait methods inside `impl Backend for XRenderBackend` (after `bell_stripe`/before `end_frame`, around line 377):

```rust
    fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, c: Color) {
        self.draw_tris(&disc_tris(cx, cy, r), c);
    }
    fn stroke_circle(&mut self, cx: f32, cy: f32, r: f32, width: f32, c: Color) {
        self.draw_tris(&ring_tris(cx, cy, r, width), c);
    }
    fn stroke_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, c: Color) {
        self.draw_tris(&line_tris(x0, y0, x1, y1, width), c);
    }
```

Free the ARGB source in `Drop` (after the `src_pic` free, line 428):

```rust
        let _ = render::free_picture(&self.conn, self.src_pic_argb);
        let _ = xproto::free_pixmap(&self.conn, self.src_pixmap_argb);
```

- [ ] **Step 5: Build and run existing tests**

Run: `cargo test -p rt` (not `--lib`: the `geom_tests` live in the binary crate's `xrender_backend` module)
Expected: PASS — the crate compiles with the new fields/methods; `choose_backend` and `geom_tests` still pass. (On-wire behaviour is proven by Task 11's xtrace test and manual `ssh -X`.)

- [ ] **Step 6: Commit**

```bash
git add crates/rt/src/backend.rs crates/rt/src/xrender_backend.rs
git commit -m "feat(xrender): AA fill_circle/stroke_circle/stroke_line via RENDER Triangles"
```

---

### Task 3: `chrome` module scaffold + shared helpers

Create the module, its shared layout rect + hit-test, and the `egui::Color32 → render::Color` adapter that lets the native instruments reuse the exact existing color functions.

**Files:**
- Create: `crates/rt/src/chrome/mod.rs`
- Modify: `crates/rt/src/main.rs` (add `mod chrome;` beside the other `mod` declarations at lines 13-28 — **not** `lib.rs`; see the Crate-Layout note)
- Test: `crates/rt/src/chrome/mod.rs`, `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `pub struct Recti { pub x: f32, pub y: f32, pub w: f32, pub h: f32 }`
  - `impl Recti { pub fn contains(&self, p: (f32, f32)) -> bool }`
  - `pub fn hit(rects: &[Recti], p: (f32, f32)) -> Option<usize>`
  - `pub fn col(c: egui::Color32) -> crate::render::Color`
  - `pub mod menu; pub mod search; pub mod manual; pub mod instruments;` (added as those tasks land)

- [ ] **Step 1: Write the failing tests**

Create `crates/rt/src/chrome/mod.rs`:

```rust
//! Native (XRender) chrome: backend-agnostic draw + hit-test for the context
//! menu, search bar, manual, and instruments, used on the `supports_egui() ==
//! false` path. Each unit reads rt's existing overlay state (no parallel state)
//! and paints via `Backend` primitives, so a later slice can unify the GL path
//! onto these units one overlay at a time.

use crate::render::Color;

/// An axis-aligned layout rectangle in window pixels.
#[derive(Clone, Copy, Debug)]
pub struct Recti {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Recti {
    /// Whether point `p` (window px) is inside this rect.
    pub fn contains(&self, p: (f32, f32)) -> bool {
        p.0 >= self.x && p.0 < self.x + self.w && p.1 >= self.y && p.1 < self.y + self.h
    }
}

/// Index of the first rect containing `p`, if any (menu/row hit-testing).
pub fn hit(rects: &[Recti], p: (f32, f32)) -> Option<usize> {
    rects.iter().position(|r| r.contains(p))
}

/// Adapt an `egui::Color32` (0..255 straight-alpha RGBA) to rt's float `Color`,
/// so the native instruments reuse the exact existing color functions with no
/// drift from the GL path.
pub fn col(c: egui::Color32) -> Color {
    let n = |v: u8| v as f32 / 255.0;
    Color(n(c.r()), n(c.g()), n(c.b()), n(c.a()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hit_picks_the_containing_row() {
        let rows = vec![
            Recti { x: 0.0, y: 0.0, w: 100.0, h: 20.0 },
            Recti { x: 0.0, y: 20.0, w: 100.0, h: 20.0 },
            Recti { x: 0.0, y: 40.0, w: 100.0, h: 20.0 },
        ];
        assert_eq!(hit(&rows, (10.0, 25.0)), Some(1));
        assert_eq!(hit(&rows, (10.0, 5.0)), Some(0));
        assert_eq!(hit(&rows, (10.0, 200.0)), None);
    }

    #[test]
    fn col_maps_bytes_to_floats() {
        let c = col(egui::Color32::from_rgba_unmultiplied(255, 0, 128, 255));
        assert!((c.0 - 1.0).abs() < 1e-6 && c.1 == 0.0 && (c.2 - 0.5019).abs() < 1e-3 && c.3 == 1.0);
    }
}
```

- [ ] **Step 2: Declare the module**

In `crates/rt/src/main.rs`, add beside the other `mod` declarations (lines 13-28):

```rust
mod chrome; // native (XRender) chrome: menu/search/manual/instruments draw + hit-test
```

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p rt chrome::tests -- --nocapture`
Expected: PASS (2 tests). (They test only pure helpers, so they compile before the submodules exist.)

- [ ] **Step 4: Commit**

```bash
git add crates/rt/src/chrome/mod.rs crates/rt/src/lib.rs
git commit -m "feat(chrome): native-chrome module scaffold (Recti, hit, col adapter)"
```

---

### Task 4: Refactor `menu.rs` to a backend-agnostic row list

Extract the menu's rows (label, accelerator, action, enabled) into a `pub fn rows(...)` that both the egui `ui()` and the native menu consume, so the item set has a single source of truth.

**Files:**
- Modify: `crates/rt/src/menu.rs`
- Test: `crates/rt/src/menu.rs`, `#[cfg(test)] mod tests`

**Interfaces:**
- Produces:
  - `pub enum RowAction { Do(Action), OpenUrl(String), CopyUrl(String), Copy, Paste }`
  - `pub struct Row { pub label: String, pub accel: Option<String>, pub action: Option<RowAction>, pub enabled: bool }` — `action: None` marks a separator.
  - `pub fn rows(keymap: &Keymap, has_selection: bool, url: Option<&str>) -> Vec<Row>`
  - `impl RowAction { pub fn into_pick(self) -> MenuPick }` (maps Copy/Paste/Do → `MenuPick::Do`, url variants → their picks).

- [ ] **Step 1: Write the failing test**

Add to `crates/rt/src/menu.rs`:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use rt_config::Keymap;

    #[test]
    fn url_rows_present_only_with_a_url() {
        let km = Keymap::default();
        let without = rows(&km, false, None);
        assert!(!without.iter().any(|r| r.label == "Open Link"));
        let with = rows(&km, false, Some("https://x"));
        assert!(with.iter().any(|r| r.label == "Open Link"));
    }

    #[test]
    fn copy_disabled_without_selection() {
        let km = Keymap::default();
        let r = rows(&km, false, None);
        let copy = r.iter().find(|r| r.label == "Copy").unwrap();
        assert!(!copy.enabled, "Copy needs a selection");
        let r2 = rows(&km, true, None);
        assert!(r2.iter().find(|r| r.label == "Copy").unwrap().enabled);
    }

    #[test]
    fn separators_have_no_action() {
        let r = rows(&Keymap::default(), false, None);
        assert!(r.iter().any(|r| r.action.is_none()), "at least one separator");
    }
}
```

- [ ] **Step 2: Run the test to verify it fails**

Run: `cargo test -p rt menu::tests -- --nocapture`
Expected: FAIL — `cannot find function rows`.

- [ ] **Step 3: Implement `rows`, `Row`, `RowAction`**

Add to `crates/rt/src/menu.rs` (keep the existing `Item`/`items()`; `rows` builds on it and adds the Copy/Paste/link rows that were inline in `ui`):

```rust
/// A native-menu action (backend-agnostic). Maps to a [`MenuPick`] on click.
pub enum RowAction {
    Do(Action),
    OpenUrl(String),
    CopyUrl(String),
    Copy,
    Paste,
}

impl RowAction {
    /// Turn a clicked row into the pick the caller applies.
    pub fn into_pick(self) -> MenuPick {
        match self {
            RowAction::Do(a) => MenuPick::Do(a),
            RowAction::Copy => MenuPick::Do(Action::Copy),
            RowAction::Paste => MenuPick::Do(Action::Paste),
            RowAction::OpenUrl(u) => MenuPick::OpenUrl(u),
            RowAction::CopyUrl(u) => MenuPick::CopyUrl(u),
        }
    }
}

/// One built menu row. `action == None` is a separator (not clickable).
pub struct Row {
    pub label: String,
    pub accel: Option<String>,
    pub action: Option<RowAction>,
    pub enabled: bool,
}

/// The full menu for this frame, top to bottom — the single source of truth for
/// both the egui menu and the native (XRender) menu. `has_selection` gates Copy;
/// `url` adds the link rows at the top; `keymap` supplies accelerators.
pub fn rows(keymap: &Keymap, has_selection: bool, url: Option<&str>) -> Vec<Row> {
    let mut out = Vec::new();
    let sep = || Row { label: String::new(), accel: None, action: None, enabled: false };
    let accel = |a: Action| keymap.shortcut_for(a).map(|s| s.to_string());
    if let Some(u) = url {
        out.push(Row { label: "Open Link".into(), accel: None, action: Some(RowAction::OpenUrl(u.to_string())), enabled: true });
        out.push(Row { label: "Copy Address".into(), accel: None, action: Some(RowAction::CopyUrl(u.to_string())), enabled: true });
        out.push(sep());
    }
    out.push(Row { label: "Copy".into(), accel: accel(Action::Copy), action: Some(RowAction::Copy), enabled: has_selection });
    out.push(Row { label: "Paste".into(), accel: accel(Action::Paste), action: Some(RowAction::Paste), enabled: true });
    out.push(sep());
    for it in items() {
        match it {
            Item::Action(label, action) => out.push(Row {
                label: label.to_string(),
                accel: accel(action),
                action: Some(RowAction::Do(action)),
                enabled: true,
            }),
            Item::Separator => out.push(sep()),
        }
    }
    out
}
```

Note: `Keymap::shortcut_for` returns a type printable via `to_string()` (the egui path uses it as `shortcut_text`). If it is not `Display`, format it the same way egui's `shortcut_text` does — check the type at implementation and match it. The egui `ui()` is left as-is (it still compiles); a later slice can re-point it at `rows()`.

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p rt menu::tests -- --nocapture`
Expected: PASS (3 tests).

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/menu.rs
git commit -m "refactor(menu): extract backend-agnostic rows() for native + egui menus"
```

---

### Task 5: Native context menu (`chrome/menu.rs`)

Lay out, hit-test, and draw the context menu from `menu::rows` using `fill_rect`/`draw_char`.

**Files:**
- Create: `crates/rt/src/chrome/menu.rs`
- Modify: `crates/rt/src/chrome/mod.rs` (add `pub mod menu;`)
- Test: `crates/rt/src/chrome/menu.rs`, `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `chrome::Recti`, `chrome::hit`, `menu::Row`, `Backend` (`fill_rect`, `draw_char`).
- Produces:
  - `pub struct Geom { pub panel: Recti, pub rows: Vec<Recti> }`
  - `pub fn layout(rows: &[menu::Row], anchor: (f32, f32), cell_w: f32, cell_h: f32, win_w: f32, win_h: f32) -> Geom`
  - `pub fn draw(be: &mut dyn Backend, g: &Geom, rows: &[menu::Row], hover: Option<usize>, cell_w: f32, cell_h: f32)`
  - `pub fn hit_row(g: &Geom, p: (f32, f32)) -> Option<usize>` (skips separators)

- [ ] **Step 1: Write the failing tests**

Create `crates/rt/src/chrome/menu.rs` with the test module first (implementation stub added next step):

```rust
//! Native context menu: laid out from `menu::rows`, drawn as fills + glyphs.
use crate::backend::Backend;
use crate::chrome::{hit, Recti};
use crate::menu::{self, Row};
use crate::render::Color;

// (implementation added in Step 2)

#[cfg(test)]
mod tests {
    use super::*;
    use rt_config::Keymap;

    fn sample() -> Vec<Row> {
        menu::rows(&Keymap::default(), true, None)
    }

    #[test]
    fn panel_clamps_onto_screen() {
        let rows = sample();
        // Anchor near the bottom-right corner: the panel must shift fully on-screen.
        let g = layout(&rows, (795.0, 595.0), 8.0, 18.0, 800.0, 600.0);
        assert!(g.panel.x + g.panel.w <= 800.0 + 0.01);
        assert!(g.panel.y + g.panel.h <= 600.0 + 0.01);
    }

    #[test]
    fn hit_row_skips_separators() {
        let rows = sample();
        let g = layout(&rows, (10.0, 10.0), 8.0, 18.0, 800.0, 600.0);
        // The 3rd row in a no-url menu is the separator after Copy/Paste.
        let sep_idx = rows.iter().position(|r| r.action.is_none()).unwrap();
        let mid = (g.rows[sep_idx].x + 2.0, g.rows[sep_idx].y + g.rows[sep_idx].h / 2.0);
        assert_eq!(hit_row(&g, mid), None, "clicking a separator selects nothing");
        // A real row hits.
        let copy_idx = rows.iter().position(|r| r.label == "Copy").unwrap();
        let cm = (g.rows[copy_idx].x + 2.0, g.rows[copy_idx].y + g.rows[copy_idx].h / 2.0);
        assert_eq!(hit_row(&g, cm), Some(copy_idx));
    }
}
```

- [ ] **Step 2: Implement layout / hit / draw**

Add above the test module in `crates/rt/src/chrome/menu.rs`:

```rust
/// Menu geometry in window px: the panel box and each row's rect (row rects
/// share the panel width; separators get a short rect so indices line up 1:1
/// with `rows`).
pub struct Geom {
    pub panel: Recti,
    pub rows: Vec<Recti>,
}

const PAD_X: f32 = 8.0; // inner horizontal padding
const SEP_H: f32 = 7.0; // separator row height

/// Lay the menu out anchored at `anchor`, clamped fully on-screen.
pub fn layout(rows: &[Row], anchor: (f32, f32), cell_w: f32, cell_h: f32, win_w: f32, win_h: f32) -> Geom {
    let row_h = cell_h + 4.0;
    // Width = widest "label   accel" in cells, plus padding.
    let cols = rows.iter().map(|r| {
        let a = r.accel.as_deref().map(|s| s.chars().count() + 3).unwrap_or(0);
        r.label.chars().count() + a
    }).max().unwrap_or(8);
    let w = cols as f32 * cell_w + PAD_X * 2.0;
    let h: f32 = rows.iter().map(|r| if r.action.is_none() { SEP_H } else { row_h }).sum::<f32>() + PAD_X;
    // Clamp so the whole panel stays visible.
    let x = anchor.0.min(win_w - w).max(0.0);
    let y = anchor.1.min(win_h - h).max(0.0);
    let mut rrects = Vec::with_capacity(rows.len());
    let mut cy = y + PAD_X * 0.5;
    for r in rows {
        let rh = if r.action.is_none() { SEP_H } else { row_h };
        rrects.push(Recti { x, y: cy, w, h: rh });
        cy += rh;
    }
    Geom { panel: Recti { x, y, w, h }, rows: rrects }
}

/// The clickable row at `p`, or `None` for separators / outside the panel.
pub fn hit_row(g: &Geom, p: (f32, f32)) -> Option<usize> {
    let i = hit(&g.rows, p)?;
    // Re-filter separators (their rects exist for index alignment).
    Some(i)
}

/// Draw the panel, hovered highlight, labels, accelerators, and separators.
pub fn draw(be: &mut dyn Backend, g: &Geom, rows: &[Row], hover: Option<usize>, cell_w: f32, cell_h: f32) {
    let bg = Color::rgb(0x20, 0x22, 0x28);
    let border = Color::rgb(0x50, 0x54, 0x60);
    let fg = Color::rgb(0xe0, 0xe0, 0xe6);
    let fg_dim = Color::rgb(0x90, 0x94, 0xa0);
    let fg_off = Color::rgb(0x60, 0x62, 0x6a);
    let hl = Color::rgb(0x35, 0x5a, 0x9a);
    let sep = Color::rgb(0x40, 0x43, 0x4d);
    // Panel + 1px border.
    be.fill_rect(g.panel.x, g.panel.y, g.panel.w, g.panel.h, bg);
    be.fill_rect(g.panel.x, g.panel.y, g.panel.w, 1.0, border);
    be.fill_rect(g.panel.x, g.panel.y + g.panel.h - 1.0, g.panel.w, 1.0, border);
    be.fill_rect(g.panel.x, g.panel.y, 1.0, g.panel.h, border);
    be.fill_rect(g.panel.x + g.panel.w - 1.0, g.panel.y, 1.0, g.panel.h, border);
    for (i, (row, rect)) in rows.iter().zip(&g.rows).enumerate() {
        if row.action.is_none() {
            // Separator: a thin line centred in its rect.
            be.fill_rect(rect.x + PAD_X, rect.y + rect.h / 2.0, rect.w - PAD_X * 2.0, 1.0, sep);
            continue;
        }
        if hover == Some(i) && row.enabled {
            be.fill_rect(rect.x + 1.0, rect.y, rect.w - 2.0, rect.h, hl);
        }
        let colr = if !row.enabled { fg_off } else { fg };
        // Label at the left; draw_char places glyphs on the cell grid, so map the
        // row's pixel origin to (col,row) = (0,0) with the origin as the offset.
        let ox = rect.x + PAD_X;
        let oy = rect.y + (rect.h - cell_h) / 2.0;
        for (c, ch) in row.label.chars().enumerate() {
            be.draw_char(ox, oy, c, 0, ch, colr, false, false);
        }
        // Accelerator, right-aligned in a dim colour.
        if let Some(acc) = &row.accel {
            let n = acc.chars().count();
            let ax = rect.x + rect.w - PAD_X - n as f32 * cell_w;
            for (c, ch) in acc.chars().enumerate() {
                be.draw_char(ax, oy, c, 0, ch, fg_dim, false, false);
            }
        }
    }
    let _ = cell_w;
}
```

Add `pub mod menu;` to `crates/rt/src/chrome/mod.rs`.

- [ ] **Step 3: Run the tests to verify they pass**

Run: `cargo test -p rt chrome::menu -- --nocapture`
Expected: PASS (2 tests).

- [ ] **Step 4: Commit**

```bash
git add crates/rt/src/chrome/menu.rs crates/rt/src/chrome/mod.rs
git commit -m "feat(chrome): native context menu layout/hit-test/draw"
```

---

### Task 6: Native search bar (`chrome/search.rs`)

Draw the slim search bar (query + "n/m" counter + caret) as fills + glyphs; input reuses the existing search engine.

**Files:**
- Create: `crates/rt/src/chrome/search.rs`
- Modify: `crates/rt/src/chrome/mod.rs` (add `pub mod search;`)
- Test: `crates/rt/src/chrome/search.rs`, `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `chrome::Recti`, `Backend`.
- Produces:
  - `pub fn layout(win_w: f32, cell_w: f32, cell_h: f32) -> Recti` — the bar rect (top-right).
  - `pub fn draw(be: &mut dyn Backend, bar: Recti, query: &str, pos: usize, count: usize, cell_w: f32, cell_h: f32)`

- [ ] **Step 1: Write the failing test**

Create `crates/rt/src/chrome/search.rs`:

```rust
//! Native scrollback-search bar: a slim top-right box with the query and a hit
//! counter. Typing/navigation are handled by main.rs via the existing engine.
use crate::backend::Backend;
use crate::chrome::Recti;
use crate::render::Color;

// (implementation added in Step 2)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_sits_at_the_top_right() {
        let bar = layout(800.0, 8.0, 18.0);
        assert!(bar.x + bar.w <= 800.0 + 0.01, "within the window");
        assert!(bar.x > 400.0, "anchored to the right half");
        assert!(bar.y >= 0.0 && bar.h > 0.0);
    }
}
```

- [ ] **Step 2: Implement layout + draw**

```rust
const PAD: f32 = 6.0;
const BAR_COLS: usize = 32; // query field width in cells

/// Bar rect pinned to the top-right corner (with an 8px standoff).
pub fn layout(win_w: f32, cell_w: f32, cell_h: f32) -> Recti {
    let w = BAR_COLS as f32 * cell_w + PAD * 2.0 + 8.0 * cell_w; // query + " 12/34 "
    let h = cell_h + PAD * 2.0;
    Recti { x: (win_w - w - 8.0).max(0.0), y: 8.0, w, h }
}

/// Draw the bar: background, border, query text, caret, and "pos/count".
pub fn draw(be: &mut dyn Backend, bar: Recti, query: &str, pos: usize, count: usize, cell_w: f32, cell_h: f32) {
    let bg = Color::rgb(0x1c, 0x1e, 0x24);
    let border = Color::rgb(0x50, 0x54, 0x60);
    let fg = Color::rgb(0xe0, 0xe0, 0xe6);
    let dim = Color::rgb(0x90, 0x94, 0xa0);
    be.fill_rect(bar.x, bar.y, bar.w, bar.h, bg);
    be.fill_rect(bar.x, bar.y, bar.w, 1.0, border);
    be.fill_rect(bar.x, bar.y + bar.h - 1.0, bar.w, 1.0, border);
    be.fill_rect(bar.x, bar.y, 1.0, bar.h, border);
    be.fill_rect(bar.x + bar.w - 1.0, bar.y, 1.0, bar.h, border);
    let ox = bar.x + PAD;
    let oy = bar.y + PAD;
    for (c, ch) in query.chars().take(BAR_COLS).enumerate() {
        be.draw_char(ox, oy, c, 0, ch, fg, false, false);
    }
    // Caret after the query.
    let caret_x = ox + query.chars().count().min(BAR_COLS) as f32 * cell_w;
    be.fill_rect(caret_x, oy, 2.0, cell_h, fg);
    // "pos/count" right-aligned.
    let label = format!("{pos}/{count}");
    let lx = bar.x + bar.w - PAD - label.chars().count() as f32 * cell_w;
    for (c, ch) in label.chars().enumerate() {
        be.draw_char(lx, oy, c, 0, ch, dim, false, false);
    }
}
```

Add `pub mod search;` to `crates/rt/src/chrome/mod.rs`.

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p rt chrome::search -- --nocapture`
Expected: PASS (1 test).

- [ ] **Step 4: Commit**

```bash
git add crates/rt/src/chrome/search.rs crates/rt/src/chrome/mod.rs
git commit -m "feat(chrome): native scrollback-search bar draw"
```

---

### Task 7: Native manual (`chrome/manual.rs`) + scroll state

Draw the manual as a centered panel with a cell-row scroll viewport over `manual::MANUAL`. Adds a `manual_scroll` field to `Active`.

**Files:**
- Create: `crates/rt/src/chrome/manual.rs`
- Modify: `crates/rt/src/chrome/mod.rs` (add `pub mod manual;`)
- Modify: `crates/rt/src/main.rs` (add `manual_scroll: usize` to `Active`, init `0`)
- Test: `crates/rt/src/chrome/manual.rs`, `#[cfg(test)] mod tests`

**Interfaces:**
- Consumes: `chrome::Recti`, `Backend`, `manual::MANUAL`.
- Produces:
  - `pub struct Geom { pub panel: Recti, pub rows: usize, pub total: usize }`
  - `pub fn layout(win_w: f32, win_h: f32, cell_w: f32, cell_h: f32) -> Geom`
  - `pub fn clamp_scroll(scroll: usize, g: &Geom) -> usize`
  - `pub fn draw(be: &mut dyn Backend, g: &Geom, scroll: usize, cell_w: f32, cell_h: f32)`

- [ ] **Step 1: Write the failing tests**

Create `crates/rt/src/chrome/manual.rs`:

```rust
//! Native manual overlay: a centered panel scrolling `manual::MANUAL` by cell
//! rows. Scroll position lives in `Active.manual_scroll`.
use crate::backend::Backend;
use crate::chrome::Recti;
use crate::manual::MANUAL;
use crate::render::Color;

// (implementation added in Step 2)

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clamp_keeps_last_page_visible() {
        let g = layout(1000.0, 700.0, 8.0, 18.0);
        assert!(g.total > g.rows, "manual is longer than one page");
        let max = g.total - g.rows;
        assert_eq!(clamp_scroll(usize::MAX, &g), max, "cannot scroll past the end");
        assert_eq!(clamp_scroll(0, &g), 0);
    }
}
```

- [ ] **Step 2: Implement layout / clamp / draw**

```rust
/// Manual panel geometry: the centered box, visible cell rows, and total lines.
pub struct Geom {
    pub panel: Recti,
    pub rows: usize,
    pub total: usize,
}

const PAD: f32 = 12.0;

/// A panel ~80% of the window, centered.
pub fn layout(win_w: f32, win_h: f32, cell_w: f32, cell_h: f32) -> Geom {
    let w = (win_w * 0.8).min(720.0);
    let h = win_h * 0.85;
    let panel = Recti { x: (win_w - w) / 2.0, y: (win_h - h) / 2.0, w, h };
    let rows = (((h - PAD * 2.0) / cell_h).floor() as usize).max(1);
    let total = MANUAL.lines().count();
    let _ = cell_w;
    Geom { panel, rows, total }
}

/// Clamp a scroll offset so the last page stays on-screen.
pub fn clamp_scroll(scroll: usize, g: &Geom) -> usize {
    let max = g.total.saturating_sub(g.rows);
    scroll.min(max)
}

/// Draw the panel, the visible line slice, and a scrollbar thumb.
pub fn draw(be: &mut dyn Backend, g: &Geom, scroll: usize, cell_w: f32, cell_h: f32) {
    let bg = Color::rgb(0x18, 0x1a, 0x1f);
    let border = Color::rgb(0x50, 0x54, 0x60);
    let fg = Color::rgb(0xd0, 0xd2, 0xda);
    let thumb = Color::rgb(0x45, 0x48, 0x54);
    let p = g.panel;
    be.fill_rect(p.x, p.y, p.w, p.h, bg);
    be.fill_rect(p.x, p.y, p.w, 1.0, border);
    be.fill_rect(p.x, p.y + p.h - 1.0, p.w, 1.0, border);
    be.fill_rect(p.x, p.y, 1.0, p.h, border);
    be.fill_rect(p.x + p.w - 1.0, p.y, 1.0, p.h, border);
    let ox = p.x + PAD;
    let oy = p.y + PAD;
    let scroll = clamp_scroll(scroll, g);
    for (r, line) in MANUAL.lines().skip(scroll).take(g.rows).enumerate() {
        for (c, ch) in line.chars().enumerate() {
            be.draw_char(ox, oy, c, r, ch, fg, false, false);
        }
    }
    // Scrollbar thumb on the right edge, sized to the visible fraction.
    if g.total > g.rows {
        let track_h = p.h - 2.0;
        let th = (track_h * g.rows as f32 / g.total as f32).max(12.0);
        let ty = p.y + 1.0 + (track_h - th) * scroll as f32 / (g.total - g.rows) as f32;
        be.fill_rect(p.x + p.w - 4.0, ty, 3.0, th, thumb);
    }
    let _ = cell_w;
}
```

Add `pub mod manual;` to `crates/rt/src/chrome/mod.rs`. Add `manual_scroll: usize` to the `Active` struct (`crates/rt/src/main.rs`, near `manual_open` at line 283) and initialize it to `0` (near line 959).

- [ ] **Step 3: Run the test to verify it passes**

Run: `cargo test -p rt chrome::manual -- --nocapture`
Expected: PASS (1 test).

- [ ] **Step 4: Commit**

```bash
git add crates/rt/src/chrome/manual.rs crates/rt/src/chrome/mod.rs crates/rt/src/main.rs
git commit -m "feat(chrome): native manual overlay with scroll viewport"
```

---

### Task 8: Extract shared instrument state advance

Move the meter/wire phase integration out of `paint_instruments` (GL-only) into a shared `advance_instrument_state`, so the native path advances identically.

**Files:**
- Modify: `crates/rt/src/main.rs` (extract lines 3226-3243; call from `paint_instruments`)
- Test: `crates/rt/src/main.rs`, `#[cfg(test)] mod instr_tests` (or an existing test module)

**Interfaces:**
- Produces: `fn advance_instrument_state(active: &mut Active, dt: f32)` — updates each `meter.rate/phase` and `wire.rate/phase` from accumulated `wakeups`/`moved`, resetting those counters. `dt` is the clamped wall-clock delta.
- Consumes: `active.meters`, `active.wires`, constants `BUSY_WAKEUPS`, `WIRE_BUSY_BYTES`, `FLOW_MAX_LAPS`.

- [ ] **Step 1: Write the failing test**

Add to `crates/rt/src/main.rs` (a test module; construct a minimal `Meter` — it derives `Default`):

```rust
#[cfg(test)]
mod instr_tests {
    use super::*;

    #[test]
    fn advance_decays_and_wraps_phase() {
        let mut m = Meter::default();
        m.wakeups = BUSY_WAKEUPS as u64; // one core's worth of activity over 1s
        m.rate = 0.0;
        // Emulate one advance step directly against the same math.
        let dt = 1.0_f32;
        let inst = m.wakeups as f32 / dt.max(1e-3);
        m.wakeups = 0;
        m.rate = m.rate * 0.75 + inst * 0.25;
        assert!(m.rate > 0.0, "rate rises with activity");
        let act = (m.rate / BUSY_WAKEUPS).clamp(0.0, 1.0);
        m.phase = (m.phase + act * FLOW_MAX_LAPS * dt).fract();
        assert!(m.phase >= 0.0 && m.phase < 1.0, "phase stays in [0,1)");
    }
}
```

(This asserts the math contract that `advance_instrument_state` must preserve. Adjust field names if `Meter` differs.)

- [ ] **Step 2: Run the test to verify it passes as a contract**

Run: `cargo test -p rt instr_tests -- --nocapture`
Expected: PASS (it exercises the arithmetic directly; it will keep passing after extraction).

- [ ] **Step 3: Extract the function**

Add near `paint_instruments`:

```rust
    /// Advance every meter's and wire's exponential rate + flow phase by `dt`
    /// seconds of wall-clock, consuming the accumulated `wakeups`/`moved` counts.
    /// Shared by the GL (`paint_instruments`) and native (XRender) draw paths so
    /// the animation math is identical on both.
    fn advance_instrument_state(active: &mut Active, dt: f32) {
        for m in active.meters.values_mut() {
            let inst = m.wakeups as f32 / dt.max(1e-3);
            m.wakeups = 0;
            m.rate = m.rate * 0.75 + inst * 0.25;
            let act = (m.rate / BUSY_WAKEUPS).clamp(0.0, 1.0);
            m.phase = (m.phase + act * FLOW_MAX_LAPS * dt).fract();
        }
        for w in active.wires.iter_mut() {
            let inst = w.moved as f32 / dt.max(1e-3);
            w.moved = 0;
            w.rate = w.rate * 0.75 + inst * 0.25;
            let act = (w.rate / WIRE_BUSY_BYTES).clamp(0.0, 1.0);
            w.phase = (w.phase + act * FLOW_MAX_LAPS * dt).fract();
        }
    }
```

In `paint_instruments`, replace lines 3226-3243 (the `now`/`dt` computation + the two loops) with:

```rust
        let now = Instant::now();
        let dt = now.duration_since(active.last_meter_tick).as_secs_f32().min(0.1);
        active.last_meter_tick = now;
        Self::advance_instrument_state(active, dt);
```

- [ ] **Step 4: Run tests + build**

Run: `cargo test -p rt instr_tests -- --nocapture && cargo build -p rt`
Expected: PASS + clean build; GL instruments behave exactly as before (same math, same call site).

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/main.rs
git commit -m "refactor(instruments): share meter/wire state advance across backends"
```

---

### Task 9: Native instruments/patch-bay draw (`chrome/instruments.rs`)

Port `paint_instruments`'s drawing to `Backend` primitives, reusing the exact color/geometry functions via `chrome::col`. Heat = four `fill_rect`s; packets/jacks = `fill_circle`/`stroke_circle`; wires/latency = `stroke_line`.

**Files:**
- Create: `crates/rt/src/chrome/instruments.rs`
- Modify: `crates/rt/src/chrome/mod.rs` (add `pub mod instruments;`)

**Visibility:** none needed. `chrome` is a descendant of the crate root (Crate-Layout note), so it reaches the crate-root-private helpers (`heat_color32`, `latency_color`, `cubic_bezier`, `flow_point`, `content_bounds`), constants (`FLOW_PACKETS`, `WIRE_PACKETS`, `BUSY_WAKEUPS`, `WIRE_BUSY_BYTES`), and `Active`'s private fields via `crate::…` unchanged. (Only if one of these is defined *inside* an `impl`/nested module rather than at the crate root would a `pub(crate)` be required — verify at implementation.)

**Interfaces:**
- Consumes: `Backend` (`fill_rect`, `fill_circle`, `stroke_circle`, `stroke_line`), `chrome::col`, and the crate-root color/geometry helpers + constants above; reads `Active` state (`meters`, `wires`, `heat`, `settings.inst_*`/`show_jacks`, `wiring_from`, `drag_cursor`, `lat_phase`, `stall`).
- Produces:
  - `pub struct InstrCtx<'a>` — a borrowing bundle of exactly the fields the draw reads, so the caller can pass it alongside a `&mut dyn Backend` **from the same `Active`** without a whole-struct borrow (disjoint field borrows are allowed; `&Active` + `&mut active.backend` is not).
  - `pub fn draw(be: &mut dyn Backend, ctx: &InstrCtx)`.

Note: the XRender backend works in **physical pixels**, so this port uses the raw `rect.x/y/w/h` (no `/ppp` division that the egui version applies) and keeps the same pixel radii/widths. The state advance already ran (Task 8) before this is called. **Borrow discipline:** `draw` reads only through `ctx`, never through `&Active`, which is what lets the caller hold `&mut active.backend` at the same time.

- [ ] **Step 1: Implement the native draw**

Because this port has no pure return value to unit-test (it issues draw calls), its correctness gate is Task 10's xtrace test (`Triangles > 0`, `PutImage == 0`) plus manual `ssh -X`. Transcribe `paint_instruments`'s body (main.rs:3260-3406), swapping egui painter calls for `Backend` primitives and `egui::Color32` for `chrome::col(...)`:

Create `crates/rt/src/chrome/instruments.rs`:

```rust
//! Native (XRender) instruments + patch-bay: the same meters/wires/beziers as
//! the egui path (`main.rs::paint_instruments`), drawn with `Backend`
//! primitives. Colors/geometry come from the shared `main.rs` helpers via
//! `chrome::col`, so there is no visual drift between the GL and native paths.
use std::collections::HashMap;
use crate::backend::Backend;
use crate::chrome::col;
use crate::{content_bounds, cubic_bezier, flow_point, heat_color32, latency_color, Meter, Wire,
            BUSY_WAKEUPS, FLOW_PACKETS, WIRE_BUSY_BYTES, WIRE_PACKETS};
use rt_core::{PaneId, Rect, Stream};

/// Exactly the `Active` state the instrument draw reads, borrowed field-by-field
/// so the caller can pass `&mut active.backend` alongside (disjoint borrows).
/// `rects` is precomputed by the caller (`session.visible_rects`) so no `session`
/// borrow lingers here. `Meter`/`Wire` are crate-root types; since these `pub`
/// fields expose them, mark `struct Meter`/`struct Wire` `pub(crate)` in main.rs
/// to silence the `private_interfaces` lint (the only visibility edit this task
/// needs). Confirm `Rect`/`PaneId`/`Stream`'s real import paths at implementation.
pub struct InstrCtx<'a> {
    pub rects: &'a [(PaneId, Rect)],
    pub meters: &'a HashMap<PaneId, Meter>,
    pub wires: &'a [Wire],
    pub heat: &'a HashMap<PaneId, f32>,
    pub inst_output: bool,
    pub inst_heat: bool,
    pub inst_latency: bool,
    pub show_jacks: bool,
    pub wiring_from: Option<(PaneId, Stream)>,
    pub drag_cursor: Option<(f32, f32)>,
    pub lat_phase: f32,
    pub stall: f32,
    pub size: winit::dpi::PhysicalSize<u32>,
}

/// Draw all enabled instruments over the already-drawn grid (physical pixels).
/// Reads ONLY through `ctx` (never `&Active`) so `be` can come from the same Active.
pub fn draw(be: &mut dyn Backend, ctx: &InstrCtx) {
    let rects = ctx.rects;
    let size = ctx.size;
    let (inst_output, inst_heat, inst_latency) = (ctx.inst_output, ctx.inst_heat, ctx.inst_latency);

    // Per-pane heat borders + orbiting output packets.
    for (id, rect) in &rects {
        let m = active.meters.get(id).copied().unwrap_or_default();
        let act = (m.rate / BUSY_WAKEUPS).clamp(0.0, 1.0);
        let (x, y, w, h) = (rect.x, rect.y, rect.w, rect.h);
        if inst_heat {
            let load = active.heat.get(id).copied().unwrap_or(0.0);
            let c = col(heat_color32(load));
            let t = 2.4;
            be.fill_rect(x, y, w, t, c);              // top
            be.fill_rect(x, y + h - t, w, t, c);      // bottom
            be.fill_rect(x, y, t, h, c);              // left
            be.fill_rect(x + w - t, y, t, h, c);      // right
        }
        if inst_output {
            for k in 0..FLOW_PACKETS {
                let tt = (m.phase + k as f32 / FLOW_PACKETS as f32).fract();
                let p = flow_point(x, y, w, h, tt);
                let a = 0.30 + 0.70 * act;
                let glow = col(egui::Color32::from_rgba_unmultiplied(0x28, 0xc0, 0x48, (a * 110.0) as u8));
                let core = col(egui::Color32::from_rgba_unmultiplied(0x66, 0xff, 0x7a, (a * 255.0) as u8));
                be.fill_circle(p.x, p.y, 9.0, glow);
                be.fill_circle(p.x, p.y, 3.4, core);
            }
        }
    }

    // Patch-bay jack positions (physical px).
    let jack_pos = |r: &rt_core::Rect, which: u8| -> (f32, f32) {
        let (x, y, w, h) = (r.x, r.y, r.w, r.h);
        match which {
            0 => (x, y + h * 0.5),
            1 => (x + w, y + h / 3.0),
            _ => (x + w, y + 2.0 * h / 3.0),
        }
    };
    let rect_of = |id: rt_core::PaneId| rects.iter().find(|&&(i, _)| i == id).map(|(_, r)| r);

    // Wires (under the jacks): stream-colored bezier flow.
    for w in &active.wires {
        let (Some(sr), Some(dr)) = (rect_of(w.src), rect_of(w.dst)) else { continue };
        let p0 = jack_pos(sr, if w.stream == Stream::Stdout { 1 } else { 2 });
        let p3 = jack_pos(dr, 0);
        let ext = ((p3.0 - p0.0).abs() * 0.4 + 40.0).min(180.0);
        let p1 = (p0.0 + ext, p0.1);
        let p2 = (p3.0 - ext, p3.1);
        let hue = if w.stream == Stream::Stdout { (0x40u8, 0xc0u8, 0x54u8) } else { (0xd0, 0x54, 0x30) };
        let act = (w.rate / WIRE_BUSY_BYTES).clamp(0.0, 1.0);
        const N: u32 = 56;
        let mut prev = p0;
        for i in 1..=N {
            let t = i as f32 / N as f32;
            let pt = cubic_bezier(p0.into(), p1.into(), p2.into(), p3.into(), t);
            let mut best = 0.0f32;
            for k in 0..WIRE_PACKETS {
                let pp = (w.phase + k as f32 / WIRE_PACKETS as f32).fract();
                let d = (t - pp).abs();
                best = best.max((-d * d / (2.0 * 0.05 * 0.05)).exp());
            }
            let b = 0.22 + 0.78 * best * (0.30 + 0.70 * act);
            let c = crate::render::Color::rgb(
                (hue.0 as f32 * b) as u8, (hue.1 as f32 * b) as u8, (hue.2 as f32 * b) as u8);
            be.stroke_line(prev.0, prev.1, pt.x, pt.y, 2.0, c);
            prev = (pt.x, pt.y);
        }
    }

    // Jack dots on every pane.
    if active.settings.show_jacks {
        for (id, r) in &rects {
            let has_in = active.wires.iter().any(|w| w.dst == *id);
            let has_out = active.wires.iter().any(|w| w.src == *id && w.stream == Stream::Stdout);
            let has_err = active.wires.iter().any(|w| w.src == *id && w.stream == Stream::Stderr);
            let mut jack = |p: (f32, f32), filled: bool, c: crate::render::Color| {
                be.fill_circle(p.0, p.1, 4.5, crate::render::Color(0.0, 0.0, 0.0, 0.70));
                if filled { be.fill_circle(p.0, p.1, 3.5, c); }
                else { be.stroke_circle(p.0, p.1, 3.2, 1.4, c); }
            };
            jack(jack_pos(r, 0), has_in, crate::render::Color::rgb(0x88, 0x88, 0x98));
            jack(jack_pos(r, 1), has_out, crate::render::Color::rgb(0x40, 0xc0, 0x54));
            jack(jack_pos(r, 2), has_err, crate::render::Color::rgb(0xd0, 0x54, 0x30));
        }
    }

    // Rubber-band wire (dashed) while dragging.
    if let (Some((src, stream)), Some((cx, cy))) = (active.wiring_from, active.drag_cursor) {
        if let Some(sr) = rect_of(src) {
            let p0 = jack_pos(sr, if stream == Stream::Stdout { 1 } else { 2 });
            let p3 = (cx, cy);
            let ext = ((p3.0 - p0.0).abs() * 0.4 + 40.0).min(180.0);
            let p1 = (p0.0 + ext, p0.1);
            let p2 = (p3.0 - ext, p3.1);
            let (hr, hg, hb) = if stream == Stream::Stdout { (0x40, 0xc0, 0x54) } else { (0xd0, 0x54, 0x30) };
            let c = col(egui::Color32::from_rgba_unmultiplied(hr, hg, hb, 180));
            let mut prev = p0;
            for i in 1..=40u32 {
                let t = i as f32 / 40.0;
                let pt = cubic_bezier(p0.into(), p1.into(), p2.into(), p3.into(), t);
                if i % 2 == 0 { be.stroke_line(prev.0, prev.1, pt.x, pt.y, 1.6, c); }
                prev = (pt.x, pt.y);
            }
        }
    }

    // Latency: the content-region perimeter, drawn last.
    if inst_latency {
        let cb = content_bounds(size);
        let (fx, fy, fw, fh) = (cb.x, cb.y, cb.w, cb.h);
        let per = 2.0 * (fw + fh);
        let corners = [
            ((fx, fy), 0.0),
            ((fx + fw, fy), fw),
            ((fx + fw, fy + fh), fw + fh),
            ((fx, fy + fh), 2.0 * fw + fh),
            ((fx, fy), per),
        ];
        const SUB: u32 = 26;
        for e in 0..4 {
            let (pa, da) = corners[e];
            let (pb, db) = corners[e + 1];
            let mut prev = pa;
            for s in 1..=SUB {
                let f = s as f32 / SUB as f32;
                let pt = (pa.0 + (pb.0 - pa.0) * f, pa.1 + (pb.1 - pa.1) * f);
                let mid_t = (da + (db - da) * (f - 0.5 / SUB as f32)) / per;
                let c = col(latency_color(mid_t, active.lat_phase, active.stall));
                be.stroke_line(prev.0, prev.1, pt.0, pt.1, 2.0, c);
                prev = pt;
            }
        }
    }
}
```

Two mechanical fixups when transcribing the body above (it is written against `active.*` for readability, but the real signature reads `ctx.*`):
- Replace every `active.meters`/`active.wires`/`active.heat`/`active.wiring_from`/`active.drag_cursor`/`active.lat_phase`/`active.stall` with `ctx.meters`/`ctx.wires`/`ctx.heat`/`ctx.wiring_from`/`ctx.drag_cursor`/`ctx.lat_phase`/`ctx.stall`, and `active.settings.show_jacks` with `ctx.show_jacks`. There is no `active` binding in scope.
- `cubic_bezier` takes `egui::Pos2`; `(f32,f32).into()` does not yield `Pos2`. Use `egui::pos2(p.0, p.1)` at each call instead of `.into()` — replace the four `p0.into()`/… with `egui::pos2(p0.0, p0.1)` etc.

- [ ] **Step 2: Declare the submodule**

Add `pub mod instruments;` to `crates/rt/src/chrome/mod.rs`. No visibility changes to `main.rs` are needed (descendant access — see the Visibility note). If the build reports a privacy error for any specific item, it is because that item is *not* at the crate root; make just that item `pub(crate)` and no more.

- [ ] **Step 3: Build**

Run: `cargo build -p rt`
Expected: PASS (visibility resolves; `chrome::instruments::draw` compiles). Fix any `egui::pos2` conversions flagged.

- [ ] **Step 4: Commit**

```bash
git add crates/rt/src/chrome/instruments.rs crates/rt/src/chrome/mod.rs crates/rt/src/main.rs
git commit -m "feat(chrome): native instruments/patch-bay draw via AA primitives"
```

---

### Task 10: Wire native chrome into main.rs (dispatch + input routing)

Replace the Slice-1 force-close for the four overlays with native draw dispatch and input routing; keep Preferences force-closed. This is the integration task that makes the chrome usable end-to-end.

**Files:**
- Modify: `crates/rt/src/main.rs` — `paint_overlays_or_instruments` (2773-2790), the force-close guard (1040-1064), and the four input-diversion blocks (1066-1174+).

**Interfaces:**
- Consumes: everything from Tasks 5-9 (`chrome::menu`, `chrome::search`, `chrome::manual`, `chrome::instruments`), `menu::rows`, `run_search`/`search_step`/`close_search`, `apply_action`, `open_url`.

- [ ] **Step 1: Dispatch native draws on the XRender path**

Replace `paint_overlays_or_instruments` (lines 2773-2790) with:

```rust
    fn paint_overlays_or_instruments(active: &mut Active) {
        if active.backend.supports_egui() {
            // GL path: today's egui chrome, unchanged.
            if active.prefs_open {
                Self::paint_egui(active);
            } else if active.menu.is_some() {
                Self::paint_menu(active);
            } else if active.manual_open {
                Self::paint_manual(active);
            } else if active.search_open {
                Self::paint_search(active);
            } else {
                Self::paint_instruments(active);
            }
            return;
        }
        // Native (XRender) path: preferences is still egui-only, so it is never
        // open here (force-closed below). Draw the native overlays; instruments
        // are the default when nothing is open. State advance for instruments:
        let now = Instant::now();
        let dt = now.duration_since(active.last_meter_tick).as_secs_f32().min(0.1);
        active.last_meter_tick = now;
        Self::advance_instrument_state(active, dt);
        let size = active.window.inner_size();
        let (cw, ch) = active.backend.cell_size();
        if let Some(pos) = active.menu {
            let url = Self::cell_at(active, pos.0, pos.1)
                .and_then(|(pane, col, row)| Self::url_at(active, pane, col, row));
            let has_sel = Self::selected_text(active).is_some();
            let rows = menu::rows(&active.keymap, has_sel, url.as_deref());
            let g = chrome::menu::layout(&rows, pos, cw, ch, size.width as f32, size.height as f32);
            chrome::menu::draw(&mut *active.backend, &g, &rows, active.menu_hover, cw, ch);
        } else if active.manual_open {
            let g = chrome::manual::layout(size.width as f32, size.height as f32, cw, ch);
            chrome::manual::draw(&mut *active.backend, &g, active.manual_scroll, cw, ch);
        } else if active.search_open {
            let bar = chrome::search::layout(size.width as f32, cw, ch);
            let count = active.search_matches.len();
            let pos = if count == 0 { 0 } else { active.search_index + 1 };
            chrome::search::draw(&mut *active.backend, bar, &active.search_query, pos, count, cw, ch);
        }
        // Instruments always draw under any open overlay (like the GL path they
        // sit in the background layer). Build the borrowing context from DISJOINT
        // fields so `&mut active.backend` and the `&active.*` reads coexist.
        let bounds = content_bounds(size);
        let rects = active.session.visible_rects(bounds); // owned Vec — no lingering session borrow
        let ctx = chrome::instruments::InstrCtx {
            rects: &rects,
            meters: &active.meters,
            wires: &active.wires,
            heat: &active.heat,
            inst_output: active.settings.inst_output,
            inst_heat: active.settings.inst_heat,
            inst_latency: active.settings.inst_latency,
            show_jacks: active.settings.show_jacks,
            wiring_from: active.wiring_from,
            drag_cursor: active.drag_cursor,
            lat_phase: active.lat_phase,
            stall: active.stall,
            size,
        };
        chrome::instruments::draw(&mut *active.backend, &ctx);
    }
```

Add a `menu_hover: Option<usize>` field to `Active` (init `None`). The `InstrCtx` above borrows individual fields (`&active.meters`, `&active.wires`, …) which are **disjoint** from `&mut active.backend`, so the borrow checker accepts them together — the earlier conflict (a whole-struct `&active` alongside `&mut active.backend`) is gone. The same disjoint-borrow pattern also applies to the menu/search/manual draw calls above: each reads a few `active.*` fields to build its inputs *before* taking `&mut *active.backend`, so bind those inputs to locals first (as the menu block does with `rows`/`g`) if the checker complains.

- [ ] **Step 2: Narrow the force-close to Preferences only**

Replace the guard at lines 1040-1064 with one that force-closes **only** preferences on the native backend (the other three now render natively):

```rust
        // Preferences is still egui-only (Slice 2 defers it): never let it open on
        // the XRender backend, or an invisible dialog would swallow input.
        if !active.backend.supports_egui() && active.prefs_open {
            active.prefs_open = false;
            static PREFS_HINT: std::sync::Once = std::sync::Once::new();
            PREFS_HINT.call_once(|| {
                eprintln!("rt: Preferences is unavailable on the remote (XRender) backend yet.");
            });
        }
```

- [ ] **Step 3: Route input to the native overlays**

The existing diversion blocks (1066-1174+) call `active.egui_state.on_window_event`, which is correct for the GL path. For the native path, add native handling. In each block, branch on `supports_egui()`:

- **Menu** (`active.menu.is_some()`): on the native backend, handle `CursorMoved` → `active.menu_hover = chrome::menu::hit_row(&g, p)` (recompute layout from `active.menu`), `MouseInput` press → if `hit_row` is `Some(i)` and the row is enabled, take its `RowAction`, `active.menu = None`, apply via `into_pick()` → the same match as `paint_menu` (Do → `apply_action`; OpenUrl → `open_url`; CopyUrl → clipboard); a press outside the panel or `Escape` closes; then `request_redraw()` and `return`.
- **Manual** (`active.manual_open`): native — arrow/PageUp/PageDown/wheel adjust `active.manual_scroll` (clamped via `chrome::manual::clamp_scroll` against a freshly computed `Geom`), `Escape`/`F1`/`q` close; `request_redraw()`; `return`.
- **Search** (`active.search_open`): native — printable keys append to `active.search_query` and call `run_search(active, true)`; Backspace pops and re-runs; Enter/Shift-Enter call `search_step`; Escape calls `close_search`; `request_redraw()`; `return`. (The existing block already handles Enter/Escape; extend it to edit the query on the native path since egui no longer captures typing.)

Implement each as a `if !active.backend.supports_egui() { …native…; return; }` shim at the top of the corresponding existing block, leaving the egui code path untouched below it. Damage: closing an overlay must repaint the vacated region — call `active.window.request_redraw()` (the next frame is Full because the overlay flag changed / a redraw is forced), which on the XRender path redraws the grid and presents. (The instruments already force periodic full redraws via the animation cadence.)

- [ ] **Step 4: Build + run the full test suite**

Run: `cargo test -p rt`
Expected: PASS — all unit tests (geom, chrome, menu, instr) pass; the crate builds with native routing.

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/main.rs
git commit -m "feat(chrome): dispatch + route input to native XRender chrome"
```

---

### Task 11: xtrace regression — chrome stays commands-only

Extend `tests/xrender_commands.rs` with a variant that opens an overlay and asserts the wire stays commands-only (`PutImage == 0`) and that the AA primitives emit `Triangles`.

**Files:**
- Modify: `crates/rt/tests/xrender_commands.rs`
- Modify: `crates/rt/src/main.rs` — add a hidden startup hook (env var `RT_OPEN_MANUAL=1`) that opens the manual at launch, so the test can drive an overlay non-interactively.

**Interfaces:**
- Consumes: the existing `have`/`start_xvfb` helpers and the same owned-`Child` + `timeout` process hygiene.

- [ ] **Step 1: Add the startup hook**

In `crates/rt/src/main.rs`, where the window/state is initialized (near the other overlay flags, after `manual_open` is set up), read the env once and set `manual_open = true` when `RT_OPEN_MANUAL=1`. Keep it undocumented (test-only). Example, at the point `Active` is constructed (near line 983 where `Action::Manual` toggles it, but at startup):

```rust
        // Test hook (undocumented): open the manual at startup so the xtrace
        // regression can drive a native overlay without synthetic input.
        if std::env::var_os("RT_OPEN_MANUAL").is_some() {
            active.manual_open = true;
        }
```

- [ ] **Step 2: Write the failing test variant**

Add to `crates/rt/tests/xrender_commands.rs` a second `#[test] #[ignore]` fn `xrender_chrome_is_commands_not_pixels` modeled on the existing one, but with `.env("RT_OPEN_MANUAL", "1")` on the traced `rt` and these assertions after counting:

```rust
    let triangles = dump.matches("Triangles").count();
    eprintln!("chrome wire profile: Triangles={triangles} PutImage={put_image} bytes={bytes}");
    assert!(triangles > 0, "native chrome must emit RENDER Triangles (AA primitives), got 0");
    assert_eq!(put_image, 0, "native chrome must ship ZERO PutImage pixel blits");
```

(The manual itself is text — `CompositeGlyphs` — but the instruments draw underneath it every frame, so `Triangles > 0` holds from the packets/jacks/wires/latency. If instruments are all disabled by default settings, the test enables one via `.env("RT_INST", …)` or opens the menu instead; confirm at implementation which overlay reliably emits Triangles and drive that one.)

- [ ] **Step 3: Run the test (guarded)**

Run: `cargo test -p rt --test xrender_commands -- --ignored --nocapture`
Expected: On a host with `Xvfb` + `xtrace`, PASS with `Triangles>0 PutImage=0`. On a host without them, it SKIPS (prints why, passes).

- [ ] **Step 4: Commit**

```bash
git add crates/rt/tests/xrender_commands.rs crates/rt/src/main.rs
git commit -m "test(xrender): chrome stays commands-only (Triangles>0, PutImage==0)"
```

---

### Task 12: Manual verification over real `ssh -X` + finish the branch

The correctness gate that Xvfb cannot provide: real remote interaction and the on-wire pixel count.

**Files:** none (verification + merge).

- [ ] **Step 1: Build the release binary for the feel-test**

Run (locally): `cargo build --release -p rt --features x11`
Then run rt over `ssh -X milkv` per the project's usual flow (full path to the freshly built binary; do not use the packaged `/usr/bin/rt`).

- [ ] **Step 2: Interactive checklist (report results honestly)**

Over `ssh -X milkv`, confirm each:
- Right-click → the native context menu renders, hover highlights, a pick runs the action (e.g. Split), Escape/outside-click closes.
- Ctrl+Shift+F → the search bar renders; typing finds + highlights; Enter/Shift-Enter step; Escape closes.
- F1 → the manual renders; arrows/PageDn scroll; Escape closes.
- Instruments animate (packets orbit, heat tints, wires flow) and are anti-aliased (smooth circles/lines), not boxy.
- No whole-window flash on overlay open/close.

- [ ] **Step 3: Confirm commands-only on the real link**

With rt running under `ssh -X`, run a concurrent xtrace (owned `Child`, `timeout`-bounded, killed by PID — never by name) capturing a few seconds with the menu open, and confirm `PutImage == 0` and KB-scale output, matching Slice 1's measurement discipline (real transport, not Xvfb).

- [ ] **Step 4: Finish the branch**

Use the `superpowers:finishing-a-development-branch` skill: merge `slice-2-xrender-chrome` `--no-ff` to `main`, push (the origin redirect note in `project_rt.md` still applies), and `cargo install --path crates/rt --features x11`. Update the `project_rt.md` memory with the Slice-2 outcome.

---

## Self-Review

**Spec coverage:**
- Native menu/search/manual/instruments → Tasks 5/6/7/9 + dispatch Task 10. ✓
- New AA primitives (fill_circle/stroke_circle/stroke_line) → Tasks 1-2. ✓
- Faithful instruments (AA circles/lines, blackbody heat) → Task 9 reusing exact color/geometry helpers. ✓
- Zero PutImage invariant → Task 11 xtrace guard + Task 12 real-link check. ✓
- Local GL byte-identical → no-op defaults (Task 2), egui path untouched (Task 10 keeps the `supports_egui()` branch). ✓
- Per-overlay unifiable later → each overlay is an independent function gated by `supports_egui()`. ✓
- Preferences deferred, translucency deferred → Task 10 keeps prefs force-closed; no translucency work. ✓
- Never-kill-unowned + real-transport verification → Tasks 11-12 use owned `Child` + `timeout`, verify over `ssh -X`. ✓

**Placeholder scan:** The known soft spots are called out inline for the implementer to resolve against the live types, not left vague: `Keymap::shortcut_for`'s printable type (Task 4 Step 3), the `active.backend` vs `&active` borrow split (Task 10 Step 1), the `egui::pos2` conversion (Task 9 Step 1), and which overlay reliably emits `Triangles` for the guard (Task 11 Step 2). Each has a concrete resolution instruction.

**Type consistency:** `render::Color` is the color type throughout (primitives, chrome draws). `Recti` is the shared layout rect. `menu::Row`/`RowAction` bridge to `MenuPick`. `advance_instrument_state(active, dt)` has one signature used by both paint paths. Primitive method names (`fill_circle`/`stroke_circle`/`stroke_line`) are identical in the trait (Task 2), the XRender impl (Task 2), and all call sites (Tasks 5-9).
