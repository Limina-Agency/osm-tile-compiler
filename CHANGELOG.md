# Changelog

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
