# Carry Mode + Held-Pane Cursor Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Cross-window pane/tab drops on every platform (Wayland included) via a modal pick-up → aim-with-cues → click-to-drop gesture, with a custom cursor showing the held pane's outline and translucent body during both drags and carries.

**Architecture:** A new App-level `CarryState` modal coexists with the drag machinery and reuses its resolver, cue painters, per-window cue fields, and commit paths (`same_window_drop`/`cross_window_drop`). Post-release, every rt window receives its own motion events (no grab), so each window resolves its own hover. The held-pane card is a pure-RGBA image set as a winit `CustomCursor` — compositor-drawn, so it follows the pointer everywhere during the button-held drag phase and over rt windows during carry.

**Tech Stack:** Existing workspace; winit 0.30.13 `CustomCursor` (`from_rgba` — alpha NOT premultiplied — via `ActiveEventLoop::create_custom_cursor`); no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-25-carry-mode-design.md`

## Global Constraints

- **No new crate dependencies.** `CustomCursor` ships in the winit already in-tree.
- **No-crash policy:** cursor-build failure (`BadImage`) falls back to `CursorIcon::Grabbing` and must never affect gesture logic; all window lookups let-else.
- **Never lose a live pane:** carry commits go ONLY through the existing `same_window_drop` / `cross_window_drop` paths (which carry the restoration logic).
- **force_full on every cue change** (cues bypass damage tracking), never relayout per motion.
- **Alpha is non-premultiplied** in the card RGBA (winit's `from_rgba` contract).
- **Plain release outside the source stays instant tear-out** (user-verified); carry entry is Ctrl-held release (A1) or explicit action (A2).
- **Every new default keybinding gets a MANUAL line** (`every_default_keybinding_is_documented` enforces it).
- Branch: continue on `feat/wayland-drag-tearout` (this ships together with the Wayland tear-out as v0.3.18). `cargo build --workspace && cargo test --workspace` green, zero new warnings, at every commit.
- Workers must never launch GUI programs on the user's display; headless runs use `env -u WAYLAND_DISPLAY DISPLAY=:99` on a self-started Xvfb, and only self-captured PIDs are ever killed.

---

## File Structure

- `crates/rt/src/carry_card.rs` — **new, pure**: RGBA card builder + tests. No winit/Backend types.
- `crates/rt/src/main.rs` — `CarryState`, enter/cancel/commit, A1 hook in the release arm, carry hover resolution, cursor plumbing, gate extensions.
- `crates/rt-config/src/lib.rs` — `Action::{PickUpPane, PickUpTab}` + chords.
- `crates/rt/src/menu.rs` — two menu rows.
- `crates/rt/src/manual.rs`, `docs/KNOWN_ISSUES.md`, `README.md`, `project-map.js` — docs task.

Current-code anchors (verified 2026-08-25): `ArmedDrag` main.rs:589, `DragState` :598, `opens_modal_overlay` :2931, `removes_pane_mid_drag` :2944, `cancel_drag` :2956, `same_window_drop(&mut self, id: WindowId, payload: dragdrop::DragPayload, r: dragdrop::ResolvedDrop) -> bool` :3177, `cross_window_drop(&mut self, source, payload, w, r) -> bool` :3204, `menu_move_targets` :3326, `clear_drag_cues` :3470, `update_cursor` :5629, `cursor_icon: Option<CursorIcon>` field :398. winit: `CustomCursor::from_rgba(rgba, width: u16, height: u16, hotspot_x: u16, hotspot_y: u16) -> Result<CustomCursorSource, BadImage>`; `ActiveEventLoop::create_custom_cursor(CustomCursorSource) -> CustomCursor` (CustomCursor is cheap-Clone); `Window::set_cursor(impl Into<Cursor>)`, `Cursor::Custom(c)`.

---

### Task 1: The held-pane card builder (pure RGBA)

**Files:**
- Create: `crates/rt/src/carry_card.rs`
- Modify: `crates/rt/src/main.rs` (one line: `mod carry_card;` near the other pure mods, e.g. after `mod dragdrop;`)

**Interfaces:**
- Produces:

```rust
/// Target size of the card's longer dimension, px.
pub const CARD_MAX_DIM: u16 = 128;
/// Aspect (w/h) clamp so degenerate panes still make a readable card.
pub const CARD_MIN_ASPECT: f32 = 0.4;
pub const CARD_MAX_ASPECT: f32 = 2.5;
/// Cursor hotspot, from the card's top-left.
pub const CARD_HOTSPOT: (u16, u16) = (8, 8);

/// Build the held-pane card as a non-premultiplied RGBA buffer.
/// `aspect` = pane width / height; `bg` = the pane's configured background.
/// Layout: 2px opaque accent outline; opaque-ish titlebar band across the top
/// (14% of height, min 8px) in the tab-active shade; translucent body in `bg`.
pub fn build_card_rgba(aspect: f32, bg: [u8; 3]) -> (Vec<u8>, u16, u16); // (rgba, w, h)
```

Exact colours/alphas (RGBA, alpha NOT premultiplied):
- outline: `(0x4a, 0x7a, 0xc8, 0xff)`, 2px on all four edges.
- titlebar band (inside the outline, full inner width, rows 2..2+band_h where `band_h = max(8, h*14/100)`): `(0x2e, 0x2e, 0x38, 230)`.
- body (everything else inside the outline): `(bg[0], bg[1], bg[2], 140)`.
- Dimensions: if `aspect >= 1.0` → `w = CARD_MAX_DIM`, `h = (CARD_MAX_DIM as f32 / aspect)`; else `h = CARD_MAX_DIM`, `w = CARD_MAX_DIM as f32 * aspect`; aspect clamped to `[CARD_MIN_ASPECT, CARD_MAX_ASPECT]` first; each dimension floored to u16 and clamped to at least 24.

- [ ] **Step 1: Write the failing tests** (in-file `#[cfg(test)] mod tests`)

```rust
use super::*;

fn px(rgba: &[u8], w: u16, x: u16, y: u16) -> [u8; 4] {
    let i = (y as usize * w as usize + x as usize) * 4;
    [rgba[i], rgba[i + 1], rgba[i + 2], rgba[i + 3]]
}

/// A wide pane (2:1) gets a 128x64 card; outline, band and body pixels land
/// where the layout says, with the exact colours and alphas.
#[test]
fn card_layout_wide_pane() {
    let (rgba, w, h) = build_card_rgba(2.0, [0x10, 0x10, 0x14]);
    assert_eq!((w, h), (128, 64));
    assert_eq!(rgba.len(), w as usize * h as usize * 4);
    assert_eq!(px(&rgba, w, 0, 0), [0x4a, 0x7a, 0xc8, 0xff], "corner = outline");
    assert_eq!(px(&rgba, w, 64, 1), [0x4a, 0x7a, 0xc8, 0xff], "2px top edge");
    // band_h = max(8, 64*14/100 = 8) = 8 → rows 2..10 are band.
    assert_eq!(px(&rgba, w, 64, 5), [0x2e, 0x2e, 0x38, 230], "titlebar band");
    assert_eq!(px(&rgba, w, 64, 32), [0x10, 0x10, 0x14, 140], "translucent body");
    assert_eq!(px(&rgba, w, 127, 63), [0x4a, 0x7a, 0xc8, 0xff], "far corner = outline");
}

/// A tall pane makes a tall card; extreme aspects clamp.
#[test]
fn card_layout_tall_and_clamped() {
    let (_, w, h) = build_card_rgba(0.5, [0, 0, 0]);
    assert_eq!((w, h), (64, 128));
    let (_, w2, h2) = build_card_rgba(100.0, [0, 0, 0]);
    assert_eq!((w2, h2), (128, (128.0 / CARD_MAX_ASPECT) as u16), "aspect clamps high");
    let (_, w3, h3) = build_card_rgba(0.0, [0, 0, 0]);
    assert_eq!((w3, h3), (((128.0 * CARD_MIN_ASPECT) as u16), 128), "aspect clamps low");
    assert!(w3 >= 24 && h3 >= 24, "minimum dimensions");
}

/// The hotspot must lie inside every possible card.
#[test]
fn hotspot_always_inside() {
    for aspect in [0.0f32, 0.4, 1.0, 2.5, 100.0] {
        let (_, w, h) = build_card_rgba(aspect, [9, 9, 9]);
        assert!(CARD_HOTSPOT.0 < w && CARD_HOTSPOT.1 < h, "aspect {aspect}");
    }
}
```

- [ ] **Step 2: Run to verify compile failure** — `cargo test -p rt carry_card` (module missing).

- [ ] **Step 3: Implement**

```rust
//! The "held pane" card: a pure-RGBA image of the dragged/carried pane —
//! accent outline, opaque titlebar band, translucent body — used as a custom
//! cursor so the user visibly holds what they picked up. Pure pixel math:
//! no winit, no Backend, unit-tested by pixel.

pub const CARD_MAX_DIM: u16 = 128;
pub const CARD_MIN_ASPECT: f32 = 0.4;
pub const CARD_MAX_ASPECT: f32 = 2.5;
pub const CARD_HOTSPOT: (u16, u16) = (8, 8);

const OUTLINE: [u8; 4] = [0x4a, 0x7a, 0xc8, 0xff];
const BAND: [u8; 4] = [0x2e, 0x2e, 0x38, 230];
const BODY_ALPHA: u8 = 140;

pub fn build_card_rgba(aspect: f32, bg: [u8; 3]) -> (Vec<u8>, u16, u16) {
    let aspect = if aspect.is_finite() && aspect > 0.0 {
        aspect.clamp(CARD_MIN_ASPECT, CARD_MAX_ASPECT)
    } else {
        CARD_MIN_ASPECT // 0/NaN input: pick the clamp floor, never divide by it
    };
    let (w, h) = if aspect >= 1.0 {
        (CARD_MAX_DIM, ((CARD_MAX_DIM as f32 / aspect) as u16).max(24))
    } else {
        (((CARD_MAX_DIM as f32 * aspect) as u16).max(24), CARD_MAX_DIM)
    };
    let band_h = ((h as usize * 14) / 100).max(8) as u16;
    let mut rgba = Vec::with_capacity(w as usize * h as usize * 4);
    for y in 0..h {
        for x in 0..w {
            let outline = x < 2 || y < 2 || x >= w - 2 || y >= h - 2;
            let p: [u8; 4] = if outline {
                OUTLINE
            } else if y < 2 + band_h {
                BAND
            } else {
                [bg[0], bg[1], bg[2], BODY_ALPHA]
            };
            rgba.extend_from_slice(&p);
        }
    }
    (rgba, w, h)
}
```

(If a boundary test fails, fix the IMPLEMENTATION to the tests — the tests are the layout contract. `2 + band_h` vs `band_h` row accounting is the likely off-by-one; the tests pin it: band rows start at y=2.)

- [ ] **Step 4: Run tests** — `cargo test -p rt carry_card` → PASS; `cargo build --workspace` clean.

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/carry_card.rs crates/rt/src/main.rs
git commit -m "feat(rt): pure RGBA held-pane card builder"
```

---

### Task 2: Card cursor during drags

**Files:**
- Modify: `crates/rt/src/main.rs` (drag promote in `CursorMoved`; `DragState`; drag-end sites)

**Interfaces:**
- Consumes: Task 1's `carry_card::{build_card_rgba, CARD_HOTSPOT}`.
- Produces: `fn make_payload_card(event_loop: &ActiveEventLoop, active: &Active, payload: dragdrop::DragPayload) -> Option<winit::window::CustomCursor>` — builds the card for a payload (aspect from the payload pane's visible rect for `Pane`; from the window's `content_bounds` for `Tab`; `bg` from `active.settings.background`), `create_custom_cursor`s it, `None` on `BadImage` (log::warn once). `DragState` gains `card: Option<winit::window::CustomCursor>`.

- [ ] **Step 1: Implement `make_payload_card`**

```rust
/// The held-pane cursor card for a drag/carry payload, or None (fall back to
/// CursorIcon::Grabbing) if the platform refuses the image. Cursor problems
/// must never affect gesture logic.
fn make_payload_card(
    event_loop: &ActiveEventLoop,
    active: &Active,
    payload: dragdrop::DragPayload,
) -> Option<winit::window::CustomCursor> {
    let bounds = content_bounds(active.window.inner_size());
    let rect = match payload {
        dragdrop::DragPayload::Pane(p) => active
            .session
            .visible_rects(bounds)
            .into_iter()
            .find(|(id, _)| *id == p)
            .map(|(_, r)| r),
        dragdrop::DragPayload::Tab { .. } => None, // a tab page fills the content area
    };
    let (w, h) = rect.map(|r| (r.w, r.h)).unwrap_or((bounds.w, bounds.h));
    let aspect = if h > 0.0 { w / h } else { 1.0 };
    let (rgba, cw, ch) = carry_card::build_card_rgba(aspect, active.settings.background);
    match winit::window::CustomCursor::from_rgba(rgba, cw, ch, carry_card::CARD_HOTSPOT.0, carry_card::CARD_HOTSPOT.1) {
        Ok(src) => Some(event_loop.create_custom_cursor(src)),
        Err(e) => {
            log::warn!("held-pane cursor unavailable ({e}); falling back to Grabbing");
            None
        }
    }
}
```

- [ ] **Step 2: Use it at drag promote**

At the drag-promotion site in `CursorMoved` (where `self.drag = Some(DragState { .. })` is built and the cursor is set to `CursorIcon::Grabbing`): build `let card = Self::make_payload_card(event_loop, active, payload);` (the `CursorMoved` arm receives `event_loop` — it is a parameter of `window_event`; thread it into the promote block if a helper hides it). Store `card: card.clone()` in the new `DragState` field. Then:

```rust
match &card {
    Some(c) => active.window.set_cursor(winit::window::Cursor::Custom(c.clone())),
    None => active.window.set_cursor(CursorIcon::Grabbing),
}
active.cursor_icon = Some(CursorIcon::Grabbing); // proxy: update_cursor's change
// detection only needs to see "not default" so it restores properly later.
```

- [ ] **Step 3: Verify drag-end restoration**

Every drag end (commit, Escape/cancel_drag, abandon) already routes through `Self::update_cursor(active)` via `clear_drag_cues` — confirm by reading that each path resets the cursor (update_cursor compares against `cursor_icon` = `Some(Grabbing)` and issues `set_cursor(Default)` when nothing wants a shape). If any end-path skips update_cursor, add it there.

- [ ] **Step 4: Build + tests + smoke**

`cargo build --workspace && cargo test --workspace` green. Headless smoke (self-started Xvfb, `env -u WAYLAND_DISPLAY DISPLAY=:99`, exact-PID kills only): drag a titlebar — no panic, drag commits as before; cursor pixels aren't capturable under xwd, so the visual is verified on the real display at the end (Task 5 hand-off note). Assert no regression: the full drag matrix from RT_SPLIT=v still behaves.

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/main.rs
git commit -m "feat(rt): held-pane card cursor during drags"
```

---

### Task 3: CarryState — enter, cancel, A1/A2 entries, gates

**Files:**
- Modify: `crates/rt-config/src/lib.rs` (Action enum + defaults)
- Modify: `crates/rt/src/menu.rs` (two rows)
- Modify: `crates/rt/src/manual.rs` (chord lines; full prose lands in Task 5)
- Modify: `crates/rt-session/src/lib.rs` (passthrough arm)
- Modify: `crates/rt/src/main.rs`

**Interfaces:**
- Consumes: `make_payload_card` (Task 2), `cancel_drag`, `clear_drag_cues`, `opens_modal_overlay`/`removes_pane_mid_drag` gate block in `on_key_press`, `Session::tab_panes`, `tab_bars`.
- Produces:

```rust
// rt-config
Action::PickUpPane   // "<Shift><Control>m"
Action::PickUpTab    // "<Shift><Control>n"
// main.rs
struct CarryState {
    payload: dragdrop::DragPayload,
    payload_panes: Vec<rt_core::PaneId>,
    source: WindowId,
    label: String,
    card: Option<winit::window::CustomCursor>,
}
// App field: carry: Option<CarryState>
// Active field: carry_cursor: bool  // this window currently shows the card; update_cursor stands down
enum WindowCmd { ..., PickUpPane, PickUpTab }
fn enter_carry(&mut self, event_loop: &ActiveEventLoop, source: WindowId, payload: dragdrop::DragPayload) -> bool;
fn cancel_carry(&mut self);
```

- [ ] **Step 1: Failing chord test** (rt-config tests)

```rust
#[test]
fn pickup_actions_have_default_chords() {
    let km = Keymap::default();
    for (accel, action) in [
        ("<Shift><Control>m", Action::PickUpPane),
        ("<Shift><Control>n", Action::PickUpTab),
    ] {
        let chord = keys::Chord::parse(accel).expect("valid chord");
        assert_eq!(km.action_for(&chord), Some(action), "{accel}");
    }
}
```

Run `cargo test -p rt-config` → compile failure. Add the two variants (doc style: `/// rt-specific: pick up the focused pane — carry mode: aim in any rt window, click to drop.`), the two defaults lines, the rt-session passthrough arm entries (return `None`), MANUAL key-list lines for both chords (format per neighbours; the coverage test is the arbiter), and menu rows `"Pick Up Pane"` / `"Pick Up Tab"` immediately after the two Detach rows in `items()`. Workspace test until green.

- [ ] **Step 2: CarryState + enter/cancel**

```rust
/// Enter carry: the modal pick-up state. Cancels any live drag or previous
/// carry first. Returns false (no state change) for a stale payload.
fn enter_carry(&mut self, event_loop: &ActiveEventLoop, source: WindowId, payload: dragdrop::DragPayload) -> bool {
    self.cancel_drag();
    self.cancel_carry();
    let Some(active) = self.windows.get_mut(&source) else { return false };
    let payload_panes = match payload {
        dragdrop::DragPayload::Pane(p) => vec![p],
        dragdrop::DragPayload::Tab { first_pane } => match active.session.tab_panes(first_pane) {
            Some(v) if !v.is_empty() => v,
            _ => return false,
        },
    };
    if matches!(payload, dragdrop::DragPayload::Pane(p) if active.session.pane(p).is_none()) {
        return false; // stale pane id
    }
    let label = match payload {
        dragdrop::DragPayload::Pane(p) => active.session.title_of(p).unwrap_or("Pane").to_string(),
        dragdrop::DragPayload::Tab { first_pane } => active
            .session
            .title_of(first_pane)
            .map(|t| format!("Tab: {t}"))
            .unwrap_or_else(|| "Tab".to_string()),
    };
    let card = Self::make_payload_card(event_loop, active, payload);
    // The payload dims at home for the whole carry.
    active.drag_dim = match payload {
        dragdrop::DragPayload::Pane(p) => Some(p),
        dragdrop::DragPayload::Tab { .. } => None,
    };
    active.force_full = true;
    active.window.request_redraw();
    self.carry = Some(CarryState { payload, payload_panes, source, label, card });
    // Every window shows the card (or Grabbing) while the carry lasts.
    self.apply_carry_cursor_all();
    true
}

/// Show the carry cursor on every open window (also called for a window
/// created mid-carry, from build_active's caller).
fn apply_carry_cursor_all(&mut self) {
    let card = self.carry.as_ref().and_then(|c| c.card.clone());
    for a in self.windows.values_mut() {
        match &card {
            Some(c) => a.window.set_cursor(winit::window::Cursor::Custom(c.clone())),
            None => a.window.set_cursor(CursorIcon::Grabbing),
        }
        a.cursor_icon = Some(CursorIcon::Grabbing);
        a.carry_cursor = true;
    }
}

/// Leave carry: clear every window's cues/dim/cursor. Safe to call when idle.
fn cancel_carry(&mut self) {
    if self.carry.take().is_none() {
        return;
    }
    for a in self.windows.values_mut() {
        a.carry_cursor = false;
        Self::clear_drag_cues(a); // cue/ghost/dim wipe + update_cursor + force_full + redraw
    }
}
```

`update_cursor` (main.rs:5629): add `|| active.carry_cursor` to the early-return guard so motion never resets the card. `build_active`'s insert site (NewWindow / tear_out): after inserting a new Active while `self.carry.is_some()`, call `apply_carry_cursor_all()`.

- [ ] **Step 3: Wire the entries**

- A2: `apply_action` arms `Action::PickUpPane => WindowCmd::PickUpPane` / `PickUpTab` (pattern of the Detach arms); `run_window_cmd` handles them: for PickUpPane `let payload = Pane(session.focus())`; for PickUpTab find the active tab's `first_pane` exactly as `detach` does (the `tab_bars(..).iter().flat_map(..).find(|t| t.active)` idiom); then `self.enter_carry(event_loop, id, payload)`.
- A1: in `commit_drag`'s tear-out arm, before calling `tear_out`: if `active.mods.control_key()` (read from the source window's Active before the arm — capture `let ctrl = ...` where `active` is available), enter carry with the drag's payload instead:

```rust
None => match (drag.cued, self.tear_out_release(id)) {
    (None, Some(at)) => {
        if ctrl_held {
            self.enter_carry(event_loop, id, drag.payload)
        } else {
            self.tear_out(event_loop, id, drag.payload, at)
        }
    }
    _ => false,
},
```

(`commit_drag` needs `event_loop` — it already has it for `tear_out`.)
- Gates: in `on_key_press`'s pre-dispatch gate (the `opens_modal_overlay`/`removes_pane_mid_drag` block), also cancel carry: while `self.carry.is_some()`, an overlay-opening OR pane-removing action calls `self.cancel_carry()` first (then dispatches). Escape while `self.carry.is_some()` cancels carry and returns (place beside the drag-Escape). The top-of-`window_event` second-button block: a Right/Middle Pressed while `self.carry.is_some()` cancels carry (mirroring drags — same block, extra condition).
- Cancel-on-death: in `about_to_wait`'s exited-pane handling, where drags cancel on payload death, also `if self.carry.as_ref().is_some_and(|c| c.payload_panes.contains(&id)) { self.cancel_carry(); }` (staged outside the per-window borrow like the drag equivalent). In `close_window`: if the closing window is `carry.source`, `cancel_carry()`.

- [ ] **Step 4: Build + tests + headless smoke**

Workspace green. Xvfb smoke (hygiene rules): `Ctrl+Shift+M` → no crash, focused pane dims (screenshot); Escape → dim clears; `Ctrl+Shift+W` mid-carry → carry cancels, pane closes; right-click mid-carry cancels and menu does NOT open on that press if that's the drag-parallel behaviour you implemented — state which in the report.

- [ ] **Step 5: Commit**

```bash
git add crates/rt-config/src/lib.rs crates/rt-session/src/lib.rs crates/rt/src/menu.rs crates/rt/src/manual.rs crates/rt/src/main.rs
git commit -m "feat(rt): carry state — pick-up actions, Ctrl-release entry, cancels and gates"
```

---

### Task 4: Carry hover cues + click-to-drop

**Files:**
- Modify: `crates/rt/src/main.rs` (`CursorMoved`, `CursorLeft`, left-press arm)

**Interfaces:**
- Consumes: `dragdrop::resolve_drop`, `same_window_drop(id, payload, r)`, `cross_window_drop(source, payload, w, r)`, `cancel_carry`, Active cue fields.
- Produces: carry hover/commit behaviour; no new public names.

- [ ] **Step 1: Hover resolution per window**

In `CursorMoved`, after the drag-motion block (which `return`s when a drag owns the pointer), add the carry block — it must come BEFORE the other motion arms (selection, hover-forwarding, focus-follows-mouse) and `return` when carry is active, except: skip resolution when this window has a modal overlay up (`prefs_open || manual_open || search_open || clip_overlay.is_some() || menu.is_some() || picker.is_some()`), where the carry block clears this window's cues and falls through to the overlay's own handling.

```rust
if let Some(carry) = self.carry.as_ref() {
    let overlay_up = active.prefs_open || active.manual_open || active.search_open
        || active.clip_overlay.is_some() || active.menu.is_some() || active.picker.is_some();
    if !overlay_up {
        let bounds = content_bounds(active.window.inner_size());
        let (panes, bars) = (active.session.visible_rects(bounds), active.session.tab_bars(bounds));
        let resolved = dragdrop::resolve_drop(
            carry.payload, &carry.payload_panes, &panes, &bars, bounds, active.mouse,
        );
        let label = carry.label.clone();
        let dim = (carry.source == id).then(|| match carry.payload {
            dragdrop::DragPayload::Pane(p) => Some(p),
            dragdrop::DragPayload::Tab { .. } => None,
        }).flatten();
        active.drag_cue = resolved;
        active.drag_ghost = Some((active.mouse, label));
        active.drag_dim = dim;
        active.force_full = true;
        active.window.request_redraw();
        return; // a carry owns plain motion in rt windows
    }
}
```

`WindowEvent::CursorLeft` (add an arm if the main match lacks one): while carry is active, clear THIS window's cue/ghost (keep `drag_dim` if it is the source), `force_full` + redraw — the card cursor keeps "holding" the payload between windows.

- [ ] **Step 2: Click-to-drop**

In the left-press arm, BEFORE the existing hit-test chain (clip affordance/jacks/divider/…), so a carry click can never leak into selection/focus side effects:

```rust
if let Some(carry) = self.carry.as_ref() {
    let committed = match active.drag_cue.clone() {
        Some(r) => {
            let (payload, source) = (carry.payload, carry.source);
            if source == id {
                self.same_window_drop(id, payload, r)
            } else {
                self.cross_window_drop(source, payload, id, r)
            }
        }
        None => false, // dead zone: the carry continues, the press is consumed
    };
    if committed {
        self.cancel_carry(); // clears every window's cues + cursors
    } else if let Some(a) = self.windows.get_mut(&id) {
        a.window.request_redraw();
    }
    return;
}
```

Borrow note: the `active` borrow from the surrounding arm must end before `self.same_window_drop`/`cross_window_drop` (both `&mut self`) — stage `active.drag_cue.clone()` into a local first, as shown, and re-derive `active` afterwards if needed. `cross_window_drop` already handles jacks/wires migration and closes an emptied source; `same_window_drop` handles the reorder-vs-move tab logic. Neither knows about carry — `cancel_carry` afterwards is what releases the modal state.

Also: the left-press RELEASE arm must not misinterpret the drop click — verify the release path is a no-op when neither `armed_drag`/`drag` nor selection state was set by the press (it is today: the press returned before selection started; state which release arms you traced in the report).

- [ ] **Step 3: Build + tests + headless verification**

Workspace green. Xvfb (hygiene rules), two windows via Ctrl+Shift+I: `Ctrl+Shift+M` in window A → move mouse into window B → cues appear (screenshot: half-pane fill / caret / centre highlight at three positions); click B's pane centre → swap… note: centre = `Swap` — with a carry payload from ANOTHER window this goes through `cross_window_drop`'s Swap branch (already implemented for drags) — verify it commits; click an edge zone → split-beside commit; pick up again, click a gutter → nothing, carry persists; Escape → all cues clear everywhere. `Ctrl`-release entry: drag a titlebar out of A, hold Ctrl, release → carry (dim stays, no new window), then click into B → drop.

- [ ] **Step 4: Commit**

```bash
git add crates/rt/src/main.rs
git commit -m "feat(rt): carry hover cues and click-to-drop across windows"
```

---

### Task 5: Docs, map, release prep

**Files:**
- Modify: `crates/rt/src/manual.rs` (carry prose in the drag & drop section: pick-up chords, Ctrl-release entry, aim/click/Escape, the card cursor, the foreign-surface cursor reversion note)
- Modify: `docs/KNOWN_ISSUES.md` (rewrite the "Cross-window DRAG is X11-only" entry: carry mode now covers cross-window drops on Wayland; the remaining X11-only piece is the single-fluid-gesture live drag; keep the data-device DnD future note)
- Modify: `README.md` (one line: carry mode / pick up & drop across windows on Wayland)
- Modify: `project-map.js` (DATA-only: `multiwindow` node desc gains carry mode; `project.updated` = completion date; deps check)

**Interfaces:** none new; every claim traced to shipped code (read the final diff first).

- [ ] **Step 1: Make the edits** (statuses reflect the merged truth; the binding-coverage manual tests are the gate for the chord lines).
- [ ] **Step 2: Verify** — `cargo build --workspace && cargo test --workspace`; `node -e 'global.window={}; require("./project-map.js"); console.log(window.PROJECT_MAP.nodes.length)'` parses; deps resolve.
- [ ] **Step 3: Commit**

```bash
git add crates/rt/src/manual.rs docs/KNOWN_ISSUES.md README.md project-map.js
git commit -m "docs: carry mode — manual, known issues, readme, project map"
```

---

### Task 6: Whole-branch verification (tear-out fix + carry mode = v0.3.18 candidate)

- [ ] `cargo build --workspace && cargo test --workspace` green, zero warnings.
- [ ] Final whole-branch review (superpowers:requesting-code-review) over `main..HEAD` of `feat/wayland-drag-tearout` — the branch now holds the Wayland tear-out enable + fix + spec + this plan's commits.
- [ ] Real-display hand-off list for Roland (cannot be automated; cursor visuals especially): the card cursor during a drag-out on cosmic-comp (visible over desktop/other apps) and over ssh -X; carry pick-up → cross-window drop on Wayland (the headline feature); Ctrl-release entry feel; card fallback behaviour if cosmic-comp rejects the cursor size.
- [ ] After Roland's verification: merge to main, release v0.3.18, deploy all three machines (standing procedure).
