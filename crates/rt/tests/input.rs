//! Tests for winit-key → Chord and typed-key → PTY-bytes translation. These are
//! pure and need no display, so they guard the fiddliest part of the app.

use rt_app::{chord_from_winit, encode_key};
use rt_config::{Action, Chord, Key as RtKey, Keymap, Mods};
use winit::keyboard::{Key, ModifiersState, NamedKey, SmolStr};

/// Helper: a winit character key from a `&str`.
fn ch(s: &str) -> Key {
    Key::Character(SmolStr::new(s)) // winit stores chars as small strings
}

#[test]
fn ctrl_shift_o_maps_to_split_horiz() {
    // Ctrl+Shift+O should resolve, through the default keymap, to SplitHoriz.
    let mods = ModifiersState::CONTROL | ModifiersState::SHIFT; // held modifiers
    let chord = chord_from_winit(&ch("o"), mods).expect("maps to a chord");
    // The chord equals the parsed Terminator accelerator.
    assert_eq!(chord, Chord::parse("<Shift><Control>o").unwrap());
    // And the default keymap turns it into the split action.
    assert_eq!(Keymap::defaults().action_for(&chord), Some(Action::SplitHoriz));
}

#[test]
fn alt_arrows_map_to_focus_moves() {
    // Alt+Left is go_left in the default map.
    let chord = chord_from_winit(&Key::Named(NamedKey::ArrowLeft), ModifiersState::ALT).unwrap();
    assert_eq!(chord.key, RtKey::Left);
    assert!(chord.mods.contains(Mods::ALT));
    assert_eq!(Keymap::defaults().action_for(&chord), Some(Action::GoLeft));
}

#[test]
fn plain_char_has_no_binding_but_encodes_to_bytes() {
    // A bare 'a' is not a binding (no modifiers) — the keymap misses...
    let chord = chord_from_winit(&ch("a"), ModifiersState::empty()).unwrap();
    assert_eq!(Keymap::defaults().action_for(&chord), None);
    // ...so it falls through to encoding: 'a' -> the byte 'a'.
    assert_eq!(encode_key(&ch("a"), ModifiersState::empty(), false), Some(b"a".to_vec()));
}

#[test]
fn control_letter_encodes_c0_control_code() {
    // Ctrl-C must send 0x03 (ETX), the interrupt control code.
    assert_eq!(encode_key(&ch("c"), ModifiersState::CONTROL, false), Some(vec![0x03]));
    // Ctrl-A sends 0x01.
    assert_eq!(encode_key(&ch("a"), ModifiersState::CONTROL, false), Some(vec![0x01]));
}

#[test]
fn special_keys_encode_ansi_sequences() {
    // Enter -> CR; Backspace -> DEL; Insert -> CSI 2~ (the mc/editor toggle).
    assert_eq!(encode_key(&Key::Named(NamedKey::Enter), ModifiersState::empty(), false), Some(b"\r".to_vec()));
    assert_eq!(encode_key(&Key::Named(NamedKey::Backspace), ModifiersState::empty(), false), Some(vec![0x7f]));
    assert_eq!(encode_key(&Key::Named(NamedKey::Insert), ModifiersState::empty(), false), Some(b"\x1b[2~".to_vec()));
    // Function keys: F1 = SS3 P, F5 = CSI 15~.
    assert_eq!(encode_key(&Key::Named(NamedKey::F1), ModifiersState::empty(), false), Some(b"\x1bOP".to_vec()));
    assert_eq!(encode_key(&Key::Named(NamedKey::F5), ModifiersState::empty(), false), Some(b"\x1b[15~".to_vec()));
}

#[test]
fn arrows_respect_application_cursor_mode() {
    // The bug behind "mc arrows don't work": in application-cursor mode arrows
    // must be SS3 (ESC O A), not CSI (ESC [ A).
    let up = Key::Named(NamedKey::ArrowUp);
    assert_eq!(encode_key(&up, ModifiersState::empty(), false), Some(b"\x1b[A".to_vec())); // normal
    assert_eq!(encode_key(&up, ModifiersState::empty(), true), Some(b"\x1bOA".to_vec())); // app-cursor
    // Home/End follow the same rule.
    assert_eq!(encode_key(&Key::Named(NamedKey::Home), ModifiersState::empty(), true), Some(b"\x1bOH".to_vec()));
}

#[test]
fn encode_text_sends_composed_and_ctrl() {
    use rt_app::input::encode_text;
    // A composed dead-key result (e.g. '+space -> ') is sent as its text.
    assert_eq!(encode_text("'", ModifiersState::empty()), b"'".to_vec());
    assert_eq!(encode_text("ñ", ModifiersState::empty()), "ñ".as_bytes().to_vec());
    // Ctrl + letter -> C0 control code.
    assert_eq!(encode_text("c", ModifiersState::CONTROL), vec![0x03]);
    // Alt (Meta) prefixes ESC.
    assert_eq!(encode_text("x", ModifiersState::ALT), vec![0x1b, b'x']);
}

#[test]
fn sequence_keys_are_classified() {
    use rt_app::input::is_sequence_key;
    assert!(is_sequence_key(&NamedKey::ArrowUp));
    assert!(is_sequence_key(&NamedKey::Enter));
    assert!(is_sequence_key(&NamedKey::F5));
    // A named key rt does not encode is NOT a sequence key: it must fall
    // through to the text path, which is what lets a dead key + space compose
    // correctly (space itself is a *character* key, `Key::Character(" ")`, and
    // so never reaches this classifier at all).
    assert!(!is_sequence_key(&NamedKey::BrowserBack));
}

// ── The legacy encoding is frozen ──────────────────────────────────────────────
//
// The kitty keyboard protocol only changes what rt sends *after* an application
// has negotiated for it. An application that never asks — bash, vim, less, and
// every program written before 2021 — must keep receiving exactly the bytes it
// received before the protocol existed. The test below is that guarantee, and it
// is deliberately exhaustive rather than representative: it pins every key
// `encode_key` handles, under every modifier combination, in both cursor modes.
// If a future change to the kitty path leaks into the un-negotiated path, this is
// what fails.

/// Every modifier combination rt distinguishes, as a `(name, ModifiersState)`
/// pair so a failure names the combination rather than a bitmask.
fn all_mod_combos() -> Vec<(&'static str, ModifiersState)> {
    let (s, c, a, m) = (
        ModifiersState::SHIFT,
        ModifiersState::CONTROL,
        ModifiersState::ALT,
        ModifiersState::META,
    );
    vec![
        ("none", ModifiersState::empty()),
        ("shift", s),
        ("ctrl", c),
        ("alt", a),
        ("super", m),
        ("ctrl+shift", c | s),
        ("alt+shift", a | s),
        ("ctrl+alt", c | a),
        ("ctrl+alt+shift", c | a | s),
        ("super+shift", m | s),
    ]
}

#[test]
fn legacy_named_keys_are_byte_identical_under_every_modifier() {
    // (key, bytes in normal cursor mode, bytes in application cursor mode).
    // Modifiers do NOT appear: without a negotiated protocol every one of these
    // keys ignores them, which is precisely the behaviour being frozen.
    let expect: &[(NamedKey, &[u8], &[u8])] = &[
        (NamedKey::Enter, b"\r", b"\r"),
        (NamedKey::Backspace, &[0x7f], &[0x7f]),
        (NamedKey::Tab, b"\t", b"\t"),
        (NamedKey::Escape, &[0x1b], &[0x1b]),
        // Cursor keys and Home/End switch CSI -> SS3 on DECCKM, and nothing else.
        (NamedKey::ArrowUp, b"\x1b[A", b"\x1bOA"),
        (NamedKey::ArrowDown, b"\x1b[B", b"\x1bOB"),
        (NamedKey::ArrowRight, b"\x1b[C", b"\x1bOC"),
        (NamedKey::ArrowLeft, b"\x1b[D", b"\x1bOD"),
        (NamedKey::Home, b"\x1b[H", b"\x1bOH"),
        (NamedKey::End, b"\x1b[F", b"\x1bOF"),
        (NamedKey::Insert, b"\x1b[2~", b"\x1b[2~"),
        (NamedKey::Delete, b"\x1b[3~", b"\x1b[3~"),
        (NamedKey::PageUp, b"\x1b[5~", b"\x1b[5~"),
        (NamedKey::PageDown, b"\x1b[6~", b"\x1b[6~"),
        (NamedKey::F1, b"\x1bOP", b"\x1bOP"),
        (NamedKey::F2, b"\x1bOQ", b"\x1bOQ"),
        (NamedKey::F3, b"\x1bOR", b"\x1bOR"),
        (NamedKey::F4, b"\x1bOS", b"\x1bOS"),
        (NamedKey::F5, b"\x1b[15~", b"\x1b[15~"),
        (NamedKey::F6, b"\x1b[17~", b"\x1b[17~"),
        (NamedKey::F7, b"\x1b[18~", b"\x1b[18~"),
        (NamedKey::F8, b"\x1b[19~", b"\x1b[19~"),
        (NamedKey::F9, b"\x1b[20~", b"\x1b[20~"),
        (NamedKey::F10, b"\x1b[21~", b"\x1b[21~"),
        (NamedKey::F11, b"\x1b[23~", b"\x1b[23~"),
        (NamedKey::F12, b"\x1b[24~", b"\x1b[24~"),
    ];
    for (named, normal, app) in expect {
        let key = Key::Named(*named);
        for (name, mods) in all_mod_combos() {
            assert_eq!(
                encode_key(&key, mods, false).as_deref(),
                Some(*normal),
                "{named:?} + {name} (normal cursor mode) must not change"
            );
            assert_eq!(
                encode_key(&key, mods, true).as_deref(),
                Some(*app),
                "{named:?} + {name} (application cursor mode) must not change"
            );
        }
    }
}

#[test]
fn legacy_character_keys_are_byte_identical() {
    // Plain typing, Shift-typing (winit hands us the shifted logical key) and
    // Ctrl-letters, all unaffected by the protocol until it is negotiated.
    for (name, mods) in all_mod_combos() {
        let ctrl = mods.control_key();
        // A lower-case letter: C0 control code under Ctrl, the letter otherwise.
        let want: Vec<u8> = if ctrl { vec![0x03] } else { b"c".to_vec() };
        assert_eq!(encode_key(&ch("c"), mods, false).as_deref(), Some(&want[..]), "'c' + {name}");
        // The shifted logical key is an upper-case letter; Ctrl still folds case.
        let want: Vec<u8> = if ctrl { vec![0x03] } else { b"C".to_vec() };
        assert_eq!(encode_key(&ch("C"), mods, false).as_deref(), Some(&want[..]), "'C' + {name}");
        // A digit has no control code, so it is sent literally even under Ctrl.
        assert_eq!(encode_key(&ch("1"), mods, false).as_deref(), Some(&b"1"[..]), "'1' + {name}");
        // Non-ASCII typing goes out as UTF-8.
        assert_eq!(
            encode_key(&ch("ä"), mods, false).as_deref(),
            Some("ä".as_bytes()),
            "'ä' + {name}"
        );
    }
    // A named key rt does not encode still produces nothing at all.
    assert_eq!(encode_key(&Key::Named(NamedKey::BrowserBack), ModifiersState::empty(), false), None);
}
