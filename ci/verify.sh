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

# rt-engine and vt-term are in here deliberately: rt-engine owns the PTY read loop and
# the vendored engine, and that is where the 2026-09-03 dead-child output-loss bug lived —
# a bug that reproduced ONLY on riscv64. Leaving them out gave the arch-sensitive code no
# multi-arch coverage at all. Neither crate pulls in Wayland/EGL, so both build on milkv.
PKGS=(-p vt-parser -p vt-conformance -p rt-handoff -p vt-term -p rt-engine)
FAIL=0

# Keep error: (colon) alongside error[ so cargo/build errors (e.g. a dead path override →
# "error: failed to update path override") stay VISIBLE in the filtered remote output
# instead of being silently dropped and mistaken for a clean run.
# Built from PKGS so the remote list can never drift from the local one (it did before:
# the package list was spelled out twice and only one copy got updated).
tests_cmd="cargo test -q ${PKGS[*]} 2>&1 | grep -E \"test result:|error\\[|error:|FAILED|panicked\""
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
  # --exclude=.cargo/config.toml: that file is LOCAL-ONLY and gitignored — it path-overrides
  # the `mullion` dep to ~/git/mullion. Pushing it makes the REMOTE demand its own
  # ~/git/mullion; when that is missing or stale the remote's build breaks (seen 2026-07-25
  # and again 2026-09-02). Without it the remote builds the published crate, which is what
  # CI and releases build anyway. NOTE: an excluded file is not deleted on the receiver, so
  # a previously-pushed copy must be removed by hand once.
  # `. /etc/profile.d/rust.sh`: apollo has a SYSTEM-WIDE rustup at /opt/rust whose
  # shims are symlinked into /usr/local/bin (ahead of the distro rust in PATH). That
  # install exports RUSTUP_HOME=/opt/rust/rustup from /etc/profile.d -- which only a
  # LOGIN shell sources. `ssh host 'cmd'` is non-login, so RUSTUP_HOME was unset, the
  # shim looked in an empty ~/.rustup and died with "rustup could not choose a version
  # of cargo to run". Same non-login-shell trap that makes `ssh host rt` run /usr/bin/rt.
  #
  # -O (--omit-dir-times): milkv's ~/git is a symlink to an NFS mount whose server
  # refuses utimes on directories ("failed to set times ... Operation not permitted",
  # rsync exit 23). File contents transfer fine; only directory mtimes fail, and
  # nothing here depends on them.
  if ! rsync -a -O --delete --exclude=target/ --exclude='*.swp' --exclude=.cargo/config.toml "$HOME/git/rt/" "$h:git/rt/"; then
    echo "$h: rsync FAILED"; FAIL=1; continue
  fi
  out=$(ssh "$h" ". /etc/profile.d/rust.sh 2>/dev/null||true; . ~/.cargo/env 2>/dev/null||true; cd ~/git/rt; $tests_cmd" 2>&1)
  echo "$out"
  eval_run "$h" "$out"
  if [ $BENCH = 1 ]; then
    ssh "$h" ". /etc/profile.d/rust.sh 2>/dev/null||true; . ~/.cargo/env 2>/dev/null||true; cd ~/git/rt; $bench_cmd" 2>&1
  fi
done

echo; [ $FAIL = 0 ] && echo "==> ALL GREEN" || echo "==> FAILURES (see above)"
exit $FAIL
