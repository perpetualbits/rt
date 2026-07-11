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
mod bg_effect; // cross-compositor blur via ext-background-effect-v1 (no-op elsewhere)
mod x11_blur; // X11 background blur via _KDE_NET_WM_BLUR_BEHIND_REGION (no-op elsewhere)
mod clipboard; // cross-backend clipboard (Wayland smithay / X11 arboard)
mod input; // (also re-exported by lib.rs for tests; declared here for the bin)
mod manual; // the built-in manual overlay (F1)
mod menu; // right-click context menu (Terminator-style)
mod preferences; // egui preferences dialog
mod render; // the GL glyph-atlas renderer

use std::num::NonZeroU32; // required by glutin's surface resize API
use std::cell::RefCell; // shared jacks map between the spawn closure and Active
use std::ffi::CString; // mkfifo path
use std::fs::{File, OpenOptions}; // the fifo endpoints
use std::io::{Read, Write}; // pipe I/O
use std::os::unix::ffi::OsStrExt; // OsStr -> bytes for CString
use std::os::unix::fs::OpenOptionsExt; // custom_flags for O_NONBLOCK
use std::path::{Path, PathBuf}; // fifo paths
use std::rc::Rc; // shared jacks map
use std::time::{Duration, Instant}; // frame pacing for async PTY updates

use glutin::config::ConfigTemplateBuilder;
use glutin::context::ContextAttributesBuilder;
use glutin::display::GetGlDisplay;
use glutin::prelude::*; // brings the Gl* traits (make_current, get_proc_address, …)
use glutin::surface::{Surface, SurfaceAttributesBuilder, WindowSurface};
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
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
type AppSession = Session<TermPane, Box<dyn FnMut(rt_core::PaneId, usize, usize) -> TermPane>>;

/// The shared map of per-pane patch-bay jacks. Shared (`Rc<RefCell<…>>`) between
/// the spawn closure (which creates a pane's jacks) and [`Active`] (which pumps
/// and renders them); winit is single-threaded so this never contends.
type SharedJacks = Rc<RefCell<std::collections::HashMap<rt_core::PaneId, Jacks>>>;

/// Which of a pane's output streams a wire draws from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Stdout, // $RT_OUT — green
    Stderr, // $RT_ERR — red
}

/// One live patch-bay connection: bytes from `src`'s output stream jack are
/// written to `dst`'s input jack. Throughput drives the wire's flow animation.
struct Wire {
    src: rt_core::PaneId,
    stream: Stream,
    dst: rt_core::PaneId,
    rate: f32,  // smoothed bytes/second
    phase: f32, // flow position along the wire (laps)
    moved: u32, // bytes carried since the last tick
}

/// A pane's side-channel pipe endpoints — patch-bay jacks separate from the tty.
/// `$RT_OUT`/`$RT_ERR` a program writes to (rt reads); `$RT_IN` rt writes to (a
/// program reads). Held open `O_RDWR|O_NONBLOCK` so a program never blocks.
struct Jacks {
    out_path: PathBuf,
    err_path: PathBuf,
    in_path: PathBuf,
    out_read: File,
    err_read: File,
    in_write: File,
}

impl Jacks {
    /// Create the three fifos for pane `id` under `dir` and open rt's ends.
    fn new(dir: &Path, id: rt_core::PaneId) -> std::io::Result<Jacks> {
        let out_path = dir.join(format!("{}.out", id.0));
        let err_path = dir.join(format!("{}.err", id.0));
        let in_path = dir.join(format!("{}.in", id.0));
        mkfifo(&out_path)?;
        mkfifo(&err_path)?;
        mkfifo(&in_path)?;
        let open = |p: &Path| {
            OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(p)
        };
        Ok(Jacks {
            out_read: open(&out_path)?,
            err_read: open(&err_path)?,
            in_write: open(&in_path)?,
            out_path,
            err_path,
            in_path,
        })
    }

    /// The environment variables advertising these jacks to the pane's shell.
    fn env(&self) -> Vec<(String, String)> {
        vec![
            ("RT_OUT".to_string(), self.out_path.to_string_lossy().into_owned()),
            ("RT_ERR".to_string(), self.err_path.to_string_lossy().into_owned()),
            ("RT_IN".to_string(), self.in_path.to_string_lossy().into_owned()),
        ]
    }
}

impl Drop for Jacks {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.out_path);
        let _ = std::fs::remove_file(&self.err_path);
        let _ = std::fs::remove_file(&self.in_path);
    }
}

/// Create a fifo at `path` (mode 0600); an existing fifo is fine.
fn mkfifo(path: &Path) -> std::io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "fifo path has a NUL"))?;
    // SAFETY: `c` is a valid NUL-terminated C string for the call's duration.
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    if rc != 0 {
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::AlreadyExists {
            return Err(err);
        }
    }
    Ok(())
}

/// Base directory for the per-session patch-bay fifos. Prefer `$XDG_RUNTIME_DIR`
/// (per-user tmpfs, mode 0700, auto-removed by the session manager on logout);
/// fall back to the system temp dir when it isn't set.
fn jacks_base() -> PathBuf {
    std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .filter(|p| p.is_dir())
        .unwrap_or_else(std::env::temp_dir)
}

/// This process's patch-bay directory: `<base>/rt-<pid>`.
fn jacks_dir_for(pid: u32) -> PathBuf {
    jacks_base().join(format!("rt-{pid}"))
}

/// Remove this process's whole patch-bay directory (fifos + dir). Called right
/// before every `process::exit`, which skips the [`Jacks`] `Drop` — so without
/// this each session would leave its `rt-<pid>` dir behind.
fn cleanup_own_jacks() {
    let _ = std::fs::remove_dir_all(jacks_dir_for(std::process::id()));
}

/// Exit cleanly, removing our patch-bay directory first. Use this instead of a
/// bare `process::exit(0)` at the app's exit points.
fn exit_clean() -> ! {
    cleanup_own_jacks();
    std::process::exit(0);
}

/// On startup, sweep away `rt-<pid>` patch-bay dirs left by sessions that have
/// since died — a crash, a kill, or any exit that skipped cleanup. Scans both
/// the current base and the temp dir (to catch dirs from older builds that used
/// `/tmp`). A dir is removed only when no live process owns its pid, so a live
/// session's dir (or an unrelated process that reused the pid) is never touched.
fn sweep_stale_jacks() {
    let mut bases = vec![jacks_base(), std::env::temp_dir()];
    bases.dedup();
    let me = std::process::id();
    for base in bases {
        let Ok(entries) = std::fs::read_dir(&base) else { continue };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(pid) = name.to_str().and_then(|n| n.strip_prefix("rt-")).and_then(|p| p.parse::<u32>().ok())
            else {
                continue; // not an `rt-<pid>` dir
            };
            // Keep our own dir and any dir whose pid is still alive.
            if pid == me || Path::new(&format!("/proc/{pid}")).exists() {
                continue;
            }
            let _ = std::fs::remove_dir_all(entry.path()); // dead owner → reclaim (EPERM ignored)
        }
    }
}

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
/// Bytes/second across a wire that reads as "fully busy" for its flow animation.
const WIRE_BUSY_BYTES: f32 = 4096.0;
/// Packets travelling along a wire at once.
const WIRE_PACKETS: u32 = 3;

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
    menu: Option<(f32, f32)>,             // open context menu, at this window position (physical px)
    ime_preedit: bool,                    // true while an IME/dead-key composition is in progress
    clipboard: Option<clipboard::Clipboard>, // CLIPBOARD + PRIMARY (Wayland or X11); None if unavailable
    bg_effect: Option<bg_effect::BackgroundEffect>, // compositor background blur (None if protocol absent)
    x11_blur: x11_blur::X11Blur,          // X11 background blur (inert on Wayland / no x11 feature)
    selection: Option<Selection>,         // the current mouse text selection, if any
    selecting: bool,                      // true while the left button is held for a drag-select
    mouse_report: Option<(rt_core::PaneId, u16)>, // pane + xterm button code we're forwarding a press to
    hover_cell: Option<(rt_core::PaneId, usize, usize)>, // last cell reported to a 1003 (any-motion) app
    scroll_drag: Option<(rt_core::PaneId, f32, Rect)>, // dragging the scrollbar: (pane, grab-offset in thumb, grid rect)
    dragging_divider: Option<rt_core::DragHandle>, // the split divider being dragged, if any
    bell_flash: std::collections::HashMap<rt_core::PaneId, Instant>, // per-pane visible-bell expiry
    meters: std::collections::HashMap<rt_core::PaneId, Meter>, // per-pane output-activity instrument
    last_meter_tick: Instant,             // wall-clock of the last instrument advance
    heat: std::collections::HashMap<rt_core::PaneId, f32>, // per-pane CPU load (heat instrument)
    heat_ticks: std::collections::HashMap<rt_core::PaneId, u64>, // last session CPU ticks per pane
    heat_last: Instant,                   // wall-clock of the last /proc heat sample
    lat_phase: f32,                       // phase of the latency frame's undulation
    stall: f32,                           // latency-spike severity (decays); flares on a late wake
    last_wake: Instant,                   // wall-clock of the previous event-loop wake
    active_until: Instant,                // poll fast (animate) until here; set on input/output, else idle-throttle
    last_input: Instant,                  // last keystroke; the cursor soft-blinks for a bounded window after it
    poll_ms: u64,                         // the wake interval set last tick, so latency is judged against it
    scrollback: Rc<std::cell::Cell<usize>>, // live scrollback size for newly spawned panes
    jacks: SharedJacks,                   // per-pane patch-bay pipe jacks (shared with the spawn closure)
    wires: Vec<Wire>,                     // active patch-bay connections
    wiring_from: Option<(rt_core::PaneId, Stream)>, // the armed wire source, mid-gesture
    drag_cursor: Option<(f32, f32)>,      // live cursor (physical px) while dragging a wire
    last_click: Option<(Instant, (f32, f32))>, // time + position of the last left-press
    click_count: u8,                      // 1=single, 2=double (word), 3=triple (line)
    egui_ctx: egui::Context,              // egui immediate-mode context (chrome/dialogs)
    egui_state: egui_winit::State,        // egui-winit input bridge
    egui_painter: egui_glow::Painter,     // egui_glow renderer (shares our GL context)
    prefs_open: bool,                     // whether the preferences dialog is showing
    manual_open: bool,                    // whether the built-in manual overlay is showing
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

/// Command-line overrides, parsed once in `main`. All optional: an unset field
/// falls back to the persisted config (font) or the default window size (grid).
/// The point is scriptability — pinning the grid/font from a benchmark harness
/// the same way `alacritty -o window.dimensions.columns=…` does.
#[derive(Clone, Default)]
struct Cli {
    cols: Option<usize>,   // --cols N : pin the initial grid width in cells
    rows: Option<usize>,   // --rows N : pin the initial grid height in cells
    font: Option<String>,  // --font "Family" : override the configured font family
    font_size: Option<f32>, // --font-size PX : override the configured size (pixels)
}

/// Parse `Cli` from `std::env::args`. Hand-rolled (no clap dependency): accepts
/// `--flag value` and `--flag=value`. On `-h`/`--help` it prints usage and
/// exits 0; on a malformed/unknown flag it prints the error and exits 2.
fn parse_cli() -> Cli {
    let mut cli = Cli::default();
    let mut args = std::env::args().skip(1); // skip argv[0]
    // Pull the value for a flag, whether it came inline (`--k=v`) or split
    // (`--k v`). Errors out with a clear message when a value is required.
    while let Some(arg) = args.next() {
        // Split a `--key=value` form into ("--key", Some("value")).
        let (key, inline) = match arg.split_once('=') {
            Some((k, v)) => (k.to_string(), Some(v.to_string())),
            None => (arg.clone(), None),
        };
        // Fetch this flag's value from the inline part or the next argument.
        let mut value = |flag: &str| -> String {
            inline.clone().or_else(|| args.next()).unwrap_or_else(|| {
                eprintln!("rt: {flag} needs a value");
                std::process::exit(2);
            })
        };
        match key.as_str() {
            "--cols" => cli.cols = Some(parse_usize(&value("--cols"), "--cols")),
            "--rows" => cli.rows = Some(parse_usize(&value("--rows"), "--rows")),
            "--font" => cli.font = Some(value("--font")),
            "--font-size" => cli.font_size = Some(parse_f32(&value("--font-size"), "--font-size")),
            "-V" | "--version" => {
                // Crate version, plus the git commit stamped in by build.rs on a
                // from-source build (e.g. "rt 0.2.1 (a1b2c3d)") so a dev build is
                // never mistaken for the release it sits ahead of. option_env!
                // keeps this compiling even if the build script didn't run.
                let v = env!("CARGO_PKG_VERSION");
                match option_env!("RT_GIT_DESC") {
                    Some(g) if !g.is_empty() => println!("rt {v} ({g})"),
                    _ => println!("rt {v}"),
                }
                std::process::exit(0);
            }
            "-h" | "--help" => {
                println!(
                    "rt — Wayland-native terminal multiplexer\n\n\
                     Usage: rt [OPTIONS]\n\n\
                     Options:\n  \
                     --cols N          pin the initial grid width (cells)\n  \
                     --rows N          pin the initial grid height (cells)\n  \
                     --font \"Family\"   override the configured font family\n  \
                     --font-size PX    override the configured font size (pixels)\n  \
                     -V, --version     print version and exit\n  \
                     -h, --help        show this message\n\n\
                     Everything else is configured in Preferences / config.toml."
                );
                std::process::exit(0);
            }
            other => {
                eprintln!("rt: unknown option '{other}' (try --help)");
                std::process::exit(2);
            }
        }
    }
    cli
}

/// Parse a `usize` CLI value or exit(2) with a clear message.
fn parse_usize(s: &str, flag: &str) -> usize {
    s.parse().unwrap_or_else(|_| {
        eprintln!("rt: {flag} expects a whole number, got '{s}'");
        std::process::exit(2);
    })
}

/// Parse an `f32` CLI value or exit(2) with a clear message.
fn parse_f32(s: &str, flag: &str) -> f32 {
    s.parse().unwrap_or_else(|_| {
        eprintln!("rt: {flag} expects a number, got '{s}'");
        std::process::exit(2);
    })
}

/// The winit application object. Holds only the font bytes until `resumed`
/// builds the `Active` state.
struct App {
    font_db: std::sync::Arc<fontdb::Database>, // system fonts, for family lookup + the picker
    mono_families: Vec<String>,                // monospace family names for the preferences combo
    cli: Cli,                                  // command-line overrides (grid/font)
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

        // Load persisted settings (before the renderer, so fonts/colours come
        // from the config). Env vars override for demos/screenshots. Loaded here
        // — ahead of the window — so `--cols`/`--rows` can pre-size it from the
        // configured font metrics and titlebar setting.
        let mut settings = rt_config::Config::load().settings;
        if let Ok(v) = std::env::var("RT_OPACITY") {
            if let Ok(o) = v.parse::<f32>() {
                settings.background_opacity = o.clamp(rt_config::Settings::MIN_OPACITY, 1.0);
            }
        }
        if let Ok(v) = std::env::var("RT_FOCUS") {
            settings.focus_follows_mouse = matches!(v.as_str(), "sloppy" | "mouse" | "follow" | "1");
        }
        // CLI overrides win over the persisted config (and over the env knobs
        // above), so a benchmark harness can pin the font without editing config.
        if let Some(family) = &self.cli.font {
            settings.font_family = family.clone();
        }
        if let Some(px) = self.cli.font_size {
            settings.font_size = px.max(1.0); // guard against a zero/negative size
        }

        // Font chains for the configured family (system fonts via fontdb). Kept
        // in `Active` so a live font change can reload them.
        let font_blobs = font_blobs(&self.font_db, &settings.font_family);

        // --- create the window and choose a GL config --------------------
        // With BOTH `--cols` and `--rows`, pre-size the window so the initial
        // (single, full-window) pane lands on that exact grid — for apples-to-
        // apples benchmarking against terminals launched with `--geometry=COLSxROWS`.
        // Without them, keep a sensible default. `cell_size_for` measures the
        // cell without a GL context (the renderer doesn't exist yet).
        let initial_size: winit::dpi::Size = match (self.cli.cols, self.cli.rows) {
            (Some(cols), Some(rows)) if cols > 0 && rows > 0 => {
                let cell = render::cell_size_for(&font_blobs, settings.font_size);
                window_size_for_grid(cols, rows, cell, settings.show_titlebar).into()
            }
            _ => winit::dpi::LogicalSize::new(960.0, 600.0).into(),
        };
        // Wayland app_id / X11 WM_CLASS. This MUST equal the installed desktop
        // entry's basename (io.github.perpetualbits.rt.desktop) so the compositor
        // can bind our icon to the window — without it, no icon shows on Wayland
        // no matter what's installed. Both winit ext traits write the same
        // `platform_specific.name` field, so one call covers both backends
        // (Wayland uses `general` as the app_id; X11 uses it as the WM_CLASS
        // class). See extra/linux/ for the desktop entry + icon.
        use winit::platform::wayland::WindowAttributesExtWayland;
        const APP_ID: &str = "io.github.perpetualbits.rt";
        let window_attrs = Window::default_attributes()
            .with_title("rt") // window title; per-pane titles update it later
            .with_name(APP_ID, "rt") // app_id (Wayland) / WM_CLASS (X11) → icon binding
            .with_transparent(true) // REQUIRED for the compositor to honour our alpha
            .with_inner_size(initial_size);
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

        // --- build the renderer ------------------------------------------
        let mut renderer = match Renderer::new(gl.clone(), &font_blobs, settings.font_size) {
            Ok(r) => r,
            Err(e) => {
                log::error!("renderer init failed: {e}");
                event_loop.exit();
                return;
            }
        };
        // Ask KWin to blur behind us (true background blur on KDE). No-op
        // elsewhere (COSMIC/GNOME/sway use the ext protocol below, or nothing).
        blur::try_enable_kwin_blur(&window);
        // Cross-compositor blur via the ext-background-effect-v1 staging protocol
        // (KDE 6.7+, COSMIC, niri). Only worth requesting while the background is
        // translucent — blur behind an opaque surface is wasted compositor work.
        // None on compositors without the protocol (the window is just translucent).
        let bg_effect = bg_effect::BackgroundEffect::try_init(&window, want_blur(&settings));
        // X11 counterpart: the _KDE_NET_WM_BLUR_BEHIND_REGION property (KWin-X11,
        // picom). Inert on Wayland and on a no-x11 build.
        let x11_blur = x11_blur::X11Blur::try_init(&window, want_blur(&settings));

        // Enable IME so dead keys / compose sequences (´+o→ó, ~+n→ñ, …) and full
        // IMEs work: composed text arrives via WindowEvent::Ime(Commit).
        window.set_ime_allowed(true);

        // Clipboard (+ PRIMARY selection), tied to the window's display. Picks the
        // Wayland (smithay) or X11 (arboard, `x11` feature) backend from the raw
        // display handle; None on a backend we don't support.
        let clipboard = window
            .display_handle()
            .ok()
            .and_then(|h| clipboard::Clipboard::from_display(h.as_raw()));

        // Size the renderer/viewport to the window's physical pixels.
        let size = window.inner_size(); // physical pixel size
        renderer.resize(size.width as f32, size.height as f32);
        let cell = renderer.cell_size(); // (cell_w, cell_h) in pixels

        // --- build the session with real PTY panes -----------------------
        let bounds = content_bounds(size);
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
        // Patch-bay: a per-session temp dir holds the pipe fifos, and a shared map
        // holds each pane's jacks — filled by the spawn closure, read by Active.
        let jacks_dir = jacks_dir_for(std::process::id());
        let _ = std::fs::create_dir_all(&jacks_dir);
        let jacks: SharedJacks = Rc::new(RefCell::new(std::collections::HashMap::new()));
        let jacks_spawn = jacks.clone();
        // Live scrollback size shared with the spawn factory, so changing it in
        // Preferences takes effect for the next terminal without a restart.
        let scrollback = Rc::new(std::cell::Cell::new(settings.scrollback));
        let scrollback_spawn = scrollback.clone();
        // The factory spawns a shell-backed pane at the requested cell size. A
        // spawn failure here is fatal (no PTY available) — we surface it by
        // panicking with a clear message rather than rendering a broken pane.
        let spawn: Box<dyn FnMut(rt_core::PaneId, usize, usize) -> TermPane> = Box::new(move |id, cols, rows| {
            // If RT_EXEC is set, run it then keep an interactive shell open.
            let shell = exec.as_ref().map(|cmd| {
                ("/bin/sh".to_string(), vec!["-c".to_string(), format!("{cmd}; exec /bin/sh -i")])
            });
            // Create this pane's patch-bay jacks and advertise them to the shell.
            let env = match Jacks::new(&jacks_dir, id) {
                Ok(j) => {
                    let e = j.env();
                    jacks_spawn.borrow_mut().insert(id, j);
                    e
                }
                Err(_) => Vec::new(), // no jacks: the pane still runs, just unwireable
            };
            let mut pane = TermPane::spawn_env(shell, None, cols.max(1), rows.max(1), &env, scrollback_spawn.get())
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
            bg_effect,
            x11_blur,
            selection: None,
            selecting: false,
            mouse_report: None,
            hover_cell: None,
            scroll_drag: None,
            dragging_divider: None,
            bell_flash: std::collections::HashMap::new(),
            meters: std::collections::HashMap::new(),
            last_meter_tick: Instant::now(),
            heat: std::collections::HashMap::new(),
            heat_ticks: std::collections::HashMap::new(),
            heat_last: Instant::now(),
            lat_phase: 0.0,
            stall: 0.0,
            last_wake: Instant::now(),
            active_until: Instant::now(),
            last_input: Instant::now(),
            poll_ms: 16,
            scrollback,
            jacks,
            wires: Vec::new(),
            wiring_from: None,
            drag_cursor: None,
            last_click: None,
            click_count: 0,
            egui_ctx,
            egui_state,
            egui_painter,
            prefs_open: false,
            manual_open: false,
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
        // Debug/verification hook: RT_MANUAL opens the manual overlay at startup.
        if std::env::var("RT_MANUAL").is_ok() {
            if let Some(active) = self.active.as_mut() {
                active.manual_open = true;
            }
        }
        // Debug/verification hook: RT_MENU opens the context menu at startup so
        // its rendering can be screenshotted without synthetic mouse input.
        if std::env::var("RT_MENU").is_ok() {
            if let Some(active) = self.active.as_mut() {
                active.menu = Some((200.0, 150.0)); // fixed, visible spot
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
        // Debug/verification hook: RT_WIRE_DEMO builds a live patch-bay scene —
        // split, wire pane1.stdout → pane2.stdin, and run a producer + reader — so
        // the wiring can be verified/screenshotted without synthetic input.
        if std::env::var("RT_WIRE_DEMO").is_ok() {
            if let Some(active) = self.active.as_mut() {
                let p1 = active.session.focus();
                active.session.apply(rt_config::Action::SplitVert); // → pane2
                active.session.apply(rt_config::Action::SplitVert); // → pane3 (focused)
                let p3 = active.session.focus();
                // Wire the leftmost pane's stdout to the rightmost pane's stdin so
                // the bezier arcs over the middle pane.
                Self::connect_wire(active, p1, Stream::Stdout, p3);
                if let Some(pane) = active.session.pane(p1) {
                    pane.write(b"while true; do echo tick $(date +%T); sleep 0.4; done | tee $RT_OUT\n");
                }
                if let Some(pane) = active.session.pane(p3) {
                    pane.write(b"cat $RT_IN\n");
                }
            }
        }
        // Poll so we keep re-checking PTYs for async output even without input.
        event_loop.set_control_flow(ControlFlow::Poll);
        if let Some(active) = &self.active {
            active.window.request_redraw(); // first paint
        }
    }

    /// Handle a window event: close, resize, key input, redraw.
    fn window_event(&mut self, _event_loop: &ActiveEventLoop, _id: WindowId, event: WindowEvent) {
        // Everything here needs the active state; ignore events before resume.
        let Some(active) = self.active.as_mut() else { return };
        // Any real interaction (not our own repaint) keeps the loop in the fast,
        // animating poll for a moment; when nothing happens it idle-throttles.
        if !matches!(event, WindowEvent::RedrawRequested) {
            active.active_until = Instant::now() + ACTIVE_TAIL;
        }

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

        // The manual overlay behaves like the preferences dialog: egui gets events
        // (for scrolling), Esc or F1 closes it, and terminal input is suspended.
        if active.manual_open {
            let r = active.egui_state.on_window_event(&active.window, &event);
            if r.repaint {
                active.window.request_redraw();
            }
            match event {
                WindowEvent::KeyboardInput { event: ref ke, .. }
                    if ke.state == ElementState::Pressed
                        && matches!(
                            ke.logical_key,
                            Key::Named(NamedKey::Escape) | Key::Named(NamedKey::F1)
                        ) =>
                {
                    active.manual_open = false;
                    active.window.request_redraw();
                    return;
                }
                WindowEvent::KeyboardInput { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::Ime(_)
                | WindowEvent::ModifiersChanged(_) => return,
                _ => {}
            }
        }

        // The context menu is an egui overlay too: egui gets first look (for
        // hover/click), Escape or a click outside closes it, and terminal input is
        // suspended while it is up.
        if active.menu.is_some() {
            let r = active.egui_state.on_window_event(&active.window, &event);
            if r.repaint {
                active.window.request_redraw();
            }
            match event {
                WindowEvent::KeyboardInput { event: ref ke, .. }
                    if ke.state == ElementState::Pressed
                        && matches!(ke.logical_key, Key::Named(NamedKey::Escape)) =>
                {
                    active.menu = None;
                    active.window.request_redraw();
                    return;
                }
                WindowEvent::KeyboardInput { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::CursorMoved { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::Ime(_)
                | WindowEvent::ModifiersChanged(_) => return,
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
            // The user closed the window (title-bar button / compositor). Exit
            // the same way the last-pane-closed path does — `process::exit`,
            // NOT `event_loop.exit()`. The latter unwinds and Drops the GL
            // context, egui_glow painter and Wayland blur objects, whose teardown
            // ordering faults (a segfault on Wayland, an X11 GetGeometry panic on
            // the x11 dev build). The OS reclaims everything; the PTY children get
            // SIGHUP. This matches SessionEvent::CloseWindow below.
            WindowEvent::CloseRequested => exit_clean(),

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
                // Keep the blur region covering the whole surface at its new size.
                if let Some(fx) = &mut active.bg_effect {
                    fx.on_resize(size.width, size.height);
                }
                // Recompute pane sizes from the new window bounds.
                let bounds = content_bounds(size);
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
                    // If the app under the pointer wants the wheel (and Shift
                    // isn't held to force scrollback), send it one report per
                    // notch instead of scrolling rt's history.
                    if !active.mods.shift_key() {
                        let up = lines > 0;
                        let notches = lines.unsigned_abs().min(8); // cap runaway touchpad deltas
                        let mut forwarded = false;
                        for _ in 0..notches {
                            if Self::forward_mouse(active, MouseReport::Scroll(up), active.mouse.0, active.mouse.1) {
                                forwarded = true;
                            } else {
                                break; // pane doesn't want the mouse: fall through to scrollback
                            }
                        }
                        if forwarded {
                            active.window.request_redraw();
                            return;
                        }
                    }
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
                if active.scroll_drag.is_some() {
                    // Dragging the scrollbar thumb: scroll to track the pointer.
                    Self::apply_scroll_drag(active, active.mouse.1);
                } else if let Some((_, btn)) = active.mouse_report {
                    // A forwarded button is held: report motion to the app as a
                    // drag (Shift still suspends forwarding, e.g. to select).
                    if !active.mods.shift_key() {
                        Self::forward_mouse(active, MouseReport::Drag(btn), active.mouse.0, active.mouse.1);
                        active.window.request_redraw();
                    }
                } else if active.wiring_from.is_some() {
                    // Rubber-band the in-progress wire to the cursor.
                    active.drag_cursor = Some(active.mouse);
                    active.window.request_redraw();
                } else if let Some(handle) = active.dragging_divider.clone() {
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
                } else if !active.mods.shift_key() && Self::forward_hover(active) {
                    // The app under the pointer wants bare motion (mode 1003): it
                    // owns the hover (Shift suspends forwarding). Nothing else here.
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
                    // Right-click cancels a pending wire, or disconnects the output
                    // jack under the cursor (before falling through to the menu).
                    if active.wiring_from.take().is_some() {
                        active.drag_cursor = None;
                        active.window.request_redraw();
                        return;
                    }
                    if let Some((id, stream)) = Self::jack_at(active, active.mouse.0, active.mouse.1) {
                        active.wires.retain(|w| !(w.src == id && w.stream == stream));
                        active.window.request_redraw();
                        return;
                    }
                    // A mouse-reporting app gets the right-press (Shift held opens
                    // rt's menu instead, the standard terminal override).
                    if !active.mods.shift_key()
                        && Self::forward_mouse(active, MouseReport::Press(2), active.mouse.0, active.mouse.1)
                    {
                        active.session.focus_at(active.mouse.0, active.mouse.1);
                        active.window.request_redraw();
                        return;
                    }
                    log::debug!("right-click at {:?} → open menu", active.mouse);
                    // Focus the pane under the cursor first, so the menu's
                    // actions apply to the pane you right-clicked. The menu itself
                    // is an egui overlay anchored here; egui clamps it on-screen.
                    active.session.focus_at(active.mouse.0, active.mouse.1);
                    active.menu = Some(active.mouse);
                    active.window.request_redraw();
                }
                (ElementState::Pressed, MouseButton::Left) => {
                    {
                        let size = active.window.inner_size();
                        let bounds = content_bounds(size);
                        let (mx, my) = active.mouse;
                        // A press on an output jack starts a drag-to-wire. Checked
                        // *before* the divider, since jacks sit on the pane edge
                        // (which is also the divider) and should win.
                        if let Some((src, stream)) = Self::jack_at(active, mx, my) {
                            active.wiring_from = Some((src, stream));
                            active.drag_cursor = Some((mx, my));
                            active.window.request_redraw();
                            return;
                        }
                        // A press on a split divider starts a drag-to-resize
                        // (checked before tabs/focus/selection).
                        if let Some(handle) = active.session.divider_at(mx, my, bounds) {
                            active.dragging_divider = Some(handle);
                            return;
                        }
                        // A press on the scrollbar starts a thumb-drag. Grabbing
                        // the thumb keeps the pointer's offset within it; clicking
                        // the track jumps the thumb's centre to the pointer.
                        if let Some((pid, srect, thumb_y, thumb_h)) = Self::scrollbar_at(active, mx, my) {
                            let grab = if my >= thumb_y && my <= thumb_y + thumb_h {
                                my - thumb_y // grabbed the thumb: preserve the offset
                            } else {
                                thumb_h * 0.5 // clicked the track: centre the thumb here
                            };
                            active.scroll_drag = Some((pid, grab, srect));
                            active.session.focus_at(mx, my); // focus the pane we're scrolling
                            Self::apply_scroll_drag(active, my); // jump immediately
                            active.window.request_redraw();
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
                            // If the pane's app wants the mouse, forward the press
                            // to it instead of starting a selection (Shift held =
                            // override, so you can always select over the app).
                            if !active.mods.shift_key()
                                && Self::forward_mouse(active, MouseReport::Press(0), mx, my)
                            {
                                active.window.request_redraw();
                                return;
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
                    // If the matching press was forwarded to an app, forward the
                    // release too and we're done.
                    if Self::end_mouse_report(active) {
                        return;
                    }
                    // Completing a drag-to-wire: connect to the pane under the cursor.
                    if let Some((src, stream)) = active.wiring_from.take() {
                        if let Some(dst) = Self::pane_at(active, active.mouse.0, active.mouse.1) {
                            Self::connect_wire(active, src, stream, dst);
                        }
                        active.drag_cursor = None;
                        active.window.request_redraw();
                        return;
                    }
                    active.dragging_divider = None; // end any divider resize
                    active.scroll_drag = None; // end any scrollbar drag
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
                    // A mouse-reporting app gets the middle-press; otherwise (or
                    // with Shift held) middle-click pastes the PRIMARY selection.
                    if !active.mods.shift_key()
                        && Self::forward_mouse(active, MouseReport::Press(1), active.mouse.0, active.mouse.1)
                    {
                        active.window.request_redraw();
                        return;
                    }
                    // Middle-click pastes the PRIMARY selection (X/Wayland idiom).
                    if let Some(cb) = &active.clipboard {
                        if let Ok(text) = cb.load_primary() {
                            if !text.is_empty() {
                                active.session.feed_input(text.as_bytes());
                            }
                        }
                    }
                }
                (ElementState::Released, MouseButton::Right)
                | (ElementState::Released, MouseButton::Middle) => {
                    // Forward the button-up to the app if its press was forwarded.
                    Self::end_mouse_report(active);
                }
                _ => {} // other buttons / states: nothing
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
        // Latency instrument: the loop is scheduled to wake ~every 16ms; a wake
        // that arrives much later means a CPU hogger stole the frame. Measure the
        // overrun, flare the frame proportionally, and breathe the undulation.
        let now = Instant::now();
        let wake_dt = now.duration_since(active.last_wake).as_secs_f32().min(0.5);
        active.last_wake = now;
        // Compare against the interval we actually scheduled last tick, not a
        // fixed 16ms — otherwise an intentional idle wake (100ms apart) would be
        // misread as a stolen frame and flare the latency instrument forever,
        // which would in turn force repaints and defeat the throttle.
        let budget = active.poll_ms as f32 / 1000.0;
        let overrun = wake_dt - budget;
        if active.poll_ms <= 16 && overrun > 0.010 {
            active.stall = active.stall.max((overrun / 0.05).clamp(0.0, 1.0)); // keep the worst recent hitch
        }
        active.lat_phase = (active.lat_phase + 0.12 * wake_dt).fract(); // calm breath
        active.stall *= (-wake_dt / 0.6).exp(); // decay toward calm
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
                            // Visible bell: briefly stripe the border of just this pane.
                            active.bell_flash.insert(id, now + Duration::from_millis(80));
                            active.active_until = now + Duration::from_millis(120); // repaint to show then clear it
                            dirty = true;
                        }
                        _ => {
                            // Wakeup → new output; count it for the pane's border
                            // flow instrument, keep the fast poll, and schedule a redraw.
                            active.meters.entry(id).or_default().wakeups += 1;
                            active.active_until = now + ACTIVE_TAIL;
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
                    exit_clean(); // remove our patch-bay dir, then exit
                }
                _ => dirty = true, // a pane closed; repaint the survivors
            }
            active.meters.remove(&id); // forget the closed pane's instrument state
            active.heat.remove(&id);
            active.heat_ticks.remove(&id);
            active.jacks.borrow_mut().remove(&id); // Drop -> remove its fifos
            active.wires.retain(|w| w.src != id && w.dst != id); // unplug its wires
            if matches!(active.wiring_from, Some((s, _)) if s == id) {
                active.wiring_from = None;
            }
        }
        // Move bytes across the patch-bay wires; repaint if anything flowed or is
        // still flowing (so the wire packets animate).
        if Self::pump_wires(active) || active.wires.iter().any(|w| w.rate > 1.0) {
            dirty = true;
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
        // And while a latency flare is fading, so the spike animates out.
        if active.stall > 0.02 {
            dirty = true;
        }
        // While the search bar is open, keep its results current as new output
        // streams into the searched pane (so a running command's fresh lines are
        // matched live). Re-run only on a change, and only when the query is set.
        if dirty && active.search_open && !active.search_query.is_empty() {
            Self::run_search(active, false); // live refresh; keep position + view
        }
        // The focused cursor soft-blinks for a bounded window after the last
        // keystroke; keep repainting (cheaply) so the fade animates, then stop.
        let blinking = active.last_input.elapsed() < Duration::from_secs_f32(CURSOR_BLINK_PERIOD * CURSOR_BLINK_CYCLES);
        if blinking {
            dirty = true;
        }
        if dirty {
            active.window.request_redraw(); // schedule a paint
        }
        // Idle-throttle: animate at ~60fps only while there's recent input/output
        // (so instruments move and interaction is crisp); a pulsing cursor uses a
        // cheaper ~20Hz; otherwise fall back to a ~10Hz poll that still catches
        // async output and heat but lets the CPU sleep. The chosen interval is
        // remembered so the next tick judges latency against it (see the budget).
        let interval = if Instant::now() < active.active_until {
            ACTIVE_POLL
        } else if blinking {
            BLINK_POLL
        } else {
            IDLE_POLL
        };
        active.poll_ms = interval.as_millis() as u64;
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + interval));
    }
}

impl App {
    /// Run a semantic [`Action`](rt_config::Action) against the live state. This
    /// is the single place actions are executed, called both by keybindings and
    /// by the context menu, so the two can never drift apart.
    ///
    /// Window-level appearance actions (opacity) are handled here because
    /// the session owns no window handle; everything else goes to the session.
    /// A `CloseWindow` result exits the process (the OS reaps the child PTYs).
    /// Keyboard wire gesture: with nothing armed, arm from the focused pane's
    /// `stream` jack; with something armed, complete to the focused pane's input.
    fn wire_gesture(active: &mut Active, stream: Stream) {
        let focus = active.session.focus();
        match active.wiring_from.take() {
            None => active.wiring_from = Some((focus, stream)), // arm
            Some((src, s)) => Self::connect_wire(active, src, s, focus), // complete
        }
        active.window.request_redraw();
    }

    /// Which output jack (if any) the physical-pixel point `(mx, my)` hits: the
    /// stdout jack sits at the right edge upper third, the stderr jack lower third.
    fn jack_at(active: &Active, mx: f32, my: f32) -> Option<(rt_core::PaneId, Stream)> {
        let size = active.window.inner_size();
        let bounds = content_bounds(size);
        const R: f32 = 12.0; // grab radius in px
        for (id, r) in active.session.visible_rects(bounds) {
            let (ox, oy) = (r.x + r.w, r.y + r.h / 3.0);
            if (mx - ox).hypot(my - oy) <= R {
                return Some((id, Stream::Stdout));
            }
            let (ex, ey) = (r.x + r.w, r.y + 2.0 * r.h / 3.0);
            if (mx - ex).hypot(my - ey) <= R {
                return Some((id, Stream::Stderr));
            }
        }
        None
    }

    /// The pane whose rectangle contains the physical-pixel point `(mx, my)`.
    fn pane_at(active: &Active, mx: f32, my: f32) -> Option<rt_core::PaneId> {
        let size = active.window.inner_size();
        let bounds = content_bounds(size);
        active
            .session
            .visible_rects(bounds)
            .into_iter()
            .find(|(_, r)| r.contains(mx, my))
            .map(|(id, _)| id)
    }

    /// Add a wire `src`.`stream` → `dst`.stdin if both panes have jacks and it is
    /// not a self-loop.
    fn connect_wire(active: &mut Active, src: rt_core::PaneId, stream: Stream, dst: rt_core::PaneId) {
        let jm = active.jacks.borrow();
        if src != dst && jm.contains_key(&src) && jm.contains_key(&dst) {
            drop(jm);
            active.wires.push(Wire { src, stream, dst, rate: 0.0, phase: 0.0, moved: 0 });
        }
    }

    fn apply_action(active: &mut Active, action: rt_config::Action) {
        use rt_config::Action;
        match action {
            Action::OpacityUp => {
                let v = active.settings.adjust_opacity(0.05); // +5% opaque
                log::info!("background opacity = {v:.2}");
                Self::persist(&active.settings); // remember across restarts
                // Blur is pointless once fully opaque; drop it as we cross 1.0.
                apply_blur(active);
                active.window.request_redraw();
            }
            Action::OpacityDown => {
                let v = active.settings.adjust_opacity(-0.05); // more see-through
                log::info!("background opacity = {v:.2}");
                Self::persist(&active.settings);
                // Now translucent → (re)enable blur if the config allows it.
                apply_blur(active);
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
            // Patch-bay: arm a wire from the focused pane's stream jack, or (if one
            // is already armed) complete it to the focused pane's input.
            Action::WireStdout => Self::wire_gesture(active, Stream::Stdout),
            Action::WireStderr => Self::wire_gesture(active, Stream::Stderr),
            Action::Unwire => {
                let f = active.session.focus();
                active.wires.retain(|w| w.src != f && w.dst != f);
                active.window.request_redraw();
            }
            Action::Manual => {
                active.manual_open = !active.manual_open; // toggle the manual overlay
                active.window.request_redraw();
            }
            Action::PipeInto => {
                // Split, then wire the (old) focused pane's stdout into the new one.
                let src = active.session.focus();
                if let Some(SessionEvent::Redraw) = active.session.apply(rt_config::Action::SplitVert) {
                    let dst = active.session.focus();
                    Self::connect_wire(active, src, Stream::Stdout, dst);
                }
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
                Some(SessionEvent::CloseWindow) => exit_clean(), // last pane closed; clean up first
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
        let bounds = content_bounds(size);
        let (cw, ch) = active.renderer.cell_size();
        for (id, rect) in active.session.visible_rects(bounds) {
            if rect.contains(mx, my) {
                // Map against the content rect (grid area minus the titlebar); a
                // click in the titlebar strip is above the content and yields no
                // cell.
                let content = active.session.content_rect(rect);
                if my < content.y {
                    return None; // in the titlebar, not the grid
                }
                let dx = (mx - content.x) / cw; // offset in cells from the content origin
                let dy = (my - content.y) / ch;
                if active.session.columns_of(id) > 1 {
                    // Newspaper columns: invert the renderer's tiling to the cell
                    // in the tall count*rows viewport the app actually sees, so
                    // mouse reports (wheel, clicks) reach the app in column mode.
                    let (col, row) = active.session.column_layout(id, rect).cell_at(dx, dy);
                    return Some((id, col, row));
                }
                let col = dx.max(0.0) as usize; // cell column
                let row = dy.max(0.0) as usize; // cell row
                return Some((id, col, row));
            }
        }
        None
    }

    /// Forward a pointer action to the application in the pane under `(mx, my)`,
    /// but only if that app has enabled mouse reporting. Returns `true` when it
    /// consumed the event — the caller then stands rt's own chrome (selection,
    /// scrollback, menu) down. Callers gate this on Shift NOT being held, so
    /// Shift is the override that always gives you rt's selection/scroll.
    fn forward_mouse(active: &mut Active, report: MouseReport, mx: f32, my: f32) -> bool {
        // Which pane + 0-based cell is under the pointer? (None over a gutter or
        // titlebar, or a multi-column pane cell_at declines to map.)
        let (pane_id, col0, row0) = match Self::cell_at(active, mx, my) {
            Some(t) => t,
            None => return false,
        };
        // Encode + write inside a block so the pane borrow ends before we touch
        // `active.mouse_report` (a disjoint field, but keep the borrows tidy).
        let wrote = {
            let Some(pane) = active.session.pane(pane_id) else { return false };
            if !pane.wants_mouse() {
                return false; // app hasn't requested the mouse: let rt handle it
            }
            let col = (col0 as u16).saturating_add(1); // xterm coordinates are 1-based
            let row = (row0 as u16).saturating_add(1);
            match encode_mouse(report, col, row, &active.mods, pane.mouse_sgr()) {
                Some(bytes) => {
                    pane.write(&bytes); // send the report to the app's stdin
                    true
                }
                None => false,
            }
        };
        if wrote {
            // Remember a pressed button so motion reports as a drag and the
            // release reaches the same pane; a release clears the memory.
            match report {
                MouseReport::Press(b) => active.mouse_report = Some((pane_id, b)),
                MouseReport::Release(_) => active.mouse_report = None,
                _ => {}
            }
        }
        wrote
    }

    /// If a forwarded button press is still outstanding, forward its release to
    /// the app and clear the state. Returns `true` if it handled the release.
    /// Sends the release to the pane under the cursor, or — if the pointer has
    /// left that pane — to the pane that got the press, so the app never misses
    /// a button-up.
    fn end_mouse_report(active: &mut Active) -> bool {
        let Some((pid, btn)) = active.mouse_report.take() else { return false };
        if !Self::forward_mouse(active, MouseReport::Release(btn), active.mouse.0, active.mouse.1) {
            // Pointer left the pane: still tell the original pane the button is up.
            if let Some(pane) = active.session.pane(pid) {
                if pane.wants_mouse() {
                    if let Some(bytes) = encode_mouse(MouseReport::Release(btn), 1, 1, &active.mods, pane.mouse_sgr()) {
                        pane.write(&bytes);
                    }
                }
            }
        }
        active.window.request_redraw();
        true
    }

    /// Report bare pointer motion (no button) to the app under the cursor, but
    /// only if it enabled any-motion tracking (mode 1003, e.g. to light up
    /// whatever is hovered). Deduplicated to cell changes so we send one report
    /// per cell entered, not one per pixel. Returns `true` when the app owns the
    /// hover (so the caller skips focus-follows for this move).
    fn forward_hover(active: &mut Active) -> bool {
        // The pane + cell under the pointer (None over gutters/titlebars).
        let Some((pane_id, col, row)) = Self::cell_at(active, active.mouse.0, active.mouse.1) else {
            active.hover_cell = None; // left the grid: forget the last cell
            return false;
        };
        // Does that pane's app want bare motion? (1003, not just clicks.)
        let wants = active
            .session
            .pane(pane_id)
            .is_some_and(|p| p.wants_motion());
        if !wants {
            active.hover_cell = None;
            return false;
        }
        // Same cell as last time → nothing new to send, but the app still owns it.
        if active.hover_cell == Some((pane_id, col, row)) {
            return true;
        }
        active.hover_cell = Some((pane_id, col, row));
        Self::forward_mouse(active, MouseReport::Move, active.mouse.0, active.mouse.1);
        active.window.request_redraw();
        true
    }

    /// If `(mx, my)` falls on a pane's scrollbar (track or thumb), return that
    /// pane, its grid rect, and the current thumb's `(y, height)`. `None` when
    /// the pointer isn't on a scrollbar or the pane has no scrollback.
    fn scrollbar_at(active: &Active, mx: f32, my: f32) -> Option<(rt_core::PaneId, Rect, f32, f32)> {
        let size = active.window.inner_size();
        let bounds = content_bounds(size);
        for (id, full) in active.session.visible_rects(bounds) {
            if !full.contains(mx, my) {
                continue; // not this pane
            }
            let rect = active.session.content_rect(full); // grid area (minus titlebar)
            let (offset, history, screen) = active.session.pane(id)?.scroll_info();
            if history == 0 {
                return None; // no scrollback → no scrollbar to grab
            }
            let (bx, bw, thumb_y, thumb_h) = scrollbar_metrics(rect, offset, history, screen);
            // Within the scrollbar's x-band (a little slop for easy grabbing) and
            // the grid's vertical extent?
            if mx >= bx - 2.0 && mx <= bx + bw + 2.0 && my >= rect.y && my <= rect.bottom() {
                return Some((id, rect, thumb_y, thumb_h));
            }
            return None; // in this pane but not on its scrollbar
        }
        None
    }

    /// Scroll the pane being scrollbar-dragged so its thumb tracks the pointer's
    /// `my`. Inverts [`scrollbar_metrics`] to turn the desired thumb position into
    /// a display offset, then scrolls by the difference. No-op if not dragging.
    fn apply_scroll_drag(active: &mut Active, my: f32) {
        let Some((pid, grab, rect)) = active.scroll_drag else { return };
        let Some(pane) = active.session.pane(pid) else { return };
        let (offset, history, screen) = pane.scroll_info();
        if history == 0 || rect.h <= 0.0 {
            return;
        }
        let (_bx, _bw, _ty, thumb_h) = scrollbar_metrics(rect, offset, history, screen);
        // Invert scrollbar_metrics: the thumb travels `rect.h - thumb_h`, and its
        // position maps linearly onto the offset range `history..0` (top..bottom).
        let travel = (rect.h - thumb_h).max(0.0);
        if travel <= 0.0 {
            return; // thumb fills the track: nothing to drag
        }
        // Desired thumb top from the cursor (minus where we grabbed it), clamped
        // to the travel range so both ends are exactly reachable.
        let want_y = (my - grab).clamp(rect.y, rect.y + travel);
        let down = (want_y - rect.y) / travel; // 1 at bottom, 0 at top
        let target = (history as f32 * (1.0 - down)).round().clamp(0.0, history as f32) as isize;
        let delta = target - offset as isize;
        if delta != 0 {
            pane.scroll(delta); // move the view; the next frame redraws the thumb
            active.window.request_redraw();
        }
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
                active.session.relayout(content_bounds(size));
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
            active.last_input = Instant::now(); // restart the cursor blink window
            // Typing returns you to the live prompt: if the focused pane was
            // scrolled up in history, snap it back to the bottom.
            if let Some(pane) = active.session.pane(active.session.focus()) {
                pane.scroll_to_bottom();
            }
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
        let bounds = content_bounds(size);
        // Clear to the (possibly translucent) background. This IS the pane
        // background — we no longer draw an opaque per-pane fill, which under
        // translucency would double-blend and darken the see-through areas.
        active.renderer.begin_frame(bg); // translucent clear

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
                            // Unfocused: hollow outline regardless of shape, steady (no blink).
                            active.renderer.cursor_hollow(ox, rect.y, cur.col, sub, ccol);
                        } else {
                            // Soft-blink: fade the focused cursor's alpha so the glyph
                            // beneath shows through, pulsing for a bounded window after
                            // the last keystroke, then holding steady.
                            let blink = cursor_blink_alpha(active.last_input.elapsed().as_secs_f32());
                            let cur_col = ccol.with_alpha(blink);
                            match cur.shape {
                                CursorShape::Block => {
                                    // Solid block + inverse glyph for contrast; both fade
                                    // together so the underlying glyph re-emerges on the dip.
                                    active.renderer.fill_cell(ox, rect.y, cur.col, sub, cur_col);
                                    if let Some(u) = snap.rows.get(cur.line).and_then(|rw| rw.get(cur.col)) {
                                        if u.c != ' ' {
                                            let ub = u.bg;
                                            active.renderer.draw_char(ox, rect.y, cur.col, sub, u.c, Color::rgb(ub[0], ub[1], ub[2]).with_alpha(blink), u.attrs.bold, u.attrs.italic);
                                        }
                                    }
                                }
                                CursorShape::HollowBlock => active.renderer.cursor_hollow(ox, rect.y, cur.col, sub, cur_col),
                                CursorShape::Underline => active.renderer.cursor_underline(ox, rect.y, cur.col, sub, cur_col),
                                CursorShape::Beam => active.renderer.cursor_beam(ox, rect.y, cur.col, sub, cur_col),
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
                    // Track + thumb; geometry shared with the drag hit-test.
                    let (bx, bw, thumb_y, thumb_h) = scrollbar_metrics(rect, offset, history, screen);
                    active.renderer.fill_rect(bx, rect.y, bw, rect.h, Color::rgb(0x22, 0x22, 0x2c));
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
                // Header colours derived from the user's scheme: the pane
                // background nudged toward the foreground. Interpolating between
                // two real colours means it can never clip at the extremes — a
                // black/white scheme just yields a grey — so the header is always
                // a valid, scheme-native tint, distinct from the terminal body,
                // with readable text. Focused panes get more tint + full-strength
                // text; unfocused ones a whisper of tint + dimmed text.
                let bg = active.settings.background; // [u8; 3], Copy
                let fg = active.settings.foreground;
                let mix = |a: [u8; 3], b: [u8; 3], t: f32| {
                    let c = |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round().clamp(0.0, 255.0) as u8;
                    Color::rgb(c(0), c(1), c(2))
                };
                let bar_bg = mix(bg, fg, if focused { 0.20 } else { 0.10 });
                let sep = mix(bg, fg, 0.40); // hairline: a touch more toward fg → a visible edge
                active.renderer.fill_rect(full.x, full.y, full.w, bar_h, bar_bg);
                active.renderer.fill_rect(full.x, full.y + bar_h - 1.0, full.w, 1.0, sep);
                let text_col = if focused { Color::rgb(fg[0], fg[1], fg[2]) } else { mix(fg, bg, 0.40) };
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
                // Scrollback meter ("buf USED/MAX") just left of the size, warming
                // grey → amber → red as the buffer fills so a runaway listing is
                // visible before it eats memory.
                let mut left_of = size_x; // right boundary the title must clear
                if let Some(pane) = active.session.pane(id) {
                    let (_, used, _) = pane.scroll_info(); // history lines currently held
                    let max = pane.scrollback_limit();
                    if max > 0 {
                        let meter = format!("buf {}/{}", fmt_lines(used), fmt_lines(max));
                        let mw = meter.chars().count() as f32 * cell_w;
                        let mx = (size_x - 2.0 * cell_w - mw).max(left_x); // a 2-cell gap before the size
                        let frac = used as f32 / max as f32;
                        let mcol = if frac > 0.85 {
                            Color::rgb(0xe0, 0x60, 0x50) // nearly full: red warning
                        } else if frac > 0.6 {
                            Color::rgb(0xe0, 0xc0, 0x50) // getting full: amber
                        } else if focused {
                            Color::rgb(0xa2, 0xac, 0xba) // idle: muted grey-blue
                        } else {
                            Color::rgb(0x7c, 0x80, 0x8c)
                        };
                        for (i, ch) in meter.chars().enumerate() {
                            active.renderer.draw_char(mx, text_top, i, 0, ch, mcol, false, false);
                        }
                        left_of = mx; // title truncates before the meter
                    }
                }
                // Title on the left, truncated so it never runs into the meter/size.
                let title = active.session.title_of(id).filter(|t| !t.is_empty()).unwrap_or("Terminal");
                let avail = ((left_of - 8.0 - left_x) / cell_w).max(0.0) as usize; // room in cells
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

        // Visible bell: a brief yellow/black hazard stripe on the border of just
        // the pane that rang — never a whole-window flash.
        if !active.bell_flash.is_empty() {
            let now = Instant::now();
            active.bell_flash.retain(|_, exp| now < *exp); // drop elapsed flashes
            let rects: Vec<_> = active.session.visible_rects(bounds);
            for (id, r) in rects {
                if active.bell_flash.contains_key(&id) {
                    active.renderer.bell_stripe(r.x, r.y, r.w, r.h);
                }
            }
        }

        active.renderer.end_frame(); // upload + draw call

        // One egui pass per frame: the preferences dialog, the context menu, the
        // manual, the search bar, or (when none is up) the border instruments.
        if active.prefs_open {
            Self::paint_egui(active);
        } else if active.menu.is_some() {
            Self::paint_menu(active);
        } else if active.manual_open {
            Self::paint_manual(active);
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
        // Estimate scrollback memory against a full-width pane at the current
        // font, so the dialog can warn before the slider picks an unrunnable size.
        let cols = (content_bounds(active.window.inner_size()).w / active.renderer.cell_size().0).max(1.0) as usize;
        let ram = total_ram_bytes();
        preferences::ui(&ctx, &mut settings, &mut close, &active.mono_families, ram, cols); // build the dialog
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
            // The blur decision depends on both the toggle and the opacity slider.
            let blur_changed = want_blur(&settings) != want_blur(&active.settings);
            active.settings = settings; // commit
            Self::persist(&active.settings);
            // Scrollback: newly spawned panes read this live cell.
            active.scrollback.set(active.settings.scrollback);
            // Background blur: toggle live if the config or opacity moved it.
            if blur_changed {
                apply_blur(active);
            }
            // Titlebar toggle changes every pane's content height → re-reserve and
            // resize all PTYs.
            if titlebar_changed {
                active.session.set_show_titlebar(active.settings.show_titlebar);
                let size = active.window.inner_size();
                active.session.relayout(content_bounds(size));
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

    /// Run the egui context menu for this frame and paint it. Applies the picked
    /// action and closes the menu on a selection, an outside click, or Escape.
    fn paint_menu(active: &mut Active) {
        let Some(pos) = active.menu else { return };
        // Menu context, computed fresh each frame: the URL under the right-click
        // (if any) drives the Open Link / Copy Address rows; a live selection
        // enables Copy.
        let url = Self::cell_at(active, pos.0, pos.1)
            .and_then(|(pane, col, row)| Self::url_at(active, pane, col, row));
        let has_sel = Self::selected_text(active).is_some();
        let raw_input = active.egui_state.take_egui_input(&active.window); // gather input
        let ctx = active.egui_ctx.clone(); // cheap Arc clone (avoids borrowing active in the closure)
        // The menu was anchored in physical pixels; egui positions in points.
        let ppp = active.window.scale_factor() as f32;
        let mut chosen: Option<menu::MenuPick> = None;
        let mut close = false;
        ctx.begin_pass(raw_input);
        menu::ui(&ctx, (pos.0 / ppp, pos.1 / ppp), &active.keymap, has_sel, url.as_deref(), &mut chosen, &mut close);
        let output = ctx.end_pass();
        // Dismiss before applying, so an action that opens another overlay (e.g.
        // Preferences) isn't immediately re-covered by the menu.
        if chosen.is_some() || close {
            active.menu = None;
        }
        active.egui_state.handle_platform_output(&active.window, output.platform_output);
        let out_ppp = output.pixels_per_point;
        let primitives = ctx.tessellate(output.shapes, out_ppp);
        let size = active.window.inner_size();
        active.egui_painter.paint_and_update_textures(
            [size.width, size.height],
            out_ppp,
            &primitives,
            &output.textures_delta,
        );
        // Apply after painting/committing state so it runs the same path as a key
        // binding (may relayout, open a dialog, or close the last pane and exit).
        match chosen {
            Some(menu::MenuPick::Do(action)) => {
                Self::apply_action(active, action);
                active.window.request_redraw();
            }
            Some(menu::MenuPick::OpenUrl(u)) => {
                Self::open_url(&u);
                active.window.request_redraw();
            }
            Some(menu::MenuPick::CopyUrl(u)) => {
                if let Some(cb) = &active.clipboard {
                    cb.store(u); // put the address on the CLIPBOARD selection
                }
                active.window.request_redraw();
            }
            None => {}
        }
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

    /// Move bytes across every patch-bay wire: read each pane's output-stream
    /// jacks and forward to the input jack of every pane wired downstream. Reads
    /// always (even unwired) so a program writing `$RT_OUT` never blocks. Returns
    /// whether any bytes moved (to keep the wire flow animating).
    fn pump_wires(active: &mut Active) -> bool {
        let jacks = active.jacks.clone(); // Rc clone; independent of `active`'s fields
        let src_ids: Vec<rt_core::PaneId> = jacks.borrow().keys().copied().collect();
        let mut moved = false;
        let mut buf = [0u8; 8192];
        for src in src_ids {
            for stream in [Stream::Stdout, Stream::Stderr] {
                loop {
                    // Read one chunk from this pane's chosen jack (non-blocking).
                    let n = {
                        let mut jm = jacks.borrow_mut();
                        let Some(j) = jm.get_mut(&src) else { break };
                        let fd = match stream {
                            Stream::Stdout => &mut j.out_read,
                            Stream::Stderr => &mut j.err_read,
                        };
                        match fd.read(&mut buf) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(_) => break,
                        }
                    };
                    moved = true;
                    let chunk = buf[..n].to_vec();
                    // Fan out to every wire leaving this pane on this stream.
                    for w in active.wires.iter_mut().filter(|w| w.src == src && w.stream == stream) {
                        let mut jm = jacks.borrow_mut();
                        if let Some(dj) = jm.get_mut(&w.dst) {
                            let _ = dj.in_write.write_all(&chunk); // best-effort
                            w.moved = w.moved.saturating_add(n as u32);
                        }
                    }
                }
            }
        }
        moved
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

    /// Run the built-in manual overlay for this frame and paint it.
    fn paint_manual(active: &mut Active) {
        let raw_input = active.egui_state.take_egui_input(&active.window);
        let ctx = active.egui_ctx.clone();
        let mut close = false;
        ctx.begin_pass(raw_input);
        manual::ui(&ctx, &mut close);
        let output = ctx.end_pass();
        if close {
            active.manual_open = false;
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
        // Advance each wire's byte-rate and flow phase (packets = the actual bytes).
        for w in active.wires.iter_mut() {
            let inst = w.moved as f32 / dt.max(1e-3);
            w.moved = 0;
            w.rate = w.rate * 0.75 + inst * 0.25;
            let act = (w.rate / WIRE_BUSY_BYTES).clamp(0.0, 1.0);
            w.phase = (w.phase + act * FLOW_MAX_LAPS * dt).fract();
        }
        // Pane rectangles in physical pixels.
        let size = active.window.inner_size();
        let bounds = content_bounds(size);
        let rects = active.session.visible_rects(bounds);

        // egui pass: a background painter drawing the orbiting packets.
        let raw = active.egui_state.take_egui_input(&active.window);
        let ctx = active.egui_ctx.clone();
        ctx.begin_pass(raw);
        let ppp = ctx.pixels_per_point().max(0.5); // physical px → egui points
        // Which instruments the settings enable (patch-bay wires always draw).
        let (inst_output, inst_heat, inst_latency) =
            (active.settings.inst_output, active.settings.inst_heat, active.settings.inst_latency);
        {
            let painter =
                ctx.layer_painter(egui::LayerId::new(egui::Order::Background, egui::Id::new("rt_instr")));
            for (id, rect) in &rects {
                let m = active.meters.get(id).copied().unwrap_or_default();
                let act = (m.rate / BUSY_WAKEUPS).clamp(0.0, 1.0);
                let (x, y, w, h) = (rect.x / ppp, rect.y / ppp, rect.w / ppp, rect.h / ppp);
                // Heat: a blackbody-tinted border stroke (the pane's temperature),
                // drawn under the green output packets so one ring shows both.
                if inst_heat {
                    let load = active.heat.get(id).copied().unwrap_or(0.0);
                    painter.rect_stroke(
                        egui::Rect::from_min_size(egui::pos2(x, y), egui::vec2(w, h)),
                        egui::CornerRadius::ZERO,
                        egui::Stroke::new(2.4, heat_color32(load)),
                        egui::StrokeKind::Inside,
                    );
                }
                for k in (0..FLOW_PACKETS).take_while(|_| inst_output) {
                    let t = (m.phase + k as f32 / FLOW_PACKETS as f32).fract();
                    let p = flow_point(x, y, w, h, t);
                    let a = 0.30 + 0.70 * act; // dim when idle, vivid when busy
                    let glow = egui::Color32::from_rgba_unmultiplied(0x28, 0xc0, 0x48, (a * 110.0) as u8);
                    let core = egui::Color32::from_rgba_unmultiplied(0x66, 0xff, 0x7a, (a * 255.0) as u8);
                    painter.circle_filled(p, 9.0, glow); // soft halo
                    painter.circle_filled(p, 3.4, core); // bright centre
                }
            }
            // ── Patch-bay: jacks on each pane + animated wires between them ────
            // Jack position (points) on a pane rect: 0=stdin (left mid),
            // 1=stdout (right upper), 2=stderr (right lower).
            let jack_pos = |r: &rt_core::Rect, which: u8| -> egui::Pos2 {
                let (x, y, w, h) = (r.x / ppp, r.y / ppp, r.w / ppp, r.h / ppp);
                match which {
                    0 => egui::pos2(x, y + h * 0.5),
                    1 => egui::pos2(x + w, y + h / 3.0),
                    _ => egui::pos2(x + w, y + 2.0 * h / 3.0),
                }
            };
            let rect_of = |id: rt_core::PaneId| rects.iter().find(|&&(i, _)| i == id).map(|(_, r)| r);
            // Wires first (under the jacks): a smooth cubic bezier from the source
            // stream jack to the destination stdin jack, with stream-coloured flow
            // packets travelling along it — the packets are the literal bytes.
            for w in &active.wires {
                let (Some(sr), Some(dr)) = (rect_of(w.src), rect_of(w.dst)) else { continue };
                let p0 = jack_pos(sr, if w.stream == Stream::Stdout { 1 } else { 2 });
                let p3 = jack_pos(dr, 0);
                let ext = ((p3.x - p0.x).abs() * 0.4 + 40.0).min(180.0); // control-point reach
                let p1 = egui::pos2(p0.x + ext, p0.y);
                let p2 = egui::pos2(p3.x - ext, p3.y);
                let hue = if w.stream == Stream::Stdout { (0x40, 0xc0, 0x54) } else { (0xd0, 0x54, 0x30) };
                let act = (w.rate / WIRE_BUSY_BYTES).clamp(0.0, 1.0);
                const N: u32 = 56;
                let mut prev = p0;
                for i in 1..=N {
                    let t = i as f32 / N as f32;
                    let pt = cubic_bezier(p0, p1, p2, p3, t);
                    // Brightness = nearest travelling packet, scaled by throughput.
                    let mut best = 0.0f32;
                    for k in 0..WIRE_PACKETS {
                        let pp = (w.phase + k as f32 / WIRE_PACKETS as f32).fract();
                        let d = (t - pp).abs();
                        best = best.max((-d * d / (2.0 * 0.05 * 0.05)).exp());
                    }
                    let b = 0.22 + 0.78 * best * (0.30 + 0.70 * act);
                    let col = egui::Color32::from_rgb(
                        (hue.0 as f32 * b) as u8,
                        (hue.1 as f32 * b) as u8,
                        (hue.2 as f32 * b) as u8,
                    );
                    painter.line_segment([prev, pt], egui::Stroke::new(2.0, col));
                    prev = pt;
                }
            }
            // Jack dots on every pane (filled ● when a wire uses them).
            for (id, r) in rects.iter().take_while(|_| active.settings.show_jacks) {
                let has_in = active.wires.iter().any(|w| w.dst == *id);
                let has_out = active.wires.iter().any(|w| w.src == *id && w.stream == Stream::Stdout);
                let has_err = active.wires.iter().any(|w| w.src == *id && w.stream == Stream::Stderr);
                let jack = |p: egui::Pos2, filled: bool, col: egui::Color32| {
                    painter.circle_filled(p, 4.5, egui::Color32::from_black_alpha(180)); // dark backing
                    if filled {
                        painter.circle_filled(p, 3.5, col);
                    } else {
                        painter.circle_stroke(p, 3.2, egui::Stroke::new(1.4, col));
                    }
                };
                jack(jack_pos(r, 0), has_in, egui::Color32::from_rgb(0x88, 0x88, 0x98));
                jack(jack_pos(r, 1), has_out, egui::Color32::from_rgb(0x40, 0xc0, 0x54));
                jack(jack_pos(r, 2), has_err, egui::Color32::from_rgb(0xd0, 0x54, 0x30));
            }
            // Rubber-band the in-progress mouse wire from its source jack to the
            // cursor (a dashed bezier, so it reads like the finished wire).
            if let (Some((src, stream)), Some((cx, cy))) = (active.wiring_from, active.drag_cursor) {
                if let Some(sr) = rect_of(src) {
                    let p0 = jack_pos(sr, if stream == Stream::Stdout { 1 } else { 2 });
                    let p3 = egui::pos2(cx / ppp, cy / ppp);
                    let ext = ((p3.x - p0.x).abs() * 0.4 + 40.0).min(180.0);
                    let p1 = egui::pos2(p0.x + ext, p0.y);
                    let p2 = egui::pos2(p3.x - ext, p3.y);
                    let (hr, hg, hb) = if stream == Stream::Stdout { (0x40, 0xc0, 0x54) } else { (0xd0, 0x54, 0x30) };
                    let col = egui::Color32::from_rgba_unmultiplied(hr, hg, hb, 180);
                    let mut prev = p0;
                    for i in 1..=40 {
                        let t = i as f32 / 40.0;
                        let pt = cubic_bezier(p0, p1, p2, p3, t);
                        if i % 2 == 0 {
                            painter.line_segment([prev, pt], egui::Stroke::new(1.6, col)); // dashed
                        }
                        prev = pt;
                    }
                }
            }
            // Latency: the whole-window frame, drawn last so it wins the outer
            // ring. Short violet segments each coloured by the undulation, with a
            // fast bright flare travelling round when a deadline was missed.
            // Trace the content region's perimeter (inset by the standoff), not
            // the raw window edge, so the frame is fully visible.
            let cb = content_bounds(size);
            let (fx, fy) = (cb.x / ppp, cb.y / ppp);
            let (fw, fh) = (cb.w / ppp, cb.h / ppp);
            if inst_latency {
                // Walk each edge explicitly, corner-to-corner, so the corners are
                // always exact segment endpoints (an even sampling of the whole
                // perimeter straddles two corners and chords across them).
                let per = 2.0 * (fw + fh);
                // (corner point, cumulative distance from the top-left, clockwise).
                let corners = [
                    (egui::pos2(fx, fy), 0.0),
                    (egui::pos2(fx + fw, fy), fw),
                    (egui::pos2(fx + fw, fy + fh), fw + fh),
                    (egui::pos2(fx, fy + fh), 2.0 * fw + fh),
                    (egui::pos2(fx, fy), per), // back to the start
                ];
                const SUB: u32 = 26; // segments per edge
                for e in 0..4 {
                    let (pa, da) = corners[e];
                    let (pb, db) = corners[e + 1];
                    let mut prev = pa;
                    for s in 1..=SUB {
                        let f = s as f32 / SUB as f32;
                        let pt = egui::pos2(pa.x + (pb.x - pa.x) * f, pa.y + (pb.y - pa.y) * f);
                        // Colour at the segment midpoint's perimeter position.
                        let mid_t = (da + (db - da) * (f - 0.5 / SUB as f32)) / per;
                        let col = latency_color(mid_t, active.lat_phase, active.stall);
                        painter.line_segment([prev, pt], egui::Stroke::new(2.0, col));
                        prev = pt;
                    }
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

/// Colour of the latency frame at perimeter position `pos` (0..1): a calm
/// purple-blue-violet undulation, plus a bright fast-moving flare scaled by
/// `stall` (a recent deadline miss). Ported from rt-mux.
fn latency_color(pos: f32, phase: f32, stall: f32) -> egui::Color32 {
    use std::f32::consts::TAU;
    let wave = 0.5 + 0.5 * (TAU * (pos * 2.0 + phase)).sin(); // two slow lobes drifting
    let base = 0.20 + 0.30 * wave;
    let spike = if stall > 0.01 {
        let sp = (phase * 4.0).fract(); // the flare laps faster than the calm wave
        let mut d = (pos - sp).abs();
        if d > 0.5 {
            d = 1.0 - d;
        }
        stall * (-d * d / (2.0 * 0.06 * 0.06)).exp() // gaussian bump
    } else {
        0.0
    };
    let v = (base + spike).clamp(0.0, 1.0);
    let r = (56.0 + 128.0 * v + spike * 120.0).min(255.0) as u8;
    let g = (32.0 + 80.0 * v + spike * 110.0).min(255.0) as u8;
    let b = (88.0 + 152.0 * v + spike * 40.0).min(255.0) as u8;
    egui::Color32::from_rgb(r, g, b)
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

/// Standoff (physical px) between the window edge and the terminal content, so
/// the edge-living features — heat border, patch-bay jacks, latency frame, and
/// the outermost text cells — have room and aren't clipped by the window edge.
const WINDOW_MARGIN: f32 = 8.0;

/// Fast wake interval while animating or interacting (~60fps).
const ACTIVE_POLL: Duration = Duration::from_millis(16);
/// Idle wake interval: when nothing is happening we still wake this often to
/// notice async PTY output and heat changes, but at ~10Hz instead of 60 — a
/// fraction of the cost, still prompt enough that output appears without lag.
const IDLE_POLL: Duration = Duration::from_millis(100);
/// Stay in the fast poll for this long after the last input or output, so a
/// burst eases to a stop smoothly and interaction never feels throttled.
const ACTIVE_TAIL: Duration = Duration::from_millis(750);

/// Cursor soft-blink: after you stop typing, the focused cursor pulses gently
/// (a soft fade, not a hard on/off) for [`CURSOR_BLINK_CYCLES`] cycles of
/// [`CURSOR_BLINK_PERIOD`], then holds steady — mirroring Terminator, which
/// blinks a handful of times then stops. Bounding it keeps idle CPU near zero:
/// we only repaint (at [`BLINK_POLL`]) during that window.
const CURSOR_BLINK_PERIOD: f32 = 1.0; // seconds per pulse (Terminator ≈ 1s)
const CURSOR_BLINK_CYCLES: f32 = 9.0; // pulses after last keystroke, then steady
const CURSOR_BLINK_MIN: f32 = 0.25; // dimmest alpha — soft, never fully off
/// Repaint cadence while the cursor is pulsing: ~20fps is smooth for a 1s fade
/// and far cheaper than the 60fps active poll.
const BLINK_POLL: Duration = Duration::from_millis(50);

/// The focused cursor's alpha `seconds` after the last keystroke: a raised-cosine
/// pulse between [`CURSOR_BLINK_MIN`] and 1.0, settling to a steady 1.0 once the
/// blink window elapses. 1.0 right after a keystroke (cursor solid), dipping and
/// returning each period.
fn cursor_blink_alpha(seconds: f32) -> f32 {
    if seconds >= CURSOR_BLINK_PERIOD * CURSOR_BLINK_CYCLES {
        return 1.0; // window over: hold steady
    }
    let s = 0.5 + 0.5 * (seconds / CURSOR_BLINK_PERIOD * std::f32::consts::TAU).cos();
    CURSOR_BLINK_MIN + (1.0 - CURSOR_BLINK_MIN) * s
}

/// The content rectangle: the window inset by [`WINDOW_MARGIN`] on every side.
/// All layout (panes, instruments, jacks, hit-testing) uses this; the background
/// clear still fills the whole window, so the margin shows the background.
/// Whether to ask the compositor for background blur: only when the user has it
/// enabled AND the background is translucent (blur behind a fully opaque surface
/// is invisible and wasted compositor work). The single source of truth for the
/// blur decision, shared by startup and every runtime opacity/config change.
fn want_blur(settings: &rt_config::Settings) -> bool {
    settings.background_blur && settings.background_opacity < 1.0
}

/// Push the current blur decision to whichever backend is live: the Wayland ext
/// protocol and/or the X11 property. Both are no-ops when not applicable, so
/// this is always safe to call after an opacity/blur change.
fn apply_blur(active: &mut Active) {
    let want = want_blur(&active.settings);
    if let Some(fx) = &mut active.bg_effect {
        fx.set_enabled(want);
    }
    active.x11_blur.set_enabled(want);
}

fn content_bounds(size: winit::dpi::PhysicalSize<u32>) -> Rect {
    let m = WINDOW_MARGIN;
    Rect::new(
        m,
        m,
        (size.width as f32 - 2.0 * m).max(1.0),
        (size.height as f32 - 2.0 * m).max(1.0),
    )
}

/// Physical window size that makes a single full-window pane come out to exactly
/// `cols`×`rows` cells, given the measured `cell` size and whether a per-pane
/// titlebar strip is reserved. Inverts [`content_bounds`] (the window margin) and
/// [`rt_session::pane_chrome`] (the pane's inner padding + titlebar). A half-cell
/// slack keeps floating-point rounding from dropping the last row/column.
fn window_size_for_grid(cols: usize, rows: usize, cell: (f32, f32), show_titlebar: bool) -> winit::dpi::PhysicalSize<u32> {
    let (pad_w, pad_h) = rt_session::pane_chrome(cell, show_titlebar);
    let w = cols as f32 * cell.0 + cell.0 * 0.5 + pad_w + 2.0 * WINDOW_MARGIN;
    let h = rows as f32 * cell.1 + cell.1 * 0.5 + pad_h + 2.0 * WINDOW_MARGIN;
    winit::dpi::PhysicalSize::new(w.ceil() as u32, h.ceil() as u32)
}

/// Total system RAM in bytes (`MemTotal` from `/proc/meminfo`), or 0 if it can't
/// be read. Excludes swap — the Preferences scrollback estimate is against
/// physical RAM, since spilling a terminal buffer to swap is already a failure.
fn total_ram_bytes() -> u64 {
    std::fs::read_to_string("/proc/meminfo")
        .ok()
        .and_then(|s| {
            s.lines().find_map(|l| {
                // Line looks like: "MemTotal:       65712345 kB"
                let kb = l.strip_prefix("MemTotal:")?.split_whitespace().next()?;
                kb.parse::<u64>().ok()
            })
        })
        .map(|kb| kb * 1024)
        .unwrap_or(0)
}

/// Format a line count compactly for the titlebar buffer meter: `950`, `12k`,
/// `1.2M`, `20M`. Whole thousands/millions drop the decimal.
fn fmt_lines(n: usize) -> String {
    if n >= 1_000_000 {
        let m = n as f64 / 1_000_000.0;
        if (m.fract()).abs() < 0.05 { format!("{}M", m.round() as u64) } else { format!("{m:.1}M") }
    } else if n >= 1_000 {
        let k = n as f64 / 1_000.0;
        if (k.fract()).abs() < 0.05 { format!("{}k", k.round() as u64) } else { format!("{k:.1}k") }
    } else {
        format!("{n}")
    }
}

/// Scrollbar geometry for a pane whose grid area is `rect` and whose scroll
/// state is `(offset, history, screen)` — returns `(bx, width, thumb_y,
/// thumb_h)` in pixels. Shared by the renderer (which draws the thumb) and the
/// drag hit-test (which grabs it), so the visible thumb and the grabbable region
/// are always the same rectangle. Only meaningful when `history > 0`.
fn scrollbar_metrics(rect: Rect, offset: usize, history: usize, screen: usize) -> (f32, f32, f32, f32) {
    let total = (history + screen) as f32; // whole buffer height in lines
    let bw = 6.0; // scrollbar width
    let bx = rect.right() - bw - 1.0; // inset slightly from the grid's right edge
    // Thumb size = the visible fraction of the buffer, floored so it stays
    // grabbable even for a huge history, and capped at the track height.
    let thumb_h = (screen as f32 / total * rect.h).max(24.0).min(rect.h);
    // Thumb position: it slides over the track region ABOVE its own height as
    // the offset runs 0 (bottom) → history (top). Mapping across this reduced
    // *travel* (not the whole track) — and against `history`, not `total` — is
    // what keeps BOTH ends reachable once the thumb is floored to its minimum.
    let travel = (rect.h - thumb_h).max(0.0); // distance the thumb can move
    let down = history.saturating_sub(offset) as f32 / history.max(1) as f32; // 1 at bottom, 0 at top
    let thumb_y = rect.y + down * travel;
    (bx, bw, thumb_y, thumb_h)
}

/// A pointer action to report to the application running inside a pane.
#[derive(Clone, Copy)]
enum MouseReport {
    Press(u16),   // button pressed; the u16 is the base xterm button code (0=L,1=M,2=R)
    Release(u16), // button released
    Drag(u16),    // pointer motion while a button is held
    Move,         // pointer motion with NO button held (any-motion / hover, mode 1003)
    Scroll(bool), // wheel notch: true = up, false = down
}

/// Encode a pointer action as an xterm mouse report at 1-based `(col, row)`, in
/// SGR form (`ESC [ < b ; col ; row M/m`) when the app enabled it, else the
/// legacy X10 byte form. Mirrors rt-mux's encoder. Shift is rt's override key
/// (the caller never forwards a Shift-held event), so only Alt/Ctrl fold into
/// the button bits here.
fn encode_mouse(report: MouseReport, col: u16, row: u16, mods: &ModifiersState, sgr: bool) -> Option<Vec<u8>> {
    // Base button/action code, and whether this is a release.
    let (mut cb, release) = match report {
        MouseReport::Press(b) => (b, false),
        MouseReport::Release(b) => (b, true),
        MouseReport::Drag(b) => (b + 32, false),               // xterm motion flag
        MouseReport::Move => (3 + 32, false),                  // no button (3) + motion (32) = 35
        MouseReport::Scroll(up) => (if up { 64 } else { 65 }, false), // wheel codes
    };
    if mods.alt_key() {
        cb += 8; // xterm Meta bit
    }
    if mods.control_key() {
        cb += 16; // xterm Control bit
    }
    if sgr {
        // SGR (mode 1006): decimal coords, press/scroll end in 'M', release in 'm'.
        let end = if release { 'm' } else { 'M' };
        Some(format!("\x1b[<{cb};{col};{row}{end}").into_bytes())
    } else {
        // Legacy X10: ESC [ M then three bytes, each offset by 32; release = code 3.
        let b = if release { 3 } else { cb };
        let cx = col.saturating_add(32).min(255) as u8;
        let cy = row.saturating_add(32).min(255) as u8;
        Some(vec![0x1b, b'[', b'M', (b + 32).min(255) as u8, cx, cy])
    }
}

/// A point on the cubic Bézier `p0→p3` (control points `p1`, `p2`) at `t` in
/// 0..1 — used to draw the smooth patch-bay wires.
fn cubic_bezier(p0: egui::Pos2, p1: egui::Pos2, p2: egui::Pos2, p3: egui::Pos2, t: f32) -> egui::Pos2 {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    egui::pos2(
        a * p0.x + b * p1.x + c * p2.x + d * p3.x,
        a * p0.y + b * p1.y + c * p2.y + d * p3.y,
    )
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
/// Build the winit event loop, choosing the backend explicitly so a universal
/// (`x11`-feature) binary uses **native Wayland** on a Wayland session and only
/// falls back to X11 otherwise — never XWayland when Wayland is present. On a
/// Wayland-only build there is nothing to disambiguate. A user-set
/// `WINIT_UNIX_BACKEND` still wins (we don't override an explicit choice).
fn build_event_loop() -> EventLoop<()> {
    let mut builder = EventLoop::builder();
    #[cfg(feature = "x11")]
    {
        use winit::platform::wayland::EventLoopBuilderExtWayland;
        use winit::platform::x11::EventLoopBuilderExtX11;
        if std::env::var_os("WINIT_UNIX_BACKEND").is_none() {
            if std::env::var_os("WAYLAND_DISPLAY").is_some() {
                builder.with_wayland(); // native Wayland, not XWayland
                log::debug!("event loop: preferring native Wayland backend");
            } else {
                builder.with_x11();
                log::debug!("event loop: using X11 backend");
            }
        }
    }
    builder.build().expect("failed to create event loop")
}

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
    // Reclaim patch-bay dirs left by dead sessions (crashes/kills/older builds).
    sweep_stale_jacks();
    // Build the winit event loop and hand it our application.
    let event_loop = build_event_loop();
    let mut app = App { font_db, mono_families, cli: parse_cli(), active: None };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("rt: event loop error: {e}"); // surface any run-loop failure
        std::process::exit(1);
    }
}
