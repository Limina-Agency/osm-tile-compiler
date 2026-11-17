# Compiler comparison laboratory

This laboratory measures the embeddable Rust compiler and the pinned Planetiler
reference on the same downloaded Małopolskie PBF. It separates three questions:

1. Did a native change preserve the decoded output and source contract?
2. How did native compilation wall time, sampled process-tree RSS and output size change?
3. Does native output implement the same OpenMapTiles profile as Planetiler?

[planetiler-effective-settings.json](planetiler-effective-settings.json) records
the pinned reference settings; unknown values remain explicit.

The Rust library now has a separate **experimental native OpenMapTiles 3.16**
profile with source-policy handlers for all 16 layers and native supplemental
readers. It does not use Tactical layer renaming or invoke Java to compile.
Its parity with the pinned full OMT profile is **unverified**. The legacy
`run.py --profile matched-omt` guard remains blocked; its Tactical executable
(`compile_region`) was removed on 2026-10-09, so use the dedicated OMT runner
below. `run.py` remains only as the helper library imported by `run_omt.py`. Neither runner
reports equivalent-work speedups.

## Native OMT API and experiment

The reusable Rust API is
`osm_tile_compiler::omt::compile_openmaptiles(&OpenMapTilesRequest)`.
The request carries local PBF/supplement paths, output/scratch paths, explicit
bounds, zooms and workers. The example is a thin command-line caller:

```powershell
cargo build --release --locked -p osm-tile-compiler --example compile_omt
compile_omt INPUT.pbf NEW_OUTPUT_DIR SOURCES_DIR --bounds WEST SOUTH EAST NORTH --threads 4 --min-zoom 0 --max-zoom 14
```

For the sequential experiment, copy the external configuration, set
`native_binary` to the preserved **compile_omt** executable, and add
`"bounds": [WEST, SOUTH, EAST, NORTH]`. Keep the pinned PBF/JAR hashes and offline
supplement directory. `run_omt.py` defaults to **one repeat**, overriding a
legacy configuration's repeat count unless `--repeats` is supplied. Use
`--engine native` for a native-only retest against a preserved reference archive
(default `--engine both`; `planetiler` is also supported):

```powershell
python benchmarks/compiler-parity/run_omt.py --config OMT_CONFIG.json --output NEW_DRY_RUN_DIR --dry-run
python benchmarks/compiler-parity/run_omt.py --config OMT_CONFIG.json --output NEW_MEASUREMENT_DIR
```

Native then Planetiler execute sequentially, with explicit identical bounds,
z0–14 and four workers when configured. Planetiler receives one tile writer,
render-maxzoom 14, disabled downloads and disabled Wikidata. Native emits
`basemap.pmtiles` and `compile-result.json`; there is no Tactical annotation
companion. Logs, process-tree RSS samples, commands, source/tool/archive hashes,
measurements, and compile result are retained. Downloads, builds, and fingerprint
verification are outside the timed region. Core source policy and default zoom
settings are intended to match; known implementation differences remain.

Comparison manifests share `OpenMapTiles3.16.0` but deliberately identify
`native-experimental` versus `full-omt`, so a matching sample cannot accidentally
be promoted to verified profile parity. A completed timing run is not an
accuracy qualification. Two sequential experiments have completed. On the same
111 sampled coordinates, v2 eliminated the observed aerodrome elevation and
mountain-peak rank mismatches; place rank mismatches fell from 1,453 to 13,
while 37 POI rank mismatches remained. These counts describe paired feature
occurrences, not unique objects or an archive parity score. Geometry and other
profile differences remain. The final native-only v3 retest against the preserved
v2 reference retained those rank improvements and removed 27 sampled native
feature occurrences after the source-point size correction. Raw totals are
29,295 native and 29,264 reference; this small count difference does not imply
semantic equivalence. V3 still has 64 housenumber, 101 park and 24 POI geometry
differences among paired label features, plus unmatched keys. The read-only
label diagnostic reports no decode errors or duplicate pairing keys; strict
geometry validation is a separate comparison check. Artifacts are preserved
under the external experiment root in `omt-v3/label-diagnostics.json`.

## Setup

### Linux

```sh
benchmarks/compiler-parity/setup-linux.sh [BENCH_DIR] [PBF_URL]
python benchmarks/compiler-parity/run_omt.py --config BENCH_DIR/omt-config.json --output BENCH_DIR/runs/NEW_DIR
```

`setup-linux.sh` prepares everything outside the checkout, in `BENCH_DIR`
(default `~/osm-tile-benchmarks`): a Temurin JDK 21, the Planetiler 0.10.2 jar
(checked against its pinned SHA-256), the extract (default Geofabrik's
Małopolskie), the supplemental datasets via `fetch-sources`, and a release
`compile_omt` copied as `tools/compile_omt-<commit>`. It writes
`omt-config.json` with the extract's bounds (read by `pbf_bounds.py`) and its
SHA-256 pinned. Geofabrik replaces its extracts daily, so a new download is a
new input: pin it and build a new Planetiler reference from it rather than
comparing against an archive built from another day's extract. The native
compiler extracts Natural Earth into `sources/.extracted` on its first run;
the runner fingerprints only the three source archives, so that cache does
not invalidate a run.

### Windows and general


Use Python 3.10+ and install `requirements.txt` in a benchmark virtual environment.
The runner uses `psutil`; the comparison tools decode archives through
`validate-pmtiles.py` (pmtiles, mapbox-vector-tile, shapely), and
`source-probes.py` reads PBFs with osmium. No Java or Python dependency is added
to the Rust library.

Build outside timed runs:

```powershell
cargo build --release --locked -p osm-tile-compiler --example compile_omt --features benchmark-profiling
python -m pip install -r benchmarks/compiler-parity/requirements.txt
```

Copy `example-config.json` outside the checkout and adjust absolute paths. Keep
the PBF and reference JAR SHA256 expectations pinned. The example selects four
configured workers, physical/detail z14, Java heap 4 GiB and three repeats.
Java heap is not process RSS. The full OMT reference also requires the previously
downloaded Natural Earth, water polygons and lake centerlines.

## Execute

```powershell
python benchmarks/compiler-parity/run.py --config YOUR_CONFIG.json --output NEW_OUTPUT_DIRECTORY --dry-run
python benchmarks/compiler-parity/run.py --config YOUR_CONFIG.json --output ANOTHER_NEW_DIRECTORY
python benchmarks/compiler-parity/run.py --config YOUR_CONFIG.json --output OMT_GATE_DIRECTORY --profile matched-omt
```

Output directories must be new. The runner preserves commands, source and binary
hashes, repository state, host details, logs, RSS timelines, archive hashes and
per-run measurements. Compilers execute sequentially and alternate order between
repeats. Downloads, builds and archive hashing are outside timed compilation.
Keep other builds and compilers idle during measurements. Page cache is not
flushed; these are ordinary warm/uncontrolled-cache runs.

`run_state` and expected/completed run counts distinguish a completed batch from
an interrupted one. Fingerprints are checked around each timed run. Git state
identifies the harness checkout; it does not prove the executable's build
revision. Preserve each executable and its build record before changing code.
An optional `native_build_revision` configuration value is recorded as an
assertion, not inferred from the harness checkout.

For before/after experiments, preserve the baseline executable, change
`native_binary` in separate configurations and repeat on the same host. Include
both the map and annotation companion in Tactical product size. A single run is
exploratory evidence, not a stable performance claim. Keep logs and compiler
phase events for attribution; overlapping or nested phases are not additive.

## Accuracy and parity

```powershell
python benchmarks/compiler-parity/compare.py LEFT.pmtiles RIGHT.pmtiles --left-manifest LEFT_COMPARISON_MANIFEST.json --right-manifest RIGHT_COMPARISON_MANIFEST.json --output comparison.json
# Full decoding, when qualifying a release:
python benchmarks/compiler-parity/compare.py LEFT.pmtiles RIGHT.pmtiles --left-manifest LEFT_COMPARISON_MANIFEST.json --right-manifest RIGHT_COMPARISON_MANIFEST.json --all-tiles --workers 4 --output comparison-full.json
```

The comparison reads all directory addresses and samples the same deterministic
union of tile coordinates on both sides. It compares decoded geometry, feature
IDs and typed attributes independently of protobuf dictionary/feature ordering.
Line direction remains significant. Comparison manifests bind the archive
SHA256, input hash, auxiliary inputs, profile, annotation policy, bounds policy
and tile compression. Missing or conflicting provenance prevents a parity pass.
Geometry validity and semantic equality are separate requirements: reproducing
an invalid polygon is not a passing accuracy result. A sample cannot prove full
archive parity. Byte identity alone does not establish valid geometry either.
The decoder is sequential by default. `--workers` bounds parallel tile decoding;
each worker opens both archives once and the report remains in coordinate order.

For actionable OMT label examples and raw counts that continue past malformed
features, reuse the comparison's exact coordinates:

```powershell
python benchmarks/compiler-parity/diagnose_omt.py NATIVE.pmtiles REFERENCE.pmtiles --comparison comparison.json --output label-diagnostics.json
# Compare a later native build on the original sample, not a freshly selected one:
python benchmarks/compiler-parity/diagnose_omt.py NATIVE_V2.pmtiles REFERENCE.pmtiles --comparison comparison-v2.json --coordinates-from comparison.json --output label-diagnostics-v2.json
```

The diagnostic counts protobuf features before decoding geometry, records
per-feature decode errors, and reports duplicate layer/ID/geometry-type pairing
keys instead of silently overwriting them. Examples retain typed attributes.
It is a read-only diagnostic, not a geometry validator or profile-parity gate.
Run archive decoding outside timed benchmark intervals.

For a later compiler iteration, audit the same selected tiles against the
reference with separate geometry, typed-attribute, membership, validity and
decode results:

```powershell
python benchmarks/compiler-parity/audit_iterations.py BASELINE.pmtiles CANDIDATE.pmtiles REFERENCE.pmtiles --coordinates-from selected-coordinates.json --original-coordinates-from original-comparison.json --repeat-archive CANDIDATE_REPEAT.pmtiles --output iteration-audit.json
```

`--original-coordinates-from` and `--repeat-archive` are optional. The audit
labels keys absent from both the candidate and reference as `absent`, distinct
from a one-sided `unmatched` key. Its tile-local content multiset ignores IDs
and excludes undecoded or invalid features; matching content does not establish
feature-ID parity. Run it outside timed compilations.

Add `--polygon-area` to quantify changed polygons that remain unequal using
symmetric-difference area against the reference. It reports closer, worse and
same, with explicit exclusions for invalid or undecodable geometry. The default
combined-coordinate cap is 2,000 per comparison; use
`--polygon-area-max-coordinates 0` for an uncapped audit of large merged polygons.
This metric measures coverage differences in MVT square units, not visual parity.

To rank remaining polygon coverage errors with bounded overlay work, run:

```powershell
python benchmarks/compiler-parity/polygon_impact.py NATIVE.pmtiles REFERENCE.pmtiles --comparison-report full-comparison.json --output polygon-impact.json
```

The report must identify the same archive hashes. Selection includes all
mismatched tiles through z8 and up to 64 deterministic samples per higher zoom.
Areas use a 256-pixel tile display; invalid, ambiguous, unmatched and over-cap
features are explicit exclusions. The default combined-coordinate overlay cap
is 2,000, with Hausdorff distance capped at 500. This bounds geometry operations
after decoding, not total decoding or validity-check costs. For before/after
experiments, reuse the recorded coordinates through the `audit` function;
changing the sample cannot establish that the same cases improved.

Use `validate-pmtiles.py` with `--probes source-probes.json` (written by
`source-probes.py INPUT.pbf --output source-probes.json`) for PBF-derived
named-point controls.
The removed Tactical profile's annotation companion archive was not MVT. Neither these controls nor normalized geometry comparison prove
all source fidelity, seam continuity or OMT cartographic equivalence.

## Engineering workflow

Choose one mechanism, record a baseline, add a regression fixture, change the
Rust library, rerun correctness checks, then repeat regional measurements.
Promote an optimization only when its output differences are explained and its
resource benefit survives repeats. Preserve Windows/macOS/Linux/iOS/Android
library boundaries; physical mobile memory and lifecycle qualification remain
separate from these desktop measurements.

[omt-target.json](omt-target.json) pins the exact reference source and records
all 16 layers as `implemented_experimental`, with remaining qualification gaps.
Per-run manifests bind PBF, supplemental inputs,
tools, output hashes and explicit settings; language-provider and cartographic
behavior still require qualification.

Run the small harness regression suite independently of measured compilations
(`scripts/check.sh` runs the Python part in `target/benchmark-venv`):

```powershell
python -m unittest discover -s benchmarks/compiler-parity -p "test_*.py" -v
cargo test --locked -p osm-tile-compiler --lib
cargo test --locked -p osm-tile-compiler --lib --features benchmark-profiling
```

Build `compile_omt` with `--features benchmark-profiling` to emit aggregate
`benchmark_total` and stage `benchmark_phase` JSON lines. Nested times overlap;
instrumented runs are diagnostic measurements, not optimized speed claims.
Worker timing totals overlap and may repeat per worker; sum matching counters
for work totals, not elapsed wall time.

### Windows memory counters

Each run retains handles for the launched process and discovered descendants and records `windows_process_memory.processes`. `os_peak_working_set_bytes` is the OS per-process lifetime peak of resident working-set pages (including shared pages); `os_peak_private_commit_bytes` is the OS lifetime peak private commit, not resident memory. `sampled_peak_private_commit_bytes` is only the largest queried current private commit. A final query occurs after exit, before closing retained handles. A short allocation/release/exit test verifies that OS counters can exceed the sampled RSS peak.

The existing `sampled_peak_tree_rss_bytes` remains a simultaneous sampled sum, subject to missed spikes and shared-page double counting. Never sum per-process lifetime peaks and call that peak tree RSS: processes may peak at different times. Descendants that start and exit between discovery samples can still be missed. Non-Windows runs retain psutil sampling and report Windows counters unavailable. Query failures appear in `sampling_errors` or the Windows counter error list; missing counters are absent, not zero. `observed_process_memory_collection_complete` concerns query errors for observed processes, not a guarantee every descendant was observed. Memory limits still use sampled tree RSS.
