//! Post-mortem breadcrumbs: a panic hook and a stderr sink.
//!
//! rt loses everything when it dies — every tab, every pane, and every shell
//! running under them. Three crashes had produced no evidence at all: apport
//! skips unpackaged binaries, so there is no core; the desktop launcher does
//! not keep stderr; and `catch_unwind` only guards per-pane parser and render
//! work, so a panic anywhere else unwinds and aborts in silence.
//!
//! So: write the panic where it can be found afterwards, and keep stderr when
//! nobody is watching it.

use std::io::Write;

/// `~/.cache/rt`, created on demand. `None` if we cannot determine or make it —
/// diagnostics must never be the reason rt fails to start.
fn dir() -> Option<std::path::PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(std::path::PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".cache")))?;
    let d = base.join("rt");
    std::fs::create_dir_all(&d).ok()?;
    Some(d)
}

/// Send stderr to `~/.cache/rt/stderr.log` when it is NOT a terminal.
///
/// Launched from a desktop, fd 2 goes nowhere and a Wayland protocol error or a
/// GL complaint — printed by a library on its way to killing us — is lost. Run
/// from a shell, stderr is the developer's and we leave it alone.
///
/// Appends, never truncates: the interesting run is usually the previous one.
pub fn capture_stderr_if_not_a_tty() {
    // SAFETY: isatty on a fd we do not own is a pure query.
    if unsafe { libc::isatty(libc::STDERR_FILENO) } == 1 {
        return;
    }
    let Some(path) = dir().map(|d| d.join("stderr.log")) else { return };
    let Ok(f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) else { return };
    // SAFETY: dup2 onto fd 2 with a fd we own; `f` is leaked deliberately so the
    // descriptor outlives this scope for the process's lifetime.
    unsafe {
        libc::dup2(std::os::unix::io::AsRawFd::as_raw_fd(&f), libc::STDERR_FILENO);
    }
    std::mem::forget(f);
    let _ = writeln!(std::io::stderr(), "\n=== {} started {} ===", crate::version_string(), now());
}

/// Append panics to `~/.cache/rt/crash.log`, then run the default hook.
///
/// Chained rather than replacing: stderr (now a file, per above) still gets the
/// standard message, and the log gets a timestamped copy with a backtrace even
/// when `RUST_BACKTRACE` is unset — which it always is for a desktop launch,
/// and a panic with no backtrace names a line but not the path that reached it.
pub fn install_panic_hook() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        if let Some(path) = dir().map(|d| d.join("crash.log")) {
            if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
                let bt = std::backtrace::Backtrace::force_capture();
                let loc = info
                    .location()
                    .map(|l| format!("{}:{}:{}", l.file(), l.line(), l.column()))
                    .unwrap_or_else(|| "<unknown location>".into());
                let msg = info
                    .payload()
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| info.payload().downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "<non-string panic payload>".into());
                let thread = std::thread::current().name().unwrap_or("<unnamed>").to_string();
                let _ = writeln!(
                    f,
                    "\n=== {} PANIC {} ===\nthread : {thread}\nat     : {loc}\nmessage: {msg}\n{bt}",
                    crate::version_string(),
                    now(),
                );
            }
        }
        previous(info);
    }));
}

/// Deliberately panic when `RT_CRASHLOG_SELFTEST=1`, so this instrumentation can be
/// proven to still work without waiting for a real crash.
///
/// Silent instrumentation rots: the hook could be dropped by a refactor, or the
/// cache directory could become unwritable, and nobody would learn until the
/// next crash produced nothing — which is exactly the situation this module was
/// written to end. One env var makes it checkable in a second.
pub fn selftest_if_asked() {
    if std::env::var_os("RT_CRASHLOG_SELFTEST").is_some_and(|v| v == "1") {
        panic!("RT_CRASHLOG_SELFTEST: deliberate panic to verify the crash log");
    }
}

/// Wall-clock stamp. Seconds since the epoch plus the local date via `date`
/// would need a dependency; the epoch alone is enough to line a crash up
/// against a journal entry, which is all this is for.
fn now() -> String {
    match std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) {
        Ok(d) => format!("epoch {}", d.as_secs()),
        Err(_) => "epoch <before 1970>".into(),
    }
}
