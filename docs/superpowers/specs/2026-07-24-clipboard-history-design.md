# Clipboard history — design

**Date:** 2026-07-24
**Status:** approved (design), pending implementation plan
**Scope:** feature **B** of the three-part selection vision (A = anchored
selection, shipped v0.3.12; C = drag auto-scroll acceleration, its own cycle).

## Problem

rt copies text to the OS CLIPBOARD/PRIMARY but keeps no history — once you copy
something new, the previous clip is gone. You often want an earlier clipping
back (a key, a path, a command) without re-selecting it. The idea, from the
original sketch: *"the title bar could have access to a clipboard and clicking
it gives access to the clipping."*

## Decisions (from brainstorming)

1. **Capture:** rt's own copies only — not the external OS clipboard.
2. **On select:** paste into the focused pane, and also set it as the clipboard.
3. **Trigger:** a titlebar affordance (focused pane) **and** a keybinding, into a
   native overlay list.
4. **Persistence:** in-memory only — never written to disk; gone on exit.

## Data model — an in-memory MRU ring

A single window-global `ClipHistory`:

- A bounded list of recent **unique** clippings, newest first.
- Capacity **20** (`CLIP_HISTORY_MAX = 20`).
- `record(text)`:
  - Skip if `text` is empty or whitespace-only.
  - If `text` already exists in the ring, move it to the front (most-recently-used)
    rather than duplicating.
  - Otherwise push it to the front; if over capacity, drop the oldest.
- `clear()` empties the ring.
- Read access: iterate newest→oldest; `get(i)` for the i-th entry.

Pure and unit-testable; kept in its own module, separate from the UI. Nothing is
serialized — it lives only while the process runs.

## Capture — every rt copy

A single `record` call at the points where rt already copies text:

- **Copy-on-select** — drag-select release, double-click word, triple-click line
  (currently `store_primary`, `main.rs` release handler + `copy_selection_to_primary`).
- **`do_copy`** — Ctrl+Shift+C (CLIPBOARD + PRIMARY, `main.rs:2608`).
- **Anchored-selection commit** — `compose_commit` (which calls `do_copy`).

So anything you deliberately grab in rt lands in history. Dedup (MRU) and the
ring cap keep bare drag-selects from flooding it; empty/whitespace clips are
skipped. Centralise so every copy path funnels through one `record`.

`record` is called at these copy *sites*, NOT inside `clipboard.store` /
`store_primary`. That keeps capture to genuine user copies: promoting a
clip selected from the history (which calls `store`/`store_primary`) must not
re-enter the capture path — it does its own move-to-front instead (see Select
action, step 3).

## Surface — titlebar affordance + keybinding

- **Affordance:** in the *focused* pane's titlebar, a small clip glyph + count
  (e.g. `⎘ 7`), shown only when the history is non-empty. Positioned among the
  right-anchored titlebar fields (near the size/meter), consistent with the
  existing per-pane titlebar layout.
- **Keybinding:** **Ctrl+Shift+H** opens the overlay too (a new keymap action).
- **Overlay:** a native list rendered like `chrome/menu.rs` — newest at top; each
  row a **one-line truncated preview** with newlines shown as `↵` (so multi-line
  clips read on one line) and a small char/line-count badge. A **Clear history**
  row sits at the bottom. `↑`/`↓` navigate, `Enter`/click selects, `Esc` closes.
  Anchored under the titlebar affordance; opened by keybinding when titlebars are
  off, it anchors to the focused pane's top edge.

## Select action — paste + promote

Selecting a clip:

1. Pastes it into the focused pane via the normal per-pane bracketed-paste path
   (`feed_paste`).
2. Sets it as the current CLIPBOARD **and** PRIMARY (`store` + `store_primary`),
   so a follow-up Ctrl+Shift+V / middle-click repeats it.
3. Moves it to the front of the ring (you just used it).
4. Closes the overlay.

## Privacy

- In-memory only; nothing written to disk; the ring is dropped on exit.
- Previews are truncated one-liners; the full clip text is only ever used at
  paste/promote time, never shown expanded.
- A **Clear history** action empties the ring immediately — reachable from the
  overlay's bottom row and from the context menu.

## What is reused vs new

**Reused:**
- `clipboard.rs` (`store` / `store_primary`) for promoting a selected clip.
- `feed_paste` (per-pane bracketed paste) for the paste.
- The native-chrome overlay pattern (`chrome/menu.rs`: rows model + draw + hit).
- The per-pane titlebar draw path (`main.rs`) for the affordance.
- The keymap/chord + `action_for` dispatch for Ctrl+Shift+H.

**New:**
- The pure `ClipHistory` ring (own module, TDD'd).
- The overlay UI (`chrome/clip_history.rs`): rows, geometry, hit-testing, draw.
- The titlebar clip glyph + count.
- The Ctrl+Shift+H keymap action + a `ClearClipHistory` action.
- One `record` funnel wired into the three copy sites.

## Testing

- **Ring (pure, TDD):** record skips empty/whitespace; dedup moves an existing
  entry to front (no duplicate); capacity evicts the oldest; `clear` empties;
  newest-first iteration order; `get(i)` bounds.
- **Preview formatting (pure):** newline → `↵`, truncation to a max width, the
  char/line badge.
- **Overlay model (pure, like menu tests):** rows built from the ring + a Clear
  row; selection wraps; `Esc`/`Enter` semantics; hit-testing maps clicks to rows.
- **Integration (manual, dop651):** copy several things → the count rises and
  dedups; open via glyph and via Ctrl+Shift+H; pick a clip → it pastes into the
  focused pane and Ctrl+Shift+V repeats it; multi-line clip shows `↵` in the
  preview but pastes with real newlines; Clear empties it; nothing persists
  across a restart.

## Out of scope (own cycles / deferred)

- External OS-clipboard capture (polling other apps' copies) — deliberately not
  captured; rt's own copies only.
- On-disk persistence across restarts — deferred on privacy grounds; could be an
  opt-in preference later.
- **C** — drag auto-scroll acceleration (separate spec).
