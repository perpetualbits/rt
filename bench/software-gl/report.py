#!/usr/bin/env python3
# report.py RUNDIR — per-phase CPU (core-seconds, cores avg) and frame counts (sendmsg/writev from the main thread)
import sys, re, os
d = sys.argv[1]
ph = [l.split() for l in open(f"{d}/phases.log") if not l.startswith("rt exit")]
ph = [(float(t), n) for t, n in ph]
cpu = [tuple(map(float, l.split())) for l in open(f"{d}/cpu.log")]
frames = []
if os.path.exists(f"{d}/strace.log"):
    for l in open(f"{d}/strace.log", errors="replace"):
        m = re.match(r"(\d+)\s+([\d.]+)\s+(sendmsg|writev)\((\d+),", l)
        if m: frames.append((float(m.group(2)), int(m.group(1)), int(m.group(4))))
def at(t):
    best = min(cpu, key=lambda r: abs(r[0]-t)); return best
print(f"{'phase':10s} {'secs':>6s} {'core-s':>7s} {'cores':>6s} {'llvm%':>6s} {'msgs':>5s} {'cs/msg':>7s}")
for i,(t,n) in enumerate(ph):
    t2 = ph[i+1][0] if i+1 < len(ph) else cpu[-1][0]
    a, b = at(t), at(t2)
    cs = (b[1]-a[1])/100.0; lp = (b[3]-a[3])/100.0
    secs = t2-t
    nf = sum(1 for f in frames if t <= f[0] < t2)
    print(f"{n:10s} {secs:6.1f} {cs:7.2f} {cs/secs if secs else 0:6.2f} {100*lp/cs if cs else 0:6.0f} {nf:5d} {cs/nf if nf else 0:7.2f}")
