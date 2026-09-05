//! The built-in manual (F1). Holds [`MANUAL`] — the full feature reference plus
//! runnable input/output/error and wiring examples — kept in-app so it always
//! matches the actual keybindings. The overlay is drawn natively by
//! [`crate::chrome::manual`] on both backends.
//!
//! Keep it in step with `rt_config::Keymap::defaults()`, the mouse handlers in
//! `main.rs`, `prefs_model.rs`, and the CLI parser: a feature that ships without a
//! paragraph here is invisible to the user. Lines are kept under ~76 columns so the
//! aligned key/description columns survive the overlay's word-wrap.

/// The manual text. Plain monospace; UPPERCASE lines are section headings.
pub const MANUAL: &str = r#"rt — a Wayland-native (and X11) terminal multiplexer with anchored
selection, clipboard history, instrumented borders and an inter-pane fd
patch-bay.
Press F1 or Esc to close this manual; Up/Down, PageUp/PageDown or the wheel
scroll it.


PANES
  Ctrl+Shift+O    split horizontally (stacked)
  Ctrl+Shift+E    split vertically (side by side)
  Ctrl+Shift+A    split along the longer axis (auto)
  Ctrl+Shift+W    close the focused pane
  Ctrl+Shift+Q    close the whole window
  Ctrl+Shift+X    zoom / maximise the focused pane (toggle)
  Ctrl+Shift+R    rotate the enclosing split 90 deg CCW (nested splits too)
  Alt+Arrows      move focus between panes
  Ctrl+Shift+Arrows   resize: grow the focused pane
  click / mouse   click-to-focus (or focus-follows-mouse in Preferences)
  drag a gutter   resize the split
  Every pane can show a titlebar (Preferences): tab number and title, the
  pane size, the scrollback meter, its group colour, the clipboard-history
  count, and — while you compose a selection — the selection status.


TABS  &  COLUMNS
  Ctrl+Shift+T    new tab beside the focused pane
  Ctrl+PageUp/Dn  previous / next tab
  Ctrl+Shift+PageUp/Dn   move the focused tab left / right
  Ctrl+.  /  Ctrl+,   more / fewer newspaper columns (text flows column to
                      column; vim/less/etc. just see a taller, narrower screen)


WINDOWS  &  DRAG-AND-DROP
  Ctrl+Shift+I    open a new, empty rt window
  Ctrl+Shift+D    detach the focused pane into a new window of its own
  Ctrl+Shift+J    detach the focused tab into a new window of its own
  Ctrl+Shift+M    pick up the focused pane (carry it: aim, then click to drop)
  Ctrl+Shift+N    pick up the focused tab (carry it: aim, then click to drop)
  middle-click    a titlebar or tab label picks it up (carry mode below)
  One rt process serves every window (see them all in the right-click
  menu's "Move Pane to ..." rows); closing a window just closes it, and
  rt exits once the last one is gone.
  Drag & drop     drag a pane by its titlebar, or a tab by its label.
                  Cues while you hover: a half-pane fill previews a
                  split, the whole pane lit up previews a swap, a 3px
                  caret previews where a tab would land, and a band
                  along a window's edge previews a new root split; a
                  ghost chip rides the cursor and the dragged pane/tab
                  dims at its old spot. Esc cancels and puts everything
                  back. Pane drag needs its titlebar (Preferences, on
                  by default) to grab — with titlebars off, drag the
                  tab instead, or use the keys above.
                  Drop OUTSIDE the window — on bare desktop — to tear
                  the pane/tab out into a brand-new window. On X11 the
                  new window lands at the drop point; on Wayland the
                  compositor places it (a client has no screen
                  coordinates there — and cannot see other windows
                  mid-drag, so a release over another rt window also
                  tears out rather than dropping in).
                  Hold Ctrl as you release to CARRY instead of tearing
                  out — see Carry below.
                  X11 only (ssh -X included): drop onto ANOTHER rt
                  window to move the pane/tab there — its CENTRE swaps
                  the two panes, an edge splits, both across windows.
                  On Wayland use the right-click menu's "Move Pane to
                  N: title" row to send a pane straight to another
                  open window, or the detach keys above.
                  A cross-window move or tear-out CUTS any patch-bay
                  wire that would end up spanning two windows — a wire
                  wholly inside what moved travels with it.
  Carry           picks the pane/tab up (Ctrl+Shift+M/N above, the
                  "Pick Up Pane"/"Pick Up Tab" menu rows, holding
                  Ctrl while releasing a drag outside the window, or
                  middle-click on a titlebar or tab label) instead of
                  dropping it right away. Aim at ANY rt window — this
                  one or another — for the SAME drop cues a live drag
                  shows; left-click commits with the same target
                  semantics (split, swap, tab-insert, cross-window
                  move) on Wayland and X11 alike. Esc, or a right- or
                  middle-click while carrying cancels and puts it
                  back (middle-click is symmetric: it picks up, and
                  puts back down). The pointer wears the same
                  held-pane card (outline, translucent body, titlebar
                  band) the whole time; it reverts to the normal
                  cursor over a foreign app or bare desktop (rt only
                  owns the cursor on its own windows), and falls back
                  to a plain grab cursor if the compositor refuses
                  custom cursor images. Works with titlebars off —
                  pick-up needs no titlebar to grab.


SELECTING TEXT
  drag             select (linear, row by row)
  Ctrl+drag        select a rectangular BLOCK (columns only)
  double-click     select the word under the pointer (grows across a
                   soft-wrap, so a long URL or key selects whole)
  triple-click     select the whole logical line (soft-wraps rejoined, so
                   the copied text has no extra newlines)
  Any selection is copied to PRIMARY at once (middle-click pastes it).
  Ctrl+Shift+C     copy the selection to the CLIPBOARD as well
  Ctrl+Shift+V     paste the CLIPBOARD;  middle-click pastes PRIMARY
  Dragging past the top or bottom edge of a pane scrolls the pane so the
  selection keeps growing; the scroll ACCELERATES the longer you hold the
  pointer past the edge (same curve and preferences as hold-arrow
  acceleration, below). Selections are anchored to buffer lines, so they
  ride along when the pane scrolls.

  Anchored selection — select across any amount of scrollback without
  holding a button:
  Shift+click      drop the START of a selection at the clicked cell. The
                   pane enters a modal selecting state: the titlebar reads
                   "◉ selecting · N lines" and the keyboard drives the
                   selection instead of the shell.
  Ctrl+Shift+click drop a start for a rectangular BLOCK selection
                   (titlebar reads "◉ selecting · COLS×ROWS")
  Arrows           move the END one cell / row (hold to accelerate)
  Home / End       move the END to the start / end of its line
  PageUp/PageDown  move the END a screenful up / down
  Ctrl+Home/End    move the END to the top / bottom of the buffer —
                   "select from here to the very end" is two keystrokes
  wheel, scrollbar scroll freely meanwhile; the selection stays put
  Shift+click      set the END where you clicked and FINISH: the text is
                   copied to both CLIPBOARD and PRIMARY
  Enter            finish at the current END (copies to both)
  Esc / click      cancel (a plain click anywhere, or a Shift+click in
                   another pane); nothing is copied
  The finished selection stays highlighted, exactly like a drag-select,
  so you can see it and re-copy it.


CLIPBOARD HISTORY   (your last 20 copies, in memory only)
  Every copy rt makes — drag / double / triple / anchored selections and
  Ctrl+Shift+C — is remembered in a ring of 20 unique clips, newest first
  (re-copying an old clip moves it to the front; whitespace-only clips
  are skipped). Nothing is written to disk; the ring is gone on exit.
  Ctrl+Shift+H     open the history for the focused pane
  click the ⎘ N    titlebar field (focused pane; shown when non-empty)
  Up / Down        choose a clip (previews are one line; ↵ marks newlines)
  Enter / click    PASTE that clip into the focused pane, make it the
                   current CLIPBOARD + PRIMARY, and move it to the front
  Esc              close;  click outside closes too
  Clear history    the last row of the list, or right-click menu
                   "Clear Clipboard History" — empties the ring at once


SCROLLBACK  &  SEARCH
  wheel            scroll the pane under the pointer
  scrollbar        drag the thumb on the pane's right edge
  Ctrl+Shift+F     open the search bar (top right) for the focused pane
  type             search (case-insensitive); every hit is highlighted
                   and the bar counts "pos/count"
  Enter            next hit      Shift+Enter  previous hit      Esc  close
  Mouse keeps working while the bar is open, so you can look around.
  The buffer holds up to the Preferences "Scrollback (lines)" setting (up to
  5M lines) for terminals opened after the change, within a per-pane memory
  budget that evicts the oldest lines first. The titlebar meter reads
  "buf used/max" and turns amber as it fills.


GROUPS  &  BROADCAST   (type once, reach many panes)
  Make a group: press Ctrl+Shift+G on each pane until they show the SAME
  corner colour. Panes sharing a colour are one group -- that's all a group is.
  Ctrl+Shift+G    cycle THIS pane's group colour (none -> 1 -> 2 -> 3 -> 4)
  right-click menu Broadcast: Off / All / Group
                  Off = focused pane only;  All = every pane;
                  Group = every pane sharing the focused pane's colour


MOUSE
  click            focus a pane (or focus-follows-mouse, see Preferences)
  drag             select text (see SELECTING TEXT for all the variants)
  wheel            scroll that pane's scrollback
  middle-click     paste the PRIMARY selection
  right-click      context menu        drag a gutter   resize the split
  Ctrl+click       open a URL under the pointer; right-click on a URL adds
                   Open Link / Copy Address to the menu
  When the program in a pane asks for the mouse (vim, htop, tmux, less
  --mouse, fzf, ...), rt forwards clicks, drags and the wheel to it instead.
  Hold SHIFT to override that and use rt's own select / scroll / menu.


TOUCH  &  STYLUS
  tap              a click: focus a pane, hit a tab, pick a menu row
  drag one finger  a left drag: select text, drag a pane by its titlebar,
                   move a gutter — everything the mouse does with a button
                   held down
  drag two fingers scroll the pane, the way the wheel does; the content
                   follows your fingers. A selection the first finger had
                   begun is undone when the second lands, so a scroll
                   never leaves a stray highlight behind.
  stylus tip       a left click / drag, pressure and tilt ignored (rt is a
                   terminal, not a canvas); the barrel buttons are the
                   right and middle buttons.
  window border      finger or pen: drag it to move the window, drag an
                     edge to resize, tap the buttons to minimise, maximise
                     or close
  On Wayland a compositor sends touch and stylus events ONLY to a client
  that asks for them — there is no emulated pointer to fall back on — so
  older rt builds were simply deaf to both. The border is drawn by winit,
  which routed only mouse events to it; rt carries a patched winit that
  routes finger and pen there as well.


KEYBOARD
  Keys without an rt binding go to the shell as the usual xterm sequences
  (arrows honour application-cursor mode, Alt prefixes ESC, Ctrl gives
  control codes; composed and dead keys send the text they produce).
  Hold-arrow acceleration: a single arrow TAP is always one move, but
  HOLDING an arrow (key auto-repeat) sends progressively more moves per
  repeat, up to "Max arrow speed" in Preferences — a long crawl along a
  line, through history, or down a man page in less becomes a second or
  two. Toggle it off in Preferences if you prefer the plain repeat rate.
  The same curve drives the anchored-selection END and drag auto-scroll.


BORDER INSTRUMENTS   (each pane's border is a live gauge; toggle in Preferences)
  Output   a green flow of packets orbits the border; speed and brightness
           track that pane's live output rate. Idle = still; busy = racing.
  Heat     the border is tinted by CPU load of the pane's whole session
           (shell + children), as a blackbody: dim deep-red idle, up through
           orange and yellow to white-hot, blue-white for a runaway.
  Latency  the window frame undulates purple-blue-violet and flares bright
           when the render loop misses a deadline (a CPU hogger stole a frame).
  Over ssh -X (the remote XRender backend) the instruments are OFF by
  default — they cost X-server CPU there — and can be switched on, static
  or animated at 6 fps, under Preferences > Border instruments.


THE PATCH-BAY   (wire terminals' fds to each other)
  Every pane exposes three pipe jacks, separate from the interactive terminal,
  advertised to its shell as environment variables:
      $RT_OUT   a program WRITES here   (its stdout jack, right edge, green)
      $RT_ERR   a program WRITES here   (its stderr jack, right edge, red)
      $RT_IN    a program READS here    (its stdin  jack, left edge, grey)
  Wire an output jack of one pane to the input jack of another and the bytes
  flow across a drawn wire (the moving packets ARE the bytes).

  Make a wire — keyboard:
      Ctrl+Shift+Y   arm a wire from the focused pane's stdout jack
      Ctrl+Shift+U   arm a wire from the focused pane's stderr jack
                     then move focus to the target pane and press it again
      Ctrl+Shift+K   disconnect every wire on the focused pane
      Ctrl+Shift+P   split, and pipe the focused pane's stdout into the new pane
  Make a wire — mouse:
      drag from a jack dot (on the pane edge) to another pane to connect
      right-click a jack to disconnect it


EXAMPLES   (type these in the panes; wire them as noted)
  1. Send output to another pane
       pane A:   seq 1 100 > $RT_OUT
       wire A.stdout -> B  (focus A, Ctrl+Shift+Y, focus B, Ctrl+Shift+Y)
       pane B:   cat $RT_IN

  2. Live stream, filtered downstream
       pane A:   ping -c 20 localhost | tee $RT_OUT
       wire A.stdout -> B
       pane B:   grep --line-buffered 'time=' < $RT_IN

  3. Split stdout and stderr to different panes
       pane A:   ls /nonesuch /etc >$RT_OUT 2>$RT_ERR
       wire A.stdout -> B  and  A.stderr -> C
       pane B shows the listing; pane C shows the error

  4. One-gesture cross-pane pipeline
       focus a producer pane, then Ctrl+Shift+P  (splits + wires its stdout in)
       in the new pane:   sort -u < $RT_IN

  5. Feed a pane's stdin from elsewhere (interactive)
       pane B:   cat $RT_IN            (waits for input)
       wire A.stdout -> B, then in A:  echo hello > $RT_OUT

  6. Collatz orbit (3x+1) looping around a two-pane ring
       pane B:   while read n; do echo $n; [ $n -eq 1 ]&&break; echo $((n%2?3*n+1:n/2))>$RT_OUT; done<$RT_IN
       pane A:   the same line, prefixed with   echo 27>$RT_OUT;   to seed it
       wire A.stdout -> B  and  B.stdout -> A   (close the ring)
       the seed hops A,B,A,B..., halved or 3x+1'd each step, until it
       reaches 1 -- the live number you watch is the packet riding the wire.

  7. Grab a whole build log without holding the mouse
       run the build; when it ends, Shift+click the first line you want,
       press Ctrl+End, then Enter — the lot is on the clipboard. Need the
       previous thing you copied back? Ctrl+Shift+H, pick it, Enter.


APPEARANCE  &  PREFERENCES   (right-click menu -> Preferences...)
  Font               family (steps through installed monospace fonts) & size
  Appearance         background opacity, compositor blur (Wayland
                     ext-background-effect / KDE; X11 KDE blur-behind)
  Colours            preset schemes (rt default, Solarized Dark, Dracula,
                     Gruvbox Dark, Nord), then tweak foreground, background
                     and the 16 ANSI palette entries with the colour picker
  Behaviour          focus-follows-mouse, per-pane titlebars, scrollback
                     size, hold-arrow acceleration and its max speed
  Border instruments output / heat / latency toggles, patch-bay jacks,
                     and the ssh -X show / animate switches
  Ctrl+=  (or Ctrl+Shift++)  Ctrl+-  Ctrl+0   font zoom in / out / reset
  Ctrl+Alt+Up / Ctrl+Alt+Down     background more opaque / more see-through
  F11   fullscreen        right-click -> Toggle Focus-Follows-Mouse
  Every change applies live and persists to  $XDG_CONFIG_HOME/rt/config.toml
  (~/.config/rt/config.toml).


STARTING rt   (command line & environment)
  rt --cols N --rows N      pin the initial grid size (both: pre-size window)
  rt --font "Family"        override the configured font family for this run
  rt --font-size PX         override the configured font size (pixels)
  rt --backend gl|xrender   force the renderer (default: GL locally, XRender
                            over ssh -X / a remote X server)
  rt -V / rt --version      print "rt X.Y.Z (commit)" — also the last line of
                            the right-click menu and the head of this manual
  RT_ENGINE=vtterm|alacritty  in-house VT engine (default) or the vendored
                            alacritty engine; rt announces its engine on start
  RT_BACKEND=gl|xrender     same as --backend
  RT_OPACITY=0.8            start with this background opacity (demo knob)
  RT_FOCUS=sloppy           start with focus-follows-mouse on (demo knob)
  rt prefers native Wayland when a Wayland session is present (never
  XWayland) and falls back to X11 otherwise; one binary serves both.
"#;

/// The macOS appendix — the Command-key defaults from `rt_config`'s
/// `MACOS_DEFAULTS`, shown only on macOS builds so a Linux reader never scrolls
/// past twenty lines about a platform they are not on.
///
/// Compiled as `""` off macOS, so [`manual_lines`] yields nothing extra there.
/// Chords are spelled exactly as `Chord`'s `Display` renders them on macOS
/// (`Cmd`, not `Super`) — `every_default_keybinding_is_documented` compares the
/// two strings literally.
#[cfg(target_os = "macos")]
pub const MANUAL_MACOS: &str = r#"

macOS — THE COMMAND (Cmd / ⌘) KEYS
  Every Ctrl+Shift key above still works on macOS. These are the extra ones
  a Mac user reaches for by reflex; both spellings do the same thing.
  Cmd+C  /  Cmd+V     copy the selection  /  paste the clipboard
  Cmd+T               new tab
  Cmd+W               close the focused pane (and the window with it, once
                      that pane was the last one)
  Shift+Cmd+W         close the whole window
  Cmd+Q               quit rt — every window. Owned by the macOS menu bar,
                      not by rt, so it is the one key rt cannot rebind.
  Cmd+N               new window
  Cmd+D               split side by side;  Shift+Cmd+D splits stacked
  Shift+Cmd+{ and Shift+Cmd+}   previous / next tab — the keys you press
                      are Shift+Cmd+[ and Shift+Cmd+], which on a Mac send
                      the braces
  Alt+Cmd+Left and Alt+Cmd+Right   previous / next tab again, the spelling
                      that works on any layout and needs no Fn key
  Cmd+,               Preferences
  Cmd+F               search this pane's scrollback
  Cmd+=  /  Cmd+-     bigger / smaller font;  Cmd+0 resets it
                      (Shift+Cmd++ is the same as Cmd+=)
  Ctrl+Cmd+F          fullscreen (F11 also works, with Fn)
  Shift+Cmd+?         this manual (F1 also works, with Fn)
  A Cmd chord rt does NOT bind types nothing at all, exactly as in
  Terminal.app — it will never leak a stray letter into your shell.
  Ctrl is untouched: Ctrl+C still interrupts.
"#;

/// Empty off macOS: there is no appendix to show.
#[cfg(not(target_os = "macos"))]
pub const MANUAL_MACOS: &str = "";

/// The manual as the user sees it: [`MANUAL`] followed by the platform
/// appendix. Everything that renders or checks the manual goes through this, so
/// the appendix can never drift out of the overlay.
pub fn manual_lines() -> impl Iterator<Item = &'static str> {
    MANUAL.lines().chain(MANUAL_MACOS.lines())
}

#[cfg(test)]
mod tests {
    use super::{MANUAL, MANUAL_MACOS};

    /// Every default keybinding must be mentioned in the manual — the manual is
    /// the only place a user learns the keys, and features have shipped without a
    /// line here before. Accepts the chord as `Keymap` displays it (`Ctrl+Shift+O`,
    /// `Ctrl+PgUp`, `Alt+Up`), or the manual's grouped shorthands for arrow/page
    /// families (`Alt+Arrows`, `Ctrl+Shift+Arrows`, `Ctrl+PageUp/Dn`,
    /// `Ctrl+Alt+Up / Ctrl+Alt+Down`).
    #[test]
    fn every_default_keybinding_is_documented() {
        let km = rt_config::Keymap::defaults();
        // The macOS appendix counts as documentation too (and is empty
        // elsewhere), so a Cmd binding must have a line there.
        let documented = |chord: &str| -> bool {
            if MANUAL.contains(chord) || MANUAL_MACOS.contains(chord) {
                return true;
            }
            // "<mods>+Up" etc. may be documented as "<mods>+Arrows".
            let (mods, key) = chord.rsplit_once('+').unwrap_or(("", chord));
            let arrows = matches!(key, "Up" | "Down" | "Left" | "Right") && MANUAL.contains(&format!("{mods}+Arrows"));
            let pages = matches!(key, "PgUp" | "PgDn") && MANUAL.contains(&format!("{mods}+PageUp"));
            arrows || pages
        };
        let missing: Vec<String> = km
            .bindings()
            .map(|(chord, action)| format!("{chord} ({action:?})"))
            .filter(|s| !documented(s.split(" (").next().unwrap()))
            .collect();
        assert!(missing.is_empty(), "default keybindings absent from the F1 manual: {missing:?}");
    }

    /// Mouse/selection features that have no keybinding still need their section.
    #[test]
    fn selection_and_clipboard_features_are_documented() {
        for needle in [
            "Shift+click",        // anchored selection
            "Ctrl+Shift+click",   // block variant
            "Ctrl+drag",          // block drag-select
            "double-click",
            "triple-click",
            "CLIPBOARD HISTORY",
            "Shift+Enter",        // search: previous hit
            "--cols",             // CLI
            "RT_ENGINE",
        ] {
            assert!(MANUAL.contains(needle), "manual lacks {needle:?}");
        }
    }
}
