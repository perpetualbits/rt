//! X11 receiver for text dragged in from another application, via XDND.
//!
//! Sibling of [`crate::text_drop_mac`] (AppKit) and of [`crate::x11_blur`],
//! whose shape it copies exactly: a real implementation under the `x11` feature,
//! an inert zero-sized stub otherwise, so `Active` can hold it with no `cfg` at
//! the field. Every failure path is a quiet `None`; nothing here can panic.
//!
//! ## Why winit cannot do this
//!
//! winit's X11 backend DOES speak XDND — and only for files. `winit-x11`'s
//! `dnd.rs` hardcodes the `text/uri-list` atom in both `convert_selection` and
//! `read_data`, and `WindowEvent::DragEntered { paths, .. }` carries
//! `Vec<PathBuf>`. There is no way to ask it for another target.
//!
//! ## Why rt cannot just listen in
//!
//! XDND is carried by `ClientMessage`s sent with `XSendEvent(.., mask = 0)`.
//! With a zero mask the server delivers the event to **the client that created
//! the window** and to nobody else, so a second X connection (rt already opens
//! one for `x11_blur`) cannot see messages addressed to winit's toplevel. And
//! the messages only ever go to a TOPLEVEL: the XDND spec says "XdndAware must
//! be placed on the top-level window", so putting an `XdndAware` child window
//! under the pointer would not be found either.
//!
//! ## XdndProxy — the mechanism the protocol provides for exactly this
//!
//! XDND version 4 added the `XdndProxy` property: a toplevel may name another
//! window that "should be checked for XdndAware and that should receive all the
//! client messages". rt creates a 1×1 unmapped window on **its own** connection,
//! marks it `XdndAware`, points the toplevel's `XdndProxy` at it, and points the
//! proxy's own `XdndProxy` at itself (the spec requires that self-reference, as
//! the guard against a stale id left behind by a crashed client). Sources then
//! send every XDND message to the proxy, which is ours, on our connection.
//!
//! All three sources that matter implement it: GTK reads and validates
//! `XdndProxy` in its X11 drag backend (so Firefox and every GTK app do), Qt
//! sends drag events to the proxy when one exists, and Chromium's
//! `X11DragDropClient::FindWindowFor` reads `XdndProxy` and then sends the
//! messages there — with the `window` field still naming the real target, which
//! is why every reply below quotes the TOPLEVEL id, never the proxy's.
//!
//! ## What this takes over
//!
//! Sources now send rt's XDND to the proxy instead of to winit's toplevel, so
//! winit's own file-drop path goes quiet on X11. Nothing is lost: rt has never
//! handled `WindowEvent::DragEntered`/`DragDropped` at all. It is the one
//! behavioural cost of the mechanism and it is stated here so a future file-drop
//! feature knows where it has to be implemented.
//!
//! ## Timing
//!
//! Our connection's fd is not in winit's poll set, so XDND events are picked up
//! by [`X11TextDrop::pump`] on rt's own event-loop turn. Idle, that is
//! `IDLE_POLL` (100 ms) — so the first `XdndEnter` can be that late — after which
//! the caller holds the loop at `ACTIVE_POLL` for the rest of the gesture (see
//! the `active_until` bump at the call site). Only the first frame of the cue
//! pays.

use winit::window::Window;

/// A handle that receives dragged-in text on X11. Inert when built without the
/// `x11` feature, or when the window is not an X11 window (native Wayland).
pub struct X11TextDrop {
    #[cfg(feature = "x11")]
    state: Option<imp::State>,
}

impl X11TextDrop {
    /// Install the XDND proxy for `window`. Degrades to an inert handle on
    /// anything that is not an X11 window, or if any setup step fails.
    pub fn try_init(window: &dyn Window) -> Self {
        #[cfg(feature = "x11")]
        {
            return X11TextDrop { state: imp::State::new(window) };
        }
        #[cfg(not(feature = "x11"))]
        {
            let _ = window;
            X11TextDrop {}
        }
    }

    /// Whether a foreign text drag is in progress over this window right now.
    /// The caller uses it to keep the event loop awake for the gesture.
    pub fn dragging(&self) -> bool {
        #[cfg(feature = "x11")]
        {
            return self.state.as_ref().is_some_and(|s| s.dragging());
        }
        #[cfg(not(feature = "x11"))]
        false
    }

    /// Drain this turn's XDND traffic. Returns `None` when nothing happened —
    /// which is every turn but the ones inside a drag.
    pub fn pump(&mut self) -> Option<crate::textdrop::DropNews> {
        #[cfg(feature = "x11")]
        {
            return self.state.as_mut().and_then(imp::State::pump);
        }
        #[cfg(not(feature = "x11"))]
        None
    }
}

#[cfg(feature = "x11")]
mod imp {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;
    use x11rb::connection::Connection;
    use x11rb::protocol::Event;
    use x11rb::protocol::xproto::{
        AtomEnum, ClientMessageEvent, ConnectionExt, CreateWindowAux, EventMask, PropMode,
        SelectionNotifyEvent, Window as XWindow, WindowClass,
    };
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _; // change_property32

    use crate::textdrop::DropNews;

    /// The XDND protocol version rt speaks. 5 has been current since 2002, and
    /// is what winit advertises on the toplevel too.
    const XDND_VERSION: u32 = 5;

    /// Text targets rt will take, best first. The two `text/plain` spellings of
    /// the charset both occur in the wild; `UTF8_STRING` is what older X clients
    /// offer; `STRING`/`TEXT` are Latin-1/undefined and come last, as a floor.
    const WANTED: [&[u8]; 6] = [
        b"text/plain;charset=utf-8",
        b"text/plain;charset=UTF-8",
        b"UTF8_STRING",
        b"text/plain",
        b"STRING",
        b"TEXT",
    ];

    /// Interned atoms, all resolved once at setup.
    struct Atoms {
        aware: u32,
        proxy: u32,
        enter: u32,
        position: u32,
        status: u32,
        leave: u32,
        drop_: u32,
        finished: u32,
        selection: u32,
        type_list: u32,
        action_copy: u32,
        incr: u32,
        wanted: [u32; 6],
    }

    /// A drag currently over the window: who is dragging, which target we agreed
    /// to take, and which XDND version they speak.
    struct Drag {
        source: XWindow,
        target: Option<u32>, // the atom we accepted, `None` = nothing we can use
        version: u32,
    }

    pub struct State {
        conn: RustConnection,
        root: XWindow,
        toplevel: XWindow, // winit's window: what every reply must name
        proxy: XWindow,    // ours: what the source actually sends to
        atoms: Atoms,
        drag: Option<Drag>,
        /// Where the last `XdndPosition` put the pointer, in the toplevel's
        /// coordinates. `XdndDrop` carries no position of its own, so this is
        /// what the drop lands on.
        last_pos: Option<(f32, f32)>,
        /// A drop whose selection transfer is still in flight, and when it was
        /// asked for. The deadline is the guard against a source that dies (or
        /// simply never answers) between `XdndDrop` and `SelectionNotify`:
        /// without it `dragging()` would stay true for the rest of the session
        /// and hold the event loop at its fast poll rate forever.
        awaiting_drop: Option<((f32, f32), std::time::Instant)>,
    }

    /// How long a selection transfer may take before rt gives up on it.
    const TRANSFER_DEADLINE: std::time::Duration = std::time::Duration::from_secs(3);

    /// The drag ghost's label on X11. Unlike AppKit, XDND does not let a
    /// destination look at the payload before the drop — the selection may only
    /// be converted once `XdndDrop` has arrived — so the chip names the kind of
    /// thing being dropped rather than previewing it.
    const HOVER_LABEL: &str = "text";

    impl State {
        pub fn new(window: &dyn Window) -> Option<Self> {
            let toplevel = match window.window_handle().ok()?.as_raw() {
                RawWindowHandle::Xlib(h) => h.window as XWindow,
                RawWindowHandle::Xcb(h) => h.window.get(),
                _ => return None, // native Wayland: not ours
            };
            let (conn, screen_num) = x11rb::connect(None).ok()?; // honours $DISPLAY
            let root = conn.setup().roots.get(screen_num)?.root;

            let intern = |name: &[u8]| -> Option<u32> {
                Some(conn.intern_atom(false, name).ok()?.reply().ok()?.atom)
            };
            let mut wanted = [0u32; 6];
            for (slot, name) in wanted.iter_mut().zip(WANTED) {
                *slot = intern(name)?;
            }
            let atoms = Atoms {
                aware: intern(b"XdndAware")?,
                proxy: intern(b"XdndProxy")?,
                enter: intern(b"XdndEnter")?,
                position: intern(b"XdndPosition")?,
                status: intern(b"XdndStatus")?,
                leave: intern(b"XdndLeave")?,
                drop_: intern(b"XdndDrop")?,
                finished: intern(b"XdndFinished")?,
                selection: intern(b"XdndSelection")?,
                type_list: intern(b"XdndTypeList")?,
                action_copy: intern(b"XdndActionCopy")?,
                incr: intern(b"INCR")?,
                wanted,
            };

            // The proxy: 1x1, never mapped, override-redirect so no window
            // manager ever looks at it. It exists only to be an address.
            // PROPERTY_CHANGE is selected because the selection transfer writes
            // its answer onto this window as a property.
            let proxy = conn.generate_id().ok()?;
            let aux = CreateWindowAux::new()
                .override_redirect(1)
                .event_mask(EventMask::PROPERTY_CHANGE);
            conn.create_window(
                x11rb::COPY_DEPTH_FROM_PARENT,
                proxy,
                root,
                -1,
                -1,
                1,
                1,
                0,
                WindowClass::INPUT_OUTPUT,
                x11rb::COPY_FROM_PARENT,
                &aux,
            )
            .ok()?
            .check()
            .ok()?;

            // The proxy must advertise XDND itself, and must point XdndProxy at
            // ITSELF: that self-reference is how a source tells a live proxy
            // from an id left behind by a crashed client (XDND spec, "Dropping
            // on the root window" / XdndProxy).
            let set = |win: XWindow, atom: u32, ty: AtomEnum, val: &[u32]| {
                conn.change_property32(PropMode::REPLACE, win, atom, ty, val).map(|c| c.ignore_error()).ok()
            };
            set(proxy, atoms.aware, AtomEnum::ATOM, &[XDND_VERSION])?;
            set(proxy, atoms.proxy, AtomEnum::WINDOW, &[proxy])?;
            // …and the toplevel hands its XDND over to it. winit's own
            // XdndAware stays where it is; a source checks XdndAware on the
            // PROXY once XdndProxy names one.
            set(toplevel, atoms.proxy, AtomEnum::WINDOW, &[proxy])?;
            conn.flush().ok()?;

            log::info!("XDND text drop: proxy window {proxy:#x} installed for toplevel {toplevel:#x}");
            Some(State {
                conn,
                root,
                toplevel,
                proxy,
                atoms,
                drag: None,
                last_pos: None,
                awaiting_drop: None,
            })
        }

        pub fn dragging(&self) -> bool {
            self.drag.is_some() || self.awaiting_drop.is_some()
        }

        /// Abandon a selection transfer the source never completed. Returns
        /// whether it just gave up, so the caller reports the (now empty) state
        /// and the cue is taken off the screen with it.
        fn expire(&mut self) -> bool {
            let stale = self
                .awaiting_drop
                .is_some_and(|(_, asked)| asked.elapsed() > TRANSFER_DEADLINE);
            if stale {
                log::debug!("XDND drop: no selection answer within the deadline; abandoned");
                self.awaiting_drop = None;
                self.last_pos = None;
                self.drag = None;
            }
            stale
        }

        /// Handle everything the server has for us, and report what the app
        /// should do about it.
        pub fn pump(&mut self) -> Option<DropNews> {
            let mut news = DropNews::default();
            let mut saw = false;
            while let Ok(Some(event)) = self.conn.poll_for_event() {
                saw |= self.handle(event, &mut news);
            }
            saw |= self.expire();
            let _ = self.conn.flush();
            (saw || !news.is_empty()).then_some(news)
        }

        /// One event. Returns whether it was XDND traffic we acted on (so a turn
        /// that only saw, say, an `XdndLeave` still repaints the cue away).
        fn handle(&mut self, event: Event, news: &mut DropNews) -> bool {
            match event {
                Event::ClientMessage(ev) => {
                    let data = ev.data.as_data32();
                    if ev.type_ == self.atoms.enter {
                        self.on_enter(data);
                        true
                    } else if ev.type_ == self.atoms.position {
                        self.on_position(data, news);
                        true
                    } else if ev.type_ == self.atoms.leave {
                        self.drag = None;
                        self.last_pos = None;
                        self.awaiting_drop = None;
                        news.hover = None;
                        true
                    } else if ev.type_ == self.atoms.drop_ {
                        self.on_drop(data);
                        true
                    } else {
                        false
                    }
                }
                Event::SelectionNotify(ev) => {
                    self.on_selection(ev, news);
                    true
                }
                _ => false,
            }
        }

        /// `XdndEnter`: the source names up to three targets inline, or sets bit
        /// 0 of `data[1]` to say "read my XdndTypeList". Pick the best text
        /// target we understand; `None` means we will decline every position.
        fn on_enter(&mut self, data: [u32; 5]) {
            let source = data[0];
            let version = data[1] >> 24;
            let offered: Vec<u32> = if data[1] & 1 != 0 {
                self.type_list(source)
            } else {
                data[2..5].iter().copied().filter(|a| *a != 0).collect()
            };
            let target = self
                .atoms
                .wanted
                .iter()
                .find(|a| offered.contains(a))
                .copied();
            if target.is_none() {
                log::debug!("XDND enter from {source:#x}: no text target among {} offered", offered.len());
            }
            self.drag = Some(Drag { source, target, version });
        }

        /// The source's full target list, when it offered more than three.
        fn type_list(&self, source: XWindow) -> Vec<u32> {
            let Ok(cookie) =
                self.conn.get_property(false, source, self.atoms.type_list, AtomEnum::ATOM, 0, 1024)
            else {
                return Vec::new();
            };
            cookie
                .reply()
                .ok()
                .and_then(|r| r.value32().map(Iterator::collect))
                .unwrap_or_default()
        }

        /// `XdndPosition`: the pointer moved. Report where, in the toplevel's
        /// own coordinates, and answer with an `XdndStatus`.
        fn on_position(&mut self, data: [u32; 5], news: &mut DropNews) {
            let Some(drag) = self.drag.as_ref() else { return };
            let (source, accept) = (drag.source, drag.target.is_some());
            // data[2] packs the ROOT-relative position as (x << 16) | y, as
            // two 16-bit unsigned fields.
            let (root_x, root_y) = ((data[2] >> 16) as i16, (data[2] & 0xffff) as i16);
            if let Some((x, y)) = self.to_window(root_x, root_y) {
                self.last_pos = Some((x, y)); // XdndDrop carries no position
                if accept {
                    news.hover = Some(((x, y), HOVER_LABEL.to_string()));
                }
            }
            // XdndStatus goes back to the SOURCE, and its window field must name
            // the real target (the toplevel), never the proxy the message
            // arrived at. A zero rectangle in data[2]/data[3] means "keep
            // sending me positions" — rt's accept/refuse answer changes per pane
            // and per overlay, so it must be re-asked on every motion.
            self.send(source, self.atoms.status, [
                self.toplevel,
                u32::from(accept),
                0,
                0,
                if accept { self.atoms.action_copy } else { 0 },
            ]);
        }

        /// `XdndDrop`: ask for the data. The answer arrives later as a
        /// `SelectionNotify`, which is where the text (and the reply to the
        /// source) is dealt with.
        fn on_drop(&mut self, data: [u32; 5]) {
            let Some(drag) = self.drag.as_ref() else { return };
            let Some(target) = drag.target else {
                // Nothing we could use: tell the source we are done, refused.
                let source = drag.source;
                self.send(source, self.atoms.finished, [self.toplevel, 0, 0, 0, 0]);
                self.drag = None;
                return;
            };
            // XDND >= 1 puts the timestamp in data[2]; older sources send none,
            // and CURRENT_TIME is the documented fallback.
            let time = if drag.version >= 1 { data[2] } else { x11rb::CURRENT_TIME };
            let _ = self.conn.convert_selection(
                self.proxy,
                self.atoms.selection,
                target,
                self.atoms.selection, // deliver onto this property of the proxy
                time,
            );
            let _ = self.conn.flush();
            // The position is not repeated in XdndDrop, so the last
            // XdndPosition's is what the drop landed on. Keep it for the moment
            // the data arrives.
            self.awaiting_drop =
                Some((self.last_pos.unwrap_or((0.0, 0.0)), std::time::Instant::now()));
        }

        /// The selection transfer completed (or failed). Read the property,
        /// hand the text to the app, and close the exchange with the source.
        fn on_selection(&mut self, ev: SelectionNotifyEvent, news: &mut DropNews) {
            let text = (ev.property != 0).then(|| self.read_property()).flatten();
            if let Some(drag) = self.drag.take() {
                // Whether the drop succeeded is the source's business: a failed
                // read must still be answered, or the source waits forever.
                self.send(drag.source, self.atoms.finished, [
                    self.toplevel,
                    u32::from(text.is_some()),
                    self.atoms.action_copy,
                    0,
                    0,
                ]);
            }
            let at = self.awaiting_drop.take().map(|(at, _)| at).or(self.last_pos);
            self.last_pos = None;
            news.hover = None; // the gesture is over; the cue goes with it
            match (text, at) {
                (Some(text), Some(at)) => news.dropped = Some((at, text)),
                // A transfer that answered with nothing, or one whose XdndPosition
                // never arrived, has no place to land: drop it on the floor.
                _ => log::debug!("XDND drop delivered no usable text"),
            }
        }

        /// Read (and delete) the delivered selection off the proxy window.
        ///
        /// Large values arrive in several chunks, which the `bytes_after` loop
        /// below collects. A value the owner decided to send INCREMENTALLY
        /// (type `INCR`, used above the server's maximum request size — hundreds
        /// of kilobytes) is refused rather than half-read: it needs a whole
        /// `PropertyNotify` state machine, and a dragged browser selection is
        /// nowhere near that size.
        fn read_property(&self) -> Option<String> {
            let mut buf: Vec<u8> = Vec::new();
            let mut offset = 0u32;
            loop {
                let reply = self
                    .conn
                    .get_property(false, self.proxy, self.atoms.selection, AtomEnum::ANY, offset, 4096)
                    .ok()?
                    .reply()
                    .ok()?;
                if reply.type_ == self.atoms.incr {
                    log::warn!("XDND drop is INCR (too large to read in one go); discarded");
                    let _ = self.conn.delete_property(self.proxy, self.atoms.selection);
                    return None;
                }
                let more = reply.bytes_after > 0;
                offset += (reply.value.len() / 4) as u32;
                buf.extend_from_slice(&reply.value);
                if !more || reply.value.is_empty() {
                    break;
                }
            }
            let _ = self.conn.delete_property(self.proxy, self.atoms.selection);
            // A `text/plain` or `STRING` target is Latin-1 by the letter of the
            // spec, but every source that offers it in practice sends UTF-8; a
            // lossy decode keeps a stray byte from throwing the whole drop away.
            Some(String::from_utf8_lossy(&buf).into_owned()).filter(|s| !s.is_empty())
        }

        /// Root coordinates -> the toplevel's own, which is the space winit
        /// reports the pointer in (X11 has no fractional scaling, so these are
        /// already physical pixels and need no factor).
        fn to_window(&self, root_x: i16, root_y: i16) -> Option<(f32, f32)> {
            let r = self
                .conn
                .translate_coordinates(self.root, self.toplevel, root_x, root_y)
                .ok()?
                .reply()
                .ok()?;
            Some((r.dst_x as f32, r.dst_y as f32))
        }

        /// Send one XDND `ClientMessage`. Mask 0 — XDND messages go to the
        /// window's creating client, which is what makes the proxy work at all.
        fn send(&self, to: XWindow, type_: u32, data: [u32; 5]) {
            let ev = ClientMessageEvent::new(32, to, type_, data);
            let _ = self.conn.send_event(false, to, EventMask::NO_EVENT, ev);
        }
    }

    impl Drop for State {
        /// Hand XDND back to winit's toplevel and take the proxy down. Without
        /// this, a closed window would leave an `XdndProxy` pointing at a dead
        /// id — the exact stale-value case the self-reference guard exists for,
        /// but there is no reason to rely on it.
        fn drop(&mut self) {
            let _ = self.conn.delete_property(self.toplevel, self.atoms.proxy);
            let _ = self.conn.destroy_window(self.proxy);
            let _ = self.conn.flush();
        }
    }
}
