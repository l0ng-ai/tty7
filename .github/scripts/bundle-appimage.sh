#!/bin/bash
# Usage: bundle-appimage.sh <target-triple> <arch-label>
# Package the release binary into a self-contained AppImage:
#   dist/tty7-<version>-linux-<arch>.AppImage
#
# Unlike the bare tarball (bundle-linux.sh), this bundles the x11/wayland/xkb/
# fontconfig/freetype runtime libraries alongside the binary, so it launches on
# distros that don't ship the exact same set Ubuntu does — Fedora, Arch, etc.
# (glibc itself is NOT bundled — an AppImage still needs the host glibc to be
# >= the build machine's, so the release runner's Ubuntu sets the floor.)
#
# Completion signatures are loaded at runtime relative to the executable
# (<exe-dir>/completions — see terminal::signature), so they go beside the
# binary at usr/bin/completions inside the AppDir.
set -euo pipefail

TARGET="$1"
ARCH="$2"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
NAME="tty7-${VERSION}-linux-${ARCH}"

# AppImage tools need FUSE to self-mount; CI runners usually lack it, so extract
# and run instead. Harmless on machines that do have FUSE.
export APPIMAGE_EXTRACT_AND_RUN=1

# linuxdeploy is published per-arch; map Rust's arch label to its naming.
case "$ARCH" in
  x86_64) LD_ARCH=x86_64 ;;
  arm64 | aarch64) LD_ARCH=aarch64 ;;
  *) echo "unsupported arch for AppImage: $ARCH" >&2; exit 1 ;;
esac

TOOLS="$(mktemp -d)"
LINUXDEPLOY="$TOOLS/linuxdeploy-${LD_ARCH}.AppImage"
APPIMAGETOOL="$TOOLS/appimagetool-${LD_ARCH}.AppImage"
curl -fsSL -o "$LINUXDEPLOY" \
  "https://github.com/linuxdeploy/linuxdeploy/releases/download/continuous/linuxdeploy-${LD_ARCH}.AppImage"
curl -fsSL -o "$APPIMAGETOOL" \
  "https://github.com/AppImage/appimagetool/releases/download/continuous/appimagetool-${LD_ARCH}.AppImage"
chmod +x "$LINUXDEPLOY" "$APPIMAGETOOL"

# NB: don't wipe dist/ — the tarball step (bundle-linux.sh) runs first and its
# artifact must survive. Only clean our own AppDir.
APPDIR="dist/AppDir"
rm -rf "$APPDIR"
mkdir -p "$APPDIR/usr/bin"

cp "target/${TARGET}/release/tty7" "$APPDIR/usr/bin/tty7"
chmod +x "$APPDIR/usr/bin/tty7"

# A desktop entry + icon are mandatory AppImage metadata; linuxdeploy places
# them and generates AppRun. Icon basename must match the desktop's Icon= key.
cat > "$TOOLS/tty7.desktop" <<'DESKTOP'
[Desktop Entry]
Type=Application
Name=tty7
Comment=A fast, native terminal
Exec=tty7
Icon=tty7
Categories=System;TerminalEmulator;
Terminal=false
StartupWMClass=tty7
DESKTOP
# linuxdeploy only accepts fixed icon resolutions (…256, 384, 512 — NOT the
# source's 1024), so downscale to 256×256.
convert assets/app-icon.png -resize 256x256 "$TOOLS/tty7.png"

# Phase 1 — populate the AppDir: copy in dependent libs (ldd + patchelf) and
# install the desktop/icon into their standard locations.
"$LINUXDEPLOY" \
  --appdir "$APPDIR" \
  --executable "$APPDIR/usr/bin/tty7" \
  --desktop-file "$TOOLS/tty7.desktop" \
  --icon-file "$TOOLS/tty7.png"

# Runtime-loaded completion specs live beside the binary (not bundled by
# linuxdeploy, which only tracks ELF deps), so drop them in after populate.
mkdir -p "$APPDIR/usr/bin/completions"
cp assets/completions/*.json "$APPDIR/usr/bin/completions/"

# Phase 2 — pack the finished AppDir. Done separately from linuxdeploy so the
# completions added above are included.
"$APPIMAGETOOL" "$APPDIR" "dist/${NAME}.AppImage"
chmod +x "dist/${NAME}.AppImage"
rm -rf "$APPDIR" "$TOOLS"
echo "✅ dist/${NAME}.AppImage"
