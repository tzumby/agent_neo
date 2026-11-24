#!/bin/bash
# Builds the Mac app (Release, signed for this Mac by Xcode's automatic
# signing), puts it in /Applications and opens it (make app-mac).
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="Agent Neo"
# The app's name until November 2026: a copy installed under it is replaced.
OLD_APP_NAME="Neo Agent"
LSREGISTER=/System/Library/Frameworks/CoreServices.framework/Frameworks/LaunchServices.framework/Support/lsregister
"$HERE/scripts/project.sh"
cd "$HERE"
xcodebuild -project NeoAgent.xcodeproj -scheme NeoAgentMac -configuration Release \
  -destination 'platform=macOS,arch=arm64' -derivedDataPath build/mac \
  -allowProvisioningUpdates -allowProvisioningDeviceRegistration build -quiet
BUILT="build/mac/Build/Products/Release/$APP_NAME.app"
# Quit by bundle ID, which a copy under the old name shares.
BUNDLE_ID="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$BUILT/Contents/Info.plist")"
osascript -e "if application id \"$BUNDLE_ID\" is running then tell application id \"$BUNDLE_ID\" to quit" >/dev/null 2>&1 || true
sleep 1
if [[ -d "/Applications/$OLD_APP_NAME.app" ]]; then
  "$LSREGISTER" -u "/Applications/$OLD_APP_NAME.app" || true
  rm -rf "/Applications/$OLD_APP_NAME.app"
fi
rm -rf "/Applications/$APP_NAME.app"
ditto "$BUILT" "/Applications/$APP_NAME.app"
# macOS caches app icons by bundle path and date, and an incremental build
# keeps the bundle's date (ditto copies it), so the Dock kept showing the old
# icon after the icon changed. A new date and registering again refresh it.
touch "/Applications/$APP_NAME.app"
"$LSREGISTER" -f "/Applications/$APP_NAME.app"
open "/Applications/$APP_NAME.app"
echo "Installed /Applications/$APP_NAME.app"
