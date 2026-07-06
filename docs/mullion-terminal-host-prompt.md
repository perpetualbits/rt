# Mullion "terminal-host" mode — implementation brief

*A prompt to hand to an agent (or yourself) working **inside the mullion repo**.
Written from rt's side: rt is the first consumer, but the mode should stay
engine-agnostic. Everything below is a requirement or a recommendation — where it
says "map to mullion's primitives," use mullion's real API (regions, borders,
bitstreams, choosers); don't invent a parallel one.*

---

## 1. Mission

Add a mode to mullion in which its **regions host live terminal grids**, so a
terminal multiplexer can use mullion for *all* of its chrome — splitting,
merging, resizing, titles, menus, colour-pickers, and the signature edge
"bitstream" animations — instead of a custom GL layer plus egui.

The proof of success is a `terminal-host` example: a window full of mullion
regions, each running a real shell, that you can split/merge/resize/zoom, with
mullion drawing every pixel of the frame around the grids (and, ideally, the
grids too).

## 2. Background — what rt is, and why mullion

- **rt** is a Wayland-native terminal multiplexer in Rust: a loose port of
  Terminator, using `alacritty_terminal` as the VTE/parser/grid engine.
- rt today renders in **two layers**: a bespoke GL renderer for the terminal
  grid, and **egui** for chrome (preferences, colour picker, search bar). It has
  a hand-rolled layout tree (splits/tabs), a right-click menu, per-pane
  titlebars, newspaper-column re-tiling, scrollback search, input groups +
  broadcast, and Wayland translucency/blur.
- This brief replaces that chrome — and optionally the grid compositing — with
  mullion. mullion already does the hard, distinctive parts (resizable/splittable
  regions, borders, edge bitstreams, titles, choosers, menus, colour-pickers); a
  "host a terminal grid inside a region" mode is the missing seam.

## 3. The contract (read this first — it's the whole design)

Keep mullion **engine-agnostic**. Define a boundary; rt implements the app side.

### 3.1 What the app (terminal engine) provides to mullion

A trait mullion calls — model it on rt-engine's existing surface:

```
trait TerminalHost {
    // Lifecycle
    fn spawn(&mut self, cols: usize, rows: usize) -> GridId;   // start a PTY+shell
    fn close(&mut self, grid: GridId);

    // Sizing (mullion owns geometry; it tells the host the cell dims)
    fn resize(&mut self, grid: GridId, cols: usize, rows: usize);

    // Per-frame content: an immutable snapshot of the visible grid.
    //   Cell = { ch: char, fg: Rgb, bg: Rgb, attrs: {bold,italic,underline,strikeout},
    //            (cursor position + shape reported separately) }
    fn snapshot(&self, grid: GridId) -> GridSnapshot;

    // Input (mullion forwards, host encodes to the PTY)
    fn feed_key(&mut self, grid: GridId, key: KeyEvent, mods: Mods);
    fn feed_text(&mut self, grid: GridId, text: &str);          // composed/IME/paste
    fn feed_scroll(&mut self, grid: GridId, lines: isize);

    // Scrollback + search (for the search bar and scrollbar)
    fn scroll_info(&self, grid: GridId) -> (offset, history, screen);
    fn search(&self, grid: GridId, needle: &str, case_sensitive: bool) -> Vec<Hit>;
    fn scroll_to_line(&self, grid: GridId, line: i32);

    // Async events the host surfaces each tick
    fn drain_events(&mut self, grid: GridId) -> Vec<HostEvent>;  // Title, Bell, Exited, Wakeup
}
```

rt-engine already exposes almost exactly this (`spawn`/`write`/`resize`/
`snapshot`/`scroll`/`scroll_info`/`search`/`scroll_to_line`/`drain_events`), so
the adapter is thin.

### 3.2 What mullion provides to the app

- A **Region** that can be flagged as a *grid surface*: mullion reserves its
  content rectangle and, each frame, either
  - **(A, recommended)** hands the host a drawable surface + cell metrics (origin,
    cell_w, cell_h) and lets the host rasterise glyphs into it (reuse rt's
    `fontdue` rasteriser, premultiplied-alpha blending), **or**
  - **(B)** accepts a CPU cell buffer from `snapshot()` and rasterises it with
    mullion's own font stack.
  Prefer **(A)**: it keeps the fast path in the engine, reuses rt's font
  discovery/fallback chains, and lets mullion stay out of the glyph business.
  Define the surface handoff explicitly (GL texture / wgpu view / raw fd —
  whatever mullion's renderer already speaks).
- **Input plumbing**: keyboard (incl. modifiers), IME preedit/commit, mouse
  buttons/motion, and wheel are delivered to the focused region's host callbacks
  with **precise cell coordinates** (mullion knows the grid's cell metrics, so it
  maps pixels→(col,row) and hands the host cell coords for selection/URL hits).
- **Region-tree operations** as first-class, animated ops: split (h / v / auto by
  longer axis), rotate a split's orientation, resize (keyboard step + drag the
  border), close/merge, and **zoom/maximise** one region to fill the window.
- **Titles**: a per-region title binding driven by the host's `Title` event.
- **Menus / choosers**: a per-region context menu and modal choosers, drawn as
  mullion widgets.

## 4. Functional parity checklist (derived from rt's feature set)

Everything rt does today must be expressible through the contract above:

- **Layout**: N regions in a resizable/splittable/mergeable tree; split h/v/auto;
  rotate; keyboard + drag resize; close/merge; **zoom/maximise**; **tabs** within
  a region slot (only the active tab hosts a live grid).
- **Per-region titlebar** (toggleable): title, `COLS×ROWS` size, a **group
  swatch**, and a focus highlight. mullion should treat this as a region
  decoration, not part of the grid content rectangle.
- **Focus**: click-to-focus **and** focus-follows-mouse (sloppy). The **bitstream
  border** is the focus indicator — see §5.
- **Context menu**: split (h/v/auto), rotate, new tab, close, columns ±, broadcast
  off/all/group, cycle group, opacity/scrim ±, preferences.
- **Preferences + colour picker** as mullion choosers: font size/family,
  background opacity, scrim/blur strength, foreground/background + the 16 ANSI
  colours, scheme presets, focus-follows-mouse, show-titlebars.
- **Newspaper columns**: a region can present its grid **re-tiled into N columns**
  — the host feeds a tall snapshot (`count·rows` tall) and mullion (or the host,
  via option A) slices it into side-by-side columns with a gap. mullion must let a
  region's content be drawn with **host-defined geometry**, not assume one
  contiguous grid.
- **Scrollback search bar**: an overlay input attached to a region; the host's
  `search()` returns hits; mullion highlights the hit cells (host draws, or
  mullion overlays quads at given cells), with next/prev/close and a hit counter.
- **Scrollbar**: per region, from `scroll_info()`.
- **Groups + broadcast**: tag regions into input groups (colour-coded swatch /
  border hue); mullion surfaces the tagging UI and the broadcast-mode indicator;
  the host performs the actual input fan-out (mullion just forwards to the focused
  region, host decides who else gets it).
- **Bell**: a transient visual flash on the region on the `Bell` event.
- **Selection + copy-on-select**, **double-click word / triple-click line**,
  **Ctrl-click URL open**: mullion forwards precise cell coords + click counts;
  the host owns the semantics (word boundaries, URL scheme detection, PRIMARY
  clipboard).
- **Translucency + blur** (Wayland): per-window opacity and a scrim/blur behind
  the grids; premultiplied-alpha compositing.

## 5. The mullion-native payoff (why do this at all)

This is the point — don't treat it as decoration:

- **Bitstream borders as the live focus/activity indicator.** The focused
  region's border runs mullion's bitstream animation. Modulate it by PTY
  activity: idle = calm/slow, streaming output = faster/denser, **bell = a
  burst**. `drain_events()` gives you `Wakeup`/`Bell` to drive this. This makes
  "which pane is talking" glanceable in a way no static border can.
- **Split / merge / resize as animated region ops**, not instant rectangle swaps.
- **Titles, places, choices** rendered as mullion choosers rather than a bespoke
  egui panel — one coherent visual language for the whole window.

## 6. Non-goals & constraints

- **Wayland-native; no X11.** rt's ADR-0003 drops X11 entirely; the terminal-host
  mode must add zero X11 dependencies to a default build.
- **Do not reimplement the VTE/parser or scrollback.** That stays in the engine
  (`alacritty_terminal`). mullion hosts and composites; it never parses escape
  sequences.
- **Performance**: target 60 fps; the grid path must be cheap — dirty-region or
  texture upload, not a full re-rasterise of every glyph every frame when nothing
  changed. Input latency must stay low (no extra frame of buffering on keypress).
- **Engine-agnostic**: prefer the `TerminalHost` trait (option A in §3.2) over
  embedding a specific engine, so mullion never depends on `alacritty_terminal`.

## 7. Deliverables

1. The `TerminalHost` trait (or mullion's idiomatic equivalent) + the region
   "grid surface" mode + docs on the surface handoff.
2. A **`terminal-host` example** driving a real shell: split/merge/resize/zoom,
   titlebars, context menu, one chooser (preferences), and the bitstream border
   tracking focus + activity.
3. A Wayland translucency/blur demo.
4. Tests where headless-feasible: region-tree ops (split/merge/resize/rotate/
   zoom), cell-coordinate mapping, search-hit highlighting geometry.

## 8. Acceptance criteria

- You can split/merge/resize/zoom regions, each running its own shell, with
  mullion drawing the entire frame.
- Titlebars, context menu, preferences, and colour picker are all mullion-drawn.
- The bitstream border tracks focus and bursts on the bell.
- Newspaper-columns and scrollback-search both work inside a hosted region.
- Zero X11 in a default build; translucency works on a real Wayland compositor.

## 9. Suggested milestones

1. `TerminalHost` trait + **one static region hosting a shell** — grid blit in,
   keystrokes out. Prove the surface handoff and input path end-to-end.
2. **Region tree**: split / merge / resize / focus, with the **bitstream border**
   as the focus indicator.
3. **Decorations**: per-region titlebars, right-click context menu, tabs.
4. **Choosers**: preferences + colour picker; Wayland translucency/blur.
5. **Terminal extras**: newspaper columns, scrollback search bar, groups +
   broadcast, bell flash.

---

### Appendix — where rt's reference implementations live (for the adapter)

- Engine surface: `crates/rt-engine/src/lib.rs`
  (`spawn`/`write`/`resize`/`snapshot`/`scroll`/`scroll_info`/`search`/
  `scroll_to_line`/`drain_events`; `SnapCell`, `CursorShape`, `SearchMatch`).
- Layout tree (split/rotate/resize/zoom/tabs): `crates/rt-core/src/layout.rs`.
- Controller (focus, groups, broadcast, columns, content-rect/titlebar sizing):
  `crates/rt-session/src/lib.rs`.
- Current chrome to reproduce in mullion: `crates/rt/src/{menu.rs,preferences.rs}`
  and the search bar + titlebar drawing in `crates/rt/src/main.rs`.

These are the *behaviours* to match; mullion should express them in its own
idiom, not copy the rendering code.
