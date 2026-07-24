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

mod backend; // rendering backend abstraction (GL today, XRender in mechanism C)
mod blur; // best-effort KDE/KWin background-blur request (no-op elsewhere)
mod bg_effect; // cross-compositor blur via ext-background-effect-v1 (no-op elsewhere)
mod chrome; // native (XRender) chrome: menu/search/manual/instruments draw + hit-test
mod gl_backend; // the default GL backend: wraps render.rs's Renderer + present resources
mod x11_blur; // X11 background blur via _KDE_NET_WM_BLUR_BEHIND_REGION (no-op elsewhere)
#[cfg(feature = "x11")]
mod x11_present; // Route 1: X11 damage-rect present (glReadPixels + XPutImage)
#[cfg(feature = "x11")]
mod xrender_backend; // mechanism C: XRender backend
mod clipboard; // cross-backend clipboard (Wayland smithay / X11 arboard)
mod damage; // pure pixel-rect damage accumulator
mod input; // (also re-exported by lib.rs for tests; declared here for the bin)
mod manual; // the built-in manual overlay (F1)
mod menu; // right-click context menu (Terminator-style)
mod prefs_model; // which setting each preferences row edits, and how a step clamps
mod raster; // CPU anti-aliased coverage masks (disc/ring/bar) shared by GL + XRender
mod render; // the GL glyph-atlas renderer
mod select; // pure head-navigation logic for anchored selection

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
use glutin::prelude::*; // brings the Gl* traits (make_current, get_proc_address, buffer_age, …)
use glutin::surface::SurfaceAttributesBuilder;
use glutin_winit::{DisplayBuilder, GlWindow};
use raw_window_handle::{HasDisplayHandle, HasWindowHandle};
use winit::application::ApplicationHandler;
use winit::event::{ElementState, Ime, MouseButton, MouseScrollDelta, WindowEvent};
use winit::event_loop::{ActiveEventLoop, ControlFlow, EventLoop};
use winit::keyboard::{Key, ModifiersState, NamedKey};
use winit::window::{CursorIcon, Window, WindowId};

use render::{Color, Renderer};
use rt_config::Keymap;
use rt_core::Rect;
use rt_engine::TermPane;
use rt_session::{Broadcast, Session, SessionEvent};

/// The concrete session type used by the app: real PTY panes, spawned by a
/// boxed closure (boxed so the `Session`'s factory type is nameable in a field).
type AppSession = Session<TermPane, Box<dyn FnMut(rt_core::PaneId, usize, usize) -> Option<TermPane>>>;

/// The shared map of per-pane patch-bay jacks. Shared (`Rc<RefCell<…>>`) between
/// the spawn closure (which creates a pane's jacks) and [`Active`] (which pumps
/// and renders them); winit is single-threaded so this never contends.
type SharedJacks = Rc<RefCell<std::collections::HashMap<rt_core::PaneId, Jacks>>>;

/// A visible pane's layout rect paired with its (once-per-frame) render
/// snapshot. Built in `redraw`'s planning loop and handed to `draw_panes`, so
/// `render_snapshot()` — which mutates the engine's damage state — runs exactly
/// once per pane per frame.
type PxRectSnap = (Rect, Option<rt_engine::Snapshot>);

/// Which of a pane's output streams a wire draws from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Stdout, // $RT_OUT — green
    Stderr, // $RT_ERR — red
}

/// One live patch-bay connection: bytes from `src`'s output stream jack are
/// written to `dst`'s input jack. Throughput drives the wire's flow animation.
pub(crate) struct Wire {
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

/// Create a fifo at `path` (mode 0600). An *existing* path is treated as an
/// error, not swallowed: `mkfifo(2)` fails with `EEXIST` unless it created the
/// node itself, so this gives us `O_EXCL` semantics — if the path is already
/// there, someone else made it (a planted fifo/symlink in a shared tmp dir, or a
/// stale node), and wiring a pane's stdio to a node we didn't create would let
/// them read or inject that pane's bytes. Refuse it; the caller drops the jack.
fn mkfifo(path: &Path) -> std::io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "fifo path has a NUL"))?;
    // SAFETY: `c` is a valid NUL-terminated C string for the call's duration.
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    if rc != 0 {
        return Err(std::io::Error::last_os_error()); // incl. EEXIST — refuse a path we didn't create
    }
    Ok(())
}

/// Securely (re)create this session's patch-bay directory with mode 0700. The
/// fallback base (`$TMPDIR`) is world-writable, and our pid is guessable, so we
/// must not blindly `create_dir_all` and trust whatever is there: an attacker
/// could plant `rt-<pid>` (or make it a symlink, or pre-create the fifos inside)
/// to hijack a pane's stdio. We `mkdir(0700)` fresh; if the path already exists
/// we accept it only when an `lstat` proves it is a real directory we own with
/// no group/other access, then wipe it clean so no planted node survives. Any
/// doubt fails closed — the caller then runs with the patch-bay disabled.
fn ensure_jacks_dir(dir: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::MetadataExt;
    let c = CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "jacks dir path has a NUL"))?;
    // SAFETY: `c` is a valid NUL-terminated C string for the call's duration.
    let mkdir = || unsafe { libc::mkdir(c.as_ptr(), 0o700) };
    if mkdir() == 0 {
        return Ok(()); // created fresh and private — the common case
    }
    let err = std::io::Error::last_os_error();
    if err.kind() != std::io::ErrorKind::AlreadyExists {
        return Err(err); // e.g. base dir missing or unwritable
    }
    // The path exists. `lstat` (does not follow a symlink) must show a plain
    // directory, owned by us, with no access for group or other.
    let meta = std::fs::symlink_metadata(dir)?;
    if !meta.is_dir() {
        return Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists,
            "patch-bay path exists and is not a directory"));
    }
    if meta.uid() != unsafe { libc::geteuid() } {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied,
            "patch-bay directory is not owned by us"));
    }
    if meta.mode() & 0o077 != 0 {
        return Err(std::io::Error::new(std::io::ErrorKind::PermissionDenied,
            "patch-bay directory is accessible by group or other"));
    }
    // It is our own stale dir (a crashed prior run that reused this pid). Wipe
    // it so no leftover fifo is silently reused, then recreate it private. If an
    // attacker races to plant the path in the gap, the second mkdir fails
    // EEXIST and we bail — fail closed.
    std::fs::remove_dir_all(dir)?;
    if mkdir() != 0 {
        return Err(std::io::Error::last_os_error());
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
pub(crate) struct Meter {
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

/// Patch-bay jack disc radii (px), shared by the GL (`paint_instruments`) and
/// native (`chrome::instruments`) paths so the two never drift. Bumped a notch
/// from the original 6.0/4.6/4.3 — the ports read as too small against the grab
/// radius (`jack_at`'s 12px). `RING_W` is the unwired-jack outline width.
const JACK_R_BACK: f32 = 7.5; // dark backing halo
const JACK_R_FILL: f32 = 5.8; // filled centre when a wire uses the jack
const JACK_R_RING: f32 = 5.4; // outline radius when the jack is idle
const JACK_RING_W: f32 = 1.8; // outline stroke width

/// Everything that only exists once a window and GL context are created (which
/// happens on the first `resumed`). Kept in an `Option` on the `App` so we can
/// build it lazily and tear it down on suspend.
struct Active {
    window: Window,                       // the OS window
    backend: Box<dyn backend::Backend>,   // rendering backend (GL renderer + present resources)
    session: AppSession,                  // layout + panes + focus + broadcast
    keymap: Keymap,                       // Terminator-style bindings
    mods: ModifiersState,                 // live modifier state (updated on change)
    settings: rt_config::Settings,        // window appearance (background opacity, …)
    mouse: (f32, f32),                    // last cursor position in physical pixels
    menu: Option<(f32, f32)>,             // open context menu, at this window position (physical px)
    menu_hover: Option<usize>,            // hovered row of the native (XRender) context menu, if any
    ime_preedit: bool,                    // true while an IME/dead-key composition is in progress
    clipboard: Option<clipboard::Clipboard>, // CLIPBOARD + PRIMARY (Wayland or X11); None if unavailable
    bg_effect: Option<bg_effect::BackgroundEffect>, // compositor background blur (None if protocol absent)
    x11_blur: x11_blur::X11Blur,          // X11 background blur (inert on Wayland / no x11 feature)
    selection: Option<Selection>,         // the current mouse text selection, if any
    selecting: bool,                      // true while the left button is held for a drag-select
    composing: bool,   // true while an anchored selection is being built (modal)
    shift_press: bool, // the in-flight left-press was Shift-held (candidate for compose entry)
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
    // RT_XDIAG loop-side diagnostic: is the event loop iterating fast (input
    // delivery is the bottleneck) or stalling (a blocking op per iteration)?
    ld_on: bool,
    ld_last: Instant,
    ld_prev: Instant,
    ld_wakes: u32,
    ld_keys: u32,
    ld_maxgap_us: u128,
    active_until: Instant,                // poll fast (animate) until here; set on input/output, else idle-throttle
    last_input: Instant,                  // last keystroke; the cursor soft-blinks for a bounded window after it
    last_anim: Instant,                   // last animation-driven repaint; throttled hard on a software renderer
    low_power: bool,                      // software GL → cap animated-chrome repaints so bling can't peg a weak CPU
    poll_ms: u64,                         // the wake interval set last tick, so latency is judged against it
    scrollback: Rc<std::cell::Cell<usize>>, // live scrollback size for newly spawned panes
    jacks: SharedJacks,                   // per-pane patch-bay pipe jacks (shared with the spawn closure)
    wires: Vec<Wire>,                     // active patch-bay connections
    wiring_from: Option<(rt_core::PaneId, Stream)>, // the armed wire source, mid-gesture
    drag_cursor: Option<(f32, f32)>,      // live cursor (physical px) while dragging a wire
    last_click: Option<(Instant, (f32, f32))>, // time + position of the last left-press
    click_count: u8,                      // 1=single, 2=double (word), 3=triple (line)
    // Arrow-key acceleration: (which arrow 0..4, last-press time, consecutive-repeat count).
    // While an arrow is held, the count grows and `arrow_accel_step` sends more moves/repeat.
    arrow_hold: Option<(u8, Instant, u32)>,
    prefs_open: bool,                     // whether the preferences dialog is showing
    prefs_sel: usize,                     // selected row index into chrome::prefs::rows()
    prefs_scroll: usize,                  // first visible row (the panel scrolls when it doesn't fit)
    prefs_pending: Option<rt_config::Settings>, // edits not yet committed (see PREFS_SETTLE)
    prefs_edits: u64,                     // edits folded into the pending commit (diagnostics)
    last_prefs_edit: Instant,             // when the last edit landed
    picker: Option<chrome::colour_picker::PickerState>, // the colour picker, open over prefs on a swatch click
    manual_open: bool,                    // whether the built-in manual overlay is showing
    manual_scroll: usize,                 // scroll offset (cell rows) into the native manual panel
    search_open: bool,                    // whether the scrollback-search bar is showing
    search_query: String,                 // the current search text
    search_matches: Vec<rt_engine::SearchMatch>, // hits for search_query in search_pane
    search_index: usize,                  // which hit is the "current" one (highlighted brighter)
    search_pane: Option<rt_core::PaneId>, // the pane the current matches belong to
    palette: std::sync::Arc<std::sync::Mutex<rt_engine::Palette>>, // shared so new panes inherit current colours
    font_db: std::sync::Arc<fontdb::Database>, // for reloading fonts on a family change
    font_blobs: render::FontBlobs,        // the current font chains (kept so a size change can reload)
    mono_families: Vec<String>,           // monospace family names for the preferences picker
    damage: crate::damage::DamageAccumulator, // this frame's accumulated pixel damage
    damage_history: std::collections::VecDeque<crate::damage::FrameDamage>, // recent frames' damage, for buffer-age
    force_full: bool,                     // next frame must be a full redraw (scroll/resize/overlay/selection/etc.)
    last_focus: rt_core::PaneId,          // focused pane at the last paint; a change moves the focus border (not cell-damage) → force full
    resize_events: u64,                   // how many Resized events this session has paid for (diagnostics)
    cursor_icon: Option<CursorIcon>,      // shape currently set (None = default); avoids a round trip per motion
    surface_pending: Option<winit::dpi::PhysicalSize<u32>>, // window resized; surface owed and PAINTING SUSPENDED until the settle
    bcast_phase: f32,                     // broadcast swatch pulse phase (0..1), advanced while broadcasting
    last_bcast_tick: Instant,             // when the pulse last advanced
    resize_pending: bool,                 // a Resized arrived; the reflow waits for the drag to settle
    last_resize_at: Instant,              // when the most recent Resized arrived (drives the settle)
    deferred_resizes: u64,                // Resized events coalesced into the pending reflow (diagnostics)
    instr_tick: bool,                     // advance the instrument animation this frame (6fps, native path)
    last_instr_tick: Instant,             // when the instrument animation last advanced
    last_autoscroll: Instant,             // last drag-select edge auto-scroll step (#3)
}

/// A text selection within one pane, anchored to ABSOLUTE buffer lines — the
/// alacritty grid `Line` index (`0..screen_lines` is the visible screen at the
/// bottom, negative is scrollback history), the same coordinate `search` uses.
/// Anchoring to the buffer (not the viewport row) is what lets the highlight
/// ride the scroll instead of staying pinned to the screen. `anchor` is where the
/// drag began, `head` the current end. Text runs linearly (row-major) unless
/// `block`, which selects the RECTANGLE between the two corners (Ctrl-drag).
///
/// A screen row `r` at scroll offset `d` shows absolute line `r - d`; conversely
/// absolute line `l` draws at screen row `l + d`. Selection endpoints are stored
/// as `r - d` at the moment they are set, so a later change in `d` moves the
/// highlight with the text.
#[derive(Clone, Copy)]
struct Selection {
    pane: rt_core::PaneId,  // the pane the selection lives in
    anchor: (usize, i32),   // (col, abs_line) where the drag began
    head: (usize, i32),     // (col, abs_line) current end of the drag
    block: bool,            // rectangular (column-block) selection, not row-major
}

impl Selection {
    /// Return the selection's endpoints ordered so `start` precedes `end` in
    /// row-major reading order (top-to-bottom, left-to-right).
    fn ordered(&self) -> ((usize, i32), (usize, i32)) {
        // Compare by (line, col) so multi-line selections read correctly.
        let a = (self.anchor.1, self.anchor.0); // (line, col)
        let b = (self.head.1, self.head.0);
        if a <= b {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Whether cell `(col, line)` (absolute grid line) falls inside the selection.
    fn contains(&self, col: usize, line: i32) -> bool {
        if self.block {
            // Rectangle: independent column and line ranges between the corners.
            let (c0, c1) = (self.anchor.0.min(self.head.0), self.anchor.0.max(self.head.0));
            let (l0, l1) = (self.anchor.1.min(self.head.1), self.anchor.1.max(self.head.1));
            return line >= l0 && line <= l1 && col >= c0 && col <= c1;
        }
        let (start, end) = self.ordered(); // start precedes end
        let (sc, sr) = start; // (col, line)
        let (ec, er) = end;
        if line < sr || line > er {
            return false; // outside the line span
        }
        // First and last lines are bounded by the columns; middle lines are full.
        let lo = if line == sr { sc } else { 0 };
        let hi = if line == er { ec } else { usize::MAX };
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
    backend: Option<String>, // --backend gl|xrender : override backend::choose_backend's pick
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
            "--backend" => {
                let v = value("--backend");
                if !v.eq_ignore_ascii_case("gl") && !v.eq_ignore_ascii_case("xrender") {
                    eprintln!("rt: --backend must be 'gl' or 'xrender', got '{v}'");
                    std::process::exit(2);
                }
                cli.backend = Some(v);
            }
            "-V" | "--version" => {
                // Crate version, plus the git commit stamped in by build.rs on a
                // from-source build (e.g. "rt 0.2.1 (a1b2c3d)") so a dev build is
                // never mistaken for the release it sits ahead of. option_env!
                // keeps this compiling even if the build script didn't run.
                println!("{}", version_string());
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
                     --backend gl|xrender  override the auto-selected rendering backend\n  \
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
            // Prefer a config whose X11 VISUAL supports transparency, before any
            // other criterion. On X11 the WINDOW's visual — not the GL drawable's
            // alpha_size — decides transparency: a config can report alpha_size 8
            // yet have a 24-bit visual, giving an OPAQUE window (background_opacity
            // is then silently dropped over ssh -X). supports_transparency() is
            // Some(true) only when the config's native visual is 32-bit ARGB, which
            // is what a translucent window over Xwayland needs. Native Wayland
            // already reports transparency-capable configs, so this is a no-op
            // there; and where no such config exists (bare X, no compositor) we
            // fall through to the alpha/sample preference and stay 24-bit.
            configs
                .reduce(|a, b| {
                    let (at, bt) = (
                        a.supports_transparency().unwrap_or(false),
                        b.supports_transparency().unwrap_or(false),
                    );
                    if at != bt {
                        return if bt { b } else { a }; // the transparency-capable one wins
                    }
                    // Tie on transparency: prefer more alpha, then more samples.
                    let better_alpha = b.alpha_size() > a.alpha_size();
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
        // Wrapped in an Arc (the renderer keeps a clone of the live GL context).
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
        // Create the patch-bay dir securely (0700, owned, no planted nodes). If
        // that can't be guaranteed, run WITHOUT the patch-bay rather than wire a
        // pane's stdio into an untrusted fifo — panes still work, just unwireable.
        let patchbay_ok = match ensure_jacks_dir(&jacks_dir) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("rt: patch-bay disabled — could not create {} securely: {e}", jacks_dir.display());
                false
            }
        };
        let jacks: SharedJacks = Rc::new(RefCell::new(std::collections::HashMap::new()));
        let jacks_spawn = jacks.clone();
        // Live scrollback size shared with the spawn factory, so changing it in
        // Preferences takes effect for the next terminal without a restart.
        let scrollback = Rc::new(std::cell::Cell::new(settings.scrollback));
        let scrollback_spawn = scrollback.clone();
        // The factory spawns a shell-backed pane at the requested cell size.
        // Returning `None` on failure lets the session refuse the split/tab
        // gracefully (the initial pane's failure is startup-fatal, handled in
        // Session::new) instead of aborting the whole process under fd/pty
        // exhaustion — panic = "abort" means an .expect() here would kill every
        // other pane too.
        let spawn: Box<dyn FnMut(rt_core::PaneId, usize, usize) -> Option<TermPane>> = Box::new(move |id, cols, rows| {
            // If RT_EXEC is set, run it then keep an interactive shell open.
            let shell = exec.as_ref().map(|cmd| {
                ("/bin/sh".to_string(), vec!["-c".to_string(), format!("{cmd}; exec /bin/sh -i")])
            });
            // Create this pane's patch-bay jacks and advertise them to the shell
            // (only when the session dir was created securely above).
            let env = match patchbay_ok.then(|| Jacks::new(&jacks_dir, id)) {
                Some(Ok(j)) => {
                    let e = j.env();
                    jacks_spawn.borrow_mut().insert(id, j);
                    e
                }
                _ => Vec::new(), // no jacks: the pane still runs, just unwireable
            };
            let mut pane = match TermPane::spawn_env(shell, None, cols.max(1), rows.max(1), &env, scrollback_spawn.get()) {
                Ok(pane) => pane,
                Err(e) => {
                    // Out of ptys/fds: report it and let the session refuse the pane
                    // rather than crash. Undo this pane's jacks so we don't leak a
                    // half-registered patch-bay entry.
                    eprintln!("rt: could not spawn a pane's PTY/shell: {e}");
                    jacks_spawn.borrow_mut().remove(&id);
                    return None;
                }
            };
            if let Ok(p) = palette_spawn.lock() {
                pane.set_palette(p.clone()); // apply the current colours
            }
            Some(pane)
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

        // Store the fully-initialised state and paint once.
        let low_power = renderer.is_software(); // read before `renderer` is moved in
        // Decide which backend *would* be used: a local unix-socket $DISPLAY (or
        // Wayland) keeps the existing GL path; a TCP/forwarded $DISPLAY (`ssh -X`
        // → `localhost:10.x`) picks XRender for mechanism C — unless overridden by
        // `--backend`/`RT_BACKEND`. XRenderBackend doesn't exist yet (arrives in a
        // later mechanism-C task), so for now we log the decision but always build
        // `GlBackend` below regardless of `kind`.
        let display_env = std::env::var("DISPLAY").ok();
        let is_x11 = display_env.is_some() && std::env::var_os("WAYLAND_DISPLAY").is_none();
        let backend_override = self.cli.backend.clone().or_else(|| std::env::var("RT_BACKEND").ok());
        let backend_kind = backend::choose_backend(display_env.as_deref(), is_x11, backend_override.as_deref());
        log::info!("backend: {backend_kind:?}");

        // Wrap the GL renderer + present resources (surface/context/X11 present) as
        // the default backend. `&window` is borrowed here, before it moves into
        // `Active` below. XRender is not yet wired (Task 3): `GlBackend` is built
        // unconditionally, even when `backend_kind` is `XRender`.
        let backend: Box<dyn backend::Backend> = {
            // Mechanism C: on the XRender path, build the command-based backend
            // that draws into this X11 window via x11rb (no GL used at render
            // time). NOTE: for now the GL context above is still created even on
            // this path (harmless for local Xvfb dev); skipping GL entirely on
            // XRender (no GLX-over-ssh) lands with the chrome-degrade task.
            #[cfg(feature = "x11")]
            let xr: Option<Box<dyn backend::Backend>> =
                if matches!(backend_kind, backend::BackendKind::XRender) {
                    xrender_backend::XRenderBackend::try_new(&window, &font_blobs, settings.font_size)
                        .map(|b| Box::new(b) as Box<dyn backend::Backend>)
                } else {
                    None
                };
            #[cfg(not(feature = "x11"))]
            let xr: Option<Box<dyn backend::Backend>> = None;
            xr.unwrap_or_else(|| Box::new(gl_backend::GlBackend::new(renderer, surface, context, &window)))
        };
        let init_focus = session.focus(); // seed last_focus before `session` is moved into Active
        self.active = Some(Active {
            window,
            backend,
            session,
            keymap: Keymap::defaults(),
            mods: ModifiersState::empty(),
            settings,
            mouse: (0.0, 0.0),
            menu: None,
            menu_hover: None,
            ime_preedit: false,
            clipboard,
            bg_effect,
            x11_blur,
            selection: None,
            selecting: false,
            composing: false,
            shift_press: false,
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
            ld_on: std::env::var_os("RT_XDIAG").is_some(),
            ld_last: Instant::now(),
            ld_prev: Instant::now(),
            ld_wakes: 0,
            ld_keys: 0,
            ld_maxgap_us: 0,
            active_until: Instant::now(),
            last_input: Instant::now(),
            last_anim: Instant::now(),
            low_power,
            poll_ms: 16,
            scrollback,
            jacks,
            wires: Vec::new(),
            wiring_from: None,
            drag_cursor: None,
            last_click: None,
            click_count: 0,
            arrow_hold: None,
            prefs_open: false,
            prefs_sel: 0,
            prefs_scroll: 0,
            prefs_pending: None,
            prefs_edits: 0,
            last_prefs_edit: Instant::now(),
            picker: None,
            manual_open: false,
            manual_scroll: 0,
            search_open: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_index: 0,
            search_pane: None,
            palette,
            font_db: self.font_db.clone(),
            font_blobs,
            mono_families: self.mono_families.clone(),
            damage: crate::damage::DamageAccumulator::new(),
            damage_history: std::collections::VecDeque::new(),
            force_full: true, // first frame is always a full redraw
            last_focus: init_focus,
            resize_events: 0,
            cursor_icon: None,
            surface_pending: None,
            bcast_phase: 0.0,
            last_bcast_tick: Instant::now(),
            resize_pending: false,
            last_resize_at: Instant::now(),
            deferred_resizes: 0,
            instr_tick: false,
            last_instr_tick: Instant::now(),
            last_autoscroll: Instant::now(),
        });
        // Debug/verification hook: RT_PREFS opens the preferences dialog at
        // startup so it can be screenshotted without synthetic input.
        if std::env::var("RT_PREFS").is_ok() {
            if let Some(active) = self.active.as_mut() {
                Self::open_prefs(active);
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
        // Test-only hook (undocumented): RT_OPEN_MANUAL opens the manual overlay
        // at startup so the xtrace commands-only regression can drive a native
        // overlay without synthetic input. Fires once, at construction.
        if std::env::var_os("RT_OPEN_MANUAL").is_some() {
            if let Some(active) = self.active.as_mut() {
                active.manual_open = true;
            }
        }
        // Debug/verification hook: RT_OPEN_PREFS opens the preferences dialog at
        // startup so the Xvfb gate can screenshot it without synthetic input
        // (mirrors RT_OPEN_MANUAL).
        if std::env::var_os("RT_OPEN_PREFS").is_some() {
            if let Some(active) = self.active.as_mut() {
                Self::open_prefs(active);
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
        if active.ld_on && matches!(&event, WindowEvent::KeyboardInput { event: ke, .. } if ke.state == ElementState::Pressed) {
            active.ld_keys += 1; // RT_XDIAG: key presses actually delivered to rt this second
        }
        // Any real interaction (not our own repaint) keeps the loop in the fast,
        // animating poll for a moment; when nothing happens it idle-throttles.
        if !matches!(event, WindowEvent::RedrawRequested) {
            active.active_until = Instant::now() + ACTIVE_TAIL;
        }

        // The colour picker sits modally over the prefs dialog (opened by clicking
        // a swatch). Pointer-driven: drag the SV square or hue strip; Esc / Done /
        // a press outside commit the pending colour and close it. All input
        // swallowed; lifecycle events fall through. A drag writes the chosen RGB
        // into `prefs_pending` and arms the settle, so the terminal recolours once
        // (via `commit_settings`) rather than per pointer-move — cheap over ssh -X.
        if active.picker.is_some() {
            use chrome::colour_picker as cp;
            let size = active.window.inner_size();
            let (cw, ch) = active.backend.cell_size();
            let g = cp::layout(cw, ch, size.width as f32, size.height as f32);
            match &event {
                WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                    if !g.panel.contains(active.mouse) {
                        Self::close_picker(active); // a click outside dismisses
                        return;
                    }
                    match cp::hit(&g, active.mouse) {
                        cp::Hit::Close => Self::close_picker(active),
                        cp::Hit::Sv => {
                            let (s, v) = cp::sv_at(&g, active.mouse);
                            {
                                let pk = active.picker.as_mut().unwrap();
                                pk.drag = Some(cp::Drag::Sv);
                                pk.s = s;
                                pk.v = v;
                            }
                            Self::picker_write(active);
                        }
                        cp::Hit::Hue => {
                            let h = cp::hue_at(&g, active.mouse);
                            {
                                let pk = active.picker.as_mut().unwrap();
                                pk.drag = Some(cp::Drag::Hue);
                                pk.h = h;
                            }
                            Self::picker_write(active);
                        }
                        cp::Hit::None => {} // press on panel chrome: swallow, do nothing
                    }
                    active.window.request_redraw();
                    return;
                }
                WindowEvent::CursorMoved { position, .. } => {
                    active.mouse = (position.x as f32, position.y as f32);
                    if let Some(d) = active.picker.and_then(|p| p.drag) {
                        {
                            let pk = active.picker.as_mut().unwrap();
                            match d {
                                cp::Drag::Sv => {
                                    let (s, v) = cp::sv_at(&g, active.mouse);
                                    pk.s = s;
                                    pk.v = v;
                                }
                                cp::Drag::Hue => pk.h = cp::hue_at(&g, active.mouse),
                            }
                        }
                        Self::picker_write(active);
                        active.window.request_redraw();
                    }
                    return;
                }
                WindowEvent::MouseInput { state: ElementState::Released, button: MouseButton::Left, .. } => {
                    if let Some(pk) = active.picker.as_mut() {
                        pk.drag = None;
                    }
                    return;
                }
                WindowEvent::KeyboardInput { event: ke, .. } if ke.state == ElementState::Pressed => {
                    if matches!(ke.logical_key, Key::Named(NamedKey::Escape)) {
                        Self::close_picker(active);
                    }
                    return; // swallow every key while the picker is up
                }
                WindowEvent::KeyboardInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::Ime(_)
                | WindowEvent::ModifiersChanged(_) => return,
                _ => {} // lifecycle events fall through
            }
        }

        // Preferences: a native dialog on BOTH backends (Task 4) — no egui
        // involved at all. Keys/clicks drive the dialog and never reach the PTY.
        // Every edit is one discrete step (no drags — see PREFS_SETTLE and the
        // design doc): it mutates PENDING settings and arms the settle;
        // `commit_settings` applies+persists ONCE per run of edits. Esc / Close
        // commit any pending edit immediately, so a fast Esc cannot strand it.
        if active.prefs_open {
            match &event {
                WindowEvent::KeyboardInput { event: ke, .. } if ke.state == ElementState::Pressed => {
                    let size = active.window.inner_size();
                    let (cw, ch) = active.backend.cell_size();
                    let s = active.prefs_pending.clone().unwrap_or_else(|| active.settings.clone());
                    let cols = (content_bounds(size).w / cw).max(1.0) as usize;
                    let rws = chrome::prefs::rows(&s, total_ram_bytes(), cols);
                    let g = chrome::prefs::layout(&rws, active.prefs_scroll, cw, ch, size.width as f32, size.height as f32);
                    match &ke.logical_key {
                        Key::Named(NamedKey::Escape) => {
                            active.prefs_open = false;
                            // Do not strand a pending edit: commit it now rather
                            // than wait out PREFS_SETTLE on a closed dialog.
                            if let Some(new) = active.prefs_pending.take() {
                                active.prefs_edits = 0;
                                Self::commit_settings(active, new);
                            }
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            active.prefs_sel = chrome::prefs::next_sel(&rws, active.prefs_sel, 1);
                            active.prefs_scroll = chrome::prefs::scroll_for(&rws, active.prefs_sel, active.prefs_scroll, g.visible);
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            active.prefs_sel = chrome::prefs::next_sel(&rws, active.prefs_sel, -1);
                            active.prefs_scroll = chrome::prefs::scroll_for(&rws, active.prefs_sel, active.prefs_scroll, g.visible);
                        }
                        Key::Named(NamedKey::ArrowLeft) => Self::prefs_step(active, &rws, -1),
                        Key::Named(NamedKey::ArrowRight) => Self::prefs_step(active, &rws, 1),
                        Key::Named(NamedKey::Enter) | Key::Named(NamedKey::Space) => {
                            if rws.get(active.prefs_sel).and_then(|r| r.pref) == Some(prefs_model::PrefRow::Close) {
                                active.prefs_open = false;
                                if let Some(new) = active.prefs_pending.take() {
                                    active.prefs_edits = 0;
                                    Self::commit_settings(active, new);
                                }
                            } else {
                                Self::prefs_step(active, &rws, 1);
                            }
                        }
                        _ => {}
                    }
                    active.window.request_redraw();
                    return;
                }
                WindowEvent::MouseInput { state: ElementState::Pressed, button: MouseButton::Left, .. } => {
                    let size = active.window.inner_size();
                    let (cw, ch) = active.backend.cell_size();
                    let s = active.prefs_pending.clone().unwrap_or_else(|| active.settings.clone());
                    let cols = (content_bounds(size).w / cw).max(1.0) as usize;
                    let rws = chrome::prefs::rows(&s, total_ram_bytes(), cols);
                    let g = chrome::prefs::layout(&rws, active.prefs_scroll, cw, ch, size.width as f32, size.height as f32);
                    match chrome::prefs::hit(&g, active.mouse) {
                        Some(chrome::prefs::Hit::Step(i, dir)) => {
                            active.prefs_sel = i;
                            Self::prefs_step(active, &rws, dir);
                        }
                        Some(chrome::prefs::Hit::Row(i)) => {
                            if rws[i].pref.is_some() {
                                active.prefs_sel = i;
                                // A click on a Toggle toggles it; on a Step row it only selects.
                                if matches!(rws[i].kind, chrome::prefs::RowKind::Toggle) {
                                    Self::prefs_step(active, &rws, 1);
                                } else if rws[i].pref == Some(prefs_model::PrefRow::Close) {
                                    active.prefs_open = false;
                                    if let Some(new) = active.prefs_pending.take() {
                                        active.prefs_edits = 0;
                                        Self::commit_settings(active, new);
                                    }
                                }
                            } else if matches!(rws[i].kind, chrome::prefs::RowKind::Swatches) {
                                // Open the colour picker on the clicked swatch.
                                let mut sw = vec![s.foreground, s.background];
                                sw.extend(s.palette.iter().copied());
                                let rects = chrome::prefs::swatch_rects(g.rows[i], sw.len(), ch);
                                if let Some(k) = rects.iter().position(|r| r.contains(active.mouse)) {
                                    let slot = chrome::colour_picker::Slot::from_swatch_index(k);
                                    active.picker =
                                        Some(chrome::colour_picker::PickerState::new(slot, sw[k]));
                                }
                            }
                        }
                        _ => {}
                    }
                    active.window.request_redraw();
                    return;
                }
                // Keep the last-known cursor position current while the dialog is
                // up, so a click that follows (without further motion outside the
                // dialog) hit-tests against where the pointer actually is, rather
                // than a stale pre-open position.
                WindowEvent::CursorMoved { position, .. } => {
                    active.mouse = (position.x as f32, position.y as f32);
                    return;
                }
                // Swallow all other input so it cannot reach the PTY.
                WindowEvent::KeyboardInput { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::Ime(_)
                | WindowEvent::ModifiersChanged(_) => return,
                _ => {} // Close / resize / redraw fall through
            }
        }

        // The manual overlay: Esc/F1/q close it, arrows/PageUp-Down/wheel scroll,
        // and every other input is swallowed so it can't reach the PTY. One native
        // handler on both backends (no egui); lifecycle events fall through.
        if active.manual_open {
            let size = active.window.inner_size();
            let (cw, ch) = active.backend.cell_size();
            let g = chrome::manual::layout(size.width as f32, size.height as f32, cw, ch);
            match &event {
                WindowEvent::KeyboardInput { event: ke, .. }
                    if ke.state == ElementState::Pressed =>
                {
                    match &ke.logical_key {
                        Key::Named(NamedKey::Escape) | Key::Named(NamedKey::F1) => {
                            active.manual_open = false;
                        }
                        Key::Named(NamedKey::ArrowDown) => {
                            active.manual_scroll =
                                chrome::manual::clamp_scroll(active.manual_scroll + 1, &g);
                        }
                        Key::Named(NamedKey::ArrowUp) => {
                            active.manual_scroll = chrome::manual::clamp_scroll(
                                active.manual_scroll.saturating_sub(1),
                                &g,
                            );
                        }
                        Key::Named(NamedKey::PageDown) => {
                            active.manual_scroll =
                                chrome::manual::clamp_scroll(active.manual_scroll + 10, &g);
                        }
                        Key::Named(NamedKey::PageUp) => {
                            active.manual_scroll = chrome::manual::clamp_scroll(
                                active.manual_scroll.saturating_sub(10),
                                &g,
                            );
                        }
                        Key::Character(s) if s.as_str() == "q" => {
                            active.manual_open = false;
                        }
                        _ => {}
                    }
                    active.window.request_redraw(); // scroll moved / overlay closed
                    return;
                }
                WindowEvent::MouseWheel { delta, .. } => {
                    // Positive y = wheel up = scroll toward the top (fewer rows).
                    let lines = match delta {
                        MouseScrollDelta::LineDelta(_, y) => *y as isize,
                        MouseScrollDelta::PixelDelta(p) => (p.y / 20.0) as isize,
                    };
                    let next = (active.manual_scroll as isize - lines).max(0) as usize;
                    active.manual_scroll = chrome::manual::clamp_scroll(next, &g);
                    active.window.request_redraw();
                    return;
                }
                // Swallow all other input (releases, keys, mouse, IME, mods) so
                // nothing leaks to the PTY; lifecycle events fall through.
                WindowEvent::KeyboardInput { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::CursorMoved { .. }
                | WindowEvent::Ime(_)
                | WindowEvent::ModifiersChanged(_) => return,
                _ => {}
            }
            // Fell through: a non-input (lifecycle) event — let normal handling run.
        }

        // The context menu: native hover/click hit-testing, Escape or a click
        // outside closes it, and terminal input is suspended while it is up.
        // The context menu: one native handler on both backends. Hover-highlight
        // on motion, act on a click of an enabled row, dismiss on an outside press
        // / Escape. All input swallowed; lifecycle events fall through.
        if let Some(pos) = active.menu {
            // Rebuild the exact rows + geometry the native draw uses this frame.
            let url = Self::cell_at(active, pos.0, pos.1)
                .and_then(|(pane, col, row)| Self::url_at(active, pane, col, row));
            let has_sel = Self::selected_text(active).is_some();
            let rows = menu::rows(&active.keymap, has_sel, url.as_deref());
            let size = active.window.inner_size();
            let (cw, ch) = active.backend.cell_size();
            let g = chrome::menu::layout(&rows, pos, cw, ch, size.width as f32, size.height as f32);
            match &event {
                WindowEvent::CursorMoved { position, .. } => {
                    active.mouse = (position.x as f32, position.y as f32);
                    active.menu_hover = chrome::menu::hit_row(&g, active.mouse);
                    active.window.request_redraw();
                    return;
                }
                WindowEvent::KeyboardInput { event: ke, .. }
                    if ke.state == ElementState::Pressed =>
                {
                    // Escape dismisses; every other key is swallowed while open.
                    if matches!(ke.logical_key, Key::Named(NamedKey::Escape)) {
                        active.menu = None;
                        active.menu_hover = None;
                        active.window.request_redraw();
                    }
                    return;
                }
                WindowEvent::MouseInput { state, button, .. }
                    if *state == ElementState::Pressed =>
                {
                    if g.panel.contains(active.mouse) {
                        // Left-click on an ENABLED row acts + closes; a click on a
                        // disabled row or separator is ignored (menu stays open).
                        if *button == MouseButton::Left {
                            if let Some(i) = chrome::menu::hit_row(&g, active.mouse) {
                                if rows[i].enabled {
                                    active.menu = None;
                                    active.menu_hover = None;
                                    active.window.request_redraw();
                                    if let Some(a) =
                                        rows.into_iter().nth(i).and_then(|r| r.action)
                                    {
                                        match a.into_pick() {
                                            menu::MenuPick::Do(act) => {
                                                Self::apply_action(active, act)
                                            }
                                            menu::MenuPick::OpenUrl(u) => Self::open_url(&u),
                                            menu::MenuPick::CopyUrl(u) => {
                                                if let Some(cb) = &active.clipboard {
                                                    cb.store(u);
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                        // Any press inside the panel is swallowed.
                        return;
                    }
                    // A press outside the panel dismisses the menu.
                    active.menu = None;
                    active.menu_hover = None;
                    active.window.request_redraw();
                    return;
                }
                // Swallow remaining input (releases, wheel, IME, mods); lifecycle
                // events fall through to normal handling.
                WindowEvent::KeyboardInput { .. }
                | WindowEvent::MouseInput { .. }
                | WindowEvent::MouseWheel { .. }
                | WindowEvent::Ime(_)
                | WindowEvent::ModifiersChanged(_) => return,
                _ => {}
            }
        }

        // The scrollback-search bar is a lighter overlay: it captures typing and
        // Enter/Escape for navigation, but leaves the terminal visible and lets
        // mouse events (scroll/click) through so the user can still look around
        // while a search is open. One native handler on both backends: the query
        // is edited directly (printable text appends + re-runs; Backspace pops;
        // Enter/Shift-Enter step; Escape closes); mouse/lifecycle fall through.
        if active.search_open {
            match &event {
                WindowEvent::KeyboardInput { event: ke, .. } => {
                    if ke.state == ElementState::Pressed {
                        match &ke.logical_key {
                            Key::Named(NamedKey::Escape) => {
                                Self::close_search(active);
                            }
                            Key::Named(NamedKey::Enter) => {
                                let dir: isize = if active.mods.shift_key() { -1 } else { 1 };
                                Self::search_step(active, dir);
                                active.window.request_redraw();
                            }
                            Key::Named(NamedKey::Backspace) => {
                                active.search_query.pop();
                                Self::run_search(active, true); // re-run + redraw
                            }
                            _ => {
                                // Append this key's produced text if it is printable.
                                if let Some(t) = ke.text.as_ref().filter(|t| !t.is_empty()) {
                                    if t.chars().all(|c| !c.is_control()) {
                                        active.search_query.push_str(t);
                                        Self::run_search(active, true); // re-run + redraw
                                    }
                                }
                            }
                        }
                    }
                    return; // swallow all keyboard (presses acted on, releases dropped)
                }
                WindowEvent::ModifiersChanged(m) => {
                    active.mods = m.state(); // Shift+Enter needs live modifiers
                    return;
                }
                WindowEvent::Ime(_) => return, // no IME into the native query
                // Mouse + lifecycle fall through to the main match below.
                _ => {}
            }
        }

        match event {
            // The user closed the window (title-bar button / compositor). Exit
            // the same way the last-pane-closed path does — `process::exit`,
            // NOT `event_loop.exit()`. The latter unwinds and Drops the GL
            // context and Wayland blur objects, whose teardown ordering faults (a
            // segfault on Wayland, an X11 GetGeometry panic on the x11 dev build).
            // The OS reclaims everything; the PTY children get SIGHUP. This matches
            // SessionEvent::CloseWindow below.
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

            // Window resized: defer EVERYTHING to the settle (see RESIZE_SETTLE).
            WindowEvent::Resized(size) => {
                // Do nothing now -- not even repaint. Two costs hide in a drag, and
                // both are paid per configure event, ~20 times, for sizes that are
                // superseded within ~50ms and never looked at:
                //
                //   relayout: Term::resize per pane = grid AND full scrollback,
                //             676ms MEDIAN on a milkv with 10k lines of history.
                //   repaint:  force_full = the ENTIRE window re-shipped as glyph
                //             commands; on a weak/remote link that is the slow
                //             operation this whole backend exists to avoid.
                //
                // Deferring only the reflow still left the window repainting per
                // event: ~3s of settling showing 5-12 discarded intermediate
                // frames. Terminator has always done the obvious thing here --
                // repaint at the RESTING size -- so do that: record the size,
                // suspend painting, and pay surface+reflow+one full frame once the
                // size holds still. While suspended the X window keeps its old
                // content (newly exposed area shows the background), which is
                // exactly how Terminator looks mid-drag.
                //
                // The surface resize is deferred too: with painting suspended
                // nothing needs the back buffer to match, and recreating both
                // pixmaps + clearing a full-window ARGB layer per configure was
                // itself area-proportional server work, ~20 times a drag.
                active.surface_pending = Some(size); // painting is suspended while Some
                active.resize_pending = true; // surface + reflow owed at the settle
                active.last_resize_at = Instant::now();
                active.deferred_resizes += 1;
                active.resize_events += 1;
                log::debug!("resize #{} to {}x{}: deferred (painting suspended)",
                    active.resize_events, size.width, size.height);
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
                    active.force_full = true; // scrollback offset changed: cell→px mapping shifts
                    active.window.request_redraw(); // repaint at the new offset
                }
            }

            // Track the cursor; when a menu is open, update its hover highlight.
            WindowEvent::CursorMoved { position, .. } => {
                active.mouse = (position.x as f32, position.y as f32); // physical px
                // A divider should say it can be dragged. Only while nothing else
                // owns the pointer, and only re-issued when the shape actually
                // changes — set_cursor is an X round trip, and motion events
                // arrive at pointer-sample rate.
                Self::update_cursor(active);
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
                    // The rubber-band spans arbitrary pixels between panes, so it
                    // cannot ride the partial path: every pointer sample costs a
                    // FULL window repaint. Where a frame is cheap (hardware GL)
                    // that is a nice animation. Where it is expensive (XRender over
                    // ssh -X, software GL) it turned wiring into many seconds of
                    // stepping — an animation too slow to follow, paid for with the
                    // one operation this backend exists to avoid. Skip it there:
                    // the release paints the COMPLETED wire in one frame, which is
                    // the state worth seeing. Feedback meanwhile is the Grabbing
                    // cursor set at press — free, no frame required.
                    if !active.backend.is_software() {
                        active.force_full = true; // rubber-band spans arbitrary pixels: repaint fully
                        active.window.request_redraw();
                    }
                } else if let Some(handle) = active.dragging_divider.clone() {
                    // Resize the split: turn the mouse position along the split's
                    // axis into a first-child ratio.
                    let axis = if handle.horizontal { active.mouse.0 } else { active.mouse.1 };
                    let ratio = ((axis - handle.start) / handle.len).clamp(0.05, 0.95);
                    // Move the split now, reflow when the drag settles. Reflowing
                    // per motion event cost ~676ms EACH on a milkv with full
                    // scrollback, so the divider crawled in ~1s steps. The divider
                    // itself still tracks the pointer live (pane rects come from
                    // the tree); only the cell grids wait for the settle. Same
                    // mechanism as WindowEvent::Resized — see RESIZE_SETTLE.
                    active.session.set_split_ratio_no_reflow(&handle, ratio);
                    active.resize_pending = true; // reflow owed once the drag stops
                    active.last_resize_at = Instant::now();
                    active.deferred_resizes += 1;
                    active.force_full = true; // layout changed: repaint the whole window
                    active.window.request_redraw();
                } else if active.selecting {
                    // Extend the selection to the cell under the pointer, anchored to
                    // the absolute buffer line (so it stays put as the view scrolls).
                    if let Some((pane, col, row)) = Self::cell_at(active, active.mouse.0, active.mouse.1) {
                        let off = active.session.pane(pane).map(|p| p.scroll_info().0 as i32).unwrap_or(0);
                        if let Some(sel) = active.selection.as_mut() {
                            if sel.pane == pane {
                                sel.head = (col, row as i32 - off); // move the drag end
                                active.shift_press = false; // a drag, not a click → not compose entry
                                active.force_full = true; // selection highlight isn't engine-tracked damage
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
                        // Focus-follows-mouse produces no engine cell-damage, so
                        // nothing else would schedule a frame — ask for one. The
                        // frame builder's central `focus != last_focus` check forces
                        // it full (so the blue border clears off the old pane and
                        // fully draws on the new); no per-site force_full needed, and
                        // omitting it keeps the *following* keystroke frame scissored.
                        active.window.request_redraw();
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
                        Self::update_cursor(active); // drop Grabbing; the wire is cancelled
                        active.force_full = true; // clear the cancelled rubber-band ghost (off the partial path)
                        active.window.request_redraw();
                        return;
                    }
                    if let Some((id, stream)) = Self::jack_at(active, active.mouse.0, active.mouse.1) {
                        active.wires.retain(|w| !(w.src == id && w.stream == stream));
                        active.force_full = true; // clear the removed wire's ghost (spans arbitrary inter-pane pixels)
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
                    // actions apply to the pane you right-clicked. The native menu
                    // is anchored here and clamped on-screen by `chrome::menu`.
                    active.session.focus_at(active.mouse.0, active.mouse.1);
                    active.menu = Some(active.mouse);
                    active.menu_hover = None; // no row highlighted until the pointer moves
                    active.window.request_redraw();
                }
                (ElementState::Pressed, MouseButton::Left) => {
                    // While composing an anchored selection, a click finishes or
                    // aborts it — it never starts a new selection. EXCEPT: a
                    // Shift+double/triple-click is two press/release pairs, and the
                    // FIRST press always resolves as a plain single-click (the
                    // click-count counter only advances on the *next* press), so its
                    // release — no drag, shift held — promotes into compose before the
                    // second press ever arrives. Without this check that second press
                    // would just cancel compose and swallow the word/line selection.
                    // Detect that continuation with the same timing+proximity test the
                    // click-count logic below uses, and if it matches, abandon compose
                    // and fall through so the word/line select still runs.
                    if active.composing {
                        let now = Instant::now();
                        let (mx, my) = active.mouse;
                        let continuation = matches!(active.last_click, Some((t, (lx, ly)))
                            if now.duration_since(t) < Duration::from_millis(400)
                                && (mx - lx).abs() < 5.0 && (my - ly).abs() < 5.0);
                        if continuation {
                            // A Shift+double/triple-click whose first release entered
                            // compose: abandon compose and let the normal word/line
                            // select below run.
                            active.composing = false;
                            active.shift_press = false;
                        } else if active.mods.shift_key() {
                            // Shift+click commits — but only in the composing pane: it
                            // sets the end at the clicked cell and copies. A Shift+click
                            // in a DIFFERENT pane cancels instead (the spec's "start
                            // fresh there" — a new anchor drops on the next click).
                            let hit = Self::cell_at(active, mx, my);
                            let same = matches!((hit, active.selection),
                                (Some((p, ..)), Some(s)) if p == s.pane);
                            if same {
                                if let Some((pane, col, row)) = hit {
                                    if let Some(sel) = active.selection.as_mut() {
                                        let off = active
                                            .session
                                            .pane(pane)
                                            .map(|p| p.scroll_info().0 as i32)
                                            .unwrap_or(0);
                                        sel.head = (col, row as i32 - off);
                                    }
                                }
                                Self::compose_commit(active);
                            } else {
                                Self::compose_cancel(active);
                            }
                            return;
                        } else {
                            Self::compose_cancel(active); // a plain click cancels
                            return;
                        }
                    }
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
                            // Say "you are dragging a wire" without costing a frame.
                            // On the expensive paths the rubber-band is suppressed
                            // (see CursorMoved), so this cursor is the ONLY feedback
                            // until the release paints the finished wire.
                            active.window.set_cursor(CursorIcon::Grabbing);
                            active.cursor_icon = Some(CursorIcon::Grabbing);
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
                            // The visible pane set changes but the newly-shown
                            // panes have no engine damage (their content didn't
                            // change), so force a full redraw — otherwise the
                            // XRender back buffer keeps the previous tab's pixels
                            // (stale content, and re-switching to an already-drawn
                            // tab shows nothing new). Matches the keyboard path.
                            active.force_full = true;
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
                                // Anchor to the absolute buffer line so the highlight
                                // rides the scroll (screen row `row` at offset `d`
                                // shows absolute line `row - d`).
                                let off = active.session.pane(pane).map(|p| p.scroll_info().0 as i32).unwrap_or(0);
                                let line = row as i32 - off;
                                match count {
                                    2 => {
                                        if let Some(((sc, sr), (ec, er))) = Self::word_at(active, pane, col, row) {
                                            // Screen rows → absolute buffer lines (so the
                                            // highlight rides the scroll); the word may span a
                                            // soft-wrap, so start/end can be on different lines.
                                            active.selection = Some(Selection {
                                                pane,
                                                anchor: (sc, sr as i32 - off),
                                                head: (ec, er as i32 - off),
                                                block: false,
                                            });
                                        }
                                        active.selecting = false;
                                        Self::copy_selection_to_primary(active);
                                    }
                                    3 => {
                                        // Whole LOGICAL line: expand across soft-wraps so a
                                        // long wrapped line (a key/URL) selects as one piece.
                                        // selection_text rejoins the wraps, so no newlines or
                                        // extra spaces are inserted — only what's really there.
                                        let (mut sr, mut er) = (row, row);
                                        if let Some(p) = active.session.pane(pane) {
                                            let screen = p.scroll_info().2; // visible row count
                                            while sr > 0 && p.line_wrapped(sr - 1) {
                                                sr -= 1; // up to the logical line's first row
                                            }
                                            while er + 1 < screen && p.line_wrapped(er) {
                                                er += 1; // down to its last row
                                            }
                                        }
                                        let last = Self::line_last_col(active, pane, er);
                                        active.selection = Some(Selection {
                                            pane,
                                            anchor: (0, sr as i32 - off),
                                            head: (last, er as i32 - off),
                                            block: false,
                                        });
                                        active.selecting = false;
                                        Self::copy_selection_to_primary(active);
                                    }
                                    _ => {
                                        // Ctrl-drag = rectangular block. (Ctrl-click ON a
                                        // URL already returned above to open it, so a
                                        // block only ever starts on non-link text.)
                                        let block = active.mods.control_key();
                                        active.selection = Some(Selection { pane, anchor: (col, line), head: (col, line), block });
                                        active.selecting = true;
                                        // A Shift-held press with no ensuing drag becomes an
                                        // anchored-compose start (resolved at release).
                                        active.shift_press = active.mods.shift_key();
                                    }
                                }
                                active.force_full = true; // selection highlight changed
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
                        Self::update_cursor(active); // drop Grabbing; wiring_from is now None
                        // The rubber-band was drawn last frame across arbitrary pixels; whether
                        // this release connected a wire or aborted (no pane / rejected self-loop),
                        // its old pixels must be cleared, so force a full frame off the partial path.
                        active.force_full = true;
                        active.window.request_redraw();
                        return;
                    }
                    active.dragging_divider = None; // end any divider resize
                    if active.scroll_drag.take().is_some() {
                        // The scrollbar drag skipped the instrument layer (kept the
                        // drag cheap); repaint it now the drag has ended.
                        active.force_full = true;
                        active.window.request_redraw();
                    }
                    active.selecting = false; // drag finished
                    // A zero-length selection was just a click: discard it, and
                    // (copy-on-select) copy a real selection to PRIMARY.
                    if let Some(sel) = active.selection {
                        if sel.anchor == sel.head {
                            if active.shift_press {
                                // No drag followed a Shift-press: promote to anchored
                                // compose. Keep the (zero-length) selection as the anchor.
                                active.composing = true;
                            } else {
                                active.selection = None; // a plain click: discard
                            }
                        } else if let Some(text) = Self::selected_text(active) {
                            if let Some(cb) = &active.clipboard {
                                cb.store_primary(text); // PRIMARY for middle-click paste
                            }
                        }
                        active.force_full = true; // selection cleared/finalised: repaint highlight
                        active.window.request_redraw();
                    }
                    active.shift_press = false; // consumed
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
        if active.ld_on {
            // Is the loop iterating briskly (input-delivery bound) or stalling
            // (a blocking op per iteration = a huge max gap)?
            let gap = now.duration_since(active.ld_prev).as_micros();
            active.ld_prev = now;
            active.ld_wakes += 1;
            active.ld_maxgap_us = active.ld_maxgap_us.max(gap);
            if active.ld_last.elapsed() >= Duration::from_secs(1) {
                eprintln!(
                    "loopdiag: {} wake/s, max gap {} ms, {} key/s, poll={}ms",
                    active.ld_wakes, active.ld_maxgap_us / 1000, active.ld_keys, active.poll_ms
                );
                active.ld_last = now;
                active.ld_wakes = 0;
                active.ld_maxgap_us = 0;
                active.ld_keys = 0;
            }
        }
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
        let mut exited: Vec<rt_core::PaneId> = Vec::new(); // panes whose shell died cleanly (reap)
        let mut nonzero_exits: Vec<(rt_core::PaneId, i32)> = Vec::new(); // died abnormally (keep + badge)
        let mut crashed: Vec<rt_core::PaneId> = Vec::new(); // parser thread panicked (keep + badge)
        let mut titles: Vec<(rt_core::PaneId, String)> = Vec::new(); // pending title updates
        for id in active.session.tree().all_panes() {
            if let Some(pane) = active.session.pane(id) {
                for ev in pane.drain_events() {
                    match ev {
                        rt_engine::PaneEvent::Exited(code) => match code {
                            // Abnormal exit (non-zero code / signal): keep the pane so its
                            // final output stays visible, and badge its header with the
                            // status. A clean exit (0) or unknown status auto-closes below.
                            Some(n) if n != 0 => {
                                nonzero_exits.push((id, n));
                                dirty = true;
                            }
                            _ => exited.push(id), // reap this pane
                        },
                        rt_engine::PaneEvent::Crashed => {
                            // The pane's parser thread panicked and was caught (isolated):
                            // keep it frozen at its last grid with a badge, don't reap.
                            crashed.push(id);
                            dirty = true;
                        }
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
        // Badge the header of every pane that died abnormally, so a crashed shell (or a
        // non-zero build) is visible at a glance instead of the pane silently vanishing.
        // Applied after the title updates above so it wins for this tick.
        for (id, code) in nonzero_exits {
            let base = active.session.title_of(id).unwrap_or("").to_string();
            let badged = if base.contains("[exited") {
                base // already tagged (shouldn't happen: Exited fires once)
            } else if base.is_empty() {
                format!("[exited {code}]")
            } else {
                format!("{base} [exited {code}]")
            };
            active.session.set_title(id, badged);
        }
        // Badge every pane whose parser thread crashed (isolated by catch_unwind): it stays
        // open, frozen at its last output, so the crash is visible rather than a silent freeze.
        for id in crashed {
            let base = active.session.title_of(id).unwrap_or("").to_string();
            if !base.contains("[crashed]") {
                let badged =
                    if base.is_empty() { "[crashed]".to_string() } else { format!("{base} [crashed]") };
                active.session.set_title(id, badged);
            }
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
            active.force_full = true; // clear removed-wire ghosts (relayout usually rescues, but be explicit)
            if matches!(active.wiring_from, Some((s, _)) if s == id) {
                active.wiring_from = None;
            }
        }
        // A resize drag owes us exactly one reflow. Pay it once the size has held
        // still for RESIZE_SETTLE — never per configure event (see the Resized
        // handler for the measurements: 676ms median per reflow, ~20 events per
        // drag, ~10.6s of frozen window). Uses the CURRENT size, so every
        // intermediate size collapses into this single reflow.
        if active.resize_pending && now.duration_since(active.last_resize_at) >= RESIZE_SETTLE {
            // A window resize also owes the surface work, skipped per-event above.
            // Use the CURRENT size: every intermediate one collapses into this.
            let size = active.window.inner_size();
            if active.surface_pending.take().is_some() {
                if let (Some(w), Some(h)) = (NonZeroU32::new(size.width), NonZeroU32::new(size.height)) {
                    active.backend.resize_surface(w, h); // back buffer + instrument layer
                }
                active.backend.resize(size.width as f32, size.height as f32); // viewport
                if let Some(fx) = &mut active.bg_effect {
                    fx.on_resize(size.width, size.height); // blur region follows the surface
                }
            }
            let bounds = content_bounds(size);
            let t0 = Instant::now();
            active.session.relayout(bounds); // reflow + push the settled size to the PTYs
            let took = t0.elapsed();
            log::info!(
                "resize settled at {}x{}: relayout={:.1}ms, {} event(s) coalesced into 1 reflow",
                size.width, size.height, took.as_secs_f32() * 1e3, active.deferred_resizes,
            );
            active.resize_pending = false;
            active.deferred_resizes = 0;
            active.force_full = true; // grid changed under us: repaint the lot
            dirty = true;
        }
        // Preferences: one commit per run of edits, not one per keystroke (see
        // PREFS_SETTLE and `commit_settings`).
        if active.prefs_pending.is_some() && now.duration_since(active.last_prefs_edit) >= PREFS_SETTLE {
            let new = active.prefs_pending.take().unwrap();
            let n = std::mem::take(&mut active.prefs_edits);
            let t0 = Instant::now();
            Self::commit_settings(active, new);
            log::info!(
                "prefs settled: commit={:.1}ms, {} edit(s) coalesced into 1 commit",
                t0.elapsed().as_secs_f32() * 1e3, n,
            );
            dirty = true;
        }
        // Broadcast pulse: while input is fanned out to more than the focused
        // pane, the receiving panes' swatches throb so an armed broadcast is hard
        // to miss. Bounded at BCAST_PULSE_TICK, and the frame builder damages ONLY
        // the swatches (see swatch_rect), so this rides the scissored path — a few
        // 13x13px redraws per tick, not a whole-window repaint.
        let broadcasting = !matches!(active.session.broadcast(), Broadcast::Off);
        if broadcasting {
            let dt = now.duration_since(active.last_bcast_tick);
            if dt >= BCAST_PULSE_TICK {
                active.last_bcast_tick = now;
                active.bcast_phase = (active.bcast_phase + dt.as_secs_f32() * BCAST_PULSE_HZ).fract();
                dirty = true;
            }
        }
        // Animated chrome: wire packets, the CPU-heat / output-flow border
        // instruments, and the latency flare. Keep calling the samplers (they
        // update state every tick), but collect whether anything WANTS to animate
        // into `anim` rather than forcing a repaint — on a software renderer each
        // repaint is real CPU, so animation-only repaints are throttled below.
        let mut anim = false;
        // Always pump the patch-bay (moves bytes) and sample heat (/proc) — these
        // are side effects that must run every tick. Whether they DRIVE an
        // animation repaint is gated: on the remote XRender backend the animated
        // chrome lives on the pane borders, so a repaint re-sends the whole screen
        // (slow over ssh -X on a weak box). So instrument animation there is off
        // unless `inst_animate` opts in; the local GL backend always animates.
        let pumped = Self::pump_wires(active);
        let heat_live = Self::sample_heat(active) && active.heat.values().any(|&h| h > 0.02);
        let animate_instruments = active.backend.is_gl()
            || (active.settings.inst_remote && active.settings.inst_animate);
        if animate_instruments {
            if pumped || active.wires.iter().any(|w| w.rate > 1.0) {
                anim = true; // wire packets still moving
            }
            if heat_live {
                anim = true; // a warm pane's heat border stays live
            }
            if active.meters.values().any(|m| m.rate > 0.5) {
                anim = true; // output-flow easing to a stop
            }
            if active.stall > 0.02 {
                anim = true; // latency flare fading out
            }
        }
        // The focused cursor soft-blinks for a bounded window after typing — but a
        // smooth pulse needs ~20fps, so skip it entirely on a software renderer
        // (steady cursor) rather than paint a choppy, expensive blink.
        let blinking = !active.low_power
            && active.last_input.elapsed() < Duration::from_secs_f32(CURSOR_BLINK_PERIOD * CURSOR_BLINK_CYCLES);
        if blinking {
            anim = true;
        }
        // Let animation drive a repaint, but throttle it to ~2 fps on a software
        // renderer (llvmpipe — a weak or remote box) so the bling can't peg the
        // CPU; content changes (output/title/bell, above) still repaint promptly.
        // GL path: a throttled full-frame repaint drives egui's inline instrument
        // animation (target "A"). Software GL caps this at ~2fps so the bling can't
        // peg the CPU; on a real GPU it repaints every frame.
        let anim_min = if active.low_power { Duration::from_millis(500) } else { Duration::ZERO };
        if anim && active.backend.is_gl() && now.duration_since(active.last_anim) >= anim_min {
            active.last_anim = now;
            dirty = true;
        }
        // Native (XRender) path: the instrument LAYER redraws on its own fixed
        // cadence (INSTRUMENT_TICK = 6fps), paced independently of `anim_min` — a
        // tick redraws only the ARGB layer (composited server-side), never a
        // content frame, so it stays cheap over ssh -X regardless of the ~2fps
        // software-GL repaint throttle. `anim` gates it so a static (no-flow)
        // patch-bay costs nothing.
        let instruments_animating = anim
            && !active.backend.is_gl()
            && active.settings.inst_remote
            && active.settings.inst_animate;
        if instruments_animating && now.duration_since(active.last_instr_tick) >= INSTRUMENT_TICK {
            active.instr_tick = true;
            active.last_instr_tick = now;
            // Instruments are baked into the content buffer now; a tick moves them,
            // so force a FULL frame to erase the previous positions (a scissored
            // frame would leave trails). Only 6fps, and only while animating.
            active.force_full = true;
            dirty = true;
        }
        // Drag-select past the pane edge: auto-scroll + extend (#3). Keeps the loop
        // awake (active_until) so it keeps scrolling while the pointer is held there.
        if Self::autoscroll_selection(active, now) {
            active.active_until = now + Duration::from_millis(120);
            dirty = true;
        }
        // While the search bar is open, keep its results current as new output
        // streams into the searched pane. Re-run only on a change, query set.
        if dirty && active.search_open && !active.search_query.is_empty() {
            Self::run_search(active, false); // live refresh; keep position + view
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
        } else if broadcasting {
            BCAST_PULSE_TICK // keep the broadcast swatches throbbing
        } else if active.resize_pending {
            // A deferred reflow must not wait on IDLE_POLL: guarantee a wake-up
            // to settle it even if nothing else asks for one.
            RESIZE_SETTLE
        } else if active.prefs_pending.is_some() {
            PREFS_SETTLE // a commit is owed; wake to pay it
        } else if blinking {
            BLINK_POLL
        } else if instruments_animating {
            INSTRUMENT_TICK // wake at ~6fps to redraw the animating instrument layer
        } else {
            IDLE_POLL
        };
        active.poll_ms = interval.as_millis() as u64;
        event_loop.set_control_flow(ControlFlow::WaitUntil(Instant::now() + interval));
    }
}

/// The chrome bands around a pane rect that egui repaints each frame (focus
/// outline + border instruments). Kept thin so forcing them into damage stays
/// cheap. `BORDER_PX` matches the renderer's focus-outline / instrument band.
/// The visual-bell / hollow-cursor outline thickness (`t = cell_h/16`, ~1px,
/// render.rs:585) must stay within `BORDER_PX` so the bell outline is covered
/// on the partial path. `top_h` widens the top band to cover the titlebar strip
/// (focus tint + title text) when the titlebar is shown; 0 leaves it at `BORDER_PX`.
const BORDER_PX: i32 = 6;
fn border_bands(rect: Rect, top_h: i32) -> [crate::damage::PxRect; 4] {
    use crate::damage::PxRect;
    let (x, y, w, h) = (rect.x as i32, rect.y as i32, rect.w as i32, rect.h as i32);
    let top = BORDER_PX.max(top_h); // cover the full titlebar strip when shown
    [
        PxRect { x, y, w, h: top },                          // top
        PxRect { x, y: y + h - BORDER_PX, w, h: BORDER_PX }, // bottom
        PxRect { x, y, w: BORDER_PX, h },                    // left
        PxRect { x: x + w - BORDER_PX, y, w: BORDER_PX, h }, // right
    ]
}

/// The outcome of the per-frame damage decision: repaint everything, or scissor
/// to a bounding box and hint the compositor with per-rect damage.
enum FramePlan {
    Full,
    Partial(crate::damage::PxRect, Vec<crate::damage::PxRect>), // (scissor bbox, compositor hints)
}

/// How many past frames' damage we retain to satisfy an EGL buffer age > 1.
const HISTORY_DEPTH: u32 = 2;

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
                // The keybinding only ever reaches here while the dialog is
                // closed — once open, the input shim below intercepts every
                // key (including this one) before it can be dispatched as an
                // Action. So this is always an OPEN, never a toggle-closed.
                if active.prefs_open {
                    active.prefs_open = false;
                } else {
                    Self::open_prefs(active);
                }
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
                active.force_full = true; // clear the removed wires' ghosts (off the partial path)
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
                Some(SessionEvent::Redraw) => {
                    // A session action that changed what's on screen — a tab
                    // switch, split, zoom, rotate, columns, etc. These change the
                    // whole visible layout, but the newly-shown panes have no
                    // per-cell engine damage (their content didn't change, they
                    // were just hidden), so a partial frame would redraw nothing
                    // and the XRender back buffer would keep the OLD tab's pixels.
                    // Force a full redraw so the new layout actually paints.
                    active.force_full = true;
                    active.window.request_redraw();
                }
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
                    // Bracketed paste (DECSET 2004) is decided PER PANE inside `feed_paste`:
                    // a broadcast paste can reach group members in different bracketed-paste
                    // states, so each is wrapped (or not) by its OWN state — not the focus's
                    // (that bug mangled a pasted key's newlines in some panes but not others).
                    // `feed_paste` also strips embedded end-markers (paste-injection guard)
                    // and respects the broadcast mode.
                    active.session.feed_paste(text.as_bytes());
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
        // Read straight from the pane's grid by ABSOLUTE line, so a selection that
        // spans scrollback the viewport isn't currently showing still copies in
        // full (the visible-only snapshot could not). Handles block mode too.
        let text = active.session.pane(sel.pane)?.selection_text(sel.anchor, sel.head, sel.block);
        Some(text)
    }

    /// Map a physical-pixel point to `(pane, col, row)` in that pane's grid, for
    /// selection. Returns `None` for column-mode panes (their re-tiled layout
    /// makes cell mapping ambiguous — selection there is a follow-up) or points
    /// outside any pane.
    /// The grid (content) rectangle of a specific pane, or `None` if it isn't
    /// currently visible. Like the per-pane branch of `cell_at`, but keyed by id.
    fn pane_content_rect(active: &Active, pane: rt_core::PaneId) -> Option<Rect> {
        let size = active.window.inner_size();
        let bounds = content_bounds(size);
        active.session.visible_rects(bounds)
            .into_iter()
            .find(|(id, _)| *id == pane)
            .map(|(_, rect)| active.session.content_rect(rect))
    }

    /// While a drag-select is active and the pointer is ABOVE or BELOW the pane's
    /// grid, scroll that pane one line (rate-limited) and extend the selection head
    /// to the edge — so you can select more than a screenful by dragging past the
    /// edge (#3). Returns `true` whenever the pointer is in the edge zone (even on
    /// a tick that didn't scroll yet), so the caller keeps the loop awake.
    fn autoscroll_selection(active: &mut Active, now: Instant) -> bool {
        if !active.selecting {
            return false;
        }
        let Some(sel) = active.selection else { return false };
        let Some(content) = Self::pane_content_rect(active, sel.pane) else { return false };
        let my = active.mouse.1;
        // +1 = scroll toward older history (pointer above top); -1 = toward newest.
        let dir: isize = if my < content.y { 1 } else if my >= content.y + content.h { -1 } else { 0 };
        if dir == 0 {
            return false; // pointer is within the grid: normal motion handling covers it
        }
        // One line per ~35ms, independent of frame rate, so the scroll is smooth
        // but not runaway-fast.
        if now.duration_since(active.last_autoscroll) < Duration::from_millis(35) {
            return true; // in the zone; ask the caller to keep waking us
        }
        active.last_autoscroll = now;
        let (cw, _) = active.backend.cell_size();
        let (off, col, edge_row) = {
            let Some(pane) = active.session.pane(sel.pane) else { return false };
            pane.scroll(dir); // move the view one line
            let (offset, _, screen) = pane.scroll_info();
            let col = ((active.mouse.0 - content.x) / cw).max(0.0) as usize;
            // Extend to the top row when scrolling up, the bottom row when down.
            let edge_row = if dir > 0 { 0i32 } else { screen.saturating_sub(1) as i32 };
            (offset as i32, col, edge_row)
        };
        if let Some(s) = active.selection.as_mut() {
            s.head = (col, edge_row - off); // screen edge row → absolute line
        }
        active.force_full = true; // selection + scroll: not engine-tracked damage
        true
    }

    fn cell_at(active: &Active, mx: f32, my: f32) -> Option<(rt_core::PaneId, usize, usize)> {
        let size = active.window.inner_size();
        let bounds = content_bounds(size);
        let (cw, ch) = active.backend.cell_size();
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
            active.force_full = true; // scrollback offset changed
            active.window.request_redraw();
        }
    }

    /// Word boundaries around `(col, row)` for double-click selection: expand
    /// left and right over "word" characters (alphanumerics plus the punctuation
    /// that usually belongs to paths/URLs). Returns the inclusive `(start, end)`
    /// columns, or `None` if the row/column is out of range. A click on a
    /// non-word character selects just that one cell.
    /// The word under `(col, row)` for a double-click, growing ACROSS soft-wrap boundaries so
    /// a path/URL/key that wraps onto the next screen row selects whole (the old version
    /// stopped at the row's end — "selects to the end of the row, not the filename"). Returns
    /// the inclusive start/end cells in SCREEN coordinates `((start_col, start_row),
    /// (end_col, end_row))`, or `None` if the click is past the row's content.
    fn word_at(
        active: &Active,
        pane: rt_core::PaneId,
        col: usize,
        row: usize,
    ) -> Option<((usize, usize), (usize, usize))> {
        let p = active.session.pane(pane)?;
        let snap = p.snapshot(); // owned; safe to hold the pane borrow alongside
        let cols = snap.cols;
        let ch = |r: usize, c: usize| snap.rows.get(r).and_then(|line| line.get(c)).map(|cell| cell.c);
        // A "word" char: alphanumeric plus the symbols common in paths/URLs/keys.
        let is_word = |c: char| c.is_alphanumeric() || "-_./~:@%+#=?&".contains(c);
        if !is_word(ch(row, col)?) {
            return Some(((col, row), (col, row))); // lone symbol/space → single cell
        }
        // Grow the start leftwards, stepping onto the previous row when it soft-wraps into this.
        let (mut sr, mut sc) = (row, col);
        loop {
            let (pr, pc) = if sc > 0 {
                (sr, sc - 1)
            } else if sr > 0 && cols > 0 && p.line_wrapped(sr - 1) {
                (sr - 1, cols - 1)
            } else {
                break;
            };
            match ch(pr, pc) {
                Some(c) if is_word(c) => (sr, sc) = (pr, pc),
                _ => break,
            }
        }
        // Grow the end rightwards, stepping onto the next row when this one soft-wraps.
        let (mut er, mut ec) = (row, col);
        loop {
            let (nr, nc) = if ec + 1 < cols {
                (er, ec + 1)
            } else if p.line_wrapped(er) {
                (er + 1, 0)
            } else {
                break;
            };
            match ch(nr, nc) {
                Some(c) if is_word(c) => (er, ec) = (nr, nc),
                _ => break,
            }
        }
        Some(((sc, sr), (ec, er)))
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

    /// Redact a URL for logging: keep the scheme and host, elide the path/query/fragment
    /// (which may carry tokens, signed-URL credentials, or private paths). [review RT-PRIV-001]
    fn redact_url(url: &str) -> String {
        if let Some(sep) = url.find("://") {
            let host_start = sep + 3;
            let rest = &url[host_start..];
            let host_end = rest.find(['/', '?', '#']).map_or(url.len(), |i| host_start + i);
            let more = host_end < url.len();
            format!("{}{}", &url[..host_end], if more { "/…" } else { "" })
        } else {
            // Schemes without `//` (mailto:, tel:, file:/path): show only the scheme.
            match url.split_once(':') {
                Some((scheme, rest)) if !rest.is_empty() => format!("{scheme}:…"),
                _ => "…".to_string(),
            }
        }
    }

    /// Open a URL with the desktop's default handler via `xdg-open`, detached so
    /// it never blocks rt. Failures are logged, not fatal (rt's no-crash policy).
    fn open_url(url: &str) {
        // Log a REDACTED form — a URL from terminal text can carry password-reset tokens,
        // signed-object credentials, or private paths that must not leak into logs. The full
        // URL is still passed to xdg-open as a single argv (no shell), which is safe.
        // [review RT-PRIV-001]
        let redacted = Self::redact_url(url);
        // Spawn xdg-open without waiting; ignore the handle so it runs detached.
        match std::process::Command::new("xdg-open").arg(url).spawn() {
            Ok(_) => log::info!("opened URL: {redacted}"),
            Err(e) => log::warn!("xdg-open failed for {redacted}: {e}"),
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

    /// Leave anchored-compose mode, discarding the in-progress selection and
    /// touching no clipboard.
    fn compose_cancel(active: &mut Active) {
        active.composing = false;
        active.shift_press = false;
        active.selection = None;
        active.force_full = true;
        active.window.request_redraw();
    }

    /// Finish anchored-compose: copy the selection to CLIPBOARD and PRIMARY, leave
    /// it highlighted (like a completed drag-select), and exit the mode.
    fn compose_commit(active: &mut Active) {
        Self::do_copy(active); // CLIPBOARD + PRIMARY
        active.composing = false;
        active.shift_press = false;
        active.force_full = true;
        active.window.request_redraw();
    }

    /// The head's movement bounds for the pane, from its grid width and buffer
    /// extent. Absolute lines run `-(history) ..= screen-1` (scrollback negative).
    fn compose_bounds(active: &Active, pane: rt_core::PaneId) -> Option<select::Bounds> {
        let content = Self::pane_content_rect(active, pane)?;
        let (cw, _) = active.backend.cell_size();
        let cols = (content.w / cw).max(0.0) as usize;
        let (_, history, screen) = active.session.pane(pane)?.scroll_info();
        Some(select::Bounds {
            cols,
            min_line: -(history as i32),
            max_line: screen as i32 - 1,
            page: screen,
        })
    }

    /// Scroll the pane just enough that absolute line `head_line` is on screen.
    /// Visible absolute lines are `[-offset, screen-1-offset]`; step one line at a
    /// time (bounded) toward the head.
    fn scroll_head_into_view(active: &mut Active, pane: rt_core::PaneId, head_line: i32) {
        for _ in 0..10_000 {
            // safety cap: never spin
            let Some(p) = active.session.pane(pane) else { return };
            let (offset, _, screen) = p.scroll_info();
            let top = -(offset as i32);
            let bottom = screen as i32 - 1 - offset as i32;
            if head_line < top {
                p.scroll(1); // toward older history
            } else if head_line > bottom {
                p.scroll(-1); // toward newest
            } else {
                return; // in view
            }
        }
    }

    /// Apply a head navigation while composing. Arrow moves (accelerate = true)
    /// repeat per the held-arrow acceleration the user configured; jumps apply
    /// once. Then scroll to keep the head visible and repaint.
    fn compose_nav(active: &mut Active, nav: select::Nav, accelerate: bool) {
        let Some(sel) = active.selection else { return };
        let pane = sel.pane;
        let Some(bounds) = Self::compose_bounds(active, pane) else { return };
        // Held-arrow acceleration: reuse arrow_hold/arrow_accel_step. `arrow` is a
        // per-direction tag so a change of direction resets the run.
        let now = Instant::now();
        let steps = if accelerate && active.settings.arrow_accel {
            let tag = nav as u8; // stable per Nav variant
            let repeats = match active.arrow_hold {
                Some((prev, t, n)) if prev == tag && now.duration_since(t) < ARROW_HOLD_GAP => n + 1,
                _ => 0,
            };
            active.arrow_hold = Some((tag, now, repeats));
            arrow_accel_step(repeats, active.settings.arrow_accel_max)
        } else {
            active.arrow_hold = None;
            1
        };
        let mut head = sel.head;
        for _ in 0..steps {
            head = select::move_head(head, nav, bounds);
        }
        if let Some(s) = active.selection.as_mut() {
            s.head = head;
        }
        Self::scroll_head_into_view(active, pane, head.1);
        active.force_full = true;
        active.window.request_redraw();
    }

    /// Reload the renderer's fonts from the current settings (rebuilding the
    /// font chains if the family changed), re-measure the cell, and resize every
    /// pane to the new (cols, rows). Shared by the preferences dialog and the
    /// zoom actions.
    fn refresh_fonts(active: &mut Active, family_changed: bool) {
        active.force_full = true; // cell metrics change: every cell→px mapping is stale
        if family_changed {
            active.font_blobs = font_blobs(&active.font_db, &active.settings.font_family);
        }
        let px = active.settings.font_size;
        match active.backend.reload_fonts(&active.font_blobs, px) {
            Ok(()) => {
                let cell = active.backend.cell_size(); // new cell metrics
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
        // Anchored-compose is modal: keyboard input drives the selection head, not
        // the shell. Arrows accelerate on hold (reusing the arrow-accel prefs);
        // Home/End/Page/Ctrl+Home/End jump; Esc cancels; anything else is swallowed.
        if active.composing {
            use select::Nav;
            let ctrl = active.mods.control_key();
            match &key_event.logical_key {
                Key::Named(NamedKey::Escape) => Self::compose_cancel(active),
                Key::Named(NamedKey::Enter) => Self::compose_commit(active),
                Key::Named(NamedKey::ArrowLeft) => Self::compose_nav(active, Nav::Left, true),
                Key::Named(NamedKey::ArrowRight) => Self::compose_nav(active, Nav::Right, true),
                Key::Named(NamedKey::ArrowUp) => Self::compose_nav(active, Nav::Up, true),
                Key::Named(NamedKey::ArrowDown) => Self::compose_nav(active, Nav::Down, true),
                Key::Named(NamedKey::Home) if ctrl => Self::compose_nav(active, Nav::BufTop, false),
                Key::Named(NamedKey::End) if ctrl => Self::compose_nav(active, Nav::BufBottom, false),
                Key::Named(NamedKey::Home) => Self::compose_nav(active, Nav::LineStart, false),
                Key::Named(NamedKey::End) => Self::compose_nav(active, Nav::LineEnd, false),
                Key::Named(NamedKey::PageUp) => Self::compose_nav(active, Nav::PageUp, false),
                Key::Named(NamedKey::PageDown) => Self::compose_nav(active, Nav::PageDown, false),
                _ => {} // swallow everything else
            }
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
            // Arrow-key acceleration: while an arrow is HELD (a run of auto-repeats), send
            // several cursor moves per repeat so it speeds up the longer you hold. A single
            // tap, a pause, or any other key resets it to exactly one move. Off → always one.
            let arrow = match &key_event.logical_key {
                Key::Named(NamedKey::ArrowLeft) => Some(0u8),
                Key::Named(NamedKey::ArrowRight) => Some(1u8),
                Key::Named(NamedKey::ArrowUp) => Some(2u8),
                Key::Named(NamedKey::ArrowDown) => Some(3u8),
                _ => None,
            };
            let now = Instant::now();
            let step = match (active.settings.arrow_accel, arrow) {
                (true, Some(a)) => {
                    let repeats = match active.arrow_hold {
                        Some((prev, t, n)) if prev == a && now.duration_since(t) < ARROW_HOLD_GAP => n + 1,
                        _ => 0, // fresh press, different arrow, or a gap → start over
                    };
                    active.arrow_hold = Some((a, now, repeats));
                    arrow_accel_step(repeats, active.settings.arrow_accel_max)
                }
                _ => {
                    active.arrow_hold = None; // a non-arrow key (or accel off) breaks the hold
                    1
                }
            };
            if step > 1 {
                let mut payload = Vec::with_capacity(bytes.len() * step);
                for _ in 0..step {
                    payload.extend_from_slice(&bytes);
                }
                active.session.feed_input(&payload); // N moves this repeat
            } else {
                active.session.feed_input(&bytes); // send to the shell(s)
            }
            active.last_input = now; // restart the cursor blink window
            // Typing returns you to the live prompt: if the focused pane was
            // scrolled up in history, snap it back to the bottom.
            if let Some(pane) = active.session.pane(active.session.focus()) {
                if pane.scroll_info().0 > 0 {
                    // Was scrolled up in history; snapping back shifts the whole
                    // viewport, so force a full repaint next frame.
                    active.force_full = true;
                }
                pane.scroll_to_bottom();
            }
        }
    }

    /// Repaint the whole window: fill each pane's background, draw its visible
    /// grid, then outline the focused pane. Finally swap buffers.
    fn redraw(&mut self) {
        let Some(active) = self.active.as_mut() else { return };
        // A window resize is in flight: the backend surface is still the old size
        // and every frame drawn now is discarded by the settle frame. Painting
        // here is what produced the 5-12 visible intermediate steps; skip it and
        // let the settle repaint once, at the resting size (see Resized).
        if active.surface_pending.is_some() {
            return;
        }
        // Terminal colours (a dark theme): near-black bg, light-grey fg.
        // The background carries the user's opacity in its alpha channel, so a
        // value < 1.0 makes empty areas translucent (the window(s) behind show
        // through, compositor permitting). Glyphs and chrome stay fully opaque.
        let cfg_bg = active.settings.background; // configured background RGB
        let bg = Color::rgb(cfg_bg[0], cfg_bg[1], cfg_bg[2]).with_alpha(active.settings.background_opacity);
        let size = active.window.inner_size(); // physical pixels
        let bounds = content_bounds(size);

        // Decide this frame's damage. The partial (scissored) path is taken ONLY
        // on software GL with an EGL surface whose buffer we can trust to be
        // preserved; everything else falls to the full redraw (today's path),
        // which is always correct.
        let overlay_open = active.prefs_open || active.menu.is_some() || active.manual_open || active.search_open;
        let (cell_w, cell_h) = active.backend.cell_size(); // px per cell
        let (cw, ch) = (cell_w as i32, cell_h as i32);

        // Build this frame's damage from the panes. This loop also fetches the
        // snapshots the draw pass reuses, so `render_snapshot()` — which mutates
        // engine damage state — runs exactly once per pane per frame.
        active.damage.begin_frame();
        let mut snapshots: Vec<(rt_core::PaneId, PxRectSnap)> = Vec::new();
        // Native (XRender) chrome: the border instruments + patch-bay are BAKED
        // into the content back buffer each frame, clipped to the frame scissor
        // (see `begin_instrument_layer`) — there is no separate composited layer.
        // So a keystroke or a line of output re-bakes only the instrument geometry
        // that falls in its (small) damage bbox and re-sends only the changed
        // cells; it never re-ships the whole screen. That is the whole point of
        // mechanism C — an earlier per-present composite of a full-window layer
        // saturated the ssh link and pegged Xwayland under load.
        //
        // The animation only ADVANCES on the 6fps `instr_tick` (see `about_to_wait`
        // and `paint_overlays_or_instruments`), decoupled from keystroke/output
        // frames; between ticks the geometry is static, so a scissored frame that
        // re-bakes it lands the SAME pixels — no ghost trail. A tick, or any move
        // of instrument geometry with no cell-damage (new pane, rotate, divider
        // drag, tab switch, wire change), needs a FULL frame so the previous
        // positions are erased — that is exactly what `force_full`/`chrome_moved`
        // already arms below (the tick sets `force_full` in `about_to_wait`). On
        // the GL path the wires are overlay chrome blended every frame across
        // arbitrary inter-pane regions the partial path can't bound, so any wire
        // still forces full there.
        //
        // Keyed on `force_full` (layout/chrome changed), NOT on "damage is full":
        // the engine reports Full damage on any clear/scroll, which under an
        // output flood is most frames, and that would put instrument geometry back
        // on content frames — the exact coupling `instrument_ticks_decoupled_from_output`
        // guards against.
        let chrome_moved = active.force_full || active.session.focus() != active.last_focus;
        if chrome_moved
            || overlay_open
            || !active.bell_flash.is_empty() // bell stripes span the pane top+bottom, not the output's cell-damage → full (and the expired entry, still present here until draw_panes retains it, gives one full frame to clear the stripe)
            || !active.backend.is_software()
            || (active.backend.is_gl() && !active.wires.is_empty())
            || !active.backend.partial_present_available()
        {
            active.damage.mark_full();
        }
        for (id, rect) in active.session.visible_rects(bounds) {
            let content = active.session.content_rect(rect);
            // ONCE per pane. Wrapped in catch_unwind so a panic building ONE pane's snapshot
            // (a render/grid bug) is isolated to that pane — it renders stale this frame
            // instead of aborting the whole window. panic = "unwind" makes this catchable.
            // On the first catch, mark the pane crashed so it is badged [crashed] and
            // skipped thereafter — otherwise a *deterministic* render panic would re-fire
            // every frame (render isn't dirty-gated), flooding stderr and burning CPU.
            let snap = active.session.pane(id).and_then(|p| {
                if p.is_crashed() {
                    return None; // frozen at its last frame; don't re-enter the panicking path
                }
                match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| p.render_snapshot())) {
                    Ok(s) => Some(s),
                    Err(_) => {
                        p.note_render_crash(); // badge + skip on future frames
                        None
                    }
                }
            });
            if let Some(snap) = &snap {
                if active.session.columns_of(id) > 1 {
                    active.damage.mark_full(); // newspaper columns: cell→px mapping ambiguous
                } else {
                    active.damage.add_cells(&snap.damage, content.x as i32, content.y as i32, cw, ch);
                }
            }
            snapshots.push((id, (rect, snap)));
        }
        // Scroll-blit fast path (XRender only): when every changed pane is a clean full-width
        // scroll and nothing else moved, blit each scrolled pane's content up server-side
        // (one CopyArea) and repaint only the exposed rows — ~1 request instead of re-shipping
        // the whole pane over ssh -X. Falls through to the normal damage path otherwise.
        if Self::redraw_scroll_blit(active, bg, bounds, &snapshots, chrome_moved, overlay_open, cw, ch) {
            active.force_full = false; // a clean scroll frame; overlay_open was false to get here
            active.last_focus = active.session.focus();
            return;
        }
        // Chrome egui blends every frame (focus outline, border instruments)
        // lives on the pane borders; force those bands into the damage set so the
        // scissored clear+redraw always precedes egui's blend (no double-blend).
        //
        // NOT on the X11 present path: there the X window preserves the border
        // pixels server-side, so folding the perimeter bands in would coalesce a
        // single-pane frame's damage to the WHOLE window and make every keystroke
        // XPutImage ~2-3MB over ssh (~1s). Chrome persists in the window and is
        // re-sent only when it actually changes (via force_full). This is the
        // whole point of Route 1: send only the changed cells, like Terminator.
        let x11_present_active = active.backend.x11_present_active();
        if !active.damage.is_full() && !x11_present_active {
            for (_id, (rect, _)) in &snapshots {
                for band in border_bands(*rect, active.session.titlebar_h() as i32) {
                    active.damage.add_rect(band);
                }
            }
        }
        // The broadcast swatch pulses (see BCAST_PULSE_TICK), and chrome carries
        // no engine cell-damage — so without this the pulse would either not
        // repaint at all or need mark_full(), i.e. the whole window several times
        // a second. Damage exactly the swatches instead: a handful of ~13x13px
        // rects, so the tick costs a scissored redraw of the swatches alone.
        // `receives_broadcast` is `feed_input`'s own rule, and `swatch_rect` is
        // the same geometry `draw_panes` paints — neither can drift from what
        // actually appears.
        if !active.damage.is_full() && !matches!(active.session.broadcast(), Broadcast::Off) {
            let bar_h = active.session.titlebar_h();
            if bar_h > 0.0 {
                for (id, (rect, _)) in &snapshots {
                    if !active.session.receives_broadcast(*id) {
                        continue; // not armed: its swatch is static
                    }
                    let (sx, sy, s) = swatch_rect(*rect, bar_h, ch as f32);
                    active.damage.add_rect(crate::damage::PxRect {
                        x: sx.floor() as i32,
                        y: sy.floor() as i32,
                        w: s.ceil() as i32 + 1, // round outward: never clip the swatch
                        h: s.ceil() as i32 + 1,
                    });
                }
            }
        }

        let frame_damage = active.damage.finish();
        // Fold in recent frames' damage per the back-buffer age, and decide.
        let plan = Self::plan_frame(active, frame_damage);

        let force_next = match plan {
            FramePlan::Full => {
                self.redraw_full(bg, bounds, snapshots); // today's exact path
                false
            }
            FramePlan::Partial(bbox, hint_rects) => {
                self.redraw_scissored(bg, bounds, snapshots, bbox, &hint_rects)
            }
        };
        // Clear the per-frame force flag. An overlay visible this frame (its
        // pixels must be cleared when it closes) or a failed partial swap arms a
        // full redraw next frame; specific handlers also re-arm it.
        if let Some(active) = self.active.as_mut() {
            active.force_full = overlay_open || force_next;
            active.last_focus = active.session.focus(); // record the focus this frame painted, so the next focus move is detected
        }
    }

    /// Draw every visible pane (grid, cursor, scrollbar, titlebar)
    /// plus the split dividers, tab strips, broadcast indicator and visible-bell
    /// stripes. Consumes the pre-fetched `snapshots` (never re-calls
    /// `render_snapshot()`) so engine damage state advances exactly once per pane
    /// per frame. The caller brackets this with `begin_frame`/`end_frame` and
    /// decides full vs scissored.
    fn draw_panes(active: &mut Active, bounds: Rect, snapshots: &[(rt_core::PaneId, PxRectSnap)]) {
        let cfg_bg = active.settings.background; // configured background RGB (for the non-default cell-bg test)

        let focus = active.session.focus(); // which pane is focused
        let (cell_w, cell_h) = active.backend.cell_size(); // px per cell
        let sep = column_separator(active.settings.foreground, cfg_bg); // fg/bg midpoint: visible but not text-weight
        // Draw every visible pane. (No per-pane background fill: the translucent
        // clear above already is the background.) Iterates the pre-fetched
        // snapshots so the engine's damage state is not advanced again here.
        for (id, snap_rect) in snapshots {
            let id = *id; // PaneId is Copy
            let (rect, snap_pre) = snap_rect; // &Rect + &Option<Snapshot> fetched in the planning loop
            let rect = *rect;
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
                // Pre-fetched once/frame in the planning loop (Some here because
                // pane(id) was Some there too). In column mode this is a
                // count*rows-tall screen.
                let snap = snap_pre.as_ref().expect("pane present ⇒ snapshot fetched in planning loop");
                let geom = active.session.column_layout(id, rect); // count/col_cells/rows/gap
                // The selection, if it belongs to this (single-column) pane.
                let pane_sel: Option<Selection> = active.selection.filter(|s| n <= 1 && s.pane == id);
                // Selection is stored in absolute buffer lines; a snapshot row `sub`
                // shows absolute line `sub - sel_offset`, so it follows the scroll.
                let sel_offset = pane.scroll_info().0 as i32;
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

                // While a resize/divider-drag is settling, the reflow is deferred (~120ms, it
                // is expensive with scrollback), so this snapshot is briefly the OLD, larger
                // grid. Clamp the draw to the pane's CURRENT content rect — minus the
                // scrollbar strip — so the stale extra columns/rows can't spill over the
                // scrollbar or into the neighbouring pane until the reflow catches up (they'd
                // "jump" into place, which reads as a glitch). No clamp once settled, so the
                // normal appearance is unchanged. Single-column panes (the common case) only.
                let (clamp_cols, clamp_rows) = if active.resize_pending && n <= 1 {
                    // The scrollbar now lives in the right padding gutter, so clamp to the full
                    // content width — just enough to keep stale columns/rows out of the
                    // neighbour pane and the gutter until the reflow catches up.
                    (
                        (rect.w / cell_w).floor().max(0.0) as usize,
                        (rect.h / cell_h).floor().max(0.0) as usize,
                    )
                } else {
                    (usize::MAX, usize::MAX)
                };

                // Draw each cell: an opaque background quad only when the cell's
                // background differs from the default (so ordinary text keeps the
                // translucent window background), then the glyph in its colour.
                for (r, row) in snap.rows.iter().enumerate() {
                    if n > 1 && r / per_col >= geom.count as usize {
                        break; // guard against a transient over-tall snapshot mid-resize
                    }
                    let (ox, sub) = place(r); // where this line draws
                    if sub >= clamp_rows {
                        continue; // stale row past the current (smaller) height
                    }
                    for (col_idx, cell) in row.iter().enumerate() {
                        if col_idx >= clamp_cols {
                            break; // stale col past the current width (kept clear of the scrollbar)
                        }
                        // Selection highlight wins over the cell's own background;
                        // otherwise draw an explicit (non-default) background.
                        if hl_cur.contains(&(r, col_idx)) {
                            active.backend.fill_cell(ox, rect.y, col_idx, sub, cur_hl);
                        } else if hl_other.contains(&(r, col_idx)) {
                            active.backend.fill_cell(ox, rect.y, col_idx, sub, other_hl);
                        } else if pane_sel.map_or(false, |s| s.contains(col_idx, sub as i32 - sel_offset)) {
                            active.backend.fill_cell(ox, rect.y, col_idx, sub, sel_bg);
                        } else if cell.bg != cfg_bg {
                            // A non-default background: draw it opaque (default-bg
                            // cells stay translucent via the window clear).
                            let c = cell.bg;
                            active.backend.fill_cell(ox, rect.y, col_idx, sub, Color::rgb(c[0], c[1], c[2]));
                        }
                        let fg = Color::rgb(cell.fg[0], cell.fg[1], cell.fg[2]); // per-cell foreground
                        if cell.c != ' ' {
                            // Glyph, in the bold/oblique face per the cell's attributes.
                            active.backend.draw_char(ox, rect.y, col_idx, sub, cell.c, fg, cell.attrs.bold, cell.attrs.italic);
                        }
                        // Text-attribute lines (drawn even on blank cells so an
                        // underlined space still shows a rule).
                        if cell.attrs.underline {
                            active.backend.draw_underline(ox, rect.y, col_idx, sub, fg);
                        }
                        if cell.attrs.strikeout {
                            active.backend.draw_strikeout(ox, rect.y, col_idx, sub, fg);
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
                    let (_, cur_sub) = place(cur.line);
                    // Also honour the mid-resize clamp so a stale cursor can't sit past the
                    // new right/bottom edge (over the scrollbar or the neighbour).
                    if in_range && cur.col < clamp_cols && cur_sub < clamp_rows {
                        use rt_engine::CursorShape;
                        let (ox, sub) = place(cur.line); // cursor's on-screen slot
                        let cc = active.settings.foreground; // cursor colour = configured foreground
                        let ccol = Color::rgb(cc[0], cc[1], cc[2]);
                        let focused = id == focus; // is this the focused pane?
                        if !focused {
                            // Unfocused: hollow outline regardless of shape, steady (no blink).
                            active.backend.cursor_hollow(ox, rect.y, cur.col, sub, ccol);
                        } else {
                            // Soft-blink: fade the focused cursor's alpha so the glyph
                            // beneath shows through, pulsing for a bounded window after
                            // the last keystroke, then holding steady.
                            // Software renderer: steady cursor (blink is disabled to save repaints).
                            let blink = if active.low_power {
                                1.0
                            } else {
                                cursor_blink_alpha(active.last_input.elapsed().as_secs_f32())
                            };
                            let cur_col = ccol.with_alpha(blink);
                            match cur.shape {
                                CursorShape::Block => {
                                    // Solid block + inverse glyph for contrast; both fade
                                    // together so the underlying glyph re-emerges on the dip.
                                    active.backend.fill_cell(ox, rect.y, cur.col, sub, cur_col);
                                    if let Some(u) = snap.rows.get(cur.line).and_then(|rw| rw.get(cur.col)) {
                                        if u.c != ' ' {
                                            let ub = u.bg;
                                            active.backend.draw_char(ox, rect.y, cur.col, sub, u.c, Color::rgb(ub[0], ub[1], ub[2]).with_alpha(blink), u.attrs.bold, u.attrs.italic);
                                        }
                                    }
                                }
                                CursorShape::HollowBlock => active.backend.cursor_hollow(ox, rect.y, cur.col, sub, cur_col),
                                CursorShape::Underline => active.backend.cursor_underline(ox, rect.y, cur.col, sub, cur_col),
                                CursorShape::Beam => active.backend.cursor_beam(ox, rect.y, cur.col, sub, cur_col),
                                CursorShape::Hidden => {} // nothing (snapshot already filters, but be safe)
                            }
                        }
                    }
                }

                // Thin separators between newspaper columns, drawn in each gap.
                if n > 1 {
                    for nc in 1..geom.count as usize {
                        let x = rect.x + nc as f32 * step - (geom.gap as f32 * 0.5) * cell_w; // gap centre
                        active.backend.fill_rect(x, rect.y, 1.0, rect.h, sep); // 1px vertical rule
                    }
                }

                // Scrollbar on the right edge, shown when the pane has scrollback.
                let (offset, history, screen) = pane.scroll_info();
                if history > 0 {
                    // Track + thumb; geometry shared with the drag hit-test.
                    let (bx, bw, thumb_y, thumb_h) = scrollbar_metrics(rect, offset, history, screen);
                    active.backend.fill_rect(bx, rect.y, bw, rect.h, Color::rgb(0x22, 0x22, 0x2c));
                    let thumb_col = if offset > 0 {
                        Color::rgb(0x88, 0x88, 0x9a) // scrolled up: highlight
                    } else {
                        Color::rgb(0x55, 0x55, 0x66) // at the bottom: dimmer
                    };
                    active.backend.fill_rect(bx, thumb_y, bw, thumb_h, thumb_col);
                    // Scrollback-search hit markers: amber ticks on the track
                    // where matches lie, like a browser's find bar. Drawn over the
                    // thumb so every hit stays visible; the current hit gets a
                    // brighter, taller tick. Only for the pane these matches belong
                    // to, while the search bar is open.
                    if active.search_open
                        && active.search_pane == Some(id)
                        && !active.search_matches.is_empty()
                    {
                        let mx = bx - 2.0; // overhang the track a touch so ticks read as markers
                        let mw = bw + 4.0;
                        let others =
                            hit_marker_ys(active.search_matches.iter().map(|m| m.line), history, screen, rect);
                        for y in others {
                            active.backend.fill_rect(mx, y, mw, 2.0, Color::rgb(0xb0, 0x90, 0x30));
                        }
                        if let Some(m) = active.search_matches.get(active.search_index) {
                            if let Some(&y) =
                                hit_marker_ys(std::iter::once(m.line), history, screen, rect).first()
                            {
                                active.backend.fill_rect(mx, y - 1.0, mw, 3.0, Color::rgb(0xff, 0xd8, 0x55));
                            }
                        }
                    }
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
                active.backend.fill_rect(full.x, full.y, full.w, bar_h, bar_bg);
                active.backend.fill_rect(full.x, full.y + bar_h - 1.0, full.w, 1.0, sep);
                let text_col = if focused { Color::rgb(fg[0], fg[1], fg[2]) } else { mix(fg, bg, 0.40) };
                let pad = TITLEBAR_PAD; // horizontal inset inside the strip
                let text_top = full.y + (bar_h - cell_h) * 0.5; // vertically centre the glyph line
                let mut left_x = full.x + pad; // running left cursor (px)
                // Group swatch, if this pane is in an input group.
                // Group swatch — and, while broadcasting, the warning that THIS
                // pane is one of the shells your keystrokes will hit. Broadcast is
                // a mode where one keypress lands in several shells at once, so
                // the panes that receive it say so themselves; that is more use
                // than a border round the window, which said only "some kind of
                // broadcast is on" and (per the note above) said it in the wrong
                // place. Ungrouped panes get a swatch too under Broadcast::All —
                // they receive, so they must show it. `receives_broadcast` is the
                // same rule `feed_input` fans out on, so this cannot drift from
                // where the keystrokes really go.
                let bcast = active.session.broadcast();
                let receiving = !matches!(bcast, Broadcast::Off) && active.session.receives_broadcast(id);
                let swatch = match (active.session.group_of(id), receiving) {
                    (_, true) if matches!(bcast, Broadcast::All) => Some(Color::rgb(0xd9, 0x4a, 0x4a)), // red: ALL panes
                    (_, true) => Some(Color::rgb(0xd9, 0x90, 0x4a)), // orange: this group
                    (Some(g), false) => Some(group_hue(g)),          // not receiving: plain group hue
                    (None, false) => None,
                };
                if let Some(col) = swatch {
                    let (sx, sy, s) = swatch_rect(full, bar_h, cell_h);
                    // Pulse only while this pane is actually armed to receive: a
                    // static group swatch must stay still, or every grouped pane
                    // would throb whether broadcasting or not.
                    let col = if receiving {
                        let t = (active.bcast_phase * std::f32::consts::TAU).sin() * 0.5 + 0.5; // 0..1
                        dim(col, 0.45 + 0.55 * t)
                    } else {
                        col
                    };
                    active.backend.fill_rect(sx, sy, s, s, col);
                    left_x = sx + s + 5.0; // leave a gap before the title
                }
                // Size text ("COLSxROWS") pinned to the right edge.
                let cols = (rect.w / cell_w).max(0.0) as usize; // content columns
                let rows = (rect.h / cell_h).max(0.0) as usize; // content rows
                let size_str = format!("{cols}x{rows}");
                let size_w = size_str.chars().count() as f32 * cell_w;
                let size_x = (full.right() - pad - size_w).max(left_x);
                for (i, ch) in size_str.chars().enumerate() {
                    active.backend.draw_char(size_x, text_top, i, 0, ch, text_col, false, false);
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
                            active.backend.draw_char(mx, text_top, i, 0, ch, mcol, false, false);
                        }
                        left_of = mx; // title truncates before the meter
                    }
                }
                // Title on the left, truncated so it never runs into the meter/size.
                // While composing an anchored selection in this pane, replace the
                // title with a live status ("◉ selecting · N lines") in the
                // focus-accent blue, so the mode is visible right where the title
                // normally sits — composing implies this pane is focused, so this
                // never fights with the unfocused/dimmed title colour.
                let composing_here =
                    active.composing && active.selection.is_some_and(|sel| sel.pane == id);
                let avail = ((left_of - 8.0 - left_x) / cell_w).max(0.0) as usize; // room in cells
                if composing_here {
                    let sel = active.selection.unwrap();
                    let status = select::status_text(sel.anchor, sel.head, sel.block);
                    let scol = Color::rgb(0x6a, 0xa9, 0xff); // the focus-accent blue
                    // Cap to the same room the title gets, so a long status ("◉
                    // selecting · 12345 lines") never runs into the meter/size.
                    for (i, ch) in status.chars().take(avail).enumerate() {
                        active.backend.draw_char(left_x, text_top, i, 0, ch, scol, false, false);
                    }
                } else {
                    let title = active.session.title_of(id).filter(|t| !t.is_empty()).unwrap_or("Terminal");
                    for (i, ch) in title.chars().take(avail).enumerate() {
                        active.backend.draw_char(left_x, text_top, i, 0, ch, text_col, false, false);
                    }
                }
            } else if let Some(g) = active.session.group_of(id) {
                // No titlebar: fall back to a small colour-coded corner square so
                // group membership is still visible.
                let m = 10.0; // marker size in pixels
                let p = 4.0; // inset from the corner
                active.backend.fill_rect(full.right() - m - p, full.y + p, m, m, group_hue(g));
            }
            // (The focused pane used to get a thin blue outline around `full`
            // here. Removed: the titlebar already carries focus — focused panes
            // take more tint and full-strength text — so the outline was a second
            // answer to a question already answered, and it was a standing bug.
            //
            // It was drawn only on FULL frames, but a scissored frame CLEARS its
            // bbox, so any partial frame crossing the outline erased the segment
            // it touched and nothing redrew it: the outline eroded into fragments.
            // It looked fine for a long time only by accident — `fill` used to
            // paint a rect at full length whenever it so much as touched the clip,
            // so unrelated frames kept repainting the whole outline. Fixing `fill`
            // to trim correctly took that accident away and exposed the fragments.
            //
            // Drawing it properly means damaging the four bands whenever anything
            // crosses them — real cost on every frame, for chrome the titlebar
            // already conveys. `last_focus` still forces a full frame on focus
            // change: the titlebar TINT is chrome with no cell-damage too.)
        }

        // Pane dividers: a thin line centred in each split gutter so the
        // boundary between panes is visible (most of the gutter stays the
        // translucent background).
        let divider_col = Color::rgb(0x3a, 0x3a, 0x46);
        for d in active.session.dividers(bounds) {
            if d.w < d.h {
                // Vertical gutter (left/right split): a vertical line.
                active.backend.fill_rect(d.x + d.w * 0.5 - 0.5, d.y, 1.0, d.h, divider_col);
            } else {
                // Horizontal gutter (top/bottom split): a horizontal line.
                active.backend.fill_rect(d.x, d.y + d.h * 0.5 - 0.5, d.w, 1.0, divider_col);
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
                active.backend.fill_rect(r.x, r.y, r.w, r.h, if tab.active { tab_active } else { tab_bg });
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
                    active.backend.draw_char(r.x + 8.0, text_top, i, 0, ch, col, tab.active, false);
                }
                // Right separator between tabs.
                active.backend.fill_rect(r.right() - 1.0, r.y, 1.0, r.h, tab_line);
            }
        }

        // (The broadcast indicator used to be a bold coloured border around the
        // whole window here. It was drawn at (0, 0, bounds.w, bounds.h) — the
        // content SIZE at the WINDOW origin, ignoring bounds.x/y — so it sat one
        // WINDOW_MARGIN up-and-left of where it belonged, hugging the top/left
        // edges while inset at the right/bottom. It is now a per-pane titlebar
        // swatch: see `draw_panes`, which marks the panes that actually receive
        // the input rather than ringing the window.)

        // Visible bell: a brief yellow/black hazard stripe on the border of just
        // the pane that rang — never a whole-window flash.
        if !active.bell_flash.is_empty() {
            let now = Instant::now();
            active.bell_flash.retain(|_, exp| now < *exp); // drop elapsed flashes
            let rects: Vec<_> = active.session.visible_rects(bounds);
            for (id, r) in rects {
                if active.bell_flash.contains_key(&id) {
                    active.backend.bell_stripe(r.x, r.y, r.w, r.h);
                }
            }
        }

    }

    /// Draw whichever overlay is up (preferences + colour picker, context menu,
    /// manual, or search bar) or — when none is — the border instruments. All
    /// native `Backend` primitives on both backends; no egui. Runs after the pane
    /// draw + `end_frame`, over the current framebuffer (inside the cleared
    /// scissor bbox on the partial path).
    fn paint_overlays_or_instruments(active: &mut Active) {
        // Preferences (and the colour picker over it): a native dialog on BOTH
        // backends. Checked before the per-backend split below, which only governs
        // how the instruments are drawn (inline on GL vs a persistent layer on
        // XRender) — the menu/manual/search overlays are native on both.
        if active.prefs_open {
            Self::paint_prefs(active);
            if active.picker.is_some() {
                Self::paint_picker(active); // modal, on top of the dialog
            }
            return;
        }
        let size = active.window.inner_size();
        let (cw, ch) = active.backend.cell_size();
        // Whether a menu/manual/search overlay is up; instruments hide beneath it.
        // (Preferences returned above.)
        let overlay_up = active.menu.is_some() || active.manual_open || active.search_open;

        // Instruments differ by backend. GL: an egui background pass each frame,
        // only when nothing covers them. XRender: a persistent, separate ARGB
        // picture composited over content by `present()` (Task 3/4), redrawn only
        // on a 6fps `instr_tick` (or the first show) so a keystroke/output burst on
        // the scissored path never re-ships instrument geometry — and hidden while
        // an overlay is up, or while dragging the scrollbar (it repaints on
        // drag-release via force_full).
        if active.backend.is_gl() {
            // GL: draw the instruments inline each frame (repaints are cheap),
            // advancing the animation every frame, only when nothing covers them.
            if !overlay_up {
                Self::draw_instrument_geometry(active, size, true);
            }
        } else {
            // XRender: instruments are OFF by default (`inst_remote`; the user opts
            // in). They are BAKED into the content buffer every frame (clipped to
            // the frame scissor) — no separate layer, no per-present composite (that
            // pegged Xwayland over ssh -X). The animation only ADVANCES on the 6fps
            // tick, and that tick forces a full frame (see about_to_wait) so moved
            // instruments leave no trail; between ticks the positions are static, so
            // a scissored frame just re-bakes whatever instrument falls in its bbox.
            let show = active.settings.inst_remote && !overlay_up && active.scroll_drag.is_none();
            if show {
                let advance = active.instr_tick;
                active.backend.begin_instrument_layer();
                Self::draw_instrument_geometry(active, size, advance);
                active.backend.end_instrument_layer();
            }
            active.instr_tick = false; // consume the tick
        }
        // Overlay draws — one native path on BOTH backends, on top of content and
        // instruments. Each reads a few `active.*` fields to build its inputs as
        // locals FIRST, then takes `&mut *active.backend` (disjoint field borrows).
        if let Some(pos) = active.menu {
            let url = Self::cell_at(active, pos.0, pos.1)
                .and_then(|(pane, col, row)| Self::url_at(active, pane, col, row));
            let has_sel = Self::selected_text(active).is_some();
            let rows = menu::rows(&active.keymap, has_sel, url.as_deref());
            let g = chrome::menu::layout(&rows, pos, cw, ch, size.width as f32, size.height as f32);
            let hover = active.menu_hover;
            chrome::menu::draw(&mut *active.backend, &g, &rows, hover, cw, ch);
        } else if active.manual_open {
            let g = chrome::manual::layout(size.width as f32, size.height as f32, cw, ch);
            let scroll = active.manual_scroll;
            chrome::manual::draw(&mut *active.backend, &g, scroll, cw, ch);
        } else if active.search_open {
            let bar = chrome::search::layout(size.width as f32, cw, ch);
            let count = active.search_matches.len();
            let pos = if count == 0 { 0 } else { active.search_index + 1 };
            chrome::search::draw(&mut *active.backend, bar, &active.search_query, pos, count, cw, ch);
        }
    }

    /// Combine this frame's damage with recent frames' damage per the EGL buffer
    /// age and decide Full vs Partial. Records this frame into the history ring
    /// (newest front, capped at `HISTORY_DEPTH`) for future frames.
    fn plan_frame(active: &mut Active, this: crate::damage::FrameDamage) -> FramePlan {
        use crate::damage::{DamageAccumulator, FrameDamage, PxRect};
        // Route 1 (X11 present) preserves the window server-side, so only this
        // frame's damage must be redrawn — treat it as age 1 regardless of the
        // GLX buffer_age (which is unusable on softpipe). EGL keeps real age.
        let age = {
            #[cfg(feature = "x11")]
            {
                if active.backend.x11_present_active() { 1 } else { active.backend.buffer_age() }
            }
            #[cfg(not(feature = "x11"))]
            {
                active.backend.buffer_age()
            }
        };
        let full_now = matches!(this, FrameDamage::Full);
        // Record this frame's damage for future frames (the back buffer we draw
        // into next may be several swaps old).
        let recorded = match &this {
            FrameDamage::Full => FrameDamage::Full,
            FrameDamage::Rects(rs) => FrameDamage::Rects(rs.clone()),
        };
        active.damage_history.push_front(recorded);
        while active.damage_history.len() > HISTORY_DEPTH as usize {
            active.damage_history.pop_back();
        }

        if full_now || age == 0 || age > HISTORY_DEPTH {
            return FramePlan::Full; // unknown/older-than-history buffer → repaint all
        }
        // Union this frame + the previous (age-1) frames' damage: the back buffer
        // still holds content from `age` swaps ago, so anything damaged since must
        // be redrawn.
        let mut acc = DamageAccumulator::new();
        acc.begin_frame();
        for fd in active.damage_history.iter().take(age as usize) {
            match fd {
                FrameDamage::Full => return FramePlan::Full, // any full in the window → full
                FrameDamage::Rects(rs) => {
                    for r in rs {
                        acc.add_rect(*r);
                    }
                }
            }
        }
        match acc.finish() {
            FrameDamage::Full => FramePlan::Full,
            FrameDamage::Rects(rs) => {
                if rs.is_empty() {
                    // Nothing changed and the buffer is fresh enough: a zero-rect
                    // partial swap is a safe no-op hint.
                    FramePlan::Partial(PxRect { x: 0, y: 0, w: 0, h: 0 }, Vec::new())
                } else {
                    let bbox = FrameDamage::Rects(rs.clone()).bbox().unwrap();
                    FramePlan::Partial(bbox, rs)
                }
            }
        }
    }

    /// Today's exact full-window path: clear everything, draw all panes + chrome,
    /// egui, full swap. Byte-for-byte the pre-damage behaviour.
    fn redraw_full(&mut self, bg: Color, bounds: Rect, snapshots: Vec<(rt_core::PaneId, PxRectSnap)>) {
        let Some(active) = self.active.as_mut() else { return };
        active.backend.begin_frame(bg); // translucent clear
        Self::draw_panes(active, bounds, &snapshots);
        active.backend.end_frame(); // upload + draw call
        Self::paint_overlays_or_instruments(active);
        // Flush any native-chrome geometry (the preferences dialog) that the
        // overlay pass batched after the content end_frame above. On XRender
        // this is a no-op (empty end_frame; chrome already drew immediately).
        // On GL it is the ONLY flush for that geometry, and a no-op whenever
        // the overlay pass batched nothing (egui self-flushes, so verts is
        // empty and end_frame early-returns).
        active.backend.end_frame();
        // Present the full window (X11 Route-1 full present, else swap_buffers).
        active.backend.present(&active.window, None);
    }

    /// Partial path (software GL, EGL surface only): preserve the buffer,
    /// clear + redraw only `bbox`, hint the compositor with `hint_rects`. Returns
    /// `true` if a full redraw must be forced next frame (the EGL partial swap was
    /// unavailable this frame, so we fell back to a full redraw + full swap here).
    fn redraw_scissored(
        &mut self,
        bg: Color,
        bounds: Rect,
        snapshots: Vec<(rt_core::PaneId, PxRectSnap)>,
        bbox: crate::damage::PxRect,
        hint_rects: &[crate::damage::PxRect],
    ) -> bool {
        let Some(active) = self.active.as_mut() else { return false };
        active.backend.begin_frame_scissored(bg, bbox); // scissor clips clear + draws to bbox
        Self::draw_panes(active, bounds, &snapshots);
        active.backend.end_frame();
        // GL blends its egui instruments into the scissored region every frame.
        // The native (XRender) path draws its instrument layer separately from
        // the content back buffer (a persistent ARGB picture composited by
        // `present()`), so a scissored content frame stays minimal — changed
        // cells only. But a native instrument tick must still run here (not just
        // on full frames): otherwise a tick that lands on a frame with no content
        // damage (the common case — ticks are decoupled from typing) would never
        // get its `paint_overlays_or_instruments` call and the layer would go
        // stale. `instr_tick` gates that: set only at 6fps in `about_to_wait`, so
        // this doesn't reintroduce coupling to keystroke/output frames.
        if active.backend.is_gl() || active.instr_tick {
            Self::paint_overlays_or_instruments(active);
        }
        active.backend.clear_scissor(); // next frame starts with a clean scissor
        // Present just the damage (X11 Route-1 bbox present, else EGL partial swap).
        if active.backend.present(&active.window, Some((bbox, hint_rects))) {
            // EGL partial swap unavailable/failed → guarantee correctness with a
            // full redraw + full swap this frame, and force a full frame next time.
            active.backend.begin_frame(bg);
            Self::draw_panes(active, bounds, &snapshots);
            active.backend.end_frame();
            Self::paint_overlays_or_instruments(active);
            active.backend.full_swap();
            return true;
        }
        false
    }

    /// The scroll-blit fast path. Returns `true` iff it handled (drew + presented) this frame.
    ///
    /// Gated hard: only the XRender backend (server-side `CopyArea`), and only a frame where
    /// nothing but pane *content* moved — no chrome/overlay/wire/bell/broadcast, and every
    /// changed pane is a clean full-width [`Damage::Scroll`](rt_engine::Damage::Scroll). For
    /// each scrolled pane it blits the content up in the back buffer (one request) and repaints
    /// only the dirty spans (the exposed rows plus the cursor's old cell, which the blit
    /// dragged up), then presents the whole content rect. Any wire could arc over a pane and be
    /// smeared by the blit, so — like the GL path — the presence of ANY wire disables it.
    fn redraw_scroll_blit(
        active: &mut Active,
        bg: Color,
        bounds: Rect,
        snapshots: &[(rt_core::PaneId, PxRectSnap)],
        chrome_moved: bool,
        overlay_open: bool,
        cw: i32,
        ch: i32,
    ) -> bool {
        if !active.backend.supports_scroll_blit()
            || chrome_moved
            || overlay_open
            || !active.bell_flash.is_empty()
            || !active.wires.is_empty()
            || !matches!(active.session.broadcast(), Broadcast::Off)
        {
            return false;
        }
        let union = |acc: &mut Option<crate::damage::PxRect>, r: crate::damage::PxRect| {
            *acc = Some(match *acc {
                None => r,
                Some(a) => {
                    let (x0, y0) = (a.x.min(r.x), a.y.min(r.y));
                    let (x1, y1) = ((a.x + a.w).max(r.x + r.w), (a.y + a.h).max(r.y + r.h));
                    crate::damage::PxRect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 }
                }
            });
        };
        let mut present_bbox: Option<crate::damage::PxRect> = None; // whole content of every scrolled pane
        let mut draw_bbox: Option<crate::damage::PxRect> = None; // just the dirty spans to repaint
        let mut any_scroll = false;
        for (id, (rect, snap)) in snapshots {
            let Some(snap) = snap else { continue };
            let idle = matches!(&snap.damage, rt_engine::Damage::Lines(l) if l.is_empty());
            if active.session.columns_of(*id) > 1 {
                if idle {
                    continue;
                }
                return false; // a multi-column pane changed: cell↔px mapping is ambiguous
            }
            match &snap.damage {
                _ if idle => {} // unchanged pane: leave its pixels alone
                rt_engine::Damage::Scroll { lines, spans } => {
                    any_scroll = true;
                    let content = active.session.content_rect(*rect);
                    let cr = crate::damage::PxRect { x: content.x as i32, y: content.y as i32, w: content.w as i32, h: content.h as i32 };
                    active.backend.scroll_blit(cr, *lines as i32 * ch);
                    union(&mut present_bbox, cr);
                    for s in spans {
                        union(&mut draw_bbox, crate::damage::PxRect {
                            x: cr.x + s.left as i32 * cw,
                            y: cr.y + s.line as i32 * ch,
                            w: (s.right - s.left + 1) as i32 * cw,
                            h: ch,
                        });
                    }
                }
                _ => return false, // Full, or a real Lines change: not a pure-scroll frame
            }
        }
        let (Some(draw_bbox), Some(present_bbox)) = (draw_bbox, present_bbox) else { return false };
        if !any_scroll {
            return false;
        }
        // Repaint only the exposed/dirty rows over the (already blitted) content — the XRender
        // primitives self-trim to the scissor, so cells outside it issue no requests — then
        // present the whole scrolled content region.
        active.backend.begin_frame_scissored(bg, draw_bbox);
        Self::draw_panes(active, bounds, snapshots);
        active.backend.end_frame();
        active.backend.clear_scissor();
        active.backend.present(&active.window, Some((present_bbox, &[])));
        true
    }

    /// Point the cursor at whatever is under it: Grab over a jack, resize arrows
    /// over a bare divider, default elsewhere.
    ///
    /// A no-op while something else owns the pointer — a drag in progress keeps
    /// its own shape. Only re-issued when the shape actually changes: set_cursor
    /// is an X round trip and motion arrives at pointer-sample rate.
    fn update_cursor(active: &mut Active) {
        if active.dragging_divider.is_some()
            || active.scroll_drag.is_some()
            || active.mouse_report.is_some()
            || active.wiring_from.is_some()
            || active.selecting
        {
            return;
        }
        let bounds = content_bounds(active.window.inner_size());
        // Jacks sit ON the divider, and a press checks `jack_at` FIRST (a jack
        // wins there). The cursor must agree: "resize" over a jack advertises the
        // wrong action on a small target, which makes the jack hard to trust even
        // though the click works. Grab = "you can pull a wire out of this".
        let want = if Self::jack_at(active, active.mouse.0, active.mouse.1).is_some() {
            Some(CursorIcon::Grab)
        } else {
            active
                .session
                .divider_at(active.mouse.0, active.mouse.1, bounds)
                // `horizontal` = the split's axis runs left/right, so the divider
                // is vertical and drags along x.
                .map(|h| if h.horizontal { CursorIcon::ColResize } else { CursorIcon::RowResize })
        };
        if want != active.cursor_icon {
            active.window.set_cursor(want.unwrap_or(CursorIcon::Default));
            active.cursor_icon = want;
        }
    }

    /// Open the preferences dialog, selecting the first REAL row.
    ///
    /// Row 0 of `chrome::prefs::rows()` is `sec("Font")` — a `Section` header,
    /// not selectable. Opening on index 0 would make the first Down call
    /// `next_sel(rows, 0, 1)`, whose `.unwrap_or(0)` fallback (0 isn't among the
    /// selectable indices) lands on the SECOND selectable row, silently skipping
    /// "Size (px)". Selecting the first real row up front avoids that entirely.
    fn open_prefs(active: &mut Active) {
        active.prefs_open = true;
        let size = active.window.inner_size();
        let (cw, _ch) = active.backend.cell_size();
        let cols = (content_bounds(size).w / cw).max(1.0) as usize;
        let rows = chrome::prefs::rows(&active.settings, total_ram_bytes(), cols);
        active.prefs_sel = chrome::prefs::selectable(&rows).first().copied().unwrap_or(0);
        active.prefs_scroll = 0;
    }

    /// Apply and persist `new`, doing only the work each change actually needs.
    ///
    /// Lifted verbatim out of the old egui `paint_egui`: it was always
    /// backend-agnostic, and deleting the egui dialog leaves it one caller.
    /// Called ONCE per settle (see PREFS_SETTLE), never per keystroke.
    fn commit_settings(active: &mut Active, new: rt_config::Settings) {
        if new == active.settings {
            return; // nothing to do
        }
        // What changed drives which subsystem we refresh.
        let colours_changed = new.foreground != active.settings.foreground
            || new.background != active.settings.background
            || new.palette != active.settings.palette;
        let family_changed = new.font_family != active.settings.font_family;
        let fonts_changed = family_changed || new.font_size != active.settings.font_size;
        let titlebar_changed = new.show_titlebar != active.settings.show_titlebar;
        // The blur decision depends on both the toggle and the opacity slider.
        let blur_changed = want_blur(&new) != want_blur(&active.settings);
        active.settings = new; // commit
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
        active.force_full = true; // chrome + metrics changed: repaint the lot
    }

    /// Draw the preferences dialog from the PENDING settings, so the value you
    /// just stepped is on screen immediately — the terminal behind it changes
    /// once, on settle.
    fn paint_prefs(active: &mut Active) {
        let size = active.window.inner_size();
        let (cw, ch) = active.backend.cell_size();
        let s = active.prefs_pending.clone().unwrap_or_else(|| active.settings.clone());
        // Exactly how the old egui dialog derived it: a full-width pane at the
        // current font size. There is no `pane.cols()`.
        let cols = (content_bounds(size).w / cw).max(1.0) as usize;
        let rows = chrome::prefs::rows(&s, total_ram_bytes(), cols);
        let g = chrome::prefs::layout(&rows, active.prefs_scroll, cw, ch, size.width as f32, size.height as f32);
        let mut sw = vec![Color::rgb(s.foreground[0], s.foreground[1], s.foreground[2])];
        sw.push(Color::rgb(s.background[0], s.background[1], s.background[2]));
        sw.extend(s.palette.iter().map(|c| Color::rgb(c[0], c[1], c[2])));
        chrome::prefs::draw(&mut *active.backend, &g, &rows, active.prefs_sel, &sw, cw, ch);
    }

    /// Draw the colour picker over the prefs dialog, from its live H/S/V.
    fn paint_picker(active: &mut Active) {
        let Some(pk) = active.picker else { return };
        let size = active.window.inner_size();
        let (cw, ch) = active.backend.cell_size();
        let g = chrome::colour_picker::layout(cw, ch, size.width as f32, size.height as f32);
        chrome::colour_picker::draw(&mut *active.backend, &g, pk.h, pk.s, pk.v, &pk.slot.label(), cw, ch);
    }

    /// Write the picker's current colour into the pending settings' slot and arm
    /// the settle — so `commit_settings` applies + persists it once the drag
    /// pauses, exactly like a stepped prefs edit (no per-move recolour/persist).
    fn picker_write(active: &mut Active) {
        let Some(pk) = active.picker else { return };
        let rgb = pk.rgb();
        let mut s = active.prefs_pending.clone().unwrap_or_else(|| active.settings.clone());
        set_slot(&mut s, pk.slot, rgb);
        active.prefs_pending = Some(s);
        active.prefs_edits += 1;
        active.last_prefs_edit = Instant::now();
    }

    /// Close the picker (prefs stays open) and commit any pending colour now,
    /// rather than stranding it behind PREFS_SETTLE — mirrors the prefs Esc path.
    fn close_picker(active: &mut Active) {
        active.picker = None;
        if let Some(new) = active.prefs_pending.take() {
            active.prefs_edits = 0;
            Self::commit_settings(active, new);
        }
        active.window.request_redraw();
    }

    /// Apply one step to the selected row: mutate the PENDING settings and arm
    /// the settle. Never applies or persists — that is `commit_settings`, once,
    /// after PREFS_SETTLE.
    fn prefs_step(active: &mut Active, rows: &[chrome::prefs::Row], dir: i32) {
        let Some(pref) = rows.get(active.prefs_sel).and_then(|r| r.pref) else { return };
        if pref == prefs_model::PrefRow::Close {
            return;
        }
        let mut s = active.prefs_pending.clone().unwrap_or_else(|| active.settings.clone());
        prefs_model::step(&mut s, pref, dir, &active.mono_families);
        active.prefs_pending = Some(s);
        active.prefs_edits += 1;
        active.last_prefs_edit = Instant::now();
    }

    /// Move bytes across every patch-bay wire: read each pane's output-stream
    /// jacks and forward to the input jack of every pane wired downstream. Reads
    /// always (even unwired) so a program writing `$RT_OUT` never blocks. Returns
    /// whether any bytes moved (to keep the wire flow animating).
    fn pump_wires(active: &mut Active) -> bool {
        // Per-tick byte budget across ALL jacks. Without it, a pane writing to `$RT_OUT`
        // faster than we drain keeps its fd readable forever, so the inner loop never ends
        // and starves input/rendering/every other pane. When the budget is spent we stop and
        // return `moved` so the run loop schedules another pump next frame — fair progress
        // instead of a monopolised event thread. [review RT-SEC-002]
        const PUMP_BUDGET: usize = 512 * 1024;
        let jacks = active.jacks.clone(); // Rc clone; independent of `active`'s fields
        let src_ids: Vec<rt_core::PaneId> = jacks.borrow().keys().copied().collect();
        let mut moved = false;
        let mut buf = [0u8; 8192];
        let mut budget = PUMP_BUDGET;
        'pump: for src in src_ids {
            for stream in [Stream::Stdout, Stream::Stderr] {
                loop {
                    if budget == 0 {
                        moved = true; // more may be waiting; come back next frame
                        break 'pump;
                    }
                    // Read one chunk from this pane's chosen jack (non-blocking), bounded by
                    // the remaining budget.
                    let cap = buf.len().min(budget);
                    let n = {
                        let mut jm = jacks.borrow_mut();
                        let Some(j) = jm.get_mut(&src) else { break };
                        let fd = match stream {
                            Stream::Stdout => &mut j.out_read,
                            Stream::Stderr => &mut j.err_read,
                        };
                        match fd.read(&mut buf[..cap]) {
                            Ok(0) => break,
                            Ok(n) => n,
                            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                            Err(_) => break,
                        }
                    };
                    moved = true;
                    budget -= n;
                    let chunk = buf[..n].to_vec();
                    // Fan out to every wire leaving this pane on this stream. The destination
                    // FIFO is non-blocking, so a slow reader means a short write; use `write`
                    // (not `write_all`, which would return WouldBlock after a partial write
                    // and silently drop the rest) and count only bytes actually delivered so
                    // the flow meter can't over-report. Excess is dropped — the patch-bay is
                    // explicit best-effort byte wiring, not a guaranteed lossless pipe.
                    // [review RT-IO-001]
                    for w in active.wires.iter_mut().filter(|w| w.src == src && w.stream == stream) {
                        let mut jm = jacks.borrow_mut();
                        if let Some(dj) = jm.get_mut(&w.dst) {
                            if let Ok(written) = dj.in_write.write(&chunk) {
                                w.moved = w.moved.saturating_add(written as u32);
                            }
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
        const HZ: f32 = 100.0; // _SC_CLK_TCK on Linux
        for id in active.session.tree().all_panes() {
            let Some(pid) = active.session.pane(id).and_then(|p| p.pid()) else { continue };
            // Read only this pane's process subtree, not one stat per system
            // process — an idle shell is ~2 file reads instead of dozens/hundreds.
            let ticks = Self::subtree_cpu_ticks(pid);
            let prev = active.heat_ticks.insert(id, ticks).unwrap_or(ticks);
            let load = ticks.saturating_sub(prev) as f32 / (dt * HZ); // fraction of one core
            let e = active.heat.entry(id).or_insert(0.0);
            *e = *e * 0.5 + load * 0.5; // smooth
        }
        true
    }

    /// Sum CPU ticks (utime+stime) over `root` and its descendants, reading only
    /// their `/proc` entries via the kernel's per-task `children` list — O(the
    /// pane's own processes), not O(all system processes). This keeps the heat
    /// instrument from dominating idle CPU on slow machines: an idle shell has no
    /// children, so it costs a single `stat` + an empty `children` read per
    /// sample. Still catches a silent CPU hog (it lives in this subtree).
    fn subtree_cpu_ticks(root: u32) -> u64 {
        let mut total: u64 = 0;
        let mut stack = vec![root];
        let mut visited = 0u32;
        while let Some(pid) = stack.pop() {
            visited += 1;
            if visited > 4096 {
                break; // safety cap against pathological/looping process trees
            }
            if let Ok(content) = std::fs::read_to_string(format!("/proc/{pid}/stat")) {
                // Fields after the parenthesised comm: [11]=utime(14), [12]=stime(15).
                if let Some(rp) = content.rfind(')') {
                    let toks: Vec<&str> = content[rp + 1..].split_whitespace().collect();
                    if toks.len() >= 13 {
                        total += toks[11].parse::<u64>().unwrap_or(0) + toks[12].parse::<u64>().unwrap_or(0);
                    }
                }
            }
            // Direct children of this process's main thread. Needs CONFIG_PROC_CHILDREN
            // (on by default in Debian/Ubuntu); if absent we simply miss grandchildren,
            // never crash.
            if let Ok(kids) = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) {
                for k in kids.split_whitespace() {
                    if let Ok(cpid) = k.parse::<u32>() {
                        stack.push(cpid);
                    }
                }
            }
        }
        total
    }

    /// Optionally advance the instrument animation by wall-clock time (see
    /// `advance`), then draw every enabled instrument (heat borders, orbiting
    /// output packets, patch-bay wires + jacks, latency frame) via
    /// `chrome::instruments` — the SAME native draw on both backends. Called inline
    /// on GL each frame; between `begin/end_instrument_layer` (baked into the
    /// content buffer) on XRender. Reads DISJOINT `active` fields so
    /// `&mut active.backend` coexists with the `&active.*` reads.
    fn draw_instrument_geometry(active: &mut Active, size: winit::dpi::PhysicalSize<u32>, advance: bool) {
        // `advance` moves the animation (orbiting packets, wire flow). GL advances
        // every frame (cheap, smooth); XRender advances only on the 6fps tick so a
        // static-between-ticks frame re-bakes the SAME positions (no ghost trail),
        // and the tick forces a full frame to erase the previous positions.
        if advance {
            let now = Instant::now();
            let dt = now.duration_since(active.last_meter_tick).as_secs_f32().min(0.1);
            active.last_meter_tick = now;
            advance_instrument_state(&mut active.meters, &mut active.wires, dt);
        }
        let bounds = content_bounds(size);
        let rects = active.session.visible_rects(bounds); // owned Vec — no session borrow lingers
        let ctx = chrome::instruments::InstrCtx {
            rects: &rects,
            meters: &active.meters,
            wires: &active.wires,
            heat: &active.heat,
            inst_output: active.settings.inst_output,
            inst_heat: active.settings.inst_heat,
            inst_latency: active.settings.inst_latency,
            show_jacks: active.settings.show_jacks,
            wiring_from: active.wiring_from,
            drag_cursor: active.drag_cursor,
            lat_phase: active.lat_phase,
            stall: active.stall,
            size,
        };
        chrome::instruments::draw(&mut *active.backend, &ctx);
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
fn latency_color(pos: f32, phase: f32, stall: f32) -> Color {
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
    Color::rgb(r, g, b)
}

/// Advance every meter's and wire's exponential rate + flow phase by `dt`
/// seconds of wall-clock, consuming the accumulated `wakeups`/`moved` counts.
/// Shared by the GL (`paint_instruments`) and native (XRender) draw paths so
/// the animation math is identical on both.
fn advance_instrument_state(
    meters: &mut std::collections::HashMap<rt_core::PaneId, Meter>,
    wires: &mut [Wire],
    dt: f32,
) {
    for m in meters.values_mut() {
        let inst = m.wakeups as f32 / dt.max(1e-3);
        m.wakeups = 0;
        m.rate = m.rate * 0.75 + inst * 0.25;
        let act = (m.rate / BUSY_WAKEUPS).clamp(0.0, 1.0);
        m.phase = (m.phase + act * FLOW_MAX_LAPS * dt).fract();
    }
    for w in wires.iter_mut() {
        let inst = w.moved as f32 / dt.max(1e-3);
        w.moved = 0;
        w.rate = w.rate * 0.75 + inst * 0.25;
        let act = (w.rate / WIRE_BUSY_BYTES).clamp(0.0, 1.0);
        w.phase = (w.phase + act * FLOW_MAX_LAPS * dt).fract();
    }
}

/// Planck/blackbody colour for a CPU load (fraction of one core): idle glows a
/// dim deep red, a busy core runs up through orange and yellow to white-hot, a
/// pathological load goes blue-white — load *is* temperature. Ported from rt-mux.
fn heat_color(load: f32) -> Color {
    let n = (load / 1.5).clamp(0.0, 1.0); // normalise (≥1.5 cores = max heat)
    let s = n * n * (3.0 - 2.0 * n); // smoothstep
    let kelvin = 1200.0 + s * 9000.0; // 1200K (dim red) .. 10200K (blue-white)
    let (r, g, b) = blackbody(kelvin);
    let bright = 0.30 + 0.70 * n; // idle dim, busy vivid
    Color::rgb((r * bright) as u8, (g * bright) as u8, (b * bright) as u8)
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

/// Horizontal inset of the titlebar strip's contents.
const TITLEBAR_PAD: f32 = 6.0;

/// How often the broadcast swatch re-paints while it pulses, and how fast it
/// cycles. 5fps is plenty for a "this is armed" throb and bounds the cost.
const BCAST_PULSE_TICK: Duration = Duration::from_millis(200);
const BCAST_PULSE_HZ: f32 = 0.7; // cycles per second

/// The group/broadcast swatch's rect in a pane's titlebar, in window pixels.
///
/// Shared on purpose. `draw_panes` paints it; the frame builder DAMAGES it while
/// it pulses. Chrome carries no engine cell-damage, so the only other way to
/// animate it is `mark_full()` — a whole-window repaint several times a second,
/// which is the exact cost this backend exists to avoid (a full frame is ~250ms
/// on a milkv over ssh -X). Damaging precisely what changes keeps the pulse on
/// the scissored path: a ~13x13px redraw instead of the screen. The two callers
/// must agree to the pixel, so they share this rather than copying it.
fn swatch_rect(full: Rect, bar_h: f32, cell_h: f32) -> (f32, f32, f32) {
    let s = cell_h * 0.6; // swatch side length
    (full.x + TITLEBAR_PAD, full.y + (bar_h - s) * 0.5, s)
}

/// Scale a colour's brightness (alpha untouched) — the broadcast pulse.
fn dim(c: Color, k: f32) -> Color {
    Color(c.0 * k, c.1 * k, c.2 * k, c.3)
}

/// Fast wake interval while animating or interacting (~60fps).
const ACTIVE_POLL: Duration = Duration::from_millis(16);
/// Remote instrument layer redraw cadence: 6fps, decoupled from content frames.
const INSTRUMENT_TICK: Duration = Duration::from_millis(166);

/// How long the window size must hold still before the panes reflow.
///
/// A reflow is `Term::resize` per pane — the grid AND the whole scrollback (10k
/// lines by default) — and it blocks the event loop: measured at a 676ms median
/// per event on a milkv (riscv64) with full history. A drag delivers ~20
/// configures; paying it on each froze the window for ~10.6s and then repainted
/// everything at once. Every intermediate size is superseded within ~50ms, so
/// only the final one is worth reflowing.
///
/// Long enough to swallow a drag's configure stream (~50ms apart), short enough
/// that a single resize doesn't feel deferred.
const RESIZE_SETTLE: Duration = Duration::from_millis(120);
/// How long a preferences value must hold still before it is applied+persisted.
///
/// Applying is expensive — a font change re-rasterises every glyph and reflows
/// every pane (~700ms on a milkv) — and `persist()` writes `config.toml`. So
/// stepping 18 → 24 must cost ONE of each, not six. Long enough to swallow a run
/// of key-repeat presses (~30ms apart), short enough that a single toggle feels
/// immediate. Same rule as RESIZE_SETTLE, same reason: never pay for an
/// intermediate value nobody keeps.
const PREFS_SETTLE: Duration = Duration::from_millis(150);
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
/// How close two arrow-key presses must be to count as the same HELD run (rather than a fresh
/// tap). A touch longer than a slow auto-repeat interval so consecutive repeats always chain.
const ARROW_HOLD_GAP: std::time::Duration = std::time::Duration::from_millis(250);

/// Cursor moves to send for a held arrow at `repeats` consecutive repeats, capped at `max`. A
/// tap or brief hold (`repeats < THRESHOLD`) stays 1:1 for precision; past that it ramps +1
/// move per repeat up to `max`. `max == 1` disables acceleration.
fn arrow_accel_step(repeats: u32, max: u32) -> usize {
    const THRESHOLD: u32 = 4;
    let max = max.max(1) as usize;
    if max == 1 {
        return 1;
    }
    (1 + repeats.saturating_sub(THRESHOLD) as usize).min(max)
}

fn scrollbar_metrics(rect: Rect, offset: usize, history: usize, screen: usize) -> (f32, f32, f32, f32) {
    let total = (history + screen) as f32; // whole buffer height in lines
    // Draw the scrollbar in the pane's right PADDING gutter (PANE_PAD = 5px, just RIGHT of
    // `content_rect`), NOT inside it — so it never overlaps the last text column. Previously
    // it sat at `right - 7`, covering `7 - slack` px of the final cell in every line.
    let bw = 4.0; // scrollbar width (fits within the 5px gutter with a hair of margin)
    let bx = rect.right() + 0.5; // 0.5px into the gutter → clear of every cell
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

/// Colour for the thin rules between newspaper columns: the midpoint of the
/// pane's foreground and background. A fixed near-background grey (the old
/// `0x2a2a33`) was too faint to read on most schemes; the fg/bg mean tracks the
/// user's colours and sits clearly above the background without reaching the
/// weight of text. (Wishlist: "column separator lines can be a bit more
/// visible … a colour halfway between foreground and background".)
/// The running build's identity — crate version plus the git commit stamped in
/// by build.rs (e.g. `rt 0.2.8 (42c5ba7)`), so a from-source build is never
/// mistaken for the release it sits ahead of. Shown by `--version` and in the
/// menu, manual and preferences. `option_env!` keeps it compiling if build.rs
/// didn't run (then it's just `rt <version>`).
pub fn version_string() -> String {
    let v = env!("CARGO_PKG_VERSION");
    match option_env!("RT_GIT_DESC") {
        Some(g) if !g.is_empty() => format!("rt {v} ({g})"),
        _ => format!("rt {v}"),
    }
}

fn column_separator(fg: [u8; 3], bg: [u8; 3]) -> Color {
    let mid = |a: u8, b: u8| ((a as u16 + b as u16) / 2) as u8;
    Color::rgb(mid(fg[0], bg[0]), mid(fg[1], bg[1]), mid(fg[2], bg[2]))
}

/// Write `rgb` into the colour slot the picker edits (foreground, background, or
/// a palette entry). Out-of-range palette indices are ignored.
fn set_slot(s: &mut rt_config::Settings, slot: chrome::colour_picker::Slot, rgb: [u8; 3]) {
    use chrome::colour_picker::Slot;
    match slot {
        Slot::Fg => s.foreground = rgb,
        Slot::Bg => s.background = rgb,
        Slot::Palette(i) => {
            if let Some(c) = s.palette.get_mut(i) {
                *c = rgb;
            }
        }
    }
}

/// Y positions (physical px) for scrollback-search hit markers on a pane's
/// scrollbar track — Chrome/Firefox-style ticks showing where the matches lie in
/// the whole buffer. `lines` are absolute grid lines (`<= 0` in history, up to
/// `screen - 1` at the newest); the track `[rect.y, rect.y + rect.h]` spans the
/// full `history + screen` lines, matching `scrollbar_metrics`' thumb mapping.
///
/// Positions are rounded to the pixel and de-duplicated, so a dense cluster of
/// hits collapses into a single tick instead of a solid bar — this is both the
/// "clusters too close together" behaviour and the cap that keeps the marker
/// count bounded by the track height (never the hit count) over `ssh -X`.
fn hit_marker_ys(
    lines: impl Iterator<Item = i32>,
    history: usize,
    screen: usize,
    rect: Rect,
) -> Vec<f32> {
    let total = (history + screen).max(1) as f32;
    let mut rows: Vec<i32> = lines
        .map(|l| {
            let norm = ((l + history as i32) as f32 / total).clamp(0.0, 1.0);
            (rect.y + norm * rect.h).round() as i32
        })
        .collect();
    rows.sort_unstable();
    rows.dedup();
    rows.into_iter().map(|y| y as f32).collect()
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
fn cubic_bezier(p0: (f32, f32), p1: (f32, f32), p2: (f32, f32), p3: (f32, f32), t: f32) -> (f32, f32) {
    let u = 1.0 - t;
    let (a, b, c, d) = (u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t);
    (
        a * p0.0 + b * p1.0 + c * p2.0 + d * p3.0,
        a * p0.1 + b * p1.1 + c * p2.1 + d * p3.1,
    )
}

/// The point `(x,y)` at normalised position `t` (0..1, clockwise from the top-
/// left) around the perimeter of the rectangle `(x, y, w, h)` — used to place the
/// orbiting output-flow packets.
fn flow_point(x: f32, y: f32, w: f32, h: f32, t: f32) -> (f32, f32) {
    let per = 2.0 * (w + h); // perimeter length
    let d = t.rem_euclid(1.0) * per; // distance travelled along it
    if d < w {
        (x + d, y) // top edge, left → right
    } else if d < w + h {
        (x + w, y + (d - w)) // right edge, top → bottom
    } else if d < 2.0 * w + h {
        (x + w - (d - w - h), y + h) // bottom edge, right → left
    } else {
        (x, y + h - (d - 2.0 * w - h)) // left edge, bottom → top
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
    // A missing display is a user-environment problem, not a bug: rt is a
    // graphical terminal. Fail with guidance and a clean exit(1) instead of the
    // winit panic + core dump (`XOpenDisplayFailed`) you get from `.expect`.
    match builder.build() {
        Ok(el) => el,
        Err(e) => {
            eprintln!(
                "rt: no display found — rt is a graphical terminal and needs a Wayland or X11 display."
            );
            eprintln!(
                "    Over a remote shell, forward X (e.g. `ssh -X <host> rt`) or run rt in a local desktop session."
            );
            eprintln!("    (winit: {e})");
            std::process::exit(1);
        }
    }
}

fn main() {
    // Honour RUST_LOG, but default to showing warnings+errors even when it's unset —
    // otherwise a GL/window-creation failure (logged via log::error!) is silent and the
    // user just sees a blank window with no clue why. RUST_LOG still overrides for more.
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("warn")).init();
    // Parse the CLI FIRST. `--version`/`--help` exit from here, and must work on
    // a machine with no display and no fonts — scripts and CI ask a binary what
    // it is without wanting a terminal window. Parsing after `build_event_loop`
    // made `rt --version` abort with XOpenDisplayFailed over a plain (non-X) ssh.
    let cli = parse_cli();
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
    let mut app = App { font_db, mono_families, cli, active: None };
    if let Err(e) = event_loop.run_app(&mut app) {
        eprintln!("rt: event loop error: {e}"); // surface any run-loop failure
        std::process::exit(1);
    }
}

#[cfg(test)]
mod selection_tests {
    use super::*;

    fn sel(anchor: (usize, i32), head: (usize, i32), block: bool) -> Selection {
        Selection { pane: rt_core::PaneId(1), anchor, head, block }
    }

    #[test]
    fn linear_spans_rows_and_bounds_ends_by_column() {
        // anchor at (col 5, line 2), head at (col 3, line 4): row-major from
        // (5,2) to (3,4). First line clipped left, last clipped right, middle full.
        let s = sel((5, 2), (3, 4), false);
        assert!(!s.contains(4, 2)); // before the start column on the start line
        assert!(s.contains(5, 2)); // start
        assert!(s.contains(80, 2)); // rest of the start line
        assert!(s.contains(0, 3)); // whole middle line
        assert!(s.contains(999, 3));
        assert!(s.contains(3, 4)); // up to the end column on the end line
        assert!(!s.contains(4, 4)); // past the end column
        assert!(!s.contains(0, 1) && !s.contains(0, 5)); // outside the line span
    }

    #[test]
    fn linear_is_order_independent_and_handles_history_lines() {
        // Same selection whichever end is the anchor, incl. negative (history) lines.
        let a = sel((2, -3), (7, -1), false);
        let b = sel((7, -1), (2, -3), false);
        for line in -4..=0 {
            for col in 0..10 {
                assert_eq!(a.contains(col, line), b.contains(col, line), "col {col} line {line}");
            }
        }
        assert!(a.contains(2, -3)); // start corner
        assert!(a.contains(7, -1)); // end corner
        assert!(a.contains(0, -2)); // full middle line
        assert!(!a.contains(1, -3)); // left of start on the start line
    }

    #[test]
    fn block_is_a_rectangle_not_row_major() {
        // Ctrl-drag from (col 6, line 1) to (col 2, line 3): the rectangle
        // cols 2..=6 on lines 1..=3 — NOT the flowing text between the corners.
        let s = sel((6, 1), (2, 3), true);
        for line in 1..=3 {
            assert!(s.contains(2, line) && s.contains(6, line));
            assert!(!s.contains(1, line) && !s.contains(7, line));
        }
        assert!(!s.contains(4, 0) && !s.contains(4, 4)); // outside the line span
        // A linear selection with the same corners would include (col 0, line 2);
        // the block must not.
        assert!(!s.contains(0, 2));
        assert!(sel((6, 1), (2, 3), false).contains(0, 2));
    }
}

#[cfg(test)]
mod sep_tests {
    use super::*;

    #[test]
    fn column_separator_is_the_fg_bg_midpoint() {
        // A light-on-dark scheme: each channel is the mean of fg and bg.
        let c = column_separator([210, 210, 210], [30, 30, 30]);
        assert_eq!(c, Color::rgb(120, 120, 120));
        // Per channel, not a single grey: colours mix independently.
        let c2 = column_separator([200, 100, 0], [0, 0, 40]);
        assert_eq!(c2, Color::rgb(100, 50, 20));
    }

    #[test]
    fn hit_markers_span_the_track_and_dedupe() {
        // A very tall buffer against a short track, so nearby lines share a row.
        let rect = Rect::new(0.0, 100.0, 10.0, 200.0); // track y ∈ [100, 300]
        let (history, screen) = (100_000usize, 100usize);

        // The oldest line pins to the top of the track; the newest to the bottom.
        let top = hit_marker_ys(std::iter::once(-(history as i32)), history, screen, rect);
        assert_eq!(top, vec![100.0]);
        let newest = hit_marker_ys(std::iter::once(screen as i32 - 1), history, screen, rect);
        assert!(newest[0] > 298.0, "newest hit sits near the track bottom");

        // Two adjacent lines collapse to one tick (the cluster behaviour) — at
        // this scale a single line is far under a pixel of track.
        let close = hit_marker_ys([0i32, 1].into_iter(), history, screen, rect);
        assert_eq!(close.len(), 1);

        // Hits spread across the whole buffer yield sorted, unique ticks.
        let spread = hit_marker_ys(
            [-(history as i32), -(history as i32) / 2, screen as i32 - 1].into_iter(),
            history,
            screen,
            rect,
        );
        assert_eq!(spread.len(), 3);
        assert!(spread.windows(2).all(|w| w[0] < w[1]));
    }
}

#[cfg(test)]
mod instr_tests {
    use super::*;

    /// `advance_instrument_state` must reset the wakeup/moved counters, raise
    /// the smoothed rate from live activity, and keep the flow phase wrapped
    /// into `[0.0, 1.0)` — for both a meter and a wire, via the real function
    /// (not a re-derivation of the math).
    #[test]
    fn advance_decays_and_wraps_phase() {
        let mut meters = std::collections::HashMap::new();
        let mut m = Meter::default();
        m.wakeups = BUSY_WAKEUPS as u32; // one core's worth of activity over 1s
        m.rate = 0.0;
        meters.insert(rt_core::PaneId(1), m);

        let mut wires = vec![Wire {
            src: rt_core::PaneId(1),
            stream: Stream::Stdout,
            dst: rt_core::PaneId(2),
            rate: 0.0,
            phase: 0.0,
            moved: WIRE_BUSY_BYTES as u32,
        }];

        advance_instrument_state(&mut meters, &mut wires, 1.0);

        let m = meters.get(&rt_core::PaneId(1)).unwrap();
        assert_eq!(m.wakeups, 0, "wakeups counter is consumed");
        assert!(m.rate > 0.0, "meter rate rises with activity");
        assert!(m.phase >= 0.0 && m.phase < 1.0, "meter phase stays in [0,1)");

        let w = &wires[0];
        assert_eq!(w.moved, 0, "moved counter is consumed");
        assert!(w.rate > 0.0, "wire rate rises with activity");
        assert!(w.phase >= 0.0 && w.phase < 1.0, "wire phase stays in [0,1)");
    }
}
