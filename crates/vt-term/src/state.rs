//! Read-only accessors for state the terminal tracks but keeps private.
//!
//! These exist for the cross-process pane handoff, which must read a live
//! pane's complete state without disturbing it. Every method here is a pure
//! read: no `&mut self`, nothing consumed. Contrast `take_title`/`take_output`,
//! which are one-shot and would steal state from the host if used here.

use crate::{Cell, Charset, Term};

/// The DECSC saved-cursor state, unpacked from its tuple.
///
/// Element order verified against both write and read sites in `lib.rs`'s
/// `esc_dispatch`: `b'7'` (DECSC) writes
/// `(self.row, self.col, self.pen, self.origin, self.pending_wrap, self.charsets)`,
/// and `b'8'` (DECRC) destructures the same tuple back onto those five fields plus
/// charsets, in that order. Note the 5th element is `pending_wrap` — the deferred
/// end-of-line wrap flag — NOT `autowrap` (DECAWM, a separate, unsaved mode field).
/// `swap_alt`'s `saved_screen` tuple confirms the same convention independently: it
/// saves `pending_wrap` (not `autowrap`) alongside cursor/pen/charsets.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SavedCursor {
    pub row: usize,
    pub col: usize,
    pub pen: Cell,
    pub origin: bool,
    /// The deferred-wrap flag captured at DECSC time (see [`Term::pending_wrap`]).
    pub pending_wrap: bool,
    pub charsets: [Charset; 4],
}

impl Term {
    /// The current SGR state — the template applied to the NEXT cell written.
    pub fn pen(&self) -> Cell {
        self.pen
    }

    /// The DECSTBM scrolling region as inclusive 0-based rows.
    pub fn margins(&self) -> (usize, usize) {
        (self.scroll_top, self.scroll_bottom)
    }

    pub fn autowrap(&self) -> bool {
        self.autowrap
    }

    pub fn origin(&self) -> bool {
        self.origin
    }

    pub fn insert_mode(&self) -> bool {
        self.insert_mode
    }

    pub fn newline_mode(&self) -> bool {
        self.newline_mode
    }

    /// The deferred-wrap flag: the cursor sits past the last column and the
    /// next printable character wraps first. Dropping it misplaces output.
    pub fn pending_wrap(&self) -> bool {
        self.pending_wrap
    }

    pub fn utf8_mouse(&self) -> bool {
        self.utf8_mouse
    }

    pub fn urgency_hints(&self) -> bool {
        self.urgency_hints
    }

    /// The G0..G3 character-set designations.
    pub fn charsets(&self) -> [Charset; 4] {
        self.charsets
    }

    /// Which G-set GL is currently locked to (0..=3).
    pub fn gl(&self) -> usize {
        self.gl
    }

    /// The DECSC saved cursor. Always present — it has a defined initial value
    /// rather than being optional, matching DECRC's behaviour before any DECSC.
    pub fn saved_cursor(&self) -> SavedCursor {
        // Element order confirmed against the DECSC (`b'7'`) / DECRC (`b'8'`) arms of
        // `esc_dispatch` in lib.rs: row, col, pen, origin, pending_wrap, charsets.
        let (row, col, pen, origin, pending_wrap, charsets) = self.saved_cursor;
        SavedCursor { row, col, pen, origin, pending_wrap, charsets }
    }

    /// True when a screen is being held aside — i.e. the alt screen is active
    /// and the primary is saved, or vice versa.
    pub fn has_inactive_screen(&self) -> bool {
        self.saved_screen.is_some()
    }

    /// How many rows the held-aside screen has, if there is one.
    pub fn inactive_rows(&self) -> Option<usize> {
        self.saved_screen.as_ref().map(|s| s.0.len())
    }

    /// One cell of the screen NOT currently displayed.
    ///
    /// Cell-at-a-time rather than returning the lines themselves: `Line` is a
    /// private type and must stay that way — handing it out would freeze an
    /// internal representation into the crate's public API for the sake of one
    /// caller that only ever reads cells.
    pub fn inactive_cell(&self, row: usize, col: usize) -> Option<Cell> {
        let saved = self.saved_screen.as_ref()?;
        saved.0.get(row).and_then(|line| line.cells.get(col)).copied()
    }
}

#[cfg(test)]
mod tests {
    use crate::Term;

    fn term_after(seq: &[u8]) -> Term {
        let mut t = Term::new(80, 24);
        t.feed(seq);
        t
    }

    #[test]
    fn margins_report_the_decstbm_region() {
        // DECSTBM 5;20 — rows are 1-based on the wire, 0-based internally.
        let t = term_after(b"\x1b[5;20r");
        let (top, bottom) = t.margins();
        assert_eq!((top, bottom), (4, 19), "DECSTBM 5;20 -> internal rows 4..=19");
    }

    #[test]
    fn margins_default_to_the_whole_screen() {
        let t = Term::new(80, 24);
        assert_eq!(t.margins(), (0, 23));
    }

    #[test]
    fn autowrap_and_origin_follow_their_dec_modes() {
        let t = term_after(b"\x1b[?7l\x1b[?6h"); // DECAWM off, DECOM on
        assert!(!t.autowrap());
        assert!(t.origin());
        let t = term_after(b"\x1b[?7h\x1b[?6l");
        assert!(t.autowrap());
        assert!(!t.origin());
    }

    #[test]
    fn insert_and_newline_modes_follow_their_ansi_modes() {
        let t = term_after(b"\x1b[4h\x1b[20h"); // IRM on, LNM on
        assert!(t.insert_mode());
        assert!(t.newline_mode());
        let t = term_after(b"\x1b[4l\x1b[20l");
        assert!(!t.insert_mode());
        assert!(!t.newline_mode());
    }

    #[test]
    fn pending_wrap_is_set_after_writing_the_last_column() {
        let mut t = Term::new(4, 2);
        t.feed(b"abcd"); // fills the row; cursor defers the wrap
        assert!(t.pending_wrap(), "the deferred-wrap flag must be visible");
        t.feed(b"e");
        assert!(!t.pending_wrap(), "and cleared once the wrap happens");
    }

    #[test]
    fn the_pen_carries_the_current_sgr_state() {
        let t = term_after(b"\x1b[1;3;38;5;200m");
        let pen = t.pen();
        assert!(pen.bold());
        assert!(pen.italic());
        assert_eq!(pen.fg, crate::Color::Indexed(200));
    }

    #[test]
    fn charsets_and_gl_report_designations() {
        // Designate DEC graphics into G1, then lock GL to G1 with SO.
        let t = term_after(b"\x1b)0\x0e");
        assert_eq!(t.gl(), 1, "SO locks GL to G1");
        assert_eq!(t.charsets()[1], crate::Charset::Special, "G1 designated to DEC graphics");
    }

    #[test]
    fn saved_cursor_round_trips_through_decsc_and_decrc() {
        let t = term_after(b"\x1b[5;10H\x1b[1m\x1b7");
        let sc = t.saved_cursor();
        assert_eq!((sc.row, sc.col), (4, 9), "DECSC captures the cursor position");
        assert!(sc.pen.bold(), "and the pen with it");
    }

    #[test]
    fn there_is_no_inactive_screen_until_the_alt_screen_is_entered() {
        let t = Term::new(80, 24);
        assert!(!t.has_inactive_screen());
        let t = term_after(b"\x1b[?1049h"); // enter alt screen
        assert!(t.has_inactive_screen(), "the primary screen is now the inactive one");
    }

    #[test]
    fn the_inactive_screen_holds_the_content_left_behind() {
        let mut t = Term::new(80, 24);
        t.feed(b"PRIMARY");
        // Switch to alt; primary is saved. `?1049h` does NOT itself move the cursor
        // (verified against `vt-conformance`'s "alt screen" diff case, which clears
        // and homes explicitly before writing) — real apps do that themselves, so
        // this test does too, rather than assuming a home the terminal doesn't do.
        t.feed(b"\x1b[?1049h\x1b[H");
        t.feed(b"ALT");
        assert_eq!(t.inactive_rows(), Some(24), "the saved screen is the same height");
        let first: String = (0..7).map(|c| t.inactive_cell(0, c).unwrap().c).collect();
        assert_eq!(first, "PRIMARY", "the saved screen keeps what was there");
        // And the live screen shows the alt content.
        let live: String = (0..3).map(|c| t.cell(0, c).c).collect();
        assert_eq!(live, "ALT");
    }

    #[test]
    fn every_accessor_is_a_pure_read() {
        // Calling them twice must give the same answer — nothing is consumed.
        let t = term_after(b"\x1b[1m\x1b[5;20r\x1b[?7l");
        assert_eq!(t.margins(), t.margins());
        assert_eq!(t.autowrap(), t.autowrap());
        assert_eq!(t.pen().fg, t.pen().fg);
        assert_eq!(t.charsets(), t.charsets());
    }
}
