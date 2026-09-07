//! rt's native macOS menu bar: the AppKit half.
//!
//! Mac users look for a program's menu on the top bar, not under a right-click.
//! rt's right-click menu stays exactly as it is (it is what Linux uses, and what
//! rt users know); this adds a real `NSMenu` in the system menu bar beside it.
//!
//! **This file contains no decisions.** Which row goes in which menu, which are
//! separators, which are greyed and what chord each advertises is
//! [`crate::menubar_model`], which is not `cfg`'d and is unit-tested on Linux.
//! Here there is only objc2: build the menus, catch the click, hand the
//! [`Action`] back to the run loop. Same split as `vibrancy.rs` /
//! `vibrancy_policy.rs`, for the same reason — no CI compiles this file.
//!
//! ## Adding to winit's menu, never replacing it
//!
//! winit's AppKit backend installs the standard application menu itself
//! (`winit-appkit/src/menu.rs`), and that menu owns ⌘Q, ⌘H and the Services
//! submenu. It runs before the first `can_create_surfaces`, so by the time
//! [`install`] is called `NSApp.mainMenu` already exists with the app menu at
//! index 0. rt **appends** its own menus after it and **inserts** one row
//! (Settings…) into it. It never calls `setMainMenu:` over winit's, so nothing
//! winit installed can be lost.
//!
//! ## Getting the click back into the event loop
//!
//! A menu action arrives on AppKit's terms, in the middle of `NSApplication`'s
//! run loop, with no `&mut App` anywhere in reach. winit 0.31 has exactly one
//! sanctioned way across that gap, and it is the one used here: put the payload
//! in shared state, then [`EventLoopProxy::wake_up`], and read it back in
//! `ApplicationHandler::proxy_wake_up` on the next turn of the loop. That is
//! verbatim the pattern winit's own `proxy_wake_up` documentation shows.
//!
//! Note that 0.31 removed the typed user event: `EventLoop::<T>::
//! with_user_event()` and `fn user_event(.., T)` no longer exist, so the
//! "switch the loop's type parameter" route is not available at this winit
//! version at all. The wake-up is a bare signal, which is why the payload
//! travels in [`Bridge::pending`] beside it. On AppKit the proxy is a
//! `CFRunLoopSource`, so `wake_up` never re-enters the handler — the click is
//! queued and the loop picks it up on its next turn, which the wake-up itself
//! guarantees will happen.
//!
//! ## Enable/disable
//!
//! `NSMenu.autoenablesItems` is left at its default YES, which is AppKit asking
//! the item's target `-validateMenuItem:` every time a menu is about to be
//! displayed *and* every time a key equivalent is about to fire. rt answers from
//! a snapshot the event loop refreshes on every turn ([`MenuBar::refresh`]), so
//! Copy greys the instant the selection goes away.
//!
//! That second half — key equivalents — is why the snapshot also has a
//! "suspended" state. A disabled `NSMenuItem`'s key equivalent does not fire, so
//! greying the bar while a modal overlay (preferences, the manual, the search
//! bar, the clipboard history, an anchored compose, the context menu, an IME
//! preedit) owns the keyboard is what keeps ⌘V typing into the search field
//! instead of pasting at the shell — the same swallow `App::on_key_press` does
//! for a keystroke that reaches winit.

use std::sync::Mutex;

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem, NSMenuItemValidation};
use objc2_foundation::NSString;
use rt_config::Action;
use winit::event_loop::EventLoopProxy;

use crate::menubar_model::{BarItem, BarModel};

/// The shared state an `NSMenuItem` click and the winit run loop both touch.
///
/// Deliberately tiny and lock-free-ish: two `Mutex`es holding `Vec`s, never held
/// across anything that can block or call back into AppKit.
struct Bridge {
    /// How a click reaches `ApplicationHandler::proxy_wake_up`.
    proxy: EventLoopProxy,
    /// Tag → the action that item runs. An item's tag is its index in
    /// [`BarModel::items`], so this vector is that same order. Built once and
    /// never mutated: the bar's SHAPE does not change, only its enable flags
    /// (see `menubar_model::structure_is_independent_of_state`).
    /// `None` at a separator's index, which keeps the indices aligned.
    actions: Vec<Option<Action>>,
    /// Tag → is this item live right now. Refreshed by the event loop.
    enabled: Mutex<Vec<bool>>,
    /// Clicked actions the loop has not collected yet. A `Vec` rather than a
    /// single slot because two clicks can in principle land before one turn.
    pending: Mutex<Vec<Action>>,
}

impl Bridge {
    /// Called from `-rtMenuAction:`, on the main thread, after AppKit has
    /// dismissed the menu.
    fn clicked(&self, tag: isize) {
        let Some(Some(action)) = usize::try_from(tag).ok().and_then(|i| self.actions.get(i)).copied() else {
            log::debug!("menu bar: click on tag {tag} with no action");
            return;
        };
        if let Ok(mut q) = self.pending.lock() {
            q.push(action);
        }
        // Only after the payload is in place, exactly as winit's docs require —
        // otherwise the loop can wake on an empty queue and the click is lost.
        self.proxy.wake_up();
    }

    /// Called from `-validateMenuItem:`. Unknown tags are dead, never live: an
    /// item we cannot account for must not be clickable and must not own a
    /// keystroke.
    fn is_enabled(&self, tag: isize) -> bool {
        let Ok(flags) = self.enabled.lock() else { return false };
        usize::try_from(tag).ok().and_then(|i| flags.get(i)).copied().unwrap_or(false)
    }
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - MenuTarget does not implement Drop.
    // - It is MainThreadOnly because -validateMenuItem: (NSMenuItemValidation)
    //   is, and because AppKit only ever sends it either message on the main
    //   thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RtMenuBarTarget"]
    #[ivars = Bridge]
    struct MenuTarget;

    impl MenuTarget {
        /// The one selector every rt menu item points at. Which item was
        /// clicked is its `tag`; see [`Bridge::actions`].
        #[unsafe(method(rtMenuAction:))]
        fn rt_menu_action(&self, sender: Option<&NSMenuItem>) {
            let tag = sender.map(|s| s.tag()).unwrap_or(-1);
            self.ivars().clicked(tag);
        }
    }

    unsafe impl NSObjectProtocol for MenuTarget {}

    // AppKit's own mechanism for live enable/disable: it asks this before
    // showing a menu and before firing a key equivalent.
    unsafe impl NSMenuItemValidation for MenuTarget {
        #[unsafe(method(validateMenuItem:))]
        fn validate_menu_item(&self, item: &NSMenuItem) -> bool {
            self.ivars().is_enabled(item.tag())
        }
    }
);

/// rt's menu bar, once installed. Owned by `App` for the life of the process.
///
/// Holding the target is not optional bookkeeping: `NSMenuItem.target` is a
/// **weak, unretained** reference, so if this were dropped every menu item in
/// the bar would be pointing at freed memory.
pub struct MenuBar {
    target: Retained<MenuTarget>,
}

impl MenuBar {
    /// Push a fresh enable snapshot at the bar. `flags[i]` is the state of the
    /// item with tag `i`, i.e. of `model.items().nth(i)` — call it with
    /// `menubar_model::model(..).items().map(|i| i.enabled).collect()`.
    ///
    /// Cheap enough to call on every turn of the event loop, which is what
    /// `App::about_to_wait` does: AppKit reads it on its own schedule (menu
    /// about to drop down, key equivalent about to fire) and rt has no hook at
    /// that moment, so the snapshot has to already be right.
    pub fn refresh(&self, flags: Vec<bool>) {
        if let Ok(mut e) = self.target.ivars().enabled.lock() {
            *e = flags;
        }
    }

    /// Take the actions clicked since the last call, oldest first.
    pub fn take_pending(&self) -> Vec<Action> {
        self.target.ivars().pending.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }
}

/// Build rt's menus and put them in the system menu bar.
///
/// Returns `None` when the menu bar could not be reached at all — off the main
/// thread, or with no `NSApplication` main menu (only possible if winit's
/// default menu were disabled, which rt does not do). Like `vibrancy.rs`, every
/// failure path logs and returns; nothing here can panic, and rt runs perfectly
/// well with no menu bar — the right-click menu and every keybinding are
/// untouched.
pub fn install(model: &BarModel, proxy: EventLoopProxy) -> Option<MenuBar> {
    // NSMenu is main-thread-only. `new()` checks the current thread for real,
    // so an off-main-thread call degrades to `None` rather than to UB.
    let Some(mtm) = MainThreadMarker::new() else {
        log::debug!("menu bar: not on the main thread; skipping");
        return None;
    };
    let app = NSApplication::sharedApplication(mtm);
    let Some(main_menu) = app.mainMenu() else {
        log::debug!("menu bar: NSApp has no main menu; skipping");
        return None;
    };

    // Tag = position in `model.items()`, separators included, so the enable
    // snapshot indexes straight into it.
    let actions: Vec<Option<Action>> = model.items().map(|i| i.action).collect();
    let enabled: Vec<bool> = model.items().map(|i| i.enabled).collect();
    let target = MenuTarget::new(
        mtm,
        Bridge { proxy, actions, enabled: Mutex::new(enabled), pending: Mutex::new(Vec::new()) },
    );

    let mut tag: isize = 0;
    let mut next = |it: &BarItem| -> Retained<NSMenuItem> {
        let item = build_item(mtm, it, tag, &target);
        tag += 1;
        item
    };

    // 1. The application menu — winit's, with rt's Settings… slipped in.
    //
    // Index 2 is directly after winit's "About rt" and the separator that
    // follows it, which is where Apple puts Settings. `min` keeps it in range
    // if winit's menu ever gets shorter; appending is a worse position, never a
    // crash.
    if !model.app_menu.is_empty() {
        match main_menu.itemAtIndex(0).and_then(|first| first.submenu()) {
            Some(app_menu) => {
                let mut at = app_menu.numberOfItems().min(2);
                for it in &model.app_menu {
                    app_menu.insertItem_atIndex(&next(it), at);
                    at += 1;
                }
                app_menu.insertItem_atIndex(&NSMenuItem::separatorItem(mtm), at);
            }
            None => {
                // Nothing to insert into. Still burn the tags so every later
                // item's tag matches its position in `model.items()`.
                log::debug!("menu bar: no application submenu; Settings… stays keyboard-only");
                for it in &model.app_menu {
                    let _ = next(it);
                }
            }
        }
    }

    // 2. rt's own menus, appended after the application menu.
    for m in &model.menus {
        let title = NSString::from_str(m.title);
        // The bar shows the SUBMENU's title; the item's title is set to match so
        // the two can never disagree.
        let submenu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &title);
        // AppKit's default, said out loud: it is what makes AppKit ask
        // `-validateMenuItem:` before showing the menu and before firing a key
        // equivalent, which is rt's whole enable/disable mechanism.
        submenu.setAutoenablesItems(true);
        let holder = NSMenuItem::new(mtm);
        holder.setTitle(&title);
        for it in &m.items {
            submenu.addItem(&next(it));
        }
        holder.setSubmenu(Some(&submenu));
        main_menu.addItem(&holder);
    }

    log::info!("menu bar: installed {} menus ({} items)", model.menus.len(), tag);
    Some(MenuBar { target })
}

impl MenuTarget {
    fn new(mtm: MainThreadMarker, bridge: Bridge) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(bridge);
        unsafe { msg_send![super(this), init] }
    }
}

/// One `NSMenuItem` (or a divider) from one model row.
fn build_item(mtm: MainThreadMarker, it: &BarItem, tag: isize, target: &MenuTarget) -> Retained<NSMenuItem> {
    if it.is_separator() {
        return NSMenuItem::separatorItem(mtm);
    }
    let title = NSString::from_str(&it.label);
    let key = NSString::from_str(it.key.as_ref().map(|k| k.key.as_str()).unwrap_or(""));
    // SAFETY: a plain designated initialiser; `rtMenuAction:` is implemented by
    // `MenuTarget` just above, and every item is targeted at one.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &title, Some(sel!(rtMenuAction:)), &key)
    };
    if let Some(k) = &it.key {
        let mut mask = NSEventModifierFlags::empty();
        if k.command {
            mask |= NSEventModifierFlags::Command;
        }
        if k.shift {
            mask |= NSEventModifierFlags::Shift;
        }
        if k.control {
            mask |= NSEventModifierFlags::Control;
        }
        if k.option {
            mask |= NSEventModifierFlags::Option;
        }
        item.setKeyEquivalentModifierMask(mask);
    }
    item.setTag(tag);
    // SAFETY: `target` outlives every menu item — `MenuBar` holds the only
    // strong reference and lives on `App` for the whole process. This is the
    // reason it does: `target` is a weak (unretained) property.
    unsafe { item.setTarget(Some(target.as_ref())) };
    item
}
