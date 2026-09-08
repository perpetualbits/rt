#!/bin/bash
L=${RT_PHASES:?RT_PHASES must name the phase log}
mark(){ echo "$(date +%s.%N) $1" >> "$L"; }
mark start; sleep 10
mark flood; seq 1 150000
mark floodend; sleep 5
mark end; sleep 1
exit 0
