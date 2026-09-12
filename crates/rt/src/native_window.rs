//! The `NSWindow` subclass rt's native Settings and Manual windows are built
//! from, so ⌘W closes the front window the way a Mac user expects.
//!
//! ## Why a window subclass, and not a menu item
//!
//! On macOS ⌘W conventionally lives on a *menu item* (File → Close), and AppKit
//! synthesises it from that item's key equivalent. rt cannot use that route:
//! its menu bar is greyed while one of these windows is key (see
//! [`crate::App::native_window_has_key`]), so a Close item would be **inert
//! exactly when it is needed** — the moment the window has focus.
//!
//! A key window gets first crack at `performKeyEquivalent:` before the main
//! menu is ever consulted, so catching ⌘W here fires while the window is front,
//! menu-bar state notwithstanding. Everything that is *not* ⌘W is passed to
//! `NSWindow`'s own implementation, which walks the content view's subtree — so
//! the Manual window's `NSTextView` still gets its ⌘F, and ⌘C / ⌘A keep working
//! normally.
//!
//! ## `performClose:`, not `close`
//!
//! `performClose:` is the red dot's own path: it runs `windowShouldClose:` if a
//! delegate is set, animates, and — because both windows are
//! `setReleasedWhenClosed:NO` and each owning struct holds the only strong
//! reference — orders the window out **without deallocating it**, so the next
//! ⌘, or Help re-shows the same instance. `close` skips the delegate and the
//! animation and is the wrong verb for a user-initiated close.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Bool, NSObjectProtocol};
use objc2::{define_class, msg_send, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{NSBackingStoreType, NSEvent, NSEventModifierFlags, NSWindow, NSWindowStyleMask};
use objc2_foundation::NSRect;

define_class!(
    // SAFETY:
    // - NSWindow permits subclassing; this overrides only `performKeyEquivalent:`,
    //   which carries no initialisation or layout contract, and delegates every
    //   other key equivalent to `super`.
    // - RtPanelWindow does not implement Drop.
    // - MainThreadOnly because NSWindow is, and AppKit only sends key
    //   equivalents on the main thread.
    #[unsafe(super(NSWindow))]
    #[thread_kind = MainThreadOnly]
    #[name = "RtPanelWindow"]
    // The unit ivar is what makes `msg_send![super(..), init]`-family methods
    // available on the subclass; see the same note in `settings_window.rs`.
    #[ivars = ()]
    struct RtPanelWindow;

    impl RtPanelWindow {
        /// Claim ⌘W for `performClose:`. Everything else — including the Manual
        /// window's ⌘F, which lives on the text view `super` walks to — is
        /// passed straight through to `NSWindow`.
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> Bool {
            let mods = event.modifierFlags();
            let chars = event.charactersIgnoringModifiers();
            let is_close = mods.contains(NSEventModifierFlags::Command)
                && !mods.contains(NSEventModifierFlags::Option)
                && !mods.contains(NSEventModifierFlags::Control)
                && !mods.contains(NSEventModifierFlags::Shift)
                && chars.map(|c| c.to_string() == "w").unwrap_or(false);
            if is_close {
                // SAFETY: `performClose:` is an NSWindow method the class
                // responds to; the sender is only read for a menu-item title
                // check, so `nil` is fine.
                unsafe {
                    let _: () = msg_send![self, performClose: Option::<&AnyObject>::None];
                }
                return Bool::YES;
            }
            unsafe { msg_send![super(self), performKeyEquivalent: event] }
        }
    }

    unsafe impl NSObjectProtocol for RtPanelWindow {}
);

/// Build an `RtPanelWindow` and hand it back typed as `NSWindow` — the two
/// window structs keep their existing `Retained<NSWindow>` field, and only the
/// concrete class behind it changes. Mirrors the exact
/// `initWithContentRect:styleMask:backing:defer:` both call sites used.
pub fn panel_window(
    mtm: MainThreadMarker,
    frame: NSRect,
    style: NSWindowStyleMask,
) -> Retained<NSWindow> {
    // `.set_ivars(())` then `super(this)` is the objc2 idiom for a subclass that
    // inherits its superclass's designated initialiser — the same shape
    // `manual_window.rs` uses to build its `NSTextView` subclass.
    let this = RtPanelWindow::alloc(mtm).set_ivars(());
    // SAFETY: `initWithContentRect:styleMask:backing:defer:` is `NSWindow`'s
    // designated initialiser; the argument types match its signature, and it is
    // sent to `super` on a freshly allocated `RtPanelWindow`.
    let window: Retained<RtPanelWindow> = unsafe {
        msg_send![super(this),
            initWithContentRect: frame,
            styleMask: style,
            backing: NSBackingStoreType::Buffered,
            defer: false,
        ]
    };
    // Hand it back as the plain `NSWindow` both call sites store.
    window.into_super()
}
