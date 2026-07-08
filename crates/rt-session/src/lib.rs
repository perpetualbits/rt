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
pub struct Session<B: Backend, F: FnMut(PaneId, usize, usize) -> B> {
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
    show_titlebar: bool,               // reserve a header strip atop each pane
    spawn: F,                          // factory that creates a new backend
}

/// Vertical padding added to the cell height to size a per-pane titlebar strip.
const TITLEBAR_PAD: f32 = 4.0;

/// Inner padding (px) between a pane's edge and its terminal text, so the pane
/// border / heat tint / latency frame never overlap the first characters.
const PANE_PAD: f32 = 5.0;

/// The pixel overhead `(horizontal, vertical)` that [`Session::content_rect`]
/// removes from a pane's rectangle: twice the inner padding across, and the same
/// plus the titlebar strip down. Inverting it lets the GUI pre-size a window so a
/// single full-window pane comes out to an exact cols×rows grid (the `--cols` /
/// `--rows` startup flags). Context-free on purpose — `main` calls it before any
/// `Session` exists. Must stay in lockstep with `content_rect`/`titlebar_h`.
pub fn pane_chrome(cell: (f32, f32), show_titlebar: bool) -> (f32, f32) {
    let titlebar = if show_titlebar { cell.1 + TITLEBAR_PAD } else { 0.0 };
    (2.0 * PANE_PAD, 2.0 * PANE_PAD + titlebar)
}

impl<B: Backend, F: FnMut(PaneId, usize, usize) -> B> Session<B, F> {
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
        panes.insert(first, spawn(first, cols, rows)); // spawn and register pane 0
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
            // Window-level appearance (background opacity + scrim) is owned by
            // the GUI shell, not the session — it holds no window handle. The
            // binary intercepts these before dispatch; this arm keeps `apply`
            // total.
            Action::OpacityUp
            | Action::OpacityDown
            | Action::ScrimUp
            | Action::ScrimDown
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
            | Action::Manual => None,
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
            self.cell.1 + TITLEBAR_PAD
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
        let p = PANE_PAD;
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
            let backend = (self.spawn)(new_id, cols, rows); // create its PTY
            self.panes.insert(new_id, backend); // register it
            self.focus = new_id; // focus follows the split
            self.relayout(self.bounds); // the sibling shrank; resize everyone
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
            let backend = (self.spawn)(new_id, cols, rows); // spawn its PTY
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
