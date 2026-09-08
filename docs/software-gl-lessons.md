# Software GL: what a frame really costs, and the three things that were wrong

Written 2026-09-08 after rt was found sitting at 2–4 cores on a Milk-V Mars
(4× SiFive U74, riscv64, no GL driver for its PowerVR BXE-4-32, so Mesa 25.0
renders with llvmpipe). Everything below was measured, not inferred; the rig is
in `bench/software-gl/`. Companion to `docs/remote-rendering-lessons.md` (the
`ssh -X` / XRender side of the same question).

## The board, in numbers

| what | value |
|---|---|
| glibc `memcpy` | 247 MB/s (`perf bench mem memcpy`); a 6 MB window buffer copy is ~25 ms |
| llvmpipe | LLVM 19, "128 bits" = emulated SIMD on rv64gc (no vector extension) |
| full-window fragment pass, 1.45 Mpx | ~0.28 core-s at 1 sample; ~1.1 core-s at 4× MSAA |
| llvmpipe fast (unscissored) clear, 1.49 Mpx | 30–45 ms wall |
| Mesa shader-variant JIT on first use of a config | ~8–10 s of one core (disk-cached afterwards) |
| XRender backend over Xwayland, same workload | 0.01–0.5 cores, zero frames while idle |

## How it was profiled

1. `ps -L` per thread: 4 threads named `llvmpipe-N` at ~42% each, the rt
   threads near zero. So: pixels, not parsing.
2. `perf record -e cpu-clock` on the live process: 93% in `[JIT]` (llvmpipe's
   LLVM-compiled fragment code), 6% libgallium, 1% libc. Rust code 0.2%.
3. `strace -e sendmsg` on the Wayland socket: one commit every ~0.6 s while
   ~35 core-seconds burned in 12 s → **~1.75 core-seconds per frame**.
4. A headless `weston --backend=headless --xwayland` on the board with rt's
   pane driven by a scripted `SHELL` (idle / 1 char per second / clears / an
   output flood / a hot child with no output), a 0.5 s per-thread CPU sampler,
   and the Wayland socket straced for frame timestamps. Never on the live
   desktop.
5. Inside rt: `RUST_LOG=rt::frame=debug` (one line per frame: plan, rects,
   bbox, vertices, wall ms, and *why* the frame was asked for) and
   `RT_FRAME_SYNC=1` (a `glFinish` after clear, draw and swap, with the CPU
   ticks each phase burned on the llvmpipe workers and the main thread, read
   from `/proc/self/task/*/stat`). `RT_FRAME_SYNC=2` proved glFinish is a real
   barrier (zero worker ticks in a 400 ms sleep after it).
6. Mesa 25.0.7 source, to interpret the numbers. Note for next time: Debian's
   release Mesa compiles out `debug_printf`, so `LP_DEBUG=…` prints nothing,
   `LP_DEBUG=counters` needs a debug build, and the `LP_PERF` flags
   (`no_tex,no_blend,no_shade,no_rast_linear` — there is no `no_rast`) only
   touch llvmpipe's *linear* fast path, so they do not move the general
   fragment path at all. Do not waste runs on them.

The per-frame anatomy of a "partial" (keystroke) frame with the shipped
v0.3.19, from `RT_FRAME_SYNC=1`:

| phase | wall | llvmpipe CPU | main CPU |
|---|---|---|---|
| scissored clear | 300–410 ms | 1.1 core-s | ~0 |
| draw (~30 quads) | ~50 ms | 0.12 core-s | ~0 |
| `eglSwapBuffers` | ~190 ms | 0.62 core-s | 0.02 |

Thirty quads cannot cost that. The three real causes:

## 1. rt was rendering into a 4× multisampled framebuffer

The GL config reducer in `main.rs` broke ties with "prefer more samples" (a
glutin example idiom). llvmpipe on Wayland exposes 4× MSAA configs, sorted
*after* the plain ones, so the tie-break picked them. Consequences: every
clear and quad shades four samples, the swap runs a full-window MSAA resolve
blit through the fragment pipeline (`drisw_swap_buffers_with_damage` →
`dri_pipe_blit`; that is the 0.62 core-s inside `eglSwapBuffers`), and the
texture is four times the memory.

MSAA cannot help rt at all: everything it draws is an axis-aligned,
pixel-snapped textured quad, and MSAA only anti-aliases triangle edges. It is
pure cost on every GPU, not only software ones. **Fix:** the reducer prefers
*fewer* samples, and the chosen config is logged at info level
(`GL config: alpha=8 samples=0 …`). This alone cut the workload 4–8×.

## 2. Every "partial" frame was the whole pane

`redraw()` folds the pane's four 6 px border bands (plus the titlebar strip)
into every non-full frame so the focus outline and instruments are redrawn
over a cleared background. `DamageAccumulator::finish()` merged any two rects
that *touched* into their bounding box. The bands touch at the corners, so
they merged into one pane-sized rect, which then swallowed the keystroke's
cell too: the frame log showed `partial 1615x898@8,8 rects=1` on a 1631×914
window, every time.

And a scissored clear is not cheap on llvmpipe: the driver does not advertise
`clear_scissored`, so Mesa's state tracker implements `glClear` with the
scissor test on as a *drawn quad* (`clear_with_quad` in `st_cb_clear.c`),
i.e. a full fragment pass over the scissor box, ten times the cost of the
unscissored tile clear. **Fixes:** (a) `finish()` merges only tightly — the
union may waste at most half again the covered area — so bands stay bands and
aligned neighbours still coalesce; (b) the GL renderer has
`begin_frame_scissored_rects`: it clears each rect and issues the frame's one
vertex batch once per rect (≤ 24 rects, else the bbox). The pixel-identity
tests (`crates/rt/tests/damage_pixel_identity.rs`, `--ignored`, needs EGL)
cover the multi-rect case and prove the gaps between rects are preserved.
XRender and wgpu keep the bbox path (their partial cost is not per pixel).

## 3. The latency flare fed on rt's own slowness

The latency instrument flares when a 16 ms wake arrives >10 ms late. A 200 ms
paint made every following wake "late", the flare requested an animation
frame, that frame was late, and so on: the frame log read `why=anim+stall`
for seconds after every keystroke. **Fix:** rt's own time — the whole
`RedrawRequested` handler (snapshots, the EGL buffer-age query, which can
block on the compositor, draw and present) and the previous tick's own work —
is subtracted before judging the overrun. Only genuinely stolen time flares.

## Result (same workload, same board, headless weston, 160×45 cells)

| phase | v0.3.19 (cores) | v0.3.20 (cores) |
|---|---|---|
| 1 keystroke / s | 2.5–3.4 | 0.24 |
| idle tail after typing | 1.7–2.4 | 0.11 |
| full clear + line, 1 / s | 2.4–2.5 | 0.28 |
| output flood (`seq 1 20000`) | 1.5–1.8 | 0.8 |
| hot child, no output | 2.0–2.3 | 0.10 |
| settled idle | ~0 | ~0 |

A keystroke frame is now clear ~25 ms + draw ~35 ms + swap ~35 ms of wall
(the swap is the main-thread memcpy of the window into the wl_shm buffer, at
this board's memcpy speed), about 0.1 core-s, down from ~1.9.

## What is still true, and what to do about it

* The floor per frame on this board is the swap's buffer copy (~6 MB at
  247 MB/s) plus ~30 ms of llvmpipe fixed cost. An output flood settles at
  ~0.8 cores at ~3 fps. That is llvmpipe on rv64gc; the way out of it is not
  in rt's GL path.
* `rt --backend xrender` with `DISPLAY` set to the Xwayland display uses
  ~1/20 of the CPU of the GL path here (Xwayland renders with pixman and rt
  ships only changed cells and scroll blits). It gives up the Wayland-only
  features (compositor-placed tear-out, Wayland text drop, blur). It is the
  right choice for a GPU-less board used interactively; it is not made the
  default because a software-GL box is also every VM on a fast x86 host,
  where GL is fine and the Wayland features matter.
* The board has a Vulkan driver for its GPU ("PowerVR B-Series Vulkan
  Driver", with `VK_KHR_wayland_surface`), but no GL driver, and Mesa's zink
  refuses it ("Imagination proprietary driver w/o geometryShader is
  unsupported"). rt's wgpu backend (macOS/Metal today) over Vulkan would be
  the hardware path for this class of board. Not started.
* Startup still JIT-compiles llvmpipe's shader variants for the config in
  use (once per Mesa version; `~/.cache/mesa_shader_cache_db`). The first
  run after this change pays it again because the non-MSAA variants are new.
