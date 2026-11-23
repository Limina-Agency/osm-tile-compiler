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
  collecting a result work the same for a compile, a dataset download or,
  later, an extract download or a delta apply. Wrappers write that once.

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
`stage` is one of `CompileStage::id`'s identifiers, `fraction` the stage's
share done (or `null`), and `overall` the whole job's (1 once it succeeded).
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
  "buildingLevels": false
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
PMTiles.

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
sprite paths, which MapLibre Native needs. Progress reports `Fetched N of M
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

## Versioning

`OTC_ABI_VERSION` covers the function signatures, the status and state
numbers, and the envelope, progress, result and error shapes. It changes only
when a host written for the old version would misbehave; new optional fields
and new request kinds do not change it. Request fields follow the crate's
semantic versioning, like the Rust API.

## Planned next

- Request kinds: `fetch_sources` (download the supplemental datasets ahead of
  a compile, with byte progress), `fetch_extract` (a Geofabrik area), then
  `verify`/`inspect` and delta `apply`.
- `otc_validate(json, &error)`: the same checks as `otc_job_start` without
  starting a job, for forms that check as the user types.
- Packaging: an XCFramework for iOS and a JNI-ready `.so` per Android ABI
  (with the CI thread's iOS and Android builds), then the Swift package (F4)
  and Kotlin AAR (F5) as thin wrappers over these ten functions.
- On-device numbers (F6) and a low-memory mode for phones (P8).
