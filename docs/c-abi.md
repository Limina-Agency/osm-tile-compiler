# C ABI

`osm-tile-compiler-ffi` gives native hosts (iOS and Android apps first, any
language with a C FFI after) the compiler as a static or dynamic library. Its
interface is small on purpose: requests and results are JSON, and every
long-running request is a **job** the host polls. No function calls back
into the host, so the bindings need no callbacks, no thread attachment
(JNI) and no `@convention(c)` closures (Swift).

The header is [`osm-tile-compiler-ffi/include/osm_tile_compiler.h`](../osm-tile-compiler-ffi/include/osm_tile_compiler.h).
`tests/header.rs` checks it against the library's exports and constants.

## Why JSON and a polled job

- **JSON in and out.** The request types will grow (0.2 reshapes them, later
  lanes add fields). A struct-based ABI would break with each change; a JSON
  envelope only breaks when a field's meaning changes. Swift's `Codable` and
  Kotlin's `kotlinx.serialization` decode it directly.
- **Polling, not callbacks.** A callback from a Rust thread into Swift or the
  JVM needs thread attachment and careful lifetimes on every platform. A
  poll is a plain call from whatever thread the host likes, at the rate its
  UI wants. The compile already throttles its own progress to 200 ms, so a
  poll every 100 to 500 ms misses nothing that matters.
- **One job type for every request kind.** Starting, polling, cancelling and
  collecting a result work the same for a compile, an extract, an update or
  a delta apply. Wrappers write that once.

## Lifecycle

```c
OtcJob *job; char *error;
if (otc_job_start(request_json, &job, &error) != OTC_OK) {
    show(error); otc_string_free(error); return;
}
while (otc_job_wait(job, 250) == OTC_JOB_RUNNING) {   // or a timer calling otc_job_poll
    char *progress = otc_job_poll(job);
    update_ui(progress); otc_string_free(progress);
    if (user_pressed_stop) otc_job_cancel(job);
}
char *result; otc_job_result(job, &result);
handle(result); otc_string_free(result);
otc_job_free(job);
```

| Function | What it does |
|---|---|
| `otc_abi_version()` | The ABI version, `1`. Check it equals `OTC_ABI_VERSION` at startup. |
| `otc_version()` | The library's version string; static. |
| `otc_job_start(json, &job, &error)` | Parses the envelope and the kind's request, then starts the job on a thread of its own. A request that cannot be parsed fails here, synchronously, with `OTC_ERROR_INVALID_REQUEST` and an error JSON. |
| `otc_job_state(job)` | `OTC_JOB_RUNNING`, `_SUCCEEDED`, `_FAILED` or `_CANCELLED`. |
| `otc_job_poll(job)` | The progress JSON. |
| `otc_job_wait(job, ms)` | Blocks up to `ms` for the job to end; returns the state. Not for a UI thread with a large timeout. |
| `otc_job_cancel(job)` | Returns at once; the job stops within about a second and removes its partial outputs. |
| `otc_job_result(job, &result)` | The result JSON once the job has ended; `OTC_ERROR_NOT_FINISHED` before. |
| `otc_job_free(job)` | Frees the handle. A running job is cancelled and cleans up on its own thread; the call does not wait. |
| `otc_string_free(s)` | Frees any string the library returned. Never use `free()`. |

Every function but `otc_job_free` may be called on the same job from several
threads at once. Every function catches panics, so nothing unwinds into the
host; this needs the default `panic = "unwind"`. A host built with
`panic = "abort"` loses that, and a panic then ends the app and leaves the
compile's scratch directory behind.

## The JSON

### Envelope (into `otc_job_start`)

```json
{ "abiVersion": 1, "kind": "compile_basemap", "request": { ... } }
```

Unknown envelope fields, an unknown `kind` and an `abiVersion` other than the
library's are rejected. Requests reject unknown fields too, as the worker
protocol does: a host and its library ship together.

### Progress (from `otc_job_poll`)

```json
{
  "state": "running",
  "kind": "compile_basemap",
  "cancelRequested": false,
  "updates": 12,
  "stage": "read_osm",
  "message": "Reading OSM and preparing vector tiles",
  "fraction": 0.42,
  "overall": 0.31
}
```

`state` is `running`, `succeeded`, `failed` or `cancelled`. `updates` counts
the reports so far, so a host can skip redrawing when it has not moved.
`stage` is a stable identifier: for `compile_basemap` one of
`CompileStage::id`'s, for the other kinds those their sections list.
`fraction` is the stage's share done (or `null`), and `overall` the whole
job's (1 once it succeeded).
Before the first report, `stage` is `null` and `message` empty.

### Result (from `otc_job_result`)

```json
{ "ok": true,  "kind": "compile_basemap", "result": { ... } }
{ "ok": false, "kind": "compile_basemap",
  "error": { "kind": "out_of_space", "message": "...", "stage": "write_tiles" } }
```

`error.kind` is one of `CompileError::kind`'s identifiers (`cancelled`,
`invalid_request`, `download`, `out_of_space`, `failed`), or `internal` for a
fault of the library. `stage` is set for `failed`. Messages are fit to show.
`otc_job_start`'s error JSON has the same `{kind, message, stage}` shape.

### `compile_basemap`

```json
{
  "inputPbf": "/…/monaco.osm.pbf",
  "extraInputs": ["/…/more.osm.pbf"],
  "output": "/…/monaco.pmtiles",
  "format": "pmtiles",
  "sources": "/…/omt-sources",
  "tempDir": "/…/tmp",
  "sourcesPolicy": "use_present",
  "omitSources": ["water_polygons", "natural_earth", "lake_centerlines"],
  "area": {
    "bounds": { "north": 43.76, "south": 43.72, "east": 7.45, "west": 7.40 },
    "polygon": "/…/area.geojson",
    "read": "near_area"
  },
  "center": { "longitude": 7.42, "latitude": 43.73, "zoom": 13 },
  "zoom": { "min": 0, "max": 14 },
  "layers": { "except": ["poi"] },
  "languages": ["en", "pl"],
  "archive": { "name": "Monaco", "description": "…", "attribution": "…", "gzipLevel": 6 },
  "threads": 2,
  "buildingLevels": false,
  "sar": false
}
```

Only the four paths are required; everything else defaults as
`BasemapRequest::new` does (every core, zooms 0 to 14, all layers and
languages, the datasets present in `sources`). `sourcesPolicy` is
`use_present`, `fetch_missing` or `fetch_and_verify`. `layers` is
`{"only": [...]}` or `{"except": [...]}`. `area.read` is `whole_extract` (the
default) or `near_area`, which reads only the OSM data in and near the area.
`format` is `pmtiles` or `mbtiles`; without it, an `output` ending in
`.mbtiles` is MBTiles and anything else PMTiles. Building levels need
PMTiles. `sar` also writes the search-and-rescue layer
([sar.md](sar.md)); `layers` may still leave it out, and `{"only": [...]}`
naming `sar` writes it too.

The result is the basemap manifest the worker writes (`format`,
`formatVersion`, `schema`, `version`, `result`, `supplementalInputs`,
`omittedSources`), plus `archive`, `archiveFormat` (`pmtiles` or
`mbtiles`), `buildingLevels` (a path or `null`) and
the `bounds` compiled.

The field layout follows the 0.2 request types (`ZoomRange`,
`LayerSelection`, `Area` with its `ReadScope`), and the library maps it onto
`BasemapRequest` field by field. `otc_job_start` then runs
`BasemapRequest::validate()`, so whatever needs no reading or network fails
synchronously: unknown layers or languages, zooms outside 0 to 14,
`threads` of 0, a gzip level above 9, bad bounds or center, building levels
with MBTiles, `near_area`
without bounds or a polygon, an output that already exists. What needs
reading (a PBF or polygon that cannot be opened, missing datasets) fails the
job instead, with `error.kind` `invalid_request`. Once the request types get
serde support with these names (roadmap item A9), the mapping goes away.

### `write_style_pack`

```json
{
  "directory": "/…/maps/berlin-style",
  "archiveUrl": "pmtiles://file:///…/maps/berlin.pmtiles",
  "archiveFile": "/…/maps/berlin.pmtiles",
  "languages": ["uk"],
  "glyphs": "https://demotiles.maplibre.org/font/{fontstack}/{range}.pbf",
  "sprite": "https://openmaptiles.github.io/osm-bright-gl-style/sprite",
  "baseUrl": "file:///…/maps/berlin-style"
}
```

Writes an offline style pack (see the README): `style.json` pointing at
`archiveUrl`, the glyph ranges its labels need under `fonts/`, the sprite and
`LICENSE.md`. Only `directory` (which must not exist) and `archiveUrl` are
required. `archiveFile` is a local copy of the archive whose labels decide
the glyph ranges beyond Latin; `languages` adds their scripts. `glyphs` and
`sprite` say where to fetch from (URLs, `file://` URLs or local paths) and
default to the servers above; `baseUrl` goes before the style's glyph and
sprite paths, which MapLibre Native needs; `sar: true` also draws the
opt-in `sar` layer, for an archive compiled with it. Progress reports `Fetched N of M
files` with a fraction and no stage. The result is the `StylePackReport`:

```json
{
  "directory": "/…/maps/berlin-style",
  "style": "/…/maps/berlin-style/style.json",
  "fontstacks": ["Noto Sans Regular", "Noto Sans Bold", "Noto Sans Italic"],
  "ranges": ["0-255", "256-511", "…"],
  "files": 87,
  "bytes": 11631298,
  "missing": []
}
```

`missing` lists files the source lacked (a glyph range, or the `@2x`
sprite), which are left out; a missing 1x sprite fails the job with
`download`. A failed or cancelled job leaves no directory behind.

### Archives and updates

These kinds do what the command line's `extract`, `update`, `diff`, `apply`,
`inspect` and `verify` do. Their results are the same reports the commands
print with `--json`, so their fields are `snake_case`. Paths to write must
not exist yet; one that does fails `otc_job_start`. Unknown fields are
refused, and so is anything else wrong that needs no reading: zooms outside
0 to 14 or the wrong way round, a bad box, an unknown replication source.

| Kind | Request | Result |
|---|---|---|
| `extract` | `source`, `output`, and optionally `bounds`, `polygon`, `zoom` | `ExtractReport` |
| `update` | `input`, `output`, `changeFiles` and/or `replication`, optionally `until`, `maxDiffs`, `bounds` or `noClip`, `threads` | `UpdateReport` |
| `diff` | `base`, `target`, `output` (the delta) | `DiffReport` |
| `apply` | `base`, `delta`, `output` | `ApplyReport` |
| `inspect` | `archive` | `ArchiveReport` |
| `verify` | `archive` | `ArchiveReport`, or a failed job |

#### `extract`

```json
{
  "source": "/…/maps/poland.pmtiles",
  "output": "/…/maps/krakow.pmtiles",
  "bounds": { "north": 50.13, "south": 49.97, "east": 20.22, "west": 19.79 },
  "polygon": "/…/krakow.geojson",
  "zoom": { "min": 0, "max": 14 }
}
```

Cuts the tiles that meet the box, the polygon (a GeoJSON or Osmosis
`.poly` file) and the zooms out of a PMTiles archive, byte for byte. With
none of the three it copies every tile; `zoom.min` defaults to 0 and
`zoom.max` to 14. A polygon that cannot be read, a source that is not
PMTiles v3, or an area or zooms with none of its tiles fail the job with
`invalid_request`. Progress has the stage `extract` and no fraction; a cancel
stops it within 4096 directory entries and leaves no output. The result has
`tiles`, `tile_contents`, `source_tiles`, `bytes`, `tile_bytes`, `zooms`,
`bounds` (west, south, east, north) and `center`.

#### `update`

```json
{
  "input": "/…/malopolskie.osm.pbf",
  "output": "/…/malopolskie-new.osm.pbf",
  "changeFiles": ["/…/124335.osc.gz"],
  "replication": "header",
  "until": 124347,
  "maxDiffs": 2000,
  "bounds": { "north": 50.6, "south": 49.1, "east": 21.5, "west": 19.0 },
  "threads": 2
}
```

Experimental, like the command. Applies the change files in order, then
the diffs a replication server has beyond the input: `replication` is
`header` (the server the PBF's header names, as Geofabrik's do), `minute`,
`hour`, `day` (the planet servers) or a server's URL. At least one of
`changeFiles` and `replication` is needed; `until` and `maxDiffs` need
`replication`. Changes are kept only within the input header's box, or
`bounds`; `noClip: true` keeps every change, for diffs already cut to the
extract. The output is a PBF to compile like any other, and `diff` then
turns the old and new archives into a delta.

Progress goes through `read_input`, `read_changes` (the fraction counts
change files and diffs read) and `write_output`. A cancel is seen between
steps: after the input is read and before each file or diff, not while the
output is written. Failed downloads fail with `download`. The result has
`changes_read`, `changes_kept`, `created`, `modified`, `deleted`, the
`diffs` applied (first and last sequence), the header's
`replication_timestamp` and `replication_sequence_number` after the update,
`output_bytes` and `seconds`.

#### `diff` and `apply`

```json
{ "base": "/…/old.pmtiles", "target": "/…/new.pmtiles", "output": "/…/new.delta" }
{ "base": "/…/old.pmtiles", "delta": "/…/new.delta", "output": "/…/new.pmtiles" }
```

Experimental, like the commands. `diff` writes the delta that turns `base`
into `target` and checks it by rebuilding `target` from it; `apply` checks
`base` against the delta's manifest, writes the new archive and checks its
SHA-256. Both report the stage `diff` or `apply` with no fraction, and both
leave nothing at `output` when they fail. Neither can stop part way: a
cancel before they start ends the job at once, and one that arrives while
they run takes effect when they end, removing the output and ending
`cancelled`. The result is the delta's manifest, flattened, plus
`delta_bytes`: `format`, `version`, `base` and `target` (`bytes`, `sha256`),
`copied_bytes`, `literal_bytes`, `patched_bytes`, `patched_tiles` and
`tiles` (`unchanged`, `changed`, `added`, `removed`).

#### `inspect` and `verify`

```json
{ "archive": "/…/maps/krakow.pmtiles" }
```

Both read every part of the archive: header, directories, metadata and
every tile. `inspect` succeeds for any readable PMTiles v3 file and lists
what is wrong in `problems` (the first 50) and `problem_count`; `verify`
fails the job with `failed` when `problem_count` is not 0, naming the first
five problems in the message, so a host that only needs a yes or no checks
`ok`. A file that is not PMTiles v3 fails either. The stage is `inspect` or
`verify`, with no fraction. The report also has the header's fields
(`tile_type`, `tile_compression`, `zooms`, `bounds`, `center`,
`addressed_tiles`, `tile_entries`, `tile_contents`), `file_bytes`, the
parsed `metadata` and `tile_stats`.

## Versioning

`OTC_ABI_VERSION` covers the function signatures, the status and state
numbers, and the envelope, progress, result and error shapes. It changes only
when a host written for the old version would misbehave; new optional fields
and new request kinds do not change it. Request fields follow the crate's
semantic versioning, like the Rust API.

## Planned next

- Request kinds: `fetch_sources` (download the supplemental datasets ahead of
  a compile, with byte progress) and `fetch_extract` (a Geofabrik area).
- `otc_validate(json, &error)`: the same checks as `otc_job_start` without
  starting a job, for forms that check as the user types.
- Packaging: an XCFramework for iOS and a JNI-ready `.so` per Android ABI
  (with the CI thread's iOS and Android builds), then the Swift package (F4)
  and Kotlin AAR (F5) as thin wrappers over these ten functions.
- On-device numbers (F6) and a low-memory mode for phones (P8).
