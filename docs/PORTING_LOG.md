# Porting log

Append-only narrative of the port: decisions, problems, dead-ends. Newest at
the bottom. Timestamps are dates (session-relative).

---

## 2026-07-06 — Session 1: bootstrap

**Environment probe.**
- rustc 1.95.0, cargo 1.95.0 — fine (alacritty needs ≥1.85).
- Present: `docker`, `podman`, `dpkg-deb`.
- Missing: `rpmbuild`, `cargo-deb`, `cargo-generate-rpm`, `cross`, `makepkg`,
  `qemu-*-static`. Plan: install the cargo-based packagers (no root needed);
  use podman/`cross` containers for the aarch64/riscv64 matrix.
- Only `x86_64-unknown-linux-gnu` rustup target installed; will add
  `aarch64-unknown-linux-gnu` and `riscv64gc-unknown-linux-gnu` at M6.

**Reference sources cloned** into `reference/` (gitignored):
- terminator (Python/GTK3, ~15.8k LOC) — the feature model.
- alacritty (Rust workspace) — `alacritty_terminal` is the reusable engine.

**Key architectural realization.** Terminator is a *GTK widget-tree* app: it
implements splits by physically reparenting VTE widgets between `Gtk.Paned`
containers. That reparenting during split/close is exactly where a whole class
of intermittent GTK crashes lives (widget used after unparent/destroy). By
modeling the layout as a **pure recursive data structure** and rendering panes
ourselves onto one GPU surface (alacritty-style), we structurally cannot hit
that bug class. This is the core of why the port can be both faster *and* more
robust. See `TERMINATOR_BUGS.md` (bug-hunt in progress).

**Decisions locked.**
- ADR-0001: reuse `alacritty_terminal`; own layout tree; one GPU surface;
  broadcast via routing bytes to N PTYs. (Details in `PLAN.md` §2.)
- License: GPLv3-or-later (port of a GPL project; no verbatim code copied).

**Next:** finish docs, land cargo skeleton (`rt-core` layout tree with tests),
commit M0/M2, then engine wrapper.

## 2026-07-06 — Session 1: bug found + layout tree landed

**The random crash: found and confirmed.** Audited terminatorlib and verified
`cwd.py:15-20` firsthand: `psutil.Process(pid).as_dict()['cwd']` with no
error handling, called on every split/new-window via `terminal.py:get_cwd`.
On a pid that just exited (routine at split/close time) it raises
`NoSuchProcess`, escaping a GTK key handler → crash. Intermittent because it
depends on whether the child pid is alive at keypress time. Four more
reentrancy/use-after-destroy bugs ranked in `docs/TERMINATOR_BUGS.md`; all share
one shape (deferred/signal callbacks touching freed state) that rt's pure-data
layout eliminates by construction.

**rt-core landed.** The layout tree (`crates/rt-core`) is the Terminator port's
heart: recursive splits + tabs as plain data, panes are just integer ids, no
widgets. Implemented split (binary, `Gtk.Paned`-faithful), new_tab, close with
container collapse, weighted `rects()` with divider gutters, `all_panes()`, and
spatial directional `neighbor()` navigation. 9 integration tests, all green.
One bug caught by tests: the empty-tree sentinel leaked into `rects()`; fixed by
short-circuiting `rects()` on `is_empty()`.

**Next:** M3 engine wrapper around `alacritty_terminal` (spawn PTY, feed a
`Term`, expose a grid snapshot), with a headless `echo hi` test.

## 2026-07-06 — Session 1: engine wrapper (M3) landed

**rt-engine** wraps `alacritty_terminal 0.26.0` (the published version matching
the source we studied). One `TermPane` = PTY + `Term` + background `EventLoop`
I/O thread + a channel for input/resize/shutdown. Host-facing API is deliberately
tiny and panic-free: `spawn`, `write`, `resize`, `snapshot`, `drain_events`, and
a `Drop` that sends `Shutdown` and joins the thread (deterministic teardown =
no close-time races à la Terminator #3/#4). Events are distilled to a small
`PaneEvent` enum drained by the GUI, replacing scattered GTK signal handlers.

Compiled first try. One test failure taught us something: a child that exits
*instantly* (`printf x`) loses its final output with `drain_on_exit=false` — the
EOF hangup races the reader. Fixed by spawning the EventLoop with
`drain_on_exit=true`, so trailing output is fully parsed before teardown. Both
PTY tests (output-reaches-grid, input-round-trips) now green.

Workspace status: rt-core 9 tests + rt-engine 2 tests, all passing.

**Next:** M2b `rt-config` (keybindings, Terminator-style, pure/testable), then
the `rt` binary wiring core+engine, then M6 packaging scaffolding.

## 2026-07-06 — Session 1: keybindings + controller (M5 logic)

User decisions (ADR-0002): renderer = winit + **GL** (alacritty-style, max
speed); sequencing = **features first**.

**rt-config** ports Terminator's keybindings: a total, fallible parser for the
`<Shift><Control>o` accelerator grammar (named keys, F-keys, `plus`/`minus`
symbol names, case-insensitive modifiers), the default keymap transcribed from
`config.py:126-210`, and an `Action` enum decoupled from physical keys. User
bindings front-insert to override defaults. 6 tests.

**rt-session** is the controller — the real "Terminator features" layer, written
as pure control flow so it verifies headless. It owns the tree + a
`HashMap<PaneId, Backend>` + focus + broadcast mode, and turns `Action`s into
splits/tabs/close/focus-nav plus broadcast input fan-out (Off/Group/All). It is
generic over a `Backend` trait: production uses `rt_engine::TermPane`, tests use
a mock that records writes/sizes. This single-owner design (no deferred
callbacks, no reparenting) is the structural fix for Terminator's close-time
races. 8 tests: split-focuses-new, directional focus, broadcast-all,
broadcast-group subset, close-last→CloseWindow, close-one→refocus.

Split orientation mapping nailed down: Terminator "split_horiz" (Ctrl+Shift+O)
= horizontal divider = panes stacked = `Orientation::TopBottom`; "split_vert"
(Ctrl+Shift+E) = side by side = `Orientation::LeftRight`.

Workspace: 25 tests green (core 9, engine 2, config 6, session 8).

**Next:** the `rt` GL binary — winit `ApplicationHandler`, GL glyph-atlas
renderer over the tree's `rects()`, and physical winit key → `rt_config::Chord`
→ `Action` → `session.apply()` wiring.

## 2026-07-06 — Session 1: GL front-end runs and RENDERS (M4) + Wayland-native

Built the `rt` binary: winit `ApplicationHandler` + glutin (EGL) context + a
`glow` GL renderer with a `fontdue` glyph atlas. One shader does everything —
vertices carry pos+uv+rgba, the fragment shader turns atlas coverage into alpha,
and a forced-opaque seed texel at (0,0) lets *solid* quads (backgrounds, focus
border, dividers) reuse the same pipeline. Per frame we walk the tree's
`rects()`, blit each pane's `snapshot()` grid, and outline the focused pane.
Input path: winit key → `chord_from_winit` → keymap → `Action`/`Session::apply`,
else `encode_key` → `Session::feed_input`. `about_to_wait` drains pane events at
~60fps so async PTY output repaints without keystrokes.

**It compiled clean (zero warnings) and, crucially, RENDERS.** No display in the
dev sandbox natively, but there is a live compositor — captured the window via
Xwayland+xwd and confirmed the bash prompt draws correctly through the glyph
atlas with the blue focus border. First light: `docs/screenshots/first-light.png`.
For a GL renderer written without being able to see it, first-try correct text
rendering is a strong result — credit to mirroring alacritty's proven
winit/glutin versions and keeping the shader minimal.

**Wayland-native (ADR-0003).** User asked to drop all X11. Removed the `x11`
feature from winit and `glx`/`x11` from glutin AND glutin-winit (whose *default*
features silently re-added them). Result verified two ways: `cargo tree` shows
zero x11/xcb/glx crates, and the binary launches with `DISPLAY` unset using only
`WAYLAND_DISPLAY`. (Trade-off: I can no longer screenshot it in this sandbox
without `grim`; the render code is backend-agnostic so first-light still stands.)

Workspace: 30 tests green (core 9, engine 2, config 6, session 8, rt input 5).

**Next:** multi-pane/split visual check + cursor + grid colours; then M6
packaging (deb/rpm/arch × x86_64/aarch64/rv64).

## 2026-07-06 — Session 1: newspaper columns (rt-original feature)

User request: newspaper columns *within* a pane — text flowing bottom-of-col-1
→ top-of-col-2, scroll shifting the flow across column boundaries. They asked
whether to do it now or later; I judged the *data-model seams* worth placing now
(cheap now, an overhaul later) and built the feature on them.

Two seams landed first, each tested:
- **Engine**: `snapshot_lines(top, rows)` reads an arbitrary line range through
  scrollback (the plain `snapshot()` only saw the visible screen — that
  assumption everywhere would have been the overhaul). Plus `line_bounds()` and
  `is_alt_screen()`.
- **Session**: per-pane column count, `column_layout()` (count/col_cells/rows/
  gap) shared by relayout + renderer, and a column scroll offset. A column pane
  runs its PTY at ONE column's width so the shell wraps per column.

Config got `ColumnsMore`/`ColumnsFewer` (`Ctrl+.`/`Ctrl+,` — Ctrl+symbol, no
Shift, because winit remaps shifted symbols to different chars). The renderer
lays the `N×rows` lines out column-major, draws gap separators, handles wheel
scroll, and falls back to single-column on the alt screen (TUIs own the screen).

**Visually verified** via a throwaway X11 build (this compositor has no
wlr-screencopy for grim and no GNOME screenshot D-Bus, but does run Xwayland;
the render code is backend-agnostic so the shot is faithful). `RT_COLUMNS=3
RT_EXEC='seq 1 200'` produced exactly the spec: col1 …139, col2 140…170, col3
171…200+prompt. `docs/screenshots/newspaper-columns.png`, design in
`docs/COLUMNS.md`. Reverted Cargo.toml to Wayland-only (verified zero x11 crates).

Tooling note: `xdotool` can't drive the Wayland-native window (X11-only); used
env-driven startup layout (`RT_SPLIT`/`RT_COLUMNS`/`RT_EXEC`) for deterministic
verification instead — doubles as the seed of the saved-layouts feature.

Workspace: 33 tests green (core 9, engine 3, config 6, session 10, rt 5).

**Next:** multi-pane split screenshot; then M6 packaging, or grid colours/cursor.
