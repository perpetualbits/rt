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
//! and precise per-line damage — now driven by vt-term's NATIVE damage tracking, so a
//! frame refreshes only the cells that changed (no whole-grid rebuild, no whole-grid diff).
//!
//! **Still simplified:** search is a plain substring scan (no regex); and any vt-term
//! reflow edge (see `docs/engine-divergence.md`) shows through here too.

use std::collections::VecDeque;
use std::io::Read;
use std::os::unix::io::{AsRawFd, FromRawFd, RawFd};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use alacritty_terminal::event::WindowSize;
use alacritty_terminal::tty::{self, EventedPty, EventedReadWrite, Options as PtyOptions, Shell};

use crate::palette::{self, Palette};
use crate::{
    CellAttrs, CellDamage, CursorPos, CursorShape, Damage, LineBounds, PaneEvent, SearchMatch,
    SnapCell, Snapshot,
};

/// Per-pane scrollback memory budget (bytes). The scrollback also honors the user's
/// configured line count, but evicts oldest-first once a pane's history estimate exceeds
/// this, so cranking the line slider to its ceiling can't turn one runaway pane into a
/// multi-GB allocation. 1 GiB is generous for real scrollback yet bounds the worst case.
const SCROLLBACK_MEMORY_BUDGET: usize = 1 << 30;

/// Cap on bytes queued to the writer thread but not yet written to the PTY. The channel is
/// otherwise unbounded, so a child that stops reading stdin while input keeps arriving (a
/// huge paste, or broadcast, into a stalled program) would grow it without limit — a
/// memory-exhaustion path. Past this, new input is dropped rather than queued: keystrokes
/// are tiny and never approach it, so only a pathological paste into a wedged child is
/// affected, and bounded memory beats OOM. [review RT-SEC-003]
const WRITE_QUEUE_MAX: usize = 8 << 20; // 8 MiB

/// A pane driven by the in-house `vt_term::Term`.
pub struct VtPane {
    /// The grid + parser, shared with the reader thread.
    term: Arc<Mutex<vt_term::Term>>,
    /// Raw PTY master fd, used for write/resize (owned by `pty`, valid while it lives).
    pty_fd: RawFd,
    /// The forked PTY. Kept alive so its `Drop` SIGHUPs and reaps the child; also polled
    /// each frame via `next_child_event` so the child is reaped promptly and its exit is
    /// detected by SIGCHLD (not just master EOF, which a backgrounded grandchild can hold
    /// open). Behind a `Mutex` because `next_child_event` needs `&mut`.
    pty: Mutex<tty::Pty>,
    /// GUI-facing event FIFO (Wakeup on new output, Exited on child exit).
    events: Arc<Mutex<VecDeque<PaneEvent>>>,
    /// Set once the child's exit (or crash) has been reported, so the SIGCHLD poll and the
    /// crash handler don't each emit a duplicate terminal event.
    exited: Arc<AtomicBool>,
    /// Set when the reader thread's parser panicked and was caught (pane crash isolation).
    /// A crashed pane is frozen: `write` drops input (there's no reader to drain it) rather
    /// than backing bytes up behind the dead parser.
    crashed: Arc<AtomicBool>,
    /// Queues input to the dedicated writer thread. A PTY-master write can block in the
    /// kernel when the child isn't reading stdin (a large paste into a paused program), so
    /// writes must NOT run on the GUI thread — they'd freeze the whole app. Sending here is
    /// non-blocking and never drops input (unbounded channel, FIFO order).
    input_tx: std::sync::mpsc::Sender<Vec<u8>>,
    /// Bytes queued to the writer thread but not yet written, so `write` can enforce
    /// `WRITE_QUEUE_MAX` and refuse to grow the channel without bound.
    queued_bytes: Arc<AtomicUsize>,
    /// Writer thread; detached — ends when `input_tx` drops (channel closes).
    _writer: std::thread::JoinHandle<()>,
    /// Set by the reader thread when new bytes have been applied (drives redraw).
    dirty: Arc<AtomicBool>,
    /// The persistent resolved grid: `render_snapshot` refreshes only the cells vt-term
    /// reports as damaged into it, then shares it into the returned snapshot via the `Arc`
    /// refcount. Kept across frames so an unchanged pane costs O(damaged cells), not O(grid).
    last_render: Mutex<Arc<Vec<Vec<SnapCell>>>>,
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

        // Honor the configured scrollback (the vendored backend already did) with a
        // per-pane memory budget so a large line setting degrades oldest-first rather than
        // exhausting RAM: with a maxed slider a single pane is capped near
        // SCROLLBACK_MEMORY_BUDGET, not the multi-GB the raw line count would imply.
        let mut term = vt_term::Term::new(cols, rows);
        term.set_scrollback(scrollback, SCROLLBACK_MEMORY_BUDGET);
        let term = Arc::new(Mutex::new(term));
        let events = Arc::new(Mutex::new(VecDeque::new()));
        let dirty = Arc::new(AtomicBool::new(true));
        let exited = Arc::new(AtomicBool::new(false));
        let crashed = Arc::new(AtomicBool::new(false));

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

        // Writer thread: blocking PTY-master writes off the GUI thread (see `input_tx`).
        // Its own dup of the master fd, owned for the thread's lifetime.
        let write_fd = unsafe { libc::dup(pty_fd) };
        if write_fd < 0 {
            let err = std::io::Error::last_os_error();
            unsafe { libc::close(read_fd) };
            return Err(err);
        }
        let (input_tx, input_rx) = std::sync::mpsc::channel::<Vec<u8>>();
        let queued_bytes = Arc::new(AtomicUsize::new(0));
        let queued_w = queued_bytes.clone();
        let writer = std::thread::spawn(move || {
            while let Ok(bytes) = input_rx.recv() {
                let mut off = 0;
                while off < bytes.len() {
                    // SAFETY: `write_fd` is a fresh dup we exclusively own here.
                    let ret = unsafe {
                        libc::write(write_fd, bytes[off..].as_ptr() as *const libc::c_void, bytes.len() - off)
                    };
                    if ret > 0 {
                        off += ret as usize;
                        continue;
                    }
                    match std::io::Error::last_os_error().raw_os_error() {
                        Some(libc::EINTR) => continue,
                        Some(libc::EAGAIN) => {
                            let mut pfd = libc::pollfd { fd: write_fd, events: libc::POLLOUT, revents: 0 };
                            unsafe { libc::poll(&mut pfd, 1, 100) };
                        }
                        _ => break, // EIO / peer gone: drop the rest of this chunk
                    }
                }
                queued_w.fetch_sub(bytes.len(), Ordering::AcqRel); // this chunk left the queue
            }
            unsafe { libc::close(write_fd) }; // channel closed (pane dropped): release the dup
        });

        let (term_r, events_r, dirty_r) = (term.clone(), events.clone(), dirty.clone());
        let reply_tx = input_tx.clone();
        let reply_queued = queued_bytes.clone();
        // A second set of handles for the crash handler below (the loop moves the `*_r` set).
        let (events_c, dirty_c, exited_c, crashed_c) =
            (events.clone(), dirty.clone(), exited.clone(), crashed.clone());
        let reader = std::thread::spawn(move || {
            // Isolate a parser/grid panic to THIS pane: `panic = "unwind"` lets us catch it,
            // mark the pane crashed, and let the GUI freeze it with a badge — instead of the
            // whole process (every other pane, an editor with unsaved work) aborting with it.
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                // SAFETY: `read_fd` is a fresh dup we exclusively own here.
                let mut file = unsafe { std::fs::File::from_raw_fd(read_fd) };
                let mut buf = [0u8; 65536];
                loop {
                    match file.read(&mut buf) {
                        Ok(0) => {
                            // Master EOF. Do NOT emit Exited here — the child may still be
                            // alive (it only closed its stdout), and even when it did exit we
                            // don't know the status. `drain_events`' `next_child_event`
                            // (SIGCHLD + try_wait) is the sole exit authority and carries the
                            // real code, so it can't race a premature unknown-status exit.
                            dirty_r.store(true, Ordering::Release);
                            break;
                        }
                        Ok(n) => {
                            let mut title = None;
                            let mut reply = Vec::new();
                            if let Ok(mut t) = term_r.lock() {
                                t.feed(&buf[..n]);
                                title = t.take_title(); // OSC 0/2 while holding the lock
                                reply = t.take_output(); // DSR/CPR, DA, DECRQM replies
                            }
                            // Answer terminal queries by writing straight back to the PTY,
                            // exactly as the vendored engine answers Event::PtyWrite
                            // (rt-engine lib.rs:300). Replies are tiny and MUST NOT be
                            // dropped (a swallowed CPR hangs the querying app), so unlike
                            // `write` this path skips the WRITE_QUEUE_MAX cap. It keeps the
                            // same queued_bytes accounting so the counter stays balanced.
                            if !reply.is_empty() {
                                reply_queued.fetch_add(reply.len(), Ordering::AcqRel);
                                let len = reply.len();
                                if reply_tx.send(reply).is_err() {
                                    reply_queued.fetch_sub(len, Ordering::AcqRel);
                                }
                            }
                            let mut q = events_r.lock().unwrap();
                            if let Some(t) = title {
                                q.push_back(PaneEvent::Title(t));
                            }
                            q.push_back(PaneEvent::Wakeup);
                            drop(q);
                            dirty_r.store(true, Ordering::Release);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(std::time::Duration::from_millis(4));
                        }
                        // `File::read` already retries EINTR internally; any other error means
                        // the master is gone. Same as EOF: leave exit detection to
                        // `next_child_event`.
                        Err(_) => {
                            dirty_r.store(true, Ordering::Release);
                            break;
                        }
                    }
                }
            }));
            if ran.is_err() {
                // The panic poisoned the term mutex; `lock_term` recovers the guard so the
                // pane's last grid stays readable. Mark the pane crashed (so `write` stops
                // feeding a dead reader) and tell the GUI once (dedup via `exited`, which
                // also suppresses a later Exited for this now-frozen pane).
                crashed_c.store(true, Ordering::Release);
                if !exited_c.swap(true, Ordering::AcqRel) {
                    events_c.lock().unwrap_or_else(|e| e.into_inner()).push_back(PaneEvent::Crashed);
                }
                dirty_c.store(true, Ordering::Release);
            }
        });

        Ok(VtPane {
            term,
            pty_fd,
            pty: Mutex::new(pty),
            events,
            exited,
            crashed,
            input_tx,
            queued_bytes,
            _writer: writer,
            dirty,
            last_render: Mutex::new(Arc::new(Vec::new())),
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

    /// Read this pane's live state into the cross-process wire model.
    ///
    /// A pure read under the term lock: nothing is written, nothing consumed, no
    /// descriptor moves. Returns the pane plus its scrollback newest-first.
    pub fn export(
        &self,
        pane_uid: u64,
        scrollback_budget: usize,
    ) -> (rt_handoff::pane::PaneWire, Vec<rt_handoff::grid::Line>) {
        let term = self.lock_term();
        crate::handoff::export_term(&term, pane_uid, self.pid().unwrap_or(0), scrollback_budget)
    }

    /// Lock the shared term, recovering the guard even if a prior parser panic poisoned the
    /// mutex. Pane crash isolation (panic = "unwind") catches a parser/grid panic on the
    /// reader thread, which drops its guard mid-update and poisons the lock; recovering it
    /// keeps the pane's last grid readable so a crashed pane freezes with a badge instead of
    /// cascading the panic into the GUI thread (whose `.unwrap()` would abort everything).
    fn lock_term(&self) -> std::sync::MutexGuard<'_, vt_term::Term> {
        self.term.lock().unwrap_or_else(|e| e.into_inner())
    }

    pub fn write(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        // A crashed pane has no live reader to drain its output; feeding it more input just
        // backs bytes up in the kernel behind a dead parser. Drop input to the frozen pane.
        if self.crashed.load(Ordering::Acquire) {
            return;
        }
        // Refuse to queue past WRITE_QUEUE_MAX: if the child has stalled and the writer
        // thread can't drain, growing the channel without bound is an OOM path. Dropping
        // here (rather than blocking, which would re-freeze the GUI) bounds memory; only a
        // pathological paste into a wedged child ever reaches the cap. (The GUI thread is
        // the primary writer of `queued_bytes`, but the reader thread also adds atomically
        // for query replies — see the reader loop above — so this check-then-add can
        // momentarily overshoot WRITE_QUEUE_MAX by a few reply bytes; benign and bounded,
        // and still needs no CAS since only this thread ever subtracts here.)
        if self.queued_bytes.load(Ordering::Acquire) + bytes.len() > WRITE_QUEUE_MAX {
            return;
        }
        self.queued_bytes.fetch_add(bytes.len(), Ordering::AcqRel);
        // Hand the bytes to the writer thread and return immediately. The actual
        // (potentially blocking) PTY-master write happens there, never on the GUI thread —
        // a big paste into a child that isn't reading stdin must not freeze the app. The
        // channel preserves order; a send error means the writer is gone (pane dropped).
        if self.input_tx.send(bytes.to_vec()).is_err() {
            self.queued_bytes.fetch_sub(bytes.len(), Ordering::AcqRel); // writer gone; undo
        }
    }

    pub fn resize(&mut self, cols: usize, rows: usize) {
        if cols == self.cols && rows == self.rows {
            return;
        }
        { let mut t = self.lock_term();
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

    /// Materialise the visible grid into a [`Snapshot`], folding bold/dim/inverse/hidden
    /// into the resolved colours the way alacritty's `capture_locked` does.
    fn capture(&self) -> Snapshot {
        let t = self.lock_term();
        let cols = t.cols();
        let grid = resolve_full(&t, &self.palette);
        Snapshot { cols, rows: Arc::new(grid), cursor: capture_cursor(&t), damage: Damage::default() }
    }

    pub fn snapshot(&self) -> Snapshot {
        self.capture()
    }

    /// Produce a snapshot, using vt-term's NATIVE per-frame damage to refresh only the cells
    /// that changed since the last call — no whole-grid rebuild and no whole-grid diff (both
    /// were O(rows×cols)). `last_render` holds the persistent resolved grid, shared into the
    /// returned snapshot via the `Arc` refcount; `Arc::make_mut` mutates it in place because
    /// the previous frame's snapshot is dropped before this runs (it clones only if some
    /// caller retains a snapshot across frames). Call once per pane per frame.
    pub fn render_snapshot(&self) -> Snapshot {
        self.dirty.store(false, Ordering::Release);
        let mut t = self.lock_term();
        let cols = t.cols();
        let dmg = t.take_damage();
        let cursor = capture_cursor(&t);
        let mut last = self.last_render.lock().unwrap_or_else(|e| e.into_inner());
        let grid = Arc::make_mut(&mut last);
        let damage = apply_damage(&t, &self.palette, dmg, grid);
        Snapshot { cols, rows: Arc::clone(&last), cursor, damage }
    }

    pub fn set_palette(&mut self, palette: Palette) {
        self.palette = palette;
    }

    // ── Scrollback viewport ────────────────────────────────────────────────────
    pub fn scroll(&self, delta: isize) {
        { let mut t = self.lock_term();
            t.scroll_display(delta as i32); // positive = up into history
        }
    }
    pub fn scroll_to_bottom(&self) {
        { let mut t = self.lock_term();
            t.scroll_to_bottom_view();
        }
    }
    pub fn scroll_to_line(&self, line: i32) {
        // Centre absolute `line` in the viewport, matching the alacritty backend.
        { let mut t = self.lock_term();
            let screen = t.rows() as i32;
            let history = t.history_size() as i32;
            let current = t.display_offset() as i32;
            let desired = (screen / 2 - line).clamp(0, history);
            t.scroll_display(desired - current);
        }
    }
    pub fn scroll_info(&self) -> (usize, usize, usize) {
        let t = self.lock_term();
        (t.display_offset(), t.history_size(), t.rows())
    }
    /// Whether visible row `row` soft-wraps into the next (its last cell has WRAPLINE).
    pub fn line_wrapped(&self, row: usize) -> bool {
        let t = self.lock_term();
        let cols = t.cols();
        if cols == 0 || row >= t.rows() {
            return false;
        }
        t.cell_at(row as i32, cols - 1).wrapline()
    }
    pub fn line_bounds(&self) -> LineBounds {
        let t = self.lock_term();
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
        let t = self.lock_term();
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
        Snapshot { cols, rows: Arc::new(out), cursor: None, damage: Damage::default() }
    }

    // ── Selection ──────────────────────────────────────────────────────────────
    /// Extract selected text between two absolute-grid endpoints, trimming trailing
    /// blanks per line (linewise) or per rectangle (block), matching the alac backend.
    pub fn selection_text(&self, anchor: (usize, i32), head: (usize, i32), block: bool) -> String {
        let t = self.lock_term();
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
                if !cell.spacer() {
                    s.push(cell.c);
                }
            }
            // A soft-wrapped line (WRAPLINE on its last cell) continues into the next, so
            // join without trimming or a newline — matching alacritty's copy behaviour.
            let wrapped = !block && line != h.1 && t.cell_at(line, cols - 1).wrapline();
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
        let t = self.lock_term();
        let cols = t.cols();
        let want = if case_sensitive { needle.to_string() } else { needle.to_lowercase() };
        let mut hits = Vec::new();
        for abs in t.topmost()..=t.bottommost() {
            let mut text = String::with_capacity(cols);
            for c in 0..cols {
                let cell = t.cell_at(abs, c);
                text.push(if cell.spacer() { '\0' } else { cell.c });
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
        resolve_cell(&self.palette, t.cell_at(abs, col))
    }

    /// Whether this pane is frozen after a caught panic (reader parser OR GUI render).
    pub fn is_crashed(&self) -> bool {
        self.crashed.load(Ordering::Acquire)
    }

    /// Record that the GUI's `render_snapshot` panicked for this pane and was caught: mark
    /// it crashed (so the render loop skips it — no per-frame panic storm) and emit
    /// `Crashed` once so it gets the same `[crashed]` badge the reader-thread path gives.
    pub fn note_render_crash(&self) {
        self.crashed.store(true, Ordering::Release);
        if !self.exited.swap(true, Ordering::AcqRel) {
            self.events.lock().unwrap_or_else(|e| e.into_inner()).push_back(PaneEvent::Crashed);
        }
        self.dirty.store(true, Ordering::Release);
    }

    // ── Mode queries the GUI uses to encode input. ──
    pub fn app_cursor_keys(&self) -> bool {
        self.lock_term().app_cursor()
    }
    pub fn is_alt_screen(&self) -> bool {
        self.lock_term().alt_screen()
    }
    pub fn wants_mouse(&self) -> bool {
        self.lock_term().wants_mouse()
    }
    pub fn wants_motion(&self) -> bool {
        self.lock_term().wants_motion()
    }
    pub fn mouse_sgr(&self) -> bool {
        self.lock_term().mouse_sgr()
    }
    pub fn bracketed_paste(&self) -> bool {
        self.lock_term().bracketed_paste()
    }
    pub fn focus_events(&self) -> bool {
        self.lock_term().focus_events()
    }
    pub fn alt_scroll(&self) -> bool {
        self.lock_term().alt_scroll()
    }

    pub fn drain_events(&self) -> Vec<PaneEvent> {
        // Reap the child and detect its exit via SIGCHLD — reliable even when a
        // backgrounded grandchild holds the PTY slave open (so master EOF never comes).
        // `next_child_event` does a non-blocking read of the signal pipe and `try_wait()`s
        // the child, so this also clears zombies promptly rather than only at drop.
        if let Ok(mut pty) = self.pty.lock() {
            if let Some(tty::ChildEvent::Exited(status)) = pty.next_child_event() {
                // This is the sole exit authority and carries the real $?-style status, so
                // there's no premature unknown-status Exited to race or upgrade.
                let code = status.and_then(crate::exit_code);
                if !self.exited.swap(true, Ordering::AcqRel) {
                    self.events.lock().unwrap_or_else(|e| e.into_inner()).push_back(PaneEvent::Exited(code));
                }
            }
        }
        self.events.lock().map(|mut q| q.drain(..).collect()).unwrap_or_else(|e| e.into_inner().drain(..).collect())
    }
}

/// Resolve a vt-term colour to concrete RGB through the palette.
fn resolve_rgb(palette: &Palette, c: vt_term::Color, is_fg: bool) -> palette::Rgb {
    match c {
        vt_term::Color::Default => {
            if is_fg {
                palette.fg
            } else {
                palette.bg
            }
        }
        vt_term::Color::Indexed(i) => palette.indexed(i),
        vt_term::Color::Rgb(r, g, b) => [r, g, b],
    }
}

/// Resolve one vt-term cell to a drawable [`SnapCell`], folding bold/dim/inverse/hidden into
/// the colours (matching alacritty's `capture_locked`).
fn resolve_cell(palette: &Palette, cell: vt_term::Cell) -> SnapCell {
    let mut fg = resolve_rgb(palette, cell.fg, true);
    let mut bg = resolve_rgb(palette, cell.bg, false);
    if cell.dim() {
        fg = palette::dim(fg);
    }
    if cell.inverse() {
        std::mem::swap(&mut fg, &mut bg);
    }
    if cell.hidden() {
        fg = bg;
    }
    SnapCell {
        c: cell.c,
        fg,
        bg,
        attrs: CellAttrs {
            bold: cell.bold(),
            underline: cell.underline(),
            italic: cell.italic(),
            strikeout: cell.strikeout(),
        },
    }
}

/// Resolve the whole visible grid (viewport row `r` shows absolute line `r - display_offset`).
fn resolve_full(t: &vt_term::Term, palette: &Palette) -> Vec<Vec<SnapCell>> {
    let (cols, rows) = (t.cols(), t.rows());
    let offset = t.display_offset() as i32;
    (0..rows)
        .map(|r| (0..cols).map(|c| resolve_cell(palette, t.cell_at(r as i32 - offset, c))).collect())
        .collect()
}

/// The cursor position for a snapshot: drawn only when visible AND at the live bottom (not
/// scrolled back into history).
fn capture_cursor(t: &vt_term::Term) -> Option<CursorPos> {
    if !(t.cursor_visible() && t.display_offset() == 0) {
        return None;
    }
    let (col, line) = t.cursor();
    let shape = match t.cursor_shape() {
        vt_term::CursorShape::Block => CursorShape::Block,
        vt_term::CursorShape::Underline => CursorShape::Underline,
        vt_term::CursorShape::Beam => CursorShape::Beam,
    };
    (line < t.rows() && col < t.cols()).then_some(CursorPos { col, line, shape })
}

/// Refresh only the cells vt-term marked damaged into `resolved`, returning the render-side
/// [`Damage`]. Rebuilds the whole grid on `Full` damage or a dimension change (which vt-term
/// also reports as `Full`), so `resolved` always ends the call equal to a full resolve.
fn apply_damage(
    t: &vt_term::Term,
    palette: &Palette,
    dmg: vt_term::Damage,
    resolved: &mut Vec<Vec<SnapCell>>,
) -> Damage {
    let (cols, rows) = (t.cols(), t.rows());
    let dims_ok = resolved.len() == rows && resolved.first().map_or(false, |r| r.len() == cols);
    // Re-resolve the cells in `spans` from the (already scroll-shifted, if any) term, updating
    // `resolved` in place and collecting the render-side `CellDamage`.
    fn repaint(t: &vt_term::Term, palette: &Palette, spans: &[vt_term::LineDamage], resolved: &mut [Vec<SnapCell>], cols: usize, rows: usize) -> Vec<CellDamage> {
        let mut out = Vec::with_capacity(spans.len());
        for s in spans {
            if s.line >= rows || cols == 0 {
                continue;
            }
            let right = s.right.min(cols - 1);
            for c in s.left..=right {
                // Native damage never marks while scrolled back (it reports Full), so the
                // viewport row IS the absolute line: `cell_at(line, c)`.
                resolved[s.line][c] = resolve_cell(palette, t.cell_at(s.line as i32, c));
            }
            out.push(CellDamage { line: s.line, left: s.left, right });
        }
        out
    }
    match dmg {
        vt_term::Damage::Spans(spans) if dims_ok => {
            Damage::Lines(repaint(t, palette, &spans, resolved, cols, rows))
        }
        vt_term::Damage::Scroll { lines, spans } if dims_ok && lines < rows => {
            // Blit the resolved grid up by `lines` to mirror the engine's scroll — the top
            // `rows-lines` rows are now correct; the bottom `lines` (exposed) rows hold stale
            // data but are in `spans` (marked dirty full-width), so `repaint` fixes them.
            resolved.rotate_left(lines);
            Damage::Scroll { lines, spans: repaint(t, palette, &spans, resolved, cols, rows) }
        }
        // Full damage, first frame, a resize, or an un-blittable scroll: rebuild wholesale.
        _ => {
            *resolved = resolve_full(t, palette);
            Damage::Full
        }
    }
}

#[cfg(test)]
mod damage_incremental_tests {
    use super::*;
    use vt_term::Term;

    /// Incrementally refreshing only the damaged cells must yield exactly the same resolved
    /// grid as re-resolving the whole thing — over a mix of print, SGR, erase, scroll, wide
    /// glyphs, and insert/delete. This is the SnapCell-level check that vt-term's native
    /// damage is complete enough to drive `render_snapshot`'s incremental path.
    #[test]
    fn incremental_apply_matches_full_resolve() {
        let palette = Palette::xterm();
        let mut t = Term::new(24, 8);
        let _ = t.take_damage(); // drain the initial full frame
        let mut resolved = resolve_full(&t, &palette);
        assert_eq!(resolved, resolve_full(&t, &palette));

        let scripts: &[&[u8]] = &[
            b"hello world",
            b"\r\n\x1b[1mBOLD\x1b[0m\x1b[3mital",
            b"\x1b[2J\x1b[H",                      // clear + home
            b"line1\r\nline2\r\nline3",
            b"\x1b[5;3H\x1b[31mX\x1b[7mY",         // move + colour + inverse
            "wide 界 chars 世".as_bytes(),
            b"\x1b[2K",                            // erase whole line
            b"\x1b[2;1H\x1b[3P",                   // delete chars
            b"\x1b[4;1H\x1b[3@zzz",                // insert chars then type
            b"\x1b[3;1H\x1b[L\x1b[M",              // insert then delete a line
            b"\x1b[H\x1b[Jend",                    // erase-below
            b"a\r\nb\r\nc\r\nd\r\ne\r\nf\r\ng\r\nh\r\ni\r\nj", // force scroll
        ];
        for s in scripts {
            t.feed(s);
            let dmg = t.take_damage();
            let render_dmg = apply_damage(&t, &palette, dmg, &mut resolved);
            let full = resolve_full(&t, &palette);
            assert_eq!(
                resolved, full,
                "incremental != full after {:?} (render damage {:?})",
                String::from_utf8_lossy(s), render_dmg,
            );
        }
    }

    /// The render-side damage contract: a fresh pane's first frame is Full; an idle frame is
    /// empty `Lines`; a localized write reports just that row's span. (What `render_snapshot`
    /// exposes to the renderer, checked without a PTY.)
    #[test]
    fn apply_damage_reports_full_then_lines() {
        let palette = Palette::xterm();
        let mut t = Term::new(10, 4);
        let mut resolved: Vec<Vec<SnapCell>> = Vec::new(); // empty, like a just-spawned pane

        // First frame: nothing resolved yet → rebuild → Full.
        let dmg = t.take_damage();
        let d0 = apply_damage(&t, &palette, dmg, &mut resolved);
        assert_eq!(d0, Damage::Full);
        assert_eq!(resolved.len(), 4);

        // Idle frame: no mutations, cursor unmoved → empty Lines.
        let dmg = t.take_damage();
        let d1 = apply_damage(&t, &palette, dmg, &mut resolved);
        assert_eq!(d1, Damage::Lines(vec![]));

        // A localized write on row 0 → Lines covering (only) row 0.
        t.feed(b"\x1b[1;1Hhi");
        let dmg = t.take_damage();
        let d2 = apply_damage(&t, &palette, dmg, &mut resolved);
        match &d2 {
            Damage::Lines(l) => {
                assert!(l.iter().any(|c| c.line == 0), "row 0 must be damaged: {d2:?}");
                assert!(!l.iter().any(|c| c.line == 3), "untouched row 3 must not be: {d2:?}");
            }
            _ => panic!("expected Lines, got {d2:?}"),
        }
        assert_eq!(resolved, resolve_full(&t, &palette));
    }
}

#[cfg(test)]
mod scroll_damage_tests {
    use super::*;
    use vt_term::Term;

    /// A full-screen scroll must report `Damage::Scroll` and leave the resolved grid equal to
    /// a full re-resolve — i.e. the rotate-up blit + span repaint reproduces the true grid.
    #[test]
    fn scroll_shifts_resolved_grid_and_reports_scroll() {
        let palette = Palette::xterm();
        let mut t = Term::new(10, 4);
        let _ = t.take_damage();
        let mut resolved = resolve_full(&t, &palette);
        t.feed(b"a\r\nb\r\nc\r\nd");
        let dmg = t.take_damage();
        let _ = apply_damage(&t, &palette, dmg, &mut resolved);
        assert_eq!(resolved, resolve_full(&t, &palette));

        t.feed(b"\r\ne"); // scroll up one, print e on the exposed row
        let dmg = t.take_damage();
        let render = apply_damage(&t, &palette, dmg, &mut resolved);
        assert!(matches!(render, Damage::Scroll { lines: 1, .. }), "got {render:?}");
        assert_eq!(resolved, resolve_full(&t, &palette), "blit+repaint must equal a full resolve");
    }
}
