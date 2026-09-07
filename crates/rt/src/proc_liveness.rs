//! Is the process that owns an `rt-<pid>` patch-bay directory still alive?
//!
//! The startup sweep ([`crate::sweep_stale_jacks`]) reclaims patch-bay directories
//! left behind by rt sessions that died without cleaning up. Getting the liveness
//! test wrong is not a leak, it is **destruction**: a false "dead" verdict makes a
//! starting rt `remove_dir_all` a *running* rt's fifos, which can never be repaired
//! because the running panes' shells already hold those paths in `$RT_IN`/`$RT_OUT`.
//!
//! The original test was `Path::new("/proc/<pid>").exists()`. That is Linux-only:
//! macOS has no `/proc` at all, so on macOS every pid looked dead and a second rt
//! deleted the first one's patch bay out from under it. (rt had the same bug once
//! before, per-window rather than cross-process — see `JACKS_DIR_ENSURED`.)
//!
//! This module holds the decision and the portable probe. It is deliberately NOT
//! `cfg`'d to any platform, for the same reason [`crate::wgpu_frame`] and
//! [`crate::vibrancy_policy`] are not: the logic is what has to be right, and Linux
//! CI is where it gets tested.

/// What the kernel says about a pid.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Liveness {
    /// A process with this pid exists. Its directory must be left alone.
    Alive,
    /// No process with this pid exists. Its directory may be reclaimed.
    Gone,
}

/// Ask the kernel whether `pid` names a live process, with `kill(pid, 0)`.
///
/// Signal 0 sends nothing; it only runs `kill`'s existence and permission checks,
/// which is exactly the question here, and it is POSIX — so it answers the same on
/// Linux, macOS and the BSDs.
///
/// The errno handling is the load-bearing part, and it is deliberately asymmetric:
/// **only `ESRCH` means gone**, everything else means alive.
///
/// * `0` — the process exists and we may signal it. Alive.
/// * `EPERM` — the process exists but belongs to someone else. **Alive.** Reading
///   this as "gone" is precisely how a running rt's fifos get deleted; it is the
///   one mistake that turns this back into a destructive bug.
/// * `ESRCH` — no such process. Gone; the only verdict that authorises a delete.
/// * anything else — `kill` defines no other errno for a valid signal, so an
///   unexpected one means the world is not as we think it is. Fail safe: alive.
pub fn probe(pid: u32) -> Liveness {
    // `kill` gives pid 0 and pid -1 BROADCAST meanings (every process in our group,
    // and every process we may signal). Neither can ever name an rt session — pids
    // come from `getpid()`, always a positive `pid_t` — so a directory naming one
    // was not written by any rt we should be reclaiming for. Do not hand it to
    // `kill` (the call would succeed and mean nothing), and do not call it dead.
    if pid == 0 || pid > i32::MAX as u32 {
        return Liveness::Alive;
    }
    // SAFETY: `kill` with signal 0 delivers no signal and touches no memory; the
    // pid is range-checked above to be a positive `pid_t`.
    if unsafe { libc::kill(pid as libc::pid_t, 0) } == 0 {
        return Liveness::Alive;
    }
    match std::io::Error::last_os_error().raw_os_error() {
        Some(e) if e == libc::ESRCH => Liveness::Gone,
        _ => Liveness::Alive, // EPERM and anything unexpected: keep the directory
    }
}

/// Should the startup sweep delete the patch-bay directory named `rt-<pid>`?
///
/// `me` is this process's own pid, checked first so no probe quirk can ever reach
/// the directory we are about to fill with our own live fifos.
///
/// **Pid reuse** is deliberately resolved the same safe way. If rt died and the
/// kernel later handed its pid to an unrelated process, this says `Alive` and the
/// stale directory survives — a few empty fifos in `$XDG_RUNTIME_DIR`, cleaned up
/// by the session manager at logout, and by the next rt that starts once the pid
/// really is free. The reverse mistake destroys a running session's panes
/// irreparably. `kill` cannot distinguish the two cases (a pid is all it has), so
/// there is no cleverer answer available here — only a choice of which way to be
/// wrong, and leaking is the cheap one. The complementary case, *our own* pid
/// having been reused, is already handled where it can be: `ensure_jacks_dir`
/// wipes a pre-existing `rt-<our pid>` on this process's first call.
pub fn should_reclaim(pid: u32, me: u32, liveness: Liveness) -> bool {
    if pid == me {
        return false; // our own directory, in use right now
    }
    liveness == Liveness::Gone
}

/// Probe `pid` and decide in one step — what the sweep actually calls.
pub fn should_reclaim_pid(pid: u32, me: u32) -> bool {
    should_reclaim(pid, me, probe(pid))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Our own patch bay is live by definition: the fifos in it are wired to the
    /// panes of the very process running this sweep.
    #[test]
    fn our_own_directory_is_never_reclaimed() {
        let me = std::process::id();
        assert!(!should_reclaim(me, me, Liveness::Alive));
        // Even if the probe somehow said "gone", our own pid wins the test — the
        // pid check comes first precisely so no probe quirk can reach it.
        assert!(!should_reclaim(me, me, Liveness::Gone));
    }

    /// The whole point of the sweep: a directory whose owner really is gone.
    #[test]
    fn an_absent_pid_is_reclaimed() {
        assert!(should_reclaim(4242, 1234, Liveness::Gone));
    }

    /// The bug: a live owner's directory must survive another rt's startup.
    #[test]
    fn a_live_pid_is_kept() {
        assert!(!should_reclaim(4242, 1234, Liveness::Alive));
    }

    /// A live process we do not own answers `kill(2)` with `EPERM`, not success.
    /// Reading that as "gone" is the exact way to turn this back into the
    /// destructive bug, so it gets its own test against a real process.
    ///
    /// pid 1 (`init`/`launchd`) is always running and is always root's. Run as an
    /// ordinary user the probe sees `EPERM`; run as root it sees success. Both
    /// must come out [`Liveness::Alive`].
    #[test]
    fn a_live_process_owned_by_someone_else_is_alive() {
        assert_eq!(probe(1), Liveness::Alive, "pid 1 is always running; EPERM means alive, not gone");
        assert!(!should_reclaim_pid(1, std::process::id()));
    }

    /// The strong version of the whole predicate: watch a real process cross from
    /// alive to gone and check the verdict flips with it.
    #[test]
    fn a_real_child_is_alive_until_it_is_reaped() {
        let mut child = std::process::Command::new("sleep")
            .arg("30")
            .spawn()
            .expect("spawn a short-lived child");
        let pid = child.id();
        let me = std::process::id();

        assert_eq!(probe(pid), Liveness::Alive, "a running child must probe alive");
        assert!(!should_reclaim_pid(pid, me), "a running rt's patch bay must never be reclaimed");

        // Kill and REAP: an unreaped zombie still has a pid table entry, so
        // `kill(pid, 0)` succeeds for it. Only after the wait is the pid free.
        child.kill().expect("kill the child we started");
        child.wait().expect("reap the child we started");

        assert_eq!(probe(pid), Liveness::Gone, "a reaped child's pid must probe gone");
        assert!(should_reclaim_pid(pid, me), "a dead owner's patch bay is reclaimable");
    }

    /// `kill(2)` gives pid 0 and pid -1 broadcast meanings (this process group,
    /// and every process we may signal). Neither can ever be an rt pid, so the
    /// probe must not hand them to `kill` and must not call them dead.
    #[test]
    fn broadcast_pids_are_never_reclaimed() {
        let me = std::process::id();
        assert_eq!(probe(0), Liveness::Alive);
        assert_eq!(probe(u32::MAX), Liveness::Alive); // would be -1 as a pid_t
        assert!(!should_reclaim_pid(0, me));
        assert!(!should_reclaim_pid(u32::MAX, me));
    }
}
