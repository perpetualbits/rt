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
    /// Open/close the clipboard-history overlay (recent copies).
    ClipHistory,
    /// Empty the clipboard history.
    ClearClipHistory,
    /// rt-specific: open a new empty rt window (same process).
    NewWindow,
    /// rt-specific: pull the focused pane out of its window into a new window.
    DetachPane,
    /// rt-specific: pull the focused tab out of its window into a new window.
    DetachTab,
    /// rt-specific: pick up the focused pane — carry mode: aim in any rt window,
    /// click to drop.
    PickUpPane,
    /// rt-specific: pick up the focused tab — carry mode: aim in any rt window,
    /// click to drop.
    PickUpTab,
    /// rt-specific: move the focused tab one position toward the start.
    MoveTabLeft,
    /// rt-specific: move the focused tab one position toward the end.
    MoveTabRight,
}

/// Which `NSVisualEffectMaterial` the macOS frosted glass is made of.
///
/// **macOS-only in EFFECT, cross-platform in TYPE.** It lives here, unguarded by
/// any `cfg`, so that one `config.toml` is portable: a Linux rt reading
/// `macos_glass_material = "hud-window"` parses it, keeps it, writes it back
/// unchanged on the next save, and ignores it. Nothing outside
/// `rt/src/vibrancy.rs` reads it, and that file is `cfg(target_os = "macos")`.
///
/// ## Why this exists at all
///
/// `NSVisualEffectView`'s `material` property "Defaults to
/// `NSVisualEffectMaterialAppearanceBased`" (AppKit's own header comment, still
/// there in the objc2 bindings) — a material deprecated since 10.14 and far
/// denser than anything Terminal.app uses. Leaving it unset is what produced the
/// "heavily blurred, almost opaque … a vague light blue or grey" report: that
/// grey-blue is the material's own tint, sitting under the user's configured
/// background colour and reading as a second, unexplained layer.
///
/// ## Ordering
///
/// [`GlassMaterial::ALL`] is the cycle order used by the preferences row, run
/// roughly lightest-to-heaviest so stepping right adds density. [`Self::SystemDefault`]
/// is deliberately LAST: it is the only entry that is not a deliberate choice
/// (it means "never call `setMaterial:`"), kept solely so the old look can be
/// compared against the new one without rebuilding.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum GlassMaterial {
    /// `.underWindowBackground` — "the material used under window backgrounds".
    /// rt's effect view IS under the window's content view, so this is the one
    /// material whose documented purpose is literally rt's placement. The
    /// default.
    #[default]
    UnderWindowBackground,
    /// `.underPageBackground` — the material behind document pages.
    UnderPageBackground,
    /// `.contentBackground` — the opaque background of scroll/table content.
    ContentBackground,
    /// `.windowBackground` — the material used by opaque window backgrounds.
    WindowBackground,
    /// `.sidebar` — the background of window sidebars (a familiar "frosted" look).
    Sidebar,
    /// `.headerView` — in-line header/footer views.
    HeaderView,
    /// `.titlebar` — the material used by window titlebars.
    Titlebar,
    /// `.menu` — the material used by menus.
    Menu,
    /// `.popover` — the background of `NSPopover` windows.
    Popover,
    /// `.sheet` — the background of sheet windows.
    Sheet,
    /// `.fullScreenUI` — the background of full-screen modal UI.
    FullScreenUi,
    /// `.hudWindow` — the background of heads-up-display windows. Dark and dense.
    HudWindow,
    /// Do not call `setMaterial:` at all: whatever AppKit defaults to, which is
    /// the deprecated `.appearanceBased`. Present only as the control case for
    /// comparing against the chosen default — not a recommended value.
    SystemDefault,
}

impl GlassMaterial {
    /// Every material, in preferences cycle order (see the type docs).
    pub const ALL: &'static [GlassMaterial] = &[
        GlassMaterial::UnderWindowBackground,
        GlassMaterial::UnderPageBackground,
        GlassMaterial::ContentBackground,
        GlassMaterial::WindowBackground,
        GlassMaterial::Sidebar,
        GlassMaterial::HeaderView,
        GlassMaterial::Titlebar,
        GlassMaterial::Menu,
        GlassMaterial::Popover,
        GlassMaterial::Sheet,
        GlassMaterial::FullScreenUi,
        GlassMaterial::HudWindow,
        GlassMaterial::SystemDefault,
    ];

    /// The name this material carries in `config.toml`, in `RT_GLASS_MATERIAL`,
    /// and in the preferences row. Kebab-case, matching the AppKit constant.
    pub fn name(self) -> &'static str {
        match self {
            GlassMaterial::UnderWindowBackground => "under-window-background",
            GlassMaterial::UnderPageBackground => "under-page-background",
            GlassMaterial::ContentBackground => "content-background",
            GlassMaterial::WindowBackground => "window-background",
            GlassMaterial::Sidebar => "sidebar",
            GlassMaterial::HeaderView => "header-view",
            GlassMaterial::Titlebar => "titlebar",
            GlassMaterial::Menu => "menu",
            GlassMaterial::Popover => "popover",
            GlassMaterial::Sheet => "sheet",
            GlassMaterial::FullScreenUi => "full-screen-ui",
            GlassMaterial::HudWindow => "hud-window",
            GlassMaterial::SystemDefault => "system-default",
        }
    }

    /// Parse a name from the config file, the env override, or the CLI.
    ///
    /// Deliberately forgiving: case-insensitive, and `_` is accepted for `-`, so
    /// `HUD_WINDOW` and `hud-window` both work. Returns `None` for anything else
    /// — callers report it and fall back rather than failing, because a typo in
    /// one cosmetic field must never cost the user the whole config file (see
    /// the `Deserialize` impl).
    pub fn from_name(s: &str) -> Option<GlassMaterial> {
        let want = s.trim().to_ascii_lowercase().replace('_', "-");
        // "default" is the obvious thing to type for "whatever AppKit does".
        if want == "default" {
            return Some(GlassMaterial::SystemDefault);
        }
        GlassMaterial::ALL.iter().copied().find(|m| m.name() == want)
    }

    /// Step `dir` (+1 / -1) places through [`Self::ALL`], wrapping at both ends.
    /// The preferences row's whole behaviour.
    pub fn step(self, dir: i32) -> GlassMaterial {
        let n = GlassMaterial::ALL.len() as i32;
        // `position` always succeeds: ALL covers every variant, which
        // `all_variants_are_in_all_and_round_trip` pins.
        let cur = GlassMaterial::ALL.iter().position(|m| *m == self).unwrap_or(0) as i32;
        GlassMaterial::ALL[(cur + dir).rem_euclid(n) as usize]
    }
}

impl serde::Serialize for GlassMaterial {
    fn serialize<S: serde::Serializer>(&self, ser: S) -> Result<S::Ok, S::Error> {
        ser.serialize_str(self.name())
    }
}

impl<'de> serde::Deserialize<'de> for GlassMaterial {
    /// Never fails. A derived enum `Deserialize` would reject an unknown name,
    /// and `Config::load` turns ANY parse error into "ignoring malformed
    /// config.toml" — i.e. one mistyped material would silently reset every
    /// preference the user has. So an unrecognised (or non-string) value is
    /// reported on stderr, exactly as `Settings::normalize` reports an
    /// out-of-range number, and the default is used.
    fn deserialize<D: serde::Deserializer<'de>>(de: D) -> Result<Self, D::Error> {
        let Ok(raw) = String::deserialize(de) else {
            eprintln!(
                "rt: config macos_glass_material is not a string; using {}",
                GlassMaterial::default().name()
            );
            return Ok(GlassMaterial::default());
        };
        Ok(GlassMaterial::from_name(&raw).unwrap_or_else(|| {
            eprintln!(
                "rt: config macos_glass_material {raw:?} is not a known material; using {}",
                GlassMaterial::default().name()
            );
            GlassMaterial::default()
        }))
    }
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
    /// When true, ask the compositor to blur whatever is behind the translucent
    /// window (the `ext-background-effect-v1` staging protocol; KDE 6.7+, COSMIC,
    /// niri). A silent no-op on compositors without the protocol (GNOME, older
    /// KWin, X11), and skipped entirely while the background is fully opaque
    /// (`background_opacity == 1.0`), where blur would be wasted work. The blur
    /// radius is the compositor's to choose — the protocol offers only on/off.
    pub background_blur: bool,
    /// **macOS only.** Which `NSVisualEffectMaterial` the frosted glass behind
    /// the window is made of. Ignored everywhere else — on Linux the compositor
    /// owns the blur and the protocol offers only on/off — but always parsed and
    /// preserved, so a single `config.toml` stays portable between machines.
    ///
    /// Only consulted while [`Self::wants_background_blur`] is true: it selects
    /// the *look* of the glass, `background_blur` decides whether there is any.
    ///
    /// Takes effect LIVE — the Preferences dialog's "Glass material" row (macOS
    /// builds only) steps through [`GlassMaterial::ALL`] and the glass changes
    /// under you, no restart needed. Editing this field in `config.toml` by hand
    /// does need a restart, since rt reads the file once at startup;
    /// `RT_GLASS_MATERIAL=<name>` overrides it for one run.
    pub macos_glass_material: GlassMaterial,
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
    /// Draw the border instruments + patch-bay AT ALL on the REMOTE (XRender /
    /// `ssh -X`) backend. OFF by default: the instrument layer lives on a
    /// server-side ARGB pixmap that `present()` composites over the content, and
    /// that composite is X-server CPU proportional to the composited area — a
    /// cost paid on every frame, including every keystroke. It costs rt itself
    /// nothing, so it hides from client-side profiling; measure the X server.
    /// The composite is clipped to the pixels the instruments actually occupy
    /// (see `XRenderBackend::instr_rects`), which makes it affordable on a
    /// reasonable link, but a milkv (riscv64, `ssh -X`) feel-test still showed
    /// default-on lagging typing, so opting in is deliberate. The LOCAL GL
    /// backend always shows the instruments regardless (the pipe is free there).
    pub inst_remote: bool,
    /// Animate the border instruments on the REMOTE (XRender / `ssh -X`) backend.
    /// OFF by default, and only meaningful with `inst_remote`. When on, the
    /// instrument layer redraws on its own decoupled 6fps tick (see
    /// `INSTRUMENT_TICK`), independent of content frames, so animated packets
    /// stay live without forcing content redraws. Off gives static instruments
    /// (still updated on resize/tab/focus). The LOCAL GL backend always animates
    /// regardless of this flag.
    pub inst_animate: bool,
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
    /// Arrow-key acceleration: when true, HOLDING an arrow key (a run of auto-repeats) sends
    /// progressively more cursor moves per repeat, so the cursor speeds up the longer you
    /// hold — turning a long crawl through a line or through `less`/history into a second or
    /// two. A single tap is always exactly one move. When false, each repeat is one move (the
    /// plain OS keyboard repeat rate). Applies to all four arrow keys.
    pub arrow_accel: bool,
    /// The cap on arrow acceleration — the most cursor moves sent per held repeat. Effective
    /// top speed ≈ this × the OS keyboard repeat rate. Clamped to `1..=MAX_ARROW_ACCEL`; `1`
    /// disables acceleration even with `arrow_accel` on.
    pub arrow_accel_max: u32,
    /// The `TERM` value exported into every pane's shell. Default [`DEFAULT_TERM`]
    /// (`xterm-256color`), and you almost certainly want to leave it there.
    ///
    /// **Read this before changing it.** `TERM` is not a preference, it is a promise:
    /// it names a terminfo entry, and every ncurses application on the machine
    /// believes that entry describes rt exactly.
    ///
    /// 1. **If the name has no terminfo entry installed on THIS machine, ncurses
    ///    applications refuse to start** — `vim`, `less`, `top`, `htop`, `mc`, and
    ///    anything else linked against ncurses die with "unknown terminal type" or
    ///    "terminal database is inadequate". Not degraded: broken. Terminfo is
    ///    per-machine, so a value that works on your desktop can break every pane
    ///    over `ssh` to a host that lacks it, and rt cannot install it for you.
    ///    Check with `infocmp <name> >/dev/null` before setting this.
    /// 2. **Borrowing another terminal's name claims everything that terminal can
    ///    do.** `xterm-kitty` is the tempting one: it makes applications that gate
    ///    on the `TERM` *name* (rather than querying `CSI ? u`) negotiate the kitty
    ///    keyboard protocol, which rt genuinely implements. But the same entry also
    ///    advertises the kitty **graphics** protocol, which rt does **not**
    ///    implement — so an image viewer, a `matplotlib` backend or an `icat` will
    ///    emit graphics escapes rt silently discards, and you have traded a missing
    ///    feature for a wrong answer. `xterm-ghostty` has the same shape.
    ///
    /// So this setting exists for a user who has measured a specific problem, knows
    /// which entry is installed where, and accepts the trade. `RT_TERM` in the
    /// environment overrides it for one-off experiments (see [`term_name`]); rt's own
    /// entry, `terminfo/rt.terminfo`, describes rt honestly but must be `tic`-installed
    /// on every machine you use before `term = "rt"` is safe (see the README).
    ///
    /// Values that are empty or contain anything outside `[A-Za-z0-9._+-]` are
    /// rejected by [`Settings::normalize`] and fall back to [`DEFAULT_TERM`].
    pub term: String,
}

/// The `TERM` rt exports into a pane's shell unless told otherwise.
///
/// A borrowed identity — rt is not xterm — but a deliberately conservative one: this
/// entry exists on every machine that has ncurses at all, and rt implements a superset
/// of what it claims (see `terminfo/rt.terminfo` for what rt actually does). Changing
/// this default is a decision with machine-wide blast radius; see [`Settings::term`].
pub const DEFAULT_TERM: &str = "xterm-256color";

/// The environment variable that overrides the configured `TERM` for one run.
pub const TERM_ENV: &str = "RT_TERM";

/// Resolve the `TERM` to export into a pane, applying rt's precedence:
/// **`RT_TERM` in the environment > the `term` config setting > [`DEFAULT_TERM`]**.
///
/// The env var wins because it is the one-off: you export `RT_TERM=xterm-kitty` in one
/// shell to test a claim, and nothing you did to `config.toml` quietly changes what you
/// measured. `configured` is `None` for hosts that have no config file to consult
/// (rt-mux, tests, the engine's own default), which is exactly "fall through to the env
/// var, then the default".
///
/// Blank or syntactically impossible names are ignored at every level rather than
/// exported — a `TERM` containing a NUL or a space cannot name a terminfo entry, and
/// exporting one only moves the failure into the child process.
pub fn term_name(configured: Option<&str>) -> String {
    term_name_from(std::env::var(TERM_ENV).ok().as_deref(), configured)
}

/// The pure half of [`term_name`] — same precedence, with the environment passed in.
/// Split out so the precedence can be tested without `set_var`, which is process-global
/// and races every other test in the binary.
pub fn term_name_from(env: Option<&str>, configured: Option<&str>) -> String {
    for name in [env, configured].into_iter().flatten() {
        let name = name.trim();
        if valid_term_name(name) {
            return name.to_string();
        }
    }
    DEFAULT_TERM.to_string()
}

/// Is `name` shaped like a terminfo entry name? This checks SYNTAX only — whether the
/// entry actually exists on this machine is a question only the machine can answer, and
/// the answer differs per host (see [`Settings::term`]).
///
/// The character set is terminfo's own: entry names are used as path components under
/// `/usr/share/terminfo/<initial>/<name>`, so anything with a `/`, a space or a NUL is
/// not a name at all, it is a way to confuse the child's environment.
pub fn valid_term_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-'))
}

/// Terminal types the Preferences picker offers, besides [`DEFAULT_TERM`] — the ones a
/// user might plausibly want and that mean something specific:
///
/// * `xterm-kitty` / `xterm-ghostty` — the two names that make applications which gate the
///   kitty keyboard protocol on the `TERM` *name* (rather than querying `CSI ? u`)
///   negotiate it. They also claim the kitty **graphics** protocol, which rt does not
///   implement: see [`Settings::term`] for the trade.
/// * `rt` — rt's own entry (`terminfo/rt.terminfo`), which claims only what rt does. It is
///   NOT installed by rt; `tic` it yourself first (see the README) or it will not appear.
const TERM_ALTERNATES: &[&str] = &["xterm-kitty", "xterm-ghostty", "rt"];

/// The terminal types Preferences will let you cycle through on THIS machine.
///
/// Deliberately filtered by [`terminfo_installed`]: offering a name whose terminfo is
/// missing would let one arrow-key press break `vim`, `less` and `htop` in every pane
/// opened afterwards, which is not a preference, it is a trap. `configured` is always
/// included even when its entry is missing — a value hand-written into `config.toml`
/// must stay visible in the list, or the user cannot see what is in effect or step off it.
///
/// The list is a local-machine answer. It says nothing about hosts you `ssh` into, which
/// have their own terminfo databases and will see whatever `TERM` you export.
pub fn term_candidates(configured: &str) -> Vec<String> {
    let mut out = vec![DEFAULT_TERM.to_string()];
    for name in TERM_ALTERNATES {
        if terminfo_installed(name) {
            out.push((*name).to_string());
        }
    }
    let configured = configured.trim();
    if valid_term_name(configured) && !out.iter().any(|n| n == configured) {
        out.push(configured.to_string());
    }
    out
}

/// Does a terminfo entry named `name` exist on this machine?
///
/// This walks the same directories ncurses does, in its order: `$TERMINFO`, then
/// `$TERMINFO_DIRS` (colon-separated; an empty element means the compiled-in default),
/// then `~/.terminfo`, then the usual system trees. Within a tree an entry lives at
/// `<dir>/<first character>/<name>`, or `<dir>/<two hex digits of the first byte>/<name>`
/// on the hashed layout some distributions use — both are checked.
///
/// It answers "is the file there", which is what "will ncurses find it" reduces to for
/// the directory-tree databases every Linux and macOS install uses. A `false` from a
/// system using a single-file hashed database (BSD `terminfo.db`) would be a false
/// negative, which costs a name in a picker list — never a wrong `TERM` export.
pub fn terminfo_installed(name: &str) -> bool {
    if !valid_term_name(name) {
        return false; // not a name at all; never touch the filesystem with it
    }
    let first = &name[..1]; // ASCII by `valid_term_name`, so a byte is a char
    let hashed = format!("{:02x}", name.as_bytes()[0]);
    let mut dirs: Vec<std::path::PathBuf> = Vec::new();
    if let Some(d) = std::env::var_os("TERMINFO") {
        dirs.push(d.into());
    }
    if let Some(list) = std::env::var_os("TERMINFO_DIRS") {
        for part in std::env::split_paths(&list) {
            // An empty element in TERMINFO_DIRS means "the compiled-in default location",
            // which the system trees below already cover.
            if !part.as_os_str().is_empty() {
                dirs.push(part);
            }
        }
    }
    if let Some(home) = std::env::var_os("HOME") {
        dirs.push(std::path::PathBuf::from(home).join(".terminfo"));
    }
    for sys in ["/etc/terminfo", "/lib/terminfo", "/usr/share/terminfo", "/usr/lib/terminfo"] {
        dirs.push(std::path::PathBuf::from(sys));
    }
    dirs.iter().any(|d| d.join(first).join(name).exists() || d.join(&hashed).join(name).exists())
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
    /// Sensible defaults: fully opaque, click-to-focus.
    fn default() -> Self {
        Settings {
            background_opacity: 1.0,       // opaque until the user dials it down
            background_blur: true,         // request compositor blur when translucent (no-op if unsupported)
            // macOS glass material. `.underWindowBackground` is the one whose
            // documented job ("the material used under window backgrounds") is
            // exactly where rt puts the effect view — under the content view —
            // and it is the lightest of the behind-window materials, which is
            // what "frosted glass you can still see the rocks through" needs.
            // NOT AppKit's own default: that is the deprecated `.appearanceBased`,
            // which is what made rt's glass read as an opaque grey-blue haze.
            macos_glass_material: GlassMaterial::UnderWindowBackground,
            focus_follows_mouse: false,    // click-to-focus by default
            show_titlebar: true,           // Terminator-style per-pane titlebars on by default
            inst_output: true,             // border instruments on by default
            inst_heat: true,
            inst_latency: true,
            show_jacks: true,              // patch-bay jacks visible by default
            inst_remote: false,            // off over ssh -X: the layer composite is server-side CPU that scales with
            // window area, and a milkv (riscv64, ssh -X) feel-test showed default-on made typing lag badly. Opt in
            // with `inst_remote = true` once you know your X server can afford it.
            inst_animate: false,           // 6fps decoupled instrument tick (see INSTRUMENT_TICK); only with inst_remote
            foreground: [0xd0, 0xd0, 0xd8], // light grey text
            background: [0x10, 0x10, 0x14], // near-black background
            palette: DEFAULT_PALETTE,      // classic xterm 16-colour palette
            font_family: "DejaVu Sans Mono".to_string(), // ubiquitous monospace default
            font_size: 18.0,               // pixels
            scrollback: 10_000,            // matches rt_engine::DEFAULT_SCROLLBACK
            arrow_accel: true,             // hold-to-accelerate arrows on by default
            arrow_accel_max: 10,           // up to 10 cursor moves per held repeat
            term: DEFAULT_TERM.to_string(), // the borrowed-but-universally-installed identity
        }
    }
}

impl Settings {
    /// The smallest opacity we allow, so the window never vanishes entirely.
    pub const MIN_OPACITY: f32 = 0.05;
    /// Upper bound for the scrollback slider. 5M lines still suits listing/searching a
    /// large tree, but a *full* buffer is heavy — very roughly ~1.5 GB per million
    /// 80-column lines — so the titlebar shows a used/max meter to watch it. On the
    /// in-house engine a per-pane memory budget (rt_engine's SCROLLBACK_MEMORY_BUDGET)
    /// evicts oldest-first before a maxed slider can exhaust RAM; the grid only allocates
    /// as it fills, so an unused ceiling is cheap. Was 20M — lowered so even the vendored
    /// backend (line-capped only, no byte budget) can't be driven to an OOM.
    pub const MAX_SCROLLBACK: usize = 5_000_000;

    /// Upper bound for the arrow-acceleration slider: the most cursor moves sent per held
    /// key-repeat. Effective top speed is this times the OS keyboard repeat rate.
    pub const MAX_ARROW_ACCEL: u32 = 30;

    /// Does this settings state want background blur / frosted glass right now?
    ///
    /// Both halves matter: the user's toggle AND a translucent background. Blur
    /// behind a fully opaque surface is invisible by construction and costs the
    /// compositor (or, on macOS, the window server) real work for nothing.
    ///
    /// The SINGLE source of truth for that decision, shared by every backend —
    /// Wayland `ext-background-effect-v1`, the X11
    /// `_KDE_NET_WM_BLUR_BEHIND_REGION` property, and the macOS
    /// `NSVisualEffectView` — at startup and on every runtime opacity/config
    /// change. It lives here, not in `rt/src/main.rs`, because `main.rs` is the
    /// binary's display-bound run-loop and cannot be unit-tested, while this is
    /// plain data.
    pub fn wants_background_blur(&self) -> bool {
        self.background_blur && self.background_opacity < 1.0
    }

    /// Nudge the opacity by `delta`, clamped to `[MIN_OPACITY, 1.0]`. Returns
    /// the new value. Used by the `OpacityUp`/`OpacityDown` actions.
    pub fn adjust_opacity(&mut self, delta: f32) -> f32 {
        // Clamp so we stay in a usable, always-visible range.
        self.background_opacity = (self.background_opacity + delta).clamp(Self::MIN_OPACITY, 1.0);
        self.background_opacity
    }

    /// Clamp deserialized values into their supported ranges. `toml::from_str` bypasses the
    /// bounds the Preferences UI enforces, so a hand-edited or corrupt file could otherwise
    /// inject a non-finite/absurd float or an out-of-policy scrollback that later drives
    /// pathological allocation or invalid rendering state. Each correction is reported.
    /// [review RT-CONF-001]
    pub fn normalize(&mut self) {
        fn clamp_f32(v: &mut f32, lo: f32, hi: f32, default: f32, name: &str) {
            if !v.is_finite() {
                eprintln!("rt: config {name} was not finite; using {default}");
                *v = default;
            } else if *v < lo || *v > hi {
                let c = v.clamp(lo, hi);
                eprintln!("rt: config {name} {v} out of range [{lo}, {hi}]; clamped to {c}");
                *v = c;
            }
        }
        clamp_f32(&mut self.background_opacity, Self::MIN_OPACITY, 1.0, 1.0, "background_opacity");
        clamp_f32(&mut self.font_size, 4.0, 200.0, 18.0, "font_size");
        if self.scrollback > Self::MAX_SCROLLBACK {
            eprintln!(
                "rt: config scrollback {} exceeds max {}; clamped",
                self.scrollback, Self::MAX_SCROLLBACK
            );
            self.scrollback = Self::MAX_SCROLLBACK;
        }
        if self.arrow_accel_max < 1 || self.arrow_accel_max > Self::MAX_ARROW_ACCEL {
            let c = self.arrow_accel_max.clamp(1, Self::MAX_ARROW_ACCEL);
            eprintln!(
                "rt: config arrow_accel_max {} out of range [1, {}]; clamped to {c}",
                self.arrow_accel_max, Self::MAX_ARROW_ACCEL
            );
            self.arrow_accel_max = c;
        }
        // A `term` that isn't shaped like a terminfo entry name can only break the child
        // (see `valid_term_name`), so refuse it here rather than export it. Note this
        // cannot check that the entry EXISTS — that is per-machine and rt has no business
        // guessing; see the field's own comment for what a wrong-but-valid name costs.
        let trimmed = self.term.trim();
        if !valid_term_name(trimmed) {
            eprintln!(
                "rt: config term {:?} is not a usable terminfo entry name; using {DEFAULT_TERM}",
                self.term
            );
            self.term = DEFAULT_TERM.to_string();
        } else if trimmed.len() != self.term.len() {
            self.term = trimmed.to_string(); // stray whitespace, otherwise fine
        }
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
            Ok(text) => match toml::from_str::<Config>(&text) {
                Ok(mut cfg) => {
                    cfg.settings.normalize(); // clamp hand-edited/corrupt values into range
                    cfg
                }
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
        // Atomic write: a crash mid-`write` would otherwise leave a truncated TOML that
        // fails to parse next launch and resets every preference. Write a sibling temp file,
        // then rename over the target (atomic on the same filesystem). [review, hardening #8]
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &path)
    }
}

/// The keymap: an ordered list of `(chord, action)` bindings.
///
/// A `Vec` (not a `HashMap`) because the list is short (a few dozen entries),
/// lookup is O(n) but trivially fast, and a `Vec` preserves the ability to have
/// later user bindings override earlier defaults simply by being searched
/// first. `action_for` returns the first match.
#[derive(Clone, Debug)]
pub struct Keymap {
    bindings: Vec<(Chord, Action)>, // searched front-to-back; user entries go first
}

impl Default for Keymap {
    fn default() -> Self {
        Keymap::defaults()
    }
}

/// macOS's Command-key defaults, layered **on top of** the Terminator table.
///
/// Why additive and not a replacement: only twenty of rt's forty-odd actions
/// have a Mac reflex worth naming. Replacing the Terminator table would strip
/// the keyboard route to the other two dozen (rotate, group cycle, the whole
/// patch-bay, detach/pick-up, resize, broadcast) — and `Ctrl+Shift+<letter>`
/// collides with nothing macOS reserves, so keeping it costs nothing. A Mac
/// user reaches for ⌘ and finds it; anyone who knows rt already keeps every
/// key they know.
///
/// **Spelling matters.** AppKit's `charactersIgnoringModifiers` — which is what
/// winit reports as the logical key once Command is held — ignores Command and
/// Option but *honours Shift*. So ⌘⇧[ arrives as `{`, ⌘⇧= as `+`, and ⌘⇧/ as
/// `?`. Binding the unshifted symbol would compile, parse, and never once fire.
///
/// **What is deliberately absent:**
/// - **⌘Q (Quit)** — winit's AppKit backend installs the standard application
///   menu, whose Quit item owns ⌘Q and calls `terminate:`. AppKit consumes that
///   key equivalent before the event reaches rt's window, so a binding here
///   could never run. It is also the right outcome: ⌘Q means "quit rt", which
///   is what `terminate:` does. rt's own escalation ladder is ⌘W (this pane,
///   and the window with it once it is the last pane) → ⇧⌘W (this window) → ⌘Q
///   (everything).
/// - **⌘H / ⌥⌘H** — same menu, Hide and Hide Others.
/// - **⌘1..⌘9** — rt has no "select tab N" action at all; only next/prev exist.
///   Inventing one is a session-layer feature, not a keymap default.
#[cfg(target_os = "macos")]
const MACOS_DEFAULTS: &[(&str, Action)] = &[
    // Clipboard — the whole reason this table exists.
    ("<Super>c", Action::Copy),
    ("<Super>v", Action::Paste),
    // Tabs, panes and windows. ⌘W closes the focused PANE (iTerm2's semantics);
    // when it is the last pane the session closes the window anyway, so the key
    // still reads as "close this thing" at every depth. ⇧⌘W takes the window.
    ("<Super>t", Action::NewTab),
    ("<Super>w", Action::CloseTerm),
    ("<Shift><Super>w", Action::CloseWindow),
    ("<Super>n", Action::NewWindow),
    // Splits, matching iTerm2: ⌘D gives two panes side by side (rt's "vertical
    // divider"), ⇧⌘D stacks them.
    ("<Super>d", Action::SplitVert),
    ("<Shift><Super>d", Action::SplitHoriz),
    // Tab cycling, twice over on purpose. ⇧⌘[ / ⇧⌘] is the reflex (Safari,
    // Chrome, Terminal.app) but is layout-dependent — it only produces `{`/`}`
    // on a US-ish layout. ⌥⌘← / ⌥⌘→ is Terminal.app's and iTerm2's other
    // spelling, comes off named keys that no layout remaps, and is the only one
    // reachable on a Mac laptop without Fn (rt's Ctrl+PageUp/Dn needs Fn there).
    ("<Shift><Super>{", Action::PrevTab),
    ("<Shift><Super>}", Action::NextTab),
    ("<Alt><Super>Left", Action::PrevTab),
    ("<Alt><Super>Right", Action::NextTab),
    // ⌘, is the strongest convention macOS has; rt had no Preferences key at
    // all before (menu only), so this adds one rather than moving one.
    ("<Super>comma", Action::Preferences),
    ("<Super>f", Action::Search), // ⌘F = Find, here the scrollback search bar
    // Font zoom. ⌘= and ⌘⇧+ both zoom in (the second is what ⌘+ really sends).
    ("<Super>equal", Action::ZoomIn),
    ("<Shift><Super>plus", Action::ZoomIn),
    ("<Super>minus", Action::ZoomOut),
    ("<Super>0", Action::ZoomReset),
    // ⌃⌘F is macOS's own fullscreen key. rt's F11 still works, but F11 on a Mac
    // keyboard is a system media key that needs Fn.
    ("<Control><Super>f", Action::Fullscreen),
    // ⌘⇧? is macOS's Help key. Same reasoning as fullscreen: F1 needs Fn, and
    // the manual is how a new user finds everything else.
    ("<Shift><Super>?", Action::Manual),
];

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
            ("<Shift><Control>h", Action::ClipHistory),  // clipboard history
            ("<Shift><Control>q", Action::CloseWindow),  // close_window
            // rt-specific newspaper-column controls. Deliberately Ctrl+symbol
            // (no Shift) so winit's shifted-symbol remapping can't break them.
            ("<Control>period", Action::ColumnsMore),    // Ctrl+.  -> more columns
            ("<Control>comma", Action::ColumnsFewer),    // Ctrl+,  -> fewer columns
            // Live background-opacity nudges (also settable in preferences).
            ("<Control><Alt>Up", Action::OpacityUp),     // more opaque
            ("<Control><Alt>Down", Action::OpacityDown), // more see-through
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
            // Multi-window pane/tab drag-and-drop keyboard equivalents.
            ("<Shift><Control>i", Action::NewWindow),    // new_window (Terminator)
            ("<Shift><Control>d", Action::DetachPane),   // detach pane to its own window
            ("<Shift><Control>j", Action::DetachTab),    // detach tab to its own window
            ("<Shift><Control>m", Action::PickUpPane),   // carry the pane: aim, then click to drop
            ("<Shift><Control>n", Action::PickUpTab),    // carry the tab: aim, then click to drop
            ("<Shift><Control>Page_Up", Action::MoveTabLeft),  // move_tab (Terminator)
            ("<Shift><Control>Page_Down", Action::MoveTabRight), // move_tab (Terminator)
        ];
        let mut map = Keymap { bindings: Vec::new() }; // empty binding list
        // macOS: the Command-key set goes in FIRST, so it wins `shortcut_for`
        // (menus and the manual then offer a Mac user ⌘C, not Ctrl+Shift+C).
        // It cannot shadow anything in `action_for` — every chord below carries
        // Super, which no Terminator default does.
        #[cfg(target_os = "macos")]
        for (accel, action) in MACOS_DEFAULTS {
            if let Some(chord) = Chord::parse(accel) {
                map.bindings.push((chord, *action));
            }
        }
        for (accel, action) in defaults {
            // Parse each default; a malformed default is a programming error, so
            // we skip it rather than panic (keeps `defaults()` infallible).
            if let Some(chord) = Chord::parse(accel) {
                map.bindings.push((chord, *action)); // register the binding
            }
        }
        map
    }

    /// Every binding in priority order (user overrides first, then defaults).
    /// Read-only; used by the manual's "every binding is documented" test.
    pub fn bindings(&self) -> impl Iterator<Item = (&Chord, &Action)> {
        self.bindings.iter().map(|(c, a)| (c, a))
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
        self.chord_for(action).map(|chord| chord.to_string()) // via the Chord Display impl
    }

    /// The first accelerator bound to `action`, UNRENDERED.
    ///
    /// [`shortcut_for`](Self::shortcut_for) is the same lookup with
    /// [`Chord`]'s `Display` applied; a caller that needs the modifier set and
    /// the key separately — the macOS menu bar, which hands AppKit a
    /// `keyEquivalent` string plus a `keyEquivalentModifierMask` and lets it
    /// draw the ⌃⌥⇧⌘ glyphs itself — takes this instead of re-parsing a string
    /// rt just finished formatting.
    pub fn chord_for(&self, action: Action) -> Option<Chord> {
        self.bindings
            .iter()
            .find(|(_, a)| *a == action) // first binding for this action
            .map(|(chord, _)| *chord)
    }
}

#[cfg(test)]
mod config_tests {
    use super::*;

    #[test]
    fn normalize_clamps_out_of_range_and_non_finite() {
        let mut s = Settings {
            background_opacity: f32::NAN,          // non-finite -> default 1.0
            font_size: 100_000.0,                  // absurd -> clamped to 200
            scrollback: Settings::MAX_SCROLLBACK + 1, // over policy -> clamped
            ..Settings::default()
        };
        s.normalize();
        assert_eq!(s.background_opacity, 1.0, "non-finite opacity replaced with default");
        assert_eq!(s.font_size, 200.0, "huge font size clamped to the max");
        assert_eq!(s.scrollback, Settings::MAX_SCROLLBACK, "scrollback clamped to policy max");

        // A negative opacity clamps up to the minimum; valid values pass through untouched.
        let mut s2 = Settings { background_opacity: -5.0, font_size: 14.0, ..Settings::default() };
        s2.normalize();
        assert_eq!(s2.background_opacity, Settings::MIN_OPACITY);
        assert_eq!(s2.font_size, 14.0, "an in-range value is left alone");
    }

    #[test]
    fn blur_is_wanted_only_when_enabled_and_translucent() {
        // The gate every backend shares — Wayland, X11 and the macOS glass.
        let mut s = Settings::default();
        s.background_blur = true;
        s.background_opacity = 1.0;
        assert!(!s.wants_background_blur(), "blur behind an opaque window is invisible work");
        s.background_opacity = 0.05;
        assert!(s.wants_background_blur(), "enabled + translucent -> yes");
        s.background_blur = false;
        assert!(!s.wants_background_blur(), "the user's toggle must be able to turn it OFF");
        s.background_opacity = 1.0;
        assert!(!s.wants_background_blur());
    }

    #[test]
    fn glass_material_defaults_to_under_window_background() {
        // NOT AppKit's own default (.appearanceBased, deprecated and dense).
        assert_eq!(Settings::default().macos_glass_material, GlassMaterial::UnderWindowBackground);
        assert_eq!(GlassMaterial::default(), GlassMaterial::UnderWindowBackground);
    }

    #[test]
    fn all_glass_materials_round_trip_through_their_names() {
        for m in GlassMaterial::ALL {
            assert_eq!(GlassMaterial::from_name(m.name()), Some(*m), "{}", m.name());
        }
        // Every name the brief promised the user can type is real.
        for want in [
            "under-window-background", "hud-window", "full-screen-ui",
            "sidebar", "popover", "window-background", "system-default",
        ] {
            assert!(GlassMaterial::from_name(want).is_some(), "{want} must be a valid material");
        }
        // Forgiving spellings.
        assert_eq!(GlassMaterial::from_name("HUD_WINDOW"), Some(GlassMaterial::HudWindow));
        assert_eq!(GlassMaterial::from_name(" Sidebar "), Some(GlassMaterial::Sidebar));
        assert_eq!(GlassMaterial::from_name("default"), Some(GlassMaterial::SystemDefault));
        assert_eq!(GlassMaterial::from_name("frosted"), None);
    }

    #[test]
    fn glass_material_steps_and_wraps_both_ways() {
        let first = GlassMaterial::ALL[0];
        let last = GlassMaterial::ALL[GlassMaterial::ALL.len() - 1];
        assert_eq!(first.step(-1), last, "wraps backward off the front");
        assert_eq!(last.step(1), first, "wraps forward off the end");
        assert_eq!(first.step(1).step(-1), first, "a step and back is identity");
    }

    /// A config naming a material must load on EVERY platform. rt is one binary
    /// per OS reading one `config.toml` the user may sync between them, and a
    /// hard parse error here would not be a warning — `Config::load` discards the
    /// whole file on any error, resetting every preference.
    #[test]
    fn a_config_naming_a_material_loads_and_survives_a_round_trip_on_any_platform() {
        let cfg: Config = toml::from_str("[settings]\nmacos_glass_material = \"hud-window\"\n")
            .expect("a material name must parse on Linux too");
        assert_eq!(cfg.settings.macos_glass_material, GlassMaterial::HudWindow);
        // And it is written back unchanged, so a Linux run does not silently
        // rewrite a Mac's chosen material.
        let text = toml::to_string_pretty(&cfg).expect("serialisable");
        assert!(text.contains("macos_glass_material = \"hud-window\""), "{text}");

        // An unknown or wrongly-typed value degrades to the default rather than
        // failing the parse and taking every other setting down with it.
        for bad in ["macos_glass_material = \"frosted-glass\"", "macos_glass_material = 7"] {
            let cfg: Config = toml::from_str(&format!("[settings]\nfont_size = 21.0\n{bad}\n"))
                .unwrap_or_else(|e| panic!("{bad} must not fail the load: {e}"));
            assert_eq!(cfg.settings.macos_glass_material, GlassMaterial::default());
            assert_eq!(cfg.settings.font_size, 21.0, "the rest of the file must survive");
        }
    }

    #[test]
    fn ctrl_shift_h_opens_clip_history_by_default() {
        let km = Keymap::default();
        let chord = keys::Chord::parse("<Shift><Control>h").expect("valid chord");
        assert_eq!(km.action_for(&chord), Some(Action::ClipHistory));
    }

    #[test]
    fn window_and_tab_move_actions_have_default_chords() {
        let km = Keymap::default();
        let expect = [
            ("<Shift><Control>i", Action::NewWindow),
            ("<Shift><Control>d", Action::DetachPane),
            ("<Shift><Control>j", Action::DetachTab),
            ("<Shift><Control>Page_Up", Action::MoveTabLeft),
            ("<Shift><Control>Page_Down", Action::MoveTabRight),
        ];
        for (accel, action) in expect {
            let chord = keys::Chord::parse(accel).expect("valid chord");
            assert_eq!(km.action_for(&chord), Some(action), "{accel}");
        }
    }

    #[test]
    fn pickup_actions_have_default_chords() {
        let km = Keymap::default();
        for (accel, action) in [
            ("<Shift><Control>m", Action::PickUpPane),
            ("<Shift><Control>n", Action::PickUpTab),
        ] {
            let chord = keys::Chord::parse(accel).expect("valid chord");
            assert_eq!(km.action_for(&chord), Some(action), "{accel}");
        }
    }

    /// The Terminator-transcribed default table, frozen.
    ///
    /// This is the list rt's Linux keymap IS — every entry, in order, with the
    /// accelerator spelled exactly as `Keymap::defaults()` spells it. It exists
    /// so that a change to Linux's defaults cannot happen quietly: adding,
    /// removing, reordering or respelling any line below is a deliberate act
    /// that fails this test until the frozen copy is updated too.
    ///
    /// Deliberately NOT `Mods::SUPER`-bearing: the platform additions (macOS's
    /// Command-key set) are the *only* bindings allowed to carry Super, which is
    /// what lets `frozen_table_survives_platform_additions` filter them out and
    /// compare the rest on every target, macOS included.
    const FROZEN_LINUX_DEFAULTS: &[(&str, Action)] = &[
        ("<Shift><Control>o", Action::SplitHoriz),
        ("<Shift><Control>e", Action::SplitVert),
        ("<Shift><Control>w", Action::CloseTerm),
        ("<Shift><Control>t", Action::NewTab),
        ("<Control>Page_Down", Action::NextTab),
        ("<Control>Page_Up", Action::PrevTab),
        ("<Alt>Up", Action::GoUp),
        ("<Alt>Down", Action::GoDown),
        ("<Alt>Left", Action::GoLeft),
        ("<Alt>Right", Action::GoRight),
        ("<Shift><Control>c", Action::Copy),
        ("<Shift><Control>v", Action::Paste),
        ("<Shift><Control>h", Action::ClipHistory),
        ("<Shift><Control>q", Action::CloseWindow),
        ("<Control>period", Action::ColumnsMore),
        ("<Control>comma", Action::ColumnsFewer),
        ("<Control><Alt>Up", Action::OpacityUp),
        ("<Control><Alt>Down", Action::OpacityDown),
        ("<Control>equal", Action::ZoomIn),
        ("<Shift><Control>plus", Action::ZoomIn),
        ("<Control>minus", Action::ZoomOut),
        ("<Control>0", Action::ZoomReset),
        ("F11", Action::Fullscreen),
        ("<Shift><Control>x", Action::ToggleZoom),
        ("<Shift><Control>f", Action::Search),
        ("<Shift><Control>Left", Action::ResizeLeft),
        ("<Shift><Control>Right", Action::ResizeRight),
        ("<Shift><Control>Up", Action::ResizeUp),
        ("<Shift><Control>Down", Action::ResizeDown),
        ("<Shift><Control>r", Action::Rotate),
        ("<Shift><Control>a", Action::SplitAuto),
        ("<Shift><Control>g", Action::GroupCycle),
        ("<Shift><Control>y", Action::WireStdout),
        ("<Shift><Control>u", Action::WireStderr),
        ("<Shift><Control>k", Action::Unwire),
        ("<Shift><Control>p", Action::PipeInto),
        ("F1", Action::Manual),
        ("<Shift><Control>i", Action::NewWindow),
        ("<Shift><Control>d", Action::DetachPane),
        ("<Shift><Control>j", Action::DetachTab),
        ("<Shift><Control>m", Action::PickUpPane),
        ("<Shift><Control>n", Action::PickUpTab),
        ("<Shift><Control>Page_Up", Action::MoveTabLeft),
        ("<Shift><Control>Page_Down", Action::MoveTabRight),
    ];

    /// The Linux table is exactly [`FROZEN_LINUX_DEFAULTS`], in that order,
    /// once any platform (Super-bearing) additions are filtered out.
    ///
    /// Runs on EVERY target on purpose. On Linux it pins the table outright; on
    /// macOS it proves the Command-key additions were layered on top without
    /// disturbing, reordering or shadowing a single Terminator binding.
    #[test]
    fn frozen_table_survives_platform_additions() {
        let km = Keymap::defaults();
        let expected: Vec<(Chord, Action)> = FROZEN_LINUX_DEFAULTS
            .iter()
            .map(|(accel, action)| (Chord::parse(accel).expect("frozen accel parses"), *action))
            .collect();
        // Every binding that does NOT carry Super is, by definition, part of the
        // portable table — collect them in priority order and compare.
        let actual: Vec<(Chord, Action)> = km
            .bindings()
            .filter(|(c, _)| !c.mods.contains(Mods::SUPER))
            .map(|(c, a)| (*c, *a))
            .collect();
        assert_eq!(actual, expected, "the Terminator-transcribed default table changed");
    }

    /// Terminator accelerator strings still parse and still resolve to the same
    /// action after the platform additions — the compatibility promise spelled
    /// out one chord at a time, in Terminator's own syntax.
    #[test]
    fn terminator_accelerators_still_resolve() {
        let km = Keymap::defaults();
        for (accel, action) in FROZEN_LINUX_DEFAULTS {
            let chord = Chord::parse(accel).unwrap_or_else(|| panic!("{accel} must parse"));
            assert_eq!(km.action_for(&chord), Some(*action), "{accel}");
        }
        // And the aliases Terminator/GTK also writes resolve identically.
        assert_eq!(Chord::parse("<Ctrl><Shift>o"), Chord::parse("<Shift><Control>o"));
        assert_eq!(Chord::parse("<Primary><Shift>o"), Chord::parse("<Shift><Control>o"));
    }

    /// Off macOS, nothing is bound to Super at all: the platform additions must
    /// not leak onto Linux. Paired with the frozen table above, this says the
    /// Linux keymap is byte-for-byte what it was.
    #[cfg(not(target_os = "macos"))]
    #[test]
    fn no_super_bindings_off_macos() {
        let km = Keymap::defaults();
        let supers: Vec<String> = km
            .bindings()
            .filter(|(c, _)| c.mods.contains(Mods::SUPER))
            .map(|(c, a)| format!("{c} ({a:?})"))
            .collect();
        assert!(supers.is_empty(), "Super bindings leaked onto a non-macOS target: {supers:?}");
        assert_eq!(
            km.bindings().count(),
            FROZEN_LINUX_DEFAULTS.len(),
            "the default keymap grew (or shrank) off macOS"
        );
    }

    /// On macOS the Command key resolves to the action a Mac user expects.
    ///
    /// The accelerators are spelled the way the key event actually ARRIVES:
    /// AppKit's `charactersIgnoringModifiers` honours Shift, so ⌘⇧[ reaches rt
    /// as `{`, ⌘⇧= as `+`, and ⌘⇧/ as `?` — binding `[`/`=`/`/` would silently
    /// never fire.
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_command_bindings_resolve() {
        let km = Keymap::defaults();
        for (accel, action) in [
            ("<Super>c", Action::Copy),
            ("<Super>v", Action::Paste),
            ("<Super>t", Action::NewTab),
            ("<Super>w", Action::CloseTerm),
            ("<Shift><Super>w", Action::CloseWindow),
            ("<Super>n", Action::NewWindow),
            ("<Super>d", Action::SplitVert),
            ("<Shift><Super>d", Action::SplitHoriz),
            ("<Shift><Super>{", Action::PrevTab),
            ("<Shift><Super>}", Action::NextTab),
            ("<Alt><Super>Left", Action::PrevTab),
            ("<Alt><Super>Right", Action::NextTab),
            ("<Super>comma", Action::Preferences),
            ("<Super>f", Action::Search),
            ("<Super>equal", Action::ZoomIn),
            ("<Shift><Super>plus", Action::ZoomIn),
            ("<Super>minus", Action::ZoomOut),
            ("<Super>0", Action::ZoomReset),
            ("<Control><Super>f", Action::Fullscreen),
            ("<Shift><Super>?", Action::Manual),
        ] {
            let chord = Chord::parse(accel).unwrap_or_else(|| panic!("{accel} must parse"));
            assert_eq!(km.action_for(&chord), Some(action), "{accel}");
        }
    }

    /// On macOS the Command bindings take PRIORITY for display: the right-click
    /// menu and the manual must offer a Mac user ⌘C, not Ctrl+Shift+C, even
    /// though both fire. (Both remain bound — see the frozen table.)
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_command_bindings_are_what_menus_show() {
        let km = Keymap::defaults();
        assert_eq!(km.shortcut_for(Action::Copy).as_deref(), Some("Cmd+C"));
        assert_eq!(km.shortcut_for(Action::Paste).as_deref(), Some("Cmd+V"));
        assert_eq!(km.shortcut_for(Action::NewTab).as_deref(), Some("Cmd+T"));
        // …while the Terminator chord still works.
        let ctrl_shift_c = Chord::parse("<Shift><Control>c").expect("valid chord");
        assert_eq!(km.action_for(&ctrl_shift_c), Some(Action::Copy));
    }

    /// ⌘Q is deliberately UNBOUND. winit's AppKit backend installs the standard
    /// application menu, whose Quit item owns ⌘Q and calls `terminate:` — the
    /// key equivalent is consumed by AppKit before the event ever reaches rt's
    /// window, so a binding here would be dead code that also lied to the user
    /// about what the key does. Same story for ⌘H (Hide) and ⌥⌘H (Hide Others).
    #[cfg(target_os = "macos")]
    #[test]
    fn macos_leaves_appkit_menu_equivalents_alone() {
        let km = Keymap::defaults();
        for accel in ["<Super>q", "<Super>h", "<Alt><Super>h"] {
            let chord = Chord::parse(accel).expect("valid chord");
            assert_eq!(km.action_for(&chord), None, "{accel} belongs to AppKit's menu");
        }
    }

    /// The Super modifier displays as macOS's own name for the key on macOS,
    /// and stays "Super" everywhere else. This is display only — it changes no
    /// binding, and no Linux default carries Super in the first place.
    #[test]
    fn super_displays_per_platform() {
        let chord = Chord::parse("<Super>c").expect("valid chord");
        let shown = chord.to_string();
        if cfg!(target_os = "macos") {
            assert_eq!(shown, "Cmd+C");
        } else {
            assert_eq!(shown, "Super+C");
        }
    }
}
