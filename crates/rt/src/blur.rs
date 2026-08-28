//! Best-effort KDE/KWin background blur via the `org_kde_kwin_blur` protocol.
//!
//! A Wayland client cannot blur what is behind its window itself — the
//! compositor must do it (see `docs/APPEARANCE.md`). KWin exposes the
//! `org_kde_kwin_blur_manager` global that lets a client *request* the
//! compositor blur behind its surface. This module makes that request when the
//! global is present and does nothing (just logs) otherwise, so it is a safe
//! no-op on COSMIC / GNOME / sway and everything else.
//!
//! ## Safety / interaction with winit
//! winit owns the real Wayland connection and its own event queue. We wrap the
//! *same* `wl_display` (via `Backend::from_foreign_display`) in our own
//! `Connection` and a private `EventQueue`, do a single startup roundtrip to
//! find the blur manager, and reconstruct winit's `wl_surface` from its raw
//! pointer to attach the blur. wayland-client routes each proxy's events to the
//! queue that owns it, so our roundtrip buffers (does not steal) winit's events
//! — a single setup roundtrip is the established pattern and does not disturb
//! winit's loop. Every failure path returns quietly; nothing here can panic.
//!
//! ## Verification status
//! Compiled and confirmed to no-op safely on a non-KDE compositor (the manager
//! global is absent → we log and return, app runs normally). NOT yet verified
//! against a live KWin session — flagged in `docs/APPEARANCE.md`.

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur::OrgKdeKwinBlur;
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur_manager::OrgKdeKwinBlurManager;
use winit::window::Window;

/// State for our private event queue: collects the blur manager if the
/// compositor advertises it during the registry roundtrip.
#[derive(Default)]
struct BlurState {
    manager: Option<OrgKdeKwinBlurManager>, // Some(..) once the KWin global is bound
}

// Handle `wl_registry` global announcements: bind the blur manager if we see it.
impl Dispatch<WlRegistry, ()> for BlurState {
    /// Called for each registry event during the roundtrip. We only care about
    /// `Global` announcements whose interface is the KWin blur manager; when we
    /// find it we bind it and stash it in `state`.
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        // Only the "a global exists" event is interesting here.
        if let wl_registry::Event::Global { name, interface, version } = event {
            // Match the KWin blur manager by its interface name.
            if interface == OrgKdeKwinBlurManager::interface().name {
                // Bind at the version the compositor offers (capped at 1, the
                // only version rt uses).
                let mgr = registry.bind::<OrgKdeKwinBlurManager, _, _>(name, version.min(1), qh, ());
                state.manager = Some(mgr); // remember it for after the roundtrip
            }
        }
    }
}

// The blur manager and blur objects emit no events we care about; ignore them.
wayland_client::delegate_noop!(BlurState: ignore OrgKdeKwinBlurManager);
wayland_client::delegate_noop!(BlurState: ignore OrgKdeKwinBlur);

/// Request KWin to blur behind `window`'s surface. Safe no-op on non-KDE
/// compositors and on any error. Call once, right after the window exists.
pub fn try_enable_kwin_blur(window: &dyn Window) {
    // Fetch winit's raw Wayland display + surface pointers; bail on anything
    // that is not a Wayland handle (rt is Wayland-only, but be defensive).
    let display_ptr = match window.display_handle().map(|h| h.as_raw()) {
        Ok(RawDisplayHandle::Wayland(d)) => d.display.as_ptr(), // *mut wl_display
        _ => {
            log::debug!("no Wayland display handle; skipping KWin blur");
            return;
        }
    };
    let surface_ptr = match window.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::Wayland(w)) => w.surface.as_ptr(), // *mut wl_surface (as wl_proxy)
        _ => {
            log::debug!("no Wayland surface handle; skipping KWin blur");
            return;
        }
    };

    // Wrap winit's existing display in our own connection + private queue.
    // SAFETY: the pointer comes straight from winit's live Wayland display and
    // outlives this function; from_foreign_display does not take ownership.
    let backend = unsafe { Backend::from_foreign_display(display_ptr as *mut _) };
    let conn = Connection::from_backend(backend); // shares winit's wl_display
    let mut queue = conn.new_event_queue(); // our own queue, separate from winit's
    let qh = queue.handle(); // handle used to route our proxies to this queue

    // Ask for the registry and round-trip once to receive the global list.
    let _registry = conn.display().get_registry(&qh, ()); // triggers Global events
    let mut state = BlurState::default(); // collects the manager if present
    if queue.roundtrip(&mut state).is_err() {
        log::debug!("registry roundtrip failed; skipping KWin blur");
        return;
    }

    // If the compositor did not advertise the blur manager, this is not KDE (or
    // blur is disabled) — the window is just translucent then.
    let Some(manager) = state.manager else {
        log::info!("KWin blur manager not advertised (non-KDE?); translucent only");
        return;
    };

    // Reconstruct winit's wl_surface as a proxy on our connection (same display,
    // so the object id is valid here too).
    // SAFETY: surface_ptr is winit's live wl_surface for this window.
    let id = match unsafe { ObjectId::from_ptr(WlSurface::interface(), surface_ptr as *mut _) } {
        Ok(id) => id,
        Err(_) => {
            log::debug!("could not adopt wl_surface for KWin blur");
            return;
        }
    };
    let surface = match WlSurface::from_id(&conn, id) {
        Ok(s) => s,
        Err(_) => return, // surface no longer valid; give up quietly
    };

    // Create a blur object bound to the surface, blur the whole surface
    // (set_region(None)), commit the blur, then commit the surface to apply.
    let blur = manager.create(&surface, &qh, ()); // request blur behind this surface
    blur.set_region(None); // None = blur the entire surface region
    blur.commit(); // finalise the blur object's state
    surface.commit(); // apply on the next surface commit
    let _ = conn.flush(); // push the requests to the compositor now
    log::info!("requested KWin background blur behind the window");
}
