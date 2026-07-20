# The engine seam

This is the boundary that lets rt run on *either* the vendored `alacritty_terminal`
engine or our future in-house `vt-parser`/`vt-term`, without the front-end knowing or
caring which. It is the enabling structure for the whole own-engine program
(`docs/own-engine-plan.md`, Phase 0).

## The happy finding (Phase 0 audit, 2026-07-21)

The decoupling was **already substantially achieved** by prior design:

- **rt and rt-mux name zero alacritty types.** `rg alacritty crates/rt/src
  crates/rt-mux/src` matches only doc comments. Neither crate depends on
  `alacritty_terminal` in its `Cargo.toml`.
- **`rt-engine`'s public API is engine-agnostic.** It exposes its OWN vocabulary —
  `Snapshot`, `SnapCell`, `CellAttrs`, `CursorShape`, `CursorPos`, `Damage`,
  `CellDamage`, `LineBounds`, `SearchMatch`, `PaneEvent` — never alacritty types.
- **rt-session already has a `Backend` trait** and is generic (`Session<B: Backend,
  F>`); a mock backend drives its tests. The abstraction seam exists.

So rt is coupled to the concrete engine in exactly ONE place: the type alias
`AppSession = Session<TermPane, …>` in `crates/rt/src/main.rs`, and the two
`TermPane::spawn_env` call sites (rt and rt-mux). Everything else already talks to
the seam.

## The contract: what an engine must provide

`rt-engine::TermPane` is today's only implementation. Its public surface IS the engine
contract — the future in-house engine must provide the same (and the differential
oracle harness will drive exactly this surface):

**Agnostic value types** (returned/consumed; all defined by rt-engine, engine-neutral):
`Snapshot { rows: Vec<Vec<SnapCell>>, cursor, damage, … }`, `SnapCell`, `CellAttrs`,
`CursorShape`, `CursorPos`, `Damage`, `CellDamage`, `LineBounds`, `SearchMatch`,
`PaneEvent`, `Palette`/`Rgb`.

**Pane interface** (the ~26 methods on `TermPane`):
- lifecycle: `spawn` / `spawn_env`, `Drop` (joins the I/O thread), `pid`,
  `scrollback_limit`, `set_palette`
- input/output: `write(bytes)`, `resize(cols, rows)`, `drain_events`
- rendering: `snapshot`, `render_snapshot`, `snapshot_lines(top, rows)`
- scrolling: `scroll(delta)`, `scroll_to_bottom`, `scroll_to_line`, `scroll_info`,
  `line_bounds`
- queries (terminal mode): `app_cursor_keys`, `wants_mouse`, `wants_motion`,
  `mouse_sgr`, `is_alt_screen`
- text: `to_text`, `selection_text(anchor, head, block)`, `search(needle, case)`

Any implementation that satisfies this surface — producing grid/cursor/scrollback/
damage state indistinguishable from the vendored engine on the same byte stream — is a
valid drop-in.

## Engine selection

- **Build-time (implemented, Phase 0):** cargo features on `rt-engine` —
  `default = ["vendored"]`; `vendored = ["dep:alacritty_terminal"]` (optional dep, so
  `--no-default-features --features own` drops alacritty entirely — verified via
  `cargo tree`); `own = []` (marker until Phase 2/3 land the crates).
- **Runtime (Phase 4):** an `RT_ENGINE={vendored|own}` env var selects at startup when
  both implementations exist. Deferred deliberately — a runtime switch is meaningless
  with one implementation, and building it now would be hollow ceremony.

## What is intentionally NOT done yet

Extracting the full contract into a formal `Engine` **trait** (beyond the existing
`Backend` trait) and making `AppSession` generic over it is deferred to **when the
first `own` skeleton exists**. Rationale: the trait's exact shape should be validated
against a real second implementation, not guessed; and touching working rt's generics
now would be churn with regression risk and zero present benefit — a direct conflict
with the program's first guarantee ("rt keeps working the whole time"). This document
is the contract of record until that trait is written.

## Boundary guard

Keep the boundary clean: rt and rt-mux must never gain a direct `alacritty_terminal`
dependency, and `rt-engine`'s public API must never expose an alacritty type. A CI
check (`rg -q 'alacritty' crates/rt/src crates/rt-mux/src` finds only comments; no
alacritty type in `pub` signatures) enforces it.
