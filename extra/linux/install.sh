#!/usr/bin/env bash
# Install (or remove) rt's desktop entry and icons so rt appears in application
# launchers with its icon. User-local by default; --system installs to /usr/share.
#
#   ./install.sh              # install to ~/.local/share
#   ./install.sh --system     # install to /usr/share   (run with sudo)
#   ./install.sh --uninstall  # remove a previous install (respects --system)
set -euo pipefail

APP_ID="io.github.perpetualbits.rt"
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SVG="$HERE/../logo/rt.svg"
DESKTOP_SRC="$HERE/$APP_ID.desktop"
SIZES=(16 24 32 48 64 128 256)

MODE=install
PREFIX="${XDG_DATA_HOME:-$HOME/.local/share}"
for arg in "$@"; do
  case "$arg" in
    --system)    PREFIX="/usr/share" ;;
    --uninstall) MODE=uninstall ;;
    -h|--help)   sed -n '2,9p' "$0"; exit 0 ;;
    *) echo "unknown option: $arg" >&2; exit 2 ;;
  esac
done

APPS="$PREFIX/applications"
ICONS="$PREFIX/icons/hicolor"

if [ "$MODE" = uninstall ]; then
  rm -f "$APPS/$APP_ID.desktop"
  rm -f "$ICONS/scalable/apps/$APP_ID.svg"
  for s in "${SIZES[@]}"; do rm -f "$ICONS/${s}x${s}/apps/$APP_ID.png"; done
  echo "removed rt desktop entry and icons from $PREFIX"
else
  # Pick a rasterizer for the PNG sizes; the scalable SVG alone is enough for
  # modern desktops, so a missing rasterizer is only a warning.
  raster=""
  for t in rsvg-convert inkscape convert magick; do command -v "$t" >/dev/null 2>&1 && { raster="$t"; break; }; done

  # Use mkdir/cp rather than coreutils `install`: a user's PATH may shadow
  # `install` with an unrelated script, so we don't rely on it being present.
  mkdir -p "$ICONS/scalable/apps"
  cp -f "$SVG" "$ICONS/scalable/apps/$APP_ID.svg"; chmod 644 "$ICONS/scalable/apps/$APP_ID.svg"
  for s in "${SIZES[@]}"; do
    out="$ICONS/${s}x${s}/apps/$APP_ID.png"; mkdir -p "$(dirname "$out")"
    case "$raster" in
      rsvg-convert)   rsvg-convert -w "$s" -h "$s" "$SVG" -o "$out" ;;
      inkscape)       inkscape "$SVG" --export-type=png -w "$s" -h "$s" -o "$out" >/dev/null 2>&1 ;;
      convert|magick) "$raster" -background none -density 384 "$SVG" -resize "${s}x${s}" "$out" ;;
      "")             : ;;
    esac
  done
  [ -z "$raster" ] && echo "note: no SVG rasterizer found; installed scalable SVG only" >&2

  # Install the desktop entry, pinning Exec/TryExec to the absolute rt path so it
  # launches from graphical sessions that don't inherit ~/.cargo/bin on PATH.
  RT_BIN="$(command -v rt || true)"
  mkdir -p "$APPS"
  if [ -n "$RT_BIN" ]; then
    sed -e "s|^TryExec=rt$|TryExec=$RT_BIN|" \
        -e "s|^Exec=rt$|Exec=$RT_BIN|" "$DESKTOP_SRC" > "$APPS/$APP_ID.desktop"
  else
    echo "warning: 'rt' not on PATH; leaving Exec=rt (install rt, then re-run)" >&2
    cp -f "$DESKTOP_SRC" "$APPS/$APP_ID.desktop"
  fi
  chmod 644 "$APPS/$APP_ID.desktop"
  echo "installed rt desktop entry + icons to $PREFIX"
fi

# Refresh the desktop/icon caches if the tools are present (harmless if not).
command -v update-desktop-database >/dev/null 2>&1 && update-desktop-database "$APPS" 2>/dev/null || true
command -v gtk-update-icon-cache    >/dev/null 2>&1 && gtk-update-icon-cache -qtf "$ICONS" 2>/dev/null || true
