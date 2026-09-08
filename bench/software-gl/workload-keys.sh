#!/bin/bash
L=${RT_PHASES:?RT_PHASES must name the phase log}
mark(){ echo "$(date +%s.%N) $1" >> "$L"; }
mark start; sleep 6
mark keys; for i in $(seq 1 5); do printf x; sleep 1.5; done
mark idle; sleep 5
mark clears; for i in $(seq 1 3); do clear; echo "frame $i"; sleep 1.5; done
mark end; sleep 1
exit 0
