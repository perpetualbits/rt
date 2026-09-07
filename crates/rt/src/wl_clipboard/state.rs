use smithay_client_toolkit as sctk;
use std::borrow::Cow;
use std::collections::HashMap;
use std::io::{Error, ErrorKind, Read, Result, Write};
use std::mem;
use std::os::unix::io::{AsRawFd, RawFd};
use std::rc::Rc;
use std::sync::mpsc::Sender;

use sctk::data_device_manager::data_device::{DataDevice, DataDeviceHandler};
use sctk::data_device_manager::data_offer::{DataOfferError, DataOfferHandler, DragOffer};
use sctk::data_device_manager::data_source::{CopyPasteSource, DataSourceHandler};
use sctk::data_device_manager::{DataDeviceManagerState, WritePipe};
use sctk::primary_selection::PrimarySelectionManagerState;
use sctk::primary_selection::device::{PrimarySelectionDevice, PrimarySelectionDeviceHandler};
use sctk::primary_selection::selection::{PrimarySelectionSource, PrimarySelectionSourceHandler};
use sctk::registry::{ProvidesRegistryState, RegistryState};
use sctk::seat::pointer::{PointerData, PointerEvent, PointerEventKind, PointerHandler};
use sctk::seat::{Capability, SeatHandler, SeatState};
use sctk::{
    delegate_data_device, delegate_pointer, delegate_primary_selection, delegate_registry,
    delegate_seat, registry_handlers,
};

use sctk::reexports::calloop::{LoopHandle, PostAction};
use sctk::reexports::client::globals::GlobalList;
use sctk::reexports::client::protocol::wl_data_device::WlDataDevice;
use sctk::reexports::client::protocol::wl_data_device_manager::DndAction;
use sctk::reexports::client::protocol::wl_data_source::WlDataSource;
use sctk::reexports::client::protocol::wl_keyboard::WlKeyboard;
use sctk::reexports::client::protocol::wl_pointer::WlPointer;
use sctk::reexports::client::protocol::wl_seat::WlSeat;
use sctk::reexports::client::protocol::wl_surface::WlSurface;
use sctk::reexports::client::{Connection, Dispatch, Proxy, QueueHandle};
use sctk::reexports::protocols::wp::primary_selection::zv1::client::{
    zwp_primary_selection_device_v1::ZwpPrimarySelectionDeviceV1,
    zwp_primary_selection_source_v1::ZwpPrimarySelectionSourceV1,
};
use sctk::reexports::client::backend::ObjectId;

use super::mime::{ALLOWED_MIME_TYPES, MimeType, normalize_to_lf};

pub struct State {
    pub primary_selection_manager_state: Option<PrimarySelectionManagerState>,
    pub data_device_manager_state: Option<DataDeviceManagerState>,
    pub reply_tx: Sender<Result<String>>,
    pub exit: bool,

    registry_state: RegistryState,
    seat_state: SeatState,

    seats: HashMap<ObjectId, ClipboardSeatState>,
    /// The latest seat which got an event.
    latest_seat: Option<ObjectId>,

    loop_handle: LoopHandle<'static, Self>,
    queue_handle: QueueHandle<Self>,

    primary_sources: Vec<PrimarySelectionSource>,
    primary_selection_content: Rc<[u8]>,

    data_sources: Vec<CopyPasteSource>,
    data_selection_content: Rc<[u8]>,

    // --- rt's additions: dragged-in text on the same wl_data_device ---------
    /// Where drags should be reported, once a window has asked for them.
    dnd: std::sync::Arc<super::dnd::DndShared>,
    /// The flavour rt accepted for the drag currently over its surface, in the
    /// SOURCE's own spelling — `wl_data_offer.receive` is matched against it
    /// verbatim. `None` means no drag of ours is live.
    dnd_mime: Option<String>,
    /// Where the last `motion` (or `enter`) put the pointer, surface-local.
    /// `wl_data_device.drop` carries no position of its own, exactly as
    /// `XdndDrop` does not, so this is what the drop lands on.
    dnd_at: Option<(f64, f64)>,
    /// Bumped for every drop transfer, and captured by that transfer's pipe
    /// reader and its deadline timer. It is how each of the two knows whether it
    /// is still the current one: a reader that finishes after its deadline fired
    /// discards its bytes, and a deadline that fires after its reader finished
    /// does nothing.
    dnd_gen: u64,
}

impl State {
    #[must_use]
    pub fn new(
        globals: &GlobalList,
        queue_handle: &QueueHandle<Self>,
        loop_handle: LoopHandle<'static, Self>,
        reply_tx: Sender<Result<String>>,
        dnd: std::sync::Arc<super::dnd::DndShared>,
    ) -> Option<Self> {
        // NOTE: while it's mutable, it's not part of the hash compute.
        #[allow(clippy::mutable_key_type)]
        let mut seats = HashMap::new();

        let data_device_manager_state = DataDeviceManagerState::bind(globals, queue_handle).ok();
        let primary_selection_manager_state =
            PrimarySelectionManagerState::bind(globals, queue_handle).ok();

        // When both globals are not available nothing could be done.
        if data_device_manager_state.is_none() && primary_selection_manager_state.is_none() {
            return None;
        }

        let seat_state = SeatState::new(globals, queue_handle);
        for seat in seat_state.seats() {
            seats.insert(seat.id(), Default::default());
        }

        Some(Self {
            registry_state: RegistryState::new(globals),
            primary_selection_content: Rc::from([]),
            data_selection_content: Rc::from([]),
            queue_handle: queue_handle.clone(),
            primary_selection_manager_state,
            primary_sources: Vec::new(),
            data_device_manager_state,
            data_sources: Vec::new(),
            latest_seat: None,
            loop_handle,
            exit: false,
            seat_state,
            reply_tx,
            seats,
            dnd,
            dnd_mime: None,
            dnd_at: None,
            dnd_gen: 0,
        })
    }

    /// Store selection for the given target.
    ///
    /// Selection source is only created when `Some(())` is returned.
    pub fn store_selection(&mut self, ty: SelectionTarget, contents: String) -> Option<()> {
        let latest = self.latest_seat.as_ref()?;
        let seat = self.seats.get_mut(latest)?;

        if !seat.has_focus {
            return None;
        }

        let contents = Rc::from(contents.into_bytes());

        match ty {
            SelectionTarget::Clipboard => {
                let mgr = self.data_device_manager_state.as_ref()?;
                self.data_selection_content = contents;
                let source =
                    mgr.create_copy_paste_source(&self.queue_handle, ALLOWED_MIME_TYPES.iter());
                source.set_selection(seat.data_device.as_ref().unwrap(), seat.latest_serial);
                self.data_sources.push(source);
            },
            SelectionTarget::Primary => {
                let mgr = self.primary_selection_manager_state.as_ref()?;
                self.primary_selection_content = contents;
                let source =
                    mgr.create_selection_source(&self.queue_handle, ALLOWED_MIME_TYPES.iter());
                source.set_selection(seat.primary_device.as_ref().unwrap(), seat.latest_serial);
                self.primary_sources.push(source);
            },
        }

        Some(())
    }

    /// Load selection for the given target.
    pub fn load_selection(&mut self, ty: SelectionTarget) -> Result<()> {
        let latest = self
            .latest_seat
            .as_ref()
            .ok_or_else(|| Error::other("no events received on any seat"))?;
        let seat = self.seats.get_mut(latest).ok_or_else(|| Error::other("active seat lost"))?;

        if !seat.has_focus {
            return Err(Error::other("client doesn't have focus"));
        }

        let (read_pipe, mime_type) = match ty {
            SelectionTarget::Clipboard => {
                let selection = seat
                    .data_device
                    .as_ref()
                    .and_then(|data| data.data().selection_offer())
                    .ok_or_else(|| Error::other("selection is empty"))?;

                let mime_type =
                    selection.with_mime_types(MimeType::find_allowed).ok_or_else(|| {
                        Error::new(ErrorKind::NotFound, "supported mime-type is not found")
                    })?;

                (
                    selection.receive(mime_type.to_string()).map_err(|err| match err {
                        DataOfferError::InvalidReceive => Error::other("offer is not ready yet"),
                        DataOfferError::Io(err) => err,
                    })?,
                    mime_type,
                )
            },
            SelectionTarget::Primary => {
                let selection = seat
                    .primary_device
                    .as_ref()
                    .and_then(|data| data.data().selection_offer())
                    .ok_or_else(|| Error::other("selection is empty"))?;

                let mime_type =
                    selection.with_mime_types(MimeType::find_allowed).ok_or_else(|| {
                        Error::new(ErrorKind::NotFound, "supported mime-type is not found")
                    })?;

                (selection.receive(mime_type.to_string())?, mime_type)
            },
        };

        // Mark FD as non-blocking so we won't block ourselves.
        set_non_blocking(read_pipe.as_raw_fd())?;

        let mut reader_buffer = [0; 4096];
        let mut content = Vec::new();
        let _ = self.loop_handle.insert_source(read_pipe, move |_, file, state| {
            let file = unsafe { file.get_mut() };
            loop {
                match file.read(&mut reader_buffer) {
                    Ok(0) => {
                        let utf8 = String::from_utf8_lossy(&content);
                        let content = match utf8 {
                            Cow::Borrowed(_) => {
                                // Don't clone the read data.
                                let mut to_send = Vec::new();
                                mem::swap(&mut content, &mut to_send);
                                String::from_utf8(to_send).unwrap()
                            },
                            Cow::Owned(content) => content,
                        };

                        // Post-process the content according to mime type.
                        let content = match mime_type {
                            MimeType::TextPlainUtf8 | MimeType::TextPlain => {
                                normalize_to_lf(content)
                            },
                            MimeType::Utf8String => content,
                        };

                        let _ = state.reply_tx.send(Ok(content));
                        break PostAction::Remove;
                    },
                    Ok(n) => content.extend_from_slice(&reader_buffer[..n]),
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break PostAction::Continue,
                    Err(err) => {
                        let _ = state.reply_tx.send(Err(err));
                        break PostAction::Remove;
                    },
                };
            }
        });

        Ok(())
    }

    fn send_request(&mut self, ty: SelectionTarget, write_pipe: WritePipe, mime: String) {
        // We can only send strings, so don't do anything with the mime-type.
        if MimeType::find_allowed(&[mime]).is_none() {
            return;
        }

        // Mark FD as non-blocking so we won't block ourselves.
        if set_non_blocking(write_pipe.as_raw_fd()).is_err() {
            return;
        }

        // Don't access the content on the state directly, since it could change during
        // the send.
        let contents = match ty {
            SelectionTarget::Clipboard => self.data_selection_content.clone(),
            SelectionTarget::Primary => self.primary_selection_content.clone(),
        };

        let mut written = 0;
        let _ = self.loop_handle.insert_source(write_pipe, move |_, file, _| {
            let file = unsafe { file.get_mut() };
            loop {
                match file.write(&contents[written..]) {
                    Ok(n) if written + n == contents.len() => {
                        written += n;
                        break PostAction::Remove;
                    },
                    Ok(n) => written += n,
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break PostAction::Continue,
                    Err(_) => break PostAction::Remove,
                }
            }
        });
    }
}

impl SeatHandler for State {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }

    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        self.seats.insert(seat.id(), Default::default());
    }

    fn new_capability(
        &mut self,
        _: &Connection,
        qh: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        let seat_state = self.seats.get_mut(&seat.id()).unwrap();

        match capability {
            Capability::Keyboard => {
                seat_state.keyboard = Some(seat.get_keyboard(qh, seat.id()));

                // Selection sources are tied to the keyboard, so add/remove decives
                // when we gain/loss capability.

                if seat_state.data_device.is_none() && self.data_device_manager_state.is_some() {
                    seat_state.data_device = self
                        .data_device_manager_state
                        .as_ref()
                        .map(|mgr| mgr.get_data_device(qh, &seat));
                }

                if seat_state.primary_device.is_none()
                    && self.primary_selection_manager_state.is_some()
                {
                    seat_state.primary_device = self
                        .primary_selection_manager_state
                        .as_ref()
                        .map(|mgr| mgr.get_selection_device(qh, &seat));
                }
            },
            Capability::Pointer => {
                seat_state.pointer = self.seat_state.get_pointer(qh, &seat).ok();
            },
            _ => (),
        }
    }

    fn remove_capability(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        seat: WlSeat,
        capability: Capability,
    ) {
        let seat_state = self.seats.get_mut(&seat.id()).unwrap();
        match capability {
            Capability::Keyboard => {
                seat_state.data_device = None;
                seat_state.primary_device = None;

                if let Some(keyboard) = seat_state.keyboard.take() {
                    if keyboard.version() >= 3 {
                        keyboard.release()
                    }
                }
            },
            Capability::Pointer => {
                if let Some(pointer) = seat_state.pointer.take() {
                    if pointer.version() >= 3 {
                        pointer.release()
                    }
                }
            },
            _ => (),
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, seat: WlSeat) {
        self.seats.remove(&seat.id());
    }
}

impl PointerHandler for State {
    fn pointer_frame(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        pointer: &WlPointer,
        events: &[PointerEvent],
    ) {
        let seat = pointer.data::<PointerData>().unwrap().seat();
        let seat_id = seat.id();
        let seat_state = match self.seats.get_mut(&seat_id) {
            Some(seat_state) => seat_state,
            None => return,
        };

        let mut updated_serial = false;
        for event in events {
            match event.kind {
                PointerEventKind::Press { serial, .. }
                | PointerEventKind::Release { serial, .. } => {
                    updated_serial = true;
                    seat_state.latest_serial = serial;
                },
                _ => (),
            }
        }

        // Only update the seat we're using when the serial got updated.
        if updated_serial {
            self.latest_seat = Some(seat_id);
        }
    }
}

// ===========================================================================
// rt's addition: dragged-in text, on this same wl_data_device.
//
// Upstream left all four of these empty -- the crate wanted the device only for
// selections. Note what is NOT here: nothing below touches
// `data_selection_content`, `primary_selection_content`, `data_sources`,
// `primary_sources`, `latest_seat` or any `ClipboardSeatState`. The drag
// callbacks are reached only from `wl_data_device`'s DRAG events, which no
// clipboard or PRIMARY path ever raises, and the drag state they do touch is
// four fields nothing else reads. That is the whole argument for why filling
// them in cannot regress copy, paste or middle-click paste.
// ===========================================================================

/// How long a drop's pipe transfer may take before rt gives up on it, matching
/// the X11 receiver's `TRANSFER_DEADLINE`. The guard is against a source that
/// accepts the drop and then neither writes nor closes: without it, `dragging()`
/// would stay true for the rest of the session and hold the event loop at its
/// fast poll rate forever. A source that merely DIES needs no deadline — its end
/// of the pipe closes, the reader sees EOF, and the drop resolves as empty.
const TRANSFER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

/// The only drag action rt ever advertises.
///
/// **Copy, never Move.** `Move` tells the source that rt has taken ownership and
/// it should delete what was dragged — which for a text editor dragging text out
/// of a document means deleting it there. A terminal inserts a copy and destroys
/// nothing, so offering `Move` would be a lie with a destructive consequence.
///
/// And never `Ask`, which means "put a menu up and tell me which"; rt has no
/// such menu, so the answer would have nowhere to come from.
///
/// A source offering only `Move` therefore intersects with rt to nothing, and
/// the compositor settles on `none`. `drop_performed` treats that as a cancel —
/// the protocol says a destination may only `receive` when the action came out
/// `copy` or `move`, and does NOT promise the `drop` event is withheld.
const RT_DND_ACTION: DndAction = DndAction::Copy;

impl State {
    /// The drop target, if a window has asked for one AND `surface` is that
    /// window's own. See `dnd::DndTarget::surface` for why the comparison
    /// matters: one data device serves the seat, not a surface.
    fn dnd_target_for(&self, surface: &WlSurface) -> Option<std::sync::Arc<super::dnd::DndTarget>> {
        let guard = self.dnd.target.lock().ok()?;
        let target = guard.as_ref()?;
        (target.surface == surface.id()).then(|| std::sync::Arc::clone(target))
    }

    /// The drop target for an event that carries no surface (`leave`, `motion`,
    /// `drop`). Those only ever follow an `enter` rt claimed, which is what
    /// `dnd_mime` records — so the surface has already been checked.
    fn dnd_target_live(&self) -> Option<std::sync::Arc<super::dnd::DndTarget>> {
        self.dnd_mime.as_ref()?;
        let guard = self.dnd.target.lock().ok()?;
        guard.as_ref().map(std::sync::Arc::clone)
    }

    /// The drag offer currently on this device, if any.
    fn drag_offer(device: &WlDataDevice) -> Option<DragOffer> {
        device.data::<sctk::data_device_manager::data_device::DataDeviceData>()?.drag_offer()
    }

    /// Note where the pointer is and wake the event loop to repaint the cue.
    fn dnd_moved(&mut self, target: &super::dnd::DndTarget, x: f64, y: f64) {
        self.dnd_at = Some((x, y));
        if let Ok(mut inbox) = target.inbox.lock() {
            inbox.hover = Some((x, y));
            inbox.accepted = true;
            inbox.changed = true;
        }
        target.wake.wake_up();
    }

    /// Forget the current drag and take the cue off the screen.
    fn dnd_end(&mut self) {
        self.dnd_mime = None;
        self.dnd_at = None;
        if let Some(target) =
            self.dnd.target.lock().ok().and_then(|g| g.as_ref().map(std::sync::Arc::clone))
        {
            if let Ok(mut inbox) = target.inbox.lock() {
                inbox.clear();
            }
            target.wake.wake_up();
        }
    }
}

impl DataDeviceHandler for State {
    /// A drag has entered one of this client's surfaces. This is the one place
    /// the accept/reject decision is made, and it is made on the offered MIME
    /// types alone — never on where in the window the pointer is.
    ///
    /// That is deliberate, and it matches both other receivers: the worker
    /// thread has no copy of the window's layout, and mirroring it here to
    /// change a cursor badge would mean keeping two copies in sync across every
    /// split, resize and tab switch. The honest signal for "this will not land"
    /// is the ABSENCE of the drop cue, which the app paints one turn later from
    /// `textdrop::resolve`. A drop over rt's own chrome is then discarded there.
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        device: &WlDataDevice,
        x: f64,
        y: f64,
        surface: &WlSurface,
    ) {
        self.dnd_mime = None;
        self.dnd_at = None;
        // Not our window (or no window is watching): stay completely out of it.
        // Answering here would fight the sibling window's device for the same
        // source, and refusing would refuse on its behalf.
        let Some(target) = self.dnd_target_for(surface) else { return };
        let Some(offer) = Self::drag_offer(device) else { return };

        let mime = offer.with_mime_types(|m| {
            crate::textdrop::pick_drop_mime(m).map(str::to_string)
        });
        // `accept(None)` is a real refusal, and worth making even though rt does
        // not depend on it: it is what puts a no-drop cursor under the user's
        // hand, and in practice compositors stop short of sending `drop` after
        // one. The protocol does not actually promise that, which is why the
        // refusal below is a state change too — with no `dnd_mime` recorded,
        // `drop_performed` finds no live drag and declines a drop that arrives
        // anyway.
        offer.accept_mime_type(offer.serial, mime.clone());
        let Some(mime) = mime else {
            log::debug!("wayland drag carries no text rt can read; declined");
            return;
        };
        offer.set_actions(RT_DND_ACTION, RT_DND_ACTION);
        self.dnd_mime = Some(mime);
        self.dnd_moved(&target, x, y);
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {
        // Only ours to clear: a `leave` for a drag we never claimed (another
        // window's, or one with no text) must not wipe a cue we are not showing.
        if self.dnd_mime.is_some() {
            self.dnd_end();
        }
    }

    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, x: f64, y: f64) {
        let Some(target) = self.dnd_target_live() else { return };
        self.dnd_moved(&target, x, y);
    }

    /// The user let go. Ask for the bytes; the answer arrives on a pipe, which
    /// is read on this thread's own event loop so nothing blocks.
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, device: &WlDataDevice) {
        let Some(target) = self.dnd_target_live() else { return };
        let (Some(mime), Some(at)) = (self.dnd_mime.take(), self.dnd_at.take()) else {
            self.dnd_end();
            return;
        };
        let Some(offer) = Self::drag_offer(device) else {
            self.dnd_end();
            return;
        };
        // What the compositor settled on. `wl_data_offer.receive` is only
        // sanctioned once that is `copy` or `move`; `none` means the source and
        // rt found no common action, and `ask` means the destination is supposed
        // to open a menu, which rt has none of — both are a cancel. Version 2
        // offers have no action negotiation at all, so there is nothing to check
        // and the drop proceeds (it just never gets a `finish`).
        let version = offer.inner().version();
        let action = offer.selected_action.bits();
        if version >= 3 && action & (crate::textdrop::DND_COPY | crate::textdrop::DND_MOVE) == 0 {
            log::debug!("wayland drop: compositor selected no usable action; cancelled");
            offer.destroy();
            self.dnd_end();
            return;
        }
        let read_pipe = match offer.receive(mime) {
            Ok(pipe) => pipe,
            Err(err) => {
                log::debug!("wayland drop: source would not open a pipe ({err}); discarded");
                offer.destroy();
                self.dnd_end();
                return;
            }
        };
        if set_non_blocking(read_pipe.as_raw_fd()).is_err() {
            offer.destroy();
            self.dnd_end();
            return;
        }

        self.dnd_gen = self.dnd_gen.wrapping_add(1);
        let generation = self.dnd_gen;
        let mut buffer = [0u8; 4096];
        let mut content: Vec<u8> = Vec::new();
        // Whether it will be legal to say `finish` when the bytes stop coming,
        // decided HERE rather than then. By the time the compositor sends `drop`
        // it has already settled the action (both sides called `set_actions`
        // during the drag), so this is the answer; re-reading the device later
        // could just as easily find a DIFFERENT drag's offer. Getting it wrong in
        // the permissive direction is the `invalid_finish` protocol error, which
        // tears down rt's whole `wl_display` and every pane with it.
        let finishable = Self::drag_offer_finishable(&offer);
        let finish_offer = offer.clone();
        let done_target = std::sync::Arc::clone(&target);
        let read = self.loop_handle.insert_source(read_pipe, move |_, file, state| {
            let file = unsafe { file.get_mut() };
            loop {
                match file.read(&mut buffer) {
                    Ok(0) => {
                        // The deadline below may already have written this drag
                        // off; if so these bytes are stale and go nowhere.
                        if state.dnd_gen == generation {
                            let text = String::from_utf8_lossy(&content).into_owned();
                            if let Ok(mut inbox) = done_target.inbox.lock() {
                                inbox.clear();
                                if !text.is_empty() {
                                    inbox.dropped = Some((at, text));
                                }
                            }
                            done_target.wake.wake_up();
                            state.dnd_gen = state.dnd_gen.wrapping_add(1);
                        }
                        // Tell the source the operation completed, then let the
                        // offer go -- both unconditionally on whether the bytes
                        // were still wanted, because the source is waiting
                        // either way. (Should a newer drag have made sctk
                        // destroy this offer meanwhile, both are no-ops:
                        // wayland-client drops a request on a dead proxy rather
                        // than putting it on the wire.)
                        if finishable {
                            finish_offer.finish();
                        }
                        finish_offer.destroy();
                        break PostAction::Remove;
                    }
                    Ok(n) => content.extend_from_slice(&buffer[..n]),
                    Err(err) if err.kind() == ErrorKind::WouldBlock => break PostAction::Continue,
                    Err(err) => {
                        log::debug!("wayland drop: read failed ({err}); discarded");
                        if state.dnd_gen == generation {
                            if let Ok(mut inbox) = done_target.inbox.lock() {
                                inbox.clear();
                            }
                            done_target.wake.wake_up();
                            state.dnd_gen = state.dnd_gen.wrapping_add(1);
                        }
                        finish_offer.destroy();
                        break PostAction::Remove;
                    }
                }
            }
        });

        // A loop that would not take the pipe leaves nothing to wait for: end
        // the gesture now rather than showing a cue until the deadline.
        if read.is_err() {
            log::debug!("wayland drop: could not watch the transfer pipe; discarded");
            offer.destroy();
            self.dnd_end();
            return;
        }

        // The deadline. Fires once; does nothing unless its transfer is still
        // the current one and still unfinished.
        let timer = sctk::reexports::calloop::timer::Timer::from_duration(TRANSFER_DEADLINE);
        let _ = self.loop_handle.insert_source(timer, move |_, _, state| {
            if state.dnd_gen == generation {
                log::debug!("wayland drop: no data within the deadline; abandoned");
                if let Ok(mut inbox) = target.inbox.lock() {
                    inbox.clear();
                }
                target.wake.wake_up();
                state.dnd_gen = state.dnd_gen.wrapping_add(1);
            }
            sctk::reexports::calloop::timer::TimeoutAction::Drop
        });
    }

    // The selection is finished and ready to be used.
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl State {
    /// Whether `wl_data_offer.finish` is legal on this offer, read straight off
    /// the offer's own reported state. The RULE is
    /// [`crate::textdrop::may_finish_drop`], where Linux CI can test it; this is
    /// only the three values it needs.
    fn drag_offer_finishable(offer: &DragOffer) -> bool {
        crate::textdrop::may_finish_drop(
            offer.inner().version(),
            offer.dropped,
            offer.selected_action.bits(),
        )
    }
}

/// `textdrop` spells the `dnd_action` bits out as plain integers so the rule
/// above can be tested without a compositor to mint a `DndAction`. This is the
/// join: if wayland-client's generated bitflags ever stop agreeing with them,
/// the build stops here rather than the guard quietly answering yes to a `none`
/// action and taking rt's `wl_display` down with an `invalid_finish`.
const _: () = {
    assert!(DndAction::Copy.bits() == crate::textdrop::DND_COPY);
    assert!(DndAction::Move.bits() == crate::textdrop::DND_MOVE);
    assert!(DndAction::Ask.bits() == crate::textdrop::DND_ASK);
    assert!(DndAction::empty().bits() == 0);
};

impl DataSourceHandler for State {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        mime: String,
        write_pipe: WritePipe,
    ) {
        self.send_request(SelectionTarget::Clipboard, write_pipe, mime)
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, deleted: &WlDataSource) {
        self.data_sources.retain(|source| source.inner() != deleted)
    }

    fn accept_mime(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &WlDataSource,
        _: Option<String>,
    ) {
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}

    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}

    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
}

impl DataOfferHandler for State {
    /// The source has told us which actions it supports. rt's answer never
    /// changes — [`RT_DND_ACTION`], and only that — but it has to be re-sent
    /// here: the compositor recomputes the selected action from the last
    /// `set_actions` each time the source's own offer changes, and a source that
    /// narrows its actions mid-drag would otherwise leave rt with none, and the
    /// drop with nothing to `finish` on.
    ///
    /// sctk has already recorded `actions` on the offer by the time this runs,
    /// so there is nothing to store; the read at drop time takes the offer's
    /// current state.
    fn source_actions(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        offer: &mut DragOffer,
        _actions: DndAction,
    ) {
        if self.dnd_mime.is_some() {
            offer.set_actions(RT_DND_ACTION, RT_DND_ACTION);
        }
    }

    /// The compositor has settled on an action. Nothing to do: rt asked for
    /// Copy and behaves identically whatever comes back, and the only place the
    /// answer matters — whether `wl_data_offer.finish` is legal — reads it off
    /// the offer at that moment rather than from a copy kept here.
    fn selected_action(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &mut DragOffer,
        _: DndAction,
    ) {
    }
}

impl ProvidesRegistryState for State {
    registry_handlers![SeatState];

    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }
}

impl PrimarySelectionDeviceHandler for State {
    fn selection(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionDeviceV1,
    ) {
    }
}

impl PrimarySelectionSourceHandler for State {
    fn send_request(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &ZwpPrimarySelectionSourceV1,
        mime: String,
        write_pipe: WritePipe,
    ) {
        self.send_request(SelectionTarget::Primary, write_pipe, mime);
    }

    fn cancelled(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        deleted: &ZwpPrimarySelectionSourceV1,
    ) {
        self.primary_sources.retain(|source| source.inner() != deleted)
    }
}

impl Dispatch<WlKeyboard, ObjectId, State> for State {
    fn event(
        state: &mut State,
        _: &WlKeyboard,
        event: <WlKeyboard as sctk::reexports::client::Proxy>::Event,
        data: &ObjectId,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        use sctk::reexports::client::protocol::wl_keyboard::Event as WlKeyboardEvent;
        let seat_state = match state.seats.get_mut(data) {
            Some(seat_state) => seat_state,
            None => return,
        };
        match event {
            WlKeyboardEvent::Key { serial, .. } | WlKeyboardEvent::Modifiers { serial, .. } => {
                seat_state.latest_serial = serial;
                state.latest_seat = Some(data.clone());
            },
            // NOTE both selections rely on keyboard focus.
            WlKeyboardEvent::Enter { serial, .. } => {
                seat_state.latest_serial = serial;
                seat_state.has_focus = true;
            },
            WlKeyboardEvent::Leave { .. } => {
                seat_state.latest_serial = 0;
                seat_state.has_focus = false;
            },
            _ => (),
        }
    }
}

delegate_seat!(State);
delegate_pointer!(State);
delegate_data_device!(State);
delegate_primary_selection!(State);
delegate_registry!(State);

#[derive(Debug, Clone, Copy)]
pub enum SelectionTarget {
    /// The target is clipboard selection.
    Clipboard,
    /// The target is primary selection.
    Primary,
}

#[derive(Debug, Default)]
struct ClipboardSeatState {
    keyboard: Option<WlKeyboard>,
    pointer: Option<WlPointer>,
    data_device: Option<DataDevice>,
    primary_device: Option<PrimarySelectionDevice>,
    has_focus: bool,

    /// The latest serial used to set the selection content.
    latest_serial: u32,
}

impl Drop for ClipboardSeatState {
    fn drop(&mut self) {
        if let Some(keyboard) = self.keyboard.take() {
            if keyboard.version() >= 3 {
                keyboard.release();
            }
        }

        if let Some(pointer) = self.pointer.take() {
            if pointer.version() >= 3 {
                pointer.release();
            }
        }
    }
}

fn set_non_blocking(raw_fd: RawFd) -> std::io::Result<()> {
    let flags = unsafe { libc::fcntl(raw_fd, libc::F_GETFL) };

    if flags < 0 {
        return Err(std::io::Error::last_os_error());
    }

    let result = unsafe { libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if result < 0 {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}
