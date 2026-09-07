//! Cross-backend clipboard: Wayland ([`crate::wl_clipboard`]) or X11 (arboard,
//! behind the `x11` feature), chosen at runtime from the window's display handle.
//!
//! Both back ends provide the CLIPBOARD *and* PRIMARY selections, so the four
//! methods below match how `main.rs` uses the clipboard: `store`/`load` for the
//! usual Ctrl+Shift+C/V clipboard, and `store_primary`/`load_primary` for the
//! X11-style middle-click PRIMARY selection. `load`/`load_primary` return
//! `Result<String, ()>` so failure/absence is easy to ignore at the call site.

use raw_window_handle::RawDisplayHandle;

/// A live clipboard connection for one of the supported windowing backends.
pub enum Clipboard {
    /// Wayland: rt's own `wl_data_device` worker, tied to the window's
    /// `wl_display` — the same one that receives dragged-in text.
    #[cfg(not(target_os = "macos"))]
    Wayland(crate::wl_clipboard::Clipboard),
    /// X11: arboard (only compiled with the `x11` feature).
    #[cfg(all(feature = "x11", not(target_os = "macos")))]
    X11(X11Clipboard),
    /// macOS: arboard over NSPasteboard.
    #[cfg(target_os = "macos")]
    Mac(MacClipboard),
}

impl Clipboard {
    /// Build a clipboard from the window's raw display handle, or `None` when the
    /// backend isn't one we support (or the X11 connection fails). Wayland is
    /// always available; the X11 arms exist only under the `x11` feature.
    pub fn from_display(handle: RawDisplayHandle) -> Option<Self> {
        match handle {
            // SAFETY: the display pointer comes from winit's live Wayland display.
            #[cfg(not(target_os = "macos"))]
            RawDisplayHandle::Wayland(d) => {
                Some(Clipboard::Wayland(unsafe { crate::wl_clipboard::Clipboard::new(d.display.as_ptr()) }))
            }
            #[cfg(all(feature = "x11", not(target_os = "macos")))]
            RawDisplayHandle::Xlib(_) | RawDisplayHandle::Xcb(_) => {
                X11Clipboard::new().map(Clipboard::X11)
            }
            #[cfg(target_os = "macos")]
            RawDisplayHandle::AppKit(_) => {
                arboard::Clipboard::new().ok().map(|c| Clipboard::Mac(MacClipboard { inner: std::cell::RefCell::new(c) }))
            }
            _ => None, // headless / unsupported backend: no clipboard
        }
    }

    /// Store `text` on the CLIPBOARD selection (Ctrl+Shift+C, app copy/paste).
    pub fn store(&self, text: String) {
        match self {
            #[cfg(not(target_os = "macos"))]
            Clipboard::Wayland(c) => c.store(text),
            #[cfg(all(feature = "x11", not(target_os = "macos")))]
            Clipboard::X11(c) => c.store(text),
            #[cfg(target_os = "macos")]
            Clipboard::Mac(c) => c.store(text),
        }
    }

    /// Store `text` on the PRIMARY selection (middle-click paste).
    pub fn store_primary(&self, text: String) {
        match self {
            #[cfg(not(target_os = "macos"))]
            Clipboard::Wayland(c) => c.store_primary(text),
            #[cfg(all(feature = "x11", not(target_os = "macos")))]
            Clipboard::X11(c) => c.store_primary(text),
            // macOS has NO PRIMARY selection. This is not a gap to fill later:
            // PRIMARY is an X11 concept, so middle-click paste does not exist on
            // macOS and `load_primary` correctly reports nothing.
            #[cfg(target_os = "macos")]
            Clipboard::Mac(_) => {}
        }
    }

    /// Read the CLIPBOARD selection, or `Err(())` if empty/unavailable.
    pub fn load(&self) -> Result<String, ()> {
        match self {
            #[cfg(not(target_os = "macos"))]
            Clipboard::Wayland(c) => c.load().map_err(|_| ()),
            #[cfg(all(feature = "x11", not(target_os = "macos")))]
            Clipboard::X11(c) => c.load(),
            #[cfg(target_os = "macos")]
            Clipboard::Mac(c) => c.load(),
        }
    }

    /// Read the PRIMARY selection, or `Err(())` if empty/unavailable.
    pub fn load_primary(&self) -> Result<String, ()> {
        match self {
            #[cfg(not(target_os = "macos"))]
            Clipboard::Wayland(c) => c.load_primary().map_err(|_| ()),
            #[cfg(all(feature = "x11", not(target_os = "macos")))]
            Clipboard::X11(c) => c.load_primary(),
            // macOS has NO PRIMARY selection. This is not a gap to fill later:
            // PRIMARY is an X11 concept, so middle-click paste does not exist on
            // macOS and `load_primary` correctly reports nothing.
            #[cfg(target_os = "macos")]
            Clipboard::Mac(_) => Err(()),
        }
    }
}

/// The X11 clipboard backend: an arboard `Clipboard` behind a `RefCell` so its
/// `&mut` get/set methods can be driven through the `&self` API above (rt is
/// single-threaded, so no locking is needed — matching smithay's `&self` shape).
#[cfg(all(feature = "x11", not(target_os = "macos")))]
pub struct X11Clipboard {
    inner: std::cell::RefCell<arboard::Clipboard>,
}

#[cfg(all(feature = "x11", not(target_os = "macos")))]
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

/// The macOS clipboard backend: an arboard `Clipboard` behind a `RefCell` so its
/// `&mut self` methods are reachable from `&self` (the event loop is
/// single-threaded, so no locking is needed — matching smithay's `&self` shape).
#[cfg(target_os = "macos")]
pub struct MacClipboard {
    inner: std::cell::RefCell<arboard::Clipboard>,
}

#[cfg(target_os = "macos")]
impl MacClipboard {
    fn store(&self, text: String) {
        let _ = self.inner.borrow_mut().set_text(text);
    }
    fn load(&self) -> Result<String, ()> {
        self.inner.borrow_mut().get_text().map_err(|_| ())
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use raw_window_handle::{AppKitDisplayHandle, RawDisplayHandle};

    // Runs on kiku only: constructing an arboard Clipboard needs a real
    // pasteboard, so this cannot be exercised in Linux CI.
    #[test]
    fn appkit_display_selects_the_mac_backend() {
        let h = RawDisplayHandle::AppKit(AppKitDisplayHandle::new());
        assert!(matches!(Clipboard::from_display(h), Some(Clipboard::Mac(_))));
    }

    #[test]
    fn primary_is_a_no_op_and_never_panics() {
        let h = RawDisplayHandle::AppKit(AppKitDisplayHandle::new());
        let c = Clipboard::from_display(h).expect("mac clipboard");
        c.store_primary("ignored".to_string()); // must not panic
        assert!(c.load_primary().is_err());     // macOS has no PRIMARY
    }
}
