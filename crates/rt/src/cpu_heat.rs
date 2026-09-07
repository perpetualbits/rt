//! How much CPU a pane's process subtree is burning — the input to the heat
//! instrument.
//!
//! The instrument colours a pane's border by the load of *everything running in
//! it*: the shell plus whatever the shell started, so a build, a test run or a
//! runaway process shows up on the pane that owns it. That means two questions
//! per sample, and they are the only two this module answers:
//!
//! 1. **How much CPU has ONE process used, ever?** ([`process_cpu_ns`]) — a
//!    monotonically rising total; `sample_heat` differences two samples.
//! 2. **Who are a process's children?** ([`child_pids`]) — so the walk stays
//!    O(the pane's own processes) rather than O(every process on the machine).
//!
//! Both are pure kernel queries with a completely different implementation per
//! platform, and the walk that joins them ([`subtree_sum`]) is neither: it is a
//! plain graph traversal, so it is written once, tested on Linux, and used by
//! both.
//!
//! # Why this file exists
//!
//! The walk used to be `App::subtree_cpu_ticks` in `main.rs` and read `/proc`
//! directly. macOS has no `/proc`, so every read failed, every pane summed to
//! zero, and the heat instrument drew a permanently cold gauge — the one failure
//! mode worse than not drawing it at all, because it *looks* like an answer.
//!
//! # The unit, and the trap in it
//!
//! Everything here is **nanoseconds of CPU time**, because that is the only unit
//! both platforms can be converted into exactly:
//!
//! * Linux reports `utime`/`stime` in clock ticks (`_SC_CLK_TCK`, 100 Hz on every
//!   architecture rt targets), so a tick is exactly 10 ms.
//! * macOS's `proc_pid_rusage` reports `ri_user_time`/`ri_system_time` in **mach
//!   absolute time units**, NOT nanoseconds — despite the plain `u64` and the
//!   suggestive name. On Intel Macs the mach timebase happens to be 1/1, so the
//!   two are numerically identical and the mistake is invisible. On Apple Silicon
//!   the timebase is 125/3: raw units read **41.67× too low**, which does not look
//!   like a bug — it looks like a busy pane idling at 2% — and it is exactly the
//!   "plausible but wrong" number this instrument must not produce.
//!
//!   Measured on an M5, macOS 26.6.2, against a process `ps` reported at 100.0%
//!   CPU: raw units gave 0.024 cores, timebase-converted gave 1.0003 cores.
//!   [`macos_ns`] does the conversion, and `a_busy_child_measures_about_one_core`
//!   is the standing check on it — it runs on both platforms and asserts against a
//!   real busy process, so a wrong scale factor fails the suite rather than
//!   quietly mis-colouring a border.

/// Ceiling on how many processes one sample will look at.
///
/// A guard against a pathological or looping process tree, not a real limit: the
/// walk already visits each pid at most once, so this only bounds the damage if
/// the kernel ever hands back a cycle. Inherited unchanged from the `/proc` walk
/// this replaced.
const MAX_VISITED: usize = 4096;

/// Nanoseconds in one Linux clock tick. `_SC_CLK_TCK` is 100 on x86-64, aarch64
/// and riscv64 — the three targets rt builds for — so a tick is 10 ms. This is
/// the same constant the `/proc` walk always assumed (it divided by `HZ = 100.0`);
/// expressing it as a duration rather than a rate is what lets macOS, whose clock
/// has nothing to do with ticks, join the same code path.
#[cfg(target_os = "linux")]
const NS_PER_TICK: u64 = 10_000_000;

/// Total CPU time `pid` has consumed since it started, in nanoseconds; 0 if the
/// process is gone, is not ours, or the platform cannot say.
///
/// Own time only — a process's reaped children are *not* included (Linux's
/// `cutime`/`cstime` and macOS's `ri_child_*_time` are both deliberately left
/// out). The walk visits the children itself, so counting them here as well
/// would double every busy subtree.
pub fn process_cpu_ns(pid: u32) -> u64 {
    platform::process_cpu_ns(pid)
}

/// The direct children of `pid`, or an empty vector if it has none, has gone, or
/// the kernel will not say.
///
/// Failure is always "no children", never an error: a missed grandchild
/// under-reports one pane's heat for one 500 ms sample, which is invisible, while
/// anything louder would be a diagnostic in the render loop.
pub fn child_pids(pid: u32) -> Vec<u32> {
    platform::child_pids(pid)
}

/// Total CPU nanoseconds used by `root` and everything descended from it.
///
/// What `sample_heat` calls, once per pane per sample. Costs one CPU query and
/// one children query per process in the subtree, so an idle shell — the common
/// case, and the one that must not burn CPU to say it is idle — is two cheap
/// kernel calls.
pub fn subtree_cpu_ns(root: u32) -> u64 {
    subtree_sum(root, process_cpu_ns, child_pids)
}

/// The traversal, with both kernel queries injected.
///
/// Split out from [`subtree_cpu_ns`] so the part that is *logic* rather than
/// platform can be tested with a fabricated process tree — including the shapes
/// no real kernel will hand you on demand: a cycle, and a tree past the cap.
///
/// Depth-first, each pid summed at most once. Visiting a pid twice is the failure
/// that matters: it would count a shared subtree's CPU repeatedly and light up a
/// pane that is doing nothing.
pub fn subtree_sum(root: u32, mut cpu_ns: impl FnMut(u32) -> u64, mut children: impl FnMut(u32) -> Vec<u32>) -> u64 {
    let mut total: u64 = 0;
    let mut stack = vec![root];
    let mut seen = std::collections::HashSet::new();
    while let Some(pid) = stack.pop() {
        if seen.len() >= MAX_VISITED {
            break; // safety cap against pathological/looping process trees
        }
        if !seen.insert(pid) {
            continue; // already counted: a cycle, or two parents claiming one child
        }
        total = total.saturating_add(cpu_ns(pid));
        stack.extend(children(pid));
    }
    total
}

// --- Linux ----------------------------------------------------------------

#[cfg(target_os = "linux")]
mod platform {
    use super::NS_PER_TICK;

    /// `utime` + `stime` from `/proc/<pid>/stat`.
    ///
    /// The fields are found by their offset *after the last `)`*, not by counting
    /// from the start: field 2 is the executable name in parentheses and may itself
    /// contain spaces and parentheses, so splitting the whole line on whitespace
    /// misaligns everything after it. Past the last `)`, `utime` is index 11 and
    /// `stime` index 12.
    pub fn process_cpu_ns(pid: u32) -> u64 {
        let Ok(content) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else { return 0 };
        let Some(rp) = content.rfind(')') else { return 0 };
        let toks: Vec<&str> = content[rp + 1..].split_whitespace().collect();
        if toks.len() < 13 {
            return 0;
        }
        let ticks = toks[11].parse::<u64>().unwrap_or(0) + toks[12].parse::<u64>().unwrap_or(0);
        ticks.saturating_mul(NS_PER_TICK)
    }

    /// The kernel's own list of a process's direct children, from the main
    /// thread's `children` file. Needs `CONFIG_PROC_CHILDREN` (on by default in
    /// Debian/Ubuntu); if absent the file simply is not there and we miss
    /// grandchildren rather than failing.
    pub fn child_pids(pid: u32) -> Vec<u32> {
        let Ok(kids) = std::fs::read_to_string(format!("/proc/{pid}/task/{pid}/children")) else {
            return Vec::new();
        };
        kids.split_whitespace().filter_map(|k| k.parse::<u32>().ok()).collect()
    }
}

// --- macOS ----------------------------------------------------------------

#[cfg(target_os = "macos")]
mod platform {
    /// `mach_timebase_info`, declared here rather than taken from `libc`, whose
    /// copy is deprecated in favour of a crate rt does not depend on. Two `u32`s
    /// and one call; a new dependency for that would be worse.
    #[repr(C)]
    #[derive(Default)]
    struct MachTimebase {
        numer: u32,
        denom: u32,
    }
    extern "C" {
        fn mach_timebase_info(info: *mut MachTimebase) -> libc::c_int;
    }

    /// Convert mach absolute time units to nanoseconds: `units * numer / denom`.
    ///
    /// The ratio is a property of the machine and never changes while it is
    /// running, so it is read once. 1/1 on Intel (where omitting this is harmless
    /// and therefore invisible) and 125/3 on Apple Silicon (where omitting it
    /// under-reports by 41.67×).
    fn macos_ns(units: u64) -> u64 {
        use std::sync::OnceLock;
        static TB: OnceLock<(u64, u64)> = OnceLock::new();
        let (numer, denom) = *TB.get_or_init(|| {
            let mut tb = MachTimebase::default();
            // SAFETY: writes the two u32s of a struct we own; cannot fail in a way
            // that leaves it partly written.
            let rc = unsafe { mach_timebase_info(&mut tb) };
            // A zero denominator would be a divide by zero, and a failed call
            // leaves the struct zeroed — fall back to 1/1, which is the identity
            // and is the true ratio on Intel.
            if rc != 0 || tb.numer == 0 || tb.denom == 0 {
                (1, 1)
            } else {
                (tb.numer as u64, tb.denom as u64)
            }
        });
        // u128 so a long-lived process's total cannot overflow the multiply: at
        // 125/3 a u64 of units would wrap somewhere past 4.4e9 seconds of CPU, and
        // "somewhere past" is not a margin worth reasoning about every release.
        ((units as u128 * numer as u128) / denom as u128) as u64
    }

    /// `ri_user_time` + `ri_system_time` from `proc_pid_rusage`, converted out of
    /// mach units.
    ///
    /// `RUSAGE_INFO_V2` is used rather than a later flavour because it is the
    /// oldest one carrying both fields — the kernel fills whatever flavour it is
    /// asked for, so asking for less is asking for a smaller struct to be
    /// validated, not for less accuracy.
    pub fn process_cpu_ns(pid: u32) -> u64 {
        if pid == 0 || pid > i32::MAX as u32 {
            return 0;
        }
        // SAFETY: `proc_pid_rusage` writes at most one `rusage_info_v2` into the
        // buffer, which is exactly what is passed, zeroed and owned here. The
        // double indirection is the C signature's (`rusage_info_t` is `void *`).
        let ri = unsafe {
            let mut ri = std::mem::zeroed::<libc::rusage_info_v2>();
            if libc::proc_pid_rusage(pid as i32, libc::RUSAGE_INFO_V2, &mut ri as *mut _ as *mut libc::rusage_info_t) != 0
            {
                return 0; // exited between the walk and here, or not ours
            }
            ri
        };
        macos_ns(ri.ri_user_time.saturating_add(ri.ri_system_time))
    }

    /// `proc_listchildpids` — the macOS counterpart of
    /// `/proc/<pid>/task/<pid>/children`, and the reason this walk did not need a
    /// `KERN_PROC_ALL` scan of every process on the machine every half second.
    ///
    /// It returns the NUMBER OF PIDS written, not a byte count (verified on macOS
    /// 26.6.2), and `<= 0` for "no children" as well as for every error.
    pub fn child_pids(pid: u32) -> Vec<u32> {
        if pid == 0 || pid > i32::MAX as u32 {
            return Vec::new();
        }
        // Two attempts: a small buffer for the overwhelmingly common case (a shell
        // with a handful of jobs), then one resize if it came back exactly full,
        // which is the only sign the kernel gives that the list was truncated.
        let mut cap = 64usize;
        for _ in 0..2 {
            let mut buf = vec![0i32; cap];
            // SAFETY: the kernel writes at most `buffersize` bytes into a buffer we
            // own and have sized in the same expression.
            let n = unsafe {
                libc::proc_listchildpids(
                    pid as i32,
                    buf.as_mut_ptr() as *mut libc::c_void,
                    (buf.len() * std::mem::size_of::<i32>()) as i32,
                )
            };
            if n <= 0 {
                return Vec::new();
            }
            let n = n as usize;
            if n >= cap && cap < super::MAX_VISITED {
                cap = super::MAX_VISITED; // truncated; ask once more, for the lot
                continue;
            }
            return buf[..n.min(cap)].iter().filter(|&&p| p > 0).map(|&p| p as u32).collect();
        }
        Vec::new()
    }
}

// --- anything else --------------------------------------------------------

/// Neither Linux nor macOS: report no load rather than guess.
///
/// rt is not built for these, but the module must still compile if someone tries;
/// `sample_heat` then differences two zeros and the border stays cold, which is
/// the honest answer when there is no way to ask.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    pub fn process_cpu_ns(_pid: u32) -> u64 {
        0
    }
    pub fn child_pids(_pid: u32) -> Vec<u32> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};

    // --- the traversal, on fabricated trees -----------------------------------

    /// The whole point of the walk: a pane's heat is its shell's CPU *plus* its
    /// descendants', however deep.
    #[test]
    fn every_descendant_is_counted_once() {
        // 1 ─ 2 ─ 4
        //   └ 3
        let kids = |p: u32| match p {
            1 => vec![2, 3],
            2 => vec![4],
            _ => vec![],
        };
        let cost = |p: u32| p as u64 * 100;
        assert_eq!(subtree_sum(1, cost, kids), 100 + 200 + 300 + 400);
        assert_eq!(subtree_sum(2, cost, kids), 200 + 400, "a subtree root sums only below itself");
        assert_eq!(subtree_sum(4, cost, kids), 400, "a leaf is just itself");
    }

    /// A pid the kernel knows nothing about is worth zero, not a panic — processes
    /// exit between the children query and the CPU query all the time.
    #[test]
    fn an_unknown_pid_is_zero() {
        assert_eq!(subtree_sum(99, |_| 0, |_| vec![]), 0);
    }

    /// The guard that stops a busy subtree being counted twice. A real
    /// `/proc` walk can hand back the same pid from two parents mid-fork, and
    /// double-counting would light a pane's border for CPU it is not using.
    #[test]
    fn a_cycle_terminates_and_counts_each_pid_once() {
        let kids = |p: u32| match p {
            1 => vec![2],
            2 => vec![3],
            3 => vec![1], // back to the root
            _ => vec![],
        };
        assert_eq!(subtree_sum(1, |_| 10, kids), 30, "three distinct pids, ten each");
    }

    /// A pid that is its own child is the degenerate cycle, and the one most
    /// likely to arrive from a confused kernel interface.
    #[test]
    fn a_self_parenting_pid_terminates() {
        assert_eq!(subtree_sum(7, |_| 5, |_| vec![7]), 5);
    }

    /// The cap bounds a pathological tree. Fan out wider than `MAX_VISITED` and
    /// the walk must stop rather than sample tens of thousands of processes inside
    /// the render loop.
    #[test]
    fn the_visit_cap_bounds_the_work() {
        let mut visits = 0usize;
        let total = subtree_sum(
            0,
            |_| {
                visits += 1;
                1
            },
            |p| if p == 0 { (1..20_000).collect() } else { vec![] },
        );
        assert!(visits <= MAX_VISITED, "visited {visits}, cap is {MAX_VISITED}");
        assert!(total <= MAX_VISITED as u64);
    }

    // --- the platform queries, against real processes --------------------------

    /// Run `sh -c "<burner> & wait"` in its own process group and return the
    /// group leader's `Child`. The shell forks the burner, so the CPU is spent one
    /// level BELOW the pid we sample — which is the shape a pane actually has (a
    /// shell that is itself idle, running something that is not) and the shape that
    /// fails if the walk does not descend.
    ///
    /// The burner is time-bounded so that it cannot outlive the test even if the
    /// kill below fails; the process group is so that the kill reaches the burner
    /// and not just the shell waiting on it.
    fn spawn_busy_group(seconds: u32) -> std::process::Child {
        use std::os::unix::process::CommandExt;
        let mut cmd = std::process::Command::new("sh");
        cmd.arg("-c").arg(format!("perl -e '$e=time+{seconds}; 1 while time<$e;' & wait"));
        cmd.process_group(0); // its own group, so one kill reaps the pair
        cmd.spawn().expect("this test needs `sh` and `perl` (both present on Debian and macOS)")
    }

    /// Kill and reap the group `child` leads. Only ever the group this test
    /// created, addressed by the exact pid it was handed.
    fn reap_group(mut child: std::process::Child) {
        // SAFETY: a negative pid signals that process group; it is the group this
        // test created via `process_group(0)`, and signalling is a pure syscall.
        unsafe { libc::kill(-(child.id() as i32), libc::SIGKILL) };
        let _ = child.kill();
        let _ = child.wait();
    }

    /// The measurement itself, end to end, against a process burning one core.
    ///
    /// This is the test the macOS port needed and could not have: it runs on
    /// whatever platform the suite is on, so it covers the `/proc` reader on Linux
    /// and `proc_pid_rusage` + `proc_listchildpids` on a Mac, and it fails for
    /// *both* ways of getting a platform wrong:
    ///
    /// * **reading zero** — what macOS did before this module existed (no `/proc`);
    /// * **reading the wrong scale** — what macOS does if `ri_user_time` is taken
    ///   as nanoseconds instead of mach units. On Apple Silicon that reads 0.024
    ///   cores for a process at 100% CPU: not zero, just wrong, and well under the
    ///   floor below.
    ///
    /// The band is deliberately wide (the suite runs its tests in parallel, so the
    /// burner is not guaranteed a whole core) but no scale error survives it: the
    /// two known failures land at 0.0 and 0.024, and a 41× over-read at 41.
    #[test]
    fn a_busy_child_measures_about_one_core() {
        let child = spawn_busy_group(8);
        let leader = child.id();
        std::thread::sleep(Duration::from_millis(400)); // let the burner get going

        let t0 = Instant::now();
        let (sub0, own0) = (subtree_cpu_ns(leader), process_cpu_ns(leader));
        std::thread::sleep(Duration::from_millis(1000));
        let dt = t0.elapsed().as_secs_f64();
        let (sub1, own1) = (subtree_cpu_ns(leader), process_cpu_ns(leader));
        reap_group(child);

        let subtree_cores = sub1.saturating_sub(sub0) as f64 / (dt * 1e9);
        let own_cores = own1.saturating_sub(own0) as f64 / (dt * 1e9);

        assert!(
            (0.25..1.75).contains(&subtree_cores),
            "a process burning one core measured as {subtree_cores:.4} cores — \
             0.0 means the platform query returns nothing, and ~0.024 on Apple Silicon \
             means mach absolute time units were mistaken for nanoseconds"
        );
        assert!(
            own_cores < 0.1,
            "the shell itself is blocked in wait() and must measure idle ({own_cores:.4} cores); \
             the load has to come from descending to its child"
        );
    }

    /// The counter-check: an idle process must read as idle. Guards against the
    /// obvious wrong implementation of the macOS half — a system-wide scan, or
    /// summing the wrong subtree — which would make every pane warm at once.
    #[test]
    fn an_idle_process_measures_no_load() {
        let mut child = std::process::Command::new("sleep").arg("5").spawn().expect("this test needs `sleep`");
        let pid = child.id();
        std::thread::sleep(Duration::from_millis(100));

        let t0 = Instant::now();
        let a = subtree_cpu_ns(pid);
        std::thread::sleep(Duration::from_millis(400));
        let dt = t0.elapsed().as_secs_f64();
        let b = subtree_cpu_ns(pid);

        let _ = child.kill();
        let _ = child.wait();

        let cores = b.saturating_sub(a) as f64 / (dt * 1e9);
        assert!(cores < 0.05, "a sleeping process must not register load, got {cores:.4} cores");
    }

    /// A total that only ever rises is what `sample_heat` differences; a counter
    /// that could go backwards would make `saturating_sub` silently swallow real
    /// load.
    #[test]
    fn a_processs_cpu_total_never_decreases() {
        let me = std::process::id();
        let a = process_cpu_ns(me);
        // Spend a little measurable CPU rather than sleeping, so this says
        // something even on a machine where the resolution is coarse.
        let mut x = 0u64;
        for i in 0..5_000_000u64 {
            x = x.wrapping_add(i);
        }
        std::hint::black_box(x);
        assert!(process_cpu_ns(me) >= a, "CPU totals are monotonic");
    }

    /// pid 0 and an out-of-range pid are not processes; on macOS they have special
    /// meanings to the `libproc` calls, so they must not reach them.
    #[test]
    fn non_pids_are_rejected_rather_than_queried() {
        assert_eq!(process_cpu_ns(0), 0);
        assert_eq!(process_cpu_ns(u32::MAX), 0);
        assert!(child_pids(0).is_empty());
        assert!(child_pids(u32::MAX).is_empty());
    }
}
