//! A minimal right-click context menu, rendered in the GL layer.
//!
//! rt has no widget toolkit (it draws everything itself), so the menu is just a
//! panel of text rows drawn with the same [`Renderer`] the terminal uses, plus
//! mouse hit-testing. It is rt's port of Terminator's right-click menu: the
//! common pane actions (split/tab/close) plus rt-specific entries (newspaper
//! columns, background opacity/scrim). Every entry maps to an
//! [`Action`](rt_config::Action), so clicking a row runs exactly the same code
//! path as the corresponding keybinding.

use crate::render::{Color, Renderer};
use rt_config::Action;

/// One row of the menu: either a clickable action or a visual separator.
enum Item {
    /// A clickable entry: a label and the action it triggers.
    Action(&'static str, Action),
    /// A thin divider line (not clickable).
    Separator,
}

/// An open context menu at a fixed window position.
pub struct Menu {
    items: Vec<Item>,      // the rows, top to bottom
    x: f32,                // panel left edge in pixels
    y: f32,                // panel top edge in pixels
    hovered: Option<usize>, // index of the row under the mouse (Action rows only)
}

// Layout constants (pixels). Item height also grows with the font via cell_h.
const PAD_X: f32 = 12.0; // horizontal text inset inside the panel
const PAD_Y: f32 = 4.0; // vertical padding at the panel's top and bottom
const SEP_H: f32 = 9.0; // height of a separator row
const ITEM_EXTRA: f32 = 8.0; // extra height added to cell_h for an action row

impl Menu {
    /// Build the standard rt context menu anchored at `(x, y)` (usually the
    /// mouse position). The caller should then [`clamp`](Menu::clamp) it to the
    /// window so it does not spill off-screen.
    pub fn new(x: f32, y: f32) -> Self {
        // The menu contents: Terminator-style pane actions, then rt extras.
        let items = vec![
            Item::Action("Split Horizontally", Action::SplitHoriz),
            Item::Action("Split Vertically", Action::SplitVert),
            Item::Action("New Tab", Action::NewTab),
            Item::Separator,
            Item::Action("Close Terminal", Action::CloseTerm),
            Item::Separator,
            Item::Action("More Columns", Action::ColumnsMore),
            Item::Action("Fewer Columns", Action::ColumnsFewer),
            Item::Separator,
            Item::Action("More Opaque", Action::OpacityUp),
            Item::Action("More Transparent", Action::OpacityDown),
            Item::Action("Stronger Blur", Action::ScrimUp),
            Item::Action("Weaker Blur", Action::ScrimDown),
        ];
        Menu { items, x, y, hovered: None }
    }

    /// Height of one clickable row for the given font cell height.
    fn item_height(cell_h: f32) -> f32 {
        cell_h + ITEM_EXTRA // text height plus padding
    }

    /// Panel width in pixels: the longest label plus horizontal padding.
    fn width(&self, cell_w: f32) -> f32 {
        // Longest label length in characters drives the width.
        let max_chars = self
            .items
            .iter()
            .map(|it| match it {
                Item::Action(label, _) => label.len(),
                Item::Separator => 0,
            })
            .max()
            .unwrap_or(0);
        max_chars as f32 * cell_w + PAD_X * 2.0 // text width + both insets
    }

    /// Total panel height in pixels for the given cell height.
    fn height(&self, cell_h: f32) -> f32 {
        let mut h = PAD_Y * 2.0; // top + bottom padding
        for it in &self.items {
            h += match it {
                Item::Action(..) => Self::item_height(cell_h), // full row
                Item::Separator => SEP_H,                      // thin divider
            };
        }
        h
    }

    /// Nudge the panel so it stays fully inside a `win_w × win_h` window.
    pub fn clamp(&mut self, win_w: f32, win_h: f32, cell_w: f32, cell_h: f32) {
        let w = self.width(cell_w); // panel dimensions
        let h = self.height(cell_h);
        // Shift left/up if it would overflow the right/bottom edge, then floor at 0.
        self.x = self.x.min(win_w - w).max(0.0);
        self.y = self.y.min(win_h - h).max(0.0);
    }

    /// Return the top y of row `index` (used by both hit-testing and drawing so
    /// they always agree).
    fn row_top(&self, index: usize, cell_h: f32) -> f32 {
        let mut y = self.y + PAD_Y; // first row starts below the top padding
        for it in self.items.iter().take(index) {
            y += match it {
                Item::Action(..) => Self::item_height(cell_h),
                Item::Separator => SEP_H,
            };
        }
        y
    }

    /// Which row (if any) contains the point `(px, py)`. Only Action rows match;
    /// separators and points outside the panel return `None`.
    pub fn hit(&self, px: f32, py: f32, cell_w: f32, cell_h: f32) -> Option<usize> {
        // Reject points outside the panel's horizontal extent.
        if px < self.x || px > self.x + self.width(cell_w) {
            return None;
        }
        // Walk rows accumulating their y-extents.
        for (i, it) in self.items.iter().enumerate() {
            let top = self.row_top(i, cell_h); // this row's top
            match it {
                Item::Action(..) => {
                    let bottom = top + Self::item_height(cell_h); // its bottom
                    if py >= top && py < bottom {
                        return Some(i); // hit this action row
                    }
                }
                Item::Separator => {} // not clickable
            }
        }
        None
    }

    /// Update the hovered row from a mouse position (for the highlight). Returns
    /// `true` if the hovered row changed (so the caller can redraw).
    pub fn set_hover(&mut self, px: f32, py: f32, cell_w: f32, cell_h: f32) -> bool {
        let new = self.hit(px, py, cell_w, cell_h); // row under the cursor
        let changed = new != self.hovered; // did it move to a different row?
        self.hovered = new;
        changed
    }

    /// The action bound to row `index`, if it is an Action row. Used when a click
    /// lands on a row.
    pub fn action_at(&self, index: usize) -> Option<Action> {
        match self.items.get(index) {
            Some(Item::Action(_, a)) => Some(*a), // clickable → its action
            _ => None,                            // separator or out of range
        }
    }

    /// Draw the menu panel on top of the terminal. Call last in the frame so it
    /// sits above everything. Uses the terminal renderer's cell metrics.
    pub fn draw(&self, r: &mut Renderer, cell_w: f32, cell_h: f32) {
        // Colours: an opaque dark panel, a lighter border, a hover highlight,
        // light text, and dim separators.
        let panel = Color::rgb(0x20, 0x20, 0x28); // menu background (opaque)
        let border = Color::rgb(0x4a, 0x4a, 0x58); // 1px outline / separators
        let hover = Color::rgb(0x37, 0x37, 0x42); // highlighted row background
        let text = Color::rgb(0xd8, 0xd8, 0xe0); // label colour

        let w = self.width(cell_w); // panel size
        let h = self.height(cell_h);
        // Panel background.
        r.fill_rect(self.x, self.y, w, h, panel);
        // 1px border (four thin rects, like the focus outline).
        r.fill_rect(self.x, self.y, w, 1.0, border); // top
        r.fill_rect(self.x, self.y + h - 1.0, w, 1.0, border); // bottom
        r.fill_rect(self.x, self.y, 1.0, h, border); // left
        r.fill_rect(self.x + w - 1.0, self.y, 1.0, h, border); // right

        // Each row.
        for (i, it) in self.items.iter().enumerate() {
            let top = self.row_top(i, cell_h); // row's top edge
            match it {
                Item::Action(label, _) => {
                    let ih = Self::item_height(cell_h); // row height
                    // Highlight the hovered row.
                    if self.hovered == Some(i) {
                        r.fill_rect(self.x + 1.0, top, w - 2.0, ih, hover);
                    }
                    // Draw the label, vertically centred in the row.
                    let text_top = top + (ih - cell_h) * 0.5; // centre the glyph line
                    for (ci, ch) in label.chars().enumerate() {
                        // ox includes the left inset; each char advances by a cell.
                        r.draw_char(self.x + PAD_X, text_top, ci, 0, ch, text, false, false);
                    }
                }
                Item::Separator => {
                    // A thin divider centred in the separator row, inset a little.
                    let ly = top + SEP_H * 0.5; // divider y
                    r.fill_rect(self.x + PAD_X * 0.5, ly, w - PAD_X, 1.0, border);
                }
            }
        }
    }
}
