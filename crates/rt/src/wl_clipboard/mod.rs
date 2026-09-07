//! rt's Wayland CLIPBOARD, PRIMARY selection **and** drag-and-drop receiver —
//! all three on the **one** `wl_data_device` a client is allowed to have.
//!
//! ## Provenance
//!
//! `mod.rs`, `state.rs`, `worker.rs` and `mime.rs` began as a verbatim copy of
//! **smithay-clipboard 0.7.3** (MIT — see `LICENSE-smithay-clipboard`, kept
//! beside them, and `docs/wayland-text-drop.md` for the exact delta). rt used
//! that crate as a dependency until drag-and-drop needed a `wl_data_device`
//! of its own, and there is no safe way to have two.
//!
//! ## Why a second `wl_data_device` is not an option
//!
//! `wayland.xml` says "there is one wl_data_device per seat" but defines no
//! behaviour for a client that asks for two, so compositors differ — and one of
//! them differs destructively. Mutter's `get_data_device`
//! (`src/wayland/meta-wayland-data-device.c`) does:
//!
//! ```text
//!   data_device_resource = wl_resource_find_for_client (&seat->data_device.resource_list, client);
//!   if (data_device_resource) {
//!       wl_list_remove (wl_resource_get_link (data_device_resource));
//!       wl_list_init   (wl_resource_get_link (data_device_resource));
//!   }
//!   wl_list_insert (&seat->data_device.resource_list, wl_resource_get_link (cr));
//! ```
//!
//! The client's EXISTING device is unlinked into a self-loop: still alive, in no
//! list, so every later `wl_resource_for_each` skips it. It never receives
//! `selection` again. No protocol error, no log — **paste just stops working**.
//! Mutter's DnD side is single-resource too (`drag_focus_data_device`, one
//! pointer, found by `wl_resource_find_for_client`).
//!
//! Smithay (so cosmic-comp), KWin and wlroots all fan out to every device and
//! would have been fine — wlroots since the fix for swaywm/wlroots#1384. But
//! "fine on three of four, and silently breaks the clipboard on GNOME" is not a
//! trade rt can make: a clipboard regression is worse than no feature. So rt
//! owns the single device, exactly as GTK and Qt do, and the hazard cannot occur
//! by construction — there is no second device to route anything to.
//!
//! ## What rt added
//!
//! Upstream's `DataDeviceHandler::{enter,leave,motion,drop_performed}` and
//! `DataOfferHandler::{source_actions,selected_action}` were empty stubs: the
//! crate wanted the device only for selections. rt fills them in, and adds
//! [`DropInbox`] for the main thread to read. Every clipboard and PRIMARY code
//! path is untouched — the DnD callbacks are only ever reached from
//! `wl_data_device` DRAG events, which the selection paths never raise.
//!
//! ## Threading
//!
//! Unchanged from upstream: a worker thread with its own `calloop` loop and its
//! own `EventQueue` over winit's `wl_display` (`Backend::from_foreign_display`,
//! the same borrow `blur.rs` and `bg_effect.rs` take). rt's additions cross to
//! the main thread through an `Arc<Mutex<DropInbox>>` plus a
//! `winit::event_loop::EventLoopProxy` wake-up, so the drop cue tracks the
//! pointer at the event loop's own rate rather than waiting for its idle poll.

#![allow(clippy::all)] // upstream code, kept verbatim; rt lints its own modules
use smithay_client_toolkit as sctk;
use std::ffi::c_void;
use std::io::Result;
use std::sync::mpsc::{self, Receiver};

use sctk::reexports::calloop::channel::{self, Sender};
use sctk::reexports::client::Connection;
use sctk::reexports::client::backend::Backend;

mod mime;
mod state;
mod worker;

/// Access to a Wayland clipboard.
pub struct Clipboard {
    request_sender: Sender<worker::Command>,
    request_receiver: Receiver<Result<String>>,
    clipboard_thread: Option<std::thread::JoinHandle<()>>,
}

impl Clipboard {
    /// Creates new clipboard which will be running on its own thread with its
    /// own event queue to handle clipboard requests.
    ///
    /// # Safety
    ///
    /// `display` must be a valid `*mut wl_display` pointer, and it must remain
    /// valid for as long as `Clipboard` object is alive.
    pub unsafe fn new(display: *mut c_void) -> Self {
        let backend = unsafe { Backend::from_foreign_display(display.cast()) };
        let connection = Connection::from_backend(backend);

        // Create channel to send data to clipboard thread.
        let (request_sender, rx_chan) = channel::channel();
        // Create channel to get data from the clipboard thread.
        let (clipboard_reply_sender, request_receiver) = mpsc::channel();

        let name = String::from("smithay-clipboard");
        let clipboard_thread = worker::spawn(name, connection, rx_chan, clipboard_reply_sender);

        Self { request_receiver, request_sender, clipboard_thread }
    }

    /// Load clipboard data.
    ///
    /// Loads content from a clipboard on a last observed seat.
    pub fn load(&self) -> Result<String> {
        let _ = self.request_sender.send(worker::Command::Load);

        if let Ok(reply) = self.request_receiver.recv() {
            reply
        } else {
            // The clipboard thread is dead, however we shouldn't crash downstream, so
            // propogating an error.
            Err(std::io::Error::other("clipboard is dead."))
        }
    }

    /// Store to a clipboard.
    ///
    /// Stores to a clipboard on a last observed seat.
    pub fn store<T: Into<String>>(&self, text: T) {
        let request = worker::Command::Store(text.into());
        let _ = self.request_sender.send(request);
    }

    /// Load primary clipboard data.
    ///
    /// Loads content from a  primary clipboard on a last observed seat.
    pub fn load_primary(&self) -> Result<String> {
        let _ = self.request_sender.send(worker::Command::LoadPrimary);

        if let Ok(reply) = self.request_receiver.recv() {
            reply
        } else {
            // The clipboard thread is dead, however we shouldn't crash downstream, so
            // propogating an error.
            Err(std::io::Error::other("clipboard is dead."))
        }
    }

    /// Store to a primary clipboard.
    ///
    /// Stores to a primary clipboard on a last observed seat.
    pub fn store_primary<T: Into<String>>(&self, text: T) {
        let request = worker::Command::StorePrimary(text.into());
        let _ = self.request_sender.send(request);
    }
}

impl Drop for Clipboard {
    fn drop(&mut self) {
        // Shutdown smithay-clipboard.
        let _ = self.request_sender.send(worker::Command::Exit);
        if let Some(clipboard_thread) = self.clipboard_thread.take() {
            let _ = clipboard_thread.join();
        }
    }
}
