//! Stamp the git commit into the binary so `rt --version` on a from-source
//! build identifies exactly what's running (e.g. `rt 0.2.1 (a1b2c3d)`), not
//! just the released crate version. Falls back to `unknown` — so `rt
//! <version> (unknown)` — when git or the repository isn't available (a
//! packaged tarball, an rsynced worktree, a distro build), so this never
//! breaks a build, and a bug report never looks indistinguishable from one
//! with a stamp that was simply never generated.
//!
//! Packagers, CI, and cross-machine deploys that know the real commit (or
//! want a different marker) can set `RT_GIT_DESC` in the build environment;
//! when present it's used verbatim and git is never invoked.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=RT_GIT_DESC");
    if let Ok(desc) = std::env::var("RT_GIT_DESC") {
        println!("cargo:rustc-env=RT_GIT_DESC={desc}");
    } else {
        println!("cargo:rustc-env=RT_GIT_DESC={}", git_desc());
    }

    // Re-run when HEAD moves or the tracked working tree changes, so the stamp
    // doesn't go stale between commits. `../../.git` is only a directory in a
    // plain checkout; in a git worktree it's a *file* pointing elsewhere, so
    // ask git for the real paths instead of assuming the layout.
    for p in git_watch_paths() {
        if Path::new(&p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
}

/// Short commit hash, suffixed with `-dirty` when tracked files differ from
/// HEAD. `unknown` if git isn't usable here (no git binary, not a repo, a
/// worktree whose `.git` file points at a path that no longer exists — the
/// case hit when source is rsynced across machines without `.git`).
fn git_desc() -> String {
    let out = Command::new("git").args(["rev-parse", "--short", "HEAD"]).output();
    let hash = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return "unknown".to_string(),
    };
    if hash.is_empty() {
        return "unknown".to_string();
    }
    // `git diff --quiet HEAD` exits non-zero when tracked files differ from HEAD;
    // untracked files are ignored so stray scratch files don't read as "dirty".
    let dirty = Command::new("git")
        .args(["diff", "--quiet", "HEAD"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(false);
    if dirty { format!("{hash}-dirty") } else { hash }
}

/// HEAD and index paths to watch for `rerun-if-changed`, resolved via git
/// itself rather than assumed to live under `../../.git/` — that assumption
/// breaks in a git worktree, where `.git` is a file (`gitdir: <path>`) and
/// the real HEAD/index live under the main repo's `.git/worktrees/<name>/`
/// (HEAD) and `.git/` (a shared index, via `--git-common-dir`). If git isn't
/// available, return nothing to watch, same as the previous behaviour.
fn git_watch_paths() -> Vec<String> {
    let git_dir = match Command::new("git").args(["rev-parse", "--git-dir"]).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return Vec::new(),
    };
    let common_dir = match Command::new("git").args(["rev-parse", "--git-common-dir"]).output() {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => git_dir.clone(),
    };
    if git_dir.is_empty() {
        return Vec::new();
    }
    vec![format!("{git_dir}/HEAD"), format!("{common_dir}/index")]
}
