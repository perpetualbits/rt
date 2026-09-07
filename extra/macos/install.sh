#!/usr/bin/env bash
# Install (or remove) rt.app so rt appears in the Finder, Launchpad and Spotlight.
# The macOS counterpart of extra/linux/install.sh. System-wide by default, because
# /Applications is group-writable by admin users and that is where a Mac user looks.
#
#   ./install.sh              # build the bundle, install it to /Applications
#   ./install.sh --user       # install to ~/Applications instead
#   ./install.sh --no-build   # install the target/macos/rt.app already sitting there
#   ./install.sh --link-cli   # also symlink /usr/local/bin/rt -> the bundled binary
#   ./install.sh --uninstall  # remove a previous install (respects --user/--link-cli)
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"

[ "$(uname -s)" = "Darwin" ] || {
  echo "install.sh: this installs a macOS .app; on Linux use extra/linux/install.sh" >&2
  exit 1
}

MODE=install
DEST=/Applications
LINK_CLI=0
BUILD=1
CLI=/usr/local/bin/rt
for arg in "$@"; do
  case "$arg" in
    --user)      DEST="$HOME/Applications" ;;
    --no-build)  BUILD=0 ;;
    --link-cli)  LINK_CLI=1 ;;
    --uninstall) MODE=uninstall ;;
    -h|--help)   sed -n '2,10p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done
APP="$DEST/rt.app"

lsregister() {
  local ls=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
  [ -x "$ls" ] && "$ls" "$@" >/dev/null 2>&1 || true
}

if [ "$MODE" = uninstall ]; then
  lsregister -u "$APP"
  rm -rf "$APP"
  # Only remove the symlink if it is ours; never delete somebody's real binary.
  if [ -L "$CLI" ]; then
    case "$(readlink "$CLI")" in
      */rt.app/Contents/MacOS/rt) rm -f "$CLI"; echo "removed $CLI" ;;
    esac
  fi
  echo "removed $APP"
  exit 0
fi

# Rebuild by default. Reusing whatever happens to be in target/macos would quietly
# install a bundle older than the source tree; `cargo build` is incremental, so the
# honest default costs seconds. --no-build is there for the deliberate case.
SRC="$ROOT/target/macos/rt.app"
if [ "$BUILD" = 1 ]; then "$HERE/bundle.sh"; fi
[ -d "$SRC" ] || { echo "no bundle at $SRC (drop --no-build)" >&2; exit 1; }

mkdir -p "$DEST"
# Replace wholesale rather than merging into an older bundle: a leftover file from
# a previous version invalidates the code signature and the app then refuses to run.
rm -rf "$APP"
if command -v ditto >/dev/null 2>&1; then ditto "$SRC" "$APP"; else cp -R "$SRC" "$APP"; fi

# Belt and braces: a bundle you built or copied locally has no quarantine flag, but
# one that arrived in a .zip or a download does, and that is the only thing Gatekeeper
# actually blocks on. Stripping it here is what stops the "damaged / cannot be opened"
# dialog for an app that is merely unsigned by Apple's reckoning.
xattr -dr com.apple.quarantine "$APP" 2>/dev/null || true

# Tell LaunchServices about it now, so Spotlight and the Finder see it immediately
# instead of whenever the next background scan happens to run. Unregister the copy
# in target/macos at the same time: two bundles with one identifier makes Launchpad
# and `open -b` pick whichever LaunchServices saw last, which can be the build tree.
lsregister -u "$SRC"
lsregister -f "$APP"

if [ "$LINK_CLI" = 1 ]; then
  # Deliberately opt-in. `cargo install --path crates/rt` already puts an rt on PATH
  # in ~/.cargo/bin, and having both means two rt binaries of possibly different
  # versions with PATH order deciding which one you get. Use this when the .app is
  # your whole installation.
  mkdir -p "$(dirname "$CLI")" 2>/dev/null || true
  if ln -sf "$APP/Contents/MacOS/rt" "$CLI" 2>/dev/null; then
    echo "linked $CLI -> $APP/Contents/MacOS/rt"
    other="$(command -v rt 2>/dev/null || true)"
    if [ -n "$other" ] && [ "$other" != "$CLI" ]; then
      echo "note: '$other' also on PATH and comes first; remove it or reorder PATH" >&2
    fi
  else
    echo "could not write $CLI (try: sudo $0 $*)" >&2
  fi
fi

echo "installed $APP"
echo "open it from the Finder, Launchpad or Spotlight."
