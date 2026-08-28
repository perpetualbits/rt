# Touch and stylus

## The symptom

On a touchscreen laptop (a Lenovo X390, multitouch + AES stylus), rt did not
respond to a finger or to a pen anywhere in its window, while gnome-terminal on
the same screen responded to both. rt's window border also looked nothing like
the rest of the desktop.

Two symptoms, two causes, one root: **rt is a winit client, not a GTK one.**

## Why nothing arrived

X11 has pointer emulation for touch: an X client that knows only about the
pointer still gets clicks when you tap, because the server synthesises them.
**Wayland has none.** A Wayland compositor delivers touch through `wl_touch` and
a stylus through the `tablet_v2` protocol, and a client that never binds those
interfaces is simply not sent the events — there is no fallback path. GTK binds
both (and synthesises its own pointer events from touch), which is why
gnome-terminal worked.

winit 0.30, which rt was built on:

- delivered Wayland touch as `WindowEvent::Touch` — and rt's event handler had
  no arm for it, so every finger event was received and dropped;
- had **no `tablet_v2` implementation at all**, so a stylus produced nothing
  whatsoever, no matter what rt did.

## The fix: winit 0.31

winit 0.31 is the first release that implements `tablet_v2`, and it also
replaces the separate mouse/touch event families with **one set of pointer
events** carrying the source that produced them:

```rust
WindowEvent::PointerButton { state, position, button: ButtonSource, .. }
WindowEvent::PointerMoved  { position, source: PointerSource, .. }
WindowEvent::PointerLeft   { kind: PointerKind, .. }
```

`ButtonSource::mouse_button()` maps a finger to `Left` and a stylus tip to
`Left` (barrel buttons to `Right`/`Middle`). rt collapses the source to that
button once, at the top of `window_event`, and every existing arm below —
menus, tabs, jacks, selection, drag-and-drop — then works for all three devices
without knowing which one it is served. **A tap became a click and a stylus
became a mouse as a consequence of the port, not as a feature written on top of
it.**

Two details that do need code:

- **The press position.** A finger has no hover, so no `PointerMoved` precedes
  its press. `PointerButton` now carries its own position, and rt syncs
  `active.mouse` from it before any hit test runs; without that, the first tap
  would be resolved against wherever the mouse pointer had last been left.
- **Two fingers.** Wayland hands over raw touch points; nothing turns two of
  them into a scroll. `crates/rt/src/touch.rs` is that gesture state — pure,
  headless-testable, no winit types — and it answers one question per touch
  event: does this drive the pointer, scroll the pane, or get swallowed?
  When a second finger lands, the selection the first one had begun is undone,
  so a scroll never leaves a stray highlight behind.

## What works, and what does not

| | finger | stylus |
|---|---|---|
| tap / tip-down = click | yes | yes |
| drag = select, move a pane, drag a gutter | yes | yes |
| two-finger drag = scroll | yes | n/a |
| barrel buttons = right / middle click | n/a | yes |
| pressure and tilt | n/a | ignored — rt is a terminal |
| the window decoration (move / resize / buttons) | yes¹ | yes¹ |

¹ Only through the vendored winit fork. Upstream winit feeds its client-side
decoration from `seat/pointer` alone. A touch or a pen over the title bar or a
resize edge reports the *frame's* surface — a subsurface, not a window in
winit's map — and both input paths threw those events away: touch by looking up
the wrong id and missing, the tablet by an explicit
`if surface_data.parent_surface().is_none()` guard. So **no winit window, in any
application, could be moved, resized or closed by finger or pen**, while every
GTK application on the same screen could. `vendor/winit-wayland` resolves the
parent surface the way the pointer handler always did and routes both to the
frame; see [vendored-winit-wayland.md](vendored-winit-wayland.md).

## The decoration's *appearance*

Separately from touch: mutter never grants a Wayland client server-side
decorations, so a winit client must draw its own. Without winit's
`wayland-csd-adwaita` feature that fallback frame is bare, which is why rt's
border looked unlike everything around it. The feature is now enabled (it pulls
`sctk-adwaita`), and the frame matches the desktop.

## Porting notes (winit 0.30 → 0.31)

Beyond the pointer events, the bump touched:

- `Window` is a **trait**; `create_window` returns `Box<dyn Window>`.
- `ActiveEventLoop` is a trait object: `&dyn ActiveEventLoop`.
- `resumed` is no longer the surface-creation hook — `can_create_surfaces` is.
- `EventLoop` lost its user-event type parameter; `run_app` takes the app by
  value.
- `inner_size` → `surface_size`; `Resized` → `SurfaceResized`.
- `inner_position` split into `outer_position` (window on the desktop, still
  `NotSupported` on Wayland) plus `surface_position` (surface within the
  window). `client_origin()` in `main.rs` puts them back together.
- Cursors moved to `winit::cursor`; `Fullscreen` to `winit::monitor`. A custom
  cursor is now built as a `CustomCursorSource` and realised by the event loop.
- Keys come from the W3C UI-Events set (`keyboard_types`): there is no
  `NamedKey::Space` any more — space is `Key::Character(" ")` — and
  `ModifiersState::super_key` is `meta_key`.
- IME is enabled with `request_ime_update(ImeRequest::Enable(..))` rather than
  `set_ime_allowed`.
- Per-backend window attributes: the app_id (Wayland) and WM_CLASS (X11) are
  set on separate attribute objects, chosen at runtime via `is_wayland()` /
  `is_x11()`.
- `glutin-winit` is still pinned to winit 0.30 and cannot bridge 0.31, so
  glutin is joined to the window directly by raw handle. That also fixes the
  order X11 needs: pick the GL config **first**, create the window with that
  config's visual (which is what makes transparency work under Xwayland), then
  build the surface.

## Testing it

The gesture state is unit-tested headlessly (`cargo test -p rt touch::`). The
rest needs real hardware: run rt on a touchscreen and check a tap focuses a
pane, a one-finger drag selects, a two-finger drag scrolls without leaving a
highlight, and a stylus does the same as a mouse.
