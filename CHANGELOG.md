# Changelog

## Unreleased

### Added

- The archive metadata records what made the archive and how current its
  OSM data is: `generator` (`osm-tile-compiler` and its version), and from
  the extract's header `osm_data_timestamp` (RFC 3339, UTC),
  `osm_replication_sequence` and `osm_replication_url`, each left out when
  the header doesn't give it. `inspect` and the compile's summary line print
  them, and `CompileResult::osm_data` (`OsmDataInfo`) carries them, so the
  manifest's `result` does too. The Monaco fingerprint changes for these
  keys only; every tile is the same.
- `update` with change files on the command line takes the newest timestamp
  from the state files beside them (`124347.osc.gz` and
  `124347.state.txt`), so an extract brought forward from downloaded diffs
  says how current it is. Before, its header kept the base extract's time.
- `extract` cuts an area out of an existing PMTiles archive into a new one:
  the tiles within `--bounds`, `--polygon` and `--min-zoom`/`--max-zoom`,
  copied byte for byte, at the positions a compile limited to the same area
  writes. From Rust, `archive::extract` with an `ExtractRequest`.
- Offline style pack: `style --offline DIR` writes `style.json` with the
  glyphs of its three Noto Sans fonts and its icon sprite as local files,
  fetched once from `--glyphs` and `--sprite` (URLs, `file://` URLs or
  local paths), so that labels and icons draw with no network. It holds the
  Latin glyph ranges, every range the archive's labels use (read from a
  local archive, or `--archive-file`) and the scripts of `--languages`.
  The style names them relative to itself, or under `--base-url`. From Rust,
  `write_style_pack` (`StylePackRequest`, `StylePackReport`), and
  `maplibre_style` prints the plain style; from C, the `write_style_pack`
  job kind.

- Experimental merge cache for recompiles after an update: `compile
  --merge-cache` writes `<NAME>.merge-cache` beside the archive, and
  `--reuse-from OLD.pmtiles` takes every tile layer whose input records are
  unchanged from that archive instead of merging and encoding it again. The
  archive is byte-identical to an uncached compile. From Rust,
  `BasemapRequest::merge_cache` (`MergeCacheOptions`), and
  `CompileResult::merge_cache` reports what was reused.

### Changed

- Large compiles spill far fewer sort runs and skip most intermediate merge
  passes. The render workers' sort buffers start at 8 MiB and grow, from the
  input PBF's size, only as far as the compile needs to end with about 96
  runs, up to half of the memory available when rendering starts;
  `compile --sort-memory <MIB>` and `BasemapRequest::sort_memory` set that
  cap. An intermediate pass merges only the smallest runs needed. Regional
  extracts are unchanged; three regions merged into one 369 MiB PBF peak at
  728 MiB with 2 threads, against 876 MiB in 0.1.3.

## 0.2.0 (2026-11-19)

### Breaking: one entry point with a typed request

`compile_basemap` is the library's one compile entry point, and
`BasemapRequest` its typed request. To migrate:

- `omt::compile_openmaptiles`, `compile_openmaptiles_with_progress` and
  `OpenMapTilesRequest` are no longer public: build a `BasemapRequest` and
  call `compile_basemap`. `OpenMapTilesResult` is `CompileResult`.
- The `omt`, `sources`, `extracts` and `tile_stats` modules are private and
  their public items are re-exported at the crate root (`omt::LAYERS` is
  `OMT_LAYERS`, `omt::LANGUAGES` is `OMT_LANGUAGES`, `omt::PROFILE_REVISION`
  is `OMT_PROFILE_REVISION`).
- `BasemapRequest`: `min_zoom` and `max_zoom` are `zoom: ZoomRange`;
  `layers` and `exclude_layers` are `layers: LayerSelection` (`All`, `Only`
  or `Except` a list of `LayerName`); `bounds`, `polygon` and `area_only` are
  `area: Area`, with `polygon: Option<AreaPolygon>` (a file or a
  `CompileArea` in memory) and `read: ReadScope` (`WholeExtract`, or
  `NearArea` for the old `area_only`, which now needs bounds or a polygon).
- Public structs and enums are `#[non_exhaustive]`: use
  `BasemapRequest::new`, `ArchiveCenter::new`, `CompileBounds::new` and field
  assignment, and match enums with a wildcard arm.
- Every public fallible function returns `CompileError` instead of
  `String`, and `CompileError` no longer converts from `String`.
- `archive::delta::{diff, apply, read_manifest}` are
  `archive::{diff, apply, read_delta_manifest}`; `omt::select_layers` is
  `LayerSelection::resolve`.
- New: `BasemapRequest::validate` runs the checks that need no reading or
  network.

The command line and the worker protocol are unchanged, except that
`compile --area-only` now needs `--bounds` or `--polygon`.

### New

- A C ABI, the `osm-tile-compiler-ffi` crate (a staticlib and a cdylib, not
  on crates.io yet): `otc_job_start` takes a JSON request and runs it on a
  thread of its own, and the host polls progress, cancels and collects a
  JSON result. Its first request, `compile_basemap`, maps onto the 0.2
  request types. `docs/c-abi.md` describes it; CI builds it for iOS and
  Android, and iOS hosts need a deployment target of 13.0 or later.
- `compile --area NAME` downloads Geofabrik's extract of a region (by
  identifier, name with or without accents, or ISO code) into
  `--download-dir` and compiles it, reusing an extract already there. The
  library has the same as `find_extract` and `fetch_extract`.
- Experimental: `update` applies OSM change files, or every diff a
  replication server published since the extract, and writes the updated
  PBF without a database; planet diffs are clipped to the extract.
  `compile --changes` and `--replication` compile from the updated data, and
  `--delta-from OLD.pmtiles` writes the delta from the previous archive
  beside the new one. The library side is `update_extract`, on
  `osm-pbf-ingest` 0.4's change files.
- Ctrl-C, SIGTERM and SIGHUP cancel `compile`, `fetch-sources`, `--area`
  downloads and `building-levels` cleanly: scratch and partial outputs are
  removed and the exit code is 130. A second Ctrl-C quits at once. The
  handler is the binary's default `ctrlc` feature; the library never
  installs one.

### Performance

- Reading OSM takes less memory: pass 2 no longer decodes relations again
  (they come from pass 1), holds two decoded blocks per worker instead of
  16, and the render workers keep their sort buffers as frames with 32-byte
  sort entries instead of an 80-byte record and a heap block each. On Linux
  the binary has mimalloc commit memory on demand, so memory a compile
  frees leaves the RSS. Berlin on four threads peaks at 350 MiB instead of
  494 MiB in the same time, with the archive byte-identical.
- Sort runs are stored as columnar blocks, seven zstd streams per block: on
  Małopolskie they shrink from 225 to 168 MiB and peak scratch disk drops
  from 369 to 312 MiB, 1.7 times the extract. The archive is
  byte-identical.
- `building-levels` reads the basemap tile by tile instead of whole and
  keeps one grid per zoom open: on Małopolskie, peak RSS drops from 541 to
  195 MiB on four threads (310 to 88 MiB on one, 1179 to 544 MiB on 16), and
  the pass is about 15% faster. The output is byte-identical.
- `diff` and `apply` work on every thread: on Małopolskie with four cores,
  a delta that rewrites every tile takes 4.9 s instead of 17.1 s to diff and
  2.2 s instead of 6.9 s to apply. Deltas are byte-identical to 0.1.3's.
- Area-only reads drop the ways outside the area exactly before resolving
  them: central Kraków from the Małopolskie extract keeps 150 thousand of
  3.16 million ways instead of 1.16 million, and takes 2.40 s instead of
  2.63 s on four threads with `osm-pbf-ingest` 0.4. Archives are
  byte-identical.
- OSM is read with `osm-pbf-ingest` 0.4, which reads way groups straight
  from the wire.

### Fixes

- A GeoJSON `--polygon` with a too-short position, and PMTiles section or
  directory offsets that overflow, are errors instead of panics. Found by
  the new cargo-fuzz targets in `fuzz/`.
- A scratch directory's lock is released before its marker is closed, so a
  process spawned during a compile no longer keeps the next compile out.

### Other

- Our own overlay calls use `i_overlay` 9.2 (geo's still use 4.5).
- Parity with Planetiler 0.10.2 is verified on every tile of Małopolskie,
  Berlin and Pomorskie (`parity_verified` in the result), measured with
  `benchmarks/compiler-parity/pairdiff.py`.
- CI runs clippy and the tests on Windows and macOS, builds for Android and
  iOS, and reports public API changes with cargo-semver-checks on pull
  requests.

## 0.1.3 (2026-11-19)

- The disk node store (about twice the extract, 370 MiB for Małopolskie) is
  deleted as soon as OSM is read instead of when the compile ends, and on a
  failed or cancelled compile too. Peak scratch disk for Małopolskie drops
  from 1049 to 890 MiB (5.4 to 4.6 times the extract), and the peak moves
  from `write_tiles` to the end of `read_osm`. Time, memory and tiles are
  unchanged.
- The sort runs spilled to the scratch directory are compressed with zstd
  (level 1, in 64 KiB blocks), which makes them 2.4 times smaller. With the
  node store change, peak scratch disk for Małopolskie is 585 MiB, 3.0 times
  the extract. Compiles take under 1.5% longer on 2 threads and the same on
  4; the tiles are unchanged. The library now builds zstd's C sources
  (BSD-3-Clause).
- `compile --area-only` (`BasemapRequest::area_only`,
  `OpenMapTilesRequest::area_only`, `areaOnly` in the worker request) reads
  only the OSM objects in and near the area, so a small area compiles from a
  large extract in seconds. The tiles at the highest zoom are those of a full
  read; lower zooms show only what lies near the area. Off by default, which
  leaves every archive as it was. Code that builds `OpenMapTilesRequest`
  field by field needs `area_only: false`.

- OSM is read with `osm-pbf-ingest` 0.3, whose packed node store keeps the
  node locations of a file sorted by ID in 146 MiB instead of 370 MiB for
  Małopolskie, with no spill or sort. An area-only compile also skips the node
  blobs and drops the ways outside the area before decoding their tags. On
  four threads, an area-only compile of the Tatras takes 2.5 s instead of
  4.4 s, and the whole Małopolskie extract takes 26.4 s instead of 29.8 s,
  with peak memory 608 MiB instead of 895 MiB and peak scratch disk 666 MiB
  instead of 904 MiB. The tiles are unchanged.
- Experimental delta archives: `diff OLD.pmtiles NEW.pmtiles --output
  FILE.delta` writes what turns one archive into the next, and `apply
  OLD.pmtiles FILE.delta --output NEW.pmtiles` rebuilds the new one byte for
  byte. Both archives' SHA-256 are checked, and a failed apply leaves nothing
  behind. 11 hours of OSM edits to Małopolskie make a 63 kB delta for a
  153 MB archive.

## 0.1.2 (2026-11-17)

- OSM is read with `osm-pbf-ingest` 0.2, which no longer depends on
  `osmpbfreader` at runtime. The unmaintained `smartstring`, and `futures`
  0.1, `par-map`, `protobuf` and the crates under WTFPL and MPL-2.0 with it,
  are out of the binary; `deny.toml` needs no exceptions. The tiles are
  unchanged.

## 0.1.1 (2026-11-17)

- `CompileError` says what went wrong, and is `#[non_exhaustive]`:
  `InvalidRequest` (found before any work), `Download`, `OutOfSpace`, or
  `Failed { stage, message }`, with `kind()` and `message()`. Code that
  matched `Failed(message)` needs a change; 0.1.0 was out for a few hours.
- The worker's error events carry `errorKind`, one of those kinds.
- Release binaries for macOS (arm64 and x86_64) and Windows (x86_64), besides
  Linux.
- CI checks the declared minimum Rust (1.95), builds the library for iOS
  devices and the simulator, and runs `cargo deny`; its actions are pinned by
  commit.
- One changelog: this file. The crate no longer ships an internal
  development log.
- Documentation: what `panic = "abort"` means for the clean-up, which setup
  each README figure comes from, measurements on Apple silicon (macOS and the
  iOS simulator), and no references to pull requests or commits outside this
  repository.

## 0.1.0 (2026-11-17)

First public release: OpenMapTiles-schema basemaps from OSM PBF into PMTiles,
with a Planetiler comparison harness.

- Every format the compiler writes starts at version 1: the worker protocol
  (`protocolVersion` 1), the basemap manifest (`format` `basemap-manifest`,
  `formatVersion` 1), building-level archives and the supplemental caches.
  `docs/formats.md` lists them and when they change.
- Building levels are opt-in: `compile --building-levels`,
  `BasemapRequest::building_levels` or the worker's `buildingLevels` write
  them; by default a compile writes only the basemap.
- `compile_basemap` compiles inside an app's process: progress by stage
  with fractions, a cancel callback that stops every stage within about a
  second, a rayon pool of the request's threads, and clean-up of scratch
  and partial outputs. The command line and the desktop worker call it.
- The supplemental datasets and their download (`sources`,
  `fetch_sources`) are in the library, with progress and cancelling.
- A compile can leave out any supplemental dataset (`omit_sources`,
  `--omit-sources`, the worker's `omitSources`); the archive's metadata
  lists them as `omitted_sources`.
- mimalloc is the binary's allocator only, behind the default feature
  `mimalloc`.
- OSM is read with `osm-pbf-ingest` 0.1 from crates.io. Its buffa block
  decoding reads 1.2–1.3x faster in pass 1, and pass 2 about as fast as
  before; the tiles are unchanged (`fixtures/monaco-features.json`).
- rusqlite 0.41, whose bundled SQLite fixes RUSTSEC-2026-0459 (the Natural
  Earth dataset is an SQLite file read from a download).
- Tiles are encoded with fast-mvt instead of an in-tree codec.
  The tiles are unchanged: `fixtures/monaco-features.json` holds, and on
  Małopolskie fast-mvt rewound none of the 2.86 million polygon rings, so
  every geometry is written as before. Building levels read the basemap with
  fast-mvt's reader.
- The store's attribute group keys are written directly instead of through
  bincode, which is unmaintained (RUSTSEC-2025-0141); the bytes, and so the
  tiles, are the same.
