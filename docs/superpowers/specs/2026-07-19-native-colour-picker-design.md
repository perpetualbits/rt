# Native colour picker (wishlist #2) — design

Bring back a colour picker in Preferences, removed when the prefs dialog went
native. Native command-drawn on **both** backends (not greyed out on X11);
repaints only the picker while dragging, so it stays cheap over `ssh -X`.

Decisions taken with the user: **HSV square + hue strip** style; **all 18
swatches editable** (fg, bg, and the 16-colour palette). No separate cursor
colour exists (the cursor uses the foreground).

## Where it plugs in

The prefs "Colours" section already renders a **Swatches** row previewing
`[fg, bg, palette…]` as small squares (`chrome::prefs`), read-only today.
Clicking a swatch opens the picker for that slot. The picker is a modal overlay
*on top of* the still-open prefs dialog.

## Applying edits — reuse the settle model

Prefs edits already flow through `prefs_pending` + a 150 ms settle
(`PREFS_SETTLE`): `commit_settings` rebuilds `rt_engine::Palette`,
`set_all_palettes`, and persists — once, after the edits stop. The picker uses
exactly this: a drag writes the chosen RGB into `prefs_pending`'s slot and arms
the settle. Consequences:

- The terminal recolours **once**, on settle (~150 ms after you pause) or on
  close — same feel as stepping the Preset today. The heavy path (recolouring
  every cell + reflow-free repaint) runs once, not per pointer-move.
- While dragging, the prefs **swatch row** and the picker **preview** update live
  (both read pending), so you see the colour before it lands on the terminal.
- Esc / Close / click-outside commits any pending edit immediately (mirroring the
  prefs Esc path), so a fast close never strands it.

**Frame cost.** Overlays force a full frame while open (`force_full =
overlay_open`), so a drag repaints the whole overlay frame per pointer-move, as
holding a prefs stepper already does. Those frames carry **no reflow and no new
terminal content** (colours land on settle) — just the moved picker over the same
glyphs. Over `ssh -X` that is still a full-window re-ship per move; acceptable for
a deliberate, brief drag, and `SV_GRID` / a future scissored-picker repaint are
the knobs if it needs to be cheaper. This is the one thing to watch over `ssh -X`.

## Modules

### `chrome/colour_picker.rs` (pure; unit-tested)

- **Colour math:** `hsv_to_rgb(h,s,v) -> [u8;3]` and `rgb_to_hsv([u8;3]) ->
  (h,s,v)` with `h ∈ [0,360)`, `s,v ∈ [0,1]`. Round-trips within ±1/255.
- **`Slot`**: `Fg | Bg | Palette(usize)` — which colour is being edited. Maps to
  swatch index `0=Fg, 1=Bg, 2+i=Palette(i)`.
- **`PickerState`**: `{ slot, h, s, v, drag: Option<Drag> }` where `Drag = Sv |
  Hue`. HSV is stored (not RGB) so dragging value/saturation to an edge never
  loses the hue.
- **`Geom`**: `panel`, `sv` (saturation/value square), `hue` (vertical strip),
  `preview` swatch, `hex` text origin, `close` button — all `Recti`.
- `layout(win_w, win_h, cell_w, cell_h) -> Geom` — centred, clamped on-screen.
- `hit(&Geom, p) -> Hit` where `Hit = Sv | Hue | Close | None`.
- `draw(be, &Geom, h, s, v, title, cell_w, cell_h)` — SV square as a coarse
  N×N grid of `fill_rect` (N≈16 ⇒ ~256 cmds) at hue `h`; hue strip as ~32
  vertical segments; position markers (an SV ring, a hue caret); the preview
  swatch and `#rrggbb`. Every primitive is `fill_rect` + `draw_char` — already on
  both backends.

### `chrome/prefs.rs` (small addition)

- `swatch_rects(row: Recti, count: usize, cell_h: f32) -> Vec<Recti>` — the
  per-swatch geometry, factored out so `draw()` and the click handler agree (no
  drift). `draw()` calls it; the mouse handler calls it to hit-test.

### `main.rs` (wiring)

- `Active.picker: Option<colour_picker::PickerState>`.
- Prefs click on the Swatches row → `swatch_rects` hit-test → open the picker for
  that slot, seeding `h,s,v` from the slot's current colour via `rgb_to_hsv`.
- A **picker input block** ahead of the prefs block (picker is modal on top):
  press on `Sv`/`Hue` starts a drag and applies; motion while dragging updates
  `h/s/v`, writes `hsv_to_rgb` into the pending slot, arms the settle, repaints
  the picker; release ends the drag; Esc / Close / outside-press commits pending
  and closes the picker (prefs stays open).
- `set_slot(&mut Settings, slot, rgb)` writes fg / bg / `palette[i]`.
- Render: in the `prefs_open` branch, `paint_prefs` then `paint_picker` when the
  picker is up.

## Guardrails

`PutImage == 0` holds — every picker primitive is `fill_rect`/`draw_char`,
including the position markers (thin rect outlines, since the GL backend has no
circle). Nothing animates on its own. The one cost to watch is the full-frame
repaint per drag-move (see **Frame cost** above); `SV_GRID` in
`chrome::colour_picker` is the documented knob.

## Out of scope (follow-ups)

Hex text entry; eyedropper; per-swatch reset-to-scheme; editing the picker via
keyboard (it is pointer-driven). The Preset stepper still cycles whole schemes.
