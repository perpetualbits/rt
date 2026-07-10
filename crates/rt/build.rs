//! Stamp the git commit into the binary so `rt --version` on a from-source
//! build identifies exactly what's running (e.g. `rt 0.2.1 (a1b2c3d)`), not
//! just the released crate version. Falls back to an empty stamp — and thus a
//! plain `rt <version>` — when git or the repository isn't available (a packaged
//! tarball, say), so this never breaks a build.

use std::path::Path;
use std::process::Command;

fn main() {
    println!("cargo:rustc-env=RT_GIT_DESC={}", git_desc());

    // Re-run when HEAD moves or the tracked working tree changes, so the stamp
    // doesn't go stale between commits. Paths are relative to this crate dir.
    for p in ["../../.git/HEAD", "../../.git/index"] {
        if Path::new(p).exists() {
            println!("cargo:rerun-if-changed={p}");
        }
    }
}

/// Short commit hash, suffixed with `-dirty` when tracked files differ from
/// HEAD. Empty string if git isn't usable here.
fn git_desc() -> String {
    let out = Command::new("git").args(["rev-parse", "--short", "HEAD"]).output();
    let hash = match out {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout).trim().to_string(),
        _ => return String::new(),
    };
    if hash.is_empty() {
        return String::new();
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
