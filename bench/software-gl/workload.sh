#!/bin/bash
# Scripted pane workload for the rt perf harness. Runs as the pane's SHELL.
L=${RT_PHASES:?RT_PHASES must name the phase log}
mark(){ echo "$(date +%s.%N) $1" >> "$L"; }
mark start;   sleep 8
mark keys;    for i in $(seq 1 8); do printf x; sleep 1; done
mark idle2;   sleep 6
mark clears;  for i in $(seq 1 6); do clear; echo "frame $i"; sleep 1; done
mark idle3;   sleep 6
mark flood;   seq 1 20000
mark floodend; sleep 6
mark heat;    timeout 8 sh -c 'while :; do :; done'
mark heatend; sleep 6
mark end;     sleep 1
exit 0
