//! The right-click context menu, rendered with egui (ADR-0004) — the same
//! toolkit as the preferences dialog, so the two share a look.
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
    out
}

/// Build the context menu for this frame at window position `pos` (egui points).
/// `has_selection` enables **Copy**; `url` — the address under the right-click,
/// if any — adds **Open Link** / **Copy Address** at the top (Terminator-style,
/// present only over a link). `keymap` supplies each action's accelerator.
/// Sets `*chosen` to the pick and `*close` when the menu should be dismissed —
/// a click on an item, or a press anywhere outside the panel.
pub fn ui(
    ctx: &egui::Context,
    pos: (f32, f32),
    keymap: &Keymap,
    has_selection: bool,
    url: Option<&str>,
    chosen: &mut Option<MenuPick>,
    close: &mut bool,
) {
    // A floating, screen-constrained panel anchored at the click point. Fore-
    // ground order so it sits above the terminal (and the border instruments).
    let area = egui::Area::new(egui::Id::new("rt_context_menu"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(pos.0, pos.1))
        .constrain(true) // keep the whole panel on-screen (replaces the old clamp)
        .show(ctx, |ui| {
            egui::Frame::menu(ui.style()).show(ui, |ui| {
                ui.set_min_width(260.0); // room for the label plus its accelerator
                // Justified layout: buttons stretch the full width and left-align
                // their text, so the rows read as a menu rather than pills.
                ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                    // Link rows: only when the right-click landed on a URL.
                    if let Some(u) = url {
                        if ui.add(egui::Button::new("Open Link")).clicked() {
                            *chosen = Some(MenuPick::OpenUrl(u.to_string()));
                            *close = true;
                        }
                        if ui.add(egui::Button::new("Copy Address")).clicked() {
                            *chosen = Some(MenuPick::CopyUrl(u.to_string()));
                            *close = true;
                        }
                        ui.separator();
                    }
                    // Copy (only meaningful with a selection) + Paste, with accelerators.
                    let mut copy = egui::Button::new("Copy");
                    if let Some(sc) = keymap.shortcut_for(Action::Copy) {
                        copy = copy.shortcut_text(sc);
                    }
                    if ui.add_enabled(has_selection, copy).clicked() {
                        *chosen = Some(MenuPick::Do(Action::Copy));
                        *close = true;
                    }
                    let mut paste = egui::Button::new("Paste");
                    if let Some(sc) = keymap.shortcut_for(Action::Paste) {
                        paste = paste.shortcut_text(sc);
                    }
                    if ui.add(paste).clicked() {
                        *chosen = Some(MenuPick::Do(Action::Paste));
                        *close = true;
                    }
                    ui.separator();
                    // The standard pane / rt actions.
                    for it in items() {
                        match it {
                            Item::Action(label, action) => {
                                // A menu button, with the action's accelerator
                                // right-aligned in a weak colour (egui draws it).
                                let mut button = egui::Button::new(label);
                                if let Some(sc) = keymap.shortcut_for(action) {
                                    button = button.shortcut_text(sc);
                                }
                                if ui.add(button).clicked() {
                                    *chosen = Some(MenuPick::Do(action));
                                    *close = true;
                                }
                            }
                            Item::Separator => {
                                ui.separator();
                            }
                        }
                    }
                });
            });
        });
    // A press outside the panel dismisses the menu, like a real context menu.
    if ctx.input(|i| i.pointer.any_pressed()) {
        let outside = ctx
            .input(|i| i.pointer.interact_pos())
            .is_some_and(|p| !area.response.rect.contains(p));
        if outside {
            *close = true;
        }
    }
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
}
