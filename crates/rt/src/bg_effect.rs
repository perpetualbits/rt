//! Cross-compositor background blur via the `ext-background-effect-v1` staging
//! protocol (KDE Plasma 6.7+, COSMIC's frosted-glass work, niri).
//!
//! A Wayland client cannot blur what is behind its own translucent window — the
//! compositor must. This protocol lets us *request* that the compositor blur a
//! region of the background behind our surface; the compositor does the actual
//! blurring. On compositors without the global (GNOME today, X11, older KWin)
//! this whole module degrades to a single debug line and a no-op.
//!
//! ## Relationship to `blur.rs`
//! [`crate::blur`] requests the older, KDE-only `org_kde_kwin_blur`. This module
//! is the standard, cross-compositor path. They are independent; on Plasma 6.7+
//! both globals may exist and both requests are harmless.
//!
//! ## Why it is stateful (unlike `blur.rs`)
//! The blur region here is explicit (a `wl_region` sized to the surface) and a
//! NULL region *removes* the effect — so we must resize the region on every
//! window resize and toggle it when the user changes opacity/config. We
//! therefore keep our private `Connection` + `EventQueue` alive for the app's
//! lifetime rather than firing one startup request and dropping it.
//!
//! ## Safety / interaction with winit
//! winit owns the real Wayland connection and its event queue. We wrap the
//! *same* `wl_display` (`Backend::from_foreign_display`) in our own `Connection`
//! and private `EventQueue`, and reconstruct winit's `wl_surface` from its raw
//! pointer. wayland-client routes each proxy's events to the queue that owns it,
//! so winit's socket reads buffer our manager's `capabilities` events into our
//! queue without stealing winit's; `dispatch_pending` drains them. Every failure
//! path returns quietly; nothing here panics.

use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle, RawWindowHandle};
use wayland_client::backend::{Backend, ObjectId};
use wayland_client::protocol::wl_compositor::WlCompositor;
use wayland_client::protocol::wl_region::WlRegion;
use wayland_client::protocol::wl_registry::{self, WlRegistry};
use wayland_client::protocol::wl_surface::WlSurface;
use wayland_client::{Connection, Dispatch, EventQueue, Proxy, QueueHandle};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_manager_v1::{
    self as mgr, Capability, ExtBackgroundEffectManagerV1,
};
use wayland_protocols::ext::background_effect::v1::client::ext_background_effect_surface_v1::ExtBackgroundEffectSurfaceV1;
use winit::window::Window;

/// The globals we bind during setup and the compositor's advertised blur
/// capability. Lives inside the [`BackgroundEffect`] and is what our event queue
/// dispatches against.
#[derive(Default)]
struct State {
    manager: Option<ExtBackgroundEffectManagerV1>, // effect factory (once bound)
    compositor: Option<WlCompositor>,              // needed to create wl_regions
    blur_supported: bool,                          // compositor's `capabilities` includes blur
}

// Registry: bind the effect manager and the compositor when announced.
impl Dispatch<WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &WlRegistry,
        event: wl_registry::Event,
        _data: &(),
        _conn: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_registry::Event::Global { name, interface, version } = event {
            if interface == ExtBackgroundEffectManagerV1::interface().name {
                // v1 is the only version; the `capabilities` event follows the bind.
                state.manager =
                    Some(registry.bind::<ExtBackgroundEffectManagerV1, _, _>(name, version.min(1), qh, ()));
            } else if interface == WlCompositor::interface().name {
                // create_region exists since v1; cap the version defensively.
                state.compositor =
                    Some(registry.bind::<WlCompositor, _, _>(name, version.min(6), qh, ()));
            }
        }
    }
}

// The manager's only event is `capabilities`: a bitfield telling us whether the
// compositor can blur. It arrives on bind and again whenever caps change.
impl Dispatch<ExtBackgroundEffectManagerV1, ()> for State {
    fn event(
        state: &mut Self,
        _mgr: &ExtBackgroundEffectManagerV1,
        event: mgr::Event,
        _data: &(),
        _conn: &Connection,
        _qh: &QueueHandle<Self>,
    ) {
        if let mgr::Event::Capabilities { flags } = event {
            // `flags` is a WEnum bitfield; treat unknown/absent as "no blur".
            let bits = flags.into_result().map(|c| c.contains(Capability::Blur)).unwrap_or(false);
            state.blur_supported = bits;
        }
    }
}

// These proxies emit no events we act on.
wayland_client::delegate_noop!(State: ignore WlCompositor);
wayland_client::delegate_noop!(State: ignore WlRegion);
wayland_client::delegate_noop!(State: ignore ExtBackgroundEffectSurfaceV1);

/// Live handle to the background-blur effect for the main window. Constructed
/// once the window/surface exists; kept in `Active` so resizes and config
/// changes can update the blur region. All methods are safe no-ops when the
/// compositor lacks the protocol.
pub struct BackgroundEffect {
    conn: Connection,             // our view of winit's wl_display (shared backend)
    queue: EventQueue<State>,     // private queue; dispatches `State`
    qh: QueueHandle<State>,       // handle to route new proxies (regions) here
    state: State,                 // bound globals + blur capability
    surface: WlSurface,           // winit's main surface, adopted onto our conn
    effect: ExtBackgroundEffectSurfaceV1, // the per-surface effect object
    size: (i32, i32),             // last known surface size (for the region)
    want: bool,                   // caller wants blur (config on AND translucent)
    applied: bool,                // whether a non-NULL region is currently set
}

impl BackgroundEffect {
    /// Try to set up background blur for `window`. `want` is the caller's final
    /// decision (typically `config.background_blur && opacity < 1.0`). Returns
    /// `None` — degrading silently — when this is not Wayland, the protocol is
    /// absent, or any setup step fails. Safe to call once, right after the window
    /// exists.
    pub fn try_init(window: &Window, want: bool) -> Option<Self> {
        // winit's raw Wayland pointers; bail on anything non-Wayland.
        let display_ptr = match window.display_handle().map(|h| h.as_raw()) {
            Ok(RawDisplayHandle::Wayland(d)) => d.display.as_ptr(),
            _ => {
                log::debug!("no Wayland display handle; skipping background-effect blur");
                return None;
            }
        };
        let surface_ptr = match window.window_handle().map(|h| h.as_raw()) {
            Ok(RawWindowHandle::Wayland(w)) => w.surface.as_ptr(),
            _ => {
                log::debug!("no Wayland surface handle; skipping background-effect blur");
                return None;
            }
        };

        // Wrap winit's live display in our own connection + private queue.
        // SAFETY: the pointer is winit's live wl_display and outlives us;
        // from_foreign_display borrows, it does not take ownership.
        let backend = unsafe { Backend::from_foreign_display(display_ptr as *mut _) };
        let conn = Connection::from_backend(backend);
        let mut queue = conn.new_event_queue();
        let qh = queue.handle();

        // Roundtrip 1: receive the global list and bind the manager + compositor.
        let _registry = conn.display().get_registry(&qh, ());
        let mut state = State::default();
        if queue.roundtrip(&mut state).is_err() {
            log::debug!("registry roundtrip failed; skipping background-effect blur");
            return None;
        }
        // Not advertised → not a supporting compositor. The scrim/opacity carry on.
        let Some(manager) = state.manager.clone() else {
            log::debug!("ext-background-effect-v1 not advertised; relying on the scrim");
            return None;
        };
        // Roundtrip 2: the `capabilities` event arrives after the manager bind.
        let _ = queue.roundtrip(&mut state);
        if state.compositor.is_none() {
            log::debug!("no wl_compositor to build a blur region; skipping");
            return None;
        }

        // Adopt winit's wl_surface as a proxy on our connection (same display, so
        // the object id is valid here too).
        // SAFETY: surface_ptr is winit's live wl_surface for this window.
        let id = unsafe { ObjectId::from_ptr(WlSurface::interface(), surface_ptr as *mut _) }.ok()?;
        let surface = WlSurface::from_id(&conn, id).ok()?;

        // Instantiate the per-surface effect object. Held for the app's lifetime.
        let effect = manager.get_background_effect(&surface, &qh, ());
        // The manager exists; report whether it can actually blur (the
        // `capabilities` event may advertise none).
        log::info!(
            "ext-background-effect-v1 present; blur capability {}",
            if state.blur_supported { "supported" } else { "not offered" }
        );

        let size = {
            let s = window.inner_size();
            (s.width as i32, s.height as i32)
        };
        let mut me = BackgroundEffect {
            conn,
            queue,
            qh,
            state,
            surface,
            effect,
            size,
            want,
            applied: false,
        };
        me.reconcile(); // apply the initial region if wanted + supported
        Some(me)
    }

    /// Enable or disable blur at runtime (opacity crossed 1.0, or the config
    /// toggle changed). `want` is the caller's final decision. Removes the effect
    /// (NULL region) when turning off.
    pub fn set_enabled(&mut self, want: bool) {
        self.pump(); // catch any capability change first
        self.want = want;
        self.reconcile();
    }

    /// Track a window resize: re-size the blur region so it keeps covering the
    /// whole surface with no unblurred strips. Cheap no-op when blur is off.
    pub fn on_resize(&mut self, w: u32, h: u32) {
        self.pump();
        self.size = (w as i32, h as i32);
        // Only re-emit the region while blur is actually active; otherwise there
        // is nothing to keep in sync and we avoid an unnecessary commit.
        if self.want && self.state.blur_supported {
            self.reconcile();
        }
    }

    /// Drain any events winit's socket reads have buffered onto our queue —
    /// namely the manager's `capabilities` changes. Non-blocking; errors ignored.
    fn pump(&mut self) {
        let _ = self.queue.dispatch_pending(&mut self.state);
    }

    /// Make the compositor state match `want && blur_supported`: set a full-size
    /// blur region when it should be on, or a NULL region (removing the effect)
    /// when it should be off. Commits the surface so the double-buffered region
    /// takes effect. Skips work when already in the desired state.
    fn reconcile(&mut self) {
        let desired = self.want && self.state.blur_supported;
        if desired {
            // A fresh region covering the whole surface. Copy semantics let us
            // destroy it immediately (the compositor clips it to the surface).
            if let Some(compositor) = &self.state.compositor {
                let region = compositor.create_region(&self.qh, ());
                region.add(0, 0, self.size.0.max(1), self.size.1.max(1));
                self.effect.set_blur_region(Some(&region));
                region.destroy();
                self.surface.commit();
                let _ = self.conn.flush();
                if !self.applied {
                    log::info!("background blur enabled ({}x{})", self.size.0, self.size.1);
                }
                self.applied = true;
            }
        } else if self.applied {
            // Turn it off: NULL region removes the effect on the next commit.
            self.effect.set_blur_region(None);
            self.surface.commit();
            let _ = self.conn.flush();
            self.applied = false;
            log::info!("background blur removed");
        }
        // else: not applied and not desired → nothing to commit.
    }
}

impl Drop for BackgroundEffect {
    /// Release the effect object so its regions are removed on the next commit
    /// (matches the protocol's destructor semantics). The shared display and
    /// winit's surface are left untouched.
    fn drop(&mut self) {
        self.effect.destroy();
        let _ = self.conn.flush();
    }
}
