#!/bin/bash
# Builds the Mac app (Release, signed for this Mac by Xcode's automatic
# signing), puts it in /Applications and opens it (make app-mac).
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
APP_NAME="Neo Agent"
"$HERE/scripts/project.sh"
cd "$HERE"
xcodebuild -project NeoAgent.xcodeproj -scheme NeoAgentMac -configuration Release \
  -destination 'platform=macOS,arch=arm64' -derivedDataPath build/mac \
  -allowProvisioningUpdates -allowProvisioningDeviceRegistration build -quiet
osascript -e "if application \"$APP_NAME\" is running then tell application \"$APP_NAME\" to quit" >/dev/null 2>&1 || true
sleep 1
rm -rf "/Applications/$APP_NAME.app"
ditto "build/mac/Build/Products/Release/$APP_NAME.app" "/Applications/$APP_NAME.app"
open "/Applications/$APP_NAME.app"
echo "Installed /Applications/$APP_NAME.app"
