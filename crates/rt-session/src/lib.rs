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
use rt_core::{Direction, Orientation, PaneId, Rect, Tree}; // the layout model

/// Abstraction over a pane's terminal backend so the controller is testable
/// without spawning real shells. The real implementation is
/// `rt_engine::TermPane`; tests use an in-memory mock.
pub trait Backend {
    /// Write raw bytes (typed input, pasted text) to this pane's PTY.
    fn write(&self, bytes: &[u8]);
    /// Resize this pane to `cols` × `rows` character cells.
    fn resize(&mut self, cols: usize, rows: usize);
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

/// How typed input fans out to other panes — rt's port of Terminator's
/// input broadcast / grouping feature.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Broadcast {
    /// Input goes only to the focused pane (the normal default).
    #[default]
    Off,
    /// Input goes to every pane sharing the focused pane's group.
    Group,
    /// Input goes to every pane in the window.
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
pub struct Session<B: Backend, F: FnMut(usize, usize) -> B> {
    tree: Tree,                        // the split/tab layout
    panes: HashMap<PaneId, B>,         // one backend per live leaf
    groups: HashMap<PaneId, u32>,      // pane -> group id (for Broadcast::Group)
    columns: HashMap<PaneId, u16>,     // pane -> newspaper column count (absent = 1)
    focus: PaneId,                     // the currently focused pane
    broadcast: Broadcast,              // current input fan-out mode
    bounds: Rect,                      // window content rectangle in pixels
    cell: (f32, f32),                  // (width, height) of one character cell in px
    spawn: F,                          // factory that creates a new backend
}

impl<B: Backend, F: FnMut(usize, usize) -> B> Session<B, F> {
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
        panes.insert(first, spawn(cols, rows)); // spawn and register pane 0
        Session {
            tree,
            panes,
            groups: HashMap::new(), // no groups assigned initially
            columns: HashMap::new(), // every pane starts single-column
            focus: first,           // focus starts on the only pane
            broadcast: Broadcast::Off,
            bounds,
            cell,
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

    /// Immutable access to the layout tree (the renderer calls `.rects(...)`).
    pub fn tree(&self) -> &Tree {
        &self.tree
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
            // Tab cycling is not yet wired into the tree API; treat as redraw
            // no-ops for now so the binding exists without misbehaving.
            Action::NextTab | Action::PrevTab => None,
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
            // Window-level appearance (background opacity + scrim) is owned by
            // the GUI shell, not the session — it holds no window handle. The
            // binary intercepts these before dispatch; this arm keeps `apply`
            // total.
            Action::OpacityUp | Action::OpacityDown | Action::ScrimUp | Action::ScrimDown => None,
        }
    }

    /// Route typed input bytes to the appropriate pane(s) according to the
    /// current broadcast mode. This is the port of Terminator's grouped input.
    ///
    /// * `Off`   → only the focused pane.
    /// * `Group` → every pane sharing the focused pane's group id (or just the
    ///   focus if it has no group).
    /// * `All`   → every pane in the window.
    pub fn feed_input(&self, bytes: &[u8]) {
        match self.broadcast {
            Broadcast::Off => {
                // Single target: the focused pane (if it still exists).
                if let Some(p) = self.panes.get(&self.focus) {
                    p.write(bytes); // deliver only here
                }
            }
            Broadcast::All => {
                // Fan out to every live pane.
                for p in self.panes.values() {
                    p.write(bytes); // deliver everywhere
                }
            }
            Broadcast::Group => {
                // Determine the focus's group; None means "just me".
                let group = self.groups.get(&self.focus).copied();
                for (id, p) in &self.panes {
                    // A pane receives input if it shares the focus's group, or
                    // if the focus is ungrouped and this is the focus itself.
                    let same_group = match group {
                        Some(g) => self.groups.get(id).copied() == Some(g),
                        None => *id == self.focus,
                    };
                    if same_group {
                        p.write(bytes); // deliver to group members
                    }
                }
            }
        }
    }

    /// Assign the focused pane to `group` (a small integer group id). Used to
    /// build broadcast groups; a future GUI exposes this via the group menu.
    pub fn set_group(&mut self, group: u32) {
        self.groups.insert(self.focus, group); // record membership for the focus
    }

    /// Recompute every pane's (cols, rows) from the current tree layout and
    /// window bounds, resizing each backend. Called by the GUI on window resize
    /// or font change. Panes on inactive tabs (absent from `rects`) keep their
    /// last size until shown.
    pub fn relayout(&mut self, bounds: Rect) {
        self.bounds = bounds; // remember the new window size
        for (id, rect) in self.tree.rects(bounds) {
            // Column mode makes the PTY ONE column WIDE and `count` columns
            // TALL (count*rows), so the app underneath just sees a single tall,
            // narrow screen; we re-tile those rows into columns at display time.
            // This is why full-screen apps (vim/vi/neovim) columnize
            // transparently — they never know the screen is being re-tiled.
            let layout = self.column_layout(id, rect); // count/col_cells/rows(=one column's height)
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
            let backend = (self.spawn)(cols, rows); // create its PTY
            self.panes.insert(new_id, backend); // register it
            self.focus = new_id; // focus follows the split
            self.relayout(self.bounds); // the sibling shrank; resize everyone
        }
    }

    /// Open a new tab beside the focused pane and focus it.
    fn new_tab(&mut self) {
        if let Some(new_id) = self.tree.new_tab(self.focus) {
            let (cols, rows) = self.pane_cells(new_id); // new tab's size
            let backend = (self.spawn)(cols, rows); // spawn its PTY
            self.panes.insert(new_id, backend); // register
            self.focus = new_id; // focus the new tab
            self.relayout(self.bounds); // reflow
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
                return cells_in(rect, self.cell); // found its rectangle
            }
        }
        cells_in(self.bounds, self.cell) // fallback: full-window sizing
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
