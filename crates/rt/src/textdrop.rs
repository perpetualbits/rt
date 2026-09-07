//! Text dragged in from **another application** — the browser-selection drop.
//!
//! Select a paragraph in Chrome or Firefox, drag it onto an rt pane, let go:
//! the text is inserted as if pasted. gnome-terminal and terminator do this;
//! Alacritty does not, and rt did not. This module is the part of that feature
//! that has no platform in it.
//!
//! **Not** `crate::dragdrop`, which is rt's INTERNAL pane/tab drag — a
//! completely different gesture that never leaves the process. The two share
//! only the cue painter (`chrome::dragdrop`), deliberately, so a drop cue looks
//! like a drop cue whatever is being dropped.
//!
//! The platform receivers (macOS `NSDraggingDestination`, and the Wayland/X11
//! equivalents) cannot be unit-tested without a compositor and a human hand.
//! The two DECISIONS they need can be, and they are the whole of this file:
//!
//! * [`resolve`] — given a drop point and the window's layout, which pane
//!   receives it, or none?
//! * [`payload`] — given the dropped text and the target pane's own
//!   bracketed-paste state, what bytes reach the pty?
//!
//! That split is the house pattern (`wgpu_frame`, `vibrancy_policy`,
//! `scale_policy`, `chrome_scale`, `cpu_heat`, `app_bundle`): the decision is a
//! pure module Linux CI compiles and tests, the syscalls are a thin
//! `cfg`-gated file around it.

use rt_core::{PaneId, Rect, TabBar};

/// One turn's worth of news from a platform receiver, in the only two facts the
/// app needs: where a foreign text drag is hovering (with the label for its
/// chip), and a drop that has just completed (where, and the text).
///
/// Every receiver — the macOS `NSDraggingDestination`, and the X11/Wayland ones
/// — reduces to this before `App::apply_text_drop` sees it, so the behaviour
/// cannot fork per platform. Positions are in the space winit reports the
/// pointer in: physical pixels from the top-left of the window's content area.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct DropNews {
    pub hover: Option<((f32, f32), String)>,
    pub dropped: Option<((f32, f32), String)>,
}

impl DropNews {
    /// Nothing to tell the app — the common case, and worth checking before it
    /// re-derives the window's layout.
    pub fn is_empty(&self) -> bool {
        self.hover.is_none() && self.dropped.is_none()
    }
}

/// A resolved text drop: which pane takes the text, and the rectangle to
/// highlight while the pointer hovers.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TextTarget {
    /// The pane under the cursor — the one that gets the text.
    pub pane: PaneId,
    /// Its rect, painted as the drop cue (see `chrome::dragdrop::draw`).
    pub cue: Rect,
}

/// Which pane a text drop at `cursor` lands in — the pane **under the pointer**,
/// not the focused one. `None` means "refuse this drop": the text is dropped on
/// the floor rather than inserted somewhere the user did not aim at.
///
/// `panes`/`tab_bars` are the window's current visible geometry (from
/// `Session::visible_rects`/`Session::tab_bars` against `content_bounds`), in
/// the same physical-pixel space the pointer arrives in. `busy` is "rt's own
/// chrome owns the pointer right now" — an overlay is open, or an internal
/// pane/tab drag is live; the caller assembles it (see `App::chrome_busy`).
///
/// Refusals, in order:
///
/// * `busy` — a drop over the preferences dialog, the context menu, the manual,
///   the search bar, the clipboard-history overlay or the colour picker must not
///   reach the pane those panels happen to be drawn over. Neither may one that
///   lands mid-way through rt's own pane drag.
/// * A **tab strip band**. A tab strip is chrome, not a terminal, and the tab
///   under the cursor is not necessarily the visible one — inserting into it
///   would type into a pane the user cannot see. Matched on the strip's vertical
///   band **and** its horizontal extent, so a split's other column at the same
///   height stays a valid target (`crate::dragdrop::resolve_tab_strip` tests only
///   the band because a pane being dragged onto a strip has a meaning there;
///   text has none).
/// * The window margin and any gutter between panes: no pane contains the point.
///
/// A pane's own titlebar strip IS accepted, and lands in that pane: it is that
/// pane's chrome, the target is unambiguous, and refusing a band a few pixels
/// tall would only make the feature feel unreliable.
pub fn resolve(
    panes: &[(PaneId, Rect)],
    tab_bars: &[TabBar],
    cursor: (f32, f32),
    busy: bool,
) -> Option<TextTarget> {
    if busy {
        return None; // rt's own chrome owns the pointer
    }
    let (cx, cy) = cursor;
    for bar in tab_bars {
        // All tabs in a bar share one vertical band; the bar's horizontal
        // extent is the union of its tabs.
        let Some(first) = bar.tabs.first() else {
            continue; // an empty bar hit-tests as nothing
        };
        let (top, h) = (first.rect.y, first.rect.h);
        if h <= 0.0 || cy < top || cy >= top + h {
            continue; // not in this bar's band
        }
        let left = bar.tabs.iter().fold(f32::INFINITY, |a, t| a.min(t.rect.x));
        let right = bar.tabs.iter().fold(f32::NEG_INFINITY, |a, t| a.max(t.rect.right()));
        if cx >= left && cx < right {
            return None; // a tab strip: chrome, and possibly a hidden pane
        }
    }
    panes
        .iter()
        .find(|(_, r)| r.contains(cx, cy))
        .map(|&(pane, cue)| TextTarget { pane, cue })
}

/// The text a dropped payload actually delivers to a pane whose own
/// bracketed-paste (DECSET 2004) state is `bracketed`.
///
/// The bytes then go out through [`rt_session::Session::paste_to_pane`], which
/// is the same wrap-and-strip the clipboard paste path uses — this function
/// only decides the BODY. Three rules, in order:
///
/// 1. **Line endings are normalised to `\n`.** A browser hands over `\r\n` (and
///    occasionally a bare `\r`) in `text/plain`; a bare `\r` reaching a terminal
///    is a carriage return, which overwrites the line just typed instead of
///    ending it. The clipboard path never had to care because X11/Wayland
///    clipboard owners normalise; a drag payload is raw.
/// 2. **Trailing newlines are dropped.** A selection dragged out of a browser
///    almost always ends in one, and rt must never be the thing that pressed
///    Return. After a drop the text is *staged* at the prompt, and the user
///    presses Enter — or does not.
/// 3. **With bracketed paste OFF, interior line breaks become spaces.**
///
/// Rule 3 is the multi-line question, and it is where a drop is deliberately
/// **not** byte-identical to a paste. A paste is two deliberate acts (copy,
/// then a paste key aimed at a focused terminal); a drag is one gesture that can
/// end over a terminal by accident, carrying text from a page the user never
/// read. Feeding that to a line editor that has not negotiated DECSET 2004 runs
/// every line as its own command. So:
///
/// * **Bracketed paste on** — every interactive shell rt will meet (bash with
///   readline, zsh's ZLE, fish) sets it — the newlines go through untouched.
///   That is what the mode is FOR: the shell shows the whole payload as one
///   editable buffer and runs nothing until Return. Nothing is lost.
/// * **Bracketed paste off** — `cat`, a `read` loop, an old editor — rt cannot
///   know whether a newline means "next line" or "run it", so it sends no bare
///   newline at all. The drop arrives as one visible line the user can see and
///   edit. Flattening is lossy and it is meant to be noticed; silently running
///   five commands is not recoverable.
///
/// What this does NOT do is strip other control bytes (`ESC` above all). That
/// hazard is identical for a drop and for a clipboard paste, it is the paste
/// path's to own, and solving it in one place only would make the two disagree.
pub fn payload(text: &str, bracketed: bool) -> String {
    // Rule 1: CRLF and lone CR both become LF.
    let mut body = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\r' {
            if chars.peek() == Some(&'\n') {
                chars.next(); // consume the LF of a CRLF pair
            }
            body.push('\n');
        } else {
            body.push(c);
        }
    }
    // Rule 2: never deliver a trailing newline — a drop stages, it does not run.
    while body.ends_with('\n') {
        body.pop();
    }
    // Rule 3: with no bracketed paste, no bare newline may reach the line editor.
    if !bracketed {
        body = body.replace('\n', " ");
    }
    body
}

/// The label for the chip that rides the cursor while a text drag hovers rt —
/// the same ghost `chrome::dragdrop::draw` paints for an internal pane drag, so
/// the user reads one visual language for "this is where it lands".
///
/// A single line shows itself (elided in the middle at [`GHOST_CHARS`], so both
/// ends stay recognisable); anything with a line break shows the line count
/// instead, because the count is the thing worth knowing before letting go.
/// Counted in CHARS, not bytes — the chip's width is `chars * cell_width`.
// Called by the platform receivers that can see the payload before the drop —
// AppKit can, XDND cannot (its selection may only be converted after XdndDrop),
// so the X11 receiver uses a fixed label instead. The tests below are the
// coverage on a Linux build, which is the point of keeping this module un-cfg'd.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub fn ghost_label(text: &str) -> String {
    let lines = text.lines().filter(|l| !l.is_empty()).count();
    let multi = text.trim_end_matches(['\n', '\r']).contains(['\n', '\r']);
    if multi {
        return format!("{lines} lines");
    }
    let one: String = text.trim_end_matches(['\n', '\r']).replace('\t', " ");
    let n = one.chars().count();
    if n <= GHOST_CHARS {
        return one;
    }
    // Elide the middle: "the quick brown…lazy dog".
    let keep = GHOST_CHARS - 1; // the ellipsis costs one cell
    let head: String = one.chars().take(keep - keep / 2).collect();
    let tail: String = one.chars().skip(n - keep / 2).collect();
    format!("{head}…{tail}")
}

/// Widest the drag ghost's label gets, in characters. Not a chrome LENGTH (the
/// chip is measured in cells, which already carry the display's factor), so it
/// is not in `chrome_scale::logical`.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
pub const GHOST_CHARS: usize = 28;

/// The flavours rt will take from a foreign drag, best first, in their
/// **canonical** (lower-case, space-free) spelling. The same preference order
/// the X11 receiver interns as atoms, so a drag from Firefox delivers the same
/// bytes on either display server.
///
/// `STRING`/`TEXT` are Latin-1 and undefined-encoding respectively by the letter
/// of the ICCCM, and sit last as a floor: every source that still offers them
/// sends UTF-8 in practice, and taking them beats refusing the drop.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub const DROP_MIMES: [&str; 5] =
    ["text/plain;charset=utf-8", "utf8_string", "text/plain", "string", "text"];

/// **The accept/reject decision**, and with it the flavour to ask for: the best
/// of the MIME types a Wayland drag source is offering, or `None` — refuse this
/// drag — when none of them is text rt can read.
///
/// Returns a borrow of the string as the SOURCE spelled it, which is the whole
/// reason this is not just an index into [`DROP_MIMES`]. `wl_data_offer.accept`
/// and `wl_data_offer.receive` are matched against the offered strings verbatim
/// by the source; answering with rt's own canonical spelling would be accepted
/// by the compositor and then deliver nothing.
///
/// Matching is on a normalised form — ASCII-lower-cased with every whitespace
/// character removed — because `text/plain;charset=utf-8`,
/// `text/plain; charset=UTF-8` and `TEXT/PLAIN;charset=utf-8` are the same type
/// and all three occur (GTK writes the first, Qt and XWayland bridges have been
/// seen with the others). The X11 receiver needs no such rule: it compares
/// interned atoms, so it just lists both spellings it cares about.
///
/// Note what is deliberately NOT special-cased: a file drag from a file manager
/// offers `text/uri-list` **and** `text/plain`, and will be accepted here as
/// text — inserting the `file://` URI. That is exactly what the X11 receiver
/// already does, and one behaviour across the two is worth more than a cleverer
/// rule on one of them.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn pick_drop_mime(offered: &[String]) -> Option<&str> {
    let canonical: Vec<String> = offered
        .iter()
        .map(|m| m.chars().filter(|c| !c.is_whitespace()).map(|c| c.to_ascii_lowercase()).collect())
        .collect();
    DROP_MIMES
        .iter()
        .find_map(|want| canonical.iter().position(|c| c == want))
        .map(|i| offered[i].as_str())
}

/// A Wayland drag position, converted into the space the rest of the feature
/// works in: physical pixels from the top-left of the window's content area.
///
/// `wl_data_device.enter`/`motion` report **surface-local** coordinates, which
/// are logical — the compositor has already divided out the scale (and, under
/// `wp_fractional_scale_v1`, the fractional one). Everything downstream —
/// `Session::visible_rects`, `resolve`, winit's own `CursorMoved` — is in
/// physical pixels. This is the same multiplication winit's Wayland pointer
/// handler does (`LogicalPosition::to_physical(scale_factor)`), which is what
/// put the pane rects on that grid to begin with; using anything else would put
/// the cue under the pointer on a 1× display and nowhere near it on a 1.5× one.
///
/// `None` for a non-finite or non-positive input rather than a NaN that would
/// hit-test as some arbitrary pane. `Rect::contains` already answers `false` to
/// a NaN, so this is belt and braces — but "no position" is the honest value,
/// and it makes the discard explicit at the receiver instead of implicit three
/// calls away.
/// The three `wl_data_device_manager.dnd_action` bits, as the protocol numbers
/// them. Spelled out here so [`may_finish_drop`] is testable without a live
/// compositor to mint a `DndAction` from — the receiver passes
/// `DndAction::bits()` straight in, and these are what those bits mean.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub const DND_COPY: u32 = 1;
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub const DND_MOVE: u32 = 2;
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub const DND_ASK: u32 = 4;

/// Whether `wl_data_offer.finish` may be sent for an offer in this state.
///
/// This one is a guard, not a feature. `finish` is how rt tells a drag source
/// "I took it, you can stop" — and sending it at the wrong moment is the
/// `invalid_finish` protocol error, which does not fail the request: it tears
/// down rt's whole `wl_display`, and with it every pane, tab and shell in the
/// process. Hence a pure function with the protocol's three preconditions
/// written out, tested, and read straight off the offer rather than off rt's
/// belief about the drag:
///
/// * **version ≥ 3** — `finish` does not exist before it (nor does the `action`
///   event that decides the third condition).
/// * **dropped** — the request is "the drop is done", so there must have been
///   one. A drag that merely left is finished by destroying the offer.
/// * **a real action was selected** — `none` means the compositor settled on
///   nothing, and `ask` means "show a menu and tell me which", which rt has no
///   menu for and never requests. Only `copy` (what rt asks for) or `move`
///   counts. `ask` alongside a real action is fine: the real one is what
///   happened.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn may_finish_drop(version: u32, dropped: bool, selected_action: u32) -> bool {
    version >= 3 && dropped && selected_action & (DND_COPY | DND_MOVE) != 0
}

#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn surface_to_physical(x: f64, y: f64, scale: f64) -> Option<(f32, f32)> {
    if !x.is_finite() || !y.is_finite() || !scale.is_finite() || scale <= 0.0 {
        return None;
    }
    let (px, py) = (x * scale, y * scale);
    (px.is_finite() && py.is_finite()).then_some((px as f32, py as f32))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pane(id: u64, x: f32, y: f32, w: f32, h: f32) -> (PaneId, Rect) {
        (PaneId(id), Rect::new(x, y, w, h))
    }

    fn bar(y: f32, h: f32, xs: &[(f32, f32)]) -> TabBar {
        TabBar {
            tabs: xs
                .iter()
                .enumerate()
                .map(|(i, &(x, w))| rt_core::Tab {
                    rect: Rect::new(x, y, w, h),
                    first_pane: PaneId(100 + i as u64),
                    active: i == 0,
                    number: i + 1,
                })
                .collect(),
        }
    }

    // --- resolve ----------------------------------------------------------

    /// The point of the whole exercise: the text lands where the POINTER is,
    /// not where the keyboard focus is. The layout below has two panes; the
    /// focused one is irrelevant here because `resolve` is never told about it.
    #[test]
    fn the_pane_under_the_cursor_takes_the_drop() {
        let panes = [pane(1, 0.0, 0.0, 100.0, 200.0), pane(2, 100.0, 0.0, 100.0, 200.0)];
        assert_eq!(resolve(&panes, &[], (10.0, 10.0), false).unwrap().pane, PaneId(1));
        assert_eq!(resolve(&panes, &[], (150.0, 10.0), false).unwrap().pane, PaneId(2));
    }

    /// The cue is the target pane's own rect, so the user sees the whole
    /// receiving pane light up before letting go.
    #[test]
    fn the_cue_is_the_target_panes_rect() {
        let panes = [pane(7, 20.0, 30.0, 100.0, 200.0)];
        let t = resolve(&panes, &[], (25.0, 35.0), false).unwrap();
        assert_eq!(t.cue, Rect::new(20.0, 30.0, 100.0, 200.0));
    }

    /// The window margin and the gutter between panes belong to no pane, so a
    /// drop there is refused rather than being nudged into a neighbour.
    #[test]
    fn a_gutter_or_margin_takes_no_drop() {
        let panes = [pane(1, 10.0, 10.0, 80.0, 80.0), pane(2, 110.0, 10.0, 80.0, 80.0)];
        assert!(resolve(&panes, &[], (2.0, 2.0), false).is_none()); // window margin
        assert!(resolve(&panes, &[], (95.0, 50.0), false).is_none()); // gutter
        assert!(resolve(&panes, &[], (150.0, 500.0), false).is_none()); // below everything
    }

    /// Dropping onto a tab strip must not insert into whatever pane happens to
    /// be drawn under it: the tab under the cursor need not be the visible one.
    #[test]
    fn a_tab_strip_takes_no_drop() {
        let panes = [pane(1, 0.0, 0.0, 200.0, 200.0)];
        let bars = [bar(0.0, 20.0, &[(0.0, 60.0), (60.0, 60.0)])];
        assert!(resolve(&panes, &bars, (30.0, 10.0), false).is_none());
        // Just below the strip is the pane again.
        assert_eq!(resolve(&panes, &bars, (30.0, 25.0), false).unwrap().pane, PaneId(1));
    }

    /// …but only within the strip's own horizontal extent. A side-by-side split
    /// where only the left column carries tabs must still accept a drop into the
    /// right column at the same height.
    #[test]
    fn a_tab_strips_band_does_not_reach_across_the_window() {
        let panes = [pane(1, 0.0, 0.0, 100.0, 200.0), pane(2, 100.0, 0.0, 100.0, 200.0)];
        let bars = [bar(0.0, 20.0, &[(0.0, 50.0), (50.0, 50.0)])];
        assert!(resolve(&panes, &bars, (25.0, 10.0), false).is_none()); // over the strip
        assert_eq!(resolve(&panes, &bars, (150.0, 10.0), false).unwrap().pane, PaneId(2));
    }

    /// An open overlay (prefs/menu/manual/search/clip history/picker) or a live
    /// internal pane drag swallows the drop whole.
    #[test]
    fn busy_chrome_takes_no_drop() {
        let panes = [pane(1, 0.0, 0.0, 200.0, 200.0)];
        assert!(resolve(&panes, &[], (50.0, 50.0), true).is_none());
    }

    /// Degenerate geometry must fail closed, not panic.
    #[test]
    fn no_panes_and_empty_bars_are_harmless() {
        assert!(resolve(&[], &[], (5.0, 5.0), false).is_none());
        let empty = TabBar { tabs: vec![] };
        assert!(resolve(&[], std::slice::from_ref(&empty), (5.0, 5.0), false).is_none());
        // A zero-height strip cannot be hovered.
        let flat = bar(0.0, 0.0, &[(0.0, 40.0)]);
        let panes = [pane(1, 0.0, 0.0, 200.0, 200.0)];
        assert_eq!(resolve(&panes, &[flat], (10.0, 0.0), false).unwrap().pane, PaneId(1));
    }

    // --- payload ----------------------------------------------------------

    /// Browsers hand over CRLF. A bare `\r` in a terminal is a carriage return,
    /// which would overwrite the line rather than end it.
    #[test]
    fn crlf_and_bare_cr_become_lf() {
        assert_eq!(payload("a\r\nb\r\nc", true), "a\nb\nc");
        assert_eq!(payload("a\rb\rc", true), "a\nb\nc");
        assert_eq!(payload("a\r\n\r\nb", true), "a\n\nb");
    }

    /// rt is never the thing that pressed Return: whatever the mode, the
    /// delivered text ends staged at the prompt.
    #[test]
    fn a_trailing_newline_is_never_delivered() {
        for &bracketed in &[true, false] {
            assert!(!payload("ls -la\n", bracketed).ends_with('\n'));
            assert!(!payload("ls -la\r\n", bracketed).ends_with('\n'));
            assert!(!payload("ls -la\n\n\n", bracketed).ends_with('\n'));
            assert_eq!(payload("ls -la\n", bracketed), "ls -la");
        }
    }

    /// With DECSET 2004 on, the shell shows the payload as one editable buffer
    /// and runs nothing until Return — so the newlines go through untouched and
    /// nothing is lost.
    #[test]
    fn bracketed_paste_keeps_every_line_break() {
        assert_eq!(payload("one\ntwo\nthree", true), "one\ntwo\nthree");
        assert_eq!(payload("one\r\ntwo\r\nthree\r\n", true), "one\ntwo\nthree");
    }

    /// The hazard the whole rule exists for: a paragraph dropped into a shell
    /// with no bracketed paste must not run as three commands.
    #[test]
    fn without_bracketed_paste_no_bare_newline_survives() {
        let out = payload("rm -rf /tmp/x\necho gotcha\nwhoami\n", false);
        assert!(!out.contains('\n'), "a bare newline reached a line editor: {out:?}");
        assert!(!out.contains('\r'));
        assert_eq!(out, "rm -rf /tmp/x echo gotcha whoami");
    }

    /// A single-line drop is unchanged in either mode — the common case pays
    /// nothing for the multi-line guard.
    #[test]
    fn a_single_line_is_byte_identical_in_both_modes() {
        for &bracketed in &[true, false] {
            assert_eq!(payload("https://example.com/x?a=1&b=2", bracketed), "https://example.com/x?a=1&b=2");
        }
    }

    /// Empty and whitespace-only payloads must not panic or invent bytes.
    #[test]
    fn empty_payloads_stay_empty() {
        assert_eq!(payload("", true), "");
        assert_eq!(payload("\n", true), "");
        assert_eq!(payload("\r\n", false), "");
    }

    /// Tabs and other in-line whitespace are content, not line structure.
    #[test]
    fn tabs_are_left_alone() {
        assert_eq!(payload("a\tb", false), "a\tb");
    }

    // --- ghost_label ------------------------------------------------------

    #[test]
    fn a_short_single_line_labels_itself() {
        assert_eq!(ghost_label("hello"), "hello");
        assert_eq!(ghost_label("hello\n"), "hello");
    }

    #[test]
    fn a_long_single_line_is_elided_in_the_middle() {
        let long = "abcdefghijklmnopqrstuvwxyz0123456789";
        let label = ghost_label(long);
        assert_eq!(label.chars().count(), GHOST_CHARS);
        assert!(label.contains('…'));
        assert!(label.starts_with('a'));
        assert!(label.ends_with('9'));
    }

    #[test]
    fn a_multi_line_payload_labels_its_line_count() {
        assert_eq!(ghost_label("one\ntwo\nthree"), "3 lines");
        assert_eq!(ghost_label("one\r\ntwo\r\n"), "2 lines");
    }

    /// The label is built from arbitrary web text, so it must survive anything.
    #[test]
    fn ghost_label_never_panics_on_odd_input() {
        for s in ["", "\n", "\r\n\r\n", "é".repeat(200).as_str(), "\t\t\t"] {
            let _ = ghost_label(s);
        }
    }

    // --- pick_drop_mime ---------------------------------------------------

    /// The whole point of returning a borrow of the OFFER: rt must answer with
    /// the spelling the source used, not with its own canonical one.
    /// `wl_data_offer.accept`/`receive` match on the exact string, so echoing a
    /// normalised form back gets nothing.
    #[test]
    fn the_offered_spelling_comes_back_verbatim() {
        let offered = vec!["text/plain; charset=UTF-8".to_string()];
        assert_eq!(pick_drop_mime(&offered), Some("text/plain; charset=UTF-8"));
    }

    /// utf-8 `text/plain` wins wherever it sits in the list a browser offers.
    #[test]
    fn utf8_text_plain_wins() {
        let offered = mimes(&["text/html", "text/plain", "text/plain;charset=utf-8", "STRING"]);
        assert_eq!(pick_drop_mime(&offered), Some("text/plain;charset=utf-8"));
    }

    /// Case, and the optional space after the `;`, are noise: GTK writes
    /// `text/plain;charset=utf-8`, Qt has been seen with both a space and an
    /// upper-case `UTF-8`, and all of them mean the same thing.
    #[test]
    fn charset_case_and_spacing_do_not_matter() {
        for spelling in
            ["text/plain;charset=UTF-8", "TEXT/PLAIN;CHARSET=utf-8", "text/plain; charset=utf-8"]
        {
            let offered = mimes(&["text/html", spelling]);
            assert_eq!(pick_drop_mime(&offered), Some(spelling));
        }
    }

    /// The fallbacks, taken strictly in order — the same preference list the
    /// X11 receiver interns as atoms.
    #[test]
    fn the_fallbacks_are_taken_in_order() {
        let pick = |v: &[&str]| pick_drop_mime(&mimes(v)).map(str::to_string);
        assert_eq!(pick(&["STRING", "UTF8_STRING", "text/plain"]).as_deref(), Some("UTF8_STRING"));
        assert_eq!(pick(&["STRING", "text/plain"]).as_deref(), Some("text/plain"));
        assert_eq!(pick(&["TEXT", "STRING"]).as_deref(), Some("STRING"));
        assert_eq!(pick(&["TEXT"]).as_deref(), Some("TEXT"));
    }

    /// The accept/reject decision. A drag carrying nothing rt can read must be
    /// REFUSED, so the source shows the user a no-drop cursor instead of
    /// pretending rt will take it — and so the compositor never sends a drop.
    #[test]
    fn a_drag_with_no_text_flavour_is_refused() {
        let offered = mimes(&["image/png", "text/html", "application/x-qt-image"]);
        assert_eq!(pick_drop_mime(&offered), None);
        assert_eq!(pick_drop_mime(&[]), None);
    }

    /// A source may repeat a type, or offer an empty or malformed one. None of
    /// that may panic or change the answer.
    #[test]
    fn odd_offer_lists_are_harmless() {
        let offered = mimes(&["", "text/plain", "text/plain", ";;;", "\u{0}"]);
        assert_eq!(pick_drop_mime(&offered), Some("text/plain"));
    }

    fn mimes(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    // --- surface_to_physical ----------------------------------------------

    /// Wayland reports a drag position in SURFACE-local coordinates, which are
    /// logical; rt hit-tests panes in physical pixels. This is winit's own
    /// conversion, which is what put the pane rects on that grid to begin with.
    #[test]
    fn surface_coordinates_scale_to_physical_pixels() {
        assert_eq!(surface_to_physical(10.0, 20.0, 1.0), Some((10.0, 20.0)));
        assert_eq!(surface_to_physical(10.0, 20.0, 2.0), Some((20.0, 40.0)));
        assert_eq!(surface_to_physical(10.0, 20.0, 1.5), Some((15.0, 30.0)));
    }

    // --- may_finish_drop --------------------------------------------------

    /// The happy path, and the only one that says yes: a version-3 offer that
    /// really was dropped, with a real action settled on it.
    #[test]
    fn a_dropped_v3_offer_with_a_real_action_may_be_finished() {
        assert!(may_finish_drop(3, true, DND_COPY));
        assert!(may_finish_drop(5, true, DND_COPY));
        // rt only ever asks for Copy, but a compositor that answers Move has
        // still settled on something, and `finish` is legal.
        assert!(may_finish_drop(3, true, DND_MOVE));
    }

    /// Each of the three ways it becomes the `invalid_finish` protocol error —
    /// which does not fail the request, it kills rt's whole `wl_display` and
    /// every pane and shell on it.
    #[test]
    fn an_untimely_finish_is_refused() {
        assert!(!may_finish_drop(3, false, DND_COPY), "not dropped yet");
        assert!(!may_finish_drop(3, true, 0), "no action was selected");
        assert!(!may_finish_drop(2, true, DND_COPY), "v2 has no finish request");
        assert!(!may_finish_drop(1, true, DND_COPY));
    }

    /// `ask` means "put a menu up and tell me which"; rt has no such menu and
    /// never asks for the action, so an `ask` answer is not something it may
    /// declare finished.
    #[test]
    fn an_ask_action_alone_is_not_finishable() {
        assert!(!may_finish_drop(3, true, DND_ASK));
        // …but ask ALONGSIDE a real action is: the real one is what happened.
        assert!(may_finish_drop(3, true, DND_ASK | DND_COPY));
    }

    /// A garbage scale or position must not become a NaN that hit-tests as some
    /// arbitrary pane: it becomes "no position", and the drop is discarded.
    #[test]
    fn non_finite_input_yields_no_position() {
        assert_eq!(surface_to_physical(f64::NAN, 1.0, 1.0), None);
        assert_eq!(surface_to_physical(1.0, f64::INFINITY, 1.0), None);
        assert_eq!(surface_to_physical(1.0, 1.0, f64::NAN), None);
        assert_eq!(surface_to_physical(1.0, 1.0, 0.0), None);
        assert_eq!(surface_to_physical(1.0, 1.0, -2.0), None);
    }
}
