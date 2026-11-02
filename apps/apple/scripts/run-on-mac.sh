#!/bin/bash
# Builds the Mac app (Release, signed for this Mac by Xcode's automatic
# signing), puts it in /Applications and opens it. The first run registers
# this Mac in the Apple developer account: push needs a provisioning profile,
# and a development profile lists the Macs it runs on.
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="Cloud Desktop"
cd "$HERE"
xcodegen generate --quiet
xcodebuild -project CloudDesktop.xcodeproj -scheme CloudDesktopMac -configuration Release \
  -destination 'platform=macOS,arch=arm64' -derivedDataPath build/mac \
  -allowProvisioningUpdates -allowProvisioningDeviceRegistration build -quiet
osascript -e "if application \"$APP_NAME\" is running then tell application \"$APP_NAME\" to quit" >/dev/null 2>&1 || true
sleep 1
rm -rf "/Applications/$APP_NAME.app"
ditto "build/mac/Build/Products/Release/$APP_NAME.app" "/Applications/$APP_NAME.app"
open "/Applications/$APP_NAME.app"
echo "Installed /Applications/$APP_NAME.app"
