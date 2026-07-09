//! Cross-backend clipboard: Wayland (smithay-clipboard) or X11 (arboard, behind
//! the `x11` feature), chosen at runtime from the window's display handle.
//!
//! Both back ends provide the CLIPBOARD *and* PRIMARY selections, so the four
//! methods below match how `main.rs` uses the clipboard: `store`/`load` for the
//! usual Ctrl+Shift+C/V clipboard, and `store_primary`/`load_primary` for the
//! X11-style middle-click PRIMARY selection. `load`/`load_primary` return
//! `Result<String, ()>` so failure/absence is easy to ignore at the call site.

use raw_window_handle::RawDisplayHandle;

/// A live clipboard connection for one of the supported windowing backends.
pub enum Clipboard {
    /// Wayland: smithay-clipboard, tied to the window's `wl_display`.
    Wayland(smithay_clipboard::Clipboard),
    /// X11: arboard (only compiled with the `x11` feature).
    #[cfg(feature = "x11")]
    X11(X11Clipboard),
}

impl Clipboard {
    /// Build a clipboard from the window's raw display handle, or `None` when the
    /// backend isn't one we support (or the X11 connection fails). Wayland is
    /// always available; the X11 arms exist only under the `x11` feature.
    pub fn from_display(handle: RawDisplayHandle) -> Option<Self> {
        match handle {
            // SAFETY: the display pointer comes from winit's live Wayland display.
            RawDisplayHandle::Wayland(d) => {
                Some(Clipboard::Wayland(unsafe { smithay_clipboard::Clipboard::new(d.display.as_ptr()) }))
            }
            #[cfg(feature = "x11")]
            RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_) => {
                X11Clipboard::new().map(Clipboard::X11)
            }
            _ => None, // headless / unsupported backend: no clipboard
        }
    }

    /// Store `text` on the CLIPBOARD selection (Ctrl+Shift+C, app copy/paste).
    pub fn store(&self, text: String) {
        match self {
            Clipboard::Wayland(c) => c.store(text),
            #[cfg(feature = "x11")]
            Clipboard::X11(c) => c.store(text),
        }
    }

    /// Store `text` on the PRIMARY selection (middle-click paste).
    pub fn store_primary(&self, text: String) {
        match self {
            Clipboard::Wayland(c) => c.store_primary(text),
            #[cfg(feature = "x11")]
            Clipboard::X11(c) => c.store_primary(text),
        }
    }

    /// Read the CLIPBOARD selection, or `Err(())` if empty/unavailable.
    pub fn load(&self) -> Result<String, ()> {
        match self {
            Clipboard::Wayland(c) => c.load().map_err(|_| ()),
            #[cfg(feature = "x11")]
            Clipboard::X11(c) => c.load(),
        }
    }

    /// Read the PRIMARY selection, or `Err(())` if empty/unavailable.
    pub fn load_primary(&self) -> Result<String, ()> {
        match self {
            Clipboard::Wayland(c) => c.load_primary().map_err(|_| ()),
            #[cfg(feature = "x11")]
            Clipboard::X11(c) => c.load_primary(),
        }
    }
}

/// The X11 clipboard backend: an arboard `Clipboard` behind a `RefCell` so its
/// `&mut` get/set methods can be driven through the `&self` API above (rt is
/// single-threaded, so no locking is needed — matching smithay's `&self` shape).
#[cfg(feature = "x11")]
pub struct X11Clipboard {
    inner: std::cell::RefCell<arboard::Clipboard>,
}

#[cfg(feature = "x11")]
impl X11Clipboard {
    fn new() -> Option<Self> {
        arboard::Clipboard::new()
            .ok()
            .map(|c| X11Clipboard { inner: std::cell::RefCell::new(c) })
    }

    fn store(&self, text: String) {
        let _ = self.inner.borrow_mut().set_text(text); // CLIPBOARD
    }

    fn store_primary(&self, text: String) {
        use arboard::{LinuxClipboardKind, SetExtLinux};
        let _ = self.inner.borrow_mut().set().clipboard(LinuxClipboardKind::Primary).text(text);
    }

    fn load(&self) -> Result<String, ()> {
        self.inner.borrow_mut().get_text().map_err(|_| ())
    }

    fn load_primary(&self) -> Result<String, ()> {
        use arboard::{GetExtLinux, LinuxClipboardKind};
        self.inner
            .borrow_mut()
            .get()
            .clipboard(LinuxClipboardKind::Primary)
            .text()
            .map_err(|_| ())
    }
}
