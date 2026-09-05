#!/bin/bash
# Two assertions that need no Mac and no macOS SDK:
#
#   1. The macOS dependency graph contains NO Wayland/X11 crates. This is the
#      regression that will recur -- someone adds an unconditional dep and
#      silently breaks macOS, which nothing else here would notice.
#   2. The Linux graph is unchanged against a committed baseline. This is what
#      makes "the macOS port cannot regress Linux" an assertion rather than a
#      promise.
#
# `cargo tree --target` only RESOLVES the graph, so both run on Linux.
# Paths are normalized to be independent of repo location (worktree, CI, machine).
set -uo pipefail
cd "$(dirname "$0")/.."
FAIL=0
BASE=ci/linux-dep-baseline.txt

# Get the repo root and normalize paths for comparison.
# This makes the check work from any checkout location (main, worktree, CI).
REPO_ROOT=$(git rev-parse --show-toplevel)
normalize_paths() {
  # Replace repo root path with (WORKSPACE) placeholder to make comparison location-independent.
  sed "s|${REPO_ROOT}|(WORKSPACE)|g"
}

echo "########## macOS graph: must be free of Wayland/X11 ##########"
mac=$(cargo tree --target aarch64-apple-darwin -p rt --edges normal 2>/dev/null \
      | grep -oE '(wayland|smithay|x11rb|glutin|khronos-egl)[a-z0-9_-]*' | sort -u)
if [ -n "$mac" ]; then
  echo "FAIL: these must not be in the macOS graph:"; echo "$mac" | sed 's/^/  /'
  FAIL=1
else
  echo "OK: no Wayland/X11/glutin crates on macOS"
fi

echo "########## Linux graph: must match the baseline ##########"
lin=$(cargo tree --target x86_64-unknown-linux-gnu -p rt --edges normal 2>/dev/null | sort -u)
lin_normalized=$(printf '%s\n' "$lin" | normalize_paths)
if [ ! -f "$BASE" ]; then
  printf '%s\n' "$lin_normalized" > "$BASE"
  echo "baseline created at $BASE -- commit it"
elif ! diff -q <(printf '%s\n' "$lin_normalized") "$BASE" >/dev/null; then
  echo "FAIL: the Linux dependency graph changed:"
  diff <(printf '%s\n' "$lin_normalized") "$BASE" | head -20
  echo "If this change is intended, update $BASE in the same commit."
  FAIL=1
else
  echo "OK: Linux graph unchanged"
fi

[ $FAIL = 0 ] && echo "ALL GREEN" || echo "FAILURES ABOVE"
exit $FAIL
