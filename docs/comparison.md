# Comparison with other tools

How osm-tile-compiler 0.4.0 compares with the tools most often used to make
vector tiles from OpenStreetMap, as of 2026-11-24. The other tools' versions
and features come from their release pages and READMEs on that date (see
[Sources](#sources)). The only performance numbers here are ones this project
measured; the other tools' own benchmarks are on their pages.

## Features

| | osm-tile-compiler 0.4.0 | Planetiler 0.10.2 | tilemaker 3.2.0 | Tippecanoe 2.79.0 (Felt) | OpenMapTiles tools 7.2.0 | Martin 1.16.1 / 2.0 beta |
|---|---|---|---|---|---|---|
| Released | 2026-11-24 | 2026-03-29 | 2026-08-29 | 2025-07-24 | 2025-09-29 | 1.16.1 on 2026-09-09; 2.0.0-beta.7 on 2026-11-11 |
| What it is | OSM to basemap compiler, as a library and a command line | OSM and other sources to tiles | OSM to tiles | GeoJSON, FlatGeobuf or CSV to tiles | Schema tools and scripts for a PostGIS pipeline (imposm3, SQL) | Tile server; `martin cp` generates MBTiles in bulk |
| Runtime | One Rust binary, or linked into an app | JVM (Java 25+) | One C++ binary, Lua profiles | One C++ binary | Docker, Python, PostgreSQL/PostGIS | One Rust binary; PostGIS for database sources |
| Input | `.osm.pbf` (several merged), plus the supplemental datasets | `.osm.pbf`, Natural Earth, GeoPackage, GeoJSON, Shapefile, GeoParquet (Overture) | `.osm.pbf`, Shapefile, GeoJSON | GeoJSON, FlatGeobuf, CSV | `.osm.pbf` through imposm3 into PostGIS | PostGIS, PMTiles, MBTiles, GeoJSON; GeoParquet through DuckDB (unstable) |
| Schema | OpenMapTiles 3.16, plus a search-and-rescue layer and building levels | OpenMapTiles built in; custom schemas in YAML or Java | OpenMapTiles-compatible config; custom in Lua | None: layers as given | OpenMapTiles (it defines it) | Whatever the source holds |
| Tile encoding | MVT; MLT opt-in (`--tile-format mlt`, experimental) | MVT; MLT with `--tile-format=mlt` (since 0.10.0) | MVT; MLT on the main branch since 2026-10-21, not in a release yet | MVT | MVT | Serves MVT and MLT |
| Archives | PMTiles, MBTiles | MBTiles, PMTiles, files | MBTiles, PMTiles, files | MBTiles, PMTiles, files | MBTiles | Serves PMTiles (local or over HTTP) and MBTiles; writes MBTiles |
| Max zoom | 14 | 16 (since 0.10.2) | Set in the config (14 in the OpenMapTiles config) | Any; `-zg` picks one | 14 | Whatever the source holds |
| OSM updates | Applies change files or replication diffs to the extract (`update`, `compile --changes`/`--replication`), lists the changed tiles (`--changed-tiles`), and writes deltas between archives | Full imports only: "no way to incorporate real-time updates" | Full rebuild; its README advises another tool for continuous updates | Full rebuild; `tile-join` merges tilesets | `import-diff` into PostGIS, then regenerate the changed tiles | `mbtiles` tool diffs MBTiles files and applies patches |
| Updating archives on a device | Signed deltas (ed25519, `diff --sign`, `apply --trusted-key`) | No | No | No | No | MBTiles patches, unsigned |
| Getting the extract | Geofabrik by name (`--area`, `fetch_extract` in the C ABI) | Geofabrik by name (`geofabrik:NAME`) | Scripts for the coastline and landuse data | No | `download-osm` | No |
| Cutting an area from an archive | `extract`, from a local file or over HTTP range requests | No | No | No | No | No (`pmtiles extract` does this for PMTiles) |
| Serving | `serve`: local preview of tiles, TileJSON, style and style pack | No | `tilemaker-server`, a demonstration server | No | `postserve`, a test server | Production tile server, styles, sprites, fonts |
| Runs inside an app | Yes: Rust library, C ABI with every command (compile, extract, update, diff, apply, verify, fetch), Python bindings; CI builds for iOS and Android | No | No | No | No | No (server) |
| Measured on a phone | Yes: Małopolskie on an iPhone 17 in 17.0 s, peak footprint 171 MiB | No | No | No | No | No |

"No" means that the tool's README and release notes, read on 2026-11-24, do
not offer it.

## Only in osm-tile-compiler

These 0.4.0 features have no counterpart in the other tools' READMEs or
release notes:

- **Offline search index** beside the archive (`compile --search-index`):
  places, POIs, streets, peaks, water and the search-and-rescue layer, with
  diacritics folded. On Małopolskie it is 4.9 MB, built in 2.5 s, with
  queries in 0.3 to 2 ms ([search.md](search.md)).
- **Resumable compiles** (`compile --resume`): a killed or cancelled compile
  continues from its last completed stage and writes the same archive byte
  for byte.
- **Signed deltas and archives**, so a device can check an update came from
  a key it trusts before applying it.
- **Progress, cancel and clean-up under the app's control**, from Rust, C and
  Python.

## Measured performance

All on Geofabrik's Małopolskie extract of 2026-11-18 (194 MiB), OpenMapTiles
profile. Other tools were not measured.

| Run | Tool | Time | Memory |
|---|---|---|---|
| Benchmark harness, Linux, 4 threads, command line, no building levels | osm-tile-compiler | 41.6 s | 0.86 GiB peak |
| Same machine and extract | Planetiler 0.10.2 | 93.0 s | 2.9 GiB peak |
| iPhone 17 (A19), C ABI 0.3.0, 4 threads, no supplemental datasets or building levels | osm-tile-compiler | 17.0 s | 171 MiB peak footprint, 633 MiB peak resident |
| Ryzen 7 5800H laptop, in an app's process, 4 threads, datasets and building levels on | osm-tile-compiler | 34 s | 0.7 to 1.3 GiB peak |

On the harness run, both archives hold the same 11,293 tiles and 2,846,782
features, attributes included; 18 merged polygons differ slightly in shape
([parity.md](parity.md)). [performance.md](performance.md) has the per-stage
numbers and the other thread counts. The iPhone footprint is what iOS's
memory limit counts and is not comparable with the Linux peaks.

## Sources

Read on 2026-11-24:

- [Planetiler releases](https://github.com/onthegomap/planetiler/releases) and [README](https://github.com/onthegomap/planetiler#readme)
- [tilemaker releases](https://github.com/systemed/tilemaker/releases), [CHANGELOG](https://github.com/systemed/tilemaker/blob/master/CHANGELOG.md), [README](https://github.com/systemed/tilemaker#readme) and [configuration docs](https://github.com/systemed/tilemaker/blob/master/docs/CONFIGURATION.md)
- [Tippecanoe releases](https://github.com/felt/tippecanoe/releases) and [README](https://github.com/felt/tippecanoe#readme)
- [openmaptiles-tools releases](https://github.com/openmaptiles/openmaptiles-tools/releases), [README](https://github.com/openmaptiles/openmaptiles-tools#readme) and [OpenMapTiles quickstart](https://github.com/openmaptiles/openmaptiles/blob/master/QUICKSTART.md)
- [Martin releases](https://github.com/maplibre/martin/releases) and [README](https://github.com/maplibre/martin#readme)
- [go-pmtiles releases](https://github.com/protomaps/go-pmtiles/releases)
