# ADR-0004 — Adopt egui for UI chrome (keep the custom terminal renderer)

> **Status: SUPERSEDED (2026-07-19).** egui was adopted as below, then removed.
> Over `ssh -X` its full-frame, pixel-shipping overlay was far too heavy (see
> `docs/remote-rendering-lessons.md`), so a command-drawn **native chrome** grew
> up on the XRender backend (menu, search, manual, preferences, instruments) and
> was then unified onto the GL path too — the GL renderer gained AA
> circle/line/mask primitives (`raster.rs` + `render.rs`) so the instruments no
> longer needed egui. egui / egui-winit / egui_glow are gone. The decision that
> held: **keep the custom GL glyph renderer for the grid.** The colour picker the
> context below anticipated is native (`chrome/colour_picker.rs`).

## Context
rt currently hand-draws everything in GL (the terminal grid, the tab strip, the
context menu). That was fine for those, but the Terminator feature set we want to
reach needs *rich* widgets that are painful to hand-roll and easy to get wrong:

- a **preferences dialog** (many pages, checkboxes, combos, spinners),
- **colour pickers** for foreground/background/cursor and the 16-colour palette,
- **font choosers**, image choosers,
- a **keybinding capture** control,
- tree/list views (profiles, layouts, plugins),
- text-entry fields (custom command, tab/title editing, search bar).

"Otherwise we are fated to invent our own [widgets]" — correct, and there are
mature options. We should use one, provided it works with our GL stack.

## Decision
Adopt **egui** (immediate-mode GUI) for rt's *chrome*, integrated as an overlay
on our existing winit + glutin + glow pipeline via **`egui-winit`** (event input)
and **`egui_glow`** (rendering). Keep the **custom GL glyph renderer for the
terminal grid** — egui is not built for a high-throughput cell grid, and our
renderer already does colours/attrs/cursor/columns well.

### Why egui (over iced / slint / imgui)
- It layers onto *our* loop instead of owning it: we render the terminal, then
  paint egui on top, then swap — no rewrite of the run-loop.
- It ships exactly the widgets we need, including a **colour picker** and
  sliders, out of the box.
- It targets our exact stack: `egui-winit` speaks winit events, `egui_glow`
  renders with glow.

### Verified compatibility (probed 2026-07-06)
`egui 0.35` resolves to **winit 0.30.13** (unifies cleanly with our 0.30.9) and
**glow 0.17**. Action item: bump rt's `glow` 0.16 → **0.17** so rt's renderer and
`egui_glow` share one `glow::Context` (a small API bump). winit already unifies.

## Consequences
- **Migrate incrementally.** The terminal grid stays custom. The preferences
  dialog and colour/font pickers are built in egui first (they need it most).
  The context menu and tab strip *may* later move to egui for consistency, but
  they work today and are not urgent.
- **One extra render pass per frame** (egui), only when chrome is visible —
  negligible.
- **Input routing:** `egui-winit` gets first look at window events; if egui wants
  an event (a dialog is open / pointer is over a widget) it consumes it,
  otherwise it falls through to the terminal. We already centralise input, so
  this is a clean insertion point.
- **Wayland-native stays intact:** egui-winit uses the same winit backend; no X11
  is introduced.

## Alternatives considered
- **Keep hand-rolling.** Rejected: a correct colour picker + preferences tree +
  keybinding capture is a lot of bespoke code for no advantage.
- **iced / slint.** Retained-mode frameworks that prefer to own the window and
  loop; integrating a custom GL terminal grid alongside is more friction than
  egui's overlay model.
- **Dear ImGui (imgui-rs).** Similar to egui but a C++ binding; egui is pure Rust
  and better integrated with winit/glow.
