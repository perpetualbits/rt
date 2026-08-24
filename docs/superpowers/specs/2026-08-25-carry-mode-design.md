# Carry mode + held-pane cursor — design

**Date:** 2026-08-25
**Status:** approved design (in-chat, 2026-08-25), pre-plan
**Builds on:** v0.3.17 drag-and-drop + multi-window; the Wayland tear-out branch
(`feat/wayland-drag-tearout`, user-verified on cosmic-comp).

## Goal

1. **Cross-window pane/tab drops on Wayland** (and everywhere) via a two-phase
   "carry" gesture: pick up → aim with full drop cues → click to drop.
2. **A visible "held" payload**: while dragging a pane/tab out of a window — and
   while carrying — the cursor itself becomes a card showing the pane's outline
   and translucent inside, so the user is visibly holding something.

## Why this shape (decided with Roland)

- All rt windows are ONE process; the only obstacle to cross-window drops on
  Wayland is pointer geometry during the implicit grab (no global coords, no
  enter events for other surfaces while the button is held).
- The moment the button is released the grab ends: every rt window then
  receives its own normal `CursorEntered`/`CursorMoved` with surface-local
  coordinates. A modal carry state therefore gets the FULL cue experience in
  the hovered window — richer than mid-grab hovering could ever be on Wayland.
- Compositor-mediated DnD (`wl_data_device`) remains the eventual "one fluid
  gesture" upgrade; `xdg_toplevel_drag` is absent from cosmic-comp (probed
  2026-08-25). Neither blocks this slice.
- During a button-held drag the compositor shows the grabbing client's cursor
  everywhere, and winit 0.30 has `CustomCursor` (RGBA) — so the held-pane card
  rides the pointer across the desktop and foreign windows for the whole drag.

## The carry state machine

New App-level modal state, coexisting with (and reusing) the drag machinery:

```
CarryState {
    payload: DragPayload,             // Pane(id) | Tab { first_pane }
    payload_panes: Vec<PaneId>,       // for self-drop exclusion + death watch
    source: WindowId,                 // where it still lives (dimmed)
    label: String,                    // title or "Pane"/"Tab N"
    cursor: winit CustomCursor,       // the held-pane card (built at pickup)
}
```

**Enter carry (two gestures, both landing in the same state):**
- **A1 — modifier release:** a live drag released OUTSIDE the source window
  with **Ctrl held** enters carry instead of tearing out. Plain release keeps
  today's instant tear-out (user-verified muscle memory).
- **A2 — explicit pickup:** `Ctrl+Shift+M` picks up the focused pane;
  `Ctrl+Shift+N` picks up its tab. Context-menu rows "Pick Up Pane" /
  "Pick Up Tab" run the same actions. This also gives pane-moving to
  titlebar-less setups and keyboard users. Both get MANUAL lines (the
  binding-coverage test enforces it).

**While carrying:**
- The payload pane(s) stay live and dimmed in the source window (`drag_dim`
  reuse). Every rt window resolves its OWN hover: on `CursorMoved` with a
  carry active, that window runs `resolve_drop` against its own layout and
  shows the standard cues (edge splits, centre swap, tab caret, edge bands)
  plus the ghost chip — all existing painters, unchanged. `force_full` rules
  as for drags.
- The held-pane card is set as the cursor on every rt window while carry is
  active (over foreign surfaces / desktop the cursor reverts to theirs —
  accepted; documented).
- Carry mode survives window focus changes; it is App-global.

**Commit / cancel:**
- **Left press** in any rt window with a resolved target commits: same-window
  targets via `move_pane`/`move_tab`/`reorder_tab`; cross-window via the
  existing `cross_window_drop` path (extract/adopt, jacks/wires migration,
  source-emptied close). The press is consumed (no focus-click side effects
  beyond what the commit implies). A left press over a dead zone (gutter) in
  an rt window: no-op, carry continues (unlike drags, there is no release to
  interpret — the user can try again).
- **Escape** (any rt window) cancels; so does the payload pane's child dying,
  or the source window closing. Right/middle press: cancel (mirrors drags).
- Entering carry cancels any live drag and vice versa; the modal-overlay gate
  extends to carry (opening prefs/manual/search/clip-history cancels it).
  Overlay-open windows don't resolve carry hovers.

## The held-pane cursor card

- Built at pickup (and at drag start — the SAME card serves both the drag
  phase and carry): an RGBA image, pane-aspect-ratio, max dimension ~128 px
  (clamped 32..192): 2 px accent outline (0x4a,0x7a,0xc8), translucent body
  (the pane's configured background at ~0.55 alpha), an opaque titlebar band
  (~14 % height) in the tab-active shade. No text — outline + translucent
  inside per the ask; pure RGBA math, no font machinery.
- Hotspot at the card's top-left corner offset (8,8) so the card trails the
  point naturally.
- Set via `Window::set_cursor(Cursor::Custom(..))` on: the source window at
  drag start (the grab broadcasts it everywhere for the drag's duration), and
  every rt window while carry is active. Restored (`update_cursor`) on
  drop/cancel/tear-out.
- Fallback: if `CustomCursor` creation fails (compositor/platform limits),
  fall back to `CursorIcon::Grabbing` exactly as today. Cursor build failures
  must never affect the gesture logic.
- Applies to X11 identically (ARGB cursors); the existing X11 drag gets the
  card too, replacing plain Grabbing during drags.

## Non-goals (this slice)

- No `wl_data_device` DnD session (later upgrade for one-fluid-gesture drags).
- No desktop-drop from carry (a desktop click is invisible to a Wayland
  client; tear-out remains the drag gesture / detach keys).
- No cross-process anything.

## Edge cases

- Pickup of a lone pane in a lone-tab window: allowed (dropping it elsewhere
  closes the emptied source, as cross-window drops already do); dropping it
  back where it came from: `move_pane` self-target guards make it a no-op.
- A1 with the payload covering ALL source panes (lone pane / only tab):
  carry is allowed even though tear-out would no-op — dropping into another
  window is exactly the use case.
- Carry + the source window's session mutating underneath (splits, closes of
  OTHER panes): fine — payload identity is by PaneId; only payload death or
  source close cancels.
- Two rapid pickups: the second replaces the first (with a cancel of the
  first, cues cleared).

## Testing

- Pure: card RGBA builder (dimensions, clamps, outline/band placement —
  assert pixel values at known offsets); carry-state transition table as a
  pure function where practicable.
- Session-level: commit paths are the existing, already-tested move/drop APIs.
- Manual (real display, Roland): pick up (both gestures) → hover cues in a
  second window → click-drop each cue family; Escape cancel; payload-death
  cancel; the card cursor visible during drag-out on cosmic-comp and over
  ssh -X.

## Delivery

One plan: ① card builder + cursor plumbing (drag phase first — visible win),
② carry state machine + hover resolution + commit/cancel, ③ A1/A2 entries,
menu rows, manual/docs/KNOWN_ISSUES/project-map. Ships as the release after
the Wayland tear-out fix.
