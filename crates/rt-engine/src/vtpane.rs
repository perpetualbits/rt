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
//! **Feature parity with the alacritty backend:** scrollback viewport (scroll / scroll-to
//! / `snapshot_lines`), linewise + block selection (wrap-aware), buffer search, mouse
//! reporting (DECSET 1000/1002/1003/1006), DECSCUSR cursor shape, OSC 0/2 window title,
//! and precise per-line damage (computed by diffing successive rendered grids, since
//! vt-term has no built-in damage tracking).
//!
//! **Still simplified:** damage is diff-derived rather than parser-tracked (correct, one
//! grid-diff per frame); search is a plain substring scan (no regex); and any vt-term
//! reflow edge (see `docs/engine-divergence.md`) shows through here too.

use std::collections::VecDeque;
use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::tty::{self, EventedReadWrite, Options as PtyOptions, Shell};

use crate::palette::{self, Palette};
use crate::{
    CellAttrs, CellDamage, CursorPos, CursorShape, Damage, LineBounds, PaneEvent, SearchMatch,
    SnapCell, Snapshot,
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
    /// The grid as of the last `render_snapshot`, diffed against the next to compute
    /// precise per-line damage (vt-term has no built-in damage tracking).
    last_render: Mutex<Vec<Vec<SnapCell>>>,
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
                        let mut title = None;
                        if let Ok(mut t) = term_r.lock() {
                            t.feed(&buf[..n]);
                            title = t.take_title(); // OSC 0/2 while holding the lock
                        }
                        let mut q = events_r.lock().unwrap();
                        if let Some(t) = title {
                            q.push_back(PaneEvent::Title(t));
                        }
                        q.push_back(PaneEvent::Wakeup);
                        drop(q);
                        dirty_r.store(true, Ordering::Release);
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
            last_render: Mutex::new(Vec::new()),
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
        let offset = t.display_offset() as i32;
        let blank = SnapCell { c: ' ', fg: self.palette.fg, bg: self.palette.bg, attrs: CellAttrs::default() };
        let mut grid = vec![vec![blank.clone(); cols]; rows];
        for r in 0..rows {
            for col in 0..cols {
                // Viewport row r shows absolute line `r - offset` (offset>0 = scrolled up).
                grid[r][col] = self.snapcell(&t, r as i32 - offset, col);
            }
        }
        // Cursor is only drawn when live (not scrolled back into history).
        let cursor = if t.cursor_visible() && offset == 0 {
            let (col, line) = t.cursor();
            let shape = match t.cursor_shape() {
                vt_term::CursorShape::Block => CursorShape::Block,
                vt_term::CursorShape::Underline => CursorShape::Underline,
                vt_term::CursorShape::Beam => CursorShape::Beam,
            };
            (line < rows && col < cols).then_some(CursorPos { col, line, shape })
        } else {
            None
        };
        Snapshot { cols, rows: grid, cursor, damage: Damage::default() }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.capture()
    }

    /// Capture the grid and compute precise per-line damage by diffing it against the
    /// previously-rendered grid — vt-term has no built-in damage, but a diff of the actual
    /// output can't miss a change. `Full` on the first frame or a size change; otherwise
    /// `Lines` with each changed row's `left..=right` span (empty when nothing changed, so
    /// an idle pane reports no damage). Call once per pane per frame.
    pub fn render_snapshot(&self) -> Snapshot {
        self.dirty.store(false, Ordering::Release);
        let snap = self.capture();
        let mut last = self.last_render.lock().unwrap();
        let same_dims =
            last.len() == snap.rows.len() && last.first().map_or(false, |r| r.len() == snap.cols);
        let damage = if !same_dims {
            Damage::Full
        } else {
            let mut lines = Vec::new();
            for (i, (new, old)) in snap.rows.iter().zip(last.iter()).enumerate() {
                if new != old {
                    let left = (0..new.len()).find(|&c| new[c] != old[c]).unwrap_or(0);
                    let right = (0..new.len()).rev().find(|&c| new[c] != old[c]).unwrap_or(0);
                    lines.push(CellDamage { line: i, left, right });
                }
            }
            Damage::Lines(lines)
        };
        *last = snap.rows.clone();
        Snapshot { cols: snap.cols, rows: snap.rows, cursor: snap.cursor, damage }
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    // ── Scrollback viewport ────────────────────────────────────────────────────
    pub fn scroll(&self, delta: isize) {
        if let Ok(mut t) = self.term.lock() {
            t.scroll_display(delta as i32); // positive = up into history
        }
    }
    pub fn scroll_to_bottom(&self) {
        if let Ok(mut t) = self.term.lock() {
            t.scroll_to_bottom_view();
        }
    }
    pub fn scroll_to_line(&self, line: i32) {
        // Centre absolute `line` in the viewport, matching the alacritty backend.
        if let Ok(mut t) = self.term.lock() {
            let screen = t.rows() as i32;
            let history = t.history_size() as i32;
            let current = t.display_offset() as i32;
            let desired = (screen / 2 - line).clamp(0, history);
            t.scroll_display(desired - current);
        }
    }
    pub fn scroll_info(&self) -> (usize, usize, usize) {
        let t = self.term.lock().unwrap();
        (t.display_offset(), t.history_size(), t.rows())
    }
    pub fn line_bounds(&self) -> LineBounds {
        let t = self.term.lock().unwrap();
        LineBounds {
            topmost: t.topmost(),
            bottommost: t.bottommost(),
            screen_lines: t.rows(),
            cols: t.cols(),
        }
    }
    /// Absolute lines `[top, top+rows)` for scrollback rendering; lines outside the
    /// readable range come back blank. Matches the alacritty backend's `snapshot_lines`.
    pub fn snapshot_lines(&self, top: i32, rows: usize) -> Snapshot {
        let t = self.term.lock().unwrap();
        let cols = t.cols();
        let (topmost, bottommost) = (t.topmost(), t.bottommost());
        let mut out = Vec::with_capacity(rows);
        for r in 0..rows {
            let abs = top + r as i32;
            let mut line = vec![SnapCell::blank(); cols];
            if abs >= topmost && abs <= bottommost {
                for c in 0..cols {
                    line[c] = self.snapcell(&t, abs, c);
                }
            }
            out.push(line);
        }
        Snapshot { cols, rows: out, cursor: None, damage: Damage::default() }
    }

    // ── Selection ──────────────────────────────────────────────────────────────
    /// Extract selected text between two absolute-grid endpoints, trimming trailing
    /// blanks per line (linewise) or per rectangle (block), matching the alac backend.
    pub fn selection_text(&self, anchor: (usize, i32), head: (usize, i32), block: bool) -> String {
        let t = self.term.lock().unwrap();
        let cols = t.cols();
        // Order the endpoints top-to-bottom (then left-to-right on the same line).
        let (mut a, mut h) = (anchor, head);
        if (h.1, h.0) < (a.1, a.0) {
            std::mem::swap(&mut a, &mut h);
        }
        let mut out = String::new();
        for line in a.1..=h.1 {
            let (lo, hi) = if block {
                (a.0.min(h.0), a.0.max(h.0))
            } else if a.1 == h.1 {
                (a.0, h.0)
            } else if line == a.1 {
                (a.0, cols - 1)
            } else if line == h.1 {
                (0, h.0)
            } else {
                (0, cols - 1)
            };
            let mut s = String::new();
            for c in lo..=hi.min(cols - 1) {
                let cell = t.cell_at(line, c);
                if !cell.spacer {
                    s.push(cell.c);
                }
            }
            // A soft-wrapped line (WRAPLINE on its last cell) continues into the next, so
            // join without trimming or a newline — matching alacritty's copy behaviour.
            let wrapped = !block && line != h.1 && t.cell_at(line, cols - 1).wrapline;
            if wrapped {
                out.push_str(&s);
            } else {
                out.push_str(s.trim_end());
                if line != h.1 {
                    out.push('\n');
                }
            }
        }
        out
    }

    // ── Search ─────────────────────────────────────────────────────────────────
    /// Scan every readable line for `needle`, returning one match per occurrence.
    pub fn search(&self, needle: &str, case_sensitive: bool) -> Vec<SearchMatch> {
        if needle.is_empty() {
            return Vec::new();
        }
        let t = self.term.lock().unwrap();
        let cols = t.cols();
        let want = if case_sensitive { needle.to_string() } else { needle.to_lowercase() };
        let mut hits = Vec::new();
        for abs in t.topmost()..=t.bottommost() {
            let mut text = String::with_capacity(cols);
            for c in 0..cols {
                let cell = t.cell_at(abs, c);
                text.push(if cell.spacer { '\0' } else { cell.c });
            }
            let hay = if case_sensitive { text.clone() } else { text.to_lowercase() };
            let mut from = 0;
            while let Some(rel) = hay[from..].find(&want) {
                let start = from + rel;
                // byte offset → column: count chars before `start`.
                let col = hay[..start].chars().count();
                hits.push(SearchMatch { line: abs, col, len: want.chars().count() });
                from = start + want.len().max(1);
            }
        }
        hits
    }

    /// One resolved cell at absolute line/col, for history snapshots.
    fn snapcell(&self, t: &vt_term::Term, abs: i32, col: usize) -> SnapCell {
        let cell = t.cell_at(abs, col);
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
        SnapCell {
            c: cell.c,
            fg,
            bg,
            attrs: CellAttrs { bold: a.bold, underline: a.underline, italic: a.italic, strikeout: a.strikeout },
        }
    }

    // ── Mode queries the GUI uses to encode input. ──
    pub fn app_cursor_keys(&self) -> bool {
        self.term.lock().map(|t| t.app_cursor()).unwrap_or(false)
    }
    pub fn is_alt_screen(&self) -> bool {
        self.term.lock().map(|t| t.alt_screen()).unwrap_or(false)
    }
    pub fn wants_mouse(&self) -> bool {
        self.term.lock().map(|t| t.wants_mouse()).unwrap_or(false)
    }
    pub fn wants_motion(&self) -> bool {
        self.term.lock().map(|t| t.wants_motion()).unwrap_or(false)
    }
    pub fn mouse_sgr(&self) -> bool {
        self.term.lock().map(|t| t.mouse_sgr()).unwrap_or(false)
    }
    pub fn bracketed_paste(&self) -> bool {
        self.term.lock().map(|t| t.bracketed_paste()).unwrap_or(false)
    }
    pub fn focus_events(&self) -> bool {
        self.term.lock().map(|t| t.focus_events()).unwrap_or(false)
    }
    pub fn alt_scroll(&self) -> bool {
        self.term.lock().map(|t| t.alt_scroll()).unwrap_or(false)
    }

    pub fn drain_events(&self) -> Vec<PaneEvent> {
        self.events.lock().map(|mut q| q.drain(..).collect()).unwrap_or_default()
    }
}
