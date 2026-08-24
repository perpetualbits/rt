# Terminator feature catalogue (reimplementation checklist)

An exhaustive inventory of Terminator's features, compiled from its source
(`reference/terminator/terminatorlib/`), used to drive rt's roadmap
(`docs/ROADMAP.md`). Status per item: ☑ done in rt · ◐ partial · ☐ not yet.

Legend for rt status is applied in `ROADMAP.md`; this file is the raw catalogue.

---

## 1. Global settings (`config.py` DEFAULTS['global_config'])

| Setting | Default | Meaning | rt |
|---|---|---|---|
| `focus` | click | focus mode: click / sloppy / mouse | ◐ (click + sloppy) |
| `dbus` | True | single-instance DBus IPC | ☐ |
| `handle_size` | -1 | split-handle width px | ◐ (fixed 6px gutter) |
| `geometry_hinting` | False | resize in char-cell increments | ☐ |
| `window_state` | normal | startup: normal/maximise/fullscreen/hidden | ☐ |
| `borderless` | False | no WM decorations | ☐ |
| `tab_position` | top | tab bar: top/bottom/left/right | ◐ (top only) |
| `broadcast_default` | group | default broadcast: off/group/all | ◐ (off default) |
| `close_button_on_tab` | True | per-tab close button | ☐ |
| `scroll_tabbar` | False | scroll arrows vs shrink tabs | ☐ |
| `homogeneous_tabbar` | True | equal-width tabs | ☑ |
| `hide_from_taskbar` | False | omit from taskbar | ☐ |
| `always_on_top` | False | keep above | ☐ |
| `hide_on_lose_focus` | False | auto-hide on blur | ☐ |
| `sticky` | False | all workspaces | ☐ |
| `use_custom_url_handler`/`custom_url_handler` | off | custom URL opener | ☐ |
| `inactive_color_offset` | 0.8 | dim fg of unfocused terminals | ☐ |
| `inactive_bg_color_offset` | 1.0 | dim bg of unfocused terminals | ☐ |
| `enabled_plugins` | 3 URL handlers | active plugins | ☐ |
| `ask_before_closing` | multiple_terminals | close confirm policy | ☐ |
| `putty_paste_style` (+source) | False | middle-click paste (PuTTY) | ☐ |
| `disable_mouse_paste` | False | disable middle-click paste | ☐ |
| `smart_copy` | True | copy passes through when no selection | ☐ |
| `clear_select_on_copy` | False | clear selection after copy | ☐ |
| `cell_width`/`cell_height` | 1.0 | global cell scaling | ☐ |
| `case_sensitive`/`invert_search` | — | search options | ☐ |
| `link_single_click` | False | single-click open links | ☐ |
| `title_at_bottom` | False | titlebar below terminal | ☐ |
| `detachable_tabs` | True | drag-detach tabs to new windows | ☑ (drag, X11; keyboard `Ctrl+Shift+J` everywhere) |
| `new_tab_after_current_tab` | False | insert new tab beside current | ☐ |

## 2. Profile settings (`DEFAULTS['profiles']['default']`)

- **Fonts:** `use_system_font`, `font` (Mono 10), `allow_bold`, `bold_is_bright`,
  `cell_width`/`cell_height`. — rt: ◐ (fixed DejaVu 18px; bold/italic done).
- **Colours:** `use_theme_colors`, `foreground_color` (#aaaaaa),
  `background_color` (#000000), `palette` (16-colour Tango). — rt: ◐ (built-in
  xterm palette; **not configurable** — the big gap the colour picker fills).
- **Background:** `background_type` (solid/transparent/image),
  `background_darkness` (transparency), `background_image` (+mode/align),
  `background_blur`. — rt: ◐ (opacity + scrim done; image bg ☐).
- **Cursor:** `cursor_shape` (block/ibeam/underline), `cursor_blink`,
  `cursor_color_default`, `cursor_fg_color`, `cursor_bg_color`. — rt: ◐ (shape
  from DECSCUSR + focus hollow done; blink ☐; custom colour ☐).
- **Scrolling:** `scrollbar_position` (left/right/hidden), `scroll_on_keystroke`,
  `scroll_on_output`, `scrollback_lines` (500), `scrollback_infinite`,
  `disable_mousewheel_zoom`. — rt: ◐ (10k scrollback via engine; no scrollbar UI;
  wheel scroll works in column mode).
- **Bell:** `audible_bell`, `visible_bell`, `urgent_bell`, `icon_bell`,
  `force_no_bell`. — rt: ☐ (engine surfaces Bell events; not rendered).
- **Command/exit:** `login_shell`, `use_custom_command`/`custom_command`,
  `exit_action` (close/restart/hold), `http_proxy`. — rt: ◐ (RT_EXEC hook; no
  config; exit=close only).
- **Env:** `term` (xterm-256color ✔), `colorterm` (truecolor ✔),
  `backspace_binding`, `delete_binding`, `word_chars` (double-click selection),
  `encoding`. — rt: ◐ (TERM/COLORTERM done; word_chars/selection ☐).
- **Behaviour:** `show_titlebar`, `mouse_autohide`, `copy_on_selection`,
  `split_to_group`, `autoclean_groups`. — rt: ☐.
- **Titlebar colours/fonts:** transmit/receive/inactive fg+bg, `title_font`,
  `title_hide_sizetext`. — rt: ☐ (no per-pane titlebar).

## 3. Keybindings (`DEFAULTS['keybindings']`)

- **Zoom (font):** zoom_in/out/normal (+ *_all variants). — rt ☐.
- **Pane zoom/maximise:** `toggle_zoom` (Ctrl+Shift+X), `scaled_zoom` (…Z). — rt ☐.
- **Splitting:** split_horiz (Ctrl+Shift+O ☑), split_vert (…E ☑), split_auto
  (…A, split longer axis ☐).
- **Rotate splits:** rotate_cw (Super+R), rotate_ccw. — rt ☐.
- **Resize splits:** resize_up/down/left/right (Ctrl+Shift+Arrows). — rt ☐.
- **Pane navigation:** go_up/down/left/right (Alt+Arrows ☑), go_next/prev,
  cycle_next/prev (Ctrl+Tab). — rt ◐ (directional done; cycle ☐).
- **Tabs:** new_tab ☑, next_tab/prev_tab ☑ (Ctrl+PgUp/PgDn), move_tab_left/right
  ☑ (Shift+Ctrl+PgUp/PgDn), switch_to_tab_1..10 ☐.
- **Close/windows:** close_term ☑, close_window ☑, new_window ☑ (Ctrl+Shift+I,
  one process/many windows), new_terminator ☐, hide_window ☐.
- **Clipboard:** copy ☐, paste ☐ (**essential gap**), paste_selection ☐,
  send_newline ☐.
- **Scrollback:** toggle_scrollbar, page_up/down(_half), line_up/down. — rt ☐.
- **Search:** search (Ctrl+Shift+F). — rt ☐.
- **Reset:** reset, reset_clear. — rt ☐.
- **Fullscreen:** full_screen (F11). — rt ☐.
- **Grouping/broadcast:** create_group, group_all/win/tab (+toggle/ungroup),
  broadcast_off/group/all. — rt ◐ (broadcast off/group/all in session; no keys/UI).
- **Numbering:** insert_number (Super+1), insert_padded (Super+0). — rt ☐.
- **Titles:** edit_window/tab/terminal_title. — rt ☐.
- **Layouts/profiles/prefs:** layout_launcher (Alt+L), next/previous_profile,
  preferences, preferences_keybindings (Ctrl+Shift+K), help (F1). — rt ☐.

## 4. Core UX behaviours

- **Recursive split tree** (H/V/auto, adjustable + persisted ratios). — rt ◐
  (H/V + weighted ratios; auto-split ☐; drag-resize ☐).
- **Tabs** (reorder, editable labels, per-tab close, scroll-to-switch, side-tab
  rotated text, detach to window). — rt ◐ (strip + switch + click + titles +
  drag-reorder + detach-to-new-window done; per-tab close button, tab
  position and scroll-to-switch/side-tab rotated text ☐).
- **Drag-and-drop** terminals (titlebar drag → drop-zone split; drag preview;
  text/URI drop paste; detach tabs). — rt ☑ in-window and cross-window on X11
  (split/swap/reorder cues, ghost chip, drag tear-out to a new window);
  Wayland is in-window drag only, cross-window via the "Move Pane to N" menu
  and keyboard detach. Text/URI drop paste still ☐.
- **Zoom/maximise a pane** (`toggle_zoom`, `scaled_zoom`). — rt ☐.
- **Rotate splits** (H↔V). — rt ☐.
- **Resize splits** via keys. — rt ☐.
- **Saved layouts** (full tree with type/parent/ratio/profile/command; launcher
  dialog; `-l` CLI). — rt ◐ (RT_SPLIT/RT_COLUMNS/RT_TABS startup hooks are the
  seed; no save/load/launcher).
- **Titlebar** (title from shell/OSC or custom; group label; size text; bell
  icon; click-to-edit; broadcast colour states). — rt ☐.
- **Search bar** (incremental scrollback search, next/prev, case, invert, wrap).
  — rt ☐.

## 5. Context menu (`terminal_popup_menu.py`) — rt ◐

On a URL match: Open link / Send email / Call VoIP + Copy address, sep. Then:
**Copy · Paste**, sep · **Set Window Title** · **Split Auto/Horizontally/
Vertically · Open Tab**, sep · **Close**, sep · **Zoom / Maximize terminal**
(or Restore all when zoomed) · **Grouping** submenu · Relaunch Command (held) ·
**Read only** ✔ · **Show scrollbar** ✔ · **Preferences** · **_Colors** theme
submenu (Solarized/Monokai/Dracula/Gruvbox/Nord/… + Custom colour dialog) ·
**Profiles** radio submenu · **Layouts…** submenu · plugin items.
rt today: Split H/V, New Tab, Close, Columns, Opacity/Scrim, Focus-follows-mouse.

## 6. Preferences dialog (`preferences.glade`, `prefseditor.py`) — rt ☐

Notebook: **Global · Profiles · Layouts · Keybindings · Plugins · About**.
- **Global** → Behaviour (window state, always-on-top, hide-on-lose-focus, DBus,
  detachable tabs, ask-before-closing, mouse-focus, broadcast-default, PuTTY
  paste, smart-copy, single-click-link, …) + Appearance (borders, unfocused
  brightness slider, separator size, cell w/h sliders, tab position, homogeneous
  tabs, title-at-bottom, new-tab-after-current, decoration style).
- **Profiles** → list + Add/Remove/Copy, and 7 sub-tabs: General (font chooser,
  cursor shape/blink/colour pickers, bell), Command (login shell, custom command,
  exit action), **Colors** (fg/bg swatches, 11 scheme presets, **16-colour
  palette editor**, bold-is-bright), Background (solid/transparent/image + image
  chooser + darkness slider + KDE blur), Scrolling (scrollbar position, scroll-on-
  output/keystroke, infinite, **scrollback-lines spinner**), Compatibility
  (backspace/delete bindings), Titlebar (6 colour pickers + font).
- **Layouts** → layout list + hierarchy tree + per-node profile/command/cwd.
- **Keybindings** → tree of ~90 actions with an accelerator-capture cell.
- **Plugins** → enable/disable toggle list.

**Rich widgets a reimplementation needs (→ egui, ADR-0004):** ~8 colour buttons
+ 18 custom colour swatches (fg/bg + 16-palette), 2 font choosers, 1 image
chooser, ~17 combos, spinbutton, 6 sliders, 5 tree/list views, an accelerator
capture, ~41 checkboxes, radio groups.

## 7. Grouping / broadcast (`terminator.py`, `titlebar.py`) — rt ◐

Per-terminal group name; global broadcast mode all/group/off. Group titlebar
with clickable group icon + editable label, colour-coded by transmit/receive/
inactive. Group menu: New group…, existing groups (radio) + None, Remove group,
Group/Ungroup all in window/tab, Broadcast all/group/off, Split-to-group,
Autoclean-groups, Insert number / padded number / name. Auto-named groups.
rt today: broadcast off/group/all routing in the session; no titlebar/menu/keys.

## 8. Layouts (`layout.py`, `layoutlauncher.py`) — rt ◐

Layout = serialized window/paned/notebook/terminal tree (position, profile,
command, cwd, group). Save via prefs or session plugins; launch via `-l name` or
the layout launcher (treeview + double-click). rt today: RT_SPLIT/RT_COLUMNS/
RT_TABS startup hooks are the seed; no save/load/launcher.

## 9. Search bar (`searchbar.py`) — rt ☐

In-terminal scrollback find: entry, Prev/Next, Match-Case / Wrap / Invert
checkboxes; Esc closes. (alacritty_terminal has a `RegexSearch` we can drive.)

## 10. IPC / remotinator (`ipc.py`) — rt ☐

DBus service exposing: new_window, new_tab, hsplit, vsplit, get_terminals,
get_focused_terminal, get/set tab title, bg_img, switch_profile,
reload_configuration, toggle_visibility, … + CLI `-u/-p/-f/-x/-t/-T`. A scripting
surface; rt could expose an equivalent over a Unix socket or zbus/DBus.

## 11. Plugins (`plugins/*.py`) — rt ☐

activitywatch (activity/silence alerts), auto_theme (follow system dark/light),
command_notify (notify on long command finish), custom_commands (user command
menu), dir_open (open cwd in file manager), insert_term_name, logger (log
scrollback to file), url_handlers (Launchpad/apt/maven), mousefree_url_handler
(keyboard URL nav), remote (clone SSH/docker sessions into splits),
run_cmd_on_match (regex→command), save_*_session_layout, terminalshot (PNG).
