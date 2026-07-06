# rt — a loose Rust port of Terminator

**Status legend:** ☐ todo · ◐ in progress · ☑ done · ⚠ blocked/hard

This file is the single source of truth for resuming work after a crash.
Read it first, then read `docs/PORTING_LOG.md` for the running narrative.
Update the status markers here whenever a milestone changes state, and commit.

---

## 1. Goal

`rt` is a *loose* Rust port of **Terminator** (the Python/GTK3 tiling terminal
emulator) that borrows **Alacritty's** architecture for speed. It is not a
line-by-line translation — it reimplements Terminator's *feature set* (infinite
tiling splits, tabs, layouts, grouped/broadcast input) on top of a fast,
Rust-native terminal engine.

### Design thesis
- **Terminator** gives us the *UX*: recursive split panes, tabs, saved layouts,
  input broadcast to groups, key-driven navigation.
- **Alacritty** gives us the *engine*: `alacritty_terminal` is a reusable crate
  providing the PTY, a damage-tracked grid, and a fast VTE/ANSI parser. Reusing
  it is the concrete meaning of "use ideas from alacritty to make it faster" —
  we get its performance work (damage tracking, batched parsing) for free
  instead of reimplementing GTK+VTE's slower widget model.

### Non-goals (for the first working version)
- Perfect config-file compatibility with Terminator's `~/.config/terminator/config`.
- Plugin API parity. Ligatures. Full GTK theming.

---

## 2. Architecture decision (ADR-0001)

```
+-------------------------------------------------------------+
|  rt  (winit window + wgpu/GL glyph renderer)                |
|                                                             |
|   +-----------------------------------------------------+   |
|   |  Layout tree  (the Terminator idea)                 |   |
|   |    Node = Split(H|V, [children], ratios)            |   |
|   |         | Tabs([children], active)                  |   |
|   |         | Leaf(Pane)                                |   |
|   +-----------------------------------------------------+   |
|                        |                                    |
|              each Leaf owns a Pane:                         |
|   +-----------------------------------------------------+   |
|   |  Pane = { Term (alacritty_terminal), PTY, title,    |   |
|   |           group, scrollback viewport }              |   |
|   +-----------------------------------------------------+   |
+-------------------------------------------------------------+
```

- **Engine layer:** depend on `alacritty_terminal` (grid, parser, PTY via its
  `tty` module) rather than rewriting it. Pin the version we vendored.
- **Layout layer:** our own recursive tree — this is the port of Terminator's
  `paned.py` / `notebook.py` / `container.py`, but as a pure data structure
  (no widget reparenting → avoids Terminator's whole class of use-after-free
  reparenting crashes; see `docs/TERMINATOR_BUGS.md`).
- **Render layer:** one GPU surface for the whole window; we blit each pane's
  visible grid into its rectangle. Modeled on alacritty's renderer but
  multi-viewport.
- **Input layer:** keybinding table (port of `keybindings.py`) + broadcast
  groups (port of Terminator's grouping) routing bytes to one or many PTYs.

Rationale for reusing `alacritty_terminal` instead of GTK VTE: it removes the
GTK main-loop reentrancy that is the suspected source of Terminator's random
crashes, and it is measurably faster at parsing throughput.

### ADR-0002 (session 1 user decisions)
- **Renderer:** winit + **OpenGL**, mirroring alacritty's GPU glyph-atlas
  renderer for maximum speed (chosen over a simpler CPU/softbuffer path). Cost:
  heavier native deps (GL/EGL) that make aarch64/rv64 cross-compiles harder —
  handled in M6 via per-arch containers.
- **Sequencing:** *features first*. Build the runnable Terminator-UX binary
  (splits, tabs, focus nav, broadcast, keybindings) before the packaging matrix.
- The feature/controller logic (`rt-session`) is written as pure, headless-
  testable code so it is verifiable in CI even though the GL window is not.

---

## 3. Workspace layout (target)

```
rt/
  Cargo.toml            # workspace
  crates/
    rt-core/            # layout tree, pane model, session state (no I/O)
    rt-engine/          # wrapper around alacritty_terminal (PTY + grid)
    rt-render/          # winit + gpu glyph rendering
    rt-config/          # config + keybindings (Terminator-compatible-ish)
    rt/                 # the binary: wires it all together
  packaging/
    deb/  rpm/  arch/   # per-format metadata
    build-matrix.sh     # 3 formats x 3 arches driver
  docs/
    PORTING_LOG.md      # running narrative (append-only)
    TERMINATOR_BUGS.md  # critical analysis of terminator + the crash bug
    ARCHITECTURE.md
    REFERENCES.md       # how to re-fetch reference/ sources
  reference/            # gitignored: terminator + alacritty clones
```

---

## 4. Milestones

### M0 — Bootstrap  ☑
- ☑ Create `~/git/rt`, confirm GitHub remote (`git-rt`).
- ☑ Clone terminator + alacritty into `reference/` (gitignored).
- ☑ Probe toolchain (rustc 1.95, cargo, docker, podman present; rpmbuild,
  cargo-deb, cargo-generate-rpm, cross, makepkg NOT present → install later).
- ☑ Write `.gitignore`, `PLAN.md`.
- ☑ Write `docs/` (PORTING_LOG, REFERENCES, TERMINATOR_BUGS). ARCHITECTURE
  folded into PLAN.md §2 for now.
- ☑ Commit M0 and push. (commit: bootstrap)

### M1 — Critical analysis of Terminator  ☑
- ☑ Bug-hunt subagent found + we verified THE random-crash bug: `cwd.py:15-20`
  unguarded `psutil` cwd probe on a dead pid.
- ☑ Findings written to `docs/TERMINATOR_BUGS.md` with rt's design mitigations.

### M2 — Cargo skeleton  ◐
- ☑ Workspace `Cargo.toml` (release profile mirrors alacritty).
- ☑ `rt-core`: layout tree (splits/tabs/close-collapse/rects/spatial nav) with
  9 headless integration tests, all green.
- ☐ Remaining crates (`rt-engine`, `rt-render`, `rt-config`, `rt`) added as
  their milestones land.
- ☑ Commit + push (this milestone).

### M3 — Engine wrapper  ☐
- ☐ `rt-engine`: spawn a PTY, run a shell, feed bytes to an
  `alacritty_terminal::Term`, expose a renderable grid snapshot.
- ☐ Headless integration test: run `echo hi`, assert grid contents.

### M4 — Renderer + window  ☐
- ☐ `rt-render`: winit window, a monospace atlas, draw one pane.
- ☐ Then multi-pane: draw the layout tree's leaves into their rects.
- ⚠ Requires a display; may only be verifiable on the user's machine.

### M5 — Terminator features  ◐  (controller logic done + tested; GL wiring next)
- ☑ Keybinding config parse (`rt-config`): Terminator accelerator syntax +
  default map + `Action` enum. 6 tests.
- ☑ Controller (`rt-session`): splits (Ctrl+Shift+O→TopBottom, E→LeftRight),
  new tab, close+refocus+collapse, spatial focus nav, broadcast Off/Group/All
  input fan-out, relayout/resize. Generic over a `Backend` trait so it is
  tested headless with a mock (8 tests). Real `rt_engine::TermPane` bridges in.
- ☐ Saved layouts (serialise the tree). Tab cycling (needs a tree tab API).
- ☐ Wire the controller into the GL binary + physical-key → `Chord` mapping.

### M6 — Packaging matrix  ☐  (3 formats × 3 arches = 9 artifacts)
Formats: **.deb**, **.rpm**, **arch** (`.pkg.tar.zst`).
Arches: **x86_64**, **aarch64**, **riscv64gc** (rv64).
- ☐ Install `cargo-deb`, `cargo-generate-rpm` (no root needed; they emit the
  archives directly without dpkg/rpmbuild).
- ☐ `cargo-deb` metadata in `crates/rt/Cargo.toml`.
- ☐ `cargo-generate-rpm` metadata.
- ☐ `packaging/arch/PKGBUILD`.
- ☐ Cross-compile targets: add rustup targets; use `cross` (docker) for
  aarch64/riscv64 because of native GUI deps.
- ⚠ **Known hard:** cross-compiling a GPU/winit GUI to riscv64 pulls in
  X11/Wayland/GL system libs. Fallback plan documented in
  `packaging/README.md`: build in per-arch containers via podman/qemu, or ship
  rv64 as source/AUR only if binary cross fails. Document whatever we achieve.

### M7 — Polish & docs  ☐
- ☐ README, man page, screenshots, CHANGELOG.
- ☐ Final commit + push + tag.

---

## 5. Coding standards (hard requirement from the user)
- **Every Rust function** gets a `///` or `//` block comment explaining what it
  does, its params, returns, and failure modes — in detail.
- **Every non-trivial line** gets an inline `//` comment explaining it.
- Trivial lines (`}`, obvious `let x = 5;`) may be left uncommented.
- This is verified by eye during review; keep it consistent from line 1.

## 6. How to resume after a crash
1. `cd ~/git/rt && git log --oneline -5` — see last committed state.
2. Read this `PLAN.md` status markers + tail `docs/PORTING_LOG.md`.
3. If `reference/` is missing, re-run the clones in `docs/REFERENCES.md`.
4. Continue at the first ◐/☐ milestone.
