//! rt's native macOS menus: the AppKit half. Two of them, one file.
//!
//! 1. **The menu bar** ([`install`]) — Mac users look for a program's menu on
//!    the top bar, not under a right-click, and every [`Action`] rt has is up
//!    there.
//! 2. **The right-click popup** ([`popup`]) — the same rows rt's own menu has,
//!    as a real `NSMenu`. On macOS rt draws no menu of its own at all:
//!    `chrome::menu` is never reached, because `main.rs` never sets
//!    `Active::menu` on this target. Linux keeps its self-drawn menu untouched;
//!    it has no NSMenu to use.
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

use std::sync::{Arc, Mutex};

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly, Message};
use objc2_app_kit::{NSApplication, NSEventModifierFlags, NSMenu, NSMenuItem, NSMenuItemValidation, NSView};
use objc2_foundation::{NSArray, NSPoint, NSRunLoopCommonModes, NSString};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use rt_config::Action;
use winit::event_loop::EventLoopProxy;
use winit::window::Window;

use crate::menu::MenuPick;
use crate::menubar_model::{BarItem, BarModel, KeyEquivalent, PopupItem};

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
    set_modifier_mask(&item, it.key.as_ref());
    item.setTag(tag);
    // SAFETY: `target` outlives every menu item — `MenuBar` holds the only
    // strong reference and lives on `App` for the whole process. This is the
    // reason it does: `target` is a weak (unretained) property.
    unsafe { item.setTarget(Some(target.as_ref())) };
    item
}

/// The ⌃⌥⇧⌘ half of a key equivalent. The character itself goes in at
/// `initWithTitle:action:keyEquivalent:`; this is the mask that goes with it.
/// Shared by the menu bar and the popup so one chord cannot be spelled two ways.
fn set_modifier_mask(item: &NSMenuItem, key: Option<&KeyEquivalent>) {
    let Some(k) = key else { return };
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

// ---------------------------------------------------------------------------
// The right-click popup
// ---------------------------------------------------------------------------
//
// On macOS rt does not draw a menu. `chrome::menu` — rt's own panel, its
// hit-test, its scrolling — is never reached, because `main.rs` never sets
// `Active::menu` on this target; a right-click pops a real `NSMenu` instead,
// with the system's own appearance, shadow, scrolling and keyboard handling.
// Linux keeps the self-drawn menu exactly as it is: it has no NSMenu to use.
//
// ## Why it is not shown from inside the right-click handler
//
// `popUpMenuPositioningItem:atLocation:inView:` is MODAL: it runs a nested event
// loop and does not return until the menu is dismissed. Calling it from inside
// `ApplicationHandler::window_event` would start that nested loop with winit's
// handler already on the stack, and every event the nested loop pumps would
// re-enter it — `&mut App` twice over.
//
// So the show is DEFERRED by one turn of the run loop, with
// `-performSelector:withObject:afterDelay:0`. That fires from the run loop
// itself, with no winit handler on the stack, which is exactly the position
// AppKit is in when it drops a menu-bar menu down — a nested tracking loop rt
// already runs safely today. `performSelector:` also RETAINS its receiver until
// it fires, which is what keeps the target (and through it the rows) alive
// across the gap without `App` having to own it.
//
// ## Getting the pick back
//
// Same route as the menu bar, and for the same reason: the click lands in
// AppKit's loop with no `&mut App` in reach. The [`MenuPick`] goes into a shared
// queue and `EventLoopProxy::wake_up` brings the loop round to drain it. The
// queue is an `Arc` rather than an ivar because the target is freed as soon as
// the deferred selector returns, while the pick still has to outlive it.

/// Where popup picks wait for the event loop. Cloned into the AppKit target;
/// `App` keeps the other end and drains it in `proxy_wake_up`.
#[derive(Clone, Default)]
pub struct PopupOutbox(Arc<Mutex<Vec<MenuPick>>>);

impl PopupOutbox {
    /// Take the picks made since the last call, oldest first.
    pub fn take(&self) -> Vec<MenuPick> {
        self.0.lock().map(|mut q| std::mem::take(&mut *q)).unwrap_or_default()
    }
}

/// The shared state one popup needs, from the moment it is scheduled to the
/// moment its click is queued.
struct PopupBridge {
    /// The rows, in order. An item's tag is its index here.
    items: Vec<PopupItem>,
    /// The view the menu is positioned in, and where in it (view coordinates,
    /// points). Retained: the window could in principle go away between the
    /// scheduling and the show, and a freed view would be a dangling receiver.
    view: Retained<NSView>,
    at: NSPoint,
    out: PopupOutbox,
    proxy: EventLoopProxy,
}

define_class!(
    // SAFETY:
    // - NSObject has no subclassing requirements.
    // - PopupTarget does not implement Drop.
    // - MainThreadOnly because NSMenu and NSView are, and because both methods
    //   below are only ever sent by AppKit on the main thread.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "RtMenuPopupTarget"]
    #[ivars = PopupBridge]
    struct PopupTarget;

    impl PopupTarget {
        /// Build the `NSMenu` and pop it up. Runs one turn of the run loop after
        /// the right-click, from `-performSelector:withObject:afterDelay:`, so
        /// winit's handler is not on the stack when the modal tracking loop
        /// starts. Blocks until the user picks or dismisses.
        #[unsafe(method(rtPopupShow:))]
        fn rt_popup_show(&self, _sender: Option<&NSObject>) {
            let Some(mtm) = MainThreadMarker::new() else { return };
            let b = self.ivars();
            let menu = NSMenu::initWithTitle(NSMenu::alloc(mtm), &NSString::from_str(""));
            // Explicit: rt decides what is live from its own model, so AppKit
            // must not second-guess it (this target answers no validation
            // protocol, and autoenabling would enable every row with a target).
            menu.setAutoenablesItems(false);
            for (tag, it) in b.items.iter().enumerate() {
                menu.addItem(&popup_item(mtm, it, tag as isize, self));
            }
            // `item: None` puts the menu's top-left corner at the point, which
            // is where a context menu belongs relative to the click. AppKit
            // clamps it onto the screen and scrolls it itself — the two things
            // rt's own menu had to grow code for.
            menu.popUpMenuPositioningItem_atLocation_inView(None, b.at, Some(&b.view));
        }

        /// A row was picked. Queue it and wake the loop — never act here.
        #[unsafe(method(rtPopupAction:))]
        fn rt_popup_action(&self, sender: Option<&NSMenuItem>) {
            let tag = sender.map(|s| s.tag()).unwrap_or(-1);
            let b = self.ivars();
            let Some(pick) = usize::try_from(tag).ok().and_then(|i| b.items.get(i)).and_then(|i| i.pick()) else {
                log::debug!("popup menu: pick on tag {tag} with no action");
                return;
            };
            if let Ok(mut q) = b.out.0.lock() {
                q.push(pick.clone());
            }
            // Payload first, then the wake-up — otherwise the loop can turn on
            // an empty queue and the pick is lost.
            b.proxy.wake_up();
        }
    }

    unsafe impl NSObjectProtocol for PopupTarget {}
);

impl PopupTarget {
    fn new(mtm: MainThreadMarker, bridge: PopupBridge) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(bridge);
        unsafe { msg_send![super(this), init] }
    }
}

/// One `NSMenuItem` (or a divider) from one popup row.
fn popup_item(mtm: MainThreadMarker, it: &PopupItem, tag: isize, target: &PopupTarget) -> Retained<NSMenuItem> {
    let PopupItem::Row { label, key, enabled, pick } = it else {
        return NSMenuItem::separatorItem(mtm);
    };
    let title = NSString::from_str(label);
    let equiv = NSString::from_str(key.as_ref().map(|k| k.key.as_str()).unwrap_or(""));
    // An informational row (the version footer) gets NO action at all, so it is
    // inert even if something later re-enables it.
    let action = pick.as_ref().map(|_| sel!(rtPopupAction:));
    // SAFETY: a plain designated initialiser; `rtPopupAction:` is implemented by
    // `PopupTarget` just above, and every item carrying it is targeted at one.
    let item = unsafe { NSMenuItem::initWithTitle_action_keyEquivalent(NSMenuItem::alloc(mtm), &title, action, &equiv) };
    set_modifier_mask(&item, key.as_ref());
    item.setTag(tag);
    item.setEnabled(*enabled);
    // SAFETY: `target` is alive for the whole of `rtPopupShow:` — it is the
    // `&self` that built this item, and `performSelector:` holds it across the
    // blocking pop-up. `NSMenuItem.target` is weak, which is why that matters.
    unsafe { item.setTarget(Some(target.as_ref())) };
    item
}

/// Schedule a native right-click menu for `window`, at `at` (window-local
/// PHYSICAL pixels, winit's coordinates, origin top-left).
///
/// Returns `false` when AppKit could not be reached at all — off the main
/// thread, or a window with no AppKit handle. Like `install` and `vibrancy.rs`,
/// every failure path logs and returns; nothing here can panic, and a right-click
/// that reaches nothing is the worst case.
pub fn popup(items: Vec<PopupItem>, window: &dyn Window, at: (f32, f32), proxy: EventLoopProxy, out: &PopupOutbox) -> bool {
    let Some(mtm) = MainThreadMarker::new() else {
        log::debug!("popup menu: not on the main thread; skipping");
        return false;
    };
    let ns_view = match window.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::AppKit(h)) => h.ns_view,
        _ => {
            log::debug!("popup menu: no AppKit window handle; skipping");
            return false;
        }
    };
    // SAFETY: the handle comes straight from winit's live window and names a
    // valid NSView; we are on the main thread (proven by `mtm`), which is where
    // NSView lives, and we retain it for as long as we hold it.
    let view: Retained<NSView> = unsafe { ns_view.cast::<NSView>().as_ref().retain() };

    // winit talks in physical pixels from the top-left of the surface; AppKit
    // wants points in the view's own coordinate system, whose origin is at the
    // BOTTOM-left unless the view says otherwise. `isFlipped` is asked rather
    // than assumed — it is winit's view, not rt's.
    let scale = window.scale_factor().max(f64::MIN_POSITIVE);
    let (lx, ly) = (f64::from(at.0) / scale, f64::from(at.1) / scale);
    let height = view.bounds().size.height;
    let y = if view.isFlipped() { ly } else { height - ly };
    let target = PopupTarget::new(mtm, PopupBridge {
        items,
        view,
        at: NSPoint::new(lx, y),
        out: out.clone(),
        proxy,
    });
    // Deferred by one turn of the run loop; see the section comment above.
    // `performSelector:` retains `target` until it fires, so dropping the last
    // Rust reference on the next line is correct.
    //
    // `inModes:` with the COMMON modes, not the plain three-argument form: that
    // one schedules in `NSDefaultRunLoopMode` alone, and the right-click that
    // gets us here is a mouse-DOWN — the run loop may sit in
    // `NSEventTrackingRunLoopMode` until the button comes up, which would hold
    // the menu back until the release. The common modes include event tracking,
    // which is also the set winit's own run-loop observer and timer are
    // registered in (`winit-appkit/src/observer.rs`), so the deferred show and
    // the event loop agree about when "the next turn" is.
    //
    // SAFETY: `rtPopupShow:` is implemented by `PopupTarget` and takes one
    // (ignored) object argument, which is what this selector family sends.
    let modes = NSArray::from_slice(&[unsafe { NSRunLoopCommonModes }]);
    unsafe {
        let _: () = msg_send![
            &*target,
            performSelector: sel!(rtPopupShow:),
            withObject: Option::<&NSObject>::None,
            afterDelay: 0.0f64,
            inModes: &*modes,
        ];
    }
    true
}
