# Plan: first-class X11 support

## Goal

Make rt run **fully** on X11, not just render for screenshots — copy/paste
included — while keeping the default build a clean, Wayland-only dependency tree.
X11 stays behind the existing `x11` Cargo feature, but that feature graduates
from "dev-only screenshot hack" to a **supported, complete backend**.

## Why this is cheap

rt is built on cross-platform layers — winit, glutin, glow, egui, fontdue,
`alacritty_terminal` — so the windowing/GL/input/parsing already work on X11
(winit auto-selects the backend at runtime from `WAYLAND_DISPLAY` / `DISPLAY`).
There are **no `cfg(feature="x11")` branches in the code**: the `x11` feature
only flips winit/glutin backend features, and the same code runs. We have run rt
under Xephyr (real X11) throughout development — rendering, the egui overlays
(menu/preferences/manual), and mouse all work.

So exactly **one** capability is Wayland-bound and must be added: the clipboard.
Two more are Wayland-only but already degrade gracefully and need no work.

## Current state (audit)

| Concern | Wayland | X11 today | Action |
|---|---|---|---|
| Window / GL / input | winit+glutin | works (feature `x11`) | none |
| Rendering (grid + egui overlay) | works | works (verified under Xephyr) | none |
| Mouse (incl. forwarding, hover, scrollbar) | works | works | none |
| **Clipboard + PRIMARY** | smithay-clipboard | **`None` → dead** | **add X11 path** |
| Background blur | ext-background-effect / KDE | no-op (graceful) | none (optional later) |
| Transparency | ARGB + compositor | ARGB + compositor | verify config picks 32-bit visual |
| IME / dead keys | winit | winit (XIM) | test |

## Design

### Clipboard abstraction (`crates/rt/src/clipboard.rs`)

A small enum wrapping either backend, selected at runtime from the window's raw
display handle, exposing the four methods `main.rs` already uses so call sites
barely change:

```rust
pub enum Clipboard {
    Wayland(smithay_clipboard::Clipboard),          // always compiled
    #[cfg(feature = "x11")] X11(X11Clipboard),      // only with the feature
}

impl Clipboard {
    pub fn from_display(handle: RawDisplayHandle) -> Option<Self>;
    pub fn store(&self, text: String);           // CLIPBOARD
    pub fn store_primary(&self, text: String);   // PRIMARY (middle-click)
    pub fn load(&self) -> Result<String, ()>;    // CLIPBOARD
    pub fn load_primary(&self) -> Result<String, ()>; // PRIMARY
}
```

- **Wayland** → smithay-clipboard (unchanged; keeps the good PRIMARY behaviour).
- **X11** → `arboard` (`default-features = false` → x11-only, no image/wayland
  deps), behind the `x11` feature. arboard needs `&mut` for get/set, so wrap it
  in a `RefCell` to present `&self` (rt is single-threaded; smithay presents
  `&self` too). PRIMARY via arboard's `LinuxClipboardKind::Primary`.
- `load`/`load_primary` return `Result<String, ()>` so the existing
  `if let Ok(text) = cb.load()` call sites compile unchanged.

`main.rs` changes: `mod clipboard;`, the field becomes
`Option<clipboard::Clipboard>`, and construction becomes
`Clipboard::from_display(window.display_handle()…)`. The `Xlib`/`Xcb` handles are
matched under the feature; the `Wayland` handle always.

### Feature / dependency wiring (`crates/rt/Cargo.toml`)

- Add `arboard` as an **optional** dep, pulled in only by the `x11` feature:
  `x11 = ["winit/x11", "glutin/glx", …, "dep:arboard"]`.
- Default build is unchanged → zero X11 crates (the clean-tree value is kept).
- `--features x11` produces a **universal** binary that runs on either backend.

### Blur / transparency

- Blur objects (`bg_effect`, `blur`) are already runtime no-ops when the display
  isn't Wayland — no change. (Optional future: `_KDE_NET_WM_BLUR_BEHIND_REGION`
  for KWin-X11/picom — **out of scope** here.)
- Transparency: confirm the GL-config picker selects a 32-bit (alpha) visual on
  X11 so a compositing WM shows through. The existing "prefer alpha_size > 0"
  logic should already do this; verify.

## Work items

- [x] `clipboard.rs`: the enum, `from_display`, four methods, `X11Clipboard`
      (arboard + RefCell) under `#[cfg(feature = "x11")]`.
- [x] `Cargo.toml`: optional `arboard`, wired into the `x11` feature (`dep:arboard`).
- [x] `main.rs`: `mod clipboard`; field type; construction from display handle;
      the four call sites are unchanged (compatible signatures).
- [x] Build **default** (Wayland) — unchanged; arboard resolved but not compiled.
- [x] Build **`--features x11`** — compiles clean (arboard + clipboard path).
- [x] Updated the stale "x11 dev build" comments to reflect first-class support.

## Status: DONE and verified (2026-07-09)

X11 is now a first-class, complete backend behind the `x11` feature.

- **Clipboard round-trip verified under Xephyr (real X11):**
  - rt → X11: triple-click selected a line; `xclip -o -selection primary` read
    back the exact text (PRIMARY store via arboard). ✓
  - X11 → rt: `xclip -i -selection primary` set text; middle-click in rt pasted
    it into the shell (PRIMARY load via arboard). ✓
  - CLIPBOARD uses the same arboard `set_text`/`get_text` calls (a different
    selection atom); not separately triggerable in bare Xephyr (no WM → no
    keyboard focus for Ctrl+Shift+C/V), but exercised by the same code path.
- Rendering, egui overlays, and mouse (forwarding/hover/scrollbar) were already
  confirmed on X11 throughout development.
- Default build stays Wayland-only (zero X11 crates compiled); `--features x11`
  is a universal binary (winit auto-selects the backend at runtime).

Follow-ups remain as listed under **Out of scope** (X11 blur; default-enabling).

## Test plan

- Default build: `cargo build`; clipboard path is the Wayland one (unchanged);
  existing tests pass.
- X11 build under Xephyr (`DISPLAY=:77`), an **automated clipboard round-trip**:
  - rt copies a selection → read it back with `xclip -o` (CLIPBOARD) and
    `xclip -o -selection primary` (PRIMARY).
  - Put text via `xclip -i` → rt pastes it into the pane (observe it reaches the
    shell).
- Manual (documented, not blocking): run the `x11` binary in a real Xorg session
  for transparency (with a compositor) and dead-key/IME behaviour.

## Risks & tradeoffs

- **Dep-tree tax** only when `x11` is enabled (arboard + x11rb/xcb). The default
  tree stays pristine — this is the deliberate compromise.
- **Two-backend maintenance**: a small test matrix and the clipboard divergence
  to keep working. Accepted; the abstraction localizes it to one module.
- **arboard PRIMARY** must be exercised (some environments/WMs are finicky about
  PRIMARY ownership); covered by the round-trip test.

## Out of scope (follow-ups)

- X11 background blur via `_KDE_NET_WM_BLUR_BEHIND_REGION`.
- Making `x11` a default feature (universal single binary shipped by default) —
  a packaging decision, easy to flip later if desired.
