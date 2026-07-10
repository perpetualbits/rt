//! The built-in manual overlay (F1). A scrollable egui window rendering
//! [`MANUAL`] — the full feature reference plus runnable input/output/error and
//! wiring examples. Kept in-app so it always matches the actual keybindings.

/// Render the manual window for this frame. Sets `*close = true` when dismissed.
pub fn ui(ctx: &egui::Context, close: &mut bool) {
    egui::Window::new("rt — manual")
        .default_width(720.0)
        .default_height(560.0)
        .collapsible(false)
        .anchor(egui::Align2::CENTER_CENTER, [0.0, 0.0])
        .show(ctx, |ui| {
            // Reserve room for the separator + Close button, then let the scroll
            // area fill the rest — otherwise (with auto_shrink off) it would eat
            // the button's space and push it below the window.
            const RESERVE: f32 = 36.0;
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .max_height((ui.available_height() - RESERVE).max(80.0))
                .show(ui, |ui| {
                    // Monospace so the aligned key columns and example code line up.
                    ui.add(
                        egui::Label::new(egui::RichText::new(MANUAL).monospace().size(13.0))
                            .wrap_mode(egui::TextWrapMode::Extend),
                    );
                });
            ui.separator();
            if ui.button("Close  (F1 / Esc)").clicked() {
                *close = true;
            }
        });
}

/// The manual text. Plain monospace; UPPERCASE lines are section headings.
pub const MANUAL: &str = "\
rt — a Wayland-native terminal multiplexer with instrumented borders and an
inter-pane fd patch-bay. Press F1 or Esc to close this manual.


PANES
  Ctrl+Shift+O    split horizontally (stacked)
  Ctrl+Shift+E    split vertically (side by side)
  Ctrl+Shift+A    split along the longer axis (auto)
  Ctrl+Shift+W    close the focused pane
  Ctrl+Shift+X    zoom / maximise the focused pane (toggle)
  Ctrl+Shift+R    rotate: flip the enclosing split H <-> V
  Alt+Arrows      move focus between panes
  Ctrl+Shift+Arrows   resize: grow the focused pane
  click / mouse   click-to-focus (or focus-follows-mouse in Preferences)
  drag a gutter   resize the split


TABS  &  COLUMNS
  Ctrl+Shift+T    new tab beside the focused pane
  Ctrl+PageUp/Dn  previous / next tab
  Ctrl+.  /  Ctrl+,   more / fewer newspaper columns (text flows column to
                      column; vim/less/etc. just see a taller, narrower screen)


SCROLLBACK  SEARCH
  Ctrl+Shift+F    open the search bar for the focused pane
  type            search (case-insensitive); hits are highlighted
  Enter / Down    next hit      Up  previous hit      Esc  close


GROUPS  &  BROADCAST   (type once, reach many panes)
  Ctrl+Shift+G    cycle the focused pane's input group (colour-coded marker)
  right-click menu Broadcast: Off / All / Group
                  Off = focused pane; All = every pane; Group = same-group panes


MOUSE
  click            focus a pane (or focus-follows-mouse, see Preferences)
  drag             select text; double-click = word, triple-click = line
  wheel            scroll that pane's scrollback
  middle-click     paste the PRIMARY selection
  right-click      context menu        drag a gutter   resize the split
  Ctrl+click       open a URL under the pointer
  When the program in a pane asks for the mouse (vim, htop, tmux, less
  --mouse, fzf, ...), rt forwards clicks, drags and the wheel to it instead.
  Hold SHIFT to override that and use rt's own select / scroll / menu.


BORDER INSTRUMENTS   (each pane's border is a live gauge; toggle in Preferences)
  Output   a green flow of packets orbits the border; speed and brightness
           track that pane's live output rate. Idle = still; busy = racing.
  Heat     the border is tinted by CPU load of the pane's whole session
           (shell + children), as a blackbody: dim deep-red idle, up through
           orange and yellow to white-hot, blue-white for a runaway.
  Latency  the window frame undulates purple-blue-violet and flares bright
           when the render loop misses a deadline (a CPU hogger stole a frame).


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


APPEARANCE   (Preferences: right-click menu -> Preferences...)
  font family & size (Ctrl+= / Ctrl+- / Ctrl+0 to zoom), colours & schemes,
  background opacity and colours, compositor blur, per-pane titlebars, focus-follows-mouse,
  and toggles for each border instrument and the patch-bay jacks.
  F11   fullscreen        Ctrl+Shift+C / V   copy / paste


All settings persist to  $XDG_CONFIG_HOME/rt/config.toml.
";
