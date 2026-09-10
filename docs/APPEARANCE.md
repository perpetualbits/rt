# Appearance: translucency and background blur

Terminator has a Profiles → Background page with "Transparent background" + a
transparency slider (and, on some setups, compositor blur). This documents where
rt stands on the same ground, and — importantly — the hard Wayland constraint
around *blur*.

## Translucency (done, native)

A Wayland client **can** make its own background translucent: render with an
alpha channel < 1 on an alpha-capable surface, and the compositor blends the
window over whatever is behind it. rt does this:

- The EGL surface is created with an alpha channel (`with_alpha_size(8)`).
- The renderer uses **premultiplied-alpha** compositing (`glBlendFunc(ONE,
  ONE_MINUS_SRC_ALPHA)`, fragment outputs `rgb·a, a`) — what compositors expect.
  For fully-opaque content this is identical to straight blending, so normal
  rendering is unchanged (verified).
- The background clear carries the opacity in its alpha; **glyphs and chrome
  stay fully opaque**, so text is always crisp regardless of background opacity.

Controls:
- `Ctrl+Alt+Up` / `Ctrl+Alt+Down` — nudge opacity ±5% (range `0.05..=1.0`).
- `RT_OPACITY=0.8` env var — seed the opacity at startup (demos/screenshots).
- Future: a preferences panel with a real slider persists this to a config file.

## Background blur — the Wayland reality

**A Wayland client cannot blur the content behind its own window.** Wayland's
security model forbids a client from reading or processing the pixels of other
windows / the framebuffer behind it. So true "blur what's underneath" is
**exclusively the compositor's job**. Concretely:

| Compositor | Blur-behind | Client control |
|---|---|---|
| KDE KWin | yes (dual-Kawase, cheap) | `org_kde_kwin_blur` protocol: client can request blur **on/off + region**. **Strength is a KWin global**, not client-settable. |
| Hyprland | yes | Configured by compositor window rules; **no client protocol**. User enables it in `hyprland.conf`. |
| wlroots/sway | no built-in blur | — |
| Mutter/GNOME | no window blur | — |

Consequences for rt:
1. rt can *request* blur where a protocol exists (KDE), as an on/off toggle.
2. A **client-controlled blur-strength slider is not achievable** on Wayland —
   there is no standard protocol for it, and KWin's strength is a global.
3. On compositors without a blur protocol, the user enables blur in the
   compositor's own settings; rt cannot do it for them.

### The scrim (implemented) — the portable slider that works everywhere

Since we can't blur what's behind, but the goal is *"the window below is visible
but its text is not too legible,"* rt draws a client-side **scrim**: over the
translucent background and *behind* the text, a neutral mid-tone wash that
compresses the contrast (and thus legibility) of whatever shows through, without
hiding its gross shapes/motion. Not Gaussian blur, but cheap, portable, and
fully rt-controllable — and it is the *only* option on COSMIC and GNOME.

- Setting: `scrim_strength` (`0.0..=0.95`; 0 = off). Wash colour is a mid neutral
  (`#505058`) chosen to kill contrast faster than it darkens.
- Controls: `Ctrl+Alt+Right` / `Ctrl+Alt+Left` (±5%), `RT_SCRIM=0.5` env.
- Verified rendering: `docs/screenshots/scrim.png` (over an opaque bg the wash is
  visible as a neutral background; over a translucent bg it de-legibilises what
  is behind — that composited case is only observable on a real display).

**Why a separate slider from opacity?** On Wayland both ultimately act through
the surface alpha, but they are tuned differently: opacity uses the *dark* bg
colour (dims toward black), while the scrim uses a *mid-neutral* colour, which
compresses the contrast range of what shows through — so text below goes
unreadable while a bright button or moving video stays perceptible. Combining a
low opacity with a moderate scrim is the "see it, can't read it" sweet spot.

### Decision (session 1): scrim slider + KWin blur — both implemented
User chose the portable scrim (built) **plus** requesting true compositor blur on
KDE/KWin as a bonus (their daily drivers are COSMIC + GNOME, which have no
compositor blur).

**KWin blur: implemented** in `crates/rt/src/blur.rs`. On startup rt wraps
winit's `wl_display` in its own connection, does one registry roundtrip, and — if
the compositor advertises `org_kde_kwin_blur_manager` — reconstructs winit's
`wl_surface` and requests blur over the whole surface. It is a safe no-op
everywhere else. wayland-client routes each proxy's events to its owning queue,
so our setup roundtrip buffers rather than steals winit's events.
- **Verified safe** on a non-KDE compositor: it logs "blur manager not
  advertised (non-KDE?); relying on the scrim" and the app runs normally — the
  foreign-display integration does not disturb winit's loop.
- **Not yet verified on live KWin** (no KDE session available here). The protocol
  calls are correct by construction; confirm on a KDE box that the window gains
  a real blur behind it.

## macOS: the frosted glass and its material

macOS is the one platform where rt gets real blur-behind for free: the window
server blurs behind a transparent window (`crates/rt/src/vibrancy.rs`), which is
the AppKit sibling of `blur.rs` and `bg_effect.rs`. Three things about it were
wrong in the first cuts and are worth recording, because all three were reported
from a real screen rather than reasoned about.

**It is gated, like every other blur path.** The effect view used to be installed
unconditionally, on the theory that glass behind an opaque background is
invisible so there was nothing to toggle. At `background_opacity = 0.05` it is
the dominant thing on screen, and `background_blur = false` did nothing at all.
Install and removal now hang off `Settings::wants_background_blur()` — the same
`background_blur && background_opacity < 1.0` the Wayland and X11 paths use — and
`apply_blur` re-runs the decision on every opacity step and settings commit, so
the preference and the slider both take effect live. `vibrancy::set_enabled` is
idempotent: it finds its own view by an `identifier` tag rather than remembering
a handle, so the hierarchy is the only source of truth and a repeated call can
never stack a second pane of glass. The decision itself is
`vibrancy_policy::glass_plan`, deliberately not `cfg`'d so Linux CI tests it —
and it decides BOTH mechanisms at once, so switching between them takes the old
one down in the same pass it brings the new one up.

**The material is chosen, not defaulted.** AppKit's `material` property "Defaults
to `NSVisualEffectMaterialAppearanceBased`" — deprecated since 10.14, and much
denser than what Terminal.app shows. Leaving it unset is what made rt's glass
read as an almost-opaque grey-blue haze with the user's own background colour
faintly on top of it.

`macos_glass_material` in `config.toml` names it. Among the materials the pick is
`under-window-background`: AppKit documents `.underWindowBackground` as "the
material used under window backgrounds", which is literally where rt puts the
effect view (below the content view, as a sibling one level up), and it is the
lightest of the behind-window materials.

**And a material was still the wrong default.** Judged on a real screen against
Terminal.app, every one of them lost, for two reasons that no amount of picking a
better material can fix:

- **Every `NSVisualEffectMaterial` carries its own tint.** It composites *under*
  the terminal's background colour, so at a low `background_opacity` the tint is
  most of what is on screen and the chosen colour scheme is no longer the colour
  that was chosen. The user's words: `hud-window` "is blue coloured, which messes
  up the colors I want to choose".
- **`NSVisualEffectView` exposes no blur radius.** AppKit picks one per material.
  "Too blurry" has no answer.

Terminal.app does not use `NSVisualEffectView` at all. Profiles → Window is a
background colour with its own opacity plus a separate **Blur slider**: a
variable-radius, *untinted* blur behind an otherwise plain window. That is the
private `CGSSetWindowBackgroundBlurRadius`, and it is now rt's default, as
`macos_glass_material = "window-blur"` — no effect view is installed, and the
only colour on screen is `background` at `background_opacity`.

rt makes that private call itself rather than through winit's `Window::set_blur`,
for exactly one reason: winit's `set_blur` hardcodes the radius at 80 ("in
general we want to specify the blur radius, but the choice of 80 should be a
reasonable default" — its own comment), and 80 *is* the "too blurry". The radius
is `macos_blur_radius`, `1`–`100`, default `24`, with a Preferences row of its
own that is live only in `window-blur` mode. Being private SPI, the call fails
soft: a non-zero `CGError` is logged and the window is merely translucent.
`Window::set_blur` remains the fallback for when AppKit cannot be reached at all.

The full list, in Preferences cycle order (the plain blur first, then the
materials roughly lightest to heaviest):

`window-blur`, `under-window-background`, `under-page-background`,
`content-background`, `window-background`, `sidebar`, `header-view`, `titlebar`,
`menu`, `popover`, `sheet`, `full-screen-ui`, `hud-window`, `system-default`.

`system-default` means "never call `setMaterial:`" — the deprecated AppKit
default, kept only as the control case to compare against.

Three ways to set it, no rebuild needed for any of them:

- **Preferences → Appearance → "Glass material"** (macOS builds only; the row is
  dimmed while there is no glass on screen). Left/Right steps through the list
  and the window changes **live**.
- `macos_glass_material = "hud-window"` in `~/.config/rt/config.toml` — read at
  startup, so this one needs a restart.
- `RT_GLASS_MATERIAL=hud-window rt` — one run, overrides the config file.

The one change that does *not* apply live is switching **to** `system-default`: a
view that already carries a material cannot be talked back into AppKit's implicit
default, so that takes a restart. Every other material applies immediately.

The setting is macOS-only in **effect** but cross-platform in **type**: a Linux rt
parses it, keeps it, and writes it back unchanged, so one `config.toml` stays
portable between machines. An unknown name is reported on stderr and falls back to
the default rather than failing the parse — `Config::load` discards the whole file
on any parse error, so one mistyped material would otherwise reset every
preference the user has.

## The in-window chrome — one design system, three themes

rt draws its own panels: the context menu, the built-in manual (F1), the
preferences dialog, the colour picker, the clipboard history and the search bar.
They are one **native** draw on both backends (GL and XRender), so Linux and
macOS see exactly the same chrome.

### The system

`crates/rt/src/chrome/theme.rs` is the whole of it, and it is the only place a
chrome colour or a chrome measurement comes from.

- **Palette roles, not greys.** `panel`, `edge`, `sep`, `hover`, `sel` /
  `sel_text`, `text`, `dim`, `off`, `accent`, `thumb`, `field`. A panel reaches
  for a role; it never writes a colour literal. (Before this, `chrome/*.rs` held
  forty hardcoded literals and no two panels agreed on any of them.)
- **Derived from your terminal.** Every role is computed from your configured
  `foreground`, `background` and palette entry 12, so the chrome belongs to the
  terminal it floats over rather than clashing with it.
- **Contrast is guaranteed, not hoped for.** Primary text clears 7:1 against the
  panel, secondary 4.5:1, headings 4.5:1, text on a selection 4.5:1 — at every
  colour scheme rt ships and at both extremes (a pure-black and a pure-white
  terminal). The panel body itself is held ≥1.15:1 against your background so it
  reads as a separate surface. All of it is asserted by unit test.
- **A 4 px spacing scale**, registered in `chrome_scale::logical` and multiplied
  by the display's backing factor at every use: `PANEL_PAD_X` 14 across,
  `PANEL_PAD_Y` 8 down, `PANEL_GAP` 8 between groups, `PANEL_RADIUS` 7 on every
  corner, `PANEL_SEL_INSET` 5 for a highlight's standoff.
- **One row rhythm.** Every list row in every panel is `cell_h + 8` logical px
  tall with its text vertically centred, and every panel is a rounded rectangle
  with a hairline edge that follows the curve.

### `chrome_theme`

Whether floating panels should take the terminal's hue or stay a neutral macOS
graphite is a matter of taste, so it is a setting rather than a decision — the
same argument that produced `macos_glass_material`. All three derive from your
own colours and all three meet the same contrast floors; they differ only in how
much of the terminal's identity the chrome borrows.

| value | what it looks like |
|---|---|
| `tinted` (default) | panels take the hue of your background, lifted (dark themes) or settled (light themes) into a legible band, with your bright-blue palette entry as the accent |
| `graphite` | neutral greys at the weight macOS uses for menus and popovers; only light-vs-dark is taken from your background |
| `contrast` | tinted, pushed to the ends: fully opaque, stronger border and separator, text held to a 9:1 floor |

Two ways to set it:

- **Preferences → Appearance → "Chrome theme"** — Left/Right steps it and every
  panel re-derives **live**, so the three can be compared in one sitting.
- `chrome_theme = "graphite"` in `~/.config/rt/config.toml`.

An unknown name is reported on stderr and falls back to `tinted`, for the same
reason an unknown glass material does.

### Panels that are taller than the window

A context menu with several "Move Pane to …" rows, or a clipboard history with
many clips, can be taller than a short rt window. Such a panel used to be pinned
to the top edge with its tail simply off-screen and no way to reach it. Now:

- The **context menu** scrolls. A `▲` / `▼` cue appears at whichever end has more
  rows; the wheel, the Up/Down arrows (which also walk the rows, with Return to
  pick one) or a click on a cue move through it.
- The **clipboard history** scrolls with its selection — the arrow keys already
  move that, so there is no second piece of state to keep in step.
- The **preferences dialog** already scrolled; it now also clamps its own height,
  and shows a scroll thumb.
- The **colour picker** fits itself to the window: the saturation/value square is
  the elastic part and gives way first, then the gaps, so nothing is ever laid
  out beyond the panel edge.
