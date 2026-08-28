# Vendored winit-wayland (touch on the window decoration)

`vendor/winit-wayland` is a **vendored fork** of `winit-wayland 0.31.0-beta.2`
carrying a single change: **touch and tablet events reach the client-side
decoration.**

## The bug it fixes

winit 0.31 made a finger a first-class pointer *inside* the window — that is
what gave rt touch and stylus support at all (see
[touch-and-stylus.md](touch-and-stylus.md)). It stops at the frame. On GNOME a
Wayland client must draw its own decorations, winit draws them as subsurfaces,
and the frame is fed **only** from `seat/pointer`:

```rust
// seat/pointer/mod.rs — the pointer handler resolves the parent surface
let parent_surface = data.parent_surface().unwrap_or(surface);
let window_id = crate::make_wid(parent_surface);
...
PointerEventKind::Press { .. } if parent_surface != surface => window.frame_click(..)
```

The touch handler did no such resolution:

```rust
// seat/touch/mod.rs — upstream
let window_id = crate::make_wid(&surface);
let scale_factor = match self.windows.get_mut().get(&window_id) {
    Some(window) => ...,
    None => return,          // <-- a touch on the frame lands here, and is dropped
};
```

A touch on the title bar or a resize edge reports the *frame's* surface, which
is not a window in `WinitState::windows`, so the lookup missed and the event was
discarded.

The tablet handler threw the same events away more explicitly still:

```rust
// types/wp_tablet_input_v2.rs — upstream
Some((surface, Some(surface_data)))
    if surface_data.parent_surface().is_none() => surface,   // top-level only
_ => continue,                                               // <-- the frame, dropped
```

The result: **no winit window could be moved, resized or closed by finger or by
pen** — not rt's, not any application's — while every GTK application on the
same screen could do both.

Note this is not avoidable by drawing your own decorations instead.
`WindowState::drag_window` (and `drag_resize_window`) iterate `self.pointers`
for a serial and carry an upstream `// TODO(kchibisov) handle touch serials.`,
so an application-drawn title bar cannot start a touch move either. A patch to
this crate is the only route.

## What exactly was changed vs. upstream

Three files. `src/window/state.rs` gains one four-line method shared by both
input paths:

- `WindowState::frame_cancel_pending_move()` — see the last bullet below.

`src/seat/touch/mod.rs`:

- A `touched_window()` helper resolves a touched surface to `(WindowId,
  on_decoration)` through `SurfaceData::parent_surface()` — the same resolution
  the pointer handler already did.
- `down`, `up`, `motion` and `cancel` route a decoration touch to
  `frame_point_moved` / `frame_click` / `frame_point_left` instead of emitting
  application pointer events. (`down` and `up` previously ignored their `serial`
  and `time` arguments; `frame_click` needs both.)
- `down` calls `frame_point_moved` **before** the click: the frame acts on the
  part it last saw a point over, and a finger has no hover to have established
  that.
- `down` calls `frame_point_moved` **again after** the click. A title-bar click
  only arms the move (`has_pending_move`); upstream fires it from the next
  pointer motion, which is how a mouse drag begins. For a finger that is wrong —
  the compositor takes the grab as the move starts, so the motion that would
  have fired it may never arrive — so the same point is fed back in to fire it
  on the press.
- `up` calls the new `WindowState::frame_cancel_pending_move()`, so a tap that
  armed a move it never used cannot leave the serial behind for a later,
  unrelated hover to trip over.

`src/types/wp_tablet_input_v2.rs` does the same for a pen, plus the plumbing
that made it possible:

- A tablet tool needs its `WlSeat` to drive the frame (`xdg_toplevel.move` and
  `.resize` are addressed to a seat) and the tool protocol never names the seat
  it belongs to. The tablet seat's user data changes from `()` to the `WlSeat`
  it was created from, and the `ToolAdded` event — previously ignored entirely —
  records that seat on the new tool. (`event_created_child!` can only
  default-construct a child's user data, so it cannot be injected at creation.)
- `TabletToolDataInner` gains that `seat` field.
- `ToolEvent::Frame { time }` was matched as `{ .. }`; the timestamp it carries
  is what `frame_click` and `frame_point_moved` want.
- A tool over a decoration is routed to `TabletManager::tool_on_decoration`,
  which mirrors the touch path: hover on enter and motion, frame click on the
  **tip** only (a barrel button is not a title-bar click), the same fire-on-press
  and disarm-on-release handling, and `frame_point_left` when the tool leaves.
  The surface is cloned rather than borrowed so the tool's state stays writable
  for the `Left` cleanup, which the decoration path performs exactly as the
  main path does.

Nothing else in the crate is touched, and no public API changes: rt depends on
`winit`, which pulls this crate in, redirected by `[patch.crates-io]` in the
root `Cargo.toml`.

## Upkeep, and going upstream

Unlike the [vendored engine](vendored-engine.md), this fork is **meant to be
temporary**. It is a plain bug fix in a pre-release crate, with no policy
obstacle to upstreaming, and it should be offered to `rust-windowing/winit`. Two
things to know when re-syncing:

- The vendored version string stays `0.31.0-beta.2` so `winit`'s
  `=0.31.0-beta.2` requirement still resolves.
- When winit 0.31 final lands, re-copy the crate and re-apply the diff — or drop
  `vendor/winit-wayland` and the patch entry entirely if the fix has landed
  upstream by then.
