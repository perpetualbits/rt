//! Best-effort macOS frosted glass: an untinted, variable-radius window blur
//! (Terminal.app's own mechanism, and the default) or an `NSVisualEffectView`.
//!
//! Same model as `blur.rs`, which notes: "A Wayland client cannot blur what is
//! behind its window itself — the compositor must do it." macOS is identical:
//! the window server blurs behind a transparent window. So this is a third
//! sibling of `blur.rs` (KDE `org_kde_kwin_blur`) and `bg_effect.rs`
//! (`ext-background-effect-v1`), and like both it degrades to a quiet no-op —
//! every failure path just logs and returns; nothing here can panic.
//!
//! The install/remove/retarget DECISION is not here: it is three booleans, an
//! enum and a number, so it lives in `vibrancy_policy.rs`, which is not `cfg`'d
//! and is therefore the only part of the frosted glass Linux CI can test. This
//! file is the AppKit half: it looks, asks [`glass_plan`] what to do, and does it.
//!
//! ## The two mechanisms, and why the untinted one is the default
//!
//! There are two ways to blur what is behind a macOS window:
//!
//! 1. an **`NSVisualEffectView`** with a named `NSVisualEffectMaterial` — public
//!    API, adapts to light/dark and the desktop tint on its own;
//! 2. the window's own **backdrop blur radius**, set through
//!    `CGSSetWindowBackgroundBlurRadius` — private SPI, a plain gaussian blur
//!    with no vibrancy and no adaptation, but **untinted and variable-radius**.
//!
//! This file started with only (1), on the reasoning that adaptation is the part
//! a Mac user notices. The user's verdict says otherwise: *"The default I want is
//! not in the list. I want the glass background OSX's own terminal uses …
//! `hud-window` comes closest, but it is too blurry and it is blue coloured,
//! which messes up the colors I want to choose."*
//!
//! Both halves of that are structural, not a matter of picking a better material:
//!
//! * **Blue coloured.** Every `NSVisualEffectMaterial` carries its own tint. That
//!   tint composites *under* the terminal's configured background colour, so at a
//!   low `background_opacity` it is most of what is on screen — and the user's
//!   chosen palette is no longer the colour he chose. No material is untinted.
//! * **Too blurry.** `NSVisualEffectView` exposes no radius at all. AppKit picks
//!   one per material and that is the end of it.
//!
//! Terminal.app does not use `NSVisualEffectView`. Its Profiles → Window pane is
//! a background colour with its own opacity plus a separate **Blur slider** — a
//! variable-radius, untinted blur behind an otherwise plain window. That is
//! mechanism (2). winit-appkit's own FFI declaration of it says as much: *"Wildly
//! used private APIs; Apple uses them for their Terminal.app."*
//!
//! So (2) is now a first-class mode, [`GlassMaterial::WindowBlur`], and the
//! default. `NSVisualEffectView` stays reachable for anyone who wants a material
//! — [`crate::vibrancy_policy::glass_plan`] decides which mechanism is live and
//! this file applies both halves of that decision in one pass.
//!
//! ### Why rt calls the SPI itself instead of winit's `set_blur`
//!
//! winit has mechanism (2) — but `WindowDelegate::set_blur` hardcodes the radius:
//! `let radius = if blur { 80 } else { 0 }`, with the comment "in general we want
//! to specify the blur radius, but the choice of 80 should be a reasonable
//! default". 80 is exactly the "too blurry" complaint. A radius the user can set
//! is the whole point of the mode, so [`set_window_blur_radius`] declares the two
//! private symbols itself. It is the same call winit makes, at a number rt
//! chooses; `main.rs` still falls back to `Window::set_blur` when this module
//! cannot reach AppKit at all.
//!
//! ## Where the view goes, and why NOT inside winit's view
//!
//! The effect view must end up strictly BELOW the terminal's pixels, or it
//! paints frosted glass over the text instead of behind it. Two placements are
//! possible and only one is safe here:
//!
//! * **As a subview of winit's view** (what most "vibrancy" snippets do — they
//!   target a WebView that is itself a subview, so the glass can sit under it).
//!   That is wrong for rt on two counts. First, `wgpu-hal`'s Metal surface
//!   `addSublayer:`s its `CAMetalLayer` onto winit's view's *own* backing layer,
//!   so a subview's layer is a sibling of the Metal layer with no ordering we
//!   control. Second, and fatally, the effect view would become the ONLY subview
//!   of winit's view, so `-[NSView hitTest:]` would return it for every click and
//!   winit's view would stop being the natural mouse target.
//! * **As a sibling of the content view, one level up** — the window's content
//!   view (which IS winit's view) has a superview: the window's frame view. We
//!   insert the effect view there, ordered `NSWindowBelow` *relative to the
//!   content view*. AppKit then keeps a whole view — layer, Metal sublayer and
//!   all — above the glass, and hit-testing still reaches winit's view first
//!   because the content view is the higher sibling. Only public API is used
//!   (`superview`, `addSubview:positioned:relativeTo:`); the frame view is never
//!   named or downcast.
//!
//! Swapping the window's `contentView` for the effect view (the other common
//! recipe) is NOT an option: winit 0.31's `WindowDelegate::view()` is
//! `self.window().contentView().unwrap().downcast().unwrap()`, so replacing the
//! content view makes winit panic on the next cursor/IME/resize call.
//!
//! ## Material
//!
//! Only reached when `settings.macos_glass_material` names one — the default,
//! [`GlassMaterial::WindowBlur`], installs no effect view at all (see above).
//! When one IS named it is set explicitly. It used to be left
//! unset — "take the system default for now" — on the theory that the look could
//! only be judged on screen. It was judged on screen, and the default lost:
//! AppKit's own header says `material` "Defaults to
//! `NSVisualEffectMaterialAppearanceBased`", a material deprecated since 10.14
//! and far denser than what Terminal.app shows. The user's report — "heavily
//! blurred, looks almost opaque … a vague light blue or grey … there may be two
//! layers somehow" — is that material's tint under the configured background
//! colour. The default is now [`GlassMaterial::UnderWindowBackground`], the one
//! whose documented purpose ("the material used under window backgrounds") is
//! literally rt's placement; every other named material stays reachable from the
//! Preferences dialog so the choice can be re-judged without a rebuild. That is
//! still the default *among materials* — but the overall default has since moved
//! off `NSVisualEffectView` entirely, for the reasons above.
//!
//! ## Opacity, and why the glass IS gated
//!
//! The glass is only visible through a translucent background: the frame clear
//! carries `settings.background_opacity` in its alpha (`main.rs` builds the clear
//! colour with `.with_alpha(background_opacity)` and `wgpu_backend` clears with
//! `a: bg.3`).
//!
//! This module previously installed the view unconditionally, reasoning that at
//! opacity 1.0 it would be invisible anyway so there was nothing to re-apply.
//! **That was wrong, and the user proved it:** at 0.05 opacity the glass is the
//! dominant thing on screen, `background_blur = false` did nothing at all, and
//! there was no way to turn it off short of editing the config and restarting.
//! The install is now gated on exactly the predicate the Wayland and X11 blur
//! paths use — `Settings::wants_background_blur` — and [`set_enabled`] is
//! re-called on every opacity step and settings commit, so the preference and
//! the slider both take effect live.
use objc2::MainThreadMarker;
use objc2::Message; // `retain()`: takes an owning reference out of a borrowed subview
use objc2::rc::Retained;
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSUserInterfaceItemIdentification, NSView,
    NSVisualEffectBlendingMode, NSVisualEffectMaterial, NSVisualEffectState, NSVisualEffectView,
    NSWindowOrderingMode,
};
use objc2_foundation::{NSString, ns_string};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use rt_config::GlassMaterial;
use winit::window::Window;

use crate::vibrancy_policy::{GlassAction, glass_plan};

// **PRIVATE Apple SPI.** Neither symbol below is in any public header; both live
// in CoreGraphics/SkyLight and have for many releases. This is the call
// Terminal.app's "Blur" slider makes, and the same pair winit-appkit declares —
// its own comment reads "Wildly used private APIs; Apple uses them for their
// Terminal.app". rt declares them only so it can pass a radius, which winit
// hardcodes to 80.
//
// Being private, they can vanish or change behaviour in any macOS release. Every
// use here **fails soft**: `CGSSetWindowBackgroundBlurRadius` returns a CGError
// that `set_window_blur_radius` logs and otherwise ignores, and nothing above
// depends on the blur having happened. If it stops working the window is simply
// translucent and unblurred — the same place the whole fallback chain ends.
#[link(name = "CoreGraphics", kind = "framework")]
unsafe extern "C" {
    /// This process's connection to the window server. Not owned; nothing to release.
    fn CGSMainConnectionID() -> *mut std::ffi::c_void;
    /// Blur `radius` pixels of whatever is behind window `window_id`. `0` = off,
    /// and is how a previously-set radius is cleared.
    fn CGSSetWindowBackgroundBlurRadius(
        connection_id: *mut std::ffi::c_void,
        window_id: isize,
        radius: i64,
    ) -> i32;
}

/// The `identifier` rt stamps on its own effect view, and the ONLY way the view
/// is found again.
///
/// Not "the first `NSVisualEffectView` among the frame view's subviews": AppKit
/// is free to put its own effect views in a window's frame view (titlebar
/// chrome has used them before), and mistaking one of those for ours would mean
/// removing a piece of the window's decoration when the user turns blur off.
/// The identifier is a public `NSView` property AppKit itself never sets.
fn tag() -> &'static NSString {
    ns_string!("rt.vibrancy.glass")
}

/// Bring the window's frosted glass in line with `want`, `material` and `radius`.
///
/// * `want` — [`rt_config::Settings::wants_background_blur`]: the user's
///   `background_blur` toggle AND a translucent background.
/// * `material` — `settings.macos_glass_material`. [`GlassMaterial::WindowBlur`]
///   (the default) means "no effect view; blur the window itself", every other
///   value names an `NSVisualEffectMaterial`.
/// * `radius` — `settings.macos_blur_radius`, used only by `WindowBlur`.
///
/// Both mechanisms are applied on every call, from one
/// [`glass_plan`] decision, so switching between them at runtime takes the old
/// one down in the same pass it brings the new one up. That is the difference
/// between "Preferences changed the look" and "Preferences added a second layer
/// on top of the first".
///
/// Returns `true` when the window's glass now reflects that state (including
/// "correctly absent"), `false` when the window could not be reached at all —
/// which means "nothing was done, try the next fallback".
///
/// **Idempotent, and meant to be called repeatedly**: at window creation, on
/// every opacity step, and on every settings commit. It never keeps a handle to
/// the effect view — `addSubview:` is what retains it, and the view is found
/// again by its [`tag`] — so there is no ownership to get wrong, no second copy
/// of the truth in `Active`, and no way for a repeat call to stack a second
/// pane of glass on top of the first. Main thread only.
pub fn set_enabled(window: &dyn Window, want: bool, material: GlassMaterial, radius: u32) -> bool {
    // NSVisualEffectView is main-thread-only. `new()` checks the current thread
    // for real (unlike `new_unchecked`), so an off-main-thread call degrades to
    // the no-op instead of to undefined behaviour.
    let Some(mtm) = MainThreadMarker::new() else {
        log::debug!("vibrancy: not on the main thread; skipping frosted glass");
        return false;
    };
    // winit's NSView pointer, via the same raw-window-handle path blur.rs uses
    // for its wl_surface.
    let ns_view = match window.window_handle().map(|h| h.as_raw()) {
        Ok(RawWindowHandle::AppKit(h)) => h.ns_view, // NonNull<c_void> -> the NSView
        _ => {
            log::debug!("vibrancy: no AppKit window handle; skipping frosted glass");
            return false;
        }
    };

    // SAFETY: the handle comes straight from winit's live window and names a
    // valid NSView; we only borrow it for the duration of this call, and we are
    // on the main thread (proven by `mtm` above), which is where NSView lives.
    unsafe {
        let view: &NSView = ns_view.cast().as_ref();

        // The window, and its content view (which is this same winit view for a
        // normal winit window — we go through the window so we never assume it).
        let Some(ns_window) = view.window() else {
            log::debug!("vibrancy: view has no window yet; skipping frosted glass");
            return false;
        };
        let Some(content) = ns_window.contentView() else {
            log::debug!("vibrancy: window has no content view; skipping frosted glass");
            return false;
        };
        // One level up: the window's frame view. Public API, never downcast.
        let Some(frame_view) = content.superview() else {
            log::debug!("vibrancy: content view has no superview; skipping frosted glass");
            return false;
        };

        // Look, don't remember: the view hierarchy is the single source of truth
        // for "is the glass installed?".
        let existing = find_glass(&frame_view);
        let plan = glass_plan(existing.is_some(), want, material, radius);
        match plan.view {
            GlassAction::Nothing => {
                log::debug!("vibrancy: no frosted glass wanted and none installed");
            }
            GlassAction::Remove => {
                // `addSubview:` held the only strong reference, so this is also
                // what frees the view and retires the window server's backdrop.
                if let Some(fx) = existing {
                    fx.removeFromSuperview();
                }
                log::info!("removed the NSVisualEffectView frosted glass");
            }
            GlassAction::Retarget(m) => {
                if let Some(fx) = existing {
                    set_material(&fx, m);
                }
                log::debug!("vibrancy: frosted glass material = {}", m.name());
            }
            GlassAction::Install(m) => {
                let effect = NSVisualEffectView::new(mtm);
                // How rt finds this view again; see `tag`.
                effect.setIdentifier(Some(tag()));
                // behindWindow is the whole point: blur what is BEHIND the window,
                // not the window's own contents.
                effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
                // Keep the glass alive when the window is not focused; the default
                // (FollowsWindowActiveState) dims it on deactivate, which reads as a
                // rendering bug in a terminal.
                effect.setState(NSVisualEffectState::Active);
                set_material(&effect, m);
                // Cover the whole frame view and keep covering it across resizes.
                effect.setFrame(frame_view.bounds());
                effect.setAutoresizingMask(
                    NSAutoresizingMaskOptions::ViewWidthSizable
                        | NSAutoresizingMaskOptions::ViewHeightSizable,
                );
                // Below the CONTENT view specifically: the terminal (winit's view, its
                // backing layer and wgpu's CAMetalLayer sublayer) stays above the glass,
                // and clicks still land on the content view first.
                frame_view.addSubview_positioned_relativeTo(
                    &effect,
                    NSWindowOrderingMode::Below,
                    Some(&content),
                );

                // winit's `with_transparent(true)` already does this; repeat it so the
                // module is correct on its own terms — an opaque window would have the
                // window server composite over the glass. Deliberately NOT undone by
                // `Remove`: a translucent-but-unblurred window still needs it, and it
                // is winit's setting to own.
                ns_window.setOpaque(false);
                log::info!(
                    "installed NSVisualEffectView frosted glass ({}) beneath the content view",
                    m.name()
                );
            }
        }

        // The other half of the plan, applied unconditionally — including the
        // radius-0 case, which is what CLEARS a blur the window server is still
        // holding from a previous call (turning blur off, or switching to a
        // material). See `glass_plan`.
        //
        // Also needs a non-opaque window, exactly as the effect view does; winit's
        // `with_transparent(true)` already did this, and repeating it keeps the
        // module correct on its own terms.
        if plan.blur_radius > 0 {
            ns_window.setOpaque(false);
        }
        set_window_blur_radius(ns_window.windowNumber(), plan.blur_radius);
    }
    true
}

/// Ask the window server to blur `radius` pixels of whatever is behind window
/// number `window_number`. `0` turns it off.
///
/// This is the Terminal.app mechanism; see the module docs and the
/// `CGSSetWindowBackgroundBlurRadius` declaration for what "private SPI" costs
/// us here. **Fails soft**: a non-zero `CGError` is logged at debug and nothing
/// else happens — the window is then merely translucent, which is where the
/// fallback chain ends anyway. Nothing in this function can panic.
fn set_window_blur_radius(window_number: isize, radius: u32) {
    // SAFETY: both symbols are plain C functions taking integers and an opaque
    // connection handle; `CGSMainConnectionID` is the process's own connection to
    // the window server and needs no release. `window_number` came from AppKit's
    // `-[NSWindow windowNumber]` a moment ago on the main thread. A stale or
    // unknown window number is not UB — the call returns a CGError, which is
    // exactly the soft failure this path is built around.
    let err = unsafe {
        CGSSetWindowBackgroundBlurRadius(CGSMainConnectionID(), window_number, radius.into())
    };
    if err != 0 {
        log::debug!("vibrancy: CGSSetWindowBackgroundBlurRadius({radius}) failed with {err}");
    } else if radius > 0 {
        log::info!("window backdrop blur set to radius {radius} (untinted, Terminal.app-style)");
    } else {
        log::debug!("vibrancy: window backdrop blur cleared");
    }
}

/// rt's own effect view among the frame view's subviews, if it is installed.
///
/// # Safety
/// `frame_view` must be a live `NSView` and the caller must be on the main thread.
unsafe fn find_glass(frame_view: &NSView) -> Option<Retained<NSVisualEffectView>> {
    frame_view.subviews().iter().find_map(|sub| {
        let fx = sub.downcast_ref::<NSVisualEffectView>()?;
        // Ours, not AppKit's own chrome. See `tag`.
        (fx.identifier().as_deref() == Some(tag())).then(|| fx.retain())
    })
}

/// Push `material` at an effect view.
///
/// [`GlassMaterial::SystemDefault`] means "never call `setMaterial:`", which is
/// only reachable on a freshly built view — a view that already carries a
/// material cannot be talked back into AppKit's implicit default, so switching
/// to `system-default` on a live window is the one change that takes a restart.
/// That is documented on the setting; every other material applies immediately.
fn set_material(effect: &NSVisualEffectView, material: GlassMaterial) {
    let m = match material {
        GlassMaterial::UnderWindowBackground => NSVisualEffectMaterial::UnderWindowBackground,
        GlassMaterial::UnderPageBackground => NSVisualEffectMaterial::UnderPageBackground,
        GlassMaterial::ContentBackground => NSVisualEffectMaterial::ContentBackground,
        GlassMaterial::WindowBackground => NSVisualEffectMaterial::WindowBackground,
        GlassMaterial::Sidebar => NSVisualEffectMaterial::Sidebar,
        GlassMaterial::HeaderView => NSVisualEffectMaterial::HeaderView,
        GlassMaterial::Titlebar => NSVisualEffectMaterial::Titlebar,
        GlassMaterial::Menu => NSVisualEffectMaterial::Menu,
        GlassMaterial::Popover => NSVisualEffectMaterial::Popover,
        GlassMaterial::Sheet => NSVisualEffectMaterial::Sheet,
        GlassMaterial::FullScreenUi => NSVisualEffectMaterial::FullScreenUI,
        GlassMaterial::HudWindow => NSVisualEffectMaterial::HUDWindow,
        // Leave AppKit's implicit default in place, as the control case.
        GlassMaterial::SystemDefault => return,
        // Not a material: `glass_plan` never routes it to Install/Retarget, so
        // this arm is unreachable by construction. It is spelled out rather than
        // wildcarded so adding a variant to `GlassMaterial` still fails to
        // compile here until someone decides what it means.
        GlassMaterial::WindowBlur => return,
    };
    effect.setMaterial(m);
}
