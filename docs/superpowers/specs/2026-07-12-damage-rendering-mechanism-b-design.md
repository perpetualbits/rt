# Damage-based rendering — mechanism B (buffer_age-independent preservation) — design

Status: approved design, pre-implementation.
Date: 2026-07-12.
Builds on: `2026-07-11-damage-based-rendering-design.md` (Phase 1, mechanism A — merged to main at `e681478`).

## Problem

Phase 1 (mechanism A) makes a software-GL keystroke cheap by re-shading only the
damaged cells and presenting with EGL `swap_buffers_with_damage`, relying on
`buffer_age()` to know how much of the back buffer to trust. On measurement, the
target board defeats this:

- **milkv (StarFive JH7110, riscv64, Mesa softpipe, headless weston/Wayland/EGL):**
  `buffer_age()` returns **0 on every frame** (53/53 measured). The EGL surface is
  real, software GL is confirmed, no wires/overlays — the partial path is blocked
  **solely** by `age == 0`, so the safety design correctly falls back to a full
  frame every time. Phase 1 delivers **no win** on this board.
- A full frame costs **~251 ms**, split (measured, glFinish-instrumented) into
  **~248 ms shading + ~3 ms present**. The cost is ~99 % full-window software
  rasterisation; the weston present is cheap.

Because the expensive part is *shading* — which partial redraw already
eliminates — and the present is cheap, the win is recoverable **if we can
preserve the undamaged pixels without depending on `buffer_age`.** That is
mechanism **B**, listed as a reserve in the Phase 1 design with exactly this
trigger ("preserved back-buffer swap proves unreliable on a target driver").

## Goal

- Recover the software-GL keystroke win (~248 ms → low-ms) on boards where
  `buffer_age()` is unusable, by preserving undamaged pixels independently of
  `buffer_age`.
- **Coexist** with mechanism A: A stays where `buffer_age` works (cheaper — no
  blit); B engages only when `buffer_age == 0`. The **hardware-GPU path stays
  byte-for-byte unchanged**, as in Phase 1.
- Reuse Phase 1 wholesale (damage accumulator, `border_bands`, `scissor_box` /
  `begin_frame_scissored`, `force_full` invalidation). **Only the
  preservation-and-present step is new.**

## Non-goals

- X11-over-ssh / indirect-GLX present — still **Phase 2** (readback + `XPutImage`),
  out of scope here. B targets local/Wayland software GL.
- Mechanism **C** (second XRender backend) — remains an un-built reserve.
- Changing the glyph-atlas renderer's drawing model or Phase 1's shading path.

## Architecture & gating

Phase 1's `redraw()` already funnels each frame to a path. B adds one branch:

```
hardware GL ........................→ full path (unchanged, byte-identical)
software GL, buffer_age usable .....→ mechanism A (Phase 1, unchanged)
software GL, buffer_age == 0 .......→ mechanism B          (NEW)
software GL, B unavailable ..........→ full path (safe fallback, today's cost)
```

B reuses Phase 1's entire damage pipeline. The scissored *shading* (re-shade only
the damage bbox) is identical; B only changes how the undamaged remainder is
preserved and how the frame is presented. Damage remains **always falsifiable to
`Full`**: any uncertainty forces a full frame.

## The decision spike (first implementation step; gates the rest)

Before building either endpoint, one throwaway measurement on the milkv resolves
which mechanism to ship:

1. **Preserved-swap support:** can the EGL surface honor
   `EGL_SWAP_BEHAVIOR = EGL_BUFFER_PRESERVED`? Query the chosen config's
   `EGL_SURFACE_TYPE` for `EGL_SWAP_BEHAVIOR_PRESERVED_BIT`; if present, set the
   attrib via `eglSurfaceAttrib` and read it back to confirm it stuck.
2. **If preserved-swap is honored:** measure a scissored keystroke frame with it
   on — expect ~low-ms (shade + cheap swap).
3. **If not honored:** measure a full-FBO→back-buffer `glBlitFramebuffer` cost, to
   confirm the FBO endpoint still beats 251 ms.

**Decision rule:**
- Preserved-swap honored → ship **Endpoint P** (near-zero new code).
- Not honored, but full-blit ≪ 248 ms → ship **Endpoint F**.
- Neither viable → B can't help this board; document it and leave the milkv to
  Phase 2. (No regression: the board keeps today's full-frame behaviour.)

The spike prevents building an FBO we may not need and prevents shipping a
mechanism that doesn't actually pay off. It reuses the existing milkv
build-and-measure workflow (headless weston, softpipe, `RUST_LOG` frame timing).

## Mechanism endpoints

Both reuse Phase 1's scissored shading unchanged — a keystroke re-shades only its
1–2 damaged cells. They differ only in preservation + present.

### Endpoint P — EGL preserved-swap (cheap; preferred if supported)

At surface init, set `EGL_SWAP_BEHAVIOR = EGL_BUFFER_PRESERVED` via raw EGL on
glutin's `Surface::Egl` handle (glutin 0.32 does not expose swap-behaviour, so we
use the raw `EGLDisplay`/`EGLSurface` with the already-linked EGL entry points).
With preserved swap, the back buffer after a swap **is** the previous frame, so
Phase 1's `redraw_scissored` (clear+redraw only the damage bbox, leave the rest)
is already correct — no `buffer_age` needed. Present is the normal cheap
`swap_buffers` (~3 ms). B-eligibility becomes "preserved-swap active" (equivalent
to `age == 1`).

**Integration caveat:** glutin selects the EGL config. If its chosen config lacks
`EGL_SWAP_BEHAVIOR_PRESERVED_BIT`, `eglSurfaceAttrib` will not take. The build
must either nudge glutin's config selection to request the preserved bit, or —
if that config isn't available — fall to Endpoint F. The spike detects this.

### Endpoint F — persistent FBO + blit (robust; driver-independent fallback)

Allocate an offscreen colour FBO sized to the window, **never cleared** (except a
full first frame). Redraw binds the FBO and scissor-redraws only the damage into
it; the FBO retains everything else across frames — true preservation, wholly
independent of `buffer_age`. To present: **blit the whole FBO → back buffer**
(`glBlitFramebuffer`, a straight copy — no per-glyph shading), then
`swap_buffers`. Blitting the full FBO every frame keeps the back buffer complete
regardless of its untrusted age.

- **egui / instruments** paint onto the back buffer **after** the blit, so chrome
  composites over the fresh terminal image each frame (one blend, no
  accumulation). Overlays (menu/prefs/manual/search) still force `Full`.
- **Resize** recreates the FBO at the new size and forces a full frame.
- **Cost:** the only unknown is the full-blit time (the spike measures it). Even
  ~15–20 ms would be a ~12× win over 251 ms; a copy is far cheaper than
  re-shading every glyph.

## Reuse of Phase 1

- **Unchanged:** the `Damage`/`CellDamage` engine plumbing, `DamageAccumulator`,
  `border_bands`, `scissor_box`, `begin_frame_scissored`, `clear_scissor`, all
  `force_full` invalidation, the wire/titlebar/overlay handling.
- **New, small:** the B eligibility check in `redraw()`'s gate; and one of
  {set-preserved-swap-attrib | FBO-alloc + bind + blit} depending on the spike.

## Correctness & testing

- **Extend the offscreen pixel-identity gate** with a *preservation* test: run a
  sequence of partial single-cell updates through Endpoint F (FBO path) and assert
  the final framebuffer is pixel-identical to a full redraw — proving no stale /
  ghost pixels accumulate across frames. (Phase 1's gate already proves scissored
  shading == full shading for one change; this adds the multi-frame preservation
  guarantee.)
- **Endpoint P** (preserved-swap) can't be fully exercised offscreen (needs a real
  swapping window surface); it leans on the on-board measurement + the
  falsifiable-to-`Full` safety net.
- **Hardware path** stays untouched (B engages only on software GL +
  `buffer_age == 0`); regression-verify visually as in Phase 1.
- **Perf gate:** on the milkv, a keystroke frame must drop from ~251 ms to the
  low-tens-of-ms or better (endpoint-dependent), measured with the same
  `RUST_LOG` frame timing under headless weston.

## Branch / merge relationship

Phase 1 is merged to `main` (`e681478`). B is built on a **fresh branch off
main**, keeping its diff small and focused (it only adds the B branch + endpoint).

## Risks / open questions

- **Preserved-swap config availability (Endpoint P).** glutin's chosen EGL config
  may lack the preserved bit; nudging config selection is the mitigation, else
  fall to F. Resolved by the spike.
- **Full-blit cost (Endpoint F).** Must be measured on the milkv; if it's a large
  fraction of 248 ms the win shrinks. The spike de-risks this before committing.
- **egui-over-FBO ordering (Endpoint F).** Chrome must paint after the blit;
  getting the order wrong would drop or double-blend instruments — covered by the
  visual regression check and the preservation test's full-frame compare.
- **Does the milkv support *either* mechanism?** If neither preserved-swap nor an
  acceptable blit is available, B honestly cannot help this board and the win
  waits for Phase 2 — the spike tells us this early, before implementation.
