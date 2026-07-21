//! `VtPane` — a terminal pane backed by the **in-house** engine (`vt_parser` +
//! `vt_term`) instead of `alacritty_terminal`. Selected at runtime with
//! `RT_ENGINE=vtterm` (see [`crate::TermPane`]); the default stays the vendored
//! alacritty backend, so this is opt-in dogfooding of the own engine on a real PTY.
//!
//! It reuses alacritty's portable `tty` only to fork the shell and own the PTY master
//! (its `Drop` reaps the child); everything else — the read loop, the grid, the render
//! snapshot — goes through `vt_term`. A background thread reads the master fd and feeds
//! `vt_term::Term`; the GUI thread writes/resizes via the raw fd and reads snapshots.
//!
//! **Degraded vs the alacritty backend** (vt-term doesn't implement these yet): no
//! scrollback *viewport* (`display_offset` is always 0, so `scroll*` are no-ops and
//! `snapshot_lines` returns the live screen), no selection/search, no mouse reporting,
//! no OSC title, block cursor only, and damage is always `Full`. These are stubbed with
//! sane defaults and called out inline.

use std::collections::VecDeque;
use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::tty::{self, EventedReadWrite, Options as PtyOptions, Shell};

use crate::palette::{self, Palette};
use crate::{
    CellAttrs, CursorPos, CursorShape, Damage, LineBounds, PaneEvent, SearchMatch, SnapCell,
    Snapshot,
};

/// A pane driven by the in-house `vt_term::Term`.
pub struct VtPane {
    /// The grid + parser, shared with the reader thread.
    term: Arc<Mutex<vt_term::Term>>,
    /// Raw PTY master fd, used for write/resize (owned by `_pty`, valid while it lives).
    pty_fd: RawFd,
    /// The forked PTY; kept alive so its `Drop` SIGHUPs and reaps the child shell.
    _pty: tty::Pty,
    /// GUI-facing event FIFO (Wakeup on new output, Exited on child EOF).
    events: Arc<Mutex<VecDeque<PaneEvent>>>,
    /// Set by the reader thread when new bytes have been applied (drives redraw).
    dirty: Arc<AtomicBool>,
    /// Reader thread; detached — it ends on its own when the child closes the PTY.
    _reader: std::thread::JoinHandle<()>,
    cols: usize,
    rows: usize,
    palette: Palette,
    pid: Option<u32>,
    scrollback_limit: usize,
}

impl VtPane {
    /// Fork `shell` in `working_directory` on a fresh PTY and start the reader thread.
    /// Mirrors [`crate::TermPane::spawn_env`]'s signature so the seam can dispatch to it.
    pub fn spawn_env(
        shell: Option<(String, Vec<String>)>,
        working_directory: Option<std::path::PathBuf>,
        cols: usize,
        rows: usize,
        env: &[(String, String)],
        scrollback: usize,
    ) -> std::io::Result<Self> {
        let mut pty_opts = PtyOptions::default();
        if let Some((program, args)) = shell {
            pty_opts.shell = Some(Shell::new(program, args));
        }
        pty_opts.working_directory = working_directory;
        pty_opts.env.insert("TERM".to_string(), "xterm-256color".to_string());
        pty_opts.env.insert("COLORTERM".to_string(), "truecolor".to_string());
        for (k, v) in env {
            pty_opts.env.insert(k.clone(), v.clone());
        }

        let window_size = WindowSize {
            num_lines: rows as u16,
            num_cols: cols as u16,
            cell_width: 8,
            cell_height: 16,
        };
        let mut pty = tty::new(&pty_opts, window_size, 0)?;
        let pid = Some(pty.child().id());
        let pty_fd = pty.reader().as_raw_fd(); // master fd; owned by `pty` for our lifetime

        let term = Arc::new(Mutex::new(vt_term::Term::new(cols, rows)));
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let dirty = Arc::new(AtomicBool::new(true));

        // Reader thread owns its own dup of the master fd so it stays valid until the
        // thread ends (independent of `_pty`'s fd lifetime).
        let read_fd = unsafe { libc::dup(pty_fd) };
        if read_fd < 0 {
            return Err(std::io::Error::last_os_error());
        }
        // alacritty opens the PTY master O_NONBLOCK (for its mio loop). We read on a
        // dedicated blocking thread instead, so clear it — the flag lives on the shared
        // open-file-description, so this also makes our writes/ioctls block, which is what
        // we want. (This runs before the read thread starts.)
        unsafe {
            let flags = libc::fcntl(read_fd, libc::F_GETFL);
            if flags >= 0 {
                libc::fcntl(read_fd, libc::F_SETFL, flags & !libc::O_NONBLOCK);
            }
        }
        let (term_r, events_r, dirty_r) = (term.clone(), events.clone(), dirty.clone());
        let reader = std::thread::spawn(move || {
            // SAFETY: `read_fd` is a fresh dup we exclusively own here.
            let mut file = unsafe { std::fs::File::from_raw_fd(read_fd) };
            let mut buf = [0u8; 65536];
            loop {
                match file.read(&mut buf) {
                    Ok(0) => {
                        events_r.lock().unwrap().push_back(PaneEvent::Exited);
                        dirty_r.store(true, Ordering::Release);
                        break;
                    }
                    Ok(n) => {
                        if let Ok(mut t) = term_r.lock() {
                            t.feed(&buf[..n]);
                        }
                        dirty_r.store(true, Ordering::Release);
                        events_r.lock().unwrap().push_back(PaneEvent::Wakeup);
                    }
                    Err(e)
                        if e.kind() == std::io::ErrorKind::WouldBlock
                            || e.kind() == std::io::ErrorKind::Interrupted =>
                    {
                        std::thread::sleep(std::time::Duration::from_millis(4));
                    }
                    Err(_) => {
                        events_r.lock().unwrap().push_back(PaneEvent::Exited);
                        dirty_r.store(true, Ordering::Release);
                        break;
                    }
                }
            }
        });

        Ok(VtPane {
            term,
            pty_fd,
            _pty: pty,
            events,
            dirty,
            _reader: reader,
            cols,
            rows,
            palette: Palette::xterm(),
            pid,
            scrollback_limit: scrollback,
        })
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }
    pub fn scrollback_limit(&self) -> usize {
        self.scrollback_limit
    }

    pub fn write(&self, bytes: &[u8]) {
        // SAFETY: `pty_fd` is a valid master fd owned by `_pty` for our lifetime; a
        // concurrent read on the dup'd fd in the reader thread is fine on a PTY master.
        unsafe {
            libc::write(self.pty_fd, bytes.as_ptr() as *const libc::c_void, bytes.len());
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        if let Ok(mut t) = self.term.lock() {
            t.resize(cols, rows);
        }
        let ws = libc::winsize {
            ws_row: rows as u16,
            ws_col: cols as u16,
            ws_xpixel: (cols * 8) as u16,
            ws_ypixel: (rows * 16) as u16,
        };
        // SAFETY: TIOCSWINSZ on our own master fd with a valid winsize.
        unsafe {
            libc::ioctl(self.pty_fd, libc::TIOCSWINSZ, &ws as *const _);
        }
        self.cols = cols;
        self.rows = rows;
        self.dirty.store(true, Ordering::Release);
    }

    /// Resolve a vt-term colour to RGB through the palette.
    fn rgb(&self, c: vt_term::Color, is_fg: bool) -> palette::Rgb {
        match c {
            vt_term::Color::Default => {
                if is_fg {
                    self.palette.fg
                } else {
                    self.palette.bg
                }
            }
            vt_term::Color::Indexed(i) => self.palette.indexed(i),
            vt_term::Color::Rgb(r, g, b) => [r, g, b],
        }
    }

    /// Materialise the visible grid into a [`Snapshot`], folding bold/dim/inverse/hidden
    /// into the resolved colours the way alacritty's `capture_locked` does.
    fn capture(&self) -> Snapshot {
        let t = self.term.lock().unwrap();
        let (cols, rows) = (t.cols(), t.rows());
        let blank = SnapCell { c: ' ', fg: self.palette.fg, bg: self.palette.bg, attrs: CellAttrs::default() };
        let mut grid = vec![vec![blank.clone(); cols]; rows];
        for r in 0..rows {
            for col in 0..cols {
                let cell = t.cell(r, col);
                let a = cell.attrs;
                let mut fg = self.rgb(cell.fg, true);
                let mut bg = self.rgb(cell.bg, false);
                if a.dim {
                    fg = palette::dim(fg);
                }
                if a.inverse {
                    std::mem::swap(&mut fg, &mut bg);
                }
                if a.hidden {
                    fg = bg;
                }
                grid[r][col] = SnapCell {
                    c: cell.c,
                    fg,
                    bg,
                    attrs: CellAttrs {
                        bold: a.bold,
                        underline: a.underline,
                        italic: a.italic,
                        strikeout: a.strikeout,
                    },
                };
            }
        }
        let cursor = if t.cursor_visible() {
            let (col, line) = t.cursor();
            (line < rows && col < cols).then_some(CursorPos { col, line, shape: CursorShape::Block })
        } else {
            None
        };
        Snapshot { cols, rows: grid, cursor, damage: Damage::default() }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.capture()
    }

    /// vt-term has no per-cell damage tracking, so every rendered frame is `Full`
    /// (correct, just not minimal). Clears the dirty flag so the caller can coalesce.
    pub fn render_snapshot(&self) -> Snapshot {
        self.dirty.store(false, Ordering::Release);
        let mut snap = self.capture();
        snap.damage = Damage::Full;
        snap
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    // ── Scrollback viewport: vt-term always observes the bottom, so these are no-ops. ──
    pub fn scroll(&self, _delta: isize) {}
    pub fn scroll_to_bottom(&self) {}
    pub fn scroll_to_line(&self, _line: i32) {}
    pub fn scroll_info(&self) -> (usize, usize, usize) {
        let t = self.term.lock().unwrap();
        (0, t.history_size(), t.rows()) // offset 0 (bottom), history count, screen height
    }

    // ── Selection / search: not yet implemented for vt-term. ──
    pub fn selection_text(&self, _anchor: (usize, i32), _head: (usize, i32), _block: bool) -> String {
        String::new()
    }
    pub fn search(&self, _needle: &str, _case_sensitive: bool) -> Vec<SearchMatch> {
        Vec::new()
    }
    pub fn line_bounds(&self) -> LineBounds {
        let t = self.term.lock().unwrap();
        // Only the visible screen is addressable (no history viewport), so topmost = 0.
        LineBounds { topmost: 0, bottommost: t.rows() as i32 - 1, screen_lines: t.rows(), cols: t.cols() }
    }
    pub fn snapshot_lines(&self, _top: i32, _rows: usize) -> Snapshot {
        self.capture() // no history view: always the live screen
    }

    // ── Mode queries the GUI uses to encode input. ──
    pub fn app_cursor_keys(&self) -> bool {
        self.term.lock().map(|t| t.app_cursor()).unwrap_or(false)
    }
    pub fn is_alt_screen(&self) -> bool {
        self.term.lock().map(|t| t.alt_screen()).unwrap_or(false)
    }
    // Mouse reporting isn't tracked yet, so the GUI treats the pane as mouse-off.
    pub fn wants_mouse(&self) -> bool {
        false
    }
    pub fn wants_motion(&self) -> bool {
        false
    }
    pub fn mouse_sgr(&self) -> bool {
        false
    }

    pub fn drain_events(&self) -> Vec<PaneEvent> {
        self.events.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default()
    }
}
