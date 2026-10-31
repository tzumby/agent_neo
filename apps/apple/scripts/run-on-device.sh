#!/bin/bash
# Builds the iPhone app, installs it on the first paired iPhone and launches it.
# The phone must be unlocked (Xcode mounts its developer disk image).
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
BUNDLE_ID=com.tzumby.clouddesktop
DEVICES="$(mktemp)"
trap 'rm -f "$DEVICES"' EXIT

xcrun devicectl list devices --json-output "$DEVICES" >/dev/null
read -r UDID IDENTIFIER < <(python3 - "$DEVICES" <<'EOF'
import json, sys
for device in json.load(open(sys.argv[1]))["result"]["devices"]:
    if device["hardwareProperties"].get("platform") == "iOS":
        print(device["hardwareProperties"]["udid"], device["identifier"])
        break
EOF
)
if [ -z "${UDID:-}" ]; then
  echo "No paired iPhone found (xcrun devicectl list devices)." >&2
  exit 1
fi

cd "$HERE"
xcodegen generate --quiet
xcodebuild -project CloudDesktop.xcodeproj -scheme CloudDesktop -configuration Debug \
  -destination "id=$UDID" -derivedDataPath build/device -allowProvisioningUpdates build -quiet
xcrun devicectl device install app --device "$IDENTIFIER" build/device/Build/Products/Debug-iphoneos/CloudDesktop.app
xcrun devicectl device process launch --device "$IDENTIFIER" "$BUNDLE_ID"
