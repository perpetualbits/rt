# rt roadmap — reaching Terminator feature parity

Derived from `docs/TERMINATOR_FEATURES.md` (full catalogue) and ADR-0004 (adopt
egui for chrome). Phases are ordered by user-felt value and dependencies. Each
item links back to a catalogue section. `[user]` marks something you called out.

Guiding split (ADR-0004): **terminal grid = custom GL renderer** (fast, done);
**chrome (dialogs, pickers, menus) = egui** where rich widgets are needed.

---

## Phase 0 — Input & essentials (no egui; highest priority) `[user]`
Small, high-impact, unblocks daily use.

1. **Dead keys / compose (IME).** `[user]` Accent composition (´+o→ó, ~+n→ñ, …)
   and general IME. winit: `window.set_ime_allowed(true)` + handle
   `WindowEvent::Ime(Commit/Preedit)`; suppress direct key input while a preedit
   is active. Mirror alacritty's IME handling. (cat. §3)
2. **Copy / paste.** `[user, essential]` Wayland clipboard (via `arboard` or
   winit's clipboard) + PRIMARY selection. Bindings Ctrl+Shift+C/V; menu items;
   middle-click paste (with the PuTTY-style options later). Needs text selection
   (Phase 2) for copy source; paste works immediately. (cat. §3, §5)
3. **Config persistence.** `[user]` Load/save `~/.config/rt/config.toml` (serde +
   toml). Persist `Settings` (opacity, scrim, focus mode), keymap overrides, and
   defaults. "Settings not remembered" → fixed. Foundation for the prefs dialog.
   (cat. §1, §2)
4. **Tab & window titles.** `[user]` We already receive OSC title events
   (`PaneEvent::Title`); store per-pane title, label tabs with it (falling back
   to a number), and set the window title from the focused pane. (cat. §4, §5)

## Phase 1 — egui integration + preferences + configurable colours
Where the widget toolkit pays off.

5. **Adopt egui.** Bump `glow` 0.16→0.17; add `egui`, `egui-winit`, `egui_glow`;
   render egui as an overlay after the terminal each frame; route input through
   `egui-winit` first (ADR-0004).
6. **Configurable colours.** `[user]` fg/background/cursor + the 16-colour
   palette, edited with egui colour pickers; theme presets (Solarized, Gruvbox,
   Nord, Dracula, …) like Terminator's `_Colors` menu. Wire the chosen palette
   into `rt-engine`'s resolver (today it's a fixed xterm palette). (cat. §2, §5)
7. **Preferences dialog (egui).** Pages mirroring Terminator: Global (focus mode,
   broadcast default, tab position, close policy…), Profile (font chooser, cursor
   shape/blink/colour, bell, scrollback lines, custom command, exit action),
   Appearance (opacity/scrim sliders, colours). Persists via Phase 0 config.
   (cat. §6)
8. **Font configuration.** Font family + size chooser (replaces the fixed DejaVu
   18px); rebuild the renderer's font chains on change. (cat. §2)

## Phase 2 — Terminal UX parity
9.  **Mouse selection + copy-on-select** ✅ — drag-select copies on release;
    double-click selects the word, triple-click the line; all copy to PRIMARY.
    (cat. §2, §5)
10. **URL detection + open** ✅ — Ctrl+click on an http/https/ftp/file/mailto URL
    opens it via xdg-open (trailing sentence punctuation trimmed). (cat. §5, §11)
11. **Pane zoom / maximise** ✅ — toggle_zoom (Ctrl+Shift+X / menu). (cat. §3, §4)
12. **Rotate / resize / auto splits** ✅ — Rotate (Ctrl+Shift+R) flips the enclosing
    split's orientation; keyboard resize (Ctrl+Shift+arrows) grows the focused pane
    into its neighbour on that axis; split_auto (Ctrl+Shift+A) splits along the
    pane's longer axis. Drag-to-resize the split gutter ✅. (cat. §3, §4)
13. **Scrollback search bar** ✅ `[user — wanted this specifically]` — Ctrl+Shift+F
    (or menu) opens a find bar over the whole scrollback; case-insensitive
    substring match; every hit highlighted (current hit brighter), Enter/Shift+Enter
    (or Next/Prev) to jump, results refresh live as output streams in, Esc closes.
    Implemented as a plain cell-accurate search (not RegexSearch) so highlights
    line up exactly with the grid. (cat. §9)
14. **Bell** ✅ — visible flash (translucent white overlay, 150ms). (cat. §2)
15. **Per-pane titlebar** with editable title + size text (optional, config
    `show_titlebar`). (cat. §4)
16. **Scrollbar UI** + scrollback config (position, infinite). (cat. §2)

## Phase 3 — Tabs, layouts, grouping polish
17. **Tabs:** reorder (drag), per-tab close button, detach-to-new-window,
    tab position (bottom/left/right), move_tab_left/right, switch_to_tab_N.
    (cat. §4)
18. **Saved layouts:** serialize the tree (serde) + save/load + a launcher; `-l`
    CLI option. The RT_SPLIT/RT_COLUMNS/RT_TABS hooks become the format seed.
    (cat. §8)
19. **Grouping / broadcast UI:** group titlebar + group menu, broadcast keys
    (broadcast_off/group/all), create/name/ungroup groups, insert terminal
    number. (cat. §7)

## Phase 4 — Profiles, IPC, plugins
20. **Profiles:** multiple named profiles (colours/font/command/…); per-pane
    profile; profile switch. (cat. §2)
21. **IPC / remotinator:** a scripting surface (zbus/DBus `net.tenshu`-style, or a
    Unix socket) — new_window/tab, hsplit/vsplit, get/set titles, switch_profile,
    reload_config. + matching CLI opts. (cat. §10)
22. **Plugins:** a plugin mechanism, or built-in equivalents of the useful ones
    (logger, custom-commands menu, activity/silence watch, terminalshot,
    command-finish notify). (cat. §11)

## Cross-cutting / deferred
- **Packaging matrix** (was M6): `.deb` / `.rpm` / arch × x86_64 / aarch64 /
  riscv64. Deferred until the feature set stabilises; egui/glow don't change the
  cross-compile story materially.
- **Background image**, cell scaling, geometry hinting. (cat. §1, §2)
- **Window flags** — always-on-top, sticky, hide-from-taskbar, borderless: these
  depend on Wayland/compositor support (`xdg-toplevel`, wlr protocols) and may be
  partial or compositor-specific. Blur stays KDE-only (ADR-0003/APPEARANCE).

---

## Suggested near-term order (my recommendation)
**Phase 0 in full** (dead keys → copy/paste → config persistence → tab titles),
then **Phase 1 items 5–6** (egui + configurable colours — the biggest visible
win). That clears every item you named. Then iterate Phase 2 by taste.
