# Compiler changes

## 2026-11-17 — osm-tile-compiler, dependencies from crates.io

- The crate and binary are `osm-tile-compiler` (library `osm_tile_compiler`),
  and scratch directories are marked `.osm-tile-compiler-scratch`.
- OSM is read with `osm-pbf-ingest` 0.1 from crates.io; every dependency now
  comes from crates.io.
- Building-level archives name their format `building-levels` in the
  metadata. The debug variables are `OSM_TILE_COMPILER_CAPTURE_FILE`,
  `OSM_TILE_COMPILER_GRAPH_DUMP` and `OSM_TILE_COMPILER_BUILDING_STAGES`.
- Format versions start over at 1: the worker protocol is 1 (was 2), the
  manifest gains `format: "basemap-manifest"` and `formatVersion: 1`, and the
  supplemental caches' stamps lead with `v1`, so caches from earlier builds
  are rebuilt once. `docs/formats.md` lists every format and its version.
- Building levels are opt-in, as only renderers that draw distant buildings
  as boxes use them: `compile --building-levels`, `BasemapRequest::building_levels`
  (now `false` by default) and the worker request's `buildingLevels`. Overall
  progress counts their stage only when they are written.

## 2026-11-11 — Ingest from its own repository, fast-mvt, dependency fixes

- The OSM ingest crate comes from its own repository, with buffa block
  decoding (faster OSM reading); tiles unchanged.
- rusqlite 0.41 (bundled SQLite fix, RUSTSEC-2026-0459).
- Attribute group keys are written without bincode, byte for byte as before.
- Tiles are encoded with fast-mvt instead of the in-tree codec, and
  building levels read the basemap with its reader; the tiles are unchanged.

## 2026-11-03 — Areas crossing the antimeridian

- `--bounds W S E N` with W > E runs from W east to 180 and on from -180 to
  E, and so does a PBF header box with west above east or east past 180, in
  the CLI and the desktop worker. `OpenMapTilesRequest::bounds` takes the
  same, and `CompileBounds` has `wraps`, `normalized`, `longitude_ranges` and
  `width`.
- `--polygon` accepts rings whose longitudes run past ±180 and polygons split
  at ±180 (RFC 7946), and covers the tiles on both sides. The polygons' box is
  the narrowest longitude range holding them all, which wraps for polygons
  close to 180 on both sides.
- Features near 180 are written into the tiles on both sides, within their
  buffer. The supplemental datasets are filtered by both longitude ranges.
- A wrapped archive's header declares longitudes -180 to 180, which viewers
  read, with the area's latitudes and a center inside it. `verify` accepts
  wrapped header boxes.
- Output for areas that do not cross the antimeridian is unchanged.

## 2026-11-03 — Layer and language selection

- `compile --layers a,b` writes only those layers and `--exclude-layers a,b`
  all but those; the metadata's `vector_layers` lists only the layers written.
  Every source is still evaluated, and features of other layers are dropped
  before rendering, so each layer written equals the full build's. Leaving out
  `water` also skips reading the ocean polygons, which make only water.
- `--languages en,de` limits the `name:<lang>` attributes to those languages,
  with Planetiler's list syntax (`default`, `-code`, an optional `name:`
  prefix); unknown codes and `*` are usage errors. The other name attributes
  are unchanged.
- `OpenMapTilesRequest` has optional `layers` and `languages`, and the worker
  request optional `layers`, `excludeLayers` and `languages`. Without them the
  output is unchanged.

## 2026-11-01 — Building levels

- `building-levels <INPUT.pmtiles> <OUTPUT.pmtiles> [--threads <N>]` writes a
  second archive from a compiled basemap's z14 tiles, for drawing buildings too
  far away for polygons. z14 holds oriented boxes, clipped to their tile, and
  z12 and z11 hold merged rectangles of a coverage grid, with the buildings
  that stand well over their cell added whole. The bytes are the same for any
  number of threads. The format is in `src/building_levels.rs`.
- `compile`, and the desktop worker, run the pass last and write
  `<NAME>-buildings.pmtiles` beside `<NAME>.pmtiles` (so `basemap-buildings.pmtiles`
  beside the app's basemap) when the archive has a `building` layer and reaches
  z14. `compile` refuses to start if that file exists.

## 2026-10-29 — Large invalid polygons

Antarctica (south of 60°S) compiles z12 in 7 s instead of 282 s, with a
byte-identical archive, and all of z0–14 in about 2 minutes, which never
finished before. Małopolskie's archive is unchanged.

- When neither the stripe clip nor the repairs give a tile valid geometry, the
  last fallback intersects only the polygon's stripes for that tile with it,
  not the whole polygon. Large self-intersecting polygons, such as Antarctic
  ice shelves, paid for their full size in every such tile.
- A tile that no ring of a polygon reaches within its column is skipped before
  any clipping.

## 2026-10-28 — A MapLibre style

- `style <ARCHIVE_URL>` prints a MapLibre style for an archive: OSM Bright from
  OpenMapTiles (BSD 3-Clause code, CC-BY 4.0 design), checked in as
  `style/osm-bright.json`, with its source pointing at the archive through the
  PMTiles protocol and crediting OpenMapTiles and OpenStreetMap. Glyphs default
  to demotiles.maplibre.org and the sprite to OSM Bright's GitHub Pages site;
  `--glyphs` and `--sprite` replace them.

## 2026-10-28 — OpenMapTiles schema fixes

- The archive's `attribution` is Planetiler's HTML string, so MapLibre's
  attribution control links to the OpenMapTiles and OpenStreetMap copyright
  pages.
- `name:latin`'s fallback to other `name:*` tags tries them in Java `HashMap`
  order, as Planetiler does, instead of by key.
- Undisputed admin_level 2 boundary lines carry `adm0_l` and `adm0_r` from
  zoom 5: the ISO3166-1:alpha3 codes of the countries on each side, found from
  the country outlines once every OSM source has been read. As in Planetiler,
  a country whose border ways do not close (as in most regional extracts)
  gets no names. `disputed_name` now drops whitespace, " at " and "Extentof"
  as Planetiler does, and comes from the way only when the way is disputed.

## 2026-10-28 — Faster supplemental sources

Reading the supplemental sources for Małopolskie takes about 0.1 s instead of
2 s once the files are in the page cache, with a byte-identical archive.

- Shapefile records are filtered by the bounding box in their record header,
  read through the `.shx` index, so the 1.3 GB of planet-wide water polygons
  are no longer decoded and reprojected to find the few in the region.
- The row counts of the Natural Earth tables the profile skips (needed only
  to number features as Planetiler does) are cached in
  `natural_earth_vector.sqlite.geometry-counts` beside the extracted database.

## 2026-10-28 — Failed and interrupted compiles

- The archive is written to `<output>.<pid>.partial`, locked while it is
  written, and moved into place. A failed compile removes it; one that cannot
  be moved into place is kept and named in the error. A later compile removes
  the unlocked ones killed compiles left.
- The scratch directory is locked for the whole compile, so a second compile
  given the same one is refused, and a tile sort directory a killed compile
  left behind is replaced instead of refused.
- The command line's default `<output>.tmp` carries a marker file, locked for
  the whole compile: one left by an interrupted compile is reused, one in use
  is refused.
- A panic in worker mode is reported as a JSON `error` event.

## 2026-10-26 — Pinned sources, warnings in the result

- The lake centerlines and Natural Earth downloads must match pinned SHA-256
  hashes (the inputs of the Planetiler reference runs); a mismatch fails the
  download instead of publishing the file. The water polygons are regenerated
  daily upstream, so only their actual hash is recorded.
- The library no longer prints to stderr. Its `OMT: …` stage lines duplicated
  the progress callback and are gone; problems that do not stop a compile
  (skipped incomplete geometries, an extraction cache that cannot be written)
  are returned in the new `OpenMapTilesResult::warnings`, which the command
  line prints after its summary. `benchmark-profiling` builds still print
  their JSON lines.

## 2026-10-26 — Command line

The compiler is now a command-line tool as well as the desktop app's
worker.

- `compile <INPUT.pbf> --output <FILE.pmtiles> --sources <DIR>` builds a
  basemap. The bounds default to the PBF header's bounding box, zooms to 0–14,
  threads to every core and scratch space to `<FILE>.tmp`, removed afterwards.
  `--fetch` downloads missing supplemental datasets first; `--json` prints the
  compile result on stdout. Progress and a closing summary go to stderr.
- `fetch-sources <DIR>` downloads, or verifies, the three supplemental datasets.
- `--help` and `--version`; usage errors exit with 2, failures with 1.
- Invocations starting with `--request` or `--parent-stdin` still run the
  worker protocol, unchanged. Running the binary with no arguments, or with an
  unknown first argument, now prints a usage error on stderr instead of a JSON
  `error` event.

## 2026-10-10 — Smaller sorter spills

The external tile sort now spills about 0.56 GB for Małopolskie instead of
2.5 GB, and its run count no longer grows with the thread count.

- Each render worker sorts with a fixed 8 MiB budget; intermediate merges, when
  a region exceeds the merge fan-in, run their groups in parallel.
- Sort records use a compact internal format: varint frame headers, one-byte
  layer and known-attribute-key ids, zigzag-delta geometry and in-record repeat
  references for duplicated name strings. Archives are unchanged.

## 2026-10-10 — Compiler performance

Małopolskie (10-core Mac, 10 threads) now compiles in about 11 s instead of
61 s, with a byte-identical archive.

- The compiler binary uses every available core instead of four.
- OSM way and relation assembly, profile evaluation and rendering run on worker
  threads; each worker sorts its own tile records, and a (source, feature,
  record) sequence key keeps the previous output order. Spool files, serial
  replay and render batch barriers are gone.
- Tile encoding streams through long-lived workers and writes tiles in order,
  replacing barrier-separated batches.
- Polygon and line geometry is clipped once per tile column instead of once
  per tile; tiles a feature cannot reach skip the clip/repair cascade.
- Polygon validity is decided with segment sweeps and point-in-ring tests,
  deferring to geo only for point contacts and collinear rings.
- Relation ring nesting no longer runs a full containment test for every ring
  pair, and Natural Earth lookups use precomputed coordinates and bounds.
- Supplemental archives are extracted once into `<sources>/.extracted/` and
  reused while the ZIP's size and mtime are unchanged (about 2.4 GB on disk).
- The compiler binaries use mimalloc.
- The ingest crate's `Pass2Sink` takes batches by value.

## 2026-10-09 — OpenMapTiles is the only profile

Removed the legacy tactical (v7) profile, its examples and the orphan
modules that were never compiled. OpenMapTiles is now the only compiler profile.

- Removed the tactical driver (`compile_basemap`, `CompileRequest`,
  `CompileResult`, `PublishedSchema`, `BuildProfile`, `PublicationPlan`) and the
  `tactical`, `schema`, `shared_ingest`, `annotations`, `multipolygon`,
  `road_width`, `ocean`, `label_point`, `tile_postprocess` and `bench_support`
  modules, plus the unused `node_store`, `relation_index` and `filled_ranges`
  files.
- Removed the `compile_region`, `bench_compile`, `dump_schema`, `inspect_chunks`,
  `inspect_spool`, `inspect_archive`, `validate_annotations`, `validate_archive`
  and `probe_pbf` examples, the `compiler_bench` benchmark, the `parity_gate`
  test, `schema/tile-schema.json` and the `bench-support` feature. `compile_omt`
  and the `benchmark-profiling` feature remain.
- Trimmed the shared `chunk_store`, `generalization` and `tile_slicer` modules to
  what the OpenMapTiles path uses and dropped `compact_attrs` and
  `binary_layout`. OpenMapTiles output is unchanged.
- Dropped dependencies that only the tactical profile used.

## 2026-09-30 — Native OpenMapTiles compiler

Merged the native OpenMapTiles compiler and its performance and correctness
improvements into main. The existing tactical profile remained available at the
time (removed 2026-10-09).

- Added an experimental OpenMapTiles 3.16 profile for all 16 layers, pinned to
  Planetiler 0.10.2 and profile revision
  `91516fdf477915b1a985015811049b9b96c7f87e`.
- Exposed the in-process `omt::compile_openmaptiles(&OpenMapTilesRequest)` API
  and a `compile_omt` example for local PBF and supplemental data inputs.
- Added bounded parallel rendering and encoding, disk-backed intermediate
  records, spool reuse, and reduced geometry, allocation, and decoding work.
- Improved source-coordinate precision, relation assembly, simplification,
  clipping, polygon hole ownership, topology repair, and line merging.
- Corrected label placement and density grids, feature thresholds, indoor and
  numeric attributes, and near-closed relation rings that previously lost
  woodland coverage or holes.
- Added bundled ICU4X script-to-Latin fallback while preserving explicit Latin
  names. Unsupported scripts remain unqualified; this is not full Any-Latin
  equivalence.
- Added regression fixtures, pinned Java development oracles, archive comparison,
  bounded polygon-impact diagnostics, timing instrumentation, and Windows OS
  peak-memory counters alongside sampled process-tree RSS.

Compilation requires no Java, GEOS, or external GIS process at runtime. The
library is intended for local embedding; physical mobile builds and resource
limits still require platform-specific validation.

The profile remains experimental: full semantic parity with Planetiler is not
claimed. Differences remain in polygon precision, representative feature IDs,
and language-provider coverage. Matching a reference geometry omission is not
automatically a quality improvement. Experimental floating-point geometry
modules are test-only and do not replace the production geometry path.

See the [comparison harness](../benchmarks/compiler-parity/README.md)
for reproducible validation and the
[profile contract](../benchmarks/compiler-parity/omt-target.json)
for pinned policies and remaining qualification gaps. Historical iteration
reports were removed; regression fixtures and reusable tooling are retained.
