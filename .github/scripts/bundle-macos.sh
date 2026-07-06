#!/bin/bash
# Usage: bundle-macos.sh <target-triple> <arch-label>
# Package the release binary into dist/tty7.app and wrap it in a
# drag-to-Applications DMG: dist/tty7-<version>-macos-<arch>.dmg.
#
# Signing posture is chosen from the environment:
#   * Developer ID secrets present (APPLE_SIGNING_IDENTITY + APPLE_CERTIFICATE)
#     -> hardened-runtime signature, then notarize + staple. Passes Gatekeeper.
#   * Otherwise -> adhoc signature, same as before. Fine for local dev, but the
#     OS will quarantine it on other machines.
set -euo pipefail

TARGET="$1"
ARCH="$2"
VERSION="$(grep -m1 '^version' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
APP="dist/tty7.app"

rm -rf dist
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/${TARGET}/release/tty7" "$APP/Contents/MacOS/tty7"
chmod +x "$APP/Contents/MacOS/tty7"
cp assets/tty7.icns "$APP/Contents/Resources/tty7.icns"
# Completion signatures are loaded at runtime (not embedded), resolved relative
# to the executable as ../Resources/completions — see terminal::signature.
mkdir -p "$APP/Contents/Resources/completions"
cp assets/completions/*.json "$APP/Contents/Resources/completions/"
printf 'APPL????' > "$APP/Contents/PkgInfo"

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>tty7</string>
    <key>CFBundleDisplayName</key><string>tty7</string>
    <key>CFBundleIdentifier</key><string>com.github.tty7</string>
    <key>CFBundleVersion</key><string>${VERSION}</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundleExecutable</key><string>tty7</string>
    <key>CFBundleIconFile</key><string>tty7</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
</dict>
</plist>
PLIST

SIGN_ID="${APPLE_SIGNING_IDENTITY:-}"

if [[ -n "$SIGN_ID" && -n "${APPLE_CERTIFICATE:-}" ]]; then
    # ---- Developer ID signing ------------------------------------------------
    # Import the cert into a throwaway keychain so we never touch the login one.
    KEYCHAIN="${RUNNER_TEMP:-/tmp}/tty7-sign.keychain-db"
    CERT_PATH="${RUNNER_TEMP:-/tmp}/tty7-cert.p12"
    KEYCHAIN_PASSWORD="${KEYCHAIN_PASSWORD:-tty7-ci}"
    # Scrub the decoded cert + temp keychain on any exit path.
    cleanup() {
        security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
        rm -f "$CERT_PATH"
    }
    trap cleanup EXIT

    security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
    security set-keychain-settings -lut 21600 "$KEYCHAIN"
    security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
    echo "$APPLE_CERTIFICATE" | base64 --decode > "$CERT_PATH"
    security import "$CERT_PATH" -P "${APPLE_CERTIFICATE_PASSWORD:-}" \
        -A -t cert -f pkcs12 -k "$KEYCHAIN"
    security set-key-partition-list -S apple-tool:,apple:,codesign: \
        -s -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN" >/dev/null
    security list-keychains -d user -s "$KEYCHAIN" login.keychain

    # Hardened runtime forbids JIT / unsigned executable memory by default; the
    # GPU/Metal path gpui uses needs them, so grant them explicitly or the
    # notarized build crashes on launch.
    ENTITLEMENTS="dist/entitlements.plist"
    cat > "$ENTITLEMENTS" <<'ENT'
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>com.apple.security.cs.allow-jit</key><true/>
    <key>com.apple.security.cs.allow-unsigned-executable-memory</key><true/>
    <key>com.apple.security.cs.disable-library-validation</key><true/>
</dict>
</plist>
ENT

    # Sign inner-out: the executable first, then the bundle.
    codesign --force --options runtime --timestamp --entitlements "$ENTITLEMENTS" \
        --sign "$SIGN_ID" "$APP/Contents/MacOS/tty7"
    codesign --force --options runtime --timestamp --entitlements "$ENTITLEMENTS" \
        --sign "$SIGN_ID" "$APP"
    codesign --verify --strict --verbose=2 "$APP"

    # ---- Notarization --------------------------------------------------------
    if [[ -n "${APPLE_ID:-}" && -n "${APPLE_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]]; then
        # Submit a zip of the .app; on success staple the ticket onto the bundle
        # so it validates offline (the distributed zip below then carries it).
        ditto -c -k --keepParent "$APP" "dist/notarize.zip"
        xcrun notarytool submit "dist/notarize.zip" \
            --apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" \
            --team-id "$APPLE_TEAM_ID" --wait
        xcrun stapler staple "$APP"
        rm -f "dist/notarize.zip"
        echo "✅ signed + notarized + stapled"
    else
        echo "⚠️  signed with Developer ID but notarization secrets missing — skipping notarize"
    fi
else
    echo "⚠️  no Developer ID secrets — adhoc signing (won't pass Gatekeeper on other machines)"
    codesign --force --deep --sign - "$APP"
fi

# Package the (now stapled) bundle as a drag-to-Applications DMG.
DMG="dist/tty7-${VERSION}-macos-${ARCH}.dmg"
STAGE="dist/dmg-stage"
rm -rf "$STAGE"
mkdir "$STAGE"
cp -R "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
hdiutil create -volname "tty7" -srcfolder "$STAGE" -ov -format UDZO "$DMG"
rm -rf "$STAGE"
if [[ -n "$SIGN_ID" && -n "${APPLE_CERTIFICATE:-}" ]]; then
    codesign --force --timestamp --sign "$SIGN_ID" "$DMG"
fi
echo "✅ $DMG"
