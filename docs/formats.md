# Formats and versions

The formats the compiler writes, and the version each one carries. Each
version is a named constant, and a test pins it.

## When a version changes

Bump a format's version only when a reader of the old version would misread
the new output: a field removed, renamed or given a new meaning, or a layout
changed. A new optional field doesn't need a bump, so readers ignore fields
they don't know. Note every bump in `CHANGELOG.md`.

## Formats this project owns

| Format | Where it is | Version now | Constant | Pinned by |
|---|---|---|---|---|
| Worker protocol | The request JSON a host passes with `--request` (which `compile --request` also reads), and the JSON lines on stdout, each with `protocolVersion`; error events carry `errorKind`, one of `CompileError::kind`'s identifiers | 1 | `PROTOCOL_VERSION` (`src/main.rs`) | `tests/cli.rs` |
| C ABI | `osm-tile-compiler-ffi`: the header's functions and `OTC_*` numbers, and the envelope (`abiVersion`), progress, result and error JSON; see `docs/c-abi.md` | 1 | `ABI_VERSION` (`osm-tile-compiler-ffi/src/lib.rs`), `OTC_ABI_VERSION` (the header) | `osm-tile-compiler-ffi/tests/header.rs` |
| Basemap manifest | `BasemapOutput::manifest()`, written by the worker to the request's `manifestPath`; `format` is `basemap-manifest` | `formatVersion` 1 | `MANIFEST_FORMAT`, `MANIFEST_VERSION` | `tests/formats.rs`, `tests/embedding.rs` |
| Building-level archives | The PMTiles metadata `{"format":"building-levels","version":1,"source_layer":"building"}` | 1 | `BUILDING_LEVELS_FORMAT`, `BUILDING_LEVELS_VERSION` | `tests/formats.rs`, `src/building_levels.rs`, `tests/building_levels.rs` |
| Search indexes | `<NAME>-search.idx` beside an archive: the magic `OTCSRCH\0`, a `u32` version, a section table, a JSON metadata section whose `format` is `search-index`, then fixed record rows, records, a dictionary and postings (`docs/search.md`). The text folding is part of the format | 1 | `SEARCH_INDEX_FORMAT`, `SEARCH_INDEX_VERSION` | `tests/formats.rs`, `tests/search.rs` |
| Tile schemas | The YAML schemas `--schema` reads: Planetiler's custom map format, the subset in `docs/schemas.md`. An archive made with one records `schema` and `schema_digest` in its metadata | Planetiler's format, no version of its own | | `tests/schemas.rs`, `src/omt/schema/` |
| SAR layer schema | The `sar` layer's classes, subclasses, attributes and zooms (`docs/sar.md`), and `sar_schema` in the metadata of an archive that has it | 1 | `SAR_SCHEMA_VERSION` (`src/omt/sar.rs`) | `tests/sar.rs`, `src/omt/sar.rs` |
| Data layer configuration | The JSON form of `DataLayer` and `DataInput` that `--data-layers`, the C ABI's and worker's `dataLayers` and Python's `data_layers` take (`docs/data-layers.md`), and the `data_layers` list in the metadata of an archive that has data layers (`id`, `file`, `format`, `features`) | none yet: fields are only added, and unknown ones are refused | `DataLayer`, `DataInput` (`src/data_sources/mod.rs`) | `src/data_sources/tests.rs`, `tests/data_layers.rs` |
| Tree archives | The PMTiles metadata `{"format":"trees","version":2,...}`, the tiles' columns (`src/trees.rs`), their stands' cells (`src/stands.rs`) and their lines of rows and hedges (`src/trees.rs`; version 1 had no lines, and a row's trees as trees). `TREE_GENERA` grows at its end without a bump: a reader takes a genus it doesn't know as unknown | 2 | `TREES_FORMAT`, `TREES_VERSION` | `tests/formats.rs`, `src/trees.rs`, `src/stands.rs` |
| HD road archives (experimental) | The PMTiles metadata `{"format":"hd-roads","version":1}`; the layers `surface` and `marking` at z16, `road`, `fence` and `thing` at z15, `structure`, `paint`, `barrier` and `portal` at z14, described in `src/hd_roads.rs` | 1 | `HD_ROADS_FORMAT`, `HD_ROADS_VERSION` | `tests/formats.rs`, `tests/hd_roads.rs` |
| Delta archives (experimental) | The files `diff` writes and `apply` reads: the magic `OTCDELTA`, a `u32` version, a JSON manifest whose `format` is `osm-tile-compiler-delta`, then copy, literal and tile patch operations, then (version 2) a footer of ed25519 signatures ending in the magic `OTCSIGNS` (`src/archive/delta.rs`, `src/archive/tile_patch.rs`). Version 1 deltas, which have no footer, still apply, as unsigned deltas | 2 | `DELTA_FORMAT`, `DELTA_VERSION` | `tests/formats.rs` |
| Signature files | `<FILE>.sig` beside a signed archive or other file: JSON whose `format` is `osm-tile-compiler-signature`, with the file's `bytes` and `sha256` and its ed25519 `signatures` (`src/archive/signing.rs`). Signing keys are 64 hex digits, a secret key's seed or a public key | 1 | `SIGNATURE_FORMAT`, `SIGNATURE_VERSION` | `tests/formats.rs` |
| Tile lists | `update --changed-tiles` and `diff --changed-tiles`: one `z/x/y` per line, rows counted from the north, as osm2pgsql's expire lists are written (`src/tile_list.rs`) | none: the format is osm2pgsql's | | `src/tile_list.rs` |
| Merge caches (experimental) | `<NAME>.merge-cache` beside an archive compiled with `--merge-cache`: the magic `OTCMRGC1` (its version), the build's code fingerprint, the gzip level, the archive's length and header, then a zstd stream of tile entries (`src/omt/merge_cache.rs`). An index from another build is ignored with a warning, so the fingerprint stands in for a version | `OTCMRGC1` | `MAGIC`, `FINGERPRINT` (`src/omt/merge_cache.rs`) | `tests/cli.rs` |
| Supplemental caches | `<sources>/.extracted/<archive>/.complete` and `<database>.geometry-counts`, each led by a stamp `v<version> <size> <mtime-ns>` | 1 | `CACHE_VERSION` (`src/omt/supplements.rs`) | its unit test |
| Benchmark reports | `format_version` in the harness's JSON reports and manifests | 1 | in each script | none (internal tooling) |

A cache whose stamp doesn't match, because of an older version or a changed
file, is rebuilt rather than read.

The worker rejects request fields it doesn't know. A host that sends a new
field therefore needs a worker that knows it, which holds when they ship
together. The protocol version guards the stdout events and the meaning of
existing fields.

The command line's `--json` output (`compile`, `inspect`, `verify`) carries
no version. It follows the crate's semantic versioning.

Scratch directories (`.osm-tile-compiler-scratch`) are removed after every
compile, so they carry no version.

## External versions

These belong to other projects and follow them:

| What | Version | Where |
|---|---|---|
| OpenMapTiles schema | 3.16.0 | `OMT_SCHEMA_VERSION`; written as the archive metadata's `version` and the manifest's `version`, as OpenMapTiles does |
| Planetiler OpenMapTiles profile | commit `91516fdf` | `omt::PROFILE_REVISION`; written as `planetiler_openmaptiles_profile_commit` |
| This crate | its version | written as the archive metadata's `generator`, `osm-tile-compiler <version>`; `tests/monaco.rs` records it as `osm-tile-compiler <version>` so a release doesn't change the fingerprint |
| OSM replication | the extract's header | written as `osm_data_timestamp` (RFC 3339, UTC), `osm_replication_sequence` and `osm_replication_url` when the header gives them |
| Planetiler (comparison) | 0.10.2 | `benchmarks/compiler-parity/` |
| PMTiles | specification v3 | the archive header; tile compression gzip (2), or none (1) with `--tile-compression none` (`tileCompression: "none"` in the worker request) |
| MBTiles | specification 1.3 | `application_id` 0x4d504258 and the `metadata` table of `--format mbtiles` archives |
| Mapbox Vector Tiles | specification 2.1 | every tile, unless the compile asks for MLT |
| MapLibre Tiles | specification v1, frame tag `0x01` | every tile of a `--tile-format mlt` compile (`tileFormat: "mlt"` in the worker request and the C ABI, `tile_format="mlt"` in Python): PMTiles tile type 6, metadata `format` `mlt`. Written by our own encoder (`src/mlt/`) from each finished MVT layer, so the features are the MVT ones, in the plain encodings only. Checked against the specification's fixtures and, in CI, decoded by mlt-core, @maplibre/mlt and MapLibre Native's C++ decoder ([mlt.md](mlt.md)) |
