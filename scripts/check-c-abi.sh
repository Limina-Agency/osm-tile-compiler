#!/usr/bin/env bash
# Builds the C ABI's static library and links tests/c/smoke.c against it with
# the system C compiler, then compiles fixtures/monaco.osm.pbf through it.
#
#   scripts/check-c-abi.sh
#
# Needs cc. Linux and macOS.
set -euo pipefail

cd "$(dirname "${BASH_SOURCE[0]}")/.."
export RUSTC_WRAPPER=

crate=osm-tile-compiler-ffi
cargo build -p "$crate" --locked
target=$(cargo metadata --format-version 1 --no-deps | python3 -c 'import json,sys; print(json.load(sys.stdin)["target_directory"])')
out=$(mktemp -d)
trap 'rm -rf "$out"' EXIT

case "$(uname -s)" in
  Darwin) libs=(-framework CoreFoundation -framework Security -lc++) ;;
  *) libs=(-lpthread -ldl -lm -lstdc++) ;;
esac
cc -std=c99 -Wall -Wextra -Werror -I "$crate/include" "$crate/tests/c/smoke.c" \
  "$target/debug/libosm_tile_compiler_ffi.a" "${libs[@]}" -o "$out/smoke"
"$out/smoke" "$PWD/fixtures/monaco.osm.pbf" "$out"
