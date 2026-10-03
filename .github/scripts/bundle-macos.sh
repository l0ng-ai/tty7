#!/bin/bash
# Usage: bundle-macos.sh <target-triple> <arch-label>
# Package the release binary into dist/tty7.app, then publish both:
#   dist/tty7-<version>-macos-<arch>.zip  (in-app updater)
#   dist/tty7-<version>-macos-<arch>.dmg  (drag-to-Applications install)
#
# Signing posture is chosen from the environment:
#   * Developer ID secrets present (APPLE_SIGNING_IDENTITY + APPLE_CERTIFICATE)
#     -> hardened-runtime signature, then notarize + staple. Passes Gatekeeper.
#     Notarization uses ASC_KEY_P8/ASC_KEY_ID/ASC_ISSUER_ID if set, else
#     APPLE_ID/APPLE_PASSWORD/APPLE_TEAM_ID.
#   * APPLE_SIGNING_IDENTITY alone -> that identity from the login keychain,
#     without a secure timestamp unless notarization credentials are set.
#   * Otherwise -> adhoc signature, same as before. Fine for local dev, but the
#     OS will quarantine it on other machines.
#
# Before either artifact is packaged, every Mach-O inside the bundle is
# asserted to be a thin <arch-label> binary (the sweep below), so a wrong-arch
# or universal file fails the build here instead of shipping.
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
# A rebranded or local build sets its own name and id, binary dir and output
# dir. TTY7_BUNDLE_ONLY stops after the .app, before the update zip and DMG.
APP_NAME="${TTY7_APP_NAME:-tty7}"
BUNDLE_ID="${TTY7_BUNDLE_ID:-com.github.tty7}"
BIN_DIR="${TTY7_BIN_DIR:-target/${TARGET}/release}"
DIST="${TTY7_DIST:-dist}"
APP="$DIST/$APP_NAME.app"

rm -rf "$DIST"
mkdir -p "$APP/Contents/MacOS" "$APP/Contents/Resources"
cp "$BIN_DIR/tty7-app" "$APP/Contents/MacOS/tty7-app"
chmod +x "$APP/Contents/MacOS/tty7-app"
# The CLI rides inside the bundle rather than beside it: a DMG is drag-to-
# Applications, so anything not in the .app never reaches the user's disk. The
# GUI symlinks it onto PATH at launch (see core::cli_install), which is why it
# sits next to tty7-app under MacOS/ — that is the directory the GUI resolves
# relative to its own executable.
cp "$BIN_DIR/tty7" "$APP/Contents/MacOS/tty7"
chmod +x "$APP/Contents/MacOS/tty7"
if [[ "$PACKAGE_UPDATE_ZIP" != "0" ]]; then
    # A focused out-of-process updater can replace the bundle after the GUI
    # exits, then relaunch or roll back without teaching the GUI to mutate
    # itself. Every macOS build carries it beside the app/CLI so its signature
    # is covered by the outer bundle — including Nightly, whose users are
    # offered the stable release that supersedes their prerelease and need a
    # working helper to get there.
    cp "$BIN_DIR/tty7-updater" "$APP/Contents/MacOS/tty7-updater"
    chmod +x "$APP/Contents/MacOS/tty7-updater"
fi
cp assets/tty7.icns "$APP/Contents/Resources/tty7.icns"
# Completion signatures are loaded at runtime (not embedded), resolved relative
# to the executable as ../Resources/completions — see terminal::signature.
mkdir -p "$APP/Contents/Resources/completions"
cp assets/completions/*.json "$APP/Contents/Resources/completions/"
printf 'APPL????' > "$APP/Contents/PkgInfo"
if [[ -n "${TTY7_LOCAL_BUILD_ID:-}" ]]; then
    # A local install's build id, which a running app can watch to offer a restart.
    printf '%s\n' "$TTY7_LOCAL_BUILD_ID" > "$APP/Contents/Resources/local-build-id"
fi

cat > "$APP/Contents/Info.plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>CFBundleName</key><string>${APP_NAME}</string>
    <key>CFBundleDisplayName</key><string>${APP_NAME}</string>
    <key>CFBundleIdentifier</key><string>${BUNDLE_ID}</string>
    <key>CFBundleVersion</key><string>${VERSION}</string>
    <key>CFBundleShortVersionString</key><string>${VERSION}</string>
    <key>CFBundleExecutable</key><string>tty7-app</string>
    <key>CFBundleIconFile</key><string>tty7</string>
    <key>CFBundlePackageType</key><string>APPL</string>
    <key>NSHighResolutionCapable</key><true/>
    <key>NSPrincipalClass</key><string>NSApplication</string>
    <!-- LaunchServices has no global default-terminal setting. These declare
         the specific document types tty7 can open, and Settings lets users
         select tty7 as their handler for them. -->
    <key>CFBundleDocumentTypes</key>
    <array>
        <dict>
            <key>CFBundleTypeName</key><string>Folder</string>
            <key>CFBundleTypeRole</key><string>Editor</string>
            <key>LSHandlerRank</key><string>Alternate</string>
            <key>LSItemContentTypes</key><array><string>public.directory</string></array>
        </dict>
        <dict>
            <key>CFBundleTypeName</key><string>Shell Script</string>
            <key>CFBundleTypeRole</key><string>Shell</string>
            <key>LSItemContentTypes</key><array><string>public.shell-script</string></array>
        </dict>
        <dict>
            <key>CFBundleTypeName</key><string>Unix Executable</string>
            <key>CFBundleTypeRole</key><string>Shell</string>
            <key>LSItemContentTypes</key><array><string>public.unix-executable</string></array>
        </dict>
    </array>
    <key>CFBundleURLTypes</key>
    <array>
        <dict>
            <key>CFBundleURLName</key><string>SSH</string>
            <key>CFBundleURLSchemes</key><array><string>ssh</string></array>
        </dict>
        <dict>
            <key>CFBundleURLName</key><string>Man Page</string>
            <key>CFBundleURLSchemes</key><array><string>x-man-page</string></array>
        </dict>
    </array>
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

# Notarization credentials: an App Store Connect API key, else an Apple ID
# password. Decided before signing, because notarization rejects a signature
# that carries no secure timestamp.
NOTARY_WITH=""
if [[ -n "${ASC_KEY_P8:-}" && -n "${ASC_KEY_ID:-}" && -n "${ASC_ISSUER_ID:-}" ]]; then
    NOTARY_WITH=asc
elif [[ -n "${APPLE_ID:-}" && -n "${APPLE_PASSWORD:-}" && -n "${APPLE_TEAM_ID:-}" ]]; then
    NOTARY_WITH=apple-id
fi

if [[ -n "$SIGN_ID" ]]; then
    # With a certificate (CI) it is imported into a throwaway keychain; without
    # one, the identity is looked up in the login keychain (a local build). The
    # secure timestamp is skipped only for a local build that is not notarized.
    TIMESTAMP=--timestamp=none
    if [[ -n "${APPLE_CERTIFICATE:-}" || -n "$NOTARY_WITH" ]]; then
        TIMESTAMP=--timestamp
    fi
    KEYCHAIN=""
    CERT_PATH=""
    ASC_KEY_PATH=""
    # Scrub the decoded cert, temp keychain and API key on any exit path.
    cleanup() {
        if [[ -n "$KEYCHAIN" ]]; then
            security delete-keychain "$KEYCHAIN" >/dev/null 2>&1 || true
        fi
        rm -f "$CERT_PATH" "$ASC_KEY_PATH"
    }
    trap cleanup EXIT

    if [[ -n "${APPLE_CERTIFICATE:-}" ]]; then
        # ---- Developer ID signing --------------------------------------------
        # Import the cert into a throwaway keychain so we never touch the login one.
        KEYCHAIN="${RUNNER_TEMP:-/tmp}/tty7-sign.keychain-db"
        CERT_PATH="${RUNNER_TEMP:-/tmp}/tty7-cert.p12"
        KEYCHAIN_PASSWORD="${KEYCHAIN_PASSWORD:-tty7-ci}"

        security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
        security set-keychain-settings -lut 21600 "$KEYCHAIN"
        security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
        echo "$APPLE_CERTIFICATE" | base64 --decode > "$CERT_PATH"
        security import "$CERT_PATH" -P "${APPLE_CERTIFICATE_PASSWORD:-}" \
            -A -t cert -f pkcs12 -k "$KEYCHAIN"
        security set-key-partition-list -S apple-tool:,apple:,codesign: \
            -s -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN" >/dev/null
        security list-keychains -d user -s "$KEYCHAIN" login.keychain
    fi

    # Hardened runtime forbids JIT / unsigned executable memory by default; the
    # GPU/Metal path gpui uses needs them, so grant them explicitly or the
    # notarized build crashes on launch.
    ENTITLEMENTS="$DIST/entitlements.plist"
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
    codesign --force --options runtime "$TIMESTAMP" \
        --sign "$SIGN_ID" "$APP/Contents/MacOS/tty7"
    if [[ "$PACKAGE_UPDATE_ZIP" != "0" ]]; then
        codesign --force --options runtime "$TIMESTAMP" \
            --sign "$SIGN_ID" "$APP/Contents/MacOS/tty7-updater"
    fi
    codesign --force --options runtime "$TIMESTAMP" --entitlements "$ENTITLEMENTS" \
        --sign "$SIGN_ID" "$APP/Contents/MacOS/tty7-app"
    codesign --force --options runtime "$TIMESTAMP" --entitlements "$ENTITLEMENTS" \
        --sign "$SIGN_ID" "$APP"
    codesign --verify --strict --verbose=2 "$APP"

    # ---- Notarization --------------------------------------------------------
    NOTARY_AUTH=()
    if [[ "$NOTARY_WITH" == asc ]]; then
        # notarytool reads the key from a file. mktemp creates it 0600, so the
        # key is never readable by others in a shared temp dir.
        ASC_KEY_PATH="$(mktemp "${RUNNER_TEMP:-${TMPDIR:-/tmp}}/tty7-asc-key.XXXXXX")"
        printf '%s\n' "$ASC_KEY_P8" > "$ASC_KEY_PATH"
        NOTARY_AUTH=(--key "$ASC_KEY_PATH" --key-id "$ASC_KEY_ID" --issuer "$ASC_ISSUER_ID")
    elif [[ "$NOTARY_WITH" == apple-id ]]; then
        NOTARY_AUTH=(--apple-id "$APPLE_ID" --password "$APPLE_PASSWORD" --team-id "$APPLE_TEAM_ID")
    fi
    if [[ ${#NOTARY_AUTH[@]} -gt 0 ]]; then
        # Submit a zip of the .app; on success staple the ticket onto the bundle
        # so it validates offline (the distributed zip below then carries it).
        ditto -c -k --keepParent "$APP" "$DIST/notarize.zip"
        xcrun notarytool submit "$DIST/notarize.zip" "${NOTARY_AUTH[@]}" --wait
        xcrun stapler staple "$APP"
        rm -f "$DIST/notarize.zip"
        echo "✅ signed + notarized + stapled"
    else
        echo "⚠️  signed with Developer ID but notarization secrets missing — skipping notarize"
    fi
else
    echo "⚠️  no Developer ID secrets — adhoc signing (won't pass Gatekeeper on other machines)"
    codesign --force --deep --sign - "$APP"
fi

# ---- Architecture sweep ----------------------------------------------------
# Everything the bundle ships has to be the thin slice its filename claims. A
# macOS 26 user read "contains Intel parts" off the Apple Silicon bundle (#687).
# The published bundles turned out clean — every Mach-O in them thin arm64 —
# but nothing here had ever checked: assert-macho.sh only ever pointed at the
# standalone tty7-server asset, so a helper built without --target, a dylib
# dragged in from the runner, or a universal binary would have shipped, and been
# found by a user rather than by this script.
#
# After the signing block, because assert-macho.sh also insists on a code
# signature, and after both postures so one pass covers Developer ID and adhoc
# alike. Before the zip and the DMG, so a bundle that fails here never becomes
# an artifact — and before the `mv` below, after which dist/tty7.app no longer
# exists. For a Developer ID build that puts it after notarization, which
# spends a few minutes of notary time on a bundle that was never going to ship;
# cheap next to carrying a second copy of this block inside each branch.
BUNDLE_FAIL=0
ASSERT_MACHO="$(dirname "$0")/assert-macho.sh"
BUNDLED_BINS=(tty7-app tty7)
if [[ "$PACKAGE_UPDATE_ZIP" != "0" ]]; then
    BUNDLED_BINS+=(tty7-updater)
fi
# First the binaries we staged ourselves, held to the full standard the server
# asset is: the right arch, links nothing macOS does not ship, carries a
# signature. This also leaves every shipped binary's load commands in the
# release log, which is where the next report like #687 gets answered from.
for bin in "${BUNDLED_BINS[@]}"; do
    bash "$ASSERT_MACHO" "$APP/Contents/MacOS/$bin" "$ARCH" || BUNDLE_FAIL=1
done

# Then the whole bundle, for whatever that list did not know to look at: walk
# every file, let `file` say which are Mach-O of any kind — executable, dylib,
# bundle — and have `lipo` name the slices in each. The answer has to be
# exactly "$ARCH". Any other name is the wrong build; two names is a universal
# binary, which is what the report described and what nothing in this pipeline
# should ever produce.
#
# `file` detects and `lipo -archs` judges, rather than reading the arch out of
# `file`'s prose: Apple's build says "64-bit executable arm64" where upstream
# libmagic says "64-bit arm64 executable, flags:<...>", and a parser written
# against one misreads the other. lipo's slice names are the same on every
# macOS, and it is the tool that would have made a fat binary in the first
# place. Captured into variables, never piped into `grep -q` — see
# assert-macho.sh for the pipefail race. Process substitution rather than
# `find | while`, so the counters survive the loop.
echo "--- Mach-O sweep of $APP, expecting ${ARCH} ---"
SWEEP_SEEN=0
while IFS= read -r -d '' f; do
    KIND="$(file -b "$f")"
    [[ "$KIND" == *"Mach-O"* ]] || continue
    SWEEP_SEEN=$((SWEEP_SEEN + 1))
    # Multi-line for a universal file (one line per slice); the first line is
    # the verdict.
    KIND="${KIND%%$'\n'*}"
    if ! ARCHS="$(lipo -archs "$f" 2>&1)"; then
        echo "::error::lipo could not read $f ($KIND): $ARCHS"
        BUNDLE_FAIL=1
        continue
    fi
    case "$ARCHS" in
        "$ARCH")
            echo "${ARCHS}  $f  ($KIND)" ;;
        *" "*)
            echo "::error::$f is a universal binary carrying [${ARCHS}]; this bundle ships ${ARCH} only"
            BUNDLE_FAIL=1 ;;
        *)
            echo "::error::$f is ${ARCHS}, not ${ARCH} ($KIND)"
            BUNDLE_FAIL=1 ;;
    esac
done < <(find "$APP" -type f -print0)
# A sweep that sees fewer Mach-Os than the binaries copied in above is not
# looking at the bundle — a changed `file` wording, an empty find — and must not
# pass as "nothing wrong found".
if (( SWEEP_SEEN < ${#BUNDLED_BINS[@]} )); then
    echo "::error::the sweep found ${SWEEP_SEEN} Mach-O file(s) in $APP, fewer than the ${#BUNDLED_BINS[@]} staged above — it is not seeing the bundle"
    BUNDLE_FAIL=1
fi
if [[ "$BUNDLE_FAIL" -ne 0 ]]; then
    exit 1
fi
echo "✅ every Mach-O in $APP is a thin ${ARCH} binary (${SWEEP_SEEN} checked)"
if [[ "${TTY7_BUNDLE_ONLY:-0}" != "0" ]]; then
    exit 0
fi

# The in-app updater needs the signed, notarized .app itself rather than a disk
# image that requires Finder interaction. The helper re-reads the full embedded
# version out of the staged bundle and refuses anything that is not the release
# it was told to install.
ZIP=""
if [[ "$PACKAGE_UPDATE_ZIP" != "0" ]]; then
    ZIP="$DIST/tty7-${VERSION}-macos-${ARCH}.zip"
    ditto -c -k --keepParent "$APP" "$ZIP"
fi

# Package the (now stapled) bundle as a drag-to-Applications DMG.
DMG="$DIST/tty7-${VERSION}-macos-${ARCH}.dmg"
STAGE="$DIST/dmg-stage"
rm -rf "$STAGE"
mkdir "$STAGE"
# `mv`, not `cp -R`: this is the peak, and a second full copy of the bundle is
# the most expensive thing on the volume that nobody needs. Nothing reads
# dist/tty7.app after this point — the zip above is what the updater ships and
# what nightly.yml verifies (it extracts that, not this), and release.yml only
# knows about tty7.app as an intermediate to keep out of the upload globs.
mv "$APP" "$STAGE/"
ln -s /Applications "$STAGE/Applications"
# Size the image explicitly. Left to itself, `-srcfolder` measures the bytes it
# is about to copy and asks for about that much, which does not cover what the
# filesystem spends carrying them — so the copy runs the *volume* out of room
# partway through and hdiutil reports "No space left on device". The path in
# that message is under /Volumes/tty7, not on the host: three nightlies died
# here on 2026-08-10 with 105 GiB free on the runner. It is a threshold, not a
# cliff — the x86_64 binaries are the larger pair and crossed it first, while
# arm64 went on building fine just underneath.
#
# Doubling the content and adding 64 MiB is far more slack than the shortfall
# needs, and it is close to free: the image is compressed on the way out, so
# measured against a stage of this shape, 127 MiB of empty volume cost 672 KiB
# in the published DMG.
STAGE_KB="$(du -sk "$STAGE" | awk '{print $1}')"
hdiutil create -volname "$APP_NAME" -srcfolder "$STAGE" -ov -format UDZO \
    -size "$(( STAGE_KB * 2 + 65536 ))k" "$DMG"
rm -rf "$STAGE"
if [[ -n "$SIGN_ID" && -n "${APPLE_CERTIFICATE:-}" ]]; then
    codesign --force --timestamp --sign "$SIGN_ID" "$DMG"
fi
if [[ -n "$ZIP" ]]; then
    echo "✅ $ZIP"
fi
echo "✅ $DMG"
