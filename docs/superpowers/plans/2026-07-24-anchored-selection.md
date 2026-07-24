# Anchored Selection Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Add an "anchored" text-selection mode: Shift+click drops a start anchor, you navigate freely (accelerating arrow keys, scrollbar, Page/Home/End) while the anchor stays pinned to its content, and a second Shift+click (or Enter) sets the end and copies — so you can select more than a screenful without a held drag.

**Architecture:** The pure navigation math (how the head moves for each key, clamped to the buffer) and the titlebar status string live in a new, fully unit-tested module `crates/rt/src/select.rs`. The event wiring in `main.rs` holds one new flag (`composing`) beside the existing `selection: Option<Selection>`; while it is set the focused pane is modal — keyboard input drives the selection head instead of reaching the shell. The feature reuses what already exists: absolute-line anchoring in `Selection` (survives scroll), `pane.scroll()` for scroll-to-follow, `arrow_accel_step`/`arrow_hold` for acceleration, `selection_text` for the copy, and `Selection::contains` for drawing.

**Tech Stack:** Rust, winit event loop, the in-house rt-engine pane API (`scroll_info()`, `scroll(dir)`, `selection_text()`).

## Global Constraints

- Rust edition/toolchain as pinned in the workspace; no new dependencies.
- Absolute buffer lines are `screen_row - scroll_offset`: scrollback lines are **negative**, live-screen lines are `0..screen-1`. `Selection.anchor`/`head` are `(col, abs_line)`. (See `main.rs:1934`, `2679`.)
- Selection copy convention: PRIMARY on drag-release (`store_primary`); explicit copy writes **both** CLIPBOARD and PRIMARY (`do_copy`, `main.rs:2608`).
- Rectangular/block selection is `Selection.block`, chosen by holding **Ctrl** at the start gesture (matches Ctrl-drag, `main.rs:1980`).
- UI wiring in `main.rs` cannot be unit-tested (winit event loop); those tasks end with a build + an explicit manual-verification checklist run on dop651 (local GL). Pure logic in `select.rs` is TDD'd with real `cargo test`.
- Build check for a wiring task: `cargo build --release -p rt --features x11` must finish clean. Full gate before any release: `cargo test --workspace --release -- --test-threads=1`.
- Deploy standing order (only when explicitly releasing, not per task): install to `~/.cargo/bin` **and** `/usr/bin` on dop651/apollo/milkv; verify via bare `ssh HOST 'rt --version'`.

---

### Task 1: Pure head-navigation logic (`select.rs`)

**Files:**
- Create: `crates/rt/src/select.rs`
- Modify: `crates/rt/src/main.rs` (add `mod select;` near the other `mod` lines, e.g. beside `mod prefs_model;`)
- Test: `crates/rt/src/select.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub enum Nav { Left, Right, Up, Down, LineStart, LineEnd, PageUp, PageDown, BufTop, BufBottom }`
  - `pub struct Bounds { pub cols: usize, pub min_line: i32, pub max_line: i32, pub page: usize }`
  - `pub fn move_head(head: (usize, i32), nav: Nav, b: Bounds) -> (usize, i32)`

- [ ] **Step 1: Write the failing test**

Create `crates/rt/src/select.rs` with only the test module and stub signatures:

```rust
//! Pure logic for the anchored selection mode (see
//! docs/superpowers/plans/2026-07-24-anchored-selection.md). No I/O, no winit —
//! just how the selection head moves and how its status reads, so both are
//! unit-testable without the event loop.

/// A keyboard navigation applied to the selection head while composing.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Nav {
    Left,
    Right,
    Up,
    Down,
    LineStart,
    LineEnd,
    PageUp,
    PageDown,
    BufTop,
    BufBottom,
}

/// The range the head may move within, in the pane's coordinates. `cols` is the
/// grid width; `min_line`/`max_line` are the inclusive ABSOLUTE-line bounds the
/// buffer offers (oldest scrollback line .. newest live line — recall scrollback
/// lines are negative); `page` is the visible row count (for Page moves).
#[derive(Clone, Copy, Debug)]
pub struct Bounds {
    pub cols: usize,
    pub min_line: i32,
    pub max_line: i32,
    pub page: usize,
}

/// Move `head` one step for `nav`, clamped to `b`. Column clamps to
/// `[0, cols-1]`; line clamps to `[min_line, max_line]`. Left/Right stay on the
/// current line (use Up/Down to change line); jumps (Home/End/Page/Buf*) are
/// absolute.
pub fn move_head(head: (usize, i32), nav: Nav, b: Bounds) -> (usize, i32) {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b() -> Bounds {
        Bounds { cols: 80, min_line: -100, max_line: 23, page: 24 }
    }

    #[test]
    fn arrows_move_one_cell_or_row_and_clamp_to_bounds() {
        // Right/Down advance; Left/Up retreat.
        assert_eq!(move_head((5, 0), Nav::Right, b()), (6, 0));
        assert_eq!(move_head((5, 0), Nav::Left, b()), (4, 0));
        assert_eq!(move_head((5, 0), Nav::Down, b()), (5, 1));
        assert_eq!(move_head((5, 0), Nav::Up, b()), (5, -1));
        // Column clamps at both ends of the row.
        assert_eq!(move_head((0, 0), Nav::Left, b()), (0, 0));
        assert_eq!(move_head((79, 0), Nav::Right, b()), (79, 0));
        // Line clamps to the buffer bounds.
        assert_eq!(move_head((5, 23), Nav::Down, b()), (5, 23));
        assert_eq!(move_head((5, -100), Nav::Up, b()), (5, -100));
    }

    #[test]
    fn home_end_hit_the_line_edges_and_page_moves_a_screenful() {
        assert_eq!(move_head((40, 5), Nav::LineStart, b()), (0, 5));
        assert_eq!(move_head((40, 5), Nav::LineEnd, b()), (79, 5));
        assert_eq!(move_head((10, 5), Nav::PageDown, b()), (10, 23)); // 5+24 clamps to 23
        assert_eq!(move_head((10, 5), Nav::PageUp, b()), (10, -19)); // 5-24
    }

    #[test]
    fn buffer_ends_jump_to_the_extremes() {
        assert_eq!(move_head((10, 5), Nav::BufTop, b()), (0, -100));
        assert_eq!(move_head((10, 5), Nav::BufBottom, b()), (79, 23));
    }

    #[test]
    fn a_zero_width_grid_keeps_the_column_at_zero() {
        let z = Bounds { cols: 0, min_line: -5, max_line: 5, page: 10 };
        assert_eq!(move_head((0, 0), Nav::Right, z), (0, 0));
        assert_eq!(move_head((0, 0), Nav::LineEnd, z), (0, 0));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rt select:: 2>&1 | tail -20`
Expected: FAIL — `move_head` panics with `not yet implemented` (todo!).

- [ ] **Step 3: Write the minimal implementation**

Replace the `todo!()` body of `move_head`:

```rust
pub fn move_head(head: (usize, i32), nav: Nav, b: Bounds) -> (usize, i32) {
    let (col, line) = (head.0, head.1);
    let max_col = b.cols.saturating_sub(1);
    let clamp_line = |l: i32| l.clamp(b.min_line, b.max_line);
    let (col, line) = match nav {
        Nav::Left => (col.saturating_sub(1), line),
        Nav::Right => ((col + 1).min(max_col), line),
        Nav::Up => (col, clamp_line(line - 1)),
        Nav::Down => (col, clamp_line(line + 1)),
        Nav::LineStart => (0, line),
        Nav::LineEnd => (max_col, line),
        Nav::PageUp => (col, clamp_line(line - b.page as i32)),
        Nav::PageDown => (col, clamp_line(line + b.page as i32)),
        Nav::BufTop => (0, b.min_line),
        Nav::BufBottom => (max_col, b.max_line),
    };
    (col.min(max_col), line)
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rt select:: 2>&1 | tail -20`
Expected: PASS — 4 tests in `select::tests`.

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/select.rs crates/rt/src/main.rs
git commit -m "feat(rt): pure head-navigation logic for anchored selection"
```

---

### Task 2: Pure compose status text (`select.rs`)

**Files:**
- Modify: `crates/rt/src/select.rs`
- Test: `crates/rt/src/select.rs` (`#[cfg(test)] mod tests`)

**Interfaces:**
- Consumes: nothing.
- Produces: `pub fn status_text(anchor: (usize, i32), head: (usize, i32), block: bool) -> String`

- [ ] **Step 1: Write the failing test**

Add to the `tests` module in `crates/rt/src/select.rs`:

```rust
    #[test]
    fn status_reads_line_count_for_linear_and_dimensions_for_block() {
        // Linear: inclusive line span, pluralised.
        assert_eq!(status_text((0, 0), (10, 0), false), "◉ selecting · 1 line");
        assert_eq!(status_text((0, 0), (3, 4), false), "◉ selecting · 5 lines");
        // Order-independent (head may be above the anchor).
        assert_eq!(status_text((0, 4), (3, 0), false), "◉ selecting · 5 lines");
        // Block: cols × rows, inclusive, order-independent.
        assert_eq!(status_text((2, 0), (13, 3), true), "◉ selecting · 12×4");
        assert_eq!(status_text((13, 3), (2, 0), true), "◉ selecting · 12×4");
    }
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p rt select:: 2>&1 | tail -20`
Expected: FAIL — `status_text` not found (cannot compile).

- [ ] **Step 3: Write the minimal implementation**

Add to `crates/rt/src/select.rs` (after `move_head`):

```rust
/// The titlebar status shown while composing. Linear selections read as a line
/// count; block selections as `cols×rows`. Both spans are inclusive and
/// order-independent (the head may sit above/left of the anchor).
pub fn status_text(anchor: (usize, i32), head: (usize, i32), block: bool) -> String {
    if block {
        let cols = (anchor.0 as i64 - head.0 as i64).unsigned_abs() as usize + 1;
        let rows = (anchor.1 - head.1).unsigned_abs() as usize + 1;
        format!("◉ selecting · {cols}×{rows}")
    } else {
        let lines = (anchor.1 - head.1).unsigned_abs() as usize + 1;
        let unit = if lines == 1 { "line" } else { "lines" };
        format!("◉ selecting · {lines} {unit}")
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p rt select:: 2>&1 | tail -20`
Expected: PASS — 5 tests in `select::tests`.

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/select.rs
git commit -m "feat(rt): compose status text for anchored selection"
```

---

### Task 3: Compose state + entry gesture + modal capture skeleton

Adds the `composing` flag, the entry gesture (Shift+click with no drag → enter compose), and a modal keyboard skeleton so keys are swallowed and **Esc** / a **plain click** cancels. Head movement, commit, and the indicator come in later tasks — after this task you can enter and always leave the mode.

**Files:**
- Modify: `crates/rt/src/main.rs` — the `Active` struct (near `selecting: bool`, `main.rs:314`), its initializer (near `selecting: false`, `main.rs:1054`), the left-press handler (`main.rs:1896`–`1988`), the drag-extend block (`main.rs:1761`–`1773`), the left-release handler (`main.rs:1991`–`2032`), and the top of `on_key_press` (after `main.rs:3050`).

**Interfaces:**
- Consumes: `select::{Nav, move_head}` (not yet called here), `Selection` (existing).
- Produces: `Active.composing: bool`, `Active.shift_press: bool`, and a private `fn compose_cancel(active: &mut Active)`.

- [ ] **Step 1: Add the state fields**

In the `Active` struct, beside `selecting: bool,` (`main.rs:314`):

```rust
    composing: bool,   // true while an anchored selection is being built (modal)
    shift_press: bool, // the in-flight left-press was Shift-held (candidate for compose entry)
```

In the initializer, beside `selecting: false,` (`main.rs:1054`):

```rust
            composing: false,
            shift_press: false,
```

- [ ] **Step 2: Add the cancel helper**

Add near `copy_selection_to_primary` (`main.rs:3002`):

```rust
    /// Leave anchored-compose mode, discarding the in-progress selection and
    /// touching no clipboard.
    fn compose_cancel(active: &mut Active) {
        active.composing = false;
        active.shift_press = false;
        active.selection = None;
        active.force_full = true;
        active.window.request_redraw();
    }
```

- [ ] **Step 3: Mark a Shift-press, and clear it on drag**

In the single-click branch of the left-press handler, where the block selection is created (`main.rs:1980`–`1982`), record whether Shift is held:

```rust
                                    _ => {
                                        // Ctrl-drag = rectangular block. (Ctrl-click ON a
                                        // URL already returned above to open it, so a
                                        // block only ever starts on non-link text.)
                                        let block = active.mods.control_key();
                                        active.selection = Some(Selection { pane, anchor: (col, line), head: (col, line), block });
                                        active.selecting = true;
                                        // A Shift-held press with no ensuing drag becomes an
                                        // anchored-compose start (resolved at release).
                                        active.shift_press = active.mods.shift_key();
                                    }
```

In the drag-extend block (`main.rs:1766`–`1771`), once the head actually moves, this was a drag, not a click — so it is no longer a compose candidate. Add inside `if sel.pane == pane {` after `sel.head = ...`:

```rust
                                sel.head = (col, row as i32 - off); // move the drag end
                                active.shift_press = false; // a drag, not a click → not compose entry
```

- [ ] **Step 4: Enter compose on a no-drag Shift release; handle clicks while composing**

At the **top** of the `(ElementState::Pressed, MouseButton::Left)` arm (`main.rs:1896`, immediately after the arm opens, before the URL/forward logic), intercept clicks while composing:

```rust
                    // While composing an anchored selection, a click finishes or
                    // aborts it — it never starts a new selection.
                    if active.composing {
                        // (Shift+click → set head + commit is added in Task 6;
                        // for now any click cancels so the mode is always escapable.)
                        Self::compose_cancel(active);
                        return;
                    }
```

In the left-release handler, replace the zero-length-selection branch (`main.rs:2021`–`2031`) so a no-drag Shift press enters compose instead of being discarded:

```rust
                    if let Some(sel) = active.selection {
                        if sel.anchor == sel.head {
                            if active.shift_press {
                                // No drag followed a Shift-press: promote to anchored
                                // compose. Keep the (zero-length) selection as the anchor.
                                active.composing = true;
                            } else {
                                active.selection = None; // a plain click: discard
                            }
                        } else if let Some(text) = Self::selected_text(active) {
                            if let Some(cb) = &active.clipboard {
                                cb.store_primary(text); // PRIMARY for middle-click paste
                            }
                        }
                        active.force_full = true; // selection cleared/finalised: repaint highlight
                        active.window.request_redraw();
                    }
                    active.shift_press = false; // consumed
```

- [ ] **Step 5: Capture keys while composing (skeleton)**

At the top of `on_key_press`, right after the `ime_preedit` guard (`main.rs:3050`) and before the keymap-chord check:

```rust
        // Anchored-compose is modal: keyboard input drives the selection, not the
        // shell. Esc cancels; navigation/commit keys are added in later tasks;
        // every other key is swallowed so nothing leaks to the pane.
        if active.composing {
            if matches!(key_event.logical_key, Key::Named(NamedKey::Escape)) {
                Self::compose_cancel(active);
            }
            return;
        }
```

- [ ] **Step 6: Build**

Run: `cargo build --release -p rt --features x11 2>&1 | grep -E "^error|Finished"`
Expected: `Finished` with no `error` lines. (An unused-import/dead-code warning for `select::move_head` is expected until Task 5.)

- [ ] **Step 7: Manual verification (dop651, local GL)**

Run `rt` (or `~/.cargo/bin/rt` after `cargo install --path crates/rt --force`). In a pane with some scrollback:
1. **Shift+click** on a character, then release without moving → nothing sent to the shell; typing letters now does nothing (keys swallowed). *(No visible indicator yet — that's Task 4.)*
2. Press **Esc** → typing works again (mode exited).
3. Shift+click to enter, then a **plain left-click** → mode exits, typing works.
4. **Shift+drag** still makes a normal selection (drag, not a click, so no compose).
5. A plain click/drag with no Shift is unchanged.

- [ ] **Step 8: Commit**

```bash
git add crates/rt/src/main.rs
git commit -m "feat(rt): enter/exit anchored-compose mode (Shift-click, Esc, plain-click)"
```

---

### Task 4: Titlebar compose indicator

Makes the mode visible: while composing, the focused pane's titlebar shows `select::status_text`. This makes Tasks 5–6 visually verifiable.

**Files:**
- Modify: `crates/rt/src/main.rs` — the per-pane titlebar drawing, in the title-text area (before the scrollback meter / size string; see `main.rs:3606`–`3639` for the right-anchored fields and `left_x` for the left cursor).

**Interfaces:**
- Consumes: `select::status_text`, `Active.composing`, `Active.selection`.
- Produces: nothing new (draw-only).

- [ ] **Step 1: Draw the indicator for the composing pane**

In the titlebar builder, where the title text is placed from `left_x` (just before the size/meter fields, `main.rs:3606`), add — inside the per-pane loop, using that pane's `id`, `full` (titlebar rect), `text_top`, `cell_w`, and `focused`:

```rust
                // Anchored-compose indicator: for the pane being composed, replace
                // the title with a live status ("◉ selecting · N lines"). Drawn in
                // the accent colour so it reads as an active mode.
                if active.composing {
                    if let Some(sel) = active.selection {
                        if sel.pane == id {
                            let status = select::status_text(sel.anchor, sel.head, sel.block);
                            let scol = Color::rgb(0x6a, 0xa9, 0xff); // the focus-accent blue
                            for (i, ch) in status.chars().enumerate() {
                                let x = left_x + i as f32 * cell_w;
                                active.backend.draw_char(x, text_top, 0, 0, ch, scol, false, false);
                            }
                        }
                    }
                }
```

Note: draw the indicator *after* `left_x` is finalised (past the broadcast swatch) but it may overlap the normal title — that is fine, the status replaces it while composing. If the existing code draws the title unconditionally, guard the title draw with `!(active.composing && sel_is_this_pane)` or simply draw the indicator last so it paints over the title. Confirm by reading the surrounding lines when implementing.

- [ ] **Step 2: Build**

Run: `cargo build --release -p rt --features x11 2>&1 | grep -E "^error|Finished"`
Expected: `Finished`, no errors.

- [ ] **Step 3: Manual verification (dop651)**

After `cargo install --path crates/rt --force`, run `rt`:
1. **Shift+click** (no drag) in a pane → its titlebar shows `◉ selecting · 1 line`.
2. **Esc** → the titlebar returns to its normal title.
3. With per-pane titlebars off (Preferences → "Show per-pane titlebars"), the indicator simply isn't shown (acceptable for v1); turn them on to see it.

- [ ] **Step 4: Commit**

```bash
git add crates/rt/src/main.rs
git commit -m "feat(rt): titlebar indicator while composing an anchored selection"
```

---

### Task 5: Keyboard head movement with acceleration + scroll-to-follow

Fills in the navigation keys: arrows move the head (accelerating on hold via the existing arrow-accel path), Home/End/Page/Ctrl+Home/End jump, and the view auto-scrolls to keep the head visible. The indicator's counts update live, so this is directly verifiable.

**Files:**
- Modify: `crates/rt/src/main.rs` — the compose branch of `on_key_press` (added in Task 3, after `main.rs:3050`); add two private helpers near `compose_cancel`.

**Interfaces:**
- Consumes: `select::{Nav, move_head, Bounds}`, `Active.{selection, arrow_hold, settings, session}`, `arrow_accel_step`, `ARROW_HOLD_GAP`, `pane_content_rect`, `backend.cell_size()`, `pane.scroll_info()`, `pane.scroll(dir)`.
- Produces: `fn compose_nav(active: &mut Active, nav: Nav, accelerate: bool)`, `fn compose_bounds(active: &Active, pane: rt_core::PaneId) -> Option<select::Bounds>`, `fn scroll_head_into_view(active: &mut Active, pane: rt_core::PaneId, head_line: i32)`.

- [ ] **Step 1: Add the bounds + scroll-follow helpers**

Near `compose_cancel` in `main.rs`:

```rust
    /// The head's movement bounds for the pane, from its grid width and buffer
    /// extent. Absolute lines run `-(history) ..= screen-1` (scrollback negative).
    fn compose_bounds(active: &Active, pane: rt_core::PaneId) -> Option<select::Bounds> {
        let content = Self::pane_content_rect(active, pane)?;
        let (cw, _) = active.backend.cell_size();
        let cols = (content.w / cw).max(0.0) as usize;
        let (_, history, screen) = active.session.pane(pane)?.scroll_info();
        Some(select::Bounds {
            cols,
            min_line: -(history as i32),
            max_line: screen as i32 - 1,
            page: screen,
        })
    }

    /// Scroll the pane just enough that absolute line `head_line` is on screen.
    /// Visible absolute lines are `[-offset, screen-1-offset]`; step one line at a
    /// time (bounded) toward the head.
    fn scroll_head_into_view(active: &mut Active, pane: rt_core::PaneId, head_line: i32) {
        for _ in 0..10_000 {
            // safety cap: never spin
            let Some(p) = active.session.pane(pane) else { return };
            let (offset, _, screen) = p.scroll_info();
            let top = -(offset as i32);
            let bottom = screen as i32 - 1 - offset as i32;
            if head_line < top {
                p.scroll(1); // toward older history
            } else if head_line > bottom {
                p.scroll(-1); // toward newest
            } else {
                return; // in view
            }
        }
    }
```

- [ ] **Step 2: Add the navigation applier (with acceleration)**

Also near `compose_cancel`:

```rust
    /// Apply a head navigation while composing. Arrow moves (accelerate = true)
    /// repeat per the held-arrow acceleration the user configured; jumps apply
    /// once. Then scroll to keep the head visible and repaint.
    fn compose_nav(active: &mut Active, nav: select::Nav, accelerate: bool) {
        let Some(sel) = active.selection else { return };
        let pane = sel.pane;
        let Some(bounds) = Self::compose_bounds(active, pane) else { return };
        // Held-arrow acceleration: reuse arrow_hold/arrow_accel_step. `arrow` is a
        // per-direction tag so a change of direction resets the run.
        let now = Instant::now();
        let steps = if accelerate && active.settings.arrow_accel {
            let tag = nav as u8; // stable per Nav variant
            let repeats = match active.arrow_hold {
                Some((prev, t, n)) if prev == tag && now.duration_since(t) < ARROW_HOLD_GAP => n + 1,
                _ => 0,
            };
            active.arrow_hold = Some((tag, now, repeats));
            arrow_accel_step(repeats, active.settings.arrow_accel_max)
        } else {
            active.arrow_hold = None;
            1
        };
        let mut head = sel.head;
        for _ in 0..steps {
            head = select::move_head(head, nav, bounds);
        }
        if let Some(s) = active.selection.as_mut() {
            s.head = head;
        }
        Self::scroll_head_into_view(active, pane, head.1);
        active.force_full = true;
        active.window.request_redraw();
    }
```

- [ ] **Step 3: Wire the keys in the compose branch**

Replace the Task-3 skeleton compose branch at the top of `on_key_press` with the full key map:

```rust
        // Anchored-compose is modal: keyboard input drives the selection head, not
        // the shell. Arrows accelerate on hold (reusing the arrow-accel prefs);
        // Home/End/Page/Ctrl+Home/End jump; Esc cancels; anything else is swallowed.
        if active.composing {
            use select::Nav;
            let ctrl = active.mods.control_key();
            match &key_event.logical_key {
                Key::Named(NamedKey::Escape) => Self::compose_cancel(active),
                Key::Named(NamedKey::ArrowLeft) => Self::compose_nav(active, Nav::Left, true),
                Key::Named(NamedKey::ArrowRight) => Self::compose_nav(active, Nav::Right, true),
                Key::Named(NamedKey::ArrowUp) => Self::compose_nav(active, Nav::Up, true),
                Key::Named(NamedKey::ArrowDown) => Self::compose_nav(active, Nav::Down, true),
                Key::Named(NamedKey::Home) if ctrl => Self::compose_nav(active, Nav::BufTop, false),
                Key::Named(NamedKey::End) if ctrl => Self::compose_nav(active, Nav::BufBottom, false),
                Key::Named(NamedKey::Home) => Self::compose_nav(active, Nav::LineStart, false),
                Key::Named(NamedKey::End) => Self::compose_nav(active, Nav::LineEnd, false),
                Key::Named(NamedKey::PageUp) => Self::compose_nav(active, Nav::PageUp, false),
                Key::Named(NamedKey::PageDown) => Self::compose_nav(active, Nav::PageDown, false),
                _ => {} // swallow everything else
            }
            return;
        }
```

- [ ] **Step 4: Build**

Run: `cargo build --release -p rt --features x11 2>&1 | grep -E "^error|Finished"`
Expected: `Finished`, no errors (the `select::move_head` dead-code warning is now gone).

- [ ] **Step 5: Manual verification (dop651)**

After `cargo install --path crates/rt --force`, run `rt`:
1. Shift+click to enter. Tap **Right/Left/Up/Down** → the highlight grows one cell/row per tap; the indicator's line count updates.
2. **Hold Down** → after a few repeats it accelerates (like the arrow-key acceleration you verified); toggle Preferences → "Hold-arrow acceleration" off → holding now moves 1 row per repeat.
3. Move the head **below the bottom row** → the view auto-scrolls to follow it; move it up into scrollback → it scrolls back.
4. **PageDown/PageUp**, **Home/End**, **Ctrl+Home/Ctrl+End** land where expected.
5. Arrows never reach the shell while composing; **Esc** cancels and typing resumes.

- [ ] **Step 6: Commit**

```bash
git add crates/rt/src/main.rs
git commit -m "feat(rt): keyboard head movement + accel + scroll-follow while composing"
```

---

### Task 6: Commit the selection (Shift+click / Enter) and copy

Finalizes the mode: a second **Shift+click** places the head at the clicked cell and commits; **Enter** commits in place. Commit copies the text to **both CLIPBOARD and PRIMARY** and leaves the selection highlighted.

**Files:**
- Modify: `crates/rt/src/main.rs` — the compose branch of the left-press handler (added in Task 3, `main.rs:1896` top-of-arm), the compose branch of `on_key_press` (Enter case), and a `compose_commit` helper near `compose_cancel`.

**Interfaces:**
- Consumes: `do_copy` (`main.rs:2608`), `cell_at`, `Active.{selection, composing, mods, session}`.
- Produces: `fn compose_commit(active: &mut Active)`.

- [ ] **Step 1: Add the commit helper**

Near `compose_cancel`:

```rust
    /// Finish anchored-compose: copy the selection to CLIPBOARD and PRIMARY, leave
    /// it highlighted (like a completed drag-select), and exit the mode.
    fn compose_commit(active: &mut Active) {
        Self::do_copy(active); // CLIPBOARD + PRIMARY
        active.composing = false;
        active.shift_press = false;
        active.force_full = true;
        active.window.request_redraw();
    }
```

- [ ] **Step 2: Shift+click sets the head and commits; plain click cancels**

Replace the Task-3 compose intercept at the top of the left-press arm (`main.rs:1896`) with the version below. **Preserve the multi-click-continuation guard that Task 3's fix added** (a Shift+double/triple-click's first release enters compose; the second press must resume the normal word/line select, not commit/cancel) — it comes FIRST, before the Shift/plain branch:

```rust
                    // While composing, a click finishes or aborts — never starts a
                    // new selection. But a rapid same-spot follow-up is a Shift+
                    // double/triple-click whose first release entered compose:
                    // abandon compose and let the normal word/line select run.
                    if active.composing {
                        let now = Instant::now();
                        let continuation = matches!(active.last_click, Some((t, (lx, ly)))
                            if now.duration_since(t) < Duration::from_millis(400)
                               && (mx - lx).abs() < 5.0 && (my - ly).abs() < 5.0);
                        if continuation {
                            active.composing = false;
                            active.shift_press = false;
                            // fall through to the normal press handling below
                        } else if active.mods.shift_key() {
                            // Shift+click sets the end at the clicked cell and commits.
                            if let Some((pane, col, row)) = Self::cell_at(active, mx, my) {
                                if let Some(sel) = active.selection.as_mut() {
                                    if sel.pane == pane {
                                        let off = active
                                            .session
                                            .pane(pane)
                                            .map(|p| p.scroll_info().0 as i32)
                                            .unwrap_or(0);
                                        sel.head = (col, row as i32 - off);
                                    }
                                }
                            }
                            Self::compose_commit(active);
                            return;
                        } else {
                            Self::compose_cancel(active); // a plain click cancels
                            return;
                        }
                    }
```

(Confirm `mx`/`my` are in scope at the top of the arm; the arm computes the pointer position early — if `mx`/`my` are derived later, use `active.mouse.0`/`active.mouse.1`, which `cell_at` also accepts. `now`/`last_click`/the 400ms·5px predicate mirror the existing click-count logic later in the arm.)

- [ ] **Step 3: Enter commits in place**

In the compose branch of `on_key_press` (Task 5), add an Enter arm before the `_ => {}`:

```rust
                Key::Named(NamedKey::Enter) => Self::compose_commit(active),
```

- [ ] **Step 4: Build**

Run: `cargo build --release -p rt --features x11 2>&1 | grep -E "^error|Finished"`
Expected: `Finished`, no errors.

- [ ] **Step 5: Manual verification (dop651)**

After `cargo install --path crates/rt --force`, run `rt`:
1. Shift+click to drop an anchor, move the head with the arrows, then press **Enter** → the selection stays highlighted and its text is on the clipboard: **Ctrl+Shift+V** in the same pane, or middle-click in another, pastes exactly it.
2. Repeat but finish with a **second Shift+click** at the target cell → same result; the head lands where you clicked.
3. Compose a selection that spans into scrollback (scroll the view with the wheel mid-compose, then Shift+click a visible cell) → the copied text matches the full range, including the off-screen part.
4. Hold **Ctrl** on the first Shift+click → the highlight and the copied text are rectangular (columns), and the indicator reads `◉ selecting · C×R`. Confirm the copy matches a Ctrl-drag of the same corners.
5. **Esc** still cancels without copying (clipboard unchanged).

- [ ] **Step 6: Full workspace gate**

Run: `cargo test --workspace --release -- --test-threads=1 2>&1 | grep -E "test result:"`
Expected: all suites pass (the two new `select::tests` groups included), 0 failed.

- [ ] **Step 7: Commit**

```bash
git add crates/rt/src/main.rs
git commit -m "feat(rt): commit anchored selection (Shift-click / Enter) and copy to clipboards"
```

---

## Self-Review

**Spec coverage** (against `docs/superpowers/specs/2026-07-24-anchored-selection-design.md`):
- Entering — Shift+click no-drag, Shift+drag unchanged, press→drag/click resolution, Ctrl=block → Task 3 (Steps 3–4), block via existing `main.rs:1980`.
- Composing — arrows move head live w/ accel, Home/End, Page, Ctrl+Home/End, scroll/wheel free, Shift+click places head → Tasks 5–6. (Scroll/wheel already scroll the pane via the existing wheel handler, untouched, and the absolute-line head stays pinned — verified in Task 6 step 3.)
- Committing — 2nd Shift+click or Enter, copy CLIPBOARD+PRIMARY, stays highlighted → Task 6.
- Cancelling — Esc and plain click → Tasks 3 & 6.
- Multi-pane — head stays in `sel.pane`; a click in another pane while composing cancels (plain) or commits/moves only if same pane (Task 6 Step 2 guards `sel.pane == pane`) → covered.
- Titlebar indicator — linear line count / block dims → Tasks 2 & 4.
- Reuse (absolute anchoring, autoscroll/scroll, arrow-accel, selection_text, contains) → Tasks 5–6 use `pane.scroll`, `arrow_accel_step`, `do_copy`/`selected_text`.
- Testing list — pure logic TDD'd in Tasks 1–2; the integration checks are the per-task manual checklists (winit loop is not unit-testable here).

**Placeholder scan:** no TBD/TODO in steps; every code step shows complete code. Two "confirm when implementing" notes (Task 4 Step 1 title-overlap, Task 6 Step 2 `mx`/`my` scope) are explicit read-the-surrounding-lines checks, not deferred work.

**Type consistency:** `Nav`, `Bounds`, `move_head`, `status_text` signatures match across Tasks 1–2 and their call sites in Tasks 4–6. `compose_cancel`/`compose_commit`/`compose_nav`/`compose_bounds`/`scroll_head_into_view` names are used consistently. `arrow_hold` is `Option<(u8, Instant, u32)>` (existing) and `Nav as u8` fits the `u8` tag. `Selection` fields (`pane`, `anchor`, `head`, `block`) match `main.rs:399`.

## Out of scope (own plans later)
- **B** — clipboard history in the titlebar.
- **C** — drag auto-scroll acceleration (ramp the fixed 35 ms `autoscroll_selection`).
