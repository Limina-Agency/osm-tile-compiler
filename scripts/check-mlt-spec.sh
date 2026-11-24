#!/usr/bin/env bash
# Checks the MLT decoder against the MapLibre Tile specification as it is
# now: clones maplibre/maplibre-tile-spec (main, or REF), decodes every v1
# synthetic fixture in an encoding the decoder reads, and compares the
# vendored copies in fixtures/ with upstream's.
#
#   scripts/check-mlt-spec.sh [--update] [REF]
#
# It fails when a fixture the decoder reads decodes wrongly, or when a
# vendored fixture differs from upstream's or is gone. --update instead
# re-copies every fixture the decoder reads into fixtures/mlt-synthetic
# (0x01) and fixtures/mlt-synthetic-rust (0x01-rust) and records the commit
# in fixtures/mlt-spec-commit, to commit together. The nightly workflow runs
# it without --update.
set -euo pipefail
shopt -s nullglob

cd "$(dirname "${BASH_SOURCE[0]}")/.."
update=false
if [[ ${1:-} == --update ]]; then
  update=true
  shift
fi
ref=${1:-main}

spec=$(mktemp -d)
trap 'rm -rf "$spec"' EXIT
git clone --quiet --depth 1 --branch "$ref" https://github.com/maplibre/maplibre-tile-spec "$spec" 2>/dev/null ||
  { git clone --quiet https://github.com/maplibre/maplibre-tile-spec "$spec" && git -C "$spec" checkout --quiet "$ref"; }
commit=$(git -C "$spec" rev-parse HEAD)
echo "maplibre-tile-spec $commit ($(git -C "$spec" log -1 --format=%cs))"

synthetic=$spec/test/synthetic
log=$(mktemp)
MLT_SPEC_FIXTURES="$synthetic/0x01:$synthetic/0x01-rust" \
  cargo test --locked -p osm-tile-compiler --lib \
  mlt::tests::the_decoder_reads_the_current_specification_fixtures -- --ignored --nocapture \
  2>&1 | tee "$log" | grep -E "fixtures decoded|test result|panicked" || true
grep -q "test result: ok. 1 passed" "$log" || { cat "$log"; exit 1; }

# The fixtures each upstream directory has that the decoder reads.
decodable() {
  local dir=$1 unsupported
  unsupported=$(grep "^unsupported in $dir:" "$log" | sed 's/^[^:]*: //')
  for tile in "$dir"/*.mlt; do
    name=$(basename "$tile" .mlt)
    [[ " $unsupported " == *" $name "* ]] || echo "$name"
  done
}

status=0
for pair in "0x01:mlt-synthetic" "0x01-rust:mlt-synthetic-rust"; do
  upstream=$synthetic/${pair%%:*}
  vendored=fixtures/${pair#*:}
  if $update; then
    rm -rf "$vendored"
    mkdir -p "$vendored"
    for name in $(decodable "$upstream"); do
      cp "$upstream/$name.mlt" "$vendored/"
      # The JSON compacted, as the rest of fixtures/ is.
      python3 -c 'import json,sys; json.dump(json.load(open(sys.argv[1])), open(sys.argv[2],"w"), separators=(",",":"))' \
        "$upstream/$name.json" "$vendored/$name.json"
    done
    echo "$vendored: $(ls "$vendored" | grep -c '\.mlt$') fixtures"
    continue
  fi
  for tile in "$vendored"/*.mlt; do
    name=$(basename "$tile" .mlt)
    if [[ ! -f $upstream/$name.mlt ]]; then
      echo "$vendored/$name.mlt: gone upstream"
      status=1
    elif ! cmp -s "$tile" "$upstream/$name.mlt"; then
      echo "$vendored/$name.mlt: changed upstream"
      status=1
    fi
  done
  missing=$(comm -13 <(ls "$vendored" 2>/dev/null | sed -n 's/\.mlt$//p' | sort) <(decodable "$upstream" | sort))
  if [[ -n $missing ]]; then
    echo "$upstream: fixtures the decoder reads that are not vendored (scripts/check-mlt-spec.sh --update):"
    echo "$missing" | sed 's/^/  /'
  fi
done
if $update; then
  echo "$commit" > fixtures/mlt-spec-commit
fi
exit $status
