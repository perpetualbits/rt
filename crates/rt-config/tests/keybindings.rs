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
fn appearance_settings_clamp() {
    use rt_config::Settings;
    let mut s = Settings::default();
    assert_eq!(s.background_opacity, 1.0); // opaque by default
    assert_eq!(s.scrim_strength, 0.0); // no scrim by default
    // Opacity clamps to [MIN_OPACITY, 1.0].
    s.adjust_opacity(-5.0);
    assert_eq!(s.background_opacity, Settings::MIN_OPACITY); // floored, not negative
    s.adjust_opacity(5.0);
    assert_eq!(s.background_opacity, 1.0); // capped at fully opaque
    // Scrim clamps to [0.0, MAX_SCRIM].
    s.adjust_scrim(5.0);
    assert_eq!(s.scrim_strength, Settings::MAX_SCRIM); // capped
    s.adjust_scrim(-5.0);
    assert_eq!(s.scrim_strength, 0.0); // floored at off
}

#[test]
fn appearance_bindings_resolve() {
    use rt_config::{Action, Chord, Keymap};
    let map = Keymap::defaults();
    // Ctrl+Alt+Down = more see-through; Ctrl+Alt+Right = stronger scrim.
    assert_eq!(
        map.action_for(&Chord::parse("<Control><Alt>Down").unwrap()),
        Some(Action::OpacityDown)
    );
    assert_eq!(
        map.action_for(&Chord::parse("<Control><Alt>Right").unwrap()),
        Some(Action::ScrimUp)
    );
}

#[test]
fn user_binding_overrides_default() {
    // Binding a chord that a default also uses must shadow the default.
    let mut map = Keymap::defaults();
    let chord = Chord::parse("<Shift><Control>o").unwrap(); // was SplitHoriz
    map.bind(chord, Action::NewTab); // user rebinds it
    assert_eq!(map.action_for(&chord), Some(Action::NewTab)); // override wins
}

#[test]
fn config_roundtrips_through_toml() {
    use rt_config::{Config, Settings};
    // A non-default config should survive serialise -> parse unchanged.
    let cfg = Config {
        settings: Settings { background_opacity: 0.8, scrim_strength: 0.3, focus_follows_mouse: true },
    };
    let text = toml::to_string(&cfg).expect("serialises");
    let back: Config = toml::from_str(&text).expect("parses");
    assert_eq!(back.settings.background_opacity, 0.8);
    assert_eq!(back.settings.scrim_strength, 0.3);
    assert!(back.settings.focus_follows_mouse);
    // A partial/empty file loads as defaults (serde(default)).
    let def: Config = toml::from_str("").expect("empty parses");
    assert_eq!(def.settings.background_opacity, 1.0);
    assert!(!def.settings.focus_follows_mouse);
}
