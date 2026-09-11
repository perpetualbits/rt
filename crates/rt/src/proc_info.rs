//! Two leaf questions about a running process: **where is it** and **what is
//! it**.
//!
//! The inputs to the derived pane title (`pane_title::derived`), and nothing
//! else. Like `cpu_heat`, this file is only the per-platform half — the decision
//! of which process to ask, and what to do with the answers, is pure logic in
//! `pane_title` where Linux CI can test it.
//!
//! Both queries have a completely different implementation per platform and the
//! same contract on both:
//!
//! * **Failure is `None`, never an error.** A process exits between one call and
//!   the next all the time; the honest response is "no title facts this refresh",
//!   and the titlebar keeps what it had. Anything louder would be a diagnostic in
//!   the render loop.
//! * **The result is already sanitised.** A directory name is attacker-controlled
//!   in exactly the way an OSC title is (anyone can `mkdir $'\x1b[2J'`), so it
//!   goes through `pane_title::sanitize` here rather than being trusted because it
//!   came from the kernel.
//!
//! # Cost
//!
//! One kernel call each. They are called at most twice a second per untitled
//! pane (see `App::refresh_derived_facts`), never per frame: `/proc` is a
//! filesystem and `proc_pidinfo` copies a 2 KB struct, and neither belongs in a
//! repaint.

use crate::pane_title::sanitize;

/// The absolute working directory of `pid`, or `None` if it has gone, is not
/// ours, or the platform will not say.
pub fn cwd_of(pid: u32) -> Option<String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None; // not a pid; on macOS these have special meanings to libproc
    }
    platform::cwd_of(pid).map(|s| sanitize(&s).into_owned()).filter(|s| !s.is_empty())
}

/// The program name of `pid` (`zsh`, `vim`, `cargo`), or `None` as above.
///
/// Both platforms answer from a fixed-size field in the kernel's process record
/// — Linux's `comm` and macOS's `pbi_comm`, both 16 bytes — so a long program
/// name arrives already truncated, identically on both. That is a property of the
/// cheap query, not a choice made here; reading the full path instead
/// (`/proc/<pid>/exe`, `proc_pidpath`) would cost a second call per refresh to
/// spell out `configure-something-long`, which no titlebar has room for anyway.
pub fn name_of(pid: u32) -> Option<String> {
    if pid == 0 || pid > i32::MAX as u32 {
        return None;
    }
    platform::name_of(pid).map(|s| sanitize(s.trim()).into_owned()).filter(|s| !s.is_empty())
}

// --- Linux ----------------------------------------------------------------

#[cfg(target_os = "linux")]
mod platform {
    /// `/proc/<pid>/cwd` is a symlink to the directory; reading it is one
    /// `readlink`. It resolves for our own processes and yields `EACCES` for
    /// anyone else's — which is `None`, as intended.
    pub fn cwd_of(pid: u32) -> Option<String> {
        let path = std::fs::read_link(format!("/proc/{pid}/cwd")).ok()?;
        Some(path.to_string_lossy().into_owned())
    }

    /// `/proc/<pid>/comm` — the kernel's 16-byte command name, with the trailing
    /// newline the file always carries.
    pub fn name_of(pid: u32) -> Option<String> {
        std::fs::read_to_string(format!("/proc/{pid}/comm")).ok()
    }
}

// --- macOS ----------------------------------------------------------------

#[cfg(target_os = "macos")]
mod platform {
    /// `proc_pidinfo(PROC_PIDVNODEPATHINFO)` fills a `proc_vnodepathinfo` whose
    /// `pvi_cdir.vip_path` is the process's current directory — the macOS
    /// counterpart of reading the `/proc/<pid>/cwd` symlink, and the only
    /// supported way to ask (there is no public API that takes a pid and returns a
    /// path other than this one).
    ///
    /// The call returns the number of BYTES written and fills the struct only if
    /// it wrote all of it, so a short return is a failure, not a partial answer.
    pub fn cwd_of(pid: u32) -> Option<String> {
        // SAFETY: the kernel writes at most one `proc_vnodepathinfo` into a
        // zeroed struct we own and whose size we pass; the double indirection is
        // the C signature's (`void *`).
        let vpi = unsafe {
            let mut vpi = std::mem::zeroed::<libc::proc_vnodepathinfo>();
            let size = std::mem::size_of::<libc::proc_vnodepathinfo>() as libc::c_int;
            let n = libc::proc_pidinfo(
                pid as libc::c_int,
                libc::PROC_PIDVNODEPATHINFO,
                0,
                &mut vpi as *mut _ as *mut libc::c_void,
                size,
            );
            if n != size {
                return None; // gone, not ours, or the kernel declined
            }
            vpi
        };
        // `vip_path` is declared `[[c_char; 32]; 32]` by the libc crate (a
        // workaround for an old rustc's array limits) but is one flat
        // MAXPATHLEN buffer holding a NUL-terminated path. Flatten it back.
        let flat = vpi.pvi_cdir.vip_path.as_flattened();
        let bytes: Vec<u8> = flat.iter().take_while(|&&c| c != 0).map(|&c| c as u8).collect();
        Some(String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `proc_name` — the same 16-byte `p_comm` Linux exposes as `comm`, copied out
    /// of the kernel's BSD process record. Returns the byte count written, `<= 0`
    /// for every failure.
    pub fn name_of(pid: u32) -> Option<String> {
        let mut buf = [0u8; 256]; // proc_name copies at most MAXCOMLEN+1 (17)
        // SAFETY: the kernel writes at most `buffersize` bytes into a buffer we
        // own and have sized in the same expression.
        let n = unsafe {
            libc::proc_name(pid as libc::c_int, buf.as_mut_ptr() as *mut libc::c_void, buf.len() as u32)
        };
        if n <= 0 {
            return None;
        }
        // Some releases return the length WITH its NUL, some without; take the
        // C string either way rather than trusting the count alone.
        let n = (n as usize).min(buf.len());
        let name: Vec<u8> = buf[..n].iter().copied().take_while(|&c| c != 0).collect();
        Some(String::from_utf8_lossy(&name).into_owned())
    }
}

// --- anything else --------------------------------------------------------

/// Neither Linux nor macOS: say nothing, and the titlebar keeps its own
/// last-resort label. rt is not built for these, but the module must compile.
#[cfg(not(any(target_os = "linux", target_os = "macos")))]
mod platform {
    pub fn cwd_of(_pid: u32) -> Option<String> {
        None
    }
    pub fn name_of(_pid: u32) -> Option<String> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The end-to-end check, on both platforms: ask about a process whose working
    /// directory this test chose, and get that directory back.
    ///
    /// This is the test the macOS half needs and cannot fake — it fails for the
    /// two ways of getting a platform wrong (returning nothing, or returning a
    /// path from the wrong field of a 2 KB struct) and it runs wherever the suite
    /// runs.
    #[test]
    fn a_childs_working_directory_is_read_back() {
        let dir = std::env::temp_dir().canonicalize().expect("a temp dir");
        let mut child = std::process::Command::new("sleep")
            .arg("5")
            .current_dir(&dir)
            .spawn()
            .expect("this test needs `sleep`");
        // Give the fork time to exec; before that it still has our cwd.
        std::thread::sleep(std::time::Duration::from_millis(200));
        let cwd = cwd_of(child.id());
        let name = name_of(child.id());
        let _ = child.kill();
        let _ = child.wait();

        assert_eq!(cwd.as_deref(), Some(dir.to_string_lossy().as_ref()), "the child's cwd is what we set");
        assert_eq!(name.as_deref(), Some("sleep"), "the child's program name");
    }

    /// Our own process answers too — the case a pane at a bare prompt hits, where
    /// the process asked about is the shell itself.
    #[test]
    fn this_process_can_be_asked_about_itself() {
        let me = std::process::id();
        let cwd = cwd_of(me).expect("our own cwd is readable");
        assert_eq!(
            std::path::Path::new(&cwd).canonicalize().ok(),
            std::env::current_dir().ok().and_then(|d| d.canonicalize().ok())
        );
        let name = name_of(me).expect("our own name is readable");
        assert!(!name.is_empty() && !name.chars().any(char::is_control));
    }

    /// pid 0 and an out-of-range pid are not processes; on macOS they have
    /// special meanings to the `libproc` calls, so they must not reach them.
    #[test]
    fn non_pids_are_rejected_rather_than_queried() {
        assert_eq!(cwd_of(0), None);
        assert_eq!(name_of(0), None);
        assert_eq!(cwd_of(u32::MAX), None);
        assert_eq!(name_of(u32::MAX), None);
    }

    /// A pid that has gone is `None`, not a panic and not a stale answer — the
    /// case every refresh can hit, because the process a pane's title describes
    /// can exit between the walk that found it and the query about it.
    #[test]
    fn a_dead_pid_is_silent() {
        let mut child = std::process::Command::new("sleep").arg("5").spawn().expect("this test needs `sleep`");
        let pid = child.id();
        let _ = child.kill();
        let _ = child.wait(); // reaped: the pid is now free
        assert_eq!(cwd_of(pid), None);
        assert_eq!(name_of(pid), None);
        // pid 1 (init/launchd) exists but is root's: it must answer rather than
        // crash, whichever way it answers.
        let _ = cwd_of(1);
        let _ = name_of(1);
    }
}
