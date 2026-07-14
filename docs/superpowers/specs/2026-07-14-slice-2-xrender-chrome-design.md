# Slice 2 — XRender-drawn chrome

**Status:** Design (approved 2026-07-14)
**Depends on:** Slice 1 (mechanism C — command-based XRender backend), merged `3e18936` + fixes `3735ac3`.
**Supersedes for the remote path:** the Slice-1 "force-close overlays on the XRender backend" degradation.

## Problem

Slice 1 made the terminal *grid* fast over `ssh -X` by drawing it as XRender
commands (glyph sets + `composite_glyphs32` + `fill_rectangles`) instead of GL
pixels — proven 2.67 MB → 15.8 KB, 0 `PutImage`, on real riscv64 hardware. But
rt's *chrome* — the context menu, search bar, manual, and the animated border
instruments / patch-bay — is all drawn by `egui_glow` into the GL framebuffer.
On the XRender backend there is no presented GL surface, so Slice 1 simply
**disabled** that chrome (`supports_egui() == false` skips the overlays and
force-closes any open one so an invisible dialog can't trap keys). The user's
first remote test confirmed the gap: "no menus, to start."

Slice 2 draws that chrome natively as XRender commands, so the remote session
has working menus, search, manual, and live instruments — while staying
commands-only (zero pixel blits) and leaving the local GL path byte-for-byte
identical.

## Goals

- Context menu, search bar, manual, and instruments/patch-bay render on the
  XRender backend, drawn as XRender commands.
- Instruments are **faithful** to the GL look — anti-aliased circles for
  packets/jacks, smooth wires, blackbody heat borders — not a boxy fallback.
- Zero `PutImage`: the xtrace regression guard (`PutImage == 0`) continues to
  hold with chrome on screen.
- Local GL path (the `GlBackend`, `supports_egui() == true`) is **unchanged** —
  no behavioral or byte-level difference. This is a hard constraint.
- Each overlay is an independent unit selected per-overlay, so a later slice can
  unify the GL path onto the native units one overlay at a time (option C).

## Non-Goals

- **Preferences** overlay stays egui-only (still force-closed on XRender). It is
  the most widget-dense overlay and earns its own later slice.
- **Translucency / blur** (Slice 3 — needs an ARGB visual + a compositor).
- Unifying the *local* GL chrome onto the native units now. The design makes it
  cheap later; it is not done here.
- Skipping GL-context creation on the XRender path (a separate deferred cleanup).

## Alternatives Considered

**XRender RENDER commands (chosen).** Draw chrome with the same RENDER
primitives the grid uses, plus a small set of anti-aliased geometry primitives
built on the RENDER `Triangles` request. Works against **any** unmodified `Xorg`
reachable over `ssh -X`; no cooperating server needed. This is the reach that
matters for the remote-terminal use case.

**Indirect GLX (rejected).** Ship rt's existing GL — including the glyph-atlas
shader — as GLX-encoded GL protocol so the server renders it. Stock X servers
cap indirect GLX at ~GL 1.4 with **no** GLSL, VBOs, or core profile, so rt's
actual shaders cannot cross the indirect wire. This is the original reason
mechanism C exists.

**SPIR-V command-stream (deferred to Rayland).** A *custom* client/server that
streams portable shader bytecode (SPIR-V) and runs it on the server GPU would
carry the full shader chrome remotely. That is not a stock-X path; it is a
bespoke cooperating peer — which is exactly the **Rayland** project
(`~/git/rayland`: command-stream C→S render on the server's GPU,
Venus + Zink + waypipe + QUIC). The clean division: **XRender works today against
any `ssh -X`; the SPIR-V stream is Rayland's remit, richer, needs a cooperating
server.** rt-over-Rayland would eventually get the full shader chrome remotely
for free. Out of scope here, recorded so the seam is explicit.

## Architecture

A new backend-agnostic **`chrome`** module. Each overlay is a self-contained
unit — a struct owning its *state* — with three methods:

- `layout(&self, ctx) -> Geometry` — compute element rects from window size,
  cell metrics, and the unit's own state.
- `draw(&mut self, &mut dyn Backend, ...)` — paint via `Backend` primitives.
- `handle_event(&mut self, WindowEvent) -> Outcome` — hit-test / hover /
  keyboard, returning an action for the caller to apply.

The units reuse today's **logic** verbatim — `menu::MenuPick` + `apply_action`,
the search engine, the manual text, and the instruments' meter/wire/bezier math.
Only *drawing* and *interaction* are reimplemented natively; the decision logic
is not touched.

**Selection** keys on the existing `Backend::supports_egui()`:

- `supports_egui() == true` (`GlBackend`) → today's egui chrome, unchanged.
- `supports_egui() == false` (`XRenderBackend`) → native chrome units.

The Slice-1 force-close guard in `main.rs` is **removed for these four
overlays** (menu, search, manual, instruments) and replaced by routing their
input to the native units. Preferences keeps the force-close.

Because each overlay is an independent unit gated by the same per-overlay check,
"unify the GL path onto native later" (option C) is literally: flip that one
overlay from the egui path to the native unit and delete its egui drawing — no
rewrite, no shared-state untangling.

### Module layout

- `crates/rt/src/chrome/mod.rs` — the `ChromeUnit` shape, shared `Geometry`
  types, `Outcome` enum, and a `Chrome` holder that owns the four units and
  dispatches draw/input on the XRender path.
- `crates/rt/src/chrome/menu.rs` — native context-menu unit (consumes
  `menu::MenuPick`).
- `crates/rt/src/chrome/search.rs` — native search-bar unit.
- `crates/rt/src/chrome/manual.rs` — native manual unit.
- `crates/rt/src/chrome/instruments.rs` — native instruments/patch-bay unit;
  the animation *state* math moves here from `main.rs`'s `paint_instruments`
  (drawing-independent), leaving `paint_instruments` as the GL-path caller.

## `Backend` trait additions

Slice 1 gives the grid `fill` (rects) and `draw_char` (glyphs). Chrome adds
three anti-aliased primitives. Each is a trait method with a **no-op default**
(so `GlBackend` — which never draws native chrome — needs no change);
`XRenderBackend` implements them for real.

```
fn fill_circle(&mut self, cx: f32, cy: f32, r: f32, color: Rgba);
fn stroke_circle(&mut self, cx: f32, cy: f32, r: f32, width: f32, color: Rgba);
fn stroke_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: Rgba);
```

- `color: Rgba` is **straight** (non-premultiplied) alpha; the primitive
  premultiplies internally.
- The heat border is **not** a new primitive — an axis-aligned rect outline is
  four `fill()` calls (crisp is correct, matches egui). No `stroke_rect`.

### XRender implementation

All three compile to the RENDER `Triangles` request (`render::triangles`),
compositing a **solid-color source Picture through a triangle mask** in the
`a8` mask format. The a8 coverage is what yields the anti-aliasing:

- filled disc → triangle fan (center + N rim points; N≈32 for r≥8, scaled down
  for small radii).
- ring → triangle strip between outer and inner radius.
- thick line → the segment expanded to a quad (two triangles), butt caps.

**Alpha.** Slice 1's `src_pic` is a 24-bit **opaque** solid. The packet glow
needs real alpha (`a ≈ 110/255`). So `XRenderBackend` gains a **32-bit ARGB
solid source** (`src_pic_argb`, 1×1 repeating) whose color+alpha is set per call
(premultiplied on upload) and composited OVER. The three primitives take the
straight-alpha `color` and premultiply into that source.

**Commands-only invariant.** `Triangles` and solid sources are server-side
geometry — no client pixels cross the wire — so the xtrace guard
(`PutImage == 0`) still holds with chrome on screen. A test asserts it.

## Per-overlay designs

Every unit follows state · layout · draw · handle_event and reuses existing
logic; only drawing/interaction is native.

### Context menu (`chrome/menu.rs`)

- **State:** item list (built from the same inputs `menu::ui` takes — keymap,
  has-selection, URL-under-cursor), `hovered: Option<usize>`, anchor position.
- **Layout:** items stacked down from the anchor; width = max label width in
  cells; clamped into the window.
- **Draw:** `fill` panel bg + 1px border; `fill` a highlight rect on the hovered
  row; `draw_char` each label; separators are a thin `fill`.
- **Input:** `CursorMoved` → hit-test row → set `hovered` + damage; `MouseButton`
  press on a row → return that row's `MenuPick` (feeds existing `apply_action` /
  URL open/copy logic); `Escape` / click-outside → close.

### Search bar (`chrome/search.rs`)

- **State:** query string, match count + current index (from the existing search
  engine), caret position.
- **Layout:** bar pinned top-right (matches current egui placement); height =
  1 cell + padding.
- **Draw:** `fill` bar bg + border; `draw_char` the query and the "n/m" counter;
  `fill` a caret.
- **Input:** text keys edit the query (re-run engine → re-damage grid
  highlights); Enter / Shift-Enter = next / prev match; Escape = close. The
  in-grid match highlight is already a `fill` pass and is reused unchanged.

### Manual (`chrome/manual.rs`)

- **State:** the manual text (static), `scroll: usize` (top line).
- **Layout:** centered panel ~80% of the window with a cell-row viewport.
- **Draw:** `fill` panel bg + border; `draw_char` the visible slice of lines;
  `fill` a scrollbar thumb.
- **Input:** arrows / PgUp / PgDn / wheel adjust `scroll` (clamped);
  Escape / `q` close.

### Instruments / patch-bay (`chrome/instruments.rs`)

The one unit that uses the new primitives. The animation **state** (loads,
packet phases, wire endpoints, jack states) moves here verbatim from
`main.rs::paint_instruments`; it is already plain backend-independent Rust.

- **Draw, per pane:**
  - heat = four `fill`s (rect outline) in `heat_color(load)` (blackbody), width
    matched to the current 2.4 px.
  - packets = `fill_circle` glow (r ≈ 9, alpha ≈ 110) + `fill_circle` core
    (r ≈ 3.4, opaque), positioned by the existing `flow_point`.
  - wires = the existing 56-point cubic-bezier tessellation → 55 `stroke_line`s
    (width 2, stream color: stdout green `0x40c054`, stderr red `0xd05430`).
  - jacks = `fill_circle` (filled) / `stroke_circle` (unfilled) backing + dot,
    at the existing `jack_pos` anchors.
- **Input:** none (non-interactive). It rides the animation-repaint cadence
  (below).

## Input routing

On the XRender path, when an overlay is open the caller diverts input to that
unit instead of the terminal/PTY, mirroring how the egui path diverts today:

- `main.rs` currently (Slice 1) force-closes `menu` / `search_open` /
  `manual_open` on the non-egui backend before its input-diversion blocks. That
  force-close is removed for these three; instead, when one is open the matching
  `Chrome` unit's `handle_event` is called first and its `Outcome` applied
  (`MenuPick` → `apply_action`; search edit → re-run engine; close → clear the
  open flag and damage the vacated region).
- Preferences keeps the existing force-close (egui-only).
- Precedence when several could be open matches the egui path's existing order.

## Damage & present

Overlays draw **on top of** the grid into the Slice-1 back pixmap, then present
via the existing `CopyArea(back_pixmap → window)` of the damage bbox — no new
present mechanism.

- **Transient overlays (menu / search / manual):** opening or a hover/scroll
  change damages the overlay's layout bounds; the frame redraws grid-then-overlay
  within that region and presents the bbox. **Closing** damages the overlay's
  last bounds so the grid repaints underneath (the region is re-shaded from the
  engine snapshot, then presented).
- **Instruments:** they animate continuously and *move* (packets orbit), so each
  animated frame must repaint the grid underneath before redrawing the
  instruments. They ride rt's **existing animation-repaint cadence** — the
  software-GL low-power throttle already limits animation to ~2 fps. On the
  XRender path an animation tick does a full grid+instruments redraw into the
  back pixmap and one `CopyArea` present; a full command redraw is ~15 KB, so
  ~2 fps ≈ 30 KB/s over the wire — cheap. No per-packet damage bookkeeping.

## Testing

- **xtrace commands-not-pixels guard (extend existing).**
  `tests/xrender_commands.rs` already asserts `CompositeGlyphs > 0`,
  `FillRectangles > 0`, `PutImage == 0` on a plain frame. Add a variant that
  drives an overlay open (menu via the same synthetic-shell approach, or an
  env/CLI hook that opens the manual at startup) and asserts the same
  `PutImage == 0` **plus** `Triangles > 0` (the AA primitives emit RENDER
  `Triangles`). This is the falsifiable proof that chrome stays commands-only.
- **Geometry unit tests (no X server).** The `layout` methods are pure functions
  of (window size, cell metrics, state) → rects; unit-test hit-testing (a click
  at point P lands on menu row K; clamp keeps the panel on-screen) and the
  bezier/flow math (moved from `main.rs`) directly.
- **Local GL unchanged (regression).** Assert `GlBackend::supports_egui()` stays
  true and the new `Backend` primitives have no-op defaults, so the GL path
  compiles and behaves identically. The existing offscreen pixel-identity gate
  (from mechanism A) continues to cover the GL render.
- **Interactive over `ssh -X milkv`.** Manual confirmation matching Slice 1's:
  open each overlay, verify it renders, is interactive (menu picks act, search
  finds, manual scrolls), instruments animate, and — via a concurrent xtrace —
  that no `PutImage` appears. Measure that an open menu stays KB-scale.

## Constraints (carried from Slice 1)

- Never kill unowned processes: every helper (Xvfb, xtrace, rt-under-test) is
  spawned as an owned `Child` and stopped by that exact handle; the traced rt is
  bounded by `timeout`. No `pkill` / `killall` / kill-by-name, on any host.
- Local (non-remote) GL path stays byte-for-byte identical; native chrome is
  XRender-only, selected by `supports_egui() == false`.
- Verify perf over the **real** transport (`ssh -X`), not a local Xvfb — Xvfb's
  SHM hides the network cost.
