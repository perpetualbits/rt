//! `rt-config` — keybindings and configuration for rt.
//!
//! This is the port of Terminator's `keybindings` section of `config.py`. We
//! keep Terminator's exact accelerator syntax (`<Shift><Control>o`) so a user's
//! muscle memory — and eventually their config file — carries over. Parsing
//! that string into a normalised [`Chord`] and mapping it to a semantic
//! [`Action`] is pure logic, unit-tested without any GUI.
//!
//! The GUI front-end converts a physical winit key event into a [`Chord`] and
//! calls [`Keymap::action_for`]; the returned [`Action`] is then handed to the
//! session controller (`rt-session`).

pub mod keys; // Chord / Key / Mods normalisation and parsing

pub use keys::{Chord, Key, Mods};

/// A semantic editor action, decoupled from the physical keys that trigger it.
///
/// This is the subset of Terminator's action list that rt implements (or will
/// implement imminently). Naming follows Terminator's config keys so the
/// mapping is one-to-one and auditable against `config.py`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Action {
    /// `split_horiz` — split with a horizontal divider (panes stacked
    /// top/bottom). Maps to `Orientation::TopBottom` in rt-core.
    SplitHoriz,
    /// `split_vert` — split with a vertical divider (panes side by side).
    /// Maps to `Orientation::LeftRight`.
    SplitVert,
    /// `close_term` — close the focused pane.
    CloseTerm,
    /// `new_tab` — open a new tab beside the focused pane.
    NewTab,
    /// `next_tab` / `prev_tab` — cycle the active tab.
    NextTab,
    PrevTab,
    /// `go_up`/`down`/`left`/`right` — move focus spatially between panes.
    GoUp,
    GoDown,
    GoLeft,
    GoRight,
    /// `copy` / `paste` — clipboard integration (wired at the GUI layer).
    Copy,
    Paste,
    /// `broadcast_off` / `broadcast_group` / `broadcast_all` — set how typed
    /// input fans out to other panes (rt's port of Terminator grouping).
    BroadcastOff,
    BroadcastGroup,
    BroadcastAll,
    /// `close_window` — close the whole window.
    CloseWindow,
    /// rt-specific: add one newspaper column to the focused pane (1 = normal).
    ColumnsMore,
    /// rt-specific: remove one newspaper column (clamped at 1 = normal).
    ColumnsFewer,
    /// rt-specific: make the window background more opaque.
    OpacityUp,
    /// rt-specific: make the window background more translucent (see-through).
    OpacityDown,
    /// rt-specific: strengthen the background scrim (less legible behind).
    ScrimUp,
    /// rt-specific: weaken the background scrim (more legible behind).
    ScrimDown,
    /// rt-specific: toggle focus-follows-mouse (sloppy focus) on/off.
    ToggleFocusFollowsMouse,
    /// Open/close the preferences dialog.
    Preferences,
    /// Increase the font size (zoom in).
    ZoomIn,
    /// Decrease the font size (zoom out).
    ZoomOut,
    /// Reset the font size to the default.
    ZoomReset,
    /// Toggle fullscreen.
    Fullscreen,
    /// Maximise/restore the focused pane (Terminator's toggle_zoom).
    ToggleZoom,
    /// Open the scrollback-search bar (find text in this pane's history).
    Search,
    /// Split the focused pane along its longer axis (Terminator's split_auto).
    SplitAuto,
    /// Flip the orientation of the split containing the focused pane.
    Rotate,
    /// Grow the focused pane leftward (shrinking its left neighbour).
    ResizeLeft,
    /// Grow the focused pane rightward (shrinking its right neighbour).
    ResizeRight,
    /// Grow the focused pane upward (shrinking its upper neighbour).
    ResizeUp,
    /// Grow the focused pane downward (shrinking its lower neighbour).
    ResizeDown,
    /// Cycle the focused pane through input groups (for Broadcast::Group).
    GroupCycle,
    /// Patch-bay: arm/complete a wire from the focused pane's stdout jack.
    WireStdout,
    /// Patch-bay: arm/complete a wire from the focused pane's stderr jack.
    WireStderr,
    /// Patch-bay: disconnect every wire touching the focused pane.
    Unwire,
    /// Patch-bay: split and wire the focused pane's stdout into the new pane.
    PipeInto,
    /// Open/close the built-in manual overlay.
    Manual,
}

/// Window-level appearance settings (Terminator's "Profiles → Background" in
/// spirit). Kept minimal for now; a future preferences panel edits these and a
/// config file persists them.
#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(default)] // missing fields in the file fall back to Default, so old/partial configs load
pub struct Settings {
    /// Background opacity, `0.05..=1.0`. `1.0` is fully opaque; lower values let
    /// the window(s) behind show through (compositor permitting). Clamped away
    /// from 0 so the window can never become completely invisible.
    pub background_opacity: f32,
    /// Background scrim strength, `0.0..=0.95`. A neutral wash drawn over the
    /// translucent background (behind the text) that compresses the *contrast*
    /// of whatever shows through — so you can still see motion/shapes below but
    /// its text becomes hard to read. `0.0` = no scrim. This is rt's portable
    /// stand-in for background blur (see `docs/APPEARANCE.md`): a Wayland client
    /// can't blur what's behind it, but it can wash out its legibility.
    pub scrim_strength: f32,
    /// When true, ask the compositor to blur whatever is behind the translucent
    /// window (the `ext-background-effect-v1` staging protocol; KDE 6.7+, COSMIC,
    /// niri). True background blur — unlike the `scrim`, which only washes out
    /// contrast. A silent no-op on compositors without the protocol (GNOME, older
    /// KWin, X11), and skipped entirely while the background is fully opaque
    /// (`background_opacity == 1.0`), where blur would be wasted work.
    pub background_blur: bool,
    /// When true, moving the mouse over a pane focuses it (sloppy focus). When
    /// false (default), focus changes only on click. In rt sloppy and strict
    /// pointer-focus coincide, since a pane is always focused (over a gutter the
    /// previous focus simply sticks).
    pub focus_follows_mouse: bool,
    /// When true, each pane shows a header strip with its title, size and group
    /// (Terminator's per-terminal titlebar). When false, panes are borderless
    /// (rt's cleaner default) and group membership shows as a corner marker.
    pub show_titlebar: bool,
    /// Border instrument: the green output-activity flow around each pane.
    pub inst_output: bool,
    /// Border instrument: the blackbody CPU-heat tint of each pane's border.
    pub inst_heat: bool,
    /// Border instrument: the violet latency frame around the window.
    pub inst_latency: bool,
    /// Show the patch-bay jack dots on each pane (existing wires draw regardless).
    pub show_jacks: bool,
    /// Default text colour (RGB). Cells that don't set an explicit foreground
    /// use this.
    pub foreground: [u8; 3],
    /// Default background colour (RGB). The window clears to this (with the
    /// opacity above), and cells with this background stay translucent.
    pub background: [u8; 3],
    /// The 16 ANSI palette colours (0–7 normal, 8–15 bright), RGB each. The
    /// 256-colour cube and greyscale ramp are derived from these by the engine.
    pub palette: [[u8; 3]; 16],
    /// Monospace font family name (as system font databases know it). If it
    /// can't be found, rt falls back to a bundled-path search.
    pub font_family: String,
    /// Font size in pixels.
    pub font_size: f32,
    /// Maximum scrollback lines retained per pane, above the visible screen.
    /// Larger keeps more history to scroll and search through, at the cost of
    /// memory (~tens of bytes per column per line). Applies to terminals opened
    /// after the change.
    pub scrollback: usize,
}

/// The default 16-colour ANSI palette (classic xterm values).
pub const DEFAULT_PALETTE: [[u8; 3]; 16] = [
    [0x00, 0x00, 0x00], // 0 black
    [0xcd, 0x00, 0x00], // 1 red
    [0x00, 0xcd, 0x00], // 2 green
    [0xcd, 0xcd, 0x00], // 3 yellow
    [0x00, 0x00, 0xee], // 4 blue
    [0xcd, 0x00, 0xcd], // 5 magenta
    [0x00, 0xcd, 0xcd], // 6 cyan
    [0xe5, 0xe5, 0xe5], // 7 white
    [0x7f, 0x7f, 0x7f], // 8 bright black
    [0xff, 0x00, 0x00], // 9 bright red
    [0x00, 0xff, 0x00], // 10 bright green
    [0xff, 0xff, 0x00], // 11 bright yellow
    [0x5c, 0x5c, 0xff], // 12 bright blue
    [0xff, 0x00, 0xff], // 13 bright magenta
    [0x00, 0xff, 0xff], // 14 bright cyan
    [0xff, 0xff, 0xff], // 15 bright white
];

impl Default for Settings {
    /// Sensible defaults: fully opaque, no scrim, click-to-focus.
    fn default() -> Self {
        Settings {
            background_opacity: 1.0,       // opaque until the user dials it down
            scrim_strength: 0.0,           // no scrim until the user enables it
            background_blur: true,         // request compositor blur when translucent (no-op if unsupported)
            focus_follows_mouse: false,    // click-to-focus by default
            show_titlebar: true,           // Terminator-style per-pane titlebars on by default
            inst_output: true,             // border instruments on by default
            inst_heat: true,
            inst_latency: true,
            show_jacks: true,              // patch-bay jacks visible by default
            foreground: [0xd0, 0xd0, 0xd8], // light grey text
            background: [0x10, 0x10, 0x14], // near-black background
            palette: DEFAULT_PALETTE,      // classic xterm 16-colour palette
            font_family: "DejaVu Sans Mono".to_string(), // ubiquitous monospace default
            font_size: 18.0,               // pixels
            scrollback: 10_000,            // matches rt_engine::DEFAULT_SCROLLBACK
        }
    }
}

impl Settings {
    /// The smallest opacity we allow, so the window never vanishes entirely.
    pub const MIN_OPACITY: f32 = 0.05;
    /// The strongest scrim we allow; above this almost nothing shows through, at
    /// which point the user may as well raise opacity instead.
    pub const MAX_SCRIM: f32 = 0.95;
    /// Upper bound for the scrollback slider. 1M lines is already ~1 GB for an
    /// 80-column pane; beyond that, redirect to a file or pager instead.
    pub const MAX_SCROLLBACK: usize = 1_000_000;

    /// Nudge the opacity by `delta`, clamped to `[MIN_OPACITY, 1.0]`. Returns
    /// the new value. Used by the `OpacityUp`/`OpacityDown` actions.
    pub fn adjust_opacity(&mut self, delta: f32) -> f32 {
        // Clamp so we stay in a usable, always-visible range.
        self.background_opacity = (self.background_opacity + delta).clamp(Self::MIN_OPACITY, 1.0);
        self.background_opacity
    }

    /// Nudge the scrim strength by `delta`, clamped to `[0.0, MAX_SCRIM]`.
    /// Returns the new value. Used by the `ScrimUp`/`ScrimDown` actions.
    pub fn adjust_scrim(&mut self, delta: f32) -> f32 {
        self.scrim_strength = (self.scrim_strength + delta).clamp(0.0, Self::MAX_SCRIM); // stay in range
        self.scrim_strength
    }
}

/// A named colour scheme (foreground + background + 16 ANSI palette), for the
/// preferences dialog's preset picker (rt's port of Terminator's `_Colors` menu).
pub struct ColorScheme {
    pub name: &'static str,
    pub foreground: [u8; 3],
    pub background: [u8; 3],
    pub palette: [[u8; 3]; 16],
}

/// Built-in colour scheme presets. Selecting one fills fg/bg/palette; the user
/// can then tweak individual colours.
pub const SCHEMES: &[ColorScheme] = &[
    ColorScheme { name: "rt default", foreground: [0xd0, 0xd0, 0xd8], background: [0x10, 0x10, 0x14], palette: DEFAULT_PALETTE },
    ColorScheme {
        name: "Solarized Dark",
        foreground: [131, 148, 150],
        background: [0, 43, 54],
        palette: [
            [7, 54, 66], [220, 50, 47], [133, 153, 0], [181, 137, 0], [38, 139, 210], [211, 54, 130], [42, 161, 152], [238, 232, 213],
            [0, 43, 54], [203, 75, 22], [88, 110, 117], [101, 123, 131], [131, 148, 150], [108, 113, 196], [147, 161, 161], [253, 246, 227],
        ],
    },
    ColorScheme {
        name: "Dracula",
        foreground: [248, 248, 242],
        background: [40, 42, 54],
        palette: [
            [0, 0, 0], [255, 85, 85], [80, 250, 123], [241, 250, 140], [189, 147, 249], [255, 121, 198], [139, 233, 253], [191, 191, 191],
            [77, 77, 77], [255, 110, 103], [90, 247, 142], [244, 249, 157], [202, 169, 250], [255, 146, 208], [154, 237, 254], [230, 230, 230],
        ],
    },
    ColorScheme {
        name: "Gruvbox Dark",
        foreground: [235, 219, 178],
        background: [40, 40, 40],
        palette: [
            [40, 40, 40], [204, 36, 29], [152, 151, 26], [215, 153, 33], [69, 133, 136], [177, 98, 134], [104, 157, 106], [168, 153, 132],
            [146, 131, 116], [251, 73, 52], [184, 187, 38], [250, 189, 47], [131, 165, 152], [211, 134, 155], [142, 192, 124], [235, 219, 178],
        ],
    },
    ColorScheme {
        name: "Nord",
        foreground: [216, 222, 233],
        background: [46, 52, 64],
        palette: [
            [59, 66, 82], [191, 97, 106], [163, 190, 140], [235, 203, 139], [129, 161, 193], [180, 142, 173], [136, 192, 208], [229, 233, 240],
            [76, 86, 106], [191, 97, 106], [163, 190, 140], [235, 203, 139], [129, 161, 193], [180, 142, 173], [143, 188, 187], [236, 239, 244],
        ],
    },
];

/// The persisted rt configuration (`~/.config/rt/config.toml`). Currently just
/// wraps [`Settings`]; keybinding overrides and colour schemes will join it as
/// those features land. `#[serde(default)]` lets an old or hand-edited file omit
/// anything and still load.
#[derive(Clone, Debug, Default, serde::Serialize, serde::Deserialize)]
#[serde(default)]
pub struct Config {
    pub settings: Settings,
}

impl Config {
    /// The path to the config file: `$XDG_CONFIG_HOME/rt/config.toml`, or
    /// `$HOME/.config/rt/config.toml`. Returns `None` if neither env var is set
    /// (in which case rt runs with defaults and simply doesn't persist).
    pub fn path() -> Option<std::path::PathBuf> {
        // Prefer the XDG base dir; fall back to ~/.config.
        let base = std::env::var_os("XDG_CONFIG_HOME")
            .map(std::path::PathBuf::from)
            .or_else(|| std::env::var_os("HOME").map(|h| std::path::PathBuf::from(h).join(".config")))?;
        Some(base.join("rt").join("config.toml"))
    }

    /// Load the config from disk, returning [`Config::default`] if the file is
    /// missing or unreadable, and a best-effort parse otherwise. Never fails —
    /// a broken config must not stop rt from starting.
    pub fn load() -> Self {
        let Some(path) = Self::path() else { return Self::default() };
        match std::fs::read_to_string(&path) {
            Ok(text) => match toml::from_str(&text) {
                Ok(cfg) => cfg, // parsed a real config
                Err(e) => {
                    // Malformed file: warn and use defaults rather than crash.
                    eprintln!("rt: ignoring malformed {}: {e}", path.display());
                    Self::default()
                }
            },
            Err(_) => Self::default(), // no file yet → defaults
        }
    }

    /// Write the config to disk (creating the directory), so the current
    /// settings survive a restart. Returns an error only for genuine I/O
    /// problems; callers typically log and continue.
    pub fn save(&self) -> std::io::Result<()> {
        let Some(path) = Self::path() else {
            return Ok(()); // nowhere to save (no HOME); silently skip
        };
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?; // ensure ~/.config/rt exists
        }
        // Serialise to TOML; map a serialisation error to an I/O error kind.
        let text = toml::to_string_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&path, text) // atomic enough for a tiny config
    }
}

/// The keymap: an ordered list of `(chord, action)` bindings.
///
/// A `Vec` (not a `HashMap`) because the list is short (a few dozen entries),
/// lookup is O(n) but trivially fast, and a `Vec` preserves the ability to have
/// later user bindings override earlier defaults simply by being searched
/// first. `action_for` returns the first match.
#[derive(Clone, Debug, Default)]
pub struct Keymap {
    bindings: Vec<(Chord, Action)>, // searched front-to-back; user entries go first
}

impl Keymap {
    /// Build the keymap pre-populated with Terminator's default bindings.
    ///
    /// Only the actions rt currently implements are included; the rest of
    /// Terminator's map is intentionally omitted until the matching feature
    /// exists, so no key silently does nothing-but-looks-bound.
    pub fn defaults() -> Self {
        // (Terminator accelerator string, action) pairs, transcribed from
        // reference/terminator/terminatorlib/config.py:126-210.
        let defaults: &[(&str, Action)] = &[
            ("<Shift><Control>o", Action::SplitHoriz),   // split_horiz
            ("<Shift><Control>e", Action::SplitVert),    // split_vert
            ("<Shift><Control>w", Action::CloseTerm),    // close_term
            ("<Shift><Control>t", Action::NewTab),       // new_tab
            ("<Control>Page_Down", Action::NextTab),     // next_tab
            ("<Control>Page_Up", Action::PrevTab),       // prev_tab
            ("<Alt>Up", Action::GoUp),                   // go_up
            ("<Alt>Down", Action::GoDown),               // go_down
            ("<Alt>Left", Action::GoLeft),               // go_left
            ("<Alt>Right", Action::GoRight),             // go_right
            ("<Shift><Control>c", Action::Copy),         // copy
            ("<Shift><Control>v", Action::Paste),        // paste
            ("<Shift><Control>q", Action::CloseWindow),  // close_window
            // rt-specific newspaper-column controls. Deliberately Ctrl+symbol
            // (no Shift) so winit's shifted-symbol remapping can't break them.
            ("<Control>period", Action::ColumnsMore),    // Ctrl+.  -> more columns
            ("<Control>comma", Action::ColumnsFewer),    // Ctrl+,  -> fewer columns
            // Live background-opacity nudges (also settable in preferences).
            ("<Control><Alt>Up", Action::OpacityUp),     // more opaque
            ("<Control><Alt>Down", Action::OpacityDown), // more see-through
            ("<Control><Alt>Right", Action::ScrimUp),    // stronger scrim (less legible behind)
            ("<Control><Alt>Left", Action::ScrimDown),   // weaker scrim (more legible behind)
            // Font zoom (Terminator's zoom_in/out/normal). Ctrl+= and Ctrl++
            // both zoom in ('+' needs Shift on most layouts); Ctrl+- and Ctrl+0.
            ("<Control>equal", Action::ZoomIn),
            ("<Shift><Control>plus", Action::ZoomIn),
            ("<Control>minus", Action::ZoomOut),
            ("<Control>0", Action::ZoomReset),
            ("F11", Action::Fullscreen),                 // fullscreen toggle
            ("<Shift><Control>x", Action::ToggleZoom),   // maximise/restore the focused pane
            ("<Shift><Control>f", Action::Search),       // open the scrollback-search bar
            // Keyboard split resize (Terminator resizes by mouse; rt adds keys).
            ("<Shift><Control>Left", Action::ResizeLeft),
            ("<Shift><Control>Right", Action::ResizeRight),
            ("<Shift><Control>Up", Action::ResizeUp),
            ("<Shift><Control>Down", Action::ResizeDown),
            ("<Shift><Control>r", Action::Rotate),       // rotate the enclosing split
            ("<Shift><Control>a", Action::SplitAuto),    // split along the longer axis
            ("<Shift><Control>g", Action::GroupCycle),   // cycle the pane's input group
            // Patch-bay wiring.
            ("<Shift><Control>y", Action::WireStdout),   // wire stdout jack
            ("<Shift><Control>u", Action::WireStderr),   // wire stderr jack
            ("<Shift><Control>k", Action::Unwire),       // disconnect focused pane
            ("<Shift><Control>p", Action::PipeInto),     // split + pipe stdout in
            ("F1", Action::Manual),                      // built-in manual
        ];
        let mut map = Keymap::default(); // empty binding list
        for (accel, action) in defaults {
            // Parse each default; a malformed default is a programming error, so
            // we skip it rather than panic (keeps `defaults()` infallible).
            if let Some(chord) = Chord::parse(accel) {
                map.bindings.push((chord, *action)); // register the binding
            }
        }
        map
    }

    /// Register (or override) a binding. Inserted at the *front* so it shadows
    /// any earlier binding for the same chord — this is how user config
    /// overrides defaults.
    pub fn bind(&mut self, chord: Chord, action: Action) {
        self.bindings.insert(0, (chord, action)); // front insert = highest priority
    }

    /// Look up the action bound to `chord`, if any. Returns the first match in
    /// priority order (user overrides before defaults).
    pub fn action_for(&self, chord: &Chord) -> Option<Action> {
        self.bindings
            .iter()
            .find(|(c, _)| c == chord) // first chord that matches exactly
            .map(|(_, a)| *a) // hand back just the action
    }

    /// The first accelerator bound to `action`, formatted for display (e.g. the
    /// right-click menu). `None` when the action has no binding.
    pub fn shortcut_for(&self, action: Action) -> Option<String> {
        self.bindings
            .iter()
            .find(|(_, a)| *a == action) // first binding for this action
            .map(|(chord, _)| chord.to_string()) // via the Chord Display impl
    }
}
