# Tile schemas

A compile writes OpenMapTiles 3.16 by default. It can instead write
[Shortbread](https://shortbread-tiles.org), which is built in, or a schema of
your own written in YAML. The YAML format is the one Planetiler's
[custom map](https://github.com/onthegomap/planetiler/blob/main/planetiler-custommap/README.md)
reads, so a schema written for `planetiler generate-custom` compiles here as
it is, within the subset listed below.

```sh
osm-tile-compiler compile berlin.osm.pbf --output berlin.pmtiles \
  --sources sources --schema shortbread
osm-tile-compiler compile monaco.osm.pbf --output roads.pmtiles --schema roads.yml
osm-tile-compiler schema verify roads.yml   # run the examples the schema lists
osm-tile-compiler schema layers shortbread  # list a schema's layers
```

| Where | How to choose the schema |
| --- | --- |
| Command line | `--schema openmaptiles\|shortbread\|FILE.yml` |
| Library | `BasemapRequest::schema`: `TileSchema::OpenMapTiles`, `TileSchema::Shortbread` or `TileSchema::Custom(Arc<Schema>)`, from `Schema::from_file` or `Schema::from_yaml` |
| Worker request | `"schema": "shortbread"` or the path of a YAML file |
| C ABI (`compile_basemap` job) | `"schema": "shortbread"`, a path, or `{"yaml": "..."}` with the schema's text, for an app that bundles its schema |
| Python | `compile(..., schema="shortbread")` or a path |

## Why YAML and not Lua

A schema here is data, not a program. The YAML is read and checked in full
before a compile starts: unknown keys, unknown geometry types, bad tag tests
and malformed expressions are errors that name where they are, and
`schema verify` runs the examples a schema carries without any OSM data. Its
`${ ... }` expressions are a small subset of CEL that always finishes: no
loops, no recursion, no file or network access, and nesting and input sizes
are capped (8 MiB of text, 2 million nodes after YAML aliases, 128 levels).
Nothing in it can hang a compile or reach outside it.

That matters most where the library is embedded. On iOS and Android a Lua
schema would mean bundling and sandboxing an interpreter, and Apple's rules
on downloaded code make a scripted schema that an app fetches doubtful. A
YAML schema is a file like any other input, the same on every platform. And
because the format is Planetiler's, schemas and their tests carry over in
both directions, Shortbread among them.

## Shortbread

The built-in Shortbread schema is the `shortbread.yml` that Planetiler 0.10.2
ships, with its specification `shortbread.spec.yml` as examples. Its 67
examples pass (`schema verify shortbread`).

Compiled from the Berlin extract of 2026-11-18, the archive matches
Planetiler 0.10.2's `generate-custom --schema=shortbread.yml` of the same
extract: the same 885 tiles, and in all 26 layers the same features with the
same attributes and the same geometry, down to the tile coordinate. Polygon
rings may start at a different vertex, which draws the same. The one
difference is `way_area`, the area in square meters some layers carry,
which differs in its tenth significant digit on 216 features, from rounding
in Java's and Rust's sine functions. The compile took 6.9 s against
Planetiler's 35 s on 4 threads.

Shortbread reads only the water polygons dataset, for its `ocean` layer, so
`--sources` needs only `water-polygons-split-3857.zip`.

## What changes with another schema

- `--layers` and `--exclude-layers` name the schema's layers.
- `--sar`, `--languages`, `--building-levels`, `--search-index` and the merge
  cache go by OpenMapTiles' layers and are refused.
- Only the supplemental datasets the schema reads are fetched and read. A
  schema with OSM sources only needs no `--sources`.
- OpenMapTiles' post-processing (merging buildings, landuse and roads,
  label density, ranks) applies to the OpenMapTiles schema only, even when
  another schema has a layer of the same name. Planetiler's custom maps do
  not post-process either, except through `tile_post_process`.
- The archive's metadata takes `name`, `description` and `attribution` from
  the schema (`schema_name`, `schema_description`, `attribution`) unless the
  request sets them; without an attribution it credits OpenStreetMap
  contributors. `type` is `overlay` for a schema with `is_overlay: true`.
  The metadata adds `schema` and `schema_digest` (a hash of the schema's
  text), gives `version` only when the schema has one, and leaves out
  OpenMapTiles' `planetiler_openmaptiles_profile_commit`.
- The manifest's `schema` and `version` are the schema's, and the compile
  result's `parity_verified` is false.
- Zooms go up to 14, as for OpenMapTiles.

## The supported subset

Everything in Planetiler's custom map format applies except what this list
leaves out or limits.

- **Top level**: `schema_name`, `schema_description`, `attribution`,
  `version`, `is_overlay`, `args`, `definitions` (with YAML anchors and
  aliases), `sources`, `layers`, `examples` (a list, or a file beside the
  schema). `tag_mappings` is refused; set types with `type` on attributes.
- **Sources**: `type: osm` reads the compile's PBF. A `shapefile` or
  `geopackage` source whose `url` or `local_path` is the
  `water-polygons-split` dataset reads the water polygons from `--sources`.
  Other files (Shapefile, GeoPackage, GeoJSON, FlatGeobuf, GeoParquet, CSV)
  are refused for now; they will come with the data source layer.
- **Layers**: `id`, `features`, `buffer`. `tile_post_process` is read and
  ignored, with a warning that `schema verify` prints.
- **Features**: `source`, `id`, `geometry`, `include_when`, `exclude_when`,
  `min_zoom`, `max_zoom`, `min_size`, `min_size_at_max_zoom`, `tolerance`,
  `tolerance_at_max_zoom`, `attributes`. Geometry types are `point`, `line`,
  `polygon`, `any`, `centroid`, `polygon_centroid`, `line_centroid`,
  `line_midpoint`, `polygon_centroid_if_convex`, `polygon_point_on_surface`,
  `point_on_line` and `innermost_point`; `split_line` is refused.
- **Attributes**: `key`, `value`, `tag_value`, `arg_value`, `type`,
  `coalesce`, `else`, `include_when`, `exclude_when`, `min_zoom`,
  `min_zoom_by_value`, `min_tile_cover_size`. Keys starting with `_` are
  refused, since the encoder drops them.
- **Tag tests**: maps of tag to value or list of values, `__any__`, `''` for
  a missing tag, `%` wildcards, `__all__`, `__any__` and `__not__` blocks,
  and `${ ... }` scripts.
- **Expressions**: `match`, `coalesce`, `default_value`, `overrides`, type
  casts, and `${ ... }` scripts with literals, arithmetic, comparisons,
  `&&`, `||`, `!`, `? :`, `in`, indexing, `has()`, the macros `map`,
  `filter`, `exists`, `exists_one` and `all`, the functions `size`,
  `coalesce`, `nullif`, `min`, `max`, `int`, `double`, `string` and `bool`,
  and the string and list methods `contains`, `startsWith`, `endsWith`,
  `indexOf`, `lowerAscii`, `upperAscii`, `replace`, `split`, `substring`,
  `trim`, `join`, `get` and `has`. The variables are those Planetiler gives
  (`feature.tags`, `feature.id`, `feature.source`, `feature.source_layer`,
  `feature.osm_type`, `feature.area(unit)`, `feature.length(unit)`,
  `feature.min_lat` and the like, `match_key`, `match_value`, `value`,
  `args`). A script that fails, such as one dividing by zero, gives null.

Measures follow Planetiler: OSM geometries are read from 31-bit world
coordinates, as Planetiler's node store keeps them, and areas and lengths in
meters use its spherical formulas.
