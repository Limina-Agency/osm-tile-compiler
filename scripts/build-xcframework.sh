#!/usr/bin/env bash
# Builds the C ABI as an XCFramework for iOS: the static library for devices
# (aarch64-apple-ios) and for the simulator on Apple silicon
# (aarch64-apple-ios-sim), with the header and a module map, so Swift can
# `import OsmTileCompilerFFI`.
#
#   scripts/build-xcframework.sh            # release build
#   PROFILE=dev scripts/build-xcframework.sh
#   INTEL_SIMULATOR=1 scripts/build-xcframework.sh   # adds x86_64-apple-ios
#
# Writes target/ios/OsmTileCompilerFFI.xcframework (OUT overrides the
# directory). Needs macOS with Xcode and rustup.
#
# A host app links it with -lc++ and the CoreFoundation and Security
# frameworks, as scripts/check-c-abi.sh does on macOS.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
export RUSTC_WRAPPER=

if [[ "$(uname -s)" != Darwin ]]; then
  echo "build-xcframework.sh needs macOS and Xcode" >&2
  exit 1
fi

# Rust links for iOS 10 by default, but the C build scripts (SQLite, ring,
# zstd) compile for the SDK's newest iOS and call ___chkstk_darwin, which
# iOS 13 introduced. The test app targets iOS 16.
export IPHONEOS_DEPLOYMENT_TARGET="${IPHONEOS_DEPLOYMENT_TARGET:-13.0}"

profile="${PROFILE:-release}"
profile_dir="$profile"
[[ "$profile" == dev ]] && profile_dir=debug
out="${OUT:-target/ios}"
crate=osm-tile-compiler-ffi
lib=libosm_tile_compiler_ffi.a
name=OsmTileCompilerFFI

device=aarch64-apple-ios
simulators=(aarch64-apple-ios-sim)
[[ "${INTEL_SIMULATOR:-0}" == 1 ]] && simulators+=(x86_64-apple-ios)

rustup target add "$device" "${simulators[@]}"
for target in "$device" "${simulators[@]}"; do
  echo "== $target ($profile)" >&2
  cargo build --locked --lib -p "$crate" --profile "$profile" --target "$target"
done

target_dir=$(cargo metadata --format-version 1 --no-deps |
  python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')

mkdir -p "$out"
work="$out/build"
rm -rf "$work" "$out/$name.xcframework"
mkdir -p "$work/include" "$work/simulator"

cp "$crate/include/osm_tile_compiler.h" "$work/include/"
cat >"$work/include/module.modulemap" <<EOF
module $name {
    header "osm_tile_compiler.h"
    export *
}
EOF

# One simulator slice holds every simulator architecture.
sim_libs=()
for target in "${simulators[@]}"; do
  sim_libs+=("$target_dir/$target/$profile_dir/$lib")
done
lipo -create "${sim_libs[@]}" -output "$work/simulator/$lib"

xcodebuild -create-xcframework \
  -library "$target_dir/$device/$profile_dir/$lib" -headers "$work/include" \
  -library "$work/simulator/$lib" -headers "$work/include" \
  -output "$out/$name.xcframework"
rm -rf "$work"

echo "Wrote $out/$name.xcframework" >&2
du -sh "$out/$name.xcframework"/*/ >&2
