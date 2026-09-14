# macOS section for the release notes

Paste this into the release description for the tag, filling in `<VERSION>`.
Update it if the signing or architecture decision ever changes — both are
asserted by `dmg.sh` at build time, but the prose here does not check itself.

---

## macOS (Apple Silicon)

**`rt-<VERSION>-macos-arm64.dmg`** — Apple Silicon (M1 and later) only. There is
no Intel build in this release; on an Intel Mac, build from source instead (see
[`docs/MACOS.md`](../../docs/MACOS.md)).

This build is **not signed with an Apple Developer ID and not notarised** — it
is ad-hoc signed only, which is what lets it run at all on Apple Silicon but
does not satisfy Gatekeeper. **The first time you open it, macOS will refuse
it as coming from an unidentified developer.** This is expected. Two ways
through, after dragging `rt.app` to Applications:

```sh
xattr -dr com.apple.quarantine /Applications/rt.app
```

or double-click it, let it be refused once, then go to **System Settings →
Privacy & Security**, scroll to the Security section, and press **"Open
Anyway"** next to the message about rt. (macOS 15 removed the older
right-click → Open shortcut — neither button in the first dialog does
anything; the real control is in Settings.)

### Installing

1. Open `rt-<VERSION>-macos-arm64.dmg`.
2. Drag `rt.app` onto the `Applications` shortcut in the same window.
3. Clear the quarantine flag (above), then launch rt from Applications,
   Launchpad, or Spotlight.

### Building it yourself instead

The Command Line Tools are required either way; see
[`docs/MACOS.md`](../../docs/MACOS.md) for the exact steps, including a build
error some CLT versions currently hit and its one-line fix.
