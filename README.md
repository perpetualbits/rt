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
- **Drag-and-drop** — drag a pane by its titlebar or a tab by its label, with live drop cues (split fill, swap highlight, tab-insert caret, ghost chip); drop on another rt window to move it there or its centre to swap (X11, incl. `ssh -X`, for the single-gesture live drag). **Carry mode** (`Ctrl+Shift+M`/`Ctrl+Shift+N`, or hold Ctrl while releasing a drag outside the window) lets you pick a pane or tab up and drop it into any rt window — on Wayland and X11 alike — with the pointer wearing a held-pane card cursor until you click to drop or Escape to cancel.
- **Text dropped in from other apps** — select text in a browser or an editor, drag it onto a pane, let go: it is inserted there like a paste, into the pane **under the pointer** (which then takes focus, so your Return goes where the text went). The receiving pane lights up while you hover; the tab strip and open dialogs refuse the drop rather than typing into a pane you cannot see. Multi-line text is bracketed-pasted so a shell stages it without running it, and with bracketed paste off the line breaks become spaces — a dropped paragraph can never turn into a series of commands. winit surfaces file drops only (and on X11 only `text/uri-list`; on Wayland, none at all), so this is rt's own receiver below it: an `NSDraggingDestination` on macOS, an XDND `XdndProxy` window on X11, and on Wayland the `wl_data_device` rt's clipboard already owns — never a second one, because Mutter answers a client's second `get_data_device` by silently unlinking the first (see [wayland-text-drop.md](docs/wayland-text-drop.md)).
- **Newspaper columns** — flow one pane's output into side-by-side columns so a wide screen shows *more rows at once*, newspaper-style (`Ctrl+.` / `Ctrl+,`). See below.
- **Scrollback search** — `Ctrl+Shift+F`; configurable buffer up to 5M lines, held to a per-pane memory budget (oldest-first eviction) with a live memory meter.
- **Selection tools** — drag, `Ctrl`+drag for a rectangular block, double-click word / triple-click line (both rejoin soft-wraps); drag past a pane edge and the auto-scroll *accelerates*. And **anchored selection**: `Shift`+click to drop a start, navigate with arrows / `PageUp` / `Ctrl+End` / the scrollbar across any amount of scrollback, `Shift`+click or `Enter` to finish with the text on the clipboard — no button held. See below.
- **Clipboard history** — every copy rt makes lands in an in-memory ring of your last 20 clips; `Ctrl+Shift+H` or the `⎘ N` titlebar field lists them, pick one to paste it. Nothing touches disk.
- **Hold-to-accelerate arrows** — tap = one move; hold = progressively faster, so a crawl through a long line or a `less` page takes a second. Same curve drives selection and drag auto-scroll.
- **Broadcast** — type once, reach a pane group or every pane.
- **Border instruments** — live gauges on each pane's edge: output flow, CPU heat (blackbody), render latency. Idle-throttled, so they cost nothing when nothing's happening.
- **Patch-bay** — wire panes' stdin/stdout/stderr to each other via `$RT_OUT` / `$RT_ERR` / `$RT_IN` (real named pipes). The animated wires *are* the bytes.
- **Mouse** — full support, including forwarding to mouse-aware TUIs (vim, htop, …); hold **Shift** to override and use rt's own selection/scroll. Draggable scrollbar.
- **Touch & stylus** — a tap is a click, one finger drags and selects, two fingers scroll, and a pen works like a mouse (barrel buttons = right/middle). Wayland sends these to no client that has not asked for them, so this is real support, not emulation — see [docs/touch-and-stylus.md](docs/touch-and-stylus.md). The window *decoration* takes both too — drag it to move, an edge to resize — via a small patched winit ([why](docs/vendored-winit-wayland.md)).
- **Background blur** — compositor blur where available (Wayland `ext-background-effect-v1` / KDE; X11 `_KDE_NET_WM_BLUR_BEHIND_REGION`).
- **Scheme-aware chrome** — per-pane headers derived from your own foreground/background colours.

Press **F1** in rt for the full built-in manual.

### On macOS: the Command keys

**New to rt on a Mac? Start at [`docs/MACOS.md`](docs/MACOS.md)** — build,
install, the frosted glass, what differs from Linux, and the macOS known issues.

The Linux bindings above are all `Ctrl+Shift+…`, which no Mac user would guess. macOS
builds therefore add a **Command (⌘) set on top** — nothing is taken away, so every
`Ctrl+Shift` key you already know keeps working:

| Key | Does |
| --- | --- |
| `⌘C` / `⌘V` | copy the selection / paste the clipboard |
| `⌘T` | new tab |
| `⌘W` | close the focused **pane** (and the window with it, once that was the last pane) |
| `⇧⌘W` | close the window |
| `⌘Q` | quit rt — **owned by the macOS menu bar**, not by rt |
| `⌘N` | new window |
| `⌘D` / `⇧⌘D` | split side by side / stacked |
| `⇧⌘[` / `⇧⌘]`, or `⌥⌘←` / `⌥⌘→` | previous / next tab |
| `⌘,` | Preferences |
| `⌘F` | search this pane's scrollback |
| `⌘=` / `⌘-` / `⌘0` | bigger / smaller / default font |
| `⌃⌘F` | fullscreen (`F11` also works, with `Fn`) |
| `⇧⌘?` | the built-in manual (`F1` also works, with `Fn`) |

A `⌘` chord rt does **not** bind types nothing at all, exactly as in Terminal.app — it
never leaks a stray letter into the shell. `Ctrl` is untouched: `Ctrl+C` still interrupts.

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

### On a box without a GL driver (llvmpipe)

rt's GL path works on Mesa's software rasteriser and, since v0.3.20, costs about
0.1 core-seconds per keystroke frame there instead of ~2 (it no longer renders
into a multisampled buffer, and repaints only the damaged rects, not the pane).
On a weak board used interactively — a Milk-V Mars, say — the XRender backend
over Xwayland is still ~20× cheaper, at the price of the Wayland-only features:

```sh
DISPLAY=:0 rt --backend xrender     # Xwayland's display; unset WAYLAND_DISPLAY if set
```

To see what a frame costs, `RUST_LOG=rt::frame=debug` logs one line per frame
(plan, damage rects, vertices, ms, and what asked for it); `RT_FRAME_SYNC=1`
adds a `glFinish`-timed clear/draw/swap split with per-thread CPU. The whole
investigation, numbers and the headless harness are in
[docs/software-gl-lessons.md](docs/software-gl-lessons.md) and `bench/software-gl/`.

### On macOS

No apt line and no X11 — just Xcode Command Line Tools (`xcode-select --install`)
and a Rust toolchain, then `cargo install --path crates/rt`. macOS draws through
wgpu/Metal rather than OpenGL, so `--backend` and the `--no-default-features`
line above do not apply there. If you ever replace the installed binary by hand,
`rm -f` the old one first — a plain `cp` over it can make the next `rt` die with
`Killed: 9` before anything runs. **[`docs/MACOS.md`](docs/MACOS.md)** has the
whole story: the ⌘ keys, the frosted glass, what differs from Linux, and the
macOS known issues.

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

On **macOS** the equivalent is `extra/macos/install.sh`, which builds and installs
an `rt.app` bundle carrying the same `io.github.perpetualbits.rt` identity. See
[docs/MACOS.md](docs/MACOS.md#the-rtapp-bundle).

## Configuration

Settings live in `$XDG_CONFIG_HOME/rt/config.toml` (or `~/.config/rt/config.toml`).
Right-click → **Preferences** edits them live.

### Terminal type (`$TERM`)

rt exports `TERM=xterm-256color` (and `COLORTERM=truecolor`) into every pane. That
is a *borrowed* identity — rt is not xterm — but a safe one: the entry exists
wherever ncurses does, and rt implements a superset of what it claims.

You can change it, per install or per run:

```toml
# ~/.config/rt/config.toml
[settings]
term = "xterm-kitty"
```

```sh
RT_TERM=xterm-kitty rt      # one-off; the env var beats the config setting
```

Precedence is **`RT_TERM` → `term` in `config.toml` → `xterm-256color`**. Preferences
→ *Terminal type* cycles only the names your machine actually has terminfo for.
Changing it affects panes opened afterwards; a running shell keeps the `TERM` it was
forked with.

**Two ways this bites, both worth understanding before you touch it.**

1. **A `TERM` with no terminfo entry on the machine breaks ncurses applications
   outright** — `vim`, `less`, `top`, `htop`, `mc` exit with "unknown terminal type"
   rather than degrading. `TERM` also travels into every `ssh`, `sudo`, container and
   `tmux` you start from a pane, so a name your desktop has and a server does not
   breaks that server. Check with `infocmp <name>` on every host you use.
2. **Borrowing a name claims everything that terminal does.** `xterm-kitty` is the
   tempting one: applications that decide whether to use the kitty keyboard protocol
   from the `TERM` *name* — rather than by querying `CSI ? u`, which rt answers
   correctly — will negotiate it, and rt does implement it. But that same entry also
   advertises the kitty *graphics* protocol, which rt does **not** implement, so image
   viewers and plotting backends will emit graphics escapes rt silently swallows.
   `xterm-ghostty` has the same shape.

### rt's own terminfo entry

[`extra/rt.terminfo`](extra/rt.terminfo) describes what rt actually implements: it is
`xterm-256color` with everything rt does not do removed (no bell, no blink, no `rep`,
no settable tab stops, no OSC 4/52, no styled underlines, no modified-key sequences),
each removal annotated with the reason in the file. Install it with:

```sh
tic -x -o ~/.terminfo extra/rt.terminfo               # this user only
sudo tic -x -o /usr/share/terminfo extra/rt.terminfo  # machine-wide
infocmp rt                                            # verify
```

`-x` is required — several capabilities are user-defined extensions and `tic` drops
them without it.

**It is not the default, and rt will not install it for you.** Until it is compiled on
a machine, `TERM=rt` breaks every ncurses application there — which is exactly the
first trap above, and the reason this stays an explicit, per-machine step you take
before setting `term = "rt"`.

## Credits

- **Terminal engine: in-house** — `vt-parser` + `vt-term` (this repo, GPL-3.0-or-later),
  verified against and benchmarked on the vendored **`alacritty_terminal`** / **`vte`**
  forks (Apache-2.0 / MIT), which remain in-tree as the differential-testing oracle and a
  selectable backend.
- Windowing, GL & UI: **winit, glutin, glow, egui, fontdue**, and the wayland-rs / x11rb / arboard stacks — MIT / Apache-2.0.
- Inspired by **Terminator** (GPLv2); its *behaviour* is reimplemented here, no code is copied (see `docs/REFERENCES.md`).

## License

**GPL-3.0-or-later.** See [`COPYING`](COPYING) for the full text.
