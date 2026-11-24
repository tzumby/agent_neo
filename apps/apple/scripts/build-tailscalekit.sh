#!/bin/bash
# Builds TailscaleKit (tailscale/libtailscale, Swift bindings over an embedded
# Go tsnet node) into apps/apple/Vendor/TailscaleKit.xcframework, with iOS,
# iOS Simulator and macOS (arm64) slices. Needs Go and Xcode.
#
# Pinned to a commit that includes the TS-2026-012 fix (d1dc45d0).
set -euo pipefail

COMMIT=ac7ed43a
HERE="$(cd "$(dirname "$0")/.." && pwd)"
SRC="$HERE/vendor/libtailscale"
OUT="$HERE/DesktopKit/Vendor/TailscaleKit.xcframework"

if [ ! -d "$SRC/.git" ]; then
  git clone --quiet https://github.com/tailscale/libtailscale.git "$SRC"
fi
git -C "$SRC" fetch --quiet origin
git -C "$SRC" checkout --quiet "$COMMIT"

cd "$SRC/swift"
make ios ios-sim macos XCPRETTIFIER=cat

PRODUCTS="$SRC/swift/build/Build/Products"
rm -rf "$OUT"
mkdir -p "$(dirname "$OUT")"
xcodebuild -create-xcframework \
  -framework "$PRODUCTS/Release-iphoneos/TailscaleKit.framework" \
  -framework "$PRODUCTS/Release-iphonesimulator/TailscaleKit.framework" \
  -framework "$PRODUCTS/Release/TailscaleKit.framework" \
  -output "$OUT"
echo "Built $OUT"
