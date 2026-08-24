//! The recursive layout tree — rt's port of Terminator's split/tab containers.
//!
//! ## What we are porting
//! Terminator arranges terminals by nesting `Gtk.Paned` widgets (binary
//! splits) and `Gtk.Notebook` widgets (tabs), and it *physically reparents* the
//! VTE widgets as the user splits and closes panes. That reparenting during
//! live signal handlers is the source of a whole class of intermittent crashes
//! (see `docs/TERMINATOR_BUGS.md`, #2/#4/#5).
//!
//! ## How rt differs
//! Here the layout is a **pure immutable-ish data structure**. A `Tree` owns a
//! single root `Node`; leaves carry only a `PaneId` (an integer handle), never
//! a live terminal or widget. The GUI keeps the real panes in a side table
//! keyed by `PaneId`. Splitting/closing is ordinary tree surgery on plain data,
//! so there is nothing to "use after free". The renderer asks the tree for a
//! list of `(PaneId, Rect)` each frame; nothing is ever reparented.
//!
//! ## Model
//! * `Node::Leaf(id)` — a single terminal pane.
//! * `Node::Split { orient, children }` — an N-ary split (we only ever create
//!   binary splits, matching `Gtk.Paned`, but N-ary keeps collapse logic
//!   simple and lets a future "even-split" feature append siblings cheaply).
//! * `Node::Tabs { children, active }` — a tab bar; only the active child is
//!   visible/laid-out.

use crate::geom::Rect;
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

/// Width in logical pixels of the draggable gutter drawn between split
/// children. Subtracted from the available space before dividing it, so panes
/// never visually overlap the divider. Kept small; the renderer draws the
/// handle within this band.
const DIVIDER: f32 = 6.0;

/// Height in logical pixels of the tab strip drawn at the top of a `Tabs` node.
/// The active tab's content is laid out below it.
const TAB_STRIP: f32 = 24.0;

/// Opaque identity of a pane (a leaf of the tree).
///
/// It is just a `u64` handle. The layout tree never dereferences it; only the
/// GUI's pane table does. Using an integer (not a pointer/`Rc`) is what makes
/// the tree trivially `Clone`, serialisable for saved layouts, and immune to
/// the aliasing hazards that bite Terminator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct PaneId(pub u64);

/// How a split arranges its children.
///
/// Named by the visible result, not by the divider's direction, to avoid the
/// perennial "horizontal split" ambiguity: `LeftRight` places children side by
/// side (a vertical divider between them); `TopBottom` stacks them (a
/// horizontal divider between them).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Orientation {
    LeftRight, // children laid out along the x axis
    TopBottom, // children laid out along the y axis
}

/// A focus-movement direction for keyboard pane navigation (Terminator's
/// Alt+Arrow / Ctrl+Shift+navigation). Resolved geometrically against the
/// current rectangles rather than by tree position, so it feels spatial.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    Left,
    Right,
    Up,
    Down,
}

/// One child slot inside a split, carrying its flexible size `weight`.
///
/// Weights are relative: a child gets `weight / sum_of_weights` of the space
/// along the split axis. Fresh splits use equal weights (`1.0` each), matching
/// Terminator's default 50/50 `Gtk.Paned`; dragging a divider later just
/// rewrites these numbers.
#[derive(Clone, Debug)]
struct Child {
    weight: f32, // relative flex factor along the split's orientation
    node: Node,  // the subtree occupying this slot
}

/// A node in the layout tree: a leaf pane, a split, or a tab group.
#[derive(Clone, Debug)]
enum Node {
    /// A single terminal pane identified by its handle.
    Leaf(PaneId),
    /// An N-ary split of children along one orientation.
    Split {
        orient: Orientation, // arrangement axis for `children`
        children: Vec<Child>, // ordered slots, first = left/top
    },
    /// A tab group: children stacked in z-order, only `active` is shown.
    Tabs {
        children: Vec<Node>, // one entry per tab
        active: usize,       // index into `children`; always kept in range
    },
}

impl Node {
    /// Whether this page's first leaf is `id` (tab identity test).
    fn has_first_leaf(node: &Node, id: PaneId) -> bool {
        Tree::first_leaf(node) == Some(id)
    }
}

/// The whole layout for one window: a root node. PaneIds are allocated from a
/// process-global counter so they are unique across all trees — this allows panes
/// to move between windows without remapping their ids or side-table entries.
#[derive(Clone, Debug)]
pub struct Tree {
    root: Node, // the current arrangement
}

impl Tree {
    /// Create a tree containing exactly one pane and return the pair
    /// `(tree, first_pane_id)`.
    ///
    /// Every window starts life as a single full-window terminal, exactly like
    /// Terminator opening a bare window before any split.
    pub fn new() -> (Self, PaneId) {
        let first = mint_global();
        let tree = Tree {
            root: Node::Leaf(first), // root starts as a lone leaf
        };
        (tree, first)
    }

    /// Mint a brand-new, never-before-used `PaneId`.
    ///
    /// Private helper used by every operation that introduces a pane. Ids are
    /// never recycled even after a pane closes, so a stale id can never
    /// accidentally alias a live pane (the same discipline that makes
    /// `deregister` idempotent in rt vs. Terminator's double-remove crash, #3).
    fn mint(&mut self) -> PaneId {
        mint_global()
    }

    /// Split the pane `target` in two, inserting a fresh pane beside it.
    ///
    /// Returns `Some(new_pane_id)` on success, or `None` if `target` is not a
    /// leaf in this tree (a caller passing a stale id — we degrade gracefully
    /// instead of panicking, per rt's no-crash policy).
    ///
    /// Semantics: the target leaf is replaced by a binary `Split` whose first
    /// child is the original pane and whose second child is the new pane, each
    /// weighted `1.0` (a 50/50 divide). This mirrors `Gtk.Paned`'s binary
    /// nesting precisely.
    pub fn split(&mut self, target: PaneId, orient: Orientation) -> Option<PaneId> {
        let new_id = self.mint(); // reserve the id up front so we can return it
        // Walk the tree mutating in place; only proceed if the target existed.
        if Self::split_node(&mut self.root, target, orient, new_id) {
            Some(new_id) // surgery succeeded
        } else {
            // Target wasn't found: we already advanced the global id counter, which is fine —
            // ids are allowed to have gaps; correctness only needs uniqueness.
            None
        }
    }

    /// Recursive worker for [`Tree::split`]. Returns `true` once it has found
    /// and replaced the target leaf somewhere in `node`'s subtree.
    ///
    /// Taking `&mut Node` lets us rewrite the matched leaf into a split in
    /// place. We stop at the first match (pane ids are unique, so there is at
    /// most one).
    fn split_node(node: &mut Node, target: PaneId, orient: Orientation, new_id: PaneId) -> bool {
        match node {
            // Base case: this leaf is the one to split.
            Node::Leaf(id) if *id == target => {
                let original = *id; // remember the pane we are wrapping
                // Replace the leaf with a two-slot split: original then new.
                *node = Node::Split {
                    orient,
                    children: vec![
                        Child { weight: 1.0, node: Node::Leaf(original) }, // keep old pane
                        Child { weight: 1.0, node: Node::Leaf(new_id) },   // add new pane
                    ],
                };
                true // found and handled
            }
            // A different leaf: not our target.
            Node::Leaf(_) => false,
            // Recurse into each split child until one reports success.
            Node::Split { children, .. } => {
                for child in children.iter_mut() {
                    if Self::split_node(&mut child.node, target, orient, new_id) {
                        return true; // short-circuit on first match
                    }
                }
                false // target not in this split
            }
            // Recurse into every tab page (target may live in an inactive tab).
            Node::Tabs { children, .. } => {
                for child in children.iter_mut() {
                    if Self::split_node(child, target, orient, new_id) {
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Add a new tab as a sibling of `target`, wrapping in a `Tabs` node if the
    /// target is not already inside one. Returns the new pane's id, or `None`
    /// if `target` was not found.
    ///
    /// Port of Terminator's "new tab" (Ctrl+Shift+T): the new tab becomes the
    /// active one, matching Terminator's behaviour of focusing a freshly opened
    /// tab.
    pub fn new_tab(&mut self, target: PaneId) -> Option<PaneId> {
        let new_id = self.mint(); // reserve id for the tab's pane
        if Self::add_tab_node(&mut self.root, target, new_id) {
            Some(new_id)
        } else {
            None
        }
    }

    /// Recursive worker for [`Tree::new_tab`]. Returns `true` when handled.
    fn add_tab_node(node: &mut Node, target: PaneId, new_id: PaneId) -> bool {
        match node {
            // The target leaf is not yet tabbed: wrap it in a fresh Tabs node
            // with the new pane as a second, now-active tab.
            Node::Leaf(id) if *id == target => {
                let original = *id;
                *node = Node::Tabs {
                    children: vec![Node::Leaf(original), Node::Leaf(new_id)],
                    active: 1, // focus the newly added tab, like Terminator
                };
                true
            }
            Node::Leaf(_) => false,
            Node::Split { children, .. } => {
                for child in children.iter_mut() {
                    if Self::add_tab_node(&mut child.node, target, new_id) {
                        return true;
                    }
                }
                false
            }
            // If the target is a *direct* leaf child of this Tabs node, append
            // the new pane as another tab here rather than nesting a second
            // Tabs (keeps the tree flat and matches user expectation).
            Node::Tabs { children, active } => {
                // First, is the target one of our own top-level tab leaves?
                let direct = children
                    .iter()
                    .position(|c| matches!(c, Node::Leaf(id) if *id == target));
                if let Some(_) = direct {
                    children.push(Node::Leaf(new_id)); // append new tab page
                    *active = children.len() - 1;      // focus it
                    return true;
                }
                // Otherwise recurse into each page (target may be nested deeper).
                for child in children.iter_mut() {
                    if Self::add_tab_node(child, target, new_id) {
                        return true;
                    }
                }
                false
            }
        }
    }

    /// Remove the pane `target` from the tree, collapsing now-redundant
    /// containers. Returns `true` if a pane was removed.
    ///
    /// Collapse rules (the inverse of split/new_tab):
    /// * A split or tab group left with a single child is replaced by that
    ///   child (no lone one-way splits linger — Terminator does the same via
    ///   `Gtk.Paned` removal, but here it cannot crash mid-reparent).
    /// * Removing the last pane leaves the tree empty; the caller (window) is
    ///   expected to close the window in that case. We report success and leave
    ///   a sentinel the caller checks via [`Tree::is_empty`].
    pub fn close(&mut self, target: PaneId) -> bool {
        // `remove_from` returns an Option<Node>: Some(replacement) when the
        // subtree still has content, None when it became empty and should be
        // dropped by the parent. We seed it with the whole root.
        match Self::remove_from(std::mem::replace(&mut self.root, Node::Leaf(PaneId(u64::MAX))), target) {
            Removal::NotFound(node) => {
                self.root = node; // restore the untouched tree; nothing removed
                false
            }
            Removal::Replaced(Some(node)) => {
                self.root = node; // install the collapsed subtree
                true
            }
            Removal::Replaced(None) => {
                // The last pane was removed; mark the tree empty with a sentinel
                // leaf id that `is_empty` recognises.
                self.root = Node::Leaf(PaneId(u64::MAX));
                true
            }
        }
    }

    /// Whether the tree has been emptied (its final pane closed). The window
    /// layer should close the window when this becomes true.
    pub fn is_empty(&self) -> bool {
        // The sentinel installed by `close` is a leaf holding u64::MAX.
        matches!(self.root, Node::Leaf(PaneId(u64::MAX)))
    }

    /// Recursive worker for [`Tree::close`]. Consumes a node and returns a
    /// [`Removal`] describing whether the target was found and what the node
    /// should become afterwards.
    ///
    /// Taking the node by value (not `&mut`) makes the collapse cases natural:
    /// we can move a surviving single child *up* to replace its parent without
    /// fighting the borrow checker.
    fn remove_from(node: Node, target: PaneId) -> Removal {
        match node {
            // A leaf: removed if it is the target, otherwise handed back intact.
            Node::Leaf(id) if id == target => Removal::Replaced(None), // this pane is gone
            Node::Leaf(id) => Removal::NotFound(Node::Leaf(id)),       // unrelated leaf, untouched
            Node::Split { orient, mut children } => {
                // Try to remove the target from exactly one child slot.
                let mut hit = false; // did we find the target in this split?
                let mut i = 0; // manual index so we can drop a slot in place
                while i < children.len() {
                    match Self::remove_from(std::mem::replace(&mut children[i].node, Node::Leaf(PaneId(u64::MAX))), target) {
                        Removal::NotFound(orig) => {
                            children[i].node = orig; // put the untouched subtree back
                            i += 1; // advance to the next slot
                        }
                        Removal::Replaced(Some(new_node)) => {
                            children[i].node = new_node; // slot survived (smaller)
                            hit = true;
                            break; // target is unique; stop scanning
                        }
                        Removal::Replaced(None) => {
                            children.remove(i); // slot emptied: drop it entirely
                            hit = true;
                            break;
                        }
                    }
                }
                if !hit {
                    // Reassemble the split unchanged; target was elsewhere.
                    return Removal::NotFound(Node::Split { orient, children });
                }
                // Collapse rules after a successful removal:
                match children.len() {
                    0 => Removal::Replaced(None), // split emptied → tell parent to drop us
                    1 => Removal::Replaced(Some(children.pop().unwrap().node)), // lone child bubbles up
                    _ => Removal::Replaced(Some(Node::Split { orient, children })), // still a real split
                }
            }
            Node::Tabs { mut children, mut active } => {
                // Same one-slot removal scan as splits, but for tab pages.
                let mut hit = false;
                let mut i = 0;
                while i < children.len() {
                    match Self::remove_from(std::mem::replace(&mut children[i], Node::Leaf(PaneId(u64::MAX))), target) {
                        Removal::NotFound(orig) => {
                            children[i] = orig;
                            i += 1;
                        }
                        Removal::Replaced(Some(new_node)) => {
                            children[i] = new_node;
                            hit = true;
                            break;
                        }
                        Removal::Replaced(None) => {
                            children.remove(i); // remove the closed tab page
                            // Keep `active` valid: if we removed at/above it,
                            // shift it left, then clamp into range.
                            if active >= i && active > 0 {
                                active -= 1; // slide focus toward the start
                            }
                            hit = true;
                            break;
                        }
                    }
                }
                if !hit {
                    return Removal::NotFound(Node::Tabs { children, active });
                }
                match children.len() {
                    0 => Removal::Replaced(None), // no tabs left → drop the group
                    1 => Removal::Replaced(Some(children.pop().unwrap())), // one tab → unwrap it
                    _ => {
                        // Clamp active in case the last tab was removed.
                        if active >= children.len() {
                            active = children.len() - 1;
                        }
                        Removal::Replaced(Some(Node::Tabs { children, active }))
                    }
                }
            }
        }
    }

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
                let mut i = 0;
                let mut taken_out = None;
                while i < children.len() {
                    let child = std::mem::replace(&mut children[i].node, Node::Leaf(PaneId(u64::MAX)));
                    let (rest, taken) = Self::take_tab_from(child, first);
                    match rest {
                        Some(n) => children[i].node = n,
                        None => {
                            children.remove(i);
                        }
                    }
                    if taken.is_some() {
                        taken_out = taken;
                        break;
                    }
                    i += 1;
                }
                if taken_out.is_some() {
                    let rest = match children.len() {
                        0 => None,
                        1 => Some(children.pop().unwrap().node),
                        _ => Some(Node::Split { orient, children }),
                    };
                    return (rest, taken_out);
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
                let mut i = 0;
                let mut taken_out = None;
                while i < children.len() {
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
                        taken_out = taken;
                        break;
                    }
                    i += 1;
                }
                if taken_out.is_some() {
                    let rest = match children.len() {
                        0 => None,
                        1 => Some(children.pop().unwrap()),
                        _ => {
                            if active >= children.len() { active = children.len() - 1; }
                            Some(Node::Tabs { children, active })
                        }
                    };
                    return (rest, taken_out);
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

    /// Collect every pane and its pixel rectangle for the given window
    /// `bounds`, in stable left-to-right / top-to-bottom order.
    ///
    /// This is the function the renderer calls each frame: it turns the
    /// abstract tree into concrete blit targets. Inactive tab pages are omitted
    /// (they are not visible), which is why the result length can be smaller
    /// than the total pane count.
    pub fn rects(&self, bounds: Rect) -> Vec<(PaneId, Rect)> {
        if self.is_empty() {
            return Vec::new(); // an emptied tree draws nothing (window is closing)
        }
        let mut out = Vec::new(); // accumulator threaded through the recursion
        Self::layout_node(&self.root, bounds, &mut out); // fill it
        out
    }

    /// Recursive worker for [`Tree::rects`]. Divides `bounds` among `node`'s
    /// children according to orientation and weights, pushing `(PaneId, Rect)`
    /// for every visible leaf into `out`.
    fn layout_node(node: &Node, bounds: Rect, out: &mut Vec<(PaneId, Rect)>) {
        match node {
            // A leaf simply occupies its whole allotted rectangle.
            Node::Leaf(id) => out.push((*id, bounds)),
            Node::Split { orient, children } => {
                // Total weight normalises each child's share; guard against a
                // zero sum (shouldn't happen, but avoids a divide-by-zero NaN).
                let total: f32 = children.iter().map(|c| c.weight).sum();
                let total = if total > 0.0 { total } else { 1.0 };
                // Reserve gutters: n children need (n-1) dividers between them.
                let gutters = DIVIDER * (children.len().saturating_sub(1) as f32);
                match orient {
                    Orientation::LeftRight => {
                        let usable = (bounds.w - gutters).max(0.0); // width left for panes
                        let mut cursor = bounds.x; // running left edge
                        for child in children {
                            // This child's width is its share of the usable width.
                            let w = usable * (child.weight / total);
                            let r = Rect::new(cursor, bounds.y, w, bounds.h);
                            Self::layout_node(&child.node, r, out); // recurse into slot
                            cursor += w + DIVIDER; // advance past pane + gutter
                        }
                    }
                    Orientation::TopBottom => {
                        let usable = (bounds.h - gutters).max(0.0); // height for panes
                        let mut cursor = bounds.y; // running top edge
                        for child in children {
                            let h = usable * (child.weight / total); // this child's height
                            let r = Rect::new(bounds.x, cursor, bounds.w, h);
                            Self::layout_node(&child.node, r, out);
                            cursor += h + DIVIDER; // advance past pane + gutter
                        }
                    }
                }
            }
            // Only the active tab page is laid out; the rest are hidden.
            Node::Tabs { children, active } => {
                if let Some(child) = children.get(*active) {
                    // Reserve the tab strip at the top; the active tab's content
                    // fills the region below it.
                    let body = Rect::new(
                        bounds.x,
                        bounds.y + TAB_STRIP,               // push content below the strip
                        bounds.w,
                        (bounds.h - TAB_STRIP).max(0.0),    // remaining height
                    );
                    Self::layout_node(child, body, out);
                }
            }
        }
    }

    /// Every pane id in the tree, including panes on inactive tab pages, in
    /// traversal order. Useful for bookkeeping (e.g. reaping closed PTYs) where
    /// visibility does not matter.
    pub fn all_panes(&self) -> Vec<PaneId> {
        let mut out = Vec::new();
        Self::collect_panes(&self.root, &mut out); // walk the whole tree
        out
    }

    /// Recursive worker for [`Tree::all_panes`].
    fn collect_panes(node: &Node, out: &mut Vec<PaneId>) {
        match node {
            Node::Leaf(id) if *id != PaneId(u64::MAX) => out.push(*id), // skip the empty sentinel
            Node::Leaf(_) => {}                                          // sentinel: nothing to collect
            Node::Split { children, .. } => {
                for c in children {
                    Self::collect_panes(&c.node, out); // recurse each slot
                }
            }
            Node::Tabs { children, .. } => {
                for c in children {
                    Self::collect_panes(c, out); // recurse each tab page
                }
            }
        }
    }

    /// Find the pane the user would move focus to when pressing a directional
    /// navigation key from `from`. Returns `None` if there is no pane in that
    /// direction (an edge of the window).
    ///
    /// Algorithm (spatial, like tmux/i3): compute all visible rectangles for
    /// `bounds`, take the source pane's centre, then among panes that lie on the
    /// correct side pick the one whose centre is nearest. This gives intuitive
    /// movement regardless of how the tree happens to be nested.
    pub fn neighbor(&self, from: PaneId, dir: Direction, bounds: Rect) -> Option<PaneId> {
        let rects = self.rects(bounds); // visible panes only — you can't focus a hidden tab
        // Locate the source rectangle; if the source isn't visible, bail.
        let (_, src) = rects.iter().find(|(id, _)| *id == from)?;
        let (sx, sy) = src.center(); // source centre, our reference point
        let mut best: Option<(PaneId, f32)> = None; // (candidate, distance-so-far)
        for (id, r) in &rects {
            if *id == from {
                continue; // never navigate to yourself
            }
            let (cx, cy) = r.center(); // candidate centre
            // Keep only candidates strictly on the requested side of the source.
            let on_side = match dir {
                Direction::Left => cx < sx,
                Direction::Right => cx > sx,
                Direction::Up => cy < sy,
                Direction::Down => cy > sy,
            };
            if !on_side {
                continue; // wrong side; ignore
            }
            // Distance metric: straight-line distance between centres. Simple
            // and good enough; ties (rare) resolve to first-seen.
            let dist = (cx - sx).powi(2) + (cy - sy).powi(2); // squared distance (monotone)
            match best {
                Some((_, bd)) if dist >= bd => {} // not closer; keep current best
                _ => best = Some((*id, dist)),    // new nearest candidate
            }
        }
        best.map(|(id, _)| id) // strip the distance, return just the pane
    }
}

/// One clickable tab in a [`TabBar`], ready for the renderer to draw and the
/// input layer to hit-test.
#[derive(Clone, Copy, Debug)]
pub struct Tab {
    /// The label/click area of this tab within the strip, in pixels.
    pub rect: Rect,
    /// The first leaf pane inside this tab — the pane to focus when the tab is
    /// selected, and a stable identifier for the tab.
    pub first_pane: PaneId,
    /// Whether this is the currently active (visible) tab of its group.
    pub active: bool,
    /// 1-based tab number, used as the label until per-pane titles exist.
    pub number: usize,
}

/// A tab strip to render: the tabs of one visible `Tabs` node.
#[derive(Clone, Debug)]
pub struct TabBar {
    pub tabs: Vec<Tab>,
}

/// A handle to a divider being dragged, from [`Tree::divider_at`]. `path`
/// locates the split; `horizontal` is true for a left/right split (drag along
/// x); `start`/`len` are the split's pixel extent along its axis, used to turn a
/// mouse position into a ratio.
#[derive(Clone, Debug)]
pub struct DragHandle {
    pub path: Vec<usize>,
    pub horizontal: bool,
    pub start: f32,
    pub len: f32,
}

impl Tree {
    /// Enumerate the tab strips visible for the given window `bounds`, so the
    /// renderer can draw them and the input layer can hit-test clicks. Only
    /// `Tabs` nodes on the visible (active) path produce a strip.
    pub fn tab_bars(&self, bounds: Rect) -> Vec<TabBar> {
        let mut out = Vec::new();
        Self::collect_tab_bars(&self.root, bounds, &mut out);
        out
    }

    /// Recursive worker for [`Tree::tab_bars`]. Mirrors [`Tree::layout_node`]'s
    /// rectangle division so tab strips land exactly above their content.
    fn collect_tab_bars(node: &Node, bounds: Rect, out: &mut Vec<TabBar>) {
        match node {
            Node::Leaf(_) => {}
            Node::Split { orient, children } => {
                // Same weighted division as layout_node (kept in sync by hand).
                let total: f32 = children.iter().map(|c| c.weight).sum();
                let total = if total > 0.0 { total } else { 1.0 };
                let gutters = DIVIDER * (children.len().saturating_sub(1) as f32);
                match orient {
                    Orientation::LeftRight => {
                        let usable = (bounds.w - gutters).max(0.0);
                        let mut cursor = bounds.x;
                        for child in children {
                            let w = usable * (child.weight / total);
                            Self::collect_tab_bars(&child.node, Rect::new(cursor, bounds.y, w, bounds.h), out);
                            cursor += w + DIVIDER;
                        }
                    }
                    Orientation::TopBottom => {
                        let usable = (bounds.h - gutters).max(0.0);
                        let mut cursor = bounds.y;
                        for child in children {
                            let h = usable * (child.weight / total);
                            Self::collect_tab_bars(&child.node, Rect::new(bounds.x, cursor, bounds.w, h), out);
                            cursor += h + DIVIDER;
                        }
                    }
                }
            }
            Node::Tabs { children, active } => {
                // Divide the strip width equally among the tabs.
                let n = children.len().max(1);
                let segw = bounds.w / n as f32;
                let tabs = children
                    .iter()
                    .enumerate()
                    .map(|(i, ch)| Tab {
                        rect: Rect::new(bounds.x + i as f32 * segw, bounds.y, segw, TAB_STRIP),
                        first_pane: Self::first_leaf(ch).unwrap_or(PaneId(0)),
                        active: i == *active,
                        number: i + 1,
                    })
                    .collect();
                out.push(TabBar { tabs });
                // Recurse into the active tab's body (below the strip).
                if let Some(child) = children.get(*active) {
                    let body = Rect::new(bounds.x, bounds.y + TAB_STRIP, bounds.w, (bounds.h - TAB_STRIP).max(0.0));
                    Self::collect_tab_bars(child, body, out);
                }
            }
        }
    }

    /// Locate a draggable divider at point `(px, py)`, returning a handle the
    /// caller keeps for the duration of a drag. Only binary splits (all rt ever
    /// creates) are resizable. `None` if the point isn't on a divider.
    pub fn divider_at(&self, px: f32, py: f32, bounds: Rect) -> Option<DragHandle> {
        let mut path = Vec::new();
        Self::find_divider(&self.root, bounds, px, py, &mut path)
    }

    /// Recursive worker for [`Tree::divider_at`], tracking the child-index path
    /// to the split so the caller can resize it later.
    fn find_divider(node: &Node, bounds: Rect, px: f32, py: f32, path: &mut Vec<usize>) -> Option<DragHandle> {
        match node {
            Node::Leaf(_) => None,
            Node::Split { orient, children } => {
                let total: f32 = children.iter().map(|c| c.weight).sum();
                let total = if total > 0.0 { total } else { 1.0 };
                let n = children.len();
                let gutters = DIVIDER * (n.saturating_sub(1) as f32);
                let horizontal = matches!(orient, Orientation::LeftRight);
                let (axis_start, axis_len) = if horizontal { (bounds.x, bounds.w) } else { (bounds.y, bounds.h) };
                let usable = (axis_len - gutters).max(0.0);
                // Walk the children, remembering each child's rect for recursion
                // and testing the gutter after each (binary splits only).
                let mut cursor = axis_start;
                let mut rects = Vec::with_capacity(n);
                for (i, child) in children.iter().enumerate() {
                    let seg = usable * (child.weight / total);
                    let rect = if horizontal {
                        Rect::new(cursor, bounds.y, seg, bounds.h)
                    } else {
                        Rect::new(bounds.x, cursor, bounds.w, seg)
                    };
                    rects.push(rect);
                    cursor += seg;
                    if i < n - 1 {
                        // Expand the grab zone a few px beyond the visual gutter
                        // so the divider is easy to grab (6px alone is too fiddly).
                        const GRAB: f32 = 5.0;
                        let g = if horizontal {
                            Rect::new(cursor - GRAB, bounds.y, DIVIDER + 2.0 * GRAB, bounds.h)
                        } else {
                            Rect::new(bounds.x, cursor - GRAB, bounds.w, DIVIDER + 2.0 * GRAB)
                        };
                        if n == 2 && g.contains(px, py) {
                            return Some(DragHandle { path: path.clone(), horizontal, start: axis_start, len: axis_len });
                        }
                        cursor += DIVIDER;
                    }
                }
                // Not on this split's gutter — descend into the children.
                for (i, child) in children.iter().enumerate() {
                    path.push(i);
                    if let Some(h) = Self::find_divider(&child.node, rects[i], px, py, path) {
                        return Some(h);
                    }
                    path.pop();
                }
                None
            }
            Node::Tabs { children, active } => {
                let child = children.get(*active)?;
                let body = Rect::new(bounds.x, bounds.y + TAB_STRIP, bounds.w, (bounds.h - TAB_STRIP).max(0.0));
                path.push(*active);
                let r = Self::find_divider(child, body, px, py, path);
                path.pop();
                r
            }
        }
    }

    /// Set the split at `path` to the given first-child `ratio` (0..1). No-op if
    /// the path doesn't lead to a binary split. Used while dragging a divider.
    pub fn set_split_ratio(&mut self, path: &[usize], ratio: f32) {
        let mut node = &mut self.root;
        for &i in path {
            node = match node {
                Node::Split { children, .. } if i < children.len() => &mut children[i].node,
                Node::Tabs { children, .. } if i < children.len() => &mut children[i],
                _ => return, // path no longer valid
            };
        }
        if let Node::Split { children, .. } = node {
            if children.len() == 2 {
                let r = ratio.clamp(0.05, 0.95); // keep both panes usable
                children[0].weight = r;
                children[1].weight = 1.0 - r;
            }
        }
    }

    /// Record the child-index path from the root down to `pane` in `path`,
    /// returning whether it was found. Each element indexes into a `Split`'s or
    /// `Tabs`' children in order; the empty path means the root itself is the
    /// leaf. Used by [`Tree::resize`] and [`Tree::rotate`] to locate a pane's
    /// enclosing splits.
    fn path_to(node: &Node, pane: PaneId, path: &mut Vec<usize>) -> bool {
        match node {
            Node::Leaf(id) => *id == pane, // found it (path holds the route)
            Node::Split { children, .. } => {
                for (i, c) in children.iter().enumerate() {
                    path.push(i); // descend into child i
                    if Self::path_to(&c.node, pane, path) {
                        return true;
                    }
                    path.pop(); // backtrack
                }
                false
            }
            Node::Tabs { children, .. } => {
                for (i, c) in children.iter().enumerate() {
                    path.push(i);
                    if Self::path_to(c, pane, path) {
                        return true;
                    }
                    path.pop();
                }
                false
            }
        }
    }

    /// Borrow the node reached by following `path` from the root, or `None` if the
    /// path is stale. The mutable analogue of the walk in
    /// [`Tree::set_split_ratio`], shared by resize/rotate.
    fn node_at_mut(&mut self, path: &[usize]) -> Option<&mut Node> {
        let mut node = &mut self.root;
        for &i in path {
            node = match node {
                Node::Split { children, .. } if i < children.len() => &mut children[i].node,
                Node::Tabs { children, .. } if i < children.len() => &mut children[i],
                _ => return None, // path no longer valid
            };
        }
        Some(node)
    }

    /// Grow the focused pane toward `dir` by `step` (a fraction of the enclosing
    /// split's extent), shrinking the neighbour it moves into. Finds the deepest
    /// ancestor split whose axis matches `dir`, so e.g. resizing Right adjusts the
    /// nearest left/right split. No-op (returns `false`) if the pane is unknown,
    /// there is no matching split, or there is no neighbour on that side.
    pub fn resize(&mut self, pane: PaneId, dir: Direction, step: f32) -> bool {
        let mut path = Vec::new();
        if !Self::path_to(&self.root, pane, &mut path) {
            return false; // unknown pane
        }
        // The split axis this direction adjusts.
        let want = match dir {
            Direction::Left | Direction::Right => Orientation::LeftRight,
            Direction::Up | Direction::Down => Orientation::TopBottom,
        };
        // Walk the path, remembering the deepest split whose orientation matches.
        let mut best: Option<usize> = None; // number of edges from root to that split
        {
            let mut node = &self.root;
            for depth in 0..path.len() {
                if let Node::Split { orient, .. } = node {
                    if *orient == want {
                        best = Some(depth); // a candidate; keep the deepest
                    }
                }
                node = match node {
                    Node::Split { children, .. } => &children[path[depth]].node,
                    Node::Tabs { children, .. } => &children[path[depth]],
                    _ => break,
                };
            }
        }
        let Some(depth) = best else { return false }; // no split on this axis
        let ci = path[depth]; // which child of that split holds our pane
        let Some(node) = self.node_at_mut(&path[..depth]) else { return false };
        if let Node::Split { children, .. } = node {
            let n = children.len();
            // Choose the neighbour we push against: toward the end for Right/Down,
            // toward the start for Left/Up.
            let neighbor = match dir {
                Direction::Right | Direction::Down => (ci + 1 < n).then(|| ci + 1),
                Direction::Left | Direction::Up => (ci > 0).then(|| ci - 1),
            };
            let Some(nb) = neighbor else { return false }; // pane is at the edge
            let total: f32 = children.iter().map(|c| c.weight).sum(); // current weight sum
            let d = step * total; // move this much weight across the boundary
            // Only apply if the neighbour stays usable (≥5% of the split).
            if children[nb].weight - d >= 0.05 * total {
                children[ci].weight += d; // focused pane grows
                children[nb].weight -= d; // neighbour shrinks by the same
                return true;
            }
        }
        false
    }

    /// Rotate the split that directly contains `pane` a quarter-turn
    /// counter-clockwise, as a rigid body: the split flips its axis *and* every
    /// descendant split rotates with it, so the whole region turns like a
    /// picture (top→left, right→top, bottom→right, left→bottom) instead of only
    /// the outer divider flipping. rt's take on Terminator's rotate. No-op if
    /// the pane is unknown or its immediate parent is not a split (e.g. a lone
    /// root leaf, or a direct tab child).
    ///
    /// Focusing a top-level pane thus rotates the whole window; focusing a pane
    /// deeper in the tree rotates just its enclosing sub-region.
    pub fn rotate(&mut self, pane: PaneId) -> bool {
        let mut path = Vec::new();
        if !Self::path_to(&self.root, pane, &mut path) || path.is_empty() {
            return false; // unknown, or the root leaf has no parent split
        }
        let parent = &path[..path.len() - 1]; // drop the last edge → the parent node
        if let Some(node) = self.node_at_mut(parent) {
            if matches!(node, Node::Split { .. }) {
                Self::rotate_ccw(node);
                return true;
            }
        }
        false
    }

    /// Rotate a subtree 90° counter-clockwise, in place. Each split flips its
    /// orientation; a `LeftRight` split additionally reverses its child order
    /// (its left child rotates down to the bottom, its right child up to the
    /// top), whereas a `TopBottom` split keeps order (top→left, bottom→right).
    /// Each child's `weight` rides along with its slot. Recurses through every
    /// descendant split; leaves and tab groups rotate as opaque units.
    ///
    /// Four applications are the identity (a full turn), and the transform is
    /// area-preserving: no pane is created, dropped, or resized in absolute
    /// terms — it is exactly the geometric picture-rotation.
    fn rotate_ccw(node: &mut Node) {
        if let Node::Split { orient, children } = node {
            for child in children.iter_mut() {
                Self::rotate_ccw(&mut child.node);
            }
            match *orient {
                Orientation::TopBottom => *orient = Orientation::LeftRight,
                Orientation::LeftRight => {
                    *orient = Orientation::TopBottom;
                    children.reverse();
                }
            }
        }
    }

    /// The gutter rectangles *between* split children, for the given window
    /// `bounds`, so the renderer can draw visible pane dividers. Each rect is one
    /// full gutter (DIVIDER wide/tall); vertical gutters have `w < h`, horizontal
    /// ones `w > h`.
    pub fn dividers(&self, bounds: Rect) -> Vec<Rect> {
        let mut out = Vec::new();
        Self::collect_dividers(&self.root, bounds, &mut out);
        out
    }

    /// Recursive worker for [`Tree::dividers`]; mirrors the rectangle division of
    /// [`Tree::layout_node`], emitting a gutter after each child but the last.
    fn collect_dividers(node: &Node, bounds: Rect, out: &mut Vec<Rect>) {
        match node {
            Node::Leaf(_) => {}
            Node::Split { orient, children } => {
                let total: f32 = children.iter().map(|c| c.weight).sum();
                let total = if total > 0.0 { total } else { 1.0 };
                let gutters = DIVIDER * (children.len().saturating_sub(1) as f32);
                let last = children.len().saturating_sub(1);
                match orient {
                    Orientation::LeftRight => {
                        let usable = (bounds.w - gutters).max(0.0);
                        let mut cursor = bounds.x;
                        for (i, child) in children.iter().enumerate() {
                            let w = usable * (child.weight / total);
                            Self::collect_dividers(&child.node, Rect::new(cursor, bounds.y, w, bounds.h), out);
                            cursor += w;
                            if i < last {
                                out.push(Rect::new(cursor, bounds.y, DIVIDER, bounds.h)); // vertical gutter
                                cursor += DIVIDER;
                            }
                        }
                    }
                    Orientation::TopBottom => {
                        let usable = (bounds.h - gutters).max(0.0);
                        let mut cursor = bounds.y;
                        for (i, child) in children.iter().enumerate() {
                            let h = usable * (child.weight / total);
                            Self::collect_dividers(&child.node, Rect::new(bounds.x, cursor, bounds.w, h), out);
                            cursor += h;
                            if i < last {
                                out.push(Rect::new(bounds.x, cursor, bounds.w, DIVIDER)); // horizontal gutter
                                cursor += DIVIDER;
                            }
                        }
                    }
                }
            }
            Node::Tabs { children, active } => {
                if let Some(child) = children.get(*active) {
                    let body = Rect::new(bounds.x, bounds.y + TAB_STRIP, bounds.w, (bounds.h - TAB_STRIP).max(0.0));
                    Self::collect_dividers(child, body, out);
                }
            }
        }
    }

    /// The first leaf pane found in `node` (depth-first, following active tabs).
    /// Returns `None` for the empty sentinel or an empty subtree.
    fn first_leaf(node: &Node) -> Option<PaneId> {
        match node {
            Node::Leaf(id) if *id != PaneId(u64::MAX) => Some(*id),
            Node::Leaf(_) => None,
            Node::Split { children, .. } => children.iter().find_map(|c| Self::first_leaf(&c.node)),
            Node::Tabs { children, active } => children.get(*active).and_then(Self::first_leaf),
        }
    }

    /// Whether `pane` appears anywhere in `node`'s subtree.
    fn contains(node: &Node, pane: PaneId) -> bool {
        match node {
            Node::Leaf(id) => *id == pane,
            Node::Split { children, .. } => children.iter().any(|c| Self::contains(&c.node, pane)),
            Node::Tabs { children, .. } => children.iter().any(|c| Self::contains(c, pane)),
        }
    }

    /// Make the tab whose first leaf is `first_pane` the active tab of its
    /// group. Returns `true` if such a tab was found. Used when a tab is
    /// clicked (its `first_pane` comes from [`Tree::tab_bars`]).
    pub fn activate_tab(&mut self, first_pane: PaneId) -> bool {
        Self::activate_node(&mut self.root, first_pane)
    }

    /// Recursive worker for [`Tree::activate_tab`].
    fn activate_node(node: &mut Node, target: PaneId) -> bool {
        match node {
            Node::Leaf(_) => false,
            Node::Split { children, .. } => children.iter_mut().any(|c| Self::activate_node(&mut c.node, target)),
            Node::Tabs { children, active } => {
                // Is `target` the first leaf of one of this group's tabs?
                for i in 0..children.len() {
                    if Self::first_leaf(&children[i]) == Some(target) {
                        *active = i; // select that tab
                        return true;
                    }
                }
                // Otherwise recurse into the tab pages (nested Tabs).
                children.iter_mut().any(|c| Self::activate_node(c, target))
            }
        }
    }

    /// Cycle the tab group *containing the focused pane* by `delta` (wrapping),
    /// returning the first pane of the newly-active tab (to move focus into it),
    /// or `None` if the focus is not inside any `Tabs` node. The innermost
    /// enclosing group is cycled.
    pub fn cycle_tab(&mut self, focused: PaneId, delta: isize) -> Option<PaneId> {
        Self::cycle_node(&mut self.root, focused, delta)
    }

    /// Recursive worker for [`Tree::cycle_tab`].
    fn cycle_node(node: &mut Node, pane: PaneId, delta: isize) -> Option<PaneId> {
        match node {
            Node::Leaf(_) => None,
            Node::Split { children, .. } => {
                for c in children.iter_mut() {
                    if let Some(p) = Self::cycle_node(&mut c.node, pane, delta) {
                        return Some(p);
                    }
                }
                None
            }
            Node::Tabs { children, active } => {
                let a = *active;
                // Prefer an inner tab group (innermost wins) before this one.
                if let Some(p) = Self::cycle_node(&mut children[a], pane, delta) {
                    return Some(p);
                }
                // The focused pane is under this group's active tab: cycle here.
                if Self::contains(&children[a], pane) {
                    let n = children.len() as isize;
                    *active = ((a as isize + delta).rem_euclid(n.max(1))) as usize;
                    return Self::first_leaf(&children[*active]);
                }
                None
            }
        }
    }
}

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

/// Result of removing a pane from a subtree, used by [`Tree::remove_from`].
///
/// * `NotFound(node)` — the target was not in this subtree; the node is handed
///   back unchanged (we moved it out to recurse, so we must return it).
/// * `Replaced(Some(node))` — the target was removed; `node` is what this
///   subtree collapsed to.
/// * `Replaced(None)` — the target was removed and this subtree is now empty;
///   the parent should drop the slot entirely.
enum Removal {
    NotFound(Node),
    Replaced(Option<Node>),
}

impl Default for Tree {
    /// A default `Tree` is a single-pane tree (id 0), matching [`Tree::new`].
    /// Provided so `Tree` slots into `#[derive(Default)]` structs cleanly.
    fn default() -> Self {
        Tree::new().0 // discard the returned first-pane id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a split node from `(weight, child)` pairs, for terse test trees.
    fn split(orient: Orientation, kids: Vec<(f32, Node)>) -> Node {
        Node::Split {
            orient,
            children: kids.into_iter().map(|(weight, node)| Child { weight, node }).collect(),
        }
    }

    /// The scenario from the feature request: a square split top/bottom, the
    /// bottom half split left/right. A rotate must turn the *whole picture* a
    /// quarter-turn CCW, recursing into the nested split:
    ///   TopBottom[ TOP, LeftRight[BL, BR] ]  →  LeftRight[ TOP, TopBottom[BR, BL] ]
    /// i.e. top→left, bottom-right→top-right, bottom-left→bottom-right.
    #[test]
    fn rotate_ccw_recurses_like_a_picture() {
        let (top, bl, br) = (PaneId(1), PaneId(2), PaneId(3));
        let root = split(
            Orientation::TopBottom,
            vec![
                (2.0, Node::Leaf(top)), // big top pane
                (1.0, split(Orientation::LeftRight, vec![(1.0, Node::Leaf(bl)), (1.0, Node::Leaf(br))])),
            ],
        );
        let mut tree = Tree { root };

        // Focusing TOP (a direct child of root) rotates the whole window.
        assert!(tree.rotate(top));

        let Node::Split { orient, children } = &tree.root else { panic!("root is not a split") };
        assert_eq!(*orient, Orientation::LeftRight, "outer split flipped to side-by-side");
        assert!(matches!(children[0].node, Node::Leaf(id) if id == top), "TOP moved to the left");
        assert_eq!(children[0].weight, 2.0, "TOP kept its size weight");

        let Node::Split { orient: inner, children: kids } = &children[1].node else {
            panic!("right child is not a split")
        };
        assert_eq!(*inner, Orientation::TopBottom, "inner split flipped to stacked");
        assert!(matches!(kids[0].node, Node::Leaf(id) if id == br), "bottom-right rotated to top-right");
        assert!(matches!(kids[1].node, Node::Leaf(id) if id == bl), "bottom-left rotated to bottom-right");
    }

    /// A rotation has order four: four quarter-turns are the identity. This
    /// guards against accidentally reversing the wrong axis or dropping weights.
    #[test]
    fn rotate_four_times_is_identity() {
        let (a, b, c, d) = (PaneId(1), PaneId(2), PaneId(3), PaneId(4));
        let root = split(
            Orientation::LeftRight,
            vec![
                (3.0, Node::Leaf(a)),
                (
                    1.0,
                    split(
                        Orientation::TopBottom,
                        vec![
                            (1.0, Node::Leaf(b)),
                            (2.0, split(Orientation::LeftRight, vec![(1.0, Node::Leaf(c)), (1.0, Node::Leaf(d))])),
                        ],
                    ),
                ),
            ],
        );
        let mut tree = Tree { root };
        let before = format!("{:?}", tree.root);
        for _ in 0..4 {
            assert!(tree.rotate(a)); // `a` stays a direct child of the rotating root
        }
        assert_eq!(format!("{:?}", tree.root), before, "four quarter-turns must equal the identity");
    }

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
}
