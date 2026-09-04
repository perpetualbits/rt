# Version-stamp fix — verification report

## Root-cause verification

Read `crates/rt/build.rs` before changing anything. Confirmed both claims:

1. `git_desc()` shells out to `git rev-parse --short HEAD` and returns `String::new()`
   on any failure (missing git, not a repo, or a `.git` file whose `gitdir:` target
   doesn't exist on this machine — exactly the rsynced-worktree case). `version_string()`
   in `crates/rt/src/main.rs` treats an empty `RT_GIT_DESC` the same as an absent one
   (`Some(g) if !g.is_empty()`), so the result is silently `rt 0.3.19` with no signal
   that a stamp was ever intended.
2. The `rerun-if-changed` loop watches `../../.git/HEAD` and `../../.git/index`,
   assuming `.git` is a directory. In this worktree it is not:
   `git rev-parse --git-dir` → `/home/roland/git/rt/.git/worktrees/version-stamp`,
   `--git-common-dir` → `/home/roland/git/rt/.git`. Neither `../../.git/HEAD` nor
   `../../.git/index` exists here, so the old code watched nothing and cargo would
   never re-run build.rs across a HEAD move — matching the prior "stale hash" incident
   referenced in the task.

## Changes

- `crates/rt/build.rs`: honor `RT_GIT_DESC` from the environment verbatim if set
  (skips git entirely, emits `cargo:rerun-if-env-changed=RT_GIT_DESC`); `git_desc()`
  now returns `"unknown"` instead of `""` on any git failure; `rerun-if-changed` paths
  are resolved via `git rev-parse --git-dir` / `--git-common-dir` instead of assumed
  relative paths, with a fallback to watching nothing if git is unavailable (unchanged
  fail-safe behavior).
- `crates/rt/src/main.rs`: updated `version_string()`'s doc comment to reflect that
  `RT_GIT_DESC` is now always set by build.rs (to a hash, an override, or `unknown`);
  no logic change needed there — `unknown` is non-empty so it already renders as
  `rt 0.3.19 (unknown)`.

## Verification output

**Normal build (this worktree, git present):**
```
rt 0.3.19 (bb5821a-dirty)
```
(`-dirty` is correct: this worktree has the uncommitted fix itself.)

**`RT_GIT_DESC` override:**
```
$ RT_GIT_DESC=deadbee cargo build -p rt && ./target/debug/rt --version
rt 0.3.19 (deadbee)
```

**No-git tree (the actual reported bug)** — rsynced this worktree to
`/tmp/rt-nogit-test` excluding `.git`, built there:

- With the *original* build.rs (pre-fix), copied into the scratch tree:
  ```
  rt 0.3.19
  ```
  No parenthetical at all — indistinguishable from a release build that never had a
  stamp, exactly what the user hit on the Mac.
- With the *fixed* build.rs:
  ```
  rt 0.3.19 (unknown)
  ```

Scratch directory removed after the test (`rm -rf /tmp/rt-nogit-test`).

`cargo test -q -p rt --bin rt`: 132 passed, 0 failed.
`cargo clippy -p rt`: no `error` lines (pre-existing unrelated warnings only, e.g.
`unnecessary_lazy_evaluations`, `unnecessary_map_or`, `too_many_arguments` — none
touch build.rs or the version-stamp code).

## Marker decision: `unknown`, not empty

Checked every caller of `version_string()` (`chrome/manual.rs`, `menu.rs`,
`chrome/prefs.rs`, `crashlog.rs`, `main.rs --version`) and searched the tree for
anything parsing the string — nothing parses it; it is display-only everywhere it's
used. That makes the choice purely about what's most useful on a bug report:

An empty stamp is indistinguishable from "no stamp was ever built" — a maintainer
reading `rt 0.3.19` in a bug report cannot tell whether this is a from-source build
whose stamping *failed* (a real defect worth fixing) or a deliberately bare release
build. `(unknown)` is unambiguous: it says a stamp was attempted, git wasn't usable at
build time, and the maintainer should ask the reporter how they got the source (tarball?
rsync? which machine?) rather than assume the crate version alone is enough to locate
the commit. Since nothing parses the string, there's no compatibility cost to changing
its shape, so I went with the more informative option.

## `rerun-if-changed` in a worktree — what I found

`.git` in a worktree is a one-line file (`gitdir: /path/to/main/.git/worktrees/<name>`),
not a directory, so `Path::new("../../.git/HEAD").exists()` is false and the old code
silently watched nothing (the `if Path::new(p).exists()` guard swallowed the miss
without any diagnostic). `git rev-parse --git-dir` returns the worktree's *own* git dir
(`.../worktrees/<name>`, where per-worktree `HEAD` lives) while `--git-common-dir`
returns the shared main repo `.git` (where the shared `index` lives) — a plain checkout
returns the same value for both, so using both calls handles worktree and non-worktree
layouts uniformly. Both resolved paths were confirmed to exist in this worktree.
