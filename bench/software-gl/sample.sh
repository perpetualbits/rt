#!/bin/bash
# sample.sh PID OUT — every 0.5s: epoch, total ticks of all threads, main-thread ticks, llvmpipe ticks
P=$1; OUT=$2
while kill -0 $P 2>/dev/null; do
  tot=0; main=0; lp=0
  for t in /proc/$P/task/*; do
    [ -r $t/stat ] || continue
    read -r _ comm _ _ _ _ _ _ _ _ _ _ _ ut st _ < $t/stat 2>/dev/null || continue
    n=$((ut+st)); tot=$((tot+n))
    case "$comm" in *llvmpipe*) lp=$((lp+n));; esac
    [ "$(basename $t)" = "$P" ] && main=$n
  done
  echo "$(date +%s.%N) $tot $main $lp" >> $OUT
  sleep 0.5
done
