#!/bin/bash
# The iPhone app for TestFlight: archived, signed for App Store distribution
# and uploaded to App Store Connect (make app-testflight). Its push token is
# for Apple's production servers (APS_ENVIRONMENT=production).
#
# Once, before the first run (docs/onboarding.md §2): the app in App Store
# Connect (bundle ID com.tzumby.clouddesktop), and your Apple ID signed in to
# Xcode (Settings → Accounts) with access to the team. Or an App Store Connect
# API key: ASC_KEY_PATH, ASC_KEY_ID and ASC_ISSUER_ID.
#
# BUILD=<number> sets the build number (default: the UTC date and time); each
# upload needs a new one.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"
TEAM_ID=4UVAPZYV4Q
VERSION="$(awk -F'"' '/MARKETING_VERSION:/ {print $2; exit}' project.yml)"
BUILD="${BUILD:-$(date -u +%Y%m%d.%H%M)}"
OUT="build/release/ios-$VERSION-$BUILD"

AUTH=()
if [[ -n "${ASC_KEY_PATH:-}" ]]; then
  AUTH=(-authenticationKeyPath "$ASC_KEY_PATH" -authenticationKeyID "$ASC_KEY_ID" -authenticationKeyIssuerID "$ASC_ISSUER_ID")
fi

rm -rf "$OUT"
mkdir -p "$OUT"
xcodegen generate --quiet

echo "Archiving Cloud Desktop $VERSION ($BUILD) for iPhone…"
xcodebuild archive -project CloudDesktop.xcodeproj -scheme CloudDesktop -configuration Release \
  -destination 'generic/platform=iOS' -archivePath "$OUT/CloudDesktop.xcarchive" \
  -allowProvisioningUpdates ${AUTH[@]+"${AUTH[@]}"} APS_ENVIRONMENT=production CURRENT_PROJECT_VERSION="$BUILD" -quiet

echo "Uploading it to App Store Connect…"
cat >"$OUT/ExportOptions.plist" <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>method</key><string>app-store-connect</string>
  <key>destination</key><string>upload</string>
  <key>teamID</key><string>$TEAM_ID</string>
  <key>signingStyle</key><string>automatic</string>
  <key>uploadSymbols</key><true/>
  <key>manageAppVersionAndBuildNumber</key><false/>
  <key>testFlightInternalTestingOnly</key><false/>
</dict>
</plist>
EOF
xcodebuild -exportArchive -archivePath "$OUT/CloudDesktop.xcarchive" \
  -exportOptionsPlist "$OUT/ExportOptions.plist" -exportPath "$OUT/export" \
  -allowProvisioningUpdates ${AUTH[@]+"${AUTH[@]}"} -quiet

echo
echo "Uploaded Cloud Desktop $VERSION, build $BUILD."
echo "App Store Connect processes it (about 5-15 minutes, then an email), then it's in TestFlight:"
echo "answer the encryption question once per build if asked, and add it to your testers' group."
