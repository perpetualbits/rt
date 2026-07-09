//! The right-click context menu, rendered with egui (ADR-0004) — the same
//! toolkit as the preferences dialog, so the two share a look.
//!
//! It is rt's port of Terminator's right-click menu: the common pane actions
//! (split/tab/close) plus rt-specific entries (newspaper columns, groups,
//! broadcast). Every entry maps to an [`Action`](rt_config::Action), so clicking
//! a row runs exactly the same code path as the corresponding keybinding.
//! Background opacity / blur live in Preferences (which has richer controls), so
//! they are intentionally absent here.

use rt_config::Action;

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
        Item::Action("Cycle Pane Group", Action::GroupCycle),
        Item::Action("Broadcast: Off", Action::BroadcastOff),
        Item::Action("Broadcast: All", Action::BroadcastAll),
        Item::Action("Broadcast: Group", Action::BroadcastGroup),
        Item::Separator,
        Item::Action("More Columns", Action::ColumnsMore),
        Item::Action("Fewer Columns", Action::ColumnsFewer),
        Item::Separator,
        Item::Action("Toggle Focus-Follows-Mouse", Action::ToggleFocusFollowsMouse),
        Item::Action("Preferences…", Action::Preferences),
        Item::Action("Manual (F1)", Action::Manual),
    ]
}

/// Build the context menu for this frame at window position `pos` (egui points).
/// Sets `*chosen` to the picked action and `*close` when the menu should be
/// dismissed — a click on an item, or a press anywhere outside the panel.
/// Call once per frame from the egui run closure while the menu is open.
pub fn ui(ctx: &egui::Context, pos: (f32, f32), chosen: &mut Option<Action>, close: &mut bool) {
    // A floating, screen-constrained panel anchored at the click point. Fore-
    // ground order so it sits above the terminal (and the border instruments).
    let area = egui::Area::new(egui::Id::new("rt_context_menu"))
        .order(egui::Order::Foreground)
        .fixed_pos(egui::pos2(pos.0, pos.1))
        .constrain(true) // keep the whole panel on-screen (replaces the old clamp)
        .show(ctx, |ui| {
            egui::Frame::menu(ui.style()).show(ui, |ui| {
                ui.set_min_width(220.0);
                // Justified layout: buttons stretch the full width and left-align
                // their text, so the rows read as a menu rather than pills.
                ui.with_layout(egui::Layout::top_down_justified(egui::Align::LEFT), |ui| {
                    for it in items() {
                        match it {
                            Item::Action(label, action) => {
                                if ui.button(label).clicked() {
                                    *chosen = Some(action);
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
