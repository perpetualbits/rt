#!/usr/bin/env bash
#
# term-render-bench.sh — compare text-output throughput of several terminal
# emulators by timing how long each takes to consume+render an identical stream.
#
# The measured quantity is wall-clock of `cat <payload>` run *inside* each
# terminal. `cat` blocks on write once the emulator's PTY buffer fills, so its
# runtime is a proxy for how fast that terminal drains and paints the bytes.
# This is the classic "cat a big file" benchmark, made fair by:
#   * every terminal replays the SAME file (built once, folded to <=80 cols so
#     no terminal line-wraps differently -> identical glyph/line workload);
#   * same repetition count, a warmup pass, and best-of-N (min) reported;
#   * terminals run one at a time with a cooldown, so they don't contend.
#
# CAVEAT (be honest about it): `cat` timing mostly reflects PTY-drain + parse.
# Terminals that read the PTY on a thread and coalesce painting to vblank can
# look faster than their raw glyph-drawing cost. For a rendering-specific
# number use vtebench (github.com/alacritty/vtebench). This script is the
# gut-check; treat the ordering as indicative, not gospel.
#
# WHAT THE SCRIPT CANNOT MATCH FOR YOU: gnome-terminal, terminator and
# cosmic-term take their font only from their own prefs (no CLI knob). Set them
# by hand to $FONT / $FONT_PT before trusting cross-terminal numbers. alacritty
# and rt are pinned by the script. See the "FONT SETUP" note it prints.
#
# Usage:
#   ./term-render-bench.sh                 # cat mode, all five terminals
#   TERMINALS="rt alacritty" ./term-render-bench.sh
#   REPS=8 TARGET_LINES=300000 ./term-render-bench.sh
#   RT_INSTRUMENTS=1 ./term-render-bench.sh # benchmark rt with its border gauges on
#   REBUILD=1 ./term-render-bench.sh        # force-regenerate the payload
#   MODE=vtebench ./term-render-bench.sh    # render-stress suite (needs vtebench)
#   MODE=vtebench VTEBENCH_ARGS="-w 100 -H 30 scrolling dense-cells" ./term-render-bench.sh
#
set -uo pipefail

# ---- tunables (all env-overridable) ---------------------------------------
FONT=${FONT:-"DejaVu Sans Mono"}      # family to use where we can set it
FONT_PT=${FONT_PT:-12}                # alacritty font size (points)
RT_FONT_PX=${RT_FONT_PX:-16}          # rt font size (pixels; rt measures in px)
COLS=${COLS:-100}                     # requested columns (must be > FOLD)
ROWS=${ROWS:-30}                      # requested rows
FOLD=${FOLD:-80}                      # wrap payload to this width so nobody re-wraps
REPS=${REPS:-5}                       # timed repetitions per terminal
WARMUP=${WARMUP:-1}                   # untimed warmup passes (settle window/atlas)
TARGET_LINES=${TARGET_LINES:-150000}  # payload size (grow for a heavier test)
WORK=${WORK:-/tmp/term-bench}         # scratch dir (payload + per-run results)
TIMEOUT=${TIMEOUT:-180}               # per-terminal give-up time (seconds)
COOLDOWN=${COOLDOWN:-1}               # pause between terminals (seconds)
RT_INSTRUMENTS=${RT_INSTRUMENTS:-0}   # 1 = leave rt's border gauges/jacks on
TERMINALS=${TERMINALS:-"rt alacritty gnome-terminal terminator cosmic-term"}

# MODE = cat      : replay a fixed text file (default; PTY-drain + parse proxy).
# MODE = vtebench : run alacritty's vtebench render-stress suite (scrolling
#                   regions, alt-screen, dense colored cells, unicode) — a far
#                   better proxy for actual glyph drawing. Needs vtebench on PATH
#                   (or point VTEBENCH at it). Both modes are timed identically.
MODE=${MODE:-cat}
VTEBENCH=${VTEBENCH:-$(command -v vtebench 2>/dev/null || echo "$HOME/.cargo/bin/vtebench")}
VTEBENCH_ARGS=${VTEBENCH_ARGS:-"-w $COLS -H $ROWS"}  # size flags; append names to pick a subset

PAYLOAD="$WORK/payload.txt"
OUT="$WORK/out"

# ---- sanity guards ---------------------------------------------------------
if [ -z "${WAYLAND_DISPLAY:-}${DISPLAY:-}" ]; then
  echo "error: no WAYLAND_DISPLAY/DISPLAY — run this inside your graphical session." >&2
  exit 1
fi
case "$MODE" in
  cat|vtebench) ;;
  *) echo "error: MODE must be 'cat' or 'vtebench' (got '$MODE')" >&2; exit 1 ;;
esac

mkdir -p "$WORK"
rm -rf "$OUT"; mkdir -p "$OUT"

if [ "$MODE" = vtebench ]; then
  # ---- vtebench mode: verify the tool, skip the text payload --------------
  if [ ! -x "$VTEBENCH" ]; then
    cat >&2 <<MSG
error: MODE=vtebench but no vtebench binary was found (looked at: $VTEBENCH).
vtebench isn't packaged; build it from source, then re-run:
    git clone https://github.com/alacritty/vtebench && cd vtebench
    cargo build --release
    VTEBENCH=\$(pwd)/target/release/vtebench MODE=vtebench $0
(or 'cargo install --git https://github.com/alacritty/vtebench' to drop it in ~/.cargo/bin).
MSG
    exit 1
  fi
  BYTES=0   # vtebench synthesizes its own stream; there is no single payload size
  echo "mode: vtebench  ($VTEBENCH $VTEBENCH_ARGS)"
  echo
else
  # ---- cat mode: build the payload once (folded, replicated) --------------
  if [ "$COLS" -le "$FOLD" ]; then
    echo "error: COLS ($COLS) must exceed FOLD ($FOLD) or lines will wrap." >&2
    exit 1
  fi
  if [ ! -s "$PAYLOAD" ] || [ "${REBUILD:-0}" = 1 ]; then
    echo "building payload from 'ls -alR ~' folded to ${FOLD} cols ..."
    base="$WORK/base.txt"
    # LC_ALL=C keeps fold byte-oriented and fast; drop errors (unreadable dirs).
    LC_ALL=C ls -alR "$HOME" 2>/dev/null | fold -w "$FOLD" > "$base"
    bl=$(wc -l < "$base")
    if [ "$bl" -eq 0 ]; then echo "error: empty listing" >&2; exit 1; fi
    if [ "$bl" -ge "$TARGET_LINES" ]; then
      head -n "$TARGET_LINES" "$base" > "$PAYLOAD"          # already big enough
    else
      n=$(( (TARGET_LINES + bl - 1) / bl ))                 # ceil(TARGET/bl) copies
      : > "$PAYLOAD"
      for ((i=0;i<n;i++)); do cat "$base"; done > "$PAYLOAD"
    fi
  fi
  LINES=$(wc -l < "$PAYLOAD"); BYTES=$(wc -c < "$PAYLOAD")
  echo "mode: cat  (payload: $LINES lines, $BYTES bytes, $PAYLOAD)"
  echo
fi

# ---- the in-terminal benchmark (written to a file so no nested quoting) ----
# args: <name> <outdir> <reps> <warmup> <cmd...>  — it times running <cmd...>,
# whose stdout goes to the terminal (that's the workload being rendered).
cat > "$WORK/bench.sh" <<'BENCH'
#!/usr/bin/env bash
name=$1; outdir=$2; reps=$3; warmup=$4; shift 4   # remaining args = command to time
export LC_ALL=C
for ((i=0;i<warmup;i++)); do "$@"; done           # untimed warmup
: > "$outdir/$name.times"
for ((i=0;i<reps;i++)); do
  s=$EPOCHREALTIME                                 # bash 5 microsecond clock
  "$@"
  e=$EPOCHREALTIME
  awk -v a="$s" -v b="$e" 'BEGIN{printf "%.6f\n", b-a}' >> "$outdir/$name.times"
done
sync
touch "$outdir/$name.done"                         # signals the driver we're finished
BENCH
chmod +x "$WORK/bench.sh"
# The command each terminal times. cat mode replays the file; vtebench mode runs
# the render-stress suite at the matched size. Both write to the terminal's tty.
if [ "$MODE" = vtebench ]; then
  RUNCMD="$VTEBENCH $VTEBENCH_ARGS"
else
  RUNCMD="cat $PAYLOAD"
fi
BENCH_ARGS() { echo "$WORK/bench.sh $1 $OUT $REPS $WARMUP $RUNCMD"; }

# ---- rt config: pin font, optionally silence the border instruments -------
if [ "$RT_INSTRUMENTS" = 1 ]; then inst=true; else inst=false; fi
RTCFG="$WORK/rtcfg"; mkdir -p "$RTCFG/rt"
cat > "$RTCFG/rt/config.toml" <<EOF
font_family = "$FONT"
font_size = $RT_FONT_PX
show_titlebar = false
inst_output = $inst
inst_heat = $inst
inst_latency = $inst
show_jacks = $inst
EOF

# ---- helpers ---------------------------------------------------------------
LPID=0                                 # pid of the most recently launched terminal

wait_done() {                          # poll for the terminal's .done marker
  local name=$1 start=$SECONDS
  until [ -f "$OUT/$name.done" ]; do
    sleep 0.3
    if (( SECONDS - start > TIMEOUT )); then return 1; fi
  done
  return 0
}

stop() {                               # Wayland-safe teardown: signals only
  local name=$1
  { [ "$LPID" -gt 0 ] && kill "$LPID"; }  2>/dev/null || true
  { [ "$LPID" -gt 0 ] && pkill -P "$LPID"; } 2>/dev/null || true
  pkill -f "$WORK/bench.sh $name" 2>/dev/null || true   # kill the in-term shell
}

cleanup() {                            # on exit, sweep any stragglers
  local t
  for t in $TERMINALS; do pkill -f "$WORK/bench.sh $t" 2>/dev/null || true; done
}
trap cleanup EXIT

# ---- per-terminal launchers (each backgrounds the term and sets LPID) ------
launch_alacritty() {
  setsid alacritty \
    -o "window.dimensions.columns=$COLS" -o "window.dimensions.lines=$ROWS" \
    -o "font.normal.family=$FONT" -o "font.size=$FONT_PT" \
    -o window.padding.x=0 -o window.padding.y=0 \
    -e bash $(BENCH_ARGS alacritty) >/dev/null 2>&1 &
  LPID=$!
}

launch_gnome-terminal() {
  # gnome-terminal daemonizes; --wait keeps the client alive until the window
  # closes, and the default profile closes the window when the command exits.
  setsid gnome-terminal --wait --geometry="${COLS}x${ROWS}" \
    -- bash $(BENCH_ARGS gnome-terminal) >/dev/null 2>&1 &
  LPID=$!
}

launch_terminator() {
  # Terminator's --geometry is pixels, not cells, so we don't pin its grid here
  # (set default_size in its config if you need an exact size). -x = run command.
  setsid terminator -x bash $(BENCH_ARGS terminator) >/dev/null 2>&1 &
  LPID=$!
}

launch_cosmic-term() {
  # cosmic-term has no exec flag, so we type the command into the focused window
  # with wtype. DON'T touch the keyboard/mouse while this one runs.
  setsid cosmic-term >/dev/null 2>&1 &
  LPID=$!
  sleep 2.5                              # let it map and the shell prompt appear
  wtype -d 6 "bash $(BENCH_ARGS cosmic-term); exit" -k Return
}

launch_rt() {
  # --cols/--rows pin rt's window to the same COLSxROWS grid the other terminals
  # get via --geometry, so every emulator paints an identical glyph/line load.
  setsid env XDG_CONFIG_HOME="$RTCFG" \
    RT_EXEC="bash $(BENCH_ARGS rt)" \
    rt --cols "$COLS" --rows "$ROWS" >/dev/null 2>&1 &
  LPID=$!
}

# ---- run each terminal in turn --------------------------------------------
echo "FONT SETUP: alacritty & rt are pinned to '$FONT' ${FONT_PT}pt/${RT_FONT_PX}px."
echo "  Set gnome-terminal / terminator / cosmic-term to the SAME font by hand"
echo "  in their preferences, or their numbers aren't comparable on font cost."
echo
for name in $TERMINALS; do
  if ! command -v "${name%% *}" >/dev/null 2>&1; then
    echo "-- $name: not installed, skipping"; continue
  fi
  echo "-- $name: launching ..."
  rm -f "$OUT/$name.done"
  "launch_$name"
  if wait_done "$name"; then
    echo "   done."
  else
    echo "   TIMEOUT after ${TIMEOUT}s (no result)."
  fi
  stop "$name"
  sleep "$COOLDOWN"
done

# ---- summary ---------------------------------------------------------------
# MB/s is only meaningful in cat mode (a known payload size); vtebench mode
# reports times only, since it synthesizes a variable-size stream.
echo
if [ "$MODE" = vtebench ]; then
  echo "============ results  (vtebench: $VTEBENCH_ARGS, best of $REPS) ============"
  printf "%-16s %9s %9s %9s\n" terminal "min(s)" "median" "mean"
else
  echo "============ results  (cat: $BYTES bytes, best of $REPS) ============"
  printf "%-16s %9s %9s %9s %10s\n" terminal "min(s)" "median" "mean" "MB/s"
fi
# Emit rows to a temp file so we can sort the table by best time.
rows="$WORK/rows.txt"; : > "$rows"
for name in $TERMINALS; do
  f="$OUT/$name.times"
  if [ ! -s "$f" ]; then
    printf "%-16s %9s\n" "$name" "(no result)"
    continue
  fi
  sort -n "$f" | awk -v name="$name" -v bytes="$BYTES" '
    { a[NR]=$1; sum+=$1 }
    END {
      n=NR; mn=a[1];
      med=(n%2 ? a[(n+1)/2] : (a[n/2]+a[n/2+1])/2);
      mbps = (bytes > 0) ? sprintf("%.1f", bytes/mn/1e6) : "-";
      printf "%s %.3f %.3f %.3f %s\n", name, mn, med, sum/n, mbps;
    }' >> "$rows"
done
# Print measured rows sorted fastest-first (the "(no result)" note printed above).
sort -k2 -n "$rows" | while read -r name mn md mean mbps; do
  if [ "$MODE" = vtebench ]; then
    printf "%-16s %9s %9s %9s\n" "$name" "$mn" "$md" "$mean"
  else
    printf "%-16s %9s %9s %9s %10s\n" "$name" "$mn" "$md" "$mean" "$mbps"
  fi
done
echo
if [ "$MODE" = vtebench ]; then
  echo "note: vtebench stresses scrolling/alt-screen/dense-cells/unicode — a much"
  echo "      better rendering proxy than cat. Lower time = faster."
else
  echo "note: 'cat' timing chiefly measures PTY-drain + parse; for a rendering-"
  echo "      specific figure re-run with MODE=vtebench."
fi
