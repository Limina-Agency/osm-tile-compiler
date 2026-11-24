# Data layers

A compile can write layers of other vector data beside the OpenMapTiles
basemap: parcels from a GeoPackage, stations from a CSV, districts from a
Shapefile, anything from GeoJSON, FlatGeobuf or GeoParquet. Each file
becomes one tile layer of the same archive, at the zooms you choose, with
the attributes you keep. Every reader is pure Rust; nothing needs GDAL, so
the same compile runs on a server, a laptop or a phone.

```sh
osm-tile-compiler compile berlin.osm.pbf --output berlin.pmtiles --sources omt-sources \
  --data "districts=bezirke.json,zoom=8-14,attributes=namgem,rename=namgem:name" \
  --data "landmarks=landmarks.csv,zoom=12-14" \
  --data "parcels=parcels.gpkg,table=flurstuecke,crs=EPSG:25833,min-zoom=13"
```

## Formats

| Format | Extensions | Geometry | Coordinate system read from |
|---|---|---|---|
| GeoJSON | `.geojson`, `.json` | any | RFC 7946 WGS84; a GeoJSON 2008 `crs` member at the start or end of the file |
| Newline-delimited GeoJSON | `.geojsonl`, `.geojsons`, `.ndjson`, `.jsonl` | any | WGS84 |
| CSV, TSV | `.csv`, `.tsv` | points from two columns, or WKT / hex WKB in one | WGS84 unless `crs` is given |
| Shapefile | `.shp` (with `.dbf`, `.shx`) | any | `.prj` beside it |
| GeoPackage | `.gpkg` | any, one feature table | `gpkg_spatial_ref_sys` |
| FlatGeobuf | `.fgb` | any | its header |
| GeoParquet 1.0, 1.1 | `.parquet`, `.geoparquet` | WKB | the `geo` metadata (PROJJSON); a plain Parquet file with a WKB `geometry`, `geom` or `wkb` column is read as WGS84 |

A `format` key overrides the extension. FlatGeobuf, GeoParquet and
reprojection from anything but WGS84 and Web Mercator come with the crate
features `flatgeobuf`, `geoparquet` and `reproject`, all in the default
`data-sources` feature; the C ABI and Python builds turn it on. A build
without one refuses that input when the request is checked.

Geometry collections are split into their parts, which keep the feature's
id and attributes. Z and M values are dropped. Features without geometry, or
with coordinates that are not finite, are skipped and counted.

## Coordinate systems

Coordinates are reprojected to WGS84 when they are read. A file's own
declaration is used unless the input's `crs` overrides it; `crs` takes
`EPSG:<code>` (also `urn:ogc:def:crs:EPSG::<code>`, an opengis.net URL or a
bare number), `OGC:CRS84`, a PROJ string or WKT 1 or 2. Reprojection is
proj4rs (a pure-Rust port of PROJ.4, 2D, no grid shifts) with the EPSG table
of crs-definitions; WKT goes through proj4wkt. WGS84, ETRS89, NAD83, GDA94
and the other datums within about a metre of it are read as WGS84 without a
shift: at zoom 14 a pixel is about ten metres.

A file that declares nothing is read as WGS84. If its coordinates cannot be
longitudes and latitudes (beyond ±360 or ±90), the read fails and says to
give the input's `crs`, rather than putting the features off the map.

Checked on Berlin's official district boundaries from the Geoportal Berlin
WFS: read in EPSG:25833 (whose GeoJSON names its CRS after the features),
they land within 10⁻⁶ degrees of the same service's EPSG:4326 output.

## Into tiles

Each data layer is written after the profile's layers, in the request's
order, with:

- **Zooms**: `min_zoom` and `max_zoom`, within the compile's zooms [default:
  the compile's]. The archive's `vector_layers` entry gives the zooms the
  layer actually holds.
- **Attributes**: all of them, or only `attributes`, less
  `exclude_attributes`, renamed by `rename_attributes`. Values keep their
  type: strings, numbers (whole numbers as integers) and booleans; nested
  values (GeoJSON objects and arrays, Parquet lists) become their JSON text,
  as Tippecanoe writes them. Empty values are left out.
- **Ids**: the `id_attribute`'s whole-number value, else the format's own id
  (GeoJSON's `id`, GeoPackage's primary key, the record number of a
  Shapefile or FlatGeobuf), else the feature's position in the file.
- **Geometry**: clipped to each tile with the profile's 4-pixel buffer,
  simplified per zoom as the profile's features are (0.1 pixels, 1/16 at the
  highest zoom), and polygons under a pixel are dropped at the zooms where
  they are that small. Features are not merged, thinned or ranked.

Features are read within the compile's bounds plus one tile at the layer's
highest zoom, so the tiles there are complete; lower zooms, like a compile
that reads only near its area, show what lies near the area. The archive
covers the compile's area, as it does without data layers.

A data layer may be named like one of the profile's layers, such as `poi`,
to add features to it; that layer must then be one the compile writes, and
the compile is no longer one the parity measurement covers
(`parity_verified` is false). Two data layers may share a name: their
features go into one layer.

The archive's metadata lists each data layer in `vector_layers` with its
fields, as it lists the profile's, and under `data_layers` with the file
name, format and number of features it came from. Each layer's
`attribution` is added to the archive's. The compile's result
(`CompileResult::data_layers`, `--json`) reports per layer the features
read, those outside the area, those without geometry and the coordinate
system they were read in.

Data layers work with MVT and MLT tiles, PMTiles and MBTiles, area and
polygon compiles, and resumable compiles: a checkpoint records the data
files' sizes and times and is discarded when one changes.

## Command line

`--data <LAYER>=<FILE>[,<KEY>=<VALUE>...]`, repeated for more layers. Keys:

| Key | Meaning |
|---|---|
| `format` | `geojson`, `geojson_seq`, `csv`, `shapefile`, `geopackage`, `flatgeobuf`, `geoparquet` (or an extension) |
| `table` | the GeoPackage feature table, needed when the file holds several |
| `crs` | the coordinate system, over what the file says |
| `zoom` | `<MIN>-<MAX>`, or one zoom |
| `min-zoom`, `max-zoom` | each on its own |
| `attributes` | the attributes to keep, separated by `\|` |
| `exclude` | attributes to leave out, separated by `\|` |
| `rename` | `<FROM>:<TO>` pairs, separated by `\|` |
| `id` | the attribute that holds the feature id |
| `attribution` | credit added to the archive's attribution |
| `lon`, `lat` | CSV coordinate columns [default: found by name: `longitude`, `lon`, `lng`, `long`, `x`; `latitude`, `lat`, `y`] |
| `geometry` | a CSV column of WKT or hex WKB [default: `wkt`, `geometry`, `geom`, `the_geom`, `wkb`] |
| `delimiter` | the CSV separator, one character or `tab` [default: tab for `.tsv`, else a comma] |

A comma that is not followed by one of these keys belongs to the value
before it, so paths, PROJ strings and WKT may hold commas.

`--data-layers <FILE.json>` reads a JSON array of data layers (or an object
with one under `data_layers`), in the Rust API's form; relative paths are
relative to the file:

```json
[
  {
    "layer": "parcels",
    "source": { "path": "parcels.gpkg", "table": "flurstuecke", "crs": "EPSG:25833" },
    "min_zoom": 13,
    "attributes": ["id", "area"],
    "rename_attributes": { "id": "parcel" },
    "attribution": "© GeoBasis-DE"
  },
  {
    "layer": "stations",
    "source": { "path": "stations.csv", "csv": { "longitude": "x", "latitude": "y" } }
  }
]
```

`data-info <FILE>[,<KEY>=<VALUE>...] [--json]` reads a file as `--data`
would and prints its format, coordinate system, feature count, geometry
types, bounds in WGS84 and attributes with their types, for checking a file
before compiling it.

## Rust

```rust
use osm_tile_compiler::{BasemapRequest, DataInput, DataLayer};

let mut request = BasemapRequest::new("berlin.osm.pbf", "berlin.pmtiles", "omt-sources", "tmp");
let mut parcels = DataInput::new("parcels.gpkg");
parcels.table = Some("flurstuecke".into());
let mut layer = DataLayer::new("parcels", parcels).zooms(13, 14);
layer.attributes = Some(vec!["id".into(), "area".into()]);
request.data_layers.push(layer);
```

`DataInput` describes only where features come from and how to read them;
it knows nothing of tiles or schemas. `DataInput::read(bounds, visit)` hands
every feature to `visit` as a `SourceFeature` (an id, a WGS84 `geo_types`
geometry and typed properties), for code that maps features itself, such as
a custom schema. `DataLayer` adds the tile layer, zooms and attributes. Both
are `serde` types: snake_case keys, camelCase accepted for the multi-word
ones.

## C ABI and Python

The `compile_basemap` job takes `dataLayers`, a list in the same JSON form
(camelCase keys such as `minZoom` and `idAttribute` work too). Python's
`compile` takes `data_layers`, a list of dicts in that form; paths may be
`pathlib.Path` objects. Unknown keys are refused, so a typo fails the
request rather than being ignored.

## Limits

- Only GeoParquet's WKB encoding is read, not GeoArrow's native encodings.
- GeoJSON's legacy `crs` member is found only in the first or last 64 KiB.
- Reprojection is 2D and has no grid shifts (NTv2, NADCON): datums that
  need one for sub-metre accuracy are a few metres off.
- A Shapefile's `.cpg` and its `.dbf` code page mark are not applied: text
  is read as UTF-8, with invalid bytes replaced. Most current Shapefiles
  (Natural Earth's among them) are UTF-8.
- Features are not merged or thinned at low zooms, so a dense point layer
  should start at a zoom where its tiles stay small; the compile warns about
  tiles above 1 MiB.
