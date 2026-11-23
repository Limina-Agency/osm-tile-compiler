# osm-tile-compiler

Compiles an OpenStreetMap `.osm.pbf` extract into an
[OpenMapTiles](https://openmaptiles.org/)-schema vector basemap, written as a
single [PMTiles](https://docs.protomaps.com/pmtiles/) archive. It is a native
Rust port of the OpenMapTiles 3.16 profile as pinned by Planetiler 0.10.2, with
no Java or external GIS process, and its output is checked against
Planetiler's. An `osm-tile-compiler` binary is included.

It was built for search and rescue: a team has to make map data for its area
quickly, on the laptop or phone it carries rather than on a server. So the
compile runs inside the app's own process, puts speed and memory first, and
stays under the app's control, with progress by stage, cancelling within
about a second, and nothing left behind when it stops or fails.

## Example

```rust,no_run
use osm_tile_compiler::{
    BasemapRequest, CompileError, SourcesPolicy, compile_basemap,
};
use std::sync::atomic::{AtomicBool, Ordering};

let cancel = AtomicBool::new(false);
let mut request = BasemapRequest::new(
    "/data/monaco.osm.pbf",
    "/data/maps/monaco.pmtiles", // must not exist yet
    "/data/omt-sources",         // the supplemental datasets
    "/data/tmp",
);
request.threads = 2;
request.sources_policy = SourcesPolicy::FetchMissing; // or UsePresent, FetchAndVerify
match compile_basemap(
    &request,
    |progress| println!("{} {:.0}%", progress.message, progress.overall * 100.0),
    || cancel.load(Ordering::Relaxed),
) {
    Ok(output) => println!("{} tiles", output.result.tiles),
    Err(CompileError::Cancelled) => println!("stopped"),
    Err(error) => println!("{}: {error}", error.kind()), // invalid_request, download, …
}
```

The sources directory holds the supplemental datasets:
`water-polygons-split-3857.zip` (OSM water polygons),
`lake_centerline.shp.zip` (OSM lake centerlines) and
`natural_earth_vector.sqlite.zip` (Natural Earth), about 1.5 GB in all.
`SourcesPolicy` decides whether the compile downloads them, and
`request.omit_sources` compiles without some or all of them
(`SupplementalDataset::without_warning` says what the map then lacks).
`compile_basemap` is the crate's one compile entry point. `request.area`
limits it to bounds or a polygon, and with `ReadScope::NearArea` reads only
the OSM data near them; `request.zoom` (a `ZoomRange`) and `request.layers`
(a `LayerSelection` of `LayerName`s) pick the zooms and layers.

## Building levels

`write_building_levels`, or the `building-levels` command, writes a second
PMTiles archive from a compiled basemap, with its buildings as instanced boxes
at z14, z12 and z11, for renderers that draw distant buildings as boxes. They
are opt-in: `compile_basemap` with `request.building_levels = true`, and the
binary's `compile --building-levels`, write `<NAME>-buildings.pmtiles` beside
`<NAME>.pmtiles` as the compile's last stage. Tiles are a count and then columns, 18 bytes a box, and the output is
the same for any number of threads.

```text
osm-tile-compiler building-levels basemap.pmtiles basemap-buildings.pmtiles
```

## Status

Pre-1.0. The OpenMapTiles profile meets the repository's parity bar against
Planetiler on three regions, and the result reports `parity_verified: true`
for the compiles that measurement covers: Planetiler's default languages,
every supplemental dataset, a full read of the extract and zoom 14 at the top.
Merged polygons can still differ slightly in shape; the repository's
`docs/parity.md` lists what differs and what is accepted.

## Data and licensing

The crate's code is Apache-2.0. Tiles you produce are not covered by that
license; they carry the terms of their data:

- **OpenStreetMap**: tiles are a derived database under the
  [ODbL 1.0](https://opendatacommons.org/licenses/odbl/1-0/). Credit
  "© OpenStreetMap contributors" (<https://www.openstreetmap.org/copyright>)
  wherever you display or distribute them.
- **OpenMapTiles schema**: tiles in this schema carry its own attribution
  terms, see <https://github.com/openmaptiles/openmaptiles/blob/master/LICENSE.md>.
- **Supplemental datasets**: each has its own license; check the terms of
  every dataset you use.

Parts of the source are ported from Planetiler (Apache-2.0) and from JTS
(Eclipse Distribution License 1.0, BSD-style). Their license texts ship in
the crate at `src/omt/LICENSE.planetiler` and `src/omt/LICENSE.jts-edl-1.0.txt`;
the OpenMapTiles code license is in `src/omt/LICENSE.openmaptiles.md`. The
style that `osm-tile-compiler style` prints (or writes, with `--offline`,
as a pack with its fonts and icons) is adapted from OSM Bright
(BSD 3-Clause code, CC-BY 4.0 design); its license is in `style/LICENSE.md`.

## License

Apache License, Version 2.0 (`LICENSE`), subject to the third-party notices
above.
