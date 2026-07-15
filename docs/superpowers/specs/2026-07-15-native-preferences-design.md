# Native Preferences — Design

**Status:** approved design, pre-plan.

**Goal:** Give rt a preferences dialog that works on the REMOTE (XRender /
`ssh -X`) backend, where today there is none — and make it the ONLY preferences
dialog, on every backend.

## Background

Preferences is egui-only. On the XRender backend `main.rs` force-closes it:

```rust
if !active.backend.supports_egui() && active.prefs_open {
    active.prefs_open = false;   // else an invisible dialog swallows all input
```

So on the milkv over `ssh -X` there are no preferences at all — including
`inst_remote` and `inst_animate`, the two settings that turned out to matter most
there, which the egui dialog never exposed either (they are config-file-only).

The menu, manual and search overlays are already drawn natively
(`chrome/{menu,manual,search}.rs`, 64-133 lines each) through `Backend`
primitives (`fill_rect` / `draw_char`). Each splits a pure `layout(...) -> Geom`
(no backend, unit-testable) from `draw(backend, &geom, ...)` and a `hit()` for
the mouse. This follows that pattern exactly.

## Decisions (settled during brainstorming)

1. **Scope: everything except per-colour editing**, plus `inst_remote` and
   `inst_animate`. Colours are chosen by PRESET SCHEME; individual colours stay
   in `config.toml`. Per-colour editing is the one genuinely expensive widget
   (18 pickers) and the one thing users set once and rarely touch.
2. **Keyboard-first, discrete steppers, no drags.** A draggable slider is a full
   window repaint per pointer sample — the exact cost we removed from the divider
   and the wire this session. Every adjustment is one step = one frame.
3. **Native everywhere.** `preferences.rs` (egui) is DELETED; the native dialog
   serves both backends. GL loses the per-colour pickers, which is acceptable
   because decision 1 drops per-colour editing anyway. The alternative — two
   dialogs that must agree about every setting forever — is the drift that bit us
   twice today (the jack cursor disagreeing with the press path; the broadcast
   indicator needing `receives_broadcast` so it could not diverge from
   `feed_input`).
4. **Commit on settle (~150ms).** The dialog shows a new value instantly (text is
   free); apply+persist happens once the value stops changing. Stepping font size
   18 → 24 costs ONE glyph re-rasterisation + reflow (~700ms on a milkv) and ONE
   `config.toml` write, not six of each. Same rule as `RESIZE_SETTLE`, same
   reason: never pay for an intermediate value nobody keeps.

## The settings surface

Rows, in order. `Toggle` is `[x]`/`[ ]`; `Step` is `◄ value ►`.

**Font**
| Row | Kind | Range / step |
|---|---|---|
| Size (px) | Step | `font_size`, 8..=48, step 1 |
| Family | Step | `font_family`, cycles `mono_families` |

**Appearance**
| Row | Kind | Range / step |
|---|---|---|
| Background opacity | Step | `background_opacity`, `MIN_OPACITY`..=1.0, step 0.05 |
| Background blur | Toggle | `background_blur` |

**Colours**
| Row | Kind | Notes |
|---|---|---|
| Preset | Step | cycles `rt_config::SCHEMES`; stepping onto one sets `foreground`, `background`, `palette` |
| (swatches) | Swatches | fg, bg and the 16 palette colours as `fill_rect` swatches — read-only; edit in `config.toml` |

Stepping the preset OVERWRITES hand-edited colours (that is what a preset is),
and — per decision 4 — only on settle, so cycling past three schemes to reach the
fourth rewrites `config.toml` once, not four times. The swatch row updates
instantly with the pending preset, so what the swatches show is what stepping
away from this row will commit.

**Behaviour**
| Row | Kind | Range / step |
|---|---|---|
| Focus follows mouse | Toggle | `focus_follows_mouse` |
| Show per-pane titlebars | Toggle | `show_titlebar` |
| Scrollback (lines) | Step | `scrollback`, 1000..=`MAX_SCROLLBACK`, ×2 / ÷2 (logarithmic, as the egui slider was) |
| (memory estimate) | Display | the egui guardrail, kept verbatim: `≈ N MB per pane if full — X% of RAM, at C cols`, grey → amber past ¼ RAM → red past ½, plus the "each pane keeps its own buffer" note |

**Instruments**
| Row | Kind | Setting |
|---|---|---|
| Output activity | Toggle | `inst_output` |
| CPU heat | Toggle | `inst_heat` |
| Latency | Toggle | `inst_latency` |
| Patch-bay jacks | Toggle | `show_jacks` |
| Show over ssh -X | Toggle | `inst_remote` (NEW — never exposed before) |
| Animate at 6fps | Toggle | `inst_animate` (NEW) |

Then a `Close` action row. The scrollback memory estimate is the one piece of
real logic worth carrying over intact: it is the guardrail that stops a slider
from picking a buffer no machine can hold.

## Interaction

| Key | Action |
|---|---|
| `Up` / `Down` | move selection; skips section headers and display rows |
| `Left` / `Right` | step a value; on a Toggle, both toggle |
| `Space` / `Enter` | toggle, or activate (`Close`) |
| `Esc` | close |

Mouse stays discrete: a click selects a row; a click inside the `◄` / `►` zone
steps it; a click on a Toggle toggles it; a click on `Close` closes. No drags.

The panel scrolls when it does not fit (≈26 rows ≈ 650px, which a small window
will not hold): the selection is always kept visible, as `chrome/manual.rs`
already does with `manual_scroll`.

## Architecture

### `crates/rt/src/chrome/prefs.rs` (new)

Follows the sibling modules exactly:

- `pub enum RowKind { Section, Toggle, Step, Display, Swatches, Action }` — what a
  row is. `Swatches` is the colours preview; `draw` paints the supplied colours
  as `fill_rect`s across it instead of text.
- `pub struct Row { kind, label, value: String, pref: Option<PrefRow>, enabled: bool }`
  — one line, already rendered to strings. The module draws text and rects; it
  never reads `Settings`.
  - `pref` is how a selected row maps back to a setting: `Some` exactly for
    selectable rows, `None` for `Section`/`Display`/`Swatches`. This is the same
    trick `chrome/menu.rs` uses with its `clickable` vec, but carried in the row
    so the two cannot fall out of step.
  - `enabled` is drawn dimmed and refuses steps. It exists for one real case:
    **`inst_animate` is meaningless while `inst_remote` is off** (the 6fps tick is
    gated on both — `instruments_animating = anim && inst_remote && inst_animate`),
    so it greys out until `inst_remote` is on. That is exactly the trap the user
    hit: `inst_remote = true` alone did nothing visible because `inst_animate`
    defaulted false.
- `pub fn rows(settings: &Settings, families: &[String], mem_total: u64, cols: usize) -> Vec<Row>`
  — the single place that turns settings into display text, including the memory
  estimate. `cols` is the focused pane's width, as the egui path computed it.
- `pub fn layout(rows: &[Row], sel: usize, scroll: usize, cell_w, cell_h, win_w, win_h) -> Geom`
  — pure; returns the panel rect, per-row rects, the `◄`/`►` hit zones, and the
  scroll offset needed to keep `sel` visible. Unit-testable with no X server.
- `pub fn draw(be: &mut dyn Backend, g: &Geom, rows: &[Row], sel: usize, swatches: &[Color])`
  — `swatches` is `[fg, bg, palette…]`, painted into the `Swatches` row.
- `pub fn hit(g: &Geom, x, y) -> Option<Hit>` where `Hit = Row(usize) | Step(usize, i32) | Close`.

### `crates/rt/src/prefs_model.rs` (new, pure)

The step semantics, with no UI and no backend — so they are unit-testable:

- `pub fn step(settings: &mut Settings, row: PrefRow, dir: i32, families: &[String])`
  — clamps at both ends (font 8..48, opacity `MIN_OPACITY`..1.0, scrollback
  1000..MAX by ×2/÷2), cycles family and preset.
- `pub enum PrefRow { FontSize, FontFamily, Opacity, Blur, Preset, Ffm, Titlebar, Scrollback, InstOutput, InstHeat, InstLatency, Jacks, InstRemote, InstAnimate, Close }`

Keeping this out of `chrome/prefs.rs` means the geometry module stays about
geometry, and the clamping rules can be tested exhaustively without a `Geom`.

### `crates/rt/src/main.rs`

- `Active` gains: `prefs_sel: usize`, `prefs_scroll: usize`,
  `prefs_pending: Option<Settings>`, `last_prefs_edit: Instant`.
- The force-close hack (`!supports_egui() && prefs_open`) is DELETED — the dialog
  now exists on that backend.
- Input shim beside the manual's: while `prefs_open`, keys drive the dialog and
  never reach the PTY.
- A new `const PREFS_SETTLE: Duration = Duration::from_millis(150);` — long enough
  to swallow a run of `◄`/`►` presses (key repeat is ~30ms), short enough that a
  single toggle feels immediate.
- `about_to_wait` gains the settle, mirroring `RESIZE_SETTLE`:
  ```rust
  if let Some(p) = &active.prefs_pending {
      if now.duration_since(active.last_prefs_edit) >= PREFS_SETTLE {
          let new = active.prefs_pending.take().unwrap();
          Self::commit_settings(active, new); // diff + apply + persist, once
          dirty = true;
      }
  }
  ```
- `commit_settings` is the EXISTING diff-and-apply block from `paint_egui`
  (`colours_changed` / `fonts_changed` / `titlebar_changed` / `blur_changed` →
  commit → `persist()` → apply each), lifted out verbatim into a function. It is
  already backend-agnostic; deleting the egui dialog leaves it one caller.

### Deleted

`crates/rt/src/preferences.rs` and its `paint_egui` path.

## Data flow

```
key/click → prefs_model::step(&mut pending, row, dir)   [pending = clone of settings on first edit]
          → prefs_pending = Some(pending); last_prefs_edit = now
          → request_redraw  ────────────► dialog redraws showing the NEW value (text, free)
                                          settings themselves UNCHANGED
   …150ms of no further edits…
about_to_wait → commit_settings(active, pending)  → diff → apply (font/colours/
                titlebar/blur/scrollback) → persist() → force_full
```

The dialog always renders from `prefs_pending.as_ref().unwrap_or(&settings)`, so
what you see is what you picked, immediately — while the terminal behind it
changes once.

## Rendering & damage

An open overlay already forces full frames (`overlay_open → mark_full`), and
input is suspended, so frames happen only on a keypress: **one full frame per
key**, exactly what the menu and manual already cost and they are fine over
`ssh -X`. The instrument layer is suppressed while an overlay is up
(`show = inst_remote && !overlay_up`), which stays true.

No new damage machinery. The dialog is chrome; chrome carries no engine
cell-damage; `overlay_open` covers it.

## Correctness & performance gates

1. **Step semantics (pure unit tests, `prefs_model`).** Font size clamps at 8 and
   48; opacity clamps at `MIN_OPACITY` and 1.0 and lands on exact multiples of
   0.05; scrollback doubles/halves and clamps at 1000 / `MAX_SCROLLBACK`; family
   and preset cycle and wrap. No X server needed.
2. **Layout (pure unit tests, `chrome::prefs::layout`).** `Up`/`Down` skips
   Section and Display rows; a selection below the fold scrolls into view; the
   panel is clamped fully on-screen; `hit()` maps a point in a `◄` zone to
   `Step(row, -1)`.
3. **Settle (falsifiable, the point of decision 4).** N rapid font-size steps
   produce exactly ONE commit. Asserted from the log line
   (`prefs settled: … N edit(s) coalesced into 1 commit`), the same shape as the
   resize settle, and one `persist()` write — not N.
4. **It renders where it could not before (Xvfb + xtrace).** With
   `RT_BACKEND=xrender` and a test-only `RT_OPEN_PREFS=1` startup hook (mirroring
   `RT_OPEN_MANUAL=1`), the dialog's text appears on screen and `PutImage == 0`
   still holds. This is the whole feature made falsifiable: on this backend the
   dialog previously could not open at all.
5. **`inst_remote` is reachable from the dialog** — toggling it from prefs
   changes the setting, which is the concrete thing that was impossible before.

## Non-goals

- Per-colour editing (decision 1). `config.toml` keeps that; a native colour
  editor is a later slice if presets prove insufficient.
- Mouse-drag sliders (decision 2).
- Keeping the egui prefs (decision 3).
- Restyling the menu/manual/search, or unifying their egui/native duplicates —
  out of scope, though this design is what that unification would look like.

## Alternatives considered (rejected)

- **Keep egui prefs on GL, native on XRender.** Follows the codebase's existing
  duality (menu/manual/search each have both), and keeps colour pickers on the
  laptop. Rejected: two dialogs must agree about every setting forever, and the
  session already paid twice for exactly that kind of drift. The egui dialog's
  one advantage is the widget we are not building.
- **Draggable sliders.** Familiar, and a full window repaint per pointer sample
  on the one backend this feature exists for.
- **Commit instantly on every step.** Simplest, matches egui's current behaviour;
  stepping font size 18 → 24 costs six glyph re-rasterisations, six pane reflows
  (~700ms each on a milkv) and six `config.toml` writes.
- **Commit only on close.** Cheapest, but font size and opacity are exactly the
  settings you want to SEE before you accept them.
