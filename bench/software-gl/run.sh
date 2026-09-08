#!/bin/bash
# run.sh NAME BINARY [extra rt args...]  — env decides the display (WAYLAND_DISPLAY / DISPLAY / RT_BACKEND)
NAME=$1; BIN=$2; shift 2
H=${RT_BENCH_HOME:-$HOME/rt-perf-harness}; D=$H/runs/$NAME; mkdir -p $D
export XDG_CONFIG_HOME=$H/cfg XDG_CACHE_HOME=$H/cache
export SHELL=${SHELL_OVERRIDE:-$H/workload.sh} RT_PHASES=$D/phases.log
rm -f $D/phases.log $D/cpu.log
setsid $BIN --cols 160 --rows 45 "$@" >$D/rt.out 2>&1 &
RT=$!; echo $RT > $D/rt.pid
$H/sample.sh $RT $D/cpu.log &
S=$!
# frame timestamps: wayland flushes (sendmsg) or X writes (writev) from rt
sudo -n strace -f -p $RT -e trace=sendmsg,writev -ttt -qq -o $D/strace.log 2>/dev/null &
wait $RT; RC=$?
wait $S 2>/dev/null
echo "rt exit $RC" >> $D/phases.log
