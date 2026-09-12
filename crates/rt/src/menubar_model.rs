//! What rt's macOS menu bar CONTAINS, as plain data.
//!
//! `menubar.rs` is `cfg(target_os = "macos")` and every line of it is an AppKit
//! call, so no Linux build compiles it and no Linux CI can test it. The
//! decisions it makes, though, are not AppKit at all — which row goes in which
//! menu, which rows are separators, which are greyed, and what chord each one
//! advertises — and every one of those is exactly the kind of thing that rots
//! silently when a new [`Action`] is added.
//!
//! So this module is deliberately NOT `cfg`'d, for the same reason
//! [`crate::vibrancy_policy`] and [`crate::wgpu_frame`] are not: it holds no
//! objc2 types, it compiles and its tests run everywhere, and it is the only
//! automated coverage the menu bar's shape can have. `menubar.rs` keeps the
//! AppKit calls and nothing else: it asks [`model`] what the bar looks like and
//! builds it.
//!
//! ## One model, three renderers
//!
//! [`crate::menu::rows`] is "the single source of truth for both the egui menu
//! and the native (XRender) menu". The macOS menu bar is the third renderer of
//! that same model, and this module does **not** fork it:
//!
//! * Every label and every enabled-state for an action that appears in the
//!   right-click menu is taken **verbatim** from `menu::rows` ([`Entry::Ctx`]).
//!   Rename a row there and the menu bar renames with it.
//! * A menu click therefore carries the same [`Action`] a keybinding does, and
//!   `main.rs` funnels both into `App::apply_action` — one code path.
//!
//! Two kinds of row exist here that `menu::rows` has no place for, and both are
//! declared, not improvised:
//!
//! * **Context-only rows** — "Open Link"/"Copy Address" (about whatever is
//!   under the pointer) and "Move Pane to …" (about a window the pointer is not
//!   in). They are meaningless in a menu bar that has no pointer position, so
//!   they never reach it; `RowAction::action` returns `None` for exactly those
//!   three variants and this module only ever asks for actions.
//! * **Menu-bar-only rows** ([`Entry::Extra`]) — every [`Action`] rt has that
//!   rt's own right-click menu does not offer. Some are the standard Mac
//!   entries a context menu has no reason to carry (Enter Full Screen, the zoom
//!   triple, tab cycling, Close Window, Clipboard History); the rest are
//!   actions that until now were reachable ONLY by keystroke — moving the focus
//!   between panes, moving a divider, the background-translucency pair and the
//!   whole patch bay. Each names an [`Action`] rt already implements and a
//!   binding rt already has; nothing here invents a keystroke.
//!
//! Which of the two a given action is, is not a judgement call that can be
//! forgotten: `every_action_reaches_the_menu_bar_or_is_excluded_with_a_reason`
//! walks [`Action::ALL`] — generated from the list that DECLARES the enum — so a
//! new variant fails the build until it is filed or excluded by name.
//!
//! ## Why the model is rebuilt rather than mutated
//!
//! [`model`] is cheap and total: given the keymap and two booleans it returns
//! the whole bar. The AppKit side builds the `NSMenu`s once from it and then
//! only ever re-reads the `enabled` flags (by tag = position in
//! [`BarModel::items`]), which is sound precisely because the STRUCTURE does not
//! depend on the two booleans — only `enabled` does. That invariant is not a
//! comment, it is `structure_is_independent_of_state` below.

// Off macOS the only caller is this file's own test module, which `cargo build` does not
// compile -- that is the whole point of the module being platform-independent, so the
// resulting dead_code warning is noise, not a finding.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use rt_config::{Action, Chord, Key, Keymap, Mods};

/// An AppKit key equivalent: the character `NSMenuItem` matches on, plus the
/// modifier mask it matches it under. AppKit draws this itself as ⌃⌥⇧⌘ + key,
/// which is what "in AppKit's own format" means — rt never renders the glyphs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyEquivalent {
    /// The `keyEquivalent` string: a single character (already lower-cased for
    /// letters), or one of AppKit's private-use function-key code points.
    pub key: String,
    pub command: bool,
    pub shift: bool,
    pub control: bool,
    pub option: bool,
}

/// One row of one menu-bar menu. `action == None` is a separator (its `label` is
/// empty and it carries no key equivalent) — the same convention
/// [`crate::menu::Row`] uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarItem {
    pub label: String,
    pub key: Option<KeyEquivalent>,
    pub action: Option<Action>,
    pub enabled: bool,
}

impl BarItem {
    /// A separator: no label, no chord, no action, never clickable.
    fn sep() -> Self {
        BarItem { label: String::new(), key: None, action: None, enabled: false }
    }

    pub fn is_separator(&self) -> bool {
        self.action.is_none()
    }
}

/// One top-level menu: its title in the bar and the rows behind it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarMenu {
    pub title: &'static str,
    pub items: Vec<BarItem>,
}

/// The whole bar. `app_menu` is inserted into the application menu winit's
/// AppKit backend already installs (the one owning ⌘Q and ⌘H) — that is where
/// macOS keeps Settings, and rt does not get to move it. `menus` are appended
/// after it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BarModel {
    pub app_menu: Vec<BarItem>,
    pub menus: Vec<BarMenu>,
}

impl BarModel {
    /// Every item in the bar, app menu first, in the order the AppKit side
    /// creates them — so an item's position here is its `NSMenuItem` tag, and
    /// the enable snapshot is just `items().map(|i| i.enabled)`.
    pub fn items(&self) -> impl Iterator<Item = &BarItem> {
        self.app_menu.iter().chain(self.menus.iter().flat_map(|m| m.items.iter()))
    }
}

/// One entry of the category table below.
#[derive(Debug, Clone, Copy)]
enum Entry {
    /// A divider.
    Sep,
    /// A row that also exists in the right-click menu: label and enabled-state
    /// come from [`crate::menu::rows`], never from here.
    Ctx(Action),
    /// A menu-bar-only row: no right-click counterpart, so it carries its own
    /// (Mac-idiomatic) label. Always enabled.
    Extra(Action, &'static str),
    /// A row that exists in the right-click menu but must be LABELLED
    /// differently in the bar. Enabled-state still comes from `menu::rows`, so
    /// the two renderers cannot drift on anything but the word.
    ///
    /// The application menu is the case: macOS 13 renamed "Preferences…" to
    /// "Settings…", and that row is the strongest naming convention the platform
    /// has. rt's own context menu keeps "Preferences…", which is what it is
    /// called everywhere else in rt and on Linux.
    Relabel(Action, &'static str),
}

/// The application menu's rt-specific rows.
///
/// Settings lives here and nowhere else: ⌘, in the app menu is the strongest
/// convention macOS has, and putting a second "Preferences…" in a File/Shell
/// menu is the thing that marks a port as a port.
const APP_MENU: &[Entry] = &[Entry::Relabel(Action::Preferences, "Settings…")];

/// The menu bar, left to right. Named after what a Mac terminal user reaches
/// for, not after rt's internals: Terminal.app and iTerm2 both call the
/// new-window/new-tab/split/close family "Shell", and Find lives under Edit on
/// every Mac app there has ever been.
fn categories() -> &'static [(&'static str, &'static [Entry])] {
    &[
        (
            "Shell",
            &[
                Entry::Ctx(Action::NewWindow),
                Entry::Ctx(Action::NewTab),
                Entry::Sep,
                // iTerm2's order: the side-by-side split first, since ⌘D is the
                // one a Mac user already has in their fingers.
                Entry::Ctx(Action::SplitVert),
                Entry::Ctx(Action::SplitHoriz),
                Entry::Ctx(Action::SplitAuto),
                Entry::Ctx(Action::Rotate),
                Entry::Sep,
                // Broadcast is "where does my typing go", which is a property of
                // the shell session, not of the view. iTerm2 files its own
                // broadcast controls under Shell for the same reason.
                Entry::Ctx(Action::BroadcastOff),
                Entry::Ctx(Action::BroadcastAll),
                Entry::Ctx(Action::BroadcastGroup),
                Entry::Ctx(Action::GroupCycle),
                Entry::Sep,
                // The patch bay: "where does my OUTPUT go", the mirror image of
                // the broadcast group just above it ("where does my typing go").
                // Both are properties of the shell session rather than of the
                // view, which is what puts them in the same menu. The ellipsis
                // on the two Wire rows is the Mac convention said honestly: the
                // row ARMS a wire and then waits for a second press in the pane
                // at the other end, so it does not complete on its own.
                Entry::Extra(Action::WireStdout, "Wire Stdout…"),
                Entry::Extra(Action::WireStderr, "Wire Stderr…"),
                Entry::Extra(Action::PipeInto, "Split and Pipe Stdout In"),
                Entry::Extra(Action::Unwire, "Disconnect Wires"),
                Entry::Sep,
                Entry::Ctx(Action::CloseTerm),
                Entry::Extra(Action::CloseWindow, "Close Window"),
            ],
        ),
        (
            "Edit",
            &[
                Entry::Ctx(Action::Copy),
                Entry::Ctx(Action::Paste),
                Entry::Sep,
                // macOS files Find under Edit; rt's Find is the scrollback
                // search bar, and the context menu already names it.
                Entry::Ctx(Action::Search),
                Entry::Sep,
                Entry::Extra(Action::ClipHistory, "Clipboard History…"),
                Entry::Ctx(Action::ClearClipHistory),
            ],
        ),
        (
            "View",
            &[
                // The zoom triple, in Apple's own wording and order.
                Entry::Extra(Action::ZoomIn, "Zoom In"),
                Entry::Extra(Action::ZoomOut, "Zoom Out"),
                Entry::Extra(Action::ZoomReset, "Actual Size"),
                Entry::Sep,
                Entry::Extra(Action::Fullscreen, "Enter Full Screen"),
                Entry::Sep,
                Entry::Ctx(Action::ColumnsMore),
                Entry::Ctx(Action::ColumnsFewer),
                Entry::Sep,
                // Background translucency. A View-menu matter for the same
                // reason the zoom triple is: it changes how the same session
                // LOOKS, not what it is. Apple's own wording for the direction
                // pair is opaque/transparent, so that is the wording here (rt's
                // manual says "more see-through", which is rt's voice, not the
                // platform's).
                Entry::Extra(Action::OpacityUp, "More Opaque"),
                Entry::Extra(Action::OpacityDown, "More Transparent"),
                Entry::Sep,
                Entry::Ctx(Action::ToggleFocusFollowsMouse),
            ],
        ),
        (
            "Window",
            &[
                // "Zoom" in Apple's Window menu means maximise, which is exactly
                // what ToggleZoom does to a pane. The context menu's own wording
                // is clearer about the scope, so it wins.
                Entry::Ctx(Action::ToggleZoom),
                Entry::Sep,
                // Moving the FOCUS between panes. Apple's Window menu is where
                // "go to another one of my views" lives (Terminal.app's own
                // "Select Next Pane" is there), and iTerm2 files its
                // Above/Below/Left/Right quad in the same place.
                Entry::Extra(Action::GoUp, "Select Pane Above"),
                Entry::Extra(Action::GoDown, "Select Pane Below"),
                Entry::Extra(Action::GoLeft, "Select Pane Left"),
                Entry::Extra(Action::GoRight, "Select Pane Right"),
                Entry::Sep,
                // Moving the DIVIDER. "Grow", not "Resize": the action grows the
                // focused pane in that direction at its neighbour's expense, and
                // "Resize Pane Left" does not say which of the two gets bigger.
                Entry::Extra(Action::ResizeLeft, "Grow Pane Left"),
                Entry::Extra(Action::ResizeRight, "Grow Pane Right"),
                Entry::Extra(Action::ResizeUp, "Grow Pane Up"),
                Entry::Extra(Action::ResizeDown, "Grow Pane Down"),
                Entry::Sep,
                Entry::Extra(Action::NextTab, "Next Tab"),
                Entry::Extra(Action::PrevTab, "Previous Tab"),
                Entry::Extra(Action::MoveTabLeft, "Move Tab Left"),
                Entry::Extra(Action::MoveTabRight, "Move Tab Right"),
                Entry::Sep,
                // "Send this somewhere else" — the same family the context menu
                // groups together, minus its pointer-dependent "Move Pane to …".
                Entry::Ctx(Action::DetachPane),
                Entry::Ctx(Action::DetachTab),
                Entry::Ctx(Action::PickUpPane),
                Entry::Ctx(Action::PickUpTab),
            ],
        ),
        ("Help", &[Entry::Ctx(Action::Manual)]),
    ]
}

/// Build the whole menu bar.
///
/// * `keymap` — supplies every key equivalent. Nothing here invents a chord: an
///   action with no binding simply shows none.
/// * `has_selection` — gates Copy, exactly as it gates the context menu's Copy.
/// * `suspended` — the keyboard is not rt's to interpret right now: no rt window
///   has focus, or a modal overlay (preferences, manual, search, the clipboard
///   history, an anchored compose, the context menu, an IME preedit) owns it.
///   Every row greys out. This is not cosmetic — an `NSMenuItem`'s key
///   equivalent is dead while the item is disabled, which is what keeps ⌘V
///   typing into the search field instead of firing Paste at the shell.
pub fn model(keymap: &Keymap, has_selection: bool, suspended: bool) -> BarModel {
    let ctx = context_rows(keymap, has_selection);
    let build = |entries: &[Entry]| -> Vec<BarItem> {
        let mut out: Vec<BarItem> = Vec::new();
        for e in entries {
            match *e {
                Entry::Sep => {
                    // Never open with a divider, and never stack two — either
                    // would mean a row above went missing.
                    if out.last().is_some_and(|i| !i.is_separator()) {
                        out.push(BarItem::sep());
                    }
                }
                Entry::Ctx(action) => {
                    // Label and enabled-state come from the right-click menu, so
                    // the two renderers can never drift. An action the context
                    // menu does not carry is a programming error, caught by
                    // `every_ctx_entry_has_a_context_menu_row`; at runtime it
                    // just does not appear.
                    if let Some((label, enabled)) = ctx.iter().find(|(a, ..)| *a == action).map(|(_, l, e)| (l.clone(), *e)) {
                        out.push(BarItem {
                            label,
                            key: key_equivalent(keymap, action),
                            action: Some(action),
                            enabled: enabled && !suspended,
                        });
                    }
                }
                Entry::Relabel(action, label) => {
                    // Same lookup as `Ctx` -- only the label differs, so the
                    // enabled-state and the "must exist in the context menu"
                    // invariant both still hold.
                    if let Some(enabled) = ctx.iter().find(|(a, ..)| *a == action).map(|(_, _, e)| *e) {
                        out.push(BarItem {
                            label: label.to_string(),
                            key: key_equivalent(keymap, action),
                            action: Some(action),
                            enabled: enabled && !suspended,
                        });
                    }
                }
                Entry::Extra(action, label) => out.push(BarItem {
                    label: label.to_string(),
                    key: key_equivalent(keymap, action),
                    action: Some(action),
                    enabled: !suspended,
                }),
            }
        }
        // A trailing divider separates a row from nothing.
        if out.last().is_some_and(|i| i.is_separator()) {
            out.pop();
        }
        out
    };
    BarModel {
        app_menu: build(APP_MENU),
        menus: categories()
            .iter()
            .map(|(title, entries)| BarMenu { title, items: build(entries) })
            .filter(|m| !m.items.is_empty())
            .collect(),
    }
}

/// One row of the macOS right-click popup.
///
/// A straight transcription of [`crate::menu::Row`] into what an `NSMenuItem`
/// needs, with the one distinction `Row` expresses by convention rather than by
/// type: a divider (empty label, no action) and the version footer (a real
/// label, no action) are both "action is `None`" there, and AppKit needs them
/// built by two different calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PopupItem {
    /// `[NSMenuItem separatorItem]`.
    Separator,
    /// A real item. `pick == None` is informational and never clickable — the
    /// version footer, the one such row rt has.
    Row {
        label: String,
        key: Option<KeyEquivalent>,
        enabled: bool,
        pick: Option<crate::menu::MenuPick>,
    },
}

impl PopupItem {
    /// The pick a click on this row runs, if it runs one.
    pub fn pick(&self) -> Option<&crate::menu::MenuPick> {
        match self {
            PopupItem::Separator => None,
            PopupItem::Row { pick, .. } => pick.as_ref(),
        }
    }
}

/// The macOS right-click popup, as plain data.
///
/// **The rows are [`crate::menu::rows`] and nothing else** — same order, same
/// labels, same separators, same enable rules, same version footer. There is no
/// second table here and no filtering: the three POINTER-DEPENDENT rows the
/// menu bar cannot have (`Open Link`, `Copy Address`, `Move Pane to …`) are
/// exactly why a popup exists beside the bar, so they come through untouched,
/// still carrying the `MenuPick` the self-drawn menu would have dispatched.
///
/// The two things this adds are AppKit's, not rt's:
///
/// * **Key equivalents.** `menu::rows` renders a chord into a string for rt's
///   own renderer to draw; AppKit wants the character and the mask separately
///   and draws the glyphs itself, so [`key_equivalent`] is applied here, exactly
///   as the menu bar applies it. Only action rows can have one — the
///   pointer-dependent picks name no `Action` and so name no chord.
/// * **`suspended`.** The same gate the menu bar has (`model`'s third
///   argument): while a modal overlay or an IME preedit owns the keyboard,
///   nothing rt's menus own may fire. A disabled `NSMenuItem` neither clicks nor
///   answers its key equivalent, so greying the popup is the whole mechanism —
///   and `main.rs` additionally declines to open it at all in that state.
pub fn popup_model(
    keymap: &Keymap,
    has_selection: bool,
    url: Option<&str>,
    move_targets: &[String],
    suspended: bool,
) -> Vec<PopupItem> {
    crate::menu::rows(keymap, has_selection, url, move_targets)
        .into_iter()
        .map(|r| {
            // A divider is the one row with neither a label nor an action.
            if r.label.is_empty() && r.action.is_none() {
                return PopupItem::Separator;
            }
            // `into_pick` is the SAME translation the self-drawn menu runs on a
            // click, so the two renderers cannot dispatch differently.
            let pick = r.action.map(|a| a.into_pick());
            let key = match &pick {
                Some(crate::menu::MenuPick::Do(a)) => key_equivalent(keymap, *a),
                _ => None,
            };
            PopupItem::Row {
                label: r.label,
                key,
                // An informational row (`pick == None`) is never live, whatever
                // `menu::rows` said; everything else follows the row, gated by
                // the suspended state on top.
                enabled: r.enabled && pick.is_some() && !suspended,
                pick,
            }
        })
        .collect()
}

/// Every actionable row of the right-click menu, as `(action, label, enabled)`.
///
/// Built from [`crate::menu::rows`] with no pointer and no move targets, so the
/// three pointer-dependent row kinds (Open Link / Copy Address / Move Pane to …)
/// are absent by construction, and the separators and the version footer drop
/// out because they carry no action.
fn context_rows(keymap: &Keymap, has_selection: bool) -> Vec<(Action, String, bool)> {
    crate::menu::rows(keymap, has_selection, None, &[])
        .into_iter()
        .filter_map(|r| {
            let action = r.action.as_ref().and_then(|a| a.action())?;
            Some((action, r.label, r.enabled))
        })
        .collect()
}

/// The AppKit key equivalent for `action`'s first binding, if it has one.
///
/// Taken from the [`Chord`], not from the rendered accelerator string: AppKit
/// wants the character and the mask separately and draws the glyphs itself.
fn key_equivalent(keymap: &Keymap, action: Action) -> Option<KeyEquivalent> {
    chord_key_equivalent(&keymap.chord_for(action)?)
}

/// A chord as AppKit spells it.
///
/// The one subtlety is Shift. For a letter, ⇧⌘D is `keyEquivalent = "d"` with
/// Shift in the mask — the shift is a real, separate press. For a character that
/// only EXISTS as a shifted press (`?`, `{`, `}`, `+`), it is not: the character
/// already implies the shift, and Apple spells its own Help item `"?"` with the
/// Command mask alone. Leaving Shift in the mask there would ask the user for a
/// modifier they are already holding to produce the character, and AppKit would
/// draw a chord (⇧⌘?) that no key produces.
fn chord_key_equivalent(chord: &Chord) -> Option<KeyEquivalent> {
    let key = match chord.key {
        Key::Char(c) => c.to_ascii_lowercase().to_string(),
        // AppKit's private-use area function-key code points (`NSEvent.h`).
        Key::Up => '\u{F700}'.to_string(),
        Key::Down => '\u{F701}'.to_string(),
        Key::Left => '\u{F702}'.to_string(),
        Key::Right => '\u{F703}'.to_string(),
        Key::PageUp => '\u{F72C}'.to_string(),
        Key::PageDown => '\u{F72D}'.to_string(),
        Key::Function(n) if (1..=12).contains(&n) => {
            char::from_u32(0xF704 + u32::from(n) - 1)?.to_string()
        }
        Key::Function(_) => return None,
        Key::Tab => "\t".to_string(),
        Key::Enter => "\r".to_string(),
    };
    // Is the character itself already the product of Shift?
    let shifted_symbol = matches!(chord.key, Key::Char(c) if !c.is_ascii_alphanumeric());
    Some(KeyEquivalent {
        key,
        command: chord.mods.contains(Mods::SUPER),
        shift: chord.mods.contains(Mods::SHIFT) && !shifted_symbol,
        control: chord.mods.contains(Mods::CONTROL),
        option: chord.mods.contains(Mods::ALT),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    /// A keymap with the Command bindings in it whatever the host OS is.
    ///
    /// `Keymap::defaults()` only folds in `MACOS_DEFAULTS` under
    /// `cfg(target_os = "macos")`, so on the Linux box that actually runs these
    /// tests the ⌘ set is absent. Binding a few by hand is what lets Linux CI
    /// check the Command formatting that only macOS will ever display.
    fn mac_keymap() -> Keymap {
        let mut km = Keymap::defaults();
        for (accel, action) in [
            ("<Super>c", Action::Copy),
            ("<Super>v", Action::Paste),
            ("<Super>d", Action::SplitVert),
            ("<Shift><Super>d", Action::SplitHoriz),
            ("<Shift><Super>?", Action::Manual),
            ("<Shift><Super>}", Action::NextTab),
            ("<Alt><Super>Left", Action::PrevTab),
            ("<Control><Super>f", Action::Fullscreen),
            ("<Super>comma", Action::Preferences),
        ] {
            km.bind(Chord::parse(accel).unwrap(), action);
        }
        km
    }

    fn find<'a>(m: &'a BarModel, label: &str) -> &'a BarItem {
        m.items().find(|i| i.label == label).unwrap_or_else(|| panic!("no item labelled {label:?}"))
    }

    /// The load-bearing test: **every** [`Action`] rt has must be filed under a
    /// menu-bar category, or named here with a reason. Not "every action the
    /// right-click menu offers" — that was the old guard, and it could only ever
    /// catch a regression in rt's OWN menu; an `Action` that was in neither menu
    /// was invisible to it, which is exactly how fourteen of them came to have no
    /// place on the Mac menu bar at all.
    ///
    /// It asserts against [`Action::ALL`], which is generated from the very list
    /// that declares the enum (`rt_config`'s `actions!`), so a new variant is in
    /// it the moment it exists and this test fails until somebody decides where
    /// it goes. That is the whole point: the decision is forced, not remembered.
    ///
    /// Empty today — every action rt has reaches the bar.
    const NOT_IN_THE_MENU_BAR: &[(Action, &str)] = &[];

    #[test]
    fn every_action_reaches_the_menu_bar_or_is_excluded_with_a_reason() {
        let km = Keymap::defaults();
        let m = model(&km, true, false);
        let in_bar: HashSet<Action> = m.items().filter_map(|i| i.action).collect();
        let excluded: HashSet<Action> = NOT_IN_THE_MENU_BAR.iter().map(|(a, _)| *a).collect();
        for (a, reason) in NOT_IN_THE_MENU_BAR {
            assert!(!reason.trim().is_empty(), "{a:?} is excluded with no reason given");
            assert!(!in_bar.contains(a), "{a:?} is declared excluded but IS in the menu bar");
        }
        let mut missing: Vec<String> = Action::ALL
            .iter()
            .filter(|a| !in_bar.contains(a) && !excluded.contains(a))
            .map(|a| format!("{a:?}"))
            .collect();
        missing.sort();
        assert!(
            missing.is_empty(),
            "{} of rt's actions reach no menu-bar category and are not declared \
             excluded: {missing:?} — file each in `categories()` or add it to \
             NOT_IN_THE_MENU_BAR with a reason",
            missing.len()
        );
    }

    /// Which menu an action is filed under is a decision, not an accident, so
    /// it is pinned — with the label it carries. These fourteen were in NO menu
    /// before (neither the bar nor rt's own right-click menu), which is what the
    /// guard above now makes impossible; this is the other half, the answer to
    /// "and where did they go".
    #[test]
    fn the_actions_that_had_no_menu_at_all_are_filed_where_a_mac_user_looks() {
        let m = model(&Keymap::defaults(), true, false);
        let placed = |a: Action| -> (&'static str, String) {
            for menu in &m.menus {
                if let Some(i) = menu.items.iter().find(|i| i.action == Some(a)) {
                    return (menu.title, i.label.clone());
                }
            }
            panic!("{a:?} is in no menu");
        };
        for (action, menu, label) in [
            // The patch bay — "where does my output go", beside broadcast's
            // "where does my typing go".
            (Action::WireStdout, "Shell", "Wire Stdout…"),
            (Action::WireStderr, "Shell", "Wire Stderr…"),
            (Action::PipeInto, "Shell", "Split and Pipe Stdout In"),
            (Action::Unwire, "Shell", "Disconnect Wires"),
            // Translucency is a matter of how it looks.
            (Action::OpacityUp, "View", "More Opaque"),
            (Action::OpacityDown, "View", "More Transparent"),
            // Moving the focus, and moving the divider.
            (Action::GoUp, "Window", "Select Pane Above"),
            (Action::GoDown, "Window", "Select Pane Below"),
            (Action::GoLeft, "Window", "Select Pane Left"),
            (Action::GoRight, "Window", "Select Pane Right"),
            (Action::ResizeLeft, "Window", "Grow Pane Left"),
            (Action::ResizeRight, "Window", "Grow Pane Right"),
            (Action::ResizeUp, "Window", "Grow Pane Up"),
            (Action::ResizeDown, "Window", "Grow Pane Down"),
        ] {
            assert_eq!(placed(action), (menu, label.to_string()), "{action:?} is filed somewhere else");
        }
    }

    #[test]
    fn every_ctx_entry_has_a_context_menu_row() {
        // The other direction: a category naming an action the right-click menu
        // does not carry would silently drop the row (and its label).
        let km = Keymap::defaults();
        let ctx: HashSet<Action> = context_rows(&km, true).into_iter().map(|(a, ..)| a).collect();
        let mut orphans: Vec<String> = APP_MENU
            .iter()
            .chain(categories().iter().flat_map(|(_, e)| e.iter()))
            .filter_map(|e| match e {
                // `Relabel` resolves through the same context-menu lookup as
                // `Ctx` (only the label differs), so it carries the same
                // requirement: no row there means the bar item silently
                // disappears.
                Entry::Ctx(a) | Entry::Relabel(a, _) if !ctx.contains(a) => Some(format!("{a:?}")),
                _ => None,
            })
            .collect();
        orphans.sort();
        assert!(orphans.is_empty(), "Entry::Ctx/Relabel name actions the context menu has no row for: {orphans:?}");
    }

    #[test]
    fn pointer_dependent_rows_never_reach_the_menu_bar() {
        // The context menu builds these from what is under the cursor; a menu
        // bar has no cursor. They must not appear even when the context menu is
        // built with a URL and move targets in play.
        let km = Keymap::defaults();
        let ctx = crate::menu::rows(&km, true, Some("https://example.invalid"), &["2: htop".into()]);
        assert!(ctx.iter().any(|r| r.label == "Open Link"), "the context menu still has its URL rows");
        assert!(ctx.iter().any(|r| r.label.starts_with("Move Pane to ")), "…and its move targets");
        let m = model(&km, true, false);
        for i in m.items() {
            assert_ne!(i.label, "Open Link");
            assert_ne!(i.label, "Copy Address");
            assert!(!i.label.starts_with("Move Pane to "), "{:?} is pointer-dependent", i.label);
        }
    }

    #[test]
    fn copy_follows_the_context_menus_selection_gate() {
        let km = Keymap::defaults();
        assert!(!find(&model(&km, false, false), "Copy").enabled, "no selection: Copy is greyed");
        assert!(find(&model(&km, true, false), "Copy").enabled, "with a selection: Copy is live");
        // Paste never depends on the selection.
        assert!(find(&model(&km, false, false), "Paste").enabled);
    }

    #[test]
    fn a_suspended_keyboard_greys_the_whole_bar() {
        let km = Keymap::defaults();
        let m = model(&km, true, true);
        for i in m.items() {
            assert!(!i.enabled, "{:?} must be greyed while the keyboard is not rt's", i.label);
        }
    }

    #[test]
    fn separators_are_never_items() {
        let m = model(&Keymap::defaults(), true, false);
        for menu in &m.menus {
            assert!(!menu.items.is_empty(), "{} is empty", menu.title);
            assert!(!menu.items[0].is_separator(), "{} opens with a divider", menu.title);
            assert!(!menu.items[menu.items.len() - 1].is_separator(), "{} ends with a divider", menu.title);
            for w in menu.items.windows(2) {
                assert!(!(w[0].is_separator() && w[1].is_separator()), "{} stacks two dividers", menu.title);
            }
            for i in &menu.items {
                if i.is_separator() {
                    assert!(i.label.is_empty(), "a divider carries no label");
                    assert!(i.key.is_none(), "a divider carries no chord");
                    assert!(!i.enabled, "a divider is never clickable");
                } else {
                    assert!(!i.label.is_empty(), "an item must be labelled");
                }
            }
        }
    }

    #[test]
    fn structure_is_independent_of_state() {
        // The AppKit side builds the menus once and thereafter only pushes the
        // `enabled` flags back in, addressed by position. That is only sound
        // while nothing else moves.
        let km = Keymap::defaults();
        let base = model(&km, false, false);
        for (sel, susp) in [(false, true), (true, false), (true, true)] {
            let other = model(&km, sel, susp);
            assert_eq!(base.menus.len(), other.menus.len());
            let a: Vec<_> = base.items().map(|i| (&i.label, &i.key, i.action)).collect();
            let b: Vec<_> = other.items().map(|i| (&i.label, &i.key, i.action)).collect();
            assert_eq!(a, b, "the bar's shape changed with (has_selection={sel}, suspended={susp})");
        }
    }

    #[test]
    fn labels_are_taken_verbatim_from_the_context_menu() {
        let km = Keymap::defaults();
        let ctx = context_rows(&km, true);
        let m = model(&km, true, false);
        for (action, label, _) in &ctx {
            // Only rows filed as Entry::Ctx; Extra rows carry their own label.
            let is_ctx_entry = APP_MENU
                .iter()
                .chain(categories().iter().flat_map(|(_, e)| e.iter()))
                .any(|e| matches!(e, Entry::Ctx(a) if a == action));
            if !is_ctx_entry {
                continue;
            }
            let item = m.items().find(|i| i.action == Some(*action)).unwrap();
            assert_eq!(&item.label, label, "{action:?} is relabelled in the menu bar");
        }
    }

    #[test]
    fn no_action_is_filed_in_two_places() {
        let m = model(&Keymap::defaults(), true, false);
        let mut seen: Vec<Action> = Vec::new();
        for a in m.items().filter_map(|i| i.action) {
            assert!(!seen.contains(&a), "{a:?} appears twice in the menu bar");
            seen.push(a);
        }
    }

    #[test]
    fn settings_lives_in_the_application_menu() {
        let m = model(&Keymap::defaults(), true, false);
        assert_eq!(m.app_menu.iter().filter_map(|i| i.action).collect::<Vec<_>>(), vec![Action::Preferences]);
        // …and nowhere else.
        for menu in &m.menus {
            assert!(!menu.items.iter().any(|i| i.action == Some(Action::Preferences)), "{} duplicates Settings", menu.title);
        }
        // …and it is deliberately RELABELLED: macOS 13 renamed Preferences to
        // Settings, and the application menu is where that convention is
        // strongest. rt's own context menu keeps "Preferences…", which is what
        // it is called everywhere else in rt and on Linux. Pin both halves, so
        // neither can drift into matching the other by accident.
        let item = m.app_menu.iter().find(|i| i.action == Some(Action::Preferences)).expect("a Settings row");
        assert_eq!(item.label, "Settings…", "the application menu must use the macOS 13+ name");
        let ctx_label = context_rows(&Keymap::defaults(), true)
            .into_iter()
            .find(|(a, ..)| *a == Action::Preferences)
            .map(|(_, l, _)| l)
            .expect("the context menu still offers Preferences");
        assert_eq!(ctx_label, "Preferences…", "rt's own menu keeps rt's own name");
    }

    #[test]
    fn the_bar_reads_left_to_right_like_a_mac_app() {
        let m = model(&Keymap::defaults(), true, false);
        let titles: Vec<_> = m.menus.iter().map(|x| x.title).collect();
        assert_eq!(titles, vec!["Shell", "Edit", "View", "Window", "Help"]);
    }

    #[test]
    fn key_equivalents_come_from_the_keymap_and_nowhere_else() {
        let km = mac_keymap();
        let m = model(&km, true, false);
        let ke = |label: &str| find(&m, label).key.clone().unwrap();
        assert_eq!(ke("Copy"), KeyEquivalent { key: "c".into(), command: true, shift: false, control: false, option: false });
        assert_eq!(ke("Paste"), KeyEquivalent { key: "v".into(), command: true, shift: false, control: false, option: false });
        assert_eq!(
            ke("Split Horizontally"),
            KeyEquivalent { key: "d".into(), command: true, shift: true, control: false, option: false }
        );
        assert_eq!(
            ke("Enter Full Screen"),
            KeyEquivalent { key: "f".into(), command: true, shift: false, control: true, option: false }
        );
        assert_eq!(
            ke("Previous Tab"),
            KeyEquivalent { key: "\u{F702}".into(), command: true, shift: false, control: false, option: true },
            "a named key becomes AppKit's function-key code point"
        );
    }

    #[test]
    fn a_shifted_symbol_carries_no_shift_in_the_mask() {
        let km = mac_keymap();
        let m = model(&km, true, false);
        // ⌘? — the character already implies the Shift the user presses.
        assert_eq!(
            find(&m, "Manual").key.clone().unwrap(),
            KeyEquivalent { key: "?".into(), command: true, shift: false, control: false, option: false }
        );
        assert_eq!(
            find(&m, "Next Tab").key.clone().unwrap(),
            KeyEquivalent { key: "}".into(), command: true, shift: false, control: false, option: false }
        );
        // …but a letter's Shift is a real, separate press and stays in the mask.
        assert!(find(&m, "Split Horizontally").key.as_ref().unwrap().shift);
    }

    // ---- the macOS right-click popup -------------------------------------
    //
    // `menubar::popup` turns these into an NSMenu and nothing else, so every
    // decision the popup makes is checked here, on Linux.

    /// The popup is `menu::rows` — all of it, in order, including the three
    /// pointer-dependent row kinds the menu bar cannot have. If this drifts, the
    /// Mac loses rows its Linux twin has.
    #[test]
    fn the_popup_is_exactly_the_context_menu() {
        let km = Keymap::defaults();
        let targets = ["2: htop".to_string(), "3: logs".to_string()];
        let rows = crate::menu::rows(&km, true, Some("https://example.invalid"), &targets);
        let popup = popup_model(&km, true, Some("https://example.invalid"), &targets, false);
        assert_eq!(popup.len(), rows.len(), "the popup has a different number of rows");
        for (item, row) in popup.iter().zip(rows.iter()) {
            match item {
                PopupItem::Separator => {
                    assert!(row.label.is_empty() && row.action.is_none(), "{:?} is not a divider", row.label);
                }
                PopupItem::Row { label, .. } => assert_eq!(label, &row.label, "row labels drifted"),
            }
        }
    }

    #[test]
    fn the_popup_carries_the_rows_the_menu_bar_cannot_have() {
        let km = Keymap::defaults();
        let targets = ["2: htop".to_string()];
        let popup = popup_model(&km, true, Some("https://example.invalid"), &targets, false);
        let pick = |label: &str| {
            popup
                .iter()
                .find(|i| matches!(i, PopupItem::Row { label: l, .. } if l == label))
                .unwrap_or_else(|| panic!("no popup row {label:?}"))
                .pick()
                .cloned()
        };
        // The three pointer-dependent picks, each carrying what it acts on.
        assert_eq!(pick("Open Link"), Some(crate::menu::MenuPick::OpenUrl("https://example.invalid".into())));
        assert_eq!(pick("Copy Address"), Some(crate::menu::MenuPick::CopyUrl("https://example.invalid".into())));
        assert_eq!(pick("Move Pane to 2: htop"), Some(crate::menu::MenuPick::MoveToWindow(0)));
        // …and they are STILL absent from the menu bar, which has no pointer.
        let bar = model(&km, true, false);
        for i in bar.items() {
            assert_ne!(i.label, "Open Link");
            assert_ne!(i.label, "Copy Address");
            assert!(!i.label.starts_with("Move Pane to "));
        }
    }

    /// A popup click and the matching keystroke must be one code path. The pick
    /// is the proof: an action row carries `MenuPick::Do(a)` for the same `a`
    /// the keymap binds, which `main.rs` feeds to `run_menu_bar_action` →
    /// `apply_action` — the very function a bound chord reaches.
    #[test]
    fn a_popup_action_row_dispatches_the_keystrokes_action() {
        let km = mac_keymap();
        let popup = popup_model(&km, true, None, &[], false);
        let row = |label: &str| {
            popup.iter().find(|i| matches!(i, PopupItem::Row { label: l, .. } if l == label)).unwrap()
        };
        assert_eq!(row("Copy").pick(), Some(&crate::menu::MenuPick::Do(Action::Copy)));
        assert_eq!(row("Paste").pick(), Some(&crate::menu::MenuPick::Do(Action::Paste)));
        assert_eq!(row("Preferences…").pick(), Some(&crate::menu::MenuPick::Do(Action::Preferences)));
        // And the chord it advertises is the keymap's, in AppKit's own terms —
        // the same conversion the menu bar uses, checked over every row.
        for i in &popup {
            let PopupItem::Row { label, key, pick, .. } = i else { continue };
            let want = match pick {
                Some(crate::menu::MenuPick::Do(a)) => km.chord_for(*a).and_then(|c| chord_key_equivalent(&c)),
                _ => None,
            };
            assert_eq!(key, &want, "{label:?} advertises a chord the keymap does not hold");
        }
    }

    #[test]
    fn a_suspended_keyboard_greys_the_whole_popup() {
        let km = Keymap::defaults();
        let targets = ["2: htop".to_string()];
        let live = popup_model(&km, true, Some("https://x"), &targets, false);
        assert!(live.iter().any(|i| matches!(i, PopupItem::Row { enabled: true, .. })), "something is live normally");
        let popup = popup_model(&km, true, Some("https://x"), &targets, true);
        for i in &popup {
            if let PopupItem::Row { label, enabled, .. } = i {
                assert!(!enabled, "{label:?} must be greyed while the keyboard is not rt's");
            }
        }
    }

    #[test]
    fn the_popups_selection_gate_and_its_footer_follow_the_context_menu() {
        let km = Keymap::defaults();
        let enabled = |popup: &[PopupItem], label: &str| {
            popup
                .iter()
                .find_map(|i| match i {
                    PopupItem::Row { label: l, enabled, .. } if l == label => Some(*enabled),
                    _ => None,
                })
                .unwrap_or_else(|| panic!("no popup row {label:?}"))
        };
        assert!(!enabled(&popup_model(&km, false, None, &[], false), "Copy"), "no selection: Copy is greyed");
        assert!(enabled(&popup_model(&km, true, None, &[], false), "Copy"));
        // The version footer is a labelled row that is neither a divider nor
        // clickable — it must not become an inert item AppKit lets you pick.
        let popup = popup_model(&km, true, None, &[], false);
        let footer = popup.last().expect("a last row");
        let PopupItem::Row { label, enabled, pick, key } = footer else { panic!("the footer is a divider") };
        assert!(label.starts_with("rt "), "the last row names the build: {label:?}");
        assert!(!enabled && pick.is_none() && key.is_none(), "the footer is informational");
    }

    #[test]
    fn a_chord_is_only_ever_what_the_keymap_says() {
        // "Do not invent bindings that do not exist": every chord the bar shows
        // is this action's first binding in the keymap, and an action with no
        // binding shows nothing. Checked over every row, not a sample.
        let km = mac_keymap();
        let m = model(&km, true, false);
        for i in m.items() {
            let Some(a) = i.action else { continue };
            assert_eq!(
                i.key,
                km.chord_for(a).and_then(|c| chord_key_equivalent(&c)),
                "{:?} advertises a chord the keymap does not hold",
                i.label
            );
        }
    }
}
