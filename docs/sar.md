# The `sar` layer (schema version 1)

`sar` is an opt-in layer for search and rescue. It holds what a search team
needs that OpenMapTiles leaves out or thins out: shelters, water, emergency
points, landing sites, hazards and graded trails. It is written only when
asked for, into the same archive as the basemap, and the sixteen
OpenMapTiles layers are byte-identical with or without it.

```sh
osm-tile-compiler compile malopolskie.osm.pbf --output tatry.pmtiles --sources sources/ --sar
osm-tile-compiler style tatry.pmtiles --sar > style.json
```

From Rust, set `BasemapRequest::sar`; through the C ABI, `"sar": true` in a
`compile_basemap` request. `--layers` and `--exclude-layers` (and
`LayerSelection`) take `sar` like any other layer: naming it among the layers
to write writes it, and leaving it out drops it.

## Why a separate layer

OpenMapTiles 3.16 carries a few of these features, but only at zoom 14 and
thinned by the `poi` label grid, and nothing for power lines, `emergency=*`
points, springs, cliffs or trail difficulty:

| Need | In OpenMapTiles 3.16 | In `sar` |
| --- | --- | --- |
| Huts and shelters | `poi`, z14: `alpine_hut`, `shelter`; grid-limited | Also `wilderness_hut` and named `building=hut`/`cabin`, from z10; capacity, shelter type, locked, fee |
| Drinking water | `poi`, z14: `drinking_water` | Also springs, wells and water points, from z12 |
| Emergency points | Hospitals, police, fire stations | `emergency=*`: mountain rescue, ambulance stations, access points with their `ref`, phones, defibrillators |
| Landing sites | Aeroway polygons | Helipads mapped as nodes, `emergency=landing_site` |
| Power lines, cliffs, caves, mine shafts | Nothing | `hazard` class |
| Trail difficulty | `mtb_scale` only | `sac_scale`, `trail_visibility`, `via_ferrata_scale` |

Peaks and saddles (`mountain_peak`) and aerial cableways
(`transportation`) are already in OpenMapTiles and are not repeated.

## Features

Every feature has a `class` and a `subclass`, the OSM value it comes from.
There is no label grid: every matching feature in a tile is kept. A feature
drawn as a polygon (a hut mapped as a building) is placed at its interior
point, as `poi` places polygons. A node or way with several matching tags is
written once, in the order of the table.

| Class | Geometry | Subclass (OSM tag) | Min zoom |
| --- | --- | --- | --- |
| `shelter` | point | `alpine_hut`, `wilderness_hut` (`tourism`) | 10 when named, else 12 |
| | | `shelter` (`amenity`) | 12 |
| | | `hut`, `cabin` (`building`, only with a `name`) | 10 |
| `landing` | point | `helipad`, `heliport` (`aeroway`); `landing_site` (`emergency`) | 10 |
| `emergency` | point | `mountain_rescue`, `ambulance_station` (`emergency`) | 10 |
| | | `access_point` (`emergency`) | 13 |
| | | `phone`, `defibrillator`, `assembly_point`, `lifeguard` (`emergency`) | 14 |
| `water` | point | `spring` (`natural`); `drinking_water`, `water_point` (`amenity`); `water_well` (`man_made`) | 12 |
| `hazard` | point | `cave_entrance` (`natural`); `adit`, `mineshaft` (`man_made`) | 14 |
| | line | `power_line` (`power=line`) | 11 |
| | line | `minor_line` (`power=minor_line`), `cliff` (`natural`) | 13 |
| `trail` | line | `path`, `footway`, `track`, `bridleway` (`highway`), only with a valid `sac_scale`, `trail_visibility` or `via_ferrata_scale`; `via_ferrata` (`highway`) always | 12 |

Every feature runs to zoom 14. Points carry their OSM object's id in the
OpenMapTiles encoding (id × 10 + 1 for nodes, 2 for ways, 3 for relations).
Lines with the same attributes are joined in each tile, as OpenMapTiles
joins roads, so a joined line carries no id.

## Attributes

Written only when OSM has a value that parses; an attribute that is absent
means unknown.

| Attribute | Classes | Type | From |
| --- | --- | --- | --- |
| `class`, `subclass` | all | string | the table above |
| `name`, `name_en`, `name_de`, `name_int`, `name:latin`, `name:nonlatin`, `name:<lang>` | all | string | `name`, `name:*`, as for the OpenMapTiles layers; `--languages` applies |
| `ele` | points | integer, metres | `ele` |
| `ref` | `emergency`, `landing` | string | `ref`, such as an access point's number given to dispatch |
| `operator` | `shelter`, `emergency`, `landing` | string | `operator` |
| `capacity` | `shelter` | integer | `capacity` |
| `shelter_type` | `shelter` | string | `shelter_type` |
| `locked`, `fee` | `shelter` | 0 or 1 | `locked`, `fee` (`yes`/`no`) |
| `drinking_water` | `water` | 0 or 1 | `drinking_water` (`yes`/`no`) |
| `seasonal` | `water` | 0 or 1 | `seasonal`, else `intermittent` |
| `voltage` | `power_line`, `minor_line` | integer, volts | `voltage`; the highest of a `;` list |
| `sac_scale` | `trail` | `hiking`, `mountain_hiking`, `demanding_mountain_hiking`, `alpine_hiking`, `demanding_alpine_hiking`, `difficult_alpine_hiking` | `sac_scale` |
| `trail_visibility` | `trail` | `excellent`, `good`, `intermediate`, `bad`, `horrible`, `no` | `trail_visibility` |
| `via_ferrata_scale` | `trail` | integer, 0 to 6 | `via_ferrata_scale` |

## Metadata and versioning

An archive with the layer lists `sar` last in `vector_layers` and has
`"sar_schema": 1` in its metadata; an archive without it has neither. A
change to the classes, subclasses, attributes or zooms raises `sar_schema`
(`SAR_SCHEMA_VERSION` in the library), so an app can tell which version it is
styling.

## Style

`style --sar` adds layers drawing `sar` above OSM Bright: trails coloured by
`sac_scale` and dotted where `trail_visibility` is bad or worse, power lines
and cliffs, and points as circles coloured by class with their name or
`ref`. They use no sprite icons, so they draw with any sprite.

## Not in version 1

- Fire hydrants and other urban `emergency=*` points.
- Areas: military danger zones, avalanche areas, restricted areas.
- Mobile coverage, which OSM does not hold.
- Contours and terrain.
- Data from outside OSM, such as incident overlays.
