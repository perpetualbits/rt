//! Tests for accelerator parsing and the default keymap. Pure/headless.

use rt_config::{Action, Chord, Key, Keymap, Mods};

#[test]
fn parses_modifier_combos() {
    // <Shift><Control>o → Shift+Control + 'o'.
    let c = Chord::parse("<Shift><Control>o").unwrap();
    assert_eq!(c.key, Key::Char('o')); // key normalised, lower-case
    assert!(c.mods.contains(Mods::SHIFT)); // shift present
    assert!(c.mods.contains(Mods::CONTROL)); // control present
    assert!(!c.mods.contains(Mods::ALT)); // alt absent
}

#[test]
fn parses_named_and_function_keys() {
    // Named navigation keys and F-keys resolve to their variants.
    assert_eq!(Chord::parse("<Alt>Up").unwrap().key, Key::Up);
    assert_eq!(Chord::parse("<Control>Page_Down").unwrap().key, Key::PageDown);
    assert_eq!(Chord::parse("F11").unwrap().key, Key::Function(11));
    // F11 has no modifiers.
    assert_eq!(Chord::parse("F11").unwrap().mods, Mods::NONE);
}

#[test]
fn case_insensitive_and_symbol_names() {
    // GTK spells '+' as "plus"; casing of modifier tokens is ignored.
    let c = Chord::parse("<control>plus").unwrap();
    assert_eq!(c.key, Key::Char('+'));
    assert!(c.mods.contains(Mods::CONTROL));
}

#[test]
fn empty_and_malformed_reject() {
    assert!(Chord::parse("").is_none()); // "" = unbound in Terminator
    assert!(Chord::parse("<Bogus>x").is_none()); // unknown modifier
    assert!(Chord::parse("<Control>").is_none()); // modifier but no key
    assert!(Chord::parse("<Control>ab").is_none()); // two-char non-name token
}

#[test]
fn default_keymap_resolves_terminator_bindings() {
    // The defaults must reproduce Terminator's signature bindings.
    let map = Keymap::defaults();
    // Ctrl+Shift+O = split_horiz.
    let split_h = Chord::parse("<Shift><Control>o").unwrap();
    assert_eq!(map.action_for(&split_h), Some(Action::SplitHoriz));
    // Ctrl+Shift+E = split_vert.
    let split_v = Chord::parse("<Shift><Control>e").unwrap();
    assert_eq!(map.action_for(&split_v), Some(Action::SplitVert));
    // Alt+Left = go_left.
    let go_left = Chord::parse("<Alt>Left").unwrap();
    assert_eq!(map.action_for(&go_left), Some(Action::GoLeft));
    // An unbound chord returns None.
    let unbound = Chord::new(Mods::NONE, Key::Char('z'));
    assert_eq!(map.action_for(&unbound), None);
}

#[test]
fn user_binding_overrides_default() {
    // Binding a chord that a default also uses must shadow the default.
    let mut map = Keymap::defaults();
    let chord = Chord::parse("<Shift><Control>o").unwrap(); // was SplitHoriz
    map.bind(chord, Action::NewTab); // user rebinds it
    assert_eq!(map.action_for(&chord), Some(Action::NewTab)); // override wins
}
