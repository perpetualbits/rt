//! `RT_TERM` — the one-off override — end to end, on both engines.
//!
//! **This file contains exactly one `#[test]` on purpose.** It calls `std::env::set_var`,
//! which mutates state shared by every thread in the process, and cargo runs a test
//! binary's tests on many threads at once. A second test here would race this one; the
//! frozen default and the config path therefore live in their own binary (`term.rs`),
//! which never touches the environment. Test *binaries* are separate processes, so they
//! cannot interfere with each other.
//!
//! Both engines are constructed by name rather than through `TermPane::spawn_env`, whose
//! backend comes from the ambient `RT_ENGINE` — see `term.rs` for why that matters.

use rt_engine::{AlacPane, TermPane, DEFAULT_SCROLLBACK};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// A name that is not a real terminfo entry anywhere, so nothing but this test's own
/// `set_var` can produce it.
const OVERRIDE: &str = "rt-env-override";

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

#[test]
fn rt_term_overrides_the_default_and_the_config_on_both_engines() {
    std::env::set_var(rt_config::TERM_ENV, OVERRIDE);

    // 1. Precedence, at the resolver: the env var beats a configured value, and beats the
    //    default. This is rt's rule — RT_TERM is the one-off you export to measure
    //    something, so nothing written to config.toml may quietly change what you measured.
    assert_eq!(rt_config::term_name(None), OVERRIDE, "env beats the default");
    assert_eq!(rt_config::term_name(Some("xterm-kitty")), OVERRIDE, "env beats the config");

    // 2. And at the PTY, for each engine explicitly. A host with no config file (rt-mux,
    //    a test) passes no TERM in `env`, so what the child sees is the engine's own
    //    resolution — which must honour the override.
    let shell = Some((
        "/bin/sh".to_string(),
        vec!["-c".to_string(), "printf '[%s]' \"$TERM\"".to_string()],
    ));
    let budget = Arc::new(rt_engine::budget::Budget::default());
    let vendored = TermPane::Alac(
        AlacPane::spawn_env(shell.clone(), None, 80, 24, &[], DEFAULT_SCROLLBACK)
            .expect("vendored pane spawns"),
    );
    assert_eq!(child_term(&vendored), OVERRIDE, "vendored engine ignored RT_TERM");
    let in_house = TermPane::spawn_vt_env(shell, None, 80, 24, &[], DEFAULT_SCROLLBACK, &budget)
        .expect("in-house pane spawns");
    assert_eq!(child_term(&in_house), OVERRIDE, "in-house engine ignored RT_TERM");
}
