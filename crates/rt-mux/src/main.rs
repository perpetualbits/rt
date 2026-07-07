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

use std::collections::HashMap; // tile id -> pane, and tile id -> title
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
/// Speed of the latency frame's calm undulation, in laps/second (slow breath).
const LAT_SPEED: f32 = 0.12;
/// Time constant for a latency spike to fade back to calm.
const STALL_TAU: f32 = 0.6;
/// Frame overrun (seconds) beyond the intended budget that reads as a full-height
/// spike. A ~50ms hitch pins the spike; smaller hitches scale down.
const MISS_FULL: f32 = 0.05;
/// Slop below which an overrun is just normal scheduling jitter, not a miss.
const MISS_SLOP: f32 = 0.010;

use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers}; // input events (mullion's crossterm)

use mullion::backend::{Backend, CrosstermBackend}; // Backend trait (size) + the real terminal backend
use mullion::border::{draw_box, render_rim, render_shared, Borders, BorderStyle, CornerStyle, LineWeight}; // borders + rim animation
use mullion::buffer::Buffer; // the cell buffer we paint into
use mullion::capabilities::Capabilities; // terminal feature probe (colour depth, unicode…)
use mullion::ease::gaussian; // bump shape for the flowing packets
use mullion::geometry::Rect; // tile rectangles
use mullion::layout::{solve, Constraint, Node, Orientation, TileId}; // the layout tree + solver
use mullion::style::{Color, Modifier, Style}; // cell colours + attributes
use mullion::terminal::{EventReader, Terminal}; // double-buffer driver + threaded input
use mullion::tree::{focus_override, Direction, Tree}; // focus/zoom wrapper + focus-thickening

use rt_engine::{PaneEvent, TermPane}; // the terminal engine (PTY + alacritty grid)

/// The prefix key that introduces a multiplexer command, tmux-style. We use
/// `Ctrl-a`; pressing it twice sends a literal `Ctrl-a` to the focused shell.
const PREFIX: char = 'a';

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
    out_path: PathBuf, // $RT_OUT — program writes, rt-mux reads
    in_path: PathBuf,  // $RT_IN  — rt-mux writes, program reads
    out_read: File,    // rt-mux's persistent handle on out_path (reads program output)
    in_write: File,    // rt-mux's persistent handle on in_path (feeds the program)
}

impl Jacks {
    /// Create the two fifos for pane `id` under `dir` and open rt-mux's ends.
    fn new(dir: &Path, id: TileId) -> io::Result<Jacks> {
        let out_path = dir.join(format!("{id}.out"));
        let in_path = dir.join(format!("{id}.in"));
        mkfifo(&out_path)?;
        mkfifo(&in_path)?;
        // O_RDWR keeps a peer present so the fifo never hits EOF and the program's
        // open/write never blocks; O_NONBLOCK keeps rt-mux's own I/O async.
        let open = |p: &Path| {
            OpenOptions::new().read(true).write(true).custom_flags(libc::O_NONBLOCK).open(p)
        };
        let out_read = open(&out_path)?;
        let in_write = open(&in_path)?;
        Ok(Jacks { out_path, in_path, out_read, in_write })
    }

    /// The environment variables advertising these jacks to the pane's shell.
    fn env(&self) -> Vec<(String, String)> {
        vec![
            ("RT_OUT".to_string(), self.out_path.to_string_lossy().into_owned()),
            ("RT_IN".to_string(), self.in_path.to_string_lossy().into_owned()),
        ]
    }
}

impl Drop for Jacks {
    /// Remove the fifos when the pane closes (the open handles drop with us).
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.out_path);
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

/// One live patch-bay connection: bytes read from `src`'s output jack are written
/// to `dst`'s input jack. Throughput drives the wire's flow animation, so the
/// packets you see crossing the wire are the literal bytes on the pipe.
struct Wire {
    src: TileId, // read from src's $RT_OUT
    dst: TileId, // write to dst's $RT_IN
    rate: f32,   // smoothed bytes/second across the wire
    phase: f32,  // flow position along the wire (laps)
    moved: u32,  // bytes carried since the last tick (reset each tick)
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
    /// When wiring: the source pane whose output jack we're dragging from.
    wiring_from: Option<TileId>,
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
            dir,
            meters: HashMap::new(),
            last_frame: Instant::now(),
            last_budget: FRAME,
            lat_phase: 0.0,
            stall: 0.0,
            next_id: 2, // 1 is taken by the first pane
            prefix_armed: false,
            area,
        };
        // Size the first pane to the content area (screen minus the 1-cell frame).
        let cols = area.width.saturating_sub(2).max(1) as usize;
        let rows = area.height.saturating_sub(2).max(1) as usize;
        let env = mux.make_jacks(1); // its $RT_OUT / $RT_IN
        let pane = TermPane::spawn_env(None, None, cols, rows, &env)?; // None = default login shell
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
        match TermPane::spawn_env(None, None, 80, 24, &env) {
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
        self.jacks.remove(&id); // Drop -> remove its fifos
        self.wires.retain(|w| w.src != id && w.dst != id); // unplug any wires on it
        if self.wiring_from == Some(id) {
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
                // Wire: first 'w' arms from the focused pane's output jack; a
                // second 'w' (after moving focus to the target) completes the
                // wire src.out -> dst.in. Re-arming on the same pane cancels.
                KeyCode::Char('w') => {
                    let focus = self.tree.focus();
                    match self.wiring_from.take() {
                        None => self.wiring_from = focus, // start dragging from here
                        Some(src) => {
                            if let Some(dst) = focus {
                                if src != dst
                                    && self.jacks.contains_key(&src)
                                    && self.jacks.contains_key(&dst)
                                {
                                    self.wires.push(Wire { src, dst, rate: 0.0, phase: 0.0, moved: 0 });
                                }
                            }
                        }
                    }
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

        self.forward_key(key); // ordinary key → the focused shell
        true
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
        let mut buf = [0u8; 8192];
        let srcs: Vec<TileId> = self.jacks.keys().copied().collect();
        for src in srcs {
            loop {
                // Read one chunk from this pane's output jack (non-blocking).
                let n = match self.jacks.get_mut(&src) {
                    Some(j) => match j.out_read.read(&mut buf) {
                        Ok(0) => break, // (O_RDWR means no EOF; treat as "nothing")
                        Ok(n) => n,
                        Err(ref e) if e.kind() == io::ErrorKind::WouldBlock => break,
                        Err(_) => break,
                    },
                    None => break,
                };
                moved = true;
                // Fan the chunk out to every wire leaving this pane. (Own the bytes
                // so the src-jack borrow above is released before we touch dsts.)
                let chunk = buf[..n].to_vec();
                for w in self.wires.iter_mut().filter(|w| w.src == src) {
                    if let Some(dj) = self.jacks.get_mut(&w.dst) {
                        let _ = dj.in_write.write_all(&chunk); // best-effort; drops if the reader is behind
                        w.moved = w.moved.saturating_add(n as u32);
                    }
                }
            }
        }
        moved
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

    /// Paint one frame: draw the shared tile borders, then blit every pane's
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

        // A calm rounded frame; the focused tile is thickened via `focus_override`.
        let border_style = BorderStyle {
            weight: LineWeight::Light,
            corners: CornerStyle::Rounded,
            style: Style::default().fg(Color::Rgb(0x50, 0x50, 0x64)),
        };
        let focus = self.tree.focus();
        let overrides = focus_override(&self.tree, LineWeight::Heavy); // thicken the focused border
        // Full tile rectangles (border included) for the instrument rings, keyed
        // by id so we can match them to the content rects below.
        let tiles: HashMap<TileId, Rect> =
            solve(self.tree.effective_root_mut(), tiling).into_iter().collect();
        // Draw shared single-cell seams + outer frame, and get each tile's interior.
        let rects = render_shared(buf, self.tree.effective_root_mut(), tiling, &border_style, &overrides);

        for (id, rect) in rects {
            if rect.width == 0 || rect.height == 0 {
                continue; // a tile squeezed to nothing this frame
            }
            // Instrument first: flow the output-activity packets around the border
            // ring (reads `meters`; runs before the pane borrow and before the
            // title so the title draws on top of it).
            if let Some(&tile) = tiles.get(&id) {
                self.draw_output_flow(buf, id, tile);
            }
            let Some(pane) = self.panes.get_mut(&id) else { continue }; // no engine for this tile
            // Size the PTY to its content rectangle (rt-engine resizes the grid
            // synchronously, so the snapshot below already reflects it).
            pane.resize((rect.width as usize).max(1), (rect.height as usize).max(1));
            let snap = pane.snapshot(); // char + fg/bg + attrs grid

            // Blit the grid cell-for-cell into the tile.
            for (r, row) in snap.rows.iter().enumerate() {
                let y = rect.y + r as u16;
                if y >= rect.bottom() {
                    break; // past the tile's bottom
                }
                for (c, cell) in row.iter().enumerate() {
                    let x = rect.x + c as u16;
                    if x >= rect.right() {
                        break; // past the tile's right edge
                    }
                    buf.set_char(x, y, cell.c, style_of(cell)); // the actual cells-into-cells write
                }
            }

            // Focused pane: mark the cursor cell with reverse video (a block cursor).
            if Some(id) == focus {
                if let Some(cur) = snap.cursor {
                    let cx = rect.x + cur.col as u16;
                    let cy = rect.y + cur.line as u16;
                    if cx < rect.right() && cy < rect.bottom() {
                        let cell = buf.get_mut(cx, cy);
                        cell.style = cell.style.add_modifier(Modifier::REVERSE);
                    }
                }
            }

            // Title overlaid on the top border (like tmux/Terminator).
            if rect.y >= 1 {
                let title = self.titles.get(&id).map(String::as_str).unwrap_or("");
                let label = if title.is_empty() {
                    format!(" shell {id} ")
                } else {
                    format!(" {title} ")
                };
                let maxw = rect.width.saturating_sub(2) as usize; // room between the corners
                let label: String = label.chars().take(maxw).collect();
                let tstyle = if Some(id) == focus {
                    Style::default().fg(Color::Rgb(0xe6, 0xe6, 0xf0)).add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Rgb(0x9a, 0x9a, 0xaa))
                };
                buf.set_string(rect.x + 1, rect.y - 1, &label, tstyle); // sits in the top border row
            }
        }

        // Bottom status line: the command cheatsheet, or the wiring prompt.
        let owned;
        let (hint, status) = match self.wiring_from {
            Some(src) => {
                owned = format!(
                    " WIRING from shell {src} → move focus to the target, then C-a w to connect (C-a w here to cancel) "
                );
                (owned.as_str(), Style::default().fg(Color::Rgb(0x20, 0x2a, 0x20)).bg(Color::Rgb(0x60, 0xc0, 0x60)))
            }
            None => (
                " C-a  %/\" split · o/←→ focus · z zoom · r rotate · w wire · x close · q quit ",
                Style::default().fg(Color::Rgb(0xc8, 0xc8, 0xd4)).bg(Color::Rgb(0x22, 0x22, 0x2c)),
            ),
        };
        // Pad the row so the background spans the full width.
        let mut line = String::from(hint);
        while (line.chars().count() as u16) < full.width {
            line.push(' ');
        }
        let line: String = line.chars().take(full.width as usize).collect();
        buf.set_string(full.x, full.bottom() - 1, &line, status);
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
    let input = EventReader::new(); // background input thread (never blocked by a slow draw)

    // Idle cadence: slow enough to stay cheap, fast enough for the latency frame
    // to visibly breathe at rest (~8 fps).
    let idle_budget = Duration::from_millis(120);
    loop {
        let start = Instant::now();
        // Handle every queued input event this frame (a burst collapses into one frame).
        for ev in input.drain() {
            if let Event::Key(k) = ev {
                if !mux.handle_key(k) {
                    return Ok(()); // a command asked to quit
                }
            }
        }
        // Titles / child exits. If every shell exited, we're done.
        if mux.poll_panes() {
            return Ok(());
        }
        // Move bytes across the patch-bay wires this iteration.
        let piped = mux.pump();
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
        let busy = piped || flowing || mux.meters.values().any(|m| m.wakeups > 0 || m.rate > 0.5);
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
    backend.set_mouse_capture(false); // leave native mouse/selection to the outer terminal
    let mut term = Terminal::new(backend)?;
    term.enter()?; // raw mode, alternate screen, hidden cursor

    let result = run(&mut term); // the app; errors are returned, not propagated past leave()

    term.leave()?; // restore the terminal no matter what
    result
}
