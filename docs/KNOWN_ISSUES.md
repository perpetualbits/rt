# Known issues / user-reported observations

Running list so nothing gets forgotten. Status: ☐ open · ◐ in progress · ☑ fixed.

## Input / keyboard
- ☑ **mc arrow keys don't navigate.** Full-screen apps enable *application
  cursor keys* mode (DECCKM, `TermMode::APP_CURSOR`); after that, arrows must
  send SS3 (`ESC O A`), not CSI (`ESC [ A`). rt always sent CSI. Also rt never
  set `TERM`, so ncurses picked an inherited/incorrect terminfo. Fixed: set
  `TERM=xterm-256color` + `COLORTERM=truecolor`, and branch arrow/Home/End
  encoding on the pane's app-cursor mode.
- ☑ **Insert key (insert/overwrite toggle) does nothing.** rt didn't encode
  `Insert`. Fixed: sends `ESC [ 2 ~`. Also added Delete/Insert/keypad and F1–F12
  input sequences.

## Rendering / fonts
- ☑ **Braille (U+2800–U+28FF) rendered as tofu** (visible in `spiral_stress`).
  Confirmed cause: DejaVu Sans Mono has no braille (blocks/box-drawing/accents
  DO work). Fixed with a **font-fallback chain**: the renderer keeps a primary
  font + fallbacks (DejaVu Sans, Agave, …) and rasterises each glyph from the
  first font that has it (`lookup_glyph_index != 0`). Verified:
  `docs/screenshots/braille-fallback.png`.
- ☑ **Text attributes:** underline / italic / strikeout now drawn. Italic uses a
  real oblique face (DejaVu Sans Mono Oblique, with fallbacks); underline and
  strikeout are thin bars in the cell's fg. Verified:
  `docs/screenshots/text-attributes.png`. (Colour, bold→bright, dim, inverse,
  hidden were already handled.)
- ☐ **Bold weight** not rendered: BOLD only brightens the colour; a heavier font
  face isn't loaded. Minor follow-up (load DejaVu Sans Mono Bold as a weight
  chain, mirroring the italic chain).

## Lifecycle
- ☑ **Pane/window stays open after its shell exits** (Ctrl-D / `exit` / `quit`).
  alacritty_terminal sends `Event::ChildExit`; rt ignored it. Fixed: engine emits
  `PaneEvent::Exited`, the run-loop reaps the pane via `Session::close_pane`, and
  closing the last pane exits the window. Verified by an engine test + a live
  `wtype "exit"` test.

## Features not yet built (not bugs)
- ☐ Terminator-style right-click context menu (+ a preferences panel to host the
  opacity/scrim sliders).
- ☐ Multi-pane split only verified by tests, not yet screenshotted.
- ☐ Clipboard copy/paste not wired to the OS.
