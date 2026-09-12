//! rt's native macOS Manual window: the AppKit half of Help → rt Manual (F1).
//!
//! An `NSScrollView` around an `NSTextView`: selectable, scrollable, resizable,
//! and searchable with ⌘F. On Linux nothing here exists and the self-drawn
//! `chrome::manual` overlay is untouched.
//!
//! **The text and its typography are not this file's.** The manual is
//! [`crate::manual::MANUAL`]; the wrapping, the derived key column, the hanging
//! indents and the heading/body/version classification are
//! [`crate::chrome::manual::wrapped`] — the same pure function the Linux overlay
//! draws from, and the same `measure_cap()` measure. Dumping `MANUAL` in as one
//! flat string would have thrown all of that away; instead the wrapped lines are
//! set here, with each [`LineKind`] and each line's key/description split given
//! its own run in an `NSAttributedString`. So the two platforms show the same
//! document, set the same way, and editing `MANUAL` moves both.
//!
//! ## ⌘F
//!
//! `setUsesFindBar:YES` gives an `NSTextView` the standard find bar for free —
//! the same one Xcode and TextEdit slide down, with live match counts and
//! next/previous. What it does NOT come with is a key equivalent: ⌘F is
//! conventionally a menu item (`Edit ▸ Find`) that targets the first responder,
//! and rt's menu bar has no Find item of its own to lend (its ⌘F is the
//! terminal's scrollback search, which is greyed while this window is key — see
//! `App::native_window_has_key`). So the text view claims ⌘F itself, in
//! `performKeyEquivalent:`, and turns it into the `NSTextFinderAction` the find
//! bar answers to.
//!
//! ## Lifetime
//!
//! As `settings_window.rs`: built once, `setReleasedWhenClosed:NO` so this
//! struct's `Retained` stays valid across a click on the red dot, re-shown with
//! `makeKeyAndOrderFront:`, and ordered out by `App::hide_native_windows` on the
//! way to `exit()`.

use objc2::rc::Retained;
use objc2::runtime::NSObjectProtocol;
use objc2::runtime::Bool;
use objc2::{define_class, msg_send, AnyThread, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSColor, NSEvent, NSEventModifierFlags, NSFont, NSFontWeightBold,
    NSFontWeightRegular, NSMenuItem, NSScrollView, NSTextView, NSWindow, NSWindowStyleMask,
};
use objc2_foundation::{
    NSAttributedString, NSDictionary, NSMutableAttributedString, NSPoint, NSRect, NSSize, NSString,
};

use crate::chrome::manual::{measure_cap, wrapped, Line, LineKind};

/// `NSTextFinderActionShowFindInterface`. AppKit reads it off the sender's
/// `tag`, which is why the ⌘F handler builds a throwaway `NSMenuItem` to carry
/// it rather than calling a method that takes the enum.
const SHOW_FIND_INTERFACE: isize = 1;

/// Points. 12 is `NSFont`'s own small-monospace size and matches what the find
/// bar and the rest of the system chrome are sized against.
const TEXT_PT: f64 = 12.0;
const INSET: f64 = 18.0;

define_class!(
    // SAFETY:
    // - NSTextView permits subclassing; nothing here overrides a method with an
    //   initialisation or layout contract, only `performKeyEquivalent:`.
    // - ManualText does not implement Drop.
    // - MainThreadOnly because NSTextView is, and because AppKit only sends key
    //   equivalents on the main thread.
    #[unsafe(super(NSTextView))]
    #[thread_kind = MainThreadOnly]
    #[name = "RtManualTextView"]
    // See the same note in `settings_window.rs`: the unit ivar is what makes
    // `msg_send![super(..), init]` available on a subclass.
    #[ivars = ()]
    struct ManualText;

    impl ManualText {
        /// Claim ⌘F for the find bar. Everything else is passed to
        /// `NSTextView`'s own implementation, which is what keeps ⌘C, ⌘A and
        /// the rest working normally.
        #[unsafe(method(performKeyEquivalent:))]
        fn perform_key_equivalent(&self, event: &NSEvent) -> Bool {
            let mods = event.modifierFlags();
            let chars = event.charactersIgnoringModifiers();
            let is_find = mods.contains(NSEventModifierFlags::Command)
                && !mods.contains(NSEventModifierFlags::Option)
                && !mods.contains(NSEventModifierFlags::Control)
                && chars.map(|c| c.to_string() == "f").unwrap_or(false);
            if is_find {
                if let Some(mtm) = MainThreadMarker::new() {
                    let sender = NSMenuItem::new(mtm);
                    sender.setTag(SHOW_FIND_INTERFACE);
                    // SAFETY: the documented entry point for a find bar; AppKit
                    // reads only `[sender tag]` off it.
                    unsafe {
                        let _: () = msg_send![self, performTextFinderAction: &*sender];
                    }
                    return Bool::YES;
                }
            }
            unsafe { msg_send![super(self), performKeyEquivalent: event] }
        }
    }

    unsafe impl NSObjectProtocol for ManualText {}
);

/// rt's Manual window, once built. Owned by `App` for the life of the process.
pub struct ManualWindow {
    window: Retained<NSWindow>,
    /// Held so the text view outlives the `performKeyEquivalent:` registration
    /// and so the window has a strong reference to its own document view
    /// regardless of what the scroll view does with it.
    #[allow(dead_code)]
    text: Retained<ManualText>,
}

impl ManualWindow {
    /// Build the window (hidden). `None` off the main thread — as everywhere in
    /// rt's AppKit code, a failure is silent and costs only the feature.
    pub fn new() -> Option<ManualWindow> {
        let mtm = MainThreadMarker::new()?;
        let font = unsafe { NSFont::monospacedSystemFontOfSize_weight(TEXT_PT, NSFontWeightRegular) };
        let bold = unsafe { NSFont::monospacedSystemFontOfSize_weight(TEXT_PT, NSFontWeightBold) };
        // Wide enough that the pre-wrapped measure is never re-wrapped: the
        // hanging indents and the key column only line up at the measure
        // `chrome::manual` set them to.
        let cell = font.maximumAdvancement().width;
        let text_w = measure_cap() as f64 * cell;
        let win_w = (text_w + INSET * 2.0 + 18.0).min(1100.0);
        let win_h = 720.0;

        let style = NSWindowStyleMask::Titled
            | NSWindowStyleMask::Closable
            | NSWindowStyleMask::Miniaturizable
            | NSWindowStyleMask::Resizable;
        let frame = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(win_w, win_h));
        // SAFETY: the designated initialiser, with a style mask AppKit accepts.
        // An RtPanelWindow, so ⌘W closes it — see native_window.rs.
        let window: Retained<NSWindow> = crate::native_window::panel_window(mtm, frame, style);
        window.setTitle(&NSString::from_str("rt Manual"));
        // See the module docs: this struct holds the only strong reference.
        // SAFETY: see `settings_window.rs` — this struct holds the only strong
        // reference, so AppKit's release-on-close default is the hazard.
        unsafe { window.setReleasedWhenClosed(false) };
        window.setMinSize(NSSize::new(360.0, 240.0));
        window.center();

        let content = NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(win_w - 18.0, win_h));
        // `init` then `setFrame:`: see the same note in `settings_window.rs`.
        // `-[NSTextView init]` is `initWithFrame:NSZeroRect`, which builds the
        // default layout manager / text container / text storage stack.
        let text: Retained<ManualText> = {
            let this = ManualText::alloc(mtm).set_ivars(());
            unsafe { msg_send![super(this), init] }
        };
        text.setFrame(content);
        text.setEditable(false);
        text.setSelectable(false); // re-enabled below; ordering keeps AppKit happy
        text.setSelectable(true);
        text.setDrawsBackground(true);
        text.setTextContainerInset(NSSize::new(INSET, INSET));
        // The find bar, and live highlighting as the query is typed.
        text.setUsesFindBar(true);
        text.setIncrementalSearchingEnabled(true);
        // Grows downwards inside the scroll view; the width follows the window
        // so a user who narrows it gets a soft wrap rather than clipped text.
        text.setVerticallyResizable(true);
        text.setHorizontallyResizable(false);
        if let Some(container) = unsafe { text.textContainer() } {
            container.setWidthTracksTextView(true);
        }
        if let Some(storage) = unsafe { text.textStorage() } {
            storage.setAttributedString(&document(&font, &bold));
        }

        let scroll = NSScrollView::new(mtm);
        scroll.setHasVerticalScroller(true);
        scroll.setAutohidesScrollers(false);
        scroll.setDrawsBackground(true);
        scroll.setDocumentView(Some(&text));
        window.setContentView(Some(&scroll));
        Some(ManualWindow { window, text })
    }

    pub fn show(&self) {
        self.window.makeKeyAndOrderFront(None);
    }

    pub fn hide(&self) {
        self.window.orderOut(None);
    }

    /// See `App::native_window_has_key` for why anyone asks.
    pub fn is_key(&self) -> bool {
        self.window.isKeyWindow()
    }
}

/// The whole manual as an attributed string, one run per typographic role.
///
/// Three roles, matching `chrome::manual::draw`'s three: the version header and
/// the UPPERCASE section headings are set bold in the primary label colour; a
/// two-column row's KEY is set bold too, so the key column reads as a column;
/// everything else is body text in the secondary colour. Blank lines, hanging
/// indents and the derived key column all come through `wrapped` unchanged.
fn document(font: &NSFont, bold: &NSFont) -> Retained<NSMutableAttributedString> {
    let out = NSMutableAttributedString::new();
    let body = attrs(font, &NSColor::labelColor());
    let dim = attrs(font, &NSColor::secondaryLabelColor());
    let head = attrs(bold, &NSColor::labelColor());
    let key = attrs(bold, &NSColor::labelColor());
    for line in wrapped(measure_cap()) {
        let Line { kind, text, key_end } = line;
        match kind {
            LineKind::Version | LineKind::Heading => append(&out, &text, &head),
            LineKind::Body => match key_end {
                // `key_end` is a CHAR index, so the split has to be taken in
                // chars — the manual has non-ASCII in it (arrows, ≈, ⌘).
                Some(end) => {
                    let chars: Vec<char> = text.chars().collect();
                    let end = end.min(chars.len());
                    append(&out, &chars[..end].iter().collect::<String>(), &key);
                    append(&out, &chars[end..].iter().collect::<String>(), &dim);
                }
                None => append(&out, &text, &body),
            },
        }
        append(&out, "\n", &body);
    }
    out
}

fn attrs(font: &NSFont, colour: &NSColor) -> Retained<NSDictionary<NSString, objc2::runtime::AnyObject>> {
    // SAFETY: the two documented attribute keys, with values of the types they
    // are declared to take.
    unsafe {
        NSDictionary::from_slices(
            &[
                objc2_app_kit::NSFontAttributeName,
                objc2_app_kit::NSForegroundColorAttributeName,
            ],
            &[
                &*(font as *const NSFont as *const objc2::runtime::AnyObject),
                &*(colour as *const NSColor as *const objc2::runtime::AnyObject),
            ],
        )
    }
}

fn append(
    out: &NSMutableAttributedString,
    text: &str,
    attrs: &NSDictionary<NSString, objc2::runtime::AnyObject>,
) {
    if text.is_empty() {
        return;
    }
    // SAFETY: a plain designated initialiser over a valid attribute dictionary.
    let run = unsafe {
        NSAttributedString::initWithString_attributes(
            NSAttributedString::alloc(),
            &NSString::from_str(text),
            Some(attrs),
        )
    };
    out.appendAttributedString(&run);
}
