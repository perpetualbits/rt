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
use rt_session::{Broadcast, Session, SessionEvent};

/// The concrete session type used by the app: real PTY panes, spawned by a
/// boxed closure (boxed so the `Session`'s factory type is nameable in a field).
type AppSession = Session<TermPane, Box<dyn FnMut(usize, usize) -> TermPane>>;

/// One pane's output-activity instrument state (ported from rt-mux): a smoothed
/// output rate and an accumulated flow phase. The border flow's speed and
/// brightness track real output — the motion *is* the measurement.
#[derive(Default, Clone, Copy)]
struct Meter {
    wakeups: u32, // output events counted since the last tick
    rate: f32,    // smoothed events/second
    phase: f32,   // accumulated flow position (laps; only the fraction matters)
}

/// Output events/second that reads as "fully busy" (flow saturates here).
const BUSY_WAKEUPS: f32 = 60.0;
/// Laps/second the border flow travels at full activity.
const FLOW_MAX_LAPS: f32 = 0.6;
/// Green packets orbiting each pane's border.
const FLOW_PACKETS: u32 = 4;

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
    dragging_divider: Option<rt_core::DragHandle>, // the split divider being dragged, if any
    bell_flash: Option<Instant>,          // expiry of the current visible-bell flash, if any
    meters: std::collections::HashMap<rt_core::PaneId, Meter>, // per-pane output-activity instrument
    last_meter_tick: Instant,             // wall-clock of the last instrument advance
    heat: std::collections::HashMap<rt_core::PaneId, f32>, // per-pane CPU load (heat instrument)
    heat_ticks: std::collections::HashMap<rt_core::PaneId, u64>, // last session CPU ticks per pane
    heat_last: Instant,                   // wall-clock of the last /proc heat sample
    last_click: Option<(Instant, (f32, f32))>, // time + position of the last left-press
    click_count: u8,                      // 1=single, 2=double (word), 3=triple (line)
    egui_ctx: egui::Context,              // egui immediate-mode context (chrome/dialogs)
    egui_state: egui_winit::State,        // egui-winit input bridge
    egui_painter: egui_glow::Painter,     // egui_glow renderer (shares our GL context)
    prefs_open: bool,                     // whether the preferences dialog is showing
    search_open: bool,                    // whether the scrollback-search bar is showing
    search_query: String,                 // the current search text
    search_matches: Vec<rt_engine::SearchMatch>, // hits for search_query in search_pane
    search_index: usize,                  // which hit is the "current" one (highlighted brighter)
    search_pane: Option<rt_core::PaneId>, // the pane the current matches belong to
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
        // Reserve a per-pane titlebar strip if the settings ask for it, then
        // relayout so the first pane is sized to its content (minus the header).
        session.set_show_titlebar(settings.show_titlebar);
        session.relayout(bounds);

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
        if std::env::var("RT_ZOOM").is_ok() {
            session.apply(rt_config::Action::ToggleZoom); // maximise the focused pane at startup
        }
        // Debug/verification hook: build three panes each in a different input
        // group so the corner group markers can be screenshotted.
        if std::env::var("RT_DEMO_GROUPS").is_ok() {
            use rt_config::Action::*;
            session.apply(GroupCycle); // pane0 → group 1
            session.apply(SplitVert); // → pane1 focused
            session.apply(GroupCycle);
            session.apply(GroupCycle); // pane1 → group 2
            session.apply(SplitHoriz); // → pane2 focused
            session.apply(GroupCycle);
            session.apply(GroupCycle);
            session.apply(GroupCycle); // pane2 → group 3
        }
        if let Ok(v) = std::env::var("RT_BROADCAST") {
            let _ = match v.as_str() {
                "all" => session.apply(rt_config::Action::BroadcastAll),
                "group" => session.apply(rt_config::Action::BroadcastGroup),
                _ => None,
            };
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
            dragging_divider: None,
            bell_flash: None,
            meters: std::collections::HashMap::new(),
            last_meter_tick: Instant::now(),
            heat: std::collections::HashMap::new(),
            heat_ticks: std::collections::HashMap::new(),
            heat_last: Instant::now(),
            last_click: None,
            click_count: 0,
            egui_ctx,
            egui_state,
            egui_painter,
            prefs_open: false,
            search_open: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_index: 0,
            search_pane: None,
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
        // Debug/verification hook: RT_SEARCH opens the search bar at startup with
        // a pre-filled query so its rendering + highlighting can be screenshotted
        // without synthetic keyboard input.
        if let Ok(q) = std::env::var("RT_SEARCH") {
            if let Some(active) = self.active.as_mut() {
                active.search_open = true;
                active.search_query = q; // e.g. RT_SEARCH=echo
                Self::run_search(active, true); // populate matches + highlight
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

        // The scrollback-search bar is a lighter overlay: it captures typing (via
        // egui) and Enter/Escape for navigation, but leaves the terminal visible
        // and lets mouse events (scroll/click) through so the user can still look
        // around while a search is open.
        if active.search_open {
            // Enter = jump to next hit (Shift+Enter = previous); Escape closes.
            if let WindowEvent::KeyboardInput { event: ke, .. } = &event {
                if ke.state == ElementState::Pressed {
                    match ke.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            Self::close_search(active);
                            return;
                        }
                        Key::Named(NamedKey::Enter) => {
                            let dir: isize = if active.mods.shift_key() { -1 } else { 1 };
                            Self::search_step(active, dir);
                            active.window.request_redraw();
                            return;
                        }
                        _ => {}
                    }
                }
            }
            // Keep the modifier state current (Shift+Enter above needs it), since
            // the normal ModifiersChanged handler is bypassed while searching.
            if let WindowEvent::ModifiersChanged(m) = &event {
                active.mods = m.state();
            }
            // Feed typing/edit keys to the egui text field.
            let r = active.egui_state.on_window_event(&active.window, &event);
            if r.repaint {
                active.window.request_redraw();
            }
            // Swallow keyboard/IME so the terminal receives no input while typing
            // a query; mouse and lifecycle events fall through to the normal path.
            match event {
                WindowEvent::KeyboardInput { .. }
                | WindowEvent::Ime(_)
                | WindowEvent::ModifiersChanged(_) => return,
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
                if let Some(handle) = active.dragging_divider.clone() {
                    // Resize the split: turn the mouse position along the split's
                    // axis into a first-child ratio.
                    let axis = if handle.horizontal { active.mouse.0 } else { active.mouse.1 };
                    let ratio = ((axis - handle.start) / handle.len).clamp(0.05, 0.95);
                    active.session.set_split_ratio(&handle, ratio);
                    active.window.request_redraw();
                } else if active.selecting {
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
                        // A press on a split divider starts a drag-to-resize
                        // (checked before tabs/focus/selection).
                        if let Some(handle) = active.session.divider_at(mx, my, bounds) {
                            active.dragging_divider = Some(handle);
                            return;
                        }
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
                            // Ctrl+click on a URL opens it in the default handler
                            // (the common terminal idiom); consume the click.
                            if active.mods.control_key() {
                                if let Some((pane, col, row)) = Self::cell_at(active, mx, my) {
                                    if let Some(url) = Self::url_at(active, pane, col, row) {
                                        Self::open_url(&url);
                                        return;
                                    }
                                }
                            }
                            // Determine the click count (single / double / triple)
                            // from timing + proximity to the previous press.
                            let now = Instant::now();
                            let count = match active.last_click {
                                Some((t, (lx, ly)))
                                    if now.duration_since(t) < Duration::from_millis(400)
                                        && (mx - lx).abs() < 5.0
                                        && (my - ly).abs() < 5.0 =>
                                {
                                    (active.click_count % 3) + 1 // 1→2→3→1
                                }
                                _ => 1,
                            };
                            active.last_click = Some((now, (mx, my)));
                            active.click_count = count;
                            // Single = start a drag-select; double = word; triple = line.
                            if let Some((pane, col, row)) = Self::cell_at(active, mx, my) {
                                match count {
                                    2 => {
                                        if let Some((s, e)) = Self::word_at(active, pane, col, row) {
                                            active.selection = Some(Selection { pane, anchor: (s, row), head: (e, row) });
                                        }
                                        active.selecting = false;
                                        Self::copy_selection_to_primary(active);
                                    }
                                    3 => {
                                        let last = Self::line_last_col(active, pane, row);
                                        active.selection = Some(Selection { pane, anchor: (0, row), head: (last, row) });
                                        active.selecting = false;
                                        Self::copy_selection_to_primary(active);
                                    }
                                    _ => {
                                        active.selection = Some(Selection { pane, anchor: (col, row), head: (col, row) });
                                        active.selecting = true;
                                    }
                                }
                            }
                            active.window.request_redraw();
                        }
                    }
                }
                (ElementState::Released, MouseButton::Left) => {
                    active.dragging_divider = None; // end any divider resize
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
                        rt_engine::PaneEvent::Bell => {
                            // Visible bell: flash the window briefly.
                            active.bell_flash = Some(Instant::now() + Duration::from_millis(150));
                            dirty = true;
                        }
                        _ => {
                            // Wakeup → new output; count it for the pane's border
                            // flow instrument and schedule a redraw.
                            active.meters.entry(id).or_default().wakeups += 1;
                            dirty = true;
                        }
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
            active.meters.remove(&id); // forget the closed pane's instrument state
            active.heat.remove(&id);
            active.heat_ticks.remove(&id);
        }
        // Refresh the CPU-heat readings (~2 Hz); repaint if a fresh sample lands
        // and any pane is warm, so the temperature border stays live even for a
        // CPU-busy-but-silent pane.
        if Self::sample_heat(active) && active.heat.values().any(|&h| h > 0.02) {
            dirty = true;
        }
        // Keep repainting while any border flow is still moving, so it eases to a
        // stop (and decays) instead of freezing mid-orbit when output pauses.
        if active.meters.values().any(|m| m.rate > 0.5) {
            dirty = true;
        }
        // While the search bar is open, keep its results current as new output
        // streams into the searched pane (so a running command's fresh lines are
        // matched live). Re-run only on a change, and only when the query is set.
        if dirty && active.search_open && !active.search_query.is_empty() {
            Self::run_search(active, false); // live refresh; keep position + view
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
            Action::Search => {
                // Open the scrollback-search bar for the focused pane, starting
                // from a clean slate each time it is opened.
                active.search_open = true;
                active.search_query.clear();
                active.search_matches.clear();
                active.search_index = 0;
                active.search_pane = Some(active.session.focus());
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
        for (id, rect) in active.session.visible_rects(bounds) {
            if rect.contains(mx, my) {
                if active.session.columns_of(id) > 1 {
                    return None; // skip newspaper-column panes for now
                }
                // Map against the content rect (grid area minus the titlebar); a
                // click in the titlebar strip is above the content and yields no
                // cell.
                let content = active.session.content_rect(rect);
                if my < content.y {
                    return None; // in the titlebar, not the grid
                }
                let col = ((mx - content.x) / cw).max(0.0) as usize; // cell column
                let row = ((my - content.y) / ch).max(0.0) as usize; // cell row
                return Some((id, col, row));
            }
        }
        None
    }

    /// Word boundaries around `(col, row)` for double-click selection: expand
    /// left and right over "word" characters (alphanumerics plus the punctuation
    /// that usually belongs to paths/URLs). Returns the inclusive `(start, end)`
    /// columns, or `None` if the row/column is out of range. A click on a
    /// non-word character selects just that one cell.
    fn word_at(active: &Active, pane: rt_core::PaneId, col: usize, row: usize) -> Option<(usize, usize)> {
        let snap = active.session.pane(pane)?.snapshot(); // the pane's grid
        let line = snap.rows.get(row)?; // the clicked row
        if col >= line.len() {
            return None; // click past the end of the row
        }
        // A "word" char: alphanumeric plus the symbols common in paths/URLs.
        let is_word = |c: char| c.is_alphanumeric() || "-_./~:@%+#=?&".contains(c);
        if !is_word(line[col].c) {
            return Some((col, col)); // lone symbol/space → single-cell select
        }
        let mut s = col; // grow the start leftwards
        while s > 0 && is_word(line[s - 1].c) {
            s -= 1;
        }
        let mut e = col; // grow the end rightwards
        while e + 1 < line.len() && is_word(line[e + 1].c) {
            e += 1;
        }
        Some((s, e))
    }

    /// Detect a URL at `(col, row)` for Ctrl+click opening. Expands over URL
    /// characters around the click, then accepts the run only if it begins with
    /// a known scheme. Trailing sentence punctuation is trimmed so a URL at the
    /// end of a sentence still opens cleanly. Returns the URL, or `None`.
    fn url_at(active: &Active, pane: rt_core::PaneId, col: usize, row: usize) -> Option<String> {
        let snap = active.session.pane(pane)?.snapshot(); // the pane's grid
        let line = snap.rows.get(row)?; // the clicked row
        if col >= line.len() {
            return None;
        }
        // URL characters per RFC 3986 (a permissive superset); notably excludes
        // whitespace, quotes and brackets that would end a URL in running text.
        let is_url = |c: char| !c.is_whitespace() && !"\"'<>`(){}[]".contains(c);
        if !is_url(line[col].c) {
            return None;
        }
        let mut s = col; // grow left over URL characters
        while s > 0 && is_url(line[s - 1].c) {
            s -= 1;
        }
        let mut e = col; // grow right over URL characters
        while e + 1 < line.len() && is_url(line[e + 1].c) {
            e += 1;
        }
        let raw: String = line[s..=e].iter().map(|cell| cell.c).collect();
        // Drop trailing punctuation that is usually sentence, not URL, syntax.
        let url = raw.trim_end_matches(['.', ',', ';', ':', '!', '?']);
        // Only treat it as a link if it carries a recognised scheme.
        const SCHEMES: [&str; 5] = ["http://", "https://", "ftp://", "file://", "mailto:"];
        if SCHEMES.iter().any(|p| url.starts_with(p)) {
            Some(url.to_string())
        } else {
            None
        }
    }

    /// Open a URL with the desktop's default handler via `xdg-open`, detached so
    /// it never blocks rt. Failures are logged, not fatal (rt's no-crash policy).
    fn open_url(url: &str) {
        // Spawn xdg-open without waiting; ignore the handle so it runs detached.
        match std::process::Command::new("xdg-open").arg(url).spawn() {
            Ok(_) => log::info!("opened URL: {url}"),
            Err(e) => log::warn!("xdg-open failed for {url}: {e}"),
        }
    }

    /// The last non-blank column on `row` (for triple-click line selection),
    /// clamped to a valid index. Falls back to 0 for an empty/blank line.
    fn line_last_col(active: &Active, pane: rt_core::PaneId, row: usize) -> usize {
        let Some(pane) = active.session.pane(pane) else { return 0 };
        let snap = pane.snapshot();
        let Some(line) = snap.rows.get(row) else { return 0 };
        line.iter().rposition(|cell| cell.c != ' ').unwrap_or(0)
    }

    /// Copy-on-select for the word/line selections made by double/triple-click
    /// (drag-selection copies on button release instead). Pushes the current
    /// selection text to the PRIMARY buffer for middle-click paste.
    fn copy_selection_to_primary(active: &Active) {
        if let Some(text) = Self::selected_text(active) {
            if let Some(cb) = &active.clipboard {
                cb.store_primary(text);
            }
        }
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
        for (id, rect) in active.session.visible_rects(bounds) {
            let n = active.session.columns_of(id); // newspaper column count (1 = normal)
            // Reserve the titlebar strip at the top: `content` is where the grid
            // draws; `full` keeps the whole pane rectangle for the strip, border
            // and markers. Shadowing `rect` with the content rect means every
            // `rect.*` below (cells, cursor, scrollbar, separators) refers to the
            // content area automatically — the grid can never overlap the header.
            let full = rect; // the pane's whole rectangle (incl. titlebar)
            let rect = active.session.content_rect(rect); // grid area (minus header)
            let bar_h = active.session.titlebar_h(); // header height (0 when disabled)
            // Copy the pane's current grid (glyphs + resolved colours + cursor).
            if let Some(pane) = active.session.pane(id) {
                let snap = pane.snapshot(); // in column mode this is a count*rows-tall screen
                let geom = active.session.column_layout(id, rect); // count/col_cells/rows/gap
                // The selection, if it belongs to this (single-column) pane.
                let pane_sel: Option<Selection> = active.selection.filter(|s| n <= 1 && s.pane == id);
                let sel_bg = Color::rgb(0x33, 0x44, 0x66); // selection highlight colour
                // Scrollback-search highlights for this pane: which (row, col)
                // cells fall inside a match. The current hit gets a brighter tint
                // than the rest. Only for single-column panes (column-mode cell
                // mapping is ambiguous). Maps each hit's absolute line to a
                // snapshot row via the pane's current scroll offset.
                let mut hl_cur: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
                let mut hl_other: std::collections::HashSet<(usize, usize)> = std::collections::HashSet::new();
                if active.search_open && active.search_pane == Some(id) && n <= 1 {
                    let offset = pane.scroll_info().0 as i32; // lines scrolled up
                    for (mi, m) in active.search_matches.iter().enumerate() {
                        let row = m.line + offset; // absolute line → snapshot row
                        if row < 0 || row as usize >= snap.rows.len() {
                            continue; // off the visible screen
                        }
                        let row = row as usize;
                        let set = if mi == active.search_index { &mut hl_cur } else { &mut hl_other };
                        for c in m.col..m.col + m.len {
                            set.insert((row, c)); // mark each cell of the hit
                        }
                    }
                }
                let cur_hl = Color::rgb(0xbb, 0x99, 0x22); // current match (brighter amber)
                let other_hl = Color::rgb(0x5a, 0x4a, 0x10); // other matches (dim amber)
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
                        if hl_cur.contains(&(r, col_idx)) {
                            active.renderer.fill_cell(ox, rect.y, col_idx, sub, cur_hl);
                        } else if hl_other.contains(&(r, col_idx)) {
                            active.renderer.fill_cell(ox, rect.y, col_idx, sub, other_hl);
                        } else if pane_sel.map_or(false, |s| s.contains(col_idx, sub)) {
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

                // Scrollbar on the right edge, shown when the pane has scrollback.
                let (offset, history, screen) = pane.scroll_info();
                if history > 0 {
                    let total = (history + screen) as f32; // whole buffer height in lines
                    let bw = 6.0; // scrollbar width
                    let bx = rect.right() - bw - 1.0; // inset slightly from the edge
                    // Track + thumb; thumb sized/placed by the visible fraction and
                    // how far we're scrolled up. Brighter when scrolled off bottom.
                    active.renderer.fill_rect(bx, rect.y, bw, rect.h, Color::rgb(0x22, 0x22, 0x2c));
                    let thumb_h = (screen as f32 / total * rect.h).max(24.0); // min grabbable size
                    let thumb_y = (rect.y + (history - offset) as f32 / total * rect.h)
                        .min(rect.bottom() - thumb_h) // keep inside the pane
                        .max(rect.y);
                    let thumb_col = if offset > 0 {
                        Color::rgb(0x88, 0x88, 0x9a) // scrolled up: highlight
                    } else {
                        Color::rgb(0x55, 0x55, 0x66) // at the bottom: dimmer
                    };
                    active.renderer.fill_rect(bx, thumb_y, bw, thumb_h, thumb_col);
                }
            }
            // Per-pane titlebar strip (Terminator-style), drawn in the reserved
            // header above the grid: focus-tinted background, an optional group
            // swatch, the pane title on the left and its cell size on the right.
            let group_hue = |g: u32| match g {
                1 => Color::rgb(0xe0, 0x6c, 0x40), // orange
                2 => Color::rgb(0x4c, 0xa0, 0xe0), // blue
                3 => Color::rgb(0x6c, 0xc0, 0x50), // green
                _ => Color::rgb(0xc0, 0x60, 0xd0), // purple (group 4+)
            };
            if bar_h > 0.0 {
                let focused = id == focus;
                // Strip background + a dark hairline separating it from the grid.
                let bar_bg = if focused { Color::rgb(0x35, 0x3b, 0x4a) } else { Color::rgb(0x24, 0x26, 0x2e) };
                active.renderer.fill_rect(full.x, full.y, full.w, bar_h, bar_bg);
                active.renderer.fill_rect(full.x, full.y + bar_h - 1.0, full.w, 1.0, Color::rgb(0x12, 0x12, 0x18));
                let text_col = if focused { Color::rgb(0xe6, 0xe6, 0xf0) } else { Color::rgb(0x9a, 0x9a, 0xa6) };
                let pad = 6.0; // horizontal inset inside the strip
                let text_top = full.y + (bar_h - cell_h) * 0.5; // vertically centre the glyph line
                let mut left_x = full.x + pad; // running left cursor (px)
                // Group swatch, if this pane is in an input group.
                if let Some(g) = active.session.group_of(id) {
                    let s = cell_h * 0.6; // swatch side length
                    active.renderer.fill_rect(left_x, full.y + (bar_h - s) * 0.5, s, s, group_hue(g));
                    left_x += s + 5.0; // leave a gap before the title
                }
                // Size text ("COLSxROWS") pinned to the right edge.
                let cols = (rect.w / cell_w).max(0.0) as usize; // content columns
                let rows = (rect.h / cell_h).max(0.0) as usize; // content rows
                let size_str = format!("{cols}x{rows}");
                let size_w = size_str.chars().count() as f32 * cell_w;
                let size_x = (full.right() - pad - size_w).max(left_x);
                for (i, ch) in size_str.chars().enumerate() {
                    active.renderer.draw_char(size_x, text_top, i, 0, ch, text_col, false, false);
                }
                // Title on the left, truncated so it never runs into the size.
                let title = active.session.title_of(id).filter(|t| !t.is_empty()).unwrap_or("Terminal");
                let avail = ((size_x - 8.0 - left_x) / cell_w).max(0.0) as usize; // room in cells
                for (i, ch) in title.chars().take(avail).enumerate() {
                    active.renderer.draw_char(left_x, text_top, i, 0, ch, text_col, false, false);
                }
            } else if let Some(g) = active.session.group_of(id) {
                // No titlebar: fall back to a small colour-coded corner square so
                // group membership is still visible.
                let m = 10.0; // marker size in pixels
                let p = 4.0; // inset from the corner
                active.renderer.fill_rect(full.right() - m - p, full.y + p, m, m, group_hue(g));
            }
            // Outline the focused pane with a thin border (four thin rects),
            // around the whole pane including its titlebar (`full`, not content).
            if id == focus {
                let t = 2.0; // border thickness in pixels
                active.renderer.fill_rect(full.x, full.y, full.w, t, focus_border); // top
                active.renderer.fill_rect(full.x, full.bottom() - t, full.w, t, focus_border); // bottom
                active.renderer.fill_rect(full.x, full.y, t, full.h, focus_border); // left
                active.renderer.fill_rect(full.right() - t, full.y, t, full.h, focus_border); // right
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

        // Broadcast indicator: when typed input is being fanned out to more than
        // the focused pane, draw a bold coloured border around the whole window
        // (red = all panes, orange = group) so it's never a surprise.
        match active.session.broadcast() {
            Broadcast::Off => {}
            mode => {
                let col = if matches!(mode, Broadcast::All) {
                    Color::rgb(0xd9, 0x4a, 0x4a) // red: broadcasting to ALL panes
                } else {
                    Color::rgb(0xd9, 0x90, 0x4a) // orange: broadcasting to the group
                };
                let t = 3.0; // border thickness
                let (w, h) = (bounds.w, bounds.h);
                active.renderer.fill_rect(0.0, 0.0, w, t, col); // top
                active.renderer.fill_rect(0.0, h - t, w, t, col); // bottom
                active.renderer.fill_rect(0.0, 0.0, t, h, col); // left
                active.renderer.fill_rect(w - t, 0.0, t, h, col); // right
            }
        }

        // Visible bell: a brief translucent white flash over the whole window.
        if let Some(exp) = active.bell_flash {
            if Instant::now() < exp {
                active.renderer.fill_rect(0.0, 0.0, bounds.w, bounds.h, Color::rgb(0xff, 0xff, 0xff).with_alpha(0.25));
            } else {
                active.bell_flash = None; // flash elapsed
            }
        }

        // The context menu draws last so it sits above the terminal content.
        if let Some(m) = active.menu.as_ref() {
            let (cw, ch) = active.renderer.cell_size(); // cell metrics for menu layout
            m.draw(&mut active.renderer, cw, ch); // panel + rows + hover highlight
        }

        active.renderer.end_frame(); // upload + draw call

        // One egui pass per frame: the preferences dialog, or the search bar, or
        // (when no dialog is up) the always-on border instruments.
        if active.prefs_open {
            Self::paint_egui(active);
        } else if active.search_open {
            Self::paint_search(active);
        } else {
            Self::paint_instruments(active);
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
            let titlebar_changed = settings.show_titlebar != active.settings.show_titlebar;
            active.settings = settings; // commit
            Self::persist(&active.settings);
            // Titlebar toggle changes every pane's content height → re-reserve and
            // resize all PTYs.
            if titlebar_changed {
                active.session.set_show_titlebar(active.settings.show_titlebar);
                let size = active.window.inner_size();
                active.session.relayout(Rect::new(0.0, 0.0, size.width as f32, size.height as f32));
            }
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

    /// Run the scrollback-search bar UI for this frame and paint it. Draws a slim
    /// bottom bar with the query field and a hit counter; re-runs the search when
    /// the query text changes and jumps to the first hit.
    fn paint_search(active: &mut Active) {
        let raw_input = active.egui_state.take_egui_input(&active.window); // gather input
        let ctx = active.egui_ctx.clone(); // cheap Arc clone
        let mut query = active.search_query.clone(); // the UI edits this clone
        let mut close = false; // set by the ✕ button
        let mut step: isize = 0; // set by the ▲/▼ buttons (prev/next)
        let count = active.search_matches.len(); // hit count for the label
        // Human 1-based position (0 of 0 when there are no hits).
        let pos = if count == 0 { 0 } else { active.search_index + 1 };
        ctx.begin_pass(raw_input);
        egui::Window::new("rt_search")
            .title_bar(false) // a bare bar, not a draggable window
            .resizable(false)
            .anchor(egui::Align2::LEFT_BOTTOM, [8.0, -8.0]) // pinned near the bottom-left
            .show(&ctx, |ui| {
                ui.horizontal(|ui| {
                ui.label("Find:"); // prompt
                // The text field: single-line so Enter is handled by us (winit),
                // and auto-focused so typing lands here the moment the bar opens.
                let edit = egui::TextEdit::singleline(&mut query).desired_width(240.0);
                let resp = ui.add(edit);
                resp.request_focus(); // keep the caret in the field every frame
                ui.label(format!("{pos} / {count}")); // e.g. "3 / 12"
                // ASCII labels: egui's default font lacks the arrow/✕ glyphs.
                if ui.button("Prev").clicked() {
                    step = -1; // previous hit
                }
                if ui.button("Next").clicked() {
                    step = 1; // next hit
                }
                if ui.button("Close").clicked() {
                    close = true; // close the bar
                }
                ui.label("(Enter next, Shift+Enter prev, Esc close)"); // hint
            });
        });
        let output = ctx.end_pass();
        // If the query text changed, re-run the search from scratch.
        if query != active.search_query {
            active.search_query = query;
            Self::run_search(active, true); // new query → jump to the first hit
        } else if step != 0 {
            Self::search_step(active, step); // ▲/▼ navigation
        }
        if close {
            Self::close_search(active);
        }
        active.egui_state.handle_platform_output(&active.window, output.platform_output);
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

    /// Sample `/proc` (~2 Hz) to update each pane's CPU load — the heat
    /// instrument. Load is summed over the pane's session (shell + children), so
    /// whatever it's running counts. Returns whether it actually sampled this
    /// call (self-throttled); ported from rt-mux.
    fn sample_heat(active: &mut Active) -> bool {
        let now = Instant::now();
        let dt = now.duration_since(active.heat_last).as_secs_f32();
        if dt < 0.4 {
            return false; // throttle to ~2 Hz
        }
        active.heat_last = now;
        // Sum cumulative CPU ticks per session in one /proc scan.
        let mut by_session: std::collections::HashMap<u32, u64> = std::collections::HashMap::new();
        if let Ok(rd) = std::fs::read_dir("/proc") {
            for ent in rd.flatten() {
                let name = ent.file_name();
                let Some(s) = name.to_str() else { continue };
                if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                    continue; // only numeric pid dirs
                }
                let Ok(content) = std::fs::read_to_string(ent.path().join("stat")) else { continue };
                // comm (field 2) is parenthesised; parse fixed fields after the last ')'.
                let Some(rp) = content.rfind(')') else { continue };
                let toks: Vec<&str> = content[rp + 1..].split_whitespace().collect();
                if toks.len() < 13 {
                    continue; // [3]=session(6) [11]=utime(14) [12]=stime(15)
                }
                let session: u32 = toks[3].parse().unwrap_or(0);
                let utime: u64 = toks[11].parse().unwrap_or(0);
                let stime: u64 = toks[12].parse().unwrap_or(0);
                *by_session.entry(session).or_default() += utime + stime;
            }
        }
        const HZ: f32 = 100.0; // _SC_CLK_TCK on Linux
        for id in active.session.tree().all_panes() {
            let Some(pid) = active.session.pane(id).and_then(|p| p.pid()) else { continue };
            let ticks = by_session.get(&pid).copied().unwrap_or(0);
            let prev = active.heat_ticks.insert(id, ticks).unwrap_or(ticks);
            let load = ticks.saturating_sub(prev) as f32 / (dt * HZ); // fraction of one core
            let e = active.heat.entry(id).or_insert(0.0);
            *e = *e * 0.5 + load * 0.5; // smooth
        }
        true
    }

    /// Draw the per-pane output-activity instrument as an egui overlay: glowing
    /// green packets orbiting each pane's border, their speed and brightness
    /// scaled by that pane's live output rate (ported from rt-mux). Runs its own
    /// egui pass; called only when no dialog is up, so there is one pass a frame.
    fn paint_instruments(active: &mut Active) {
        // Advance the flow by real wall-clock time.
        let now = Instant::now();
        let dt = now.duration_since(active.last_meter_tick).as_secs_f32().min(0.1);
        active.last_meter_tick = now;
        for m in active.meters.values_mut() {
            let inst = m.wakeups as f32 / dt.max(1e-3);
            m.wakeups = 0;
            m.rate = m.rate * 0.75 + inst * 0.25;
            let act = (m.rate / BUSY_WAKEUPS).clamp(0.0, 1.0);
            m.phase = (m.phase + act * FLOW_MAX_LAPS * dt).fract();
        }
        // Pane rectangles in physical pixels.
        let size = active.window.inner_size();
        let bounds = Rect::new(0.0, 0.0, size.width as f32, size.height as f32);
        let rects = active.session.visible_rects(bounds);

        // egui pass: a background painter drawing the orbiting packets.
        let raw = active.egui_state.take_egui_input(&active.window);
        let ctx = active.egui_ctx.clone();
        ctx.begin_pass(raw);
        let ppp = ctx.pixels_per_point().max(0.5); // physical px → egui points
        {
            let painter =
                ctx.layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("rt_instr")));
            for (id, rect) in &rects {
                let m = active.meters.get(id).copied().unwrap_or_default();
                let act = (m.rate / BUSY_WAKEUPS).clamp(0.0, 1.0);
                let (x, y, w, h) = (rect.x / ppp, rect.y / ppp, rect.w / ppp, rect.h / ppp);
                // Heat: a blackbody-tinted border stroke (the pane's temperature),
                // drawn under the green output packets so one ring shows both.
                let load = active.heat.get(id).copied().unwrap_or(0.0);
                painter.rect_stroke(
                    egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h)),
                    egui::CornerRadius::ZERO,
                    egui::Stroke::new(2.4, heat_color32(load)),
                    egui::StrokeKind::Inside,
                );
                for k in 0..FLOW_PACKETS {
                    let t = (m.phase + k as f32 / FLOW_PACKETS as f32).fract();
                    let p = flow_point(x, y, w, h, t);
                    let a = 0.30 + 0.70 * act; // dim when idle, vivid when busy
                    let glow = egui::Color32::from_rgba_unmultiplied(0x28, 0xc0, 0x48, (a * 110.0) as u8);
                    let core = egui::Color32::from_rgba_unmultiplied(0x66, 0xff, 0x7a, (a * 255.0) as u8);
                    painter.circle_filled(p, 9.0, glow); // soft halo
                    painter.circle_filled(p, 3.4, core); // bright centre
                }
            }
        }
        let output = ctx.end_pass();
        active.egui_state.handle_platform_output(&active.window, output.platform_output);
        let ppp2 = output.pixels_per_point;
        let primitives = ctx.tessellate(output.shapes, ppp2);
        active.egui_painter.paint_and_update_textures(
            [size.width, size.height],
            ppp2,
            &primitives,
            &output.textures_delta,
        );
    }

    /// Re-run the current search query against the focused pane and replace the
    /// stored hits. When `jump` is true (the query just changed) it resets to the
    /// first hit and scrolls it into view; when false (a live refresh as new
    /// output arrives) it keeps the user's current hit position and does not
    /// scroll, so streaming output never yanks the viewport around.
    fn run_search(active: &mut Active, jump: bool) {
        let pane_id = active.session.focus(); // search the focused pane
        active.search_pane = Some(pane_id);
        active.search_matches = match active.session.pane(pane_id) {
            // Case-insensitive by default (the common expectation for find bars).
            Some(pane) => pane.search(&active.search_query, false),
            None => Vec::new(),
        };
        if jump {
            // Fresh query: start at the first hit and bring it into view.
            active.search_index = 0;
            if let Some(m) = active.search_matches.first() {
                if let Some(pane) = active.session.pane(pane_id) {
                    pane.scroll_to_line(m.line);
                }
            }
        } else {
            // Live refresh: keep the position valid without moving the view.
            active.search_index = active.search_index.min(active.search_matches.len().saturating_sub(1));
        }
        active.window.request_redraw();
    }

    /// Move to the next (`dir = 1`) or previous (`dir = -1`) search hit, wrapping
    /// around the ends, and scroll it into view. No-op when there are no hits.
    fn search_step(active: &mut Active, dir: isize) {
        let count = active.search_matches.len();
        if count == 0 {
            return; // nothing to step through
        }
        // Wrap with modular arithmetic (add count before %-ing so -1 wraps up).
        let next = ((active.search_index as isize + dir).rem_euclid(count as isize)) as usize;
        active.search_index = next;
        let line = active.search_matches[next].line; // the hit's absolute line
        if let Some(pane_id) = active.search_pane {
            if let Some(pane) = active.session.pane(pane_id) {
                pane.scroll_to_line(line); // centre it in the viewport
            }
        }
    }

    /// Close the search bar and clear its state (matches + query), then redraw so
    /// the highlights disappear.
    fn close_search(active: &mut Active) {
        active.search_open = false;
        active.search_query.clear();
        active.search_matches.clear();
        active.search_index = 0;
        active.search_pane = None;
        active.window.request_redraw();
    }
}

/// Planck/blackbody colour for a CPU load (fraction of one core): idle glows a
/// dim deep red, a busy core runs up through orange and yellow to white-hot, a
/// pathological load goes blue-white — load *is* temperature. Ported from rt-mux.
fn heat_color32(load: f32) -> egui::Color32 {
    let n = (load / 1.5).clamp(0.0, 1.0); // normalise (≥1.5 cores = max heat)
    let s = n * n * (3.0 - 2.0 * n); // smoothstep
    let kelvin = 1200.0 + s * 9000.0; // 1200K (dim red) .. 10200K (blue-white)
    let (r, g, b) = blackbody(kelvin);
    let bright = 0.30 + 0.70 * n; // idle dim, busy vivid
    egui::Color32::from_rgb((r * bright) as u8, (g * bright) as u8, (b * bright) as u8)
}

/// Approximate blackbody RGB (0..255) for a colour temperature in kelvin
/// (Tanner Helland's fit).
fn blackbody(kelvin: f32) -> (f32, f32, f32) {
    let t = (kelvin / 100.0).clamp(10.0, 400.0);
    let r = if t <= 66.0 { 255.0 } else { (329.698_73 * (t - 60.0).powf(-0.133_204_76)).clamp(0.0, 255.0) };
    let g = if t <= 66.0 {
        (99.470_8 * t.ln() - 161.119_57).clamp(0.0, 255.0)
    } else {
        (288.122_16 * (t - 60.0).powf(-0.075_514_85)).clamp(0.0, 255.0)
    };
    let b = if t >= 66.0 {
        255.0
    } else if t <= 19.0 {
        0.0
    } else {
        (138.517_73 * (t - 10.0).ln() - 305.044_8).clamp(0.0, 255.0)
    };
    (r, g, b)
}

/// The point at normalised position `t` (0..1, clockwise from the top-left)
/// around the perimeter of the rectangle `(x, y, w, h)` — used to place the
/// orbiting output-flow packets. All in egui points.
fn flow_point(x: f32, y: f32, w: f32, h: f32, t: f32) -> egui::Pos2 {
    let per = 2.0 * (w + h); // perimeter length
    let d = t.rem_euclid(1.0) * per; // distance travelled along it
    if d < w {
        egui::pos2(x + d, y) // top edge, left → right
    } else if d < w + h {
        egui::pos2(x + w, y + (d - w)) // right edge, top → bottom
    } else if d < 2.0 * w + h {
        egui::pos2(x + w - (d - w - h), y + h) // bottom edge, right → left
    } else {
        egui::pos2(x, y + h - (d - 2.0 * w - h)) // left edge, bottom → top
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
