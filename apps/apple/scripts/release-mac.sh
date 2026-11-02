#!/bin/bash
# The Mac app for someone else's Mac: signed with your Developer ID,
# notarized by Apple and stapled, in a DMG (make app-release-mac).
# Its push token is for Apple's production servers (APS_ENVIRONMENT=production).
#
# Once, before the first run (docs/onboarding.md §2):
# - a "Developer ID Application" certificate in your keychain;
# - notarytool credentials in your keychain, named $NOTARY_PROFILE:
#     xcrun notarytool store-credentials cloud-desktop-notary \
#       --apple-id <your Apple ID> --team-id 4UVAPZYV4Q
#   (it asks for an app-specific password from account.apple.com).
#
# BUILD=<number> sets the build number (default: the UTC date and time).
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"
TEAM_ID=4UVAPZYV4Q
APP_NAME="Cloud Desktop"
NOTARY_PROFILE="${NOTARY_PROFILE:-cloud-desktop-notary}"
VERSION="$(awk -F'"' '/MARKETING_VERSION:/ {print $2; exit}' project.yml)"
BUILD="${BUILD:-$(date -u +%Y%m%d.%H%M)}"
OUT="build/release/mac-$VERSION-$BUILD"

IDENTITY="$(security find-identity -v -p codesigning | awk -F'"' -v team="($TEAM_ID)" '/Developer ID Application/ && index($2, team) {print $2; exit}')"
if [[ -z "$IDENTITY" ]]; then
  echo "No \"Developer ID Application\" certificate for team $TEAM_ID in your keychain." >&2
  echo "Xcode → Settings → Accounts → your team → Manage Certificates → + → Developer ID Application." >&2
  exit 1
fi
if ! xcrun notarytool history --keychain-profile "$NOTARY_PROFILE" >/dev/null 2>&1; then
  echo "No notarytool credentials named $NOTARY_PROFILE in your keychain. Once:" >&2
  echo "  xcrun notarytool store-credentials $NOTARY_PROFILE --apple-id <your Apple ID> --team-id $TEAM_ID" >&2
  exit 1
fi

# Submits a file to Apple's notary service and waits; on failure, prints its log.
notarize() {
  local file="$1" result id status
  result="$(xcrun notarytool submit "$file" --keychain-profile "$NOTARY_PROFILE" --wait --output-format json)"
  id="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["id"])' "$result")"
  status="$(python3 -c 'import json,sys; print(json.loads(sys.argv[1])["status"])' "$result")"
  if [[ "$status" != Accepted ]]; then
    echo "Notarization of $(basename "$file"): $status. Apple's log:" >&2
    xcrun notarytool log "$id" --keychain-profile "$NOTARY_PROFILE" >&2 || true
    exit 1
  fi
  echo "Notarized $(basename "$file") ($id)."
}

rm -rf "$OUT"
mkdir -p "$OUT"
xcodegen generate --quiet

echo "Archiving $APP_NAME $VERSION ($BUILD)…"
xcodebuild archive -project CloudDesktop.xcodeproj -scheme CloudDesktopMac -configuration Release \
  -destination 'generic/platform=macOS' -archivePath "$OUT/CloudDesktopMac.xcarchive" \
  -allowProvisioningUpdates APS_ENVIRONMENT=production CURRENT_PROJECT_VERSION="$BUILD" \
  ARCHS=arm64 -quiet # for the packages too: TailscaleKit has no Intel slice

echo "Signing it with your Developer ID…"
cat >"$OUT/ExportOptions.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>method</key><string>developer-id</string>
  <key>teamID</key><string>$TEAM_ID</string>
  <key>signingStyle</key><string>automatic</string>
</dict>
</plist>
EOF
xcodebuild -exportArchive -archivePath "$OUT/CloudDesktopMac.xcarchive" \
  -exportOptionsPlist "$OUT/ExportOptions.plist" -exportPath "$OUT/export" -allowProvisioningUpdates -quiet
APP="$OUT/export/$APP_NAME.app"

codesign --verify --deep --strict "$APP"
aps="$(codesign -d --entitlements - --xml "$APP" 2>/dev/null |
  python3 -c 'import plistlib,sys; print(plistlib.loads(sys.stdin.buffer.read()).get("com.apple.developer.aps-environment", ""))')"
if [[ "$aps" != production ]]; then
  echo "The app's push entitlement is '$aps', not production: notifications wouldn't arrive." >&2
  exit 1
fi

echo "Notarizing the app…"
ditto -c -k --keepParent "$APP" "$OUT/notarize.zip"
notarize "$OUT/notarize.zip"
xcrun stapler staple -q "$APP"
spctl --assess --type execute "$APP"

echo "Making the DMG…"
mkdir -p "$OUT/dmg"
ditto "$APP" "$OUT/dmg/$APP_NAME.app"
ln -s /Applications "$OUT/dmg/Applications"
DMG="$OUT/Cloud-Desktop-$VERSION-$BUILD.dmg"
hdiutil create -volname "$APP_NAME" -srcfolder "$OUT/dmg" -ov -format UDZO -quiet "$DMG"
codesign --sign "$IDENTITY" --timestamp "$DMG"
notarize "$DMG"
xcrun stapler staple -q "$DMG"
spctl --assess --type open --context context:primary-signature "$DMG"

rm -rf "$OUT/dmg" "$OUT/notarize.zip"
echo
echo "Ready to send: $HERE/$DMG"
echo "($APP_NAME $VERSION, build $BUILD: Developer ID, notarized and stapled; macOS 15.6+, Apple silicon.)"
