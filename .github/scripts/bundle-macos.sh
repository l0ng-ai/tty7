#!/bin/bash
# Usage: bundle-macos.sh <target-triple> <arch-label>
# Package the release binary into dist/tty7.app, then publish both:
#   dist/tty7-<version>-macos-<arch>.zip  (in-app updater)
#   dist/tty7-<version>-macos-<arch>.dmg  (drag-to-Applications install)
#
# Signing posture is chosen from the environment:
#   * Developer ID secrets present (APPLE_SIGNING_IDENTITY + APPLE_CERTIFICATE)
#     -> hardened-runtime signature, then notarize + staple. Passes Gatekeeper.
#   * Otherwise -> adhoc signature, same as before. Fine for local dev, but the
#     OS will quarantine it on other machines.
set -euo pipefail

TARGET="$1"
ARCH="$2"
# Anchored on `= "` because the root manifest's `[package]` section leads with
# `version.workspace = true` — a bare `^version` match grabs that line, finds no
# quotes to substitute, and passes it through as the "version", which then lands
# in CFBundleVersion and the .dmg filename. Guard against a silent recurrence.
VERSION="$(grep -m1 '^version = "' Cargo.toml | sed -E 's/.*"([^"]+)".*/\1/')"
if [[ ! "$VERSION" =~ ^[0-9]+\.[0-9]+\.[0-9]+ ]]; then
  echo "bundle-macos: could not read a version from Cargo.toml (got '$VERSION')" >&2
  exit 1
fi
PACKAGE_UPDATE_ZIP="${TTY7_PACKAGE_UPDATE_ZIP:-1}"
APP="dist/tty7.app"

rm -rf dist
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "target/${TARGET}/release/tty7-app" "$APP/Contents/MacOS/tty7-app"
chmod +x "$APP/Contents/MacOS/tty7-app"
# The CLI rides inside the bundle rather than beside it: a DMG is drag-to-
# Applications, so anything not in the .app never reaches the user's disk. The
# GUI symlinks it onto PATH at launch (see core::cli_install), which is why it
# sits next to tty7-app under MacOS/ — that is the directory the GUI resolves
# relative to its own executable.
cp "target/${TARGET}/release/tty7" "$APP/Contents/MacOS/tty7"
chmod +x "$APP/Contents/MacOS/tty7"
if [[ "$PACKAGE_UPDATE_ZIP" != "0" ]]; then
    # A focused out-of-process updater can replace the bundle after the GUI
    # exits, then relaunch or roll back without teaching the GUI to mutate
    # itself. Every macOS build carries it beside the app/CLI so its signature
    # is covered by the outer bundle — including Nightly, whose users are
    # offered the stable release that supersedes their prerelease and need a
    # working helper to get there.
    cp "target/${TARGET}/release/tty7-updater" "$APP/Contents/MacOS/tty7-updater"
    chmod +x "$APP/Contents/MacOS/tty7-updater"
fi
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
    <key>CFBundleExecutable</key><string>tty7-app</string>
    <key>CFBundleIconFile</key><string>tty7</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
    <!-- tty7 is a terminal workbench: panes are forked from the bundled
         executable, so macOS attributes a child process's protected-resource
         requests to tty7.app. Without these usage strings a program you run in
         a pane that asks for camera / microphone / contacts / calendar /
         photos / location / reminders / Apple Events is denied outright with
         no prompt, and cannot even be granted in System Settings. Declaring
         them mirrors what kitty and Kaku ship for exactly this reason: Mac
         TCC reads the responsible bundle's usage string, not the child's. -->
    <key>NSCameraUsageDescription</key>
    <string>A program running inside tty7 would like to access the camera.</string>
    <key>NSMicrophoneUsageDescription</key>
    <string>A program running inside tty7 would like to access the microphone.</string>
    <key>NSContactsUsageDescription</key>
    <string>A program running inside tty7 would like to access your contacts.</string>
    <key>NSCalendarsFullAccessUsageDescription</key>
    <string>A program running inside tty7 would like to access your calendar data.</string>
    <key>NSRemindersFullAccessUsageDescription</key>
    <string>A program running inside tty7 would like to access your reminders.</string>
    <key>NSPhotoLibraryUsageDescription</key>
    <string>A program running inside tty7 would like to access your photo library.</string>
    <key>NSLocationUsageDescription</key>
    <string>A program running inside tty7 would like to access your location information.</string>
    <key>NSMotionUsageDescription</key>
    <string>A program running inside tty7 would like to access motion data.</string>
    <key>NSLocalNetworkUsageDescription</key>
    <string>A program running inside tty7 would like to access the local network.</string>
    <key>NSBluetoothAlwaysUsageDescription</key>
    <string>A program running inside tty7 would like to use Bluetooth.</string>
    <key>NSSpeechRecognitionUsageDescription</key>
    <string>A program running inside tty7 would like to use speech recognition.</string>
    <key>NSSystemAdministrationUsageDescription</key>
    <string>A program running inside tty7 requires elevated privileges.</string>
    <key>NSAppleEventsUsageDescription</key>
    <string>A program running inside tty7 would like to control other applications via Apple Events.</string>
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
    <!-- Deliberately nothing beyond those three, and in particular no TCC
         entitlement to match the usage strings in Info.plist. Those strings
         are about a *child* process's request: macOS attributes it to tty7.app
         as the responsible process and reads the wording from its bundle. The
         hardened-runtime entitlement, by contrast, is checked against the
         process actually sending the request — the child, carrying its own
         signature, since entitlements are per-executable and never inherited.
         So camera / microphone / location / apple-events on tty7.app would do
         nothing for a pane, while widening what injected code could reach
         under tty7's identity; this bundle already carries
         disable-library-validation. Same reasoning the comments below use to
         keep the GUI's entitlements off the CLI. -->
</dict>
</plist>
ENT

    # Sign inner-out: the executables first, then the bundle. The CLI must be
    # signed explicitly — notarization rejects a bundle carrying an unsigned
    # Mach-O, and the outer `codesign "$APP"` does not descend into MacOS/ for
    # anything but CFBundleExecutable.
    #
    # It gets hardened runtime (notarization requires it) but none of the GUI's
    # entitlements: the JIT and library-validation exemptions exist for gpui's
    # Metal path, and a CLI that never renders anything has no business holding
    # them.
    codesign --force --options runtime --timestamp \
        --sign "$SIGN_ID" "$APP/Contents/MacOS/tty7"
    if [[ "$PACKAGE_UPDATE_ZIP" != "0" ]]; then
        codesign --force --options runtime --timestamp \
            --sign "$SIGN_ID" "$APP/Contents/MacOS/tty7-updater"
    fi
    codesign --force --options runtime --timestamp --entitlements "$ENTITLEMENTS" \
        --sign "$SIGN_ID" "$APP/Contents/MacOS/tty7-app"
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

# The in-app updater needs the signed, notarized .app itself rather than a disk
# image that requires Finder interaction. The helper re-reads the full embedded
# version out of the staged bundle and refuses anything that is not the release
# it was told to install.
ZIP=""
if [[ "$PACKAGE_UPDATE_ZIP" != "0" ]]; then
    ZIP="dist/tty7-${VERSION}-macos-${ARCH}.zip"
    ditto -c -k --keepParent "$APP" "$ZIP"
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
if [[ -n "$ZIP" ]]; then
    echo "✅ $ZIP"
fi
echo "✅ $DMG"
