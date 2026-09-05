//! What `$TERM` a pane's shell actually sees.
//!
//! rt describes itself to every program it runs through one environment variable, so this
//! is a contract test, not a smoke test: an application decides whether to negotiate the
//! kitty keyboard protocol, which colour depth to assume, and whether to run at all, from
//! this string. The default is frozen here deliberately — changing it is a decision with
//! machine-wide blast radius (a name whose terminfo is missing breaks every ncurses
//! application), and it must never drift as a side effect of some other change.
//!
//! **Both engines, explicitly.** `TermPane::spawn_env` picks its backend from the ambient
//! `RT_ENGINE`, so a test that used it would silently exercise only whichever engine the
//! developer's shell happened to export — and this is exactly the kind of "one engine
//! quietly disagrees with the other" bug the seam invites. Each engine is therefore
//! constructed by name here (`AlacPane::spawn_env` / `TermPane::spawn_vt_env`), which needs
//! no process-global state and cannot be overridden from outside.
//!
//! `RT_TERM` is NOT exercised here: `set_var` is process-global and would race the other
//! tests in this binary. It has its own single-test binary, `term_env.rs`.

use rt_engine::{AlacPane, TermPane, DEFAULT_SCROLLBACK};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn budget() -> Arc<rt_engine::budget::Budget> {
    Arc::new(rt_engine::budget::Budget::default())
}

/// A shell that prints its own `$TERM`, bracketed so a substring assertion cannot be
/// satisfied by a prefix (`xterm-256color` contains `xterm`, and `rt` contains nothing
/// useful at all).
fn echo_term() -> Option<(String, Vec<String>)> {
    Some((
        "/bin/sh".to_string(),
        vec!["-c".to_string(), "printf '[%s]' \"$TERM\"".to_string()],
    ))
}

/// Read what the child printed, polling until it appears. The reader thread parses
/// asynchronously, so a fixed sleep would be either slow or flaky.
fn child_term(pane: &TermPane) -> String {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let text = pane.snapshot().to_text();
        if let Some(open) = text.find('[') {
            if let Some(close) = text[open..].find(']') {
                return text[open + 1..open + close].to_string();
            }
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("the child never printed its $TERM");
}

fn vendored(env: &[(String, String)]) -> TermPane {
    TermPane::Alac(
        AlacPane::spawn_env(echo_term(), None, 80, 24, env, DEFAULT_SCROLLBACK)
            .expect("vendored pane spawns"),
    )
}

fn in_house(env: &[(String, String)]) -> TermPane {
    TermPane::spawn_vt_env(echo_term(), None, 80, 24, env, DEFAULT_SCROLLBACK, &budget())
        .expect("in-house pane spawns")
}

/// The frozen default. `xterm-256color` is installed on every machine that has ncurses at
/// all, and rt's own sequences are a superset of what it claims — see `rt_config::term`'s
/// documentation for why moving off it is a user decision rt must not make for them.
#[test]
fn the_default_term_is_exactly_xterm_256color_on_both_engines() {
    assert_eq!(rt_config::DEFAULT_TERM, "xterm-256color", "the default TERM must not drift");
    assert_eq!(rt_config::Settings::default().term, "xterm-256color");
    assert_eq!(child_term(&vendored(&[])), "xterm-256color", "vendored engine");
    assert_eq!(child_term(&in_house(&[])), "xterm-256color", "in-house engine");
}

/// A host that has resolved a configured `TERM` passes it in the spawn env, which is
/// applied after the engine's own default and therefore wins. This is the path a
/// `term = "..."` line in `config.toml` actually travels.
#[test]
fn a_host_supplied_term_reaches_the_child_on_both_engines() {
    // A name no real terminfo entry uses, so a pass cannot come from anywhere else.
    let env = vec![("TERM".to_string(), "rt-test-term".to_string())];
    assert_eq!(child_term(&vendored(&env)), "rt-test-term", "vendored engine");
    assert_eq!(child_term(&in_house(&env)), "rt-test-term", "in-house engine");
}

/// `COLORTERM` is a separate promise (24-bit colour, which rt does resolve) and is not
/// part of this setting. Nothing about choosing a `TERM` may disturb it.
#[test]
fn colorterm_stays_truecolor_regardless_of_term() {
    let shell = Some((
        "/bin/sh".to_string(),
        vec!["-c".to_string(), "printf '[%s]' \"$COLORTERM\"".to_string()],
    ));
    let env = vec![("TERM".to_string(), "rt-test-term".to_string())];
    let pane = TermPane::spawn_vt_env(shell, None, 80, 24, &env, DEFAULT_SCROLLBACK, &budget())
        .expect("pane spawns");
    assert_eq!(child_term(&pane), "truecolor");
}
