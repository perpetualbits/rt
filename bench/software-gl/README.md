# Software-GL frame-cost harness

Measures what one rt frame costs on a box without a GL driver (llvmpipe), on a
headless Wayland compositor so nothing touches a live desktop. This is the rig
that found the v0.3.20 software-GL fixes (see `docs/software-gl-lessons.md`).

## Setup (on the target box)

```sh
mkdir -p ~/rt-perf-harness && cp bench/software-gl/* ~/rt-perf-harness/
mkdir -p ~/rt-perf-harness/cfg && cp -r ~/.config/rt ~/rt-perf-harness/cfg/   # the real settings
export XDG_RUNTIME_DIR=/run/user/$(id -u)
setsid weston --backend=headless --socket=wl-rttest --width=1600 --height=900 \
    --xwayland --idle-time=0 --no-config --log=$HOME/rt-perf-harness/weston.log &
echo $! > ~/rt-perf-harness/weston.pid        # kill ONLY this pid when done
```

## Run

```sh
cd ~/rt-perf-harness
export WAYLAND_DISPLAY=wl-rttest; unset DISPLAY          # GL on Wayland (llvmpipe)
RUST_LOG=rt::frame=debug ./run.sh NAME /path/to/rt        # scripted pane workload
python3 report.py runs/NAME                                # per-phase cores + frames
grep rt::frame cache/rt/stderr.log | tail                  # one line per frame
```

`run.sh` starts rt with `SHELL=workload.sh` (a scripted pane: idle, 1 char/s,
clears, an output flood, a hot child), samples every thread's CPU twice a second
into `runs/NAME/cpu.log`, straces the compositor socket for frame timestamps, and
writes phase markers. `SHELL_OVERRIDE=workload-keys.sh` (or `workload-flood.sh`)
picks a shorter script. The Mesa shader cache lives in `cache/` and is shared
across runs on purpose: a cold cache costs ~10 s of LLVM JIT on a slow core and
would swamp the first phases.

Knobs in rt itself:

* `RUST_LOG=rt::frame=debug` — per frame: plan (full / partial with rect count
  and bbox), pixels, vertices, wall ms, and what asked for it (output, anim,
  heat, meter, stall, blink).
* `RT_FRAME_SYNC=1` — adds `gl-sync clear=..ms[lpN mM] draw=.. swap=.. finish=..`
  with a `glFinish` after each phase and the CPU ticks (10 ms) the llvmpipe
  worker threads (`lp`) and the main thread (`m`) burned inside it.
  `RT_FRAME_SYNC=2` also sleeps 400 ms after the draw to prove glFinish is a
  barrier (it is).

XRender comparison: `export DISPLAY=:1; unset WAYLAND_DISPLAY; ./run.sh NAME rt --backend xrender`
(weston's `--xwayland` puts an X server on `:1`).
