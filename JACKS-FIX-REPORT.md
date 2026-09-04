# jacks-dir not idempotent — fix report

## The bug

`ensure_jacks_dir` treats *every* `EEXIST` on `mkdir(rt-<pid>)` as "stale dir
from a crashed prior run that reused this pid" and wipes it with
`remove_dir_all`. But `build_active` calls it once per **window**, all within
the same live process. On the second window (e.g. a tear-off / `DetachPane`
opening a new window), the dir already exists because *we* created it for the
first window — so the "stale dir" branch fires and deletes every live pane's
fifos, including panes that never moved. Those paths are baked into each
child shell's environment at spawn, so a running pane can never recover them.

`build_active`'s doc comment asserted `ensure_jacks_dir` "is idempotent" —
false; it is destructive on every call after the first per process.

## The fix

Added a process-wide `static JACKS_DIR_ENSURED: AtomicBool`. `ensure_jacks_dir`
reads it at the top of the call to decide whether this is the process's first
call for the dir. The ownership/mode validation (`lstat`, dir check, uid
check, mode check) still runs on **every** call — I did not skip that, since
the directory could in principle be tampered with mid-run. Only the
"wipe-and-recreate" step is now gated: it only executes when `first_call` is
true; a later call that passes validation just returns `Ok(())` without
touching the directory's contents.

### Flag placement: set after success, never before

The flag is stored only at the two points where `ensure_jacks_dir` is about
to return `Ok(())` having established the directory:
1. Immediately after a fresh `mkdir()` succeeds (the common case).
2. Immediately after the wipe-and-recreate `mkdir()` succeeds (the stale-dir
   case).

It is never set speculatively before an attempt. If it were set *before* the
first `mkdir`, and that `mkdir` then failed for a reason unrelated to
`EEXIST` (e.g. base dir missing), a later call in the same process would see
the flag already true and skip validation-triggered wiping for what might
actually still be a genuinely stale directory nobody has cleaned up yet —
wrong, and pointless caution for no benefit. Setting it only after success
ties "we consider our dir first-established" to it actually *being*
established.

### First-call failure, then a later window calls again

If the first call fails (patch-bay disabled — validation failed, or the
wipe/recreate `mkdir` raced and lost), the flag is left `false`, on purpose.
So when a second window's `build_active` later calls `ensure_jacks_dir` again
for the same dir, it is treated as a fresh "first call" and gets the full
original handshake (mkdir, or on EEXIST validate-then-wipe-then-recreate)
again. This is correct: nothing was ever successfully created or wiped by the
failed attempt, so there is nothing yet to protect from a second wipe, and
retrying gives a transient failure (e.g. a momentary permissions/race issue)
a chance to succeed on the second window instead of being permanently wedged
for the rest of the process's life. If the underlying cause is persistent,
the second call just fails again the same way — no worse than today.

## Comments fixed

- `ensure_jacks_dir`'s own doc comment now says the wipe only ever runs on
  the process's first call, and why (only the first "already exists" can
  mean a crashed prior run's leftover — every later one is our own live
  dir).
- `build_active`'s doc comment no longer claims `ensure_jacks_dir` "is
  idempotent" as a blanket property; it now explains that the wipe is
  gated to the first call so opening additional windows doesn't destroy
  already-spawned panes' fifos.

## Regression test

`jacks_dir_tests::second_call_in_same_process_does_not_wipe_live_fifos` in
`crates/rt/src/main.rs`:

1. Builds a unique path under `std::env::temp_dir()`, namespaced by pid and a
   nanosecond timestamp, so concurrent test runs (or this repo's other
   `#[cfg(test)]` code, which never touches this dir) cannot collide with it.
   `remove_dir_all` first, ignoring errors, in case a prior aborted run left
   it behind.
2. Calls `ensure_jacks_dir(&dir)` once (this is a path that has never existed
   in this process before, so it is unconditionally a fresh `mkdir` — this
   step is meaningful regardless of the global flag's state going in, which
   is what keeps the test non-vacuous: see below).
3. Creates a fifo inside it with the file's own `mkfifo` helper, standing in
   for a pane's `$RT_IN` jack the way `Jacks::new` would create it.
4. Calls `ensure_jacks_dir(&dir)` a second time, same process, same path —
   the tear-off / second-window scenario.
5. Asserts the fifo still exists.
6. Cleans up the temp dir unconditionally at the end.

### Test isolation reasoning

The fix's whole mechanism is a *process-wide* static, so by design the two
calls have to happen in the same test binary process for the gate to do
anything — which is exactly the real-world scenario (one `rt` process, two
windows). The risk called out in the task is that this could make the test
accidentally pass for the wrong reason if some earlier test (or test
ordering) had already flipped `JACKS_DIR_ENSURED` to `true` before this test
even ran its own "first" call — then the test's first call wouldn't exercise
the mkdir-fresh path at all.

That risk doesn't materialize here because the test's own step 2 is a fresh
`mkdir` on a path that is guaranteed never to have existed before (unique
per-run name) — `mkdir()==0` succeeds unconditionally on a fresh path
regardless of the static flag's prior value, and unconditionally sets the
flag to `true` as a side effect. So by the time step 4 runs, the flag is
guaranteed `true` in this process, whatever it was before step 2. Conversely,
before the fix there was no flag at all, and the EEXIST branch always wiped
regardless of any global state — so the test fails identically on the
unfixed code no matter what other tests ran first. Either way, the test's
result is determined by the behavior under test, not by execution order.

I did not need `#[serial]`/mutex-based test serialization: no test in this
file exercises `ensure_jacks_dir` concurrently on the *same* path, and
concurrent calls on different paths don't interact except through the shared
`AtomicBool`, whose only effect (as reasoned above) is monotonic
false→true and doesn't change this test's outcome either way.

## RED run (test added, fix not yet applied)

```
$ cargo test -q -p rt --bin rt jacks_dir_tests
running 1 test
jacks_dir_tests::second_call_in_same_process_does_not_wipe_live_fifos --- FAILED

failures:

---- jacks_dir_tests::second_call_in_same_process_does_not_wipe_live_fifos stdout ----

thread 'jacks_dir_tests::second_call_in_same_process_does_not_wipe_live_fifos' (2037904) panicked at crates/rt/src/main.rs:7547:9:
second ensure_jacks_dir call wiped a live fifo
note: run with `RUST_BACKTRACE=1` environment variable to display a backtrace


failures:
    jacks_dir_tests::second_call_in_same_process_does_not_wipe_live_fifos

test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 131 filtered out; finished in 0.00s
error: test failed, to rerun pass `-p rt --bin rt`
```

## GREEN run (after the fix)

```
$ cargo test -q -p rt --bin rt jacks_dir_tests
running 1 test
.
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 131 filtered out; finished in 0.01s
```

Full suite, build, and clippy after the fix:

```
$ cargo test -q -p rt --bin rt
running 132 tests
....................................................................................... 87/132
.............................................
test result: ok. 132 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s

$ cargo build -p rt
   Compiling rt v0.3.19 (.../crates/rt)
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 3.84s

$ cargo clippy -p rt 2>&1 | grep -E "^error|^warning: unused"
(no output)
```
