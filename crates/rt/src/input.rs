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
    if m.super_key() {
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
    let rt_key = key_from_winit(key)?; // the non-modifier key, or bail
    Some(Chord::new(mods_from_winit(mods), rt_key)) // combine with modifiers
}

/// Build a cursor/Home/End escape sequence with the given final byte, choosing
/// SS3 (`ESC O x`) when application-cursor-keys mode is on, else CSI (`ESC [ x`).
/// This one helper keeps all six keys consistent.
fn cursor(app_cursor: bool, final_byte: u8) -> Vec<u8> {
    // 0x1b = ESC; then 'O' for SS3 (application) or '[' for CSI (normal).
    let mid = if app_cursor { b'O' } else { b'[' };
    vec![0x1b, mid, final_byte]
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
            NamedKey::Space => Some(b" ".to_vec()),
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
