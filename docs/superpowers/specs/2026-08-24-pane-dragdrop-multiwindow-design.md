# Pane drag-and-drop + multi-window — design

**Date:** 2026-08-24
**Status:** approved design, pre-plan
**Roadmap:** Phase 3 #17 (tabs: reorder/detach/move) + the new-window half of #21; groundwork for #18 (saved layouts).

## Goal

Make rt's panes and tabs physically rearrangeable with the mouse:

1. **Tear out** — drag a pane (by its titlebar) or a tab (by its label) out of the
   window and drop it on the desktop: it becomes a new OS window that does **not**
   close when the mother window closes.
2. **Cross-window move** — drop a pane/tab into another rt window instead.
3. **In-window reorganize** — reorder tabs, and re-place panes within the same
   window (split beside another pane, swap with it, wrap as a new tab).
4. **Visual cues at the receiving end** — an insert caret between tab labels
   ("put here?") and translucent tiling candidates (half-pane N/S/E/W, whole-pane
   swap, window-edge root splits) ("put new tile here?").

## Decisions (made with Roland, 2026-08-23)

- **Window scope: same process only.** One rt process owns many OS windows; panes
  move between them in memory, so PTY, scrollback, selection, title and group all
  survive. Windows of a separately launched `rt` are not drop targets. The
  extract/adopt seam is kept serializable-shaped so a later IPC slice (roadmap
  #21) can cross processes without redoing the UI.
- **Drag handles: both.** Pane titlebar drags the single pane; tab label drags the
  whole tab (its entire split subtree). Plain clicks keep today's behaviour
  (focus / switch tab); a drag only starts past a movement threshold.
- **Drop targets: all four cue families** — pane halves (N/S/E/W split), pane
  centre (swap), tab-strip insert caret, window edges (root-level split).
- **Wayland: full in-window feature; tear-out on drop-outside; no cross-window
  hover.** Native Wayland exposes no global coordinates and no other-surface
  hover during an implicit grab, and winit exposes neither data-device DnD nor
  `xdg_toplevel_drag`. Cross-window moves on Wayland go through a
  "Move to window ▸" menu and keyboard actions instead. X11 (including ssh -X)
  gets the full hover experience via global pointer + own-window geometry.

## Rejected approaches

- **Process-per-window with re-exec/handoff** — a live PTY + in-memory scrollback
  cannot follow the pane across a process boundary without SCM_RIGHTS fd passing
  plus grid serialization; that is the deferred IPC slice, not this feature.
- **Protocol-level DnD (wayland data-device / xdg_toplevel_drag, X11 XDND)** —
  winit doesn't expose initiating either; bypassing winit with raw protocol
  clients is fragile. Same-process transfer doesn't need protocol data transfer
  at all — only hover hit-testing, which X11 gives us directly.

## Architecture

### 1. Multi-window App (`crates/rt/src/main.rs`)

- `App { active: Option<Active> }` → `App { windows: HashMap<WindowId, Active>,
  drag: Option<DragState>, … }`.
- The window/GL-or-XRender/palette/spawn construction currently inlined in
  `resumed()` (~`main.rs:699-957`) is factored into an `Active::new(event_loop, …)`
  factory. `resumed()` creates the first window with it; `Action::NewWindow` and
  tear-out create more. Each `Active` keeps its own render context, damage state,
  chrome state, patch bay, and `Session`.
- `window_event()` routes by the `WindowId` it currently discards; `about_to_wait`
  iterates all windows' sessions for pane events and redraw scheduling.
- **Close semantics:** `CloseRequested` and `Action::Quit` close *that* window:
  its panes' children are killed (per-pane, as `exit_clean` does today), its
  `Active` is dropped, and the process exits only when `windows` is empty. This
  is the property that makes a torn-out pane survive the mother window.
- Shared, not per-window: the config/keymap, the clipboard history, the global
  `DragState`, the PaneId allocator (below).

### 2. Pure tree ops (`crates/rt-core/src/layout.rs`)

New operations, all pure and unit-testable:

- `take(PaneId) -> Option<Node>` — remove the leaf with the same container
  collapse as `close`, but *return* the node instead of dropping it.
- `take_tab(index) -> Option<Node>` — remove a whole tab's subtree.
- `insert_beside(target: PaneId, node: Node, orient, before: bool)` — split
  `target` and place `node` on the chosen side (the N/S/E/W drop).
- `insert_root_edge(node, orient, before)` — full-width/height split at the root
  (the window-edge drop).
- `insert_tab_at(node, index)` — wrap/insert as a tab at a position (the caret
  drop; also used by adopt-into-empty-window).
- `reorder_tab(from, to)` and `swap(a: PaneId, b: PaneId)`.

**PaneId uniqueness:** `Tree::next_id` is per-tree today, so ids collide across
windows. Allocation moves to a process-wide `AtomicU64`, making PaneIds unique
process-wide and letting subtrees move between trees with zero remapping of the
session side tables.

### 3. Session extract/adopt (`crates/rt-session`)

- `PanePackage` — the moved subtree plus every side-table entry for the panes in
  it: backend (PTY + grid), title, group, columns, zoom (cleared on extract),
  titlebar flag. A plain struct; the shape a future IPC slice would serialize.
- `Session::extract(node: Node) -> PanePackage` — remove the subtree's entries
  from this session; fix focus to a surviving pane.
- `Session::adopt(pkg: PanePackage, at: DropTarget)` — merge the package's
  entries and insert its node via the rt-core op that `DropTarget` names; focus
  the arriving pane; `relayout` (so every moved PTY gets resized).
- `DropTarget` (also the hover-cue model):
  `SplitBeside { pane, orient, before } | Swap { pane } | TabAt { index } |
  RootEdge { orient, before } | NewWindow`.
- This is deliberately the anti-Terminator design (`docs/TERMINATOR_BUGS.md`):
  id-keyed data surgery between ticks, never reparenting live state inside an
  event handler.

### 4. Drag state machine (owned by `App` — it spans windows)

- **Arm:** left-press on a pane titlebar strip or a tab label records
  `ArmedDrag { source_window, payload, press_pos }`. Payload is
  `Pane(PaneId)` or `Tab(index)`.
- **Start:** motion beyond ~4 px promotes it to `DragState`; below the threshold,
  release performs today's click action (focus / switch tab) unchanged.
- **Track:** the source window receives motion via the implicit grab even outside
  its bounds (both X11 and Wayland). On X11 we additionally resolve the *global*
  pointer position against our own windows' outer geometries each motion, so any
  rt window can become the hover window. The hover window computes
  `Option<DropTarget>` from its layout rects + tab bars (a pure resolver:
  `cursor + geometry -> DropTarget`; centre box ≈ inner 40% = swap, else nearest
  edge = split, tab strip = caret index, window-edge strips = root split).
  Dragging a tab over its own strip resolves only to `TabAt` (reorder); a tab
  cannot be dropped into its own subtree.
- **Commit (release):**
  - same window → apply the tree op in place;
  - another rt window (X11) → `extract` from source, `adopt` into target;
  - no rt window (X11: global point in none of ours; Wayland: surface-local
    point outside source bounds) → **tear-out**: `Active::new`, adopt as the
    root tab; on X11 position the new window at the drop point, on Wayland the
    compositor places it;
  - **Escape cancels** (state cleared, nothing moved).
- The dragged pane keeps running and rendering in place, dimmed, until commit.
- `force_full = true` on every drag motion for affected windows (cues and ghost
  bypass the damage tracker — same rule the patch-bay wire rubber-band follows,
  `main.rs:1812`).

### 5. Drop cues (overlay pass; GL and XRender identically)

Drawn in `paint_overlays_or_instruments` on whichever window is hovered:

- **Pane halves:** translucent highlight over the receiving half (N/S/E/W).
- **Pane centre:** whole-pane highlight = swap.
- **Tab caret:** a caret/pointer between tab labels at the insertion index.
- **Window edges:** thin strips; hovering one highlights the full-width/height
  band the root split would create.
- **Ghost chip:** a small translucent rect with the pane/tab title follows the
  cursor in the hovered rt window; between windows only `CursorIcon::Grabbing`
  shows (we can't draw on the desktop).
- Colours derive from the palette's focus/accent colour at low alpha, so cues
  read on both light and dark schemes.

### 6. Keyboard, menu, Wayland parity

New `rt_config::Action` variants, each with a default chord, context-menu entry
(`menu.rs`), and a `MANUAL` line (the `every_default_keybinding_is_documented`
test enforces the manual):

- `NewWindow` — open an empty new window.
- `DetachPane` / `DetachTab` — tear the focused pane / current tab out to a new
  window (keyboard tear-out; also the guaranteed Wayland path).
- `MoveTabLeft` / `MoveTabRight` — keyboard tab reorder (roadmap #17).
- **"Move to window ▸"** context-menu submenu listing the other windows by
  title — moves the focused pane there (splitting beside that window's focused
  pane). This is the Wayland substitute for cross-window drag.

### 7. Edge cases

- Dragging the **last pane of a window** into another window closes the emptied
  source window afterwards. Tearing out the last pane of a single-pane window is
  a no-op (it would recreate the same window).
- A **zoomed** pane unzooms when its drag starts; zoom state never travels.
- **Group membership** travels with the pane (groups/broadcast remain
  per-window predicates over the panes present).
- **Patch bay:** a moved pane's jacks re-register in the target window's jack
  map; wires that would cross windows are cut on commit.
- Tab drag payloads carry the whole subtree; every edge above applies to each
  pane inside it.
- A pane whose child process exits mid-drag: the drag is cancelled.

### 8. Risks

- **Wayland out-of-bounds motion during implicit grab** (needed for
  drop-outside-source tear-out) varies by compositor. Spike this first on KDE;
  if unreliable, `DetachPane`/`DetachTab` (keyboard + menu) is the documented
  Wayland tear-out and drag-tear-out stays X11.
- **Multi-window GL context/glutin interactions** (context per window,
  swap/frame scheduling) — slice ① lands this alone, before any drag code, so
  it soaks first.
- **ssh -X**: cue drawing is plain `fill_rect`/`draw_char` chrome and follows
  the existing batching rules; verify no per-motion request storm (see the
  instrument-lag lesson: batch, don't flush per event).

### 9. Testing

- **rt-core:** unit tests for every new op — collapse invariants after `take`,
  weight preservation, id uniqueness across trees, reorder/swap round-trips,
  fuzz: random sequences of take/insert leave a well-formed tree containing
  exactly the expected panes.
- **rt-session:** extract/adopt against the existing mock `Backend`: side tables
  move completely, focus lands correctly on both sides, relayout resizes every
  moved pane, `DropTarget` variants each produce the intended tree.
- **rt (binary):** the drop-target resolver is a pure function tested with
  synthetic layouts (zone boundaries, caret indices, edge strips, own-subtree
  exclusion). Drag threshold/cancel logic tested as a pure state machine where
  practical.
- **Manual:** X11 local, ssh -X, native Wayland (KDE): tear out, cross-window
  drop each cue family, reorder tabs, close mother window and confirm the
  orphan survives; deploy to all three machines.

## Delivery slices (one plan, phased)

1. **Multi-window foundation** — `Active::new` factory, `WindowId` routing,
   per-window close, global PaneId allocator, `NewWindow`/`DetachPane`/
   `DetachTab` actions (+ extract/adopt + `take`/`insert_tab_at` to power them).
   Tear-out already usable via keyboard at the end of this slice.
2. **In-window drag** — armed-drag gesture, drop-target resolver, all four cue
   families, `insert_beside`/`swap`/`reorder_tab`/`insert_root_edge`, tab
   reorder + `MoveTabLeft/Right`.
3. **Cross-window drag (X11) + drag tear-out** — global-pointer hover
   resolution, extract/adopt on drop, tear-out on desktop drop, Wayland
   drop-outside-source tear-out (post-spike).
4. **Wayland parity + polish** — "Move to window ▸" submenu, ghost chip polish,
   docs (`MANUAL`, README), `project-map.js` (add a multi-window node; #17
   status), TERMINATOR_FEATURES checkboxes.
