#!/usr/bin/env bash
# Build rt's release .dmg — the drag-to-Applications disk image a Mac user
# expects, wrapped around the rt.app bundle.sh already produces.
#
#   ./dmg.sh                 # bundle.sh (which builds), then wrap into a .dmg
#   ./dmg.sh --no-build       # reuse target/release/rt (skips cargo build)
#   ./dmg.sh --app PATH       # wrap an already-built rt.app instead of building one
#   ./dmg.sh --out DIR        # write DIR/rt-<version>-macos-arm64.dmg
#
# The result:
#
#   rt-<version>-macos-arm64.dmg
#     rt.app            <- the ad-hoc-signed bundle bundle.sh built
#     Applications       <- a symlink to /Applications
#
# Dragging rt.app onto Applications inside the mounted image is the entire
# install. No installer, no scripts run on the user's machine — that is the
# convention this format exists to give a Mac user.
#
# THIS BUILD IS UNSIGNED (ad-hoc only, no Apple Developer ID, no notarisation)
# and Apple Silicon (arm64) ONLY. Both are load-bearing for the filename and the
# release notes in RELEASE_NOTES_TEMPLATE.md next to this script — read that
# file before publishing a .dmg this script produced, and do not silently start
# shipping an Intel or a signed build without updating both.
#
# Runs on macOS only: hdiutil has no equivalent this script uses elsewhere.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
BUNDLE_SH="$HERE/bundle.sh"

if [ "$(uname -s)" != "Darwin" ]; then
  echo "dmg.sh only runs on macOS (hdiutil is not available elsewhere)" >&2
  exit 1
fi

APP=""
OUT="$ROOT/target/macos"
BUNDLE_ARGS=()
while [ $# -gt 0 ]; do
  case "$1" in
    --no-build)  BUNDLE_ARGS+=(--no-build) ;;
    --bin)       BUNDLE_ARGS+=(--bin "${2:?--bin needs a path}"); shift ;;
    --app)       APP="${2:?--app needs a path}"; shift ;;
    --out)       OUT="${2:?--out needs a directory}"; shift ;;
    -h|--help)   sed -n '2,17p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

VERSION="$(sed -n 's/^version = "\(.*\)".*/\1/p' "$ROOT/crates/rt/Cargo.toml" | head -1)"
[ -n "$VERSION" ] || { echo "could not read version from crates/rt/Cargo.toml" >&2; exit 1; }

# Architecture check, at BUILD time, not just in the filename: a bundle.sh run
# on an Intel Mac (or under Rosetta) would silently produce an x86_64 or
# translated binary that this script would then happily label "arm64". This
# release is arm64-only by decision (see RELEASE_NOTES_TEMPLATE.md); catch a
# mismatch here rather than shipping a mislabelled .dmg.
HOST_ARCH="$(uname -m)"
if [ "$HOST_ARCH" != "arm64" ]; then
  echo "dmg.sh: this build is arm64 (Apple Silicon) only, host reports $HOST_ARCH" >&2
  echo "refusing to produce a .dmg that would be mislabelled" >&2
  exit 1
fi

mkdir -p "$OUT"

if [ -z "$APP" ]; then
  # `${arr[@]}` on an empty array trips `set -u` under bash 3.2 (macOS's system
  # bash), which this script must run under — guard rather than assume bash 4+.
  if [ "${#BUNDLE_ARGS[@]}" -gt 0 ]; then
    "$BUNDLE_SH" "${BUNDLE_ARGS[@]}" --out "$OUT"
  else
    "$BUNDLE_SH" --out "$OUT"
  fi
  APP="$OUT/rt.app"
fi
[ -d "$APP" ] || { echo "no app bundle at $APP" >&2; exit 1; }

BIN_ARCH="$(lipo -archs "$APP/Contents/MacOS/rt" 2>/dev/null || true)"
case "$BIN_ARCH" in
  *arm64*) ;;
  *) echo "dmg.sh: $APP/Contents/MacOS/rt reports architecture '$BIN_ARCH', expected arm64" >&2; exit 1 ;;
esac

DMG_NAME="rt-$VERSION-macos-arm64.dmg"
DMG_PATH="$OUT/$DMG_NAME"
VOL_NAME="rt $VERSION"

# --- assemble the staging directory -----------------------------------------
# A plain window (Finder's default icon view, no custom background/positions)
# rather than a scripted layout: hdiutil's own -fs/-volname options give a
# correct, working image with zero AppleScript. Achieving the "icon view with
# rt.app and Applications laid out side by side" look needs osascript driving
# Finder — UI scripting against a live session, which this build must not do
# (see the module docs: no synthesised input, no driving Finder/System Events).
# A plain window with the two icons present is what most .dmg-building tools
# fall back to anyway when they skip AppleScript; it is correct, just unstyled.
STAGE="$(mktemp -d)"
trap 'rm -rf "$STAGE"' EXIT
cp -R "$APP" "$STAGE/rt.app"
ln -s /Applications "$STAGE/Applications"

rm -f "$DMG_PATH"
# UDZO: compressed, read-only — the standard shape for a distributed .dmg.
hdiutil create -volname "$VOL_NAME" -srcfolder "$STAGE" -ov -format UDZO "$DMG_PATH" >/dev/null

echo "built $DMG_PATH  (rt $VERSION, arm64, unsigned)"
echo "verify with:  hdiutil verify $DMG_PATH"
