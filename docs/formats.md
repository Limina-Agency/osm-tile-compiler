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
| Worker protocol | The request JSON a host passes with `--request`, and the JSON lines on stdout, each with `protocolVersion`; error events carry `errorKind`, one of `CompileError::kind`'s identifiers | 1 | `PROTOCOL_VERSION` (`src/main.rs`) | `tests/cli.rs` |
| C ABI | `osm-tile-compiler-ffi`: the header's functions and `OTC_*` numbers, and the envelope (`abiVersion`), progress, result and error JSON; see `docs/c-abi.md` | 1 | `ABI_VERSION` (`osm-tile-compiler-ffi/src/lib.rs`), `OTC_ABI_VERSION` (the header) | `osm-tile-compiler-ffi/tests/header.rs` |
| Basemap manifest | `BasemapOutput::manifest()`, written by the worker to the request's `manifestPath`; `format` is `basemap-manifest` | `formatVersion` 1 | `MANIFEST_FORMAT`, `MANIFEST_VERSION` | `tests/formats.rs`, `tests/embedding.rs` |
| Building-level archives | The PMTiles metadata `{"format":"building-levels","version":1,"source_layer":"building"}` | 1 | `BUILDING_LEVELS_FORMAT`, `BUILDING_LEVELS_VERSION` | `tests/formats.rs`, `src/building_levels.rs`, `tests/building_levels.rs` |
| Tree archives | The PMTiles metadata `{"format":"trees","version":1,...}`, and the tiles' columns (`src/trees.rs`). `TREE_GENERA` grows at its end without a bump: a reader takes a genus it doesn't know as unknown | 1 | `TREES_FORMAT`, `TREES_VERSION` | `tests/formats.rs`, `src/trees.rs` |
| Delta archives (experimental) | The files `diff` writes and `apply` reads: the magic `OTCDELTA`, a `u32` version, a JSON manifest whose `format` is `osm-tile-compiler-delta`, then copy, literal and tile patch operations (`src/archive/delta.rs`, `src/archive/tile_patch.rs`) | 1 | `DELTA_FORMAT`, `DELTA_VERSION` | `tests/formats.rs` |
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
| Planetiler (comparison) | 0.10.2 | `benchmarks/compiler-parity/` |
| PMTiles | specification v3 | the archive header |
| Mapbox Vector Tiles | specification 2.1 | every tile |
