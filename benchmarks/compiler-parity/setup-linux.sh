#!/usr/bin/env bash
# Prepares a Planetiler comparison on Linux x86_64, outside the checkout:
#
#   benchmarks/compiler-parity/setup-linux.sh [BENCH_DIR] [PBF_URL]
#
# BENCH_DIR (default ~/osm-tile-benchmarks) receives a JDK 21, the pinned
# Planetiler 0.10.2 jar, the extract (default Geofabrik's Malopolskie), the
# supplemental datasets, a copy of the release compile_omt named after the
# commit it was built from, and omt-config.json pinning the extract's and the
# jar's SHA-256. Existing downloads are kept. Then:
#
#   python benchmarks/compiler-parity/run_omt.py --config BENCH_DIR/omt-config.json --output NEW_DIR
set -euo pipefail

bench=${1:-$HOME/osm-tile-benchmarks}
pbf_url=${2:-https://download.geofabrik.de/europe/poland/malopolskie-latest.osm.pbf}
jar_sha256=f310bd0413e2e4512b27f4046d418664e8e1d3bf31603c2a70e23de06c167e4d
here=$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)
repo=$(cd "$here/../.." && pwd)
region=$(basename "$pbf_url" -latest.osm.pbf)
region=${region%.osm.pbf}

mkdir -p "$bench/tools" "$bench/$region/input" "$bench/sources"

if [[ ! -x $bench/tools/jdk21/bin/java ]]; then
  echo "Downloading JDK 21 (Temurin)"
  curl -fsSL -o "$bench/tools/jdk21.tar.gz" \
    "https://api.adoptium.net/v3/binary/latest/21/ga/linux/x64/jdk/hotspot/normal/eclipse"
  mkdir -p "$bench/tools/jdk21"
  tar -xzf "$bench/tools/jdk21.tar.gz" -C "$bench/tools/jdk21" --strip-components=1
  rm "$bench/tools/jdk21.tar.gz"
fi
jar=$bench/tools/planetiler-0.10.2.jar
if [[ ! -f $jar ]]; then
  echo "Downloading Planetiler 0.10.2"
  curl -fsSL -o "$jar.part" \
    "https://github.com/onthegomap/planetiler/releases/download/v0.10.2/planetiler.jar"
  mv "$jar.part" "$jar"
fi
echo "$jar_sha256  $jar" | sha256sum -c --quiet -

pbf=$bench/$region/input/$region.osm.pbf
if [[ ! -f $pbf ]]; then
  echo "Downloading $pbf_url"
  curl -fsSL -o "$pbf.part" "$pbf_url"
  mv "$pbf.part" "$pbf"
fi

echo "Building compile_omt"
(cd "$repo" && RUSTC_WRAPPER= cargo build --release --locked -p osm-tile-compiler \
  --features benchmark-profiling --bin osm-tile-compiler --example compile_omt)
revision=$(git -C "$repo" rev-parse --short=12 HEAD)
native=$bench/tools/compile_omt-$revision
cp "$repo/target/release/examples/compile_omt" "$native"

"$repo/target/release/osm-tile-compiler" fetch-sources "$bench/sources"

read -r west south east north < <(python3 -I "$here/pbf_bounds.py" "$pbf")
pbf_sha256=$(sha256sum "$pbf" | cut -d' ' -f1)
cat > "$bench/omt-config.json" <<EOF
{
  "input_pbf": "$pbf",
  "native_binary": "$native",
  "native_build_revision": "$(git -C "$repo" rev-parse HEAD)",
  "java_binary": "$bench/tools/jdk21/bin/java",
  "planetiler_jar": "$jar",
  "planetiler_sources": "$bench/sources",
  "java_heap": "4g",
  "threads": 4,
  "repeats": 1,
  "timeout_seconds": 3600,
  "sample_interval_seconds": 0.1,
  "rss_limit_gib": 9,
  "bounds": [$west, $south, $east, $north],
  "expected_sha256": {
    "input_pbf": "$pbf_sha256",
    "planetiler_jar": "$jar_sha256"
  }
}
EOF
echo "Wrote $bench/omt-config.json (extract $pbf_sha256, bounds $west $south $east $north)"
