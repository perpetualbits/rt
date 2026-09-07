//! macOS receiver for text dragged in from another application.
//!
//! Sibling of `vibrancy.rs`: it reaches winit's `NSView` through the same
//! `AppKitWindowHandle` path, does one AppKit thing, and degrades to a quiet
//! no-op on every failure. The DECISIONS — which pane takes the drop, and what
//! bytes it delivers — are not here; they are in `crate::textdrop`, which Linux
//! CI compiles and tests. This file is only the wire.
//!
//! ## Why winit cannot do this
//!
//! winit's macOS backend registers the **`NSWindow`** for exactly one dragged
//! type, `NSFilenamesPboardType`, and implements `NSDraggingDestination` on its
//! `WinitWindowDelegate` (`winit-appkit-0.31.0-beta.2/src/window_delegate.rs`).
//! Its `draggingEntered:`/`performDragOperation:` both start with
//! `propertyListForType(NSFilenamesPboardType)` and `return false` when that is
//! absent — so a text drag is refused before any winit event exists. The public
//! surface says the same thing: `WindowEvent::DragEntered { paths, .. }` carries
//! `Vec<PathBuf>` and nothing else. There is no MIME/pasteboard-type hook.
//!
//! ## Why a subview, and why it does not steal the mouse
//!
//! An `NSDraggingDestination` has to be an object AppKit already asks. The three
//! candidates and why two are closed:
//!
//! * **winit's content view** — the class is winit's; adding the protocol
//!   methods to it means swizzling `WinitView` at runtime.
//! * **the window / its delegate** — the window IS registered, but Apple's
//!   *Dragging Destinations* is explicit that "the delegate's implementation
//!   takes precedence if there are implementations in both places", so even an
//!   `NSWindow` subclass installed by isa-swizzling would lose to winit's
//!   delegate. Replacing the delegate means proxying every `NSWindowDelegate`
//!   message winit depends on.
//! * **our own view, as a subview of the content view** — public API only, no
//!   swizzling, and rt owns the class outright. This is what we do.
//!
//! The catch `vibrancy.rs` documents for its effect view applies here too: a
//! subview covering winit's view would become the answer to `-[NSView hitTest:]`
//! for every click, and winit's view would stop being the mouse target. The
//! standard AppKit answer is a **glass view** — `hitTest:` returns `nil`, so
//! pointer events fall through to the view beneath, while drag delivery is
//! unaffected because a dragging destination is found from the window's
//! registered-view list rather than from `hitTest:`. rt relies on exactly that
//! separation, and it is the one property here that no test on this machine can
//! prove: if a build ever stops receiving drops while clicks still work, this is
//! the assumption that broke.
//!
//! Registering only `NSPasteboardTypeString` also keeps the two receivers apart
//! by construction: a **file** drag carries no string type, matches nothing here,
//! and goes on reaching winit's window-level destination exactly as before. We
//! never touch the window's own registration.
//!
//! ## How a drop reaches the event loop
//!
//! AppKit calls these methods on the main thread, from the main run loop —
//! including while a drag is tracking, since winit's `CFRunLoopObserver` is
//! installed in `kCFRunLoopCommonModes` (which contains
//! `NSEventTrackingRunLoopMode`). So the view writes into a plain
//! `Rc<RefCell<DropInbox>>` shared with `Active`, and `App::about_to_wait` —
//! which that same observer drives — picks it up on the next turn of the loop.
//! No channel, no wakeup call, no thread. It is the same latency winit's own
//! `DragMoved` events get, because it is the same mechanism.
use std::cell::RefCell;
use std::rc::Rc;

use objc2::rc::Retained;
use objc2::runtime::ProtocolObject;
use objc2::{DefinedClass, MainThreadMarker, define_class, msg_send};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSDragOperation, NSDraggingDestination, NSDraggingInfo,
    NSPasteboardTypeString, NSResponder, NSView,
};
use objc2_foundation::{NSArray, NSObject, NSObjectProtocol, NSPoint};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// What the AppKit side has to say, read once per turn of the event loop by
/// `App::about_to_wait`.
///
/// Positions are in the same space `winit` reports the pointer in: **physical
/// pixels, origin at the top-left of the window's content area** — so they can
/// be hit-tested against `Session::visible_rects` with no further conversion.
/// (`draggingLocation` is window-base, bottom-left, in points; the view converts
/// it and multiplies by the window's backing factor.)
#[derive(Default)]
pub struct DropInbox {
    /// Where a text drag is hovering right now, if one is over this window.
    /// `None` once it leaves or drops.
    pub hover: Option<(f32, f32)>,
    /// `textdrop::ghost_label` for the payload currently hovering — computed
    /// ONCE, when the drag enters, and reused for every motion after it. A
    /// dragged browser selection can be megabytes; converting the whole
    /// `NSString` on each `draggingUpdated:` would make the cue's cost scale
    /// with the payload rather than with the gesture.
    pub label: String,
    /// Whether the drag currently over this window is one rt claimed on entry.
    /// A separate flag rather than "the label is non-empty", because a payload
    /// CAN label as the empty string (a lone newline) and would then be silently
    /// declined for the rest of the gesture.
    pub accepted: bool,
    /// A completed drop, waiting to be delivered: where, and what.
    pub dropped: Option<((f32, f32), String)>,
    /// Set by the view on every change, cleared by the reader. Lets the loop
    /// skip the work (and the repaint) on the overwhelming majority of turns,
    /// when nothing is being dragged at all.
    pub changed: bool,
}

impl DropInbox {
    /// Take whatever changed since the last look. `None` when nothing did — the
    /// state of affairs on essentially every turn of the loop, which is why the
    /// caller can skip re-deriving the window's layout.
    pub fn take_change(&mut self) -> Option<crate::textdrop::DropNews> {
        if !self.changed {
            return None;
        }
        self.changed = false;
        Some(crate::textdrop::DropNews {
            hover: self.hover.map(|at| (at, self.label.clone())),
            dropped: self.dropped.take(),
        })
    }
}

define_class!(
    /// A full-bleed, click-through `NSView` whose only job is to be a dragging
    /// destination for `NSPasteboardTypeString`. It draws nothing and owns
    /// nothing but the shared inbox.
    #[unsafe(super(NSView, NSResponder, NSObject))]
    #[name = "RtTextDropView"]
    #[ivars = Rc<RefCell<DropInbox>>]
    struct TextDropView;

    unsafe impl NSObjectProtocol for TextDropView {}

    impl TextDropView {
        /// Top-left origin, matching winit's view — so the converted drag
        /// location needs no y-flip before it is compared with pane rects.
        #[unsafe(method(isFlipped))]
        fn is_flipped(&self) -> bool {
            true
        }

        /// The glass-view half of the module doc: never the answer to a mouse
        /// hit-test, so clicks, selection, scrolling, the cursor shape and IME
        /// all keep landing on winit's view underneath. Drag delivery does not
        /// go through `hitTest:` and is unaffected.
        ///
        /// Returned as a raw pointer rather than `Option<Retained<NSView>>`:
        /// `hitTest:` belongs to no method family, and nil is the whole answer.
        #[unsafe(method(hitTest:))]
        fn hit_test(&self, _point: NSPoint) -> *mut NSView {
            std::ptr::null_mut()
        }
    }

    unsafe impl NSDraggingDestination for TextDropView {
        /// The one place the payload is read during the gesture: the ghost
        /// label is built here and cached (see [`DropInbox::label`]).
        #[unsafe(method(draggingEntered:))]
        fn dragging_entered(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            let Some(text) = self.dragged_text(sender) else {
                return NSDragOperation::None; // not text: not ours
            };
            {
                let mut inbox = self.ivars().borrow_mut();
                inbox.label = crate::textdrop::ghost_label(&text);
                inbox.accepted = true;
            }
            self.note_hover(sender, true)
        }

        #[unsafe(method(draggingUpdated:))]
        fn dragging_updated(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> NSDragOperation {
            self.note_hover(sender, false)
        }

        /// No timer-driven updates: the cue only has to move when the pointer
        /// does, and `draggingUpdated:` already fires on every motion.
        #[unsafe(method(wantsPeriodicDraggingUpdates))]
        fn wants_periodic_dragging_updates(&self) -> bool {
            false
        }

        #[unsafe(method(draggingExited:))]
        fn dragging_exited(&self, _sender: Option<&ProtocolObject<dyn NSDraggingInfo>>) {
            let mut inbox = self.ivars().borrow_mut();
            inbox.hover = None;
            inbox.label.clear();
            inbox.accepted = false;
            inbox.changed = true; // so the loop repaints the cue away
        }

        #[unsafe(method(prepareForDragOperation:))]
        fn prepare_for_drag_operation(&self, _sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            true
        }

        #[unsafe(method(performDragOperation:))]
        fn perform_drag_operation(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> bool {
            // One expression, no early `return`: objc2 converts the TAIL of a
            // `-> bool` method body into an ObjC `BOOL`, and an explicit
            // `return false` skips that conversion (it fails to compile as
            // `expected Bool, found bool` — the sort of error worth not
            // rediscovering).
            match (self.dragged_text(sender), self.location(sender)) {
                (Some(text), Some(at)) => {
                    let mut inbox = self.ivars().borrow_mut();
                    inbox.hover = None; // the drag is over; the cue goes away with it
                    inbox.label.clear();
                    inbox.accepted = false;
                    inbox.dropped = Some((at, text));
                    inbox.changed = true;
                    true
                }
                // Not text after all, or no window to convert against: decline,
                // and let AppKit look for another destination.
                _ => false,
            }
        }
    }
);

impl TextDropView {
    /// The dragged payload as plain text, if it has any. `public.utf8-plain-text`
    /// is what a browser puts on the pasteboard for a dragged selection (next to
    /// RTF and HTML flavours we deliberately ignore — a terminal takes text).
    fn dragged_text(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> Option<String> {
        let pb = sender.draggingPasteboard();
        let s = unsafe { pb.stringForType(NSPasteboardTypeString) }?;
        let s = s.to_string();
        (!s.is_empty()).then_some(s)
    }

    /// The drag's position in rt's coordinate space: physical pixels from the
    /// top-left of the content area. See [`DropInbox`].
    fn location(&self, sender: &ProtocolObject<dyn NSDraggingInfo>) -> Option<(f32, f32)> {
        let p = self.convertPoint_fromView(sender.draggingLocation(), None);
        // Points -> physical pixels. `backingScaleFactor` is the same number
        // winit reports as the window's scale factor, and `Active.chrome_sc` is
        // derived from that, so this lands on exactly the grid the pane rects
        // were laid out on.
        let sc = self.window()?.backingScaleFactor() as f32;
        Some(((p.x as f32) * sc, (p.y as f32) * sc))
    }

    /// Record a hover and answer AppKit's "would you take this?".
    ///
    /// Always `Copy` while the pasteboard carries text, even over rt's own
    /// chrome: the view has no layout to hit-test against, and the honest signal
    /// for "this will not land" is the ABSENCE of the drop cue, which the app
    /// paints from `textdrop::resolve` one turn of the loop later. Refusing here
    /// instead would need the window's geometry mirrored into AppKit and kept in
    /// sync, to change a cursor badge.
    ///
    /// `entered` says the caller has just claimed this drag and refreshed
    /// [`DropInbox::label`]; a motion update trusts [`DropInbox::accepted`]
    /// instead of re-reading the pasteboard, which is what keeps the cue's cost
    /// independent of the payload's size.
    fn note_hover(&self, sender: &ProtocolObject<dyn NSDraggingInfo>, entered: bool) -> NSDragOperation {
        let Some(at) = self.location(sender) else {
            return NSDragOperation::None;
        };
        let mut inbox = self.ivars().borrow_mut();
        if !entered && !inbox.accepted {
            return NSDragOperation::None; // a drag we already declined on entry
        }
        inbox.hover = Some(at);
        inbox.changed = true;
        NSDragOperation::Copy
    }
}

/// Install the text-drop destination on `window`, returning the inbox `Active`
/// should poll. `None` means the platform did not cooperate and rt simply has no
/// text drop on this window — never an error the caller has to handle, exactly
/// like `vibrancy::set_enabled` returning `false`.
///
/// Call once per window, at creation. The returned `Rc` is a second handle to
/// the same inbox the view holds; `addSubview:` owns the view, and the view goes
/// away with the content view when the window closes.
pub fn install(window: &dyn Window) -> Option<Rc<RefCell<DropInbox>>> {
    // NSView is main-thread-only; `MainThreadMarker::new` checks for real, so an
    // off-main-thread call degrades to "no text drop" rather than to UB.
    let Some(mtm) = MainThreadMarker::new() else {
        log::debug!("text drop: not on the main thread; skipping");
        return None;
    };
    let ns_view = match window.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::AppKit(h)) => h.ns_view,
        _ => {
            log::debug!("text drop: no AppKit window handle; skipping");
            return None;
        }
    };

    let inbox = Rc::new(RefCell::new(DropInbox::default()));

    // SAFETY: the handle comes straight from winit's live window and names a
    // valid NSView; we borrow it only for this call, on the main thread (proven
    // by `mtm`), which is where NSView lives.
    unsafe {
        let content: &NSView = ns_view.cast().as_ref();

        let this = mtm.alloc::<TextDropView>().set_ivars(Rc::clone(&inbox));
        let view: Retained<TextDropView> = msg_send![super(this), init];

        // Cover the content view, and keep covering it across every resize —
        // a drop must be catchable anywhere a pane can be drawn.
        view.setFrame(content.bounds());
        view.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        // On top of the content view's subviews (there are normally none:
        // wgpu's CAMetalLayer is a LAYER on winit's view, not a subview, and
        // vibrancy.rs's effect view is a sibling one level up in the frame
        // view). The view draws nothing and is click-through, so being topmost
        // costs the terminal neither a pixel nor an event.
        content.addSubview(&view);
        // Only the string type. A file drag carries none of it, so it still
        // reaches winit's own window-level destination untouched — the two
        // receivers cannot collide.
        //
        // AFTER `addSubview:`, deliberately: a view's dragged types are
        // propagated to the window it lives in, and registering while the view
        // is still window-less relies on AppKit re-propagating them on
        // `viewDidMoveToWindow`. Registering once the view is already in the
        // window needs no such assumption.
        view.registerForDraggedTypes(&NSArray::from_slice(&[NSPasteboardTypeString]));
    }

    log::info!("text drop: NSPasteboardTypeString destination installed over the content view");
    Some(inbox)
}
