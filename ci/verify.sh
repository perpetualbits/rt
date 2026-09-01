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

PKGS=(-p vt-parser -p vt-conformance -p rt-handoff)
FAIL=0

# Keep error: (colon) alongside error[ so cargo/build errors (e.g. a dead path override →
# "error: failed to update path override") stay VISIBLE in the filtered remote output
# instead of being silently dropped and mistaken for a clean run.
tests_cmd='cargo test -q -p vt-parser -p vt-conformance -p rt-handoff 2>&1 | grep -E "test result:|error\[|error:|FAILED|panicked"'
bench_cmd='cargo run -q --release --example parser_bench -p vt-conformance 2>&1 | grep -vE "Compiling|Finished|Running|warning:"'

# Evaluate one host's captured test output. A run PASSES only if it emitted at least one
# "test result:" line AND shows no failure markers. Crucially, EMPTY or error-only output
# (build failure, missing cargo, a dead .cargo/config.toml path override, an unreachable
# host, …) has no "test result:" line → treated as FAILURE. This closes the trap where a
# remote that produced nothing usable was silently counted as green (no riscv64 coverage).
eval_run() { # $1 = label, $2 = captured output
  local label=$1 out=$2
  if printf '%s\n' "$out" | grep -qE "FAILED|panicked|error\[|error:"; then
    echo "$label: FAIL (errors above)"; FAIL=1
  elif ! printf '%s\n' "$out" | grep -qE "test result:"; then
    echo "$label: FAIL (no 'test result:' line — build/env error or no output from host)"; FAIL=1
  fi
}

echo "########## LOCAL ($(uname -m)) ##########"
out=$(cargo test -q "${PKGS[@]}" 2>&1); echo "$out" | grep -E "test result:|error\[|error:|FAILED|panicked"
eval_run "LOCAL" "$out"
if [ $BENCH = 1 ]; then eval "$bench_cmd"; fi

for h in "${REMOTES[@]}"; do
  echo; echo "########## $h ##########"
  if ! rsync -a --delete --exclude=target/ --exclude='*.swp' "$HOME/git/rt/" "$h:git/rt/"; then
    echo "$h: rsync FAILED"; FAIL=1; continue
  fi
  out=$(ssh "$h" ". ~/.cargo/env 2>/dev/null||true; cd ~/git/rt; $tests_cmd" 2>&1)
  echo "$out"
  eval_run "$h" "$out"
  if [ $BENCH = 1 ]; then
    ssh "$h" ". ~/.cargo/env 2>/dev/null||true; cd ~/git/rt; $bench_cmd" 2>&1
  fi
done

echo; [ $FAIL = 0 ] && echo "==> ALL GREEN" || echo "==> FAILURES (see above)"
exit $FAIL
