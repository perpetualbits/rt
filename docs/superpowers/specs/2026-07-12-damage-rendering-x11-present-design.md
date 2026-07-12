# Damage-based rendering — X11 damage-rect present ("Phase 2", Route 1) — design

Status: approved design, pre-implementation. Spike-validated.
Date: 2026-07-12.
Builds on: `2026-07-11-damage-based-rendering-design.md` (Phase 1 / mechanism A — merged `e681478`).
Supersedes: the "Phase 2 (X11 present)" deferred by that spec, and the abandoned
`2026-07-12-damage-rendering-mechanism-b-design.md` (a full non-GL backend is *not* needed).

## Problem

rt is slow on GPU-less boards over X11-over-ssh (the milkv: ~1–2 s keystroke
latency). Phase 1 (mechanism A) doesn't help there: measured `buffer_age()==0`, so
the partial path never engages. Mechanism B's two preservation routes both failed
on softpipe (preserved-swap → `EGL_BAD_MATCH`; full-FBO blit ≈ 953 ms). The common
wall: every way to **preserve + present** a partial update *through GL* on softpipe
is either unavailable or pathologically slow.

**A spike proved a way around the wall.** On the milkv under Xvfb (softpipe GLX):

- Presenting only a keystroke-sized damage rect (240×64) via `glReadPixels` +
  `XPutImage` costs **~0.8 ms** (readback ~0.65 ms + put-image ~0.14 ms) vs ~250 ms
  for a full swap — ~300×. A full-window present is ~56 ms readback + ~5.5 ms put.
- `XPutImage` to the GLX window returns `Ok` every frame; a captured screen shows a
  **complete, correct rt render** (text, cursor, titlebar, focus border, patch-bay
  jacks) — GLX render and X present coexist, chrome included.

**Why it works where A and B could not:** it needs **no buffer preservation at
all**. We read back only the region we just rendered correctly (garbage elsewhere
in the GL back buffer is never read), and the **X window keeps every other pixel
server-side** — X windows are persistent. This is precisely why Terminator is fast
over ssh: only the changed pixels cross the wire.

## Goal

- On X11 software GL (esp. X11-over-ssh), a keystroke frame drops from ~250 ms to
  a **few milliseconds**, by reusing Phase 1's cheap scissored *shading* and
  presenting only the damage rect via `glReadPixels` + `XPutImage`.
- **Reuse all of Phase 1** (damage accumulator, `border_bands`, `scissor_box` /
  `begin_frame_scissored`, `force_full`, the `redraw()` gate). The only new thing is
  an X11 present path replacing `swap_buffers`.
- **Chrome fully intact** (egui overlays/instruments render via GL as today).
- **Hardware GPUs and Wayland untouched.**

## Non-goals

- **Wayland** (`wl_shm` present) — Route 1 is X11-only (`XPutImage` is X11). Wayland
  software GL keeps mechanism A / full path; a `wl_shm` analogue is possible later.
- **Mechanism C** (a full non-GL / XRender backend) — obviated by this spike; not built.
- Changing Phase 1's shading path or the glyph-atlas renderer.

## Architecture & gating

One branch is added to Phase 1's `redraw()` funnel:

```
hardware GL .......................→ full path (unchanged, byte-identical)
software GL, EGL + buffer_age .....→ mechanism A (Phase 1, unchanged)
software GL, X11 / GLX ............→ ROUTE 1  (scissored render + readback/XPutImage damage rect)   NEW
everything else ...................→ full path (safe fallback)
```

Route 1 activates when the surface is an **X11 (GLX) window on software GL** and an
X11 present handle is available. It reuses Phase 1's damage plumbing wholesale; the
`glReadPixels` + `XPutImage` step replaces the `swap_buffers` / `swap_buffers_with_damage`
present.

## Mechanism (per keystroke, Route 1 active)

1. Phase 1's `DamageAccumulator` yields the frame's damage bbox (top-left physical px).
2. **Scissor-render only the bbox** into the GL back buffer via Phase 1's
   `begin_frame_scissored` + `draw_panes` (cheap — a few cells).
3. **`glReadPixels`** the bbox from `GL_BACK` as `BGRA`/`UNSIGNED_BYTE`, then **flip
   rows** (readback is bottom-up; `XPutImage` `ZPixmap` is top-down). `gl.finish()`
   forces the scissored render + readback to complete.
4. **`XPutImage`** the flipped buffer to the window at the bbox (`ZPixmap`, the
   window's depth). **No `swap_buffers`.** The X window retains all other pixels.

The GL back buffer is never fully valid and is never swapped; correctness lives in
(a) the just-rendered damage region we read back, and (b) the X window's server-side
persistence of everything else.

## Chrome

egui (menu / preferences / manual / search / instruments / patch-bay wires) renders
via `egui_glow` unchanged. On any frame where chrome changes, Phase 1's existing
`force_full` fires → **full-window render + full-window present** (`glReadPixels` +
`XPutImage` of the whole window ≈ 60 ms). Acceptable: chrome is user-driven /
transient, instruments already throttle to ~2 fps on software GL, and keystrokes
stay on the ~ms partial path. (The spike screenshot is a full-window present, and it
renders chrome correctly.)

## Present-path unit

A small `x11_present` module (real version of the spike's throwaway) owns the X
connection, window id, a GC, and the window depth (built from winit's raw handle,
mirroring `x11_blur`). It exposes a present-rect operation
(`glReadPixels` + row-flip + `XPutImage`) used for both the damage bbox (partial) and
the whole window (full). It is **X11-only** (`#[cfg(feature = "x11")]`), returns
`None` on Wayland, and is not a second renderer.

## Platform & pixel format

- **X11 (GLX) only.** Primary target: the milkv over X11-over-ssh, where a small
  `XPutImage` = small data over the link. Local X11 benefits too.
- Handle **depth 24 and 32 TrueColor** (BGRX / BGRA on little-endian, matching a
  `GL_BGRA` readback). On any **unfamiliar visual/depth/byte-order**, do not engage
  Route 1 — fall back to the full path (`swap_buffers`).

## Failure-safety (core invariant, inherited from Phase 1)

Route 1 is **always falsifiable to a full frame**. Any uncertainty — unusual depth,
a `glReadPixels` or `XPutImage` error, a damage bbox we can't trust, first frame,
resize — forces a full-window render + present, or falls back to plain
`swap_buffers`. It can never corrupt the display; worst case is today's cost.
Hardware GPUs and Wayland are entirely untouched.

## Testing / verification

- **Scissored shading correctness:** reuse Phase 1's offscreen pixel-identity gate
  unchanged (it already proves scissored == full shading).
- **X11 present round-trip (new):** render a known rect, `XPutImage` it, `get_image`
  it back from the window, assert equality. Runnable under Xvfb; `#[ignore]`d by
  default like the existing GL gate (needs a live X server).
- **Perf gate:** milkv keystroke frame ~250 ms → few-ms, measured under Xvfb *and*
  confirmed over real X11-over-ssh.
- **Visual confirmation:** on a real X11-over-ssh session (the user's display).
- **Regression:** hardware-GPU and Wayland paths unchanged (Route 1 gated to X11 + software GL).

## Risks / open questions (each with a safe fallback)

- **Pixel format / byte order across real X servers.** The spike validated depth-24
  BGRX under Xvfb; real servers may differ. Handle 24/32; else fall back to full swap.
- **Never swapping a double-buffered GLX context.** The spike ran many frames cleanly
  with readback+XPutImage and no swap; low risk, but watch for drivers that assume a
  swap — fall back if the back buffer read comes back empty.
- **Coordinate alignment.** The readback rect, the `XPutImage` rect, and the damage
  bbox must match exactly; all share Phase 1's top-left physical-px space, with only
  `glReadPixels` needing the Y-flip.
- **ssh readback cost at scale.** `glReadPixels` runs locally on the milkv (softpipe),
  measured ~0.65 ms for a small rect; only the (small) `XPutImage` crosses the wire.

## Spike evidence (2026-07-12, milkv, Xvfb, softpipe GLX)

- `SPIKE x11 small=Some((650, 144))` — 240×64 rect: readback 0.65 ms, XPutImage 0.14 ms.
- `SPIKE x11 full=Some((57967, 5536))` — full window: readback ~56 ms, XPutImage ~5.5 ms.
- Captured Xvfb screen = complete correct render (verified visually); `XPutImage` `Ok` every frame.
