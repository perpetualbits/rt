#!/usr/bin/env bash
# Build rt.app — the macOS application bundle. The macOS counterpart of
# extra/linux/install.sh, which does the same job for a freedesktop launcher.
#
#   ./bundle.sh                 # cargo build --release, then assemble target/macos/rt.app
#   ./bundle.sh --no-build      # assemble around an already-built target/release/rt
#   ./bundle.sh --bin PATH      # assemble around some other rt binary
#   ./bundle.sh --out DIR       # write DIR/rt.app instead of target/macos/rt.app
#
# The bundle it produces:
#
#   rt.app/Contents/Info.plist          <- from Info.plist.in, with @VERSION@ filled in
#   rt.app/Contents/MacOS/rt            <- the release binary
#   rt.app/Contents/Resources/rt.icns   <- from rt.png (itself from ../logo/rt.svg)
#   rt.app/Contents/PkgInfo             <- "APPL????", the four-byte legacy stamp
#
# Runs on macOS (the normal case) and on Linux (to author or inspect a bundle
# without a Mac). The icon is the only step that differs: macOS uses the sips +
# iconutil pair that every Mac already has, Linux uses png2icns from libicns
# ("apt install icnsutils"). If neither is available the bundle is still built,
# without an icon, and says so — a missing icon costs the Dock picture, nothing else.
#
# On macOS the result is ad-hoc signed (`codesign --force -s -`). That is not a
# Developer ID and it is not notarised; it is what makes the bundle launchable at
# all on Apple silicon, where the kernel refuses an unsigned Mach-O.
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
ROOT="$(cd "$HERE/../.." && pwd)"
PNG="$HERE/rt.png"
SVG="$ROOT/extra/logo/rt.svg"
PLIST_IN="$HERE/Info.plist.in"

BUILD=1
BIN=""
OUT="$ROOT/target/macos"
while [ $# -gt 0 ]; do
  case "$1" in
    --no-build)  BUILD=0 ;;
    --bin)       BIN="${2:?--bin needs a path}"; BUILD=0; shift ;;
    --out)       OUT="${2:?--out needs a directory}"; shift ;;
    -h|--help)   sed -n '2,9p' "$0"; exit 0 ;;
    *) echo "unknown option: $1" >&2; exit 2 ;;
  esac
  shift
done

# The version the plist advertises is read from the crate, so it cannot drift
# from what `rt --version` prints.
VERSION="$(sed -n 's/^version = "\(.*\)".*/\1/p' "$ROOT/crates/rt/Cargo.toml" | head -1)"
[ -n "$VERSION" ] || { echo "could not read version from crates/rt/Cargo.toml" >&2; exit 1; }

if [ "$BUILD" = 1 ]; then
  echo "building rt $VERSION (release)…"
  ( cd "$ROOT" && cargo build --release --bin rt )
fi
[ -n "$BIN" ] || BIN="$ROOT/target/release/rt"
[ -x "$BIN" ] || { echo "no rt binary at $BIN (drop --no-build, or pass --bin)" >&2; exit 1; }

APP="$OUT/rt.app"
rm -rf "$APP"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"

# --- the icon, first, because the plist only names it if it exists -----------
# rt.png is a 1024x1024 render of extra/logo/rt.svg, committed because macOS has
# no SVG rasteriser at all (sips cannot read SVG). Regenerate it on a Linux box:
#   rsvg-convert -w 1024 -h 1024 extra/logo/rt.svg -o extra/macos/rt.png
ICNS="$APP/Contents/Resources/rt.icns"

# Rasterise one size into $2: from the SVG when a rasteriser is around (sharper),
# otherwise by downscaling the committed PNG.
rasterise() { # size outfile
  if command -v rsvg-convert >/dev/null 2>&1 && [ -f "$SVG" ]; then
    rsvg-convert -w "$1" -h "$1" "$SVG" -o "$2"
  elif command -v magick >/dev/null 2>&1; then
    magick "$PNG" -resize "$1x$1" "$2"
  elif command -v convert >/dev/null 2>&1; then
    convert "$PNG" -resize "$1x$1" "$2"
  elif command -v sips >/dev/null 2>&1; then
    sips -z "$1" "$1" "$PNG" --out "$2" >/dev/null
  else
    return 1
  fi
}

if [ ! -f "$PNG" ] && [ ! -f "$SVG" ]; then
  echo "warning: no $PNG and no $SVG; bundling without an icon" >&2
elif command -v iconutil >/dev/null 2>&1; then
  # The macOS path. iconutil wants an .iconset directory with Apple's exact names.
  TMP="$(mktemp -d)"; SET="$TMP/rt.iconset"; mkdir -p "$SET"
  ok=1
  for pair in 16:icon_16x16 32:icon_16x16@2x 32:icon_32x32 64:icon_32x32@2x \
              128:icon_128x128 256:icon_128x128@2x 256:icon_256x256 \
              512:icon_256x256@2x 512:icon_512x512 1024:icon_512x512@2x; do
    rasterise "${pair%%:*}" "$SET/${pair#*:}.png" || { ok=0; break; }
  done
  if [ "$ok" = 1 ]; then iconutil -c icns "$SET" -o "$ICNS"; fi
  rm -rf "$TMP"
elif command -v png2icns >/dev/null 2>&1; then
  # The Linux path: libicns. One file per size, only the sizes it knows.
  TMP="$(mktemp -d)"; pngs=()
  for s in 16 32 48 128 256 512 1024; do
    if rasterise "$s" "$TMP/$s.png"; then pngs+=("$TMP/$s.png"); fi
  done
  if [ ${#pngs[@]} -gt 0 ]; then png2icns "$ICNS" "${pngs[@]}" >/dev/null; fi
  rm -rf "$TMP"
else
  echo "warning: no iconutil (macOS) and no png2icns (libicns); bundling without an icon" >&2
fi

# --- Info.plist -------------------------------------------------------------
# Drop CFBundleIconFile when there is no icns: a plist that names a missing file
# makes the Finder show the generic app icon *and* log an error every launch.
if [ -f "$ICNS" ]; then
  sed "s/@VERSION@/$VERSION/g" "$PLIST_IN" > "$APP/Contents/Info.plist"
else
  sed "s/@VERSION@/$VERSION/g" "$PLIST_IN" \
    | awk '/<key>CFBundleIconFile<\/key>/{skip=2} skip{skip--;next} {print}' \
    > "$APP/Contents/Info.plist"
fi
# APPL???? is the classic type/creator stamp. Modern macOS reads the plist, but
# the file costs nothing and some tools still look for it.
printf 'APPL????' > "$APP/Contents/PkgInfo"

# --- the binary -------------------------------------------------------------
# Copy to a temp name and rename into place. Overwriting a Mach-O in place whose
# signature the kernel has cached gives every later launch "Killed: 9" — the same
# trap docs/MACOS.md warns about for ~/.cargo/bin/rt.
cp -f "$BIN" "$APP/Contents/MacOS/.rt.new"
mv -f "$APP/Contents/MacOS/.rt.new" "$APP/Contents/MacOS/rt"
chmod 755 "$APP/Contents/MacOS/rt"

# --- sign -------------------------------------------------------------------
# Ad-hoc ("-"). --force replaces the signature rustc already put on the binary,
# which no longer covers it now that it sits inside a bundle with a plist.
# Not --deep: it is deprecated for signing, and there is nothing nested to reach.
if command -v codesign >/dev/null 2>&1; then
  codesign --force -s - "$APP"
fi

echo "built $APP  (rt $VERSION)"
echo "install it with:  $HERE/install.sh"
