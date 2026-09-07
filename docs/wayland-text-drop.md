# Wayland text drag-and-drop, and why it took the clipboard with it

rt receives text dragged in from another application on three platforms. macOS
and X11 landed first; this is the Wayland half, and the reason it was held back.

The user-visible feature is identical on all three — select a paragraph in
Chrome or Firefox, drag it onto a pane, let go, and it is inserted as if pasted
into the pane **under the pointer**. What differs is the wire, and on Wayland
the wire runs straight through the clipboard.

## The hazard

A Wayland client receives drags on a `wl_data_device`, obtained from
`wl_data_device_manager.get_data_device(seat)`. rt already had one: the clipboard
(and the PRIMARY selection behind it) is `smithay-clipboard`, which creates a
`wl_data_device` per seat for exactly that purpose and exposes no drag hook.

So the question was whether rt could ask for a **second** device — one for
drags, leaving the clipboard's alone. `wayland.xml` neither permits nor forbids
it. The `wl_data_device` interface description says

> There is one wl_data_device per seat which can be obtained from the global
> wl_data_device_manager singleton.

but that is prose in a description block, not a rule with an error attached to
it. Compositor behaviour is therefore de-facto, and wlroots had a bug here that
crashed Firefox (swaywm/wlroots#1384) — enough of a warning to go and read the
source rather than guess.

## What the compositors actually do

| Compositor | `selection` / `data_offer` | DnD `enter`/`leave`/`motion`/`drop` | a client's 2nd `get_data_device` |
|---|---|---|---|
| **Smithay** (cosmic-comp) | all devices | all devices | appends — safe |
| **KWin** | all devices | all devices | appends — safe |
| **wlroots** | all devices | all devices | appends, and seeds the new one — safe |
| **Mutter** (GNOME) | all devices *in the list* | **one**, first found | **unlinks the existing one** |

* **Smithay** — `src/wayland/selection/seat_data.rs`, `SeatData::send_selection`
  iterates `known_devices: Vec<SelectionDevice>` and builds a fresh
  `SelectionOffer` per device; `add_device` pushes and never replaces. The DnD
  side (`selection/data_device/mod.rs`) sends `enter`/`motion`/`leave`/`drop` in
  a `for device in seat_data.known_data_devices()` loop, one `wl_data_offer` per
  device. Checked at the revision cosmic-comp pins, and on master.
* **KWin** — `src/wayland/seat.cpp`, `dataDevicesForSurface` returns a
  `QList<DataDeviceInterface *>` of every matching device; both the selection
  path and `drag.targets` iterate it. `registerDataDevice` even offers the
  current selection to a device created after focus.
* **wlroots** — `types/data_device/wlr_data_device.c`,
  `seat_client_send_selection` is `wl_resource_for_each(device_resource,
  &seat_client->data_devices)`, one offer per resource. That per-device offer IS
  the fix for #1384; the old bug was a single shared `wl_data_source.offer`
  announced twice, not single delivery.
* **Mutter** — the one that bites. `src/wayland/meta-wayland-data-device.c`:

  ```c
  static void
  get_data_device (struct wl_client *client, struct wl_resource *manager_resource,
                   uint32_t id, struct wl_resource *seat_resource)
  {
    cr = wl_resource_create (client, &wl_data_device_interface, ...);
    ...
    data_device_resource =
      wl_resource_find_for_client (&seat->data_device.resource_list, client);
    if (data_device_resource)
      {
        wl_list_remove (wl_resource_get_link (data_device_resource));
        wl_list_init   (wl_resource_get_link (data_device_resource));
      }
    wl_list_insert (&seat->data_device.resource_list, wl_resource_get_link (cr));
  }
  ```

  The client's **existing** device has its list link removed and re-initialised
  to a self-loop. The resource is still alive and the client still holds it, but
  it is in no list, so every later `wl_resource_for_each` over `resource_list` /
  `focus_resource_list` — including the two that send `selection`
  (`owner_changed_cb`, `meta_wayland_data_device_set_focus`) — skips it forever.

  There is no protocol error, no log line, and nothing the client can observe.
  **Paste simply stops working.** Mutter's DnD side is single-resource too: one
  `drag_grab->drag_focus_data_device` pointer, found with
  `wl_resource_find_for_client`, and all four drag events go only to it.

## The decision

Three of four compositors would have been fine with a second device. That is not
good enough when the failure mode on the fourth is *the clipboard silently
stops working on the user's other main desktop*, so rt took the other road: it
owns the one `wl_data_device`, and that device does clipboard, PRIMARY and
drag-and-drop. GTK and Qt both keep exactly one; so does rt now.

The invariant, which is stronger and easier to check than "exactly one":

> **rt creates no `wl_data_device` it did not already create before this
> feature existed.**

Adding drag-and-drop added zero devices, in every configuration, on every
compositor. Whatever a compositor does about multiple devices, it does exactly
what it did before.

## How it was done

`crates/rt/src/wl_clipboard/{mod,state,worker,mime}.rs` began as a **verbatim
copy of smithay-clipboard 0.7.3** (MIT — `LICENSE-smithay-clipboard` sits beside
them). It was landed as a separate, behaviour-free commit precisely so the
clipboard risk is auditable: `diff` against the published crate shows five
lines, all of them imports and a module header.

```
+ use smithay_client_toolkit as sctk;      # the crate got this from a Cargo.toml rename
- use crate::mime::…      + use super::mime::…
- use crate::state::…     + use super::state::…
- use wayland_backend::client::ObjectId;   + use sctk::reexports::client::backend::ObjectId;
```

The dependency graph lost exactly one crate (`smithay-clipboard`) and gained
none: `smithay-client-toolkit` was already in the tree via winit-wayland.

Drag-and-drop is then **additive**. Upstream's
`DataDeviceHandler::{enter,leave,motion,drop_performed}` and
`DataOfferHandler::{source_actions,selected_action}` were empty stubs — the
crate wanted the device only for selections — and rt fills them in. Nothing in
those callbacks touches `data_selection_content`, `primary_selection_content`,
`data_sources`, `primary_sources`, `latest_seat` or any `ClipboardSeatState`;
they are reached only from `wl_data_device`'s DRAG events, which no clipboard or
PRIMARY path ever raises. That is the whole argument for why the feature cannot
regress copy, paste, or middle-click paste.

## Where the decisions live

Nothing above decides anything. Every decision is in `crates/rt/src/textdrop.rs`,
which is un-`cfg`'d so Linux CI tests it — the same split the macOS and X11
receivers use, and the reason all three cannot drift apart:

| | |
|---|---|
| `resolve` | which pane takes the drop (shared with macOS and X11) |
| `payload` | what bytes reach the pty (shared) |
| `pick_drop_mime` | the accept/reject decision, and which flavour to ask for |
| `surface_to_physical` | surface-local (logical) coordinates → physical pixels |
| `may_finish_drop` | whether `wl_data_offer.finish` is legal yet |

`may_finish_drop` is a guard, not a feature. `finish` sent at the wrong moment is
the `invalid_finish` protocol error, and a protocol error does not fail the
request — it tears down rt's whole `wl_display`, and with it every pane, tab and
shell in the process. Its three preconditions (version ≥ 3, actually dropped, a
`copy`-or-`move` action settled) are written out and tested, and a
`const _: () = { assert!(…) }` in `wl_clipboard/state.rs` pins rt's plain-integer
action bits to wayland-client's generated ones so the two cannot drift.

## Deliberate differences from the other two receivers

* **The hover chip says `text`, not a preview of the payload.** Same as X11.
  `wl_data_offer.receive` may legally be called before the drop, but doing it for
  a *label* means a speculative pipe transfer against an arbitrary source on
  every drag enter. AppKit hands the string over for free, so macOS previews;
  neither Linux protocol does, so neither Linux receiver does.
* **The accept/reject answer is made on MIME types alone**, never on where in
  the window the pointer is — the worker thread has no copy of the layout, and
  mirroring it there to change a cursor badge would mean keeping two copies in
  sync across every split, resize and tab switch. The honest signal for "this
  will not land" is the absence of the drop cue. All three receivers do this.
* **Copy, never Move, never Ask.** `Move` tells the source to delete what was
  dragged; a terminal inserts a copy and destroys nothing. `Ask` means "open a
  menu", which rt does not have.
* **The cue does not wait for the idle poll.** The X11 receiver's connection is
  outside winit's poll set, so its first `XdndEnter` can be up to `IDLE_POLL`
  (100 ms) late. The Wayland worker is a thread, so it wakes the loop through
  winit's `EventLoopProxy` and a turn happens per pointer motion.

## Known limitation: two windows on GNOME

rt builds one `Clipboard` per **window**, so two rt windows means two
`wl_data_device`s — which on Mutter means the second window's
`get_data_device` unlinks the first's, and the first window loses its clipboard.

That is **pre-existing** and unchanged by this work: it is a property of one
clipboard per window, which is how rt has always built them, and the drag
receiver rides whichever device its own window already had. Fixing it means a
process-wide selection worker with a surface→window registry; it belongs in its
own change, where it can be reasoned about without a feature riding on it.
