//! Best-effort macOS frosted glass via `NSVisualEffectView`.
//!
//! Same model as `blur.rs`, which notes: "A Wayland client cannot blur what is
//! behind its window itself — the compositor must do it." macOS is identical:
//! the window server blurs behind a transparent window. So this is a third
//! sibling of `blur.rs` (KDE `org_kde_kwin_blur`) and `bg_effect.rs`
//! (`ext-background-effect-v1`), and like both it degrades to a quiet no-op —
//! every failure path just logs and returns; nothing here can panic.
//!
//! The install/remove/retarget DECISION is not here: it is three booleans and an
//! enum, so it lives in `vibrancy_policy.rs`, which is not `cfg`'d and is
//! therefore the only part of the frosted glass Linux CI can test. This file is
//! the AppKit half: it looks, asks [`glass_action`] what to do, and does it.
//!
//! ## Why not winit's `set_blur(true)`
//!
//! winit implements macOS blur as `CGSSetWindowBackgroundBlurRadius(.., 80)` — a
//! PRIVATE CoreGraphics/SkyLight call at a hardcoded radius (verified in
//! `winit-appkit`'s `WindowDelegate::set_blur`). It is a plain gaussian backdrop
//! blur: no vibrancy, no material, and no automatic light/dark or desktop-tint
//! adaptation. `NSVisualEffectView` is public API and adapts on its own, which is
//! the part a Mac user actually notices. We keep the private path only as
//! fallback 2, taken by `main.rs` when [`set_enabled`] returns `false`.
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
//! Set explicitly, from `settings.macos_glass_material`. It used to be left
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
//! Preferences dialog so the choice can be re-judged without a rebuild.
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

use crate::vibrancy_policy::{GlassAction, glass_action};

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

/// Bring the window's frosted glass in line with `want` and `material`.
///
/// * `want` — [`rt_config::Settings::wants_background_blur`]: the user's
///   `background_blur` toggle AND a translucent background.
/// * `material` — `settings.macos_glass_material`.
///
/// Returns `true` when the window's glass now reflects that state (including
/// "correctly absent"), `false` when `NSVisualEffectView` could not be reached
/// at all — which means "nothing was done, try the next fallback".
///
/// **Idempotent, and meant to be called repeatedly**: at window creation, on
/// every opacity step, and on every settings commit. It never keeps a handle to
/// the effect view — `addSubview:` is what retains it, and the view is found
/// again by its [`tag`] — so there is no ownership to get wrong, no second copy
/// of the truth in `Active`, and no way for a repeat call to stack a second
/// pane of glass on top of the first. Main thread only.
pub fn set_enabled(window: &dyn Window, want: bool, material: GlassMaterial) -> bool {
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
        match glass_action(existing.is_some(), want, material) {
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
    }
    true
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
    };
    effect.setMaterial(m);
}
