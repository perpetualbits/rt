//! Background blur on X11, via the de-facto `_KDE_NET_WM_BLUR_BEHIND_REGION`
//! window property (honoured by KWin-X11 and picom with `kawase-blur`/`--blur`).
//!
//! This is the X11 counterpart to [`crate::bg_effect`] (Wayland). Setting the
//! property with an empty region asks the compositor to blur the whole window
//! behind us; deleting it turns blur off. Compositors that don't implement it
//! simply ignore the property, so this is a safe best-effort no-op there.
//!
//! The type compiles in both build configs: real under the `x11` feature, an
//! inert zero-sized stub otherwise, so `Active` can hold it without `cfg`.

use winit::window::Window;

/// A handle that can toggle the X11 background-blur property on rt's window.
/// Inert when built without `x11`, or when the window isn't an X11 window
/// (e.g. running the universal binary under native Wayland).
pub struct X11Blur {
    #[cfg(feature = "x11")]
    state: Option<imp::State>,
}

impl X11Blur {
    /// Set up X11 blur for `window`, applying the initial `enabled` state.
    pub fn try_init(window: &Window, enabled: bool) -> Self {
        #[cfg(feature = "x11")]
        {
            let state = imp::State::new(window);
            if let Some(s) = &state {
                s.set(enabled);
            }
            return X11Blur { state };
        }
        #[cfg(not(feature = "x11"))]
        {
            let _ = (window, enabled);
            X11Blur {}
        }
    }

    /// Turn the blur on or off. No-op when inert.
    pub fn set_enabled(&self, enabled: bool) {
        #[cfg(feature = "x11")]
        if let Some(s) = &self.state {
            s.set(enabled);
        }
        #[cfg(not(feature = "x11"))]
        let _ = enabled;
    }
}

#[cfg(feature = "x11")]
mod imp {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use winit::window::Window;
    use x11rb::connection::Connection;
    use x11rb::protocol::xproto::{AtomEnum, ConnectionExt, PropMode};
    use x11rb::rust_connection::RustConnection;
    use x11rb::wrapper::ConnectionExt as _; // for change_property32

    /// A live X11 connection plus the window id and interned atom, so toggling
    /// blur is a single property change. The connection is separate from winit's
    /// (X properties are server-global, so this is fine).
    pub struct State {
        conn: RustConnection,
        window: u32,
        atom: u32,
    }

    impl State {
        pub fn new(window: &Window) -> Option<Self> {
            // The X11 window id, or bail if this isn't an X11 window (Wayland).
            let window = match window.window_handle().ok()?.as_raw() {
                RawWindowHandle::Xlib(h) => h.window as u32,
                RawWindowHandle::Xcb(h) => h.window.get(),
                _ => return None,
            };
            let (conn, _screen) = x11rb::connect(None).ok()?; // honours $DISPLAY
            let atom = conn
                .intern_atom(false, b"_KDE_NET_WM_BLUR_BEHIND_REGION")
                .ok()?
                .reply()
                .ok()?
                .atom;
            Some(State { conn, window, atom })
        }

        pub fn set(&self, enabled: bool) {
            if enabled {
                // An empty region property = blur the entire window behind us.
                let _ = self.conn.change_property32(
                    PropMode::REPLACE,
                    self.window,
                    self.atom,
                    AtomEnum::CARDINAL,
                    &[],
                );
            } else {
                let _ = self.conn.delete_property(self.window, self.atom);
            }
            let _ = self.conn.flush();
        }
    }
}
