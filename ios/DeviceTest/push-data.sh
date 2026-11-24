#!/usr/bin/env bash
# Copies files into the installed app's Documents folder on a connected
# iPhone, without rebuilding: extracts (*.osm.pbf) and, with --sources DIR,
# the supplemental datasets `osm-tile-compiler fetch-sources DIR` wrote.
#
#   ios/DeviceTest/push-data.sh ~/maps/malopolskie-261118.osm.pbf
#   ios/DeviceTest/push-data.sh --sources ~/maps/sources
#   ios/DeviceTest/push-data.sh --pull          # copies runs.jsonl back here
#
# DEVICE picks the device (a name or identifier from
# `xcrun devicectl list devices`); the first connected iPhone otherwise.
set -euo pipefail

bundle_id="${BUNDLE_ID:-agency.limina.otc-device-test}"
device="${DEVICE:-}"
if [[ -z "$device" ]]; then
  json=$(mktemp)
  xcrun devicectl list devices --json-output "$json" >/dev/null
  device=$(python3 -c '
import json, sys
devices = json.load(open(sys.argv[1]))["result"]["devices"]
for d in devices:
    if d.get("hardwareProperties", {}).get("platform") == "iOS" and \
       d.get("connectionProperties", {}).get("tunnelState") != "unavailable":
        print(d["identifier"]); break
' "$json")
  rm -f "$json"
fi
[[ -n "$device" ]] || { echo "No connected iPhone found" >&2; exit 1; }

copy() { # source destination-in-Documents
  xcrun devicectl device copy to --device "$device" \
    --domain-type appDataContainer --domain-identifier "$bundle_id" \
    --source "$1" --destination "Documents/$2"
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --sources)
      dir="$2"; shift 2
      for f in "$dir"/*; do copy "$f" "sources/$(basename "$f")"; done ;;
    --pull)
      shift
      xcrun devicectl device copy from --device "$device" \
        --domain-type appDataContainer --domain-identifier "$bundle_id" \
        --source Documents/runs.jsonl --destination ./runs.jsonl
      echo "Wrote ./runs.jsonl" >&2 ;;
    *)
      copy "$1" "$(basename "$1")"; shift ;;
  esac
done
