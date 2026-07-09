//! `rt-mux` — a text-mode (tmux-style) terminal multiplexer.
//!
//! This is rt's *text-mode sibling*. Where the `rt` binary is a Wayland/GL
//! terminal that draws its own pixels, `rt-mux` runs **inside any terminal** and
//! draws with characters — yet it reuses the exact same terminal engine
//! (`rt-engine`, itself a thin wrapper over `alacritty_terminal`). One engine,
//! two front-ends.
//!
//! The whole design is *cells into cells*:
//!   * [`mullion`] is a ratatui-shaped TUI tiling engine. It owns the frame: a
//!     tree of tiles, a diffing double-buffer, borders, focus/zoom, and input.
//!     Hosting real subprocess terminals is a documented *non-goal* it leaves to
//!     "a future consumer" — which is exactly us.
//!   * Each mullion leaf tile hosts one [`rt_engine::TermPane`] (a PTY + shell +
//!     ANSI grid). Every frame we ask mullion for each tile's content rectangle,
//!     take the pane's [`Snapshot`](rt_engine::Snapshot) (a grid of
//!     `char + fg/bg + attrs`), and **blit** it cell-for-cell into mullion's
//!     `Buffer`. mullion then diffs and flushes only what changed.
//!
//! Because the pane snapshot and the mullion cell share the same shape, the
//! adapter is trivial: an `Rgb` becomes a `Color::Rgb`, a `CellAttrs` becomes a
//! `Modifier`, and `buf.set_char` writes it. Splitting, focus, zoom and borders
//! are all mullion's; the PTY layer is all rt-engine's; this file is only the
//! seam between them.

use std::collections::{HashMap, HashSet}; // pane/title maps + routable free cells
use std::ffi::CString; // mkfifo path
use std::fs::{File, OpenOptions}; // the fifo endpoints
use std::io::{self, Read, Stdout, Write}; // stdout backend + Result + pipe I/O
use std::os::unix::ffi::OsStrExt; // OsStr -> bytes for CString
use std::os::unix::fs::OpenOptionsExt; // custom_flags for O_NONBLOCK
use std::path::{Path, PathBuf}; // fifo paths
use std::time::{Duration, Instant}; // frame pacing

/// Number of green "packets" spaced evenly around each pane's border ring. They
/// sit still when the pane is idle and march when it produces output.
const OUTPUT_PACKETS: u32 = 4;
/// Width (in normalised ring position) of each packet's Gaussian bump.
const PACKET_SIGMA: f32 = 0.035;
/// Wakeups-per-second that reads as "fully busy" — the flow speed and brightness
/// saturate here so a torrent of output doesn't spin absurdly fast.
const BUSY_WAKEUPS: f32 = 60.0;
/// Laps-per-second the flow travels at full activity (visually calm, not dizzy).
const MAX_LAPS_PER_SEC: f32 = 0.6;

/// Bytes/second across a wire that reads as "fully busy" for its flow animation.
const WIRE_BUSY_BYTES: f32 = 4096.0;
/// Packets travelling along a wire at once.
const WIRE_PACKETS: u32 = 3;
/// Gaussian width of a wire packet (in normalised path position).
const WIRE_SIGMA: f32 = 0.06;
/// Speed of the latency frame's calm undulation, in laps/second (slow breath).
const LAT_SPEED: f32 = 0.12;
/// Time constant for a latency spike to fade back to calm.
const STALL_TAU: f32 = 0.6;
/// Frame overrun (seconds) beyond the intended budget that reads as a full-height
/// spike. A ~50ms hitch pins the spike; smaller hitches scale down.
const MISS_FULL: f32 = 0.05;
/// Slop below which an overrun is just normal scheduling jitter, not a miss.
const MISS_SLOP: f32 = 0.010;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind}; // input events

use mullion::backend::{Backend, CrosstermBackend}; // Backend trait (size) + the real terminal backend
use mullion::border::{draw_box, render_rim, Borders, BorderStyle, CornerStyle, LineWeight}; // borders + rim animation
use mullion::buffer::Buffer; // the cell buffer we paint into
use mullion::capabilities::Capabilities; // terminal feature probe (colour depth, unicode…)
use mullion::ease::{gaussian, smoothstep}; // bump shape + eased heat ramp
use mullion::float::free_cells_in_window; // routable cells between panes
use mullion::geometry::Rect; // tile rectangles
use mullion::label::Side; // socket edges
use mullion::layout::{solve, Constraint, Node, Orientation, TileId}; // the layout tree + solver
use mullion::route::{render as render_connectors, route_all, Connector, RouteRequest}; // wire routing
use mullion::socket::{draw_socket, Flow, Socket}; // patch-bay jacks
use mullion::style::{Color, Modifier, Style}; // cell colours + attributes
use mullion::terminal::{EventReader, Terminal}; // double-buffer driver + threaded input
use mullion::tree::{Direction, Tree}; // focus/zoom wrapper

use rt_engine::{PaneEvent, TermPane}; // the terminal engine (PTY + alacritty grid)

/// The prefix key that introduces a multiplexer command, tmux-style. We use
/// `Ctrl-a`; pressing it twice sends a literal `Ctrl-a` to the focused shell.
const PREFIX: char = 'a';

/// The built-in manual (Ctrl-a ? or F1). UPPERCASE lines are section headings.
const MANUAL: &str = "\
rt-mux — a text-mode terminal multiplexer with instrumented borders and an
inter-pane fd patch-bay.  The prefix is Ctrl-a; press it, then a command key.

PANES
  C-a %      split side by side        C-a \"     split stacked
  C-a o      focus next               C-a arrows/hjkl   directional focus
  C-a z      zoom / restore           C-a r     rotate the split
  C-a x      close focused pane       C-a q     quit
  mouse      click a pane to focus

SCROLLBACK SEARCH
  C-a /      open search;  type to search;  Enter/Down next, Up prev, Esc close

BORDER INSTRUMENTS  (each pane's border is a live gauge)
  Output   green packets orbit the border; speed = that pane's output rate.
  Heat     the border's colour is the pane's CPU temperature (blackbody):
           dim deep-red idle -> orange -> yellow -> white-hot -> blue-white.

THE PATCH-BAY  (wire terminals' fds together)
  Each pane advertises three pipe jacks to its shell, separate from the tty:
      $RT_OUT  a program writes (stdout jack, right edge, green)
      $RT_ERR  a program writes (stderr jack, right edge, red)
      $RT_IN   a program reads  (stdin  jack, left edge, grey)
  Wire an output jack to another pane's input jack; the moving packets on the
  drawn wire ARE the bytes crossing it.
    keyboard:  C-a w  arm a wire from stdout,  C-a e  from stderr; then move
               focus to the target and press w/e again to connect.
               C-a u  disconnect focused pane.   C-a |  split + pipe stdout in.
    mouse:     drag from a jack to another pane;  right-click a jack to unplug.

EXAMPLES  (type in the panes; wire as noted)
  1. Output to another pane
       A:  seq 1 100 > $RT_OUT     wire A.stdout->B     B:  cat $RT_IN
  2. Stream + filter downstream
       A:  ping -c 20 localhost | tee $RT_OUT   wire A->B   B:  grep time= <$RT_IN
  3. Split stdout and stderr
       A:  ls /nope /etc >$RT_OUT 2>$RT_ERR    wire A.stdout->B, A.stderr->C
  4. One-gesture pipeline
       focus a producer, C-a |  (splits + wires stdout in); new pane:  sort -u <$RT_IN

  Esc or q closes this manual.  Up/Down/PgUp/PgDn scroll.
";

/// Target frame period. We repaint the whole buffer every frame and let mullion
/// diff it, mirroring aerie's `spiral_stress` loop; ~60 fps is smooth and the
/// per-pane snapshot is cheap.
const FRAME: Duration = Duration::from_millis(16);

/// The live "instrument" state for one pane's border: a smoothed output rate and
/// the accumulated flow phase (how far the green packets have marched). Phase
/// advances by the rate, so a busy pane flows and an idle one is frozen — the
/// motion *is* the measurement, not decoration.
#[derive(Default, Clone, Copy)]
struct Meter {
    /// Wakeups counted since the last tick (reset each tick).
    wakeups: u32,
    /// Exponentially-smoothed output activity in wakeups/second.
    rate: f32,
    /// Accumulated flow position in laps (only the fractional part matters).
    phase: f32,
}

/// A pane's side-channel pipe endpoints — its patch-bay jacks, kept separate
/// from the interactive tty. `$RT_OUT` is a fifo a program *writes* to (rt-mux
/// reads it); `$RT_IN` is a fifo rt-mux *writes* to (a program reads it). rt-mux
/// holds both ends open `O_RDWR|O_NONBLOCK` for the pane's whole life so a
/// program never blocks opening or writing even when nothing is wired yet.
struct Jacks {
    out_path: PathBuf, // $RT_OUT — program writes stdout, rt-mux reads
    err_path: PathBuf, // $RT_ERR — program writes stderr, rt-mux reads
    in_path: PathBuf,  // $RT_IN  — rt-mux writes, program reads
    out_read: File,    // rt-mux's persistent handle on out_path
    err_read: File,    // rt-mux's persistent handle on err_path
    in_write: File,    // rt-mux's persistent handle on in_path (feeds the program)
}

impl Jacks {
    /// Create the three fifos for pane `id` under `dir` and open rt-mux's ends.
    fn new(dir: &Path, id: TileId) -> io::Result<Jacks> {
        let out_path = dir.join(format!("{id}.out"));
        let err_path = dir.join(format!("{id}.err"));
        let in_path = dir.join(format!("{id}.in"));
        mkfifo(&out_path)?;
        mkfifo(&err_path)?;
        mkfifo(&in_path)?;
        // O_RDWR keeps a peer present so the fifo never hits EOF and the program's
        // open/write never blocks; O_NONBLOCK keeps rt-mux's own I/O async.
        let open = |p: &Path| {
            OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(p)
        };
        let out_read = open(&out_path)?;
        let err_read = open(&err_path)?;
        let in_write = open(&in_path)?;
        Ok(Jacks { out_path, err_path, in_path, out_read, err_read, in_write })
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
    /// Remove the fifos when the pane closes (the open handles drop with us).
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.out_path);
        let _ = std::fs::remove_file(&self.err_path);
        let _ = std::fs::remove_file(&self.in_path);
    }
}

/// Create a fifo at `path` (mode 0600); an existing fifo is fine.
fn mkfifo(path: &Path) -> io::Result<()> {
    let c = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "fifo path has a NUL byte"))?;
    // SAFETY: `c` is a valid NUL-terminated C string for the duration of the call.
    let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
    if rc != 0 {
        let err = io::Error::last_os_error();
        if err.kind() != io::ErrorKind::AlreadyExists {
            return Err(err); // a real failure (permissions, missing dir, …)
        }
    }
    Ok(())
}

/// Which of a pane's output streams a wire draws from.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Stream {
    Stdout, // $RT_OUT — green
    Stderr, // $RT_ERR — red
}

/// One live patch-bay connection: bytes read from `src`'s output stream jack are
/// written to `dst`'s input jack. Throughput drives the wire's flow animation, so
/// the packets you see crossing the wire are the literal bytes on the pipe.
struct Wire {
    src: TileId,     // source pane
    stream: Stream,  // which of its output streams (stdout / stderr)
    dst: TileId,     // write to dst's $RT_IN
    rate: f32,       // smoothed bytes/second across the wire
    phase: f32,      // flow position along the wire (laps)
    moved: u32,      // bytes carried since the last tick (reset each tick)
}

/// The whole multiplexer: the mullion layout tree, one engine pane per leaf, the
/// per-pane titles, and the small amount of interaction state.
struct Mux {
    /// mullion's focus/zoom-aware layout tree. Its leaves are `Node::Tile(id)`;
    /// each `id` keys a pane in `panes`.
    tree: Tree,
    /// One terminal engine (PTY + shell) per visible tile.
    panes: HashMap<TileId, TermPane>,
    /// Latest OSC/shell title per pane, shown in its titlebar.
    titles: HashMap<TileId, String>,
    /// Per-pane side-channel pipe jacks ($RT_OUT / $RT_IN).
    jacks: HashMap<TileId, Jacks>,
    /// Active patch-bay wires (src.out -> dst.in).
    wires: Vec<Wire>,
    /// When wiring: the source pane + stream whose jack we're dragging from.
    wiring_from: Option<(TileId, Stream)>,
    /// Pane box rectangles from the last frame, for mouse hit-testing.
    last_boxes: Vec<(TileId, Rect)>,
    /// Live cursor cell while dragging a wire with the mouse (rubber-band).
    drag_cursor: Option<(u16, u16)>,
    /// Whether the built-in manual overlay is open.
    manual_open: bool,
    /// Scroll offset (top line) of the manual overlay.
    manual_scroll: usize,
    /// Whether the scrollback-search bar is open.
    search_open: bool,
    /// The current search query.
    search_query: String,
    /// Hits for `search_query` in `search_pane`.
    search_matches: Vec<rt_engine::SearchMatch>,
    /// Which hit is "current" (highlighted brighter, scrolled to).
    search_index: usize,
    /// The pane the current matches belong to (the focused pane when opened).
    search_pane: Option<TileId>,
    /// Session temp directory holding all the fifos.
    dir: PathBuf,
    /// Per-pane border instrument state (output-activity flow).
    meters: HashMap<TileId, Meter>,
    /// Timestamp of the previous frame, for the animation's wall-clock `dt`.
    last_frame: Instant,
    /// The frame budget we slept for last iteration, so an overrun can be told
    /// apart from an intentionally-lazy idle tick.
    last_budget: Duration,
    /// Phase of the latency frame's calm undulation (laps).
    lat_phase: f32,
    /// Current latency-spike severity (0 = calm, 1 = a bad deadline miss); decays.
    stall: f32,
    /// Per-pane CPU load (fraction of one core), smoothed — the heat instrument.
    heat: HashMap<TileId, f32>,
    /// Per-pane cumulative session CPU ticks at the last heat sample.
    heat_ticks: HashMap<TileId, u64>,
    /// When the heat instrument last sampled `/proc`.
    heat_last: Instant,
    /// Monotonic id allocator for new tiles (never reused, so stale ids are inert).
    next_id: TileId,
    /// True while we are between the prefix key and its command key.
    prefix_armed: bool,
    /// The tiling area from the last frame, used by directional focus (which
    /// needs a viewport but runs before the next draw computes one).
    area: Rect,
}

impl Mux {
    /// Build the multiplexer with a single pane filling `area` (minus the outer
    /// border). Fails only if the first PTY cannot be spawned.
    fn new(area: Rect) -> io::Result<Self> {
        // A per-session temp directory holds every pane's fifos.
        let dir = std::env::temp_dir().join(format!("rt-mux-{}", std::process::id()));
        std::fs::create_dir_all(&dir)?;
        let mut mux = Mux {
            tree: Tree::new(Node::Tile(1)), // start with one leaf, id 1
            panes: HashMap::new(),
            titles: HashMap::new(),
            jacks: HashMap::new(),
            wires: Vec::new(),
            wiring_from: None,
            last_boxes: Vec::new(),
            drag_cursor: None,
            manual_open: false,
            manual_scroll: 0,
            search_open: false,
            search_query: String::new(),
            search_matches: Vec::new(),
            search_index: 0,
            search_pane: None,
            dir,
            meters: HashMap::new(),
            last_frame: Instant::now(),
            last_budget: FRAME,
            lat_phase: 0.0,
            stall: 0.0,
            heat: HashMap::new(),
            heat_ticks: HashMap::new(),
            heat_last: Instant::now(),
            next_id: 2, // 1 is taken by the first pane
            prefix_armed: false,
            area,
        };
        // Size the first pane to the content area (screen minus the 1-cell frame).
        let cols = area.width.saturating_sub(2).max(1) as usize;
        let rows = area.height.saturating_sub(2).max(1) as usize;
        let env = mux.make_jacks(1); // its $RT_OUT / $RT_IN
        let pane = TermPane::spawn_env(None, None, cols, rows, &env, rt_engine::DEFAULT_SCROLLBACK)?; // None = default login shell
        mux.panes.insert(1, pane);
        mux.tree.focus_set(1); // focus the only pane
        Ok(mux)
    }

    /// Create the pipe jacks for pane `id` and return the env vars to export.
    /// Best-effort: if the fifos can't be made the pane still runs, just without
    /// wiring (it simply has no jacks entry).
    fn make_jacks(&mut self, id: TileId) -> Vec<(String, String)> {
        match Jacks::new(&self.dir, id) {
            Ok(j) => {
                let env = j.env();
                self.jacks.insert(id, j);
                env
            }
            Err(_) => Vec::new(),
        }
    }

    /// Spawn a fresh engine pane at a placeholder size (the next frame resizes it
    /// to its real rectangle) and register it under a new id, with its own jacks.
    /// Returns the id, or `None` if the PTY could not be created.
    fn spawn_pane(&mut self) -> Option<TileId> {
        let id = self.next_id; // candidate id (committed only on success)
        let env = self.make_jacks(id);
        // 80x24 is only a seed; `render` resizes every pane to its tile each frame.
        match TermPane::spawn_env(None, None, 80, 24, &env, rt_engine::DEFAULT_SCROLLBACK) {
            Ok(pane) => {
                self.next_id += 1;
                self.panes.insert(id, pane);
                Some(id)
            }
            Err(_) => {
                self.jacks.remove(&id); // reap the jacks we just made
                None
            }
        }
    }

    /// Split the focused tile along `orient`, spawning a new pane in the freed
    /// half and moving focus to it. Un-zooms first so the edit targets the real
    /// tree, not a zoomed subtree.
    fn split(&mut self, orient: Orientation) {
        if self.tree.is_zoomed() {
            self.tree.zoom_reset(); // structural edits operate on the full tree
        }
        let Some(focus) = self.tree.focus() else { return };
        let Some(new_id) = self.spawn_pane() else { return }; // no pane → no split
        // Replace the focused `Tile` leaf with a 2-child `Split`.
        if split_tile(self.tree.root_mut(), focus, new_id, orient) {
            self.tree.ensure_focus_valid(); // the tree shape changed
            self.tree.focus_set(new_id); // focus follows the split, like Terminator/tmux
        } else {
            self.panes.remove(&new_id); // couldn't place it: reap the orphan pane
        }
    }

    /// Close tile `id`: drop its pane (which shuts the PTY down via `Drop`) and
    /// prune it from the tree, collapsing any split left with a single child.
    /// Returns `true` when no panes remain (the caller should quit).
    fn close_tile(&mut self, id: TileId) -> bool {
        if !self.panes.contains_key(&id) {
            return false; // unknown/stale id: nothing to do
        }
        if self.tree.is_zoomed() {
            self.tree.zoom_reset();
        }
        // If the root itself is this tile, the tree becomes empty → we quit.
        let root_was_target = matches!(self.tree.root_mut(), Node::Tile(t) if *t == id);
        if !root_was_target {
            remove_tile(self.tree.root_mut(), id); // prune + collapse
        }
        self.panes.remove(&id); // Drop -> PTY shutdown + thread join
        self.titles.remove(&id);
        self.meters.remove(&id); // forget its instrument state
        self.heat.remove(&id); // forget its heat reading
        self.heat_ticks.remove(&id);
        self.jacks.remove(&id); // Drop -> remove its fifos
        self.wires.retain(|w| w.src != id && w.dst != id); // unplug any wires on it
        if matches!(self.wiring_from, Some((s, _)) if s == id) {
            self.wiring_from = None; // a pending wire lost its source
        }
        self.tree.ensure_focus_valid(); // reseat focus onto a survivor
        self.tree.ensure_zoom_valid();
        root_was_target || self.panes.is_empty()
    }

    /// Handle one key. Returns `false` to quit the multiplexer.
    ///
    /// Keys pass straight through to the focused shell *unless* they follow the
    /// prefix, in which case they are multiplexer commands (split/focus/zoom/…).
    fn handle_key(&mut self, key: KeyEvent) -> bool {
        // The manual overlay captures every key while it is open.
        if self.manual_open {
            match key.code {
                KeyCode::Esc | KeyCode::Char('q') => self.manual_open = false,
                KeyCode::Down | KeyCode::Char('j') => self.manual_scroll += 1,
                KeyCode::Up | KeyCode::Char('k') => self.manual_scroll = self.manual_scroll.saturating_sub(1),
                KeyCode::PageDown | KeyCode::Char(' ') => self.manual_scroll += 10,
                KeyCode::PageUp => self.manual_scroll = self.manual_scroll.saturating_sub(10),
                _ => {}
            }
            return true;
        }
        // F1 (no prefix) toggles the manual.
        if key.code == KeyCode::F(1) {
            self.manual_open = true;
            self.manual_scroll = 0;
            return true;
        }
        // The scrollback-search bar captures every key while it is open.
        if self.search_open {
            self.handle_search_key(key);
            return true;
        }
        // Is this the prefix chord (Ctrl-a)?
        let is_prefix = matches!(key.code, KeyCode::Char(PREFIX))
            && key.modifiers.contains(KeyModifiers::CONTROL);

        if self.prefix_armed {
            self.prefix_armed = false; // the command key consumes the armed state
            match key.code {
                // Prefix twice → send a literal Ctrl-a to the shell.
                KeyCode::Char(PREFIX) if key.modifiers.contains(KeyModifiers::CONTROL) => {
                    self.forward_key(key);
                }
                // Splits (tmux mnemonics): % / v = side-by-side, " / s = stacked.
                KeyCode::Char('%') | KeyCode::Char('v') => self.split(Orientation::Horizontal),
                KeyCode::Char('"') | KeyCode::Char('s') => self.split(Orientation::Vertical),
                // Close the focused pane; quit if it was the last one.
                KeyCode::Char('x') => {
                    if let Some(f) = self.tree.focus() {
                        if self.close_tile(f) {
                            return false;
                        }
                    }
                }
                // Cycle focus / directional focus.
                KeyCode::Char('o') | KeyCode::Tab => self.tree.focus_next(),
                KeyCode::Left | KeyCode::Char('h') => self.tree.focus_dir_cross(Direction::Left, self.area),
                KeyCode::Right | KeyCode::Char('l') => self.tree.focus_dir_cross(Direction::Right, self.area),
                KeyCode::Up | KeyCode::Char('k') => self.tree.focus_dir_cross(Direction::Up, self.area),
                KeyCode::Down | KeyCode::Char('j') => self.tree.focus_dir_cross(Direction::Down, self.area),
                // Zoom (maximise) the focused pane / restore.
                KeyCode::Char('z') => {
                    if self.tree.is_zoomed() {
                        self.tree.zoom_out();
                    } else {
                        self.tree.zoom_focus();
                    }
                }
                // Rotate: flip the focused tile's parent split H<->V.
                KeyCode::Char('r') => self.tree.flip_focused_parent(),
                // Wire: 'w' arms from the focused pane's stdout jack, 'e' from its
                // stderr jack; the next 'w'/'e' (after moving focus to the target)
                // completes the wire src -> dst.in. Re-firing on the same pane cancels.
                KeyCode::Char('w') => self.wire_gesture(Stream::Stdout),
                KeyCode::Char('e') => self.wire_gesture(Stream::Stderr),
                // Unwire: disconnect every wire touching the focused pane.
                KeyCode::Char('u') => {
                    if let Some(f) = self.tree.focus() {
                        self.wires.retain(|w| w.src != f && w.dst != f);
                    }
                }
                // Pipe: split, then wire the (old) focused pane's stdout into the
                // new pane's stdin — a cross-pane pipeline. Run a consumer reading
                // $RT_IN in the new pane (e.g. `grep foo < $RT_IN`).
                KeyCode::Char('|') => self.pipe_into_new_pane(),
                // Scrollback search on the focused pane.
                KeyCode::Char('/') => self.open_search(),
                // Built-in manual.
                KeyCode::Char('?') => {
                    self.manual_open = true;
                    self.manual_scroll = 0;
                }
                // Quit the whole multiplexer.
                KeyCode::Char('q') => return false,
                _ => {} // unknown command: ignore
            }
            return true;
        }

        if is_prefix {
            self.prefix_armed = true; // arm; the next key is a command
            return true;
        }

        // Esc cancels a pending wire rather than reaching the shell.
        if self.wiring_from.is_some() && matches!(key.code, KeyCode::Esc) {
            self.wiring_from = None;
            self.drag_cursor = None;
            return true;
        }

        self.forward_key(key); // ordinary key → the focused shell
        true
    }

    /// Keyboard wire gesture: with nothing armed, arm from the focused pane's
    /// `stream` jack; with something armed, complete to the focused pane's input.
    fn wire_gesture(&mut self, stream: Stream) {
        let focus = self.tree.focus();
        match self.wiring_from.take() {
            None => self.wiring_from = focus.map(|f| (f, stream)), // arm
            Some((src, s)) => {
                if let Some(dst) = focus {
                    self.connect_wire(src, s, dst); // complete (uses the armed stream)
                }
            }
        }
    }

    /// Add a wire `src`.`stream` → `dst`.stdin, if both panes have jacks and it's
    /// not a self-loop. Shared by the keyboard and mouse gestures.
    fn connect_wire(&mut self, src: TileId, stream: Stream, dst: TileId) {
        if src != dst && self.jacks.contains_key(&src) && self.jacks.contains_key(&dst) {
            self.wires.push(Wire { src, stream, dst, rate: 0.0, phase: 0.0, moved: 0 });
        }
    }

    /// Split off a new pane and wire the *previously* focused pane's stdout into
    /// it — a cross-pane pipeline. The new pane is a fresh shell; run a consumer
    /// on `$RT_IN` there (e.g. `grep pattern < $RT_IN`).
    fn pipe_into_new_pane(&mut self) {
        let Some(src) = self.tree.focus() else { return };
        self.split(Orientation::Horizontal); // split() focuses the new pane
        if let Some(dst) = self.tree.focus() {
            self.connect_wire(src, Stream::Stdout, dst);
        }
    }

    /// Open the scrollback-search bar for the focused pane.
    fn open_search(&mut self) {
        self.search_open = true;
        self.search_query.clear();
        self.search_matches.clear();
        self.search_index = 0;
        self.search_pane = self.tree.focus();
    }

    /// Close the search bar and clear its state.
    fn close_search(&mut self) {
        self.search_open = false;
        self.search_query.clear();
        self.search_matches.clear();
        self.search_index = 0;
        self.search_pane = None;
    }

    /// Handle a key while the search bar is open: edit the query, or navigate.
    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => self.close_search(),
            KeyCode::Enter | KeyCode::Down => self.search_step(1),
            KeyCode::Up => self.search_step(-1),
            KeyCode::Backspace => {
                self.search_query.pop();
                self.run_search(true);
            }
            KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                self.search_query.push(c);
                self.run_search(true);
            }
            _ => {}
        }
    }

    /// Re-run the query against the search pane. `jump` (a fresh query) resets to
    /// the first hit and scrolls to it; otherwise the position is kept.
    fn run_search(&mut self, jump: bool) {
        let Some(id) = self.search_pane else { return };
        self.search_matches = match self.panes.get(&id) {
            Some(p) => p.search(&self.search_query, false), // case-insensitive
            None => Vec::new(),
        };
        if jump {
            self.search_index = 0;
            if let (Some(m), Some(p)) = (self.search_matches.first(), self.panes.get(&id)) {
                p.scroll_to_line(m.line);
            }
        } else {
            self.search_index = self.search_index.min(self.search_matches.len().saturating_sub(1));
        }
    }

    /// Move to the next (`+1`) or previous (`-1`) hit, wrapping, and scroll to it.
    fn search_step(&mut self, dir: isize) {
        let n = self.search_matches.len();
        if n == 0 {
            return;
        }
        self.search_index = ((self.search_index as isize + dir).rem_euclid(n as isize)) as usize;
        let line = self.search_matches[self.search_index].line;
        if let Some(p) = self.search_pane.and_then(|id| self.panes.get(&id)) {
            p.scroll_to_line(line);
        }
    }

    /// Mouse: left-press on an output jack starts a drag-wire (rubber-band
    /// follows the cursor); release over a pane connects to its input jack.
    /// Left-press elsewhere focuses the pane under the cursor; right-press
    /// cancels a pending wire.
    fn handle_mouse(&mut self, m: MouseEvent) {
        let (mx, my) = (m.column, m.row);
        // Wiring gestures own the mouse when they apply; otherwise the event is
        // for the pane under the cursor.
        match m.kind {
            // Left-press on an output jack begins a drag-wire.
            MouseEventKind::Down(MouseButton::Left) if self.jack_at(mx, my).is_some() => {
                self.wiring_from = self.jack_at(mx, my);
                self.drag_cursor = Some((mx, my));
                return;
            }
            // Drag / release / right-press while wiring drive the rubber-band.
            MouseEventKind::Drag(MouseButton::Left) if self.wiring_from.is_some() => {
                self.drag_cursor = Some((mx, my));
                return;
            }
            MouseEventKind::Up(MouseButton::Left) if self.wiring_from.is_some() => {
                if let Some((src, stream)) = self.wiring_from.take() {
                    if let Some(dst) = self.pane_at(mx, my) {
                        self.connect_wire(src, stream, dst);
                    }
                }
                self.drag_cursor = None;
                return;
            }
            MouseEventKind::Down(MouseButton::Right) => {
                // Cancel a pending wire, or disconnect the output jack under the
                // cursor (fan-out/merge management).
                if self.wiring_from.take().is_some() {
                    self.drag_cursor = None;
                    return;
                }
                if let Some((id, stream)) = self.jack_at(mx, my) {
                    self.wires.retain(|w| !(w.src == id && w.stream == stream));
                    return;
                }
                // else: fall through and forward to the app
            }
            _ => {}
        }
        // Not a wiring gesture: focus on press, then forward to the pane's app.
        if matches!(m.kind, MouseEventKind::Down(_)) {
            if let Some(id) = self.pane_at(mx, my) {
                self.tree.focus_set(id);
            }
        }
        self.forward_mouse(m, mx, my);
    }

    /// Forward a mouse event to the app in the pane under the cursor, translated
    /// to that pane's local content coordinates — but only if the app enabled
    /// mouse reporting (else the sequence would be garbage in a plain shell).
    fn forward_mouse(&mut self, m: MouseEvent, mx: u16, my: u16) {
        let Some(id) = self.pane_at(mx, my) else { return };
        let Some(&(_, bx)) = self.last_boxes.iter().find(|&&(i, _)| i == id) else { return };
        // Content region = box interior; ignore clicks on the border.
        let (cx0, cy0) = (bx.x + 1, bx.y + 1);
        if mx < cx0 || my < cy0 || mx + 1 >= bx.right() || my + 1 >= bx.bottom() {
            return;
        }
        let (lcol, lrow) = (mx - cx0 + 1, my - cy0 + 1); // 1-based, pane-local
        if let Some(pane) = self.panes.get(&id) {
            if !pane.wants_mouse() {
                return; // the app isn't listening for the mouse
            }
            if let Some(bytes) = encode_mouse(&m, lcol, lrow, pane.mouse_sgr()) {
                pane.write(&bytes);
            }
        }
    }

    /// Which output jack (if any) the point `(mx, my)` hits: the right edge of a
    /// pane box carries the stdout jack (upper third) and stderr jack (lower
    /// third). Accepts the border column and the cell just outside it.
    fn jack_at(&self, mx: u16, my: u16) -> Option<(TileId, Stream)> {
        for &(id, bx) in &self.last_boxes {
            if bx.width < 2 || bx.height < 2 {
                continue;
            }
            if mx + 1 >= bx.right() && mx <= bx.right() {
                let so_row = bx.y + (bx.height / 3).max(1);
                let se_row = bx.y + (2 * bx.height / 3).max(2);
                if my.abs_diff(so_row) <= 1 {
                    return Some((id, Stream::Stdout));
                }
                if my.abs_diff(se_row) <= 1 {
                    return Some((id, Stream::Stderr));
                }
            }
        }
        None
    }

    /// The pane box containing `(mx, my)`, if any.
    fn pane_at(&self, mx: u16, my: u16) -> Option<TileId> {
        self.last_boxes.iter().find(|&&(_, bx)| bx.contains(mx, my)).map(|&(id, _)| id)
    }

    /// Encode a key event and write it to the focused pane's PTY. The pane's
    /// application-cursor-keys mode selects CSI vs SS3 arrow encodings.
    fn forward_key(&mut self, key: KeyEvent) {
        let Some(focus) = self.tree.focus() else { return };
        if let Some(pane) = self.panes.get(&focus) {
            let app_cursor = pane.app_cursor_keys(); // DECCKM: arrows as SS3 not CSI
            if let Some(bytes) = encode_key(&key, app_cursor) {
                pane.write(&bytes); // straight to the shell
            }
        }
    }

    /// Drain each pane's async engine events (titles, child exits). Returns
    /// `true` when every pane has exited (quit the multiplexer).
    fn poll_panes(&mut self) -> bool {
        // Snapshot the ids first so we can mutate `panes` while iterating.
        let ids: Vec<TileId> = self.panes.keys().copied().collect();
        let mut exited: Vec<TileId> = Vec::new();
        for id in ids {
            if let Some(pane) = self.panes.get(&id) {
                for ev in pane.drain_events() {
                    match ev {
                        PaneEvent::Title(t) => {
                            self.titles.insert(id, t); // update the titlebar text
                        }
                        PaneEvent::Exited => exited.push(id), // shell exited: close later
                        PaneEvent::Wakeup => {
                            // New output was parsed → one unit of activity for the
                            // border flow (the honest, zero-cost throughput proxy).
                            self.meters.entry(id).or_default().wakeups += 1;
                        }
                        PaneEvent::Bell => {} // (a bell burst will drive the flow later)
                    }
                }
            }
        }
        // Close every pane whose shell exited; quit if that empties the tree.
        for id in exited {
            if self.close_tile(id) {
                return true;
            }
        }
        false
    }

    /// Move bytes across every wire: read whatever each pane wrote to its output
    /// jack and forward it to the input jack of every pane wired downstream.
    /// Reading always happens (even unwired) so a program writing to `$RT_OUT`
    /// never blocks. Returns whether any bytes moved (to keep the loop lively).
    fn pump(&mut self) -> bool {
        let mut moved = false;
        let srcs: Vec<TileId> = self.jacks.keys().copied().collect();
        // Drain both output streams of every pane, tagging which stream.
        for src in srcs {
            moved |= self.pump_stream(src, Stream::Stdout);
            moved |= self.pump_stream(src, Stream::Stderr);
        }
        moved
    }

    /// Drain one pane's given output stream jack and fan it to matching wires.
    fn pump_stream(&mut self, src: TileId, stream: Stream) -> bool {
        let mut moved = false;
        let mut buf = [0u8; 8192];
        loop {
            // Read one chunk from the chosen jack (non-blocking).
            let n = match self.jacks.get_mut(&src) {
                Some(j) => {
                    let fd = match stream {
                        Stream::Stdout => &mut j.out_read,
                        Stream::Stderr => &mut j.err_read,
                    };
                    match fd.read(&mut buf) {
                        Ok(0) => break, // (O_RDWR means no EOF; treat as "nothing")
                        Ok(n) => n,
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(_) => break,
                    }
                }
                None => break,
            };
            moved = true;
            // Fan the chunk to every wire leaving this pane on this stream. (Own
            // the bytes so the src-jack borrow is released before touching dsts.)
            let chunk = buf[..n].to_vec();
            for w in self.wires.iter_mut().filter(|w| w.src == src && w.stream == stream) {
                if let Some(dj) = self.jacks.get_mut(&w.dst) {
                    let _ = dj.in_write.write_all(&chunk); // best-effort; drops if the reader is behind
                    w.moved = w.moved.saturating_add(n as u32);
                }
            }
        }
        moved
    }

    /// Sample `/proc` (about twice a second) to update each pane's CPU load — the
    /// heat instrument. A pane's load is summed over its whole session (the shell
    /// plus whatever it's running), so `make`/`vim`/etc. count. Cheap: one pass
    /// over `/proc`, ~2 Hz.
    fn sample_heat(&mut self) {
        let now = Instant::now();
        let dt = now.duration_since(self.heat_last).as_secs_f32();
        if dt < 0.4 {
            return; // throttle to ~2 Hz
        }
        self.heat_last = now;
        // Sum cumulative CPU ticks per session in a single /proc scan.
        let mut by_session: HashMap<u32, u64> = HashMap::new();
        if let Ok(rd) = std::fs::read_dir("/proc") {
            for ent in rd.flatten() {
                let name = ent.file_name();
                let Some(s) = name.to_str() else { continue };
                if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
                    continue; // only numeric pid dirs
                }
                let Ok(content) = std::fs::read_to_string(ent.path().join("stat")) else { continue };
                // comm (field 2) is parenthesised and may contain spaces/`)`, so
                // parse the fixed fields after the final ')'.
                let Some(rp) = content.rfind(')') else { continue };
                let toks: Vec<&str> = content[rp + 1..].split_whitespace().collect();
                // After ')': [0]=state(3) [3]=session(6) [11]=utime(14) [12]=stime(15)
                if toks.len() < 13 {
                    continue;
                }
                let session: u32 = toks[3].parse().unwrap_or(0);
                let utime: u64 = toks[11].parse().unwrap_or(0);
                let stime: u64 = toks[12].parse().unwrap_or(0);
                *by_session.entry(session).or_default() += utime + stime;
            }
        }
        const HZ: f32 = 100.0; // _SC_CLK_TCK on Linux
        let ids: Vec<TileId> = self.panes.keys().copied().collect();
        for id in ids {
            let Some(pid) = self.panes.get(&id).and_then(|p| p.pid()) else { continue };
            let ticks = by_session.get(&pid).copied().unwrap_or(0);
            let prev = self.heat_ticks.insert(id, ticks).unwrap_or(ticks);
            let load = ticks.saturating_sub(prev) as f32 / (dt * HZ); // fraction of one core
            let e = self.heat.entry(id).or_insert(0.0);
            *e = *e * 0.5 + load * 0.5; // smooth
        }
    }

    /// Advance the border instruments by `dt` seconds: convert each pane's
    /// wakeup count into a smoothed rate and march its flow phase by that rate.
    /// A silent pane's phase does not move (idle borders are frozen); a busy
    /// pane's marches, saturating at [`MAX_LAPS_PER_SEC`].
    fn tick(&mut self, dt: f32) {
        if dt <= 0.0 {
            return; // no time passed (or the clock went backwards): nothing to do
        }
        for meter in self.meters.values_mut() {
            // Instantaneous rate this frame, then an exponential moving average so
            // the flow eases in/out instead of strobing frame-to-frame.
            let instant = meter.wakeups as f32 / dt;
            meter.wakeups = 0;
            meter.rate = meter.rate * 0.75 + instant * 0.25;
            // Map activity (0..=BUSY) to a lap speed, and advance the phase.
            let activity = (meter.rate / BUSY_WAKEUPS).clamp(0.0, 1.0);
            meter.phase = (meter.phase + activity * MAX_LAPS_PER_SEC * dt).fract();
        }
        // Wires: smooth each wire's byte-rate and march its flow by that rate, so
        // the packets crossing a wire are the literal bytes on the pipe.
        for w in self.wires.iter_mut() {
            let instant = w.moved as f32 / dt;
            w.moved = 0;
            w.rate = w.rate * 0.75 + instant * 0.25;
            let activity = (w.rate / WIRE_BUSY_BYTES).clamp(0.0, 1.0);
            w.phase = (w.phase + activity * MAX_LAPS_PER_SEC * dt).fract();
        }
        // Latency frame: undulation always breathes; the spike decays toward calm.
        self.lat_phase = (self.lat_phase + LAT_SPEED * dt).fract();
        self.stall *= (-dt / STALL_TAU).exp();
    }

    /// Register that a frame took `dt` seconds against an intended `budget`. An
    /// overrun beyond normal jitter raises the latency spike proportionally — this
    /// is what makes an external CPU hogger visible as a violet flare on the frame.
    fn note_latency(&mut self, dt: f32, budget: f32) {
        let overrun = dt - budget; // how much longer than we asked for
        if overrun > MISS_SLOP {
            let severity = (overrun / MISS_FULL).clamp(0.0, 1.0);
            self.stall = self.stall.max(severity); // keep the worst recent hitch
        }
    }

    /// Draw the latency instrument on the window frame `outer`: a calm
    /// purple-blue-violet undulation, with a bright fast-moving flare when the
    /// render loop missed its deadline (a hogger stole the frame).
    fn draw_latency_frame(&self, buf: &mut Buffer, outer: Rect) {
        // Base structural frame to recolour (dim violet).
        let base = BorderStyle {
            weight: LineWeight::Light,
            corners: CornerStyle::Rounded,
            style: Style::default().fg(Color::Rgb(0x3a, 0x2c, 0x50)),
        };
        draw_box(buf, outer, Borders::ALL, &base);
        let phase = self.lat_phase;
        let stall = self.stall;
        render_rim(buf, outer, &[], |pos, cur| {
            // Calm undulation: two slow lobes drifting around the ring.
            use std::f32::consts::TAU;
            let wave = 0.5 + 0.5 * (TAU * (pos * 2.0 + phase)).sin(); // 0..1
            let base_v = 0.20 + 0.30 * wave; // gentle brightness band
            // Spike: a sharp bright bump that laps fast while a stall is active.
            let spike = if stall > 0.01 {
                let sp = (phase * 4.0).fract(); // moves faster than the calm wave
                let mut d = (pos - sp).abs();
                if d > 0.5 {
                    d = 1.0 - d;
                }
                stall * gaussian(d, 0.06)
            } else {
                0.0
            };
            let v = (base_v + spike).clamp(0.0, 1.0);
            // Purple-blue-violet: red & blue lead, green trails; a spike whitens it.
            let r = (56.0 + 128.0 * v + spike * 120.0).min(255.0) as u8;
            let g = (32.0 + 80.0 * v + spike * 110.0).min(255.0) as u8;
            let b = (88.0 + 152.0 * v + spike * 40.0).min(255.0) as u8;
            Some(cur.fg(Color::Rgb(r, g, b)))
        });
    }

    /// Draw the flowing green output instrument onto pane `id`'s border ring
    /// (`tile` is its full rectangle, border included). Packets are Gaussian
    /// bumps spaced around the perimeter; their brightness scales with activity
    /// so an idle pane shows dim static dots and a busy one bright marching ones.
    fn draw_output_flow(&self, buf: &mut Buffer, id: TileId, tile: Rect) {
        let Some(meter) = self.meters.get(&id) else { return };
        let phase = meter.phase; // where the packets are this frame
        let activity = (meter.rate / BUSY_WAKEUPS).clamp(0.0, 1.0); // 0=idle .. 1=busy
        // No gaps: the title is drawn *after* this pass, so it naturally sits on top.
        render_rim(buf, tile, &[], |pos, cur| {
            // Nearest packet to this perimeter position, measured as a circular
            // distance so bumps cross the corners seamlessly.
            let mut best = 0.0f32;
            for k in 0..OUTPUT_PACKETS {
                let p = (phase + k as f32 / OUTPUT_PACKETS as f32).fract();
                let mut d = (pos - p).abs();
                if d > 0.5 {
                    d = 1.0 - d; // wrap the ring
                }
                let g = gaussian(d, PACKET_SIGMA);
                if g > best {
                    best = g;
                }
            }
            if best <= 0.04 {
                return None; // between packets: leave the border its base colour
            }
            // Green intensity: brighter with both the bump and the pane's activity.
            let i = best * (0.30 + 0.70 * activity); // dim when idle, vivid when busy
            let r = (0x14 as f32 + 0x2c as f32 * i) as u8;
            let g = (0x30 as f32 + 0xc0 as f32 * i) as u8;
            let b = (0x1c as f32 + 0x2c as f32 * i) as u8;
            Some(cur.fg(Color::Rgb(r, g, b)))
        });
    }

    /// Overlay a green flow along a routed wire's `path` (already drawn as dim
    /// base glyphs), scaled by the wire's live throughput. Packets travel
    /// source→destination: an idle wire stays dim, a busy one shows bright green
    /// packets marching across — the literal bytes on the pipe.
    fn draw_wire_flow(buf: &mut Buffer, path: &[(u16, u16)], phase: f32, rate: f32, hue: (u8, u8, u8)) {
        if path.len() < 2 {
            return;
        }
        let activity = (rate / WIRE_BUSY_BYTES).clamp(0.0, 1.0);
        let last = (path.len() - 1) as f32;
        let (bw, bh) = (buf.area.width, buf.area.height);
        for (j, &(x, y)) in path.iter().enumerate() {
            if x >= bw || y >= bh {
                continue; // never index outside the buffer
            }
            let pos = j as f32 / last; // 0 at the source .. 1 at the destination
            // Nearest of the marching packets (linear, not wrapped — a wire is a
            // line with a direction, not a loop).
            let mut best = 0.0f32;
            for k in 0..WIRE_PACKETS {
                let pp = (phase + k as f32 / WIRE_PACKETS as f32).fract();
                let g = gaussian(pos - pp, WIRE_SIGMA);
                if g > best {
                    best = g;
                }
            }
            let i = best * (0.15 + 0.85 * activity);
            if i <= 0.06 {
                continue; // leave the dim base wire colour between packets
            }
            // Interpolate the stream hue from a dim base up to full brightness.
            let mix = |c: u8| ((c as f32) * (0.28 + 0.72 * i)) as u8;
            let cell = buf.get_mut(x, y);
            cell.style = cell.style.fg(Color::Rgb(mix(hue.0), mix(hue.1), mix(hue.2))); // recolour, keep glyph
        }
    }

    /// Draw a straight dotted rubber-band line from a wire's source jack to the
    /// mouse cursor while a drag-to-wire is in progress.
    fn draw_rubber_band(buf: &mut Buffer, from: (u16, u16), to: (u16, u16), hue: (u8, u8, u8)) {
        let (x0, y0) = (from.0 as i32, from.1 as i32);
        let (x1, y1) = (to.0 as i32, to.1 as i32);
        let steps = (x1 - x0).abs().max((y1 - y0).abs()).max(1);
        let (bw, bh) = (buf.area.width as i32, buf.area.height as i32);
        for s in 0..=steps {
            let t = s as f32 / steps as f32;
            let x = (x0 as f32 + (x1 - x0) as f32 * t).round() as i32;
            let y = (y0 as f32 + (y1 - y0) as f32 * t).round() as i32;
            if x < 0 || y < 0 || x >= bw || y >= bh {
                continue;
            }
            buf.set_char(x as u16, y as u16, '·', Style::default().fg(Color::Rgb(hue.0, hue.1, hue.2)));
        }
    }

    /// Paint one frame: draw the floating pane boxes and routed wires, then blit
    /// snapshot into its content rectangle, overlay titles, and mark the focused
    /// pane's cursor. Finally draw a one-line command hint at the very bottom.
    fn render(&mut self, buf: &mut Buffer) {
        let full = buf.area; // the whole screen this frame
        // Reserve the bottom row for a status line; the window frame is everything
        // above it, and it belongs to the latency instrument.
        let outer = Rect::new(full.x, full.y, full.width, full.height.saturating_sub(1));
        if outer.width >= 4 && outer.height >= 4 {
            self.draw_latency_frame(buf, outer); // the breathing violet window frame
        }
        // Panes live one cell inside the frame — a moat that keeps the per-pane
        // green rings from ever touching the violet latency ring.
        let tiling = Rect::new(
            outer.x + 1,
            outer.y + 1,
            outer.width.saturating_sub(2),
            outer.height.saturating_sub(2),
        );
        self.area = tiling; // remember for next frame's directional focus

        let focus = self.tree.focus();
        // Each solved tile inset by one cell → a floating pane box, leaving gutters
        // between panes for wires to route through (the wires.rs aesthetic).
        let tiles = solve(self.tree.effective_root_mut(), tiling);
        let boxes: Vec<(TileId, Rect)> = tiles
            .iter()
            .filter_map(|&(id, t)| {
                let b = inset(t, 1);
                (b.width >= 3 && b.height >= 3).then_some((id, b))
            })
            .collect();
        let obstacles: Vec<Rect> = boxes.iter().map(|&(_, r)| r).collect();

        // ── Wires first (drawn under the panes) ─────────────────────────────
        if !self.wires.is_empty() {
            // Cells not covered by any pane box are routable.
            let free: HashSet<(u16, u16)> =
                free_cells_in_window(tiling, &obstacles, 0, tiling).into_iter().collect();
            let box_of = |id: TileId| boxes.iter().find(|&&(i, _)| i == id).map(|&(_, r)| r);
            // One routing request per wire; keep which wire each maps to.
            let mut reqs = Vec::new();
            let mut active: Vec<usize> = Vec::new();
            for (wi, w) in self.wires.iter().enumerate() {
                let (Some(sb), Some(db)) = (box_of(w.src), box_of(w.dst)) else { continue };
                let (so, di) = (out_socket(sb.height, w.stream), in_socket(db.height));
                let (Some(start), Some(goal)) = (so.attach(sb), di.attach(db)) else { continue };
                reqs.push(RouteRequest::new(start, goal, so.outward().opposite(), di.outward().opposite()));
                active.push(wi);
            }
            // Route them together (nudges parallels apart, biases away crossings).
            let conns = route_all(&free, &reqs, 4, 8);
            // Base wire glyphs, dim-tinted by each wire's stream (green/red).
            let connectors: Vec<Connector> = conns.iter().flatten().cloned().collect();
            let base: Vec<Style> = conns
                .iter()
                .enumerate()
                .filter_map(|(j, c)| {
                    c.as_ref().map(|_| {
                        let (r, g, b) = stream_hue(self.wires[active[j]].stream);
                        Style::default().fg(Color::Rgb(r / 4, g / 4, b / 4)) // dim base
                    })
                })
                .collect();
            let canvas = Rect::new(0, 0, full.width, full.height);
            render_connectors(buf, canvas, (0, 0), &connectors, &base, &obstacles, LineWeight::Light);
            // Flow along each routed wire (stream-coloured), scaled by throughput.
            for (j, c) in conns.iter().enumerate() {
                if let Some(conn) = c {
                    let w = &self.wires[active[j]];
                    Self::draw_wire_flow(buf, &conn.path, w.phase, w.rate, stream_hue(w.stream));
                }
            }
        }

        // ── Panes on top ────────────────────────────────────────────────────
        for &(id, bx) in &boxes {
            let focused = Some(id) == focus;
            // The border base colour is the pane's blackbody heat (CPU load);
            // the green output flow rides over it, so one ring shows both at once.
            let load = self.heat.get(&id).copied().unwrap_or(0.0);
            let bstyle = BorderStyle {
                weight: if focused { LineWeight::Heavy } else { LineWeight::Light },
                corners: CornerStyle::Rounded,
                style: Style::default().fg(heat_color(load)),
            };
            draw_box(buf, bx, Borders::ALL, &bstyle);
            self.draw_output_flow(buf, id, bx); // green output-activity ring
            // Jacks: stdin (left, grey), stdout (right upper, green), stderr
            // (right lower, red). A jack is "connected" (filled ●) when a wire
            // uses it.
            let has_in = self.wires.iter().any(|w| w.dst == id);
            let has_out = self.wires.iter().any(|w| w.src == id && w.stream == Stream::Stdout);
            let has_err = self.wires.iter().any(|w| w.src == id && w.stream == Stream::Stderr);
            let (og, og2, ob) = stream_hue(Stream::Stdout);
            let (eg, eg2, eb) = stream_hue(Stream::Stderr);
            draw_socket(buf, bx, &in_socket(bx.height), has_in, Style::default().fg(Color::Rgb(0x88, 0x88, 0x98)));
            draw_socket(buf, bx, &stdout_socket(bx.height), has_out, Style::default().fg(Color::Rgb(og, og2, ob)));
            draw_socket(buf, bx, &stderr_socket(bx.height), has_err, Style::default().fg(Color::Rgb(eg, eg2, eb)));

            // Blit the pane's grid into the box interior.
            let cw = bx.width.saturating_sub(2);
            let ch = bx.height.saturating_sub(2);
            let (cx0, cy0) = (bx.x + 1, bx.y + 1);
            if let Some(pane) = self.panes.get_mut(&id) {
                pane.resize((cw as usize).max(1), (ch as usize).max(1));
                let snap = pane.snapshot();
                // Search-match highlight cells for this pane (current hit brighter).
                let mut hl: HashSet<(usize, usize)> = HashSet::new();
                let mut hl_cur: HashSet<(usize, usize)> = HashSet::new();
                if self.search_open && self.search_pane == Some(id) {
                    let offset = pane.scroll_info().0 as i32; // lines scrolled up
                    for (mi, m) in self.search_matches.iter().enumerate() {
                        let row = m.line + offset; // absolute line → snapshot row
                        if row < 0 {
                            continue;
                        }
                        let row = row as usize;
                        let set = if mi == self.search_index { &mut hl_cur } else { &mut hl };
                        for c in m.col..m.col + m.len {
                            set.insert((row, c));
                        }
                    }
                }
                for (ry, row) in snap.rows.iter().enumerate() {
                    let y = cy0 + ry as u16;
                    if y >= cy0 + ch {
                        break;
                    }
                    for (rx, cell) in row.iter().enumerate() {
                        let x = cx0 + rx as u16;
                        if x >= cx0 + cw {
                            break;
                        }
                        // A search hit repaints the cell background (current hit
                        // brighter amber, others dim).
                        let st = if hl_cur.contains(&(ry, rx)) {
                            style_of(cell).bg(Color::Rgb(0xbb, 0x99, 0x22))
                        } else if hl.contains(&(ry, rx)) {
                            style_of(cell).bg(Color::Rgb(0x5a, 0x4a, 0x10))
                        } else {
                            style_of(cell)
                        };
                        buf.set_char(x, y, cell.c, st);
                    }
                }
                // Cursor of the focused pane (reverse-video block).
                if focused {
                    if let Some(cur) = snap.cursor {
                        let ccx = cx0 + cur.col as u16;
                        let ccy = cy0 + cur.line as u16;
                        if ccx < cx0 + cw && ccy < cy0 + ch {
                            let cell = buf.get_mut(ccx, ccy);
                            cell.style = cell.style.add_modifier(Modifier::REVERSE);
                        }
                    }
                }
            }
            // Title on the top border.
            let title = self
                .titles
                .get(&id)
                .map(String::as_str)
                .filter(|t| !t.is_empty())
                .unwrap_or("shell");
            let label = format!(" {title} ");
            let maxw = bx.width.saturating_sub(4) as usize;
            let label: String = label.chars().take(maxw).collect();
            let tstyle = if focused {
                Style::default().fg(Color::Rgb(0xe6, 0xe6, 0xf0)).add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Rgb(0x9a, 0x9a, 0xaa))
            };
            buf.set_string(bx.x + 2, bx.y, &label, tstyle);
        }

        // Remember the boxes for the next frame's mouse hit-testing.
        self.last_boxes = boxes;

        // Rubber-band the in-progress mouse wire from its source jack to the cursor.
        if let (Some((src, stream)), Some(cur)) = (self.wiring_from, self.drag_cursor) {
            let anchor = self
                .last_boxes
                .iter()
                .find(|&&(i, _)| i == src)
                .and_then(|&(_, sb)| out_socket(sb.height, stream).anchor(sb));
            if let Some(a) = anchor {
                Self::draw_rubber_band(buf, a, cur, stream_hue(stream));
            }
        }

        // Bottom status line: search bar, wiring prompt, or command cheatsheet.
        let owned;
        let (hint, status) = if self.search_open {
            let pos = if self.search_matches.is_empty() { 0 } else { self.search_index + 1 };
            owned = format!(
                " /{}   {}/{}   Enter/↓ next · ↑ prev · Esc close ",
                self.search_query,
                pos,
                self.search_matches.len()
            );
            (owned.as_str(), Style::default().fg(Color::Rgb(0x22, 0x22, 0x14)).bg(Color::Rgb(0xcc, 0xb0, 0x40)))
        } else {
            match self.wiring_from {
                Some((src, stream)) => {
                    let s = match stream {
                        Stream::Stdout => "stdout",
                        Stream::Stderr => "stderr",
                    };
                    owned = format!(
                        " WIRING {s} from shell {src} → click a pane (or focus it + C-a w/e) to connect · Esc/right-click cancels "
                    );
                    (owned.as_str(), Style::default().fg(Color::Rgb(0x20, 0x2a, 0x20)).bg(Color::Rgb(0x60, 0xc0, 0x60)))
                }
                None => (
                    " C-a  %/\" split · |pipe · o/←→ focus · z zoom · w/e wire · u unwire · / search · x close · q quit ",
                    Style::default().fg(Color::Rgb(0xc8, 0xc8, 0xd4)).bg(Color::Rgb(0x22, 0x22, 0x2c)),
                ),
            }
        };
        // Pad the row so the background spans the full width.
        let mut line = String::from(hint);
        while (line.chars().count() as u16) < full.width {
            line.push(' ');
        }
        let line: String = line.chars().take(full.width as usize).collect();
        buf.set_string(full.x, full.bottom() - 1, &line, status);

        // The manual overlay is drawn last, over everything.
        if self.manual_open {
            let area = buf.area;
            let bg = Style::default().fg(Color::Rgb(0xd2, 0xd2, 0xdc)).bg(Color::Rgb(0x12, 0x14, 0x1c));
            // Fill the panel background.
            let blank: String = " ".repeat(area.width as usize);
            for y in area.y..area.bottom() {
                buf.set_string(area.x, y, &blank, bg);
            }
            draw_box(
                buf,
                area,
                Borders::ALL,
                &BorderStyle {
                    weight: LineWeight::Light,
                    corners: CornerStyle::Rounded,
                    style: Style::default().fg(Color::Rgb(0x5c, 0x80, 0xb4)),
                },
            );
            buf.set_string(
                area.x + 2,
                area.y,
                " rt-mux — manual ",
                Style::default().fg(Color::Rgb(0xff, 0xff, 0xff)).add_modifier(Modifier::BOLD),
            );
            let lines: Vec<&str> = MANUAL.lines().collect();
            let inner_h = area.height.saturating_sub(2) as usize;
            let inner_w = area.width.saturating_sub(4) as usize;
            let max_scroll = lines.len().saturating_sub(inner_h);
            self.manual_scroll = self.manual_scroll.min(max_scroll); // clamp
            for (i, line) in lines.iter().skip(self.manual_scroll).take(inner_h).enumerate() {
                let y = area.y + 1 + i as u16;
                let text: String = line.chars().take(inner_w).collect();
                // Section headings (start at column 0 with a capital) get an accent.
                let is_heading = !line.starts_with(' ')
                    && line.chars().next().is_some_and(|c| c.is_ascii_uppercase());
                let style = if is_heading {
                    Style::default()
                        .fg(Color::Rgb(0x8a, 0xd0, 0xff))
                        .bg(Color::Rgb(0x12, 0x14, 0x1c))
                        .add_modifier(Modifier::BOLD)
                } else {
                    bg
                };
                buf.set_string(area.x + 2, y, &text, style);
            }
        }
    }
}

/// Planck/blackbody colour for a CPU load: an idle process glows a dim deep red,
/// a busy one runs up through orange and yellow to white-hot, and a pathological
/// one goes blue-white — load *is* temperature. `load` is fraction of one core.
fn heat_color(load: f32) -> Color {
    let n = (load / 1.5).clamp(0.0, 1.0); // normalise (≥1.5 cores = max heat)
    let s = smoothstep(n); // ease the ramp
    let kelvin = 1200.0 + s * 9000.0; // 1200K (dim red) .. 10200K (blue-white)
    let (r, g, b) = blackbody(kelvin);
    let bright = 0.28 + 0.72 * n; // idle is dim; busy is vivid
    Color::Rgb((r * bright) as u8, (g * bright) as u8, (b * bright) as u8)
}

/// Approximate blackbody RGB (0..255) for a colour temperature in kelvin
/// (Tanner Helland's fit). Red/green/blue each follow the Planck locus.
fn blackbody(kelvin: f32) -> (f32, f32, f32) {
    let t = (kelvin / 100.0).clamp(10.0, 400.0);
    let r = if t <= 66.0 {
        255.0
    } else {
        (329.698_73 * (t - 60.0).powf(-0.133_204_76)).clamp(0.0, 255.0)
    };
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

/// Shrink a rect by `n` cells on every side (saturating).
fn inset(r: Rect, n: u16) -> Rect {
    Rect::new(r.x + n, r.y + n, r.width.saturating_sub(2 * n), r.height.saturating_sub(2 * n))
}

/// The stdout jack: upper-third of the box's right edge.
fn stdout_socket(h: u16) -> Socket {
    Socket::new(Side::Right, (h / 3).max(1), Flow::Out, 0)
}

/// The stderr jack: lower-third of the box's right edge.
fn stderr_socket(h: u16) -> Socket {
    Socket::new(Side::Right, (2 * h / 3).max(2), Flow::Out, 1)
}

/// The output socket for a given stream.
fn out_socket(h: u16, stream: Stream) -> Socket {
    match stream {
        Stream::Stdout => stdout_socket(h),
        Stream::Stderr => stderr_socket(h),
    }
}

/// The input (stdin) jack: mid-height on the box's left edge.
fn in_socket(h: u16) -> Socket {
    Socket::new(Side::Left, h / 2, Flow::In, 0)
}

/// The display colour of a stream: stdout green, stderr red.
fn stream_hue(stream: Stream) -> (u8, u8, u8) {
    match stream {
        Stream::Stdout => (0x40, 0xc0, 0x54),
        Stream::Stderr => (0xd0, 0x54, 0x30),
    }
}

/// Translate one engine cell (`char` + `Rgb` fg/bg + attribute flags) into a
/// mullion [`Style`]. This is the entire colour/attribute half of the seam.
fn style_of(cell: &rt_engine::SnapCell) -> Style {
    // Opaque fg/bg for every cell — a terminal has no transparency in text mode.
    let mut style = Style::default()
        .fg(Color::Rgb(cell.fg[0], cell.fg[1], cell.fg[2]))
        .bg(Color::Rgb(cell.bg[0], cell.bg[1], cell.bg[2]));
    // Map the drawing attributes mullion also understands (strikeout has no
    // mullion Modifier, so it is dropped — acceptable for a text-mode host).
    if cell.attrs.bold {
        style = style.add_modifier(Modifier::BOLD);
    }
    if cell.attrs.italic {
        style = style.add_modifier(Modifier::ITALIC);
    }
    if cell.attrs.underline {
        style = style.add_modifier(Modifier::UNDERLINE);
    }
    style
}

/// Replace the leaf `Node::Tile(target)` with a two-child split (the old tile
/// plus `new_id`), searching the tree depth-first. Returns whether it was found.
fn split_tile(node: &mut Node, target: TileId, new_id: TileId, orient: Orientation) -> bool {
    match node {
        Node::Tile(id) if *id == target => {
            let old = *id; // keep the existing pane as the first child
            *node = Node::Split {
                orientation: orient,
                children: vec![
                    (Constraint::default(), Node::Tile(old)),   // equal weights (Fill(1))
                    (Constraint::default(), Node::Tile(new_id)),
                ],
            };
            true
        }
        Node::Tile(_) => false, // a different leaf
        Node::Split { children, .. } => {
            for (_, child) in children.iter_mut() {
                // Clone per branch: `Orientation` is not `Copy` (its Adaptive
                // variant carries state), and only the matching leaf consumes it.
                if split_tile(child, target, new_id, orient.clone()) {
                    return true;
                }
            }
            false
        }
        Node::Carousel { children, .. } => {
            for (_, child) in children.iter_mut() {
                if split_tile(child, target, new_id, orient.clone()) {
                    return true;
                }
            }
            false
        }
    }
}

/// Remove the leaf `target` from the tree, collapsing any split that is left
/// with a single child back into that child (so no redundant 1-way splits
/// linger). Returns `true` if `node` itself became empty and should be removed
/// by its parent. Mirrors rt-core's `close`, but on mullion's `Node`.
fn remove_tile(node: &mut Node, target: TileId) -> bool {
    match node {
        Node::Tile(id) => *id == target, // ask the parent to drop me if I'm the target
        Node::Split { children, .. } => {
            // Drop any child that reports itself empty (the target, or a split
            // that emptied out beneath it).
            let mut i = 0;
            while i < children.len() {
                if remove_tile(&mut children[i].1, target) {
                    children.remove(i);
                } else {
                    i += 1;
                }
            }
            if children.is_empty() {
                return true; // this split has no children left
            }
            if children.len() == 1 {
                // Collapse: this split now just wraps one child — become it.
                let (_, only) = children.remove(0);
                *node = only;
            }
            false
        }
        Node::Carousel { children, .. } => {
            let mut i = 0;
            while i < children.len() {
                if remove_tile(&mut children[i].1, target) {
                    children.remove(i);
                } else {
                    i += 1;
                }
            }
            children.is_empty()
        }
    }
}

/// Encode a crossterm key event as the byte sequence a PTY expects, or `None`
/// for keys we do not forward. `app_cursor` picks SS3 (`ESC O x`) over CSI
/// (`ESC [ x`) for the arrow/Home/End keys, as DECCKM requires.
fn encode_key(key: &KeyEvent, app_cursor: bool) -> Option<Vec<u8>> {
    use KeyCode::*;
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);
    // Cursor-ish keys: SS3 when the app enabled application cursor keys, else CSI.
    let cursor = |c: u8| -> Vec<u8> {
        if app_cursor {
            vec![0x1b, b'O', c]
        } else {
            vec![0x1b, b'[', c]
        }
    };
    // CSI ... ~ sequences (PageUp, Delete, function keys…).
    let tilde = |n: &[u8]| -> Vec<u8> {
        let mut v = vec![0x1b, b'[']; // ESC [
        v.extend_from_slice(n);
        v.push(b'~');
        v
    };

    let mut out = match key.code {
        Char(c) => {
            if ctrl {
                // Control combos → C0 control bytes; unknown combos send the char.
                match ctrl_byte(c) {
                    Some(b) => vec![b],
                    None => c.to_string().into_bytes(),
                }
            } else {
                c.to_string().into_bytes() // plain (shift already folded into `c`)
            }
        }
        Enter => vec![b'\r'],
        Tab => vec![b'\t'],
        BackTab => vec![0x1b, b'[', b'Z'],
        Backspace => vec![0x7f],
        Esc => vec![0x1b],
        Left => cursor(b'D'),
        Right => cursor(b'C'),
        Up => cursor(b'A'),
        Down => cursor(b'B'),
        Home => cursor(b'H'),
        End => cursor(b'F'),
        PageUp => tilde(b"5"),
        PageDown => tilde(b"6"),
        Insert => tilde(b"2"),
        Delete => tilde(b"3"),
        F(n) => match n {
            1 => vec![0x1b, b'O', b'P'],
            2 => vec![0x1b, b'O', b'Q'],
            3 => vec![0x1b, b'O', b'R'],
            4 => vec![0x1b, b'O', b'S'],
            5 => tilde(b"15"),
            6 => tilde(b"17"),
            7 => tilde(b"18"),
            8 => tilde(b"19"),
            9 => tilde(b"20"),
            10 => tilde(b"21"),
            11 => tilde(b"23"),
            12 => tilde(b"24"),
            _ => return None,
        },
        _ => return None, // keys we don't forward (media keys, etc.)
    };
    // Alt/Meta on a printable char → prefix with ESC (the classic meta encoding).
    if alt && matches!(key.code, Char(_)) {
        let mut v = vec![0x1b];
        v.append(&mut out);
        out = v;
    }
    Some(out)
}

/// Re-encode a crossterm mouse event as an xterm mouse report for the pane's
/// app, at 1-based pane-local `(col, row)`. Uses SGR (`ESC [ < … M/m`) when the
/// app requested it, else the legacy X10 byte form. Bare motion is not forwarded.
fn encode_mouse(m: &MouseEvent, col: u16, row: u16, sgr: bool) -> Option<Vec<u8>> {
    // Button/action base code, and whether this is a release.
    let (mut cb, release) = match m.kind {
        MouseEventKind::Down(b) => (button_code(b), false),
        MouseEventKind::Up(b) => (button_code(b), true),
        MouseEventKind::Drag(b) => (button_code(b) + 32, false), // motion flag
        MouseEventKind::ScrollUp => (64, false),
        MouseEventKind::ScrollDown => (65, false),
        MouseEventKind::ScrollLeft => (66, false),
        MouseEventKind::ScrollRight => (67, false),
        MouseEventKind::Moved => return None, // don't spam bare motion
    };
    // Fold in keyboard modifiers (the xterm bit positions).
    if m.modifiers.contains(KeyModifiers::SHIFT) {
        cb += 4;
    }
    if m.modifiers.contains(KeyModifiers::ALT) {
        cb += 8;
    }
    if m.modifiers.contains(KeyModifiers::CONTROL) {
        cb += 16;
    }
    if sgr {
        // SGR: press/scroll end in 'M', release in 'm'; coordinates are decimal.
        let end = if release { 'm' } else { 'M' };
        Some(format!("\x1b[<{cb};{col};{row}{end}").into_bytes())
    } else {
        // Legacy X10: ESC [ M then three bytes offset by 32; release is button 3.
        let b = if release { 3 } else { cb };
        let cx = (col.saturating_add(32)).min(255) as u8;
        let cy = (row.saturating_add(32)).min(255) as u8;
        Some(vec![0x1b, b'[', b'M', (b + 32).min(255) as u8, cx, cy])
    }
}

/// The xterm button code for a mouse button (before the motion/modifier bits).
fn button_code(b: MouseButton) -> u16 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

/// The C0 control byte for `Ctrl`+`c`, or `None` if the combination has no
/// standard control code (in which case the caller sends the plain char).
fn ctrl_byte(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - b'a' + 1),   // Ctrl-a..z → 0x01..0x1a
        'A'..='Z' => Some(c as u8 - b'A' + 1),   // Ctrl-A..Z → same
        ' ' | '@' => Some(0x00),                 // Ctrl-Space / Ctrl-@ → NUL
        '[' => Some(0x1b),                       // Ctrl-[ → ESC
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '/' => Some(0x1f),
        '?' => Some(0x7f),                       // Ctrl-? → DEL
        _ => None,
    }
}

/// The render + input loop. Sets up the mux from the initial terminal size, then
/// each frame: drain input → drain pane events → repaint → pace the frame.
fn run(term: &mut Terminal<CrosstermBackend<Stdout>>) -> io::Result<()> {
    let area = term.backend().size()?; // initial screen rectangle
    let mut mux = Mux::new(area)?;
    // Debug/demo hook: build a small live scene so the whole feature set is
    // visible in one shot (floating panes, all three instruments, a live wire).
    if std::env::var("RT_MUX_DEMO").is_ok() {
        mux.split(Orientation::Horizontal); // → pane 2 (right)
        mux.split(Orientation::Vertical); // → pane 3 (right-bottom)
        mux.connect_wire(1, Stream::Stdout, 2); // wire pane1.stdout → pane2.stdin
        // pane 1: steady output (green flow) that also feeds the wire via tee.
        if let Some(p) = mux.panes.get(&1) {
            p.write(b"while true; do echo tick $(date +%T); sleep 0.6; done | tee $RT_OUT\n");
        }
        // pane 2: read the wire.
        if let Some(p) = mux.panes.get(&2) {
            p.write(b"cat $RT_IN\n");
        }
        // pane 3: burn a core (heat → white-hot border), no output.
        if let Some(p) = mux.panes.get(&3) {
            p.write(b"yes >/dev/null\n");
        }
    }
    let input = EventReader::new(); // background input thread (never blocked by a slow draw)

    // Idle cadence: slow enough to stay cheap, fast enough for the latency frame
    // to visibly breathe at rest (~8 fps).
    let idle_budget = Duration::from_millis(120);
    loop {
        let start = Instant::now();
        // Handle every queued input event this frame (a burst collapses into one frame).
        let mut interacted = false;
        for ev in input.drain() {
            interacted = true; // any input → keep the frame lively (e.g. rubber-band)
            match ev {
                Event::Key(k) => {
                    if !mux.handle_key(k) {
                        return Ok(()); // a command asked to quit
                    }
                }
                Event::Mouse(m) => mux.handle_mouse(m),
                _ => {}
            }
        }
        // Titles / child exits. If every shell exited, we're done.
        if mux.poll_panes() {
            return Ok(());
        }
        // Move bytes across the patch-bay wires this iteration.
        let piped = mux.pump();
        // Refresh the CPU-heat readings (self-throttled to ~2 Hz).
        mux.sample_heat();
        // Advance the instruments by real wall-clock time (framerate-independent),
        // and register any deadline overrun as a latency spike.
        let now = Instant::now();
        let dt = now.duration_since(mux.last_frame).as_secs_f32().min(0.5); // clamp a long stall
        mux.last_frame = now;
        mux.note_latency(dt, mux.last_budget.as_secs_f32());
        mux.tick(dt);

        // Always repaint (the frame breathes even at rest); mullion diffs and
        // flushes only the changed cells, so a static screen costs almost nothing
        // downstream. A busy pane paces at 60 fps, an idle mux at ~8 fps.
        term.draw(|buf| mux.render(buf))?;

        let flowing = mux.wires.iter().any(|w| w.rate > 1.0);
        let busy = interacted || piped || flowing || mux.meters.values().any(|m| m.wakeups > 0 || m.rate > 0.5);
        let budget = if busy { FRAME } else { idle_budget };
        mux.last_budget = budget; // so next frame's overrun is measured correctly
        std::thread::sleep(budget.saturating_sub(start.elapsed()));
    }
}

/// Set up the terminal, run the loop, and always restore the terminal on the way
/// out (even if the loop returns an error).
fn main() -> io::Result<()> {
    let mut backend = CrosstermBackend::new(io::stdout());
    backend.apply_capabilities(&Capabilities::detect()); // truecolor/unicode probe
    backend.set_mouse_capture(true); // rt-mux uses the mouse for focus + drag-to-wire
    let mut term = Terminal::new(backend)?;
    term.enter()?; // raw mode, alternate screen, hidden cursor

    let result = run(&mut term); // the app; errors are returned, not propagated past leave()

    term.leave()?; // restore the terminal no matter what
    result
}
