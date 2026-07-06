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
- ☑ **Bold weight** now rendered from a bold font chain (DejaVu Sans Mono Bold +
  fallbacks), with a bold-italic chain for cells that are both. Bold still
  brightens ANSI colours too (standard). Verified: `docs/screenshots/bold.png`.

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

## Features implemented since
- ☑ **Right-click context menu** (Terminator-style): Split Horizontally/
  Vertically, New Tab, Close Terminal, More/Fewer Columns, More Opaque/
  Transparent, Stronger/Weaker Blur. Each entry runs the same `Action` path as
  its keybinding. Rendered in the GL layer (`crates/rt/src/menu.rs`). Verified
  rendering: `docs/screenshots/context-menu.png`. Live right-click open is
  standard winit `MouseInput` — couldn't inject synthetic mouse in the dev
  sandbox (no ydotoold; winit ignores xdotool's synthetic X events), so the
  open-on-right-click is confirmed by construction; `RT_MENU=1` opens it at
  startup for inspection.

## Focus & menu targeting (2026-07-06)
- ☑ **Focus stuck on last-created pane; no click-to-focus.** Focus only moved via
  Alt+arrows. Added `Session::focus_at(px,py)`: **left-click focuses the pane
  under the cursor**, and **right-click focuses it before opening the menu** — so
  the menu's Split/Close/Columns act on the pane you clicked, not whichever was
  focused. Unit-tested (`click_to_focus_selects_pane_under_point`). This also
  explains the earlier "menu items don't work": Close etc. WERE working, just on
  the focused (last) pane rather than the right-clicked one.
- ☑ **Focus-follows-mouse (sloppy focus)** implemented as an opt-in: enable via
  `RT_FOCUS=sloppy` at startup, the menu's "Toggle Focus-Follows-Mouse", or the
  `ToggleFocusFollowsMouse` action. On CursorMoved it focuses the pane under the
  pointer (repainting only when focus changes); over a gutter the focus sticks
  (sloppy). Default remains click-to-focus.
- ☑ **Tabbed terminals work.** Added a clickable tab strip (rt-core `tab_bars`/
  `activate_tab`/`cycle_tab`), wired NextTab/PrevTab (Ctrl+PageUp/PageDown), and
  click-to-switch. The active tab is highlighted. Tabs are labelled by number
  for now (per-pane titles are a follow-up). Verified: `docs/screenshots/tabs.png`;
  unit-tested (`tabs_cycle_switch_and_click`). Open via New Tab (Ctrl+Shift+T) or
  the menu; `RT_TABS=n` opens n tabs at startup.
- ☐ **Opacity/Blur menu items need a compositing compositor.** They change the
  window's alpha/scrim; with an opaque window or nothing behind it, there's no
  visible effect. Not a dispatch bug.

## Cursor & transparency (2026-07-06)
- ☑ **Cursor shape honours the terminal + focus.** rt now reads
  `Term::cursor_style()` (DECSCUSR) and draws Block / Underline / Beam / hidden
  accordingly — so an editor's insert (beam) vs overwrite (underline) cursor
  shows correctly. An **unfocused** pane always draws a hollow outline; the
  focused pane draws the requested shape (solid block by default). Verified:
  `docs/screenshots/cursor-focus.png`, `docs/screenshots/cursor-underline.png`.
- ☑ **Transparency was ignored** because the window was never marked transparent.
  Fixed: `Window::with_transparent(true)` + the GL config selection now prefers an
  alpha-capable config. This should make both the opacity slider AND the scrim's
  see-through effect work on a compositing Wayland compositor. (True Gaussian
  blur still only on KDE; COSMIC/GNOME have no blur protocol — use the scrim.)
  Not visually verifiable in this sandbox (no compositing capture); please
  confirm on your machine with e.g. `RT_OPACITY=0.8 RT_SCRIM=0.4`.

## Phase 0 essentials (2026-07-06)
- ☑ **Tab & window titles** from OSC (`docs/screenshots/tab-titles.png`).
- ☑ **Config persistence** — settings saved to `~/.config/rt/config.toml`, loaded
  at startup (opacity, scrim, focus-follows-mouse; more as features land).
- ☑ **Dead keys / compose** — `[user: ~ \` ' " ^ ]`. Two-part fix: (1) enabled
  winit IME (`set_ime_allowed(true)`) so IME/CJK commits arrive via `Ime::Commit`
  (keys gated during a preedit). (2) **The real fix for `'`+space→`'`:** send
  `key_event.text` (winit's already dead-key/compose-resolved text) for character
  input instead of deriving a char from `logical_key` — the composed base char
  is in `text` while `logical_key` is a `Dead`/unidentified key we were ignoring.
  Navigation/function keys still use ANSI sequences (`is_sequence_key`); Ctrl/Alt
  handled in `encode_text`. Unit-tested; normal typing re-verified. Confirm the
  `'`+space / `\``/`~`/`^`/`"` cases on a compose layout.
- ☑ **Copy/paste** (Wayland). Mouse drag-selects text (highlight verified,
  `docs/screenshots/selection.png`); Ctrl+Shift+C copies to CLIPBOARD + PRIMARY;
  Ctrl+Shift+V pastes; middle-click pastes PRIMARY; copy-on-select to PRIMARY.
  Uses smithay-clipboard (pure Wayland — no X11 crates; arboard would add X11).
  Selection is single-column panes for now (column-mode is a follow-up). The
  clipboard round-trip couldn't be inject-tested in the sandbox (this compositor
  lacks wlr-data-control; x11 dev build has no clipboard) — verify on-machine.
