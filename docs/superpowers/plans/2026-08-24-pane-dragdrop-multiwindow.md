# Pane Drag-and-Drop + Multi-Window Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Panes and tabs can be torn out to new OS windows, dragged between rt windows, and rearranged in place, with drop-zone cues (half-pane split highlights, whole-pane swap, tab-insert caret, window-edge strips).

**Architecture:** One rt process owns many OS windows (`App.windows: HashMap<WindowId, Active>`); the process exits only when the last window closes. Panes move by pure tree surgery: `rt-core` gains take/insert ops on an opaque `Subtree`, `rt-session` gains `extract`/`adopt` of a `PanePackage` (backend + side tables travel together), and the rt binary adds an App-owned drag state machine plus overlay cues. Cross-window hover works via winit window positions (X11 only); Wayland gets keyboard/menu parity.

**Tech Stack:** Rust workspace; winit 0.30 + glutin; existing GL and XRender backends; no new dependencies.

**Spec:** `docs/superpowers/specs/2026-08-24-pane-dragdrop-multiwindow-design.md`

## Global Constraints

- **No new crate dependencies.** Everything uses winit/std/x11 features already in the tree.
- **rt-core stays dependency-free and pure** (no serde yet, no GUI types). `Node` stays private; the new public wrapper is the opaque `Subtree`.
- **No-crash policy:** every new op degrades to a `bool`/`Option` no-op on stale ids — never panics.
- **PaneIds become globally unique** (process-wide `AtomicU64`); the empty-tree sentinel stays `PaneId(u64::MAX)`, and `PaneId(u64::MAX - 1)` is reserved as the swap placeholder — the allocator can never mint either.
- **Every new default keybinding gets a `MANUAL` line** — `manual::tests::every_default_keybinding_is_documented` fails otherwise.
- **Damage rule:** any drag motion / cue change / layout change sets `force_full = true` (cues bypass the damage tracker; see the wire rubber-band comments at `crates/rt/src/main.rs:1809-1825`).
- **Reflow rule:** never call `Session::relayout` per pointer-motion event (676 ms/pane measured on milkv). Relayout on commit only.
- **Branch:** all work on `feat/pane-dragdrop-multiwindow`, forked from `main` at this plan's commit.
- Workspace check after every task: `cargo build --workspace && cargo test --workspace` must pass at every commit.

---

## File Structure

- `crates/rt-core/src/layout.rs` — global id mint; `Subtree`; `take`/`take_tab`/`insert_beside`/`insert_root_edge`/`insert_tab_at`/`adopt_root`/`swap`/`replace_leaf`/`reorder_tab`.
- `crates/rt-session/src/lib.rs` — `PaneEntries`, `PanePackage`, `DropTarget`; `extract_pane`/`extract_tab`/`adopt`/`move_pane`/`move_tab`/`reorder_tab`/`is_empty`.
- `crates/rt-config/src/lib.rs` — `Action::{NewWindow, DetachPane, DetachTab, MoveTabLeft, MoveTabRight}` + default chords.
- `crates/rt/src/dragdrop.rs` — **new**: pure drop-target resolver + cue geometry (`resolve_drop`), `DragPayload`, zone constants; unit tests.
- `crates/rt/src/chrome/dragdrop.rs` — **new**: cue painting (fills, caret, ghost chip) via `Backend` primitives.
- `crates/rt/src/main.rs` — multi-window `App`; `build_active` factory; WindowId routing; per-window close; drag arm/track/commit; cross-window hover; tear-out.
- `crates/rt/src/menu.rs` — "Move to window ▸" rows; new action rows.
- `crates/rt/src/manual.rs` — manual lines for the new bindings.
- `docs/ROADMAP.md`, `docs/TERMINATOR_FEATURES.md`, `README.md`, `project-map.js` — final docs/status task.

---

### Task 1: Global PaneId allocation (rt-core)

**Files:**
- Modify: `crates/rt-core/src/layout.rs` (Tree struct at :104-135, tests at :1036-1109)

**Interfaces:**
- Produces: `PaneId`s unique across ALL `Tree`s in the process. `Tree` loses its `next_id` field. Internal `fn mint_global() -> PaneId`. Everything else (`Tree::new`, `split`, `new_tab`) keeps its signature.

- [ ] **Step 1: Create the branch**

```bash
cd ~/git/rt && git checkout -b feat/pane-dragdrop-multiwindow main
```

- [ ] **Step 2: Write the failing test** (append to the `tests` module in `layout.rs`)

```rust
/// Two trees must never hand out the same PaneId — panes move between
/// windows, so ids are process-global (spec: multi-window, zero remapping).
#[test]
fn pane_ids_are_unique_across_trees() {
    let (mut t1, a) = Tree::new();
    let (mut t2, b) = Tree::new();
    let c = t1.split(a, Orientation::LeftRight).unwrap();
    let d = t2.split(b, Orientation::TopBottom).unwrap();
    let ids = [a, b, c, d];
    for (i, x) in ids.iter().enumerate() {
        for y in &ids[i + 1..] {
            assert_ne!(x, y, "ids collide across trees");
        }
    }
}
```

- [ ] **Step 3: Run it to verify it fails**

Run: `cargo test -p rt-core pane_ids_are_unique_across_trees`
Expected: FAIL — both trees start at `PaneId(0)` so `a == b`.

- [ ] **Step 4: Implement the global mint**

In `layout.rs`: add at the top (after `use crate::geom::Rect;`):

```rust
use std::sync::atomic::{AtomicU64, Ordering};

/// Process-global PaneId allocator. Panes can move between windows (trees), so
/// ids must be unique across the whole process, not per-tree — that is what
/// lets a moved subtree keep its side-table entries with zero remapping.
/// u64::MAX stays the empty-tree sentinel and u64::MAX-1 the swap placeholder;
/// a monotonically increasing counter can never reach either.
static NEXT_ID: AtomicU64 = AtomicU64::new(0);

fn mint_global() -> PaneId {
    PaneId(NEXT_ID.fetch_add(1, Ordering::Relaxed))
}
```

Then:
- `struct Tree` (:104-108): delete the `next_id` field (keep `root`).
- `Tree::new` (:116-123): `let first = mint_global();` and build `Tree { root: Node::Leaf(first) }`. It still returns `(tree, first)`.
- `Tree::mint` (:131-135): body becomes `mint_global()` (keep the method so `split`/`new_tab` don't change).
- Tests constructing `Tree { root, next_id }` (`rotate_ccw_recurses_like_a_picture` :1063, `rotate_four_times_is_identity` :1102): drop the `next_id: …` field from the struct literals. Note these tests hand-pick ids 1..4 that the allocator may also mint — that's fine, the tests never call `split`.
- Update the `Tree` doc comment (:99-103): `next_id` is now process-global; note why.

- [ ] **Step 5: Run the tests**

Run: `cargo test -p rt-core && cargo build --workspace`
Expected: all PASS (grep for other `next_id` users first: `grep -rn next_id crates/` must show only layout.rs).

- [ ] **Step 6: Commit**

```bash
git add crates/rt-core/src/layout.rs && git commit -m "feat(rt-core): process-global PaneId allocation for multi-window"
```

---

### Task 2: Subtree extraction and insertion ops (rt-core)

**Files:**
- Modify: `crates/rt-core/src/layout.rs`

**Interfaces:**
- Produces (all `pub` on `Tree` unless noted):
  - `pub struct Subtree(Node)` — opaque, `Debug`; `impl Subtree { pub fn panes(&self) -> Vec<PaneId>; pub fn first_pane(&self) -> Option<PaneId>; }`
  - `pub fn take(&mut self, target: PaneId) -> Option<Subtree>` — remove a leaf with the same collapse as `close`, returning it.
  - `pub fn take_tab(&mut self, first_pane: PaneId) -> Option<Subtree>` — remove the whole tab page whose first leaf is `first_pane` (id from `Tab::first_pane`), collapsing a 1-child `Tabs`.
  - `pub fn insert_beside(&mut self, target: PaneId, sub: Subtree, orient: Orientation, before: bool) -> Result<(), Subtree>` — split `target`, placing `sub` on the chosen side (50/50). `Err` hands the subtree back on a stale target (caller must not lose live panes).
  - `pub fn insert_root_edge(&mut self, sub: Subtree, orient: Orientation, before: bool)` — full-window split at the root (or becomes the root if empty).
  - `pub fn insert_tab_at(&mut self, anchor: PaneId, sub: Subtree, index: usize) -> Result<(), Subtree>` — insert as a tab at `index` in the `Tabs` group directly containing `anchor` as a tab page's first leaf; if `anchor` is a leaf not directly in a `Tabs` group, wrap it (like `new_tab` does). The inserted tab becomes active.
  - `pub fn adopt_root(&mut self, sub: Subtree) -> Result<(), Subtree>` — install as root; refuses (handing the subtree back) unless `is_empty()`.

- [ ] **Step 1: Write the failing tests** (append to the `tests` module)

```rust
/// take() must collapse exactly like close() and hand the leaf back.
#[test]
fn take_collapses_and_returns_the_leaf() {
    let (mut t, a) = Tree::new();
    let b = t.split(a, Orientation::LeftRight).unwrap();
    let sub = t.take(b).expect("b exists");
    assert_eq!(sub.panes(), vec![b]);
    assert_eq!(sub.first_pane(), Some(b));
    // The 1-child split collapsed: the tree is a lone leaf `a` again.
    assert_eq!(t.all_panes(), vec![a]);
    assert!(t.take(b).is_none(), "stale id degrades to None");
}

/// Taking the last pane empties the tree; adopt_root refills it.
#[test]
fn take_last_pane_then_adopt_root() {
    let (mut t, a) = Tree::new();
    let sub = t.take(a).unwrap();
    assert!(t.is_empty());
    assert!(t.adopt_root(sub).is_ok());
    assert_eq!(t.all_panes(), vec![a]);
    let (mut full, _) = Tree::new();
    let again = full.take(full.all_panes()[0]).unwrap();
    let refused = t.adopt_root(again).expect_err("adopt_root refuses a non-empty tree");
    assert_eq!(refused.panes().len(), 1, "the subtree is handed back, not dropped");
}

/// take_tab removes the whole page (a nested split) and collapses a
/// now-single-tab group into its lone page.
#[test]
fn take_tab_removes_the_whole_page_and_collapses() {
    let (mut t, a) = Tree::new();
    let b = t.new_tab(a).unwrap();               // tabs: [a, b], b active
    let c = t.split(b, Orientation::LeftRight).unwrap(); // page 2 = split(b, c)
    let sub = t.take_tab(b).expect("page's first leaf");
    assert_eq!(sub.panes(), vec![b, c], "the whole page travels");
    // One tab left → the Tabs wrapper unwraps to the bare leaf `a`.
    assert_eq!(t.all_panes(), vec![a]);
    assert!(t.tab_bars(Rect::new(0.0, 0.0, 800.0, 600.0)).is_empty(), "no strip for a single pane");
}

/// insert_beside splits the target 50/50 with the subtree on the asked side.
#[test]
fn insert_beside_places_before_or_after() {
    let (mut t, a) = Tree::new();
    let (mut src, x) = Tree::new();
    let sub = src.take(x).unwrap();
    t.insert_beside(a, sub, Orientation::LeftRight, true).unwrap();
    let rects = t.rects(Rect::new(0.0, 0.0, 806.0, 600.0)); // 800 + DIVIDER
    assert_eq!(rects.len(), 2);
    assert_eq!(rects[0].0, x, "before=true → subtree is the left child");
    assert_eq!(rects[1].0, a);
    assert!((rects[0].1.w - rects[1].1.w).abs() < 1.0, "50/50 split");
    // Stale target hands the subtree back instead of dropping panes.
    let (mut src2, y) = Tree::new();
    let sub2 = src2.take(y).unwrap();
    let returned = t.insert_beside(PaneId(u64::MAX - 5), sub2, Orientation::TopBottom, false)
        .expect_err("stale target");
    assert_eq!(returned.panes(), vec![y]);
}

/// insert_root_edge wraps the whole tree; before=true puts the newcomer first.
#[test]
fn insert_root_edge_wraps_the_root() {
    let (mut t, a) = Tree::new();
    let b = t.split(a, Orientation::LeftRight).unwrap();
    let (mut src, x) = Tree::new();
    t.insert_root_edge(src.take(x).unwrap(), Orientation::TopBottom, true);
    let rects = t.rects(Rect::new(0.0, 0.0, 800.0, 606.0));
    assert_eq!(rects[0].0, x, "newcomer is the top band");
    assert_eq!(rects.len(), 3);
    assert!(t.all_panes() == vec![x, a, b]);
}

/// insert_tab_at drops into an existing group at the index (and activates it),
/// and wraps a bare leaf into a fresh Tabs group when there is no strip yet.
#[test]
fn insert_tab_at_inserts_and_wraps() {
    // Existing group: [a, b]; insert x at index 1 → [a, x, b], x active.
    let (mut t, a) = Tree::new();
    let b = t.new_tab(a).unwrap();
    let (mut src, x) = Tree::new();
    t.insert_tab_at(a, src.take(x).unwrap(), 1).unwrap();
    let bounds = Rect::new(0.0, 0.0, 900.0, 600.0);
    let bar = &t.tab_bars(bounds)[0];
    let order: Vec<_> = bar.tabs.iter().map(|tb| tb.first_pane).collect();
    assert_eq!(order, vec![a, x, b]);
    assert!(bar.tabs[1].active, "inserted tab becomes active");
    // No strip: wrapping a lone leaf. index 0 puts the newcomer first.
    let (mut t2, p) = Tree::new();
    let (mut src2, q) = Tree::new();
    t2.insert_tab_at(p, src2.take(q).unwrap(), 0).unwrap();
    let bar2 = &t2.tab_bars(bounds)[0];
    let order2: Vec<_> = bar2.tabs.iter().map(|tb| tb.first_pane).collect();
    assert_eq!(order2, vec![q, p]);
}
```

- [ ] **Step 2: Run to verify they fail to compile** (missing methods)

Run: `cargo test -p rt-core`
Expected: compile errors naming `take`, `Subtree`, etc.

- [ ] **Step 3: Implement**

Add after the `Removal` enum:

```rust
/// A detached fragment of a layout tree — the payload of a pane/tab move.
/// Opaque on purpose: the tree's `Node` stays private, so the only way to make
/// one is `Tree::take`/`take_tab` and the only way to use one is the insert
/// ops, which keeps every pane accounted for.
#[derive(Debug)]
pub struct Subtree(Node);

impl Subtree {
    /// Every pane id inside this fragment, in traversal order.
    pub fn panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        Tree::collect_panes(&self.0, &mut out);
        out
    }
    /// The first leaf (used as the arriving focus / tab identity).
    pub fn first_pane(&self) -> Option<PaneId> {
        Tree::first_leaf(&self.0)
    }
}
```

Add the ops in `impl Tree` (near `close`):

```rust
/// Remove the pane `target` (collapsing like [`Tree::close`]) and return it
/// as a movable [`Subtree`]. `None` if the id is stale.
pub fn take(&mut self, target: PaneId) -> Option<Subtree> {
    if self.close(target) {
        Some(Subtree(Node::Leaf(target))) // a leaf payload IS just the leaf
    } else {
        None
    }
}

/// Remove the whole tab page whose FIRST leaf is `first_pane` (the id
/// `Tab::first_pane` carries) and return it. Collapses a 1-page group.
pub fn take_tab(&mut self, first_pane: PaneId) -> Option<Subtree> {
    let root = std::mem::replace(&mut self.root, Node::Leaf(PaneId(u64::MAX)));
    match Self::take_tab_from(root, first_pane) {
        (rest, Some(taken)) => {
            self.root = rest.unwrap_or(Node::Leaf(PaneId(u64::MAX)));
            Some(Subtree(taken))
        }
        (rest, None) => {
            self.root = rest.expect("nothing was removed, the node survives");
            None
        }
    }
}

/// Worker for take_tab: returns (what this node becomes, the removed page).
fn take_tab_from(node: Node, first: PaneId) -> (Option<Node>, Option<Node>) {
    match node {
        Node::Leaf(id) => (Some(Node::Leaf(id)), None),
        Node::Split { orient, mut children } => {
            for i in 0..children.len() {
                let child = std::mem::replace(&mut children[i].node, Node::Leaf(PaneId(u64::MAX)));
                let (rest, taken) = Self::take_tab_from(child, first);
                match rest {
                    Some(n) => children[i].node = n,
                    None => { children.remove(i); }
                }
                if taken.is_some() {
                    let rest = match children.len() {
                        0 => None,
                        1 => Some(children.pop().unwrap().node),
                        _ => Some(Node::Split { orient, children }),
                    };
                    return (rest, taken);
                }
                if !matches!(children.get(i), Some(_)) {
                    break; // a slot vanished without a take: impossible, bail
                }
            }
            (Some(Node::Split { orient, children }), None)
        }
        Node::Tabs { mut children, mut active } => {
            // Is one of OUR pages the wanted tab?
            if let Some(i) = children.iter().position(|c| Self::first_leaf(c) == Some(first)) {
                let taken = children.remove(i);
                if active >= i && active > 0 {
                    active -= 1;
                }
                let rest = match children.len() {
                    0 => None,
                    1 => Some(children.pop().unwrap()),
                    _ => {
                        if active >= children.len() { active = children.len() - 1; }
                        Some(Node::Tabs { children, active })
                    }
                };
                return (rest, Some(taken));
            }
            // Otherwise recurse into the pages (nested Tabs).
            for i in 0..children.len() {
                let child = std::mem::replace(&mut children[i], Node::Leaf(PaneId(u64::MAX)));
                let (rest, taken) = Self::take_tab_from(child, first);
                match rest {
                    Some(n) => children[i] = n,
                    None => {
                        children.remove(i);
                        if active >= i && active > 0 { active -= 1; }
                    }
                }
                if taken.is_some() {
                    let rest = match children.len() {
                        0 => None,
                        1 => Some(children.pop().unwrap()),
                        _ => {
                            if active >= children.len() { active = children.len() - 1; }
                            Some(Node::Tabs { children, active })
                        }
                    };
                    return (rest, taken);
                }
            }
            (Some(Node::Tabs { children, active }), None)
        }
    }
}

/// Split `target`, placing `sub` on the chosen side, 50/50 — the drag-and-drop
/// insert. On a stale target the subtree is handed back (never dropped).
pub fn insert_beside(
    &mut self,
    target: PaneId,
    sub: Subtree,
    orient: Orientation,
    before: bool,
) -> Result<(), Subtree> {
    fn walk(node: &mut Node, target: PaneId, incoming: &mut Option<Node>, orient: Orientation, before: bool) -> bool {
        match node {
            Node::Leaf(id) if *id == target => {
                let original = *id;
                let new = incoming.take().expect("incoming consumed once");
                let (first, second) = if before {
                    (new, Node::Leaf(original))
                } else {
                    (Node::Leaf(original), new)
                };
                *node = Node::Split {
                    orient,
                    children: vec![
                        Child { weight: 1.0, node: first },
                        Child { weight: 1.0, node: second },
                    ],
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { children, .. } => children
                .iter_mut()
                .any(|c| walk(&mut c.node, target, incoming, orient, before)),
            Node::Tabs { children, .. } => children
                .iter_mut()
                .any(|c| walk(c, target, incoming, orient, before)),
        }
    }
    let mut incoming = Some(sub.0);
    if walk(&mut self.root, target, &mut incoming, orient, before) {
        Ok(())
    } else {
        Err(Subtree(incoming.take().expect("unconsumed on miss")))
    }
}

/// Full-width/height split at the very top of the tree (the window-edge drop).
/// On an empty tree the subtree simply becomes the root.
pub fn insert_root_edge(&mut self, sub: Subtree, orient: Orientation, before: bool) {
    if self.is_empty() {
        self.root = sub.0;
        return;
    }
    let old = std::mem::replace(&mut self.root, Node::Leaf(PaneId(u64::MAX)));
    let (first, second) = if before { (sub.0, old) } else { (old, sub.0) };
    self.root = Node::Split {
        orient,
        children: vec![
            Child { weight: 1.0, node: first },
            Child { weight: 1.0, node: second },
        ],
    };
}

/// Insert `sub` as a tab at `index` in the group that has `anchor` as one of
/// its pages' first leaves; wraps a bare `anchor` leaf in a fresh group when
/// no strip exists (mirroring `new_tab`). The inserted tab becomes active.
pub fn insert_tab_at(&mut self, anchor: PaneId, sub: Subtree, index: usize) -> Result<(), Subtree> {
    fn walk(node: &mut Node, anchor: PaneId, incoming: &mut Option<Node>, index: usize) -> bool {
        match node {
            Node::Leaf(id) if *id == anchor => {
                // No strip yet: wrap the leaf, honouring index 0 vs 1+.
                let original = *id;
                let new = incoming.take().expect("consumed once");
                let (children, active) = if index == 0 {
                    (vec![new, Node::Leaf(original)], 0)
                } else {
                    (vec![Node::Leaf(original), new], 1)
                };
                *node = Node::Tabs { children, active };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { children, .. } => children
                .iter_mut()
                .any(|c| walk(&mut c.node, anchor, incoming, index)),
            Node::Tabs { children, active } => {
                if children.iter().any(|c| Node::has_first_leaf(c, anchor)) {
                    let i = index.min(children.len());
                    children.insert(i, incoming.take().expect("consumed once"));
                    *active = i; // reveal the arriving tab
                    return true;
                }
                children.iter_mut().any(|c| walk(c, anchor, incoming, index))
            }
        }
    }
    let mut incoming = Some(sub.0);
    if walk(&mut self.root, anchor, &mut incoming, index) {
        Ok(())
    } else {
        Err(Subtree(incoming.take().expect("unconsumed on miss")))
    }
}

/// Install `sub` as the root of an emptied tree (a tear-out landing in a fresh
/// window). Refuses — handing the subtree back — when the tree still has
/// content, so live panes can never be dropped by a mis-aimed adopt.
pub fn adopt_root(&mut self, sub: Subtree) -> Result<(), Subtree> {
    if !self.is_empty() {
        return Err(sub);
    }
    self.root = sub.0;
    Ok(())
}
```

And a tiny helper on `Node` (private, next to the enum):

```rust
impl Node {
    /// Whether this page's first leaf is `id` (tab identity test).
    fn has_first_leaf(node: &Node, id: PaneId) -> bool {
        Tree::first_leaf(node) == Some(id)
    }
}
```

(If the borrow checker fights the `Split` early-`break` in `take_tab_from`, restructure that loop as an index loop that `return`s from inside — the test suite is the arbiter, not the exact shape shown here. The behaviour contract is the tests.)

- [ ] **Step 4: Run the tests**

Run: `cargo test -p rt-core`
Expected: all PASS, including the pre-existing rotate/close tests.

- [ ] **Step 5: Commit**

```bash
git add crates/rt-core/src/layout.rs && git commit -m "feat(rt-core): Subtree take/insert ops for pane drag-and-drop"
```

---

### Task 3: Swap, replace_leaf, reorder_tab (rt-core)

**Files:**
- Modify: `crates/rt-core/src/layout.rs`

**Interfaces:**
- Produces:
  - `pub fn replace_leaf(&mut self, from: PaneId, to: PaneId) -> bool` — rewrite one leaf's id in place (cross-window swap building block).
  - `pub fn swap(&mut self, a: PaneId, b: PaneId) -> bool` — exchange two leaves in this tree (centre-drop).
  - `pub fn reorder_tab(&mut self, first_pane: PaneId, to: usize) -> bool` — move the tab whose first leaf is `first_pane` to index `to` in its group; the moved tab stays/becomes active.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn swap_exchanges_two_leaves_in_place() {
    let (mut t, a) = Tree::new();
    let b = t.split(a, Orientation::LeftRight).unwrap();
    let c = t.split(b, Orientation::TopBottom).unwrap();
    assert!(t.swap(a, c));
    let bounds = Rect::new(0.0, 0.0, 806.0, 606.0);
    let rects = t.rects(bounds);
    assert_eq!(rects[0].0, c, "c took a's slot (left)");
    assert_eq!(rects.iter().map(|(id, _)| *id).collect::<Vec<_>>(), vec![c, b, a]);
    assert!(!t.swap(a, a), "self-swap is a no-op");
    assert!(!t.swap(a, PaneId(9999)), "stale id is a no-op");
}

#[test]
fn replace_leaf_rewrites_one_id() {
    let (mut t, a) = Tree::new();
    let b = t.split(a, Orientation::LeftRight).unwrap();
    assert!(t.replace_leaf(a, PaneId(4242)));
    assert_eq!(t.all_panes(), vec![PaneId(4242), b]);
    assert!(!t.replace_leaf(a, PaneId(1)), "old id is gone");
}

#[test]
fn reorder_tab_moves_and_follows_focus() {
    let (mut t, a) = Tree::new();
    let b = t.new_tab(a).unwrap();
    let c = t.new_tab(b).unwrap(); // tabs [a, b, c], c active
    assert!(t.reorder_tab(c, 0));
    let bounds = Rect::new(0.0, 0.0, 900.0, 600.0);
    let bar = &t.tab_bars(bounds)[0];
    let order: Vec<_> = bar.tabs.iter().map(|tb| tb.first_pane).collect();
    assert_eq!(order, vec![c, a, b]);
    assert!(bar.tabs[0].active, "the moved tab stays the active one");
    assert!(t.reorder_tab(a, 99), "index clamps to the end");
    let order2: Vec<_> = t.tab_bars(bounds)[0].tabs.iter().map(|tb| tb.first_pane).collect();
    assert_eq!(order2, vec![c, b, a]);
    assert!(!t.reorder_tab(PaneId(9999), 0), "stale id is a no-op");
}
```

- [ ] **Step 2: Run to verify they fail to compile**

Run: `cargo test -p rt-core`

- [ ] **Step 3: Implement**

```rust
/// Rewrite the leaf `from` to carry id `to`. The building block for
/// cross-window swap (each tree rewrites one leaf; the sessions then exchange
/// the panes' side-table entries). Returns false if `from` isn't a leaf here.
pub fn replace_leaf(&mut self, from: PaneId, to: PaneId) -> bool {
    fn walk(node: &mut Node, from: PaneId, to: PaneId) -> bool {
        match node {
            Node::Leaf(id) if *id == from => { *id = to; true }
            Node::Leaf(_) => false,
            Node::Split { children, .. } => children.iter_mut().any(|c| walk(&mut c.node, from, to)),
            Node::Tabs { children, .. } => children.iter_mut().any(|c| walk(c, from, to)),
        }
    }
    walk(&mut self.root, from, to)
}

/// Exchange two leaves of THIS tree (the centre-drop "swap panes" gesture).
/// Pure id rewrites — weights, splits and tab structure stay put.
pub fn swap(&mut self, a: PaneId, b: PaneId) -> bool {
    // The placeholder id is reserved: the global mint never reaches u64::MAX-1.
    const TMP: PaneId = PaneId(u64::MAX - 1);
    if a == b || !Self::contains(&self.root, a) || !Self::contains(&self.root, b) {
        return false;
    }
    self.replace_leaf(a, TMP) && self.replace_leaf(b, a) && self.replace_leaf(TMP, b)
}

/// Move the tab whose first leaf is `first_pane` to index `to` (clamped) in
/// its own group; the moved tab remains the active one.
pub fn reorder_tab(&mut self, first_pane: PaneId, to: usize) -> bool {
    fn walk(node: &mut Node, first: PaneId, to: usize) -> bool {
        match node {
            Node::Leaf(_) => false,
            Node::Split { children, .. } => children.iter_mut().any(|c| walk(&mut c.node, first, to)),
            Node::Tabs { children, active } => {
                if let Some(i) = children.iter().position(|c| Tree::first_leaf(c) == Some(first)) {
                    let to = to.min(children.len() - 1);
                    let page = children.remove(i);
                    children.insert(to, page);
                    *active = to; // focus follows the moved tab
                    return true;
                }
                children.iter_mut().any(|c| walk(c, first, to))
            }
        }
    }
    walk(&mut self.root, first_pane, to)
}
```

- [ ] **Step 4: Run the tests**

Run: `cargo test -p rt-core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/rt-core/src/layout.rs && git commit -m "feat(rt-core): swap, replace_leaf and reorder_tab"
```

---

### Task 4: PanePackage extract/adopt (rt-session)

**Files:**
- Modify: `crates/rt-session/src/lib.rs`

**Interfaces:**
- Consumes: Task 2/3's `Subtree` + tree ops.
- Produces (all in `rt_session`):

```rust
pub struct PaneEntries<B> {
    pub panes: Vec<(PaneId, B)>,
    pub groups: Vec<(PaneId, u32)>,
    pub columns: Vec<(PaneId, u16)>,
    pub titles: Vec<(PaneId, String)>,
}
pub struct PanePackage<B> {
    pub sub: rt_core::Subtree,
    pub entries: PaneEntries<B>,
}
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DropTarget {
    Root,                                                        // empty window (tear-out landing)
    SplitBeside { pane: PaneId, orient: Orientation, before: bool },
    Swap { pane: PaneId },                                       // move_pane only, same window
    TabAt { anchor: PaneId, index: usize },
    RootEdge { orient: Orientation, before: bool },
}
impl<B: Backend, F: …> Session<B, F> {
    pub fn is_empty(&self) -> bool;
    pub fn extract_pane(&mut self, id: PaneId) -> Option<PanePackage<B>>;
    pub fn extract_tab(&mut self, first_pane: PaneId) -> Option<PanePackage<B>>;
    pub fn adopt(&mut self, pkg: PanePackage<B>, at: DropTarget) -> Result<(), PanePackage<B>>;
    pub fn move_pane(&mut self, id: PaneId, at: DropTarget) -> bool;   // same-window commit
    pub fn move_tab(&mut self, first_pane: PaneId, at: DropTarget) -> bool;
    pub fn reorder_tab(&mut self, first_pane: PaneId, to: usize) -> bool;
    pub fn eject_entries(&mut self, ids: &[PaneId]) -> PaneEntries<B>; // pub: cross-window swap uses it
    pub fn inject_entries(&mut self, e: PaneEntries<B>);
}
```

- [ ] **Step 1: Write the failing tests** (in the existing `tests` module; reuse `MockPane` — give it `bracketed: false` and a fresh `Rc<RefCell<Vec<u8>>>` via a small `fn mock_session() -> Session<MockPane, impl FnMut(PaneId, usize, usize) -> Option<MockPane>>` helper that spawns MockPanes)

```rust
fn mock_session() -> Session<MockPane, impl FnMut(PaneId, usize, usize) -> Option<MockPane>> {
    Session::new(
        Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 },
        (8.0, 16.0),
        |_id, _c, _r| Some(MockPane { writes: std::rc::Rc::new(std::cell::RefCell::new(Vec::new())), bracketed: false }),
    )
}

/// extract_pane carries the backend AND every side-table entry; adopt puts
/// them all back and focuses the arrival.
#[test]
fn extract_then_adopt_moves_everything() {
    let mut src = mock_session();
    let a = src.focus();
    src.apply(Action::SplitVert);
    let b = src.focus();
    src.set_title(b, "worker".into());
    src.set_group(2); // focus (b) joins group 2
    src.apply(Action::ColumnsMore); // b gets 2 columns

    let pkg = src.extract_pane(b).expect("b exists");
    assert_eq!(pkg.sub.panes(), vec![b]);
    assert_eq!(pkg.entries.panes.len(), 1);
    assert_eq!(pkg.entries.titles, vec![(b, "worker".to_string())]);
    assert_eq!(pkg.entries.groups, vec![(b, 2)]);
    assert_eq!(pkg.entries.columns, vec![(b, 2)]);
    // Source: b is gone everywhere, focus re-seated on a survivor.
    assert!(src.pane(b).is_none());
    assert_eq!(src.focus(), a);
    assert_eq!(src.title_of(b), None);

    let mut dst = mock_session();
    let d = dst.focus();
    dst.adopt(pkg, DropTarget::SplitBeside { pane: d, orient: Orientation::LeftRight, before: false })
        .unwrap();
    assert!(dst.pane(b).is_some(), "the backend arrived");
    assert_eq!(dst.focus(), b, "focus lands on the arrival");
    assert_eq!(dst.title_of(b), Some("worker"));
    assert_eq!(dst.group_of(b), Some(2));
    assert_eq!(dst.columns_of(b), 2);
}

/// Extracting the last pane leaves an empty session (the App closes it);
/// adopting at Root refills an empty one (the tear-out landing).
#[test]
fn extract_last_pane_empties_adopt_root_refills() {
    let mut s = mock_session();
    let a = s.focus();
    let pkg = s.extract_pane(a).expect("only pane");
    assert!(s.is_empty());
    let mut w = mock_session();
    let first = w.focus();
    let seed = w.extract_pane(first).unwrap(); // empty the new window
    drop(seed);
    assert!(w.is_empty());
    w.adopt(pkg, DropTarget::Root).unwrap();
    assert!(!w.is_empty());
    assert_eq!(w.focus(), a);
}

/// A failed adopt (stale target) hands the package back — panes never vanish.
#[test]
fn failed_adopt_returns_the_package() {
    let mut src = mock_session();
    src.apply(Action::SplitVert);
    let b = src.focus();
    let pkg = src.extract_pane(b).unwrap();
    let mut dst = mock_session();
    let pkg = dst
        .adopt(pkg, DropTarget::SplitBeside { pane: PaneId(u64::MAX - 7), orient: Orientation::LeftRight, before: true })
        .expect_err("stale target");
    assert_eq!(pkg.sub.panes(), vec![b], "package intact for retry/cancel");
    assert!(dst.pane(b).is_none());
}

/// move_pane: Swap keeps both panes alive and exchanges their slots;
/// SplitBeside re-homes a pane within the same window.
#[test]
fn move_pane_swap_and_split_within_one_window() {
    let mut s = mock_session();
    let a = s.focus();
    s.apply(Action::SplitVert);
    let b = s.focus();
    assert!(s.move_pane(a, DropTarget::Swap { pane: b }));
    let bounds = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
    let rects = s.visible_rects(bounds);
    assert_eq!(rects[0].0, b, "b now sits left");
    assert!(s.move_pane(b, DropTarget::SplitBeside { pane: a, orient: Orientation::TopBottom, before: true }));
    assert!(s.pane(a).is_some() && s.pane(b).is_some(), "nothing lost");
    assert!(!s.move_pane(a, DropTarget::Swap { pane: a }), "self-target no-op");
}

/// Zoom never travels: extracting a zoomed pane clears zoom on the source,
/// and adopting into a zoomed session unzooms it (the layout must be visible).
#[test]
fn zoom_is_cleared_on_both_sides() {
    let mut src = mock_session();
    src.apply(Action::SplitVert);
    let b = src.focus();
    src.apply(Action::ToggleZoom);
    assert!(src.is_zoomed());
    let pkg = src.extract_pane(b).unwrap();
    assert!(!src.is_zoomed(), "extracting the zoomed pane unzooms the source");
    let mut dst = mock_session();
    dst.apply(Action::ToggleZoom);
    let d = dst.focus();
    dst.adopt(pkg, DropTarget::SplitBeside { pane: d, orient: Orientation::LeftRight, before: false }).unwrap();
    assert!(!dst.is_zoomed(), "adopting unzooms the target");
}
```

- [ ] **Step 2: Run to verify compile failure**

Run: `cargo test -p rt-session`

- [ ] **Step 3: Implement**

Key implementation notes (write real code, this is the contract):

```rust
pub fn is_empty(&self) -> bool {
    self.tree.is_empty()
}

pub fn eject_entries(&mut self, ids: &[PaneId]) -> PaneEntries<B> {
    let mut e = PaneEntries { panes: Vec::new(), groups: Vec::new(), columns: Vec::new(), titles: Vec::new() };
    for &id in ids {
        if let Some(b) = self.panes.remove(&id) { e.panes.push((id, b)); }
        if let Some(g) = self.groups.remove(&id) { e.groups.push((id, g)); }
        if let Some(c) = self.columns.remove(&id) { e.columns.push((id, c)); }
        if let Some(t) = self.titles.remove(&id) { e.titles.push((id, t)); }
        if self.zoomed == Some(id) { self.zoomed = None; }
    }
    e
}

pub fn inject_entries(&mut self, e: PaneEntries<B>) {
    for (id, b) in e.panes { self.panes.insert(id, b); }
    for (id, g) in e.groups { self.groups.insert(id, g); }
    for (id, c) in e.columns { self.columns.insert(id, c); }
    for (id, t) in e.titles { self.titles.insert(id, t); }
}

fn finish_extract(&mut self, sub: rt_core::Subtree) -> PanePackage<B> {
    let ids = sub.panes();
    let entries = self.eject_entries(&ids);
    if ids.contains(&self.focus) {
        if let Some((id, _)) = self.tree.rects(self.bounds).into_iter().next() {
            self.focus = id;
        }
    }
    self.relayout(self.bounds); // survivors grew
    PanePackage { sub, entries }
}

pub fn extract_pane(&mut self, id: PaneId) -> Option<PanePackage<B>> {
    let sub = self.tree.take(id)?;
    Some(self.finish_extract(sub))
}

pub fn extract_tab(&mut self, first_pane: PaneId) -> Option<PanePackage<B>> {
    let sub = self.tree.take_tab(first_pane)?;
    Some(self.finish_extract(sub))
}

pub fn adopt(&mut self, pkg: PanePackage<B>, at: DropTarget) -> Result<(), PanePackage<B>> {
    let PanePackage { sub, entries } = pkg;
    let arriving = sub.first_pane();
    let placed = match at {
        DropTarget::Root => self.tree.adopt_root(sub),
        DropTarget::SplitBeside { pane, orient, before } => self.tree.insert_beside(pane, sub, orient, before),
        DropTarget::TabAt { anchor, index } => self.tree.insert_tab_at(anchor, sub, index),
        DropTarget::RootEdge { orient, before } => { self.tree.insert_root_edge(sub, orient, before); Ok(()) }
        DropTarget::Swap { .. } => Err(sub), // swap is not an adopt — refuse
    };
    match placed {
        Ok(()) => {
            self.inject_entries(entries);
            self.zoomed = None; // the new layout must be visible
            if let Some(f) = arriving { self.focus = f; }
            self.relayout(self.bounds);
            Ok(())
        }
        Err(sub) => Err(PanePackage { sub, entries }),
    }
}
```

```rust
pub fn move_pane(&mut self, id: PaneId, at: DropTarget) -> bool {
    match at {
        DropTarget::Swap { pane } => {
            if self.tree.swap(id, pane) {
                self.relayout(self.bounds);
                true
            } else { false }
        }
        DropTarget::SplitBeside { pane, .. } if pane == id => false, // dropping on yourself
        _ => {
            let Some(pkg) = self.extract_pane(id) else { return false };
            match self.adopt(pkg, at) {
                Ok(()) => true,
                Err(pkg) => {
                    // Put it back where the tree will take it: as a root edge (never lose a pane).
                    let PanePackage { sub, entries } = pkg;
                    self.tree.insert_root_edge(sub, Orientation::LeftRight, false);
                    self.inject_entries(entries);
                    self.relayout(self.bounds);
                    false
                }
            }
        }
    }
}

pub fn move_tab(&mut self, first_pane: PaneId, at: DropTarget) -> bool {
    // Same shape as move_pane's extract path, via extract_tab. A tab payload
    // never Swaps (the resolver never offers it); refuse defensively.
    if matches!(at, DropTarget::Swap { .. }) { return false; }
    let Some(pkg) = self.extract_tab(first_pane) else { return false };
    match self.adopt(pkg, at) {
        Ok(()) => true,
        Err(pkg) => {
            let PanePackage { sub, entries } = pkg;
            self.tree.insert_root_edge(sub, Orientation::LeftRight, false);
            self.inject_entries(entries);
            self.relayout(self.bounds);
            false
        }
    }
}

pub fn reorder_tab(&mut self, first_pane: PaneId, to: usize) -> bool {
    if self.tree.reorder_tab(first_pane, to) {
        self.focus = first_pane; // follow the moved tab (it is active now)
        self.relayout(self.bounds);
        true
    } else { false }
}
```

Guard inside `move_pane`/`move_tab` before extracting: if the drop target names a pane that is inside the payload (`SplitBeside`/`TabAt` anchor == a payload pane), return `false` up front — the resolver excludes this, the session must too.

- [ ] **Step 4: Run the tests**

Run: `cargo test -p rt-session && cargo test -p rt-core`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/rt-session/src/lib.rs crates/rt-core/src/layout.rs && git commit -m "feat(rt-session): PanePackage extract/adopt + DropTarget commits"
```

---

### Task 5: New actions, chords, menu rows, manual lines

**Files:**
- Modify: `crates/rt-config/src/lib.rs` (Action enum :23-104, defaults :438-497)
- Modify: `crates/rt/src/menu.rs` (items() :24-49)
- Modify: `crates/rt/src/manual.rs` (the `MANUAL` string)
- Modify: `crates/rt-session/src/lib.rs` (the catch-all arm in `apply` :484-499)

**Interfaces:**
- Produces: `Action::NewWindow`, `Action::DetachPane`, `Action::DetachTab`, `Action::MoveTabLeft`, `Action::MoveTabRight`. Defaults: `<Shift><Control>i` NewWindow (Terminator's new_window), `<Shift><Control>d` DetachPane, `<Shift><Control>j` DetachTab, `<Shift><Control>Page_Up` MoveTabLeft, `<Shift><Control>Page_Down` MoveTabRight (Terminator's move_tab). All five are GUI-level: `Session::apply` returns `None` for them (they join the `OpacityUp | …` passthrough arm). Behaviour lands in Tasks 6–8.

- [ ] **Step 1: Write the failing test** (rt-config tests module)

```rust
#[test]
fn window_and_tab_move_actions_have_default_chords() {
    let km = Keymap::default();
    let expect = [
        ("<Shift><Control>i", Action::NewWindow),
        ("<Shift><Control>d", Action::DetachPane),
        ("<Shift><Control>j", Action::DetachTab),
        ("<Shift><Control>Page_Up", Action::MoveTabLeft),
        ("<Shift><Control>Page_Down", Action::MoveTabRight),
    ];
    for (accel, action) in expect {
        let chord = keys::Chord::parse(accel).expect("valid chord");
        assert_eq!(km.action_for(&chord), Some(action), "{accel}");
    }
}
```

- [ ] **Step 2: Run to verify it fails to compile** (`cargo test -p rt-config`)

- [ ] **Step 3: Implement**

- Add the five variants to `Action` with doc comments in the existing style (`/// rt-specific: open a new empty rt window (same process).` etc.).
- Append the five `(accel, action)` pairs to `Keymap::defaults()`.
- rt-session `apply`: add `Action::NewWindow | Action::DetachPane | Action::DetachTab | Action::MoveTabLeft | Action::MoveTabRight` to the GUI-passthrough arm (returns `None`).
- menu.rs `items()`: after the `New Tab` row add
  `Item::Action("New Window", Action::NewWindow),`
  and after `Maximise / Restore Pane`:
  `Item::Action("Detach Pane to New Window", Action::DetachPane),`
  `Item::Action("Detach Tab to New Window", Action::DetachTab),`
- manual.rs: add lines to `MANUAL` in its existing key-listing style for all five chords (read the surrounding format first; the unit test `every_default_keybinding_is_documented` defines done). Describe: New Window; Detach pane → own window; Detach tab → own window; Move tab left/right. Also add a short "Drag & drop" paragraph stub naming titlebar-drag and tab-drag (Tasks 8–12 make it true; keep wording to what will exist by the end of this branch).

- [ ] **Step 4: Run the tests**

Run: `cargo test -p rt-config && cargo test -p rt 2>/dev/null || cargo test --workspace`
Expected: PASS, including the manual-coverage test.

- [ ] **Step 5: Commit**

```bash
git add crates/rt-config/src/lib.rs crates/rt-session/src/lib.rs crates/rt/src/menu.rs crates/rt/src/manual.rs
git commit -m "feat(rt-config): NewWindow/Detach/MoveTab actions with chords, menu and manual entries"
```

---

### Task 6: Multi-window App (the big refactor)

**Files:**
- Modify: `crates/rt/src/main.rs` — `App` (:550-555), `resumed` (:699-1209), `window_event` (:1212-2231), `about_to_wait` (:2236+), `apply_action` (:2634-2766), `redraw` (:3468), `exit_clean` call sites (:1684, :2363, :2749)

**Interfaces:**
- Produces:
  - `struct App { …, windows: std::collections::HashMap<WindowId, Active> }` (replaces `active: Option<Active>`).
  - `fn build_active(&mut self, event_loop: &ActiveEventLoop) -> Option<Active>` — the whole body of today's `resumed` from settings-load through the `Active { … }` literal, minus the `RT_*` demo hooks (those stay in `resumed`, first window only). CLI `--cols/--rows` sizing applies only when `self.windows.is_empty()`.
  - `fn close_window(&mut self, id: WindowId)` — remove + drop that `Active`; `exit_clean()` when the map empties. **Do not drop the last `Active`** — on the last window call `exit_clean()` while it is still in the map (today's teardown-fault avoidance, see the comment at :1676-1684: process::exit skips the faulting GL/Wayland Drop order).
  - `fn redraw(&mut self, id: WindowId)`.
  - `apply_action` return type: `fn apply_action(active: &mut Active, action: Action) -> WindowCmd` with

```rust
/// What an action needs the App (window-owner) level to do afterwards.
/// Active-level code can't create or close OS windows — it has no event loop.
#[derive(Clone, Copy, PartialEq)]
enum WindowCmd {
    None,
    CloseWindow,          // this window should close (last pane gone / close_window action)
    NewWindow,            // open an empty extra window
    DetachPane,           // tear the focused pane out to a new window
    DetachTab,            // tear the current tab out to a new window
}
```

- [ ] **Step 1: Restructure App + resumed**

- `App { font_db, mono_families, cli, windows: HashMap<WindowId, Active> }`.
- Move the construction body into `build_active` ending with `Some(active)`; `resumed` becomes: if `!self.windows.is_empty() { return; }` → `let Some(active) = self.build_active(event_loop) else { return };` → run the `RT_*` startup hooks against it → `let id = active.window.id(); self.windows.insert(id, active);` → `event_loop.set_control_flow(ControlFlow::Poll);` → `request_redraw`.
- The `jacks_dir`/`sweep` logic is already process-scoped (`rt-<pid>`); every `build_active` call reuses `jacks_dir_for(std::process::id())` and `ensure_jacks_dir` (idempotent). Each `Active` keeps its own `SharedJacks` map + spawn closure, as today.

- [ ] **Step 2: Route by WindowId**

- `window_event(&mut self, event_loop: &ActiveEventLoop, id: WindowId, event: WindowEvent)`: replace `let Some(active) = self.active.as_mut()` with `let Some(active) = self.windows.get_mut(&id) else { return };` (rename `_event_loop`/`_id` to used names).
- `WindowEvent::CloseRequested => { self.close_window(id); return; }` (replaces `exit_clean()`; `close_window` itself calls `exit_clean` for the last one).
- `WindowEvent::RedrawRequested => self.redraw(id)`; change `redraw` to take `id` and look up `self.windows.get_mut(&id)`.
- `on_key_press(&mut self, event_loop: &ActiveEventLoop, id: WindowId, key_event: KeyEvent)` — thread the two new params from `window_event`'s `KeyboardInput` arm.
- Everywhere inside `window_event`/helpers that referenced `self.active` (grep: `grep -n "self.active" crates/rt/src/main.rs`) becomes the looked-up `active` or a `self.windows.get_mut(&id)`.

- [ ] **Step 3: about_to_wait over all windows**

Wrap the existing per-active body in:

```rust
fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
    let mut to_close: Vec<WindowId> = Vec::new();
    for (&wid, active) in self.windows.iter_mut() {
        // …existing body, with two changes:
        // (a) the exited-pane CloseWindow arm (:2361-2364) becomes
        //     `to_close.push(wid);` + `break` out of the exited loop —
        //     never `self.active = None; exit_clean()`.
        // (b) `event_loop` uses (control flow/poll timing) move OUT of the
        //     loop if they were per-app, stay if per-window state.
    }
    for wid in to_close {
        self.close_window(wid);
    }
    // …existing tail (control-flow/poll decision), now computed over all
    // windows (fastest wanted wake wins).
}
```

- [ ] **Step 4: apply_action returns WindowCmd**

- Change the two `exit_clean()` / `SessionEvent::CloseWindow` sites inside `apply_action` to `return WindowCmd::CloseWindow;`; all other arms `WindowCmd::None`. Add arms: `Action::NewWindow => WindowCmd::NewWindow`, `Action::DetachPane => WindowCmd::DetachPane`, `Action::DetachTab => WindowCmd::DetachTab`, `Action::MoveTabLeft/Right` → call `active.session` tab reorder (Task 4's `Session::reorder_tab` needs the tab's first_pane + current index: compute via `active.session.tab_bars(bounds)` — find the bar containing the focused pane's tab, its index, then `reorder_tab(first_pane, index ± 1)`), `force_full`, redraw, `WindowCmd::None`.
- Callers (`on_key_press` binding hit :3393-3397, the menu-click handler — grep `into_pick`): capture the returned `WindowCmd` and hand it to a new `fn run_window_cmd(&mut self, event_loop: &ActiveEventLoop, id: WindowId, cmd: WindowCmd)`; in this task `NewWindow` opens `build_active` + insert (detach arms are `WindowCmd::None`-equivalent stubs until Task 7 — leave a `// Task 7` marker and make them no-ops that log).

- [ ] **Step 5: Build + full test suite**

Run: `cargo build --workspace && cargo test --workspace`
Expected: compiles, all tests pass.

- [ ] **Step 6: Manual verification (behaviour must be unchanged + NewWindow works)**

Run: `cargo run -p rt` — then: split, tabs, zoom, menu, prefs, close panes until exit. Then `Ctrl+Shift+I` → second window appears; type in both; close window 1 via titlebar — window 2 must survive; close window 2 → process exits (check `pgrep -x rt` empty; run from a second terminal).
On Wayland AND under `WINIT_UNIX_BACKEND=x11`: closing a NON-last window must not crash (this exercises the Active Drop path the old code never ran — see :1676-1684). If it segfaults, fix by dropping fields in safe order (reorder `Active` fields: `backend` before `window`) — if still faulty, document and `std::mem::forget` the backend Box on non-last close with a `// teardown-fault workaround` comment, and file it in docs/KNOWN_ISSUES.md.

- [ ] **Step 7: Commit**

```bash
git add crates/rt/src/main.rs && git commit -m "feat(rt): multi-window App — WindowId routing, per-window close, NewWindow"
```

---

### Task 7: Keyboard tear-out (DetachPane / DetachTab)

**Files:**
- Modify: `crates/rt/src/main.rs` (`run_window_cmd` from Task 6)

**Interfaces:**
- Consumes: `Session::{extract_pane, extract_tab, adopt, is_empty}` (Task 4), `build_active` (Task 6).
- Produces: `fn detach(&mut self, event_loop: &ActiveEventLoop, id: WindowId, tab: bool)` — used again by drag tear-out in Task 11.

- [ ] **Step 1: Implement `detach`**

```rust
/// Tear the focused pane (or its whole tab) out of window `id` into a fresh
/// OS window. The package moves in memory: PTY, scrollback, title, group all
/// survive. No-op when it would just recreate the same window (lone pane).
fn detach(&mut self, event_loop: &ActiveEventLoop, id: WindowId, tab: bool) {
    let Some(active) = self.windows.get_mut(&id) else { return };
    let focus = active.session.focus();
    // A lone pane in a lone tab: tearing out = the same window. No-op.
    let bounds = content_bounds(active.window.inner_size());
    if active.session.tree().all_panes().len() <= 1 {
        return;
    }
    let pkg = if tab {
        // The focused pane's tab: find its strip entry (first_pane identity).
        let first = active
            .session
            .tab_bars(bounds)
            .iter()
            .flat_map(|b| b.tabs.iter())
            .find(|t| t.active) // the visible tab of the focused strip
            .map(|t| t.first_pane);
        let Some(first) = first else { return }; // no tab strip → nothing to detach
        active.session.extract_tab(first)
    } else {
        active.session.extract_pane(focus)
    };
    let Some(pkg) = pkg else { return };
    // Move the panes' patch-bay jacks with them; cut wires that would cross.
    let moved: Vec<rt_core::PaneId> = pkg.sub.panes();
    let mut moved_jacks = Vec::new();
    for pid in &moved {
        if let Some(j) = self.windows.get_mut(&id).unwrap().jacks.borrow_mut().remove(pid) {
            moved_jacks.push((*pid, j));
        }
    }
    self.windows.get_mut(&id).unwrap().wires
        .retain(|w| !(moved.contains(&w.src) ^ moved.contains(&w.dst)));
    // …the retained set with BOTH ends moved must travel too:
    let (travelling, staying): (Vec<Wire>, Vec<Wire>) = self
        .windows.get_mut(&id).unwrap().wires.drain(..)
        .partition(|w| moved.contains(&w.src) && moved.contains(&w.dst));
    self.windows.get_mut(&id).unwrap().wires = staying;
    let src_became_empty = self.windows.get(&id).map(|a| a.session.is_empty()).unwrap_or(false);

    let Some(mut new_active) = self.build_active(event_loop) else {
        // Window creation failed: put the package back beside the focus.
        if let Some(a) = self.windows.get_mut(&id) {
            let _ = a.session.adopt(pkg, rt_session::DropTarget::RootEdge {
                orient: rt_core::Orientation::LeftRight, before: false });
            for (pid, j) in moved_jacks { a.jacks.borrow_mut().insert(pid, j); }
            a.wires.extend(travelling);
        }
        return;
    };
    // The fresh window spawned one pane of its own; drop it before adopting.
    let seed = new_active.session.focus();
    if let Some(seed_pkg) = new_active.session.extract_pane(seed) {
        drop(seed_pkg); // its shell gets SIGHUP via Drop, as a closed pane does
    }
    if new_active.session.adopt(pkg, rt_session::DropTarget::Root).is_err() {
        log::error!("detach: adopt into the new window failed"); // cannot happen: tree is empty
        return;
    }
    for (pid, j) in moved_jacks { new_active.jacks.borrow_mut().insert(pid, j); }
    new_active.wires = travelling;
    new_active.force_full = true;
    new_active.window.request_redraw();
    let new_id = new_active.window.id();
    self.windows.insert(new_id, new_active);
    if src_became_empty {
        self.close_window(id); // moved the last pane away → the shell is gone
    } else if let Some(a) = self.windows.get_mut(&id) {
        a.force_full = true;
        a.window.request_redraw();
    }
}
```

Wire `WindowCmd::DetachPane => self.detach(event_loop, id, false)`, `DetachTab => …, true)` in `run_window_cmd`. (The repeated `self.windows.get_mut(&id).unwrap()` chains above are for exposition — hoist into one mutable borrow block per phase when writing it.)

**Simplification note:** a cleaner seeding approach is a `build_active_empty` variant whose `Session` is built via `Session::new` and immediately emptied — but `Session::new` insists on spawning pane 0 (`:166-190`). Extracting-and-dropping the seed pane (shown above) avoids touching that contract. Do NOT add a spawnless constructor to rt-session in this task.

- [ ] **Step 2: Build + tests**

Run: `cargo build --workspace && cargo test --workspace` — PASS.

- [ ] **Step 3: Manual verification**

`cargo run -p rt`, split twice, run `top` in one pane, `Ctrl+Shift+D` on it → new window contains the running `top` with its scrollback; source window relayouts. `RT_TABS=3 cargo run -p rt`, `Ctrl+Shift+J` → whole tab (with its splits) moves out. Close the mother window → detached windows keep running. Detach the last pane of a 1-pane window → nothing happens. Verify on Wayland and `WINIT_UNIX_BACKEND=x11`.

- [ ] **Step 4: Commit**

```bash
git add crates/rt/src/main.rs && git commit -m "feat(rt): keyboard tear-out — DetachPane/DetachTab to a new window"
```

---

### Task 8: Drop-target resolver (pure) — `dragdrop.rs`

**Files:**
- Create: `crates/rt/src/dragdrop.rs` (+ `mod dragdrop;` in main.rs)

**Interfaces:**
- Consumes: `rt_session::DropTarget`, `rt_core::{PaneId, Rect, TabBar}`.
- Produces:

```rust
/// What is being dragged.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DragPayload {
    Pane(rt_core::PaneId),
    Tab { first_pane: rt_core::PaneId },
}

/// A resolved hover: where a release would put the payload, plus the cue
/// rectangle the renderer highlights.
#[derive(Clone, Debug, PartialEq)]
pub struct ResolvedDrop {
    pub target: rt_session::DropTarget,
    pub cue: rt_core::Rect,   // the highlight (half-pane / whole pane / edge band)
    pub caret: bool,          // true → draw as a thin insert caret, not a fill
}

/// Pixel width of the window-edge root-split strips.
pub const EDGE_STRIP: f32 = 24.0;
/// Fraction of a pane's width/height forming the centre (swap) box.
pub const CENTRE_FRAC: f32 = 0.4;
/// Movement (px) beyond which an armed press becomes a drag.
pub const DRAG_THRESHOLD: f32 = 4.0;

pub fn resolve_drop(
    payload: DragPayload,
    payload_panes: &[rt_core::PaneId], // every pane inside the payload (self-drop exclusion)
    panes: &[(rt_core::PaneId, rt_core::Rect)], // target window's visible_rects
    tab_bars: &[rt_core::TabBar],               // target window's tab_bars
    bounds: rt_core::Rect,                       // target window's content bounds
    cursor: (f32, f32),
) -> Option<ResolvedDrop>;
```

Resolution priority: **tab strip → window-edge strip → pane zones**, then `None` (gutters).

- Tab strip: cursor inside any `Tab::rect`'s vertical band (`y..y+h` of the bar) → insertion index = count of tabs whose horizontal midpoint is left of the cursor. Target `TabAt { anchor: bar.tabs[0].first_pane, index }`; cue = 3px-wide caret Rect at the insertion x, `caret: true`. A `Tab` payload over its OWN strip (its `first_pane` is one of the bar's tabs) still resolves to `TabAt` (that's the reorder). A `Pane` payload dropping on the strip wraps as a new tab (same `TabAt`).
- Window edges: within `EDGE_STRIP` of a `bounds` edge → `RootEdge { orient, before }`: top = `TopBottom, before=true`, bottom = `TopBottom, before=false`, left = `LeftRight, before=true`, right = `LeftRight, before=false`; cue = the half of `bounds` the newcomer would take (e.g. top half for the top edge), `caret: false`.
- Pane zones: find the pane whose rect contains the cursor; if that pane is in `payload_panes` → `None`. Inside the centre box (`CENTRE_FRAC` of w/h, centred): `Pane` payload → `Swap { pane }`, cue = whole pane rect; `Tab` payload → fall through to nearest-edge (a tab never swaps). Otherwise nearest edge by normalized distance ((cursor.x-r.x)/r.w vs (r.right()-cursor.x)/r.w etc., minimum wins) → `SplitBeside { pane, orient, before }` (left → `LeftRight, before=true`; right → `LeftRight, before=false`; top → `TopBottom, before=true`; bottom → `TopBottom, before=false`); cue = that half of the pane's rect.
- For `TabAt`: exclude a bar whose `tabs[0].first_pane` is inside `payload_panes` **unless** the payload is a Tab of that same bar (reorder is allowed; dropping a pane into a tab strip belonging to the dragged subtree is not).

- [ ] **Step 1: Write the failing tests** (same file, `#[cfg(test)] mod tests`)

```rust
use super::*;
use rt_core::{PaneId, Rect};
use rt_session::DropTarget;

fn two_panes() -> Vec<(PaneId, Rect)> {
    vec![
        (PaneId(1), Rect::new(0.0, 0.0, 400.0, 600.0)),
        (PaneId(2), Rect::new(406.0, 0.0, 400.0, 600.0)),
    ]
}
fn bounds() -> Rect { Rect::new(0.0, 0.0, 806.0, 600.0) }

#[test]
fn centre_of_a_pane_swaps_for_a_pane_payload() {
    let r = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &two_panes(), &[], bounds(), (200.0, 300.0)).unwrap();
    assert_eq!(r.target, DropTarget::Swap { pane: PaneId(1) });
    assert!(!r.caret);
    assert_eq!((r.cue.w, r.cue.h), (400.0, 600.0), "whole-pane cue");
}

#[test]
fn near_an_edge_splits_on_that_side() {
    // 30px from pane 2's left edge, vertically centred → LeftRight before=true.
    let r = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &two_panes(), &[], bounds(), (436.0, 300.0)).unwrap();
    assert_eq!(r.target, DropTarget::SplitBeside { pane: PaneId(2), orient: rt_core::Orientation::LeftRight, before: true });
    assert!(r.cue.w < 401.0 * 0.6, "cue is (about) the left half of pane 2");
    // Near pane 1's bottom → TopBottom before=false.
    let r2 = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &two_panes(), &[], bounds(), (200.0, 590.0)).unwrap();
    assert_eq!(r2.target, DropTarget::SplitBeside { pane: PaneId(1), orient: rt_core::Orientation::TopBottom, before: false });
}

#[test]
fn window_edge_strip_beats_pane_zones() {
    let r = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &two_panes(), &[], bounds(), (200.0, 10.0)).unwrap();
    assert_eq!(r.target, DropTarget::RootEdge { orient: rt_core::Orientation::TopBottom, before: true });
}

#[test]
fn a_pane_never_drops_on_itself() {
    assert!(resolve_drop(DragPayload::Pane(PaneId(1)), &[PaneId(1)], &two_panes(), &[], bounds(), (200.0, 300.0)).is_none());
}

#[test]
fn tab_strip_yields_an_insert_caret() {
    // One bar, two 100px tabs at y 0..24.
    let bar = make_bar(&[(PaneId(1), 0.0), (PaneId(2), 100.0)]); // helper below
    let panes = vec![(PaneId(1), Rect::new(0.0, 24.0, 806.0, 576.0))];
    // Cursor past tab 1's midpoint and tab 2's midpoint → index 2 (the end).
    let r = resolve_drop(DragPayload::Pane(PaneId(9)), &[PaneId(9)], &panes, &[bar], bounds(), (190.0, 12.0)).unwrap();
    assert_eq!(r.target, DropTarget::TabAt { anchor: PaneId(1), index: 2 });
    assert!(r.caret);
}

#[test]
fn a_tab_reorders_on_its_own_strip() {
    let bar = make_bar(&[(PaneId(1), 0.0), (PaneId(2), 100.0)]);
    let panes = vec![(PaneId(2), Rect::new(0.0, 24.0, 806.0, 576.0))];
    let r = resolve_drop(DragPayload::Tab { first_pane: PaneId(2) }, &[PaneId(2)], &panes, &[bar], bounds(), (10.0, 12.0)).unwrap();
    assert_eq!(r.target, DropTarget::TabAt { anchor: PaneId(1), index: 0 });
}
```

`make_bar` builds an `rt_core::TabBar` — `Tab`'s fields are public (`rect`, `first_pane`, `active`, `number`, see layout.rs:535-552):

```rust
fn make_bar(tabs: &[(PaneId, f32)]) -> rt_core::TabBar {
    rt_core::TabBar {
        tabs: tabs.iter().enumerate().map(|(i, (id, x))| rt_core::Tab {
            rect: Rect::new(*x, 0.0, 100.0, 24.0),
            first_pane: *id,
            active: i == 0,
            number: i + 1,
        }).collect(),
    }
}
```

- [ ] **Step 2: Run to verify failure** — `cargo test -p rt dragdrop`

- [ ] **Step 3: Implement `resolve_drop` per the priority spec above**

- [ ] **Step 4: Run the tests** — `cargo test -p rt dragdrop` → PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/dragdrop.rs crates/rt/src/main.rs && git commit -m "feat(rt): pure drop-target resolver with cue geometry"
```

---

### Task 9: In-window drag state machine

**Files:**
- Modify: `crates/rt/src/main.rs` (mouse press :1915-2146, motion :1792-1875, release :2148-2197, `on_key_press` Escape, `update_cursor` :4438)

**Interfaces:**
- Consumes: Task 8's `resolve_drop`/`DragPayload`/`DRAG_THRESHOLD`; Task 4's `move_pane`/`move_tab`/`reorder_tab`.
- Produces on `App` (drag spans windows later, so it lives on App, not Active):

```rust
struct ArmedDrag { window: WindowId, payload: dragdrop::DragPayload, press: (f32, f32) }
struct DragState {
    source: WindowId,
    payload: dragdrop::DragPayload,
    payload_panes: Vec<rt_core::PaneId>,
    label: String,                         // ghost chip text (title or "Tab N")
    hover: Option<(WindowId, dragdrop::ResolvedDrop)>,
    cursor: (f32, f32),                    // in hover-window coords (this task: source window)
}
// App fields: armed_drag: Option<ArmedDrag>, drag: Option<DragState>
```

Also on `Active`: `drag_cue: Option<dragdrop::ResolvedDrop>` + `drag_ghost: Option<((f32, f32), String)>` + `drag_dim: Option<rt_core::PaneId>` — per-frame values App writes before requesting a redraw, `paint_overlays_or_instruments` reads (Task 10 draws them; set them already in this task).

- [ ] **Step 1: Arm on press**

In the left-press handler, **after** the scrollbar check (:2026) and **before** the tab-click check (:2029):

```rust
// A press on a pane's titlebar strip arms a pane drag (it may still be a
// plain click — the threshold decides). The titlebar band is the top
// `titlebar_h` of the pane's rect; the clip affordance already returned above.
let tb_h = active.session.titlebar_h();
if tb_h > 0.0 {
    let hit = active.session.visible_rects(bounds).into_iter()
        .find(|(_, r)| r.contains(mx, my) && my < r.y + tb_h);
    if let Some((pid, _)) = hit {
        active.session.focus_at(mx, my); // titlebar click still focuses
        self_armed = Some(ArmedDrag { window: id, payload: dragdrop::DragPayload::Pane(pid), press: (mx, my) });
        // (assign to self.armed_drag after the borrow of `active` ends — see note)
        active.window.request_redraw();
        return;
    }
}
```

And change the tab-click arm (:2029-2045): instead of calling `focus_tab` on press, arm a Tab drag (`DragPayload::Tab { first_pane }`) and remember the press; the **release** without a started drag performs today's `focus_tab` + force_full. (Borrow note: `self.armed_drag` can't be set while `active` is borrowed from `self.windows`; stage into a local and set it after the `match` on the event, or restructure the press arm into a helper returning the ArmedDrag.)

- [ ] **Step 2: Promote to a drag on motion**

At the top of `CursorMoved` (after `active.mouse = …`):

```rust
if let Some(armed) = self.armed_drag.as_ref().filter(|a| a.window == id) {
    let d = ((active.mouse.0 - armed.press.0).powi(2) + (active.mouse.1 - armed.press.1).powi(2)).sqrt();
    if d > dragdrop::DRAG_THRESHOLD {
        let armed = self.armed_drag.take().unwrap();
        // Unzoom before dragging: zones only exist on the real layout.
        if active.session.is_zoomed() { active.session.toggle_zoom(); }
        let (payload_panes, label) = /* Pane → vec![p] + title_of(p) or "Pane";
            Tab → the tab's page panes: session.tree() has no public page walk —
            use extract-free identity: for a Tab payload store first_pane and
            approximate payload_panes = the panes of that page via
            tab_bars + rects is NOT enough; add a tiny rt-core helper:
            `pub fn tab_panes(&self, first_pane: PaneId) -> Option<Vec<PaneId>>`
            (find the page like take_tab, collect_panes WITHOUT removing) —
            add it (with a 5-line test) as part of this task. */;
        self.drag = Some(DragState { source: id, payload: armed.payload, payload_panes, label, hover: None, cursor: active.mouse });
        active.window.set_cursor(CursorIcon::Grabbing);
        active.cursor_icon = Some(CursorIcon::Grabbing);
    }
}
if let Some(drag) = self.drag.as_mut().filter(|d| d.source == id) {
    drag.cursor = active.mouse;
    let bounds = content_bounds(active.window.inner_size());
    let resolved = dragdrop::resolve_drop(
        drag.payload, &drag.payload_panes,
        &active.session.visible_rects(bounds),
        &active.session.tab_bars(bounds),
        bounds, active.mouse,
    );
    drag.hover = resolved.clone().map(|r| (id, r));
    active.drag_cue = resolved;
    active.drag_ghost = Some((active.mouse, drag.label.clone()));
    active.drag_dim = match drag.payload { dragdrop::DragPayload::Pane(p) => Some(p), _ => None };
    active.force_full = true;               // cues span arbitrary pixels
    active.window.request_redraw();
    return;                                  // a pane drag owns the pointer
}
```

The existing motion arms (`scroll_drag`/`mouse_report`/`wiring_from`/…) run only when no pane-drag is active — the `return` above guarantees it.

- [ ] **Step 3: Commit or cancel on release / Escape**

In `Released, MouseButton::Left`, before the wiring arm:

```rust
if let Some(armed) = self.armed_drag.take() {
    // Below threshold: this was a click. A tab press performs its switch now.
    if let dragdrop::DragPayload::Tab { first_pane } = armed.payload {
        active.session.focus_tab(first_pane);
        active.force_full = true;
        active.window.request_redraw();
    }
    return;
}
if let Some(drag) = self.drag.take() {
    let committed = match (drag.payload, drag.hover) {
        (dragdrop::DragPayload::Pane(p), Some((_w, r))) => active.session.move_pane(p, r.target),
        (dragdrop::DragPayload::Tab { first_pane }, Some((_w, r))) => match r.target {
            rt_session::DropTarget::TabAt { index, .. } /* own strip = reorder */
                => active.session.reorder_tab(first_pane, index_for_reorder(first_pane, index, active)),
            other => active.session.move_tab(first_pane, other),
        },
        (_, None) => false, // released over nothing (gutter): cancel
    };
    active.drag_cue = None; active.drag_ghost = None; active.drag_dim = None;
    active.force_full = true;
    Self::update_cursor(active);
    active.window.request_redraw();
    let _ = committed;
    return;
}
```

`index_for_reorder`: when a tab is dropped on its own strip, `TabAt.index` counts insertion slots including its own current position — moving right must subtract 1 (removing it first shifts later slots left). Compute: find the tab's current index in that bar; if `index > current`, use `index - 1`, else `index`. Write it as a small pure function next to `resolve_drop` **with a unit test** (drop tab 0 at slot 2 of [a,b,c] → reorder to 1 → [b,a,c]… no: dropping a at the caret after b means [b,a,c], i.e. reorder_tab(a, 1); the test pins this).

Escape: in `on_key_press`, before the keymap lookup: `if self.drag.take().is_some() { /* clear cue fields on the source Active, force_full, redraw */ return; }`.

`update_cursor` (:4438): add `|| /* drag active for this window */` to the early-return guard — pass a flag or check a new `active.drag_cue.is_some()`.

Also: cancel the drag if the payload pane dies mid-drag — in `about_to_wait`'s exited-pane loop, if `self.drag` payload_panes contains the exited id → clear `self.drag` + cue fields.

- [ ] **Step 4: Build, test, manual verify**

`cargo build --workspace && cargo test --workspace`. Manual (`RT_SPLIT=v cargo run -p rt`, titlebars on): drag pane A's titlebar onto pane B's left half → releases into a left split; centre → swap; onto the tab strip (make tabs with `RT_TABS=2`) → becomes a tab at the caret; drag a tab label left/right → reorders; a plain titlebar/tab click still focuses/switches; Escape mid-drag cancels; drag over a gutter and release → nothing happens. (Cues aren't painted until Task 10 — verify by behaviour only, this is expected.)

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/main.rs crates/rt/src/dragdrop.rs crates/rt-core/src/layout.rs
git commit -m "feat(rt): in-window pane/tab drag — arm, threshold, commit, cancel"
```

---

### Task 10: Cue rendering (fills, caret, ghost chip, source dim)

**Files:**
- Create: `crates/rt/src/chrome/dragdrop.rs` (+ `pub mod dragdrop;` in `chrome/mod.rs`)
- Modify: `crates/rt/src/main.rs` (`paint_overlays_or_instruments` :4130)

**Interfaces:**
- Consumes: `Active.drag_cue/drag_ghost/drag_dim` (Task 9), `Backend` primitives (`fill_rect`, `draw_char`, `cell_size` — `crates/rt/src/backend.rs:20`).
- Produces: `pub fn draw(backend: &mut dyn Backend, cue: Option<&ResolvedDrop>, ghost: Option<&((f32, f32), String)>, dim: Option<rt_core::Rect>, cell: (f32, f32))`.

- [ ] **Step 1: Implement the painter**

```rust
//! Drag-and-drop cues: the "put new tile here?" highlight, the tab-insert
//! caret, the ghost chip riding the cursor, and the dim over the dragged pane.
//! Pure Backend primitives so GL and XRender render identically.

use crate::backend::Backend;
use crate::dragdrop::ResolvedDrop;
// Color: use the same type + import the other chrome modules use (see
// chrome/menu.rs's imports and copy them exactly).

/// Cue colours: a translucent focus-blue accent, built inside `draw` because
/// `with_alpha` is not const. Values:
///   fill  = Color::rgb(0x4a, 0x7a, 0xc8).with_alpha(0.30)
///   edge  = Color::rgb(0x4a, 0x7a, 0xc8).with_alpha(0.90)
///   dim   = Color::rgb(0x00, 0x00, 0x00).with_alpha(0.35)
///   chip  = Color::rgb(0x10, 0x10, 0x14).with_alpha(0.85), text Color::rgb(0xd0, 0xd0, 0xd8)

pub fn draw(
    backend: &mut dyn Backend,
    cue: Option<&ResolvedDrop>,
    ghost: Option<&((f32, f32), String)>,
    dim: Option<rt_core::Rect>,
    cell: (f32, f32),
) {
    let cue_fill = Color::rgb(0x4a, 0x7a, 0xc8).with_alpha(0.30);
    let cue_edge = Color::rgb(0x4a, 0x7a, 0xc8).with_alpha(0.90);
    let dim_col = Color::rgb(0x00, 0x00, 0x00).with_alpha(0.35);
    if let Some(r) = dim {
        backend.fill_rect(r.x, r.y, r.w, r.h, dim_col); // the pane being dragged
    }
    if let Some(c) = cue {
        if c.caret {
            // A 3px caret between tabs, full strip height, plus little wings.
            backend.fill_rect(c.cue.x, c.cue.y, c.cue.w, c.cue.h, cue_edge);
            backend.fill_rect(c.cue.x - 3.0, c.cue.y, c.cue.w + 6.0, 3.0, cue_edge);
        } else {
            backend.fill_rect(c.cue.x, c.cue.y, c.cue.w, c.cue.h, cue_fill);
            // A 2px border so the zone reads even over busy content.
            backend.fill_rect(c.cue.x, c.cue.y, c.cue.w, 2.0, cue_edge);
            backend.fill_rect(c.cue.x, c.cue.y + c.cue.h - 2.0, c.cue.w, 2.0, cue_edge);
            backend.fill_rect(c.cue.x, c.cue.y, 2.0, c.cue.h, cue_edge);
            backend.fill_rect(c.cue.x + c.cue.w - 2.0, c.cue.y, 2.0, c.cue.h, cue_edge);
        }
    }
    if let Some(((x, y), label)) = ghost {
        // A small chip to the lower-right of the cursor with the payload label.
        let pad = 6.0;
        let w = label.chars().count() as f32 * cell.0 + 2.0 * pad;
        let h = cell.1 + 2.0 * pad;
        let (cx, cy) = (x + 12.0, y + 12.0);
        backend.fill_rect(cx, cy, w, h, Color::rgb(0x10, 0x10, 0x14).with_alpha(0.85));
        backend.fill_rect(cx, cy, w, 1.0, cue_edge);
        let text = Color::rgb(0xd0, 0xd0, 0xd8);
        for (i, ch) in label.chars().enumerate() {
            backend.draw_char(cx + pad, cy + pad, i, 0, ch, text, false, false);
        }
    }
}
```

(Exact `Color` construction: copy the idiom used by `chrome/menu.rs`; if `fill_rect` alpha turns out unsupported on the XRender backend, fall back to a solid 2px border + diagonal hatch of 1px lines — test on `--backend xrender` under Xvfb before deciding.)

- [ ] **Step 2: Hook into the frame**

In `paint_overlays_or_instruments` (:4130), before the prefs/overlay checks (cues must draw above content but a drag can't coexist with an open overlay anyway):

```rust
if active.drag_cue.is_some() || active.drag_ghost.is_some() {
    let dim = active.drag_dim.and_then(|p| {
        let bounds = content_bounds(active.window.inner_size());
        active.session.visible_rects(bounds).into_iter().find(|(id, _)| *id == p).map(|(_, r)| r)
    });
    let cell = active.backend.cell_size();
    chrome::dragdrop::draw(&mut *active.backend, active.drag_cue.as_ref(), active.drag_ghost.as_ref(), dim, cell);
}
```

- [ ] **Step 3: Build + manual verify** (GL and `--backend xrender` under `Xvfb`): all four cue families visible and tracking; ghost chip shows the pane title; the dragged pane dims; everything clears on drop/Escape.

- [ ] **Step 4: Commit**

```bash
git add crates/rt/src/chrome/dragdrop.rs crates/rt/src/chrome/mod.rs crates/rt/src/main.rs
git commit -m "feat(rt): drag-and-drop cue rendering — zone fills, tab caret, ghost chip"
```

---

### Task 11: Cross-window hover + drop + drag tear-out

**Files:**
- Modify: `crates/rt/src/main.rs` (drag motion/release from Task 9; `detach` from Task 7)

**Interfaces:**
- Consumes: everything above.
- Produces: `fn window_under_global(&self, global: (f64, f64)) -> Option<WindowId>`; drag release handles other-window and no-window drops; `DragState.hover` may now name a non-source window.

- [ ] **Step 1: Global hover resolution**

winit gives `inner_position()`/`outer_position()` on X11 and errors on Wayland — that IS the platform gate:

```rust
/// The rt window whose CONTENT rect contains the global point. X11 only:
/// on Wayland `inner_position()` errs and this returns None for every window,
/// which cleanly disables cross-window hover (the spec's Wayland stance).
fn window_under_global(&self, global: (f64, f64)) -> Option<WindowId> {
    for (&wid, a) in self.windows.iter() {
        let Ok(pos) = a.window.inner_position() else { continue };
        let size = a.window.inner_size();
        let (lx, ly) = (global.0 - pos.x as f64, global.1 - pos.y as f64);
        if lx >= 0.0 && ly >= 0.0 && lx < size.width as f64 && ly < size.height as f64 {
            return Some(wid);
        }
    }
    None
}
```

In the drag-motion block (Task 9 step 2): compute `global = source.inner_position() + active.mouse` (when `inner_position()` is Ok). If `window_under_global(global)` is a DIFFERENT window `w`, resolve against `w`'s session with `w`-local coords, set `drag.hover = Some((w, resolved))`, write the cue fields on `w`'s Active (clearing them on the previous hover window, with `force_full` + redraw on both). If it is the source (or position unsupported), Task 9's path runs unchanged. Track the last hover window on `DragState` so leave-events clear stale cues.

- [ ] **Step 2: Cross-window commit + swap**

In the release handler, extend the commit match: when `drag.hover = Some((w, r))` with `w != drag.source`:

```rust
let committed = match r.target {
    rt_session::DropTarget::Swap { pane: b } => {
        // Cross-window swap: rewrite one leaf id in each tree, then exchange
        // the two panes' side-table entries between the sessions.
        let dragged = match drag.payload { dragdrop::DragPayload::Pane(p) => p, _ => unreachable!("resolver never offers Swap for a tab") };
        // src tree: dragged → b ; dst tree: b → dragged
        let src_ok = self.windows.get_mut(&drag.source).unwrap().session.tree_replace(dragged, b);
        let dst_ok = src_ok && self.windows.get_mut(&w).unwrap().session.tree_replace(b, dragged);
        if dst_ok {
            let from_src = self.windows.get_mut(&drag.source).unwrap().session.eject_entries(&[dragged]);
            let from_dst = self.windows.get_mut(&w).unwrap().session.eject_entries(&[b]);
            self.windows.get_mut(&drag.source).unwrap().session.inject_entries(from_dst);
            self.windows.get_mut(&w).unwrap().session.inject_entries(from_src);
            for wid in [drag.source, w] {
                let a = self.windows.get_mut(&wid).unwrap();
                a.session.relayout(content_bounds(a.window.inner_size()));
            }
            // Jacks swap between the two windows' maps the same way.
        }
        dst_ok
    }
    other => {
        // Extract from the source (pane or tab), adopt into `w` at `other`.
        let src = self.windows.get_mut(&drag.source).unwrap();
        let pkg = match drag.payload {
            dragdrop::DragPayload::Pane(p) => src.session.extract_pane(p),
            dragdrop::DragPayload::Tab { first_pane } => src.session.extract_tab(first_pane),
        };
        match pkg {
            None => false,
            Some(pkg) => {
                let moved = pkg.sub.panes();
                match self.windows.get_mut(&w).unwrap().session.adopt(pkg, other) {
                    Ok(()) => {
                        // Jacks + wholly-internal wires travel; crossing wires are cut.
                        // Factor Task 7's block into
                        //   fn migrate_pane_extras(&mut self, from: WindowId, to: WindowId, moved: &[rt_core::PaneId])
                        // (it removes the moved panes' Jacks from `from`, inserts them
                        // into `to`, drains `from.wires` keeping both-ends-moved wires
                        // for `to` and dropping wires that would cross windows) and
                        // call it from BOTH detach and here.
                        self.migrate_pane_extras(drag.source, w, &moved);
                        let t = self.windows.get_mut(&w).unwrap();
                        t.force_full = true;
                        t.window.request_redraw();
                        true
                    }
                    Err(pkg) => {
                        // Target refused (stale hover): put the package back in the
                        // source as a root-edge split — a pane must never vanish.
                        let s = self.windows.get_mut(&drag.source).unwrap();
                        let rt_session::PanePackage { sub, entries } = pkg;
                        // (needs `Session::readopt_root_edge(sub, entries)` — a 6-line
                        // pub helper doing insert_root_edge + inject_entries + relayout;
                        // add it to rt-session in this task with a doc comment.)
                        s.session.readopt_root_edge(sub, entries);
                        s.force_full = true;
                        s.window.request_redraw();
                        false
                    }
                }
            }
        }
    }
};
// If the source emptied, close it (the last pane moved out):
if self.windows.get(&drag.source).map(|a| a.session.is_empty()).unwrap_or(false) {
    self.close_window(drag.source);
}
```

This needs `Session::tree_replace(&mut self, from: PaneId, to: PaneId) -> bool` — a one-line public delegate to `Tree::replace_leaf` (add it to rt-session with a 3-line doc comment; no new test needed beyond rt-core's, but extend `extract_then_adopt_moves_everything`'s file with a `cross_session_swap_exchanges_entries` test mirroring the App logic at the session level: replace+eject+inject on two mock sessions, assert titles/groups landed swapped).

- [ ] **Step 3: Tear-out on outside drop**

Release with `drag.hover == None` AND the cursor outside the source window's bounds (`active.mouse` outside `(0,0)..inner_size` — valid on X11 via global point in no window; on Wayland via the surface-local coords winit keeps delivering during the implicit grab):

- Refactor Task 7's `detach` so its extract-package-into-new-window core becomes `fn tear_out(&mut self, event_loop, source: WindowId, payload: dragdrop::DragPayload, position: Option<PhysicalPosition<i32>>)`; `detach` calls it with `position: None`; the drag release calls it with the X11 global drop point (`window.set_outer_position` on the new window right after `build_active`; skip on Wayland — the compositor places it).
- Release **inside** the source but over no target (a gutter) stays a cancel.

**Wayland spike (do this FIRST in this task, ~15 min):** on KDE Wayland run a build with an `eprintln!` in `CursorMoved` while holding a titlebar press, and check whether coords keep arriving (and go negative/out-of-range) once the cursor leaves the window. If they do NOT, drag-tear-out is X11-only: gate the outside-drop check on X11 (`inner_position().is_ok()`) and note it in the manual + KNOWN_ISSUES. Record the spike's finding in the commit message.

- [ ] **Step 4: Build + manual verify (X11 + ssh -X + Wayland)**

Two windows (`Ctrl+Shift+I`): drag a pane from one into the other — cues appear in the target window while hovering; all four target families commit; centre-drop swaps panes ACROSS windows; drag a pane out onto the desktop → new window at the cursor (X11); mother-close survival; last-pane drag-out closes the source; on Wayland verify in-window drag still perfect and tear-out per the spike's finding. Watch an `ssh -X` session for cue-drag request storms (they ride force_full full frames — acceptable; if it visibly lags, drop the ghost chip on `backend.is_software()` like the wire rubber-band does at :1822).

- [ ] **Step 5: Commit**

```bash
git add crates/rt/src/main.rs crates/rt-session/src/lib.rs
git commit -m "feat(rt): cross-window drag, cross-window swap, and drag tear-out"
```

---

### Task 12: "Move to window ▸" menu (Wayland parity)

**Files:**
- Modify: `crates/rt/src/menu.rs`, `crates/rt/src/main.rs` (menu build + click sites)

**Interfaces:**
- Produces: `menu::rows(keymap, has_selection, url, move_targets: &[String]) -> Vec<Row>`; `RowAction::MoveToWindow(usize)` / `MenuPick::MoveToWindow(usize)` — index into the caller's parallel `Vec<WindowId>`.

- [ ] **Step 1: Write the failing test** (menu.rs tests)

```rust
#[test]
fn move_to_window_rows_appear_per_target() {
    let km = Keymap::default();
    let none = rows(&km, false, None, &[]);
    assert!(!none.iter().any(|r| r.label.starts_with("Move Pane to ")));
    let some = rows(&km, false, None, &["2: htop".into(), "3: logs".into()]);
    let labels: Vec<_> = some.iter().filter(|r| r.label.starts_with("Move Pane to ")).map(|r| r.label.clone()).collect();
    assert_eq!(labels, vec!["Move Pane to 2: htop", "Move Pane to 3: logs"]);
}
```

(Existing `rows(...)` tests gain a `&[]` fourth argument.)

- [ ] **Step 2: Implement** — `rows` inserts, after the Detach rows, one `Row { label: format!("Move Pane to {t}"), action: Some(RowAction::MoveToWindow(i)), enabled: true, accel: None }` per target. In main.rs: where the menu is opened/hit-tested, build `move_targets`: every OTHER window as `"{n}: {title}"` (title = its focused pane's `title_of` or `"rt"`), plus a parallel `Vec<WindowId>` kept on `Active` next to the open-menu state (`menu_windows: Vec<WindowId>`). On `MenuPick::MoveToWindow(i)`: target focus pane `tf = target.session.focus()`; commit `extract_pane(focus)` from the source + `adopt` into the target at `DropTarget::SplitBeside { pane: tf, orient: Orientation::LeftRight, before: false }`, with jacks/wires migration via `migrate_pane_extras`, close-if-emptied, `force_full` + redraw on both.

- [ ] **Step 3: Run tests + manual verify on Wayland** (two windows, right-click → Move Pane to 2 → arrives split beside the target's focus).

- [ ] **Step 4: Commit**

```bash
git add crates/rt/src/menu.rs crates/rt/src/main.rs && git commit -m "feat(rt): Move-to-window menu — cross-window moves without drag (Wayland parity)"
```

---

### Task 13: Docs, roadmap, project map

**Files:**
- Modify: `crates/rt/src/manual.rs` (flesh out the Drag & drop paragraph to what actually shipped, incl. the Wayland spike's outcome)
- Modify: `docs/ROADMAP.md` (#17: mark reorder/drag, detach-to-new-window, move_tab ✅; note tab position/close-button still open; #21: note `new_window` exists in-process)
- Modify: `docs/TERMINATOR_FEATURES.md` (`detachable_tabs`, `new_window`, drag-and-drop checkboxes)
- Modify: `README.md` (feature list: multi-window, tear-out, drag-and-drop with drop cues)
- Modify: `project-map.js` (per CLAUDE.md standing order): add a `multiwindow` node (status `done`, deps → `rt-session`, desc: one process/many windows, tear-out, cross-window drag on X11, Move-to-window on Wayland); update `tabs-adv` parts (drag-reorder + detach `done`, tab position/close-button `planned`); set `project.updated` to the completion date; check every `deps` id still resolves.

- [ ] **Step 1: Make the edits** (statuses must reflect what actually merged — re-read the diff, not this plan, before writing them).
- [ ] **Step 2: Verify** — `cargo test --workspace` (the manual test guards the bindings); open `project-map.html` via `python3 -m http.server` and check zero console errors.
- [ ] **Step 3: Commit**

```bash
git add crates/rt/src/manual.rs docs/ROADMAP.md docs/TERMINATOR_FEATURES.md README.md project-map.js
git commit -m "docs: manual, roadmap, feature matrix and project map for drag-and-drop + multi-window"
```

---

### Task 14: Whole-branch verification

- [ ] `cargo build --workspace && cargo test --workspace` — green.
- [ ] `ci/verify.sh` if configured for this branch (see the milkv silent-green caveat in the memory: empty remote output is a FAILURE, not a pass).
- [ ] Full manual sweep from the spec's Testing section on: local Wayland (KDE), local X11 (`WINIT_UNIX_BACKEND=x11`), `ssh -X` to apollo. Script: 2 windows, every cue family in-window, every cue family cross-window (X11), tab reorder by drag + keys, detach pane/tab by key, drag tear-out, mother-window close survival, last-pane moves, Escape cancel, click-vs-drag threshold feel, patch-bay wire still draggable (regression), divider drag still works (regression), selection drag still works (regression).
- [ ] Final review: invoke superpowers:requesting-code-review for the whole branch, then merge to main per the standard loop (release + deploy are the post-merge steps from the how-we-work memory, not part of this plan).
