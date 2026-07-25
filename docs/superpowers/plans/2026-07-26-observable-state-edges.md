# Observable-State Edges (Modes + Cursor Shape) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Implement the mode/cursor-shape edges the query/report slice excluded — ANSI SM/RM, IRM, LNM, DECSCUSR shape+blink, the mouse-trio bit model, 1005/1042 — to match the vendored oracle, and extend the two existing differentials to cover them at 0 divergences on both architectures.

**Architecture:** All engine behavior lands in `vt-term`. ANSI `SM`/`RM` gets plumbed into `csi_dispatch` (today a no-op); IRM inserts on print; LNM CR-on-LF; DECSCUSR shape is wired into the conformance `observe()` and blink is tracked for DECRQM 12; the `MouseMode` enum is replaced by three independent bits matching the oracle (public accessors preserved, so rt-engine is untouched). Verification reuses `vtterm_fuzz` (grid/cursor/shape) and `vtterm_report` (DECRQM) — no new machinery.

**Tech Stack:** Rust workspace. `vt-term` (engine), `vt-conformance` (dev-only differential harness) driven against vendored `alacritty_terminal`. `ci/verify.sh` runs the battery on x86-64 (local + apollo) and riscv-64 (milkv).

## Global Constraints

- **Match the vendored oracle exactly.** Oracle references in `vendor/alacritty_terminal/src/term/mod.rs`: `report_mode` (2245, ANSI), `report_private_mode` (2156, private), `set_mode`/`unset_mode` (Insert=IRM, LineFeedNewLine=LNM), and the `TermMode` mouse bits (`MOUSE_REPORT_CLICK`/`MOUSE_DRAG`/`MOUSE_MOTION`, set independently). `ModeState`: 0 = not-recognised, 1 = set, 2 = reset.
- **Mouse modes are independent bits, not an enum.** Setting 1003 does NOT clear 1000; resetting one leaves the others. This is the whole reason the query/report differential excluded them.
- **1005 and 1006 are independent.** `1005h` must NOT clear `mouse_sgr` (1006) — the current `1005 => if set { self.mouse_sgr = false }` is a divergence to remove.
- **Preserve public accessors** `wants_mouse()`, the any-motion accessor (`crates/vt-term/src/lib.rs:~745`), and `mouse_sgr()` — rt-engine/rt read only these, so their signatures must not change.
- **IRM is gated:** when `insert_mode` is off, the print path (`put_char`/`print_str`) must be byte-for-byte unchanged.
- **Both differentials keep a 0 ceiling** on x86-64 AND riscv-64 via `ci/verify.sh`. A divergence surfaced by the fuzz is fixed in vt-term to match the oracle, never hidden by narrowing a pool or loosening a comparator.
- **Keep the project map in sync** (CLAUDE.md standing order).

## File Structure

- `crates/vt-term/src/lib.rs` — MODIFY: new mode fields; `set_ansi_mode` + `'h'`/`'l'` arms; IRM insert-on-print; LNM in `line_feed`; DECSCUSR blink; the mouse-bit refactor (remove `MouseMode` enum + `mouse_mode` + `mouse_off_if`); 1005/1042; extend `report_mode`. Unit tests in the crate's `#[cfg(test)]` module.
- `crates/vt-conformance/src/vtterm.rs` — MODIFY: `observe()` reports the real cursor shape (map `CursorShape` → 0/1/2) instead of hardcoded 0.
- `crates/vt-conformance/src/lib.rs` — MODIFY: `gen_script` emits ANSI SM/RM (4/20) and DECSCUSR.
- `crates/vt-conformance/tests/vtterm_report.rs` — MODIFY: add the now-supported modes to `QUERIES`/`MUTATORS`.
- Docs: `docs/vt-term-design.md`, `docs/engine-divergence.md`, `docs/own-engine-plan.md`, `project-map.js`.

Reference (do not re-derive): `csi_dispatch` main match ends at `lib.rs:~1928` (the `_ => {}` before the `}`); it has no `'h'`/`'l'` arm today. `set_mode` (private DECSET/DECRST) is at `:1399`. `line_feed` at `:970`. `put_char` at `:1091`; `print_str` at `:1808` (its early fallbacks at `:1811`); `insert_chars` at `:1272` (the shift primitive). `set_cursor_shape` at `:1440`; `CursorShape` enum (Block/Underline/Beam) at `:214`; `cursor_shape` field at `:443`. `report_mode` at `:2039`. `MouseMode` enum at `:223`; `mouse_mode` field at `:446`; `wants_mouse()` at `:740`, any-motion accessor at `:745`, `mouse_sgr()` at `:749`; mouse arms in `set_mode` at `:1417`; `mouse_off_if` at `:1431`. Conformance `observe()` hardcodes `shape: 0` at `vtterm.rs:60`; the oracle maps shape at `vendored.rs:153-163` (Block→0, Underline→1, Beam→2, HollowBlock→3, Hidden→4, `visible: shape != 4`). `gen_script` at `vt-conformance/src/lib.rs:218` (a `match r.below(13)`).

---

### Task 1: ANSI SM/RM plumbing + LNM (newline mode, 20)

**Files:**
- Modify: `crates/vt-term/src/lib.rs` (`csi_dispatch` ~:1928; new `set_ansi_mode`; `line_feed` :970; `report_mode` :2039)
- Test: `crates/vt-term/src/lib.rs` `#[cfg(test)]`

**Interfaces:**
- Produces: `Term::set_ansi_mode(&mut self, &[u16], bool)`; a `newline_mode: bool` field; `'h'`/`'l'` arms in `csi_dispatch`'s main match; DECRQM ANSI `20`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn lnm_makes_linefeed_carriage_return() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[5;10Habc");      // cursor to row5 col10 (1-based), print -> col advances
    t.feed(b"\x1b[20h");            // LNM on
    t.feed(b"\n");                  // LF should now also CR -> column 0
    let (col, _line) = t.cursor();
    assert_eq!(col, 0, "LNM on: LF returns to column 0");
}

#[test]
fn lnm_off_linefeed_keeps_column() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[5;10Habc\x1b[20l\n"); // LNM explicitly off
    let (col, _line) = t.cursor();
    assert_eq!(col, 12, "LNM off: LF preserves column (col 10 + 'abc'=3 -> 0-based 12)");
}

#[test]
fn decrqm_ansi_lnm() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[20$p");
    assert_eq!(t.take_output(), b"\x1b[20;2$y"); // default reset
    t.feed(b"\x1b[20h\x1b[20$p");
    assert_eq!(t.take_output(), b"\x1b[20;1$y"); // set
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vt-term lnm_ decrqm_ansi_lnm 2>&1 | tail -20`
Expected: FAIL — `\x1b[20h` is a no-op today (SM/RM unhandled), so LNM tests fail; DECRQM ANSI returns `;0`.

- [ ] **Step 3: Implement SM/RM plumbing + LNM**

Add the field near the other mode flags (~:448): `newline_mode: bool,` and init `newline_mode: false,` in `Term::new`.

Add `set_ansi_mode` next to `set_mode`:

```rust
    /// ANSI SM/RM (`CSI Ps h` / `CSI Ps l`, no `?`). vt-term supports the ANSI modes
    /// alacritty does: 4 = IRM (insert), 20 = LNM (newline). Unknown modes are ignored,
    /// matching the oracle (`set_mode`/`unset_mode`, term/mod.rs).
    fn set_ansi_mode(&mut self, p: &[u16], set: bool) {
        for &mode in p {
            match mode {
                4 => self.insert_mode = set,   // IRM — Task 2 adds the field + behavior
                20 => self.newline_mode = set, // LNM
                _ => {}
            }
        }
    }
```

> NOTE: `insert_mode` is added in Task 2. For THIS task, include only the `20 => self.newline_mode = set` arm (drop the `4 =>` line); Task 2 adds the `4` arm and the field. Do not reference `insert_mode` yet.

In `csi_dispatch`'s main `match action` block (alongside `'n'`, `'c'`), add:

```rust
            'h' => self.set_ansi_mode(&p, true),
            'l' => self.set_ansi_mode(&p, false),
```

In `line_feed` (:970), apply LNM — CR on LF when the mode is set:

```rust
    fn line_feed(&mut self) {
        if self.newline_mode {
            self.col = 0;          // LNM: LF also carriage-returns
            self.pending_wrap = false;
        }
        if self.row == self.scroll_bottom {
            self.scroll_up(1);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }
```

(`line_feed` is the shared LF/VT/FF handler — see `execute`'s `0x0A|0x0B|0x0C => self.line_feed()` — so LNM covers all three, matching the oracle.)

In `report_mode`'s ANSI branch (:2056), replace the flat `0` with:

```rust
        } else {
            match mode {
                4 => st(self.insert_mode),   // Task 2 (until then, this arm is added with Task 2)
                20 => st(self.newline_mode),
                _ => 0,
            }
        };
```

> NOTE: same as set_ansi_mode — for THIS task include only the `20 =>` arm; Task 2 adds `4 =>`.

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p vt-term lnm_ decrqm_ansi_lnm 2>&1 | tail -20`
Expected: PASS (3 tests).

- [ ] **Step 5: Full vt-term suite**

Run: `cargo test -p vt-term 2>&1 | tail -5`
Expected: all pass (SM/RM plumbing is additive; private `?h`/`?l` untouched).

- [ ] **Step 6: Commit**

```bash
git add crates/vt-term/src/lib.rs
git commit -m "feat(vt-term): ANSI SM/RM plumbing + LNM (newline mode)"
```

---

### Task 2: IRM (insert mode, 4)

**Files:**
- Modify: `crates/vt-term/src/lib.rs` (`set_ansi_mode`; `put_char` :1091; `print_str` :1811; `report_mode`)
- Test: `crates/vt-term/src/lib.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `set_ansi_mode` (Task 1), `insert_chars` (:1272).
- Produces: `insert_mode: bool` field; insert-on-print in `put_char`; DECRQM ANSI `4`.

- [ ] **Step 1: Write failing tests**

```rust
#[test]
fn irm_inserts_shifting_cells_right() {
    let mut t = Term::new(10, 2);
    t.feed(b"ABCDE\x1b[H");   // row0 = "ABCDE     ", cursor home (row0 col0)
    t.feed(b"\x1b[4h");        // IRM on
    t.feed(b"X");              // insert X at col0 -> "XABCDE    "
    assert_eq!(row_string(&t, 0), "XABCDE    ");
    let (col, _l) = t.cursor();
    assert_eq!(col, 1);
}

#[test]
fn irm_off_overwrites() {
    let mut t = Term::new(10, 2);
    t.feed(b"ABCDE\x1b[H\x1b[4lX"); // IRM explicitly off -> overwrite
    assert_eq!(row_string(&t, 0), "XBCDE     ");
}

#[test]
fn irm_drops_last_cell_at_edge() {
    let mut t = Term::new(5, 1);
    t.feed(b"ABCDE\x1b[H\x1b[4hZ"); // insert at col0 in a full row -> "ZABCD" (E dropped)
    assert_eq!(row_string(&t, 0), "ZABCD");
}

#[test]
fn decrqm_ansi_irm() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[4$p");
    assert_eq!(t.take_output(), b"\x1b[4;2$y");
    t.feed(b"\x1b[4h\x1b[4$p");
    assert_eq!(t.take_output(), b"\x1b[4;1$y");
}
```

Add this test helper to the `#[cfg(test)]` module if one like it does not already exist (check first; reuse the existing cell/row accessor if present):

```rust
fn row_string(t: &Term, row: usize) -> String {
    (0..t.cols()).map(|c| t.cell(row, c).c).collect()
}
```

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vt-term irm_ decrqm_ansi_irm 2>&1 | tail -20`
Expected: FAIL — no `insert_mode`; `\x1b[4h` currently a no-op (or unrecognised), so inserts overwrite.

- [ ] **Step 3: Implement IRM**

Add the field (~:448): `insert_mode: bool,` and init `insert_mode: false,`.

Add the `4 =>` arm to `set_ansi_mode` and to `report_mode`'s ANSI match (the arms deferred from Task 1):
```rust
    // in set_ansi_mode:
    4 => self.insert_mode = set,   // IRM
    // in report_mode ANSI branch:
    4 => st(self.insert_mode),
```

In `put_char` (:1091), after the `pending_wrap`/`soft_wrap` handling and BEFORE the write, shift for insert mode. Insert applies to the narrow and wide write paths alike — do it once, by width, right before writing:

```rust
        // IRM (insert mode): shift the row's cells from the cursor right by the glyph
        // width, dropping the rightmost, then write into the freed cell(s). Matches
        // alacritty's INSERT branch in `input`. `insert_chars` is the same shift ICH uses.
        if self.insert_mode {
            self.insert_chars(width);
        }
```

Place this immediately after the `if self.pending_wrap { self.soft_wrap(); }` block (line ~1102), before the `if width == 2 { … } else { … }` write. `width` is already computed at the top of `put_char`.

In `print_str` (:1811), add `insert_mode` to the wholesale-fallback guard so the batched fast path is skipped when inserting (the fast path overwrites in place):

```rust
        if self.charsets[self.gl] != Charset::Ascii || self.insert_mode {
            for c in s.chars() {
                self.put_char(c);
            }
            return;
        }
```

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p vt-term irm_ decrqm_ansi_irm 2>&1 | tail -20`
Expected: PASS (4 tests).

- [ ] **Step 5: Full vt-term suite (IRM-off path unchanged)**

Run: `cargo test -p vt-term 2>&1 | tail -5`
Expected: all pass — with `insert_mode` false, `put_char`/`print_str` behave exactly as before.

- [ ] **Step 6: Commit**

```bash
git add crates/vt-term/src/lib.rs
git commit -m "feat(vt-term): IRM (insert mode) — insert-on-print"
```

---

### Task 3: Mouse-trio bit refactor + Utf8Mouse (1005) + UrgencyHints (1042)

**Files:**
- Modify: `crates/vt-term/src/lib.rs` (`MouseMode` enum :223, `mouse_mode` field :446, accessors :740/:745, `set_mode` :1417, `mouse_off_if` :1431, `report_mode`)
- Test: `crates/vt-term/src/lib.rs` `#[cfg(test)]`

**Interfaces:**
- Produces: `mouse_click`/`mouse_drag`/`mouse_motion`/`utf8_mouse`/`urgency_hints` bool fields; unchanged `wants_mouse()`/any-motion/`mouse_sgr()` signatures; DECRQM `1000`/`1002`/`1003`/`1005`/`1042`.

- [ ] **Step 1: Confirm `MouseMode` is vt-term-internal**

Run: `grep -rn "MouseMode" crates/ --include=*.rs | grep -v "crates/vt-term/src/lib.rs"`
Expected: no matches (only vt-term uses it). If any external use exists, STOP and report — the refactor would need to preserve it.

- [ ] **Step 2: Write failing tests**

```rust
#[test]
fn mouse_modes_are_independent() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[?1000h\x1b[?1003h");   // click + any-motion both on
    t.feed(b"\x1b[?1000$p"); assert_eq!(t.take_output(), b"\x1b[?1000;1$y");
    t.feed(b"\x1b[?1003$p"); assert_eq!(t.take_output(), b"\x1b[?1003;1$y");
    // resetting one leaves the other
    t.feed(b"\x1b[?1003l");
    t.feed(b"\x1b[?1000$p"); assert_eq!(t.take_output(), b"\x1b[?1000;1$y");
    t.feed(b"\x1b[?1003$p"); assert_eq!(t.take_output(), b"\x1b[?1003;2$y");
    assert!(t.wants_mouse()); // 1000 still on
}

#[test]
fn utf8_mouse_does_not_clear_sgr() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[?1006h\x1b[?1005h");   // SGR then UTF8
    assert!(t.mouse_sgr(), "1005 must NOT clear 1006");
    t.feed(b"\x1b[?1005$p"); assert_eq!(t.take_output(), b"\x1b[?1005;1$y");
    t.feed(b"\x1b[?1006$p"); assert_eq!(t.take_output(), b"\x1b[?1006;1$y");
}

#[test]
fn urgency_hints_flag() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[?1042$p"); assert_eq!(t.take_output(), b"\x1b[?1042;2$y");
    t.feed(b"\x1b[?1042h\x1b[?1042$p"); assert_eq!(t.take_output(), b"\x1b[?1042;1$y");
}
```

- [ ] **Step 3: Run to verify failure**

Run: `cargo test -p vt-term mouse_modes_ utf8_mouse_ urgency_hints_ 2>&1 | tail -20`
Expected: FAIL — enum model reports 1000 reset after 1003 set; 1005 clears sgr; 1042 unrecognised.

- [ ] **Step 4: Refactor to independent bits**

Delete the `MouseMode` enum (:223) and the `mouse_off_if` method (:1431). Replace the `mouse_mode: MouseMode` field with:

```rust
    mouse_click: bool,   // DECSET 1000
    mouse_drag: bool,    // DECSET 1002 (button-motion)
    mouse_motion: bool,  // DECSET 1003 (any-motion)
    utf8_mouse: bool,    // DECSET 1005 (encoding; independent of SGR 1006)
    urgency_hints: bool, // DECSET 1042
```

Init all `false` in `Term::new` (replace `mouse_mode: MouseMode::Off,`).

Rewrite the accessors (:740, :745) to derive from the bits (keep names/signatures):

```rust
    pub fn wants_mouse(&self) -> bool {
        self.mouse_click || self.mouse_drag || self.mouse_motion
    }
    // the any-motion accessor (keep its existing name):
    pub fn <any_motion_name>(&self) -> bool {
        self.mouse_motion
    }
```

> Use the accessor's real existing name (read it at `:745`); do not rename it.

Replace the mouse arms in `set_mode` (:1417-1421) with independent bit assignments, and add 1005/1042:

```rust
                1000 => self.mouse_click = set,
                1002 => self.mouse_drag = set,
                1003 => self.mouse_motion = set,
                1006 => self.mouse_sgr = set,   // SGR encoding — independent
                1005 => self.utf8_mouse = set,  // UTF-8 encoding — independent (does NOT touch mouse_sgr)
                1004 => self.focus_events = set,
                1007 => self.alt_scroll = set,
                1042 => self.urgency_hints = set,
                2004 => self.bracketed_paste = set,
```

In `report_mode`'s private match, add:

```rust
                1000 => st(self.mouse_click),
                1002 => st(self.mouse_drag),
                1003 => st(self.mouse_motion),
                1005 => st(self.utf8_mouse),
                1042 => st(self.urgency_hints),
```

- [ ] **Step 5: Run to verify pass + full suite**

Run: `cargo test -p vt-term mouse_modes_ utf8_mouse_ urgency_hints_ 2>&1 | tail -20` → PASS.
Run: `cargo test -p vt-term 2>&1 | tail -5` → all pass.
Run: `cargo build -p rt-engine -p rt 2>&1 | tail -5` → clean (accessors unchanged, so rt-engine/rt compile untouched).

- [ ] **Step 6: Commit**

```bash
git add crates/vt-term/src/lib.rs
git commit -m "refactor(vt-term): independent mouse-mode bits; 1005/1042 tracked"
```

---

### Task 4: DECSCUSR cursor-shape observability + blink (12)

**Files:**
- Modify: `crates/vt-term/src/lib.rs` (`set_cursor_shape` :1440, `set_mode` :1399, `report_mode`; new `cursor_blink`)
- Modify: `crates/vt-conformance/src/vtterm.rs` (`observe()` :56-60)
- Test: `crates/vt-term/src/lib.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `CursorShape` (:214), `cursor_shape()` accessor (:736).
- Produces: `cursor_blink: bool`; DECRQM `12`; a neutral-shape mapping in the harness `observe()`.

- [ ] **Step 1: Write failing tests (vt-term unit)**

```rust
#[test]
fn decscusr_sets_shape() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[4 q"); assert_eq!(t.cursor_shape(), CursorShape::Underline); // steady underline
    t.feed(b"\x1b[6 q"); assert_eq!(t.cursor_shape(), CursorShape::Beam);      // steady bar
    t.feed(b"\x1b[2 q"); assert_eq!(t.cursor_shape(), CursorShape::Block);     // steady block
}

#[test]
fn decrqm_blink_from_decscusr_and_mode12() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[2 q");           // steady block -> blink off
    t.feed(b"\x1b[?12$p"); assert_eq!(t.take_output(), b"\x1b[?12;2$y");
    t.feed(b"\x1b[1 q");           // blinking block -> blink on
    t.feed(b"\x1b[?12$p"); assert_eq!(t.take_output(), b"\x1b[?12;1$y");
    t.feed(b"\x1b[?12l");          // mode 12 reset -> blink off
    t.feed(b"\x1b[?12$p"); assert_eq!(t.take_output(), b"\x1b[?12;2$y");
}
```

> The `Ps 0` default-blink case is intentionally not unit-asserted here — it is config-dependent in the oracle and is pinned by the DECRQM differential in Task 6. Match whatever the oracle reports for a fresh terminal + `\x1b[0 q`.

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vt-term decscusr_ decrqm_blink 2>&1 | tail -20`
Expected: FAIL — no `cursor_blink`; DECRQM 12 returns `;0`.

- [ ] **Step 3: Implement blink + wire observe()**

Add field (~:443, near `cursor_shape`): `cursor_blink: bool,` init `cursor_blink: false,`.

Update `set_cursor_shape` (:1440) to also set blink from `Ps` parity (odd = blink; matches alacritty's DECSCUSR):

```rust
    /// DECSCUSR (`CSI Ps SP q`): 0/1/2 = block, 3/4 = underline, 5/6 = bar; odd = blink.
    fn set_cursor_shape(&mut self, ps: u16) {
        self.cursor_shape = match ps {
            3 | 4 => CursorShape::Underline,
            5 | 6 => CursorShape::Beam,
            _ => CursorShape::Block,
        };
        // Blink: 1/3/5 blink, 2/4/6 steady. Ps 0 = default; start with steady and let
        // the DECRQM differential (Task 6) pin whether the oracle blinks on `\x1b[0 q`.
        self.cursor_blink = matches!(ps, 1 | 3 | 5);
    }
```

> If Task 6's differential shows the oracle blinks on `\x1b[0 q`, add `0` to the `matches!` set. Keep it a single `matches!`.

Add mode 12 to `set_mode`'s private match (:1399): `12 => self.cursor_blink = set,`.

Add mode 12 to `report_mode`'s private match: `12 => st(self.cursor_blink),`.

In `crates/vt-conformance/src/vtterm.rs`, replace the hardcoded `shape: 0` (:60) with the real shape. The `observe()` currently builds the cursor only when `self.cursor_visible()`; map the shape there:

```rust
        let cursor = if self.cursor_visible() {
            let shape = match self.cursor_shape() {
                vt_term::CursorShape::Block => 0,
                vt_term::CursorShape::Underline => 1,
                vt_term::CursorShape::Beam => 2,
            };
            Some(NCursor { col, line, shape, visible: true })
        } else {
            None
        };
```

(The oracle also returns `None`/shape via its own path; neutral 3/4 never arise from DECSCUSR, so Block/Underline/Beam → 0/1/2 is complete.)

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p vt-term decscusr_ decrqm_blink 2>&1 | tail -20` → PASS.
Run: `cargo test -p vt-term -p vt-conformance 2>&1 | tail -6` → all pass (observe() change compiles against both; existing strands unaffected because the fuzz doesn't emit DECSCUSR yet).

- [ ] **Step 5: Commit**

```bash
git add crates/vt-term/src/lib.rs crates/vt-conformance/src/vtterm.rs
git commit -m "feat(vt-term): DECSCUSR blink + cursor-shape observability"
```

---

### Task 5: Grid differential — extend gen_script (SM/RM + DECSCUSR)

**Files:**
- Modify: `crates/vt-conformance/src/lib.rs` (`gen_script` :218)

**Interfaces:**
- Consumes: the modes implemented in Tasks 1–4; the existing `vtterm_fuzz` and `vtterm_reflow` strands (both drive `gen_script`).

- [ ] **Step 1: Extend gen_script**

Widen the top-level `match r.below(13)` to `r.below(15)` and add two arms that emit the newly-supported grid-observable sequences interleaved with the existing tokens:

```rust
            13 => {
                // ANSI SM/RM for IRM (4) and LNM (20): toggle insert / newline mode.
                let mode = if r.below(2) == 0 { 4 } else { 20 };
                let set = r.below(2) == 0;
                out.extend_from_slice(b"\x1b[");
                out.extend_from_slice(format!("{mode}").as_bytes());
                out.push(if set { b'h' } else { b'l' });
            }
            14 => {
                // DECSCUSR: CSI Ps SP q, Ps in 0..=6.
                out.extend_from_slice(b"\x1b[");
                out.extend_from_slice(format!("{}", r.below(7)).as_bytes());
                out.extend_from_slice(b" q");
            }
```

(IRM and LNM change grid/cursor state; DECSCUSR changes the observed cursor shape. Both engines now implement all three, so the `ScreenState` diff must stay identical.)

- [ ] **Step 2: Run the grid + reflow differentials locally**

Run: `cargo test -p vt-conformance --test vtterm_fuzz --test vtterm_reflow 2>&1 | tail -20`
Expected: PASS (0 divergences). If a seed diverges, the panic prints the script + a state diff — delta-debug it, compare vt-term's handler to the cited oracle function, and FIX vt-term (e.g. an IRM edge, or the DECSCUSR `Ps 0` shape) to match. Never narrow the generator to hide it.

- [ ] **Step 3: Full conformance suite**

Run: `cargo test -p vt-conformance 2>&1 | tail -8`
Expected: all strands green.

- [ ] **Step 4: Multi-arch verify**

Run: `bash ci/verify.sh 2>&1 | tail -25`
Expected: `==> ALL GREEN` on local + apollo + milkv (the hardened script now fails loudly on any empty/errored remote).

- [ ] **Step 5: Commit**

```bash
git add crates/vt-conformance/src/lib.rs
git commit -m "test(vt-conformance): fuzz IRM/LNM/DECSCUSR in the grid differential"
```

---

### Task 6: DECRQM differential — extend vtterm_report pools

**Files:**
- Modify: `crates/vt-conformance/tests/vtterm_report.rs`

**Interfaces:**
- Consumes: DECRQM for the modes implemented in Tasks 1–4.

- [ ] **Step 1: Add the now-supported modes to the pools**

In `QUERIES`, add the DECRQM queries for the modes now matched:
```rust
    b"\x1b[4$p",      // DECRQM IRM (ANSI)
    b"\x1b[20$p",     // DECRQM LNM (ANSI)
    b"\x1b[?12$p",    // DECRQM cursor blink
    b"\x1b[?1000$p",  // DECRQM mouse click
    b"\x1b[?1002$p",  // DECRQM mouse drag
    b"\x1b[?1003$p",  // DECRQM mouse any-motion
    b"\x1b[?1005$p",  // DECRQM utf8 mouse
    b"\x1b[?1042$p",  // DECRQM urgency hints
```

In `MUTATORS`, add setters/resetters so the queries observe varied state (include DECSCUSR so blink varies both ways):
```rust
    b"\x1b[4h", b"\x1b[4l", b"\x1b[20h", b"\x1b[20l",
    b"\x1b[?12h", b"\x1b[?12l", b"\x1b[1 q", b"\x1b[2 q",
    b"\x1b[?1000h", b"\x1b[?1002h", b"\x1b[?1003h", b"\x1b[?1003l",
    b"\x1b[?1005h", b"\x1b[?1042h",
```

Update the strand's header comment: these modes are no longer excluded; the only remaining documented exclusions are the ones still genuinely unimplemented (none from this list).

- [ ] **Step 2: Run the DECRQM differential**

Run: `cargo test -p vt-conformance --test vtterm_report 2>&1 | tail -30`
Expected: PASS (0 divergences). A failure means a mode's reported state or the `Ps 0` blink default disagrees with the oracle — delta-debug the printed script + replies and fix vt-term to match (this is where the `Ps 0` blink default and any mouse-bit edge get pinned).

- [ ] **Step 3: Multi-arch verify**

Run: `bash ci/verify.sh 2>&1 | tail -25`
Expected: `==> ALL GREEN` on both architectures.

- [ ] **Step 4: Commit**

```bash
git add crates/vt-conformance/tests/vtterm_report.rs
git commit -m "test(vt-conformance): DECRQM differential covers IRM/LNM/12/mouse/1005/1042"
```

---

### Task 7: Docs + project map

**Files:**
- Modify: `docs/vt-term-design.md`, `docs/engine-divergence.md`, `docs/own-engine-plan.md`, `project-map.js`

- [ ] **Step 1: vt-term design doc**

Add to `docs/vt-term-design.md`: ANSI SM/RM handling; **IRM** (insert-on-print via the ICH shift, print_str fast-path fallback); **LNM** (CR-on-LF in the shared linefeed path); **DECSCUSR** (shape mapping 0/1/2 = block/underline/bar and the blink bit, set by both DECSCUSR parity and DECSET 12); and the **mouse-bit model** (three independent bits replacing the enum, with `wants_mouse()`/any-motion derived), plus 1005/1042 as independent bits.

- [ ] **Step 2: Divergence ledger**

In `docs/engine-divergence.md`: remove IRM(4)/LNM(20), private 12/1005/1042, and the mouse trio 1000/1002/1003 from the "deliberately-excluded DECRQM modes" list (they are now implemented and differentially covered). Note the extended `vtterm_fuzz` (SM/RM + DECSCUSR) and `vtterm_report` coverage, both still 0-ceiling on x86-64 + riscv-64. Record the 1005-no-longer-clears-1006 reconciliation, and (if the differential pinned it) the DECSCUSR `Ps 0` blink default.

- [ ] **Step 3: Own-engine plan**

In `docs/own-engine-plan.md`, mark Phase-3 mode coverage progressed: ANSI modes (IRM/LNM), DECSCUSR shape/blink, and the mouse-bit reconciliation now implemented and verified; colon-subparam SGR and OSC/DCS still open.

- [ ] **Step 4: Project map**

In `project-map.js`: update the `vt-term` node's "OSC / DCS & query-report edges" part to reflect the widened mode coverage (IRM/LNM/DECSCUSR/mouse-bits now done; OSC side-effects, DCS, and colon-SGR still open). Set `project.updated` to today. Keep every `nodes[].deps` id valid. Sanity: `node --check project-map.js`.

- [ ] **Step 5: Commit**

```bash
git add docs/vt-term-design.md docs/engine-divergence.md docs/own-engine-plan.md project-map.js
git commit -m "docs: observable-state edges (IRM/LNM/DECSCUSR/mouse-bits); map sync"
```

---

## Self-Review

**Spec coverage:**
- ANSI SM/RM plumbing → Task 1. ✓
- IRM → Task 2; LNM → Task 1. ✓
- DECSCUSR shape observability + blink → Task 4. ✓
- Mouse-trio bit refactor (accessors preserved) → Task 3. ✓
- 1005 (decoupled from 1006) + 1042 → Task 3. ✓
- DECRQM covers 4/20/12/1000/1002/1003/1005/1042 → Tasks 1–4 build the replies, Task 6 verifies. ✓
- Grid differential extension (SM/RM + DECSCUSR) → Task 5; DECRQM differential extension → Task 6; both 0 on both arches. ✓
- Docs + map → Task 7. ✓
- Out of scope (colon-SGR, esctest, OSC side-effects/DCS) → not implemented; Task 7 keeps them listed as open.

**Placeholder scan:** The `<any_motion_name>` and `<oracle-default-for-ps0>` markers are explicit "read the real name / pin via the differential" instructions with a concrete first implementation given, not open TBDs. Tasks 1 and 2 flag the two arms (`4 =>`) that Task 1 defers to Task 2 — deliberate, to keep Task 1 compiling without `insert_mode`.

**Type consistency:** `insert_mode`/`newline_mode`/`cursor_blink`/`utf8_mouse`/`urgency_hints`/`mouse_click`/`mouse_drag`/`mouse_motion` are all `bool` fields on `Term`, set in `set_mode`/`set_ansi_mode`, read in `report_mode` via the `st(bool)` closure. `wants_mouse()`/any-motion/`mouse_sgr()` keep their signatures (Task 3 Step 1 guards external `MouseMode` use). `set_ansi_mode(&[u16], bool)` mirrors `set_mode`'s signature. `row_string` test helper is introduced in Task 2 (guarded against duplication).

**Ordering:** Task 1 establishes SM/RM + `set_ansi_mode` + the ANSI `report_mode` branch with only mode 20; Task 2 adds mode 4 to both. Tasks 5/6 (differentials) run after all implementation, and are the forcing function where IRM edges, the DECSCUSR `Ps 0` blink default, and any mouse-bit corner get pinned — fixes there land in vt-term, never in the harness.
