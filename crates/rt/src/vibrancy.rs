//! Best-effort macOS frosted glass via `NSVisualEffectView`.
//!
//! Same model as `blur.rs`, which notes: "A Wayland client cannot blur what is
//! behind its window itself — the compositor must do it." macOS is identical:
//! the window server blurs behind a transparent window. So this is a third
//! sibling of `blur.rs` (KDE `org_kde_kwin_blur`) and `bg_effect.rs`
//! (`ext-background-effect-v1`), and like both it degrades to a quiet no-op —
//! every failure path just logs and returns; nothing here can panic.
//!
//! ## Why not winit's `set_blur(true)`
//!
//! winit implements macOS blur as `CGSSetWindowBackgroundBlurRadius(.., 80)` — a
//! PRIVATE CoreGraphics/SkyLight call at a hardcoded radius (verified in
//! `winit-appkit`'s `WindowDelegate::set_blur`). It is a plain gaussian backdrop
//! blur: no vibrancy, no material, and no automatic light/dark or desktop-tint
//! adaptation. `NSVisualEffectView` is public API and adapts on its own, which is
//! the part a Mac user actually notices. We keep the private path only as
//! fallback 2, taken by `main.rs` when this function returns `false`.
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
//! Deliberately NOT set: take the system default for now. The look can only be
//! judged on screen, so picking a material up front would be guessing. If the
//! default looks flat, `NSVisualEffectMaterial::UnderWindowBackground` is the
//! closest match to how rt looks on KDE.
//!
//! ## Opacity
//!
//! The glass is only visible through a translucent background: the frame clear
//! carries `settings.background_opacity` in its alpha (`main.rs` builds the clear
//! colour with `.with_alpha(background_opacity)` and `wgpu_backend` clears with
//! `a: bg.3`). At opacity 1.0 the effect view is installed but completely hidden,
//! which looks exactly like the vibrancy having failed. That is why the view is
//! installed unconditionally rather than gated on `want_blur`: nothing to
//! re-apply when the user moves the opacity slider.
use objc2::MainThreadMarker;
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSView, NSVisualEffectBlendingMode, NSVisualEffectState,
    NSVisualEffectView, NSWindowOrderingMode,
};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use winit::window::Window;

/// Install an `NSVisualEffectView` beneath the window's content view.
///
/// Returns `true` only when the view is actually in the hierarchy; `false` means
/// "nothing was installed, try the next fallback". Call once, right after the
/// window exists, on the main thread.
pub fn try_enable(window: &dyn Window) -> bool {
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

        let effect = NSVisualEffectView::new(mtm);
        // behindWindow is the whole point: blur what is BEHIND the window, not
        // the window's own contents.
        effect.setBlendingMode(NSVisualEffectBlendingMode::BehindWindow);
        // Keep the glass alive when the window is not focused; the default
        // (FollowsWindowActiveState) dims it on deactivate, which reads as a
        // rendering bug in a terminal.
        effect.setState(NSVisualEffectState::Active);
        // Cover the whole frame view and keep covering it across resizes.
        effect.setFrame(frame_view.bounds());
        effect.setAutoresizingMask(
            NSAutoresizingMaskOptions::ViewWidthSizable | NSAutoresizingMaskOptions::ViewHeightSizable,
        );
        // Below the CONTENT view specifically: the terminal (winit's view, its
        // backing layer and wgpu's CAMetalLayer sublayer) stays above the glass,
        // and clicks still land on the content view first.
        frame_view.addSubview_positioned_relativeTo(&effect, NSWindowOrderingMode::Below, Some(&content));

        // winit's `with_transparent(true)` already does this; repeat it so the
        // module is correct on its own terms — an opaque window would have the
        // window server composite over the glass.
        ns_window.setOpaque(false);
    }
    log::info!("installed NSVisualEffectView frosted glass beneath the content view");
    true
}
