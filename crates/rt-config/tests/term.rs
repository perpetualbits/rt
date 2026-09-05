//! The `term` setting: its frozen default, its precedence against `RT_TERM`, and the
//! names `normalize` refuses.
//!
//! Every assertion here is pure — the environment is passed to `term_name_from` rather
//! than exported — because `set_var` is process-global and would race the other tests in
//! this binary. The env var's *real* effect is proved end-to-end, on a live PTY and on
//! both engines, in `rt-engine`'s single-test `term_env.rs`.

use rt_config::{term_name_from, valid_term_name, Settings, DEFAULT_TERM, TERM_ENV};

/// Frozen. rt's `TERM` is a promise made to every program the user runs, and changing it
/// can break `vim`, `less` and `htop` on any machine missing the named terminfo entry —
/// so it may only ever move as a deliberate, reviewed decision, never as a side effect.
#[test]
fn the_default_term_is_exactly_xterm_256color() {
    assert_eq!(DEFAULT_TERM, "xterm-256color");
    assert_eq!(Settings::default().term, "xterm-256color");
    // And the override is spelled RT_TERM; the name is user-facing documentation.
    assert_eq!(TERM_ENV, "RT_TERM");
}

/// The precedence rule, stated as one table: env > config > default.
#[test]
fn the_env_var_beats_the_config_which_beats_the_default() {
    assert_eq!(term_name_from(None, None), "xterm-256color", "neither set → default");
    assert_eq!(term_name_from(None, Some("xterm-kitty")), "xterm-kitty", "config alone");
    assert_eq!(term_name_from(Some("rt"), None), "rt", "env alone");
    assert_eq!(term_name_from(Some("rt"), Some("xterm-kitty")), "rt", "env wins over config");
}

/// A blank or malformed value at ANY level falls through to the next one rather than
/// being exported: a `TERM` with a space or a slash in it cannot name a terminfo entry,
/// so exporting it only moves the failure into the child process.
#[test]
fn unusable_names_fall_through_instead_of_being_exported() {
    assert_eq!(term_name_from(Some(""), Some("xterm-kitty")), "xterm-kitty", "empty env");
    assert_eq!(term_name_from(Some("  "), Some("xterm-kitty")), "xterm-kitty", "blank env");
    assert_eq!(term_name_from(Some("bad name"), None), DEFAULT_TERM, "space in env");
    assert_eq!(term_name_from(Some("../etc/passwd"), None), DEFAULT_TERM, "path in env");
    assert_eq!(term_name_from(None, Some("")), DEFAULT_TERM, "empty config");
    // Surrounding whitespace is a typo, not a rejection.
    assert_eq!(term_name_from(Some(" rt "), None), "rt");
}

#[test]
fn valid_term_name_accepts_real_entry_names_and_rejects_the_rest() {
    for ok in ["rt", "xterm-256color", "xterm-kitty", "screen.linux", "foot+base", "vt100"] {
        assert!(valid_term_name(ok), "{ok} is a real terminfo entry name");
    }
    for bad in ["", " ", "a b", "a/b", "a\0b", "café", &"x".repeat(65)] {
        assert!(!valid_term_name(bad), "{bad:?} must be rejected");
    }
}

/// A hand-edited `config.toml` bypasses the Preferences picker, so `normalize` is the
/// last gate before the value reaches a child's environment.
#[test]
fn normalize_replaces_an_unusable_term_and_trims_a_sloppy_one() {
    let mut s = Settings { term: "not a term".to_string(), ..Settings::default() };
    s.normalize();
    assert_eq!(s.term, DEFAULT_TERM, "a name with a space is refused");

    let mut s = Settings { term: String::new(), ..Settings::default() };
    s.normalize();
    assert_eq!(s.term, DEFAULT_TERM, "an empty name is refused");

    let mut s = Settings { term: "  xterm-kitty  ".to_string(), ..Settings::default() };
    s.normalize();
    assert_eq!(s.term, "xterm-kitty", "whitespace is trimmed, the name kept");

    // A name rt cannot verify is still accepted: whether the entry EXISTS is a per-machine
    // question (and differs on every host you ssh to), so refusing it here would be rt
    // guessing. The cost of getting it wrong is documented on the setting itself.
    let mut s = Settings { term: "some-terminal-rt-never-heard-of".to_string(), ..Settings::default() };
    s.normalize();
    assert_eq!(s.term, "some-terminal-rt-never-heard-of");
}

/// Round-trip through the config file, since `#[serde(default)]` means an old file simply
/// omits the key — and must then get the default, not an empty string.
#[test]
fn an_old_config_file_without_a_term_key_still_gets_the_default() {
    let cfg: rt_config::Config = toml::from_str("[settings]\nfont_size = 20.0\n").expect("parses");
    assert_eq!(cfg.settings.term, DEFAULT_TERM);
    let text = toml::to_string_pretty(&cfg).expect("serialises");
    assert!(text.contains("term = \"xterm-256color\""), "the setting is visible in the file:\n{text}");
}

/// The Preferences picker only ever offers names this machine has terminfo for — one
/// arrow-key press must not be able to break every ncurses application in the next pane.
/// The configured value is the exception: it stays in the list even when its entry is
/// missing, so a value hand-written into `config.toml` is visible and can be stepped off.
#[test]
fn the_picker_offers_the_default_and_never_drops_the_configured_value() {
    let list = rt_config::term_candidates("some-terminal-rt-never-heard-of");
    assert_eq!(list[0], DEFAULT_TERM, "the safe default leads the list");
    assert!(
        list.contains(&"some-terminal-rt-never-heard-of".to_string()),
        "the configured value must stay visible: {list:?}"
    );
    for name in &list {
        // Every offered name must at least be usable as a name.
        assert!(valid_term_name(name), "{name} is not a terminfo entry name");
    }
    // Offering the default twice would make the cycle stutter.
    let mut sorted = list.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(sorted.len(), list.len(), "duplicate candidates: {list:?}");

    // `xterm-256color` is the one entry we can rely on being present wherever ncurses is,
    // so it is also the one case where the installed-check has a knowable answer.
    assert!(
        rt_config::terminfo_installed(DEFAULT_TERM) || !rt_config::terminfo_installed("xterm"),
        "terminfo lookup found `xterm` but not `xterm-256color`, which cannot be right"
    );
    assert!(!rt_config::terminfo_installed("definitely-not-a-terminal-9137"));
    assert!(!rt_config::terminfo_installed("../../etc/passwd"), "never walk a path");
}

// ── rt's own terminfo entry ──────────────────────────────────────────────────
//
// `extra/rt.terminfo` describes what rt actually implements. It is groundwork — nothing
// sets `TERM=rt` — but a description that has quietly rotted is worse than none, so it is
// checked here rather than only when a user installs it.

fn terminfo_source() -> std::path::PathBuf {
    // crates/rt-config → repo root.
    std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../extra/rt.terminfo")
}

/// `tic -c` compiles the entry into nothing and reports syntax errors and dubious
/// capabilities — the only mechanical check there is that the file is a real terminfo
/// source. Skipped (loudly) where `tic` is absent, since ncurses' compiler is not
/// something a Rust test can ship.
#[test]
fn the_terminfo_entry_compiles() {
    let path = terminfo_source();
    assert!(path.exists(), "{} is missing", path.display());
    // `-x` matters: several capabilities in the entry are user-defined extensions (Tc,
    // BE/BD, Ss/Se, XM/xm, setrgbf/setrgbb) and without it tic rejects them.
    match std::process::Command::new("tic").args(["-c", "-x"]).arg(&path).output() {
        Ok(out) => assert!(
            out.status.success(),
            "tic -c -x rejected {}:\n{}{}",
            path.display(),
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr),
        ),
        Err(e) => eprintln!("skipping: tic is not installed on this machine ({e})"),
    }
}

/// The entry's whole value is that it does NOT claim what rt cannot do. These are the
/// capabilities that were removed from the xterm-256color base, each for a reason recorded
/// in the file itself; a later "let me just copy kitty's entry" would silently undo that,
/// so the absences are asserted rather than trusted.
#[test]
fn the_terminfo_entry_claims_nothing_rt_does_not_implement() {
    let text = std::fs::read_to_string(terminfo_source()).expect("the entry is readable");
    // Comment lines carry the *explanations* for these absences, so they must not be
    // searched — only the capability lines are the claim.
    let caps: String =
        text.lines().filter(|l| !l.trim_start().starts_with('#')).collect::<Vec<_>>().join("\n");
    let forbidden = [
        ("bel=", "BEL is ignored: rt has no audible or visual bell"),
        ("flash=", "DECSCNM (?5) reverse video is not implemented"),
        ("blink=", "SGR 5/6 blink is not implemented"),
        ("rep=", "REP (CSI b) is not implemented; ncurses would corrupt output with it"),
        ("hts=", "rt tracks no tab stops (hard-coded every 8 columns)"),
        ("tbc=", "rt tracks no tab stops"),
        ("initc=", "OSC 4 palette redefinition is not implemented"),
        ("oc=", "OSC 104 palette reset is not implemented"),
        ("Ms=", "OSC 52 clipboard is parsed and ignored"),
        ("Cr=", "OSC 112 cursor-colour reset is not implemented"),
        ("Cs=", "OSC 12 cursor colour is not implemented"),
        ("Smulx=", "SGR 4:3 styled underline is not implemented"),
        ("Setulc=", "SGR 58/59 underline colour is not implemented"),
        ("Sync=", "rt's sync is DECSET ?2026; it handles no DCS at all"),
        ("fullkbd", "rt honours only kitty flag 1 (disambiguate), not the full protocol"),
        ("kcbt=", "Shift+Tab sends a plain tab: the legacy encoder drops modifiers"),
        ("kLFT", "modified keys send the UNMODIFIED sequence"),
        ("kRIT", "modified keys send the UNMODIFIED sequence"),
        ("kf13=", "rt encodes F1-F12 only"),
        ("RGB,", "ncurses would derive a nonsensical bit depth from colors#256; Tc is correct"),
    ];
    for (cap, why) in forbidden {
        assert!(!caps.contains(cap), "extra/rt.terminfo claims `{cap}` — but {why}");
    }
    // And the things rt genuinely does, which must not be lost either.
    for cap in ["Tc,", "BE=\\E[?2004h", "Ss=", "E3=\\E[3J", "smcup=\\E[?1049h", "bce,"] {
        assert!(caps.contains(cap), "extra/rt.terminfo lost `{cap}`, which rt does implement");
    }
}
