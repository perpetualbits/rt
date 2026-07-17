# Remote translucency over `ssh -X` (XRender) — Design

**Status:** approved design, pre-plan.

**Goal:** Make rt's window translucent over `ssh -X` (the XRender backend),
honouring the existing `background_opacity` setting — matching what Terminator
already does on the same Xwayland/cosmic-comp path. Local (Wayland) translucency
already works and is untouched.

## Background

rt supports background translucency, and it works on the local GL/Wayland
backend: the clear colour carries alpha, and cosmic-comp blends the window.
Over `ssh -X` it does not — the window comes out opaque regardless of
`background_opacity`.

Diagnosis (this session, on the milkv over `ssh -X` to the laptop's Xwayland):

- The Xwayland the milkv talks to **does offer a 32-bit ARGB visual**
  (`xdpyinfo` shows a `depth 32` visual), so translucency is achievable on this
  compositor. Terminator proves it: it is translucent over `ssh -X` on the same
  path (blur is a separate, out-of-scope matter — see Non-goals).
- But rt's window over `ssh -X` is **`depth=24`** (opaque). Confirmed from rt's
  own `xrender: ready (… depth=24 …)` startup log, twice. A 24-bit drawable has
  no alpha channel, so the XRender clear's `background_opacity` alpha is silently
  discarded. rt's back-buffer pixmap is created at the window's depth
  (`xrender_backend.rs`: `create_pixmap(depth, back_pixmap, …)`, `depth =
  geo.depth`), so the whole pipeline is opaque and the presented window has no
  alpha for the compositor to blend.

### Why the window is 24-bit

rt requests transparency — `WindowAttributes::with_transparent(true)` and
`ConfigTemplateBuilder::with_alpha_size(8)` (`main.rs`). The window is created by
glutin's `DisplayBuilder`, whose chosen GL config drives the X11 window's visual.
rt's config-selection closure prefers configs by GL-drawable `alpha_size()` and
`num_samples()`:

```rust
configs.reduce(|a, b| {
    let better_alpha = b.alpha_size() > a.alpha_size();
    let same_more_samples = b.alpha_size() == a.alpha_size() && b.num_samples() > a.num_samples();
    if better_alpha || same_more_samples { b } else { a }
})
```

On X11 this is the wrong discriminator. A GL config can have `alpha_size == 8`
(alpha in the GL *drawable*) while its native X11 **visual** is still 24-bit —
producing an **opaque window**. What actually decides window transparency on X11
is the config's *visual*, exposed by glutin as
`GlConfig::supports_transparency() -> Option<bool>` (confirmed present in glutin
0.32.3; the `GlConfig` trait is already in scope via `glutin::prelude::*`). It is
`Some(true)` only when the config's native visual is 32-bit ARGB. rt never
consults it, so it lands on a 24-bit-visual config. Terminator/GTK avoids this by
explicitly selecting a 32-bit ARGB visual for its window.

## Architecture

Two small, independent changes. No new config, no prefs change, no new modules.

### 1. Prefer a transparency-capable GL config (`main.rs`)

In the `display_builder.build(…)` config-selection closure, prefer configs whose
**visual** supports transparency, *before* the existing alpha-size / sample-count
preference:

```rust
configs.reduce(|a, b| {
    // On X11 the WINDOW's visual — not the GL drawable's alpha — decides
    // transparency: a config can have alpha_size 8 yet a 24-bit visual, giving
    // an OPAQUE window. supports_transparency() is Some(true) only when the
    // config's native visual is 32-bit ARGB, which is what a translucent window
    // over Xwayland needs. Prefer that first; then alpha size; then samples.
    let (at, bt) = (a.supports_transparency().unwrap_or(false),
                    b.supports_transparency().unwrap_or(false));
    if at != bt {
        return if bt { b } else { a };
    }
    let better_alpha = b.alpha_size() > a.alpha_size();
    let same_more_samples = b.alpha_size() == a.alpha_size() && b.num_samples() > a.num_samples();
    if better_alpha || same_more_samples { b } else { a }
})
```

Effect on X11/Xwayland: rt selects the 32-bit ARGB-visual config, so the window
is created 32-bit. Because the XRender back-buffer is created at the window depth,
it too becomes 32-bit and carries alpha end-to-end; `present`'s server-side
`CopyArea` copies that alpha to the 32-bit window, and cosmic-comp blends it.

Effect on native Wayland: transparency-capable configs are already chosen there
(local translucency works today), so preferring them first changes nothing.

Effect where no 32-bit-visual config exists (a bare X server with no compositor,
or an unusual setup): `supports_transparency()` is `Some(false)`/`None` for all
configs, the closure falls through to today's alpha/sample preference, and the
window is 24-bit — exactly as now. No regression.

### 2. Premultiplied translucent clear on a 32-bit window (`xrender_backend.rs`)

`begin_frame` clears the back-buffer with `to_render_color(bg)` — **straight**
alpha. Wayland/Xwayland surfaces expect **premultiplied** alpha (the GL path
already premultiplies: `clear_color(bg.0*a, bg.1*a, bg.2*a, a)`). Straight alpha
would blend too bright / haloed.

Branch the clear on the window depth (`self.depth`, already a field):

- **`depth == 32`:** clear with `premultiply(bg)` — the existing helper that
  produces premultiplied 16-bit RENDER colour. Translucent, correctly blended.
- **`depth != 32`:** clear opaque as today. On a 24-bit drawable the alpha is
  moot anyway; crucially, do **not** premultiply there — premultiplying without
  an alpha channel to carry it would just darken the RGB, producing a wrong dark
  opaque background.

Only the *clear* is translucent. Cells, glyphs, cursor, chrome continue to draw
opaque (`alpha = 1.0`) on top — exactly as the GL path does (translucent base,
opaque content). No other draw path changes.

## Data flow (32-bit path, per frame)

```
config selection → 32-bit ARGB window  (fix 1)
  → back_pixmap created at depth 32 (carries alpha)
begin_frame: SRC-clear back_pic with premultiply(bg)   (fix 2)  → translucent base
draw_panes / chrome: opaque cells + glyphs on top      (unchanged)
present: CopyArea back_pixmap → 32-bit window          (unchanged; copies alpha)
  → cosmic-comp blends the window over the desktop      (compositor)
```

## Components / files

- `crates/rt/src/main.rs` — the GL-config-selection closure (fix 1).
- `crates/rt/src/xrender_backend.rs` — `begin_frame`'s clear (fix 2). `premultiply`
  and `self.depth` already exist.

No new files, no config schema change, no prefs UI change (the opacity control
already exists and will simply start working over `ssh -X`).

## Correctness & performance gates

1. **Window depth flips to 32 over `ssh -X` (falsifiable, milkv).** With the
   config fix, rt's `xrender: ready (… depth=NN …)` log reads `depth=32` over
   `ssh -X` where it read `depth=24` before. This is the mechanical proof the
   ARGB visual was selected.
2. **Translucency is visible and matches Terminator (milkv feel-test).** rt over
   `ssh -X` shows the desktop through its background at `background_opacity`,
   side-by-side comparable with Terminator on the same display. Glyphs/chrome
   stay fully opaque and legible.
3. **No local regression (Wayland).** On the laptop's native Wayland session,
   translucency and blur look exactly as before (the config fix is a no-op there;
   the clear fix's `depth == 32` branch matches the GL path's premultiplied
   clear).
4. **No regression on a non-compositing X server.** Under Xvfb (typically no
   32-bit-visual config, or no compositor), rt still starts and renders opaque,
   no crash — the config closure falls through to the opaque path. Even if a
   given Xvfb *does* expose a 32-bit visual and the clear takes the `depth == 32`
   branch, there is no visible change: the default `background_opacity` is `1.0`,
   and `premultiply(bg)` at alpha 1.0 is `bg` with opaque alpha — identical to the
   opaque clear. So the existing `xrender_commands` / `instrument_compositing`
   Xvfb gates (which never set opacity and assert `PutImage == 0`, glyph/fill
   counts, etc.) must still pass unchanged.

## Non-goals

- **Blur over `ssh -X`.** cosmic-comp implements blur (local Wayland blur works),
  but there is no path for an X11 client to *request* it: Xwayland does not map
  an X11 blur property (`_KDE_NET_WM_BLUR_BEHIND_REGION`, which `x11_blur.rs`
  sets) to the Wayland `ext-background-effect-v1` protocol, and cosmic-comp does
  not honour that property for Xwayland surfaces. Terminator hits the same wall.
  Fixing this belongs in Xwayland or cosmic-comp, not rt.
- **Changing the local (Wayland) translucency or blur** — both work; untouched.
- **New opacity/blur config or prefs controls** — the existing `background_opacity`
  and its prefs slider are reused as-is.
- **Premultiplication of the whole draw pipeline** — only the clear needs it; all
  other content is opaque.

## Alternatives considered (rejected)

- **A dedicated 32-bit ARGB window created directly via x11rb for the XRender
  backend, bypassing glutin.** Matches how GTK/Terminator do it and removes the
  glutin dependency from the transparency question. Rejected: rt has ONE window,
  created by glutin for the GL path; introducing a second window-creation path
  for XRender means two code paths for window lifecycle, resize, and event
  routing. Preferring the right glutin config is one line of intent and reuses
  the existing single-window flow.
- **Forcing `depth=32` unconditionally.** Rejected: on a server with no 32-bit
  visual (bare X, no compositor) this would fail window creation or present a
  window nothing composites. Preferring-when-available degrades cleanly.
- **Premultiplying the clear regardless of depth.** Rejected: on a 24-bit
  drawable it darkens the background (RGB scaled by alpha with no alpha channel
  to carry it), a visible wrong result. The depth branch is necessary.
