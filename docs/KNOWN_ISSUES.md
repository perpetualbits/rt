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

## Pane drag-and-drop (2026-08-24)
- ◐ **Cross-window DRAG-WITH-CUES (the single fluid button-held gesture) is
  X11-only; carry mode covers cross-window drops everywhere; drag TEAR-OUT
  also works everywhere.** Dragging a pane/tab into ANOTHER rt window as one
  unbroken button-held motion needs the pointer in SCREEN coordinates: rt asks
  winit for `Window::inner_position()`, which Wayland has no answer for (a
  client is never told where its surface is, and `set_outer_position` is a
  no-op), so that gesture is gated on `inner_position().is_ok()`. On Wayland
  reach for **carry mode** instead (2026-08-25): `Ctrl+Shift+M` picks up the
  focused pane, `Ctrl+Shift+N` its tab (same as the "Pick Up Pane"/"Pick Up
  Tab" menu rows), or hold Ctrl while releasing a drag outside the window —
  then hover ANY rt window for the full drop-cue set and left-click to drop
  (Escape or right/middle-click cancels). Carry needs no `inner_position()`
  because each window resolves its own hover locally once the button is up,
  so it works identically on Wayland and X11. The right-click menu's "Move
  Pane to N: title" row and `Ctrl+Shift+D`/`Ctrl+Shift+J` detach remain
  available too. Dropping OUTSIDE the source window to tear out needs only
  surface-local coordinates, and the Wayland implicit grab keeps delivering
  motion past the surface edge — measured on cosmic-comp (2026-08-25,
  instrumented run: 366 out-of-bounds motion events during a 12s hold, release
  delivered outside, zero cursor-left) — so drag tear-out is enabled on
  Wayland too, with the compositor choosing the new window's placement (X11
  places it at the drop point). Wayland caveat: a live (non-carry) drag still
  cannot see other windows mid-drag, so a plain release over ANOTHER rt window
  during a button-held drag still tears out (on top of it) instead of dropping
  in — Ctrl-holding at release turns that same gesture into a carry pick-up
  instead. Lifting this remaining single-gesture gap needs a
  compositor-mediated protocol — a real data-device drag-and-drop session with
  a custom mime type would work on every compositor incl. cosmic-comp (probed
  2026-08-25: `wl_data_device_manager` v3 present, `xdg_toplevel_drag_v1`
  absent) — a separate piece of work.
- ◐ **Overlapping rt windows: hover target picked by map order, not stacking
  order.** `App::window_under_global` (X11 only, see above) walks every open
  window's `inner_position()`/`inner_size()` and returns the first whose
  content rect contains the pointer; when two rt windows overlap on screen,
  winit exposes no window-stacking order to consult, so the pick can be the
  occluded window instead of the one actually on top. rt windows rarely
  overlap in practice (each opens at its own placement), so this is a real
  but narrow edge case. Fixing it needs a stacking-order source rt doesn't
  have today (an X11 `_NET_CLIENT_LIST_STACKING` query, most plausibly).
- ◐ **A release over another rt window's WM title bar / decoration tears out
  a new window instead of dropping in.** rt only knows its own CONTENT rect
  (`Window::inner_position()`/`inner_size()`); the window manager's title bar
  and borders around that rect are invisible to it. Releasing there lands
  outside every rt window's content rect, so it reads as "the desktop" and
  tears out, even though visually the pointer was over the other rt window.

## Carry mode + held-pane cursor (2026-08-25)
- ☑ **Cross-window pane/tab drops on Wayland (and X11), without a live drag.**
  `Ctrl+Shift+M`/`Ctrl+Shift+N` (or the "Pick Up Pane"/"Pick Up Tab" menu
  rows) pick the focused pane/tab up into a modal carry; holding Ctrl while
  releasing a drag outside the window enters the same state instead of
  tearing out. While carrying, every rt window resolves its own hover and
  shows the same drop cues a live drag would (split fill, swap highlight,
  tab caret, edge band, ghost chip); a left-click commits with the same
  target semantics as a same-window/cross-window drag drop, Escape or a
  right/middle-click anywhere cancels. Also closes the old "moving a pane
  needs its titlebar" gap for titlebar-off setups: pick-up is a keyboard/menu
  action, so it needs no titlebar to grab (in-window drag-to-REARRANGE by
  mouse still does).
- ☑ **A held-pane cursor card** (accent outline, translucent body, opaque
  titlebar band, pane aspect ratio; `crates/rt/src/carry_card.rs`) rides the
  pointer as a custom cursor during any button-held drag and throughout a
  carry, on every rt window. It reverts to the normal cursor over a foreign
  app or the bare desktop — rt only owns the cursor on its own surfaces — and
  falls back to plain `CursorIcon::Grabbing` if the compositor/platform
  refuses custom cursor images.
- ◐ **The right-click context menu can overflow a short window; no scrolling
  yet.** With the pick-up rows added, the full menu is now roughly 622px
  tall; on a window shorter than that the tail rows (Preferences, Manual,
  etc.) fall off the bottom and are unreachable by mouse — the panel clamps
  to the top edge (`panel_taller_than_the_window_pins_to_the_top`) so at
  least the head rows stay reachable, but there is no menu scrolling. Use a
  keybinding for anything that falls off, or resize the window. Follow-up
  filed to add scrolling.

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
