//! rt — a fast, tiling terminal emulator; a loose Rust port of Terminator.
//!
//! This file is the GL front-end and winit run-loop. It cannot be unit-tested
//! (it needs a display and a GPU), so the testable pieces live elsewhere:
//! layout in `rt-core`, the engine in `rt-engine`, the controller in
//! `rt-session`, keybindings in `rt-config`, and key translation in
//! `rt_app::input`. Here we only *wire* those together and drive the GPU.
//!
//! Flow per key press: winit event → `chord_from_winit` → keymap lookup. A hit
//! becomes an `Action` fed to `Session::apply`; a miss becomes PTY bytes via
//! `encode_key` fed to `Session::feed_input` (respecting broadcast mode).

mod blur; // best-effort KDE/KWin background-blur request (no-op elsewhere)
mod input; // (also re-exported by lib.rs for tests; declared here for the bin)
mod menu; // right-click context menu (Terminator-style)
mod render; // the GL glyph-atlas renderer

use std::num::NonZeroU32; // required by glutin's surface resize API
use std::time::{Duration, Instant}; // frame pacing for async PTY updates

use glutin::config::ConfigTemplateBuilder;
use glutin::context::ContextAttributesBuilder;
use glutin::display::GetGlDisplay;
use glutin::prelude::*; // brings the Gl* traits (make_current, get_proc_address, …)
use glutin::surface::{Surface, SurfaceAttributesBuilder, WindowSurface};
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::HasWindowHandle;
use winit::application::ApplicationHandler;
use winit::event::{ElementState, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{Window, WindowId};

use render::{Color, Renderer};
use rt_config::Keymap;
use rt_core::Rect;
use rt_engine::TermPane;
use rt_session::{Session, SessionEvent};

/// The concrete session type used by the app: real PTY panes, spawned by a
/// boxed closure (boxed so the `Session`'s factory type is nameable in a field).
type AppSession = Session<TermPane, Box<dyn FnMut(usize, usize) -> TermPane>>;

/// Everything that only exists once a window and GL context are created (which
/// happens on the first `resumed`). Kept in an `Option` on the `App` so we can
/// build it lazily and tear it down on suspend.
struct Active {
    window: Window,                       // the OS window
    surface: Surface<WindowSurface>,      // the GL drawing surface
    context: glutin::context::PossiblyCurrentContext, // the current GL context
    renderer: Renderer,                   // our glyph-atlas renderer
    session: AppSession,                  // layout + panes + focus + broadcast
    keymap: Keymap,                       // Terminator-style bindings
    mods: ModifiersState,                 // live modifier state (updated on change)
    settings: rt_config::Settings,        // window appearance (background opacity, …)
    mouse: (f32, f32),                    // last cursor position in physical pixels
    menu: Option<menu::Menu>,             // the open right-click context menu, if any
}

/// The winit application object. Holds only the font bytes until `resumed`
/// builds the `Active` state.
struct App {
    fonts: render::FontBlobs, // regular/bold/italic/bold-italic font byte chains
    active: Option<Active>,   // populated on first resume
}

/// Locate a monospace font (plus fallback fonts for coverage gaps) on the
/// system. rt does not ship fonts (to avoid bundling a binary in git); it probes
/// the usual Linux locations. Returns `[primary, fallback…]` bytes, or `None` if
/// no primary is found (the app then exits with a helpful message).
///
/// The fallbacks matter because the usual primary — DejaVu Sans Mono — lacks
/// some ranges (notably braille U+2800–U+28FF, used by `spiral_stress`). We add
/// TrueType fonts that DO cover them so the renderer can fall back per glyph.
fn load_fonts() -> Option<render::FontBlobs> {
    // Read every existing path in `paths` into a chain of byte blobs.
    let load = |paths: &[&str], label: &str| -> Vec<Vec<u8>> {
        let mut out = Vec::new();
        for p in paths {
            if let Ok(bytes) = std::fs::read(p) {
                log::info!("{label} font {p}"); // record each face we picked
                out.push(bytes);
            }
        }
        out
    };
    // Regular chain: a monospace primary (first match) then coverage fallbacks
    // for ranges DejaVu Sans Mono lacks (e.g. braille). TrueType only (fontdue
    // can't read CFF/OTF); the renderer skips any that fail to parse.
    let regular = load(
        &[
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
            "/usr/share/fonts/liberation/LiberationMono-Regular.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/noto/NotoSansMono-Regular.ttf",
            // coverage fallbacks (appended after the primary):
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/truetype/agave/agave-r-autohinted.ttf",
            "/usr/share/fonts/truetype/freefont/FreeMono.ttf",
            "/usr/share/fonts/truetype/noto/NotoSansSymbols2-Regular.ttf",
        ],
        "regular",
    );
    if regular.is_empty() {
        return None; // no primary → the app cannot render text
    }
    // Bold, italic, and bold-italic chains — all optional; each falls back to
    // the regular face when absent.
    let bold = load(
        &[
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Bold.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-Bold.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Bold.ttf",
        ],
        "bold",
    );
    let italic = load(
        &[
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-Oblique.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-Italic.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-Oblique.ttf",
        ],
        "italic",
    );
    let bold_italic = load(
        &[
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono-BoldOblique.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-BoldItalic.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans-BoldOblique.ttf",
        ],
        "bold-italic",
    );
    Some(render::FontBlobs { regular, bold, italic, bold_italic })
}

impl ApplicationHandler for App {
    /// Called when the app is (re)activated. On the first call we build the
    /// window, GL context, renderer, and session. Subsequent calls (after a
    /// suspend) are no-ops here because we keep the state alive.
    fn resumed(&mut self, event_loop: &ActiveEventLoop) {
        if self.active.is_some() {
            return; // already initialised; nothing to do on re-resume
        }

        // --- create the window and choose a GL config --------------------
        let window_attrs = Window::default_attributes()
            .with_title("rt") // window title; per-pane titles update it later
            .with_transparent(true) // REQUIRED for the compositor to honour our alpha
            .with_inner_size(winit::dpi::LogicalSize::new(960.0, 600.0)); // sensible default
        let template = ConfigTemplateBuilder::new().with_alpha_size(8); // want an alpha channel
        // DisplayBuilder creates the window AND enumerates GL configs together,
        // which is the supported winit-0.30/glutin-0.32 pattern.
        let display_builder = DisplayBuilder::new().with_window_attributes(Some(window_attrs));
        let (window, gl_config) = match display_builder.build(event_loop, template, |configs| {
            // Pick a config that HAS an alpha channel first (needed for a
            // transparent window), then, among equal alpha, the most samples.
            configs
                .reduce(|a, b| {
                    let better_alpha = b.alpha_size() > a.alpha_size(); // prefer any alpha
                    let same_more_samples = b.alpha_size() == a.alpha_size() && b.num_samples() > a.num_samples();
                    if better_alpha || same_more_samples { b } else { a }
                })
                .expect("at least one GL config")
        }) {
            Ok((Some(window), config)) => (window, config), // got a window + config
            Ok((None, _)) => {
                log::error!("window creation returned no window");
                event_loop.exit();
                return;
            }
            Err(e) => {
                log::error!("failed to create window/GL config: {e}");
                event_loop.exit();
                return;
            }
        };

        // --- create the GL context and surface ---------------------------
        let gl_display = gl_config.display(); // the platform GL display
        // Raw handle needed to bind the context to this specific window.
        let raw_handle = window.window_handle().ok().map(|h| h.as_raw());
        let context_attrs = ContextAttributesBuilder::new().build(raw_handle);
        // Create a not-yet-current context, then a surface, then make current.
        let not_current = match unsafe { gl_display.create_context(&gl_config, &context_attrs) } {
            Ok(c) => c,
            Err(e) => {
                log::error!("GL context creation failed: {e}");
                event_loop.exit();
                return;
            }
        };
        // Build the window surface at the window's current size.
        let attrs = window.build_surface_attributes(SurfaceAttributesBuilder::new());
        let attrs = match attrs {
            Ok(a) => a,
            Err(e) => {
                log::error!("surface attributes failed: {e}");
                event_loop.exit();
                return;
            }
        };
        let surface = match unsafe { gl_display.create_window_surface(&gl_config, &attrs) } {
            Ok(s) => s,
            Err(e) => {
                log::error!("GL surface creation failed: {e}");
                event_loop.exit();
                return;
            }
        };
        // Make the context current on the surface so GL calls target it.
        let context = match not_current.make_current(&surface) {
            Ok(c) => c,
            Err(e) => {
                log::error!("make_current failed: {e}");
                event_loop.exit();
                return;
            }
        };

        // --- load GL function pointers into glow -------------------------
        let gl = unsafe {
            glow::Context::from_loader_function_cstr(|s| gl_display.get_proc_address(s).cast())
        };

        // --- build the renderer ------------------------------------------
        let mut renderer = match Renderer::new(gl, &self.fonts, 18.0) {
            Ok(r) => r,
            Err(e) => {
                log::error!("renderer init failed: {e}");
                event_loop.exit();
                return;
            }
        };
        // Ask KWin to blur behind us (true background blur on KDE). No-op on
        // COSMIC/GNOME/sway, where the portable scrim does the job instead.
        blur::try_enable_kwin_blur(&window);

        // Size the renderer/viewport to the window's physical pixels.
        let size = window.inner_size(); // physical pixel size
        renderer.resize(size.width as f32, size.height as f32);
        let cell = renderer.cell_size(); // (cell_w, cell_h) in pixels

        // --- build the session with real PTY panes -----------------------
        let bounds = Rect::new(0.0, 0.0, size.width as f32, size.height as f32);
        // Optional demo/verification hook: RT_EXEC runs a command in every new
        // pane before dropping to an interactive shell. Handy for screenshots
        // (e.g. RT_EXEC='seq 1 200') and a stepping-stone toward saved layouts.
        let exec = std::env::var("RT_EXEC").ok();
        // The factory spawns a shell-backed pane at the requested cell size. A
        // spawn failure here is fatal (no PTY available) — we surface it by
        // panicking with a clear message rather than rendering a broken pane.
        let spawn: Box<dyn FnMut(usize, usize) -> TermPane> = Box::new(move |cols, rows| {
            // If RT_EXEC is set, run it then keep an interactive shell open.
            let shell = exec.as_ref().map(|cmd| {
                ("/bin/sh".to_string(), vec!["-c".to_string(), format!("{cmd}; exec /bin/sh -i")])
            });
            TermPane::spawn(shell, None, cols.max(1), rows.max(1))
                .expect("failed to spawn a PTY/shell for a pane")
        });
        let mut session = Session::new(bounds, cell, spawn);

        // Dev/demo startup layout (seed of the future saved-layouts feature):
        //   RT_SPLIT=h|v    → perform one split at startup
        //   RT_COLUMNS=N    → put the initial pane into N-column newspaper mode
        if let Ok(v) = std::env::var("RT_SPLIT") {
            let _ = match v.as_str() {
                "h" => session.apply(rt_config::Action::SplitHoriz), // stacked split
                "v" => session.apply(rt_config::Action::SplitVert),  // side-by-side split
                _ => None,
            };
        }
        if let Ok(v) = std::env::var("RT_COLUMNS") {
            if let Ok(n) = v.parse::<u16>() {
                // Each ColumnsMore adds one column; go from 1 up to N.
                for _ in 1..n.max(1) {
                    session.apply(rt_config::Action::ColumnsMore);
                }
            }
        }
        if let Ok(v) = std::env::var("RT_TABS") {
            if let Ok(n) = v.parse::<u16>() {
                // Open N tabs total (each NewTab adds one beside the current).
                for _ in 1..n.max(1) {
                    session.apply(rt_config::Action::NewTab);
                }
            }
        }

        // Appearance settings. RT_OPACITY (0.05..=1.0) seeds the background
        // opacity for demos/screenshots; a preferences panel will edit it later.
        let mut settings = rt_config::Settings::default(); // opaque by default
        if let Ok(v) = std::env::var("RT_OPACITY") {
            if let Ok(o) = v.parse::<f32>() {
                settings.background_opacity = o.clamp(rt_config::Settings::MIN_OPACITY, 1.0); // clamp to usable range
            }
        }
        if let Ok(v) = std::env::var("RT_SCRIM") {
            if let Ok(s) = v.parse::<f32>() {
                settings.scrim_strength = s.clamp(0.0, rt_config::Settings::MAX_SCRIM); // clamp scrim range
            }
        }
        // RT_FOCUS=sloppy|mouse|follow|1 enables focus-follows-mouse at startup.
        if let Ok(v) = std::env::var("RT_FOCUS") {
            settings.focus_follows_mouse = matches!(v.as_str(), "sloppy" | "mouse" | "follow" | "1");
        }

        // Store the fully-initialised state and paint once.
        self.active = Some(Active {
            window,
            surface,
            context,
            renderer,
            session,
            keymap: Keymap::defaults(),
            mods: ModifiersState::empty(),
            settings,
            mouse: (0.0, 0.0),
            menu: None,
        });
        // Debug/verification hook: RT_MENU opens the context menu at startup so
        // its rendering can be screenshotted without synthetic mouse input.
        if std::env::var("RT_MENU").is_ok() {
            if let Some(active) = self.active.as_mut() {
                let (cw, ch) = active.renderer.cell_size();
                let size = active.window.inner_size();
                let mut m = menu::Menu::new(200.0, 150.0); // fixed, visible spot
                m.clamp(size.width as f32, size.height as f32, cw, ch);
                active.menu = Some(m);
            }
        }
        // Poll so we keep re-checking PTYs for async output even without input.
        event_loop.set_control_flow(ControlFlow::Poll);
        if let Some(active) = &self.active {
            active.window.request_redraw(); // first paint
        }
    }

    /// Handle a window event: close, resize, key input, redraw.
    fn window_event(&mut self, event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Everything here needs the active state; ignore events before resume.
        let Some(active) = self.active.as_mut() else { return };

        match event {
            // The user closed the window.
            WindowEvent::CloseRequested => event_loop.exit(),

            // Track modifier state so key events can build correct chords.
            WindowEvent::ModifiersChanged(new_mods) => {
                active.mods = new_mods.state(); // remember Ctrl/Shift/Alt/Super
            }

            // Window resized: resize the GL surface, viewport, and relayout PTYs.
            WindowEvent::Resized(size) => {
                // glutin needs non-zero dimensions to resize the surface.
                if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
                    active.surface.resize(&active.context, w, h); // resize GL surface
                }
                active.renderer.resize(size.width as f32, size.height as f32); // viewport
                // Recompute pane sizes from the new window bounds.
                let bounds = Rect::new(0.0, 0.0, size.width as f32, size.height as f32);
                active.session.relayout(bounds); // push new sizes to PTYs
                active.window.request_redraw(); // repaint at the new size
            }

            // A key was pressed or released.
            WindowEvent::KeyboardInput { event: key_event, .. } => {
                // Only act on presses (ignore auto-repeat=false-positives and releases).
                if key_event.state != ElementState::Pressed {
                    return;
                }
                self.on_key_press(key_event);
            }

            // Mouse wheel scrolls the focused pane's newspaper-column view
            // through history (positive y = up = toward older content).
            WindowEvent::MouseWheel { delta, .. } => {
                // Normalise both delta kinds to a signed line count.
                let lines = match delta {
                    MouseScrollDelta::LineDelta(_, y) => y as isize, // notch-based devices
                    MouseScrollDelta::PixelDelta(p) => (p.y / 20.0) as isize, // touchpads (~20px/line)
                };
                if lines != 0 {
                    let focus = active.session.focus(); // scroll the focused pane
                    // Drive the terminal's own scrollback; in column mode the
                    // whole tall viewport shifts, giving the cross-column flow.
                    if let Some(pane) = active.session.pane(focus) {
                        pane.scroll(lines); // &self method: locks the Term internally
                    }
                    active.window.request_redraw(); // repaint at the new offset
                }
            }

            // Track the cursor; when a menu is open, update its hover highlight.
            WindowEvent::CursorMoved { position, .. } => {
                active.mouse = (position.x as f32, position.y as f32); // physical px
                if let Some(m) = active.menu.as_mut() {
                    let (cw, ch) = active.renderer.cell_size(); // cell metrics for hit layout
                    if m.set_hover(active.mouse.0, active.mouse.1, cw, ch) {
                        active.window.request_redraw(); // hovered row changed
                    }
                } else if active.settings.focus_follows_mouse {
                    // Sloppy focus: focus the pane under the pointer. Only redraw
                    // when focus actually changes (not on every motion), and only
                    // when a pane is hit (over a gutter, focus sticks).
                    let before = active.session.focus();
                    active.session.focus_at(active.mouse.0, active.mouse.1);
                    if active.session.focus() != before {
                        active.window.request_redraw(); // repaint the focus border
                    }
                }
            }

            // Mouse button presses: right opens the context menu; left either
            // activates a menu item (if open) or is ignored for now.
            WindowEvent::MouseInput { state: ElementState::Pressed, button, .. } => match button {
                MouseButton::Right => {
                    log::debug!("right-click at {:?} → open menu", active.mouse);
                    // Focus the pane under the cursor first, so the menu's
                    // actions (Split, Close, Columns…) apply to the pane you
                    // right-clicked — not whichever pane happened to be focused.
                    active.session.focus_at(active.mouse.0, active.mouse.1);
                    // Open the menu at the cursor, clamped inside the window.
                    let (cw, ch) = active.renderer.cell_size();
                    let size = active.window.inner_size();
                    let mut m = menu::Menu::new(active.mouse.0, active.mouse.1);
                    m.clamp(size.width as f32, size.height as f32, cw, ch);
                    active.menu = Some(m);
                    active.window.request_redraw();
                }
                MouseButton::Left => {
                    if let Some(m) = active.menu.take() {
                        // A menu is open: a click anywhere closes it; if it
                        // landed on an item, run that item's action.
                        let (cw, ch) = active.renderer.cell_size();
                        let action = m.hit(active.mouse.0, active.mouse.1, cw, ch).and_then(|i| m.action_at(i));
                        active.window.request_redraw();
                        if let Some(a) = action {
                            Self::apply_action(active, a); // may exit on the last pane
                        }
                    } else {
                        // No menu. A click on a tab label selects that tab;
                        // otherwise it click-to-focuses the pane under the cursor.
                        let size = active.window.inner_size();
                        let bounds = Rect::new(0.0, 0.0, size.width as f32, size.height as f32);
                        let (mx, my) = active.mouse;
                        // Find a tab whose label rect contains the click.
                        let clicked_tab = active
                            .session
                            .tab_bars(bounds)
                            .into_iter()
                            .flat_map(|bar| bar.tabs)
                            .find(|t| t.rect.contains(mx, my))
                            .map(|t| t.first_pane);
                        if let Some(first_pane) = clicked_tab {
                            active.session.focus_tab(first_pane); // switch to that tab
                            active.window.request_redraw();
                        } else if active.session.focus_at(mx, my) {
                            active.window.request_redraw(); // repaint the focus border
                        }
                    }
                }
                _ => {} // middle/other buttons: nothing yet
            },

            // Time to paint.
            WindowEvent::RedrawRequested => {
                self.redraw();
            }

            _ => {} // ignore the many other window events for now
        }
    }

    /// Called whenever the loop is about to block. We use it to poll each pane
    /// for asynchronous PTY output and request a redraw when anything changed,
    /// so terminal output appears without the user touching the keyboard.
    fn about_to_wait(&mut self, event_loop: &ActiveEventLoop) {
        let Some(active) = self.active.as_mut() else { return };
        // Drain events from every live pane. Output/title/bell means repaint; a
        // pane whose shell exited is collected for closing after the loop (we
        // can't mutate the session while iterating its panes).
        let mut dirty = false; // did anything change this tick?
        let mut exited: Vec<rt_core::PaneId> = Vec::new(); // panes whose shell died
        for id in active.session.tree().all_panes() {
            if let Some(pane) = active.session.pane(id) {
                for ev in pane.drain_events() {
                    match ev {
                        rt_engine::PaneEvent::Exited => exited.push(id), // reap this pane
                        _ => dirty = true, // Wakeup/Title/Bell → needs a redraw
                    }
                }
            }
        }
        // Close every pane whose child exited. If that empties the window, quit.
        for id in exited {
            match active.session.close_pane(id) {
                Some(SessionEvent::CloseWindow) => {
                    self.active = None; // drop everything (PTYs shut down on Drop)
                    std::process::exit(0); // clean exit
                }
                _ => dirty = true, // a pane closed; repaint the survivors
            }
        }
        if dirty {
            active.window.request_redraw(); // schedule a paint
        }
        // Re-check about every 16ms (~60fps) even when idle, so async output is
        // picked up promptly without a hot busy-loop.
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + Duration::from_millis(16)));
    }
}

impl App {
    /// Run a semantic [`Action`](rt_config::Action) against the live state. This
    /// is the single place actions are executed, called both by keybindings and
    /// by the context menu, so the two can never drift apart.
    ///
    /// Window-level appearance actions (opacity/scrim) are handled here because
    /// the session owns no window handle; everything else goes to the session.
    /// A `CloseWindow` result exits the process (the OS reaps the child PTYs).
    fn apply_action(active: &mut Active, action: rt_config::Action) {
        use rt_config::Action;
        match action {
            Action::OpacityUp => {
                let v = active.settings.adjust_opacity(0.05); // +5% opaque
                log::info!("background opacity = {v:.2}");
                active.window.request_redraw();
            }
            Action::OpacityDown => {
                let v = active.settings.adjust_opacity(-0.05); // more see-through
                log::info!("background opacity = {v:.2}");
                active.window.request_redraw();
            }
            Action::ScrimUp => {
                let v = active.settings.adjust_scrim(0.05); // stronger wash
                log::info!("scrim strength = {v:.2}");
                active.window.request_redraw();
            }
            Action::ScrimDown => {
                let v = active.settings.adjust_scrim(-0.05); // weaker wash
                log::info!("scrim strength = {v:.2}");
                active.window.request_redraw();
            }
            Action::ToggleFocusFollowsMouse => {
                active.settings.focus_follows_mouse = !active.settings.focus_follows_mouse; // flip mode
                log::info!("focus-follows-mouse = {}", active.settings.focus_follows_mouse);
                active.window.request_redraw();
            }
            // Everything else is a session action.
            other => match active.session.apply(other) {
                Some(SessionEvent::CloseWindow) => std::process::exit(0), // last pane closed
                Some(SessionEvent::Redraw) => active.window.request_redraw(),
                // No-op or clipboard (not yet wired to the OS): nothing to do.
                _ => {}
            },
        }
    }

    /// Handle one key *press*: close an open menu on Escape, otherwise translate
    /// to a chord and either run the bound action or type the key into the
    /// focused PTY(s).
    fn on_key_press(&mut self, key_event: winit::event::KeyEvent) {
        let Some(active) = self.active.as_mut() else { return };
        // Escape dismisses an open context menu (and is then consumed).
        if active.menu.is_some() && matches!(key_event.logical_key, Key::Named(NamedKey::Escape)) {
            active.menu = None;
            active.window.request_redraw();
            return;
        }
        let mods = active.mods; // current modifier state
        // Is this chord bound to an rt action?
        if let Some(chord) = input::chord_from_winit(&key_event.logical_key, mods) {
            if let Some(action) = active.keymap.action_for(&chord) {
                Self::apply_action(active, action); // shared with the menu
                return; // consumed
            }
        }
        // Not a binding: treat as ordinary typing. Encode to PTY bytes and feed
        // the focused pane (or the broadcast set). We consult the focused pane's
        // application-cursor-keys mode so arrows are encoded the way the running
        // program (mc/vim/…) expects.
        let app_cursor = active
            .session
            .pane(active.session.focus()) // the focused pane's backend
            .map(|p| p.app_cursor_keys()) // its DECCKM state
            .unwrap_or(false); // default to normal cursor keys
        if let Some(bytes) = input::encode_key(&key_event.logical_key, mods, app_cursor) {
            active.session.feed_input(&bytes); // send to the shell(s)
        }
    }

    /// Repaint the whole window: fill each pane's background, draw its visible
    /// grid, then outline the focused pane. Finally swap buffers.
    fn redraw(&mut self) {
        let Some(active) = self.active.as_mut() else { return };
        // Terminal colours (a dark theme): near-black bg, light-grey fg.
        // The background carries the user's opacity in its alpha channel, so a
        // value < 1.0 makes empty areas translucent (the window(s) behind show
        // through, compositor permitting). Glyphs and chrome stay fully opaque.
        let bg = Color::rgb(0x10, 0x10, 0x14).with_alpha(active.settings.background_opacity);
        let focus_border = Color::rgb(0x4a, 0x90, 0xd9); // blue focus outline (opaque)

        let size = active.window.inner_size(); // physical pixels
        let bounds = Rect::new(0.0, 0.0, size.width as f32, size.height as f32);
        // Clear to the (possibly translucent) background. This IS the pane
        // background — we no longer draw an opaque per-pane fill, which under
        // translucency would double-blend and darken the see-through areas.
        active.renderer.begin_frame(bg); // translucent clear

        // Scrim: a neutral wash over the whole window, drawn FIRST (behind all
        // text), that compresses the contrast of whatever shows through the
        // translucent background — rt's portable stand-in for background blur.
        // A mid-neutral tone is used so it washes out legibility faster than it
        // hides gross shapes/motion. At strength 0 this is a no-op.
        let scrim = active.settings.scrim_strength; // 0.0 = off
        if scrim > 0.0 {
            let wash = Color::rgb(0x50, 0x50, 0x58).with_alpha(scrim); // mid neutral at the chosen strength
            active.renderer.fill_rect(0.0, 0.0, size.width as f32, size.height as f32, wash);
        }

        let focus = active.session.focus(); // which pane is focused
        let (cell_w, cell_h) = active.renderer.cell_size(); // px per cell
        let sep = Color::rgb(0x2a, 0x2a, 0x33); // subtle inter-column separator colour
        // Draw every visible pane. (No per-pane background fill: the translucent
        // clear above already is the background.)
        for (id, rect) in active.session.tree().rects(bounds) {
            let n = active.session.columns_of(id); // newspaper column count (1 = normal)
            // Copy the pane's current grid (glyphs + resolved colours + cursor).
            if let Some(pane) = active.session.pane(id) {
                let snap = pane.snapshot(); // in column mode this is a count*rows-tall screen
                let geom = active.session.column_layout(id, rect); // count/col_cells/rows/gap
                // One column's height in rows. For a single pane the whole
                // snapshot stacks directly, so we use a sentinel that keeps the
                // mapping a no-op.
                let per_col = if n <= 1 { usize::MAX } else { geom.rows.max(1) };
                let step = (geom.col_cells + geom.gap) as f32 * cell_w; // px between column origins

                // Map a snapshot row `r` to (its column's origin x, its sub-row).
                // Single mode: rows stack at rect.x. Column mode: row r lands in
                // newspaper column r/per_col at sub-row r%per_col.
                let place = |r: usize| -> (f32, usize) {
                    if n <= 1 {
                        (rect.x, r) // stack directly
                    } else {
                        (rect.x + (r / per_col) as f32 * step, r % per_col) // tile into columns
                    }
                };

                // Draw each cell: an opaque background quad only when the cell's
                // background differs from the default (so ordinary text keeps the
                // translucent window background), then the glyph in its colour.
                for (r, row) in snap.rows.iter().enumerate() {
                    if n > 1 && r / per_col >= geom.count as usize {
                        break; // guard against a transient over-tall snapshot mid-resize
                    }
                    let (ox, sub) = place(r); // where this line draws
                    for (col_idx, cell) in row.iter().enumerate() {
                        if cell.bg != rt_engine::DEFAULT_BG {
                            let c = cell.bg; // per-cell background colour
                            active.renderer.fill_cell(ox, rect.y, col_idx, sub, Color::rgb(c[0], c[1], c[2]));
                        }
                        let fg = Color::rgb(cell.fg[0], cell.fg[1], cell.fg[2]); // per-cell foreground
                        if cell.c != ' ' {
                            // Glyph, in the bold/oblique face per the cell's attributes.
                            active.renderer.draw_char(ox, rect.y, col_idx, sub, cell.c, fg, cell.attrs.bold, cell.attrs.italic);
                        }
                        // Text-attribute lines (drawn even on blank cells so an
                        // underlined space still shows a rule).
                        if cell.attrs.underline {
                            active.renderer.draw_underline(ox, rect.y, col_idx, sub, fg);
                        }
                        if cell.attrs.strikeout {
                            active.renderer.draw_strikeout(ox, rect.y, col_idx, sub, fg);
                        }
                    }
                }

                // Cursor: shape depends on what the app requested (block/
                // underline/beam) and on focus — an UNFOCUSED pane always shows a
                // hollow outline (your request). A focused solid block is filled
                // and the glyph under it is redrawn in the cell background so it
                // stays legible; the other shapes sit over the normal glyph.
                if let Some(cur) = snap.cursor {
                    let in_range = cur.line < snap.rows.len() && (n <= 1 || cur.line / per_col < geom.count as usize);
                    if in_range {
                        use rt_engine::CursorShape;
                        let (ox, sub) = place(cur.line); // cursor's on-screen slot
                        let cc = rt_engine::CURSOR; // cursor colour
                        let ccol = Color::rgb(cc[0], cc[1], cc[2]);
                        let focused = id == focus; // is this the focused pane?
                        if !focused {
                            // Unfocused: hollow outline regardless of shape.
                            active.renderer.cursor_hollow(ox, rect.y, cur.col, sub, ccol);
                        } else {
                            match cur.shape {
                                CursorShape::Block => {
                                    // Solid block + inverse glyph for contrast.
                                    active.renderer.fill_cell(ox, rect.y, cur.col, sub, ccol);
                                    if let Some(u) = snap.rows.get(cur.line).and_then(|rw| rw.get(cur.col)) {
                                        if u.c != ' ' {
                                            let ub = u.bg;
                                            active.renderer.draw_char(ox, rect.y, cur.col, sub, u.c, Color::rgb(ub[0], ub[1], ub[2]), u.attrs.bold, u.attrs.italic);
                                        }
                                    }
                                }
                                CursorShape::HollowBlock => active.renderer.cursor_hollow(ox, rect.y, cur.col, sub, ccol),
                                CursorShape::Underline => active.renderer.cursor_underline(ox, rect.y, cur.col, sub, ccol),
                                CursorShape::Beam => active.renderer.cursor_beam(ox, rect.y, cur.col, sub, ccol),
                                CursorShape::Hidden => {} // nothing (snapshot already filters, but be safe)
                            }
                        }
                    }
                }

                // Thin separators between newspaper columns, drawn in each gap.
                if n > 1 {
                    for nc in 1..geom.count as usize {
                        let x = rect.x + nc as f32 * step - (geom.gap as f32 * 0.5) * cell_w; // gap centre
                        active.renderer.fill_rect(x, rect.y, 1.0, rect.h, sep); // 1px vertical rule
                    }
                }
            }
            // Outline the focused pane with a thin border (four thin rects).
            if id == focus {
                let t = 2.0; // border thickness in pixels
                active.renderer.fill_rect(rect.x, rect.y, rect.w, t, focus_border); // top
                active.renderer.fill_rect(rect.x, rect.bottom() - t, rect.w, t, focus_border); // bottom
                active.renderer.fill_rect(rect.x, rect.y, t, rect.h, focus_border); // left
                active.renderer.fill_rect(rect.right() - t, rect.y, t, rect.h, focus_border); // right
            }
        }

        // Tab strips: draw each visible Tabs node's bar in its reserved region
        // (above the active tab's content, which the pane loop already drew).
        let tab_bg = Color::rgb(0x14, 0x14, 0x1a); // inactive tab background
        let tab_active = Color::rgb(0x2e, 0x2e, 0x38); // active tab background
        let tab_line = Color::rgb(0x30, 0x30, 0x38); // separators
        let txt_on = Color::rgb(0xe0, 0xe0, 0xe8); // active label colour
        let txt_off = Color::rgb(0x88, 0x88, 0x92); // inactive label colour
        for bar in active.session.tab_bars(bounds) {
            for tab in &bar.tabs {
                let r = tab.rect; // this tab's strip rectangle
                // Segment background (active tab stands out).
                active.renderer.fill_rect(r.x, r.y, r.w, r.h, if tab.active { tab_active } else { tab_bg });
                // Label: the tab number, vertically centred, left-padded.
                let label = tab.number.to_string(); // "1", "2", …
                let text_top = r.y + (r.h - cell_h) * 0.5; // centre the glyph line
                let col = if tab.active { txt_on } else { txt_off };
                for (i, ch) in label.chars().enumerate() {
                    active.renderer.draw_char(r.x + 8.0, text_top, i, 0, ch, col, tab.active, false);
                }
                // Right separator between tabs.
                active.renderer.fill_rect(r.right() - 1.0, r.y, 1.0, r.h, tab_line);
            }
        }

        // The context menu draws last so it sits above the terminal content.
        if let Some(m) = active.menu.as_ref() {
            let (cw, ch) = active.renderer.cell_size(); // cell metrics for menu layout
            m.draw(&mut active.renderer, cw, ch); // panel + rows + hover highlight
        }

        active.renderer.end_frame(); // upload + draw call
        // Present the frame.
        if let Err(e) = active.surface.swap_buffers(&active.context) {
            log::error!("swap_buffers failed: {e}"); // non-fatal; log and continue
        }
    }
}

/// Program entry point: set up logging, load a font, and run the winit loop.
fn main() {
    env_logger::init(); // honour RUST_LOG for diagnostics
    // A font is mandatory; fail early with guidance if none is installed.
    let fonts = match load_fonts() {
        Some(f) => f,
        None => {
            eprintln!(
                "rt: no monospace font found. Install e.g. 'fonts-dejavu-core' \
                 (Debian/Ubuntu) or 'ttf-dejavu' (Arch) and retry."
            );
            std::process::exit(1);
        }
    };
    // Build the winit event loop and hand it our application.
    let event_loop = EventLoop::new().expect("failed to create event loop");
    let mut app = App { fonts, active: None };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("rt: event loop error: {e}"); // surface any run-loop failure
        std::process::exit(1);
    }
}
