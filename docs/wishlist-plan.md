# Wishlist plan — what to pick up, what not

Triage of `docs/wishlist`, read against rt's development history — especially the
long road to X11 support and what `ssh -X` taught us
(`docs/remote-rendering-lessons.md`). Each item is judged by one test: **does it
ship language, not pixels, and repaint only what changed?** That is the invariant
the whole X11 arc was fought to protect.

## The finding that reframes the list

rt runs **two chrome toolkits in parallel**, chosen per frame by
`Backend::supports_egui()`:

| Chrome surface        | Wayland / local-GL      | `ssh -X` / XRender        |
|-----------------------|-------------------------|---------------------------|
| Preferences           | native (`chrome::prefs`)| native — **unified** ✅    |
| Instruments, search   | native                  | native — **unified** ✅    |
| Context menu          | **egui** (`menu.rs`)    | native (`chrome::menu`)   |
| Manual                | **egui** (`manual.rs`)  | native (`chrome::manual`) |

egui was adopted for chrome (ADR-0004), then found too heavy over `ssh -X` — it
re-renders full frames and ships pixels, exactly the cost
`remote-rendering-lessons.md` §0–1 dissects. It has been retired surface by
surface ever since; the convergence is **~80 % done**. Prefs already renders as
commands on *both* backends. Only the **context menu** and the **manual** still
take the egui path on GL — and a native equivalent of each already ships on the
XRender path.

So wishlist #1's musing — "maybe two different toolkits needed for Wayland and
X11" — points the wrong way. The uniform-look problem *is* the two-toolkit split.
The fix is not a second toolkit; it is **finishing the convergence and deleting
egui**.

## Guardrails (non-negotiable, from the lessons doc)

Every item below must hold these, or it does not ship:

1. **`PutImage == 0`** stays true — chrome draws as XRender commands, never blits
   (§0). There is a test behind this invariant.
2. **Repaint only what changed.** No full-window composite per keystroke; a
   chrome element that repaints must scissor to its own rect and use a partial
   present (§1, §3).
3. **A visual change with no cell-damage needs one explicit full frame**, keyed
   on the central `force_full` rule — never a per-site flag (§4).
4. **Measure the server, not just rt.** Any item that makes the X server draw
   gets a gate that samples the server's CPU across a keystroke burst (§1).
5. **No continuously-animating chrome over the wire.** Interaction-driven
   repaint only.

## Items — pick up

### Slice A — polish batch (small, independent, ship anytime)

These four are self-contained, low-risk, and do not depend on the toolkit
decision. Each is a candidate for its own short spec, or bundled.

- **#3 rt version in the menu.** `env!("CARGO_PKG_VERSION")` in a disabled row
  (or a small "About"). Pure text — cheap on both backends. Add to the native
  menu; if the egui menu still exists when this lands, add to both, else it comes
  free once Slice B removes the egui menu.

- **#4 column separators more visible.** A colour tweak on the separator
  `fill_rect`. The wishlist suggests the RGB mean of fg and bg; note that on a
  light-on-dark scheme a pure mean lands on muddy mid-grey. Blend biased ~60 %
  toward fg and eyeball it against several schemes. One-line change, one visual
  check.

- **#5 jack ports — draw order and size.** The "little vertical lines" are the
  pane edge / divider painted *over* the jack, because jacks are not drawn last
  (`remote-rendering-lessons.md` §4 is the same draw-order family). Fix: draw
  jacks **after** edges/dividers, and bump the radius a notch. Bounded; verify
  the jack sits proud of the edge and no seam crosses it.

- **#6 scrollbar search hit-markers.** Chrome/Firefox-style ticks on the
  scrollbar track marking where scrollback hits fall, clustering when too dense
  to separate. The search already computes hit lines for highlighting; map each
  to a normalised track position and draw a short `fill_rect`. Painted **only
  while search is active**, and only when the hit set changes — no per-frame
  cost. Bounded feature, high user value.

### Slice B — finish the toolkit convergence, delete egui (the backbone)

Resolves #1 and clears the ground for #2. Splits in two once you look at what
each surface needs from the GL backend:

**B1 — re-home menu + manual + search to native on GL. ✅ done (`e5c7452`).**
The GL path drew these three overlays with egui; XRender already had native
equivalents. Unified on `chrome::menu` / `chrome::manual` / `chrome::search`
(rendering *and* input), which need only `fill_rect` + `draw_char` — both on
`GlBackend`, as the native preferences dialog already proved. Deleted
`paint_menu`/`paint_manual`/`paint_search` and the egui `menu::ui`/`manual::ui`.
This is the user-visible uniform-look win. One behaviour change: the GL search
bar is now the simpler native field (append + backspace) instead of a full egui
text field, matching what `ssh -X` users already had.

**B2 — delete egui entirely. ✅ done (`81ee7eb`).** The GL renderer gained AA
circle/ring/line primitives (coverage masks in the glyph atlas; `raster.rs` +
`render.rs`), so `chrome::instruments` draws on both backends and `egui` /
`egui-winit` / `egui_glow` are gone from the tree. `supports_egui()` → `is_gl()`.
ADR-0004 superseded. Original notes below.

**B2 (original note) — the shape it took:**
The last egui user is the **border instruments** on GL (`paint_instruments`).
The native `chrome::instruments` needs `fill_circle`/`stroke_circle`/
`stroke_line`, but the GL renderer is a coverage-atlas triangle-soup with **no
circle or arbitrary-angle-line primitive** — only rects and glyphs. So finishing
egui removal means writing an **AA vector layer in the GL renderer** (AA discs
for packets/jacks; AA thick lines for the bezier wires; the latency frame is
axis-aligned = rects). The natural fit is the same coverage-mask approach XRender
uses (A8 masks), reusing the existing atlas pipeline — no new shader.

The payoff of B2 is **not visual** (GL instruments already look identical to the
native path via the `chrome::col` no-drift design) — it is dependency hygiene:
drop `egui`/`egui-winit`/`egui_glow`, the egui value types (`Color32`, `Pos2`,
`Rect`), the `egui_ctx`/`state`/`painter` fields, and collapse `supports_egui()`.
B2 blocks nothing (Slice C does not need it), so it can come last.

Verify (both slices): the chrome tests still pass on both backends, and
`PutImage == 0` holds.

### Slice C — native colour picker (built once, on the converged chrome)

Depends on Slice B so it is built exactly once. Decision taken: **native on both
backends, not greyed out on X11.** The heaviness that scared the wishlist was
egui's per-frame repaint, not the picker concept.

- Draw a hue strip + saturation/value square as a few gradient-filled rects plus
  a position cursor, as XRender commands.
- Repaint **only while dragging** (pointer-driven), scissored to the picker's
  rect with a partial present — cheap even over `ssh -X`.
- Wire the chosen colour into the same config path the prefs rows already use
  (fg/bg/cursor + the 16-colour palette), so there is no parallel state.
- Gate it: a server-CPU sample across a drag burst must stay flat (guardrail 4),
  and `PutImage == 0` must hold through a full pick.

## Items — do NOT pick up

- **A second, heavier toolkit for X11**, or deepening egui. The history is
  unambiguous: that is the cost the whole arc removed.
- **Greying the colour picker out on X11** (the wishlist's fallback idea) — a
  command-drawn picker that repaints only on drag is cheap; a capability gap is
  not warranted.
- **Any chrome that animates continuously over the wire.** Interaction-driven
  repaint only.

## Sequencing

1. **Slice A** ✅ done — four quick wins: `9222db4` (version footer), `ee1c1db`
   (column rule), `9b4bcc1` (jacks), `d4aa7e3` (scrollbar hit-markers).
2. **Slice B1** ✅ done (`e5c7452`) — menu/manual/search native on both backends;
   the uniform-look win. **B2** (delete egui) pending — see above; blocks nothing.
3. **Slice C** ✅ done — native HSV colour picker (spec:
   `docs/superpowers/specs/2026-07-19-native-colour-picker-design.md`). Click any
   of the 18 swatches in the prefs Colours row to open a saturation/value square +
   hue strip; all `fill_rect`/`draw_char`, so it works on both backends with no
   dependency on B2. Edits flow through the existing `prefs_pending` + settle, so
   the terminal recolours once on pause. **B2** (delete egui) remains the only
   open slice.

Each slice gets its own design spec + implementation plan when it is picked up;
this document is the triage and the order, not the per-slice design.

## Status (2026-07-19)

Slice A shipped; Slice B1 shipped. Paused for live testing of B1 on the user's
machine (and over `ssh -X`) before taking on B2 or Slice C. Everything committed
builds in both feature configs (Wayland+X11 and Wayland-only) with the full test
suite green; the GL-path chrome changes (B1) and all rendering tweaks still need
a human eyeball, especially over `ssh -X`, since the sandbox can't capture a
compositing GL frame.
