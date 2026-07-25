# Query/Report Differential + Runtime Wiring Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make the in-house `vt-term` answer terminal queries (DSR/CPR, DA1/DA2, DECRQM), prove those replies byte-identical to the vendored oracle under a new differential fuzz strand, and wire the replies to the PTY so real apps get answered under the default engine.

**Architecture:** vt-term accumulates reply bytes in an internal buffer drained by a new `take_output()` (mirroring the existing `take_title()`). The conformance harness gains a `take_output` on its `VtEngine` trait; the vendored oracle captures its `Event::PtyWrite` replies; a new fuzz strand interleaves queries with normal input and compares reply streams (masking DA2's implementation-specific version field). Finally the rt-engine `vtpane` reader loop drains `take_output()` after each feed and forwards to the existing PTY writer thread.

**Tech Stack:** Rust workspace (`vt-parser`, `vt-term`, `vt-conformance` dev-only, `rt-engine`). Differential vs vendored `alacritty_terminal`. `ci/verify.sh` runs `cargo test -p vt-parser -p vt-conformance` locally + on milkv (riscv64) / apollo (x86_64).

## Global Constraints

- **Match the oracle exactly** — every reply vt-term emits for an in-scope query must be byte-identical to `alacritty_terminal`'s, except DA2's version field (intentional, masked). Oracle reply sources, verified: `vendor/alacritty_terminal/src/term/mod.rs` — `identify_terminal` (1367), `device_status` (1442), `report_mode` (2245), `report_private_mode` (2156), `ModeState { NotSupported=0, Set=1, Reset=2 }` (2387).
- **Only differential what the oracle answers.** In scope: DA1, DA2, DSR 5, CPR (DSR 6), DECRQM (ANSI + private). Out: DECRQCRA/checksum, pixel window-ops, kitty keyboard reports, DCS queries — the oracle is silent or environment-dependent, so no valid differential exists.
- **CPR is absolute**, 1-based `\x1b[{line};{col}R` (alacritty uses `grid.cursor.point` directly, not DECOM-relative).
- **Ledger discipline:** the new differential strand's divergence ceiling is **0**, a strict regression guard, green on x86_64 AND riscv64 via `ci/verify.sh`.
- **Hot path untouched:** replies are generated inside `Term::feed` (under the parser lock at the call site); draining happens right after, never on the render/observe path.
- **Keep the project map in sync** (CLAUDE.md standing order): update `project-map.js` when status changes.

---

## File Structure

- `crates/vt-term/src/lib.rs` — MODIFY: add `output: Vec<u8>` field to `Term`, `take_output()`, an internal reply-push helper, and the DSR/DA/DECRQM handlers in `csi_dispatch`. Add unit tests in the crate's `#[cfg(test)]` module.
- `crates/vt-conformance/src/lib.rs` — MODIFY: add `fn take_output(&mut self) -> Vec<u8>` to the `VtEngine` trait; add a `mask_da2`/`reports_match` reply comparator.
- `crates/vt-conformance/src/vendored.rs` — MODIFY: replace the `Noop` listener with a capturing listener that appends `Event::PtyWrite` bytes; implement `take_output`.
- `crates/vt-conformance/src/vtterm.rs` — MODIFY: implement `take_output` by draining `Term::take_output`.
- `crates/vt-conformance/tests/vtterm_report.rs` — CREATE: the query/report differential fuzz strand (ceiling 0).
- `crates/rt-engine/src/vtpane.rs` — MODIFY: drain `take_output()` in the reader loop and forward to the writer thread.
- Docs: `docs/vt-term-design.md`, `docs/engine-divergence.md`, `docs/own-engine-plan.md`, `project-map.js`.

Reference (do not re-derive): `csi_dispatch` is at `crates/vt-term/src/lib.rs:1882`; its intermediate early-return block handles `?h`/`?l`/` q` then `return`s; the main `match action` block has no `'c'` or `'n'` arm today. `flat(params) -> Vec<u16>` yields `p`; `count(&p, i)` returns param `i` (1-based-defaulting). `take_title()` at :762 is the drain pattern to mirror. vt-term's tracked private modes (in `set_mode`, :1382): `1` app_cursor, `6` origin, `7` autowrap, `25` show_cursor, `47/1047/1049` alt-screen, `1000/1002/1003` mouse, `1006` mouse_sgr, `1004` focus_events, `1007` alt_scroll, `2004` bracketed_paste.

---

### Task 1: Output buffer + DSR/CPR (device status)

**Files:**
- Modify: `crates/vt-term/src/lib.rs` (Term struct ~:394, `new` ~:494, drain area ~:762, `csi_dispatch` ~:1897)
- Test: `crates/vt-term/src/lib.rs` `#[cfg(test)]` module (append)

**Interfaces:**
- Produces: `Term::take_output(&mut self) -> Vec<u8>` (drains all pending reply bytes, empty when none); an internal `Term::reply(&mut self, &[u8])` push helper; a `'n'` (DSR) arm in `csi_dispatch`.

- [ ] **Step 1: Write the failing tests**

Append to the `#[cfg(test)]` module in `crates/vt-term/src/lib.rs`:

```rust
#[test]
fn dsr_status_report() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[5n");
    assert_eq!(t.take_output(), b"\x1b[0n");
    // Drained: a second take returns empty.
    assert_eq!(t.take_output(), Vec::<u8>::new());
}

#[test]
fn cpr_reports_absolute_cursor_1based() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[6n"); // cursor home
    assert_eq!(t.take_output(), b"\x1b[1;1R");
    // Move to row 3, col 5 (CUP is 1-based) then query.
    t.feed(b"\x1b[3;5H\x1b[6n");
    assert_eq!(t.take_output(), b"\x1b[3;5R");
}

#[test]
fn dsr_unknown_arg_is_silent() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[9n");
    assert_eq!(t.take_output(), Vec::<u8>::new());
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vt-term dsr_ cpr_ 2>&1 | tail -20`
Expected: FAIL — `take_output` does not exist (compile error), or once the field/method exist but the handler doesn't, the reply assertions fail.

- [ ] **Step 3: Add the buffer, drain, and DSR handler**

In `struct Term` (near the `title: Option<String>` field ~:453) add:

```rust
    /// Bytes the terminal wants to send back to the host (query replies: DSR/CPR,
    /// device attributes, DECRQM). Drained by [`take_output`](Term::take_output)
    /// after each feed — mirrors `title`/`take_title`. Empty on the common path.
    output: Vec<u8>,
```

In `Term::new` initialise it (near `title: None,` ~:494): `output: Vec::new(),`.

Add the drain + push helper near `take_title` (~:762):

```rust
    /// Take the pending host-bound reply bytes (DSR/CPR, DA, DECRQM), clearing the
    /// buffer. Empty when the last feed produced no query reply. The host writes these
    /// straight back to the PTY (see rt-engine `vtpane`), exactly as the vendored engine
    /// answers `Event::PtyWrite`.
    pub fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut self.output)
    }

    /// Queue reply bytes for the host. Called from CSI query handlers during `feed`.
    fn reply(&mut self, bytes: &[u8]) {
        self.output.extend_from_slice(bytes);
    }

    /// DSR — Device Status Report (`CSI Ps n`). Matches alacritty `device_status`
    /// (term/mod.rs:1442): 5 → terminal-OK, 6 → CPR (absolute, 1-based cursor).
    /// Any other argument is silently ignored, as in the oracle.
    fn device_status(&mut self, arg: u16) {
        match arg {
            5 => self.reply(b"\x1b[0n"),
            6 => {
                let r = self.row + 1;
                let c = self.col + 1;
                self.reply(format!("\x1b[{r};{c}R").as_bytes());
            }
            _ => {}
        }
    }
```

In `csi_dispatch`'s main `match action` block (~:1926, alongside the other arms) add:

```rust
            'n' => self.device_status(p.first().copied().unwrap_or(0)),
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vt-term dsr_ cpr_ 2>&1 | tail -20`
Expected: PASS (3 tests).

- [ ] **Step 5: Run the whole vt-term suite (no regressions)**

Run: `cargo test -p vt-term 2>&1 | tail -5`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/vt-term/src/lib.rs
git commit -m "feat(vt-term): reply buffer + DSR/CPR (device status)"
```

---

### Task 2: Device attributes (DA1, DA2)

**Files:**
- Modify: `crates/vt-term/src/lib.rs` (`csi_dispatch` intermediate block ~:1888, main block ~:1897)
- Test: `crates/vt-term/src/lib.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `Term::reply`, `Term::take_output` (Task 1).
- Produces: DA1 on `'c'` (no intermediate); DA2 on `(>, 'c')`.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn da1_primary_device_attributes() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[c");
    assert_eq!(t.take_output(), b"\x1b[?6c"); // VT102, matching alacritty
    t.feed(b"\x1b[0c"); // explicit 0 is the same query
    assert_eq!(t.take_output(), b"\x1b[?6c");
}

#[test]
fn da2_secondary_device_attributes_shape() {
    let mut t = Term::new(80, 24);
    t.feed(b"\x1b[>c");
    let out = t.take_output();
    // `\x1b[>0;<version>;1c` — version is vt-term's own (masked in the differential).
    assert!(out.starts_with(b"\x1b[>0;"), "got {out:?}");
    assert!(out.ends_with(b";1c"), "got {out:?}");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vt-term da1_ da2_ 2>&1 | tail -20`
Expected: FAIL — no reply emitted (`take_output` empty).

- [ ] **Step 3: Implement DA1 + DA2**

Add a helper near `device_status`:

```rust
    /// DA — Device Attributes. Matches alacritty `identify_terminal` (term/mod.rs:1367):
    /// primary (`CSI c` / `CSI 0 c`) → `\x1b[?6c` (VT102); secondary (`CSI > c`) →
    /// `\x1b[>0;<version>;1c`. The secondary version is this emulator's own — apps use it
    /// only for feature sniffing — so it legitimately differs from the oracle's and is the
    /// one field the differential masks.
    fn device_attributes(&mut self, secondary: bool) {
        if secondary {
            // vt-term's crate version in xterm's major*10000+minor*100+patch form.
            let v = env!("CARGO_PKG_VERSION");
            let mut it = v.split('.').map(|s| s.parse::<u32>().unwrap_or(0));
            let (maj, min, pat) = (it.next().unwrap_or(0), it.next().unwrap_or(0), it.next().unwrap_or(0));
            let ver = maj * 10_000 + min * 100 + pat;
            self.reply(format!("\x1b[>0;{ver};1c").as_bytes());
        } else {
            self.reply(b"\x1b[?6c");
        }
    }
```

In the main `match action` block add (next to `'n'`):

```rust
            'c' => self.device_attributes(false), // DA1 (param 0/absent; alacritty ignores others)
```

In the intermediate early-return block (`match (intermediates.first(), action)` ~:1888) add an arm:

```rust
                (Some(&b'>'), 'c') => self.device_attributes(true), // DA2
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vt-term da1_ da2_ 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/vt-term/src/lib.rs
git commit -m "feat(vt-term): DA1/DA2 device attributes"
```

---

### Task 3: DECRQM (request mode) — ANSI + private

**Files:**
- Modify: `crates/vt-term/src/lib.rs` (`csi_dispatch` intermediate block ~:1888; new helper)
- Test: `crates/vt-term/src/lib.rs` `#[cfg(test)]`

**Interfaces:**
- Consumes: `Term::reply`, tracked mode flags (`app_cursor`, `origin`, `autowrap`, `show_cursor`, `alt_screen()`, `focus_events`, `mouse_sgr`, `alt_scroll`, `bracketed_paste`).
- Produces: DECRQM replies on `($,'p')` (ANSI) and `(?…$,'p')` (private).

**Design note (why this exact set):** the differential can only reach 0 where vt-term and alacritty represent a mode identically. vt-term reports state for the private modes it tracks; everything else replies `0` (NotSupported). alacritty tracks a few modes vt-term does not (`12` BlinkingCursor, `1005` Utf8Mouse, `1042` UrgencyHints) and the ANSI modes `4` IRM / `20` LNM — for those vt-term returns `0` while alacritty returns `1`/`2`, a legitimate difference. **The fuzz generator (Task 5) therefore never queries those**; they are documented gaps, deferred to the observable-state-edges slice. The three mouse modes `1000/1002/1003` are excluded from queries too (enum-vs-bitflags representation), though vt-term still answers them at runtime.

- [ ] **Step 1: Write the failing tests**

```rust
#[test]
fn decrqm_private_tracked_modes() {
    let mut t = Term::new(80, 24);
    // DECOM (6): default reset → 2; set → 1.
    t.feed(b"\x1b[?6$p");
    assert_eq!(t.take_output(), b"\x1b[?6;2$y");
    t.feed(b"\x1b[?6h\x1b[?6$p");
    assert_eq!(t.take_output(), b"\x1b[?6;1$y");
    // DECAWM (7): default set → 1.
    t.feed(b"\x1b[?7$p");
    assert_eq!(t.take_output(), b"\x1b[?7;1$y");
    // DECTCEM (25): default set → 1; reset → 2.
    t.feed(b"\x1b[?25l\x1b[?25$p");
    assert_eq!(t.take_output(), b"\x1b[?25;2$y");
    // Bracketed paste (2004): default reset → 2.
    t.feed(b"\x1b[?2004$p");
    assert_eq!(t.take_output(), b"\x1b[?2004;2$y");
}

#[test]
fn decrqm_fixed_and_unknown() {
    let mut t = Term::new(80, 24);
    // SyncUpdate (2026): alacritty always reports Reset(2).
    t.feed(b"\x1b[?2026$p");
    assert_eq!(t.take_output(), b"\x1b[?2026;2$y");
    // ColumnMode (3): NotSupported(0).
    t.feed(b"\x1b[?3$p");
    assert_eq!(t.take_output(), b"\x1b[?3;0$y");
    // Unknown private mode → 0.
    t.feed(b"\x1b[?9999$p");
    assert_eq!(t.take_output(), b"\x1b[?9999;0$y");
    // Unknown ANSI mode → 0 (note: no `?`).
    t.feed(b"\x1b[99$p");
    assert_eq!(t.take_output(), b"\x1b[99;0$y");
}
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test -p vt-term decrqm_ 2>&1 | tail -20`
Expected: FAIL — no reply emitted.

- [ ] **Step 3: Implement DECRQM**

Add the helper near `device_attributes`:

```rust
    /// DECRQM — Request Mode (`CSI Ps $ p`, or `CSI ? Ps $ p` for private/DEC modes).
    /// Replies `CSI [?] Ps ; St $ y` where St is the DEC mode state: 0 not-recognised,
    /// 1 set, 2 reset (alacritty's `ModeState`, term/mod.rs:2387). We report the private
    /// modes vt-term actually tracks; every other mode replies 0 — matching the oracle
    /// wherever the oracle also has no state (see the plan's Task 3 design note for the
    /// deliberately-excluded modes).
    fn report_mode(&mut self, mode: u16, private: bool) {
        // DEC mode state: 1 = set, 2 = reset (bool→ModeState), 0 = not recognised.
        let st = |b: bool| if b { 1u8 } else { 2u8 };
        let state: u8 = if private {
            match mode {
                1 => st(self.app_cursor),
                6 => st(self.origin),
                7 => st(self.autowrap),
                25 => st(self.show_cursor),
                1004 => st(self.focus_events),
                1006 => st(self.mouse_sgr),
                1007 => st(self.alt_scroll),
                2004 => st(self.bracketed_paste),
                1049 => st(self.alt_screen()),
                2026 => 2, // SyncUpdate: alacritty always reports Reset
                _ => 0,    // ColumnMode(3), unknown, and untracked → NotSupported
            }
        } else {
            0 // vt-term tracks no ANSI modes yet (IRM/LNM unimplemented) → NotSupported
        };
        let marker = if private { "?" } else { "" };
        self.reply(format!("\x1b[{marker}{mode};{state}$y").as_bytes());
    }
```

In the intermediate early-return block add two arms (private must also carry the `$` intermediate):

```rust
                (Some(&b'?'), 'p') if intermediates.last() == Some(&b'$') =>
                    self.report_mode(p.first().copied().unwrap_or(0), true),
                (Some(&b'$'), 'p') =>
                    self.report_mode(p.first().copied().unwrap_or(0), false),
```

Confirm `self.alt_screen()` exists (it's used by the harness at `vtterm.rs:66`); if the field is named differently, use the same accessor the harness uses.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p vt-term decrqm_ 2>&1 | tail -20`
Expected: PASS (2 tests).

- [ ] **Step 5: Full vt-term suite**

Run: `cargo test -p vt-term 2>&1 | tail -5`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/vt-term/src/lib.rs
git commit -m "feat(vt-term): DECRQM (request mode) for tracked private modes"
```

---

### Task 4: Harness — `take_output` on VtEngine + oracle capture + DA2-mask comparator

**Files:**
- Modify: `crates/vt-conformance/src/lib.rs` (trait ~:138; add comparator helpers)
- Modify: `crates/vt-conformance/src/vendored.rs` (:14 `Noop`, :38 struct, :92 impl)
- Modify: `crates/vt-conformance/src/vtterm.rs` (:30 impl)
- Test: `crates/vt-conformance/src/lib.rs` `#[cfg(test)]` (or a small `tests/` file)

**Interfaces:**
- Consumes: `Term::take_output` (Task 1).
- Produces: `VtEngine::take_output(&mut self) -> Vec<u8>`; `pub fn reports_match(a: &[u8], b: &[u8]) -> bool` (byte-equal after masking any DA2 version field).

- [ ] **Step 1: Write the failing tests**

Add to `crates/vt-conformance/src/lib.rs` `#[cfg(test)]`:

```rust
#[test]
fn oracle_and_vtterm_agree_on_cpr() {
    use crate::{vendored::Vendored, VtEngine};
    let mut o = Vendored::spawn(80, 24);
    let mut v = <vt_term::Term as VtEngine>::spawn(80, 24);
    for e in [&mut o as &mut dyn VtEngine, &mut v as &mut dyn VtEngine] {
        e.feed(b"\x1b[3;5H\x1b[6n");
    }
    assert!(crate::reports_match(&o.take_output(), &v.take_output()));
}

#[test]
fn da2_version_is_masked() {
    // Same shape, different version → still a match.
    assert!(crate::reports_match(b"\x1b[>0;4001;1c", b"\x1b[>0;314;1c"));
    // Different structure → not a match.
    assert!(!crate::reports_match(b"\x1b[>0;1;1c", b"\x1b[?6c"));
}
```

(If `VtEngine` is not object-safe as `dyn`, feed each engine directly instead of the loop — keep the assertion.)

- [ ] **Step 2: Run to verify failure**

Run: `cargo test -p vt-conformance oracle_and_vtterm_agree_on_cpr da2_version 2>&1 | tail -20`
Expected: FAIL — `take_output`/`reports_match` undefined.

- [ ] **Step 3: Add `take_output` to the trait + the comparator**

In `crates/vt-conformance/src/lib.rs`, add to `trait VtEngine` (after `observe`):

```rust
    /// Drain the engine's pending host-bound reply bytes (query replies: DSR/CPR, DA,
    /// DECRQM). Empty when the last feed produced none. This is the query/report
    /// counterpart to `observe`: `observe` reads screen state, `take_output` reads what
    /// the engine would write back to the PTY.
    fn take_output(&mut self) -> Vec<u8>;
```

Add the comparator (module level):

```rust
/// Compare two reply streams for the differential. Byte-equal, EXCEPT the DA2 secondary-
/// device-attributes version field: `\x1b[>0;<version>;1c` embeds the emulator's own
/// version, which vt-term legitimately reports differently from the oracle — an
/// intentional, documented divergence (see docs/engine-divergence.md). We normalise that
/// one numeric field before comparing; everything else must match exactly.
pub fn reports_match(a: &[u8], b: &[u8]) -> bool {
    mask_da2(a) == mask_da2(b)
}

/// Replace the version field of any `\x1b[>0;<n>;1c` DA2 reply with a fixed sentinel.
fn mask_da2(s: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(s.len());
    let mut i = 0;
    let tag = b"\x1b[>0;";
    while i < s.len() {
        if s[i..].starts_with(tag) {
            // Find the terminating `;1c` and drop the digits between.
            if let Some(end) = s[i + tag.len()..].windows(3).position(|w| w == b";1c") {
                out.extend_from_slice(tag);
                out.extend_from_slice(b"V"); // version sentinel
                out.extend_from_slice(b";1c");
                i += tag.len() + end + 3;
                continue;
            }
        }
        out.push(s[i]);
        i += 1;
    }
    out
}
```

In `crates/vt-conformance/src/vtterm.rs`, add to the `impl VtEngine for vt_term::Term` block:

```rust
    fn take_output(&mut self) -> Vec<u8> {
        vt_term::Term::take_output(self)
    }
```

In `crates/vt-conformance/src/vendored.rs`, replace the `Noop` listener with a capturing one and drain it. The oracle's `Term<L>` runs single-threaded and synchronous here, so `Rc<RefCell<..>>` is sound:

```rust
use std::cell::RefCell;
use std::rc::Rc;
use alacritty_terminal::event::{Event, EventListener};

/// Captures the oracle's host-bound writes. `Term` calls `send_event(Event::PtyWrite(s))`
/// for query replies (DSR/CPR, DA, DECRQM); we append the bytes so the harness can diff
/// them against vt-term. All other events don't affect grid state or replies — dropped.
#[derive(Clone)]
struct Capture(Rc<RefCell<Vec<u8>>>);
impl EventListener for Capture {
    fn send_event(&self, event: Event) {
        if let Event::PtyWrite(text) = event {
            self.0.borrow_mut().extend_from_slice(text.as_bytes());
        }
    }
}
```

Change `struct Vendored` to `term: Term<Capture>` and add `out: Rc<RefCell<Vec<u8>>>`. In `spawn`:

```rust
        let out = Rc::new(RefCell::new(Vec::new()));
        let term = Term::new(config, &Dims { cols, rows }, Capture(out.clone()));
        Vendored { term, parser: ansi::Processor::new(), out }
```

Add to `impl VtEngine for Vendored`:

```rust
    fn take_output(&mut self) -> Vec<u8> {
        std::mem::take(&mut *self.out.borrow_mut())
    }
```

(Confirm the `Event` import path and the `PtyWrite(String)` variant against `crates/rt-engine/src/lib.rs:300`, which already matches on `AlacEvent::PtyWrite(text)`.)

- [ ] **Step 4: Run to verify pass**

Run: `cargo test -p vt-conformance oracle_and_vtterm_agree_on_cpr da2_version 2>&1 | tail -20`
Expected: PASS.

- [ ] **Step 5: Full conformance suite (existing strands still green)**

Run: `cargo test -p vt-conformance 2>&1 | tail -8`
Expected: all pass (the new trait method compiles against both engines; no existing test regresses).

- [ ] **Step 6: Commit**

```bash
git add crates/vt-conformance/src/lib.rs crates/vt-conformance/src/vendored.rs crates/vt-conformance/src/vtterm.rs
git commit -m "feat(vt-conformance): take_output seam + oracle reply capture + DA2-mask comparator"
```

---

### Task 5: Query/report differential fuzz strand

**Files:**
- Create: `crates/vt-conformance/tests/vtterm_report.rs`

**Interfaces:**
- Consumes: `VtEngine::{spawn,feed,resize,take_output}`, `reports_match`, and the existing seeded RNG + script generator in the crate (see `tests/vtterm_fuzz.rs` for the established pattern — reuse its RNG/`gen_script` imports verbatim).

- [ ] **Step 1: Read the sibling strand to match conventions**

Run: `sed -n '1,60p' crates/vt-conformance/tests/vtterm_fuzz.rs`
Note the exact imports (RNG type, `gen_script`/`split`, seed-loop shape, `feed_whole`/engine construction) so this strand mirrors them. Reuse them; do not invent a second RNG.

- [ ] **Step 2: Write the differential (this IS the test — it must fail if replies diverge)**

Create `crates/vt-conformance/tests/vtterm_report.rs`:

```rust
//! Query/report differential: interleave terminal queries (DSR/CPR, DA1/DA2, DECRQM)
//! with normal grid-mutating input, feed BOTH engines the identical stream, and assert
//! their host-bound reply streams match byte-for-byte (DA2 version masked). Ceiling: 0
//! divergences — a strict regression guard, run on x86_64 and riscv64 via ci/verify.sh.
//!
//! Scope note: the query pool is the set of modes vt-term and the oracle represent
//! identically (see docs/superpowers/plans Task 3 design note). Modes where they
//! legitimately differ (ANSI IRM/LNM, private 12/1005/1042, and the 1000/1002/1003 mouse
//! trio) are deliberately excluded until their own coverage slice.

use vt_conformance::{reports_match, VtEngine};

// Queries both engines answer identically. Setters (below) exercise Set/Reset states.
const QUERIES: &[&[u8]] = &[
    b"\x1b[6n",       // CPR
    b"\x1b[5n",       // DSR status
    b"\x1b[c",        // DA1
    b"\x1b[0c",       // DA1 (explicit 0)
    b"\x1b[>c",       // DA2 (version masked)
    b"\x1b[?1$p",     // DECRQM DECCKM
    b"\x1b[?6$p",     // DECRQM DECOM
    b"\x1b[?7$p",     // DECRQM DECAWM
    b"\x1b[?25$p",    // DECRQM DECTCEM
    b"\x1b[?1004$p",  // DECRQM focus events
    b"\x1b[?1006$p",  // DECRQM SGR mouse
    b"\x1b[?1007$p",  // DECRQM alt scroll
    b"\x1b[?2004$p",  // DECRQM bracketed paste
    b"\x1b[?1049$p",  // DECRQM alt screen
    b"\x1b[?2026$p",  // DECRQM sync update (always reset)
    b"\x1b[?3$p",     // DECRQM column mode (not supported)
    b"\x1b[?9999$p",  // DECRQM unknown private
    b"\x1b[99$p",     // DECRQM unknown ANSI
];

// Mode-changing / cursor-moving input, to vary the state the queries observe.
const MUTATORS: &[&[u8]] = &[
    b"\x1b[?6h", b"\x1b[?6l",          // DECOM on/off
    b"\x1b[?7l", b"\x1b[?7h",          // DECAWM off/on
    b"\x1b[?25l", b"\x1b[?25h",        // DECTCEM off/on
    b"\x1b[?1004h", b"\x1b[?1006h",    // focus / sgr mouse on
    b"\x1b[?2004h", b"\x1b[?1049h",    // bracketed paste / alt screen on
    b"\x1b[?1049l",                    // alt screen off
    b"\x1b[5;10H", b"\x1b[H", b"hello", b"\r\n", b"\x1b[2J",
];

// Tiny dependency-free xorshift, seeded per-iteration (matches the crate's RNG style;
// Date/rand are unavailable — determinism is required for reproducibility).
fn next(state: &mut u64) -> u64 {
    let mut x = *state;
    x ^= x << 13; x ^= x >> 7; x ^= x << 17;
    *state = x; x
}

#[test]
fn query_report_differential_matches_oracle() {
    use vt_conformance::vendored::Vendored;
    let iters = 4000;
    for seed in 0..iters {
        let mut rng = seed.wrapping_mul(0x9E37_79B9_7F4A_7C15).wrapping_add(1);
        // Build one script mixing mutators and queries.
        let mut script: Vec<u8> = Vec::new();
        let steps = 4 + (next(&mut rng) % 20) as usize;
        for _ in 0..steps {
            if next(&mut rng) % 2 == 0 {
                script.extend_from_slice(MUTATORS[(next(&mut rng) as usize) % MUTATORS.len()]);
            } else {
                script.extend_from_slice(QUERIES[(next(&mut rng) as usize) % QUERIES.len()]);
            }
        }
        let mut o = Vendored::spawn(80, 24);
        let mut v = <vt_term::Term as VtEngine>::spawn(80, 24);
        o.feed(&script);
        v.feed(&script);
        let (ro, rv) = (o.take_output(), v.take_output());
        assert!(
            reports_match(&ro, &rv),
            "seed {seed}: reply divergence\n script: {:?}\n oracle: {:?}\n vtterm: {:?}",
            String::from_utf8_lossy(&script), ro, rv,
        );
    }
}
```

Adjust `use` paths to whatever the crate actually exposes (`vendored::Vendored` may need to be `pub`; if it isn't, make the module/`Vendored` `pub` in `lib.rs` — the sibling fuzz strand already constructs it, so follow that import exactly).

- [ ] **Step 3: Run — expect PASS (implementations already match by construction)**

Run: `cargo test -p vt-conformance --test vtterm_report 2>&1 | tail -30`
Expected: PASS (0 divergences). If a seed fails, the panic prints the minimal-ish script + both replies — delta-debug it: shrink the script to the smallest failing prefix, compare vt-term's handler to the cited oracle function, and fix vt-term (never loosen the comparator except for a genuinely intentional, documented divergence).

- [ ] **Step 4: Full multi-arch verify**

Run: `bash ci/verify.sh 2>&1 | tail -25`
Expected: `==> ALL GREEN` — the new strand runs under `cargo test -p vt-conformance` on local + milkv + apollo automatically (no `verify.sh` edit needed).

- [ ] **Step 5: Commit**

```bash
git add crates/vt-conformance/tests/vtterm_report.rs crates/vt-conformance/src/lib.rs
git commit -m "test(vt-conformance): query/report differential vs oracle (0 ceiling)"
```

---

### Task 6: Runtime wiring — vtpane drains replies to the PTY

**Files:**
- Modify: `crates/rt-engine/src/vtpane.rs` (reader thread spawn ~:192-196, feed site ~:215-228)

**Interfaces:**
- Consumes: `Term::take_output` (Task 1); the existing `input_tx: Sender<Vec<u8>>` (:73/:163), `queued_bytes` (:76/:164), and `WRITE_QUEUE_MAX`.

- [ ] **Step 1: Add reply forwarding in the reader loop**

Before the reader thread is spawned (just after `let (term_r, events_r, dirty_r) = (…);` ~:192), clone the sender and counter for the reader:

```rust
        let reply_tx = input_tx.clone();
        let reply_queued = queued_bytes.clone();
```

Move them into the reader closure (they are captured by the `move ||` at :196). Inside the `Ok(n) =>` arm, extend the locked section to also drain replies, then forward after unlocking:

```rust
                        Ok(n) => {
                            let mut title = None;
                            let mut reply = Vec::new();
                            if let Ok(mut t) = term_r.lock() {
                                t.feed(&buf[..n]);
                                title = t.take_title(); // OSC 0/2 while holding the lock
                                reply = t.take_output(); // DSR/CPR, DA, DECRQM replies
                            }
                            // Answer terminal queries by writing straight back to the PTY,
                            // exactly as the vendored engine answers Event::PtyWrite
                            // (rt-engine lib.rs:300). Replies are tiny and MUST NOT be
                            // dropped (a swallowed CPR hangs the querying app), so unlike
                            // `write` this path skips the WRITE_QUEUE_MAX cap. It keeps the
                            // same queued_bytes accounting so the counter stays balanced.
                            if !reply.is_empty() {
                                reply_queued.fetch_add(reply.len(), Ordering::AcqRel);
                                let len = reply.len();
                                if reply_tx.send(reply).is_err() {
                                    reply_queued.fetch_sub(len, Ordering::AcqRel);
                                }
                            }
                            let mut q = events_r.lock().unwrap();
                            if let Some(t) = title {
                                q.push_back(PaneEvent::Title(t));
                            }
                            q.push_back(PaneEvent::Wakeup);
                            drop(q);
                            dirty_r.store(true, Ordering::Release);
                        }
```

Update the `write()` comment at :304-305 ("Single writer of `queued_bytes` — the GUI thread") to note the reader thread now also adds (atomically) for query replies, so the check-then-add in `write` may momentarily overshoot `WRITE_QUEUE_MAX` by a few reply bytes — benign and bounded.

- [ ] **Step 2: Build the workspace**

Run: `cargo build -p rt-engine 2>&1 | tail -15`
Expected: compiles clean (no unused-variable / borrow errors).

- [ ] **Step 3: Full rt-engine + rt tests**

Run: `cargo test -p rt-engine -p rt 2>&1 | tail -12`
Expected: all pass (including the existing OSC-title pane test at `rt-engine/src/lib.rs:1346`, which proves the reader-loop path still delivers events).

- [ ] **Step 4: Manual PTY smoke on dop651 (real child answers a query)**

Deploy per the rt-deploy memory (build + install to `~/.cargo/bin` and `/usr/bin`), then, in an rt pane under the default engine:

```bash
rt --version                       # confirm the new build
printf '\e[6n'; IFS= read -rsN 12 x; printf '%q\n' "$x"   # expect e.g. $'\E[1;NR' CPR
```

Also start `vim` and `tmux` in a pane (both issue DA/DSR handshakes on startup) and confirm they open and redraw cleanly. Sanity: `RT_ENGINE=alacritty rt` still behaves identically (no regression on the fallback).

- [ ] **Step 5: Commit**

```bash
git add crates/rt-engine/src/vtpane.rs
git commit -m "feat(rt-engine): answer terminal queries under the in-house engine"
```

---

### Task 7: Docs + project map

**Files:**
- Modify: `docs/vt-term-design.md`, `docs/engine-divergence.md`, `docs/own-engine-plan.md`, `project-map.js`

- [ ] **Step 1: vt-term design doc**

Add a "Query / report" section to `docs/vt-term-design.md`: the `output` buffer + `take_output` drain (parallel to `take_title`); the reply formats for DSR 5 / CPR 6, DA1 (`\x1b[?6c`) / DA2 (version), and DECRQM (the `$y` state reply with the 0/1/2 semantics and the tracked-mode table); and the "generated inline during `feed`, drained immediately after" timing rationale (so CPR reflects the cursor at parse time).

- [ ] **Step 2: Divergence ledger**

In `docs/engine-divergence.md`: move query/report out of "Known not-yet-implemented"; record the **DA2 version** as an *intentional, documented* divergence (masked in the comparator); note the new `vtterm_report` strand and its **0 ceiling**; and list the deliberately-excluded DECRQM modes (ANSI IRM 4 / LNM 20; private 12, 1005, 1042; mouse-report trio 1000/1002/1003) as known gaps awaiting the observable-state-edges slice.

- [ ] **Step 3: Own-engine plan**

In `docs/own-engine-plan.md`, mark the Phase-3 query/report forcing-function step as progressed (replies implemented + differentially verified; real `esctest` hookup still pending).

- [ ] **Step 4: Project map (CLAUDE.md standing order)**

In `project-map.js`: update the `vt-term` node's `parts[]` entry "OSC / DCS & query-report edges" — split or reword so query/report reads `done` while OSC/DCS stays `active` (e.g. rename to reflect query/report shipped, or add a distinct `done` part). Set `project.updated` to the implementation date. Keep all `deps` ids valid.

- [ ] **Step 5: Commit**

```bash
git add docs/vt-term-design.md docs/engine-divergence.md docs/own-engine-plan.md project-map.js
git commit -m "docs: query/report replies, ledger, and project-map sync"
```

---

## Self-Review

**Spec coverage:**
- Reply surfacing via drainable `take_output` (mirrors `take_title`) → Task 1. ✓
- Sequence set DA1/DA2/DSR5/CPR/DECRQM → Tasks 1–3, each matched to the cited oracle function. ✓
- DECRQM state table (0/1/2) + tracked-mode set → Task 3 with the exclusion design note. ✓
- Harness `take_output` on `VtEngine` + oracle `PtyWrite` capture + DA2-mask comparator → Task 4. ✓
- Differential fuzz strand, 0 ceiling, x86_64+riscv64 via `ci/verify.sh` → Task 5. ✓
- Runtime wiring in vtpane mirroring the vendored `Proxy::PtyWrite` handler → Task 6. ✓
- Docs (vt-term-design, ledger, own-engine-plan) + project-map sync → Task 7. ✓
- Out-of-scope items (DECRQCRA, esctest, pixel ops, OSC side-effects) → not implemented, and Task 7 records them as deferred. ✓

**Placeholder scan:** Task 3's helper intentionally shows an expanded-then-simplified form with an explicit instruction to keep only the concrete block — not a TBD. No other placeholders.

**Type consistency:** `take_output(&mut self) -> Vec<u8>` is identical across `Term` (Task 1), the `VtEngine` trait, and both impls (Task 4). `reply`/`device_status`/`device_attributes`/`report_mode` are all `Term` methods introduced in Tasks 1–3 and consumed only within `vt-term`. `reports_match`/`mask_da2` defined in Task 4, consumed in Tasks 4–5. `reply_tx`/`reply_queued` local to Task 6. CPR uses `self.row`/`self.col` (0-based fields, +1 on output) consistent with `csi_dispatch`'s existing `count(&p,0)-1` 1-based→0-based convention.

**Risk to watch during execution:** any DECRQM mode where vt-term and the oracle disagree will surface as a Task 5 seed failure — the fix is to correct vt-term (or, if genuinely intentional, document it in the ledger and exclude it from `QUERIES`), never to weaken `reports_match`.
