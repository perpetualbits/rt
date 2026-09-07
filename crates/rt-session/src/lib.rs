//! `rt-session` — the controller that makes rt behave like Terminator.
//!
//! It owns the [`Tree`](rt_core::Tree) layout, one *backend* per leaf pane, the
//! current focus, and the broadcast mode, and it turns semantic
//! [`Action`](rt_config::Action)s (produced by the keymap) into concrete
//! changes: spawn/close panes, move focus spatially, fan typed input out to
//! groups. All of this is pure control flow over data structures, so it is
//! unit-tested headlessly by substituting a mock [`Backend`] for the real PTY.
//!
//! This layer is the direct answer to Terminator's crash class: there is a
//! single owner (`Session`) that mutates the tree and the pane map together,
//! with no deferred callbacks and no widget reparenting, so a pane can never be
//! used after it is closed.

use std::collections::HashMap; // pane_id -> backend, and pane_id -> group

use rt_config::Action; // the semantic actions we dispatch on
use rt_core::{Direction, Orientation, PaneId, Rect, TabBar, Tree}; // the layout model

/// Abstraction over a pane's terminal backend so the controller is testable
/// without spawning real shells. The real implementation is
/// `rt_engine::TermPane`; tests use an in-memory mock.
pub trait Backend {
    /// Write raw bytes (typed input, pasted text) to this pane's PTY.
    fn write(&self, bytes: &[u8]);
    /// Resize this pane to `cols` × `rows` character cells.
    fn resize(&mut self, cols: usize, rows: usize);
    /// Apply a colour palette to this pane (for live colour-scheme changes).
    fn set_palette(&mut self, palette: rt_engine::Palette);
    /// Whether this pane's app enabled bracketed paste (DECSET 2004). A paste must be
    /// wrapped in `\x1b[200~`…`\x1b[201~` for panes where this is true and sent raw where it
    /// is false — decided PER PANE, since a broadcast paste can reach panes in either state.
    fn bracketed_paste(&self) -> bool;
}

// Bridge the real engine pane into the `Backend` trait. This is the only place
// rt-session touches rt-engine's concrete type; everything else is generic.
impl Backend for rt_engine::TermPane {
    fn write(&self, bytes: &[u8]) {
        rt_engine::TermPane::write(self, bytes); // delegate to the inherent method
    }
    fn resize(&mut self, cols: usize, rows: usize) {
        rt_engine::TermPane::resize(self, cols, rows); // delegate
    }
    fn set_palette(&mut self, palette: rt_engine::Palette) {
        rt_engine::TermPane::set_palette(self, palette); // delegate
    }
    fn bracketed_paste(&self) -> bool {
        rt_engine::TermPane::bracketed_paste(self) // delegate to the engine pane's state
    }
}

/// Maximum newspaper columns a single pane may be split into. A soft cap that
/// keeps each column wide enough to be readable.
pub const MAX_COLUMNS: u16 = 8;

/// Cells of horizontal gap drawn between adjacent newspaper columns.
const COL_GAP: usize = 2;

/// The computed geometry of a pane's newspaper-column view, shared by the
/// controller (to size the PTY) and the renderer (to place text). When
/// `count == 1` this describes an ordinary single-column pane and `col_cells`
/// equals the pane's full width.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ColumnLayout {
    /// Number of newspaper columns (>= 1; 1 means a normal pane).
    pub count: u16,
    /// Width of one column in character cells (the PTY runs at this width).
    pub col_cells: usize,
    /// Height of the pane in character rows (every column is this tall).
    pub rows: usize,
    /// Gap between columns in character cells.
    pub gap: usize,
}

impl ColumnLayout {
    /// Inverse of the renderer's column tiling: map a point `(dx, dy)` in cells,
    /// measured from the pane's content top-left, to the `(col, row)` cell in the
    /// tall `count * rows` viewport the app actually sees. The renderer places
    /// grid line `r` at column `r / rows`, sub-row `r % rows`, with each column
    /// `col_cells` wide followed by a `gap`; this reverses that. Clamps into
    /// range (a point in a gap snaps to the adjacent column's edge), so mouse
    /// forwarding into a newspaper-column pane always yields a valid cell.
    pub fn cell_at(&self, dx: f32, dy: f32) -> (usize, usize) {
        let step = (self.col_cells + self.gap) as f32; // cells between column origins
        let dx = dx.max(0.0);
        let k = ((dx / step) as usize).min(self.count.saturating_sub(1) as usize); // which column
        let col = ((dx - k as f32 * step) as usize).min(self.col_cells.saturating_sub(1));
        let sub = (dy.max(0.0) as usize).min(self.rows.saturating_sub(1)); // row within the column
        (col, k * self.rows + sub)
    }
}

/// How typed input fans out to other panes — rt's port of Terminator's
/// input broadcast / grouping feature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Broadcast {
    /// Input goes only to the focused pane (the normal default).
    #[default]
    Off,
    /// Input goes to every pane sharing the focused pane's group — INCLUDING
    /// panes in other windows of this process. `Session` only ever delivers
    /// to its own panes (this fans out locally, see `feed_input`); reaching
    /// a group member that was torn out into a separate window is `App`'s
    /// job (`main.rs`'s `broadcast_group_input`/`broadcast_group_paste`),
    /// via `write_to_group`/`paste_to_group` below. In-process only.
    Group,
    /// Input goes to every pane in THIS window. Deliberately NOT extended
    /// across windows like `Group` is: "every pane in every window" is a
    /// blast radius the typist can't see, so `All` keeps meaning "every pane
    /// here" — do not "fix" this into matching `Group`'s reach.
    All,
}

/// Something the controller wants the GUI shell to do in response to an action
/// that it cannot perform itself (it owns no window handle).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionEvent {
    /// The last pane was closed (or `close_window` pressed); close the window.
    CloseWindow,
    /// The clipboard should copy the current selection (GUI-owned).
    Copy,
    /// The clipboard contents should be pasted into the focused pane.
    Paste,
    /// Something visible changed; schedule a redraw.
    Redraw,
}

/// The whole state of one window: layout, backends, focus, broadcast, and the
/// pixel geometry needed to size panes.
///
/// Generic over `B: Backend` and a factory `F` that spawns a new backend given
/// a size. The factory indirection is what lets tests inject mock panes while
/// production injects real PTYs.
pub struct Session<B: Backend, F: FnMut(PaneId, usize, usize) -> Option<B>> {
    tree: Tree,                        // the split/tab layout
    panes: HashMap<PaneId, B>,         // one backend per live leaf
    groups: HashMap<PaneId, u32>,      // pane -> group id (for Broadcast::Group)
    columns: HashMap<PaneId, u16>,     // pane -> newspaper column count (absent = 1)
    titles: HashMap<PaneId, String>,   // pane -> latest OSC/shell title (for tab + window titles)
    zoomed: Option<PaneId>,            // if set, this pane is maximised to fill the window
    focus: PaneId,                     // the currently focused pane
    broadcast: Broadcast,              // current input fan-out mode
    bounds: Rect,                      // window content rectangle in pixels
    cell: (f32, f32),                  // (width, height) of one character cell in px
    chrome_scale: f32,                 // display backing factor for the flat pads below (1.0 = as it always was)
    show_titlebar: bool,               // reserve a header strip atop each pane
    spawn: F,                          // factory that creates a new backend
}

/// The side-table entries (backend + group/columns/title metadata) for a set
/// of panes, ejected from one `Session` and injected into another (or the
/// same one) alongside their `Subtree`. Kept separate from `Subtree` because
/// the tree shape lives in `rt-core` and knows nothing about backends.
pub struct PaneEntries<B> {
    pub panes: Vec<(PaneId, B)>,
    pub groups: Vec<(PaneId, u32)>,
    pub columns: Vec<(PaneId, u16)>,
    pub titles: Vec<(PaneId, String)>,
}

/// A self-contained bundle of layout (`Subtree`) plus every side-table entry
/// for the panes it contains — the unit that moves between windows (or within
/// one) on a pane/tab drag. Never dropped on a failed adopt: a failure hands
/// the whole package back so the pane is never lost.
pub struct PanePackage<B> {
    pub sub: rt_core::Subtree,
    pub entries: PaneEntries<B>,
}

// Manual (not derived) so `Result<(), PanePackage<B>>::unwrap()`/`expect_err()`
// works in tests without requiring `B: Debug` — the real backend (a live PTY
// wrapper) has no meaningful Debug representation anyway.
impl<B> std::fmt::Debug for PanePackage<B> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PanePackage")
            .field("sub", &self.sub)
            .field("panes", &self.entries.panes.len())
            .finish()
    }
}

/// Where a `PanePackage` (or a same-window pane/tab) lands.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum DropTarget {
    /// Empty window (tear-out landing).
    Root,
    /// Split beside an existing pane.
    SplitBeside { pane: PaneId, orient: Orientation, before: bool },
    /// Swap slots with an existing pane. `move_pane` only, same window.
    Swap { pane: PaneId },
    /// Insert as a tab at `index` within the tab group anchored at `anchor`.
    TabAt { anchor: PaneId, index: usize },
    /// Insert as a new top-level split at the root's edge.
    RootEdge { orient: Orientation, before: bool },
}

/// Vertical padding added to the cell height to size a per-pane titlebar strip,
/// in LOGICAL pixels.
///
/// Logical, like [`PANE_PAD`] below: the cell height it is added to already
/// doubles on a 2x display (the glyph is rasterised at the scaled size), so a
/// pad that stayed at a flat 4 physical px would shrink to half its intended
/// share of the strip. Every use multiplies it by the session's chrome scale —
/// [`Session::set_chrome_scale`], or the `scale` argument of [`pane_chrome`].
const TITLEBAR_PAD: f32 = 4.0;

/// Inner padding between a pane's edge and its terminal text, in LOGICAL pixels,
/// so the pane border / heat tint / latency frame never overlap the first
/// characters. It is also the gutter `rt` draws the scrollbar in — and `rt`
/// scales the scrollbar by the same factor, so the fit holds at every scale.
const PANE_PAD: f32 = 5.0;

/// The pixel overhead `(horizontal, vertical)` that [`Session::content_rect`]
/// removes from a pane's rectangle: twice the inner padding across, and the same
/// plus the titlebar strip down. Inverting it lets the GUI pre-size a window so a
/// single full-window pane comes out to an exact cols×rows grid (the `--cols` /
/// `--rows` startup flags). Context-free on purpose — `main` calls it before any
/// `Session` exists. Must stay in lockstep with `content_rect`/`titlebar_h`.
pub fn pane_chrome(cell: (f32, f32), show_titlebar: bool, scale: f32) -> (f32, f32) {
    let pad = scale * PANE_PAD;
    let titlebar = if show_titlebar { cell.1 + scale * TITLEBAR_PAD } else { 0.0 };
    (2.0 * pad, 2.0 * pad + titlebar)
}

impl<B: Backend, F: FnMut(PaneId, usize, usize) -> Option<B>> Session<B, F> {
    /// Create a session with a single initial pane filling `bounds`.
    ///
    /// `cell` is the pixel size of one character cell, used to convert pane
    /// pixel rectangles into terminal (cols, rows). `spawn` is invoked once here
    /// to create the first pane's backend.
    pub fn new(bounds: Rect, cell: (f32, f32), mut spawn: F) -> Self {
        let (tree, first) = Tree::new(); // start with one leaf pane
        // Size the first backend to fill the window.
        let (cols, rows) = cells_in(bounds, cell); // full-window cell dimensions
        let mut panes = HashMap::new(); // the pane->backend table
        // The first pane is required for the app to run at all; a failure here is a
        // clean startup error, not a mid-session crash of other panes.
        let first_backend = spawn(first, cols, rows)
            .expect("failed to spawn the initial pane's PTY/shell — out of ptys or file descriptors?");
        panes.insert(first, first_backend); // register pane 0
        Session {
            tree,
            panes,
            groups: HashMap::new(), // no groups assigned initially
            columns: HashMap::new(), // every pane starts single-column
            titles: HashMap::new(),  // no titles until the shell sets one
            zoomed: None,            // no pane maximised initially
            focus: first,           // focus starts on the only pane
            broadcast: Broadcast::Off,
            bounds,
            cell,
            // 1.0 until the GUI reports the display's factor (it does so before the
            // first relayout); 1.0 is exactly the pre-HiDPI behaviour.
            chrome_scale: 1.0,
            show_titlebar: false, // off until the GUI enables it from settings
            spawn,
        }
    }

    /// The currently focused pane id (used by the renderer to draw the focus
    /// highlight and by input routing).
    pub fn focus(&self) -> PaneId {
        self.focus
    }

    /// The current broadcast mode (renderer may show an indicator).
    pub fn broadcast(&self) -> Broadcast {
        self.broadcast
    }

    /// The group id a pane belongs to, if any (0 = ungrouped/None). The renderer
    /// draws a small colour-coded marker per group so membership is visible even
    /// without per-pane titlebars.
    pub fn group_of(&self, id: PaneId) -> Option<u32> {
        self.groups.get(&id).copied()
    }

    /// Cycle the focused pane through group ids: ungrouped → 1 → 2 → … → MAX →
    /// ungrouped. This is rt's title-bar-free way to build Terminator-style
    /// groups; combined with `Broadcast::Group`, typing then fans out to every
    /// pane sharing the focus's group. Returns `Redraw` so the marker updates.
    fn cycle_group(&mut self) -> Option<SessionEvent> {
        const MAX_GROUP: u32 = 4; // a small, colour-distinguishable set of groups
        let next = match self.groups.get(&self.focus).copied() {
            None => Some(1),                       // ungrouped → group 1
            Some(g) if g < MAX_GROUP => Some(g + 1), // advance to the next group
            Some(_) => None,                       // past the last → back to ungrouped
        };
        match next {
            Some(g) => {
                self.groups.insert(self.focus, g); // join group g
            }
            None => {
                self.groups.remove(&self.focus); // leave all groups
            }
        }
        Some(SessionEvent::Redraw)
    }

    /// Immutable access to the layout tree (the renderer calls `.rects(...)`).
    pub fn tree(&self) -> &Tree {
        &self.tree
    }

    /// Every pane inside the tab page anchored at `first_pane` (the id a
    /// [`rt_core::Tab`] carries), or `None` if no page is anchored there. Drag
    /// -and-drop asks this so a dragged tab knows its own panes.
    pub fn tab_panes(&self, first_pane: PaneId) -> Option<Vec<PaneId>> {
        self.tree.tab_panes(first_pane)
    }

    /// Whether a pane is currently maximised (zoomed to fill the window).
    pub fn is_zoomed(&self) -> bool {
        self.zoomed.is_some()
    }

    /// Toggle maximising the focused pane: when zoomed, that pane fills the
    /// window and its siblings/dividers/tab-strips are hidden. Toggling again
    /// restores the layout.
    pub fn toggle_zoom(&mut self) {
        self.zoomed = if self.zoomed.is_some() { None } else { Some(self.focus) };
        self.relayout(self.bounds); // resize the (un)zoomed pane(s)
    }

    /// The panes to actually draw for `bounds`: just the zoomed pane (full
    /// window) when zoomed, otherwise the normal layout. The renderer and mouse
    /// hit-testing use this so zoom is respected everywhere.
    pub fn visible_rects(&self, bounds: Rect) -> Vec<(PaneId, Rect)> {
        match self.zoomed {
            Some(z) if self.panes.contains_key(&z) => vec![(z, bounds)],
            _ => self.tree.rects(bounds),
        }
    }

    /// How many newspaper columns pane `id` is showing (1 = a normal pane).
    pub fn columns_of(&self, id: PaneId) -> u16 {
        self.columns.get(&id).copied().unwrap_or(1).max(1) // absent means single-column
    }

    /// Compute the newspaper-column geometry for pane `id` occupying `rect`.
    /// Shared by [`Session::relayout`] (to size the PTY) and by the renderer (to
    /// place each column). For a single-column pane, `col_cells` is the pane's
    /// full width and `rows` the full height.
    ///
    /// Note `rows` is the height of *one* column (= the pane height). In column
    /// mode the PTY is made `count × rows` tall (see [`Session::relayout`]) so
    /// the app sees one tall screen; this `rows` is how the renderer slices that
    /// tall screen back into columns.
    pub fn column_layout(&self, id: PaneId, rect: Rect) -> ColumnLayout {
        let (full_cols, rows) = cells_in(rect, self.cell); // full pane cell dims
        let count = self.columns_of(id); // 1..=MAX_COLUMNS
        let col_cells = if count <= 1 {
            full_cols // ordinary pane: the column *is* the whole pane
        } else {
            // Subtract the inter-column gaps, then split the rest evenly.
            let gap_total = COL_GAP * (count as usize - 1); // cells consumed by gaps
            (full_cols.saturating_sub(gap_total) / count as usize).max(1) // per-column width
        };
        ColumnLayout { count, col_cells, rows, gap: COL_GAP }
    }

    /// Borrow a pane's backend by id (renderer reads snapshots through the
    /// concrete type; this generic accessor is mostly for input routing/tests).
    pub fn pane(&self, id: PaneId) -> Option<&B> {
        self.panes.get(&id)
    }

    /// Record the latest title for pane `id` (from an OSC/shell title event).
    /// An empty title clears it (the app asked to reset to the default).
    pub fn set_title(&mut self, id: PaneId, title: String) {
        if title.is_empty() {
            self.titles.remove(&id); // reset → fall back to a default label
        } else {
            self.titles.insert(id, title);
        }
    }

    /// The current title for pane `id`, if the shell has set one.
    pub fn title_of(&self, id: PaneId) -> Option<&str> {
        self.titles.get(&id).map(String::as_str)
    }

    /// The tab strips to draw/hit-test for the current window `bounds`. Hidden
    /// while a pane is zoomed.
    pub fn tab_bars(&self, bounds: Rect) -> Vec<TabBar> {
        if self.zoomed.is_some() {
            return Vec::new();
        }
        self.tree.tab_bars(bounds)
    }

    /// The divider gutter rectangles between panes. Hidden while zoomed.
    pub fn dividers(&self, bounds: Rect) -> Vec<Rect> {
        if self.zoomed.is_some() {
            return Vec::new();
        }
        self.tree.dividers(bounds)
    }

    /// A draggable divider at `(px, py)`, if any. None while zoomed (no
    /// dividers are shown).
    pub fn divider_at(&self, px: f32, py: f32, bounds: Rect) -> Option<rt_core::DragHandle> {
        if self.zoomed.is_some() {
            return None;
        }
        self.tree.divider_at(px, py, bounds)
    }

    /// Set a split's first-child ratio (while dragging a divider), then reflow
    /// so the affected panes resize their PTYs.
    pub fn set_split_ratio(&mut self, handle: &rt_core::DragHandle, ratio: f32) {
        self.tree.set_split_ratio(&handle.path, ratio);
        self.relayout(self.bounds);
    }

    /// Move a split WITHOUT reflowing the panes — the caller owes a later
    /// [`Session::relayout`].
    ///
    /// For dragging a divider. [`Session::set_split_ratio`] relayouts, and
    /// `relayout` is `Term::resize` per pane, which reflows the grid AND the
    /// whole scrollback (10k lines by default): measured at ~676ms per call on a
    /// milkv (riscv64) with full history. A drag fires one motion event per
    /// pointer sample, so paying it per event made the divider move in ~1s steps.
    /// The tree update alone is cheap, and pane RECTS come from the tree, so the
    /// divider still tracks the pointer live; only the cell grids lag until the
    /// caller reflows once the drag settles (see rt's `RESIZE_SETTLE`).
    pub fn set_split_ratio_no_reflow(&mut self, handle: &rt_core::DragHandle, ratio: f32) {
        self.tree.set_split_ratio(&handle.path, ratio);
    }

    /// Select the tab whose first pane is `first_pane` (from a clicked
    /// [`TabBar`] tab), move focus into it, and reflow. Returns `true` if the
    /// tab was found. This is what a click on a tab label calls.
    pub fn focus_tab(&mut self, first_pane: PaneId) -> bool {
        if self.tree.activate_tab(first_pane) {
            self.focus = first_pane; // focus the newly-shown tab's pane
            self.relayout(self.bounds); // its content region changed
            true
        } else {
            false
        }
    }

    /// Cycle the focused pane's tab group by `delta`, moving focus into the new
    /// active tab. Returns `Redraw` if a tab group was found, else `None`.
    fn cycle_tab_focus(&mut self, delta: isize) -> Option<SessionEvent> {
        match self.tree.cycle_tab(self.focus, delta) {
            Some(pane) => {
                self.focus = pane; // follow the tab switch
                self.relayout(self.bounds); // reflow the newly-active tab
                Some(SessionEvent::Redraw)
            }
            None => None, // focus isn't inside any Tabs node
        }
    }

    /// Move focus to the pane whose rectangle contains the point `(px, py)` (in
    /// the same physical-pixel space as the window bounds). Returns `true` if a
    /// pane was found there (whether or not focus actually changed).
    ///
    /// This is what powers click-to-focus and "the menu acts on the pane you
    /// right-clicked": the GUI calls it on a mouse press with the cursor
    /// position. Only visible panes are considered (inactive tab pages have no
    /// rectangle), which is exactly right — you can't click what you can't see.
    pub fn focus_at(&mut self, px: f32, py: f32) -> bool {
        for (id, rect) in self.visible_rects(self.bounds) {
            if rect.contains(px, py) {
                self.focus = id; // adopt the clicked pane as the focus
                return true;
            }
        }
        false // the point hit no pane (e.g. a divider gutter)
    }

    /// Dispatch a semantic action. Returns an optional [`SessionEvent`] the GUI
    /// shell must handle (close window, clipboard). This is the single entry
    /// point the keymap feeds, keeping all state mutation in one auditable place.
    pub fn apply(&mut self, action: Action) -> Option<SessionEvent> {
        match action {
            // Splits: horizontal accelerator → stacked (TopBottom); vertical
            // accelerator → side-by-side (LeftRight), matching Terminator.
            Action::SplitHoriz => {
                self.split(Orientation::TopBottom); // Ctrl+Shift+O behaviour
                Some(SessionEvent::Redraw)
            }
            Action::SplitVert => {
                self.split(Orientation::LeftRight); // Ctrl+Shift+E behaviour
                Some(SessionEvent::Redraw)
            }
            // Split along the focused pane's longer axis (split_auto).
            Action::SplitAuto => {
                self.split_auto();
                Some(SessionEvent::Redraw)
            }
            // Flip the enclosing split's orientation.
            Action::Rotate => self.rotate_focus(),
            // Keyboard split resize: grow the focused pane toward the arrow.
            Action::ResizeLeft => self.resize_focus(Direction::Left),
            Action::ResizeRight => self.resize_focus(Direction::Right),
            Action::ResizeUp => self.resize_focus(Direction::Up),
            Action::ResizeDown => self.resize_focus(Direction::Down),
            // Cycle the focused pane's input group (Broadcast::Group membership).
            Action::GroupCycle => self.cycle_group(),
            Action::NewTab => {
                self.new_tab(); // open a tab beside the focus
                Some(SessionEvent::Redraw)
            }
            Action::CloseTerm => self.close_pane(self.focus), // may request CloseWindow
            Action::CloseWindow => Some(SessionEvent::CloseWindow), // GUI closes us
            // Directional focus movement; a no-op (no neighbour) still redraws
            // harmlessly but we only redraw if focus actually changed.
            Action::GoUp => self.move_focus(Direction::Up),
            Action::GoDown => self.move_focus(Direction::Down),
            Action::GoLeft => self.move_focus(Direction::Left),
            Action::GoRight => self.move_focus(Direction::Right),
            // Cycle the tab group containing the focused pane, moving focus into
            // the newly-active tab.
            Action::NextTab => self.cycle_tab_focus(1),
            Action::PrevTab => self.cycle_tab_focus(-1),
            // Maximise/restore the focused pane.
            Action::ToggleZoom => {
                self.toggle_zoom();
                Some(SessionEvent::Redraw)
            }
            // Broadcast mode changes: update state, ask for a redraw so any
            // indicator refreshes.
            Action::BroadcastOff => {
                self.broadcast = Broadcast::Off;
                Some(SessionEvent::Redraw)
            }
            Action::BroadcastGroup => {
                self.broadcast = Broadcast::Group;
                Some(SessionEvent::Redraw)
            }
            Action::BroadcastAll => {
                self.broadcast = Broadcast::All;
                Some(SessionEvent::Redraw)
            }
            // Newspaper columns: adjust the focused pane's column count and
            // resize its PTY (relayout uses the per-pane column width).
            Action::ColumnsMore => {
                let n = self.columns.entry(self.focus).or_insert(1); // default single
                *n = (*n + 1).min(MAX_COLUMNS); // add a column, capped
                self.relayout(self.bounds); // PTY now runs at the narrower width
                Some(SessionEvent::Redraw)
            }
            Action::ColumnsFewer => {
                let n = self.columns.entry(self.focus).or_insert(1);
                *n = n.saturating_sub(1).max(1); // remove a column, floor at 1
                self.relayout(self.bounds); // PTY widens back out
                Some(SessionEvent::Redraw)
            }
            // Clipboard is owned by the GUI shell; just forward the intent.
            Action::Copy => Some(SessionEvent::Copy),
            Action::Paste => Some(SessionEvent::Paste),
            // Window-level appearance (background opacity) is owned by the GUI
            // shell, not the session — it holds no window handle. The binary
            // intercepts these before dispatch; this arm keeps `apply` total.
            Action::OpacityUp
            | Action::OpacityDown
            | Action::ToggleFocusFollowsMouse
            | Action::Preferences
            | Action::ZoomIn
            | Action::ZoomOut
            | Action::ZoomReset
            | Action::Fullscreen
            | Action::Search
            | Action::WireStdout
            | Action::WireStderr
            | Action::Unwire
            | Action::PipeInto
            | Action::Manual
            | Action::ClipHistory
            | Action::ClearClipHistory
            // Multi-window pane/tab drag-and-drop: the GUI shell owns window
            // creation/destruction, so it intercepts these before dispatch too.
            // Behaviour lands in Tasks 6-8; this arm just keeps `apply` total.
            | Action::NewWindow
            | Action::DetachPane
            | Action::DetachTab
            | Action::PickUpPane
            | Action::PickUpTab
            | Action::MoveTabLeft
            | Action::MoveTabRight => None,
        }
    }

    /// Apply a colour `palette` to every live pane (a colour-scheme change).
    pub fn set_all_palettes(&mut self, palette: rt_engine::Palette) {
        for p in self.panes.values_mut() {
            p.set_palette(palette.clone());
        }
    }

    /// Route typed input bytes to the appropriate pane(s) according to the
    /// current broadcast mode. This is the port of Terminator's grouped input.
    ///
    /// * `Off`   → only the focused pane.
    /// * `Group` → every pane sharing the focused pane's group id (or just the
    ///   focus if it has no group).
    /// * `All`   → every pane in the window.
    ///
    /// For bytes that are the SAME for every pane (composed IME text, say).
    /// Anything whose encoding depends on what the receiving pane negotiated —
    /// every keystroke — must go through
    /// [`feed_input_with`](Session::feed_input_with) instead; see its doc.
    ///
    /// Targeting is [`receives_broadcast`](Session::receives_broadcast), the
    /// single copy of the fan-out rule (the UI's "your typing goes here"
    /// indicator reads the same function, so the two cannot drift).
    pub fn feed_input(&self, bytes: &[u8]) {
        for (id, p) in &self.panes {
            if self.receives_broadcast(*id) {
                p.write(bytes); // deliver to every target of the current mode
            }
        }
    }

    /// [`feed_input`](Session::feed_input) for input whose BYTES depend on the
    /// receiving pane: `encode` is called once per target pane and may return a
    /// different sequence for each (`None` = nothing to send to that pane).
    ///
    /// A keystroke's encoding is a property of the program in the pane, not of
    /// the keyboard: DECCKM (application cursor keys) decides whether an arrow
    /// is `CSI A` or `SS3 A`, and the kitty keyboard flags decide whether
    /// Shift+Enter is `\r` or `\x1b[13;2u`. Under broadcast one keystroke
    /// reaches panes in different states, so encoding it ONCE from the focused
    /// pane — the old bug, exactly the one [`feed_paste`](Session::feed_paste)'s
    /// doc records for bracketed paste — fed the wrong form to every mismatched
    /// pane: a shell that negotiated nothing got `\x1b[13;2u` where it used to
    /// get `\r`, and `\x1b[99;5u` where it used to get the 0x03 that raises
    /// SIGINT. The whole safety property of the kitty keyboard protocol is that
    /// a program which never negotiated sees byte-identical input to before it
    /// existed, and only a per-pane decision keeps that true under broadcast.
    ///
    /// `Broadcast::Off` has exactly ONE target (the focus), so it makes exactly
    /// ONE call — the common path does not turn into N encodes.
    pub fn feed_input_with(&self, encode: impl Fn(&B) -> Option<Vec<u8>>) {
        for (id, p) in &self.panes {
            if self.receives_broadcast(*id) {
                if let Some(bytes) = encode(p) {
                    p.write(&bytes); // this pane's own encoding of the keystroke
                }
            }
        }
    }

    /// Deliver a clipboard **paste** to the same targets as [`feed_input`](Session::feed_input),
    /// but wrap it in bracketed-paste markers (`\x1b[200~`…`\x1b[201~`) **per pane**, according
    /// to each target pane's own DECSET-2004 state. A broadcast paste can reach panes in either
    /// state, so deciding once from the focused pane (the old bug) fed the wrong form to
    /// mismatched panes — a bracketed-paste app got raw text and treated a pasted key's newlines
    /// as Enter, so line breaks appeared in one pane but not another. The end marker is stripped
    /// from the body first so pasted content can't break out of the bracket (injection guard).
    pub fn feed_paste(&self, text: &[u8]) {
        // Built once and shared by every bracketed-paste target.
        let bracketed = wrap_bracketed_paste(text);
        for (id, p) in &self.panes {
            if self.receives_broadcast(*id) {
                p.write(if p.bracketed_paste() { &bracketed } else { text });
            }
        }
    }

    /// Paste `text` into ONE named pane, bracketed (or not) by **that pane's
    /// own** DECSET-2004 state — [`feed_paste`](Session::feed_paste) narrowed
    /// from "the broadcast targets" to "this pane", sharing the same
    /// [`wrap_bracketed_paste`] and therefore the same injection guard.
    ///
    /// It exists for text dragged in from another application and dropped on a
    /// pane (`rt::textdrop`). That target is chosen by the POINTER, not by the
    /// focus, so `feed_paste` cannot express it: its fan-out is
    /// [`receives_broadcast`](Session::receives_broadcast), which is defined
    /// entirely relative to `self.focus`. A spatial drop onto a pane that is not
    /// the focus has no meaning under that rule without redefining broadcast, so
    /// a drop delivers to exactly the pane it was aimed at and nowhere else.
    ///
    /// This is deliberately NOT a second paste path: the per-pane bracketing
    /// decision — the one `feed_paste`'s doc records a real bug for — is made
    /// here by the same rule from the same helper. Only the target set differs.
    ///
    /// No-op if `id` names no pane in this session (a pane can exit between the
    /// pointer landing on it and the drop being processed).
    pub fn paste_to_pane(&self, id: PaneId, text: &[u8]) {
        if let Some(p) = self.panes.get(&id) {
            if p.bracketed_paste() {
                p.write(&wrap_bracketed_paste(text));
            } else {
                p.write(text);
            }
        }
    }

    /// Whether pane `id` has bracketed paste (DECSET 2004) on, or `None` if it
    /// names no pane here.
    ///
    /// The one legitimate reason to ask instead of just calling
    /// [`paste_to_pane`](Session::paste_to_pane): a dropped payload's BODY
    /// depends on the mode too, not only its wrapper (`rt::textdrop::payload`
    /// flattens line breaks that would otherwise run as commands in a shell that
    /// never negotiated the mode). Caller reads it and delivers in the same
    /// turn, off the same pane, so the two decisions cannot disagree.
    pub fn pane_bracketed_paste(&self, id: PaneId) -> Option<bool> {
        self.panes.get(&id).map(|p| p.bracketed_paste())
    }

    /// Write `bytes` to every pane in THIS session carrying `group` — the
    /// cross-window half of `Broadcast::Group` delivery. `Session` only
    /// knows its own panes (see the module doc), so `App` (in `main.rs`)
    /// calls this once per OPEN WINDOW after resolving the group id from the
    /// window the user is actually typing in, so a pane that was torn out of
    /// its group into a separate window still gets reached. Doesn't touch
    /// `self.focus`/`self.broadcast` — a plain targeted write, same as one
    /// arm of [`feed_input`](Session::feed_input).
    ///
    /// In-process only: a pane can be handed to a DIFFERENT rt process
    /// (`PanePackage`/`adopt`); a group spanning processes would need that
    /// handoff protocol to also carry live delivery (a new IPC path) — out
    /// of scope, and `App` is where that limit would be lifted, not here.
    pub fn write_to_group(&self, group: u32, bytes: &[u8]) {
        for (id, p) in &self.panes {
            if self.groups.get(id).copied() == Some(group) {
                p.write(bytes);
            }
        }
    }

    /// [`write_to_group`](Session::write_to_group) for input whose bytes depend
    /// on the receiving pane — the cross-window half of
    /// [`feed_input_with`](Session::feed_input_with), and the key equivalent of
    /// [`paste_to_group`](Session::paste_to_group).
    ///
    /// The per-pane encode is load-bearing for exactly the reason spelled out on
    /// `feed_input_with`, and crossing a window boundary does not make it less
    /// so: a group member torn out into another window has its own DECCKM state
    /// and its own kitty keyboard flags, negotiated by its own program.
    pub fn write_to_group_with(&self, group: u32, encode: impl Fn(&B) -> Option<Vec<u8>>) {
        for (id, p) in &self.panes {
            if self.groups.get(id).copied() == Some(group) {
                if let Some(bytes) = encode(p) {
                    p.write(&bytes);
                }
            }
        }
    }

    /// Paste equivalent of [`write_to_group`](Session::write_to_group):
    /// same group targeting, but wrapped in bracketed-paste markers PER PANE
    /// exactly like [`feed_paste`](Session::feed_paste) — the per-pane
    /// DECSET-2004 decision is load-bearing (see `feed_paste`'s doc) and
    /// must not collapse into a single decision just because delivery now
    /// crosses windows.
    pub fn paste_to_group(&self, group: u32, text: &[u8]) {
        let bracketed = wrap_bracketed_paste(text);
        for (id, p) in &self.panes {
            if self.groups.get(id).copied() == Some(group) {
                p.write(if p.bracketed_paste() { &bracketed } else { text });
            }
        }
    }

    /// Would a keystroke reach pane `id` right now?
    ///
    /// Mirrors [`Session::feed_input`]'s fan-out exactly, and exists so the UI
    /// cannot drift from it: an indicator that says "your typing goes here" must
    /// be derived from the same rule that decides where typing goes, not from a
    /// second copy of it. Broadcast is a mode where a keystroke hits several
    /// shells at once, so a wrong indicator is worse than none.
    pub fn receives_broadcast(&self, id: PaneId) -> bool {
        match self.broadcast {
            Broadcast::Off => id == self.focus,
            Broadcast::All => true,
            Broadcast::Group => match self.groups.get(&self.focus).copied() {
                // Focus is grouped: everyone sharing its group.
                Some(g) => self.groups.get(&id).copied() == Some(g),
                // Focus is ungrouped: "just me".
                None => id == self.focus,
            },
        }
    }

    /// Assign the focused pane to `group` (a small integer group id). Used to
    /// build broadcast groups; a future GUI exposes this via the group menu.
    pub fn set_group(&mut self, group: u32) {
        self.groups.insert(self.focus, group); // record membership for the focus
    }

    /// Enable or disable the per-pane titlebar strip. Reserving (or freeing) the
    /// header changes every pane's content height, so the caller should
    /// [`relayout`](Session::relayout) afterwards to resize the PTYs.
    pub fn set_show_titlebar(&mut self, on: bool) {
        self.show_titlebar = on;
    }

    /// Height of the per-pane titlebar strip in pixels (0 when disabled). One
    /// text line plus a little padding, so it scales with the font size.
    pub fn titlebar_h(&self) -> f32 {
        if self.show_titlebar {
            self.cell.1 + self.chrome_scale * TITLEBAR_PAD
        } else {
            0.0
        }
    }

    /// The content rectangle of a pane whose full rectangle is `rect`: the box
    /// minus the titlebar strip at its top and a small inner padding on every
    /// side (so the border / heat tint / latency frame never sit on the text).
    /// This is the single definition of "where a pane's terminal grid lives";
    /// both the layout (PTY sizing) and the renderer (drawing/hit-testing) route
    /// through it so nothing can desync the grid from what the mouse hits.
    pub fn content_rect(&self, rect: Rect) -> Rect {
        let p = self.chrome_scale * PANE_PAD;
        let top = self.titlebar_h() + p; // titlebar (0 if off) + top padding
        Rect::new(
            rect.x + p,
            rect.y + top,
            (rect.w - 2.0 * p).max(0.0),
            (rect.h - top - p).max(0.0),
        )
    }

    /// Update the character-cell pixel size (after a font change) so subsequent
    /// [`Session::relayout`] converts pane rectangles to (cols, rows) correctly.
    pub fn set_cell(&mut self, cell: (f32, f32)) {
        self.cell = cell;
    }

    /// Report the display's backing factor, so the flat chrome this session owns
    /// — the pane padding, the titlebar strip's pad, and (forwarded to the tree)
    /// the split gutters, the tab strips and the divider grab band — comes out
    /// the right APPARENT size on a HiDPI panel.
    ///
    /// A plain number rather than a window handle on purpose: `rt-session` knows
    /// nothing about winit and must keep it that way. The GUI reads
    /// `Window::scale_factor()`, sanitises it (`rt`'s `chrome_scale`), and pushes
    /// the result down here. Call it BEFORE the relayout that should use it —
    /// [`Session::content_rect`] and the tree's layout both read it.
    ///
    /// Ignores a factor that is not a usable number: keeping the last good scale
    /// beats laying every pane out at zero.
    pub fn set_chrome_scale(&mut self, scale: f32) {
        if scale.is_finite() && scale > 0.0 {
            self.chrome_scale = scale;
            self.tree.set_chrome_scale(scale); // gutters, tab strips, divider grab
        }
    }

    /// The factor [`Session::set_chrome_scale`] last accepted (1.0 by default).
    pub fn chrome_scale(&self) -> f32 {
        self.chrome_scale
    }

    /// Recompute every pane's (cols, rows) from the current tree layout and
    /// window bounds, resizing each backend. Called by the GUI on window resize
    /// or font change. Panes on inactive tabs (absent from `rects`) keep their
    /// last size until shown.
    pub fn relayout(&mut self, bounds: Rect) {
        self.bounds = bounds; // remember the new window size
        // When zoomed, only the maximised pane is sized (to the full window,
        // minus its titlebar strip).
        if let Some(z) = self.zoomed {
            let layout = self.column_layout(z, self.content_rect(bounds));
            if let Some(p) = self.panes.get_mut(&z) {
                p.resize(layout.col_cells, (layout.rows * layout.count as usize).max(1));
            }
            return;
        }
        for (id, rect) in self.tree.rects(bounds) {
            // Column mode makes the PTY ONE column WIDE and `count` columns
            // TALL (count*rows), so the app underneath just sees a single tall,
            // narrow screen; we re-tile those rows into columns at display time.
            // This is why full-screen apps (vim/vi/neovim) columnize
            // transparently — they never know the screen is being re-tiled.
            // Size from the content rect so the titlebar strip is excluded.
            let layout = self.column_layout(id, self.content_rect(rect)); // count/col_cells/rows(=one column's height)
            if let Some(p) = self.panes.get_mut(&id) {
                let pty_rows = layout.rows * layout.count as usize; // total screen height fed to the app
                p.resize(layout.col_cells, pty_rows.max(1)); // narrow + tall
            }
        }
    }

    // ----- internal helpers -------------------------------------------------

    /// Split the focused pane along `orient`, spawning a backend for the new
    /// pane and moving focus to it (Terminator focuses the new pane on split).
    fn split(&mut self, orient: Orientation) {
        // Ask the tree to split; a stale focus id yields None (no crash).
        if let Some(new_id) = self.tree.split(self.focus, orient) {
            // Size the new pane from its freshly computed rectangle.
            let (cols, rows) = self.pane_cells(new_id); // its cell dimensions
            match (self.spawn)(new_id, cols, rows) {
                // Spawn succeeded: register the pane, focus it, reflow.
                Some(backend) => {
                    self.panes.insert(new_id, backend);
                    self.focus = new_id; // focus follows the split
                    self.relayout(self.bounds); // the sibling shrank; resize everyone
                }
                // Spawn failed (out of ptys/fds): undo the split so we never leave a
                // backend-less node, and keep the existing panes intact — do NOT crash.
                None => {
                    self.tree.close(new_id);
                }
            }
        }
    }

    /// Split the focused pane along its *longer* axis (Terminator's split_auto):
    /// a wide pane splits left/right, a tall one top/bottom, so each split keeps
    /// panes as square as possible. Falls back to a left/right split if the
    /// focused pane's rectangle can't be found.
    fn split_auto(&mut self) {
        // Look up the focused pane's current rectangle to compare its dimensions.
        let orient = self
            .tree
            .rects(self.bounds)
            .into_iter()
            .find(|(id, _)| *id == self.focus)
            .map(|(_, r)| {
                if r.w >= r.h {
                    Orientation::LeftRight // wider than tall → side by side
                } else {
                    Orientation::TopBottom // taller than wide → stacked
                }
            })
            .unwrap_or(Orientation::LeftRight); // no rect (shouldn't happen): sane default
        self.split(orient);
    }

    /// Grow the focused pane toward `dir` by one keyboard step, resizing the
    /// nearest split on that axis. Returns `Redraw` if anything moved.
    fn resize_focus(&mut self, dir: Direction) -> Option<SessionEvent> {
        const STEP: f32 = 0.03; // 3% of the split's extent per keypress
        if self.tree.resize(self.focus, dir, STEP) {
            self.relayout(self.bounds); // panes changed size → reflow + resize PTYs
            Some(SessionEvent::Redraw)
        } else {
            None // at an edge or no matching split: nothing changed
        }
    }

    /// Flip the orientation of the split containing the focused pane. Returns
    /// `Redraw` when it actually rotated.
    fn rotate_focus(&mut self) -> Option<SessionEvent> {
        if self.tree.rotate(self.focus) {
            self.relayout(self.bounds); // the arrangement changed → reflow
            Some(SessionEvent::Redraw)
        } else {
            None // lone pane / non-split parent: nothing to rotate
        }
    }

    /// Open a new tab beside the focused pane and focus it.
    fn new_tab(&mut self) {
        if let Some(new_id) = self.tree.new_tab(self.focus) {
            let (cols, rows) = self.pane_cells(new_id); // new tab's size
            match (self.spawn)(new_id, cols, rows) {
                Some(backend) => {
                    self.panes.insert(new_id, backend);
                    self.focus = new_id; // focus the new tab
                    self.relayout(self.bounds); // reflow
                }
                None => {
                    self.tree.close(new_id); // undo the tab; keep existing panes
                    // In a pre-existing multi-tab group, close() lands `active` on the last
                    // surviving tab, which may not be the one holding the still-focused
                    // original pane — re-reveal it so focus doesn't hide behind a non-active
                    // tab page, then reflow.
                    self.tree.activate_tab(self.focus);
                    self.relayout(self.bounds);
                }
            }
        }
    }

    /// Close pane `closing`: drop its backend (which cleanly shuts down the PTY
    /// via `Drop`), remove it from the tree, and re-seat focus if needed.
    /// Returns `CloseWindow` if that was the last pane, `Redraw` otherwise, or
    /// `None` if the id was not in the tree.
    ///
    /// Public because it is driven both by the `CloseTerm` action (close the
    /// focused pane) and by the run-loop when a pane's shell exits on its own
    /// (Ctrl-D / `exit`) — the fix for "the pane stays open after bash exits".
    pub fn close_pane(&mut self, closing: PaneId) -> Option<SessionEvent> {
        // Remove from the tree first; if it was not present, do nothing.
        if !self.tree.close(closing) {
            return None; // stale id; nothing to do
        }
        self.panes.remove(&closing); // drop backend → PTY shutdown+join (Drop)
        self.groups.remove(&closing); // forget any group membership
        self.columns.remove(&closing); // forget its column count
        self.titles.remove(&closing); // forget its title
        if self.zoomed == Some(closing) {
            self.zoomed = None; // un-zoom if we closed the maximised pane
        }
        if self.tree.is_empty() {
            return Some(SessionEvent::CloseWindow); // no panes left → close window
        }
        // If the pane we closed held focus, re-seat it on a surviving visible
        // pane (nearest by traversal). If some other pane exited, focus is fine.
        if self.focus == closing {
            if let Some((id, _)) = self.tree.rects(self.bounds).into_iter().next() {
                self.focus = id; // pick the first visible pane as the new focus
            }
        }
        self.relayout(self.bounds); // survivors may have grown; resize them
        Some(SessionEvent::Redraw)
    }

    /// Move focus one pane in `dir`, if a neighbour exists. Returns `Redraw`
    /// only when focus actually changed, so a bump against the window edge is a
    /// silent no-op.
    fn move_focus(&mut self, dir: Direction) -> Option<SessionEvent> {
        match self.tree.neighbor(self.focus, dir, self.bounds) {
            Some(next) => {
                self.focus = next; // adopt the neighbour as the new focus
                Some(SessionEvent::Redraw)
            }
            None => None, // edge of the window; nothing to do
        }
    }

    /// Compute the (cols, rows) for a specific pane id from the current layout,
    /// falling back to the full window if the pane is not currently visible
    /// (e.g. just created on an inactive path — rare, but keeps sizing sane).
    fn pane_cells(&self, id: PaneId) -> (usize, usize) {
        for (pid, rect) in self.tree.rects(self.bounds) {
            if pid == id {
                return cells_in(self.content_rect(rect), self.cell); // minus the titlebar strip
            }
        }
        cells_in(self.content_rect(self.bounds), self.cell) // fallback: full-window sizing
    }

    // ----- PanePackage extract/adopt (pane drag-and-drop) -------------------

    /// Whether this session holds no panes at all (the App closes a window
    /// once its session goes empty).
    pub fn is_empty(&self) -> bool {
        self.tree.is_empty()
    }

    /// Pull every side-table entry (backend, group, columns, title) for `ids`
    /// out of this session's maps, clearing zoom on any of them that was
    /// zoomed. Public because a cross-window swap ejects both sides before
    /// injecting them back into each other's session.
    pub fn eject_entries(&mut self, ids: &[PaneId]) -> PaneEntries<B> {
        let mut e = PaneEntries { panes: Vec::new(), groups: Vec::new(), columns: Vec::new(), titles: Vec::new() };
        for &id in ids {
            if let Some(b) = self.panes.remove(&id) {
                e.panes.push((id, b));
            }
            if let Some(g) = self.groups.remove(&id) {
                e.groups.push((id, g));
            }
            if let Some(c) = self.columns.remove(&id) {
                e.columns.push((id, c));
            }
            if let Some(t) = self.titles.remove(&id) {
                e.titles.push((id, t));
            }
            if self.zoomed == Some(id) {
                self.zoomed = None;
            }
        }
        e
    }

    /// Merge previously-ejected side-table entries back into this session's
    /// maps (the counterpart to [`Session::eject_entries`]).
    pub fn inject_entries(&mut self, e: PaneEntries<B>) {
        for (id, b) in e.panes {
            self.panes.insert(id, b);
        }
        for (id, g) in e.groups {
            self.groups.insert(id, g);
        }
        for (id, c) in e.columns {
            self.columns.insert(id, c);
        }
        for (id, t) in e.titles {
            self.titles.insert(id, t);
        }
    }

    /// Shared tail of `extract_pane`/`extract_tab`: eject the side tables for
    /// every pane in `sub`, re-seat focus if it was inside the extracted
    /// subtree, and reflow the survivors (they may have grown).
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

    /// Remove pane `id` (and everything it carries) from this session's tree,
    /// returning a self-contained package for adoption elsewhere. `None` if
    /// `id` is not in this session's tree.
    pub fn extract_pane(&mut self, id: PaneId) -> Option<PanePackage<B>> {
        let sub = self.tree.take(id)?;
        Some(self.finish_extract(sub))
    }

    /// Remove the whole tab group anchored at `first_pane` from this
    /// session's tree, returning a self-contained package for adoption
    /// elsewhere. `None` if `first_pane` is not a tab group anchor.
    pub fn extract_tab(&mut self, first_pane: PaneId) -> Option<PanePackage<B>> {
        let sub = self.tree.take_tab(first_pane)?;
        Some(self.finish_extract(sub))
    }

    /// Insert a previously-extracted package into this session's tree at
    /// `at`, restoring its side-table entries and focusing the arrival. On
    /// failure (a stale/nonexistent target) the package is handed back intact
    /// — a live pane is never dropped.
    pub fn adopt(&mut self, pkg: PanePackage<B>, at: DropTarget) -> Result<(), PanePackage<B>> {
        let PanePackage { sub, entries } = pkg;
        let arriving = sub.first_pane();
        let placed = match at {
            DropTarget::Root => self.tree.adopt_root(sub),
            DropTarget::SplitBeside { pane, orient, before } => self.tree.insert_beside(pane, sub, orient, before),
            DropTarget::TabAt { anchor, index } => self.tree.insert_tab_at(anchor, sub, index),
            DropTarget::RootEdge { orient, before } => {
                self.tree.insert_root_edge(sub, orient, before);
                Ok(())
            }
            DropTarget::Swap { .. } => Err(sub), // swap is not an adopt — refuse
        };
        match placed {
            Ok(()) => {
                self.inject_entries(entries);
                self.zoomed = None; // the new layout must be visible
                if let Some(f) = arriving {
                    self.focus = f;
                }
                self.relayout(self.bounds);
                Ok(())
            }
            Err(sub) => Err(PanePackage { sub, entries }),
        }
    }

    /// Rewrite one leaf of this session's TREE from `from` to `to`, leaving
    /// the layout (weights, splits, tab structure) exactly as it was.
    ///
    /// This is the tree half of a CROSS-window swap: no single session owns
    /// both panes, so [`Session::move_pane`]'s `Swap` arm cannot serve it. The
    /// App rewrites one leaf in each of the two trees and then exchanges the
    /// two panes' side-table entries with [`Session::eject_entries`] /
    /// [`Session::inject_entries`]. `false` if `from` is not a leaf here (a
    /// stale target — the caller must undo the other tree's rewrite).
    pub fn tree_replace(&mut self, from: PaneId, to: PaneId) -> bool {
        self.tree.replace_leaf(from, to)
    }

    /// Take back a package this session handed out that nobody adopted (a
    /// cross-window drop onto a stale target), re-inserting it as a root-edge
    /// split. The failure path of every cross-window move: a live pane must
    /// never be lost, and the source window is the one place guaranteed to
    /// still exist.
    pub fn readopt_root_edge(&mut self, sub: rt_core::Subtree, entries: PaneEntries<B>) {
        self.tree.insert_root_edge(sub, Orientation::LeftRight, false);
        self.inject_entries(entries);
        self.relayout(self.bounds);
    }

    /// Focus pane `id` if this session owns it, else leave focus alone
    /// (returning `false`). The cross-window swap needs it: a session whose
    /// focused pane just left for another window must not keep pointing at a
    /// pane it no longer owns.
    pub fn focus_pane(&mut self, id: PaneId) -> bool {
        if self.panes.contains_key(&id) {
            self.focus = id;
            true
        } else {
            false
        }
    }

    /// Move pane `id` to `at` within/into this session, committing
    /// immediately (no package survives the call — same-window drag-and-drop
    /// and `Swap`). Returns `false` on a no-op or stale target; a pane is
    /// never lost even on the fallback path.
    pub fn move_pane(&mut self, id: PaneId, at: DropTarget) -> bool {
        // Guard: the drop target must not name a pane inside the payload.
        match at {
            DropTarget::SplitBeside { pane, .. } if pane == id => return false,
            DropTarget::TabAt { anchor, .. } if anchor == id => return false,
            _ => {}
        }
        match at {
            DropTarget::Swap { pane } => {
                if self.tree.swap(id, pane) {
                    self.relayout(self.bounds);
                    true
                } else {
                    false
                }
            }
            _ => {
                let Some(pkg) = self.extract_pane(id) else { return false };
                match self.adopt(pkg, at) {
                    Ok(()) => true,
                    Err(pkg) => {
                        // Put it back where the tree will take it: as a root edge (never lose a pane).
                        self.readopt_root_edge(pkg.sub, pkg.entries);
                        false
                    }
                }
            }
        }
    }

    /// Move the tab group anchored at `first_pane` to `at` within/into this
    /// session, committing immediately. A tab payload never `Swap`s (the
    /// resolver never offers it); refused defensively here too.
    pub fn move_tab(&mut self, first_pane: PaneId, at: DropTarget) -> bool {
        if matches!(at, DropTarget::Swap { .. }) {
            return false;
        }
        // Guard: the drop target must not name a pane inside the payload.
        match at {
            DropTarget::SplitBeside { pane, .. } if pane == first_pane => return false,
            DropTarget::TabAt { anchor, .. } if anchor == first_pane => return false,
            _ => {}
        }
        let Some(pkg) = self.extract_tab(first_pane) else { return false };
        match self.adopt(pkg, at) {
            Ok(()) => true,
            Err(pkg) => {
                self.readopt_root_edge(pkg.sub, pkg.entries);
                false
            }
        }
    }

    /// Reorder the tab group anchored at `first_pane` to position `to` within
    /// its tab strip, focusing it (it becomes the active tab). Returns
    /// `false` if `first_pane` is not a tab group anchor.
    pub fn reorder_tab(&mut self, first_pane: PaneId, to: usize) -> bool {
        if self.tree.reorder_tab(first_pane, to) {
            self.focus = first_pane; // follow the moved tab (it is active now)
            self.relayout(self.bounds);
            true
        } else {
            false
        }
    }
}

/// Convert a pixel rectangle and a cell size into a (cols, rows) pair, clamped
/// to at least 1×1 so a terminal is never told it has zero columns (which would
/// make the grid math divide by zero). Free function so both `Session` and its
/// helpers share one definition.
fn cells_in(rect: Rect, cell: (f32, f32)) -> (usize, usize) {
    // Guard against a zero/negative cell size (bad font metrics) by flooring it.
    let cw = if cell.0 > 0.0 { cell.0 } else { 1.0 }; // cell width, never <= 0
    let ch = if cell.1 > 0.0 { cell.1 } else { 1.0 }; // cell height, never <= 0
    let cols = (rect.w / cw).floor() as usize; // whole columns that fit
    let rows = (rect.h / ch).floor() as usize; // whole rows that fit
    (cols.max(1), rows.max(1)) // clamp to a minimum 1x1 grid
}

/// Wrap `text` in bracketed-paste markers (`\x1b[200~`…`\x1b[201~`), first
/// stripping any embedded end marker from the body (paste-injection guard —
/// see [`feed_paste`](Session::feed_paste)'s doc). Shared by `feed_paste` and
/// [`Session::paste_to_group`] so the two build the identical wrapped form.
fn wrap_bracketed_paste(text: &[u8]) -> Vec<u8> {
    let body = strip_paste_end_marker(text);
    let mut w = Vec::with_capacity(body.len() + 12);
    w.extend_from_slice(b"\x1b[200~");
    w.extend_from_slice(&body);
    w.extend_from_slice(b"\x1b[201~");
    w
}

/// Remove every embedded bracketed-paste END marker (`\x1b[201~`) from pasted text, so the
/// content can't terminate the bracket early and inject commands (paste-injection guard).
fn strip_paste_end_marker(text: &[u8]) -> Vec<u8> {
    const END: &[u8] = b"\x1b[201~";
    let mut out = Vec::with_capacity(text.len());
    let mut i = 0;
    while i < text.len() {
        if text[i..].starts_with(END) {
            i += END.len();
        } else {
            out.push(text[i]);
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod chrome_scale_tests {
    use super::*;

    /// The smallest possible stand-in for a PTY pane: these tests are about
    /// geometry, and never write, resize or paste.
    struct Stub;
    impl Backend for Stub {
        fn write(&self, _bytes: &[u8]) {}
        fn resize(&mut self, _c: usize, _r: usize) {}
        fn set_palette(&mut self, _p: rt_engine::Palette) {}
        fn bracketed_paste(&self) -> bool {
            false
        }
    }

    fn session() -> Session<Stub, impl FnMut(PaneId, usize, usize) -> Option<Stub>> {
        Session::new(Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 }, (8.0, 16.0), |_id, _c, _r| Some(Stub))
    }

    /// The pane chrome this crate owns is FLAT — a 5px inner pad and a 4px
    /// titlebar pad — while the cell it sits around doubles on a Retina panel.
    /// Left unscaled it would shrink to half its intended share of the pane, so
    /// it takes the display's factor too.
    #[test]
    fn pane_chrome_scales_with_the_display() {
        let cell = (8.0_f32, 16.0_f32);
        // No titlebar: chrome is 2*PANE_PAD on both axes.
        assert_eq!(pane_chrome(cell, false, 1.0), (10.0, 10.0));
        assert_eq!(pane_chrome(cell, false, 2.0), (20.0, 20.0));
        // With one: the strip is cell height (already scaled by the caller,
        // which measures the cell at the scaled font size) plus a scaled pad.
        assert_eq!(pane_chrome(cell, true, 1.0), (10.0, 10.0 + 16.0 + 4.0));
        assert_eq!(pane_chrome(cell, true, 2.0), (20.0, 20.0 + 16.0 + 8.0));
    }

    /// And at 1.0 it is bit-for-bit the flat 5px/4px it always was — the hard
    /// requirement that keeps every Linux frame unchanged.
    #[test]
    fn pane_chrome_at_scale_one_is_bit_identical() {
        for &cell in &[(8.0_f32, 16.0_f32), (11.0, 21.0), (6.5, 13.25)] {
            let (w, h) = pane_chrome(cell, true, 1.0);
            assert_eq!(w.to_bits(), (2.0_f32 * 5.0).to_bits());
            assert_eq!(h.to_bits(), (2.0_f32 * 5.0 + cell.1 + 4.0).to_bits());
        }
    }

    /// A factor that is not a usable number must be refused rather than laid out
    /// against — a NaN pad would make every pane rect NaN and the grid vanish.
    #[test]
    fn an_unusable_factor_is_refused() {
        let mut s = session();
        assert_eq!(s.chrome_scale(), 1.0, "a session starts at 1.0");
        s.set_chrome_scale(2.0);
        assert_eq!(s.chrome_scale(), 2.0);
        for bad in [0.0_f32, -1.0, f32::NAN, f32::INFINITY] {
            s.set_chrome_scale(bad);
            assert_eq!(s.chrome_scale(), 2.0, "{bad} must be ignored, not adopted");
        }
    }

    /// `content_rect` is the single definition of "where a pane's grid lives" —
    /// both the PTY sizing and the renderer/hit-test route through it — so
    /// scaling it moves draw and hit in one step, by construction.
    #[test]
    fn content_rect_padding_and_titlebar_scale_together() {
        let mut s = session();
        s.set_show_titlebar(true);
        let pane = Rect { x: 100.0, y: 50.0, w: 400.0, h: 300.0 };

        let at_1x = s.content_rect(pane);
        assert_eq!(s.titlebar_h(), 16.0 + 4.0);
        assert_eq!(at_1x.x, 100.0 + 5.0);
        assert_eq!(at_1x.y, 50.0 + 20.0 + 5.0);
        assert_eq!(at_1x.w, 400.0 - 10.0);

        s.set_chrome_scale(2.0);
        let at_2x = s.content_rect(pane);
        assert_eq!(s.titlebar_h(), 16.0 + 8.0, "the strip's PAD doubles; the cell is the caller's");
        assert_eq!(at_2x.x, 100.0 + 10.0);
        assert_eq!(at_2x.y, 50.0 + 24.0 + 10.0);
        assert_eq!(at_2x.w, 400.0 - 20.0);
    }

    /// A session pushes its factor down into the layout tree, so the split
    /// gutters, tab strips and the divider GRAB band scale with the padding
    /// around them rather than being left behind in `rt-core`.
    #[test]
    fn the_scale_reaches_the_layout_tree() {
        let mut s = session();
        s.set_chrome_scale(2.0);
        assert_eq!(s.tree().chrome_scale(), 2.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `ColumnLayout::cell_at` must invert the renderer's tiling: a point at the
    /// centre of the cell the renderer drew for grid line `k*rows + s`, char
    /// column `c`, maps back to exactly `(c, k*rows + s)`.
    #[test]
    fn column_cell_at_inverts_the_renderer_tiling() {
        let g = ColumnLayout { count: 3, col_cells: 20, rows: 10, gap: 2 };
        let step = g.col_cells + g.gap; // cells between column origins
        for k in 0..g.count as usize {
            for s in 0..g.rows {
                for c in [0usize, 5, g.col_cells - 1] {
                    let dx = (k * step + c) as f32 + 0.5; // centre of the cell
                    let dy = s as f32 + 0.5;
                    assert_eq!(g.cell_at(dx, dy), (c, k * g.rows + s), "k={k} s={s} c={c}");
                }
            }
        }
    }

    /// A point in a gap, or past the last column/row, clamps into range — mouse
    /// forwarding must always yield a valid cell, never an out-of-bounds one.
    #[test]
    fn column_cell_at_clamps_gaps_and_edges() {
        let g = ColumnLayout { count: 2, col_cells: 10, rows: 5, gap: 2 };
        // Just past column 0's text (into the gap) → column 0's last cell.
        assert_eq!(g.cell_at(g.col_cells as f32 + 0.5, 0.5), (g.col_cells - 1, 0));
        // Far right / far down clamps, never out of bounds.
        let (col, row) = g.cell_at((g.col_cells + g.gap) as f32 * 9.0, 999.0);
        assert!(col < g.col_cells && row < g.count as usize * g.rows);
    }

    /// A write-capturing, bracketed-paste-configurable stand-in for a real PTY pane.
    struct MockPane {
        writes: std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
        bracketed: bool,
    }
    impl Backend for MockPane {
        fn write(&self, bytes: &[u8]) {
            self.writes.borrow_mut().extend_from_slice(bytes);
        }
        fn resize(&mut self, _c: usize, _r: usize) {}
        fn set_palette(&mut self, _p: rt_engine::Palette) {}
        fn bracketed_paste(&self) -> bool {
            self.bracketed
        }
    }

    /// A broadcast paste must be bracketed PER PANE — each pane wrapped (or not) by its OWN
    /// DECSET-2004 state — not decided once from the focused pane. The bug used the focused
    /// pane's state for the whole group, so a group member with the opposite bracketed-paste
    /// mode got the wrong form and mangled the newlines in a pasted multi-line key.
    #[test]
    fn broadcast_paste_wraps_per_pane_not_per_focus() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let buf0 = Rc::new(RefCell::new(Vec::new())); // focus pane: bracketed OFF
        let buf1 = Rc::new(RefCell::new(Vec::new())); // grouped pane: bracketed ON
        let b0 = buf0.clone();
        let mut s = Session::new(
            Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 },
            (8.0, 16.0),
            move |_id, _c, _r| Some(MockPane { writes: b0.clone(), bracketed: false }),
        );
        let id0 = s.focus(); // bracketed OFF (the focused pane)
        // A high sentinel, not PaneId(1): PaneId is now a process-global counter
        // (shared across every test in this binary, run in parallel), so a small
        // hardcoded id can collide with one a concurrently-running test just
        // allocated. Mirrors the stale-id sentinel used in the adopt tests below.
        let id1 = PaneId(u64::MAX - 1000);
        s.panes.insert(id1, MockPane { writes: buf1.clone(), bracketed: true });
        s.groups.insert(id0, 1);
        s.groups.insert(id1, 1);
        s.broadcast = Broadcast::Group;

        s.feed_paste(b"line1\nline2");

        assert_eq!(&buf0.borrow()[..], b"line1\nline2", "focus pane (OFF) must get RAW text");
        assert_eq!(
            &buf1.borrow()[..],
            b"\x1b[200~line1\nline2\x1b[201~",
            "grouped pane (ON) must get its OWN bracketed wrap",
        );
    }

    /// A text drop names its pane by where the POINTER was, so it must reach
    /// that pane and no other — even under `Broadcast::All`, where every
    /// keystroke goes everywhere. Two panes here, broadcast wide open; only the
    /// named one may see the bytes.
    #[test]
    fn a_drop_reaches_only_the_named_pane_even_under_broadcast() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let buf0 = Rc::new(RefCell::new(Vec::new()));
        let buf1 = Rc::new(RefCell::new(Vec::new()));
        let b0 = buf0.clone();
        let mut s = Session::new(
            Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 },
            (8.0, 16.0),
            move |_id, _c, _r| Some(MockPane { writes: b0.clone(), bracketed: false }),
        );
        let _focus = s.focus();
        let id1 = PaneId(u64::MAX - 2000); // see the sentinel note above
        s.panes.insert(id1, MockPane { writes: buf1.clone(), bracketed: false });
        s.broadcast = Broadcast::All;

        s.paste_to_pane(id1, b"dropped");

        assert!(buf0.borrow().is_empty(), "a drop must not fan out to the focus");
        assert_eq!(&buf1.borrow()[..], b"dropped", "the pane under the pointer gets it");
    }

    /// And it is bracketed by THAT pane's own state, not the focus's — the same
    /// load-bearing rule `feed_paste` enforces, reached through the same helper.
    /// The focus here is bracketed OFF and the drop target ON, so a "decide from
    /// the focus" bug would deliver raw text.
    #[test]
    fn a_drop_is_bracketed_by_its_own_panes_state() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let buf0 = Rc::new(RefCell::new(Vec::new()));
        let buf1 = Rc::new(RefCell::new(Vec::new()));
        let b0 = buf0.clone();
        let mut s = Session::new(
            Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 },
            (8.0, 16.0),
            move |_id, _c, _r| Some(MockPane { writes: b0.clone(), bracketed: false }),
        );
        let id1 = PaneId(u64::MAX - 2001);
        s.panes.insert(id1, MockPane { writes: buf1.clone(), bracketed: true });

        assert_eq!(s.pane_bracketed_paste(id1), Some(true));
        assert_eq!(s.pane_bracketed_paste(s.focus()), Some(false));
        s.paste_to_pane(id1, b"one\ntwo");

        assert_eq!(&buf1.borrow()[..], b"\x1b[200~one\ntwo\x1b[201~");
    }

    /// Dropped text carrying a bracketed-paste END marker must not be able to
    /// close the bracket early and inject a command — the same guard the
    /// clipboard path has, reached because both build the wrap in one place.
    #[test]
    fn a_drop_cannot_break_out_of_its_own_bracket() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let buf = Rc::new(RefCell::new(Vec::new()));
        let b = buf.clone();
        let mut s = Session::new(
            Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 },
            (8.0, 16.0),
            move |_id, _c, _r| Some(MockPane { writes: b.clone(), bracketed: true }),
        );
        let id = s.focus();
        s.paste_to_pane(id, b"safe\x1b[201~rm -rf /");
        assert_eq!(&buf.borrow()[..], b"\x1b[200~saferm -rf /\x1b[201~");
        let _ = &mut s;
    }

    /// A pane can exit between the pointer landing on it and the drop being
    /// processed. That must be a quiet no-op, not a panic and not a misdelivery.
    #[test]
    fn a_drop_onto_a_dead_pane_is_a_no_op() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let buf = Rc::new(RefCell::new(Vec::new()));
        let b = buf.clone();
        let s = Session::new(
            Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 },
            (8.0, 16.0),
            move |_id, _c, _r| Some(MockPane { writes: b.clone(), bracketed: false }),
        );
        let gone = PaneId(u64::MAX - 2002);
        assert_eq!(s.pane_bracketed_paste(gone), None);
        s.paste_to_pane(gone, b"nowhere");
        assert!(buf.borrow().is_empty());
    }

    // ----- Cross-window Group broadcast (Task 12) ---------------------------

    /// A session with one pane (the focus, in the given bracketed-paste
    /// state) whose writes are inspectable via the returned buffer.
    /// `Session` never has cross-window knowledge itself (that's `App`'s
    /// job — see `write_to_group`'s doc), so these tests just stand up
    /// several `Session`s directly, one per simulated window, and call them
    /// the way `App::broadcast_group_input`/`broadcast_group_paste` would.
    /// `bracketed` is a parameter (not hardcoded) so a caller can build
    /// sibling sessions in DIFFERENT bracketed-paste states — needed to
    /// exercise `paste_to_group`'s per-pane decision below.
    fn buffered_session(bracketed: bool) -> (
        Session<MockPane, impl FnMut(PaneId, usize, usize) -> Option<MockPane>>,
        PaneId,
        std::rc::Rc<std::cell::RefCell<Vec<u8>>>,
    ) {
        use std::cell::RefCell;
        use std::rc::Rc;
        let buf = Rc::new(RefCell::new(Vec::new()));
        let b = buf.clone();
        let s = Session::new(
            Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 },
            (8.0, 16.0),
            move |_id, _c, _r| Some(MockPane { writes: b.clone(), bracketed }),
        );
        let focus = s.focus();
        (s, focus, buf)
    }

    /// Membership already travelled correctly across windows before this
    /// task (`PanePackage`/`adopt`); the reported bug was that DELIVERY
    /// stayed window-scoped, so a pane torn out of a group into its own
    /// window silently stopped receiving broadcast input. `write_to_group`
    /// is the seam that fixes it: input "typed" in one window (`win_a`) must
    /// reach a same-group pane living in a completely separate window
    /// (`win_b`). Breaking the group-match condition inside
    /// `write_to_group` (e.g. hardcoding it to `true`) still makes THIS
    /// assertion pass — that's expected, it's `pane_in_a_different_group_
    /// receives_nothing` below that catches that particular break.
    #[test]
    fn group_broadcast_reaches_a_pane_in_a_different_session() {
        let (mut win_a, focus_a, _buf_a) = buffered_session(false);
        win_a.groups.insert(focus_a, 7);
        win_a.broadcast = Broadcast::Group;

        let (mut win_b, pane_b, buf_b) = buffered_session(false);
        win_b.groups.insert(pane_b, 7); // same group id, different session == different window

        // What `App::broadcast_group_input` does: resolve the group from the
        // window actually being typed in, then deliver into every OTHER window.
        let group = win_a.group_of(focus_a).expect("focus is grouped");
        win_b.write_to_group(group, b"sync");

        assert_eq!(&buf_b.borrow()[..], b"sync", "pane in the OTHER window must receive group input");
    }

    /// A pane carrying a DIFFERENT group id must not receive input meant for
    /// another group, even across windows.
    #[test]
    fn pane_in_a_different_group_receives_nothing() {
        let (mut win_a, focus_a, _buf_a) = buffered_session(false);
        win_a.groups.insert(focus_a, 1);
        win_a.broadcast = Broadcast::Group;

        let (mut win_b, pane_b, buf_b) = buffered_session(false);
        win_b.groups.insert(pane_b, 2); // a DIFFERENT group

        let group = win_a.group_of(focus_a).expect("focus is grouped");
        win_b.write_to_group(group, b"sync");

        assert!(buf_b.borrow().is_empty(), "a pane in a different group must receive nothing");
    }

    // A `broadcast_all_does_not_cross_windows` test previously lived here,
    // asserting `Broadcast::All` in `win_a` never reaches `win_b`'s pane.
    // Deleted on review: `feed_input`'s `All` arm only ever iterates
    // `self.panes`, and `Session` holds no reference of any kind to another
    // `Session` — there is no code change inside `Session` that could make
    // that assertion fail, so it was proving nothing (the "false assurance"
    // this project's test standard rules out). The actual guard against
    // `All` growing `Group`'s cross-window reach is structural (`Session`'s
    // pure, single-session API has nothing to route through) plus the doc
    // comments on `Broadcast::All` above and `App::broadcast_group_input` in
    // `main.rs`, which explicitly has no `All` equivalent.

    /// `paste_to_group` must decide bracketed-paste PER PANE from that
    /// pane's OWN state — the same load-bearing rule `feed_paste` enforces
    /// locally (see its doc, and `broadcast_paste_wraps_per_pane_not_per_focus`
    /// above) — even when the pane lives in a DIFFERENT window/session than
    /// the one initiating the broadcast paste. Two sibling windows here are
    /// in OPPOSITE bracketed states, so a "decide once" bug (using the
    /// broadcasting window's own state, or always/never wrapping) would wrap
    /// one of them wrong.
    #[test]
    fn group_paste_wraps_per_pane_not_per_session() {
        let (mut win_a, focus_a, _buf_a) = buffered_session(false); // origin window, state irrelevant here
        win_a.groups.insert(focus_a, 6);
        win_a.broadcast = Broadcast::Group;

        let (mut win_b, pane_b, buf_b) = buffered_session(false); // sibling: bracketed OFF
        win_b.groups.insert(pane_b, 6);

        let (mut win_c, pane_c, buf_c) = buffered_session(true); // sibling: bracketed ON
        win_c.groups.insert(pane_c, 6);

        let group = win_a.group_of(focus_a).expect("focus is grouped");
        win_b.paste_to_group(group, b"line1\nline2");
        win_c.paste_to_group(group, b"line1\nline2");

        assert_eq!(&buf_b.borrow()[..], b"line1\nline2", "bracketed-OFF sibling pane must get RAW text");
        assert_eq!(
            &buf_c.borrow()[..],
            b"\x1b[200~line1\nline2\x1b[201~",
            "bracketed-ON sibling pane must get its OWN bracketed wrap",
        );
    }

    // ----- PanePackage extract/adopt (Task 4) -------------------------------

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

    /// A CROSS-window swap at the session level: rewrite one leaf in each
    /// tree, then exchange the two panes' side-table entries. Both panes stay
    /// alive, each lands in the other session complete with title/group, and
    /// each tree keeps its own shape (the swap is a pure id rewrite).
    #[test]
    fn cross_session_swap_exchanges_entries() {
        let mut left = mock_session();
        let a = left.focus();
        left.set_title(a, "left-pane".into());
        left.set_group(3); // focus (a) joins group 3
        left.apply(Action::SplitVert); // a second pane so `left` keeps a shape
        let keep = left.focus();

        let mut right = mock_session();
        let b = right.focus();
        right.set_title(b, "right-pane".into());
        right.set_group(4);

        // The App's cross-window swap, verbatim: tree rewrite on both sides…
        assert!(left.tree_replace(a, b), "a is a leaf of left");
        assert!(right.tree_replace(b, a), "b is a leaf of right");
        // …then the side tables cross over.
        let from_left = left.eject_entries(&[a]);
        let from_right = right.eject_entries(&[b]);
        left.inject_entries(from_right);
        right.inject_entries(from_left);
        let bounds = Rect { x: 0.0, y: 0.0, w: 800.0, h: 600.0 };
        left.relayout(bounds);
        right.relayout(bounds);

        // b now lives in `left` with everything it owned; a in `right`.
        assert!(left.pane(b).is_some() && left.pane(a).is_none(), "b moved into left");
        assert!(right.pane(a).is_some() && right.pane(b).is_none(), "a moved into right");
        assert_eq!(left.title_of(b), Some("right-pane"));
        assert_eq!(right.title_of(a), Some("left-pane"));
        assert_eq!(left.group_of(b), Some(4));
        assert_eq!(right.group_of(a), Some(3));
        // Shapes are untouched: left still has its two panes, right one.
        assert_eq!(left.tree().all_panes().len(), 2);
        assert_eq!(right.tree().all_panes(), vec![a]);
        assert!(left.visible_rects(bounds).iter().any(|(id, _)| *id == keep));
        // Focus repair: `left`'s focus was `keep`, `right`'s was b (departed).
        assert!(right.focus_pane(a), "the arrival can take focus");
        assert_eq!(right.focus(), a);
        assert!(!right.focus_pane(b), "a pane that left cannot be focused");
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
}
