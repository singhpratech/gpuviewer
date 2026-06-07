#!/bin/sh
# Build the gpuviewer AppImage from an already-built release binary.
#
# WHY AppImage: the .deb/.rpm cover only the Debian and RHEL families. An AppImage is a
# single self-contained file that runs on any glibc->=floor distribution (Arch, openSUSE,
# Gentoo, NixOS, ...) with no package manager and no root — the distro-agnostic channel.
# It does NOT bundle glibc, so the same Ubuntu-22.04 glibc 2.35 floor as the tarball
# applies (docs/design/cross-platform.md §7). It deliberately does NOT bundle
# libnvidia-ml.so.1: that lib is dlopen'd at runtime (invisible to ldd), is host/driver
# specific, and must come from the user's system — bundling a vendor driver lib is exactly
# the soname-churn trap the project avoids.
#
# Usage:  tools/make-appimage.sh [path-to-gpuviewer-binary]
#   VERSION=<x.y.z>   override version (default: gpuviewer-tui manifest version)
#   ARCH=<x86_64>     target arch token used in the output name and embedded runtime
#   OUT=<dir>         output directory (default: <repo>/dist)
#   APPIMAGETOOL=<f>  use this appimagetool AppImage instead of downloading (CI passes a
#                     pre-fetched, SHA-verified copy; the download path below SHA-verifies too)
#
# POSIX sh on purpose (runs the same on a minimal runner and a dev box).
set -eu

# Pinned appimagetool — a VERSIONED tag, never the rolling "continuous" build, so this
# SHA stays valid across releases (SHA-pin culture, same as the action pins in CI).
APPIMAGETOOL_VERSION=1.9.1
APPIMAGETOOL_SHA256=ed4ce84f0d9caff66f50bcca6ff6f35aae54ce8135408b3fa33abfc3cb384eb0

REPO_ROOT=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
BIN="${1:-$REPO_ROOT/target/release/gpuviewer}"
ARCH="${ARCH:-x86_64}"
OUT="${OUT:-$REPO_ROOT/dist}"
VERSION="${VERSION:-$(cargo pkgid -p gpuviewer-tui --manifest-path "$REPO_ROOT/Cargo.toml" | sed 's/.*[@#]//')}"

ICON_PNG="$REPO_ROOT/docs/assets/icon/png/256x256/gpuviewer.png"
ICON_SVG="$REPO_ROOT/docs/assets/icon/gpuviewer.svg"

[ -x "$BIN" ] || { echo "error: release binary not found/executable: $BIN" >&2; echo "build it first: cargo build --release --locked -p gpuviewer-tui" >&2; exit 1; }
[ -f "$ICON_PNG" ] || { echo "error: icon missing: $ICON_PNG" >&2; exit 1; }

work=$(mktemp -d)
trap 'rm -rf "$work"' EXIT
appdir="$work/gpuviewer.AppDir"

# --- Assemble the AppDir -----------------------------------------------------------------
mkdir -p \
  "$appdir/usr/bin" \
  "$appdir/usr/share/applications" \
  "$appdir/usr/share/icons/hicolor/256x256/apps" \
  "$appdir/usr/share/icons/hicolor/scalable/apps"

install -m 0755 "$BIN" "$appdir/usr/bin/gpuviewer"

# AppRun: the entry point. readlink -f resolves the mounted AppImage path so the binary is
# found whether the AppImage is run directly (FUSE) or via --appimage-extract-and-run.
cat > "$appdir/AppRun" <<'APPRUN'
#!/bin/sh
HERE=$(dirname "$(readlink -f "$0")")
exec "$HERE/usr/bin/gpuviewer" "$@"
APPRUN
chmod 0755 "$appdir/AppRun"

# Desktop entry: a CLEAN copy (no leading comments) so appimagetool's desktop-file
# validation is happy. Terminal=true — gpuviewer is a TUI, launchers open it in a terminal.
# Icon= is the basename of the icon file at the AppDir root (AppImage convention).
desktop_body='[Desktop Entry]
Type=Application
Name=gpuviewer
Comment=GPU flight recorder
Exec=gpuviewer
Icon=gpuviewer
Terminal=true
Categories=System;Monitor;'
printf '%s\n' "$desktop_body" > "$appdir/gpuviewer.desktop"
printf '%s\n' "$desktop_body" > "$appdir/usr/share/applications/gpuviewer.desktop"

# Icon at AppDir root (the file appimagetool turns into .DirIcon) + hicolor copies.
cp "$ICON_PNG" "$appdir/gpuviewer.png"
cp "$ICON_PNG" "$appdir/usr/share/icons/hicolor/256x256/apps/gpuviewer.png"
[ -f "$ICON_SVG" ] && cp "$ICON_SVG" "$appdir/usr/share/icons/hicolor/scalable/apps/gpuviewer.svg"

# --- Obtain appimagetool (SHA-verified) --------------------------------------------------
if [ -n "${APPIMAGETOOL:-}" ]; then
  tool="$APPIMAGETOOL"
else
  tool="$work/appimagetool.AppImage"
  url="https://github.com/AppImage/appimagetool/releases/download/${APPIMAGETOOL_VERSION}/appimagetool-${ARCH}.AppImage"
  echo "downloading appimagetool ${APPIMAGETOOL_VERSION} ($ARCH)..."
  curl -fsSL "$url" -o "$tool"
  got=$(sha256sum "$tool" | awk '{print $1}')
  if [ "$ARCH" = x86_64 ] && [ "$got" != "$APPIMAGETOOL_SHA256" ]; then
    echo "error: appimagetool SHA256 mismatch" >&2
    echo "  expected $APPIMAGETOOL_SHA256" >&2
    echo "  got      $got" >&2
    exit 1
  fi
  chmod +x "$tool"
fi

# --- Build the AppImage ------------------------------------------------------------------
mkdir -p "$OUT"
output="$OUT/gpuviewer-${VERSION}-${ARCH}.AppImage"
# --appimage-extract-and-run: appimagetool is itself an AppImage; this avoids needing FUSE
# on the build host (GitHub runners have no FUSE). ARCH must be exported for the embedded
# runtime selection. --no-appstream: we ship no AppStream metainfo (a CLI tool), and the
# appstreamcli validator is not guaranteed present on the runner.
echo "packaging $output ..."
ARCH="$ARCH" "$tool" --appimage-extract-and-run --no-appstream "$appdir" "$output"
chmod +x "$output"

echo "built: $output ($(stat -c%s "$output" 2>/dev/null || stat -f%z "$output") bytes)"
