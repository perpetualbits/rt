# rt

A fast, **Wayland-native** (and fully **X11-capable**) tiling terminal multiplexer,
built on its **own in-house VT engine** — a from-scratch ANSI parser and terminal grid,
verified cell-for-cell against [`alacritty_terminal`](https://crates.io/crates/alacritty_terminal)
and faster than it — with a custom OpenGL glyph renderer and an egui chrome layer. A loose
port of [Terminator](https://gnome-terminator.org/)'s ideas.

One binary runs on both display backends and prefers **native Wayland** when a Wayland
session is present (never XWayland), falling back to X11 otherwise.

![rt with four panes — a process monitor, a directory listing in a custom orange/purple scheme, an inline image, and a live reaction-diffusion demo — with lit border instruments and patch-bay wires running between panes](docs/screenshots/rt.png)

## The engine

rt reads and renders terminal output with **`vt-parser` + `vt-term`**, an in-house VT/ANSI
engine written from scratch instead of leaning on a black box:

- **Verified, not hoped.** Every step is checked by *differential testing* against
  `alacritty_terminal` as an oracle — feed both the same bytes, then compare the resulting
  grid, cursor, modes, wide-glyph placement, charsets, and scrollback **cell-for-cell**.
  The result is **0 divergences** across 10 000+ generated scripts (in *every* chunk
  framing, which stresses sequence- and UTF-8-resumption), a spec suite, and real-world
  captures. Where alacritty has a quirk, the engine matches the quirk — it is the
  reference, not the abstract spec.
- **Faster than what it replaces, on big *and* small hardware.** Six measured optimisation
  passes — occupied-length clearing, a packed 16-byte cell, batched printing,
  stack-allocated CSI params, an ASCII width fast-path, and a malloc-free recycling scroll —
  put the Term ahead of the vendored alacritty engine on throughput: **geomean ~1.2× on
  x86-64 and ~1.05× on riscv-64** (plain text ~1.9×), across representative workloads. Every
  pass was benchmarked on **both** a fast x86-64 machine and a humble **MilkV Mars** riscv-64
  board — the slow, in-order RISC-V core is a *co-equal* optimisation target, not an
  afterthought. Profiling *on the Mars itself* is what surfaced the biggest win: the
  per-line scroll allocation that a fast x86 allocator had hidden. Correctness stayed pinned
  at 0 divergences through every pass.
- **The parser, too.** `vt-parser` is a complete VT500/Williams state machine that beats
  the `vte` parser it replaces (~1.1×), including synchronized updates (DECSET 2026) — a
  gap the differential harness caught that fuzzing never would.

The vendored `alacritty_terminal` / `vte` forks stay in-tree as the differential-testing
**oracle** and a selectable backend. rt uses the in-house engine by default and **announces
its active engine on startup**; `RT_ENGINE=alacritty rt` switches to the vendored engine to
compare. See [`docs/vt-term-design.md`](docs/vt-term-design.md) and
[`docs/vt-parser-design.md`](docs/vt-parser-design.md) for the design, and
[`docs/engine-divergence.md`](docs/engine-divergence.md) for the (short) list of known
edges still being driven to zero.

## Features

- **Panes & tabs** — split any way, keyboard- or mouse-driven, Terminator keybindings.
- **Multi-window** — one rt process, any number of windows (`Ctrl+Shift+I`); tear a pane or tab out into a window of its own with a key (`Ctrl+Shift+D` / `Ctrl+Shift+J`) or by dragging it onto bare desktop.
- **Drag-and-drop** — drag a pane by its titlebar or a tab by its label, with live drop cues (split fill, swap highlight, tab-insert caret, ghost chip); drop on another rt window to move it there or its centre to swap (X11, incl. `ssh -X` — Wayland drags stay in-window, with a "Move Pane to..." menu for the rest).
- **Newspaper columns** — flow one pane's output into side-by-side columns so a wide screen shows *more rows at once*, newspaper-style (`Ctrl+.` / `Ctrl+,`). See below.
- **Scrollback search** — `Ctrl+Shift+F`; configurable buffer up to 5M lines, held to a per-pane memory budget (oldest-first eviction) with a live memory meter.
- **Selection tools** — drag, `Ctrl`+drag for a rectangular block, double-click word / triple-click line (both rejoin soft-wraps); drag past a pane edge and the auto-scroll *accelerates*. And **anchored selection**: `Shift`+click to drop a start, navigate with arrows / `PageUp` / `Ctrl+End` / the scrollbar across any amount of scrollback, `Shift`+click or `Enter` to finish with the text on the clipboard — no button held. See below.
- **Clipboard history** — every copy rt makes lands in an in-memory ring of your last 20 clips; `Ctrl+Shift+H` or the `⎘ N` titlebar field lists them, pick one to paste it. Nothing touches disk.
- **Hold-to-accelerate arrows** — tap = one move; hold = progressively faster, so a crawl through a long line or a `less` page takes a second. Same curve drives selection and drag auto-scroll.
- **Broadcast** — type once, reach a pane group or every pane.
- **Border instruments** — live gauges on each pane's edge: output flow, CPU heat (blackbody), render latency. Idle-throttled, so they cost nothing when nothing's happening.
- **Patch-bay** — wire panes' stdin/stdout/stderr to each other via `$RT_OUT` / `$RT_ERR` / `$RT_IN` (real named pipes). The animated wires *are* the bytes.
- **Mouse** — full support, including forwarding to mouse-aware TUIs (vim, htop, …); hold **Shift** to override and use rt's own selection/scroll. Draggable scrollbar.
- **Background blur** — compositor blur where available (Wayland `ext-background-effect-v1` / KDE; X11 `_KDE_NET_WM_BLUR_BEHIND_REGION`).
- **Scheme-aware chrome** — per-pane headers derived from your own foreground/background colours.

Press **F1** in rt for the full built-in manual.

### Newspaper columns — use that wide screen

Modern displays are wide, but a terminal only fills them with *columns*, not
*rows* — so `less`, a build log, or `git log` leaves the bottom two-thirds of a
27-inch screen blank. rt's **newspaper columns** flow a single pane's output into
two, three, or more side-by-side columns: text runs down the first column, then
continues at the top of the next, exactly like a newspaper. One `Ctrl+.` doubles
how much scrollback you see at a glance; `Ctrl+,` folds it back.

![A single pane split into newspaper columns, showing far more of a long listing at once](docs/screenshots/newspaper-columns.png)

It's transparent to the program underneath — it just sees an ordinary scrollback
scroll — so it works with anything: `man` pages, `cat` of a long file, logs, `vim`.

### Anchored selection — select a screenful (or a thousand) without holding a button

Selecting more than a screenful by dragging means holding the button while the
pane crawls past the edge. rt decouples the two ends: `Shift`+click drops the
*start* of a selection, the pane enters a selecting mode (the titlebar shows
`◉ selecting · 42 lines`), and you move the *end* with the keyboard — arrows
(accelerating when held), `Home`/`End`, `PageUp`/`PageDown`, `Ctrl+Home`/`Ctrl+End`
for the top/bottom of the whole buffer — or just scroll the view with the wheel or
scrollbar and `Shift`+click where it should end. `Enter` or that second `Shift`+click
finishes and puts the text on both CLIPBOARD and PRIMARY; `Esc` or a plain click
cancels. `Ctrl+Shift`+click starts a rectangular block instead. "Everything from
here to the end of the build log" is `Shift`+click, `Ctrl+End`, `Enter`.

## Build & install

Requires a stable Rust toolchain. On Debian/Ubuntu, the build needs a few dev libraries:

```sh
sudo apt install libwayland-dev libxkbcommon-dev libxcb1-dev libx11-dev libgl1-mesa-dev libfontconfig1-dev
```

Install the GUI terminal:

```sh
cargo install --path crates/rt                        # universal: Wayland + X11 (default)
cargo install --path crates/rt --no-default-features  # lean, Wayland-only (zero X11 crates)
```

The in-house engine is the default. To run the vendored alacritty engine instead — for a
one-off comparison or if you hit an edge — set `RT_ENGINE=alacritty` in the environment; rt
prints which engine it started with. (`RT_ENGINE=vtterm` forces the in-house engine.)

There's also a text-mode multiplexer that hosts the same panes inside any terminal:

```sh
cargo install --path crates/rt-mux
```

## Desktop integration

`cargo install` only places the binary. To get a launcher entry and icon (so rt
shows up in your application menu and the compositor binds its icon to the window):

```sh
extra/linux/install.sh              # user-local, ~/.local/share
extra/linux/install.sh --system     # system-wide, /usr/share (run with sudo)
extra/linux/install.sh --uninstall  # remove it again
```

This installs the [icon](extra/logo/rt.svg) into the hicolor theme and the
`io.github.perpetualbits.rt.desktop` entry, pinning `Exec` to your resolved `rt`
path. rt sets its Wayland `app_id` / X11 `WM_CLASS` to `io.github.perpetualbits.rt`,
which is what lets the compositor match the window to that icon.

## Configuration

Settings live in `$XDG_CONFIG_HOME/rt/config.toml` (or `~/.config/rt/config.toml`).
Right-click → **Preferences** edits them live.

## Credits

- **Terminal engine: in-house** — `vt-parser` + `vt-term` (this repo, GPL-3.0-or-later),
  verified against and benchmarked on the vendored **`alacritty_terminal`** / **`vte`**
  forks (Apache-2.0 / MIT), which remain in-tree as the differential-testing oracle and a
  selectable backend.
- Windowing, GL & UI: **winit, glutin, glow, egui, fontdue**, and the wayland-rs / x11rb / arboard stacks — MIT / Apache-2.0.
- Inspired by **Terminator** (GPLv2); its *behaviour* is reimplemented here, no code is copied (see `docs/REFERENCES.md`).

## License

**GPL-3.0-or-later.** See [`COPYING`](COPYING) for the full text.
