#!/bin/bash
# Builds the iPhone app, installs it on the first iPhone connected to this Mac
# and launches it (make app-device). The phone must be unlocked, trust this Mac,
# and have Developer Mode on (Settings → Privacy & Security → Developer Mode).
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
"$HERE/scripts/project.sh"
DEVICES="$(mktemp)"
trap 'rm -f "$DEVICES"' EXIT

xcrun devicectl list devices --json-output "$DEVICES" >/dev/null
read -r UDID IDENTIFIER < <(python3 - "$DEVICES" <<'PY'
import json, sys
for device in json.load(open(sys.argv[1]))["result"]["devices"]:
    if device["hardwareProperties"].get("platform") == "iOS":
        print(device["hardwareProperties"]["udid"], device["identifier"])
        break
PY
) || true
if [ -z "${UDID:-}" ]; then
  echo "No iPhone found. Connect it with a cable, unlock it and tap Trust; then" >&2
  echo "xcrun devicectl list devices should list it." >&2
  exit 1
fi

cd "$HERE"
xcodebuild -project NeoAgent.xcodeproj -scheme NeoAgent -configuration Debug \
  -destination "id=$UDID" -derivedDataPath build/device \
  -allowProvisioningUpdates -allowProvisioningDeviceRegistration build -quiet
APP=build/device/Build/Products/Debug-iphoneos/NeoAgent.app
xcrun devicectl device install app --device "$IDENTIFIER" "$APP"
BUNDLE_ID="$(/usr/libexec/PlistBuddy -c 'Print :CFBundleIdentifier' "$APP/Info.plist")"
if ! xcrun devicectl device process launch --device "$IDENTIFIER" "$BUNDLE_ID"; then
  echo >&2
  echo "Installed, but the iPhone wouldn't open it. With a free Apple account, trust" >&2
  echo "your certificate first: Settings → General → VPN & Device Management → your" >&2
  echo "Apple ID → Trust. Then open Neo Agent from the Home Screen." >&2
  exit 1
fi
