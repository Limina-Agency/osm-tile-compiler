#!/usr/bin/env bash
# Decodes the compiler's MLT tiles with the reference decoders that renderers
# use, and checks that each reads exactly the features the tiles hold:
#
#   rust  mlt-core, through the `mlt` command line tool (Martin, QGIS, Python)
#   ts    @maplibre/mlt (MapLibre GL JS)
#   cpp   maplibre-tile-spec's C++ decoder at the commit MapLibre Native's
#         newest iOS and Android releases bundle (MapLibre Native)
#
#   scripts/check-mlt-conformance.sh [ARCHIVE.pmtiles]
#
# The corpus is every tile of Monaco compiled with --tile-format mlt, or of
# the MLT archive given. The versions are pinned below; bump them when a
# new release ships (the nightly workflow's check-mlt-spec.sh tracks the
# specification itself). DECODERS limits the run, for example DECODERS=ts.
# Needs cargo, node and npm, and for cpp git, cmake and a C++20 compiler.
set -euo pipefail

MLT_CLI_VERSION=0.7.0 # mlt-core 0.23.0
# maplibre-tile-spec as vendored by MapLibre Android 13.7.1 and iOS 7.0.0
# (vendor/maplibre-tile-spec of maplibre/maplibre-native).
MLT_CPP_COMMIT=${MLT_CPP_COMMIT:-751ebb1837e4fd03c9074b91fce24727013c1868}
DECODERS=${DECODERS:-rust ts cpp}

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
cd "$root"
work=${MLT_CONFORMANCE_WORK:-$root/target/mlt-conformance}
corpus=$work/corpus
rm -rf "$corpus"
mkdir -p "$corpus"

archive=()
if [[ $# -gt 0 ]]; then
  archive=(env "MLT_CONFORMANCE_ARCHIVE=$(cd "$(dirname "$1")" && pwd)/$(basename "$1")")
fi
"${archive[@]}" env MLT_CONFORMANCE_DIR="$corpus" cargo test --release --locked -q \
  -p osm-tile-compiler --test mlt write_conformance_corpus -- --ignored --nocapture 2>&1 |
  grep -E "tiles,|panicked|error" || true
ls "$corpus"/*.mlt > /dev/null

for decoder in $DECODERS; do
  case $decoder in
    rust)
      mlt=$work/rust/bin/mlt
      if [[ ! -x $mlt ]] || [[ $("$mlt" --version) != *"$MLT_CLI_VERSION"* ]]; then
        cargo install --quiet --locked --root "$work/rust" mlt --version "$MLT_CLI_VERSION"
      fi
      for tile in "$corpus"/*.mlt; do
        "$mlt" decode --format geo-json "$tile" > "${tile%.mlt}.rust.json"
      done
      echo "rust: $(ls "$corpus" | grep -c '\.rust\.json$') tiles decoded"
      ;;
    ts)
      (cd scripts/mlt-conformance && npm ci --silent --no-audit --no-fund)
      node --import ./scripts/mlt-conformance/resolve-js.mjs scripts/mlt-conformance/decode-ts.mjs "$corpus"
      ;;
    cpp)
      src=$work/cpp-$MLT_CPP_COMMIT
      if [[ ! -x $src/build/tool/mlt-cpp-json ]]; then
        rm -rf "$src"
        git init --quiet "$src"
        git -C "$src" fetch --quiet --depth 1 https://github.com/maplibre/maplibre-tile-spec "$MLT_CPP_COMMIT"
        git -C "$src" checkout --quiet FETCH_HEAD
        # The C++ build needs its vendored libraries, not the tests' googletest.
        git -C "$src" submodule --quiet update --init --depth 1 \
          $(git -C "$src" config -f .gitmodules --get-regexp "^submodule\.cpp/vendor/.*\.path$" | awk '$2 != "cpp/vendor/googletest" {print $2}')
        cmake -S "$src/cpp" -B "$src/build" -DCMAKE_BUILD_TYPE=Release -DMLT_WITH_TESTS=OFF \
          -DMLT_WITH_TOOLS=ON > /dev/null
        cmake --build "$src/build" --target mlt-cpp-json --parallel > /dev/null
      fi
      failed=0
      for tile in "$corpus"/*.mlt; do
        if ! "$src/build/tool/mlt-cpp-json" "$tile" > "${tile%.mlt}.cpp.json" 2> "$work/cpp.err"; then
          echo "cpp: $(basename "$tile"): $(cat "$work/cpp.err")"
          echo '{"features":[]}' > "${tile%.mlt}.cpp.json"
          failed=$((failed + 1))
        fi
      done
      echo "cpp: $(ls "$corpus" | grep -c '\.cpp\.json$') tiles decoded, $failed failed"
      ;;
    *)
      echo "unknown decoder $decoder" >&2
      exit 2
      ;;
  esac
done

MLT_CONFORMANCE_DIR="$corpus" MLT_CONFORMANCE_DECODERS="$DECODERS" cargo test --release --locked -q \
  -p osm-tile-compiler --test mlt reference_decoders_agree -- --ignored --nocapture
