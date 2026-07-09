//! `rt-engine` — one terminal pane's backend, wrapping `alacritty_terminal`.
//!
//! Each visible leaf in the `rt-core` layout tree is backed by exactly one
//! [`TermPane`] from this crate. A `TermPane` owns:
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

mod palette; // xterm 256-colour palette + cell-colour resolution
pub use palette::{Palette, Rgb, CURSOR, DEFAULT_BG, DEFAULT_FG}; // colours + configurable palette
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

/// High-level events a pane can surface to the GUI, distilled from
/// `alacritty_terminal`'s richer event enum down to what rt's UI actually acts
/// on. Draining these (via [`TermPane::drain_events`]) replaces Terminator's
/// scattered GTK signal handlers with one explicit, race-free queue.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PaneEvent {
    /// The program asked to change the window/tab title.
    Title(String),
    /// The terminal bell rang (we render a visible-bell flash, never a
    /// fire-later GTK timeout — see TERMINATOR_BUGS.md #2).
    Bell,
    /// The child process exited; the GUI should close this pane.
    Exited,
    /// New grid content is available; the GUI should schedule a redraw.
    Wakeup,
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

/// An immutable snapshot of a pane's visible grid, produced for rendering or
/// for headless assertions in tests. Row-major: `rows[y]` is one screen line.
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub cols: usize,              // number of columns captured
    pub rows: Vec<Vec<SnapCell>>, // one inner Vec per visible screen line
    pub cursor: Option<CursorPos>, // cursor location, if visible
}

impl Snapshot {
    /// Flatten the snapshot to plain text, one `\n`-separated line per row with
    /// trailing blanks trimmed. Handy for tests and for debugging what the
    /// engine actually parsed.
    pub fn to_text(&self) -> String {
        let mut out = String::new(); // accumulates the whole screen as text
        for row in &self.rows {
            // Build the row string, then trim trailing spaces so blank padding
            // at the end of a line does not defeat `contains` checks in tests.
            let line: String = row.iter().map(|cell| cell.c).collect();
            out.push_str(line.trim_end()); // drop right-hand blank padding
            out.push('\n'); // row separator
        }
        out
    }
}

/// Line-index bounds of the grid, returned by [`TermPane::line_bounds`]. All
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
/// recomputes this from pixel size ÷ cell size and calls [`TermPane::resize`].
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
            AlacEvent::ChildExit(_status) => self.push(PaneEvent::Exited),
            // The terminal wants to send bytes back to the program (query
            // replies, bracketed-paste acks, etc.). Forward to the PTY.
            AlacEvent::PtyWrite(text) => {
                // Lock the optional sender; if it is wired up, ship the bytes.
                if let Ok(guard) = self.sender.lock() {
                    if let Some(sender) = guard.as_ref() {
                        // Owned bytes → 'static Cow, as Msg::Input requires.
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
pub struct TermPane {
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
}

impl TermPane {
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
        let config = Config { scrolling_history: scrollback, ..Config::default() };
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

        Ok(TermPane {
            term,
            sender,
            events,
            io_thread: Some(io_thread),
            cols,
            rows,
            palette: palette::Palette::xterm(), // standard xterm 256-colour table
            pid,
            scrollback_limit: scrollback,
        })
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
        use alacritty_terminal::term::TermMode; // for the cursor-visibility flag
        let term = self.term.lock(); // read access to the grid
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
        Snapshot { cols, rows: grid, cursor }
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

    /// Whether the program has enabled *any* mouse reporting (click, drag, or
    /// motion). A host multiplexer should only forward mouse events to the pane
    /// when this is true — otherwise the escape sequences would land as garbage
    /// keystrokes in a plain shell.
    pub fn wants_mouse(&self) -> bool {
        use alacritty_terminal::term::TermMode;
        let term = self.term.lock();
        term.mode().intersects(TermMode::MOUSE_MODE) // click | motion | drag
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
    /// view shows at once); [`TermPane::snapshot`] handles the ordinary visible
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
        Snapshot { cols, rows: out, cursor: None }
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

impl Drop for TermPane {
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
