# Colour-repaint bug: diagnostic probes

Branch `fix/colour-repaint`. All four probes are `log::debug!`, tagged
`[colourdbg]`, zero cost unless `RUST_LOG` enables `debug` for the `rt` crate.
No behaviour changed — only logging was added (one line was refactored from
`cell.bg != cfg_bg` inline to a named `differs` variable used in the same two
places, so the diagnostics can count exactly what the draw call does).

## Where each probe lives

1. **`commit_settings`** (`crates/rt/src/main.rs`, ~line 6493-6499 and again
   at the `force_full = true` line): two log lines per commit —
   - `old_bg`/`new_bg`/`old_fg`/`new_fg` (the raw `[u8;3]` arrays) and the
     `colours_changed`/`fonts_changed`/`titlebar_changed` booleans, logged
     right after they're computed, before `active.settings = new` overwrites
     the old value.
   - a second line right after `active.force_full = true;` confirming the
     flag's actual value (trivially `true` today, but logged from the real
     field rather than assumed, in case future refactors gate it).
   - Not rate-limited: a settings commit is a discrete user action, not a
     per-frame event.

2. **`plan_frame`'s outcome**, logged at its call site in `redraw()`
   (`let plan = Self::plan_frame(...)`) via a new free function
   `log_plan_ratelimited(&plan, chrome_moved, active.force_full)`. Logs
   `plan` (`Full` or `Partial(bbox=.., hints=N)`), `chrome_moved`, and
   `force_full` — the exact inputs/output the ticket asked for.
   - **Rate limit:** a process-wide (not per-window) `AtomicU8` remembers the
     last plan *kind*; logs immediately on a Full<->Partial transition,
     otherwise at most once per 500ms (checked via a monotonic `Instant`
     epoch stored in `OnceLock` + `AtomicU64` millis). This runs every frame
     under `ControlFlow::Poll`, so unconditional logging would flood output.

3. **`draw_panes`**, the decisive probe. Inside the per-pane block, right
   after the per-cell draw loop: counts `colourdbg_filled` (cells where
   `cell.bg != cfg_bg`, the branch that calls `fill_cell`) vs
   `colourdbg_skipped` (cell.bg == cfg_bg, left translucent), and captures
   `colourdbg_first_row_bg` = the `bg` of the first 6 cells of the pane's
   first visible row (`r == 0`). Logged via `log_pane_bg_ratelimited(id,
   cfg_bg, first_row_bg, filled, skipped)`.
   - **Rate limit:** a `Mutex<HashMap<PaneId, last_log_ms>>` behind a
     `OnceLock`, keyed per pane so a busy pane doesn't starve a quiet one's
     log line; each pane logs at most once per 500ms. (A poisoned mutex —
     from an unrelated panic elsewhere while it was held — is recovered via
     `into_inner()` rather than propagating, since this is diagnostics-only
     and must never be the thing that brings the process down.)

4. **`redraw_full`**, right before `active.backend.begin_frame(bg)`: logs
   `bg` (the `Color(r,g,b,a)` tuple in 0..1 floats, alpha included) that is
   actually passed to the window clear.

## Capture command

```
RUST_LOG=rt=debug rt 2>&1 | grep '\[colourdbg\]'
```

(or `RUST_LOG=debug` if you want every crate's debug output; `rt=debug` is
enough and much quieter.) Then: open preferences, change the background
colour, watch the interiors stay old, and only THEN resize/maximise. Keep
the capture running across both steps so the "before resize" and "after
resize" log windows are both in the file.

## How to read it, and what each answer looks like

- **Stale snapshot** (`draw_panes`'s `cfg_bg` is already the NEW colour, but
  `first_row_bg` for the pane(s) still shows the OLD colour, and/or
  `filled`/`skipped` counts don't shift the way they should for cells that
  should now differ from the new `cfg_bg`): the engine-side snapshot itself
  still carries old cell background values — the palette apply
  (`session.set_all_palettes`) isn't reaching the snapshot the renderer
  reads, or reaches it too late relative to this frame.

- **Stale clear**: `redraw_full`'s logged `bg` still shows the OLD rgb
  (or alpha) even though the SAME frame's `commit_settings` log (or the
  immediately preceding one) shows `new_bg` as the new colour, AND/OR the
  `draw_panes` `cfg_bg` in that same frame doesn't match what
  `redraw_full` cleared with. Since both `bg` (redraw_full) and `cfg_bg`
  (draw_panes) are read from `active.settings.background` at frame time,
  these two logged values should always agree in the same frame; if the
  `commit_settings` log shows the write happened but a temporally-later
  `redraw_full`/`draw_panes` pair still logs the old value, `active.settings`
  itself is not the copy being read (e.g. a different `Active`/window, or a
  stale borrow) — grep the window id / thread interleaving around the commit.

- **Wrong plan**: the `plan_frame` log on the frames right after the
  `commit_settings` log shows `plan=Partial(...)` instead of `Full`, despite
  `force_full=true` in the same line — i.e. `force_full` was set but didn't
  actually route to `FramePlan::Full` (or got cleared before this frame ran).
  Since `chrome_moved` in `redraw()` already ORs in `active.force_full`, and
  `chrome_moved` unconditionally calls `active.damage.mark_full()`, this
  should be very hard to hit — if it does, that's the bug, distinct from the
  first two hypotheses entirely.

Cross-check across the three: a genuine repaint-on-resize-only bug should
show, right after the colour commit, either (a) `draw_panes` `filled`/
`skipped`/`first_row_bg` NOT reflecting the new colour while `cfg_bg` does
(stale snapshot), or (b) `redraw_full`'s `bg` not matching `draw_panes`'
`cfg_bg` in the same frame (stale clear / settings desync), or (c) a
`Partial` plan where `force_full=true` was logged (wrong plan) — and then,
on the frame after the resize, whichever of these flips to correct is the
mechanism the resize is accidentally re-triggering.
