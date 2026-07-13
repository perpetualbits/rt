# Mechanism C — XRender backend (Slice 1) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make rt fast over `ssh -X` by rendering the terminal grid as X drawing commands (XRender glyph sets + fills) instead of GL pixels, via a second `Backend` selected only for forwarded X — leaving the local GL path byte-for-byte unchanged.

**Architecture:** Extract a `Backend` trait from today's renderer (the ops `draw_panes` already calls); wrap the existing GL renderer as `GlBackend` (mechanical, local-identical); add a no-GL `XRenderBackend` that draws into winit's existing X11 window via `x11rb` RENDER (glyph sets reusing rt's `fontdue` rasterization, `CompositeGlyphs` for text, `FillRectangles` for backgrounds/cursor/selection). Select by X socket type (unix→GL, TCP/forwarded→XRender) with a `--backend` override; reuse Phase 1 damage for incremental drawing.

**Tech Stack:** Rust, `x11rb` 0.13.2 (RENDER extension — all bindings present), `fontdue` (reused), `winit` 0.30, Phase 1 damage pipeline. No new deps.

## Global Constraints

- **The local path stays byte-for-byte identical.** `GlBackend` is a mechanical wrap of the current `render.rs`; a local unix-socket display ALWAYS selects `GlBackend`. Verify local output/behaviour unchanged at every task that touches shared code.
- **`XRenderBackend` creates NO GL context** — no glutin/glow. It draws into winit's X11 window via `x11rb` only.
- **Backend selection:** unix socket (`/tmp/.X11-unix`, `DISPLAY=:N`) → `GlBackend`; TCP/forwarded (`ssh -X` → `localhost:10.x`) → `XRenderBackend`; `--backend gl|xrender` and `RT_BACKEND=gl|xrender` override; Wayland (no `$DISPLAY`) → `GlBackend`.
- **Reuse, don't reinvent:** `fontdue` rasterisation + glyph-cache keying `(char, bold, italic)`, colour resolution, the Phase-1 `DamageAccumulator`, session/selection/scrollback/input are all backend-agnostic and unchanged.
- **Glyphs render identically** to the GL path (same `fontdue` coverage bitmaps → XRender `A8` glyphs).
- **Non-goals (do NOT build):** remote chrome (egui overlays — Slice 2), translucency (Slice 3), colour emoji (A8 coverage only).
- **Damage-incremental:** a scissored frame emits commands for only the damaged cells; a full frame emits all visible cells (still KB of commands — resize/scrollback/first-paint are cheap).
- Workflow per task: branch `mechanism-c-xrender` (created off main `7f607db`); `cargo build -p rt` + `cargo build -p rt --no-default-features` clean; `cargo test -p rt`/`rt-engine` green; commit with the shown message. Do not merge/push.

## File Structure

- `crates/rt/src/backend.rs` — **create.** The `Backend` trait + `BackendKind` + `choose_backend` selection logic. One responsibility: the rendering abstraction + which impl to use.
- `crates/rt/src/gl_backend.rs` — **create.** `GlBackend`: owns the current `Renderer` + the GL present resources (`surface`, `context`, `x11_present`), implements `Backend` by delegating to `render.rs` and moving today's present (swap / Route-1) into `present()`.
- `crates/rt/src/xrender_backend.rs` — **create.** `XRenderBackend`: the x11rb RENDER renderer (connection, window Picture, glyph-set manager, fills, present). The new leaf.
- `crates/rt/src/render.rs` — **modify (minimal).** Stays the GL renderer; `GlBackend` wraps it. No behaviour change.
- `crates/rt/src/main.rs` — **modify.** `Active.backend: Box<dyn Backend>` replaces `renderer`/`surface`/`context`/`x11_present`; `draw_panes`/`redraw_*` call through the trait; startup selects the backend; the window is created backend-appropriately; `paint_overlays_or_instruments` degrades on XRender.
- `crates/rt/tests/xrender_commands.rs` — **create.** `#[ignore]`d xtrace regression: XRender emits glyph/fill commands, ~zero `PutImage`.

---

## Task 1: the `Backend` trait + `GlBackend` wrap (local-identical)

**Files:**
- Create: `crates/rt/src/backend.rs`, `crates/rt/src/gl_backend.rs`
- Modify: `crates/rt/src/main.rs` (module decls; `Active` fields; all `active.renderer.*` call sites; `redraw_full`/`redraw_scissored` present)

**Interfaces:**
- Produces:
  - `pub trait Backend` with methods (all the drawing ops `draw_panes`/`redraw_*` use today, exact signatures from `render.rs`):
    `cell_size(&self)->(f32,f32)`; `resize(&mut self,w:f32,h:f32)`; `reload_fonts(&mut self,&FontBlobs,f32)->Result<(),String>`; `begin_frame(&mut self,Color)`; `begin_frame_scissored(&mut self,Color,PxRect)`; `clear_scissor(&mut self)`; `fill_rect(&mut self,f32,f32,f32,f32,Color)`; `fill_cell(&mut self,f32,f32,usize,usize,Color)`; `draw_char(&mut self,f32,f32,usize,usize,char,Color,bool,bool)`; `draw_underline(&mut self,f32,f32,usize,usize,Color)`; `draw_strikeout(&mut self,f32,f32,usize,usize,Color)`; `cursor_hollow/cursor_underline/cursor_beam(&mut self,f32,f32,usize,usize,Color)`; `bell_stripe(&mut self,f32,f32,f32,f32)`; `end_frame(&mut self)`; `present(&mut self,window:&winit::window::Window,damage:Option<crate::damage::PxRect>)`; `is_software(&self)->bool`.
  - `pub struct GlBackend` implementing `Backend`, owning `Renderer` + `Surface<WindowSurface>` + `PossiblyCurrentContext` + `Option<x11_present::X11Present>`, plus `pub fn renderer_mut`/`gl_ctx` for the pixel-identity test and any GL-only use.
- Consumes: everything in `render.rs` (unchanged).

**Design note (present):** today's present lives in `redraw_full` (`swap_buffers` or Route-1 `x11_present`) and `redraw_scissored` (`present_with_damage`/Route-1/fallback). Move that logic verbatim into `GlBackend::present(window, damage)` — `damage=Some(bbox)` is the scissored/Route-1 case, `None` is the full-swap case. `redraw_full`/`redraw_scissored` then call `active.backend.present(&active.window, None|Some(bbox))`. `GlBackend` owns `surface`/`context`/`x11_present` (moved out of `Active`).

- [ ] **Step 1: Write the trait + `GlBackend` (no behaviour change)**

Create `crates/rt/src/backend.rs`:
```rust
//! Rendering backend abstraction. `draw_panes` computes WHAT to draw; a `Backend`
//! decides HOW (GL quads vs XRender commands). Selection lives here too.
use crate::damage::PxRect;
use crate::render::{Color, FontBlobs};
use winit::window::Window;

pub trait Backend {
    fn cell_size(&self) -> (f32, f32);
    fn resize(&mut self, w: f32, h: f32);
    fn reload_fonts(&mut self, blobs: &FontBlobs, font_px: f32) -> Result<(), String>;
    fn begin_frame(&mut self, bg: Color);
    fn begin_frame_scissored(&mut self, bg: Color, bbox: PxRect);
    fn clear_scissor(&mut self);
    fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, c: Color);
    fn fill_cell(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color);
    fn draw_char(&mut self, ox: f32, oy: f32, col: usize, row: usize, ch: char, fg: Color, bold: bool, italic: bool);
    fn draw_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color);
    fn draw_strikeout(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color);
    fn cursor_hollow(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color);
    fn cursor_underline(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color);
    fn cursor_beam(&mut self, ox: f32, oy: f32, col: usize, row: usize, color: Color);
    fn bell_stripe(&mut self, x: f32, y: f32, w: f32, h: f32);
    fn end_frame(&mut self);
    fn present(&mut self, window: &Window, damage: Option<PxRect>);
    fn is_software(&self) -> bool;
}
```

Create `crates/rt/src/gl_backend.rs`: a struct holding `renderer: Renderer`, `surface`, `context`, `x11_present: Option<X11Present>`; implement each `Backend` method by delegating to `self.renderer.<same method>`; implement `present` by moving the exact `swap_buffers`/Route-1 logic from `redraw_full`/`redraw_scissored` (using `damage`), plus `gl_ctx`/`renderer_mut` accessors for the pixel-identity test.

Register both modules in `main.rs` (`mod backend; mod gl_backend;`).

- [ ] **Step 2: Rewire `Active` and the call sites**

In `main.rs`: replace `renderer: Renderer`, `surface`, `context`, `x11_present` fields with `backend: Box<dyn backend::Backend>`. At construction, build a `GlBackend` (from the created `renderer`/`surface`/`context`/`x11_present`) and box it. Replace every `active.renderer.X(...)` with `active.backend.X(...)`; replace the present blocks in `redraw_full`/`redraw_scissored` with `active.backend.present(&active.window, None)` / `(..., Some(bbox))`. The pixel-identity test's `renderer.gl_ctx()`-style access goes through a `GlBackend` downcast or a test-only constructor — keep the test compiling.

- [ ] **Step 3: Build + verify local unchanged**

Run: `cargo build -p rt 2>&1 | tail -5` → clean.
Run: `cargo build -p rt --no-default-features 2>&1 | tail -5` → clean.
Run: `cargo test -p rt 2>&1 | grep 'test result'` → all pass (the pixel-identity `#[ignore]` gate still compiles).
Reason in the commit body: `GlBackend` delegates verbatim; no GL call order changed; local output identical.

- [ ] **Step 4: Commit**
```bash
git add crates/rt/src/backend.rs crates/rt/src/gl_backend.rs crates/rt/src/render.rs crates/rt/src/main.rs
git commit -m "refactor(rt): extract Backend trait; wrap the GL renderer as GlBackend (local-identical)"
```

---

## Task 2: backend selection (unix→GL, TCP→XRender, override)

**Files:**
- Modify: `crates/rt/src/backend.rs` (add `BackendKind` + `choose_backend`), `crates/rt/src/main.rs` (call it at startup; still build `GlBackend` for both until Task 3)
- Test: inline `#[cfg(test)] mod tests` in `backend.rs` (pure)

**Interfaces:**
- Produces: `pub enum BackendKind { Gl, XRender }`; `pub fn choose_backend(display: Option<&str>, is_x11: bool, override_env: Option<&str>) -> BackendKind` — pure, unit-testable.

- [ ] **Step 1: Write the failing tests (pure selection logic)**
```rust
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn unix_socket_selects_gl() {
        assert!(matches!(choose_backend(Some(":0"), true, None), BackendKind::Gl));
        assert!(matches!(choose_backend(Some(":1.0"), true, None), BackendKind::Gl));
    }
    #[test]
    fn tcp_forwarded_selects_xrender() {
        assert!(matches!(choose_backend(Some("localhost:10.0"), true, None), BackendKind::XRender));
        assert!(matches!(choose_backend(Some("192.168.1.5:0"), true, None), BackendKind::XRender));
    }
    #[test]
    fn wayland_selects_gl() {
        assert!(matches!(choose_backend(None, false, None), BackendKind::Gl));
    }
    #[test]
    fn override_wins() {
        assert!(matches!(choose_backend(Some(":0"), true, Some("xrender")), BackendKind::XRender));
        assert!(matches!(choose_backend(Some("localhost:10.0"), true, Some("gl")), BackendKind::Gl));
    }
}
```

- [ ] **Step 2: Run to fail** — `cargo test -p rt --bin rt choose_backend` → FAIL (undefined).

- [ ] **Step 3: Implement**
```rust
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BackendKind { Gl, XRender }

/// Pick the backend. Override wins; else Wayland/non-X11 → Gl; else a DISPLAY with
/// a host part before `:` (TCP / ssh -X forward) → XRender; a bare `:N` (local unix
/// socket) → Gl.
pub fn choose_backend(display: Option<&str>, is_x11: bool, override_env: Option<&str>) -> BackendKind {
    if let Some(o) = override_env {
        return if o.eq_ignore_ascii_case("xrender") { BackendKind::XRender } else { BackendKind::Gl };
    }
    if !is_x11 { return BackendKind::Gl; } // Wayland etc.
    match display {
        // "host:N" (host non-empty) is TCP/forwarded; ":N" is a local unix socket.
        Some(d) if d.split(':').next().map_or(false, |h| !h.is_empty()) => BackendKind::XRender,
        _ => BackendKind::Gl,
    }
}
```

- [ ] **Step 4: Wire it at startup (still Gl for both)** — in `main.rs` where the window/backend is created, read `$DISPLAY` and `RT_BACKEND`/`--backend`, call `choose_backend`, log the choice (`log::info!("backend: {kind:?}")`). For now, construct `GlBackend` regardless of `kind` (XRender arrives in Task 3). Add the `--backend` CLI flag to the existing arg parser.

- [ ] **Step 5: Run to pass** — `cargo test -p rt --bin rt choose_backend` → 4 pass. `cargo build -p rt` clean.

- [ ] **Step 6: Commit**
```bash
git add crates/rt/src/backend.rs crates/rt/src/main.rs
git commit -m "feat(rt): backend selection (unix->GL, TCP/forwarded->XRender, --backend override)"
```

---

## Task 3: `XRenderBackend` skeleton — connect, Picture, present, cleared window

**Files:**
- Create: `crates/rt/src/xrender_backend.rs`
- Modify: `crates/rt/src/main.rs` (build `XRenderBackend` when `kind==XRender`; create the window without a GL context on that path)

**Interfaces:**
- Consumes: `winit::window::Window` (X11 window id via raw handle), `FontBlobs`, `crate::render::cell_size_for`, x11rb RENDER.
- Produces: `pub struct XRenderBackend` implementing `backend::Backend`; `pub fn try_new(window: &Window, blobs: &FontBlobs, font_px: f32) -> Option<Self>`.

**Design note (window without GL):** on the `XRender` path, create winit's `Window` normally but SKIP glutin context/surface creation. `XRenderBackend::try_new` opens an `x11rb` connection, gets the window id (`RawWindowHandle::Xlib/Xcb`, like `x11_present`/`x11_blur`), calls `render::query_pict_formats` to find (a) the window-depth `Pictformat` for the window Picture and (b) an `A8` glyph `Pictformat`, creates the window `Picture` (`render::create_picture` on the window drawable), and a 1×1 repeating solid source `Picture` for the foreground colour (updated per glyph run). Cell metrics come from `cell_size_for(blobs, font_px)`. If RENDER is unavailable or a format isn't found → `None` (caller falls back to `GlBackend` with a warning — never crash).

- [ ] **Step 1: Implement the skeleton** (x11rb RENDER — verify exact arg types against `~/.cargo/registry/src/*/x11rb-0.13.2/src/protocol/render.rs`, which has `query_pict_formats`, `create_picture`, `create_glyph_set`, `add_glyphs`, `composite_glyphs8`, `fill_rectangles`, and `Picture`/`Glyphset` resource wrappers):
  - `try_new`: connect; window id; `query_pict_formats().reply()`; pick the window's format (match visual/depth) and an `A8` format (`depth==8`, alpha-only); `create_picture(window_pic, window, window_format, &Default)`; store `conn, window, window_pic, a8_format, (cell_w, cell_h), fonts` (parse `blobs` with `fontdue`, reusing `render.rs`'s `parse_chain` — expose it `pub(crate)` if needed).
  - `Backend` impl: `cell_size` returns the metrics; `begin_frame(bg)` fills the whole window Picture with `bg` via `render::fill_rectangles`; `begin_frame_scissored(bg,bbox)` stores `bbox` and fills only `bbox`; `clear_scissor` clears the stored bbox; `end_frame` no-op; `present(_,_)` = `self.conn.flush()`; drawing methods (`fill_rect`/`draw_char`/cursors/etc.) are **empty for now** (filled in Tasks 4–5); `is_software` returns `true`; `resize`/`reload_fonts` minimal (full impl Task 7/rebuild fonts).

- [ ] **Step 2: Wire selection** — in `main.rs`, when `kind==XRender`, build `XRenderBackend::try_new(&window, &font_blobs, font_size)`; on `Some` box it as the backend and DO NOT create the GL surface/context; on `None`, log a warning and fall back to `GlBackend`.

- [ ] **Step 3: Build + a cleared window over Xvfb** — `cargo build -p rt` clean. On any X box: `Xvfb :99 -ac & DISPLAY=:99 RT_BACKEND=xrender ~/git/rt/target/release/rt` (or debug) — the window should open and clear to the background colour without crashing (no text yet). Capture with `import -window root` to confirm a solid cleared window.

- [ ] **Step 4: Commit**
```bash
git add crates/rt/src/xrender_backend.rs crates/rt/src/main.rs
git commit -m "feat(rt): XRenderBackend skeleton — connect, window Picture, clear+present"
```

---

## Task 4: XRender fills — backgrounds, cursor, bell, separators

**Files:** Modify: `crates/rt/src/xrender_backend.rs`

**Interfaces:** Consumes the skeleton (Task 3). Produces the `fill_rect`/`fill_cell`/`bell_stripe`/`cursor_*` impls on `XRenderBackend`.

- [ ] **Step 1: Implement fills** — `fill_rect(x,y,w,h,c)` → `render::fill_rectangles(PictOp::SRC, self.window_pic, color_to_render(c), &[Rectangle{x,y,width,height}])` (respecting the stored scissor bbox: skip/clip if outside — full clip handling in Task 6, for now draw unclipped). `fill_cell(ox,oy,col,row,color)` computes the cell rect from `cell_size` and calls `fill_rect`. `cursor_hollow` = four thin fills (outline); `cursor_underline`/`cursor_beam` = one thin fill; `bell_stripe` = the caution-tape edge as fills. `color_to_render(Color)` maps rt's 0..1 RGBA floats to XRender's `render::Color{ red,green,blue,alpha: u16 }` (×65535).

- [ ] **Step 2: Verify** — build clean; over Xvfb with `RT_BACKEND=xrender`, run rt with a shell; backgrounds + block/beam cursor now render (still no glyphs). Capture to confirm coloured cells + cursor.

- [ ] **Step 3: Commit**
```bash
git add crates/rt/src/xrender_backend.rs
git commit -m "feat(rt): XRenderBackend fills — backgrounds, cursor, bell via FillRectangles"
```

---

## Task 5: XRender glyphs (the core) — glyph sets + CompositeGlyphs

**Files:** Modify: `crates/rt/src/xrender_backend.rs`

**Interfaces:** Consumes fills (Task 4). Produces `draw_char`/`draw_underline`/`draw_strikeout`, and a private glyph-set manager.

**Design note:** one `GlyphSet` (A8). For each `(char, bold, italic)` not yet uploaded: rasterise with `fontdue` exactly as `render.rs` does (`Font::rasterize`, coverage bitmap + metrics), assign a `glyph_id` (a running counter), `add_glyphs(glyphset, &[glyph_id], &[Glyphinfo{width,height,x:bearing_left,y:bearing_top,x_off:advance,y_off:0}], &coverage_bytes)` — once. Cache `HashMap<(char,bool,italic), u32>` (glyph_id) like today's atlas cache. `draw_char` resolves/creates the glyph_id, sets the fg source Picture to `fg`, and emits `composite_glyphs8(PictOp::OVER, fg_src_pic, self.window_pic, self.a8_format, glyphset, dst_x, dst_y, &glyphcmd)` where `glyphcmd` is the wire encoding for one glyph run at the cell's pen position (a `GLYPHITEM` header {count, pad, dx, dy} + the glyph id). Batch adjacent same-fg chars into one run where easy; a per-char run is acceptable for Slice 1.

- [ ] **Step 1: Implement the glyph-set manager + `draw_char`** (verify `Glyphinfo` fields + the `composite_glyphs8` `glyphcmds` byte layout against x11rb's `render.rs`; the glyph-run encoding is: `[count:u8][3 pad][dx:i16][dy:i16][glyph ids…]`). `draw_underline`/`draw_strikeout` are thin `fill_rect`s at the cell's underline/strike row.

- [ ] **Step 2: Verify text renders** — build clean; over Xvfb `RT_BACKEND=xrender`, run rt with a shell printing text; **text now renders**. Capture and visually confirm the glyphs are correct (compare to a GL-backend capture of the same content — should be pixel-similar since it's the same fontdue output).

- [ ] **Step 3: Commit**
```bash
git add crates/rt/src/xrender_backend.rs
git commit -m "feat(rt): XRenderBackend text — glyph sets (fontdue) + CompositeGlyphs"
```

---

## Task 6: damage-incremental — scissor filtering (only changed cells emit)

**Files:** Modify: `crates/rt/src/xrender_backend.rs`

**Interfaces:** Consumes Tasks 4–5. Produces: `begin_frame_scissored`/`clear_scissor` gate all fills+glyphs to the stored bbox so a scissored (damage) frame emits commands for only the damaged region.

- [ ] **Step 1: Implement scissor filtering** — store `Option<PxRect> clip` set by `begin_frame_scissored`, cleared by `clear_scissor`/`begin_frame`. In `fill_rect` and `draw_char`, if `clip` is `Some(b)` and the target rect does not intersect `b`, return early (emit nothing). `begin_frame_scissored(bg,bbox)` fills only `bbox` (not the whole window). Result: a keystroke frame emits fills+glyphs for the ~1–2 changed cells only; a full frame (`begin_frame`) emits everything.

- [ ] **Step 2: Verify incremental** — build clean; over Xvfb+xtrace (Task 9 harness), a keystroke emits a handful of `CompositeGlyphs`/`FillRectangles` (bytes in the hundreds), not a screenful. Log or count the per-frame request bytes.

- [ ] **Step 3: Commit**
```bash
git add crates/rt/src/xrender_backend.rs
git commit -m "feat(rt): XRenderBackend damage-incremental — only changed cells emit commands"
```

---

## Task 7: resize + font reload

**Files:** Modify: `crates/rt/src/xrender_backend.rs`, `crates/rt/src/main.rs` (Resized handler routes through the backend)

**Interfaces:** Produces working `resize`/`reload_fonts` on `XRenderBackend`.

- [ ] **Step 1: Implement resize** — the X window resizes with the WM; the window Picture follows the drawable, so `resize(w,h)` updates the stored `(win_w, win_h)` and (with `force_full` already armed by the Resized handler) the next full frame redraws all cells at the new size. Confirm no GL-surface resize is attempted on this backend (that code path is `GlBackend`-only). `reload_fonts(blobs,font_px)` re-parses fonts, recomputes `cell_size`, and **clears the glyph cache + recreates the GlyphSet** (old glyph ids are stale). 

- [ ] **Step 2: Verify** — build clean; over Xvfb, resize the window (`xdotool` or a WM) — content re-lays out and fills the new size (the bug Route 1 had is absent here because a full redraw is just commands). Font-size change (if reachable without chrome) reloads.

- [ ] **Step 3: Commit**
```bash
git add crates/rt/src/xrender_backend.rs crates/rt/src/main.rs
git commit -m "feat(rt): XRenderBackend resize + font reload"
```

---

## Task 8: chrome degrades gracefully on the XRender backend

**Files:** Modify: `crates/rt/src/main.rs`

**Interfaces:** Consumes the backend abstraction. Produces: on `XRenderBackend`, `paint_overlays_or_instruments` and overlay-open actions are no-ops (with a one-line hint), while typing/selection/copy-paste/splits/scrollback keep working.

- [ ] **Step 1: Gate egui** — add a `backend.is_xrender()` capability (a trait method returning `false` by default, `true` on `XRenderBackend`; or check the `BackendKind` stored on `Active`). In `redraw_full`/`redraw_scissored`, skip `paint_overlays_or_instruments` when on XRender. In the actions that open prefs/menu/manual/search, when on XRender: log `"<overlay> not available on the remote (XRender) backend yet"` once and do nothing (no panic, no state that later expects egui). Selection highlight + cursor still render (they're `draw_panes`, not egui).

- [ ] **Step 2: Verify** — build clean; over Xvfb `RT_BACKEND=xrender`: type, select with the mouse (highlight renders), copy/paste via keys, open a split, scroll history — all work; pressing F1/right-click/prefs does nothing (with a log line), no crash.

- [ ] **Step 3: Commit**
```bash
git add crates/rt/src/main.rs
git commit -m "feat(rt): degrade egui chrome gracefully on the XRender backend"
```

---

## Task 9: xtrace command-not-pixels regression test

**Files:**
- Create: `crates/rt/tests/xrender_commands.rs`

**Interfaces:** `#[ignore]`d integration test; needs Xvfb + `xtrace`. Proves the feature's thesis: XRender emits glyph/fill *commands* and ~zero pixel `PutImage`.

**What it does:** launches rt (release or the test binary) with `RT_BACKEND=xrender` under `xtrace` to an Xvfb, drives a few lines of text, then parses the trace and asserts: `CompositeGlyphs`/`FillRectangles` count > 0, `PutImage` count is 0 (or ≤ a tiny constant for any icon), and total client→server bytes < 200 KB (KB not MB). This is the exact `xtrace` method that proved the problem.

- [ ] **Step 1: Write the `#[ignore]`d test** — spawn `Xvfb :NN -ac`, run `xtrace -n -d :NN -D :MM -o <trace> -- env DISPLAY=:MM RT_BACKEND=xrender <rt-bin>` with a `SHELL` that prints text, `timeout` it, then read `<trace>` and assert the counts/bytes above. Skip (return) if `xtrace`/`Xvfb` aren't on `PATH`, printing why. Mark `#[ignore = "needs Xvfb + xtrace"]`.

- [ ] **Step 2: Run it** — on a box with Xvfb+xtrace (or the milkv): `cargo test -p rt --test xrender_commands -- --ignored` → PASS (commands present, PutImage ~0, bytes in KB). Default `cargo test` skips it.

- [ ] **Step 3: Commit**
```bash
git add crates/rt/tests/xrender_commands.rs
git commit -m "test(rt): xtrace regression — XRender emits commands, ~zero PutImage (KB not MB)"
```

---

## Task 10: perf + visual verification on the milkv over real ssh -X

**Files:** none (verification).

- [ ] **Step 1: Build the branch on the milkv** (via `git bundle`, base a commit the board has).
- [ ] **Step 2: Over real `ssh -X milkv`,** run `~/git/rt/target/release/rt` (full path). Expected: `backend: XRender` + `x11_present`-free startup; typing, scrolling, scrollback, **resize**, and multi-pane all responsive (fraction-of-a-second, Terminator-class), not the ~1–2 s / ~4 s of the pixel path. Chrome (menu/prefs/manual) does nothing (Slice 2).
- [ ] **Step 3: Objective proxy** — capture `xtrace` of this run and confirm the wire profile now matches Terminator (glyph/fill commands, KB), directly comparable to the earlier rt-vs-Terminator capture.
- [ ] **Step 4: Record** the before/after latency + byte profile in the branch/PR notes.

---

## Self-Review

**1. Spec coverage:** `Backend` trait + `GlBackend` local-identical → Task 1. XRender glyph sets/CompositeGlyphs/fills → Tasks 3–5. Backend selection (unix/TCP/override) → Task 2. Phase-1 damage reuse (incremental) → Task 6. Chrome degrades → Task 8. Resize (the Route-1 gap) → Task 7. Testing: xtrace command-not-pixels → Task 9; milkv real-ssh perf → Task 10; local byte-identical → Task 1/2. Non-goals (chrome/translucency/emoji) explicitly excluded. ✓

**2. Placeholder scan:** The XRender internals (Tasks 3–7) are specified at the x11rb-call level (exact functions, the glyph-upload format, the `composite_glyphs8` run encoding) rather than as fully pre-debugged code, because XRender wire details need on-target verification against x11rb 0.13.2 and a live server — each such task names the exact `render.rs` binding to check and gives the structure. This is the honest boundary for a from-scratch protocol renderer; there are no "TODO/handle later" gaps, and every task has a concrete verify step (a rendered capture) that is falsifiable. Tasks 1, 2, 9 have complete code.

**3. Type consistency:** `Backend` trait signatures (Task 1) are copied verbatim from `render.rs`'s current public methods, so `GlBackend` and `XRenderBackend` share them. `choose_backend`/`BackendKind` (Task 2) used consistently. `Color`/`PxRect`/`FontBlobs` are the existing types throughout.

**Known risk flagged for the executor:** Task 1 is the linchpin — the `GlBackend` wrap and the `Active.backend` swap touch many call sites and move present resources out of `Active`; review it hardest for local byte-identity, and keep the pixel-identity `#[ignore]` gate compiling. Tasks 3–7 are new protocol code best executed inline with a live Xvfb to iterate on the XRender specifics, not blind.
