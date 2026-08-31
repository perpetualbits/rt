/* project-map.js — data for rt's interactive project map.
 *
 * Consumed by project-map.html (a project-agnostic renderer). Status is derived
 * from docs/ROADMAP.md, docs/own-engine-plan.md, docs/engine-divergence.md, the
 * README, and what actually exists in-tree — not invented. Keep in sync with the
 * roadmap whenever status changes.
 */
window.PROJECT_MAP = {
  project: {
    name: "rt",
    tagline: "A Wayland-native tiling terminal multiplexer on its own verified VT engine",
    repo: "github.com/perpetualbits/rt",
    updated: "2026-08-31"
  },

  statuses: {
    done:    { label: "Shipped",       hint: "In the tree, tested, in daily use" },
    active:  { label: "In progress",   hint: "Being built or hardened right now" },
    planned: { label: "Planned",       hint: "On the roadmap; not yet built" },
    seam:    { label: "Seam / oracle", hint: "Engine-agnostic boundary or the differential oracle" }
  },

  // Architectural bands, drawn top → bottom.
  layers: [
    { id: "frontend", label: "Frontend & Rendering", hint: "winit run-loop, GL + XRender renderers, input mapping" },
    { id: "chrome",   label: "Chrome & Interaction", hint: "Native dialogs, selection, instruments, patch-bay" },
    { id: "control",  label: "Session & Model",      hint: "Layout tree, focus, broadcast, configuration" },
    { id: "seam",     label: "Engine Seam",          hint: "Engine-agnostic snapshot/damage + impl selection" },
    { id: "engine",   label: "In-house VT Engine",   hint: "Clean-room parser + Term, verified cell-for-cell" },
    { id: "verify",   label: "Verification",         hint: "Differential harness driven by the vendored oracle" }
  ],

  nodes: [
    /* ---- Frontend & Rendering ---- */
    {
      id: "rt-app", label: "rt (run-loop)", layer: "frontend", status: "done",
      tags: ["binary", "winit"],
      desc: "The rt binary: the winit event loop that owns the window, drives one frame per change, maps keyboard/mouse/touch/stylus into rt actions or PTY bytes, and prefers native Wayland (never XWayland), falling back to X11. Everything else hangs off this loop. On winit 0.31, whose unified pointer events and tablet_v2 support are what let a finger or a pen reach the window at all — Wayland emulates no pointer for either.",
      files: ["crates/rt/src/main.rs", "crates/rt/src/input.rs", "crates/rt/src/touch.rs", "vendor/winit-wayland"],
      specs: [
        { label: "Touch & stylus", href: "docs/touch-and-stylus.md" },
        { label: "Vendored winit-wayland", href: "docs/vendored-winit-wayland.md" }
      ],
      parts: [
        { label: "Input mapping (keys → PTY / actions)", status: "done", desc: "Dead-key/IME compose, app-cursor-aware arrow encoding, keymap chords." },
        { label: "Frame scheduler", status: "done", desc: "Idle-throttled redraws; forces full frames only when needed." },
        { label: "Touch & stylus", status: "done", desc: "Tap = click, one-finger drag = drag/select, two-finger drag = scroll, stylus = mouse. The window decoration takes finger and pen as well, via a patched winit (vendor/winit-wayland) that routes touch and tablet events to the frame upstream drops." }
      ],
      deps: ["rt-session", "render-gl", "render-xrender", "damage"]
    },
    {
      id: "render-gl", label: "GL renderer", layer: "frontend", status: "done",
      tags: ["OpenGL", "glyphs"],
      desc: "The custom OpenGL glyph-atlas renderer used on Wayland and local X11. Rasterises glyphs on the CPU (anti-aliased coverage masks) through a multi-font fallback chain, then blits cells on the GPU. Handles bold/italic/underline, truecolour, and compositor blur.",
      files: ["crates/rt/src/render.rs", "crates/rt/src/gl_backend.rs", "crates/rt/src/raster.rs", "crates/rt/src/blur.rs"],
      specs: [],
      parts: [
        { label: "Font-fallback chain", status: "done", desc: "Primary + fallbacks (DejaVu, Agave …) so braille/box glyphs aren't tofu." },
        { label: "Background blur", status: "done", desc: "Wayland ext-background-effect-v1 / KDE; X11 _KDE_NET_WM_BLUR." }
      ],
      deps: []
    },
    {
      id: "render-xrender", label: "XRender backend", layer: "frontend", status: "done",
      tags: ["ssh -X", "perf"],
      desc: "A server-side XRender backend for remote X11 (ssh -X), where the GL path is unusable. Presents from a back-pixmap with a server-side CopyArea, adds a scroll-blit fast path (measured ~20–29× cheaper than a full redraw on the slow riscv board over ssh -X), and supports remote translucency.",
      files: ["crates/rt/src/xrender_backend.rs", "crates/rt/src/backend.rs", "crates/rt/src/x11_present.rs", "crates/rt/src/x11_blur.rs"],
      specs: [
        { label: "XRender backend design", href: "docs/superpowers/specs/2026-07-13-mechanism-c-xrender-backend-design.md" },
        { label: "X11 present design", href: "docs/superpowers/specs/2026-07-12-damage-rendering-x11-present-design.md" },
        { label: "Remote translucency", href: "docs/superpowers/specs/2026-07-17-remote-translucency-design.md" }
      ],
      parts: [
        { label: "Scroll-blit fast path", status: "done", desc: "Intra-pixmap CopyArea on scroll, gated per-pane to avoid wire ghosts." },
        { label: "Remote translucency", status: "done", desc: "32-bit depth + premultiplied SRC clear over ssh -X." }
      ],
      deps: ["damage"]
    },
    {
      id: "damage", label: "Damage tracking", layer: "frontend", status: "done",
      tags: ["Phase perf"],
      desc: "The damage pipeline that turns 'what changed' into the minimum pixels redrawn. The engine emits native per-line dirty spans / scroll signals; the renderer translates them into scissored draws and (on XRender) blits — no per-frame full-grid diff.",
      files: ["crates/rt/src/damage.rs"],
      specs: [
        { label: "Damage-based rendering", href: "docs/superpowers/specs/2026-07-11-damage-based-rendering-design.md" },
        { label: "Damage → X11 present", href: "docs/superpowers/specs/2026-07-12-damage-rendering-x11-present-design.md" }
      ],
      parts: [],
      deps: ["rt-engine"]
    },
    {
      id: "rt-mux", label: "rt-mux (text mode)", layer: "frontend", status: "active",
      tags: ["sibling", "mullion"],
      desc: "A text-mode sibling of rt: a tmux-style multiplexer that runs inside any terminal and draws with characters (via the mullion TUI engine) while reusing the exact same rt-engine panes. A working prototype validating 'cells into cells' — the mullion-hosts-terminals idea — at text scope.",
      files: ["crates/rt-mux/src/main.rs", "docs/rt-mux.md"],
      specs: [{ label: "rt-mux design", href: "docs/rt-mux.md" }],
      parts: [],
      deps: ["rt-engine"]
    },
    {
      id: "multiwindow", label: "Multi-window & drag-and-drop", layer: "frontend", status: "done",
      tags: ["Phase 3", "X11 + Wayland"],
      desc: "One rt process, any number of windows (App::windows keyed by WindowId; Ctrl+Shift+I opens one, each closes independently, rt exits with the last). A pane or tab tears out with a key (Ctrl+Shift+D / Ctrl+Shift+J) or by dragging it to bare desktop. In-window drag (pane by titlebar, tab by label) gives live cues — half-pane split fill, whole-pane swap, tab-insert caret, window-edge root-split band, a ghost chip, source dim — on both backends. Cross-window drag-with-cues (the single fluid button-held gesture) is X11-only (gated on winit's inner_position, which Wayland never answers); drag tear-out works on Wayland too (surface-local out-of-bounds release, compositor-placed — verified on cosmic-comp). Carry mode now covers cross-window drops everywhere: Ctrl+Shift+M/N pick up the focused pane/tab, a middle-click on a titlebar or tab label picks up that pane/tab one-handed, or Ctrl-held release a drag outside the window, then hovering ANY rt window (Wayland or X11) shows the full drop-cue set and a left-click commits with the same semantics as the live drag; Escape or a right/middle-click cancels. While a button is held mid-drag, or throughout a carry, the pointer wears a held-pane card cursor (accent outline, translucent body, opaque titlebar band, pane aspect ratio) built as a pure-RGBA custom cursor; it reverts to the foreign app's or desktop's own cursor once the pointer leaves every rt window (cursor ownership stays with that surface on both Wayland and X11), and falls back to plain CursorIcon::Grabbing if the compositor refuses custom cursor images. A cross-window move or tear-out cuts any patch-bay wire that would end up spanning two windows; a wire wholly inside the moved payload travels with it.",
      files: ["crates/rt/src/main.rs", "crates/rt/src/dragdrop.rs", "crates/rt/src/chrome/dragdrop.rs", "crates/rt/src/carry_card.rs", "crates/rt-session/src/lib.rs", "crates/rt-core/src/layout.rs"],
      specs: [
        { label: "Pane drag-and-drop + multi-window design", href: "docs/superpowers/specs/2026-08-24-pane-dragdrop-multiwindow-design.md" },
        { label: "Pane drag-and-drop + multi-window plan", href: "docs/superpowers/plans/2026-08-24-pane-dragdrop-multiwindow.md" },
        { label: "Carry mode + held-pane cursor design", href: "docs/superpowers/specs/2026-08-25-carry-mode-design.md" },
        { label: "Carry mode + held-pane cursor plan", href: "docs/superpowers/plans/2026-08-25-carry-mode.md" }
      ],
      parts: [
        { label: "One process, many windows", status: "done", desc: "App::windows map, WindowId routing, per-window close, process-global PaneId allocation." },
        { label: "Keyboard tear-out", status: "done", desc: "DetachPane/DetachTab (Ctrl+Shift+D/J) into a new window; MoveTabLeft/Right for in-tab-strip reorder." },
        { label: "In-window drag + cues", status: "done", desc: "Pure drop-target resolver + native cue rendering; needs the pane titlebar for pane payloads." },
        { label: "Cross-window drag & tear-out (X11)", status: "done", desc: "Live cross-window cues and drop, gated on inner_position(); wire-cutting on any cross-window move." },
        { label: "Move-to-window menu (Wayland parity)", status: "done", desc: "\"Move Pane to N: title\" context-menu rows send a pane to another open window without drag." },
        { label: "Carry mode (pick up, aim, click to drop)", status: "done", desc: "Ctrl+Shift+M/N pickup (also \"Pick Up Pane\"/\"Pick Up Tab\" menu rows, middle-click a titlebar or tab label, or a Ctrl-held drag-release outside the window) enters carry; every rt window resolves its own hover cues, a left-click commits cross-window on Wayland and X11 alike, Escape/right/middle-click cancels (middle-click is symmetric: it also picks up when idle). Works with titlebars off." },
        { label: "Held-pane cursor card", status: "done", desc: "Pure-RGBA card (accent outline, translucent body, titlebar band, pane aspect ratio) set as the custom cursor during a held-button drag and throughout a carry; reverts on foreign surfaces, falls back to CursorIcon::Grabbing if the platform refuses custom cursor images." }
      ],
      deps: ["rt-session"]
    },

    /* ---- Chrome & Interaction ---- */
    {
      id: "selection", label: "Text selection", layer: "chrome", status: "done",
      tags: ["v0.3.12"],
      desc: "All the ways you grab text: drag-select with copy-on-release, wrap-aware double-click word and triple-click logical-line select, and the new anchored mode — Shift+click drops a start anchor, you navigate freely (accelerating arrows, scrollbar, Page/Home/End) with the anchor pinned to its content, and a second Shift+click or Enter commits and copies. Ctrl gives a rectangular block.",
      files: ["crates/rt/src/select.rs", "crates/rt/src/clipboard.rs"],
      specs: [{ label: "Anchored selection design", href: "docs/superpowers/specs/2026-07-24-anchored-selection-design.md" }],
      parts: [
        { label: "Drag / word / line select", status: "done", desc: "Copy-on-select to PRIMARY; wrap-aware word and logical-line." },
        { label: "Anchored mode", status: "done", desc: "Shipped v0.3.12: Shift-click anchor → navigate → commit, block via Ctrl." },
        { label: "Clipboard history in titlebar (B)", status: "done", desc: "Shipped v0.3.13: in-memory MRU ring (cap 20, dedup, no disk); a ⎘ N titlebar affordance + Ctrl+Shift+H open a native overlay; picking a clip pastes it and promotes it to CLIPBOARD+PRIMARY." },
        { label: "Drag auto-scroll acceleration (C)", status: "done", desc: "Shipped v0.3.14: drag-select past the pane edge ramps the auto-scroll the longer the edge is held (capped), reusing the arrow-accel curve + prefs; off ⇒ flat 1 line/35ms. Distance ruled out — terminal runs vertically maximized." }
      ],
      deps: ["rt-session"]
    },
    {
      id: "chrome-prefs", label: "Preferences", layer: "chrome", status: "done",
      tags: ["native chrome"],
      desc: "A native (not egui) preferences dialog drawn with the same glyph pipeline as the terminal: toggles, steppers, and section headers over font, appearance, behaviour, scrollback, instruments, and the arrow-key acceleration controls. Persists through the rt-config store.",
      files: ["crates/rt/src/chrome/prefs.rs", "crates/rt/src/prefs_model.rs"],
      specs: [
        { label: "Native preferences design", href: "docs/superpowers/specs/2026-07-15-native-preferences-design.md" },
        { label: "XRender chrome slice", href: "docs/superpowers/specs/2026-07-14-slice-2-xrender-chrome-design.md" }
      ],
      parts: [],
      deps: ["rt-config"]
    },
    {
      id: "chrome-colour", label: "Colour picker", layer: "chrome", status: "done",
      tags: ["native chrome"],
      desc: "A native colour picker for foreground/background/cursor and the 16-colour palette: an SV square plus a hue strip, pointer-driven, writing the chosen RGB straight into the settings. Scheme presets feed the scheme-aware per-pane chrome.",
      files: ["crates/rt/src/chrome/colour_picker.rs"],
      specs: [{ label: "Native colour picker design", href: "docs/superpowers/specs/2026-07-19-native-colour-picker-design.md" }],
      parts: [],
      deps: ["chrome-prefs"]
    },
    {
      id: "chrome-menu", label: "Menu & manual", layer: "chrome", status: "done",
      tags: ["native chrome"],
      desc: "The right-click context menu that drives rt actions, and the built-in F1 manual — both drawn natively. The menu shares its action table with the keymap so a binding and its menu item can never drift apart.",
      files: ["crates/rt/src/chrome/menu.rs", "crates/rt/src/chrome/manual.rs"],
      specs: [],
      parts: [],
      deps: ["rt-session"]
    },
    {
      id: "chrome-search", label: "Scrollback search", layer: "chrome", status: "done",
      tags: ["Phase 2"],
      desc: "Ctrl+Shift+F opens a find bar over the whole scrollback: case-insensitive substring match, every hit highlighted (current hit brighter), Enter/Shift+Enter to jump, results refreshing live as output streams in. Cell-accurate so highlights line up exactly with the grid.",
      files: ["crates/rt/src/chrome/search.rs"],
      specs: [],
      parts: [],
      deps: ["rt-session"]
    },
    {
      id: "instruments", label: "Border instruments", layer: "chrome", status: "done",
      tags: ["gauges"],
      desc: "Live gauges painted on each pane's edge: output flow, CPU heat (blackbody colour), and render latency. Composited into the border and idle-throttled, so they cost nothing when a pane is quiet.",
      files: ["crates/rt/src/chrome/instruments.rs"],
      specs: [{ label: "Instrument compositing design", href: "docs/superpowers/specs/2026-07-14-instrument-compositing-design.md" }],
      parts: [],
      deps: ["rt-app"]
    },
    {
      id: "patchbay", label: "Patch-bay", layer: "chrome", status: "done",
      tags: ["pipes"],
      desc: "Wire panes' stdin/stdout/stderr to each other through real named pipes exposed as $RT_IN / $RT_OUT / $RT_ERR. Drag from an edge jack to another pane; the animated wires drawn between panes are the actual bytes in flight.",
      files: ["crates/rt/src/main.rs"],
      specs: [],
      parts: [],
      deps: ["rt-session"]
    },
    {
      id: "columns", label: "Newspaper columns", layer: "chrome", status: "done",
      tags: ["layout"],
      desc: "Flow one pane's output into side-by-side columns (Ctrl+. / Ctrl+,) so a wide screen shows more rows at once, newspaper-style. A re-tiling display mode over a single pane's grid.",
      files: ["docs/COLUMNS.md", "crates/rt/src/main.rs"],
      specs: [{ label: "Columns notes", href: "docs/COLUMNS.md" }],
      parts: [],
      deps: ["rt-engine"]
    },
    {
      id: "tabs-adv", label: "Tabs & layouts UX", layer: "chrome", status: "planned",
      tags: ["Phase 3"],
      desc: "The remaining tab and layout polish from the roadmap. Drag-reorder and detach-to-new-window shipped with multi-window (see Multi-window & drag-and-drop); still open: per-tab close button, tab position (bottom/left/right), switch_to_tab_N, and a launcher for saved layouts.",
      files: ["docs/ROADMAP.md"],
      specs: [{ label: "Roadmap · Phase 3", href: "docs/ROADMAP.md" }],
      parts: [
        { label: "Drag-reorder + detach-to-new-window", status: "done", desc: "Shipped with multi-window/drag-and-drop; see that node." },
        { label: "Per-tab close button", status: "planned", desc: "Close a tab from its label without focusing it first." },
        { label: "Tab position (bottom/left/right)", status: "planned", desc: "Terminator lets the tab bar move off the top edge." },
        { label: "switch_to_tab_N", status: "planned", desc: "Direct jump to tab N by number." }
      ],
      deps: ["rt-session", "layouts", "multiwindow"]
    },
    {
      id: "plugins", label: "Plugins", layer: "chrome", status: "planned",
      tags: ["Phase 4"],
      desc: "A plugin mechanism (or built-in equivalents) for the useful Terminator plugins: logger, custom-commands menu, activity/silence watch, terminalshot, command-finish notify.",
      files: ["docs/ROADMAP.md"],
      specs: [{ label: "Roadmap · Phase 4", href: "docs/ROADMAP.md" }],
      parts: [],
      deps: ["rt-session"]
    },

    /* ---- Session & Model ---- */
    {
      id: "rt-session", label: "rt-session", layer: "control", status: "done",
      tags: ["controller"],
      desc: "The controller that ties the layout tree, the per-pane engine, focus, and broadcast into user actions. Owns the Backend trait (with a mock for headless tests), the broadcast fan-out predicate, per-pane bracketed paste, and the central content_rect that keeps grid, hit-testing, selection, and search in sync.",
      files: ["crates/rt-session"],
      specs: [],
      parts: [
        { label: "Focus & actions", status: "done", desc: "Click/keyboard focus, action dispatch shared with the menu." },
        { label: "Backend trait + mock", status: "done", desc: "Headless-testable seam already present before the engine split." }
      ],
      deps: ["rt-core", "rt-engine"]
    },
    {
      id: "rt-core", label: "rt-core", layer: "control", status: "done",
      tags: ["model"],
      desc: "The pure layout-tree and session model: pane ids, split tree, drag handles, tiling geometry — no I/O, fully headless-testable. The data structures every higher layer manipulates.",
      files: ["crates/rt-core"],
      specs: [],
      parts: [],
      deps: []
    },
    {
      id: "broadcast", label: "Broadcast & groups", layer: "control", status: "done",
      tags: ["Phase 3"],
      desc: "Type once, reach a pane group or every pane. Broadcast off/group/all with a window-border indicator and per-pane colour-coded corner markers; input fans out on the same predicate the paste path uses so it can't drift. Remaining polish (a per-pane group titlebar for naming / drag-assignment) is planned.",
      files: ["crates/rt-session/src/lib.rs"],
      specs: [],
      parts: [
        { label: "Broadcast off / group / all", status: "done", desc: "Group-scoped input fan-out with a live indicator." },
        { label: "Group titlebar (name / drag-assign)", status: "planned", desc: "Per-pane group header for naming and drag-to-group." }
      ],
      deps: ["rt-session"]
    },
    {
      id: "rt-config", label: "rt-config", layer: "control", status: "done",
      tags: ["settings"],
      desc: "Settings and keybindings with a Terminator-compatible syntax, persisted to ~/.config/rt via serde. Holds opacity/scrim, focus mode, scrollback budget, font, the colour palette, and the arrow-accel preferences, all normalised and clamped on load.",
      files: ["crates/rt-config"],
      specs: [],
      parts: [],
      deps: []
    },
    {
      id: "layouts", label: "Saved layouts", layer: "control", status: "planned",
      tags: ["Phase 3"],
      desc: "Serialize the split tree (serde) with save/load and a launcher, plus a -l CLI option. The existing RT_SPLIT / RT_COLUMNS / RT_TABS hooks become the format seed.",
      files: ["docs/ROADMAP.md"],
      specs: [{ label: "Roadmap · Phase 3", href: "docs/ROADMAP.md" }],
      parts: [],
      deps: ["rt-core"]
    },
    {
      id: "profiles", label: "Profiles", layer: "control", status: "planned",
      tags: ["Phase 4"],
      desc: "Multiple named profiles (colours / font / command / scrollback …), per-pane profile assignment, and profile switching.",
      files: ["docs/ROADMAP.md"],
      specs: [{ label: "Roadmap · Phase 4", href: "docs/ROADMAP.md" }],
      parts: [],
      deps: ["rt-config"]
    },
    {
      id: "ipc", label: "IPC / remotinator", layer: "control", status: "planned",
      tags: ["Phase 4"],
      desc: "A scripting surface (DBus net.tenshu-style, or a Unix socket): new_window/tab, hsplit/vsplit, get/set titles, switch_profile, reload_config — with matching CLI options.",
      files: ["docs/ROADMAP.md"],
      specs: [{ label: "Roadmap · Phase 4", href: "docs/ROADMAP.md" }],
      parts: [],
      deps: ["rt-session"]
    },
    {
      id: "rt-handoff", label: "rt-handoff (wire format)", layer: "control", status: "active",
      tags: ["zero-dep", "wire v1"],
      desc: "The frozen wire format for moving a pane or a whole tab from one rt process to another — including across different rt versions, so a live session survives an rt upgrade. A deliberately zero-dependency crate: a future rt vendors an old copy of itself to check its encoder against an old decoder, which only works while the crate is a self-contained pile of Rust with no build graph behind it. Phase 1 (this crate: frame/TLV/varint plumbing, style/grid/pane/tree encoding, the Hello/Offer/Claim/Adopted/Bye handshake messages, a seeded test-data generator, and a committed golden corpus pinning v1 byte-for-byte) is complete. Phase 2 (fd passing over SCM_RIGHTS, the engine-neutral snapshot bridge) and phase 3 (the four entry points: carry, bulk migrate, keyboard picker, X11 XDND) are not started.",
      files: ["crates/rt-handoff"],
      specs: [{ label: "Cross-instance pane & tab transfer design", href: "docs/superpowers/specs/2026-08-29-cross-instance-pane-transfer-design.md" }],
      parts: [
        { label: "Frame + TLV + varint core", status: "done", desc: "Frame header, tagged-field walker, LEB128 varints — little-endian and architecture-neutral (verified on x86-64 and riscv-64)." },
        { label: "Style / grid / pane / tree encoding", status: "done", desc: "Cell styles (indexed colour stays indexed), scrollback grid, whole-pane snapshot, and the split/tab layout tree." },
        { label: "Handshake messages", status: "done", desc: "Hello/Offer/Claim/Adopted/Bye, forward-compatible: an unknown TLV tag is skipped by its length and reported, never fatal. An unknown message TYPE is an error — the frame header's length is what lets the stream survive it, not the decoder." },
        { label: "Golden corpus (wire v1)", status: "done", desc: "32 committed fixtures spanning panes/trees/messages; every future build must decode and re-encode each one byte-for-byte. Demonstrated to fail when the format changes." },
        { label: "fd passing (SCM_RIGHTS) + snapshot bridge", status: "planned", desc: "Phase 2: move the PTY fd between processes; bridge to/from rt-engine's Snapshot." },
        { label: "Transfer entry points", status: "planned", desc: "Phase 3: carry across instances, bulk migrate, keyboard picker, X11 XDND." }
      ],
      deps: []
    },

    /* ---- Engine Seam ---- */
    {
      id: "rt-engine", label: "rt-engine (seam)", layer: "seam", status: "seam",
      tags: ["agnostic"],
      desc: "The engine seam: engine-agnostic types (Snapshot, SnapCell, CursorShape, Damage) and the pane interface that both rt and rt-mux consume, plus build-feature / RT_ENGINE selection between the in-house engine (default) and the vendored oracle. Names zero alacritty types across rt/rt-mux.",
      files: ["crates/rt-engine"],
      specs: [{ label: "Engine seam contract", href: "docs/engine-seam.md" }],
      parts: [
        { label: "Agnostic snapshot / damage types", status: "done", desc: "The interface the renderer draws from and the harness drives." },
        { label: "RT_ENGINE impl selection", status: "done", desc: "in-house default; RT_ENGINE=alacritty selects the vendored fallback." }
      ],
      deps: ["vt-term", "vendored-oracle"]
    },
    {
      id: "vendored-oracle", label: "Vendored oracle", layer: "seam", status: "seam",
      tags: ["oracle", "fallback"],
      desc: "The forked alacritty_terminal + vte kept in-tree. It plays two roles: the battle-tested differential-testing oracle every in-house behaviour is checked against, and a selectable fallback backend (RT_ENGINE=alacritty) — a permanent escape hatch.",
      files: ["vendor/alacritty_terminal", "vendor/vte", "docs/vendored-engine.md"],
      specs: [{ label: "Vendored engine provenance", href: "docs/vendored-engine.md" }],
      parts: [],
      deps: []
    },

    /* ---- In-house VT Engine ---- */
    {
      id: "vt-parser", label: "vt-parser", layer: "engine", status: "done",
      tags: ["parser"],
      desc: "A clean-room VT500 / Williams state machine (all 14 states + UTF-8) that emits an action stream. Keeps the memchr ground fast path and batched print_str runs, and beats the vte parser it replaces (~1.1–1.17×) — including synchronized updates (DECSET 2026), a gap the differential harness caught that fuzzing alone never would.",
      files: ["crates/vt-parser"],
      specs: [{ label: "vt-parser design", href: "docs/vt-parser-design.md" }],
      parts: [
        { label: "Williams state machine + UTF-8", status: "done", desc: "Byte-identical action stream to vte over 8000+ fuzzed cases." },
        { label: "Performance passes", status: "done", desc: "Alloc-free Params, byte-scan dispatch; verified on x86-64 and riscv-64." }
      ],
      deps: []
    },
    {
      id: "vt-term", label: "vt-term", layer: "engine", status: "done",
      tags: ["Term"],
      desc: "The in-house Term: grid, scrollback, pen/modes, native damage, wide-glyph handling, and reflow — consuming vt-parser's action stream. Matches the vendored oracle cell-for-cell: 0 divergences across 10 000+ generated scripts (every chunk framing) and 0 across 20 000 resize/reflow scripts, on x86-64 and riscv-64.",
      files: ["crates/vt-term"],
      specs: [
        { label: "vt-term design", href: "docs/vt-term-design.md" },
        { label: "vt-term damage design", href: "docs/vt-term-damage-design.md" },
        { label: "Divergence ledger", href: "docs/engine-divergence.md" }
      ],
      parts: [
        { label: "Grid, scroll regions, SGR, modes", status: "done", desc: "Printing/autowrap, DECSTBM, erase, DECSC/DECRC, alt-screen." },
        { label: "Wide characters", status: "done", desc: "WIDE_CHAR / spacer handling matched to the oracle." },
        { label: "Reflow on resize", status: "done", desc: "Grow/shrink both dims incl. wide glyphs + scrollback: 0/20000." },
        { label: "Native damage tracking", status: "done", desc: "Per-line dirty spans + scroll signals; no per-frame full diff." },
        { label: "OSC / DCS & query-report edges", status: "active", desc: "query/report shipped v0.3.15: OSC (title) and query-report (DSR/CPR, DA1/DA2, DECRQM) wired end-to-end — reply bytes proven byte-identical to the oracle by a dedicated 0/4000 differential, and the vtpane reader loop now forwards them to the real PTY so real apps (vim, tmux, vttest) under RT_ENGINE=vtterm actually get answered. Mode coverage widened (observable-state-edges slice, shipped v0.3.16): ANSI IRM (insert mode, 4) and LNM (mode 20, tracked for DECRQM only — a print-stream no-op, matching the oracle), DECSCUSR cursor shape+blink, and the mouse-mode reconciliation (tracking trio + encoding pair mutually exclusive on SET, urgency hints 1042 defaults on) are implemented and 0-divergence verified (widened vtterm_fuzz + vtterm_report, x86-64 AND riscv-64). OSC side-effects (clipboard/hyperlink), DCS, and colon-subparam SGR still unexercised." }
      ],
      deps: ["vt-parser"]
    },

    /* ---- Verification ---- */
    {
      id: "vt-conformance", label: "vt-conformance", layer: "verify", status: "done",
      tags: ["differential"],
      desc: "The dev-only conformance harness: a neutral ScreenState, a VtEngine trait implemented by both the oracle and vt-term, a structured escape-sequence fuzzer, a 32-case spec runner (the esctest/vttest role), and a replay corpus. Drives the full differential and the reflow differential to zero, on both architectures, via ci/verify.sh.",
      files: ["crates/vt-conformance", "ci/verify.sh"],
      specs: [
        { label: "Own-engine plan", href: "docs/own-engine-plan.md" },
        { label: "Divergence ledger", href: "docs/engine-divergence.md" }
      ],
      parts: [
        { label: "Chunk-invariance + spec suite", status: "done", desc: "Parser resumes across read boundaries; 32 spec cases pass." },
        { label: "Full + reflow differential", status: "done", desc: "0/10000 grid+cursor+modes+scrollback; 0/20000 resize." },
        { label: "Query/report differential", status: "done", desc: "Shipped v0.3.15: DSR/CPR, DA1/DA2, DECRQM interleaved with mode/cursor mutators; reply streams matched byte-for-byte (DA2 version masked), 0/4000 vs the oracle on x86-64 AND riscv-64. Caught+fixed two real engine divergences (DECSET 1007 default, DECOM homing)." },
        { label: "xtask verify / nightly soak", status: "planned", desc: "Phase 5: cargo xtask verify in CI + coverage-guided soak." }
      ],
      deps: ["vt-term", "vendored-oracle"]
    }
  ],

  roadmap: [
    { id: "p0", kind: "phase",  label: "Phase 0 · Input & essentials",        status: "done" },
    { id: "p1", kind: "phase",  label: "Phase 1 · Chrome, prefs & colours",    status: "done" },
    { id: "p2", kind: "phase",  label: "Phase 2 · Terminal UX parity",         status: "done" },
    { id: "p3", kind: "phase",  label: "Phase 3 · Tabs, layouts, grouping",    status: "active" },
    { id: "p4", kind: "phase",  label: "Phase 4 · Profiles, IPC, plugins",     status: "planned" },

    { id: "h0", kind: "harden", label: "Engine seam established",              status: "done" },
    { id: "h1", kind: "harden", label: "Conformance & oracle harness",         status: "done" },
    { id: "h2", kind: "harden", label: "In-house parser (beats vte)",          status: "done" },
    { id: "h3", kind: "harden", label: "In-house Term (0 divergences)",        status: "done" },
    { id: "h4", kind: "harden", label: "In-house engine wired as default",     status: "done" },
    { id: "h5", kind: "harden", label: "CI xtask verify / nightly soak",       status: "planned" }
  ]
};
