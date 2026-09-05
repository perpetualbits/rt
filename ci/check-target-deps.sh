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
# Resolve the graph FIRST and check cargo actually succeeded. Piping cargo's
# stderr to /dev/null and grepping the result means a cargo failure yields an
# empty string, no match, and a confident "OK" over no data at all -- the same
# silent-green shape that hid the missing riscv64 coverage on milkv.
if ! mac_tree=$(cargo tree --target aarch64-apple-darwin -p rt --edges normal 2>&1); then
  echo "FAIL: cargo tree could not resolve the macOS graph -- this gate checked NOTHING:"
  printf '%s\n' "$mac_tree" | tail -5 | sed 's/^/  /'
  FAIL=1
  mac_tree=""
  mac_resolved=0
else
  mac_resolved=1
fi
mac=$(printf '%s\n' "$mac_tree" \
      | grep -oE '(wayland|smithay|x11rb|glutin|khronos-egl)[a-z0-9_-]*' | sort -u)
if [ -n "$mac" ]; then
  echo "FAIL: these must not be in the macOS graph:"; echo "$mac" | sed 's/^/  /'
  FAIL=1
elif [ "$mac_resolved" = 1 ]; then
  echo "OK: no Wayland/X11/glutin crates on macOS"
fi

echo "########## Lean build: --no-default-features must stay X11-free ##########"
# README advertises `cargo install --path crates/rt --no-default-features` as
# "lean, Wayland-only (zero X11 crates)". Nothing checked that, and the macOS
# port quietly broke it by making arboard unconditional -- arboard depends on
# x11rb on Linux, so four X11 crates entered the build README calls X11-free.
# The two checks above resolve the DEFAULT feature set only and cannot see it.
#
# `xcursor` is deliberately NOT in this list: it arrives via wayland-cursor and
# parses Xcursor THEME FILES, which Wayland uses too. It is not an X11 client.
if ! lean_tree=$(cargo tree -p rt --no-default-features --edges normal 2>&1); then
  echo "FAIL: cargo tree could not resolve the lean graph -- this gate checked NOTHING:"
  printf '%s\n' "$lean_tree" | tail -5 | sed 's/^/  /'
  FAIL=1
else
  lean=$(printf '%s\n' "$lean_tree" \
         | grep -oE '\b(x11rb|x11rb-protocol|as-raw-xcb-connection|arboard|x11-dl|xcb)\b' | sort -u)
  if [ -n "$lean" ]; then
    echo "FAIL: --no-default-features must not pull X11 crates, but it pulls:"
    printf '%s\n' "$lean" | sed 's/^/  /'
    echo "Either re-gate the dependency behind the \`x11\` feature, or fix README's claim."
    FAIL=1
  else
    echo "OK: lean build is X11-free"
  fi
fi

echo "########## Linux graph: must match the baseline ##########"
if ! lin_tree=$(cargo tree --target x86_64-unknown-linux-gnu -p rt --edges normal 2>&1); then
  echo "FAIL: cargo tree could not resolve the Linux graph -- this gate checked NOTHING:"
  printf '%s\n' "$lin_tree" | tail -5 | sed 's/^/  /'
  echo "Refusing to compare (or to write a baseline) from an empty graph."
  exit 1
fi
lin=$(printf '%s\n' "$lin_tree" | sort -u)
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
