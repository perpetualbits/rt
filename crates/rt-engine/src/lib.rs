//! `rt-engine` — one terminal pane's backend, wrapping `alacritty_terminal`.
//!
//! Each visible leaf in the `rt-core` layout tree is backed by exactly one
//! [`AlacPane`] from this crate. A `AlacPane` owns:
//!   * a PTY running the user's shell,
//!   * an `alacritty_terminal::Term` (the fast grid + VTE/ANSI parser),
//!   * the background I/O thread (`EventLoop`) that reads PTY bytes and applies
//!     them to the `Term`,
//!   * a channel to send keystrokes / resizes / shutdown to that thread.
//!
//! The design goal is to expose a *tiny, panic-free* surface to the GUI:
//! create a pane, feed it input bytes, ask for a text snapshot to render, and
//! drain high-level events (title changes, bell, child-exited). Everything that
//! can fail returns `Result`/`Option` — no unwrap on the hot path — which is
//! the direct antidote to Terminator's unguarded-callback crashes.

pub mod budget; // process-wide scrollback memory budget, shared proportionally across panes
mod handoff; // live vt-term cells -> rt-handoff wire runs (pane export)
mod palette; // xterm 256-colour palette + cell-colour resolution
mod vtpane; // in-house (vt-term) pane backend, selected by RT_ENGINE=vtterm
pub use palette::{Palette, Rgb, CURSOR, DEFAULT_BG, DEFAULT_FG}; // colours + configurable palette
// The frozen cross-process wire model `TermPane::export` produces. Re-exported so a later
// phase's caller (rt-session) can name `rt_engine::rt_handoff::pane::PaneWire` etc. without
// adding its own path dependency on rt-handoff.
pub use rt_handoff;
// (CursorShape/CursorPos are defined below and used by the renderer.)

use std::borrow::Cow; // Msg::Input takes a Cow<[u8]>; we always own our bytes
use std::collections::VecDeque; // FIFO queue of high-level events for the GUI to drain
use std::sync::{Arc, Mutex}; // shared, lock-guarded state between us and the I/O thread

use alacritty_terminal::event::{Event as AlacEvent, EventListener, WindowSize};
use alacritty_terminal::event_loop::{EventLoop, EventLoopSender, Msg};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::sync::FairMutex;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::tty::{self, Options as PtyOptions, Shell};

/// Default scrollback lines retained above the screen (matches alacritty's own
/// default). rt's front-end overrides this from the user's Preferences.
pub const DEFAULT_SCROLLBACK: usize = 10_000;

/// Bytes one grid cell occupies (alacritty's `Cell`). The front-end multiplies
/// this by columns × lines to estimate the memory a full scrollback would use,
/// so the Preferences slider can warn before the user picks a size their RAM
/// can't hold. Computed from the real type so it tracks upstream changes.
pub const CELL_BYTES: usize = std::mem::size_of::<alacritty_terminal::term::cell::Cell>();

/// Map a child `ExitStatus` to the status a shell reports in `$?`: the exit code, or
/// `128 + signal` for a signal death (so a segfault reads as 139, like bash). `None` only
/// when neither is available.
pub(crate) fn exit_code(status: std::process::ExitStatus) -> Option<i32> {
    use std::os::unix::process::ExitStatusExt;
    status.code().or_else(|| status.signal().map(|s| 128 + s))
}

/// High-level events a pane can surface to the GUI, distilled from
/// `alacritty_terminal`'s richer event enum down to what rt's UI actually acts
/// on. Draining these (via [`AlacPane::drain_events`]) replaces Terminator's
/// scattered GTK signal handlers with one explicit, race-free queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneEvent {
    /// The program asked to change the window/tab title.
    Title(String),
    /// The terminal bell rang (we render a visible-bell flash, never a
    /// fire-later GTK timeout — see TERMINATOR_BUGS.md #2).
    Bell,
    /// The child process exited. The payload is its status as a shell would report it
    /// (`$?`): the exit code, or `128 + signal` for a signal death, or `None` when the
    /// status is unknown (the pty closed but the child wasn't reaped by us). A clean exit
    /// (`Some(0)` / `None`) closes the pane; a non-zero status keeps it open with a notice.
    Exited(Option<i32>),
    /// New grid content is available; the GUI should schedule a redraw.
    Wakeup,
    /// The pane's parser/grid thread panicked on some input and was caught (panic =
    /// "unwind"): the pane is isolated — frozen at its last state — rather than taking the
    /// whole process down. The GUI keeps it open with a "[crashed]" badge.
    Crashed,
}

/// The text-attribute flags a cell can carry that affect *how* the glyph is
/// drawn (as opposed to colour, which is already baked into `fg`/`bg`). These
/// are the ones the renderer acts on: underline (any style), italic (slanted
/// face), and strikeout (a line through the middle).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct CellAttrs {
    pub bold: bool,      // render with a heavier (bold) face
    pub underline: bool, // draw a line under the glyph
    pub italic: bool,    // render with a slanted/oblique face
    pub strikeout: bool, // draw a line through the glyph
}

/// A single terminal cell: its glyph, already-resolved foreground/background RGB
/// (bold/dim/inverse/hidden are baked into the colours by `snapshot`), and the
/// drawing attributes the renderer still needs (underline/italic/strikeout).
#[derive(Clone, Debug, PartialEq)]
pub struct SnapCell {
    pub c: char,          // the glyph to draw in this cell
    pub fg: Rgb,          // resolved foreground colour
    pub bg: Rgb,          // resolved background colour
    pub attrs: CellAttrs, // underline / italic / strikeout
}

impl SnapCell {
    /// A blank cell (space) in the default colours with no attributes — used to
    /// pre-fill rows.
    fn blank() -> Self {
        SnapCell { c: ' ', fg: DEFAULT_FG, bg: DEFAULT_BG, attrs: CellAttrs::default() }
    }
}

/// The shape the terminal has requested for its cursor (via DECSCUSR). Editors
/// use this to signal e.g. insert (a beam/bar) vs overwrite (a block/underline)
/// mode. `HollowBlock` is what an *unfocused* terminal shows; `Hidden` means the
/// app asked for no cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Block,       // solid block, the usual default
    Underline,   // a bar along the bottom of the cell (common for overwrite mode)
    Beam,        // a thin vertical bar at the cell's left (common for insert mode)
    HollowBlock, // an outline block
    Hidden,      // draw nothing
}

/// Where the text cursor is within a snapshot, in the snapshot's own row/column
/// coordinates, plus the shape to draw it. `None` (on [`Snapshot`]) when the
/// cursor is hidden or the view is scrolled back.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CursorPos {
    pub col: usize,          // column within the captured grid
    pub line: usize,         // row within the captured grid (0 = top of the snapshot)
    pub shape: CursorShape,  // the shape the app requested for the cursor
}

/// Which cells changed since the previous rendered frame, in the pane's
/// viewport cell coordinates. `Full` means "repaint everything" — the honest,
/// always-correct answer for the first frame, a resize, newspaper columns, or
/// anything the engine can't describe precisely. `Lines` is a per-row inclusive
/// changed-column span (`left..=right`). `Scroll { lines, spans }` means the pane
/// scrolled UP by `lines` whole rows: a backend that can move pixels (the XRender
/// server-side `CopyArea`) may blit its content up by `lines` and then repaint
/// only `spans`; a backend that can't treats it as `Full`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Damage {
    Full,
    Lines(Vec<CellDamage>),
    Scroll { lines: usize, spans: Vec<CellDamage> },
}

impl Default for Damage {
    fn default() -> Self {
        Damage::Full // a fresh snapshot makes no promises: repaint all
    }
}

impl Damage {
    /// Does this damage cover viewport row `line`? `Full` covers every row.
    pub fn contains_line(&self, line: usize) -> bool {
        match self {
            Damage::Full => true,
            Damage::Lines(v) | Damage::Scroll { spans: v, .. } => v.iter().any(|d| d.line == line),
        }
    }

    /// Is this the "repaint everything" variant?
    pub fn is_full(&self) -> bool {
        matches!(self, Damage::Full)
    }

    /// The scroll distance in rows if this is a scroll-blittable frame, else `None`.
    pub fn scroll_lines(&self) -> Option<usize> {
        match self {
            Damage::Scroll { lines, .. } => Some(*lines),
            _ => None,
        }
    }
}

/// One damaged span on a single viewport row: columns `left..=right` (inclusive)
/// of row `line` changed. Mirrors `alacritty_terminal`'s `LineDamageBounds` in
/// the pane's own cell space.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CellDamage {
    pub line: usize,
    pub left: usize,
    pub right: usize,
}

/// An immutable snapshot of a pane's visible grid, produced for rendering or
/// for headless assertions in tests. Row-major: `rows[y]` is one screen line.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub cols: usize,                    // number of columns captured
    pub rows: Arc<Vec<Vec<SnapCell>>>,  // one inner Vec per visible screen line
    pub cursor: Option<CursorPos>,      // cursor location, if visible
    pub damage: Damage,                 // cells changed since the previous rendered frame
}

impl Snapshot {
    /// Flatten the snapshot to plain text, one `\n`-separated line per row with
    /// trailing blanks trimmed. Handy for tests and for debugging what the
    /// engine actually parsed.
    pub fn to_text(&self) -> String {
        let mut out = String::new(); // accumulates the whole screen as text
        for row in self.rows.iter() {
            // Build the row string, then trim trailing spaces so blank padding
            // at the end of a line does not defeat `contains` checks in tests.
            let line: String = row.iter().map(|cell| cell.c).collect();
            out.push_str(line.trim_end()); // drop right-hand blank padding
            out.push('\n'); // row separator
        }
        out
    }
}

/// Line-index bounds of the grid, returned by [`AlacPane::line_bounds`]. All
/// values are in `alacritty_terminal`'s integer line space: `0..screen_lines`
/// is the visible screen, negative indices are scrollback history.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineBounds {
    /// Oldest readable line (`<= 0`); the top of scrollback.
    pub topmost: i32,
    /// Newest readable line (`screen_lines - 1`); the bottom of the screen.
    pub bottommost: i32,
    /// Height of the visible screen in rows.
    pub screen_lines: usize,
    /// Width of the grid in columns.
    pub cols: usize,
}

/// One scrollback-search hit: an absolute grid line, the starting column, and
/// the length in cells. Coordinates are in `alacritty_terminal`'s integer line
/// space (negative = scrollback history, `0..screen_lines` = visible screen), so
/// the caller can both scroll the hit into view and highlight the exact cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SearchMatch {
    /// Absolute grid line of the hit (`<= 0` in history).
    pub line: i32,
    /// Starting column of the hit.
    pub col: usize,
    /// Length of the hit in cells.
    pub len: usize,
}

/// Fixed initial grid dimensions expressed as an `alacritty_terminal`
/// `Dimensions`. The engine is told how many columns/lines it has; the renderer
/// recomputes this from pixel size ÷ cell size and calls [`AlacPane::resize`].
struct Size {
    cols: usize,        // visible columns
    screen_lines: usize, // visible rows
}

impl Dimensions for Size {
    /// Total buffered lines. At construction we have no scrollback yet, so the
    /// total equals the visible height; `Term` grows history internally as
    /// content scrolls off the top.
    fn total_lines(&self) -> usize {
        self.screen_lines // no history at init; Term manages growth thereafter
    }
    /// Height of the viewport in lines.
    fn screen_lines(&self) -> usize {
        self.screen_lines
    }
    /// Width of the viewport in columns.
    fn columns(&self) -> usize {
        self.cols
    }
}

/// The `EventListener` we hand to `alacritty_terminal`. Its `send_event` is
/// invoked from the I/O thread whenever the terminal wants to tell the host
/// something. We translate the subset we care about into [`PaneEvent`]s on a
/// shared queue, and we answer `PtyWrite` (terminal query replies, e.g. cursor
/// position reports) by writing straight back to the PTY.
#[derive(Clone)]
struct Proxy {
    // Shared event queue drained by the GUI thread. Arc<Mutex<..>> because the
    // I/O thread pushes while the GUI thread pops.
    queue: Arc<Mutex<VecDeque<PaneEvent>>>,
    // The channel back to the PTY, needed to answer terminal queries. It is set
    // *after* the EventLoop is built (chicken-and-egg: the sender comes from the
    // loop), hence the Mutex<Option<..>>.
    sender: Arc<Mutex<Option<EventLoopSender>>>,
}

/// Reduce a kitty-keyboard query reply to the flags rt's key encoder actually honours.
///
/// The vendored engine implements all five enhancement flags and answers `CSI ? u` with
/// whatever the application pushed — but rt's encoder honours flag 1 alone. Passing that
/// reply through unchanged would tell an application it has, say, flag 8 ("report all
/// keys as escape codes") while rt keeps sending plain text for letters, and the
/// application would sit waiting for sequences that never come. Masking here is the one
/// place both engines can be made to agree without touching the vendored source: vt-term
/// masks on the way IN (it never stores a flag it does not honour), and this masks the
/// vendored engine on the way OUT.
///
/// Only an exact `ESC [ ? <digits> u` is touched; every other reply is passed through
/// untouched, which is all of them in practice — this event carries one reply at a time.
fn mask_kitty_keyboard_reply(text: String) -> String {
    let flags = text
        .strip_prefix("\x1b[?")
        .and_then(|rest| rest.strip_suffix('u'))
        .and_then(|digits| digits.parse::<u32>().ok());
    match flags {
        // `& 1` is `vt_term::KITTY_KBD_SUPPORTED`, spelled out rather than imported so
        // the vendored backend does not gain a dependency on the in-house engine.
        Some(f) => format!("\x1b[?{}u", f & 1),
        None => text,
    }
}

impl EventListener for Proxy {
    /// Called by the engine's I/O thread for every terminal event. We keep this
    /// fast and non-blocking: translate-and-enqueue, or reply to the PTY. It
    /// must never panic (it runs on a thread we do not control), so every path
    /// degrades quietly.
    fn send_event(&self, event: AlacEvent) {
        match event {
            // Title changes → enqueue for the tab/titlebar to pick up.
            AlacEvent::Title(t) => self.push(PaneEvent::Title(t)),
            // Some programs reset the title to the default.
            AlacEvent::ResetTitle => self.push(PaneEvent::Title(String::new())),
            // Bell → enqueue; the GUI turns this into a transient flash.
            AlacEvent::Bell => self.push(PaneEvent::Bell),
            // New content parsed → ask the GUI to redraw.
            AlacEvent::Wakeup => self.push(PaneEvent::Wakeup),
            // The child process exited (shell `exit`/`quit`/Ctrl-D). Tell the
            // GUI so it closes this pane — otherwise a dead shell lingers.
            AlacEvent::ChildExit(status) => self.push(PaneEvent::Exited(exit_code(status))),
            // The terminal wants to send bytes back to the program (query
            // replies, bracketed-paste acks, etc.). Forward to the PTY.
            AlacEvent::PtyWrite(text) => {
                // Lock the optional sender; if it is wired up, ship the bytes.
                if let Ok(guard) = self.sender.lock() {
                    if let Some(sender) = guard.as_ref() {
                        // Owned bytes → 'static Cow, as Msg::Input requires.
                        let text = mask_kitty_keyboard_reply(text);
                        let _ = sender.send(Msg::Input(Cow::Owned(text.into_bytes())));
                    }
                }
            }
            // All other events (clipboard, colour queries, cursor-blink, mouse
            // cursor shape) are not yet wired into rt's UI; ignore them safely.
            _ => {}
        }
    }
}

impl Proxy {
    /// Push one high-level event onto the shared queue, silently dropping it if
    /// the lock is poisoned (a poisoned lock means a prior panic; we choose
    /// resilience over propagating it into the engine thread).
    fn push(&self, ev: PaneEvent) {
        if let Ok(mut q) = self.queue.lock() {
            q.push_back(ev); // enqueue for the GUI to drain later
        }
    }
}

/// One terminal pane: PTY + parser + I/O thread, with a small host-facing API.
pub struct AlacPane {
    // The shared terminal state. `FairMutex` (alacritty's fair lock) is shared
    // with the I/O thread, which locks it to apply parsed bytes while we lock it
    // to read a render snapshot.
    term: Arc<FairMutex<Term<Proxy>>>,
    // Channel to the I/O thread for input/resize/shutdown.
    sender: EventLoopSender,
    // The GUI-facing event queue (same Arc the Proxy pushes to).
    events: Arc<Mutex<VecDeque<PaneEvent>>>,
    // Join handle for the I/O thread; kept so the thread lives as long as the
    // pane and is joined on drop. `Option` so `Drop` can take it.
    io_thread: Option<std::thread::JoinHandle<()>>,
    // Current grid size, tracked so resizes can rebuild a correct WindowSize.
    cols: usize,
    rows: usize,
    // The 256-colour palette used to resolve cell colours to RGB. Built once.
    palette: palette::Palette,
    // The child shell's process id, captured at spawn. rt-mux uses it (as the
    // pane's session leader) to attribute CPU/memory to the pane.
    pid: Option<u32>,
    // The configured maximum scrollback (lines) this pane was built with, so the
    // GUI can show a "used / max" buffer meter. `scroll_info().1` is the current
    // fill against this ceiling.
    scrollback_limit: usize,
    // Set if the GUI's render_snapshot panicked for this pane and was caught; the
    // render loop then skips it (frozen at its last frame) so a deterministic render
    // bug can't re-panic every frame.
    crashed: std::sync::atomic::AtomicBool,
}

impl AlacPane {
    /// Spawn a new pane running `shell` (or the user's default shell if `None`)
    /// in `working_directory`, sized `cols` × `rows` cells.
    ///
    /// Returns an error only if the PTY or I/O thread cannot be created (e.g.
    /// the system is out of file descriptors); a bad `working_directory` is
    /// tolerated by the OS/shell rather than failing here.
    ///
    /// Wiring, in order: build the shared queues → construct the `Term` → open
    /// the PTY → build the `EventLoop`, grab its sender → hand the sender to the
    /// proxy (so query replies work) → spawn the loop thread.
    pub fn spawn(
        shell: Option<(String, Vec<String>)>, // (program, args); None = default shell
        working_directory: Option<std::path::PathBuf>,
        cols: usize,
        rows: usize,
    ) -> std::io::Result<Self> {
        // Default scrollback; callers that expose a setting use `spawn_env`.
        Self::spawn_env(shell, working_directory, cols, rows, &[], DEFAULT_SCROLLBACK)
    }

    /// Like [`spawn`](Self::spawn) but with extra environment variables exported
    /// into the child shell (each `(name, value)`). rt-mux uses this to advertise
    /// a pane's side-channel pipe endpoints (`$RT_OUT` / `$RT_IN`) so programs can
    /// opt into inter-pane wiring.
    pub fn spawn_env(
        shell: Option<(String, Vec<String>)>,
        working_directory: Option<std::path::PathBuf>,
        cols: usize,
        rows: usize,
        env: &[(String, String)], // extra environment variables for the child
        scrollback: usize,        // max scrollback lines to retain above the screen
    ) -> std::io::Result<Self> {
        // Shared state between this struct and the proxy/I/O thread.
        let events = Arc::new(Mutex::new(VecDeque::new())); // event FIFO
        let sender_slot = Arc::new(Mutex::new(None)); // filled in below
        let proxy = Proxy { queue: events.clone(), sender: sender_slot.clone() };

        // Build the terminal grid + parser. `scrolling_history` is the buffer the
        // user can grow for long-running output (see rt's Preferences).
        // `kitty_keyboard` gates the vendored engine's whole keyboard-protocol state
        // machine: with it at its `false` default every `CSI ? u` / `CSI > … u` is
        // silently dropped, so an application would query, hear nothing, and conclude
        // rt does not support the protocol. rt implements the host half (see
        // `rt::input::encode_key_kitty`), so the engine half must be switched on.
        let config =
            Config { scrolling_history: scrollback, kitty_keyboard: true, ..Config::default() };
        let size = Size { cols, screen_lines: rows }; // initial dimensions
        // Term is shared behind alacritty's FairMutex so the I/O thread and the
        // renderer can both reach it without starving each other.
        let term = Arc::new(FairMutex::new(Term::new(config, &size, proxy.clone())));

        // PTY options: which shell to run and where.
        let mut pty_opts = PtyOptions::default(); // defaults to the login shell
        if let Some((program, args)) = shell {
            pty_opts.shell = Some(Shell::new(program, args)); // explicit shell override
        }
        pty_opts.working_directory = working_directory; // may be None → shell's default
        // Advertise a terminal type the child's terminfo/ncurses will recognise.
        // We emit standard xterm-compatible sequences, and we resolve 24-bit
        // colour, so xterm-256color + truecolor is accurate. Without this, apps
        // like `mc` inherit whatever TERM launched rt and mis-decode our keys.
        pty_opts.env.insert("TERM".to_string(), "xterm-256color".to_string());
        pty_opts.env.insert("COLORTERM".to_string(), "truecolor".to_string());
        // Caller-supplied extras (e.g. rt-mux's $RT_OUT / $RT_IN pipe jacks).
        for (k, v) in env {
            pty_opts.env.insert(k.clone(), v.clone());
        }

        // Cell pixel size is only advisory to the kernel's winsize; the parser
        // cares about cols/rows. 8×16 is a reasonable placeholder until the
        // renderer knows the real font metrics and calls `resize`.
        let window_size = WindowSize {
            num_lines: rows as u16,   // visible rows
            num_cols: cols as u16,    // visible columns
            cell_width: 8,            // px per cell (advisory)
            cell_height: 16,          // px per cell (advisory)
        };

        // Open the PTY and fork the shell. window_id 0: rt is single-window per
        // engine pane at this layer, so a constant id is fine.
        let pty = tty::new(&pty_opts, window_size, 0)?;
        // Grab the child shell's pid before the event loop takes ownership of the
        // PTY. It is the pane's session leader, so summing over its session gives
        // the pane's whole process tree (shell + whatever it runs).
        let pid = Some(pty.child().id());

        // The event loop owns the PTY and drives the Term. drain_on_exit=true so
        // a fast-exiting child's final output (e.g. `printf x` that exits
        // immediately) is fully read into the grid before teardown, instead of
        // being lost to an EOF race. ref_test=false (no synthetic-input mode).
        let event_loop = EventLoop::new(term.clone(), proxy, pty, true, false)?;
        let sender = event_loop.channel(); // the handle we use to send input/resize

        // Now that we have the sender, hand a clone to the proxy so terminal
        // query replies (PtyWrite) can reach the PTY.
        if let Ok(mut slot) = sender_slot.lock() {
            *slot = Some(sender.clone()); // wire the reply path
        }

        // Start the background I/O thread. It returns (Self, State) on join; we
        // discard both — we only need it to run until Shutdown.
        let handle = event_loop.spawn();
        // spawn() gives a JoinHandle<(EventLoop, State)>; we wrap it in a thread
        // that joins it so our stored handle is a plain JoinHandle<()>.
        let io_thread = std::thread::spawn(move || {
            let _ = handle.join(); // block until the loop stops, ignore its result
        });

        Ok(AlacPane {
            term,
            sender,
            events,
            io_thread: Some(io_thread),
            cols,
            rows,
            palette: palette::Palette::xterm(), // standard xterm 256-colour table
            pid,
            scrollback_limit: scrollback,
            crashed: std::sync::atomic::AtomicBool::new(false),
        })
    }

    /// Whether the GUI's `render_snapshot` for this pane panicked and was caught.
    pub fn is_crashed(&self) -> bool {
        self.crashed.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Record a caught render panic: mark the pane crashed (render loop skips it — no
    /// per-frame panic storm) and emit `Crashed` once for the `[crashed]` badge.
    pub fn note_render_crash(&self) {
        if !self.crashed.swap(true, std::sync::atomic::Ordering::AcqRel) {
            if let Ok(mut q) = self.events.lock() {
                q.push_back(PaneEvent::Crashed);
            }
        }
    }

    /// The pane's configured scrollback ceiling in lines (what it was spawned
    /// with). Pair with `scroll_info().1` for a "used / max" buffer meter.
    pub fn scrollback_limit(&self) -> usize {
        self.scrollback_limit
    }

    /// The child shell's process id (the pane's session leader), or `None` if it
    /// could not be determined. Used to attribute CPU/memory to the pane.
    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Feed raw input bytes (already encoded keystrokes / pasted text) to the
    /// shell. Non-blocking: it queues the bytes on the I/O thread's channel.
    /// A send error (thread gone) is swallowed because the pane is on its way
    /// out anyway.
    pub fn write(&self, bytes: &[u8]) {
        // Msg::Input needs 'static bytes; copy into an owned Vec.
        let owned = bytes.to_vec(); // own the data so it outlives this call
        let _ = self.sender.send(Msg::Input(Cow::Owned(owned))); // enqueue for the PTY
    }

    /// Resize the pane to `cols` × `rows` cells (called by the renderer when the
    /// pane's pixel rectangle or the font changes). Resizes both the `Term`
    /// grid and the kernel PTY winsize so the shell learns the new size.
    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return; // no-op: avoid churning the grid on identical sizes
        }
        self.cols = cols; // remember the new geometry
        self.rows = rows;
        // Resize the Term grid under the lock.
        {
            let mut term = self.term.lock(); // exclusive access to the grid
            term.resize(Size { cols, screen_lines: rows }); // reflow to new size
        }
        // Tell the PTY (and thus the shell via SIGWINCH) about the new size.
        let ws = WindowSize {
            num_lines: rows as u16,
            num_cols: cols as u16,
            cell_width: 8,
            cell_height: 16,
        };
        let _ = self.sender.send(Msg::Resize(ws)); // propagate to the kernel PTY
    }

    /// Capture the current visible grid as a [`Snapshot`] for rendering or
    /// testing. Locks the `Term` briefly, copies out the visible cells, and
    /// releases — it never hands out a reference into shared state.
    pub fn snapshot(&self) -> Snapshot {
        let term = self.term.lock(); // read access to the grid
        self.capture_locked(&term)
    }

    /// Build a [`Snapshot`] from an already-locked `Term`. Split out so
    /// `render_snapshot()` can capture the grid and the damage under one lock.
    /// The `damage` field is left at its `Full` default here; only
    /// `render_snapshot()` fills it.
    fn capture_locked(&self, term: &Term<Proxy>) -> Snapshot {
        use alacritty_terminal::term::TermMode; // for the cursor-visibility flag
        let cols = term.columns(); // current column count
        let rows = term.screen_lines(); // current visible row count
        // How many lines the view is scrolled up into history. `display_iter`
        // yields cells with their ABSOLUTE grid line (negative into history), so
        // we add this offset to map them back onto viewport rows 0..rows.
        let offset = term.grid().display_offset() as i32;
        // Pre-fill a blank grid in the CONFIGURED background colour, so any cell
        // the iterator doesn't cover (e.g. above the top of history) stays the
        // translucent default rather than an opaque hardcoded colour.
        let blank = SnapCell { c: ' ', fg: self.palette.fg, bg: self.palette.bg, attrs: CellAttrs::default() };
        let mut grid = vec![vec![blank.clone(); cols]; rows];
        // Walk the visible cells.
        for cell in term.grid().display_iter() {
            let row = cell.point.line.0 + offset; // absolute line → viewport row (top = 0)
            let col = cell.point.column.0; // usize column index
            // Guard the indices so scrolling or an engine change can't panic.
            if row >= 0 && (row as usize) < rows && col < cols {
                // Resolve this cell's colours (attribute flags folded in) and
                // its drawing attributes (underline/italic/strikeout).
                let (fg, bg) = self.resolve_colors(&cell); // fg/bg RGB
                let attrs = Self::attrs_of(cell.flags); // underline/italic/strikeout
                grid[row as usize][col] = SnapCell { c: cell.c, fg, bg, attrs };
            }
        }
        // Capture the cursor position, but only when it is actually shown and
        // the view is not scrolled back into history (a scrolled-back cursor is
        // off-screen and must not be drawn).
        let cursor = if term.mode().contains(TermMode::SHOW_CURSOR) && term.grid().display_offset() == 0 {
            // Map the terminal's requested cursor shape to ours; a Hidden shape
            // means "draw nothing".
            let shape = match term.cursor_style().shape {
                alacritty_terminal::vte::ansi::CursorShape::Block => CursorShape::Block,
                alacritty_terminal::vte::ansi::CursorShape::Underline => CursorShape::Underline,
                alacritty_terminal::vte::ansi::CursorShape::Beam => CursorShape::Beam,
                alacritty_terminal::vte::ansi::CursorShape::HollowBlock => CursorShape::HollowBlock,
                alacritty_terminal::vte::ansi::CursorShape::Hidden => CursorShape::Hidden,
            };
            let p = term.grid().cursor.point; // cursor point in viewport coords
            let line = p.line.0; // i32 line
            let col = p.column.0; // usize column
            if shape != CursorShape::Hidden && line >= 0 && (line as usize) < rows && col < cols {
                Some(CursorPos { col, line: line as usize, shape }) // on-screen: report it
            } else {
                None // hidden shape or out of the visible region
            }
        } else {
            None // hidden or scrolled back
        };
        Snapshot { cols, rows: Arc::new(grid), cursor, damage: Damage::default() }
    }

    /// Like [`snapshot`](Self::snapshot) but also captures the terminal's damage
    /// (which cells changed since the last call) and resets it, so the next call
    /// reports damage relative to this frame. Call this **once per pane per
    /// frame** from the render path only — `Term::damage()` mutates damage state.
    ///
    /// Precise `Damage::Lines` is produced only for the ordinary case: a single
    /// column of grid, scrolled to the bottom (`display_offset == 0`), where a
    /// damaged viewport row maps 1:1 onto snapshot row `line`. Any other case
    /// (scrolled into history, mid-resize) already comes back as `Full` from the
    /// engine, which the renderer honours by repainting everything.
    pub fn render_snapshot(&self) -> Snapshot {
        use alacritty_terminal::term::TermDamage;
        let mut term = self.term.lock(); // exclusive: damage() is &mut
        let mut snap = self.capture_locked(&term); // grid + cursor (immutable borrow ends here)
        let damage = match term.damage() {
            TermDamage::Full => Damage::Full,
            TermDamage::Partial(iter) => {
                // Collect before reset_damage(): the iterator borrows the damage
                // buffer, and reset_damage() needs to borrow it mutably.
                let lines: Vec<CellDamage> = iter
                    .filter(|b| b.is_damaged())
                    .map(|b| CellDamage { line: b.line, left: b.left, right: b.right })
                    .collect();
                Damage::Lines(lines)
            }
        };
        term.reset_damage(); // next frame's damage is relative to this one
        snap.damage = damage;
        snap
    }

    /// Extract the drawing attributes (underline/italic/strikeout) from a cell's
    /// flag bitset. "Underline" covers every underline style (single, double,
    /// undercurl, dotted, dashed) as a plain underline for now.
    fn attrs_of(flags: alacritty_terminal::term::cell::Flags) -> CellAttrs {
        use alacritty_terminal::term::cell::Flags;
        CellAttrs {
            bold: flags.contains(Flags::BOLD),                  // heavier weight face
            underline: flags.intersects(Flags::ALL_UNDERLINES), // any underline style
            italic: flags.contains(Flags::ITALIC),              // slanted face
            strikeout: flags.contains(Flags::STRIKEOUT),        // line through the glyph
        }
    }

    /// Resolve one cell's abstract foreground/background `Color`s to concrete
    /// RGB, folding in the attribute flags. Returns `(fg, bg)`.
    ///
    /// Rules mirror common terminal behaviour: BOLD promotes an ANSI 0–7
    /// foreground to its bright 8–15 variant; DIM darkens the foreground;
    /// INVERSE swaps fg and bg; HIDDEN makes the glyph invisible (fg = bg).
    fn resolve_colors(&self, cell: &alacritty_terminal::term::cell::Cell) -> (Rgb, Rgb) {
        use alacritty_terminal::term::cell::Flags;
        use alacritty_terminal::vte::ansi::Color;

        // Resolve one abstract Color to RGB against our palette + defaults.
        let resolve = |color: Color, is_bg: bool| -> Rgb {
            match color {
                Color::Spec(rgb) => [rgb.r, rgb.g, rgb.b], // a literal 24-bit colour
                Color::Indexed(i) => self.palette.indexed(i), // 256-colour table
                Color::Named(n) => {
                    let idx = n as usize; // NamedColor's discriminant doubles as an index
                    match idx {
                        0..=15 => self.palette.indexed(idx as u8), // the 16 ANSI colours
                        256 => self.palette.fg,                     // Foreground (configurable)
                        257 => self.palette.bg,                     // Background (configurable)
                        258 => self.palette.cursor,                 // Cursor colour
                        267 => self.palette.fg,                     // BrightForeground
                        268 => palette::dim(self.palette.fg),       // DimForeground
                        259..=266 => palette::dim(self.palette.indexed((idx - 259) as u8)), // DimBlack..White
                        _ => if is_bg { self.palette.bg } else { self.palette.fg }, // any other named default
                    }
                }
            }
        };

        let flags = cell.flags; // attribute bitset for this cell
        let mut fg = resolve(cell.fg, false); // base foreground
        let mut bg = resolve(cell.bg, true); // base background

        // BOLD brightens a base ANSI foreground (0–7 → 8–15), the common default.
        if flags.contains(Flags::BOLD) {
            match cell.fg {
                Color::Named(n) if (n as usize) < 8 => fg = self.palette.indexed(n as u8 + 8),
                Color::Indexed(i) if i < 8 => fg = self.palette.indexed(i + 8),
                _ => {} // explicit/bright colours are left as-is
            }
        }
        // DIM darkens the foreground.
        if flags.contains(Flags::DIM) {
            fg = palette::dim(fg);
        }
        // INVERSE swaps foreground and background (e.g. selections, `rev`).
        if flags.contains(Flags::INVERSE) {
            std::mem::swap(&mut fg, &mut bg);
        }
        // HIDDEN makes the glyph invisible by painting it in the background.
        if flags.contains(Flags::HIDDEN) {
            fg = bg;
        }
        (fg, bg)
    }

    /// Replace this pane's colour palette (foreground/background/cursor + the
    /// 16 ANSI colours and derived cube/greyscale). Used to apply configured or
    /// preset colour schemes live; the next `snapshot` resolves cells against it.
    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    /// Scroll the terminal's scrollback view by `delta` lines: positive scrolls
    /// up (toward older history), negative scrolls down (toward the newest line).
    /// Takes `&self` because it locks the shared `Term` internally.
    ///
    /// In a newspaper-column pane the whole (tall) viewport shifts by whole
    /// lines, so a line leaving the bottom of one column reappears at the top of
    /// the next — the flow the feature promises — while the app underneath is
    /// none the wiser (it just sees an ordinary scrollback scroll).
    pub fn scroll(&self, delta: isize) {
        use alacritty_terminal::grid::Scroll; // the scroll command enum
        let mut term = self.term.lock(); // exclusive access to move the viewport
        term.scroll_display(Scroll::Delta(delta as i32)); // shift by whole lines
    }

    /// Snap the viewport back to the newest line (`display_offset == 0`). Called
    /// when the user types into a pane that's scrolled up in history, so a
    /// keystroke returns them to the live prompt — the standard terminal
    /// behaviour. A no-op (cheap) when already at the bottom.
    pub fn scroll_to_bottom(&self) {
        use alacritty_terminal::grid::Scroll;
        let mut term = self.term.lock();
        if term.grid().display_offset() != 0 {
            term.scroll_display(Scroll::Bottom);
        }
    }

    /// Scrollbar state: `(offset, history, screen)` — how many lines the view is
    /// scrolled up (`offset`, 0 = at the bottom), the number of scrollback lines
    /// (`history`), and the visible height (`screen`). The renderer uses this to
    /// draw a scrollbar thumb. `history == 0` means nothing to scroll.
    pub fn scroll_info(&self) -> (usize, usize, usize) {
        let term = self.term.lock(); // read the grid metrics
        let offset = term.grid().display_offset(); // lines scrolled up
        let history = term.history_size(); // scrollback line count
        let screen = term.screen_lines(); // visible rows
        (offset, history, screen)
    }

    /// Whether visible row `row` soft-wraps into the next (its last cell has WRAPLINE), so a
    /// word or selection that reaches the row's end continues onto row `row + 1`.
    pub fn line_wrapped(&self, row: usize) -> bool {
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::cell::Flags;
        let term = self.term.lock();
        let cols = term.columns();
        if cols == 0 || row >= term.screen_lines() {
            return false;
        }
        term.grid()[Line(row as i32)][Column(cols - 1)].flags.contains(Flags::WRAPLINE)
    }

    /// Extract the text of a selection given by two endpoints in ABSOLUTE grid
    /// lines (the alacritty `Line` index — `0..screen_lines` is the visible
    /// screen at the bottom, negative is scrollback history; the same coordinate
    /// `search` returns in `SearchMatch::line`). Reading straight from the grid
    /// means it works across scrollback the viewport is not currently showing —
    /// unlike a snapshot, which only holds the visible rows. Linear (row-major
    /// reading order) unless `block`, which takes the rectangle between the two
    /// corners. Trailing blanks per line are trimmed; rows join with '\n'.
    pub fn selection_text(&self, anchor: (usize, i32), head: (usize, i32), block: bool) -> String {
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::cell::Flags;
        let term = self.term.lock();
        let grid = term.grid();
        let cols = term.columns();
        let (top, bot) = (term.topmost_line().0, term.bottommost_line().0); // readable line range
        let last_col = cols.saturating_sub(1);
        // A row's chars over `[cs, ce]`, skipping wide-char spacer cells — invisible padding
        // beside a wide glyph, not real spaces (they'd otherwise show up in the copy).
        let row_text = |l: i32, cs: usize, ce: usize| -> String {
            let row = &grid[Line(l)];
            (cs..=ce.min(last_col))
                .filter(|&c| {
                    !row[Column(c)].flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                })
                .map(|c| row[Column(c)].c)
                .collect()
        };
        if block {
            // Rectangle: the same column range on every line; each row is its own line.
            let (c0, c1) = (anchor.0.min(head.0), anchor.0.max(head.0).min(last_col));
            let (l0, l1) = (anchor.1.min(head.1), anchor.1.max(head.1));
            let mut lines: Vec<String> = Vec::new();
            for l in l0..=l1 {
                if l < top || l > bot {
                    continue; // outside the readable buffer
                }
                lines.push(row_text(l, c0, c1).trim_end().to_string());
            }
            lines.join("\n")
        } else {
            // Linear: order the endpoints by (line, col); first/last lines are bounded by
            // their column, the middle lines run full width.
            let (start, end) = if (anchor.1, anchor.0) <= (head.1, head.0) { (anchor, head) } else { (head, anchor) };
            let mut out = String::new();
            for l in start.1..=end.1 {
                if l < top || l > bot {
                    continue;
                }
                let cs = if l == start.1 { start.0.min(last_col) } else { 0 };
                let ce = if l == end.1 { end.0.min(last_col) } else { last_col };
                let s = if cs <= ce { row_text(l, cs, ce) } else { String::new() };
                // A soft-wrapped line (WRAPLINE on its last cell) is one logical line that
                // continues into the next, so join it WITHOUT trimming or a newline — else a
                // copied long line gains a spurious break at every screen wrap. (VtPane and
                // alacritty's own copy do this; the vendored path here never did.)
                let wrapped = l != end.1 && grid[Line(l)][Column(last_col)].flags.contains(Flags::WRAPLINE);
                if wrapped {
                    out.push_str(&s);
                } else {
                    out.push_str(s.trim_end());
                    if l != end.1 {
                        out.push('\n');
                    }
                }
            }
            out
        }
    }

    /// The line-index bounds of everything currently in the grid, so a caller
    /// (notably newspaper-column view) can compute which slice of the line
    /// buffer to show and how far it may scroll.
    ///
    /// Returns [`LineBounds`] with the topmost (most negative = oldest history)
    /// and bottommost (newest visible) line indices, the visible height, and the
    /// column count — all in `alacritty_terminal`'s `Line`/`Column` integer
    /// space where `0..screen_lines` is the visible screen and negatives are
    /// scrollback.
    pub fn line_bounds(&self) -> LineBounds {
        let term = self.term.lock(); // read access to the grid metrics
        LineBounds {
            topmost: term.topmost_line().0,       // oldest line (<= 0), from history size
            bottommost: term.bottommost_line().0, // newest visible line (screen_lines-1)
            screen_lines: term.screen_lines(),    // viewport height in rows
            cols: term.columns(),                 // viewport width in columns
        }
    }

    /// Whether the terminal has *application cursor keys* mode enabled (DECCKM).
    /// Full-screen apps (mc, vim, less…) turn this on; while it is on, the arrow
    /// and Home/End keys must be encoded as SS3 (`ESC O A`) rather than CSI
    /// (`ESC [ A`). The input layer queries this to pick the right sequence.
    pub fn app_cursor_keys(&self) -> bool {
        use alacritty_terminal::term::TermMode; // the mode bitflags
        let term = self.term.lock(); // read the current terminal mode
        term.mode().contains(TermMode::APP_CURSOR) // set by DECCKM (\e[?1h)
    }

    /// The kitty keyboard enhancement flags the program in this pane has negotiated.
    /// The input layer encodes keys against them: 0 means nothing was negotiated and
    /// the legacy bytes must go out unchanged.
    ///
    /// This engine already implements all five flags, but rt's key encoder honours only
    /// flag 1 ("disambiguate escape codes"), so only that bit is reported — a caller
    /// must never be told about a flag rt would not act on. The engine's own `CSI ? u`
    /// reply is NOT filtered this way and can name flags rt does not encode; that is the
    /// recorded divergence in `docs/engine-divergence.md`.
    pub fn kitty_keyboard_flags(&self) -> u8 {
        use alacritty_terminal::term::TermMode;
        let term = self.term.lock();
        u8::from(term.mode().contains(TermMode::DISAMBIGUATE_ESC_CODES))
    }

    /// Whether the program has enabled *any* mouse reporting (click, drag, or
    /// motion). A host multiplexer should only forward mouse events to the pane
    /// when this is true — otherwise the escape sequences would land as garbage
    /// keystrokes in a plain shell.
    pub fn wants_mouse(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        let term = self.term.lock();
        term.mode().intersects(TermMode::MOUSE_MODE) // click | motion | drag
    }

    /// Whether the program enabled bracketed paste (DECSET 2004): the host should wrap
    /// pasted text in `\x1b[200~`…`\x1b[201~` so the app can tell paste from typing.
    pub fn bracketed_paste(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::BRACKETED_PASTE)
    }
    /// Whether focus reporting (DECSET 1004) is on: the host emits `\x1b[I`/`\x1b[O` on
    /// focus in/out.
    pub fn focus_events(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::FOCUS_IN_OUT)
    }
    /// Whether alternate scroll (DECSET 1007) is on: on the alt screen the host turns wheel
    /// events into cursor-key presses.
    pub fn alt_scroll(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        self.term.lock().mode().contains(TermMode::ALTERNATE_SCROLL)
    }

    /// Whether the program requested *any-motion* mouse tracking (mode 1003): it
    /// wants pointer motion reported even with no button held, e.g. to highlight
    /// whatever the pointer hovers over. Distinct from the click/drag modes so the
    /// GUI can avoid spamming bare motion at apps that only asked for clicks.
    pub fn wants_motion(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        let term = self.term.lock();
        term.mode().contains(TermMode::MOUSE_MOTION) // DECSET 1003
    }

    /// Whether the program requested SGR mouse encoding (mode 1006). Selects the
    /// `ESC [ < … M/m` form over the legacy `ESC [ M` byte form.
    pub fn mouse_sgr(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        let term = self.term.lock();
        term.mode().contains(TermMode::SGR_MOUSE)
    }

    /// Whether the terminal is on its alternate screen (as full-screen TUIs like
    /// `vim`/`htop`/`less` use). Newspaper-column flow is meaningless there — the
    /// app owns the whole screen — so the renderer falls back to a single column
    /// when this is true.
    pub fn is_alt_screen(&self) -> bool {
        use alacritty_terminal::term::TermMode; // the mode bitflags
        let term = self.term.lock(); // read the current terminal mode
        term.mode().contains(TermMode::ALT_SCREEN) // set while on the alt screen
    }

    /// Capture an arbitrary run of `rows` lines starting at grid line index
    /// `top`, reading through scrollback history as needed. Lines outside the
    /// valid `[topmost, bottommost]` range come back blank, so callers never
    /// have to bounds-check. This is the history-aware primitive that newspaper
    /// columns are built on (it fetches the `N × height` lines a multi-column
    /// view shows at once); [`AlacPane::snapshot`] handles the ordinary visible
    /// screen.
    pub fn snapshot_lines(&self, top: i32, rows: usize) -> Snapshot {
        use alacritty_terminal::index::{Column, Line}; // integer grid coordinates
        let term = self.term.lock(); // read access for the whole capture
        let grid = term.grid(); // the cell storage
        let cols = term.columns(); // width to copy per line
        let topmost = term.topmost_line().0; // oldest readable line index
        let bottommost = term.bottommost_line().0; // newest readable line index
        let mut out = Vec::with_capacity(rows); // one inner Vec per requested line
        for r in 0..rows {
            let idx = top + r as i32; // the grid line this output row maps to
            // Start blank; only fill if the line index is actually in the grid.
            let mut line = vec![SnapCell::blank(); cols];
            if idx >= topmost && idx <= bottommost {
                let row = &grid[Line(idx)]; // borrow the stored row
                for c in 0..cols {
                    // Resolve colours here too so column-mode history reads (if
                    // ever used for rendering) are also full-colour.
                    let cell = &row[Column(c)];
                    let (fg, bg) = self.resolve_colors(cell);
                    let attrs = Self::attrs_of(cell.flags);
                    line[c] = SnapCell { c: cell.c, fg, bg, attrs };
                }
            }
            out.push(line); // append this (possibly blank) line
        }
        Snapshot { cols, rows: Arc::new(out), cursor: None, damage: Damage::default() }
    }

    /// Search the whole grid (scrollback history + visible screen) for `needle`,
    /// returning every hit top-to-bottom. A plain substring search — not a regex
    /// — matched cell-by-cell so it lines up exactly with the rendered grid; wide
    /// glyphs and colours are ignored (only the character content matters).
    /// `case_sensitive == false` folds ASCII/Unicode case on both sides.
    ///
    /// This is rt's answer to the scrollback-search Terminator never had. It runs
    /// under one lock and allocates only per line, so even a full 10k-line buffer
    /// searches in a few milliseconds.
    pub fn search(&self, needle: &str, case_sensitive: bool) -> Vec<SearchMatch> {
        use alacritty_terminal::index::{Column, Line}; // integer grid coordinates
        if needle.is_empty() {
            return Vec::new(); // an empty needle matches nothing (avoids a match storm)
        }
        // Fold one char to a single lowercase char for case-insensitive compares
        // (first char of its lowercase mapping — good enough for terminal text).
        let fold = |c: char| -> char {
            if case_sensitive { c } else { c.to_lowercase().next().unwrap_or(c) }
        };
        let needle_chars: Vec<char> = needle.chars().map(fold).collect(); // folded needle
        let nlen = needle_chars.len(); // length in cells
        let term = self.term.lock(); // read access for the whole scan
        let grid = term.grid(); // the cell storage
        let cols = term.columns(); // width of every line
        let topmost = term.topmost_line().0; // oldest readable line
        let bottommost = term.bottommost_line().0; // newest readable line
        let mut out = Vec::new(); // accumulates hits in reading order
        for idx in topmost..=bottommost {
            let row = &grid[Line(idx)]; // borrow this line
            // Fold the whole line to a char vector so column index == char index.
            let hay: Vec<char> = (0..cols).map(|c| fold(row[Column(c)].c)).collect();
            if nlen > hay.len() {
                continue; // needle longer than the line: no hit possible
            }
            // Slide a window of the needle's width across the line.
            for start in 0..=(hay.len() - nlen) {
                if hay[start..start + nlen] == needle_chars[..] {
                    out.push(SearchMatch { line: idx, col: start, len: nlen });
                }
            }
        }
        out
    }

    /// Scroll the view so absolute grid line `line` sits near the vertical centre
    /// of the screen (for jumping to a search hit). Clamps to the valid scroll
    /// range: it will not scroll below the newest line or above the oldest
    /// history. Takes `&self` because it locks the shared `Term`.
    pub fn scroll_to_line(&self, line: i32) {
        use alacritty_terminal::grid::Scroll; // the scroll command enum
        let mut term = self.term.lock(); // exclusive access to move the viewport
        let screen = term.screen_lines() as i32; // viewport height
        let history = term.history_size() as i32; // how far up we may scroll
        let current = term.grid().display_offset() as i32; // current scroll amount
        // A cell at absolute `line` renders at viewport row `line + offset`; to
        // centre it we want that row ≈ screen/2, so offset = screen/2 - line.
        let desired = (screen / 2 - line).clamp(0, history); // clamp to the scrollable range
        let delta = desired - current; // relative move scroll_display expects
        if delta != 0 {
            term.scroll_display(Scroll::Delta(delta)); // shift the viewport
        }
    }

    /// Remove and return all pending high-level events (title/bell/exit/wakeup)
    /// since the last drain. The GUI calls this once per frame. Returns an empty
    /// Vec if the lock is poisoned rather than propagating the panic.
    pub fn drain_events(&self) -> Vec<PaneEvent> {
        match self.events.lock() {
            Ok(mut q) => q.drain(..).collect(), // hand over everything queued
            Err(_) => Vec::new(),               // poisoned → behave as "nothing"
        }
    }
}

impl Drop for AlacPane {
    /// Cleanly stop the I/O thread when the pane is dropped (pane closed). We
    /// send `Shutdown`, then join the thread so no orphaned PTY reader lingers.
    /// This deterministic teardown is what lets rt avoid Terminator's
    /// close-time races (#3/#4): there is exactly one owner and one shutdown.
    fn drop(&mut self) {
        let _ = self.sender.send(Msg::Shutdown); // ask the I/O thread to stop
        if let Some(handle) = self.io_thread.take() {
            let _ = handle.join(); // wait for it to actually exit
        }
    }
}

/// Why a pane could not be exported for a cross-process move.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// This pane runs on an engine that cannot export its state. Phase 2 covers
    /// the in-house vt-term engine; the vendored alacritty engine is the
    /// differential-testing oracle and fallback, and gains export later if it
    /// is ever wanted. Named rather than silent so the UI can say which pane
    /// and why.
    EngineUnsupported { engine: &'static str },
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::EngineUnsupported { engine } => {
                write!(f, "the {engine} engine cannot export a pane for transfer")
            }
        }
    }
}

impl std::error::Error for ExportError {}

/// A terminal pane, backed by either the vendored `alacritty_terminal` engine
/// ([`AlacPane`]) or the in-house `vt_parser` + `vt_term` engine ([`vtpane::VtPane`]).
/// Every method dispatches on the variant, so callers (rt's GUI) use one type regardless
/// of backend — the seam is invisible above this line.
///
/// Selection (see [`spawn_env`](Self::spawn_env)): the `RT_ENGINE` env var wins if set
/// (`vtterm`/`own` → in-house, `alacritty`/`vendored` → vendored); otherwise the default is
/// the build-time `vtterm-default` feature (off → alacritty). Installing with
/// `--features vtterm-default` makes an installed binary run the in-house engine with no
/// env var — the reliable way to dogfood, since a GUI-launched rt never sources a shell
/// rc. The active engine is announced once on stderr at startup.
pub enum TermPane {
    Alac(AlacPane),
    Vt(vtpane::VtPane),
}

impl TermPane {
    /// Spawn a pane; the backend is chosen by `RT_ENGINE` (`vtterm` → in-house, anything
    /// else / unset → the vendored alacritty engine). `budget` is the caller's process-wide
    /// scrollback budget coordinator (see `crate::budget::Budget`) — every `Vt` pane spawned
    /// registers with it; the vendored `Alac` backend does not participate (a known, tracked
    /// gap: it has no per-pane byte accounting to register). The caller owns `budget`,
    /// typically one `Arc<Budget>` created once at startup and passed to every pane spawn —
    /// there is no default or fallback here, deliberately: a hidden per-process default was
    /// tried and rejected (see the fix-round-2 report) because it was still shared, global
    /// state that every caller, including tests, silently touched.
    pub fn spawn(
        shell: Option<(String, Vec<String>)>,
        working_directory: Option<std::path::PathBuf>,
        cols: usize,
        rows: usize,
        budget: &Arc<budget::Budget>,
    ) -> std::io::Result<Self> {
        Self::spawn_env(shell, working_directory, cols, rows, &[], DEFAULT_SCROLLBACK, budget)
    }

    /// Like [`spawn`](Self::spawn) with extra child env + explicit scrollback.
    pub fn spawn_env(
        shell: Option<(String, Vec<String>)>,
        working_directory: Option<std::path::PathBuf>,
        cols: usize,
        rows: usize,
        env: &[(String, String)],
        scrollback: usize,
        budget: &Arc<budget::Budget>,
    ) -> std::io::Result<Self> {
        // Engine selection: `RT_ENGINE` wins if set (`vtterm` / `alacritty`); otherwise the
        // default is baked in at build time — the `vtterm-default` feature makes the
        // in-house engine the default so an *installed* binary uses it without any env var
        // (a GUI-launched rt never sources ~/.bashrc). The active engine is announced once
        // per process so it is always clear which is running.
        let use_vtterm = match std::env::var("RT_ENGINE").ok().as_deref() {
            Some("vtterm") | Some("vt-term") | Some("own") => true,
            Some("alacritty") | Some("alac") | Some("vendored") => false,
            _ => cfg!(feature = "vtterm-default"),
        };
        static ANNOUNCE: std::sync::Once = std::sync::Once::new();
        ANNOUNCE.call_once(|| {
            eprintln!(
                "rt: terminal engine = {}",
                if use_vtterm {
                    "vt-term (in-house vt-parser + vt-term)"
                } else {
                    "alacritty (vendored)"
                }
            );
        });
        if use_vtterm {
            Ok(TermPane::Vt(vtpane::VtPane::spawn_env(
                shell, working_directory, cols, rows, env, scrollback, budget,
            )?))
        } else {
            Ok(TermPane::Alac(AlacPane::spawn_env(
                shell, working_directory, cols, rows, env, scrollback,
            )?))
        }
    }

    /// Spawn a pane on the in-house vt-term engine specifically, regardless of
    /// the build's default or `RT_ENGINE`.
    ///
    /// Callers that need an EXPORTABLE pane need a way to ask for one:
    /// `export` refuses on the vendored engine by design, so "spawn, then
    /// discover you cannot move it" is not a usable contract. Phase 2b's
    /// session layer needs this for the same reason.
    pub fn spawn_vt_env(
        shell: Option<(String, Vec<String>)>,
        working_directory: Option<std::path::PathBuf>,
        cols: usize,
        rows: usize,
        env: &[(String, String)],
        scrollback: usize,
        budget: &Arc<budget::Budget>,
    ) -> std::io::Result<TermPane> {
        Ok(TermPane::Vt(vtpane::VtPane::spawn_env(
            shell, working_directory, cols, rows, env, scrollback, budget,
        )?))
    }

    pub fn pid(&self) -> Option<u32> {
        match self {
            Self::Alac(p) => p.pid(),
            Self::Vt(p) => p.pid(),
        }
    }
    pub fn scrollback_limit(&self) -> usize {
        match self {
            Self::Alac(p) => p.scrollback_limit(),
            Self::Vt(p) => p.scrollback_limit(),
        }
    }
    pub fn write(&self, bytes: &[u8]) {
        match self {
            Self::Alac(p) => p.write(bytes),
            Self::Vt(p) => p.write(bytes),
        }
    }
    pub fn resize(&mut self, cols: usize, rows: usize) {
        match self {
            Self::Alac(p) => p.resize(cols, rows),
            Self::Vt(p) => p.resize(cols, rows),
        }
    }
    pub fn snapshot(&self) -> Snapshot {
        match self {
            Self::Alac(p) => p.snapshot(),
            Self::Vt(p) => p.snapshot(),
        }
    }
    pub fn render_snapshot(&self) -> Snapshot {
        match self {
            Self::Alac(p) => p.render_snapshot(),
            Self::Vt(p) => p.render_snapshot(),
        }
    }
    /// Whether the pane is frozen after a caught panic (parser or render).
    pub fn is_crashed(&self) -> bool {
        match self {
            Self::Alac(p) => p.is_crashed(),
            Self::Vt(p) => p.is_crashed(),
        }
    }
    /// Record a caught `render_snapshot` panic so the render loop stops re-rendering it.
    pub fn note_render_crash(&self) {
        match self {
            Self::Alac(p) => p.note_render_crash(),
            Self::Vt(p) => p.note_render_crash(),
        }
    }
    pub fn set_palette(&mut self, palette: Palette) {
        match self {
            Self::Alac(p) => p.set_palette(palette),
            Self::Vt(p) => p.set_palette(palette),
        }
    }
    pub fn scroll(&self, delta: isize) {
        match self {
            Self::Alac(p) => p.scroll(delta),
            Self::Vt(p) => p.scroll(delta),
        }
    }
    pub fn scroll_to_bottom(&self) {
        match self {
            Self::Alac(p) => p.scroll_to_bottom(),
            Self::Vt(p) => p.scroll_to_bottom(),
        }
    }
    pub fn scroll_to_line(&self, line: i32) {
        match self {
            Self::Alac(p) => p.scroll_to_line(line),
            Self::Vt(p) => p.scroll_to_line(line),
        }
    }
    pub fn scroll_info(&self) -> (usize, usize, usize) {
        match self {
            Self::Alac(p) => p.scroll_info(),
            Self::Vt(p) => p.scroll_info(),
        }
    }
    pub fn line_wrapped(&self, row: usize) -> bool {
        match self {
            Self::Alac(p) => p.line_wrapped(row),
            Self::Vt(p) => p.line_wrapped(row),
        }
    }
    pub fn selection_text(&self, anchor: (usize, i32), head: (usize, i32), block: bool) -> String {
        match self {
            Self::Alac(p) => p.selection_text(anchor, head, block),
            Self::Vt(p) => p.selection_text(anchor, head, block),
        }
    }
    pub fn line_bounds(&self) -> LineBounds {
        match self {
            Self::Alac(p) => p.line_bounds(),
            Self::Vt(p) => p.line_bounds(),
        }
    }
    pub fn app_cursor_keys(&self) -> bool {
        match self {
            Self::Alac(p) => p.app_cursor_keys(),
            Self::Vt(p) => p.app_cursor_keys(),
        }
    }
    pub fn kitty_keyboard_flags(&self) -> u8 {
        match self {
            Self::Alac(p) => p.kitty_keyboard_flags(),
            Self::Vt(p) => p.kitty_keyboard_flags(),
        }
    }
    pub fn wants_mouse(&self) -> bool {
        match self {
            Self::Alac(p) => p.wants_mouse(),
            Self::Vt(p) => p.wants_mouse(),
        }
    }
    pub fn wants_motion(&self) -> bool {
        match self {
            Self::Alac(p) => p.wants_motion(),
            Self::Vt(p) => p.wants_motion(),
        }
    }
    pub fn mouse_sgr(&self) -> bool {
        match self {
            Self::Alac(p) => p.mouse_sgr(),
            Self::Vt(p) => p.mouse_sgr(),
        }
    }
    pub fn bracketed_paste(&self) -> bool {
        match self {
            Self::Alac(p) => p.bracketed_paste(),
            Self::Vt(p) => p.bracketed_paste(),
        }
    }
    pub fn focus_events(&self) -> bool {
        match self {
            Self::Alac(p) => p.focus_events(),
            Self::Vt(p) => p.focus_events(),
        }
    }
    pub fn alt_scroll(&self) -> bool {
        match self {
            Self::Alac(p) => p.alt_scroll(),
            Self::Vt(p) => p.alt_scroll(),
        }
    }
    pub fn is_alt_screen(&self) -> bool {
        match self {
            Self::Alac(p) => p.is_alt_screen(),
            Self::Vt(p) => p.is_alt_screen(),
        }
    }
    pub fn snapshot_lines(&self, top: i32, rows: usize) -> Snapshot {
        match self {
            Self::Alac(p) => p.snapshot_lines(top, rows),
            Self::Vt(p) => p.snapshot_lines(top, rows),
        }
    }
    pub fn search(&self, needle: &str, case_sensitive: bool) -> Vec<SearchMatch> {
        match self {
            Self::Alac(p) => p.search(needle, case_sensitive),
            Self::Vt(p) => p.search(needle, case_sensitive),
        }
    }
    pub fn drain_events(&self) -> Vec<PaneEvent> {
        match self {
            Self::Alac(p) => p.drain_events(),
            Self::Vt(p) => p.drain_events(),
        }
    }

    /// Read this pane's state for a cross-process move. See `ExportError`.
    ///
    /// Phase 2 covers the in-house vt-term engine only, by decision: the alacritty arm is a
    /// named, explicit refusal rather than an omission, so adding export to that engine later
    /// is filling in an arm, not reshaping this API.
    pub fn export(
        &self,
        pane_uid: u64,
        scrollback_budget: usize,
    ) -> Result<(rt_handoff::pane::PaneWire, Vec<rt_handoff::grid::Line>), ExportError> {
        match self {
            TermPane::Vt(p) => Ok(p.export(pane_uid, scrollback_budget)),
            TermPane::Alac(_) => Err(ExportError::EngineUnsupported { engine: "alacritty" }),
        }
    }

    /// Test-only: force the vendored alacritty backend regardless of `RT_ENGINE`/the build's
    /// default, so `the_alacritty_engine_refuses_to_export_by_name` can exercise the refusal
    /// arm deterministically. `None` when the crate is built without the `vendored` feature
    /// (no `AlacPane` to construct) — the caller skips the assertion rather than failing.
    ///
    /// Deliberately NOT done by setting `RT_ENGINE` from the test: the engine choice in
    /// `spawn_env` above is process-global (read from the environment on every call, with no
    /// caching here — but cargo still runs a binary's tests on many threads in one process),
    /// so a test that mutated it would race every other test spawning a pane in this binary.
    /// Constructing the variant directly needs no shared, mutable, process-wide state at all.
    #[cfg(all(test, feature = "vendored"))]
    fn spawn_env_with_engine_for_test_alac(
        shell: Option<(String, Vec<String>)>,
        working_directory: Option<std::path::PathBuf>,
        cols: usize,
        rows: usize,
        env: &[(String, String)],
        scrollback: usize,
    ) -> Option<Self> {
        Some(TermPane::Alac(
            AlacPane::spawn_env(shell, working_directory, cols, rows, env, scrollback)
                .expect("spawn"),
        ))
    }

    #[cfg(all(test, not(feature = "vendored")))]
    fn spawn_env_with_engine_for_test_alac(
        _shell: Option<(String, Vec<String>)>,
        _working_directory: Option<std::path::PathBuf>,
        _cols: usize,
        _rows: usize,
        _env: &[(String, String)],
        _scrollback: usize,
    ) -> Option<Self> {
        None
    }
}

#[cfg(test)]
mod kitty_reply_tests {
    use super::mask_kitty_keyboard_reply;

    #[test]
    fn a_query_reply_never_names_a_flag_rt_does_not_honour() {
        // The vendored engine stores all five flags, so an application that pushed 31
        // would be told it has 31 while rt only disambiguates.
        assert_eq!(mask_kitty_keyboard_reply("\x1b[?31u".into()), "\x1b[?1u");
        assert_eq!(mask_kitty_keyboard_reply("\x1b[?30u".into()), "\x1b[?0u");
        assert_eq!(mask_kitty_keyboard_reply("\x1b[?1u".into()), "\x1b[?1u");
        assert_eq!(mask_kitty_keyboard_reply("\x1b[?0u".into()), "\x1b[?0u");
    }

    #[test]
    fn every_other_query_reply_passes_through_untouched() {
        // DA1, DA2, CPR, DECRQM and DECRPM must not be rewritten by a filter aimed at
        // one sequence — the `?` prefix alone is shared by several of them.
        for reply in ["\x1b[?6c", "\x1b[>0;10300;1c", "\x1b[12;5R", "\x1b[?1;1$y", "\x1b[0n", ""] {
            assert_eq!(mask_kitty_keyboard_reply(reply.into()), reply);
        }
    }
}

#[cfg(test)]
mod vtpane_tests {
    use super::*;

    /// The shell's exit must be detected even when a backgrounded grandchild keeps the PTY
    /// slave open (so master EOF never arrives) — regression test for SIGCHLD-based reaping.
    #[test]
    fn vtpane_exit_detected_despite_grandchild() {
        let pane = vtpane::VtPane::spawn_env(
            Some(("/bin/sh".into(), vec!["-c".into(), "sleep 3 & exit".into()])),
            None,
            40,
            10,
            &[],
            1000,
            &Arc::new(budget::Budget::default()),
        )
        .expect("spawn");
        let mut saw_exit = false;
        for _ in 0..300 {
            if pane.drain_events().iter().any(|e| matches!(e, PaneEvent::Exited(_))) {
                saw_exit = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(saw_exit, "shell exit not detected while a backgrounded child held the PTY open");
    }

    /// An unanswered `CSI ? u` is a dead feature: the application concludes rt has no
    /// keyboard protocol and never enables it. So prove the whole loop with a real
    /// child on a real PTY — the query goes down, `Term::reply` queues it, the reader
    /// loop drains `take_output()`, and the bytes come back up to the child.
    #[test]
    fn a_real_child_gets_its_keyboard_query_answered() {
        let pane = vtpane::VtPane::spawn_env(
            Some((
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    // Raw mode: the reply has no newline, so a cooked line discipline
                    // would never hand it to `head`, and echo would print it back at us.
                    // Then: enable the protocol, ask what is active, read the 5-byte
                    // reply, and print it as hex where the test can read it off the grid.
                    "stty raw -echo; printf '\\033[>1u\\033[?u'; \
                     head -c 5 | od -An -tx1 | tr -d ' \\n'; printf '_DONE\\r\\n'"
                        .into(),
                ],
            )),
            None,
            40,
            10,
            &[],
            1000,
            &Arc::new(budget::Budget::default()),
        )
        .expect("spawn vt-term pane");

        let mut screen = String::new();
        for _ in 0..300 {
            screen = pane.snapshot().to_text();
            if screen.contains("_DONE") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        // `1b 5b 3f 31 75` is `ESC [ ? 1 u` — the flags the child just pushed, read
        // back by the child itself off the PTY.
        assert!(
            screen.contains("1b5b3f3175"),
            "child never received `\\x1b[?1u`; screen was:\n{screen}"
        );
    }

    /// The in-house backend drives a real PTY: spawn a shell that prints many lines (so
    /// scrollback builds), poll until the marker appears, confirm the child-exit event,
    /// then exercise scroll / search / selection / title / resize.
    #[test]
    fn vtpane_runs_a_real_command() {
        let mut pane = vtpane::VtPane::spawn_env(
            Some((
                "/bin/sh".into(),
                vec![
                    "-c".into(),
                    // 20 lines (> 10 rows) so lines scroll into history, plus a title.
                    "printf '\\033]0;VT TITLE\\007'; for i in $(seq 1 20); do echo LINE_$i; done; \
                     echo HELLO_VTTERM"
                        .into(),
                ],
            )),
            None,
            40,
            10,
            &[],
            1000,
            &Arc::new(budget::Budget::default()),
        )
        .expect("spawn vt-term pane");

        let mut saw_text = false;
        let mut saw_exit = false;
        let mut saw_title = false;
        for _ in 0..200 {
            if pane.snapshot().to_text().contains("HELLO_VTTERM") {
                saw_text = true;
            }
            for e in pane.drain_events() {
                match e {
                    PaneEvent::Exited(_) => saw_exit = true,
                    PaneEvent::Title(t) if t == "VT TITLE" => saw_title = true,
                    _ => {}
                }
            }
            if saw_text && saw_exit {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(saw_text, "vt-term pane never rendered the child's output");
        assert!(saw_exit, "vt-term pane never reported the child exit");
        assert!(saw_title, "vt-term pane never reported the OSC title");

        // Scrollback built up: history > 0, and scrolling up moves the viewport.
        let (_, history, _) = pane.scroll_info();
        assert!(history > 0, "expected scrollback, got {history}");
        pane.scroll(5);
        assert_eq!(pane.scroll_info().0, 5.min(history), "viewport scrolled up");
        pane.scroll_to_bottom();
        assert_eq!(pane.scroll_info().0, 0, "back to bottom");

        // Search finds the marker somewhere in the readable buffer.
        let hits = pane.search("LINE_7", true);
        assert!(!hits.is_empty(), "search found no LINE_7");

        // Selection of a full visible row yields its text.
        let text = pane.selection_text((0, 0), (39, 0), false);
        assert!(!text.is_empty(), "selection was empty");

        // Resize must not panic and keeps content addressable.
        pane.resize(20, 6);
        let _ = pane.snapshot();
        let _ = pane.wants_mouse();

        // Precise damage: the child has exited (idle), so after one render establishes the
        // baseline, the next render on unchanged content reports no damaged lines.
        let _ = pane.render_snapshot(); // baseline
        match pane.render_snapshot().damage {
            Damage::Lines(l) => assert!(l.is_empty(), "idle pane reported damage: {l:?}"),
            other => panic!("idle pane should report empty Lines, got {other:?}"),
        }
    }

    /// Poll — draining events as a real host would — until the WHOLE of `want`
    /// is on screen, or the deadline passes.
    ///
    /// Whole-substring, never a single leading character. A probe on the first
    /// character of a multi-character write can fire between the reader thread
    /// applying that character and the rest of the write, so the test then runs
    /// against a half-written screen. For `export_does_not_disturb_the_pane`
    /// that is worse than flaky: if the tail of `ALIVE` lands between the two
    /// exports, the two screens differ and the test fails claiming "export is a
    /// pure read" — a false accusation against correct code. Same convention as
    /// `tests/export.rs`'s `has_text`.
    fn wait_for_text(pane: &TermPane, want: &str) {
        for _ in 0..500 {
            let _ = pane.drain_events();
            if pane.snapshot().to_text().contains(want) {
                return;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        panic!("the pane never printed {want:?}");
    }

    #[test]
    fn a_live_vt_pane_exports_its_screen() {
        // Force the in-house engine deterministically: `TermPane::spawn_env` picks the
        // backend from `RT_ENGINE`/the build's default feature, neither of which this test
        // controls, so construct the `Vt` arm directly — the same pattern the other tests in
        // this module already use (`vtpane_runs_a_real_command` etc.) to test the in-house
        // backend on its own, without depending on which engine the build defaults to.
        let pane = TermPane::Vt(
            vtpane::VtPane::spawn_env(
                Some(("/bin/sh".into(), vec!["-c".into(), "printf 'EXPORTED'; sleep 5".into()])),
                None, 40, 6, &[], 1000, &Arc::new(budget::Budget::default()),
            )
            .expect("spawn"),
        );
        // Give the child a moment to write, draining events as a real host would.
        wait_for_text(&pane, "EXPORTED");
        let (wire, _scroll) = pane.export(42, 0).expect("the in-house engine exports");
        assert_eq!(wire.pane_uid, 42);
        assert_eq!(wire.cols, 40);
        assert_eq!(wire.rows, 6);
        assert_ne!(wire.child_pid, 0, "the child pid rides along for the pidfd");
        let text: String = wire.screen_primary.lines[0]
            .runs
            .iter()
            .map(|r| r.text.as_str())
            .collect();
        assert!(text.starts_with("EXPORTED"), "got {text:?}");
    }

    #[test]
    fn export_does_not_disturb_the_pane() {
        let pane = TermPane::Vt(
            vtpane::VtPane::spawn_env(
                Some(("/bin/sh".into(), vec!["-c".into(), "printf 'ALIVE'; sleep 5".into()])),
                None, 20, 4, &[], 1000, &Arc::new(budget::Budget::default()),
            )
            .expect("spawn"),
        );
        wait_for_text(&pane, "ALIVE");
        let (a, _) = pane.export(1, 0).unwrap();
        let (b, _) = pane.export(1, 0).unwrap();
        assert_eq!(a.screen_primary, b.screen_primary, "export is a pure read");
        // And the pane still works afterwards.
        pane.write(b"\n");
        assert!(!pane.is_crashed());
    }

    #[test]
    fn the_alacritty_engine_refuses_to_export_by_name() {
        let pane = TermPane::spawn_env_with_engine_for_test_alac(
            Some(("/bin/sh".into(), vec!["-c".into(), "sleep 5".into()])),
            None, 20, 4, &[], 1000,
        );
        let Some(pane) = pane else {
            return; // built without the vendored engine; nothing to assert
        };
        match pane.export(1, 0) {
            Err(ExportError::EngineUnsupported { engine }) => {
                assert_eq!(engine, "alacritty");
            }
            other => panic!("expected a named refusal, got {other:?}"),
        }
    }
}

#[cfg(test)]
mod damage_tests {
    use super::*;

    #[test]
    fn damage_default_is_full() {
        assert_eq!(Damage::default(), Damage::Full);
        assert!(Snapshot::default().damage.is_full());
    }

    #[test]
    fn contains_line_semantics() {
        assert!(Damage::Full.contains_line(7)); // Full covers everything
        let d = Damage::Lines(vec![
            CellDamage { line: 2, left: 0, right: 3 },
            CellDamage { line: 5, left: 1, right: 1 },
        ]);
        assert!(d.contains_line(2));
        assert!(d.contains_line(5));
        assert!(!d.contains_line(3));
        assert!(!d.is_full());
    }

    // Real PTY, so poll with a bounded budget. Proves: render_snapshot()
    // clears the engine's initial Full and converges to precise Lines on an
    // idle pane (i.e. reset_damage() actually runs).
    #[test]
    fn render_snapshot_resets_and_converges_to_lines() {
        let pane = AlacPane::spawn(
            Some(("sh".into(), vec!["-c".into(), "printf 'hello\\n'; sleep 30".into()])),
            None,
            20,
            5,
        )
        .expect("spawn test pane");

        // Wait for the child's output to reach the grid.
        let mut saw_hello = false;
        for _ in 0..600 {
            if pane.snapshot().to_text().contains("hello") {
                saw_hello = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert!(saw_hello, "child output never reached the grid");

        // Drain the initial Full frame(s). On an idle pane the damage must
        // stop being Full within a few frames — that only happens if
        // reset_damage() runs each call.
        let mut converged = None;
        for _ in 0..200 {
            let d = pane.render_snapshot().damage;
            if !d.is_full() {
                converged = Some(d);
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        let d = converged.expect("idle pane never converged off Full");
        assert!(matches!(d, Damage::Lines(_)), "idle damage should be Lines, got {d:?}");

        // A line we never wrote to (row 4, below "hello") must not be damaged
        // on a now-idle pane — precise damage, not blanket.
        let d2 = pane.render_snapshot().damage;
        assert!(!d2.contains_line(4), "unwritten line 4 should be undamaged: {d2:?}");
    }

    /// `write()` must NOT block the caller when the child isn't draining stdin. Writes go
    /// to a dedicated writer thread precisely so a large paste into a paused/non-reading
    /// program can't freeze the GUI. Here the child (`sleep`) never reads stdin, so its PTY
    /// input buffer fills and stays full; a blocking write on the caller (the old design)
    /// would hang forever. The writer thread must absorb the backpressure and let `write()`
    /// return immediately.
    #[test]
    fn vtpane_write_does_not_block_when_child_ignores_stdin() {
        use std::time::{Duration, Instant};
        let pane = vtpane::VtPane::spawn_env(
            Some(("/bin/sh".into(), vec!["-c".into(), "sleep 10".into()])),
            None,
            40,
            10,
            &[],
            1000,
            &Arc::new(budget::Budget::default()),
        )
        .expect("spawn");
        std::thread::sleep(Duration::from_millis(100)); // let the child settle

        // Far more than the PTY input buffer (~64 KiB) to a child that never reads it.
        let payload = vec![b'x'; 512 * 1024];
        let start = Instant::now();
        pane.write(&payload);
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_millis(250),
            "write() blocked the caller for {elapsed:?} on a child ignoring stdin — it must \
             enqueue to the writer thread, not block the GUI"
        );
        // (The pane drops here; its writer thread unblocks once the child is reaped.)
    }

    /// The writer thread must actually deliver input, in order, to the child: type a command
    /// and confirm it round-trips onto the grid. Guards against the channel/writer path
    /// dropping or reordering bytes.
    #[test]
    fn vtpane_write_round_trips_through_the_shell() {
        use std::time::{Duration, Instant};
        let pane = vtpane::VtPane::spawn_env(
            Some(("/bin/sh".into(), vec![])), // bare shell, reads stdin
            None,
            80,
            24,
            &[],
            1000,
            &Arc::new(budget::Budget::default()),
        )
        .expect("spawn");
        std::thread::sleep(Duration::from_millis(200)); // let the shell start

        let marker = "vtwriter-roundtrip-5561";
        pane.write(format!("echo {marker}\n").as_bytes());

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = false;
        while Instant::now() < deadline {
            if pane.snapshot().to_text().contains(marker) {
                seen = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(seen, "writer thread never delivered the typed command to the shell");
    }

    /// Task 6: the reader thread must drain vt-term's auto-generated replies
    /// (`take_output()`) and forward them back through the PTY to the child, exactly as a
    /// real terminal answers a query. `printf '\e[6n'` (DSR, cursor position report) makes
    /// vt-term queue a `\e[row;colR` reply; the child then reads it off its own stdin (that's
    /// how a real app like vim/tmux receives the answer to its handshake) and re-emits it
    /// through `%q` so the raw ESC byte becomes literal, renderable text (`$'\E[...'`)
    /// instead of being reinterpreted as another control sequence when it loops back through
    /// vt-term. Proves the whole drain -> reply_tx -> writer-thread -> PTY path end to end,
    /// without needing an interactive session.
    #[test]
    fn vtpane_answers_cursor_position_query() {
        use std::time::{Duration, Instant};
        let pane = vtpane::VtPane::spawn_env(
            Some((
                "/bin/bash".into(),
                vec![
                    "-c".into(),
                    "printf '\\e[6n'; IFS= read -rs -t 5 -d R x; printf 'CPR_Q_ANSWER:%q\\n' \"$x\""
                        .into(),
                ],
            )),
            None,
            80,
            24,
            &[],
            1000,
            &Arc::new(budget::Budget::default()),
        )
        .expect("spawn");

        let deadline = Instant::now() + Duration::from_secs(10);
        let mut text = String::new();
        while Instant::now() < deadline {
            text = pane.snapshot().to_text();
            if text.contains("CPR_Q_ANSWER:") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            text.contains("CPR_Q_ANSWER:"),
            "child never got past its `read` — no CPR reply arrived on its stdin: {text:?}"
        );
        // `%q` renders an embedded ESC as the literal text `\E`, so a real CSI cursor-position
        // reply (not an empty/garbage answer from a dropped or malformed query) shows up as
        // `$'\E[`, regardless of the exact row/col numbers.
        assert!(
            text.contains("CPR_Q_ANSWER:$'\\E["),
            "reply didn't look like a CSI cursor position report: {text:?}"
        );
    }

    /// Copying a line that WRAPS across screen rows (one logical line, too long for the width)
    /// must NOT insert a newline at the wrap — otherwise a pasted key/URL/token gains spurious
    /// line breaks. 25 chars into a 20-wide pane wraps 20+5; the copy must rejoin them.
    #[test]
    fn copy_rejoins_soft_wrapped_line_without_a_break() {
        use std::time::{Duration, Instant};
        let s = "ABCDEFGHIJKLMNOPQRSTUVWXY"; // 25 chars, a single logical line
        let pane = AlacPane::spawn(
            Some(("sh".into(), vec!["-c".into(), format!("printf %s {s}; sleep 30")])),
            None,
            20,
            5,
        )
        .expect("spawn test pane");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut ready = false;
        while Instant::now() < deadline {
            if pane.snapshot().to_text().contains("UVWXY") {
                ready = true;
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(ready, "wrapped output never rendered");
        // Select the whole wrapped line: (col 0, row 0) → (col 4, row 1).
        let got = pane.selection_text((0, 0), (4, 1), false);
        assert_eq!(got, s, "a soft-wrapped copy must be one logical line");
    }

    /// `line_wrapped` reports the soft-wrap flag that double-click word selection rides across:
    /// a 25-char line in a 20-wide pane wraps, so row 0 is wrapped and its tail row is not.
    #[test]
    fn line_wrapped_reports_the_soft_wrap_flag() {
        use std::time::{Duration, Instant};
        let pane = AlacPane::spawn(
            Some(("sh".into(), vec!["-c".into(), "printf %s ABCDEFGHIJKLMNOPQRSTUVWXY; sleep 30".into()])),
            None,
            20,
            5,
        )
        .expect("spawn test pane");
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            if pane.snapshot().to_text().contains("UVWXY") {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(pane.line_wrapped(0), "row 0 (full, autowrapped) must report wrapped");
        assert!(!pane.line_wrapped(1), "row 1 (the short tail) must not");
    }
}
