#!/bin/bash
# Builds the iOS app and uploads it to App Store Connect for TestFlight.
#
# Usage: scripts/testflight.sh [--build-number N] [--no-upload]
#   --build-number N  the build is <version>.N (default: the time, yyjjjHHMM)
#   --no-upload       export a signed .ipa into build/testflight/ instead
#
# Needs Xcode signed in (Settings → Accounts) to an account on team 78SZC3DQ7B.
#
# Why not just `tauri ios build --export-method app-store-connect`:
# - The archive is signed with a development profile first, and the team has
#   no registered devices to make one from. So the archive is built unsigned
#   and Xcode signs it for the App Store at export, where no device is needed.
#
# The default build number is the time — year, day of the year, hour, minute —
# so each upload is higher than the last without keeping a counter, and it
# stays under 2^31.
set -euo pipefail

cd "$(dirname "$0")/.."

UPLOAD=1
N=$(date +%y%j%H%M)
while [ $# -gt 0 ]; do
  case "$1" in
    --no-upload) UPLOAD=0 ;;
    --build-number) N="${2:?--build-number needs a number}"; shift ;;
    *) echo "usage: $0 [--build-number N] [--no-upload]" >&2; exit 2 ;;
  esac
  shift
done

TEAM=78SZC3DQ7B
PROJECT=src-tauri/gen/apple/tty7-mobile.xcodeproj/project.pbxproj
ARCHIVE=src-tauri/gen/apple/build/tty7-mobile_iOS.xcarchive
OUT=build/testflight
mkdir -p "$OUT"

[ -f "$PROJECT" ] || npm run tauri ios init

# `tauri ios init` fills the asset catalog with Tauri's placeholder icon;
# put ours back so a regenerated gen/ never ships it.
cp src-tauri/icons/ios/*.png src-tauri/gen/apple/Assets.xcassets/AppIcon.appiconset/

# Release signing off for the archive only; the project is put back however
# this exits.
cp "$PROJECT" "$OUT/project.pbxproj.orig"
trap 'cp "$OUT/project.pbxproj.orig" "$PROJECT"' EXIT
perl -0pi -e 's{(/\* release \*/ = \{\n\t+isa = XCBuildConfiguration;\n\t+buildSettings = \{\n)(?=(?:(?!\n\t+\};).)*?CODE_SIGN_ENTITLEMENTS)}{$1\t\t\t\tCODE_SIGNING_ALLOWED = NO;\n}sg' "$PROJECT"
if [ "$(grep -c 'CODE_SIGNING_ALLOWED = NO' "$PROJECT")" != 1 ]; then
  echo "could not find the app target's release configuration in $PROJECT" >&2
  exit 1
fi

echo "==> Archiving build number $N"
rm -rf "$ARCHIVE"
npx tauri ios build --archive-only --build-number "$N"

APP=$(ls -d "$ARCHIVE"/Products/Applications/*.app)
VERSION=$(/usr/libexec/PlistBuddy -c "Print :CFBundleShortVersionString" "$APP/Info.plist")
BUILD=$(/usr/libexec/PlistBuddy -c "Print :CFBundleVersion" "$APP/Info.plist")

DESTINATION=$([ "$UPLOAD" = 1 ] && echo upload || echo export)
cat > "$OUT/ExportOptions.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>method</key><string>app-store-connect</string>
  <key>destination</key><string>$DESTINATION</string>
  <key>teamID</key><string>$TEAM</string>
  <key>signingStyle</key><string>automatic</string>
  <key>manageAppVersionAndBuildNumber</key><false/>
</dict>
</plist>
EOF

echo "==> Signing$([ "$UPLOAD" = 1 ] && echo " and uploading") $VERSION ($BUILD)"
xcodebuild -exportArchive \
  -archivePath "$ARCHIVE" \
  -exportOptionsPlist "$OUT/ExportOptions.plist" \
  -exportPath "$OUT" \
  -allowProvisioningUpdates

if [ "$UPLOAD" = 1 ]; then
  echo "==> Uploaded $VERSION ($BUILD); it shows in TestFlight once App Store Connect has processed it"
else
  echo "==> Signed .ipa in $OUT"
fi
