//! The right-click context menu model: the rows and their actions. Rendering
//! and hit-testing are native (`crate::chrome::menu`) on both backends, so the
//! menu shares a look with the preferences dialog and the manual.
//!
//! It is rt's port of Terminator's right-click menu: the common pane actions
//! (split/tab/close) plus rt-specific entries (newspaper columns, groups,
//! broadcast). Every entry maps to an [`Action`](rt_config::Action), so clicking
//! a row runs exactly the same code path as the corresponding keybinding.
//! Background opacity / blur live in Preferences (which has richer controls), so
//! they are intentionally absent here.

use rt_config::{Action, Keymap};

/// One row of the menu: a clickable action or a visual separator.
enum Item {
    /// A clickable entry: a label and the action it triggers.
    Action(&'static str, Action),
    /// A thin divider line (not clickable).
    Separator,
}

/// The menu's rows, top to bottom. Terminator-style pane actions first, then
/// rt-specific groups/broadcast/columns, then the settings entries.
fn items() -> Vec<Item> {
    vec![
        Item::Action("Split Horizontally", Action::SplitHoriz),
        Item::Action("Split Vertically", Action::SplitVert),
        Item::Action("Split Automatically", Action::SplitAuto),
        Item::Action("Rotate Split", Action::Rotate),
        Item::Action("New Tab", Action::NewTab),
        Item::Action("Maximise / Restore Pane", Action::ToggleZoom),
        Item::Action("Search Scrollback…", Action::Search),
        Item::Action("Clear Clipboard History", Action::ClearClipHistory),
        Item::Separator,
        Item::Action("Close Terminal", Action::CloseTerm),
        Item::Separator,
        Item::Action("Group: Cycle This Pane's Colour", Action::GroupCycle),
        Item::Action("Broadcast: Off", Action::BroadcastOff),
        Item::Action("Broadcast: All", Action::BroadcastAll),
        Item::Action("Broadcast: Group (same colour)", Action::BroadcastGroup),
        Item::Separator,
        Item::Action("More Columns", Action::ColumnsMore),
        Item::Action("Fewer Columns", Action::ColumnsFewer),
        Item::Separator,
        Item::Action("Toggle Focus-Follows-Mouse", Action::ToggleFocusFollowsMouse),
        Item::Action("Preferences…", Action::Preferences),
        Item::Action("Manual", Action::Manual),
    ]
}

/// What the user picked. Most rows map to an [`Action`] (run like the matching
/// keybinding); the URL rows carry the dynamic address they act on.
pub enum MenuPick {
    Do(Action),
    OpenUrl(String),
    CopyUrl(String),
}

/// A native-menu action (backend-agnostic). Maps to a [`MenuPick`] on click.
pub enum RowAction {
    Do(Action),
    OpenUrl(String),
    CopyUrl(String),
    Copy,
    Paste,
}

impl RowAction {
    /// Turn a clicked row into the pick the caller applies.
    pub fn into_pick(self) -> MenuPick {
        match self {
            RowAction::Do(a) => MenuPick::Do(a),
            RowAction::Copy => MenuPick::Do(Action::Copy),
            RowAction::Paste => MenuPick::Do(Action::Paste),
            RowAction::OpenUrl(u) => MenuPick::OpenUrl(u),
            RowAction::CopyUrl(u) => MenuPick::CopyUrl(u),
        }
    }
}

/// One built menu row. `action == None` is a separator (not clickable).
pub struct Row {
    pub label: String,
    pub accel: Option<String>,
    pub action: Option<RowAction>,
    pub enabled: bool,
}

/// The full menu for this frame, top to bottom — the single source of truth for
/// both the egui menu and the native (XRender) menu. `has_selection` gates Copy;
/// `url` adds the link rows at the top; `keymap` supplies accelerators.
pub fn rows(keymap: &Keymap, has_selection: bool, url: Option<&str>) -> Vec<Row> {
    let mut out = Vec::new();
    let sep = || Row { label: String::new(), accel: None, action: None, enabled: false };
    // `shortcut_for` already renders the chord via `Chord`'s `Display` impl (the
    // same string egui's `shortcut_text` shows), so no extra formatting here.
    let accel = |a: Action| keymap.shortcut_for(a);
    if let Some(u) = url {
        out.push(Row { label: "Open Link".into(), accel: None, action: Some(RowAction::OpenUrl(u.to_string())), enabled: true });
        out.push(Row { label: "Copy Address".into(), accel: None, action: Some(RowAction::CopyUrl(u.to_string())), enabled: true });
        out.push(sep());
    }
    out.push(Row { label: "Copy".into(), accel: accel(Action::Copy), action: Some(RowAction::Copy), enabled: has_selection });
    out.push(Row { label: "Paste".into(), accel: accel(Action::Paste), action: Some(RowAction::Paste), enabled: true });
    out.push(sep());
    for it in items() {
        match it {
            Item::Action(label, action) => out.push(Row {
                label: label.to_string(),
                accel: accel(action),
                action: Some(RowAction::Do(action)),
                enabled: true,
            }),
            Item::Separator => out.push(sep()),
        }
    }
    // A dim, non-clickable footer naming the running build (version + git commit),
    // so which build is running — and whether all machines match — is visible at a
    // glance in the menu.
    out.push(sep());
    out.push(Row { label: crate::version_string(), accel: None, action: None, enabled: false });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use rt_config::Keymap;

    #[test]
    fn url_rows_present_only_with_a_url() {
        let km = Keymap::default();
        let without = rows(&km, false, None);
        assert!(!without.iter().any(|r| r.label == "Open Link"));
        let with = rows(&km, false, Some("https://x"));
        assert!(with.iter().any(|r| r.label == "Open Link"));
    }

    #[test]
    fn copy_disabled_without_selection() {
        let km = Keymap::default();
        let r = rows(&km, false, None);
        let copy = r.iter().find(|r| r.label == "Copy").unwrap();
        assert!(!copy.enabled, "Copy needs a selection");
        let r2 = rows(&km, true, None);
        assert!(r2.iter().find(|r| r.label == "Copy").unwrap().enabled);
    }

    #[test]
    fn separators_have_no_action() {
        let r = rows(&Keymap::default(), false, None);
        assert!(r.iter().any(|r| r.action.is_none()), "at least one separator");
    }

    #[test]
    fn version_row_is_a_disabled_info_footer() {
        let r = rows(&Keymap::default(), false, None);
        let v = r.iter().find(|r| r.label.starts_with("rt ")).expect("a version row");
        assert!(v.label.contains(env!("CARGO_PKG_VERSION")), "shows the crate version");
        assert!(!v.enabled, "the version row is informational, not clickable");
        assert!(v.action.is_none(), "the version row dispatches nothing");
        // It is not a separator: separators carry an empty label.
        assert!(!v.label.is_empty());
    }
}
