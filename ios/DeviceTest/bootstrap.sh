#!/usr/bin/env bash
# Builds the XCFramework, puts the Monaco fixture (and any extracts named on
# the command line) into DeviceData/ for bundling, and generates
# OtcDeviceTest.xcodeproj with XcodeGen.
#
#   DEVELOPMENT_TEAM=ABCDE12345 ios/DeviceTest/bootstrap.sh [extract.osm.pbf ...]
#
# DEVELOPMENT_TEAM is the signing team ID (Xcode > Settings > Accounts, or
# leave it empty and pick the team in Xcode). BUNDLE_ID defaults to
# agency.limina.otc-device-test; a personal team may need a unique one.
# INCREASED_MEMORY=1 signs with the increased-memory-limit entitlement,
# which the App ID must allow. SKIP_RUST=1 skips the XCFramework build.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
root="$(cd "$here/../.." && pwd)"

if [[ "${SKIP_RUST:-0}" != 1 ]]; then
  "$root/scripts/build-xcframework.sh"
fi

mkdir -p "$here/DeviceData"
cp "$root/fixtures/monaco.osm.pbf" "$here/DeviceData/"
for extract in "$@"; do
  cp "$extract" "$here/DeviceData/"
done
du -sh "$here/DeviceData"/* >&2

if ! command -v xcodegen >/dev/null; then
  echo "XcodeGen is missing: brew install xcodegen" >&2
  exit 1
fi
export DEVELOPMENT_TEAM="${DEVELOPMENT_TEAM:-}"
export BUNDLE_ID="${BUNDLE_ID:-agency.limina.otc-device-test}"
if [[ "${INCREASED_MEMORY:-0}" == 1 ]]; then
  export ENTITLEMENTS=IncreasedMemory.entitlements
else
  export ENTITLEMENTS=
fi
(cd "$here" && xcodegen generate)
echo "Open $here/OtcDeviceTest.xcodeproj, pick the iPhone and Run." >&2
