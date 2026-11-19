# osm-tile-compiler

Turns an OpenStreetMap extract (`.osm.pbf`) into a vector basemap in the
[OpenMapTiles](https://openmaptiles.org/) schema, written as one
[PMTiles](https://docs.protomaps.com/pmtiles/) file you can serve from any
static host or open on a device. It is a Rust port of the OpenMapTiles
profile that [Planetiler](https://github.com/onthegomap/planetiler) runs,
built for regional extracts on ordinary laptops, and inside apps.

## Built for search and rescue

The compiler comes from search and rescue (SAR) requirements. A team needs
current map data for its search area quickly, made where the team is: on a
field laptop or a phone at the edge, not on a server it may not reach. That
set the design:

- **In-process.** The compile is a library that runs inside the app that
  shows the map, with no separate service, database or Java runtime to
  ship or start. CI builds it for iOS devices and the simulator, and it has
  run in an app in the simulator; it has not yet been measured on a phone. The command line is a thin layer over the same
  call.
- **Speed and memory first.** A region compiles in seconds to a minute, in
  a fraction of the memory a server-side tool expects, so it fits on the
  hardware a team carries and leaves room for the rest of the app.
- **Under the app's control.** Progress is reported stage by stage, a
  cancel stops the compile within about a second, and a stopped or failed
  compile leaves nothing behind.
- **Little to carry.** One extract and, optionally, the supplemental
  datasets downloaded in advance; a compile can also go without them.

## Why use it

- **Fast and light.** On the Małopolskie extract from Geofabrik, with the
  benchmark harness on four threads (Linux, command line, no building
  levels), a compile takes 41.6 s and peaks at 0.86 GiB of memory, where
  Planetiler takes 93.0 s and 2.9 GiB on the same machine: more than twice
  as fast in under a third of the memory.
- **Close to Planetiler's output.** On that run both archives hold the same
  11,293 tiles and the same 2,846,782 features, attributes included. Only 18
  merged polygons (14 building clusters, 4 landuse areas) differ, slightly,
  in shape (see [Compatibility with Planetiler](#compatibility-with-planetiler)).
- **One binary, one file.** No Java, database or GIS tools: a single
  executable reads the extract and writes a single archive.
- **Checked output.** `verify` decodes every tile of an archive, and the
  test suite pins the Monaco build's features.

It is pre-1.0. The OpenMapTiles profile meets the parity bar against
Planetiler on three regions (`parity_verified: true`; see
[docs/parity.md](docs/parity.md)): the same features and attributes, and
polygons identical or within 1% of the area and 1 pixel, with a few
accepted exceptions.

## How it works

A compile runs in stages, each reported as progress:

1. **Supplemental datasets** (`read_sources`). OpenMapTiles takes the sea
   from OSM's water polygons, and low zooms and label ranks from Natural
   Earth; lake labels follow lake centerlines. `fetch-sources` downloads these
   once (about 1.5 GB), checking Natural Earth and the lake centerlines
   against pinned SHA-256 hashes (the water polygons change daily upstream).
   The first compile extracts them, and later compiles reuse that.
2. **First pass over the PBF** (`index_osm`). Node locations and relations
   are read with [osm-pbf-ingest](https://crates.io/crates/osm-pbf-ingest).
3. **Second pass** (`read_osm`). Ways and relations are assembled into
   features, the OpenMapTiles 3.16 rules applied layer by layer, and the
   features simplified and cut into each zoom's tiles, spilling to scratch
   disk.
4. **Tiles** (`write_tiles`). Each tile's features are sorted and, where the
   profile merges them (buildings, landuse, roads), merged, then encoded with
   [fast-mvt](https://crates.io/crates/fast-mvt), and written to the
   PMTiles archive.
5. **Building levels** (`building_levels`, only when asked for). A second,
   smaller archive holds the buildings as boxes, for renderers that draw
   distant buildings that way.

Each stage runs in parallel on the threads you give it.

## Quick start

Install the binary (prebuilt binaries for Linux, macOS and Windows are on
the releases page):

```sh
cargo install osm-tile-compiler
```

Download the supplemental datasets, then compile an extract, for example
Monaco from [Geofabrik](https://download.geofabrik.de/europe/monaco.html):

```sh
osm-tile-compiler fetch-sources sources/
osm-tile-compiler compile monaco-latest.osm.pbf --output monaco.pmtiles --sources sources/
```

Or let `compile` download the extract by its region's name:

```sh
osm-tile-compiler compile --area monaco --output monaco.pmtiles --sources sources/
```

This writes `monaco.pmtiles`, zooms 0 to 14. `compile --fetch` downloads any dataset missing from
`sources/` instead. Progress goes to stderr; `--json` prints the result,
with tile size statistics, on stdout. `osm-tile-compiler --help` lists every
option.

To look at the map, print a [MapLibre](https://maplibre.org/) style for
the archive:

```sh
osm-tile-compiler style monaco.pmtiles > style.json
```

Put this `index.html` beside the style and the archive, and serve the
directory over HTTP with range requests (for example `npx http-server`;
Python's `http.server` cannot serve ranges):

```html
<!doctype html>
<meta charset="utf-8">
<link rel="stylesheet" href="https://unpkg.com/maplibre-gl@6.14.0/dist/maplibre-gl.css">
<script src="https://unpkg.com/pmtiles@4.5.0/dist/pmtiles.js"></script>
<style>html, body, #map { margin: 0; height: 100%; }</style>
<div id="map"></div>
<script type="module">
  import { Map, addProtocol } from "https://unpkg.com/maplibre-gl@6.14.0/dist/maplibre-gl.mjs";
  const protocol = new pmtiles.Protocol();
  addProtocol("pmtiles", protocol.tile);
  const archive = new pmtiles.PMTiles("monaco.pmtiles");
  protocol.add(archive);
  const { centerLon, centerLat, centerZoom } = await archive.getHeader();
  new Map({ container: "map", style: "style.json", center: [centerLon, centerLat], zoom: centerZoom });
</script>
```

The style is OSM Bright from OpenMapTiles, adapted to this compiler's layers.
It credits OpenMapTiles and OpenStreetMap in the map's corner, as their
licenses require.

## Choosing what to compile

### Area

`--area NAME` compiles a Geofabrik extract instead of a local PBF, as
Planetiler's `--area` does. NAME is looked up in Geofabrik's index: an
identifier (`malopolskie`, `us/georgia`), a name with or without accents
(`Małopolskie`, `Lesser Poland Voivodeship`) or an ISO code (`PL-12`). A
name that several regions share, such as `Georgia`, is refused with their
identifiers. The extract is saved in `--download-dir` (by default the
`--sources` directory) as `<ID>.osm.pbf` and reused by later compiles;
delete it to download a newer one.

By default the area is the PBF header's bounding box. `--bounds W S E N`
sets another box, and `--polygon FILE` (GeoJSON, or the Osmosis `.poly`
files Geofabrik publishes) keeps only the tiles the polygon covers; those
tiles keep their full content.

Areas may cross the antimeridian (Fiji, Chukotka, the Aleutians): a box with
W greater than E runs east from W across 180, and polygons may either run on
past ±180 or be split there (RFC 7946). Features near 180 are written into
the tiles on both sides. The archive then declares longitudes -180 to 180,
because MapLibre requests no tiles for a box whose west exceeds its east.

By default the whole extract is read, so the low-zoom tiles over the area
show everything in the extract that they reach. `--area-only` reads only
the OSM objects in and near the area: those whose bounding box touches the
tiles written at the highest zoom, or the tile around them, each read whole.
A small area then compiles from a large extract in seconds: a 9 by 8 km box
in the Tatras takes 2.5 s from the Małopolskie extract instead of 9.9 s
(four threads, Linux, no supplemental datasets). The tiles at the highest
zoom are the same as from a full read, byte for byte; lower zooms show only
what lies near the area, besides the supplemental datasets.

Viewers open the archive at the middle of the area, zoomed to show all of
it, unless you pass `--center LON LAT ZOOM`. `--min-zoom` and `--max-zoom`
narrow the zooms from 0 to 14.

### Layers and languages

`--layers poi,place` writes only those layers, and `--exclude-layers
housenumber` every layer but those. Each layer written is identical to the
same layer of a full build, since layers depend on one another (water names
on water polygons, road names on roads).

`--languages en,de,pl` keeps the `name:<lang>` attributes of those languages
only, out of the 80 the schema lists. The syntax is Planetiler's: `default`
adds the whole list and `-ru` leaves one out, so `default,-ru` keeps all but
Russian. `name`, `name_en`, `name_de`, `name_int`, `name:latin` and
`name:nonlatin` are always written.

### Without the supplemental datasets

`--omit-sources water_polygons,natural_earth,lake_centerlines` (or `all`)
compiles without some or all of them, which saves the download and the disk
they take. The archive's metadata lists what was left out, and the map
lacks:

- **without water polygons**, the sea from zoom 6 in: it is drawn as land,
  so coastlines and islands do not show. Lakes and rivers are unaffected.
- **without Natural Earth**, most of zooms 0 to 5 (oceans, large lakes,
  rivers, glaciers, country and state borders), state names, and the ranks
  that let city and sea names show early.
- **without lake centerlines**, lake names along their lakes: they are
  placed at one point instead.

An inland region loses little without them. `SupplementalDataset::without_warning()`
gives these sentences for an app to show.

### Archive details

`--name`, `--description` and `--attribution` set the archive's metadata;
the attribution must keep crediting OpenMapTiles and OpenStreetMap.
`--gzip-level` trades compile time for size, `--threads` sets the worker
threads (every core by default), and `--temp-dir` the scratch space
(`<output>.tmp` by default, removed afterwards).

## Building levels

Building levels are an optional second archive for renderers that draw
distant buildings as boxes: the basemap's buildings as instanced boxes at
zooms 11, 12 and 14. They are not written by default. `compile
--building-levels` writes them beside the archive as
`<NAME>-buildings.pmtiles`, and `building-levels` writes them from an
existing basemap:

```sh
osm-tile-compiler building-levels monaco.pmtiles monaco-buildings.pmtiles
```

## Checking archives

```sh
osm-tile-compiler inspect monaco.pmtiles   # header, metadata, tile sizes, problems
osm-tile-compiler verify monaco.pmtiles    # exits with 1 if anything is wrong
```

Both read any PMTiles v3 archive. `verify` checks the layout (sections,
directories, entry order, clustering, the header's counts) and decodes every
tile, checking its MVT encoding and that its layers are declared in the
metadata. Tiles over 1 MiB compressed produce a warning at compile time.

## Updating archives in the field (experimental)

A device that already holds an archive can update to a newer compile by
downloading only what changed:

```sh
osm-tile-compiler diff old.pmtiles new.pmtiles --output update.delta    # on the server
osm-tile-compiler apply old.pmtiles update.delta --output new.pmtiles   # on the device
```

`apply` rebuilds `new.pmtiles` byte for byte. Every part of the new archive
(header, directories, metadata, each distinct tile) is copied from the old
archive, which may hold it anywhere, rebuilt from the old tile at the same
position and a patch, or carried in the delta. `apply` checks the old
archive's SHA-256 before it starts and the result's after, and leaves nothing
behind if either check fails. Both commands read any PMTiles v3 archive;
patches need tiles stored as this compiler stores them, and other archives
get a delta without them.

Small edits stay small because compiles are deterministic and an edit
changes only a little of each tile it touches. Małopolskie (153 MB, zoom 0
to 14, without the supplemental datasets) compiled before and after 11 hours
of OpenStreetMap edits gave a 63 kB delta: 245 of its 11,305 tiles changed and
27 were added. `diff` took 1.4 s and `apply` 0.7 s on a 4-core cloud VM. Recompiling with
`--languages en` instead, which changes half the tiles, gave a 5.8 MB delta.

The delta format is experimental: a delta applies only with a build that reads
its version (`docs/formats.md`). From Rust, the same is
`archive::delta::{diff, apply, read_manifest}`.

## Using it as a library

`compile_basemap` runs the whole compile in the caller's process: it fetches
the supplemental datasets if asked, compiles, writes the building levels if
`building_levels` is set, and returns the result and a manifest. The command line calls it too, so
both produce the same bytes.

```rust
let mut request = BasemapRequest::new(pbf, output, sources_dir, temp_dir);
request.threads = 2;
request.sources_policy = SourcesPolicy::FetchMissing; // or UsePresent, FetchAndVerify
let output: Result<BasemapOutput, CompileError> =
    compile_basemap(&request, |progress: &CompileProgress| { /* ... */ }, || cancelled());
```

- **Progress.** The callbacks run on the calling thread, while the work runs
  on a rayon pool of `request.threads` threads built for the compile. Each
  report names its stage (the ids above), the stage's fraction, and an
  overall fraction; stages are reported as they start, then at most every
  200 ms.
- **Cancelling.** `should_cancel` is asked every 50 ms. Once it says yes the
  compile returns `CompileError::Cancelled` within about a second in every
  stage; a stalled download can hold it until the connection times out.
- **Clean-up.** A cancelled or failed compile leaves neither archive and
  removes its scratch directory. Complete downloads and extracted datasets
  stay for the next compile.
- **Embedding.** The library prints nothing, never exits the process,
  installs no allocator (build with `default-features = false` to leave out
  the binary's mimalloc) and never reads the working directory, so pass
  absolute paths. While the PBF is read, up to about twice `threads` threads
  are busy.
- **Panics.** The clean-up relies on unwinding: a panic in the compile is
  caught and returned as an error. Built with `panic = "abort"`, common in
  mobile release profiles, a panic ends the app instead and leaves its
  scratch directory in `temp_dir`; clear that directory at launch.
- **Errors.** `CompileError` says what went wrong: `InvalidRequest` (found
  before any work), `Download`, `OutOfSpace`, or `Failed` with the stage it
  happened in. `kind()` gives a stable identifier, and new kinds may be
  added, so match with a wildcard arm.

Apps that would rather run a separate process can start the binary as a
worker (`--request <request.json>`), which reports progress as JSON lines on
stdout. [`docs/formats.md`](docs/formats.md) describes the request, the manifest and the other
formats the compiler writes, and how their versions change.

## Performance

Times depend on the setup. The comparison above is the benchmark harness on
Linux: the command line with mimalloc, without building levels. Inside an
app's process, with the system allocator, the real supplemental datasets and
building levels on, Małopolskie took:

- on a Ryzen 7 5800H laptop (Linux): 61 s on 2 threads, 34 s on 4 and 22 s
  on 16, peaking at 0.7 to 1.3 GiB of memory;
- on an Apple M5 (an app in the iOS simulator, 4 threads): 20.5 s, peaking
  at 0.87 GiB for the whole app. The macOS command line took 16.1 s and
  0.78 GiB, writing the same bytes.

The basemap is 147 MiB and its building levels 26 MiB; Monaco takes well
under a second. A phone has not been measured yet: its CPU is slower than
the simulator's (which is the Mac's) and its memory limit stricter.
[`docs/performance.md`](docs/performance.md) has the measurements, per stage
and thread count.

Plan disk for the supplemental datasets: 1.45 GB of downloads, unpacked once
into another 2.23 GB. Scratch disk peaks at about five times the extract.

## Compatibility with Planetiler

The profile follows OpenMapTiles 3.16 as Planetiler 0.10.2 implements it
(profile revision `91516fdf`). [`benchmarks/compiler-parity/`](benchmarks/compiler-parity/) builds the same
extract with both and compares every tile; [`docs/parity.md`](docs/parity.md) has the results
for Małopolskie, Berlin and Pomorskie. In short: the same tiles and the same
features. What differs is the shape of a few merged polygons, where
Planetiler merges nearby buildings and landuse through JTS's buffer and
union and the port comes out slightly different: 18 on Małopolskie, 44 on
Berlin and 15 on Pomorskie (0.1.3, 2026-11-19), all but two within 1% of
their area and a pixel. No polygon is invalid where Planetiler's is valid.
Berlin also has one motorway with a vertex 0.125 px away and a romanised Han
name that ICU4X spaces differently from Java's ICU4J. These are accepted, and
the profile reports `parity_verified: true`.

Unlike Planetiler, a `--bounds` box with W greater than E crosses the
antimeridian, and `--languages` refuses codes outside the schema's list.

## Licensing

The code is under the Apache License, Version 2.0 ([`LICENSE`](LICENSE), [`NOTICE`](NOTICE)).
Parts are ported from Planetiler (Apache-2.0) and JTS (Eclipse Distribution
License 1.0), with their licenses in `osm-tile-compiler/src/omt/`; the style
is adapted from OSM Bright (BSD 3-Clause code, CC-BY 4.0 design), with its
license in `osm-tile-compiler/style/`. Release archives also carry
`THIRD-PARTY-LICENSES` for the crates the binary includes.

Tiles you build are not covered by the code's license. They are a derived
database of OpenStreetMap under the Open Database License 1.0: credit
"© OpenStreetMap contributors" (https://www.openstreetmap.org/copyright).
Tiles in the OpenMapTiles schema carry that schema's attribution terms
(https://github.com/openmaptiles/openmaptiles/blob/master/LICENSE.md), and
each supplemental dataset carries its own license.

By default the style loads fonts from the MapLibre demo server and icons
from OSM Bright's GitHub Pages site, neither meant for production. Host your
own with `style --glyphs URL --sprite URL` (the style uses Noto Sans Regular,
Bold and Italic). The sprite lacks icons for some POI classes, which then
show only their names.

## Development

```sh
scripts/check.sh
```

runs what CI runs: formatting, clippy (warnings are errors), the Rust tests
and the benchmark harness's Python tests. [`CONTRIBUTING.md`](CONTRIBUTING.md) covers changes to
tile output, and [`RELEASING.md`](RELEASING.md) the release steps.
