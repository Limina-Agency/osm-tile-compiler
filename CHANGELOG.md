# Changelog

## 0.5.0 (2026-11-25)

### Changed

- MapLibre Tile (MLT) output (`--tile-format mlt`) is supported, no longer
  experimental. It is read by MapLibre GL JS 5.12, MapLibre Native Android
  13.4 and iOS 6.28 and later; [docs/mlt.md](docs/mlt.md) says what is
  written and why older MapLibre Native draws polygon edges a few levels
  off. `--search-index` and `--building-levels` now work with MLT: both read
  the tiles through an MLT-to-MVT transcode, and on Berlin and Monaco the
  side files are byte for byte those of the MVT basemap.
- The `data-sources` feature (see below) is on by default, and the C ABI and
  Python crates turn it on.

### New

- `--max-zoom` goes up to 16, as Planetiler's `--maxzoom` does
  (`ZoomRange::LIMIT`, `ZoomRange::up_to`, the C ABI's `zoom.max`, Python's
  `max_zoom`). The default stays 14 (`ZoomRange::DEFAULT_MAX_ZOOM`) and its
  output is byte for byte what it was. Above 14 the profile follows
  Planetiler: features with an open zoom range reach the top, the max zoom
  tolerance and minimum size apply at the top only, and lake labels without
  a centerline go on to the top. `extract` without a max keeps what the
  source holds.
- Serving with Martin (experimental, [docs/martin.md](docs/martin.md)): the
  new `martin-config` command (`martin::write_martin_config`) writes a
  Martin config that serves the archives, watching the directories given,
  and an OSM Bright style per archive pointing at Martin's TileJSON, with MLT
  encoding and the `sar` layers where they apply.
- `apply --in-place` (`archive::apply_in_place`) writes the new archive
  beside the old one and renames it over the old one only after the delta's
  signature and the result's SHA-256 are checked, so a tile server reading
  the archive sees the old archive or the new one, never a mix.
- `--tile-compression none` (`ArchiveOptions::tile_compression`,
  `TileCompression`, `tileCompression` in the worker request) stores PMTiles
  tiles uncompressed, for Martin 2.0.0-beta.7, which refuses gzipped MLT
  archives. The tiles are the gzipped ones unzipped; on Berlin (MLT) the
  archive is 54.4 MB against 33.3 MB gzipped. PMTiles only, and not with the
  merge cache.
- C ABI: `compile_basemap` and `write_style_pack` take `tileFormat`. Python's
  `compile` takes `tile_format` and `search_index`, and returns the index's
  path.
- `scripts/check-mlt-conformance.sh` decodes an MLT compile's tiles with
  mlt-core, @maplibre/mlt and MapLibre Native's decoder and compares them
  with the features the tiles hold (CI runs it on Monaco), and
  `scripts/check-mlt-spec.sh` checks the vendored MLT specification fixtures
  (416 of them) against the specification's main branch (nightly).
- Tile schemas besides OpenMapTiles ([docs/schemas.md](docs/schemas.md)):
  `compile --schema shortbread` writes Shortbread, from the `shortbread.yml`
  Planetiler 0.10.2 ships, and `--schema FILE.yml` a schema in Planetiler's
  custom map YAML. On Berlin the Shortbread archive has the same 885 tiles
  and, in all 26 layers, the same features, attributes and geometry as
  Planetiler's, in 6.9 s against 35 s; its 67 specification examples pass.
  `schema verify FILE.yml` runs a schema's examples and `schema layers`
  lists its layers. The library takes `TileSchema` (`Schema::from_file`,
  `Schema::from_yaml`, `shortbread()`), the worker request and the C ABI a
  `schema` field (the C ABI also `{"yaml": "..."}`), and Python a `schema`
  argument. Only the supplemental datasets a schema reads are used; the
  archive's metadata carries the schema's name, description, attribution,
  `schema` and `schema_digest`. `BasemapRequest` and `BasemapOutput` gain
  `schema`.
- The engine renders through a `Profile` trait, with OpenMapTiles as one
  profile; OpenMapTiles archives are byte for byte as before.
- Data layers: a compile can write layers of other vector data beside the
  basemap, from GeoJSON, newline-delimited GeoJSON, CSV/TSV (coordinate
  columns, WKT or hex WKB), Shapefile, GeoPackage, FlatGeobuf and GeoParquet
  files, all read in pure Rust. Coordinates are reprojected to WGS84 from
  what each file declares, or from an input's `crs` (any EPSG code, PROJ
  string or WKT, through proj4rs). Each layer has its zooms, a choice and
  renaming of attributes, an id attribute and an attribution, and may add to
  a profile layer such as `poi`. `BasemapRequest::data_layers` with the new
  `DataLayer` and `DataInput` types (serde, for configuration files);
  `DataInput::read` hands a file's features to any caller, independent of
  tiles. The command line takes `compile --data LAYER=FILE[,KEY=VALUE...]`
  and `--data-layers FILE.json`, and the new `data-info` command describes a
  file. The C ABI's `compile_basemap` takes `dataLayers`, worker requests
  `dataLayers` and Python's `compile` `data_layers`. The archive's metadata
  lists the layers in `vector_layers` and their sources in `data_layers`;
  `CompileResult::data_layers` reports what each read. See
  [docs/data-layers.md](docs/data-layers.md).
- Crate features `flatgeobuf`, `geoparquet` and `reproject`, together the
  default `data-sources`. GeoJSON, CSV, Shapefile and GeoPackage in WGS84 or
  Web Mercator need none of them.
- [docs/comparison.md](docs/comparison.md) compares osm-tile-compiler 0.4.0
  with Planetiler, tilemaker, Tippecanoe, the OpenMapTiles tools and Martin.

### Deprecated

- `ZoomRange::MAX_ZOOM`: the highest zoom is no longer fixed. Use
  `ZoomRange::DEFAULT_MAX_ZOOM`, or `ZoomRange::LIMIT` for the highest
  allowed.

### Fixed

- `serve` accepts each connection at once. It used to sleep 100 ms whenever
  no connection was waiting, so clients that open a connection per request
  got about 80 tiles/s with 8 clients on Berlin; they now get 1,867 tiles/s,
  as with Martin.

## 0.4.0 (2026-11-24)

### Breaking

- Deltas are format version 2 (`DELTA_VERSION`): after the end operation
  comes a footer of ed25519 signatures, ending in the magic `OTCSIGNS`. This
  build still applies version 1 deltas, as unsigned deltas, but an older
  build cannot apply a version 2 delta.
- `archive::ApplyReport` gains `signed_by` and `trusted`, and
  `archive::DiffReport` gains `signed_by` and `changed_tiles`.
  `UpdateRequest` gains `changed_tiles` and `UpdateReport` gains
  `changed_tiles`, so code that builds either with a struct literal needs the
  new field (`UpdateRequest::new` sets it to `None`).
- `CompileStage` has a new `SearchIndex` stage, after `BuildingLevels`, and
  `CompileStage::ALL` lists eight stages. Like `FetchSources` it is not part
  of `overall`.
- `ExtractReport` gains `requests` and `bytes_read`.
- Depends on osm-pbf-ingest 0.7.0.

### New

- An offline search index beside the archive: `compile --search-index`
  (`BasemapRequest::search_index`) and the new `search-index` command write
  `<NAME>-search.idx`, the names of the archive's highest zoom: places, POIs,
  streets, peaks, lakes, rivers and the `sar` layer's huts, springs and
  emergency points (also by ref), each with the nearest city, town or
  village as its context. `search` and `SearchIndex::open`/`search` match
  words regardless of case and diacritics (`fold_search_text`: Łódź = lodz),
  the last word as a prefix, and rank by prominence, whole-name match and
  distance from an optional point, within an optional box and layers. On
  Małopolskie the index is 4.9 MB beside a 153.7 MB archive, built in 2.5 s,
  and queries take 0.3 to 2 ms. The format (version 1) is in
  [docs/search.md](docs/search.md). PMTiles and MVT archives only.
- Python bindings: the new `osm-tile-compiler-python` crate builds the
  `osm_tile_compiler` module (PyO3, abi3 wheels for CPython 3.10 and later,
  with maturin) over compile, extract, update, diff, apply,
  read_delta_manifest, inspect and verify. Every call releases the GIL;
  compile and update report progress to a callable, and compile, extract
  and update stop on a `CancelToken`, a callable or Ctrl-C. Errors are an
  exception tree under `osm_tile_compiler.Error` with stable `kind` and
  `stage`. CI builds and tests the wheels on Linux, macOS and Windows; they
  are not on PyPI yet.
- C ABI: new job kinds `extract`, `update`, `diff`, `apply` (with
  `trustedKeys`), `inspect`, `verify` and `verify_delta`, which do what their
  commands do and return the same reports, and `fetch_sources` and
  `fetch_extract`, which download a compile's supplemental datasets or a
  Geofabrik region ahead of it with byte progress. `otc_validate` runs
  `otc_job_start`'s checks without starting a job. `compile_basemap` takes
  `resume`. See [docs/c-abi.md](docs/c-abi.md).
- Experimental MapLibre Tile (MLT) output: `compile --tile-format mlt`
  (`ArchiveOptions::tile_format`, `TileFormat::Mlt`, `tileFormat: "mlt"` in
  the worker request) writes every tile as MLT instead of MVT, in PMTiles
  (tile type 6) or MBTiles (metadata `format` `mlt`). The default stays MVT,
  so parity and the Monaco fingerprint don't change. Our own encoder
  transcodes each finished MVT layer, so an MLT tile holds exactly the MVT
  tile's features; the MLT crates are still changing too fast to depend on.
  Berlin's archive is about 20% smaller (33.3 MB against 41.9 MB, both
  gzipped), and MapLibre GL JS 5.24 and MapLibre Native 6.4 render it the
  same as the MVT one. `style --tile-format mlt` (and
  `StylePackRequest::tile_format`, `with_tile_format`) sets the source's
  `encoding` to `mlt`, which MapLibre needs to read the tiles. `verify` and
  `inspect` decode MLT tiles, and `serve` serves them as `.mlt` with the
  source's `encoding` set. Building levels and the C ABI request don't take
  MLT yet, and a merge cache from an archive of the other tile format is not
  reused.
- Resumable compiles: `BasemapRequest::resume` and `compile --resume`. A
  resumable compile records the stages it completes in its scratch
  directory (the merged extracts, the sorted tile features once every source
  is read, each pass of the sort's merge, the archive and the building
  levels) and keeps them when it is killed, crashes, fails or is cancelled;
  the next resumable compile of the same request continues from the last
  one, and writes the same archive byte for byte. A checkpoint for a changed
  request or changed inputs is discarded with a warning.
  `BasemapOutput::resumed_from` names the stage a compile continued from;
  `checkpoint_directory` and `discard_checkpoint` find and remove a
  checkpoint. The desktop worker's request takes `resume` too.
- `serve` runs a small local HTTP server for previews: each PMTiles or
  MBTiles archive's tiles, TileJSON 3.0.0, MapLibre style and a map page,
  plus the glyphs and sprite of an offline style pack (`--style-pack`), so a
  browser or a phone on the same network (`--host 0.0.0.0`) draws the map
  with no other network. From Rust, `TileServer` and `ServeRequest`.
- `extract` reads a remote archive: give it an `http://` or `https://` URL
  instead of a file, on any server that answers range requests. Only the
  header, the root directory, the leaf directories that may address kept
  tiles and the kept tiles are downloaded, nearby tiles in one range and
  eight ranges at a time; the output is the same, byte for byte, as from a
  local copy. A server that ignores ranges, or an archive that changes
  while it is read, is an error and nothing is written. From Rust,
  `ExtractRequest::remote` (the new field `ExtractRequest::source_url`) and
  `archive::is_remote_source`. `ExtractReport` gains `requests` and
  `bytes_read`. Local extracts skip the leaf directories outside the area
  too.
- `compile --request <request.json>` runs a worker request from the command
  line, with its progress printed as `compile` prints it, and writes the
  request's manifest. Only `--threads` and `--json` go with it.
- `completions <bash|zsh|fish>` prints a shell completion script for the
  commands, their options and the values the tool knows (formats, layers,
  datasets, replication sources).
- Signed deltas, so a device can tell an update came from someone it trusts
  before applying it. `keygen FILE.key` writes an ed25519 key and
  `FILE.key.pub`; `diff --sign FILE.key`, `compile --delta-from OLD
  --sign-delta FILE.key` and `sign FILE.delta --key FILE.key` sign a delta
  in its footer, beside any other signatures; `apply --trusted-key KEY`
  (repeatable, 64 hex digits or a `.pub` file) refuses a delta without a
  valid signature by a trusted key before it writes anything, and checks the
  signed bytes again as it applies them. Without `--trusted-key` an unsigned
  delta applies as before, but one whose signature doesn't match never does.
  Each signature signs the SHA-256 of everything before the footer,
  manifest included, so it vouches for both archives' digests and every
  operation. From Rust: `archive::{PublicKey, SecretKey, diff_with,
  DiffOptions, apply_trusted, verify_delta, sign_delta}`.
- Signed archives: `sign FILE --key FILE.key` on anything other than a
  delta writes a detached signature file, `FILE.sig` (JSON, format
  `osm-tile-compiler-signature` version 1), and `verify-signature FILE
  [--trusted-key KEY]...` checks a delta's signatures or a file's `.sig`
  (`archive::{sign_file, verify_file_signature}`).
- `update --changed-tiles FILE` also writes the tiles the changes reach,
  one `z/x/y` per line as osm2pgsql's expire lists are, at zooms 0 to 14 or
  `--min-zoom`/`--max-zoom`: the boxes of every changed object before and
  after, of ways whose node moved and of multipolygons and boundaries whose
  member changed, widened by 64 pixels (`CHANGED_TILE_BUFFER_PIXELS`) for
  labels and cut to the extract's box. On Małopolskie with 13 hours of
  planet diffs it listed 5,112 tiles, every one of the 278 a recompile
  changed among them. The boxes come from osm-pbf-ingest's
  new `changes::changed_area`. `UpdateRequest::changed_tiles`
  (`ChangedTilesRequest`) does the same from Rust, and the report's
  `changed_tiles` says how many tiles at each zoom.
- `diff --changed-tiles FILE` writes the tiles that differ between the two
  archives (changed, added or removed), in the same format.
- `scripts/build-xcframework.sh` builds the C ABI as an iOS XCFramework
  (`aarch64-apple-ios` and `aarch64-apple-ios-sim`, header and module map).
- `ios/DeviceTest`, a SwiftUI app that compiles a region on an iPhone
  through the C ABI, writes its style pack, renders it with MapLibre Native,
  and reports time and peak memory per run.

## 0.3.0 (2026-11-23)

### Breaking

- `CompileStage` has a new `MergeExtracts` stage, between `FetchSources`
  and `ReadSources`, and `CompileStage::ALL` lists seven stages. Hosts that
  match on stages or index progress by them need the new one; like
  `FetchSources` it is not part of `overall`.
- The archive metadata gains `generator`, `osm_data_timestamp`,
  `osm_replication_sequence` and `osm_replication_url` (see below), so
  archives of the same data are no longer byte-identical to 0.2.0's. Every
  tile is the same.

### New

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
- Several extracts compile as one (osm-pbf-ingest 0.6.0): `compile
  a.osm.pbf b.osm.pbf` (and `BasemapRequest::extra_inputs`, `extraInputs` in
  the C ABI request, `extraInputPbfPaths` in the worker request) merges them into one sorted
  file in the scratch directory first, with osm-pbf-ingest's `merge_files`,
  in a new `merge_extracts` stage (`CompileStage::MergeExtracts`, between
  `fetch_sources` and `read_sources`; like `fetch_sources`, not part of
  `overall`). Shared objects are read once, from the extract with the newest
  replication timestamp. Compiling Małopolskie and Pomorskie this way gives
  the same tiles as compiling the file `osmium merge` writes.
- MBTiles output: a compile writes MBTiles 1.3 when `--output` ends in
  `.mbtiles` or `--format mbtiles` is given (`BasemapRequest::format`,
  `"format"` in the C ABI request), and PMTiles otherwise. The tiles and
  metadata are the same as the PMTiles archive's, and identical tiles of up
  to 1 KiB are stored once. Building levels, `--delta-from` and the merge
  cache need PMTiles and refuse MBTiles.
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
- Opt-in search-and-rescue layer `sar` (schema version 1, `docs/sar.md`):
  shelters, water, emergency points, landing sites, hazards (power lines,
  cliffs, caves, mine shafts) and trails graded by `sac_scale`,
  `trail_visibility` and `via_ferrata_scale`, at zooms 10 to 14 without a
  label grid. `compile --sar`, `BasemapRequest::sar` or `"sar": true` in the
  C ABI request write it into the archive, and the metadata then carries
  `"sar_schema": 1`; `--layers`/`--exclude-layers` take `sar` too. The
  OpenMapTiles layers are byte-identical with or without it. `style --sar`
  (also with `--offline`, `StylePackRequest::sar`) adds layers that draw it,
  and `with_sar_layers` adds them to a style from Rust. `SAR_LAYER` and
  `SAR_SCHEMA_VERSION` are public.
- Experimental merge cache for recompiles after an update: `compile
  --merge-cache` writes `<NAME>.merge-cache` beside the archive, and
  `--reuse-from OLD.pmtiles` takes every tile layer whose input records are
  unchanged from that archive instead of merging and encoding it again. The
  archive is byte-identical to an uncached compile. From Rust,
  `BasemapRequest::merge_cache` (`MergeCacheOptions`), and
  `CompileResult::merge_cache` reports what was reused.
- `compile --profile FILE.json` and `building-levels --profile` measure each
  stage's wall time and CPU time, and on Linux CPU per thread group and peak
  RSS, plus timed work inside stages (render, queue waits, spills, tile
  encoding, archive writes). The report is printed as a table and written as
  JSON; library users get it from `stage_profile::start` and `finish`.
  Nothing is measured unless a profile runs. `docs/performance.md` has the
  first profiles.

### Performance

- Large compiles spill far fewer sort runs and skip most intermediate merge
  passes. The render workers' sort buffers start at 8 MiB and grow, from the
  input PBF's size, only as far as the compile needs to end with about 96
  runs, up to half of the memory available when rendering starts;
  `compile --sort-memory <MIB>` and `BasemapRequest::sort_memory` set that
  cap. An intermediate pass merges only the smallest runs needed. Regional
  extracts are unchanged; Poland compiles in 385 s with a 2865 MiB peak on 2
  threads, against 487 s and 2994 MiB in 0.1.3.
- The final merge decodes sort runs ahead of the merge on up to `threads`
  worker threads, so the merging thread only orders records. With the
  encoders stubbed out, merging Małopolskie's runs takes 1.6 s instead of
  6.4 s on four threads; with real encoding the phase gains 3 to 12%.
- Merging nearby buildings is cheaper: proximity uses an early-exit segment
  scan (4.5 to 5 times faster, same answers as geo), records decode without
  owned string maps, the JTS port's noding allocates less, and clusters the
  area filter is bound to drop skip the union, unbuffer and buffering. Encode
  CPU is 10 to 23% lower on Berlin, Pomorskie and Małopolskie, and merge CPU
  6 to 13% lower, with byte-identical archives.
- Way and relation rendering: relation rings nest by one vertex when their
  boundaries are apart, line merging uses hash maps and compares edges in
  place, and a feature's attributes are converted once and encoded once per
  zoom instead of per tile.

### Fixed

- `--polygon` rejects a ring that encloses no area, naming it. Before, a
  ring whose points lie on one line next to a hole tripped an assertion in
  geo in debug builds and was accepted silently in release builds.
- Geofabrik's index gives some ISO codes as a plain string rather than a
  list, which made every `compile --area` fail; both forms are read now.

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
