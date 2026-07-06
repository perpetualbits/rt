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
mod preferences; // egui preferences dialog
mod render; // the GL glyph-atlas renderer

use std::num::NonZeroU32; // required by glutin's surface resize API
use std::time::{Duration, Instant}; // frame pacing for async PTY updates

use glutin::config::ConfigTemplateBuilder;
use glutin::context::ContextAttributesBuilder;
use glutin::display::GetGlDisplay;
use glutin::prelude::*; // brings the Gl* traits (make_current, get_proc_address, …)
use glutin::surface::{Surface, SurfaceAttributesBuilder, WindowSurface};
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle, RawDisplayHandle};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
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
    ime_preedit: bool,                    // true while an IME/dead-key composition is in progress
    clipboard: Option<smithay_clipboard::Clipboard>, // Wayland clipboard + PRIMARY (None on x11 dev builds)
    selection: Option<Selection>,         // the current mouse text selection, if any
    selecting: bool,                      // true while the left button is held for a drag-select
    egui_ctx: egui::Context,              // egui immediate-mode context (chrome/dialogs)
    egui_state: egui_winit::State,        // egui-winit input bridge
    egui_painter: egui_glow::Painter,     // egui_glow renderer (shares our GL context)
    prefs_open: bool,                     // whether the preferences dialog is showing
    palette: std::sync::Arc<std::sync::Mutex<rt_engine::Palette>>, // shared so new panes inherit current colours
    font_db: std::sync::Arc<fontdb::Database>, // for reloading fonts on a family change
    font_blobs: render::FontBlobs,        // the current font chains (kept so a size change can reload)
    mono_families: Vec<String>,           // monospace family names for the preferences picker
}

/// A rectangular-by-lines text selection within one pane, in that pane's grid
/// cell coordinates. `anchor` is where the drag started, `head` is the current
/// end; text runs linearly (row-major) between the two.
#[derive(Clone, Copy)]
struct Selection {
    pane: rt_core::PaneId, // the pane the selection lives in
    anchor: (usize, usize), // (col, row) where the drag began
    head: (usize, usize),   // (col, row) current end of the drag
}

impl Selection {
    /// Return the selection's endpoints ordered so `start` precedes `end` in
    /// row-major reading order (top-to-bottom, left-to-right).
    fn ordered(&self) -> ((usize, usize), (usize, usize)) {
        // Compare by (row, col) so multi-line selections read correctly.
        let a = (self.anchor.1, self.anchor.0); // (row, col)
        let b = (self.head.1, self.head.0);
        if a <= b {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Whether cell `(col, row)` falls inside the selection.
    fn contains(&self, col: usize, row: usize) -> bool {
        let (start, end) = self.ordered(); // start precedes end
        let (sc, sr) = start;
        let (ec, er) = end;
        if row < sr || row > er {
            return false; // outside the row span
        }
        // First and last rows are bounded by the columns; middle rows are full.
        let lo = if row == sr { sc } else { 0 };
        let hi = if row == er { ec } else { usize::MAX };
        col >= lo && col <= hi
    }
}

/// The winit application object. Holds only the font bytes until `resumed`
/// builds the `Active` state.
struct App {
    font_db: std::sync::Arc<fontdb::Database>, // system fonts, for family lookup + the picker
    mono_families: Vec<String>,                // monospace family names for the preferences combo
    active: Option<Active>,                    // populated on first resume
}

/// Build the system font database (scans the usual font directories).
fn build_font_db() -> fontdb::Database {
    let mut db = fontdb::Database::new();
    db.load_system_fonts();
    db
}

/// The sorted, de-duplicated names of every monospace family in the database —
/// what the preferences font picker offers.
fn monospace_families(db: &fontdb::Database) -> Vec<String> {
    let mut names = std::collections::BTreeSet::new();
    for face in db.faces() {
        if face.monospaced {
            if let Some((name, _)) = face.families.first() {
                names.insert(name.clone());
            }
        }
    }
    names.into_iter().collect()
}

/// Fetch the raw bytes of the `family` face nearest the given weight/style, or
/// `None` if the family isn't installed.
fn face_data(db: &fontdb::Database, family: &str, weight: fontdb::Weight, style: fontdb::Style) -> Option<Vec<u8>> {
    let id = db.query(&fontdb::Query {
        families: &[fontdb::Family::Name(family)],
        weight,
        stretch: fontdb::Stretch::Normal,
        style,
    })?;
    db.with_face_data(id, |data, _index| data.to_vec())
}

/// Build the four font chains (regular/bold/italic/bold-italic) for `family`,
/// each with coverage fallbacks appended to the regular chain (DejaVu Sans etc.
/// cover braille that DejaVu Sans Mono lacks). Falls back to a path search if
/// the database yields no usable primary.
fn font_blobs(db: &fontdb::Database, family: &str) -> render::FontBlobs {
    use fontdb::{Style, Weight};
    // Primary regular face: the chosen family, then sensible monospace fallbacks.
    let primary = face_data(db, family, Weight::NORMAL, Style::Normal)
        .or_else(|| face_data(db, "DejaVu Sans Mono", Weight::NORMAL, Style::Normal))
        .or_else(|| face_data(db, "monospace", Weight::NORMAL, Style::Normal));
    let mut regular = Vec::new();
    if let Some(d) = primary {
        regular.push(d);
    }
    // Coverage fallbacks (braille, symbols) appended after the primary.
    for fam in ["DejaVu Sans", "Noto Sans Symbols2", "FreeMono"] {
        if let Some(d) = face_data(db, fam, Weight::NORMAL, Style::Normal) {
            regular.push(d);
        }
    }
    // Nothing at all from the DB → last-resort path loader.
    if regular.is_empty() {
        if let Some(fb) = load_fonts() {
            return fb;
        }
    }
    render::FontBlobs {
        regular,
        bold: face_data(db, family, Weight::BOLD, Style::Normal).into_iter().collect(),
        italic: face_data(db, family, Weight::NORMAL, Style::Italic).into_iter().collect(),
        bold_italic: face_data(db, family, Weight::BOLD, Style::Italic).into_iter().collect(),
    }
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
        // Wrapped in an Arc so the terminal renderer and egui_glow's painter
        // share one live GL context (ADR-0004).
        let gl = std::sync::Arc::new(unsafe {
            glow::Context::from_loader_function_cstr(|s| gl_display.get_proc_address(s).cast())
        });

        // Load persisted settings (before the renderer, so fonts/colours come
        // from the config). Env vars override for demos/screenshots.
        let mut settings = rt_config::Config::load().settings;
        if let Ok(v) = std::env::var("RT_OPACITY") {
            if let Ok(o) = v.parse::<f32>() {
                settings.background_opacity = o.clamp(rt_config::Settings::MIN_OPACITY, 1.0);
            }
        }
        if let Ok(v) = std::env::var("RT_SCRIM") {
            if let Ok(s) = v.parse::<f32>() {
                settings.scrim_strength = s.clamp(0.0, rt_config::Settings::MAX_SCRIM);
            }
        }
        if let Ok(v) = std::env::var("RT_FOCUS") {
            settings.focus_follows_mouse = matches!(v.as_str(), "sloppy" | "mouse" | "follow" | "1");
        }

        // Font chains for the configured family (system fonts via fontdb). Kept
        // in `Active` so a live font change can reload them.
        let font_blobs = font_blobs(&self.font_db, &settings.font_family);

        // --- build the renderer ------------------------------------------
        let mut renderer = match Renderer::new(gl.clone(), &font_blobs, settings.font_size) {
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

        // Enable IME so dead keys / compose sequences (´+o→ó, ~+n→ñ, …) and full
        // IMEs work: composed text arrives via WindowEvent::Ime(Commit).
        window.set_ime_allowed(true);

        // Wayland clipboard (+ PRIMARY selection), tied to the window's display.
        // None on an x11 dev build (smithay-clipboard is Wayland-only).
        // SAFETY: the display pointer comes from winit's live Wayland display.
        let clipboard = match window.display_handle().map(|h| h.as_raw()) {
            Ok(RawDisplayHandle::Wayland(d)) => {
                Some(unsafe { smithay_clipboard::Clipboard::new(d.display.as_ptr()) })
            }
            _ => None, // not Wayland (x11 dev build): clipboard unavailable
        };

        // Size the renderer/viewport to the window's physical pixels.
        let size = window.inner_size(); // physical pixel size
        renderer.resize(size.width as f32, size.height as f32);
        let cell = renderer.cell_size(); // (cell_w, cell_h) in pixels

        // --- build the session with real PTY panes -----------------------
        let bounds = Rect::new(0.0, 0.0, size.width as f32, size.height as f32);
        // RT_EXEC runs a command in every new pane before dropping to an
        // interactive shell (handy for screenshots / demos).
        let exec = std::env::var("RT_EXEC").ok();
        // Shared colour palette (built from the configured colours). Every new
        // pane picks up the *current* palette, so a colour-scheme change applies
        // to panes created afterwards too.
        let palette = std::sync::Arc::new(std::sync::Mutex::new(rt_engine::Palette::new(
            settings.foreground,
            settings.background,
            settings.palette,
        )));
        let palette_spawn = palette.clone();
        // The factory spawns a shell-backed pane at the requested cell size. A
        // spawn failure here is fatal (no PTY available) — we surface it by
        // panicking with a clear message rather than rendering a broken pane.
        let spawn: Box<dyn FnMut(usize, usize) -> TermPane> = Box::new(move |cols, rows| {
            // If RT_EXEC is set, run it then keep an interactive shell open.
            let shell = exec.as_ref().map(|cmd| {
                ("/bin/sh".to_string(), vec!["-c".to_string(), format!("{cmd}; exec /bin/sh -i")])
            });
            let mut pane = TermPane::spawn(shell, None, cols.max(1), rows.max(1))
                .expect("failed to spawn a PTY/shell for a pane");
            if let Ok(p) = palette_spawn.lock() {
                pane.set_palette(p.clone()); // apply the current colours
            }
            pane
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

        // egui overlay for chrome (preferences, colour pickers). Shares our GL
        // context via egui_glow's painter; egui-winit bridges window input.
        let egui_ctx = egui::Context::default();
        let egui_state = egui_winit::State::new(
            egui_ctx.clone(),
            egui::ViewportId::ROOT,
            &window,
            Some(window.scale_factor() as f32),
            None,
            None,
        );
        let egui_painter = egui_glow::Painter::new(gl.clone(), "", None, false)
            .expect("failed to create egui painter");

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
            ime_preedit: false,
            clipboard,
            selection: None,
            selecting: false,
            egui_ctx,
            egui_state,
            egui_painter,
            prefs_open: false,
            palette,
            font_db: self.font_db.clone(),
            font_blobs,
            mono_families: self.mono_families.clone(),
        });
        // Debug/verification hook: RT_PREFS opens the preferences dialog at
        // startup so its egui rendering can be screenshotted.
        if std::env::var("RT_PREFS").is_ok() {
            if let Some(active) = self.active.as_mut() {
                active.prefs_open = true;
            }
        }
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

        // While the preferences dialog is open, egui gets first look at events
        // and terminal input is suspended (window lifecycle still flows through).
        if active.prefs_open {
            let r = active.egui_state.on_window_event(&active.window, &event);
            if r.repaint {
                active.window.request_redraw();
            }
            match event {
                // Escape closes the dialog.
                WindowEvent::KeyboardInput { event: ref ke, .. }
                    if ke.state == ElementState::Pressed
                        && matches!(ke.logical_key, Key::Named(NamedKey::Escape)) =>
                {
                    active.prefs_open = false;
                    active.window.request_redraw();
                    return;
                }
                // Swallow terminal input while the dialog is up.
                WindowEvent::KeyboardInput { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::Ime(_)
                | WindowEvent::ModifiersChanged(_) => return,
                // Close / resize / redraw fall through to the normal handling.
                _ => {}
            }
        }

        match event {
            // The user closed the window.
            WindowEvent::CloseRequested => event_loop.exit(),

            // Track modifier state so key events can build correct chords.
            WindowEvent::ModifiersChanged(new_mods) => {
                active.mods = new_mods.state(); // remember Ctrl/Shift/Alt/Super
            }

            // IME: dead-key/compose and full input methods. Composed text is
            // committed here; while a preedit is in progress, on_key_press is
            // suppressed so the intermediate keys don't also reach the PTY.
            WindowEvent::Ime(ime) => match ime {
                Ime::Commit(text) => {
                    active.ime_preedit = false; // composition finished
                    if !text.is_empty() {
                        active.session.feed_input(text.as_bytes()); // send the composed text (e.g. "ó")
                    }
                }
                Ime::Preedit(text, _cursor) => {
                    // A non-empty preedit means a composition is under way.
                    active.ime_preedit = !text.is_empty();
                }
                Ime::Enabled => {} // IME turned on; nothing to do
                Ime::Disabled => active.ime_preedit = false, // IME off; clear any preedit gate
            },

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
                if active.selecting {
                    // Extend the selection to the cell under the pointer.
                    if let Some((pane, col, row)) = Self::cell_at(active, active.mouse.0, active.mouse.1) {
                        if let Some(sel) = active.selection.as_mut() {
                            if sel.pane == pane {
                                sel.head = (col, row); // move the drag end
                                active.window.request_redraw();
                            }
                        }
                    }
                } else if let Some(m) = active.menu.as_mut() {
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

            // Mouse buttons: right opens the menu; left drives menu/tab/focus and
            // starts/ends a text selection; middle pastes the PRIMARY selection.
            WindowEvent::MouseInput { state, button, .. } => match (state, button) {
                (ElementState::Pressed, MouseButton::Right) => {
                    log::debug!("right-click at {:?} → open menu", active.mouse);
                    // Focus the pane under the cursor first, so the menu's
                    // actions apply to the pane you right-clicked.
                    active.session.focus_at(active.mouse.0, active.mouse.1);
                    let (cw, ch) = active.renderer.cell_size();
                    let size = active.window.inner_size();
                    let mut m = menu::Menu::new(active.mouse.0, active.mouse.1);
                    m.clamp(size.width as f32, size.height as f32, cw, ch);
                    active.menu = Some(m);
                    active.window.request_redraw();
                }
                (ElementState::Pressed, MouseButton::Left) => {
                    if let Some(m) = active.menu.take() {
                        // Menu open: click activates the hovered item (or closes).
                        let (cw, ch) = active.renderer.cell_size();
                        let action = m.hit(active.mouse.0, active.mouse.1, cw, ch).and_then(|i| m.action_at(i));
                        active.window.request_redraw();
                        if let Some(a) = action {
                            Self::apply_action(active, a); // may exit on the last pane
                        }
                    } else {
                        let size = active.window.inner_size();
                        let bounds = Rect::new(0.0, 0.0, size.width as f32, size.height as f32);
                        let (mx, my) = active.mouse;
                        // A tab label click switches tabs; else focus the pane and
                        // begin a text selection at the clicked cell.
                        let clicked_tab = active
                            .session
                            .tab_bars(bounds)
                            .into_iter()
                            .flat_map(|bar| bar.tabs)
                            .find(|t| t.rect.contains(mx, my))
                            .map(|t| t.first_pane);
                        if let Some(first_pane) = clicked_tab {
                            active.session.focus_tab(first_pane);
                            active.window.request_redraw();
                        } else {
                            active.session.focus_at(mx, my); // click-to-focus
                            // Start a selection at this cell (cleared on release
                            // if it stays a zero-length click).
                            if let Some((pane, col, row)) = Self::cell_at(active, mx, my) {
                                active.selection = Some(Selection { pane, anchor: (col, row), head: (col, row) });
                                active.selecting = true;
                            }
                            active.window.request_redraw();
                        }
                    }
                }
                (ElementState::Released, MouseButton::Left) => {
                    active.selecting = false; // drag finished
                    // A zero-length selection was just a click: discard it, and
                    // (copy-on-select) copy a real selection to PRIMARY.
                    if let Some(sel) = active.selection {
                        if sel.anchor == sel.head {
                            active.selection = None;
                        } else if let Some(text) = Self::selected_text(active) {
                            if let Some(cb) = &active.clipboard {
                                cb.store_primary(text); // PRIMARY for middle-click paste
                            }
                        }
                        active.window.request_redraw();
                    }
                }
                (ElementState::Pressed, MouseButton::Middle) => {
                    // Middle-click pastes the PRIMARY selection (X/Wayland idiom).
                    if let Some(cb) = &active.clipboard {
                        if let Ok(text) = cb.load_primary() {
                            if !text.is_empty() {
                                active.session.feed_input(text.as_bytes());
                            }
                        }
                    }
                }
                _ => {} // other buttons / releases: nothing
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
        let mut titles: Vec<(rt_core::PaneId, String)> = Vec::new(); // pending title updates
        for id in active.session.tree().all_panes() {
            if let Some(pane) = active.session.pane(id) {
                for ev in pane.drain_events() {
                    match ev {
                        rt_engine::PaneEvent::Exited => exited.push(id), // reap this pane
                        rt_engine::PaneEvent::Title(t) => {
                            titles.push((id, t)); // apply after the loop (needs &mut session)
                            dirty = true;
                        }
                        _ => dirty = true, // Wakeup/Bell → needs a redraw
                    }
                }
            }
        }
        // Apply title updates, then refresh the window title from the focused pane.
        if !titles.is_empty() {
            for (id, t) in titles {
                active.session.set_title(id, t);
            }
            let win_title = active
                .session
                .title_of(active.session.focus()) // focused pane's title
                .map(|t| format!("{t} — rt")) // suffix so it's identifiable
                .unwrap_or_else(|| "rt".to_string());
            active.window.set_title(&win_title);
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
                Self::persist(&active.settings); // remember across restarts
                active.window.request_redraw();
            }
            Action::OpacityDown => {
                let v = active.settings.adjust_opacity(-0.05); // more see-through
                log::info!("background opacity = {v:.2}");
                Self::persist(&active.settings);
                active.window.request_redraw();
            }
            Action::ScrimUp => {
                let v = active.settings.adjust_scrim(0.05); // stronger wash
                log::info!("scrim strength = {v:.2}");
                Self::persist(&active.settings);
                active.window.request_redraw();
            }
            Action::ScrimDown => {
                let v = active.settings.adjust_scrim(-0.05); // weaker wash
                log::info!("scrim strength = {v:.2}");
                Self::persist(&active.settings);
                active.window.request_redraw();
            }
            Action::ToggleFocusFollowsMouse => {
                active.settings.focus_follows_mouse = !active.settings.focus_follows_mouse; // flip mode
                log::info!("focus-follows-mouse = {}", active.settings.focus_follows_mouse);
                Self::persist(&active.settings);
                active.window.request_redraw();
            }
            Action::Preferences => {
                active.prefs_open = !active.prefs_open; // toggle the dialog
                active.window.request_redraw();
            }
            Action::ZoomIn => {
                active.settings.font_size = (active.settings.font_size + 2.0).min(72.0);
                Self::persist(&active.settings);
                Self::refresh_fonts(active, false); // same family, new size
                active.window.request_redraw();
            }
            Action::ZoomOut => {
                active.settings.font_size = (active.settings.font_size - 2.0).max(6.0);
                Self::persist(&active.settings);
                Self::refresh_fonts(active, false);
                active.window.request_redraw();
            }
            Action::ZoomReset => {
                active.settings.font_size = rt_config::Settings::default().font_size;
                Self::persist(&active.settings);
                Self::refresh_fonts(active, false);
                active.window.request_redraw();
            }
            Action::Fullscreen => {
                // Toggle borderless fullscreen on the current monitor.
                let fs = if active.window.fullscreen().is_some() {
                    None
                } else {
                    Some(winit::window::Fullscreen::Borderless(None))
                };
                active.window.set_fullscreen(fs);
                active.window.request_redraw();
            }
            // Everything else is a session action.
            other => match active.session.apply(other) {
                Some(SessionEvent::CloseWindow) => std::process::exit(0), // last pane closed
                Some(SessionEvent::Copy) => Self::do_copy(active),   // selection → clipboard
                Some(SessionEvent::Paste) => Self::do_paste(active), // clipboard → focused PTY
                Some(SessionEvent::Redraw) => active.window.request_redraw(),
                None => {}
            },
        }
    }

    /// Paste the clipboard's text into the focused pane(s). No-op if the
    /// clipboard is empty/unavailable.
    fn do_paste(active: &mut Active) {
        if let Some(cb) = &active.clipboard {
            if let Ok(text) = cb.load() {
                if !text.is_empty() {
                    active.session.feed_input(text.as_bytes()); // respects broadcast mode
                }
            }
        }
    }

    /// Copy the current selection to the clipboard (and PRIMARY for middle-click
    /// paste). No-op if there is no selection.
    fn do_copy(active: &mut Active) {
        if let Some(text) = Self::selected_text(active) {
            if !text.is_empty() {
                if let Some(cb) = &active.clipboard {
                    cb.store(text.clone()); // CLIPBOARD (Ctrl+Shift+V / apps)
                    cb.store_primary(text); // PRIMARY (middle-click paste)
                }
            }
        }
    }

    /// Extract the selected text from its pane's grid, row by row, trimming
    /// trailing blanks and joining rows with newlines. `None` if no selection.
    fn selected_text(active: &Active) -> Option<String> {
        let sel = active.selection.as_ref()?; // the active selection
        let snap = active.session.pane(sel.pane)?.snapshot(); // that pane's grid
        let ((sc, sr), (ec, er)) = sel.ordered(); // start precedes end (row-major)
        let mut out = String::new();
        for row in sr..=er {
            let Some(line) = snap.rows.get(row) else { break }; // row out of range
            let last = line.len().saturating_sub(1); // last valid column
            let c0 = if row == sr { sc.min(last) } else { 0 }; // first column on this row
            let c1 = if row == er { ec.min(last) } else { last }; // last column on this row
            if c0 <= c1 {
                let s: String = line[c0..=c1].iter().map(|cell| cell.c).collect();
                out.push_str(s.trim_end()); // drop trailing blanks
            }
            if row != er {
                out.push('\n'); // newline between rows
            }
        }
        Some(out)
    }

    /// Map a physical-pixel point to `(pane, col, row)` in that pane's grid, for
    /// selection. Returns `None` for column-mode panes (their re-tiled layout
    /// makes cell mapping ambiguous — selection there is a follow-up) or points
    /// outside any pane.
    fn cell_at(active: &Active, mx: f32, my: f32) -> Option<(rt_core::PaneId, usize, usize)> {
        let size = active.window.inner_size();
        let bounds = Rect::new(0.0, 0.0, size.width as f32, size.height as f32);
        let (cw, ch) = active.renderer.cell_size();
        for (id, rect) in active.session.tree().rects(bounds) {
            if rect.contains(mx, my) {
                if active.session.columns_of(id) > 1 {
                    return None; // skip newspaper-column panes for now
                }
                let col = ((mx - rect.x) / cw).max(0.0) as usize; // cell column
                let row = ((my - rect.y) / ch).max(0.0) as usize; // cell row
                return Some((id, col, row));
            }
        }
        None
    }

    /// Reload the renderer's fonts from the current settings (rebuilding the
    /// font chains if the family changed), re-measure the cell, and resize every
    /// pane to the new (cols, rows). Shared by the preferences dialog and the
    /// zoom actions.
    fn refresh_fonts(active: &mut Active, family_changed: bool) {
        if family_changed {
            active.font_blobs = font_blobs(&active.font_db, &active.settings.font_family);
        }
        let px = active.settings.font_size;
        match active.renderer.reload_fonts(&active.font_blobs, px) {
            Ok(()) => {
                let cell = active.renderer.cell_size(); // new cell metrics
                active.session.set_cell(cell);
                let size = active.window.inner_size();
                active.session.relayout(Rect::new(0.0, 0.0, size.width as f32, size.height as f32));
            }
            Err(e) => log::warn!("font reload failed: {e}"),
        }
    }

    /// Persist the current settings to the config file, logging (not failing) on
    /// error. Called after any setting change so it survives a restart.
    fn persist(settings: &rt_config::Settings) {
        let cfg = rt_config::Config { settings: settings.clone() }; // wrap for serialisation
        if let Err(e) = cfg.save() {
            log::warn!("could not save config: {e}"); // non-fatal
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
        // While an IME/dead-key composition is in progress, swallow key presses:
        // the composed result arrives via WindowEvent::Ime(Commit) instead. This
        // is what prevents the dead key (´) and its result (ó) both being sent.
        if active.ime_preedit {
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
        // Not a binding: ordinary typing. Navigation/editing/function keys become
        // ANSI escape sequences; everything else sends the key's *produced text*
        // (`key_event.text`), which already contains dead-key / compose results
        // (e.g. `'`+space → `'`) that the logical key alone would miss.
        let app_cursor = active
            .session
            .pane(active.session.focus()) // the focused pane's backend
            .map(|p| p.app_cursor_keys()) // its DECCKM state
            .unwrap_or(false); // default to normal cursor keys
        let bytes = match &key_event.logical_key {
            Key::Named(n) if input::is_sequence_key(n) => {
                input::encode_key(&key_event.logical_key, mods, app_cursor) // arrows/enter/…
            }
            _ => match key_event.text.as_ref().filter(|t| !t.is_empty()) {
                Some(text) => Some(input::encode_text(text, mods)), // the composed text
                None => input::encode_key(&key_event.logical_key, mods, app_cursor), // fallback (Ctrl combos, etc.)
            },
        };
        if let Some(bytes) = bytes {
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
        let cfg_bg = active.settings.background; // configured background RGB
        let bg = Color::rgb(cfg_bg[0], cfg_bg[1], cfg_bg[2]).with_alpha(active.settings.background_opacity);
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
                // The selection, if it belongs to this (single-column) pane.
                let pane_sel: Option<Selection> = active.selection.filter(|s| n <= 1 && s.pane == id);
                let sel_bg = Color::rgb(0x33, 0x44, 0x66); // selection highlight colour
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
                        // Selection highlight wins over the cell's own background;
                        // otherwise draw an explicit (non-default) background.
                        if pane_sel.map_or(false, |s| s.contains(col_idx, sub)) {
                            active.renderer.fill_cell(ox, rect.y, col_idx, sub, sel_bg);
                        } else if cell.bg != cfg_bg {
                            // A non-default background: draw it opaque (default-bg
                            // cells stay translucent via the window clear).
                            let c = cell.bg;
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
                        let cc = active.settings.foreground; // cursor colour = configured foreground
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

        // Pane dividers: a thin line centred in each split gutter so the
        // boundary between panes is visible (most of the gutter stays the
        // translucent background).
        let divider_col = Color::rgb(0x3a, 0x3a, 0x46);
        for d in active.session.dividers(bounds) {
            if d.w < d.h {
                // Vertical gutter (left/right split): a vertical line.
                active.renderer.fill_rect(d.x + d.w * 0.5 - 0.5, d.y, 1.0, d.h, divider_col);
            } else {
                // Horizontal gutter (top/bottom split): a horizontal line.
                active.renderer.fill_rect(d.x, d.y + d.h * 0.5 - 0.5, d.w, 1.0, divider_col);
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
                // Label: the pane's title if it has one, else the tab number.
                // Truncate to what fits in the segment (leaving room for the
                // number prefix and padding).
                let max_chars = ((r.w - 16.0) / cell_w).floor().max(1.0) as usize;
                let label = match active.session.title_of(tab.first_pane) {
                    Some(title) => {
                        let prefixed = format!("{}: {}", tab.number, title); // "1: user@host …"
                        if prefixed.chars().count() > max_chars {
                            // Truncate with an ellipsis.
                            let keep = max_chars.saturating_sub(1);
                            format!("{}…", prefixed.chars().take(keep).collect::<String>())
                        } else {
                            prefixed
                        }
                    }
                    None => tab.number.to_string(), // untitled → just the number
                };
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

        // egui overlay (preferences dialog), painted on top of the terminal.
        if active.prefs_open {
            Self::paint_egui(active);
        }

        // Present the frame.
        if let Err(e) = active.surface.swap_buffers(&active.context) {
            log::error!("swap_buffers failed: {e}"); // non-fatal; log and continue
        }
    }

    /// Run the egui preferences UI for this frame and paint it. Applies any
    /// changed settings (persisting them) and closes the dialog when requested.
    fn paint_egui(active: &mut Active) {
        let raw_input = active.egui_state.take_egui_input(&active.window); // gather input
        let ctx = active.egui_ctx.clone(); // cheap Arc clone (avoids borrowing active in the closure)
        let mut settings = active.settings.clone(); // the UI edits this clone
        let mut close = false; // set by the Close button
        // egui 0.35 frame API: begin_pass → build UI → end_pass.
        ctx.begin_pass(raw_input);
        preferences::ui(&ctx, &mut settings, &mut close, &active.mono_families); // build the dialog
        let output = ctx.end_pass();
        // Apply + persist any change the user made.
        if settings != active.settings {
            // What changed drives which subsystem we refresh.
            let colours_changed = settings.foreground != active.settings.foreground
                || settings.background != active.settings.background
                || settings.palette != active.settings.palette;
            let family_changed = settings.font_family != active.settings.font_family;
            let fonts_changed = family_changed || settings.font_size != active.settings.font_size;
            active.settings = settings; // commit
            Self::persist(&active.settings);
            // Colours: rebuild the palette and apply it live to every pane.
            if colours_changed {
                let pal = rt_engine::Palette::new(
                    active.settings.foreground,
                    active.settings.background,
                    active.settings.palette,
                );
                if let Ok(mut p) = active.palette.lock() {
                    *p = pal.clone(); // future panes inherit these colours
                }
                active.session.set_all_palettes(pal); // recolour existing panes
            }
            // Fonts: reload the family (if changed) and/or size, then re-measure
            // the cell and resize every pane to the new (cols, rows).
            if fonts_changed {
                Self::refresh_fonts(active, family_changed);
            }
        }
        if close {
            active.prefs_open = false;
        }
        active.egui_state.handle_platform_output(&active.window, output.platform_output);
        // Tessellate and paint egui's shapes over the current framebuffer.
        let ppp = output.pixels_per_point;
        let primitives = ctx.tessellate(output.shapes, ppp);
        let size = active.window.inner_size();
        active.egui_painter.paint_and_update_textures(
            [size.width, size.height],
            ppp,
            &primitives,
            &output.textures_delta,
        );
    }
}

/// Program entry point: set up logging, load a font, and run the winit loop.
fn main() {
    env_logger::init(); // honour RUST_LOG for diagnostics
    // Scan system fonts up front (for the family picker + lookup). A monospace
    // font is mandatory; fail early with guidance if none is installed.
    let font_db = std::sync::Arc::new(build_font_db());
    let mono_families = monospace_families(&font_db);
    if mono_families.is_empty() && load_fonts().is_none() {
        eprintln!(
            "rt: no monospace font found. Install e.g. 'fonts-dejavu-core' \
             (Debian/Ubuntu) or 'ttf-dejavu' (Arch) and retry."
        );
        std::process::exit(1);
    }
    // Build the winit event loop and hand it our application.
    let event_loop = EventLoop::new().expect("failed to create event loop");
    let mut app = App { font_db, mono_families, active: None };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("rt: event loop error: {e}"); // surface any run-loop failure
        std::process::exit(1);
    }
}
