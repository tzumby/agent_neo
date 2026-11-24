#!/bin/bash
# Checks what building the apps needs, then generates NeoAgent.xcodeproj with
# your Apple team and the apps' bundle ID (make app-project; make app-device
# and make app-mac run it first).
#
#   APPLE_TEAM_ID  your Apple developer team's ID (10 letters and digits);
#                  without it, this lists the teams this Mac can sign for.
#   APP_BUNDLE_ID  optional: the apps' bundle ID. Defaults to one made from your
#                  team ID, so it's yours (a bundle ID belongs to one team).
set -euo pipefail

HERE="$(cd "$(dirname "$0")/.." && pwd)"
cd "$HERE"

die() {
  printf '%s\n' "$@" >&2
  exit 1
}

# The teams this Mac has an Apple Development certificate for: the OU of each
# certificate (the ID in brackets in its name is something else).
signing_teams() {
  local dir
  dir="$(mktemp -d)"
  security find-certificate -a -c "Apple Development" -p 2>/dev/null |
    awk -v dir="$dir" '/BEGIN CERTIFICATE/ {n++} n {print > (dir "/" n ".pem")}'
  for pem in "$dir"/*.pem; do
    [[ -e "$pem" ]] || continue
    openssl x509 -in "$pem" -noout -subject -nameopt multiline 2>/dev/null |
      awk -F' = ' '/organizationalUnitName/ {print $2}'
  done | sort -u
  rm -rf "$dir"
}

xcode="$(xcodebuild -version 2>/dev/null | awk '/^Xcode/ {print $2}')" || true
if [[ -z "$xcode" ]]; then
  die "Xcode isn't set up. Install Xcode 26 or later from the App Store, open it once," \
    "then: sudo xcode-select -s /Applications/Xcode.app"
fi
if ((${xcode%%.*} < 26)); then
  die "Xcode $xcode is too old: the iPhone app needs Xcode 26 or later (iOS 26)."
fi
command -v xcodegen >/dev/null || die "XcodeGen isn't installed: brew install xcodegen"
[[ -d DesktopKit/Vendor/TailscaleKit.xcframework ]] ||
  die "TailscaleKit isn't built yet: make app-tailscalekit (once, a few minutes; needs Go)."

if [[ -z "${APPLE_TEAM_ID:-}" ]]; then
  teams="$(signing_teams)"
  if [[ -n "$teams" ]]; then
    die "Set APPLE_TEAM_ID to the Apple developer team to sign the apps with." \
      "This Mac can sign for:" "$(sed 's/^/  export APPLE_TEAM_ID=/' <<<"$teams")"
  fi
  die "Set APPLE_TEAM_ID to the Apple developer team to sign the apps with." \
    "This Mac can't sign for any yet: open Xcode → Settings → Accounts, add your" \
    "Apple ID (a free one works), select its team, and Manage Certificates → + →" \
    "Apple Development. Then run this again: it lists the team's ID."
fi
[[ "$APPLE_TEAM_ID" =~ ^[A-Z0-9]{10}$ ]] ||
  die "APPLE_TEAM_ID is 10 capital letters and digits, e.g. A1B2C3D4E5; not '$APPLE_TEAM_ID'."

# Lowercase: bundle IDs are case-insensitive, and Apple keeps the first spelling.
team_lower="$(tr '[:upper:]' '[:lower:]' <<<"$APPLE_TEAM_ID")"
export APPLE_TEAM_ID
export APP_BUNDLE_ID="${APP_BUNDLE_ID:-dev.neoagent.app.$team_lower}"
[[ "$APP_BUNDLE_ID" =~ ^[A-Za-z0-9.-]+$ ]] ||
  die "APP_BUNDLE_ID has letters, digits, dots and dashes only; not '$APP_BUNDLE_ID'."

# Notifications are opt-in: APP_PUSH=1 adds the Push Notifications capability
# (aps-environment). It needs the paid Apple Developer Program — a free
# Personal Team can't provision it, and the build fails. Off by default, so the
# apps build and run on any account; the agent still reaches browsers by Web
# Push either way.
push=off
case "${APP_PUSH:-}" in 1 | on | true | yes) push=on ;; esac

xcodegen generate --quiet

# The entitlement files each target points at, written AFTER xcodegen (which
# would otherwise overwrite a path it's given with an empty file). aps-environment
# is present only when push is on, so a free account can still build the rest.
aps_ios=""
aps_mac=""
if [[ "$push" == on ]]; then
  aps_ios='  <key>aps-environment</key>
  <string>development</string>'
  aps_mac='  <key>com.apple.developer.aps-environment</key>
  <string>development</string>'
fi
cat >NeoAgent/iOS/NeoAgent.entitlements <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
$aps_ios
</dict>
</plist>
EOF
cat >NeoAgent/macOS/NeoAgent.entitlements <<EOF
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>com.apple.security.app-sandbox</key>
  <true/>
  <key>com.apple.security.network.client</key>
  <true/>
  <!-- The app's Tailscale node serves its SOCKS5 proxy on 127.0.0.1. -->
  <key>com.apple.security.network.server</key>
  <true/>
  <!-- Memory: import a file, save an export. -->
  <key>com.apple.security.files.user-selected.read-write</key>
  <true/>
$aps_mac
</dict>
</plist>
EOF

echo "Generated $HERE/NeoAgent.xcodeproj (team $APPLE_TEAM_ID, bundle ID $APP_BUNDLE_ID, push $push)."
if [[ "$push" == on ]]; then
  echo "Push Notifications are on: this needs the paid Apple Developer Program."
fi
