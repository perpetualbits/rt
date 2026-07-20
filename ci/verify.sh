#!/usr/bin/env bash
# Multi-arch verify: run the terminal-engine conformance battery (and optionally the
# parser benchmark) LOCALLY and on each remote architecture, from one command — so
# correctness and performance are checked on x86_64 (dop651/apollo) AND riscv64 (milkv).
# This is the standing "CI/CD to milkv" (docs/own-engine-plan.md, Phase 5): the slow
# riscv board is the perf canary, so nothing perf-relevant lands unmeasured there.
#
# Usage:
#   ci/verify.sh            # correctness on local + all remotes
#   ci/verify.sh --bench    # also run the parser throughput benchmark on each
#   ci/verify.sh --bench milkv   # restrict to one remote
#
# Uses ssh ALIASES (milkv/apollo) and keeps each remote's target/ cache (see the
# rt-deploy-all-machines memory). Exit non-zero if any host has a test failure.
set -uo pipefail

BENCH=0
REMOTES=(milkv apollo)
args=()
for a in "$@"; do
  case "$a" in
    --bench) BENCH=1 ;;
    *) args+=("$a") ;;
  esac
done
[ ${#args[@]} -gt 0 ] && REMOTES=("${args[@]}")

PKGS=(-p vt-parser -p vt-conformance)
FAIL=0

tests_cmd='cargo test -q -p vt-parser -p vt-conformance 2>&1 | grep -E "test result:|error\[|FAILED|panicked"'
bench_cmd='cargo run -q --release --example parser_bench -p vt-conformance 2>&1 | grep -vE "Compiling|Finished|Running|warning:"'

check_output() { grep -qE "FAILED|error\[|panicked|0 passed; [1-9]" && return 1 || return 0; }

echo "########## LOCAL ($(uname -m)) ##########"
out=$(cargo test -q "${PKGS[@]}" 2>&1); echo "$out" | grep -E "test result:|error\[|FAILED|panicked"
echo "$out" | grep -qE "FAILED|error\[|panicked" && { echo "LOCAL: FAIL"; FAIL=1; }
if [ $BENCH = 1 ]; then eval "$bench_cmd"; fi

for h in "${REMOTES[@]}"; do
  echo; echo "########## $h ##########"
  if ! rsync -a --delete --exclude=target/ --exclude='*.swp' "$HOME/git/rt/" "$h:git/rt/"; then
    echo "$h: rsync FAILED"; FAIL=1; continue
  fi
  out=$(ssh "$h" ". ~/.cargo/env 2>/dev/null||true; cd ~/git/rt; $tests_cmd" 2>&1)
  echo "$out"
  echo "$out" | grep -qE "FAILED|error\[|panicked" && { echo "$h: FAIL"; FAIL=1; }
  if [ $BENCH = 1 ]; then
    ssh "$h" ". ~/.cargo/env 2>/dev/null||true; cd ~/git/rt; $bench_cmd" 2>&1
  fi
done

echo; [ $FAIL = 0 ] && echo "==> ALL GREEN" || echo "==> FAILURES (see above)"
exit $FAIL
