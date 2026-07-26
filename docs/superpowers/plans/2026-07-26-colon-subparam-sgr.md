# Colon-subparam SGR Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make vt-term's `sgr()` consume the parser's structured (colon-aware) params so colon-delimited SGR — extended colour, underline styles, underline colour — parses correctly and matches the vendored oracle, verified by the grid differential at 0 divergences on both architectures.

**Architecture:** All engine work is in `vt-term`. `sgr()` changes from a flattened `&[u16]` to the structured `&Params` (whose `iter()` already yields one `&[u16]` per parameter = its colon subparams). Colon extended colour and underline styles are observable (fg/bg + new neutral attr bits) and differentially verified; underline colour (58/59) is parsed + tracked in the pen but deliberately NOT stored per-cell (rt renders none; keeps `Cell` 16 bytes/`Copy`). The neutral model gains four underline-style bits.

**Tech Stack:** Rust workspace. `vt-term` (engine), `vt-conformance` (dev-only differential harness) vs vendored `alacritty_terminal`/`vte`. `ci/verify.sh` runs the battery on x86-64 (local + apollo) and riscv-64 (milkv).

## Global Constraints

- **Match the vendored oracle.** SGR attr handling: `vendor/alacritty_terminal/src/term/mod.rs:2008-2032` (underline Attr variants: `Underline`→UNDERLINE, `DoubleUnderline`→DOUBLE_UNDERLINE, `Undercurl`→UNDERCURL, `DottedUnderline`→DOTTED_UNDERLINE, `DashedUnderline`→DASHED_UNDERLINE, `CancelUnderline`→remove ALL_UNDERLINES; each style-set first removes `ALL_UNDERLINES`). Extended-colour + colon/semicolon parsing lives in `vendor/vte/src/ansi.rs` (the SGR `Attr` decoder) — consult it for the exact `38`/`48`/`58` `2`/`5` handling and the colon colorspace-id form.
- **Semicolon forms must keep working** (`38;2;r;g;b`, `38;5;n`, `48;…`): they pass today and are covered by existing tests. The refactor preserves them.
- **`Cell` stays 16 bytes and `Copy`.** Underline colour is tracked on the Term (a `pen_underline_color: Color` field), NOT on `Cell`. Do not add fields to `Cell`.
- **Underline styles are mutually exclusive** (mirror alacritty's ALL_UNDERLINES remove-then-insert): setting one clears the others.
- **Grid differential keeps a 0 ceiling** on x86-64 AND riscv-64. A divergence is fixed in vt-term to match the oracle, never hidden by narrowing `gen_script` or the comparator.
- **Keep the project map in sync** (CLAUDE.md standing order).

## File Structure

- `crates/vt-term/src/lib.rs` — MODIFY: 4 underline flags + accessors + `ATTR_MASK` + `clear_all_underlines`; `pen_underline_color` field; rewrite `sgr` to `&Params` + a colour-decode helper; change the `'m'` call site. Unit tests in `#[cfg(test)]`.
- `crates/vt-conformance/src/lib.rs` — MODIFY: 4 new `attr` bits; extend `gen_script` with an extended-SGR arm.
- `crates/vt-conformance/src/vendored.rs` — MODIFY: `neutral_attrs` maps alacritty's 4 underline flags.
- `crates/vt-conformance/src/vtterm.rs` — MODIFY: `nattrs` maps vt-term's 4 underline flags.
- Docs: `docs/vt-term-design.md`, `docs/engine-divergence.md`, `docs/own-engine-plan.md`, `project-map.js`.

Reference (do not re-derive): `sgr` at `lib.rs:1429`, `sgr_color` at `:1466`; `csi_dispatch` gets `params: &Params` at `:2060`, computes `let p = flat(params)` at `:2061`, and `'m' => self.sgr(&p)` at `:2120`. `flat()` at `:1930` keeps only `g.first()` per param (the subparam-drop bug) — leave it for the OTHER CSI handlers. `Params::iter()` yields `&[u16]` per param. Flag consts at `:75-97` (bits 0–9 used; 10–15 free); `ATTR_MASK` at `:100`; `Cell` at `:107`; `Color` at `:28`; `pen: Cell` at `:408`, init `:497`. Neutral `attr` consts at `vt-conformance/src/lib.rs:32-39` (bits 0–7 used); `neutral_attrs` at `vendored.rs:63-99` (`Flags::` → `attr::`); `nattrs` at `vtterm.rs:18-28` (Cell accessors → `attr::`). `gen_script` at `vt-conformance/src/lib.rs:222` is `match r.below(15)` (arms 0–14 used; widen to 16, add arm 15).

---

### Task 1: Structured `sgr(&Params)` + colon extended colour

**Files:**
- Modify: `crates/vt-term/src/lib.rs` (`sgr` :1429, `sgr_color` :1466, `'m'` call site :2120)
- Test: `crates/vt-term/src/lib.rs` `#[cfg(test)]`

**Interfaces:**
- Produces: `Term::sgr(&mut self, params: &Params)`; a colour-decode helper usable from both the colon (subparam slice) and semicolon (following-params) forms.

- [ ] **Step 1: Write failing tests**

Add a small test helper if none exists (check first): feed bytes, then read a cell's fg/bg. Use the existing `t.cell(row, col)` accessor.

```rust
#[test]
fn sgr_colon_truecolor_fg_and_bg() {
    let mut t = Term::new(20, 2);
    t.feed(b"\x1b[38:2:10:20:30mX");        // colon truecolor fg
    assert_eq!(t.cell(0, 0).fg, vt_term::Color::Rgb(10, 20, 30));
    t.feed(b"\x1b[48:2:1:2:3mY");           // colon truecolor bg
    assert_eq!(t.cell(0, 1).bg, vt_term::Color::Rgb(1, 2, 3));
}

#[test]
fn sgr_colon_colorspace_id_is_skipped() {
    let mut t = Term::new(20, 1);
    // ISO form: 38:2:<colorspace-id>:r:g:b — the id (6-subparam variant) is ignored.
    t.feed(b"\x1b[38:2:0:44:55:66mX");
    assert_eq!(t.cell(0, 0).fg, vt_term::Color::Rgb(44, 55, 66));
}

#[test]
fn sgr_colon_indexed() {
    let mut t = Term::new(20, 1);
    t.feed(b"\x1b[38:5:200mX");
    assert_eq!(t.cell(0, 0).fg, vt_term::Color::Indexed(200));
}

#[test]
fn sgr_semicolon_forms_unchanged() {
    let mut t = Term::new(20, 2);
    t.feed(b"\x1b[38;2;10;20;30mX");
    assert_eq!(t.cell(0, 0).fg, vt_term::Color::Rgb(10, 20, 30));
    t.feed(b"\x1b[38;5;200mY");
    assert_eq!(t.cell(0, 1).fg, vt_term::Color::Indexed(200));
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vt-term sgr_colon 2>&1 | tail -20`
Expected: FAIL — colon forms currently drop the subparams (fg stays `Default`); the semicolon test should already PASS (guards the refactor).

- [ ] **Step 3: Rewrite `sgr` to structured params**

Change the call site (`lib.rs:2120`): `'m' => self.sgr(params),` (pass the structured `&Params`, not `&p`).

Replace `sgr(&mut self, p: &[u16])` with `sgr(&mut self, params: &Params)`. Structure:

```rust
    fn sgr(&mut self, params: &Params) {
        if params.is_empty() {
            self.pen = Cell::default();
            self.pen_underline_color = Color::Default; // added in Task 3; omit this line until then
            return;
        }
        let mut it = params.iter();
        while let Some(g) = it.next() {
            if g.len() >= 2 {
                // Colon-grouped parameter: the whole attribute is in `g`.
                match g[0] {
                    38 => { if let Some(c) = decode_color(&g[1..]) { self.pen.fg = c; } }
                    48 => { if let Some(c) = decode_color(&g[1..]) { self.pen.bg = c; } }
                    // 4 (underline style) -> Task 2; 58 (underline colour) -> Task 3
                    _ => {}
                }
            } else {
                match g.first().copied().unwrap_or(0) {
                    0 => self.pen = Cell::default(),
                    1 => self.pen.flags |= BOLD,
                    // ... all the existing single-code arms (2,3,4,7,8,9,22,23,24,27,28,29,
                    //     30..=37, 39, 40..=47, 49, 90..=97, 100..=107) unchanged ...
                    38 => { if let Some(c) = decode_color_following(&mut it) { self.pen.fg = c; } }
                    48 => { if let Some(c) = decode_color_following(&mut it) { self.pen.bg = c; } }
                    _ => {}
                }
            }
        }
    }
```

Add two free functions (module level, near `flat`):

```rust
/// Decode an extended-colour subparam slice (colon form): the slice is everything AFTER
/// the 38/48. `[2, r, g, b]` -> Rgb; `[2, cs, r, g, b]` (ISO colorspace-id, 5 elems) ->
/// Rgb skipping the id; `[5, n]` -> Indexed. Returns None if unrecognised.
fn decode_color(sub: &[u16]) -> Option<Color> {
    match sub.first().copied()? {
        2 => {
            // 3 trailing values = r,g,b; 4 trailing = colorspace-id,r,g,b (skip id).
            let rgb = if sub.len() >= 5 { &sub[2..5] } else { sub.get(1..4)? };
            Some(Color::Rgb(rgb[0] as u8, rgb[1] as u8, rgb[2] as u8))
        }
        5 => Some(Color::Indexed(sub.get(1).copied().unwrap_or(0) as u8)),
        _ => None,
    }
}

/// Decode the semicolon form by consuming FOLLOWING single-value params from the iterator:
/// after a bare `38`/`48`, the next param is `2` (then r,g,b) or `5` (then n).
fn decode_color_following(it: &mut vt_parser::ParamsIter<'_>) -> Option<Color> {
    match it.next().and_then(|g| g.first().copied())? {
        2 => {
            let r = it.next().and_then(|g| g.first().copied()).unwrap_or(0) as u8;
            let g = it.next().and_then(|g| g.first().copied()).unwrap_or(0) as u8;
            let b = it.next().and_then(|g| g.first().copied()).unwrap_or(0) as u8;
            Some(Color::Rgb(r, g, b))
        }
        5 => Some(Color::Indexed(it.next().and_then(|g| g.first().copied()).unwrap_or(0) as u8)),
        _ => None,
    }
}
```

> Confirm `ParamsIter` is exported from `vt_parser` (it is used as the return of `Params::iter()`); if the type isn't `pub`, make it `pub` in `crates/vt-parser/src/lib.rs` (it is already the return type of a `pub fn`, so it should be nameable — if not, take `&mut impl Iterator<Item = &[u16]>` instead). Delete the old `sgr_color` once its logic is fully subsumed by `decode_color`/`decode_color_following` (verify no other caller).

- [ ] **Step 4: Run to verify pass + full suite**

Run: `cargo test -p vt-term sgr_ 2>&1 | tail -20` → PASS (colon + semicolon).
Run: `cargo test -p vt-term 2>&1 | tail -5` → all pass (the refactor preserves every existing SGR behavior).

- [ ] **Step 5: Commit**

```bash
git add crates/vt-term/src/lib.rs
git commit -m "feat(vt-term): structured sgr(&Params) + colon extended colour"
```

---

### Task 2: Underline styles

**Files:**
- Modify: `crates/vt-term/src/lib.rs` (flag consts :75-97, `ATTR_MASK` :100, accessors near the other flag accessors, `sgr`)
- Test: `crates/vt-term/src/lib.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: the structured `sgr` (Task 1).
- Produces: `DOUBLE_UNDERLINE`/`UNDERCURL`/`DOTTED_UNDERLINE`/`DASHED_UNDERLINE` flags + `Cell` accessors `double_underline()`/`undercurl()`/`dotted_underline()`/`dashed_underline()`; `clear_all_underlines`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn sgr_underline_styles() {
    let mut t = Term::new(10, 1);
    t.feed(b"\x1b[4:3mX");   // curly
    let c = t.cell(0, 0);
    assert!(c.undercurl() && !c.underline() && !c.double_underline() && !c.italic());
    t.feed(b"\x1b[4:2mY");   // double (clears curly)
    let c = t.cell(0, 1);
    assert!(c.double_underline() && !c.undercurl());
}

#[test]
fn sgr_underline_single_and_reset() {
    let mut t = Term::new(10, 1);
    t.feed(b"\x1b[4mX");   assert!(t.cell(0, 0).underline());
    t.feed(b"\x1b[4:0mY"); assert!(!t.cell(0, 1).underline() && !t.cell(0,1).undercurl()); // clear all
    t.feed(b"\x1b[4:3m\x1b[24mZ"); assert!(!t.cell(0, 2).undercurl()); // 24 clears all
}

#[test]
fn sgr_double_underline_21() {
    let mut t = Term::new(10, 1);
    t.feed(b"\x1b[21mX");
    assert!(t.cell(0, 0).double_underline());
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vt-term sgr_underline sgr_double 2>&1 | tail -20`
Expected: FAIL — no `undercurl()`/`double_underline()` accessors (compile error), then wrong flags once they exist.

- [ ] **Step 3: Implement**

Add flags (bits 10–13) near `:97`:

```rust
const DOUBLE_UNDERLINE: u16 = 1 << 10;
const UNDERCURL: u16        = 1 << 11;
const DOTTED_UNDERLINE: u16 = 1 << 12;
const DASHED_UNDERLINE: u16 = 1 << 13;
const ALL_UNDERLINES: u16 = UNDERLINE | DOUBLE_UNDERLINE | UNDERCURL | DOTTED_UNDERLINE | DASHED_UNDERLINE;
```

Add them to `ATTR_MASK` (`:100`): `... | STRIKEOUT | DOUBLE_UNDERLINE | UNDERCURL | DOTTED_UNDERLINE | DASHED_UNDERLINE`.

Add `Cell` accessors next to `underline()`:

```rust
pub fn double_underline(&self) -> bool { self.flags & DOUBLE_UNDERLINE != 0 }
pub fn undercurl(&self) -> bool { self.flags & UNDERCURL != 0 }
pub fn dotted_underline(&self) -> bool { self.flags & DOTTED_UNDERLINE != 0 }
pub fn dashed_underline(&self) -> bool { self.flags & DASHED_UNDERLINE != 0 }
```

Add a pen helper:

```rust
fn set_underline(&mut self, flag: u16) { self.pen.flags = (self.pen.flags & !ALL_UNDERLINES) | flag; }
```

(`flag == 0` clears all — used by `4:0`/`24`.)

In `sgr`, wire the arms:
- Colon group `g[0] == 4`: `self.set_underline(underline_flag_for(g.get(1).copied().unwrap_or(1)))`.
- Single `4` → `self.set_underline(UNDERLINE)`.
- Single `21` → `self.set_underline(DOUBLE_UNDERLINE)`.
- Single `24` → `self.set_underline(0)`.
- **Remove** the old `4 => self.pen.flags |= UNDERLINE` and `24 => self.pen.flags &= !UNDERLINE` single arms (replaced by `set_underline`).

with:

```rust
fn underline_flag_for(sub: u16) -> u16 {
    match sub { 0 => 0, 2 => DOUBLE_UNDERLINE, 3 => UNDERCURL, 4 => DOTTED_UNDERLINE, 5 => DASHED_UNDERLINE, _ => UNDERLINE }
}
```

- [ ] **Step 4: Run to verify pass + full suite**

Run: `cargo test -p vt-term sgr_underline sgr_double 2>&1 | tail -20` → PASS.
Run: `cargo test -p vt-term 2>&1 | tail -5` → all pass (existing underline tests still green; `4`/`24` behavior preserved via `set_underline`).

- [ ] **Step 5: Commit**

```bash
git add crates/vt-term/src/lib.rs
git commit -m "feat(vt-term): SGR underline styles (single/double/curly/dotted/dashed)"
```

---

### Task 3: Underline colour (58/59) — pen only

**Files:**
- Modify: `crates/vt-term/src/lib.rs` (Term field near `pen` :408, init :497, `sgr`)
- Test: `crates/vt-term/src/lib.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `decode_color`/`decode_color_following` (Task 1).
- Produces: `pen_underline_color: Color` field + a `pub fn pen_underline_color(&self) -> Color` accessor for tests.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn sgr_underline_color_pen() {
    let mut t = Term::new(10, 1);
    t.feed(b"\x1b[58:2:9:8:7m");
    assert_eq!(t.pen_underline_color(), vt_term::Color::Rgb(9, 8, 7));
    t.feed(b"\x1b[58:5:42m");
    assert_eq!(t.pen_underline_color(), vt_term::Color::Indexed(42));
    t.feed(b"\x1b[59m");
    assert_eq!(t.pen_underline_color(), vt_term::Color::Default);
}

#[test]
fn sgr_underline_color_does_not_corrupt_attrs() {
    let mut t = Term::new(10, 1);
    t.feed(b"\x1b[1;58:2:9:8:7;4mX"); // bold + underline colour + underline
    let c = t.cell(0, 0);
    assert!(c.bold() && c.underline());
    assert_eq!(c.fg, vt_term::Color::Default); // underline colour must NOT leak into fg
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vt-term sgr_underline_color 2>&1 | tail -20`
Expected: FAIL — no `pen_underline_color` accessor; `58` currently ignored/dropped.

- [ ] **Step 3: Implement**

Add field near `pen: Cell` (`:408`): `pen_underline_color: Color,` and init at `:497`: `pen_underline_color: Color::Default,`. Add accessor:

```rust
/// The pen's current SGR underline colour (58/59). Tracked for parse-correctness;
/// NOT stored per-cell (rt renders no underline colour) — a documented boundary.
pub fn pen_underline_color(&self) -> Color { self.pen_underline_color }
```

Ensure the `sgr` reset paths (empty params and single `0`) reset it to `Color::Default` (add `self.pen_underline_color = Color::Default;` to both — the line flagged in Task 1's skeleton).

Wire the `58`/`59` arms:
- Colon group `g[0] == 58`: `if let Some(c) = decode_color(&g[1..]) { self.pen_underline_color = c; }`
- Single `58` → `if let Some(c) = decode_color_following(&mut it) { self.pen_underline_color = c; }`
- Single `59` → `self.pen_underline_color = Color::Default;`

- [ ] **Step 4: Run to verify pass + full suite**

Run: `cargo test -p vt-term sgr_underline_color 2>&1 | tail -20` → PASS.
Run: `cargo test -p vt-term 2>&1 | tail -5` → all pass.

- [ ] **Step 5: Commit**

```bash
git add crates/vt-term/src/lib.rs
git commit -m "feat(vt-term): SGR underline colour (58/59) tracked in the pen"
```

---

### Task 4: Neutral model + grid differential

**Files:**
- Modify: `crates/vt-conformance/src/lib.rs` (`attr` consts :32-39; `gen_script` :222)
- Modify: `crates/vt-conformance/src/vendored.rs` (`neutral_attrs` :63-99)
- Modify: `crates/vt-conformance/src/vtterm.rs` (`nattrs` :18-28)

**Interfaces:**
- Consumes: the underline-style flags (Task 2), the colour parsing (Tasks 1/3).

- [ ] **Step 1: Extend the neutral model**

`crates/vt-conformance/src/lib.rs` `attr` module — add (bits 8–11):

```rust
    pub const DOUBLE_UNDERLINE: u16 = 1 << 8;
    pub const UNDERCURL: u16        = 1 << 9;
    pub const DOTTED_UNDERLINE: u16 = 1 << 10;
    pub const DASHED_UNDERLINE: u16 = 1 << 11;
```

`vendored.rs` `neutral_attrs` — add after the `UNDERLINE` mapping (use alacritty's flag names as in `vendor/alacritty_terminal/src/term/cell.rs`):

```rust
    if f.contains(Flags::DOUBLE_UNDERLINE) { a |= attr::DOUBLE_UNDERLINE; }
    if f.contains(Flags::UNDERCURL) { a |= attr::UNDERCURL; }
    if f.contains(Flags::DOTTED_UNDERLINE) { a |= attr::DOTTED_UNDERLINE; }
    if f.contains(Flags::DASHED_UNDERLINE) { a |= attr::DASHED_UNDERLINE; }
```

`vtterm.rs` `nattrs` — add:

```rust
    if c.double_underline() { m |= attr::DOUBLE_UNDERLINE; }
    if c.undercurl() { m |= attr::UNDERCURL; }
    if c.dotted_underline() { m |= attr::DOTTED_UNDERLINE; }
    if c.dashed_underline() { m |= attr::DASHED_UNDERLINE; }
```

(Underline colour is deliberately not added to the neutral cell.)

- [ ] **Step 2: Extend `gen_script` with an extended-SGR arm**

Widen the top-level `match r.below(15)` to `r.below(16)` and add arm 15:

```rust
            15 => {
                // Extended SGR: colon + semicolon extended colour, underline styles + colour.
                let pick = r.below(12);
                let s: &[u8] = match pick {
                    0 => b"\x1b[38;2;10;20;30m",
                    1 => b"\x1b[48;2;1;2;3m",
                    2 => b"\x1b[38;5;200m",
                    3 => b"\x1b[38:2:10:20:30m",
                    4 => b"\x1b[38:2:0:44:55:66m", // colorspace-id
                    5 => b"\x1b[38:5:123m",
                    6 => b"\x1b[48:5:99m",
                    7 => b"\x1b[4:3m",   // curly
                    8 => b"\x1b[4:0m",   // clear
                    9 => b"\x1b[21m",    // double
                    10 => b"\x1b[58:2:9:8:7m", // underline colour (no observable effect)
                    _ => b"\x1b[59m",
                };
                out.extend_from_slice(s);
            }
```

- [ ] **Step 3: Drive the grid differentials to 0 locally**

Run: `cargo test -p vt-conformance --test vtterm_fuzz --test vtterm_reflow 2>&1 | tail -30`
Expected: PASS (0 divergences). If a seed diverges, the panic prints the script + a state diff — delta-debug it, compare vt-term to the cited oracle (`term/mod.rs`/`vte ansi.rs`), and FIX vt-term to match (e.g. an underline flag choice, or the colorspace-id skip). Never narrow the generator or the neutral model to hide it. Likely surfaces: which flag `21` maps to, or the exact colorspace-id parsing.

- [ ] **Step 4: Full conformance suite**

Run: `cargo test -p vt-conformance 2>&1 | tail -8`
Expected: all strands green.

- [ ] **Step 5: Multi-arch verify**

Run: `bash ci/verify.sh 2>&1 | tail -25`
Expected: `==> ALL GREEN` on local + apollo + milkv.

- [ ] **Step 6: Commit**

```bash
git add crates/vt-conformance/src/lib.rs crates/vt-conformance/src/vendored.rs crates/vt-conformance/src/vtterm.rs
git commit -m "test(vt-conformance): fuzz colon/extended SGR + underline styles; neutral bits"
```

---

### Task 5: Docs + project map

**Files:**
- Modify: `docs/vt-term-design.md`, `docs/engine-divergence.md`, `docs/own-engine-plan.md`, `project-map.js`

- [ ] **Step 1: vt-term design doc**

Rewrite the SGR section of `docs/vt-term-design.md`: the structured-param parsing (colon vs semicolon vs the ISO colorspace-id form via `decode_color`/`decode_color_following`); the underline-style flags (single/double/curly/dotted/dashed, mutually exclusive, `4:n`/`21`/`24`); and the pen-only underline colour with its documented boundary (not per-cell, rt renders none).

- [ ] **Step 2: Divergence ledger**

In `docs/engine-divergence.md`: record colon-subparam SGR now handled (extended colour both forms + colorspace-id; underline styles). Record the **underline-colour intentional boundary** (parsed + pen-tracked, not per-cell stored/verified — a sparse per-line side-map is the deferred follow-up). Note the extended `gen_script` coverage (still 0-ceiling, both arches).

- [ ] **Step 3: Own-engine plan**

In `docs/own-engine-plan.md`, mark Phase-3 SGR coverage progressed (colon extended colour + underline styles implemented & verified; underline colour parsed/pen-only; OSC/DCS/esctest still open).

- [ ] **Step 4: Project map**

In `project-map.js`: update the `vt-term` node part to reflect colon-SGR done (colon extended colour + underline styles verified; underline colour parsed/pen-only; OSC side-effects, DCS still open). Set `project.updated` to today. Keep every `nodes[].deps` id valid. Sanity: `node --check project-map.js`.

- [ ] **Step 5: Commit**

```bash
git add docs/vt-term-design.md docs/engine-divergence.md docs/own-engine-plan.md project-map.js
git commit -m "docs: colon-subparam SGR (extended colour, underline styles); map sync"
```

---

## Self-Review

**Spec coverage:**
- Structured `sgr(&Params)` → Task 1. ✓
- Colon extended colour (incl. colorspace-id) + semicolon preserved → Task 1. ✓
- Underline styles (single/double/curly/dotted/dashed, mutually exclusive) → Task 2. ✓
- Underline colour 58/59 pen-only (not per-cell) → Task 3. ✓
- Neutral model (4 underline-style bits, both mappings; colour NOT added) + gen_script + differential to 0 both arches → Task 4. ✓
- Docs + map → Task 5. ✓
- Out of scope (per-cell underline colour, OSC/DCS, esctest) → not implemented; Task 5 documents the boundary.

**Placeholder scan:** Task 1's skeleton flags the `pen_underline_color` reset line as "added in Task 3" — a deliberate cross-task note; Task 3 wires it. The `ParamsIter` export caveat gives a concrete fallback (`&mut impl Iterator`). No open TBDs.

**Type consistency:** `sgr(&mut self, params: &Params)` matches the `csi_dispatch` `params` type. `decode_color(&[u16]) -> Option<Color>` and `decode_color_following(&mut ParamsIter) -> Option<Color>` are used in Tasks 1 (colour) and 3 (underline colour). The 4 flag consts (`DOUBLE_UNDERLINE`/`UNDERCURL`/`DOTTED_UNDERLINE`/`DASHED_UNDERLINE`) and their `Cell` accessors (Task 2) are consumed by `nattrs` (Task 4). `pen_underline_color: Color` field + accessor (Task 3). Neutral `attr::` bits (Task 4) match the accessor names.

**Ordering:** Task 1 establishes the structured `sgr` + colour helpers; Task 2 adds underline flags and the `set_underline` replacement for the old `4`/`24` arms; Task 3 adds the pen colour + the reset-path line; Task 4 makes the styles observable and fuzzes them (the forcing function — exact edge behavior like `21`'s flag and colorspace-id parsing is pinned here, fixed in vt-term). Task 4's `gen_script` change perturbs both `vtterm_fuzz` and `vtterm_reflow`; both must stay 0.
