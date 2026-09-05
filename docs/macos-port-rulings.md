# macOS port — rulings made on your behalf

Ratification-after-the-fact. Everything below was decided by an agent without asking you,
and is now merged into `main` (`67633cd`).

**33 rulings. 13 touch Linux.** Two are wrong-or-broken today and one report describes code
that is not in the tree:

1. **`arboard` became unconditional, so `--no-default-features` now drags in `x11rb`** —
   README:107 still advertises that build as "zero X11 crates". It is no longer true, and
   the port's own CI gate cannot see it (§1.2).
2. **Font rasterisation is now multiplied by the window scale factor on Linux too** — bit-
   identical at 1.0, but a Linux HiDPI display (Wayland/X11 reporting 2.0) now rasterises
   glyphs at double the old size. Nobody asked you (§1.1).
3. **The macOS half of the CI gate can pass vacuously** (`2>/dev/null` + empty grep = "OK"),
   and no pipeline anywhere compiles `wgpu_backend.rs`, `wgpu_text.rs` or `vibrancy.rs` (§1.3, §1.4).

---

## 1. Decisions that affect Linux or both platforms

### 1.1 Glyph rasterisation is scaled by the window's HiDPI factor on every platform
- **Decided:** `font_size` is multiplied by `window.scale_factor()` before it reaches the
  rasteriser, at all six call sites — including the Linux GL renderer and the Linux XRender
  backend — while all layout stays in physical pixels.
- **Lives in:** `crates/rt/src/main.rs:7471` (`physical_font_px`); call sites
  `main.rs:989`, `:1188` (Linux GL `Renderer::new`), `:1268`, `:1386` (Linux
  `XRenderBackend::try_new`), `:1411` (macOS), `:5579` (`refresh_fonts`).
- **Rejected:** scaling only inside the macOS backend, or moving layout to logical pixels.
  The agent judged a single shared helper safer than a macOS-only fork of the metric path.
- **Reversal cost:** contained (one helper, six call sites).
- **Affects Linux?** **Yes.** Identical at `scale_factor == 1.0` (asserted by a test), but any
  Linux output reporting 2.0 now gets 2x glyphs where it previously got 1x. This is the one
  place the port changed what a Linux user sees.

### 1.2 `arboard` moved from an `x11`-feature optional dep to an unconditional dependency
- **Decided:** `arboard` backs the clipboard on X11 *and* macOS, so it left the `x11` feature
  list and became a plain dependency. `default-features = false` was kept (deviating from the
  brief's literal snippet) to avoid pulling the `image` crate tree.
- **Lives in:** `crates/rt/Cargo.toml:76` (dep), `:42` (`x11` feature no longer names
  `dep:arboard`); README claim at `README.md:107`.
- **Rejected:** keeping it optional and adding a separate macOS-only clipboard dep. Cargo
  hard-errors on `dep:arboard` in a feature list once the dep is non-optional, which forced
  the removal.
- **Reversal cost:** contained (feature/target gate on the dep plus a `cfg` in `clipboard.rs`).
- **Affects Linux?** **Yes, and it is currently wrong.** `cargo tree -p rt --no-default-features`
  now contains `arboard → x11rb → x11rb-protocol, xcursor, libloading, as-raw-xcb-connection,
  gethostname`. `README.md:107` still says `--no-default-features  # lean, Wayland-only (zero
  X11 crates)`. Either the README is corrected or `arboard` is gated again.

### 1.3 The Linux-graph guarantee is one sorted `cargo tree` diff, with stderr discarded
- **Decided:** "the port cannot regress Linux" is enforced by `diff`ing a `sort -u`'d
  `cargo tree` against a committed baseline, with repo paths normalised to `(WORKSPACE)`.
- **Lives in:** `ci/check-target-deps.sh` (macOS grep at :28, Linux tree at :37),
  `ci/linux-dep-baseline.txt`, `.woodpecker/gate.yaml` (`target-deps` step, blocking).
- **Rejected:** a lockfile/metadata comparison. `cargo tree`'s text was chosen for
  readability; the agent then had to drop an explicit `khronos-egl` entry the brief asked
  for purely because it moved a line in that text output.
- **Reversal cost:** trivial.
- **Affects Linux?** **Yes** — this *is* the Linux guarantee. Two holes: (a) both `cargo tree`
  calls pipe stderr to `/dev/null`, so a cargo failure yields an empty macOS result and prints
  `OK: no Wayland/X11/glutin crates on macOS` (same silent-green shape as the milkv
  `verify.sh` problem); (b) it only resolves the **default** feature set, which is why §1.2
  slipped through.

### 1.4 No pipeline compiles any macOS source file
- **Decided:** CI coverage for the port is dependency-graph resolution only; the Mac (`kiku`)
  is not a CI runner and was driven by hand over `rsync`+`ssh`.
- **Lives in:** `.woodpecker/gate.yaml` (`target-deps` step); no `macos`/`darwin` job exists.
- **Rejected:** adding `cargo check --target aarch64-apple-darwin` (needs an SDK) or a Mac
  runner.
- **Reversal cost:** structural (needs a Mac agent) for real coverage; contained for a
  cross-`check`.
- **Affects Linux?** Indirectly yes: `wgpu_backend.rs` (474 lines), `wgpu_text.rs` (808) and
  `vibrancy.rs` (142) can be broken by any Linux-side refactor and nothing will say so.

### 1.5 The `x11` Cargo feature stays default-on for every target; every `cfg(feature="x11")` gains `not(target_os="macos")`
- **Decided:** rather than making `default` target-conditional, each of the ~15 existing
  `#[cfg(feature = "x11")]` sites was rewritten to `#[cfg(all(feature = "x11",
  not(target_os = "macos")))]`.
- **Lives in:** `crates/rt/src/clipboard.rs:18,29,36,53,65,80,93,106,111`;
  `crates/rt/src/main.rs` module decls and the X11-visual-fold arm.
- **Rejected:** a target-conditional `default` in `Cargo.toml`, which would have kept the
  guards single-clause.
- **Reversal cost:** structural (a convention every future `x11` guard must follow).
- **Affects Linux?** No behaviour change, but it is now a standing rule for Linux code: a bare
  `feature = "x11"` guard is a macOS build break waiting to happen.

### 1.6 The vendored alacritty engine's `Config::kitty_keyboard` flipped from `false` to `true`
- **Decided:** rt now constructs the vendored engine with the keyboard protocol enabled; it
  had been inert (every `Handler` method early-returned), so an application querying rt on
  `RT_ENGINE=alacritty` heard nothing.
- **Lives in:** `crates/rt-engine/src/lib.rs:437`.
- **Rejected:** leaving the vendored engine a no-op (the brief expected zero changes there),
  which would have made the two engines disagree on something an application can observe.
- **Reversal cost:** trivial (one field).
- **Affects Linux?** **Yes** — this is the Linux comparison engine.

### 1.7 rt rewrites one reply on its way out of the vendored engine
- **Decided:** `CSI ? u` replies from the vendored engine are masked to flag 1, because that
  engine stores pushed flags faithfully and would otherwise tell an application rt honours
  flags it does not.
- **Lives in:** `crates/rt-engine/src/lib.rs:299` (`mask_kitty_keyboard_reply`), applied at
  `:337`; documented `docs/engine-divergence.md:250-290`.
- **Rejected:** staying honest to the oracle. "Never report a flag you do not honour" was
  judged the stronger constraint.
- **Reversal cost:** contained.
- **Affects Linux?** **Yes.** This is the only place rt edits an engine reply — a precedent as
  much as a change.

### 1.8 Only kitty enhancement flag 1 is honoured, and flags are masked on storage
- **Decided:** flags 2/4/8/16 are accepted, silently masked away, and read back as absent.
  The mode stack is bounded at 4096 and drops its **oldest** entry (fixing an upstream slip
  where alacritty pops `title_stack` instead).
- **Lives in:** `crates/vt-term/src/lib.rs:39` (`KITTY_KBD_SUPPORTED`), `:44`
  (`KITTY_KBD_STACK_MAX`), `:866`; `crates/rt/src/input.rs:244` (`kitty_disambiguates`).
- **Rejected:** implementing the full protocol, or refusing to answer at all.
- **Reversal cost:** structural (flags 2/4/8/16 each need real encoder work).
- **Affects Linux?** **Yes.**

### 1.9 Shift+arrow, Shift+F-key and numpad keys stay on legacy bytes; base-layout codes are not reported
- **Decided:** disambiguation follows alacritty's `should_build_sequence` literally. Real
  kitty sends `CSI 1;2A` for Shift+Up; rt does not, because rt's legacy arrow encoding
  carries no modifiers and changing it was ruled a separate change. Numpad is not
  disambiguated because rt's `encode_key` only receives `&Key`, never winit's `KeyLocation`.
  `Shift+1` reports `!`'s codepoint, not `1`'s (needs `key_without_modifiers()`).
- **Lives in:** `crates/rt/src/input.rs:228-266`, `:275-358`.
- **Rejected:** plumbing `KeyLocation` / `key_without_modifiers` through `main.rs`,
  `input.rs` and every `encode_key` call site.
- **Reversal cost:** structural (the plumbing crosses `main.rs`/`input.rs`/all call sites).
- **Affects Linux?** **Yes** — no regression (today's bytes are unchanged), but it is a
  permanent narrowing versus real kitty.

### 1.10 `Broadcast::Group` now spans every window in the process; `Broadcast::All` deliberately does not
- **Decided:** a pane torn into its own window keeps receiving group broadcasts. `Session`
  gained two pure `&self` methods; `App` owns the cross-window walk and skips the originating
  window. `All` was left per-window on purpose. Cross-*process* groups are explicitly out of scope.
- **Lives in:** `crates/rt-session/src/lib.rs:651` (`write_to_group`), `:665`
  (`paste_to_group`), `:1159` (`wrap_bracketed_paste`); `crates/rt/src/main.rs:2333` and the
  other five call sites, plus `App::group_broadcast_targets`.
- **Rejected:** giving `Session` knowledge of other sessions (would have destroyed its purity).
- **Reversal cost:** structural (two crates, six call sites, an `apply_action` signature change).
- **Affects Linux?** **Yes** — this is a plain rt behaviour change, found while testing macOS
  but not part of the port at all.

### 1.11 `apply_action` returns a tuple instead of taking an out-parameter
- **Decided:** `fn apply_action(..) -> (WindowCmd, Option<(u32, Vec<u8>)>)`, so a caller
  cannot silently forget the cross-window echo.
- **Lives in:** `crates/rt/src/main.rs` (`apply_action` and its two call sites — menu pick and
  keymap dispatch).
- **Rejected:** the `&mut Option<..>` out-parameter shipped in round 1; a forgotten read would
  have made cross-window paste vanish with no compile error.
- **Reversal cost:** contained.
- **Affects Linux?** **Yes** (shared code path), behaviourally inert.

### 1.12 A palette change now forces a full grid re-resolve and reports `Damage::Full`
- **Decided:** `set_palette` sets a new `palette_dirty` `AtomicBool`; the next
  `render_snapshot` consumes it, re-runs `resolve_full`, and reports `Damage::Full` so
  scissored consumers repaint too.
- **Lives in:** `crates/rt-engine/src/vtpane.rs:96,306,428-430,443`;
  test `crates/rt-engine/tests/palette_repaint.rs`.
- **Rejected:** doing the re-resolve inside `set_palette` (no access to the locked `Term`),
  or adding a second resolve path.
- **Reversal cost:** contained.
- **Affects Linux?** **Yes** — it fixes a Linux-visible bug (background colour change only
  repainted the window margin) and costs one full re-resolve per palette change.

### 1.13 Task 9's debug instrumentation was left in the shipped binary
- **Decided:** the chord-resolution, keymap-lookup and cursor-presence probes added to
  diagnose macOS input were never removed. They are `log::debug!`, platform-neutral, and the
  cursor probe is rate-limited to ~500ms behind a `log_enabled!` check.
- **Lives in:** `crates/rt/src/input.rs:90,94`; `crates/rt/src/main.rs:5679,5689,5985,7410`.
- **Rejected:** deleting them once the macOS input question was answered.
- **Reversal cost:** trivial.
- **Affects Linux?** **Yes** — free at default log levels, noisy under `RUST_LOG=debug`.

### 1.14 The EGL dev-dependency and the pixel-identity test are target-gated off macOS
- **Decided:** `[dev-dependencies]` moved under `cfg(not(target_os = "macos"))` and
  `damage_pixel_identity.rs` got a file-level `#![cfg(not(target_os = "macos"))]`, because
  Cargo links every dev-dependency into *any* test build in the package and `khronos-egl`'s
  build script needs `pkg-config` + a system EGL that macOS cannot provide.
- **Lives in:** `crates/rt/Cargo.toml:131-146`; `crates/rt/tests/damage_pixel_identity.rs:1`.
- **Rejected:** leaving it and accepting that `cargo test -p rt` is impossible on macOS.
- **Reversal cost:** trivial.
- **Affects Linux?** No behaviour change — the test still compiles and still reports `ignored`
  without a live GL context.

---

## 2. macOS-only behaviour you may disagree with

### 2.1 Courier New is the macOS font, not SF Mono or Menlo
- **Decided:** the regular/bold/italic/bold-italic chains all start at Courier New; SF Mono is
  a secondary regular and italic fallback only.
- **Lives in:** `crates/rt/src/main.rs:849-856` (regular), `:868` (bold), `:881-884` (italic),
  `:893` (bold-italic).
- **Rejected:** SF Mono as primary. `fontdue` reads TrueType only, so every `.ttc` collection
  (Menlo, Monaco, the real SF Mono family) is unusable, and pairing SF Mono regular with
  Courier New bold/italic would mismatch advance widths against a cell sized from the regular
  face. Consequence: rt on a Mac looks like a slab-serif typewriter, not a modern Mac terminal.
- **Reversal cost:** trivial (reorder the constants) — but a genuinely better fix means
  teaching the rasteriser `.ttc`.
- **Affects Linux?** No (the Linux lists are byte-identical to before).

### 2.2 The frosted glass is an `NSVisualEffectView` inserted as a sibling of the content view
- **Decided:** the effect view goes into the window's *frame view*, ordered `NSWindowBelow`
  relative to the content view. Not a subview of winit's view (wgpu's `CAMetalLayer` is a
  sublayer of that view's own layer, and the effect view would capture every `hitTest:`), and
  not a `contentView` swap (winit 0.31 `downcast().unwrap()`s the content view and would panic).
- **Lives in:** `crates/rt/src/vibrancy.rs:112-133`, call site `crates/rt/src/main.rs:1216`.
- **Rejected:** both popular vibrancy recipes, for the reasons above.
- **Reversal cost:** contained (one function).
- **Affects Linux?** No.

### 2.3 The glass is installed unconditionally, kept `Active`, and takes the system default material
- **Decided:** no `want_blur()` gate (at opacity 1.0 the view is simply invisible, so the
  opacity slider needs no re-apply path); `NSVisualEffectState::Active` rather than the
  default `FollowsWindowActiveState` so the glass does not dim on focus loss; no material set.
- **Lives in:** `crates/rt/src/vibrancy.rs:120-128`, rationale at `:48-63`.
- **Rejected:** mirroring the Linux blur paths, which *are* gated on `want_blur`. Also
  rejected: picking a material up front (`UnderWindowBackground` is named as the likely choice
  if the default reads flat).
- **Reversal cost:** trivial.
- **Affects Linux?** No.

### 2.4 winit's private `set_blur` is the fallback, taken only when the effect view fails
- **Decided:** fallback chain is NSVisualEffectView → `set_blur(true)`
  (`CGSSetWindowBackgroundBlurRadius(.., 80)`, private SkyLight API) → plain transparency.
  Exactly one of the first two runs, so what you see identifies which path ran.
- **Lives in:** `crates/rt/src/main.rs:1215-1219`.
- **Rejected:** the brief's unconditional `set_blur` call alongside the effect view.
- **Reversal cost:** trivial.
- **Affects Linux?** No.

### 2.5 `RT_BACKEND` is ignored on macOS, and the check runs before the override
- **Decided:** `choose_backend_on` returns `Wgpu` before it ever looks at the override, because
  a macOS build contains exactly one backend and `RT_BACKEND=xrender` would name a `cfg`-removed
  module. The platform is a *parameter*, not a `cfg!` in the body, so the rule is unit-testable
  on Linux.
- **Lives in:** `crates/rt/src/backend.rs:149-172` (the `if is_macos` early return at `:158`).
- **Rejected:** honouring the override and failing later; or a `cfg!` in the body, which would
  have made this the one selection rule CI can never check.
- **Reversal cost:** trivial.
- **Affects Linux?** No (the Linux arms are unchanged and still covered by four tests).

### 2.6 The macOS backend advertises no damage capability at all — every frame is a full redraw
- **Decided:** `partial_present_available() → false`, `buffer_age() → 0`,
  `x11_present_active() → false`, `present()` ignores its damage argument,
  `supports_scroll_blit()` stays false, and `is_gl()` returns **true** (it distinguishes
  "repaints inline" from XRender's persistent-surface split, not "is OpenGL").
  `begin/end_instrument_layer` are left as the trait's no-op defaults.
- **Lives in:** `crates/rt/src/wgpu_backend.rs:391,404-416`, module rationale at `:10-15`,
  instrument-layer comment at `:314-318`.
- **Rejected:** wiring damage rects into wgpu. Consequence: `main.rs` marks full damage every
  frame on macOS, so all of rt's damage-tracking work is inert there.
- **Reversal cost:** structural.
- **Affects Linux?** No.

### 2.7 Surface configuration: first available format, AutoVsync, PostMultiplied when offered
- **Decided:** `format: caps.formats[0]` taken as-is (no sRGB preference, explicitly left
  un-examined pending a human at a real screen), `PresentMode::AutoVsync`, and
  `CompositeAlphaMode::PostMultiplied` when the adapter offers it (this is what lets
  `background_opacity` reach the glass).
- **Lives in:** `crates/rt/src/wgpu_backend.rs:68,71,75-77`.
- **Rejected:** searching `caps.formats` for an sRGB format — the review explicitly deferred
  that judgement.
- **Reversal cost:** trivial.
- **Affects Linux?** No.

### 2.8 All wgpu geometry was taken from `render.rs`, overruling the brief in six places
- **Decided:** cursor/underline/strikeout constants follow the GL reference exactly —
  `cursor_hollow` `cell_h/16`, `cursor_underline` `cell_h/8`, `cursor_beam` `cell_w/8`,
  underline at `cell_top + ascent + 1.0`, strikeout at `cell_top + ascent*0.6`. `bell_stripe`
  is a yellow/black hazard-stripe *frame* (`T=5.0`, `SEG=12.0`, `0xf2c94c`/`0x141414`), not the
  brief's solid orange fill. This forced a new `TextPipeline::ascent()` accessor.
- **Lives in:** `crates/rt/src/wgpu_backend.rs:163` (`striped_edge`), `:248-302`;
  `crates/rt/src/wgpu_text.rs:430` (`ascent()`).
- **Rejected:** the brief's approximations (0.05/0.12/0.15/cell-bottom-relative), which would
  have made macOS visibly different from Linux.
- **Reversal cost:** contained.
- **Affects Linux?** No — **but** `striped_edge` is a hand copy of `render.rs`'s private helper
  and will silently drift if the GL one changes. There is an inline comment, not a test.

### 2.9 The glyph atlas deliberately mirrors `render.rs` rather than using a text library
- **Decided:** 2048² `R8Unorm` atlas with texel (0,0) forced opaque (so solid fills share the
  glyph pipeline), a shelf packer starting at (2,2) with 1px gutters, `FilterMode::Nearest`
  everywhere with `filterable: false`, `.round()`ed glyph placement, and a shape-mask cache
  keyed on quarter-pixel quantisation (`(kind, (r*4).round(), (width*4).round())`) rather than
  `f32::to_bits()`. `pack` returns `None` when the atlas is full and the glyph is refused
  rather than cached.
- **Lives in:** `crates/rt/src/wgpu_text.rs:28` (`ATLAS_SIZE`), `:205` (`mask_key`), `:295-297`
  (sampler), `:332`, `:447-455` (`pack`), `:530` (seed texel).
- **Rejected:** a linear sampler or an off-the-shelf text pipeline; both would have made macOS
  glyph edges differ from the GL build.
- **Reversal cost:** contained.
- **Affects Linux?** No.

### 2.10 macOS has no PRIMARY selection: `store_primary` is a silent no-op, `load_primary` returns `Err`
- **Decided:** middle-click paste does nothing on a Mac, quietly.
- **Lives in:** `crates/rt/src/clipboard.rs:70-71`, `:97-98`; tests at `:161-180`.
- **Rejected:** emulating PRIMARY with a second in-process buffer.
- **Reversal cost:** trivial.
- **Affects Linux?** No.

### 2.11 `Active::window` is an `Arc<dyn Window>` on macOS only
- **Decided:** the field is `cfg`-split. wgpu's `Surface<'static>` needs an owned `'static`
  window handle, which a `Box`/`&dyn Window` cannot give. The struct's "declared LAST so the
  window outlives its GPU resources" ordering comment no longer applies on macOS (refcounting
  does that job) but the field stayed last anyway.
- **Lives in:** `crates/rt/src/main.rs:498-513`, conversion at `:1404`.
- **Rejected:** making the field `Arc` on both platforms — that would have changed Linux.
- **Reversal cost:** contained.
- **Affects Linux?** No.

### 2.12 The keymap is unchanged on macOS — Copy is still `Ctrl+Shift+C`, not `Cmd+C`
- **Decided (by omission):** no report claims this decision, but nothing in `rt-config` or
  `input.rs` is `target_os`-aware. Cmd arrives as `Super`, and no default binding uses `Super`.
  A Mac user gets the Linux keymap.
- **Lives in:** `crates/rt-config/src/lib.rs:468` (`("<Shift><Control>c", Action::Copy)`);
  `crates/rt-config/src/keys.rs:22` (`Mods::SUPER`).
- **Rejected:** nothing — it was never considered.
- **Reversal cost:** contained (a `cfg`-selected default keymap table).
- **Affects Linux?** No, if done as a `cfg`-selected default.

---

## 3. Deferred or knowingly incomplete work

### 3.1 `ScaleFactorChanged` is logged but never applied
- **What:** the event has a real arm that logs and deliberately does nothing.
- **Lives in:** `crates/rt/src/main.rs:3130-3151`.
- **Rejected:** re-measuring the cell + resizing the backend + relaying out the session, which
  collides with the `surface_pending`/`RESIZE_SETTLE` deferred-resize machinery and could not
  be verified without real multi-monitor Retina hardware.
- **Breaks when:** you drag the rt window between a Retina and a 1x display. Text stays at the
  old scale — half or double size — until the next zoom/Preferences commit, which reads
  `window.scale_factor()` fresh and self-heals.
- **Reversal cost:** contained. **Affects Linux?** Yes on a mixed-DPI Wayland setup.

### 3.2 `--cols`/`--rows` pre-sizing guesses the scale factor from the primary monitor
- **What:** the window does not exist yet, so `event_loop.primary_monitor()` is used.
- **Lives in:** `crates/rt/src/main.rs:986,989`.
- **Breaks when:** the window opens on a secondary monitor with a different scale — the
  pre-sized grid is wrong until the first font reload.
- **Reversal cost:** contained. **Affects Linux?** Yes on mixed-DPI.

### 3.3 `WINDOW_MARGIN` / `PANE_PAD` / `TITLEBAR_PAD` stay unscaled physical pixels
- **What:** 8px/5px/4px chrome constants were explicitly left alone; only glyph size scales.
- **Lives in:** `crates/rt/src/main.rs:7318` and `rt-session`'s pane padding.
- **Rejected:** guessing an "intended logical size" for values never expressed as one, in
  shared non-macOS code the agent could not visually verify.
- **Breaks when:** at 2x or 3x the margins and pane padding look proportionally hairline.
  Cosmetic. **Reversal cost:** trivial. **Affects Linux?** Yes on HiDPI.

### 3.4 The colour-picker self-dismiss bug: instrumented, then the instrumentation was reverted, and the bug is unfixed
- **⚠ Report/tree mismatch.** `task-13-report.md` describes five `[clickdbg]` probe families in
  `main.rs`. **None of them are in the tree** — commit `78e6786` added them and `7f11ce4`
  reverted them. `grep clickdbg crates/` returns nothing.
- **What is still known and unfixed:** `close_picker` unconditionally flushes
  `active.prefs_pending` through `commit_settings` on *any* dismissal path, including a click
  outside the panel; and the picker's `PointerMoved` handler is not gated on button state, so a
  live drag writes a colour on every hover sample.
- **Breaks when:** you click away from the colour picker and an unintended colour commits.
- **Reversal cost:** n/a (nothing to reverse — this is work not done).
- **Affects Linux?** Yes, the bug is platform-neutral.

### 3.5 Nothing on macOS was ever visually verified
- **What:** every rendering task (4, 5, 5b, 5c, 6, 7, 8) ends with "a human must run it on the
  Mac". `ssh kiku` has no bound WindowServer, so winit's `resumed()` never fires and no window
  opens; all macOS evidence is unit tests plus `cargo build`.
- **Unverified specifically:** glyph baseline sign (upside-down/offset text), the initial
  `set_screen` seeding (text scattered off-window), bell hazard-stripe geometry, instrument
  disc/ring anti-aliasing, `caps.formats[0]` colour correctness, and whether the frosted glass
  sits behind the terminal rather than over it.
- **Reversal cost:** n/a. **Affects Linux?** No.

### 3.6 Retina coordinate-space risk in hit-testing was identified and not chased
- **What:** `cp::layout` builds panel rects from `window.surface_size()` (physical) and
  `backend.cell_size()`, while `active.mouse` comes from winit's event position. Task 13 named
  a logical/physical mismatch as a plausible cause of phantom `Hit::Sv`/`Hit::Hue`.
- **Breaks when:** clicking chrome (picker, prefs, menus) on a Retina display lands in the
  wrong control. Nobody has checked.
- **Reversal cost:** contained. **Affects Linux?** Possibly, on HiDPI.

### 3.7 Kitty keyboard handoff is export-only, and the inactive screen's stack is dropped
- **What:** `PaneWire` carries one stack; there is no `import_term` anywhere in `crates/`, so
  the receiving half (`Term::set_kitty_keyboard_stack`) is only exercised by tests.
- **Lives in:** `crates/rt-engine/src/handoff.rs:440`; `crates/vt-term/src/lib.rs:866`;
  `crates/rt-handoff/src/pane.rs:195`.
- **Breaks when:** a pane is torn out while a full-screen app holds the alt screen — it arrives
  with the app's flags and the shell underneath reverts to legacy keys. Called "the safe
  direction to lose". Notice: only once phase-2b wires an import path.
- **Reversal cost:** structural. **Affects Linux?** Yes.

### 3.8 `vt-conformance` does not cover keyboard-mode state
- **What:** `ScreenState` has no keyboard field and the differential generator emits no `CSI u`.
  Adding one would immediately fail on the §1.7 masking divergence, so nothing was added.
- **Lives in:** `docs/engine-divergence.md:283-289`.
- **Breaks when:** a future change to either engine's keyboard state machine diverges silently
  — the two engines' unit tests are the whole coverage.
- **Reversal cost:** structural. **Affects Linux?** Yes.

### 3.9 `rt-mux` has its own `encode_key` and got no kitty support
- **What:** deliberately untouched — the mux runs inside somebody else's terminal, which owns
  the keyboard protocol.
- **Lives in:** `crates/rt-mux/src/main.rs:1600`.
- **Breaks when:** Shift+Enter inside `rt-mux` is still plain Enter, forever.
- **Reversal cost:** contained. **Affects Linux?** Yes.

### 3.10 macOS is entirely undocumented for users
- **What:** `README.md`, `docs/ROADMAP.md` and `docs/KNOWN_ISSUES.md` contain no occurrence of
  "macOS". The only prose about the port lives in `docs/superpowers/specs/` and
  `docs/superpowers/plans/`. `project-map.js` was updated (`render-wgpu` node, `:52-55`).
- **Breaks when:** anyone tries to build or install rt on a Mac from the README; and the known
  gaps above (no Cmd bindings, no PRIMARY, Courier New, no live scale-factor handling) are
  written down nowhere a user will look.
- **Reversal cost:** trivial. **Affects Linux?** No.

### 3.11 Cargo.lock grew ~700 lines of macOS-only crates
- **What:** `wgpu` and its `ash`/`naga`/`bytemuck` tree plus `objc2*` are now resolved in the
  lockfile for every developer, on every platform.
- **Lives in:** `Cargo.lock`.
- **Rejected:** nothing — this is how Cargo works (the lockfile is a union across targets), and
  the Linux *build* graph is genuinely unchanged.
- **Breaks when:** `cargo update` on Linux now has to resolve crates Linux never compiles.
- **Reversal cost:** n/a. **Affects Linux?** Lockfile only, not the build.
