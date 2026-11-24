# osm-tile-compiler for Python

Python bindings for [osm-tile-compiler](https://github.com/Limina-Agency/osm-tile-compiler):
compile an OpenStreetMap `.osm.pbf` extract into an OpenMapTiles-schema
basemap (one PMTiles or MBTiles archive), bring an extract up to date with
change files, and cut, diff, patch, inspect and verify the archives.

The bindings are built with [PyO3](https://pyo3.rs) and
[maturin](https://www.maturin.rs) as abi3 wheels: one wheel per platform
serves CPython 3.10 and later. They are not on PyPI yet; build a wheel from
this directory:

```sh
pip install maturin
maturin build --release        # the wheel lands in ../target/wheels
pip install ../target/wheels/osm_tile_compiler-*.whl
```

Output derived from OpenStreetMap data is under the ODbL: credit
"© OpenStreetMap contributors". The OpenMapTiles schema and each supplemental
dataset carry their own terms; see the crate README.

## Example

```python
import threading
import osm_tile_compiler as otc

token = otc.CancelToken()       # token.cancel() from any thread stops the compile

def show(progress: otc.Progress) -> None:
    print(f"{progress.stage}: {progress.message} {progress.overall:.0%}")

try:
    result = otc.compile(
        "/data/poland.osm.pbf",
        "/data/maps/krakow.pmtiles",
        "/data/omt-sources",
        "/data/tmp",
        bounds=(19.79, 49.97, 20.22, 50.13),  # west, south, east, north
        read="near_area",
        max_zoom=14,
        exclude_layers=["housenumber"],
        sources_policy="fetch_missing",
        threads=4,
        progress=show,
        cancel=token,
    )
    print(result["result"]["tiles"], "tiles in", result["archive"])
except otc.Cancelled:
    print("stopped")
except otc.Error as error:
    print(error.kind, error.stage, error)

otc.verify("/data/maps/krakow.pmtiles")
otc.extract("/data/maps/krakow.pmtiles", "/data/maps/old-town.pmtiles",
            bounds=(19.92, 50.05, 19.95, 50.07))
delta = otc.diff("/data/maps/old.pmtiles", "/data/maps/new.pmtiles", "/data/maps/update.delta")
otc.apply("/data/maps/old.pmtiles", "/data/maps/update.delta", "/data/maps/rebuilt.pmtiles")
```

## Functions

Paths are `str` or `os.PathLike`; pass absolute ones. Boxes are
`(west, south, east, north)` in degrees; one whose west is greater than its
east crosses the antimeridian. Outputs must not exist yet. Reports are plain
dicts, the same fields the command line prints with `--json`.

| Function | What it does | `progress` | `cancel` |
|---|---|---|---|
| `compile(input_pbf, output, sources, temp_dir, **options)` | Compiles a basemap | yes | yes |
| `extract(source, output, *, bounds, polygon, min_zoom, max_zoom)` | Cuts an area or zooms out of a PMTiles archive | no | yes |
| `update(input, output, *, change_files, replication, until, max_diffs, clip, threads)` | Applies `.osc`/`.osc.gz` files and replication diffs to a PBF (experimental) | yes | yes |
| `diff(base, target, output)` | Writes the delta from one archive to another | no | no |
| `apply(base, delta, output)` | Applies a delta | no | no |
| `read_delta_manifest(path)` | Reads a delta's manifest | no | no |
| `inspect(path)` | Reads every part of an archive; problems are listed, not raised | no | no |
| `verify(path)` | `inspect`, raising `ArchiveInvalid` (with `.report`) on any problem | no | no |

`compile`'s options mirror the Rust `BasemapRequest`: `extra_inputs`,
`format` (`"pmtiles"`/`"mbtiles"`, default by extension), `sources_policy`
(`"use_present"`, `"fetch_missing"`, `"fetch_and_verify"`), `omit_sources`
(ids from `SUPPLEMENTAL_DATASETS`, or `["all"]`), `bounds`, `polygon` (a
GeoJSON or `.poly` file), `read` (`"whole_extract"`, `"near_area"`), `center`
(`(longitude, latitude, zoom)`), `name`, `description`, `attribution`,
`gzip_level`, `min_zoom`, `max_zoom`, `threads`, `layers` or `exclude_layers`
(names from `LAYERS`), `languages`, `building_levels`, `sar`, `sort_memory`.
It returns `archive`, `format`, `bounds`, `building_levels` (a path or None),
`result` (the compile's statistics), `supplemental_inputs`,
`omitted_sources` and `manifest` (the `basemap-manifest` document).

## Progress and cancellation

`progress` is called on the calling thread with a `Progress`: `stage` (for
`compile`, an id from `COMPILE_STAGES`; for `update`, `scan`, `replication`,
`read_changes` or `apply`), `message`, `fraction` of the stage (or None) and
`overall` (or None). A compile reports each stage as it starts and then at
most every 200 ms.

`cancel` is a `CancelToken`, which any thread can set without the GIL, or a
callable returning True to stop; a compile asks about every 50 ms and stops
within about a second. A stopped call raises `Cancelled` and leaves nothing
at its output. Ctrl-C on the main thread stops `compile`, `extract` and
`update` the same way and raises `KeyboardInterrupt`. An exception raised by
`progress` or `cancel` stops the call and is raised from it.

`diff`, `apply`, `inspect` and `verify` take no callbacks: the library offers
none for them. They release the GIL; a Ctrl-C is raised when they return.

## Errors

Every library error is an `osm_tile_compiler.Error` with `kind` (a stable id)
and `stage` (the compile stage of a failure, or None): `Cancelled`,
`InvalidRequest`, `DownloadError`, `OutOfSpace`, `Failed`, and
`ArchiveInvalid` from `verify`.

## Tests

```sh
pip install maturin pytest
maturin build --release && pip install ../target/wheels/osm_tile_compiler-*.whl
pytest
```

The tests compile `../fixtures/monaco.osm.pbf` without the supplemental
datasets and work on the archive.
