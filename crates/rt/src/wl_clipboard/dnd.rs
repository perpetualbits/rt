//! The Wayland text-drop receiver's two halves that are **rt's**, not
//! smithay-clipboard's: the mailbox the worker thread fills, and the handle the
//! event loop reads it through.
//!
//! The wire itself — the `wl_data_device` drag callbacks — is in [`super::state`],
//! filling in stubs upstream left empty. Everything that DECIDES anything is in
//! [`crate::textdrop`], which Linux CI compiles and tests: this file only moves
//! two facts across a thread boundary.
//!
//! ## Why a mailbox and not a channel
//!
//! Exactly the shape [`crate::text_drop_mac`] uses, for the same reason: a drag
//! produces one event per pointer motion and the app only ever wants the LATEST.
//! A channel would queue a frame's worth of stale positions for the loop to
//! throw away; a mailbox coalesces them for free. `changed` then lets the loop
//! skip the work — and the re-derivation of the window's layout — on the
//! overwhelming majority of turns, when nothing is being dragged at all.
//!
//! ## Why the positions are logical here and physical everywhere else
//!
//! `wl_data_device.enter`/`motion` report surface-local (logical) coordinates.
//! The worker thread cannot convert them: the scale factor is winit's, changes
//! when the window moves between outputs, and is only knowable on the main
//! thread. So the inbox carries them as they arrived and
//! [`WaylandTextDrop::take_change`] applies `textdrop::surface_to_physical` with
//! the scale factor read in the same turn that hit-tests the result.

use std::sync::{Arc, Mutex};

use smithay_client_toolkit as sctk;

/// The chip that rides the cursor while a Wayland text drag hovers rt.
///
/// A fixed word, not a preview of the payload — the same choice the X11 receiver
/// makes, and for a near-identical reason. `wl_data_offer.receive` MAY be called
/// before the drop ("destination clients may preemptively fetch data"), but that
/// starts a speculative pipe transfer against an arbitrary source on every drag
/// enter, for a label. AppKit hands rt the pasteboard string for free, so the
/// macOS receiver previews; neither Linux protocol does, so neither Linux
/// receiver does. Keeping the two the same matters more than the preview.
const HOVER_LABEL: &str = "text";

/// What the worker thread has to say about a foreign text drag, read once per
/// turn of the event loop by `App::about_to_wait`.
///
/// Positions are **surface-local** — see the module doc.
#[derive(Default)]
pub struct DropInbox {
    /// Where a text drag is hovering right now, if one is over this window's
    /// surface. `None` once it leaves or drops.
    pub hover: Option<(f64, f64)>,
    /// Whether the drag currently over this window is one rt claimed on entry.
    /// A separate flag rather than "`hover` is set", so the loop can tell a
    /// declined drag (no cue, no chip) from one that simply has not moved yet.
    pub accepted: bool,
    /// A completed drop, waiting to be delivered: where, and what.
    pub dropped: Option<((f64, f64), String)>,
    /// Set by the worker on every change, cleared by the reader. Lets the loop
    /// skip the work on every turn where nothing is being dragged.
    pub changed: bool,
}

impl DropInbox {
    /// Wipe every trace of a drag: used by `leave`, by a completed drop, and by
    /// a transfer that ran out of time. Always marks `changed`, so the loop
    /// takes the cue off the screen even when the drag ended without a drop.
    pub fn clear(&mut self) {
        self.hover = None;
        self.accepted = false;
        self.changed = true;
    }
}

/// Where the worker should send drags, and how to wake the loop when it does.
/// Set once, by `Clipboard::attach_text_drop`, before any drag can arrive.
pub struct DndTarget {
    /// rt's own `wl_surface`. A `wl_data_device` is per SEAT, not per surface,
    /// so a client with two windows sees `enter` for whichever one the pointer
    /// is over on every window's device: without this comparison a drag over
    /// window 2 would light up a cue on window 1 as well, and both would answer
    /// the source.
    pub surface: sctk::reexports::client::backend::ObjectId,
    pub inbox: Arc<Mutex<DropInbox>>,
    pub wake: winit::event_loop::EventLoopProxy,
}

/// The slot [`super::Clipboard`] hands the worker at spawn time and fills in
/// later. Empty means "this clipboard has no drop target", which is the state
/// every build is in until a window asks for one — and the state it stays in if
/// the window's surface could not be resolved.
#[derive(Default)]
pub struct DndShared {
    pub target: Mutex<Option<Arc<DndTarget>>>,
}

/// A live text-drop receiver for one window. Inert (`WaylandTextDrop::none`) on
/// X11 and on any Wayland window whose surface rt could not adopt — never an
/// error the caller has to handle, exactly like the X11 and macOS receivers.
pub struct WaylandTextDrop {
    inbox: Option<Arc<Mutex<DropInbox>>>,
}

impl WaylandTextDrop {
    pub(super) fn new(inbox: Arc<Mutex<DropInbox>>) -> Self {
        WaylandTextDrop { inbox: Some(inbox) }
    }

    /// The receiver that never receives: X11, or a Wayland window whose surface
    /// could not be adopted onto the worker's connection.
    pub fn none() -> Self {
        WaylandTextDrop { inbox: None }
    }

    /// Whether a foreign text drag is over this window right now. The caller
    /// uses it to hold the event loop at its fast poll rate for the gesture, the
    /// same way the X11 receiver's `dragging()` does — the wake-ups already
    /// deliver each motion, but a drag that pauses still wants a live cue.
    pub fn dragging(&self) -> bool {
        self.inbox
            .as_ref()
            .and_then(|i| i.lock().ok().map(|i| i.accepted))
            .unwrap_or(false)
    }

    /// Take whatever changed since the last look, converted into the space the
    /// rest of the feature works in. `None` when nothing did, which is the state
    /// of affairs on essentially every turn — so the caller can skip re-deriving
    /// the window's layout.
    ///
    /// `scale` is the window's winit scale factor, read in the same turn that
    /// will hit-test the result; see the module doc for why the conversion
    /// cannot happen on the worker thread.
    pub fn take_change(&self, scale: f64) -> Option<crate::textdrop::DropNews> {
        let inbox = self.inbox.as_ref()?;
        let mut inbox = inbox.lock().ok()?;
        if !inbox.changed {
            return None;
        }
        inbox.changed = false;
        let at = |(x, y): (f64, f64)| crate::textdrop::surface_to_physical(x, y, scale);
        // A hover position that will not convert is dropped rather than guessed
        // at; the chip simply does not appear that frame, and the next motion
        // brings a fresh one.
        let hover = inbox
            .hover
            .filter(|_| inbox.accepted)
            .and_then(at)
            .map(|p| (p, HOVER_LABEL.to_string()));
        // A DROP that will not convert has nowhere to land, so it is discarded
        // outright rather than inserted at an invented point.
        let dropped = inbox.dropped.take().and_then(|(p, text)| at(p).map(|p| (p, text)));
        Some(crate::textdrop::DropNews { hover, dropped })
    }
}
