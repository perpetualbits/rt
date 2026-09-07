# rt on macOS

rt was written for Wayland and ported to macOS afterwards. The terminal itself —
the VT engine, the panes, the tabs, the scrollback, the patch-bay — is the same
code on both platforms. What changed is the layer underneath: on macOS rt draws
through **wgpu/Metal** instead of OpenGL, blurs through **AppKit** instead of a
Wayland compositor protocol, and adds a **Command-key** set on top of the Linux
keybindings.

This page is what a Mac user needs and the [README](../README.md) does not say.
Where something on this page contradicts the README, this page is right *for a
Mac*; the README describes the Linux build.

Everything below was checked against the source at `a6a2396` or measured on a
Mac (Apple silicon, macOS 26.6.2, `rustc` 1.98.1). Where a claim could not be
checked, it says so.

---

## What you need

Two things, and nothing else — no Homebrew, no `pkg-config`, no X11. The
`sudo apt install …` line in the README is Linux-only; the macOS build pulls in
no Wayland or X11 libraries at all (CI asserts this in `ci/check-target-deps.sh`).

1. **Xcode Command Line Tools** — for the linker and the system headers.

   ```sh
   xcode-select --install     # skip if `xcode-select -p` already prints a path
   ```

2. **A stable Rust toolchain**, from <https://rustup.rs>.

   ```sh
   curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
   ```

## Build and install

From the root of the rt source tree:

```sh
cargo install --path crates/rt
```

That puts `rt` in `~/.cargo/bin`, which rustup already adds to your `PATH`.
Check it landed:

```sh
rt -V          # e.g.  rt 0.3.19 (a6a2396)
```

A first build takes a while (rt vendors two terminal engines and builds wgpu);
an incremental rebuild afterwards is seconds. If you are building over `ssh`,
put `caffeinate -i` in front of the `cargo` command so the Mac does not sleep
mid-build.

The README's second install line — `cargo install --path crates/rt
--no-default-features` — is about keeping X11 crates out of a *Linux* build. It
has nothing to do with macOS; use the plain line.

### Replacing an installed binary: the `Killed: 9` trap

If you install with `cargo install`, you will not meet this. It bites when you
copy a freshly built binary over an old one by hand:

```sh
cp target/release/rt ~/.cargo/bin/rt     # ← don't
```

and then the next `rt` dies instantly with

```
zsh: killed     rt
```

(exit status 137). **Nothing ran.** There is no log, no window, no panic
message — it looks exactly like an rt crash and it is not one: macOS refused to
launch the file at all, because `cp` rewrote the bytes of a binary whose code
signature the kernel had already cached.

Measured on the Mac: replacing a binary in place with `cp` **while a copy of it
was still running** makes every later launch of that path `Killed: 9`.
Replacing one that was not running was fine — but you cannot rely on noticing
which case you are in, so use the form that is always safe:

```sh
rm -f ~/.cargo/bin/rt \
  && cp target/release/rt ~/.cargo/bin/rt \
  && codesign --force -s - ~/.cargo/bin/rt
```

Both halves were measured to work on their own: `rm -f` before the copy avoids
the problem, and `codesign --force -s -` repairs a binary that has already been
poisoned. Doing both is the habit worth keeping. (`cargo install` writes a
staged copy and *renames* it into place, which never trips this — also
measured.)

Quit any running rt before you replace its binary, and this never comes up.

## First run

Type `rt` in Terminal.app or iTerm2. rt opens a window of its own and prints
which VT engine it started with into the shell you launched it from; that shell
stays occupied until rt exits.

- **Settings live in `~/.config/rt/config.toml`** — *not* in
  `~/Library/Application Support`. rt uses the XDG path on every platform so one
  config file is portable between your Mac and a Linux box.
- **Preferences**: `⌘,` or right-click → Preferences. `↑`/`↓` move between rows,
  `←`/`→` step the selected value, `Esc` closes. Every change applies live and is
  written back to `config.toml` for you.
- **The manual**: `⇧⌘?` (or `Fn`+`F1`). It is the complete key reference and it
  has a macOS appendix.

There is no `.app` bundle and no installer. `extra/linux/install.sh` in the
README's "Desktop integration" section is Linux-only — it installs a `.desktop`
file and an icon into a freedesktop icon theme, neither of which macOS has.
On a Mac, `cargo install` places the binary and that is the whole installation.

---

## The Command (⌘) keys

rt's own keybindings are all `Ctrl+Shift+…`, inherited from
[Terminator](https://gnome-terminator.org/). Those **all still work on macOS**;
the Command set below is layered *on top* of them, not instead of them. Anything
rt can do that has no ⌘ key still has its `Ctrl+Shift+…` key — rotate a split,
cycle a pane group, wire the patch-bay, pick up a pane, resize by keyboard. Press
`⇧⌘?` for the full list.

| Key | Does |
| --- | --- |
| `⌘C` | copy the selection |
| `⌘V` | paste the clipboard |
| `⌘T` | new tab |
| `⌘W` | close the focused **pane** — and the window with it, once that was the last pane |
| `⇧⌘W` | close the whole window |
| `⌘Q` | quit rt, every window — owned by the macOS menu bar, not by rt (see below) |
| `⌘N` | new window |
| `⌘D` | split side by side |
| `⇧⌘D` | split stacked |
| `⇧⌘[` / `⇧⌘]` | previous / next tab |
| `⌥⌘←` / `⌥⌘→` | previous / next tab (the spelling that works on any keyboard layout) |
| `⌘,` | Preferences |
| `⌘F` | search this pane's scrollback |
| `⌘=` (or `⇧⌘+`) | bigger font |
| `⌘-` | smaller font |
| `⌘0` | default font size |
| `⌃⌘F` | fullscreen |
| `⇧⌘?` | the built-in manual |

Two ways to change tabs because `⇧⌘[` / `⇧⌘]` is the reflex from Safari and
Terminal.app but depends on a US-ish keyboard layout, while `⌥⌘←` / `⌥⌘→` comes
off named keys no layout remaps. rt's own `Ctrl+PageUp` / `Ctrl+PageDown` also
work, but need `Fn` on a Mac laptop keyboard.

**`Ctrl` is untouched.** `Ctrl+C` still interrupts, `Ctrl+D` still ends input,
`Ctrl+Z` still suspends. rt binds nothing that would get in the way.

**A ⌘ chord rt does not bind types nothing at all** — exactly as in
Terminal.app. `⌘K`, `⌘A`, `⌘S` and the rest are swallowed rather than leaking a
stray letter into your shell. (They used to leak, because AppKit reports `⌘K` as
the character `k`; that is fixed.)

### `⌘Q` is not rt's key

`⌘W` → `⇧⌘W` → `⌘Q` is the escalation ladder: this pane, this window,
everything. But only the first two are rt's.

`⌘Q` belongs to the **macOS application menu**, which winit's AppKit backend
installs for every window. Its Quit item owns the `⌘Q` key equivalent and calls
AppKit's `terminate:`; AppKit consumes the keystroke before it ever reaches rt's
window. So rt cannot bind `⌘Q`, cannot rebind it, and cannot run anything when
you press it. `⌘H` (Hide) and `⌥⌘H` (Hide Others) are the same menu's keys, for
the same reason.

That is the right outcome — `⌘Q` means "quit rt", which is what `terminate:`
does — with one consequence worth knowing: `terminate:` exits the process
without running rt's own cleanup, so a `⌘Q` leaves a patch-bay directory behind.
See [Known issues](#known-issues-on-macos).

`⌘1`…`⌘9` do nothing: rt has no "switch to tab N" action at all, on any
platform. Use `⌥⌘←` / `⌥⌘→`.

---

## The frosted glass

macOS is the one platform where rt gets real blur-behind for free. On Wayland a
client is forbidden from touching the pixels behind its own window, so blur is
the compositor's business and most compositors do not offer it. On macOS the
window server blurs behind a transparent window through `NSVisualEffectView`,
and rt asks it to.

Three settings control it, all in **Preferences → Appearance**:

| Setting | What it does |
| --- | --- |
| `background_opacity` | how see-through the window's background is, `0.05`–`1.0` (default `1.0`, fully opaque) |
| `background_blur` | whether rt asks for the frosted glass at all (default `true`) |
| `macos_glass_material` | which frosted look the glass has (default `under-window-background`) |

**There is no glass until the background is translucent.** Blur behind an opaque
window is invisible work, so rt installs the effect view only while
`background_blur` is on *and* `background_opacity` is below `1.0`. Turn either
one back and the glass goes away immediately; turn it on again and it returns.
Neither direction needs a restart.

Opacity also has keys — `⌃⌥↑` / `⌃⌥↓` nudge it ±5%, down to a floor of `0.05` —
and an environment variable for a one-off, `RT_OPACITY=0.8 rt`.

### `macos_glass_material`

AppKit's `NSVisualEffectView` has a `material` property, and its documented
default (`NSVisualEffectMaterialAppearanceBased`) has been deprecated since
10.14 and is far denser than anything Terminal.app shows — it reads as an
almost-opaque grey-blue haze with your own background colour faintly on top of
it. rt therefore picks a material explicitly.

The values, in the order Preferences steps through them, roughly lightest to
heaviest:

`under-window-background`, `under-page-background`, `content-background`,
`window-background`, `sidebar`, `header-view`, `titlebar`, `menu`, `popover`,
`sheet`, `full-screen-ui`, `hud-window`, `system-default`

- **`under-window-background` is the default.** AppKit documents
  `.underWindowBackground` as "the material used under window backgrounds",
  which is literally where rt puts its effect view, and it is the lightest of
  the behind-window materials — the one that leaves the desktop behind the
  window legible rather than merely present.
- **`system-default` means "never set a material"** — i.e. AppKit's deprecated
  default. It is kept only so the old look can be compared against the new one.
  It is not a recommended value.

Three ways to set it:

```
Preferences → Appearance → "Glass material"   ← Left/Right steps; the window changes live
```

```toml
# ~/.config/rt/config.toml — read at startup, so this one needs a restart
[settings]
macos_glass_material = "hud-window"
```

```sh
RT_GLASS_MATERIAL=hud-window rt      # one run; beats the config file
```

The Preferences row is dimmed while blur is off or the background is opaque —
there is no glass on screen to shape.

**One change does not apply live: switching *to* `system-default`.** A view that
already carries a material cannot be talked back into AppKit's implicit default,
so that one takes a restart. Every other material applies the moment you step
onto it.

An unrecognised name is reported on stderr and the default is used — a typo
here never costs you the rest of `config.toml`. The setting is macOS-only in
effect but cross-platform in type: a Linux rt parses it, keeps it, and writes it
back unchanged, so one config file stays portable.

The design rationale, and the Wayland/X11 side of the same story, is in
[`docs/APPEARANCE.md`](APPEARANCE.md).

---

## What is different from Linux

Everything the README describes works on macOS unless it is listed here.

### The renderer

Linux rt draws with **OpenGL** (or XRender over a forwarded X connection). macOS
rt draws with **wgpu**, which means **Metal**. The output geometry is copied from
the GL renderer deliberately — same cursor thicknesses, same underline and
strikeout offsets, same nearest-neighbour glyph atlas — so a pane should look the
same on both.

Two consequences:

- **`--backend` and `RT_BACKEND` do nothing on macOS.** A macOS build contains
  exactly one backend, so the choice is made before the override is read.
  `rt --help` still lists `--backend gl|xrender`; on a Mac it is inert.
- **Every frame is a full redraw.** rt's damage tracking — the machinery that
  repaints only the cells that changed — is not wired into the wgpu backend, so
  it is switched off on macOS. You are unlikely to notice on a modern Mac; it is
  why rt on macOS does more GPU work per frame than rt on Linux.

### Selection and the clipboard

X11 (and Wayland) have a second clipboard, PRIMARY: selecting text puts it there
automatically and middle-click pastes it. **macOS has no PRIMARY selection**, so
rt does not emulate one:

- **Selecting text does not copy it.** Drag-select, double-click a word, or
  triple-click a line, then press **`⌘C`**. On Linux the selection is already
  pasteable; on macOS it is not.
- **Middle-click paste does nothing.**

`⌘C` / `⌘V` and `Ctrl+Shift+C` / `Ctrl+Shift+V` work normally against the macOS
pasteboard, and rt's clipboard history (`Ctrl+Shift+H`, or the `⎘ N` field in a
pane titlebar) still records every selection you make — including the ones that
never reached the pasteboard.

### Fonts

Linux rt starts from DejaVu Sans Mono. macOS has no such font, so rt falls back
to a chain of system paths and lands on **Courier New**, with SF Mono and Andale
Mono behind it and Apple Braille / Symbol / ZapfDingbats appended for glyph
coverage. Courier New was chosen because it is a complete four-weight TrueType
family — regular, bold, italic and bold-italic all share one advance width,
which is what keeps bold text inside its grid cell. The honest consequence is
that rt on a Mac looks like a typewriter out of the box.

You can change it in **Preferences → Font → Family**, which offers the monospace
families your Mac actually has. On a stock macOS 26 install that is
`.SF NS Mono`, `Andale Mono`, `Courier New`, `GB18030 Bitmap`, `Menlo` and
`PT Mono`. Measured, with rt's own font loader:

- **`Courier New` and `Andale Mono` are plain `.ttf` files and behave.** Courier
  New has real bold and italic faces; Andale Mono has only one face, so bold and
  italic fall back to regular.
- **`Menlo` and `PT Mono` ship as `.ttc` collections, and rt only ever reads the
  first face out of a collection.** Menlo works, but its bold and italic render
  as regular. PT Mono is worse: its *first* face is PT Mono **Bold**, so
  choosing PT Mono renders everything in bold.
- **`GB18030 Bitmap` cannot be rasterised at all** — it is bitmap-only: seven
  megabytes of perfectly valid font that carries no outlines. The Family stepper
  therefore walks straight past it, so it cannot be picked. A `config.toml` that
  names it (or any family this Mac no longer has) still shows that name in the
  row — it is what the file says — with a line under it saying rt is drawing a
  fallback font instead. Earlier versions showed the new name while the screen
  kept the old font, with nothing to say so.
- **`.SF NS Mono`** is the hidden system family; its first face is the *Light*
  weight, and bold resolves to it too.

Note that rt's own default, `DejaVu Sans Mono`, is not installed on a stock Mac
either — it is a Linux ubiquity, not a portable one. Out of the box Preferences
therefore shows `DejaVu Sans Mono` in the Family row with *"not installed here —
rt is drawing a fallback font"* underneath, which is the honest description of
the Courier New you are looking at. Pick a family from the list to make the row
and the screen agree.

### Opening URLs

`Ctrl`+click on a URL opens it in your browser on Linux. On macOS **it does
nothing**: rt shells out to `xdg-open`, which macOS does not have. (macOS's
equivalent is `open`; rt does not call it.) Copy the URL and open it yourself.

### Border instruments

Each pane's border can show three live gauges. On macOS:

- **output flow** — works.
- **render latency** — works.
- **CPU heat** — **stays cold, always.** It is measured by reading `/proc`,
  which macOS does not have, so every pane reads as zero load. The instrument
  can be switched off in Preferences.

### Smaller things

- **Touch and stylus** support is Wayland-specific and does not apply.
- The **Preferences scrollback readout** says how much memory a full buffer
  would cost, but omits the "% of RAM" figure — that comes from
  `/proc/meminfo`.
- If you read [`docs/APPEARANCE.md`](APPEARANCE.md) for the background story,
  note that its **scrim** section is stale: the scrim (a client-drawn wash, with
  `scrim_strength` and `Ctrl+Alt+Left`/`Right`) is no longer in the code on any
  platform. `background_opacity` and `background_blur` are the whole story. The
  macOS half of that file is current.

---

## Terminal type (`TERM`), and Shift+Enter

rt exports **`TERM=xterm-256color`** (and `COLORTERM=truecolor`) into every
pane, on macOS as on Linux. That is a borrowed identity — rt is not xterm — but
a safe one: the entry exists everywhere ncurses does, and rt implements a
superset of what it claims.

### Shift+Enter, and why some applications ignore it

A plain terminal cannot tell `Enter` from `Shift+Enter`: both send one byte,
`\r`. The **kitty keyboard protocol** fixes that, and rt implements the part
that matters (progressive-enhancement flag 1, "disambiguate escape codes"). rt
answers the protocol's query correctly, and with the protocol negotiated
`Shift+Enter` sends `CSI 13;2u` — verified at rt's own prompt:

```sh
stty raw -echo; printf '\033[>1u'; head -c 7 | od -An -c
# press Shift+Enter →   033   [   1   3   ;   2   u
```

So rt does its half. The problem is the other half: some applications decide
whether to *use* the protocol by looking at the **`TERM` name** rather than by
asking the terminal (`CSI ? u`), which rt answers. Those applications will never
negotiate with a terminal calling itself `xterm-256color`, however capable it is.

The opt-in is to lend rt a different name — with two large caveats.

```toml
# ~/.config/rt/config.toml
[settings]
term = "xterm-kitty"
```

```sh
RT_TERM=xterm-kitty rt      # one run; the env var beats the config setting
```

Precedence is **`RT_TERM` → `term` in `config.toml` → `xterm-256color`**.
Preferences → *Terminal type* cycles only the names your machine actually has
terminfo for. A change affects panes opened afterwards; a running shell keeps
the `TERM` it was forked with.

**Caveat 1 — a `TERM` whose terminfo is not installed breaks ncurses
applications outright.** Not degrades: breaks. `vim`, `less`, `top`, `htop` and
`mc` exit with "unknown terminal type". This is not hypothetical on a Mac:
macOS ships terminfo for `xterm-256color`, and **not** for `xterm-kitty` or
`xterm-ghostty` (checked with `infocmp` on macOS 26.6.2). So on a stock Mac,
setting `term = "xterm-kitty"` by hand breaks every full-screen program in every
pane you open afterwards. `TERM` also travels into every `ssh`, `sudo`,
container and `tmux` you start from a pane, so a name your Mac has and a server
does not breaks that server. Check with `infocmp <name>` on each host.

**Caveat 2 — borrowing a name claims everything that terminal does.**
`xterm-kitty` is the tempting one, and it does make name-sniffing applications
negotiate the keyboard protocol, which rt genuinely implements. But that same
terminfo entry also advertises the kitty **graphics** protocol, which rt does
**not** implement — so image viewers and plotting backends will emit graphics
escapes that rt silently swallows. `xterm-ghostty` has the same shape.

### rt's own terminfo entry

[`extra/rt.terminfo`](../extra/rt.terminfo) describes what rt actually
implements: `xterm-256color` with everything rt does not do removed (no bell, no
blink, no `rep`, no settable tab stops, no OSC 4/52, no styled underlines, no
modified-key sequences), each removal annotated with its reason in the file.

macOS ships `tic` and `infocmp` at `/usr/bin` (ncurses 6.0), and the install
line works there unchanged — verified on macOS 26.6.2:

```sh
tic -x -o ~/.terminfo extra/rt.terminfo               # this user only
sudo tic -x -o /usr/share/terminfo extra/rt.terminfo  # machine-wide
infocmp rt                                            # verify
```

`-x` is required: several capabilities are user-defined extensions and `tic`
drops them without it.

**rt does not install it for you, and it is not the default.** Until it is
compiled on a machine, `TERM=rt` breaks every ncurses application there — which
is caveat 1 again, and the reason this stays an explicit step you take before
setting `term = "rt"`. Once `tic` has run, `rt` appears in the Preferences
Terminal-type list on its own.

---

## Known issues on macOS

Status: ☐ open · ◐ in progress · ☑ fixed. The same list, kept alongside rt's
other open issues, is in [`docs/KNOWN_ISSUES.md`](KNOWN_ISSUES.md).

- ☐ **`⌘Q` leaves a patch-bay directory behind, and starting a second rt
  deletes a running one's.** `⌘Q` is AppKit's menu item and calls `terminate:`,
  which exits without running rt's cleanup, so the session's `rt-<pid>`
  directory of named pipes stays in `$TMPDIR`. Worse, the sweep that is supposed
  to reclaim such leftovers at startup tests whether a pid is alive with
  `/proc/<pid>`, which never exists on macOS — so it treats *every* other rt
  session as dead and removes its directory. **If you run two rt processes at
  once, starting the second breaks the first's patch-bay wires.** Everything
  else in the first session is unaffected. Closing rt with `⇧⌘W` (or
  `Ctrl+Shift+Q`) on its last window exits cleanly and leaves nothing behind.

- ☐ **Moving the window between displays with different scale factors does not
  resize the text.** rt sees the scale-factor change and deliberately does
  nothing with it, so glyphs stay rasterised for the old display — half or double
  size on the new one. It heals itself at the next font reload: press `⌘=` then
  `⌘-`, or `⌘0`, or open Preferences and change anything.

- ☐ **Window chrome is not scaled for Retina.** Only the glyphs are scaled by
  the display's factor; the 8px window margin and the pane/titlebar padding are
  flat pixel constants. At 2x they look proportionally hairline. Cosmetic.

- ☐ **`Ctrl`+click on a URL does nothing** — rt runs `xdg-open`, which macOS
  does not have.

- ☐ **Selecting text does not put it on the clipboard, and middle-click paste
  does nothing** — both rely on the X11 PRIMARY selection. Use `⌘C` / `⌘V`.

- ☐ **The CPU-heat instrument never lights up** — it reads `/proc`.

- ☐ **A `.ttc` font family gives you one face for everything.** rt reads only
  the first face out of a font collection, so Menlo's bold and italic render as
  regular, and PT Mono renders entirely in its Bold face.

- ☐ **`--backend` / `RT_BACKEND` are accepted and ignored**, and `rt --help`
  still lists `--backend gl|xrender`. macOS builds contain one backend.

- ☐ **rt's damage tracking is inert on macOS** — every frame is a full redraw.

- ☑ **The Command key did nothing.** Every binding used to be `Ctrl+Shift+…`
  and `⌘` reached the keymap only to find nothing bound. Fixed: the Command
  table above.

- ☑ **An unbound `⌘` chord typed a stray letter.** AppKit reports `⌘K`'s
  character as `k`, so unbound chords leaked a literal letter into the shell.
  Fixed: on macOS an unbound Command chord is swallowed, as in Terminal.app.

- ☑ **The frosted glass ignored the blur preference and took AppKit's
  deprecated default material.** Fixed: see [the frosted glass](#the-frosted-glass).

### Not verified

The port's rendering was originally shipped without anyone seeing it: a Mac
reached over `ssh` has no window server bound to the session, so no rt window
can open there, and all the early macOS evidence was unit tests plus
`cargo build`. The frosted glass and the Command keys have since been corrected
from real reports on a real screen, so the basics are known good. Not confirmed
either way, on a Mac, at the time of writing: cross-window pane drag-and-drop,
the bell's hazard-stripe geometry, the instrument disc anti-aliasing, and
whether the surface's chosen colour format is right.
