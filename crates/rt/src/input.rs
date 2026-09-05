//! Translation between winit keyboard events and rt's semantics.
//!
//! Two directions:
//!   1. [`chord_from_winit`] turns a `(winit Key, ModifiersState)` into an
//!      `rt_config::Chord`, which the keymap resolves to an [`Action`] (splits,
//!      focus moves, …).
//!   2. [`encode_key`] turns an *ordinary* typed key (one with no rt binding)
//!      into the byte sequence a terminal expects, so it can be written to the
//!      focused PTY (arrows → ANSI escapes, Enter → `\r`, characters → UTF-8).
//!
//! Keeping this pure (no window, no PTY) makes it unit-testable — which matters
//! because off-by-one escape sequences are a classic terminal bug.

use rt_config::{Chord, Key as RtKey, Mods};
use winit::keyboard::{Key, ModifiersState, NamedKey};

/// Build an `rt_config::Mods` bitset from winit's `ModifiersState`.
///
/// winit reports the live modifier state as booleans; we fold the four we care
/// about into rt's compact `Mods`. NumLock/CapsLock and other exotic modifiers
/// are intentionally ignored so they never block a binding from matching.
fn mods_from_winit(m: ModifiersState) -> Mods {
    let mut out = Mods::NONE; // start with no modifiers
    if m.control_key() {
        out = out.with(Mods::CONTROL); // Ctrl held
    }
    if m.shift_key() {
        out = out.with(Mods::SHIFT); // Shift held
    }
    if m.alt_key() {
        out = out.with(Mods::ALT); // Alt held
    }
    // winit 0.31 calls this modifier `meta`; it is the same physical key rt
    // binds as Super.
    if m.meta_key() {
        out = out.with(Mods::SUPER); // Super/Meta held
    }
    out
}

/// Map a winit logical [`Key`] to rt's normalised [`RtKey`], or `None` if it is
/// a key rt has no representation for (e.g. a dead key or an unmapped named
/// key). Characters are lower-cased to match the keymap's case-insensitive
/// storage; the shift state is carried separately in the modifiers.
fn key_from_winit(key: &Key) -> Option<RtKey> {
    match key {
        // Named (non-printable) keys we care about.
        Key::Named(named) => match named {
            NamedKey::ArrowUp => Some(RtKey::Up),
            NamedKey::ArrowDown => Some(RtKey::Down),
            NamedKey::ArrowLeft => Some(RtKey::Left),
            NamedKey::ArrowRight => Some(RtKey::Right),
            NamedKey::Tab => Some(RtKey::Tab),
            NamedKey::Enter => Some(RtKey::Enter),
            NamedKey::PageUp => Some(RtKey::PageUp),
            NamedKey::PageDown => Some(RtKey::PageDown),
            // winit spells function keys F1..F35; map the 1..=12 we bind.
            NamedKey::F1 => Some(RtKey::Function(1)),
            NamedKey::F2 => Some(RtKey::Function(2)),
            NamedKey::F3 => Some(RtKey::Function(3)),
            NamedKey::F4 => Some(RtKey::Function(4)),
            NamedKey::F5 => Some(RtKey::Function(5)),
            NamedKey::F6 => Some(RtKey::Function(6)),
            NamedKey::F7 => Some(RtKey::Function(7)),
            NamedKey::F8 => Some(RtKey::Function(8)),
            NamedKey::F9 => Some(RtKey::Function(9)),
            NamedKey::F10 => Some(RtKey::Function(10)),
            NamedKey::F11 => Some(RtKey::Function(11)),
            NamedKey::F12 => Some(RtKey::Function(12)),
            _ => None, // any other named key is not something we bind
        },
        // Printable character keys: take the first char, lower-cased.
        Key::Character(s) => s.chars().next().map(|c| RtKey::Char(c.to_ascii_lowercase())),
        _ => None, // dead keys, unidentified, etc.
    }
}

/// Turn a winit key event into an `rt_config::Chord` suitable for keymap
/// lookup, or `None` if the key does not map to anything rt recognises.
///
/// This is the function the run-loop calls first for every key press; a `Some`
/// result is looked up in the keymap for an [`Action`], and only if that misses
/// do we fall back to [`encode_key`] for plain typing.
pub fn chord_from_winit(key: &Key, mods: ModifiersState) -> Option<Chord> {
    let Some(rt_key) = key_from_winit(key) else {
        // key_from_winit bailed: this is a key rt has no RtKey mapping for
        // (dead key, unidentified, or a NamedKey we don't bind). Log the raw
        // winit key/mods so a platform that delivers an unexpected Key variant
        // (e.g. macOS sending something Linux never does) shows up here.
        log::debug!("chord_from_winit: key={key:?} mods={mods:?} -> None (key_from_winit bailed)");
        return None;
    };
    let chord = Chord::new(mods_from_winit(mods), rt_key); // combine with modifiers
    log::debug!("chord_from_winit: key={key:?} mods={mods:?} -> {chord:?}");
    Some(chord)
}

/// Whether a key press that matched **no** binding should be swallowed instead
/// of typed into the PTY.
///
/// This exists for exactly one case: the macOS Command key. AppKit reports the
/// logical key of a ⌘-chord via `charactersIgnoringModifiers`, so winit hands rt
/// `Key::Character("k")` with `text: Some("k")` for ⌘K — and the ordinary typing
/// path would then push a literal `k` into the shell. Every Mac terminal treats
/// ⌘ as a menu accelerator and *nothing else*: an unbound ⌘-chord does nothing.
/// Without this guard, ⌘K / ⌘A / ⌘S / ⌘Z — all things a Mac user presses by
/// reflex and rt does not bind — would silently corrupt the command line.
///
/// Note what this does NOT touch. Ctrl is untouched, so `Ctrl+C` still becomes
/// `0x03` and still sends `SIGINT`; Alt/Option still take the ESC prefix. Only
/// Super/Command is affected, and only when the chord found no binding — a
/// bound ⌘-chord has already returned before this is consulted.
///
/// `cfg!` rather than `#[cfg]` so both arms always compile: off macOS this is
/// the constant `false` and folds away, leaving Linux behaviour bit-identical.
pub fn swallows_unbound(mods: ModifiersState) -> bool {
    cfg!(target_os = "macos") && mods.meta_key()
}

/// Whether a named key must be sent as an ANSI escape sequence (via
/// [`encode_key`]) rather than as its produced text. These are the navigation/
/// editing/function keys; *everything else* (printable characters, and crucially
/// keys whose text is a dead-key/compose result like `'`+space→`'`) is sent as
/// [`encode_text`] of `key_event.text`.
pub fn is_sequence_key(named: &NamedKey) -> bool {
    matches!(
        named,
        NamedKey::ArrowUp
            | NamedKey::ArrowDown
            | NamedKey::ArrowLeft
            | NamedKey::ArrowRight
            | NamedKey::Home
            | NamedKey::End
            | NamedKey::PageUp
            | NamedKey::PageDown
            | NamedKey::Insert
            | NamedKey::Delete
            | NamedKey::Enter
            | NamedKey::Backspace
            | NamedKey::Tab
            | NamedKey::Escape
            | NamedKey::F1
            | NamedKey::F2
            | NamedKey::F3
            | NamedKey::F4
            | NamedKey::F5
            | NamedKey::F6
            | NamedKey::F7
            | NamedKey::F8
            | NamedKey::F9
            | NamedKey::F10
            | NamedKey::F11
            | NamedKey::F12
    )
}

/// The C0 control code for `Ctrl` + a single character, or `None` if the
/// character doesn't map to one. Covers letters (Ctrl-A=1 … Ctrl-Z=26) and the
/// standard symbol combos (Ctrl-@/Space=NUL, Ctrl-[ = ESC, etc.).
fn ctrl_code(c: char) -> Option<u8> {
    match c {
        'a'..='z' => Some(c as u8 - 0x60), // a→1 … z→26
        'A'..='Z' => Some(c as u8 - 0x40), // A→1 … Z→26
        ' ' | '@' => Some(0x00),           // NUL
        '[' => Some(0x1b),                 // ESC
        '\\' => Some(0x1c),
        ']' => Some(0x1d),
        '^' => Some(0x1e),
        '_' | '?' => Some(0x1f),
        _ => None,
    }
}

/// Encode the *text* a key produced (already dead-key/compose-resolved by
/// winit's `key_event.text`) into PTY bytes, applying Ctrl (→ C0 control code)
/// and Alt (→ ESC/Meta prefix). This is the path that fixes composed characters
/// like `'`+space→`'`, since it sends the produced text rather than deriving a
/// character from the logical key.
pub fn encode_text(text: &str, mods: ModifiersState) -> Vec<u8> {
    // Ctrl + a lone printable char → its control code (unless the text is
    // already a control character, which passes straight through below).
    if mods.control_key() {
        let mut chars = text.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            if let Some(code) = ctrl_code(c) {
                let mut out = Vec::new();
                if mods.alt_key() {
                    out.push(0x1b); // Alt → ESC prefix even with Ctrl
                }
                out.push(code);
                return out;
            }
        }
    }
    // Otherwise send the text as UTF-8, prefixed by ESC when Alt (Meta) is held.
    let mut out = Vec::new();
    if mods.alt_key() {
        out.push(0x1b);
    }
    out.extend_from_slice(text.as_bytes());
    out
}

/// Build a cursor/Home/End escape sequence with the given final byte, choosing
/// SS3 (`ESC O x`) when application-cursor-keys mode is on, else CSI (`ESC [ x`).
/// This one helper keeps all six keys consistent.
fn cursor(app_cursor: bool, final_byte: u8) -> Vec<u8> {
    // 0x1b = ESC; then 'O' for SS3 (application) or '[' for CSI (normal).
    let mid = if app_cursor { b'O' } else { b'[' };
    vec![0x1b, mid, final_byte]
}

// ── Kitty keyboard protocol ───────────────────────────────────────────────────
//
// The legacy encoding below throws modifiers away: Shift+Enter, Ctrl+Enter and
// Enter are all `\r`, so no application can tell them apart. The kitty keyboard
// protocol fixes that, but only for applications that ASK — an application
// pushes the enhancement flags it wants, and until it does, the terminal must
// keep sending the legacy bytes. Sending `\x1b[13;2u` unconditionally would make
// Shift+Enter *undo* in vim (ESC leaves insert, `u` undoes) and leave literal
// `3;2u` in the bash line buffer, which is why the negotiation is the feature.
//
// rt implements progressive-enhancement flag 1 only (see `kitty_disambiguates`).

/// Progressive-enhancement flag 1, "disambiguate escape codes" — the only flag
/// rt implements, and the only one it will ever report as active.
pub const KITTY_DISAMBIGUATE: u8 = 0b1;

/// The kitty modifier bitset for `mods`: shift 1, alt 2, ctrl 4, super 8. The
/// escape sequence carries this value **plus one**, so an unmodified key has
/// parameter 1 and can omit it entirely.
fn kitty_mod_bits(mods: ModifiersState) -> u8 {
    let mut bits = 0u8;
    if mods.shift_key() {
        bits |= 0b0001;
    }
    if mods.alt_key() {
        bits |= 0b0010;
    }
    if mods.control_key() {
        bits |= 0b0100;
    }
    // winit 0.31 spells Super as `meta`, exactly as `mods_from_winit` does.
    if mods.meta_key() {
        bits |= 0b1000;
    }
    bits
}

/// Whether this key press must be disambiguated — i.e. sent in the kitty
/// protocol's form rather than its legacy bytes — given the pane's active
/// enhancement flags.
///
/// This is alacritty's `should_build_sequence` (`alacritty/src/input/keyboard.rs`)
/// restricted to flag 1, and it is deliberately narrow: under flag 1 alone the
/// only keys that change are the ones whose legacy encoding is genuinely
/// ambiguous. Shift+letter is *not* one of them — `A` already says everything
/// there is to say — which is why plain typing survives the protocol untouched.
///
/// The one clause of alacritty's condition rt cannot express is
/// `key.location == KeyLocation::Numpad`: rt's key path carries only the logical
/// `Key`, never winit's `KeyLocation`, so an unmodified numpad key stays legacy
/// here where alacritty would disambiguate it. That costs an application the
/// ability to tell numpad `1` from row `1`; it costs nothing for the reported
/// bug, and plumbing `KeyLocation` through four crates is not in this change.
pub fn kitty_disambiguates(key: &Key, mods: ModifiersState, kbd_flags: u8) -> bool {
    if kbd_flags & KITTY_DISAMBIGUATE == 0 {
        return false; // nothing negotiated: legacy bytes, always
    }
    // Escape is disambiguated with or without modifiers: a bare `\x1b` is
    // indistinguishable from the start of any escape sequence, which is the
    // ambiguity the flag is named after.
    if matches!(key, Key::Named(NamedKey::Escape)) {
        return true;
    }
    if mods.is_empty() {
        return false; // an unmodified key is never ambiguous
    }
    if mods == ModifiersState::SHIFT {
        // Shift ALONE only matters for the three keys whose legacy byte has no
        // room for a modifier — including Enter, which is the reported bug.
        // Everything else shifted (letters, digits, symbols) is already carried
        // faithfully by the character it produces.
        return matches!(key, Key::Named(NamedKey::Tab | NamedKey::Enter | NamedKey::Backspace));
    }
    true // any ctrl/alt/super combination
}

/// Build the kitty-protocol sequence for a disambiguated key, or `None` if rt
/// has no encoding for it (the caller then falls back to the legacy bytes).
///
/// Mirrors alacritty's `build_sequence`: a payload, an optional `;<modifiers>`
/// parameter, and a terminator that is either the key's own legacy final byte
/// (`A`, `~`, …) or `u` for the CSI-u form. Keys that already own a legacy escape
/// sequence keep it and take the modifier as a parameter (`CSI 1;2A`); keys whose
/// legacy encoding is a bare control byte switch to `CSI <code> u`.
fn kitty_sequence(key: &Key, mods: ModifiersState) -> Option<Vec<u8>> {
    let bits = kitty_mod_bits(mods);
    // The `1` in `CSI 1;2A` is the default parameter, omitted when there is no
    // modifier parameter to follow it.
    let one = if bits == 0 { "" } else { "1" };
    let (payload, terminator): (String, char) = match key {
        Key::Named(named) => match named {
            // Keys with a legacy CSI form: keep the final byte, add the modifier.
            // Note these are always CSI here, never SS3 — a modified arrow is CSI
            // in every terminal, DECCKM or not, and only the *unmodified* arrow
            // (which never reaches this function) follows application-cursor mode.
            NamedKey::ArrowUp => (one.into(), 'A'),
            NamedKey::ArrowDown => (one.into(), 'B'),
            NamedKey::ArrowRight => (one.into(), 'C'),
            NamedKey::ArrowLeft => (one.into(), 'D'),
            NamedKey::Home => (one.into(), 'H'),
            NamedKey::End => (one.into(), 'F'),
            NamedKey::Insert => ("2".into(), '~'),
            NamedKey::Delete => ("3".into(), '~'),
            NamedKey::PageUp => ("5".into(), '~'),
            NamedKey::PageDown => ("6".into(), '~'),
            NamedKey::F1 => (one.into(), 'P'),
            NamedKey::F2 => (one.into(), 'Q'),
            // F3 is the one key the protocol re-spells: its legacy final byte is
            // `R`, which is also the terminator of a cursor-position report, so
            // `CSI 1;2R` would be unreadable. kitty (and alacritty, which notes
            // the same divergence from its own terminfo) sends `CSI 13;2~`.
            NamedKey::F3 => ("13".into(), '~'),
            NamedKey::F4 => (one.into(), 'S'),
            NamedKey::F5 => ("15".into(), '~'),
            NamedKey::F6 => ("17".into(), '~'),
            NamedKey::F7 => ("18".into(), '~'),
            NamedKey::F8 => ("19".into(), '~'),
            NamedKey::F9 => ("20".into(), '~'),
            NamedKey::F10 => ("21".into(), '~'),
            NamedKey::F11 => ("23".into(), '~'),
            NamedKey::F12 => ("24".into(), '~'),
            // Control characters: the CSI-u form, keyed on the code point the
            // legacy encoding collapsed them to.
            NamedKey::Tab => ("9".into(), 'u'),
            NamedKey::Enter => ("13".into(), 'u'),
            NamedKey::Escape => ("27".into(), 'u'),
            NamedKey::Backspace => ("127".into(), 'u'),
            // Space has no `NamedKey` in this winit: it arrives as
            // `Key::Character(" ")` and is encoded as code point 32 below.
            _ => return None, // a named key rt does not encode at all
        },
        Key::Character(s) => {
            // Only a single-code-point key has a "unicode key code"; anything
            // longer (a compose result) has no place in the protocol's key
            // namespace and falls back to being sent as text.
            let mut chars = s.chars();
            let (c, rest) = (chars.next()?, chars.next());
            if rest.is_some() {
                return None;
            }
            // The protocol reports the key the user pressed, not what Shift made
            // of it, so `Ctrl+Shift+C` is code point 99 (`c`) with the shift bit
            // set — not 67. Base-layout keys like `!` -> `1` need winit's
            // `key_without_modifiers`, which rt's `&Key` does not carry; those
            // report their shifted code point instead.
            let base = if mods.shift_key() { c.to_lowercase().next().unwrap_or(c) } else { c };
            (u32::from(base).to_string(), 'u')
        }
        _ => return None, // dead/unidentified keys
    };
    let mut out = format!("\x1b[{payload}");
    if bits != 0 {
        out.push_str(&format!(";{}", bits + 1)); // the protocol's modifier param
    }
    out.push(terminator);
    Some(out.into_bytes())
}

/// Encode a plain typed key (one that carried no rt binding) into the bytes to
/// write to the PTY. Returns `None` for keys that produce no input (e.g. a lone
/// modifier press, or a named key we do not translate).
///
/// `app_cursor` is the terminal's DECCKM (application cursor keys) state: when
/// true, arrows and Home/End are encoded as SS3 (`ESC O x`) instead of CSI
/// (`ESC [ x`). Full-screen apps like `mc`/`vim` toggle this, and getting it
/// wrong is exactly why their arrow navigation appears dead. The sequences
/// follow standard xterm conventions that `alacritty_terminal`'s parser expects.
pub fn encode_key(key: &Key, mods: ModifiersState, app_cursor: bool) -> Option<Vec<u8>> {
    encode_key_kitty(key, mods, app_cursor, 0)
}

/// The whole "ordinary typing" rule for one winit key event, as a pure function
/// of the key, the text winit produced for it, and **the receiving pane's**
/// terminal state — `app_cursor` (DECCKM) and `kbd_flags` (the kitty keyboard
/// enhancement flags the program in that pane pushed).
///
/// Those last two are per-PANE, not per-keystroke: under broadcast one keypress
/// reaches several panes whose programs negotiated differently, so the caller
/// runs this once per target pane (`Session::feed_input_with`). That is exactly
/// why it lives here as a function of `(app_cursor, kbd_flags)` rather than
/// inline in the event handler over one pane's values — see the doc on
/// `Session::feed_input_with` for the bug that shape caused.
///
/// Order matters and is load-bearing:
///   1. A key the negotiated protocol disambiguates takes the protocol's form,
///      tested BEFORE the produced-text branch — Ctrl-combos arrive *with* text
///      (the C0 byte), and letting that win would silently give a negotiated
///      application half a protocol.
///   2. Navigation/editing/function keys become their ANSI escape sequences.
///   3. Everything else sends the key's produced text, which already carries
///      dead-key / compose results (`'` + space → `'`) the logical key misses;
///      with no text (lone Ctrl combos, etc.) it falls back to `encode_key`.
///
/// `None` means the key produces no input at all (a lone modifier press, say).
pub fn encode_key_event(
    key: &Key,
    text: Option<&str>,
    mods: ModifiersState,
    app_cursor: bool,
    kbd_flags: u8,
) -> Option<Vec<u8>> {
    if kitty_disambiguates(key, mods, kbd_flags) {
        return encode_key_kitty(key, mods, app_cursor, kbd_flags);
    }
    if let Key::Named(n) = key {
        if is_sequence_key(n) {
            return encode_key(key, mods, app_cursor); // arrows/enter/…
        }
    }
    match text.filter(|t| !t.is_empty()) {
        Some(text) => Some(encode_text(text, mods)), // the composed text
        None => encode_key(key, mods, app_cursor),   // fallback (Ctrl combos, etc.)
    }
}

/// As [`encode_key`], but honouring `kbd_flags` — the pane's active kitty
/// keyboard enhancement flags, as pushed by the program running in it.
///
/// `kbd_flags == 0` (no program has negotiated) is byte-for-byte [`encode_key`];
/// that equivalence is not incidental, it is the safety property of the whole
/// feature and `crates/rt/tests/input.rs` pins it key by key.
pub fn encode_key_kitty(
    key: &Key,
    mods: ModifiersState,
    app_cursor: bool,
    kbd_flags: u8,
) -> Option<Vec<u8>> {
    // A disambiguated key takes the protocol's form; if rt has no protocol
    // encoding for it, fall through to the legacy bytes rather than send
    // nothing — a key that used to type something must still type something.
    if kitty_disambiguates(key, mods, kbd_flags) {
        if let Some(bytes) = kitty_sequence(key, mods) {
            return Some(bytes);
        }
    }
    match key {
        Key::Named(named) => match named {
            // Enter sends a carriage return (the shell converts to newline).
            NamedKey::Enter => Some(b"\r".to_vec()),
            // Backspace sends DEL (0x7f), the xterm default.
            NamedKey::Backspace => Some(vec![0x7f]),
            NamedKey::Tab => Some(b"\t".to_vec()),
            NamedKey::Escape => Some(vec![0x1b]),
            // Cursor keys + Home/End: SS3 form in application-cursor mode, CSI
            // form otherwise. `cursor(final_byte)` builds the right one.
            NamedKey::ArrowUp => Some(cursor(app_cursor, b'A')),
            NamedKey::ArrowDown => Some(cursor(app_cursor, b'B')),
            NamedKey::ArrowRight => Some(cursor(app_cursor, b'C')),
            NamedKey::ArrowLeft => Some(cursor(app_cursor, b'D')),
            NamedKey::Home => Some(cursor(app_cursor, b'H')),
            NamedKey::End => Some(cursor(app_cursor, b'F')),
            // Editing / navigation keys (CSI ~ sequences).
            NamedKey::Insert => Some(b"\x1b[2~".to_vec()), // toggles insert/overwrite in editors/mc
            NamedKey::Delete => Some(b"\x1b[3~".to_vec()),
            NamedKey::PageUp => Some(b"\x1b[5~".to_vec()),
            NamedKey::PageDown => Some(b"\x1b[6~".to_vec()),
            // Function keys F1–F4 use SS3; F5–F12 use CSI ~ codes (xterm).
            NamedKey::F1 => Some(b"\x1bOP".to_vec()),
            NamedKey::F2 => Some(b"\x1bOQ".to_vec()),
            NamedKey::F3 => Some(b"\x1bOR".to_vec()),
            NamedKey::F4 => Some(b"\x1bOS".to_vec()),
            NamedKey::F5 => Some(b"\x1b[15~".to_vec()),
            NamedKey::F6 => Some(b"\x1b[17~".to_vec()),
            NamedKey::F7 => Some(b"\x1b[18~".to_vec()),
            NamedKey::F8 => Some(b"\x1b[19~".to_vec()),
            NamedKey::F9 => Some(b"\x1b[20~".to_vec()),
            NamedKey::F10 => Some(b"\x1b[21~".to_vec()),
            NamedKey::F11 => Some(b"\x1b[23~".to_vec()),
            NamedKey::F12 => Some(b"\x1b[24~".to_vec()),
            _ => None, // other named keys produce nothing
        },
        Key::Character(s) => {
            // If Ctrl is held with a letter, send the C0 control code
            // (Ctrl-A = 0x01 … Ctrl-Z = 0x1a), matching every terminal.
            if mods.control_key() {
                if let Some(c) = s.chars().next() {
                    let lower = c.to_ascii_lowercase(); // control codes ignore case
                    if lower.is_ascii_lowercase() {
                        // 'a' is 0x61; the control code is 0x01, so subtract 0x60.
                        let code = (lower as u8) - 0x60; // map a..z -> 1..26
                        return Some(vec![code]);
                    }
                }
            }
            // Otherwise send the characters as UTF-8 bytes (normal typing).
            Some(s.as_bytes().to_vec())
        }
        _ => None, // dead/unidentified keys: nothing to send
    }
}
