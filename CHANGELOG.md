# Changelog

## Unreleased

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
