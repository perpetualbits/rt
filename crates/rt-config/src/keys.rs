//! Key-chord normalisation and parsing of Terminator's accelerator strings.
//!
//! Terminator (via GTK) writes bindings like `<Shift><Control>o`,
//! `<Control>Page_Down`, `<Alt>Up`, or `F11`. This module parses that grammar
//! into a [`Chord`] = (modifier set, key), which the GUI compares against
//! live key events. Parsing is total and fallible: anything malformed yields
//! `None` rather than panicking.

/// Keyboard modifier flags, stored as a small bitset in a `u8`.
///
/// We roll our own tiny bitflags (no dependency) because there are only four.
/// `CONTROL`/`SHIFT`/`ALT`/`SUPER` mirror GTK's `<Control>`/`<Shift>`/`<Alt>`/
/// `<Super>` tokens (Super = the "Windows"/Command key).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Mods(u8);

impl Mods {
    pub const NONE: Mods = Mods(0);
    pub const CONTROL: Mods = Mods(1 << 0); // <Control> / <Ctrl>
    pub const SHIFT: Mods = Mods(1 << 1); // <Shift>
    pub const ALT: Mods = Mods(1 << 2); // <Alt> / <Mod1>
    pub const SUPER: Mods = Mods(1 << 3); // <Super>

    /// Union of two modifier sets (bitwise OR), used while accumulating the
    /// `<...>` tokens at the front of an accelerator string.
    pub fn with(self, other: Mods) -> Mods {
        Mods(self.0 | other.0) // set the bits from both operands
    }

    /// Whether this set contains all the bits of `other`. Handy for the GUI
    /// which may report extra, irrelevant modifiers (e.g. NumLock) that we want
    /// to ignore — though for now we compare exactly in `Chord`.
    pub fn contains(self, other: Mods) -> bool {
        (self.0 & other.0) == other.0 // every bit of `other` is present here
    }
}

/// A normalised keyboard key: either a single printable character or a named
/// special key. Characters are stored lower-cased so `o` and `O` compare equal
/// (the shift state lives in [`Mods`], exactly as GTK models it).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Key {
    /// A printable character key (already lower-cased).
    Char(char),
    Up,
    Down,
    Left,
    Right,
    Tab,
    Enter,      // GTK "Return"
    PageUp,     // GTK "Page_Up"
    PageDown,   // GTK "Page_Down"
    Function(u8), // F1..F12 → Function(1)..Function(12)
}

/// A full key chord: a set of modifiers plus one key. Two chords are equal iff
/// both their modifier sets and keys match exactly — the basis of keymap
/// lookup.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Chord {
    pub mods: Mods, // required modifier set
    pub key: Key,   // the non-modifier key
}

impl Chord {
    /// Construct a chord directly (used by the GUI to build a chord from a live
    /// key event before looking it up).
    pub fn new(mods: Mods, key: Key) -> Self {
        Chord { mods, key }
    }

    /// Parse a Terminator/GTK accelerator string such as `<Shift><Control>o`
    /// into a [`Chord`]. Returns `None` for the empty string (an unbound
    /// action) or any syntax we do not recognise.
    ///
    /// Grammar: zero or more `<Modifier>` tokens, followed by exactly one key
    /// token (a single char, or a named key like `Up`/`Page_Down`/`F11`).
    pub fn parse(accel: &str) -> Option<Chord> {
        if accel.is_empty() {
            return None; // Terminator uses "" to mean "no binding"
        }
        let mut mods = Mods::NONE; // accumulate modifiers here
        let mut rest = accel; // the yet-unparsed tail of the string
        // Consume leading `<...>` modifier tokens one at a time.
        while rest.starts_with('<') {
            // Find the closing '>'; a missing one is malformed input.
            let close = rest.find('>')?;
            let token = &rest[1..close]; // the text between the angle brackets
            // Map the token (case-insensitively) to a modifier bit.
            let m = match token.to_ascii_lowercase().as_str() {
                "control" | "ctrl" | "primary" => Mods::CONTROL, // GTK aliases
                "shift" => Mods::SHIFT,
                "alt" | "mod1" => Mods::ALT,
                "super" => Mods::SUPER,
                _ => return None, // unknown modifier token → reject
            };
            mods = mods.with(m); // fold this modifier into the set
            rest = &rest[close + 1..]; // advance past the '>' we just consumed
        }
        // Whatever remains is the key token; it must be non-empty.
        if rest.is_empty() {
            return None; // modifiers with no key is not a valid chord
        }
        let key = Self::parse_key(rest)?; // interpret the key token
        Some(Chord { mods, key })
    }

    /// Parse the trailing key token of an accelerator string into a [`Key`].
    /// Recognises GTK key names (`Up`, `Page_Down`, `Return`, `F1`..`F12`) and
    /// single characters (including named symbols `plus`/`minus`). Returns
    /// `None` for anything unrecognised.
    fn parse_key(token: &str) -> Option<Key> {
        // Named keys first (compared case-insensitively for robustness).
        match token.to_ascii_lowercase().as_str() {
            "up" => return Some(Key::Up),
            "down" => return Some(Key::Down),
            "left" => return Some(Key::Left),
            "right" => return Some(Key::Right),
            "tab" => return Some(Key::Tab),
            "return" | "enter" => return Some(Key::Enter),
            "page_up" | "pageup" => return Some(Key::PageUp),
            "page_down" | "pagedown" => return Some(Key::PageDown),
            "plus" => return Some(Key::Char('+')), // GTK spells '+' as "plus"
            "minus" => return Some(Key::Char('-')), // and '-' as "minus"
            "period" => return Some(Key::Char('.')), // GTK name for '.'
            "comma" => return Some(Key::Char(',')), // GTK name for ','
            "equal" => return Some(Key::Char('=')), // GTK name for '='
            _ => {}
        }
        // Function keys: an 'F'/'f' followed by 1..=12.
        if let Some(num) = token.strip_prefix(['F', 'f']) {
            if let Ok(n) = num.parse::<u8>() {
                if (1..=12).contains(&n) {
                    return Some(Key::Function(n)); // valid F-key
                }
            }
        }
        // Otherwise it must be a single character; lower-case it so the shift
        // state is carried only by Mods, matching GTK's model.
        let mut chars = token.chars(); // iterator over the token's characters
        let first = chars.next()?; // the (only, we hope) character
        if chars.next().is_some() {
            return None; // more than one char and not a known name → reject
        }
        Some(Key::Char(first.to_ascii_lowercase())) // normalise case
    }
}

impl std::fmt::Display for Key {
    /// Human-readable key name for accelerator display (menus, tooltips).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Key::Char(c) => write!(f, "{}", c.to_ascii_uppercase()), // 'o' → "O", '.' → "."
            Key::Up => write!(f, "Up"),
            Key::Down => write!(f, "Down"),
            Key::Left => write!(f, "Left"),
            Key::Right => write!(f, "Right"),
            Key::Tab => write!(f, "Tab"),
            Key::Enter => write!(f, "Enter"),
            Key::PageUp => write!(f, "PgUp"),
            Key::PageDown => write!(f, "PgDn"),
            Key::Function(n) => write!(f, "F{n}"),
        }
    }
}

impl std::fmt::Display for Chord {
    /// The chord as a conventional accelerator string, e.g. `Ctrl+Shift+O`,
    /// `Ctrl+.`, `F1`. Modifiers are shown in the usual Ctrl, Shift, Alt, Super
    /// order (independent of how they were written in the config).
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        for (bit, name) in [
            (Mods::CONTROL, "Ctrl"),
            (Mods::SHIFT, "Shift"),
            (Mods::ALT, "Alt"),
            (Mods::SUPER, "Super"),
        ] {
            if self.mods.contains(bit) {
                write!(f, "{name}+")?; // prefix each present modifier
            }
        }
        write!(f, "{}", self.key) // then the key itself
    }
}
