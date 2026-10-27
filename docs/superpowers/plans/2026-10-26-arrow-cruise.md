# Arrow cruise — time-based hold ramp + keep-moving-after-release

> **For agentic workers:** execute task-by-task with TDD (superpowers:test-driven-development).
> Steps use checkbox (`- [ ]`) syntax for tracking.

**Decided 2026-10-26 (Roland, in session a44069a0):** "go with your recommendations". The
design below is exactly the recommendation table from that session; nothing here is open
except the tuning constants, which Roland tunes by feel after deploy.

**Goal:** Holding an arrow ramps speed linearly with *time held* (not repeat count). With the
new opt-in "Keep moving after release" preference, releasing the arrow freezes the reached
speed and rt keeps moving at it (cruise) until a stop event.

**Replaces:** the repeat-count curve of `2db372f` (THRESHOLD 10 repeats, +1 move / 2 repeats).

## Behaviour (the contract)

1. Tap shorter than the OS repeat delay → one move. (Unchanged; the OS does this.)
2. Held past the delay → OS repeats arrive at the OS rate (desktop setting, NOT tunable in rt
   and NOT a VT-spec thing — DECARM is only on/off). Moves per repeat =
   `1 + floor((now - first_repeat_time) / RAMP_INTERVAL)`, `RAMP_INTERVAL = 250ms`, capped at
   the existing "Max arrow speed" (`arrow_accel_max`). Time-based, so the feel no longer
   depends on the OS repeat rate.
3. Release (real `ElementState::Released` of that arrow), **only if** the new pref
   `arrow_cruise` is on **and** the hold reached the ramp (speed > 1): enter CRUISE — rt
   itself sends `speed` moves per tick at the last observed OS repeat interval (fallback
   33ms), from an event-loop timer.
4. Cruise stops on:
   - a press of the **same or any arrow** → stop only; that press is **swallowed** (no move);
   - **any other key** → stop, and the key is delivered normally;
   - focus loss (`WindowEvent::Focused(false)`), focus pane/tab change, any mouse press;
   - hard limit `CRUISE_MAX = 10s`.
5. Scope: keyboard arrows to the shell, and arrow moves of the anchored-selection END
   (`compose_nav`). **Not** drag auto-scroll (pointer-driven; keeps using the curve, no cruise).
6. `arrow_cruise` defaults **off**. Requires `arrow_accel` on (row disabled otherwise, like
   `ArrowAccelMax`).

## Files

- `crates/rt-config/src/lib.rs` — `arrow_cruise: bool` setting (default false), serde default.
- `crates/rt/src/main.rs` — new `ArrowHold` state replacing `arrow_hold: Option<(u8, Instant, u32)>`
  (line ~587); ramp fn; release handling; cruise tick in the about-to-wait path next to the
  existing `ControlFlow::WaitUntil` (line ~4178); stop hooks.
- New `crates/rt/src/arrow_cruise.rs` — pure state machine (no winit), unit-tested.
- `crates/rt/src/prefs_model.rs`, `prefs_native.rs` — `PrefRow::ArrowCruise` switch.
- `crates/rt/src/manual.rs` — KEYBOARD section text (line ~235).
- `project-map.js` — only if this ships as a node/part status change.

## Task 1 — pure state machine `arrow_cruise.rs`

- [x] Write failing tests first. API sketch:

```rust
pub enum Phase { Idle, Holding { dir: u8, since: Instant, last: Instant, interval: Duration },
                 Cruising { dir: u8, speed: usize, interval: Duration, next: Instant, until: Instant } }
pub struct ArrowCruise { phase: Phase }
impl ArrowCruise {
    /// Arrow press/repeat. Returns moves to send now (0 = swallowed: it braked a cruise).
    pub fn press(&mut self, dir: u8, now: Instant, max: u32) -> usize;
    /// Arrow release. Enters Cruising iff cruise_on && speed > 1.
    pub fn release(&mut self, dir: u8, now: Instant, max: u32, cruise_on: bool);
    /// Any stop event (other key, focus loss, pane switch, mouse press).
    pub fn stop(&mut self);
    /// Timer tick: Some((dir, moves)) when a cruise step is due; advances `next`.
    pub fn tick(&mut self, now: Instant) -> Option<(u8, usize)>;
    pub fn next_deadline(&self) -> Option<Instant>;
}
pub fn ramp(held: Duration, max: u32) -> usize; // 1 + held/250ms, capped; max<=1 → 1
```

  Tests: tap → 1, no cruise on release; ramp values at 0/249/250/1000ms and at cap; release
  below ramp → Idle; release at speed 4 with cruise_on → ticks yield (dir,4) at `interval`;
  cruise_on=false → Idle; arrow press while cruising → returns 0 and Idle; `stop()` → Idle;
  tick past `until` (10s) → None and Idle; direction change while holding restarts ramp;
  repeat interval learned from successive repeats (fallback 33ms).
- [x] Implement; `cargo test -p rt arrow_cruise`; commit. **Done 2026-10-27** — 13 tests,
      each guard checked by a deliberate break (mutation) of the code. As built, deviating from the sketch:
  - `release(dir, now, cruise_on)` — no `max` (the speed reached is frozen, not recomputed).
  - Ramp clock starts at the FIRST AUTO-REPEAT, not the key press, so the OS repeat delay
    doesn't count as ramp time.
  - `HOLD_GAP = 1s` (was 250ms in the old code): a backstop for a lost release only. 250ms is
    shorter than the OS repeat delay (500-600ms), so the first repeat looked like a fresh
    press and every hold restarted one repeat late — an old quirk the mutation check exposed.
  - Module is declared in `lib.rs` only so far; Task 2 adds `mod arrow_cruise;` to main.rs.

## Task 2 — wire keyboard arrows (shell path, main.rs ~7605)

**Done 2026-10-27, together with Task 3** — both used the one `arrow_hold` field, so
replacing it for the shell alone would have let a selection-mode cruise send arrows to
the shell. Also pulled forward from Task 4: the `arrow_cruise` config field (default
false); the Preferences row + manual text remain Task 4. As built:
  - `cruise_observe` (called at the top of `window_event`, ahead of the modal shims that
    swallow keys) handles arrow releases and the stop events; bare modifier presses
    (Shift/Ctrl/Alt/AltGr/Meta) do NOT stop a cruise — they type nothing.
  - Brake: an arrow press while cruising is swallowed before the keymap lookup.
  - `send_key` = the old typing tail of `on_key_press`, shared with `cruise_step`.
  - `cruise_step` ends the cruise when focus is on another pane than where it started
    (covers pane/tab switches by any route), compose mode changed, or IME preedit.
  - `ARROW_HOLD_GAP` removed; `arrow_accel_step` kept for drag auto-scroll only.

- [x] Replace `arrow_hold` + `ARROW_HOLD_GAP` logic with `active.cruise.press(...)`; a 0 return
      sends nothing. Non-arrow key → `cruise.stop()` then normal delivery.
- [x] Handle `KeyboardInput` Released for arrows → `cruise.release(...)`. Check that releases
      actually reach this handler (today only Pressed is matched at ~2630/2652; the modal
      branches must not eat them).
- [x] Cruise tick: in the about-to-wait path, `if let Some((dir, n)) = cruise.tick(now)` encode
      the arrow exactly as the press path does (app-cursor mode, kitty flags, Group broadcast)
      — factor the encode+send out of the press path rather than duplicating it. Fold
      `cruise.next_deadline()` into `min_interval`.
- [x] Stop hooks: `Focused(false)`, focus-pane change, tab switch, any mouse press.
- [x] Keep `arrow_accel_step` only for drag auto-scroll (`autoscroll_step`); its tests stay.
- [x] Tests green; commit.

## Task 3 — selection END (`compose_nav`, main.rs ~7342)

- [x] Use the same `ArrowCruise` (tag = `nav as u8`); cruise ticks call `compose_nav` with the
      stored nav while a selection is composing; selection end/cancel → `stop()`.
- [x] Commit.

## Task 4 — preference + manual

**Done 2026-10-27.** Row "Keep moving after release" sits under "Max arrow speed" in
Behaviour; it is greyed (and refuses steps) while acceleration is off, like the cap. The
manual's old line "the same curve drives ... drag auto-scroll" was corrected: drag
auto-scroll still uses the repeat-count curve (`arrow_accel_step`), only the selection END
shares the time ramp. The manual also says the repeat delay/rate are the desktop's.

- [x] `arrow_cruise` in rt-config (default false, serde default so old configs load) — pulled into Task 2.
- [x] `PrefRow::ArrowCruise` (Switch), disabled when `arrow_accel` is off; prefs tests.
- [x] Manual KEYBOARD paragraph: time-based ramp, cruise opt-in, how to stop (tap any arrow;
      any other key stops and still types), 10s limit. Run
      `manual::tests::every_default_keybinding_is_documented`.
- [x] Commit.

## Task 5 — verify + deploy

**Run 2026-10-27 on 3e4bd9c:** `cargo test --workspace` 1105 passed / 0 failed; clippy: no
findings in `arrow_cruise.rs`, and the only ones in touched files are the
`field_reassign_with_default` test idiom those files already use throughout; `ci/verify.sh
apollo` ALL GREEN (local x86_64 + apollo). milkv and kiku were unreachable, so there is no
riscv64 or macOS run yet. The verify battery covers the engine crates only, which this branch
doesn't touch. Deployed to dop561/apollo/dionysus/dogstar.

- [x] `cargo test --workspace`, `cargo clippy`; `ci/verify.sh` (see memory
      rt-verify-milkv-silent-green for milkv). milkv leg pending: host down.
- [ ] Deploy to the five machines per standing order (dop561/apollo/dionysus/milkv/kiku);
      skip unreachable ones and SAY which. Done except milkv + kiku (down).
- [x] Give Roland a numbered PASS/FAIL checklist (memory rt-user-test-checklists): tap = 1 move;
      hold ramps smoothly; cruise OFF by default; with cruise ON release keeps going, arrow tap
      stops without moving, letter key stops and types, focus-away stops, 10s cap; selection
      END cruises; drag auto-scroll unchanged.
- [x] Do NOT run rt on the live desktop to test (memory never-run-gui-on-live-desktop).

## Tuning constants (Roland tunes after feel test)

`RAMP_INTERVAL = 250ms`, `CRUISE_MAX = 10s`, fallback tick 33ms, cap = existing "Max arrow speed".
Known trade-off raised in session: at top speed overshoot ≈ reaction time × speed, and
stopping a cruise costs an extra tap; if landing precision feels bad, lower the default cap.
