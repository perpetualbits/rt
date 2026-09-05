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
server blurs behind a transparent window through `NSVisualEffectView`
(`crates/rt/src/vibrancy.rs`), which is the AppKit sibling of `blur.rs` and
`bg_effect.rs`. Two things about it were wrong in the first cut and are worth
recording, because both were reported from a real screen rather than reasoned
about.

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
`vibrancy_policy::glass_action`, deliberately not `cfg`'d so Linux CI tests it.

**The material is chosen, not defaulted.** AppKit's `material` property "Defaults
to `NSVisualEffectMaterialAppearanceBased`" — deprecated since 10.14, and much
denser than what Terminal.app shows. Leaving it unset is what made rt's glass
read as an almost-opaque grey-blue haze with the user's own background colour
faintly on top of it.

`macos_glass_material` in `config.toml` names it. The default is
`under-window-background`: AppKit documents `.underWindowBackground` as "the
material used under window backgrounds", which is literally where rt puts the
effect view (below the content view, as a sibling one level up), and it is the
lightest of the behind-window materials — the one that leaves the desktop behind
the window legible rather than merely present.

The full list, in Preferences cycle order (roughly lightest to heaviest):

`under-window-background`, `under-page-background`, `content-background`,
`window-background`, `sidebar`, `header-view`, `titlebar`, `menu`, `popover`,
`sheet`, `full-screen-ui`, `hud-window`, `system-default`.

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
