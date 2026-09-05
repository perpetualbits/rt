//! Tests for winit-key → Chord and typed-key → PTY-bytes translation. These are
//! pure and need no display, so they guard the fiddliest part of the app.

use rt_app::input::encode_key_kitty;
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

#[test]
fn shift_enter_is_distinguishable_once_the_protocol_is_negotiated() {
    // The reported bug: in Claude Code (and any app that asks for the kitty
    // keyboard protocol) Shift+Enter was byte-identical to Enter, so it could
    // never be bound to "insert a newline" — it always submitted.
    let enter = Key::Named(NamedKey::Enter);
    // Un-negotiated: both are a bare carriage return, as they always were.
    assert_eq!(encode_key(&enter, ModifiersState::empty(), false), Some(b"\r".to_vec()));
    assert_eq!(encode_key(&enter, ModifiersState::SHIFT, false), Some(b"\r".to_vec()));
    // With flag 1 ("disambiguate escape codes") pushed, Shift+Enter becomes the
    // CSI-u form: unicode key code 13 (CR), modifier parameter shift(1) + 1 = 2.
    assert_eq!(
        encode_key_kitty(&enter, ModifiersState::SHIFT, false, 0b1),
        Some(b"\x1b[13;2u".to_vec())
    );
    // Plain Enter under the same flag keeps the legacy byte: it is not ambiguous,
    // so disambiguation leaves it alone and `\r` still submits.
    assert_eq!(encode_key_kitty(&enter, ModifiersState::empty(), false, 0b1), Some(b"\r".to_vec()));
}

#[test]
fn flag_one_encodes_exactly_the_ambiguous_keys() {
    let (s, c, a) = (ModifiersState::SHIFT, ModifiersState::CONTROL, ModifiersState::ALT);
    let named = |n| Key::Named(n);
    // (key, modifiers, expected bytes with flag 1 active).
    let table: &[(Key, ModifiersState, &[u8])] = &[
        // The three control characters whose legacy byte has no room for a
        // modifier switch to the CSI-u form as soon as ANY modifier is held.
        (named(NamedKey::Enter), s, b"\x1b[13;2u"),
        (named(NamedKey::Enter), c, b"\x1b[13;5u"),
        (named(NamedKey::Enter), a, b"\x1b[13;3u"),
        (named(NamedKey::Enter), c | s, b"\x1b[13;6u"),
        (named(NamedKey::Tab), s, b"\x1b[9;2u"),
        (named(NamedKey::Backspace), s, b"\x1b[127;2u"),
        (named(NamedKey::Backspace), c, b"\x1b[127;5u"),
        // Escape is disambiguated even bare — that is the ambiguity the flag is
        // named for: a lone `\x1b` looks like the start of a sequence.
        (named(NamedKey::Escape), ModifiersState::empty(), b"\x1b[27u"),
        (named(NamedKey::Escape), s, b"\x1b[27;2u"),
        // Keys that already own a legacy escape sequence keep its final byte and
        // take the modifier as a parameter — they do NOT move to CSI-u.
        (named(NamedKey::ArrowUp), c, b"\x1b[1;5A"),
        (named(NamedKey::ArrowLeft), c, b"\x1b[1;5D"),
        (named(NamedKey::ArrowRight), c | s, b"\x1b[1;6C"),
        (named(NamedKey::Home), a, b"\x1b[1;3H"),
        (named(NamedKey::End), c | s, b"\x1b[1;6F"),
        (named(NamedKey::Delete), c, b"\x1b[3;5~"),
        (named(NamedKey::PageUp), c, b"\x1b[5;5~"),
        (named(NamedKey::F1), c, b"\x1b[1;5P"),
        // F3's legacy final byte `R` collides with a cursor-position report, so
        // the protocol re-spells it as `CSI 13 ~` rather than `CSI 1;5R`.
        (named(NamedKey::F3), c, b"\x1b[13;5~"),
        (named(NamedKey::F5), a, b"\x1b[15;3~"),
        (named(NamedKey::F12), c, b"\x1b[24;5~"),
        // Characters: the UNSHIFTED code point, with the shift bit in the
        // parameter. Ctrl+Shift+C is 99 (`c`) + shift + ctrl, not 67.
        (ch("c"), c, b"\x1b[99;5u"),
        (ch("C"), c | s, b"\x1b[99;6u"),
        (ch("a"), a, b"\x1b[97;3u"),
        (ch(" "), c, b"\x1b[32;5u"),
    ];
    for (key, mods, want) in table {
        assert_eq!(
            encode_key_kitty(key, *mods, false, 0b1).as_deref(),
            Some(*want),
            "{key:?} + {mods:?} under flag 1"
        );
    }

    // And the other half of the contract: with flag 1 active, everything that is
    // NOT ambiguous is still exactly its legacy bytes.
    let unchanged: &[(Key, ModifiersState, &[u8])] = &[
        (named(NamedKey::Enter), ModifiersState::empty(), b"\r"),
        (named(NamedKey::Tab), ModifiersState::empty(), b"\t"),
        (named(NamedKey::Backspace), ModifiersState::empty(), &[0x7f]),
        (named(NamedKey::ArrowUp), ModifiersState::empty(), b"\x1b[A"),
        (named(NamedKey::F1), ModifiersState::empty(), b"\x1bOP"),
        // Shift+letter is the guard that keeps ordinary typing out of the
        // protocol: it stays the shifted character, not `CSI 65;2u`.
        (ch("A"), s, b"A"),
        (ch("!"), s, b"!"),
        (ch("a"), ModifiersState::empty(), b"a"),
        // Shift ALONE, on a key that is not Tab/Enter/Backspace, is likewise not
        // an ambiguity flag 1 promises to resolve — alacritty's
        // `should_build_sequence` excludes it, and rt follows. These keep the
        // bytes rt has always sent for them, modifier and all thrown away.
        (named(NamedKey::ArrowUp), s, b"\x1b[A"),
        (named(NamedKey::Delete), s, b"\x1b[3~"),
        (named(NamedKey::F5), s, b"\x1b[15~"),
    ];
    for (key, mods, want) in unchanged {
        assert_eq!(
            encode_key_kitty(key, *mods, false, 0b1).as_deref(),
            Some(*want),
            "{key:?} + {mods:?} must stay legacy under flag 1"
        );
    }
    // An unmodified arrow still follows DECCKM; the protocol never sees it.
    assert_eq!(
        encode_key_kitty(&named(NamedKey::ArrowUp), ModifiersState::empty(), true, 0b1).as_deref(),
        Some(&b"\x1bOA"[..])
    );
}
