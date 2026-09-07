//! What changes when rt is launched from Finder instead of from a shell.
//!
//! A double-clicked `.app` is started by LaunchServices, not by a shell, and it
//! inherits **launchd's** GUI-session environment rather than a terminal's. Two
//! differences matter to a terminal emulator:
//!
//! 1. **The working directory is `/`.** LaunchServices hands a bundled app `/`
//!    as its cwd, always. rt spawns every pane's shell with
//!    `working_directory: None`, i.e. "inherit mine" — so a Finder-launched rt
//!    would open every pane sitting in the root of the filesystem. That is not
//!    broken, but it is wrong: `Terminal.app` and `iTerm2` both open in `$HOME`,
//!    and a prompt in `/` is a surprise you have to notice and undo by hand.
//!    [`normalise_working_directory`] fixes it before the first pane is spawned.
//!
//! 2. **`PATH` is launchd's, not a shell's.** That one needs no code: rt's panes
//!    go through `/usr/bin/login` → a *login* shell, and a login shell sources
//!    `/etc/zprofile` (or `/etc/profile`), which runs `/usr/libexec/path_helper`
//!    and builds the full `PATH` from `/etc/paths` and `/etc/paths.d`. The shell
//!    inside the pane therefore has the same `PATH` it would have in
//!    Terminal.app, whatever rt's own `PATH` was. Likewise `SHELL`, `HOME` and
//!    `USER`: launchd sets all three for GUI apps, and the PTY layer falls back
//!    to the `passwd` entry (`getpwuid`) for any that is missing, so a bare
//!    environment still resolves the user's real login shell.
//!
//! Everything here is compiled on every platform — only the one call site in
//! `main` is `cfg`'d — so the decision is unit-tested on Linux CI, which is
//! where rt's tests actually run.

// Same reason as `wgpu_frame` and `vibrancy_policy`: the file is compiled everywhere so
// its decisions are tested on Linux CI, but only macOS calls into it.
#![cfg_attr(not(target_os = "macos"), allow(dead_code))]

use std::path::{Path, PathBuf};

/// The `.app` directory `exe` lives in, if it lives in one.
///
/// A macOS application bundle always has the shape
/// `Something.app/Contents/MacOS/<executable>`, so that is exactly what this
/// matches — not "the path contains `.app`", which would fire on a binary that
/// merely sits under a directory with that name.
pub fn bundle_root(exe: &Path) -> Option<&Path> {
    let macos = exe.parent()?; // …/Contents/MacOS
    let contents = macos.parent()?; // …/Contents
    let app = contents.parent()?; // …/Something.app
    (macos.file_name()? == "MacOS"
        && contents.file_name()? == "Contents"
        && app.extension().is_some_and(|e| e == "app"))
    .then_some(app)
}

/// Whether a process started as `exe` with working directory `cwd` should be
/// moved to the user's home directory.
///
/// Both conditions must hold, and the second is what keeps a terminal launch
/// untouched: running `rt.app/Contents/MacOS/rt` by hand from a project
/// directory keeps that directory, because only LaunchServices leaves the cwd
/// at `/`.
///
/// A third safeguard falls out of macOS for free, and was measured rather than
/// assumed: `std::env::current_exe()` on macOS returns the path the process was
/// *exec'd through*, symlink and all — it does not resolve to the real file. So
/// an `rt` invoked through a `/usr/local/bin/rt` symlink into the bundle reports
/// `/usr/local/bin/rt` here, never matches [`bundle_root`], and is never
/// relocated whatever its cwd.
pub fn should_relocate(exe: &Path, cwd: &Path) -> bool {
    bundle_root(exe).is_some() && cwd == Path::new("/")
}

/// The user's home directory: `$HOME` when launchd set it (it does, for GUI
/// apps), otherwise the `passwd` entry, which is set even in a truly bare
/// environment.
pub fn home_dir() -> Option<PathBuf> {
    if let Some(h) = std::env::var_os("HOME") {
        let p = PathBuf::from(h);
        if p.is_absolute() {
            return Some(p);
        }
    }
    passwd_home()
}

#[cfg(unix)]
fn passwd_home() -> Option<PathBuf> {
    use std::ffi::{CStr, OsStr};
    use std::os::unix::ffi::OsStrExt;
    // SAFETY: getpwuid returns a pointer into a static buffer owned by libc; we
    // copy out of it immediately and never keep it. A null return means "no such
    // entry", which is a None here.
    let pw = unsafe { libc::getpwuid(libc::getuid()) };
    if pw.is_null() {
        return None;
    }
    let dir = unsafe { (*pw).pw_dir };
    if dir.is_null() {
        return None;
    }
    let bytes = unsafe { CStr::from_ptr(dir) }.to_bytes();
    (!bytes.is_empty()).then(|| PathBuf::from(OsStr::from_bytes(bytes)))
}

#[cfg(not(unix))]
fn passwd_home() -> Option<PathBuf> {
    None
}

/// Move a Finder-launched bundle out of `/` and into the user's home, so the
/// panes rt is about to spawn start where a Mac user expects them to.
///
/// Called once from `main`, before any pane exists and before any thread is
/// started — `chdir` is process-wide, so doing it later would be a race.
/// A failure is ignored on purpose: the worst case is panes in `/`, which is
/// exactly where they would have been anyway, and rt's policy is that nothing
/// cosmetic may keep the terminal from opening.
pub fn normalise_working_directory() {
    let Ok(exe) = std::env::current_exe() else { return };
    let Ok(cwd) = std::env::current_dir() else { return };
    if !should_relocate(&exe, &cwd) {
        return;
    }
    if let Some(home) = home_dir() {
        let _ = std::env::set_current_dir(&home);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_a_real_bundle_layout() {
        let exe = Path::new("/Applications/rt.app/Contents/MacOS/rt");
        assert_eq!(bundle_root(exe), Some(Path::new("/Applications/rt.app")));
    }

    #[test]
    fn rejects_paths_that_merely_mention_app() {
        for p in [
            "/usr/local/bin/rt",
            "/Users/x/.cargo/bin/rt",
            "/Users/x/rt.app/rt",                    // no Contents/MacOS
            "/Users/x/rt.app/Contents/Resources/rt", // wrong leaf dir
            "/Users/x/my.app.stuff/Contents/MacOS/rt", // extension is not `app`
            "/Users/x/Contents/MacOS/rt",            // no `.app` above Contents
        ] {
            assert_eq!(bundle_root(Path::new(p)), None, "{p} should not look like a bundle");
        }
    }

    #[test]
    fn relocates_only_a_bundle_left_in_the_root() {
        let bundled = Path::new("/Applications/rt.app/Contents/MacOS/rt");
        let plain = Path::new("/Users/x/.cargo/bin/rt");
        assert!(should_relocate(bundled, Path::new("/")), "Finder launch: move to $HOME");
        assert!(
            !should_relocate(bundled, Path::new("/Users/x/src/rt")),
            "the bundle run by hand from a project dir keeps that dir"
        );
        assert!(!should_relocate(plain, Path::new("/")), "a plain binary is never relocated");
        assert!(!should_relocate(plain, Path::new("/Users/x")), "and neither is one in $HOME");
    }

    #[test]
    fn home_is_absolute_or_absent() {
        // Whatever the environment, `home_dir` never hands back a relative path
        // (a relative `$HOME` would `chdir` somewhere arbitrary).
        if let Some(h) = home_dir() {
            assert!(h.is_absolute(), "home_dir returned a relative path: {}", h.display());
        }
    }
}
