# Cross-instance pane & tab transfer — design

Move a pane or a whole tab from one **rt process** to another **rt process** on the
same machine — including between **different rt versions**, so a running Claude Code
session can be walked over to a newer build without dying.

Status: design. Nothing implemented yet.
Date: 2026-08-29.

## Goal

Today rt moves panes between *windows of one process* (`Session::extract_pane` →
`Session::adopt`, `crates/rt/src/main.rs:3765`). The pane's `TermPane` is simply moved
in memory; the PTY, the grid and the child never notice.

This design extends that to two independent rt processes, with three properties:

1. **The child survives.** The shell and everything under it — `claude`, `vim`, a
   build — keeps running, same pid, same pts, no SIGHUP, no restart.
2. **The screen survives.** Grid, scrollback (to a budget), modes, cursor, title.
3. **Versions may differ.** An rt 0.4.x window can adopt a pane out of an rt 0.3.20
   window, in either direction, forever.

Non-goal: cross-machine. See [Non-goals](#non-goals).

## What Linux gives us, and what it does not

These are load-bearing; everything below follows from them.

- **You cannot reparent a running process.** No syscall changes a live process's
  parent. `CLONE_PARENT` picks a parent only at `clone()` time; `ptrace` does not
  change PPID; `PR_SET_CHILD_SUBREAPER` only redirects *future* orphans. A process's
  PPID changes exactly once, when its parent dies.
- **You do not need to.** The shell already `setsid()`s with the PTY slave as its
  controlling terminal, so job control belongs to the *session*, not to rt. `SIGHUP`
  is sent when the **last master fd closes**, not when the parent dies. Whoever holds
  the master owns the terminal.
- **`SCM_RIGHTS` moves an fd between unrelated processes** over an `AF_UNIX` socket.
  This is the whole mechanism.
- **`pidfd_open()` gives a pollable death notification for any process**, not just
  your children (Linux 5.3+). `POLLIN` means it exited.
- **`waitid(P_PIDFD)` for the exit *status* still requires being the parent.** A
  moved pane therefore reports `Exited(None)` — "the pty closed but the child wasn't
  reaped by us" — which `rt-engine` already models (`crates/rt-engine/src/lib.rs:64`).

## Decisions (with Roland)

| Question | Decision |
|---|---|
| Scrollback volume | **Negotiated budget, default 50 000 lines**, oldest dropped first. |
| Entry points | **All four**: carry across instances, bulk migrate, keyboard picker, X11 XDND. |
| State representation | **Engine-neutral snapshot cells**, never engine internals. |
| Mode representation | **By DEC/ANSI mode number** — the spec's namespace, not rt's. |
| Colours | **Indexed stays indexed.** Never flattened to RGB. |

### Why not the tmux server model

The obvious alternative is to split rt into a session server owning PTYs+engines and
thin view clients; moving a pane becomes a re-attach, with no fd passing and no state
serialisation, and detach/reattach comes free. Rejected for now: it changes rt's
identity from "one process, many windows" into a daemon, it makes every pane's bytes
cross a socket on every frame, and it does not by itself solve the stated problem —
a *newer server* still cannot adopt an *older server's* panes without exactly the
wire format specified here. The daemon model remains open as a later, separate
decision; this design does not block it.

## Architecture

One new crate plus three seams.

```
crates/rt-handoff/          NEW. No GUI, no engine, no winit.
  wire.rs                   frozen v1 encode/decode (the compatibility contract)
  frame.rs                  framing + LEB128 + TLV skip
  sock.rs                   AF_UNIX, SCM_RIGHTS, SO_PEERCRED
  registry.rs               instance discovery in $XDG_RUNTIME_DIR/rt/

rt-engine   += TermPane::export(budget) -> PaneWire
            += TermPane::adopt(PaneWire, master_fd, pidfd) -> TermPane
            += Parser::pending_raw() -> &[u8]
            (both engines implement it; the wire type is neutral, so
             alacritty-engine → in-house-engine transfers work)
rt-session  += PaneWire / TreeWire, the serialisable twin of PanePackage
rt (main)   += registry registration, listener, offer broadcast, migrate,
               picker UI, XDND source/target
```

`rt-handoff` depends on nothing in rt except `libc`. That is deliberate: the
compatibility contract must be readable and testable without dragging in the GUI, and
future versions must be able to vendor an old copy of it to check themselves.

## Discovery and registry

`$XDG_RUNTIME_DIR/rt/` (fallback `/run/user/$UID/rt`, then `$TMPDIR/rt-$UID`), mode
`0700`. Each instance on start:

- binds `inst-<pid>-<rand>.sock`, mode `0600`;
- writes `inst-<pid>-<rand>.json`: pid, rt version, `proto_min`/`proto_max`, engine
  name, display server, boot id, window titles (refreshed lazily);
- holds an `flock` on the json for its lifetime.

Discovery = read the dir, skip entries whose flock is free (stale) and unlink them,
`connect()` to the rest. `ECONNREFUSED` also means stale → unlink.

The socket is the liveness test; the json is only metadata for the picker.

## Handshake and security

On accept, and before any state moves:

1. `SO_PEERCRED`: peer uid must equal our uid, else close silently. Cross-user
   transfer is never supported.
2. Both sides send `Hello` (magic, `proto_min`, `proto_max`, `rt_version`, engine,
   boot id, capability tags, `max_scrollback_lines`, `max_payload_bytes`).
3. Negotiated protocol = `min(proto_max_a, proto_max_b)`; abort if it is below either
   `proto_min`, with an error naming **both rt versions** so the user sees
   "rt 0.3.20 and rt 0.9.0 share no handoff protocol", not a generic failure.
4. Boot id mismatch → abort (a stale socket from a previous boot).
5. A `Claim` must carry the 128-bit token minted for that offer. Tokens are single-use
   and expire with the carry.

The token exists so that any *other* local process of the same user cannot walk up to
an rt socket and ask for a pane. It is not a defence against the user's own uid — that
boundary does not exist on Linux and this design does not pretend otherwise.

## The transfer protocol

Two-phase commit. **D** = donor (holds the pane), **R** = receiver (adopts it).

```
R → D   Hello                          (and D → R Hello)
D → R   Offer      { token, pane count, titles, byte estimate }
R → D   Claim      { token, drop target, accepted budget }
D       FREEZE: stop the reader thread at a parse boundary, drain the PTY into
               the Term, capture Parser::pending_raw()
D → R   Tree       { TreeWire }
        for each pane:
D → R     PaneState { ... }            (metadata, modes, cursor, grids)
D → R     PaneFds   { pane_uid }       + SCM_RIGHTS[master_fd, pidfd]
D → R     ScrollChunk*                 (newest-first, until budget or exhausted)
R       build panes, resize to the target rect, render one frame
R → D   Adopted    { pane_uids }   |   Failed { code, text }
D       on Adopted → DISARM and drop; on Failed/EOF/timeout → THAW and keep
```

`Failed` codes, frozen with the rest of v1: 1 protocol mismatch · 2 peer credentials
rejected · 3 bad or expired token · 4 payload over the accepted budget · 5 malformed
frame · 6 fd passing failed · 7 receiver could not place the tree at the drop target ·
8 internal error. Each carries human text for the notice; a receiver that meets an
unknown code shows the text and treats it as 8.

**Failure is safe by construction.** D holds everything until `Adopted`; R holds
everything it needs before it sends `Adopted`. If R dies mid-stream, D thaws and the
pane never blinks. If D dies after sending the fds but before receiving `Adopted`, R
already owns master + pidfd + state and commits anyway. The only lossy case is both
dying inside the same window, which loses no more than a crash already does.

**Freeze/thaw** is not optional. Bytes already read into D's userspace buffer but not
yet parsed would otherwise be lost, and the parser may be mid-sequence. Freeze drains
to a parse boundary and captures any incomplete sequence as **raw bytes**
(`Parser::pending_raw()`), which R replays into its own fresh parser — version-neutral,
because raw VT bytes mean the same thing in every build. Bytes the child writes after
the freeze simply sit in the kernel's pty buffer and are read by R.

**One fd message per pane.** `SCM_RIGHTS` is capped (`SCM_MAX_FD`, 253) and a bulk
migration can exceed it; `PaneFds` carries exactly two fds and is keyed by `pane_uid`.

### The child after the move

- D **disarms**: the `Pty` must not run its `Drop` (it SIGHUPs and reaps —
  `crates/rt-engine/src/vtpane.rs:55-58`). Disarm = take the raw fd and
  `mem::forget` the handle.
- D remains the child's parent while it lives. It records the pid in a `detached` set
  and reaps it with `waitpid(WNOHANG)` **silently**, so no zombies accumulate and no
  `Exited` event is raised for a pane it no longer owns. When D exits, the child
  reparents to init.
- R detects death by **polling the pidfd**, with master EOF as a backstop. Status is
  unavailable → `Exited(None)`. This is exactly the case rt already handles, and it is
  the reason the pidfd is passed at all: master EOF alone is unreliable, because a
  backgrounded grandchild can hold the slave open.
- The CPU/heat instrument keys on the **session leader pid**, not on parentage, so it
  keeps working across the move and after D exits.

## Wire format v1

The compatibility contract. Everything else in this document can change; this cannot.

### Framing

```
Frame := u32 payload_len (LE) | u16 msg_type | u16 msg_flags | payload[payload_len]
```

`payload_len` ≤ 64 MiB. All multi-byte integers outside grid blobs are LEB128
unsigned varints; fixed-width fields are little-endian.

| type | message | direction |
|---|---|---|
| 0x01 | Hello | both |
| 0x02 | Offer | D → R |
| 0x03 | Claim | R → D |
| 0x04 | Tree | D → R |
| 0x05 | PaneState | D → R |
| 0x06 | PaneFds (carries SCM_RIGHTS) | D → R |
| 0x07 | ScrollChunk | D → R |
| 0x08 | Adopted | R → D |
| 0x09 | Failed | either |
| 0x0A | Cancel | R → D |
| 0x0B | Ping | either |
| 0x0C | Pong | either |
| 0x0D | Bye | either |

### TLV bodies

`Hello`, `Offer`, `Claim` and `PaneState` — the messages that negotiate and
describe, and so the ones that will grow — carry TLV bodies. `Tree`,
`ScrollChunk` and the grid encodings use the explicit positional grammars given
below, because they are dense, repeated structures whose shape is fixed by what
a terminal is; the frame header's length still lets a receiver skip one whole.
`PaneFds`, `Adopted`, `Failed`, `Cancel`, `Ping`, `Pong` and `Bye` are small and
frozen.

A TLV body is a sequence of fields:

```
Field := varint tag | varint len | value[len]
```

Compatibility rules, binding on every future version:

- **R1** A tag is never reused, renumbered, or given a new meaning. Tags are only
  ever added.
- **R2** An unknown tag is skipped by its length. Never an error.
- **R3** An absent tag means the documented default, listed below.
- **R4** A v1-or-later donor always emits every field marked *required*.
- **R5** A field's *meaning* never depends on the negotiated version; only its
  presence does.
- **R6** A receiver that skips a field the user would notice surfaces one line in the
  pane: `3 fields not understood by rt 0.3.20: columns, image_table, kitty_kbd`. Never
  silent. A receiver cannot name a tag it has never heard of, so the donor ships the
  names: field `0x0F tag_names` maps every tag it emitted to a short name, and the
  decoder reports the skipped tags so the caller can look them up. A skipped tag with
  no name is reported by number.
- **R7** An encoder emits fields in **ascending tag order**; a decoder accepts any
  order. This makes the encoding canonical, which is what lets the golden corpus be
  compared byte-for-byte rather than semantically.

### `PaneState` fields

| tag | field | req | default |
|---|---|---|---|
| 0x01 | `pane_uid` u64 — donor-local, correlates `PaneFds`/`ScrollChunk` | ● | — |
| 0x02 | `title` utf8 | ● | shell name |
| 0x03 | `cwd` utf8 (OSC 7, else `/proc/<pid>/cwd`) | | none |
| 0x04 | `cols` varint | ● | — |
| 0x05 | `rows` varint | ● | — |
| 0x06 | `scrollback_limit` varint | | receiver default |
| 0x07 | `columns_count` varint — newspaper columns | | 1 |
| 0x08 | `group` varint | | none |
| 0x09 | `broadcast` u8 | | off |
| 0x0A | `child_pid` varint — informational (instruments) | ● | — |
| 0x0B | `shell_argv` — list of utf8 | | none |
| 0x0C | `env_extras` — list of (utf8, utf8) | | none |
| 0x0D | `show_titlebar` u8 | | receiver pref |
| 0x0E | `palette` — 256 × RGB, only if OSC-4 overridden | | receiver palette |
| 0x0F | `tag_names` — list of (varint tag, utf8 short name) for every tag emitted, so a receiver can name what it skipped (R6) | ● | — |
| 0x20 | `modes` — see below | ● | all defaults |
| 0x21 | `cursor` { col, row, shape, visible, blink, pending_wrap } | ● | — |
| 0x22 | `saved_cursor` (DECSC: pos, pen, charsets, origin) | | none |
| 0x23 | `charsets` — G0..G3 designators + GL/GR locks | | ASCII |
| 0x24 | `tab_stops` — bitmap over `cols` | | every 8 |
| 0x25 | `margins` { top, bottom, left, right } | | full screen |
| 0x26 | `title_stack` — list of utf8 (XTPUSHTITLE) | | empty |
| 0x27 | `pen` — the current SGR state, as a Style entry | ● | default |
| 0x28 | `active_screen` u8 — 0 primary, 1 alt | | 0 |
| 0x29 | `kitty_kbd` — keyboard protocol flag stack, modifyOtherKeys level | | off |
| 0x2A | `pending_raw` — bytes of an incomplete sequence, replayed by R | | empty |
| 0x3F | `style_table` — see below | ● | — |
| 0x40 | `screen_primary` — GridBlob | ● | — |
| 0x41 | `screen_alt` — GridBlob | | absent |
| 0x50 | `uri_table` — list of (varint id, utf8 uri), OSC 8 | | empty |
| 0x51 | `image_table` — list of (id, format, w, h, bytes) | | empty (v1 may omit) |

**Modes (0x20)** are the version-independence trick. A list of entries:

```
ModeEntry := varint kind (0 = ANSI, 1 = DEC private) | varint number | u8 value
```

`number` is the mode number **from the spec** — 1 DECCKM, 4 IRM, 6 DECOM, 7 DECAWM,
20 LNM, 25 DECTCEM, 1000/1002/1003/1006 mouse, 1049 alt screen, 2004 bracketed paste,
2026 synchronised update. Any rt build, of any age, understands the ones it implements
and skips the rest by rule R2, because the namespace belongs to the VT spec rather
than to rt's internal enum ordering.

### Style table (0x3F) and colours

```
Style  := Colour fg | Colour bg | Colour underline | varint attrs | varint link_id
Colour := u8 kind | u8 a | u8 b | u8 c
          kind 0 = default (a,b,c ignored)
          kind 1 = indexed  (a = index, b,c = 0)
          kind 2 = rgb      (a,b,c = r,g,b)
```

`kind 1` is why indexed colours are not flattened: a moved pane must still follow the
receiver's colour scheme and live `set_all_palettes` changes.

`attrs` bits, frozen at v1: 1 bold · 2 dim · 4 italic · 8 underline · 16
double-underline · 32 curly · 64 dotted · 128 dashed · 256 blink · 512 rapid-blink ·
1024 reverse · 2048 hidden · 4096 strikeout · 8192 overline. Higher bits are reserved;
a receiver masks off bits it does not know.

### Lines, runs and grids

```
GridBlob := varint row_count | Line × row_count
Line     := varint line_flags | varint run_count | Run × run_count
Run      := u8 run_flags | varint style_id | varint cell_span
            | varint text_len | text[text_len]
            | [if run_flags & 1: varint char_count × cell_span]
```

- `line_flags`: bit0 soft-wrapped into the next line · bit1 DECDWL · bit2 DECDHL top ·
  bit3 DECDHL bottom.
- `run_flags`: bit0 per-cell char counts follow (combining marks) · bit1 wide-char run
  (every cell spans 2 columns).
- `cell_span` is measured in **columns**. The run's cell count is `cell_span` normally
  and `cell_span / 2` when bit1 is set; the bit0 array has exactly that many entries.
  A run never mixes wide and narrow cells — it breaks instead.
- `text_len == 0` means `cell_span` blank cells in `style_id` — the common case.
- Trailing default-blank cells are omitted; a short line is padded by the receiver.

A typical 200-column shell line is one run: a few dozen bytes. 50 000 lines lands
around 1–3 MB, which is why v1 specifies no compression. A `compressed` frame flag is
reserved for later without a format change.

### Scrollback (0x07 `ScrollChunk`)

```
ScrollChunk := varint pane_uid | u8 more | varint line_count | Line × line_count
```

Sent **newest-first**, so a cancelled or budget-truncated transfer keeps the history
the user actually cares about. R prepends each chunk above what it already has. R
stops reading at `min(donor_has, receiver_max, negotiated_budget)` and sends `Cancel`;
D stops cleanly.

### Tree (0x04) — tabs and bulk migration

```
Node := varint kind
        kind 0 leaf   : varint pane_uid
        kind 1 hsplit : u32 ratio_bits (f32 LE) | varint n | Node × n
        kind 2 vsplit : u32 ratio_bits (f32 LE) | varint n | Node × n
        kind 3 tabs   : varint active | varint n | (utf8 title | Node) × n
```

One `Tree` describes everything in the transfer: a single pane is a one-leaf tree; a
tab is a subtree; a bulk migration is the donor's whole forest.

## What does not survive a move

Written down so it is a decision and not a bug report:

- Scrollback beyond the negotiated budget (oldest dropped first).
- The child's **exit status** — `Exited(None)` thereafter.
- Selection, search state, and instrument history (deliberately dropped).
- Inline images if the donor's v1 omits `image_table`; those cells render blank.
- The child's environment still names the donor's `$RT_OUT`/`$RT_IN` paths — a
  running process's environment cannot be rewritten. See [Patch-bay](#patch-bay).
- Patch-bay wire *rendering* between panes that now live in different processes.
- Fields the receiver is too old to understand (surfaced per rule R6).

## Patch-bay

Jack FIFOs move from a per-process temp dir to
`$XDG_RUNTIME_DIR/rt/jacks/<pane-uuid>/`, so they outlive the donor and the paths
already baked into the child's environment keep resolving. A moved pane keeps its own
jacks and its wires to panes that moved with it. Wires whose other end stayed behind
are dropped from the wire list with a one-line notice: the FIFOs still function, but
no single window can draw a wire to a rect it does not own.

## Entry points

All four, in this order of implementation:

1. **Carry across instances.** `Ctrl+Shift+M`/`N` picks up as today; the offer is
   broadcast to every registry instance; a click in any rt window claims it. Works
   identically on Wayland and X11 because it never touches a DnD protocol. This is the
   baseline and the fallback for the other three.
2. **Bulk migrate.** `rt migrate --from <instance>` and a menu item: move *every* tab
   and pane out of another instance, layout tree intact, in one gesture. This is the
   one that serves "walk my live Claude sessions onto the new build".
3. **Keyboard picker.** A key opens a list of running instances (version + titles);
   pick one and the focused pane or tab goes there. No pointer.
4. **X11 XDND.** Extends the existing live drag across processes with a private MIME
   type `application/x-rt-pane-offer` whose payload is `{socket path, token}`; the drop
   then runs the ordinary protocol over the socket. X11 only (incl. `ssh -X`).

## Non-goals

- **Rescuing panes from builds that predate the protocol.** Both ends must speak it:
  the donor holds the fd and the grid in its own memory, so it has to cooperate. The
  oldest rt a pane can ever be moved *out of* is the first release that ships this.
  Sessions running in an older build stay there until they end. (An escape hatch using
  `pidfd_getfd(2)` was specified and then cut — it needed `CAP_SYS_PTRACE` under
  `yama/ptrace_scope = 1`, killed the source process, and lost all scrollback.)
- **Cross-machine.** An fd cannot cross a network, and the child cannot follow. A
  future "serialise and respawn" mode is a different feature with different promises.
- **The daemon/server model.** Discussed above, deliberately deferred.
- **Cross-user transfer.** Refused at `SO_PEERCRED`.
- **Detach/reattach persistence.** rt panes still die with their last holder.
- **Compression, in v1.** Reserved flag, no implementation.

## Testing

The protocol's whole value is that it still works against a build that does not exist
yet, so most of the testing budget goes there.

- **Round-trip property tests.** Generate grids with the differential harness's
  existing generators; encode → decode → compare **cell-for-cell**, the same standard
  the engine is already held to.
- **Golden corpus.** `crates/rt-handoff/tests/fixtures/wire-v1/*.bin`, committed. Every
  future version must decode them to identical grids. This is the cross-version
  guarantee, testable in CI with a single build.
- **Unknown-tag injection.** Splice synthetic unknown tags at every nesting level into
  a fixture; the decoder must skip them and produce a byte-identical grid.
- **Two-process integration**, on the existing Xvfb harness (`crates/rt/tests/common`):
  run `cat` in a pane, move it, type, expect the echo. Assert the child pid and the
  pts path are unchanged.
- **Death detection.** Kill the child after a move; assert the receiver reports
  `Exited(None)` from the pidfd, not from EOF.
- **Failure injection.** Kill D and R at each protocol phase; assert exactly one side
  ends up owning the pane, never zero and never both.
- **fd leak check.** Count `/proc/self/fd` before and after a move on both sides.
- **Zombie check.** After a move, D must accumulate no zombies over 100 moves.

## Delivery

Five phases, each independently useful and independently reviewable.

1. **`rt-handoff` wire v1** — encode/decode, round-trip tests, golden corpus. No GUI,
   no sockets. The compatibility contract lands first and alone, because it is the one
   thing that can never be changed later.
2. **Engine export/adopt** — `TermPane::export`/`adopt` and `Parser::pending_raw()` on
   both engines; freeze/thaw; fd passing. Headless two-process integration test.
3. **Registry, handshake, two-phase commit, carry across instances.** The feature
   becomes usable here.
4. **Bulk migrate + keyboard picker.** The migration workflow.
5. **X11 XDND across processes.**

Phase 1 gates everything. Phases 4 and 5 are independent of each other.
