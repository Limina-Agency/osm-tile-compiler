//! Python bindings for osm-tile-compiler, the extension module
//! `osm_tile_compiler._native`. The package `osm_tile_compiler` re-exports
//! everything here; see its README for the Python API.
//!
//! Every function releases the GIL while it works, so other Python threads
//! keep running. The ones that take `progress` and `cancel` call back into
//! Python on the calling thread, holding the GIL only for the call. Reports
//! come back as dicts: the library's own serde output, converted with
//! `pythonize`.

use osm_tile_compiler::archive::{self, ArchiveReport, ExtractRequest};
use osm_tile_compiler::{
    ArchiveCenter, Area, AreaPolygon, BasemapOutput, BasemapRequest, ClipTo, CompileArea,
    CompileBounds, CompileError, CompileProgress, LayerSelection, OutputFormat, ReadScope,
    Replication, SourcesPolicy, SupplementalDataset, UpdateEvent, UpdateRequest, ZoomRange,
};
use pyo3::create_exception;
use pyo3::exceptions::PyException;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

create_exception!(
    osm_tile_compiler,
    Error,
    PyException,
    "Base class of every error the library raises. `kind` is the library's \
     stable identifier (`cancelled`, `invalid_request`, `download`, \
     `out_of_space`, `failed`, `archive_invalid`); `stage` is the compile \
     stage a failure happened in, or None."
);
create_exception!(
    osm_tile_compiler,
    Cancelled,
    Error,
    "The cancel token or callback asked to stop; nothing was left at the output."
);
create_exception!(
    osm_tile_compiler,
    InvalidRequest,
    Error,
    "The request cannot run as given: found before any work started."
);
create_exception!(
    osm_tile_compiler,
    DownloadError,
    Error,
    "Downloading or verifying a supplemental dataset, an extract or a diff failed."
);
create_exception!(
    osm_tile_compiler,
    OutOfSpace,
    Error,
    "A write failed because the disk is full."
);
create_exception!(
    osm_tile_compiler,
    Failed,
    Error,
    "Anything else, with the compile stage it happened in, if one had started."
);
create_exception!(
    osm_tile_compiler,
    ArchiveInvalid,
    Error,
    "`verify` found problems: `report` is the full `inspect` report."
);

/// A flag another thread sets to stop a running call. Checking it needs no
/// GIL, so a call stops within the library's own latency (about a second in
/// a compile) however busy the Python side is.
#[pyclass(frozen, module = "osm_tile_compiler")]
#[derive(Default)]
struct CancelToken {
    cancelled: AtomicBool,
}

#[pymethods]
impl CancelToken {
    #[new]
    fn new() -> Self {
        Self::default()
    }

    /// Asks every call holding this token to stop.
    fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }

    /// Whether `cancel` was called.
    #[getter]
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn __repr__(&self) -> String {
        format!(
            "CancelToken(cancelled={})",
            if self.cancelled() { "True" } else { "False" }
        )
    }
}

/// One progress report: `stage` (a compile stage id such as
/// `"read_osm"`, or an update step), `message` in words fit to show,
/// `fraction` of the current stage done (0 to 1, or None when it cannot be
/// told), and `overall`, the fraction of the whole call done, when known.
#[pyclass(frozen, get_all, module = "osm_tile_compiler")]
#[derive(Debug, Clone)]
struct Progress {
    stage: String,
    message: String,
    fraction: Option<f64>,
    overall: Option<f64>,
}

#[pymethods]
impl Progress {
    fn __repr__(&self) -> String {
        let number = |value: Option<f64>| value.map_or("None".to_string(), |v| v.to_string());
        format!(
            "Progress(stage={:?}, message={:?}, fraction={}, overall={})",
            self.stage,
            self.message,
            number(self.fraction),
            number(self.overall)
        )
    }
}

impl From<&CompileProgress> for Progress {
    fn from(progress: &CompileProgress) -> Self {
        Self {
            stage: progress.stage.id().to_string(),
            message: progress.message.clone(),
            fraction: progress.fraction,
            overall: Some(progress.overall),
        }
    }
}

/// How a call is asked to stop.
enum Cancel {
    Never,
    Token(Py<CancelToken>),
    /// A callable returning a truthy value to stop.
    Callable(Py<PyAny>),
}

impl Cancel {
    fn from_arg(cancel: Option<Bound<'_, PyAny>>) -> PyResult<Self> {
        let Some(cancel) = cancel else {
            return Ok(Self::Never);
        };
        if let Ok(token) = cancel.cast::<CancelToken>() {
            return Ok(Self::Token(token.clone().unbind()));
        }
        if cancel.is_callable() {
            return Ok(Self::Callable(cancel.unbind()));
        }
        Err(pyo3::exceptions::PyTypeError::new_err(
            "cancel must be a CancelToken or a callable returning a bool",
        ))
    }
}

/// The Python side of one call: its callbacks, and the first exception one
/// raised, which stops the call and is raised in place of its result.
struct Hooks {
    progress: Option<Py<PyAny>>,
    cancel: Cancel,
    raised: Mutex<Option<PyErr>>,
}

impl Hooks {
    fn new(progress: Option<Bound<'_, PyAny>>, cancel: Option<Bound<'_, PyAny>>) -> PyResult<Self> {
        if let Some(progress) = &progress
            && !progress.is_callable()
        {
            return Err(pyo3::exceptions::PyTypeError::new_err(
                "progress must be a callable taking a Progress",
            ));
        }
        Ok(Self {
            progress: progress.map(Bound::unbind),
            cancel: Cancel::from_arg(cancel)?,
            raised: Mutex::new(None),
        })
    }

    fn raise(&self, error: PyErr) {
        let mut raised = self.raised.lock().unwrap_or_else(|e| e.into_inner());
        raised.get_or_insert(error);
    }

    fn has_raised(&self) -> bool {
        self.raised
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_some()
    }

    /// Hands `progress` to the callback. Once one has raised, the call is
    /// stopping and later reports are dropped.
    fn report(&self, progress: Progress) {
        let Some(callback) = &self.progress else {
            return;
        };
        if self.has_raised() {
            return;
        }
        Python::attach(|py| {
            if let Err(error) = callback.call1(py, (progress,)) {
                self.raise(error);
            }
        });
    }

    /// Whether to stop: the token or callback says so, a callback raised, or
    /// a signal handler raised (Ctrl-C's KeyboardInterrupt, on the main
    /// thread).
    fn should_cancel(&self) -> bool {
        if self.has_raised() {
            return true;
        }
        if let Cancel::Token(token) = &self.cancel
            && token.get().cancelled()
        {
            return true;
        }
        Python::attach(|py| {
            if let Err(error) = py.check_signals() {
                self.raise(error);
                return true;
            }
            let Cancel::Callable(callback) = &self.cancel else {
                return false;
            };
            match callback
                .call0(py)
                .and_then(|answer| answer.bind(py).is_truthy())
            {
                Ok(answer) => answer,
                Err(error) => {
                    self.raise(error);
                    true
                }
            }
        })
    }

    /// The call's outcome: the exception a callback raised, if any, else the
    /// library's result.
    fn finish<T>(self, py: Python<'_>, result: Result<T, CompileError>) -> PyResult<T> {
        let raised = self.raised.into_inner().unwrap_or_else(|e| e.into_inner());
        match (raised, result) {
            (Some(error), _) => Err(error),
            (None, result) => result.map_err(|error| to_py_err(py, &error)),
        }
    }
}

fn to_py_err(py: Python<'_>, error: &CompileError) -> PyErr {
    let message = error.message().to_string();
    let (error_type, stage) = match error {
        CompileError::Cancelled => (py.get_type::<Cancelled>(), None),
        CompileError::InvalidRequest(_) => (py.get_type::<InvalidRequest>(), None),
        CompileError::Download(_) => (py.get_type::<DownloadError>(), None),
        CompileError::OutOfSpace(_) => (py.get_type::<OutOfSpace>(), None),
        CompileError::Failed { stage, .. } => (py.get_type::<Failed>(), stage.map(|s| s.id())),
        _ => (py.get_type::<Error>(), None),
    };
    let py_error = PyErr::from_type(error_type, message);
    annotate(py, &py_error, error.kind(), stage);
    py_error
}

/// Sets the `kind` and `stage` attributes every library error carries.
fn annotate(py: Python<'_>, error: &PyErr, kind: &str, stage: Option<&str>) {
    let value = error.value(py);
    // Exception instances take attributes; failing here would hide the error.
    let _ = value.setattr("kind", kind);
    let _ = value.setattr("stage", stage);
}

fn invalid(message: impl Into<String>) -> CompileError {
    CompileError::InvalidRequest(message.into())
}

/// A box given as `(west, south, east, north)`, the order the reports use.
fn bounds(bounds: (f64, f64, f64, f64)) -> CompileBounds {
    let (west, south, east, north) = bounds;
    CompileBounds::new(north, south, east, west)
}

fn bounds_list(bounds: CompileBounds) -> [f64; 4] {
    [bounds.west, bounds.south, bounds.east, bounds.north]
}

fn zoom_range(min: Option<u8>, max: Option<u8>) -> Result<ZoomRange, CompileError> {
    let all = ZoomRange::ALL;
    ZoomRange::new(min.unwrap_or(all.min()), max.unwrap_or(all.max()))
}

fn to_python(py: Python<'_>, value: &impl serde::Serialize) -> PyResult<Py<PyAny>> {
    let value = serde_json::to_value(value)
        .map_err(|e| PyErr::new::<Failed, _>(format!("cannot convert the report: {e}")))?;
    Ok(pythonize::pythonize(py, &value)
        .map_err(|e| PyErr::new::<Failed, _>(format!("cannot convert the report: {e}")))?
        .unbind())
}

/// The fields of `compile` that are not the four paths, to build its
/// request from.
struct CompileOptions {
    extra_inputs: Vec<PathBuf>,
    format: Option<String>,
    sources_policy: String,
    omit_sources: Vec<String>,
    bounds: Option<(f64, f64, f64, f64)>,
    polygon: Option<PathBuf>,
    read: String,
    center: Option<(f64, f64, u8)>,
    name: Option<String>,
    description: Option<String>,
    attribution: Option<String>,
    gzip_level: Option<u32>,
    min_zoom: Option<u8>,
    max_zoom: Option<u8>,
    threads: Option<usize>,
    layers: Option<Vec<String>>,
    exclude_layers: Option<Vec<String>>,
    languages: Option<Vec<String>>,
    building_levels: bool,
    search_index: bool,
    tile_format: String,
    sar: bool,
    sort_memory: Option<u64>,
    schema: Option<String>,
    data_layers: Vec<osm_tile_compiler::DataLayer>,
}

fn basemap_request(
    input_pbf: PathBuf,
    output: PathBuf,
    sources: PathBuf,
    temp_dir: PathBuf,
    options: CompileOptions,
) -> Result<BasemapRequest, CompileError> {
    let mut request = BasemapRequest::new(input_pbf, output, sources, temp_dir);
    request.extra_inputs = options.extra_inputs;
    if let Some(id) = options.format {
        request.format = Some(OutputFormat::from_id(&id).ok_or_else(|| {
            invalid(format!(
                "unknown output format '{id}': it is pmtiles or mbtiles"
            ))
        })?);
    }
    request.sources_policy = match options.sources_policy.as_str() {
        "use_present" => SourcesPolicy::UsePresent,
        "fetch_missing" => SourcesPolicy::FetchMissing,
        "fetch_and_verify" => SourcesPolicy::FetchAndVerify,
        other => {
            return Err(invalid(format!(
                "unknown sources_policy '{other}': it is use_present, fetch_missing or \
                 fetch_and_verify"
            )));
        }
    };
    request.omit_sources = if options.omit_sources.iter().any(|id| id == "all") {
        SupplementalDataset::ALL.to_vec()
    } else {
        options
            .omit_sources
            .iter()
            .map(|id| {
                SupplementalDataset::from_id(id).ok_or_else(|| {
                    let known: Vec<_> = SupplementalDataset::ALL.iter().map(|d| d.id()).collect();
                    invalid(format!(
                        "unknown supplemental dataset '{id}': it is one of {} or all",
                        known.join(", ")
                    ))
                })
            })
            .collect::<Result<_, _>>()?
    };
    let mut area = Area::whole_extract();
    area.bounds = options.bounds.map(bounds);
    area.polygon = options.polygon.map(AreaPolygon::File);
    area.read = match options.read.as_str() {
        "whole_extract" => ReadScope::WholeExtract,
        "near_area" => ReadScope::NearArea,
        other => {
            return Err(invalid(format!(
                "unknown read scope '{other}': it is whole_extract or near_area"
            )));
        }
    };
    request.area = area;
    request.center = options
        .center
        .map(|(longitude, latitude, zoom)| ArchiveCenter::new(longitude, latitude, zoom));
    let archive = &mut request.archive;
    if let Some(name) = options.name {
        archive.name = name;
    }
    if let Some(description) = options.description {
        archive.description = description;
    }
    if let Some(attribution) = options.attribution {
        archive.attribution = attribution;
    }
    if let Some(level) = options.gzip_level {
        archive.gzip_level = level;
    }
    request.zoom = zoom_range(options.min_zoom, options.max_zoom)?;
    if let Some(threads) = options.threads {
        request.threads = threads;
    }
    request.layers = match (options.layers, options.exclude_layers) {
        (Some(_), Some(_)) => {
            return Err(invalid("give layers or exclude_layers, not both"));
        }
        (Some(layers), None) => LayerSelection::only(layers),
        (None, Some(layers)) => LayerSelection::except(layers),
        (None, None) => LayerSelection::All,
    };
    request.languages = options.languages;
    request.building_levels = options.building_levels;
    request.search_index = options.search_index;
    request.archive.tile_format = osm_tile_compiler::TileFormat::from_id(&options.tile_format)
        .ok_or_else(|| {
            invalid(format!(
                "unknown tile_format '{}': it is mvt or mlt",
                options.tile_format
            ))
        })?;
    request.sar = options.sar;
    request.sort_memory = options.sort_memory;
    if let Some(schema) = &options.schema {
        request.schema =
            osm_tile_compiler::TileSchema::from_name_or_path(schema).map_err(invalid)?;
    }
    request.data_layers = options.data_layers;
    request.validate()?;
    Ok(request)
}

fn compile_output(output: &BasemapOutput) -> Value {
    json!({
        "archive": output.archive,
        "format": output.format.id(),
        "bounds": bounds_list(output.bounds),
        "building_levels": output.building_levels.as_ref().map(|(path, _)| path),
        "search_index": output.search_index.as_ref().map(|(path, _)| path),
        "result": output.result,
        "supplemental_inputs": output.supplemental_inputs,
        "omitted_sources": output.omitted_sources.iter().map(|d| d.id()).collect::<Vec<_>>(),
        "manifest": output.manifest(),
    })
}

/// The data layers a Python list of dicts describes, as the Rust
/// `DataLayer` reads them from JSON.
fn data_layers_from(
    py: Python<'_>,
    layers: &Bound<'_, PyAny>,
) -> PyResult<Vec<osm_tile_compiler::DataLayer>> {
    let json = py.import("json")?;
    let options = pyo3::types::PyDict::new(py);
    // Paths may be pathlib.Path objects.
    options.set_item("default", py.import("builtins")?.getattr("str")?)?;
    let text: String = json
        .call_method("dumps", (layers,), Some(&options))?
        .extract()?;
    serde_json::from_str(&text)
        .map_err(|error| to_py_err(py, &invalid(format!("data_layers: {error}"))))
}

/// Compiles an `.osm.pbf` extract into an OpenMapTiles basemap archive
/// (PMTiles or MBTiles, by `format` or the output's extension).
#[pyfunction]
#[pyo3(signature = (
    input_pbf, output, sources, temp_dir, *,
    extra_inputs = Vec::new(), format = None, sources_policy = "use_present".to_string(),
    omit_sources = Vec::new(), bounds = None, polygon = None,
    read = "whole_extract".to_string(), center = None, name = None, description = None,
    attribution = None, gzip_level = None, min_zoom = None, max_zoom = None, threads = None,
    layers = None, exclude_layers = None, languages = None, building_levels = false,
    search_index = false, tile_format = "mvt".to_string(), sar = false, sort_memory = None, schema = None,
    data_layers = None, progress = None, cancel = None,
))]
#[allow(clippy::too_many_arguments)]
fn compile(
    py: Python<'_>,
    input_pbf: PathBuf,
    output: PathBuf,
    sources: PathBuf,
    temp_dir: PathBuf,
    extra_inputs: Vec<PathBuf>,
    format: Option<String>,
    sources_policy: String,
    omit_sources: Vec<String>,
    bounds: Option<(f64, f64, f64, f64)>,
    polygon: Option<PathBuf>,
    read: String,
    center: Option<(f64, f64, u8)>,
    name: Option<String>,
    description: Option<String>,
    attribution: Option<String>,
    gzip_level: Option<u32>,
    min_zoom: Option<u8>,
    max_zoom: Option<u8>,
    threads: Option<usize>,
    layers: Option<Vec<String>>,
    exclude_layers: Option<Vec<String>>,
    languages: Option<Vec<String>>,
    building_levels: bool,
    search_index: bool,
    tile_format: String,
    sar: bool,
    sort_memory: Option<u64>,
    schema: Option<String>,
    data_layers: Option<Bound<'_, PyAny>>,
    progress: Option<Bound<'_, PyAny>>,
    cancel: Option<Bound<'_, PyAny>>,
) -> PyResult<Py<PyAny>> {
    let data_layers = match data_layers {
        Some(layers) => data_layers_from(py, &layers)?,
        None => Vec::new(),
    };
    let options = CompileOptions {
        extra_inputs,
        format,
        sources_policy,
        omit_sources,
        bounds,
        polygon,
        read,
        center,
        name,
        description,
        attribution,
        gzip_level,
        min_zoom,
        max_zoom,
        threads,
        layers,
        exclude_layers,
        languages,
        building_levels,
        search_index,
        tile_format,
        sar,
        sort_memory,
        schema,
        data_layers,
    };
    let request = basemap_request(input_pbf, output, sources, temp_dir, options)
        .map_err(|error| to_py_err(py, &error))?;
    let hooks = Hooks::new(progress, cancel)?;
    let result = py.detach(|| {
        osm_tile_compiler::compile_basemap(
            &request,
            |progress| hooks.report(progress.into()),
            || hooks.should_cancel(),
        )
    });
    let output = hooks.finish(py, result)?;
    to_python(py, &compile_output(&output))
}

/// Cuts the tiles of a PMTiles archive that meet a box, a polygon and a
/// zoom range into a new archive.
#[pyfunction]
#[pyo3(signature = (source, output, *, bounds = None, polygon = None, min_zoom = None, max_zoom = None, cancel = None))]
#[allow(clippy::too_many_arguments)]
fn extract(
    py: Python<'_>,
    source: PathBuf,
    output: PathBuf,
    bounds: Option<(f64, f64, f64, f64)>,
    polygon: Option<PathBuf>,
    min_zoom: Option<u8>,
    max_zoom: Option<u8>,
    cancel: Option<Bound<'_, PyAny>>,
) -> PyResult<Py<PyAny>> {
    let hooks = Hooks::new(None, cancel)?;
    let mut request = ExtractRequest::new(source, output);
    request.bounds = bounds.map(self::bounds);
    let setup = (|| {
        if let Some(polygon) = &polygon {
            request.polygon = Some(CompileArea::read(polygon)?);
        }
        if min_zoom.is_some() || max_zoom.is_some() {
            // Without `max_zoom`, up to whatever the source holds.
            let max = max_zoom.unwrap_or(ZoomRange::LIMIT);
            request.zoom = Some(ZoomRange::new(min_zoom.unwrap_or(0).min(max), max)?);
        }
        Ok(())
    })();
    setup.map_err(|error: CompileError| to_py_err(py, &error))?;
    let result = py.detach(|| archive::extract(&request, || hooks.should_cancel()));
    let report = hooks.finish(py, result)?;
    to_python(py, &report)
}

fn update_progress(event: &UpdateEvent<'_>) -> Progress {
    let progress = |stage: &str, message: String, fraction| Progress {
        stage: stage.to_string(),
        message,
        fraction,
        overall: None,
    };
    match event {
        UpdateEvent::Scanned { objects, sorted } => progress(
            "scan",
            format!(
                "Read {objects} objects{}",
                if *sorted {
                    ""
                } else {
                    " (not sorted by type and id: new objects go last)"
                }
            ),
            Some(1.0),
        ),
        UpdateEvent::Replication {
            url,
            latest,
            first,
            last,
        } => progress(
            "replication",
            if first > last {
                format!("{url} is at {latest}: nothing newer than the input")
            } else {
                format!("{url} is at {latest}: applying diffs {first} to {last}")
            },
            None,
        ),
        UpdateEvent::Read {
            name,
            index,
            total,
            report,
        } => progress(
            "read_changes",
            format!(
                "[{}/{total}] {name}: kept {} of {} changes",
                index + 1,
                report.kept,
                report.objects
            ),
            Some((index + 1) as f64 / (*total).max(1) as f64),
        ),
        UpdateEvent::Applying { changes } => {
            progress("apply", format!("Applying {changes} changes"), None)
        }
    }
}

/// Writes `input` with OSM change files and downloaded replication diffs
/// applied to `output`. Experimental, like the library's `update_extract`.
#[pyfunction]
#[pyo3(signature = (
    input, output, *, change_files = Vec::new(), replication = None, until = None,
    max_diffs = None, clip = None, threads = None, progress = None, cancel = None,
))]
#[allow(clippy::too_many_arguments)]
fn update(
    py: Python<'_>,
    input: PathBuf,
    output: PathBuf,
    change_files: Vec<PathBuf>,
    replication: Option<String>,
    until: Option<u64>,
    max_diffs: Option<u64>,
    clip: Option<Bound<'_, PyAny>>,
    threads: Option<usize>,
    progress: Option<Bound<'_, PyAny>>,
    cancel: Option<Bound<'_, PyAny>>,
) -> PyResult<Py<PyAny>> {
    let mut request = UpdateRequest::new(input, output);
    request.change_files = change_files;
    request.replication = replication
        .map(|text| Replication::parse(&text).map_err(invalid))
        .transpose()
        .map_err(|error| to_py_err(py, &error))?;
    request.until = until;
    if let Some(max_diffs) = max_diffs {
        request.max_diffs = max_diffs;
    }
    request.clip = update_clip(clip).map_err(|error| to_py_err(py, &error))?;
    if let Some(threads) = threads {
        request.threads = threads;
    }
    let hooks = Hooks::new(progress, cancel)?;
    let result = py.detach(|| {
        osm_tile_compiler::update_extract(
            &request,
            |event| hooks.report(update_progress(&event)),
            || hooks.should_cancel(),
        )
    });
    let report = hooks.finish(py, result)?;
    to_python(py, &report)
}

/// `clip`: None or `"header"` for the PBF header's box, `"none"` to keep
/// every change, or a `(west, south, east, north)` box.
fn update_clip(clip: Option<Bound<'_, PyAny>>) -> Result<ClipTo, CompileError> {
    let Some(clip) = clip else {
        return Ok(ClipTo::HeaderBounds { fallback: None });
    };
    if let Ok(text) = clip.extract::<String>() {
        return match text.as_str() {
            "header" => Ok(ClipTo::HeaderBounds { fallback: None }),
            "none" => Ok(ClipTo::Nothing),
            other => Err(invalid(format!(
                "unknown clip '{other}': give \"header\", \"none\" or a (west, south, east, \
                 north) box"
            ))),
        };
    }
    clip.extract::<(f64, f64, f64, f64)>()
        .map(|b| ClipTo::Bounds(bounds(b)))
        .map_err(|_| {
            invalid("clip must be \"header\", \"none\" or a (west, south, east, north) box")
        })
}

/// Writes the delta that turns the archive `base` into `target` to `output`.
#[pyfunction]
fn diff(py: Python<'_>, base: PathBuf, target: PathBuf, output: PathBuf) -> PyResult<Py<PyAny>> {
    let result = py.detach(|| archive::diff(&base, &target, &output));
    let report = result.map_err(|error| to_py_err(py, &error))?;
    to_python(py, &report)
}

/// Applies the delta at `delta` to the archive `base`, writing `output`.
#[pyfunction]
fn apply(py: Python<'_>, base: PathBuf, delta: PathBuf, output: PathBuf) -> PyResult<Py<PyAny>> {
    let result = py.detach(|| archive::apply(&base, &delta, &output));
    let report = result.map_err(|error| to_py_err(py, &error))?;
    to_python(py, &report)
}

/// The manifest at the start of a delta.
#[pyfunction]
fn read_delta_manifest(py: Python<'_>, path: PathBuf) -> PyResult<Py<PyAny>> {
    let result = py.detach(|| archive::read_delta_manifest(&path));
    let manifest = result.map_err(|error| to_py_err(py, &error))?;
    to_python(py, &manifest)
}

fn read_report(py: Python<'_>, path: &Path) -> PyResult<ArchiveReport> {
    py.detach(|| archive::inspect(path))
        .map_err(|error| to_py_err(py, &error))
}

/// Reads every part of a PMTiles archive. Raises only when it is not a
/// readable PMTiles v3 archive; anything else wrong is in `problems`.
#[pyfunction]
fn inspect(py: Python<'_>, path: PathBuf) -> PyResult<Py<PyAny>> {
    let report = read_report(py, &path)?;
    to_python(py, &report)
}

/// `inspect`, raising `ArchiveInvalid` when the report lists problems.
#[pyfunction]
fn verify(py: Python<'_>, path: PathBuf) -> PyResult<Py<PyAny>> {
    let report = read_report(py, &path)?;
    let report_dict = to_python(py, &report)?;
    if report.problem_count == 0 {
        return Ok(report_dict);
    }
    let unlisted = report.problem_count - report.problems.len() as u64;
    let mut message = format!(
        "{} has {} problems{}",
        path.display(),
        report.problem_count,
        if unlisted > 0 {
            format!(" ({unlisted} not listed)")
        } else {
            String::new()
        }
    );
    if let Some(first) = report.problems.first() {
        message.push_str(&format!("; the first: {first}"));
    }
    let error = PyErr::new::<ArchiveInvalid, _>(message);
    annotate(py, &error, "archive_invalid", None);
    let _ = error.value(py).setattr("report", report_dict);
    Err(error)
}

#[pymodule]
fn _native(module: &Bound<'_, PyModule>) -> PyResult<()> {
    let py = module.py();
    module.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module.add("OMT_SCHEMA_VERSION", osm_tile_compiler::OMT_SCHEMA_VERSION)?;
    module.add("LAYERS", osm_tile_compiler::OMT_LAYERS.to_vec())?;
    module.add("LANGUAGES", osm_tile_compiler::OMT_LANGUAGES.to_vec())?;
    module.add(
        "SUPPLEMENTAL_DATASETS",
        SupplementalDataset::ALL
            .iter()
            .map(|d| d.id())
            .collect::<Vec<_>>(),
    )?;
    let stages = PyDict::new(py);
    for stage in osm_tile_compiler::CompileStage::ALL {
        stages.set_item(stage.id(), stage.weight())?;
    }
    module.add("COMPILE_STAGES", stages)?;
    module.add_class::<CancelToken>()?;
    module.add_class::<Progress>()?;
    module.add("Error", py.get_type::<Error>())?;
    module.add("Cancelled", py.get_type::<Cancelled>())?;
    module.add("InvalidRequest", py.get_type::<InvalidRequest>())?;
    module.add("DownloadError", py.get_type::<DownloadError>())?;
    module.add("OutOfSpace", py.get_type::<OutOfSpace>())?;
    module.add("Failed", py.get_type::<Failed>())?;
    module.add("ArchiveInvalid", py.get_type::<ArchiveInvalid>())?;
    module.add_function(wrap_pyfunction!(compile, module)?)?;
    module.add_function(wrap_pyfunction!(extract, module)?)?;
    module.add_function(wrap_pyfunction!(update, module)?)?;
    module.add_function(wrap_pyfunction!(diff, module)?)?;
    module.add_function(wrap_pyfunction!(apply, module)?)?;
    module.add_function(wrap_pyfunction!(read_delta_manifest, module)?)?;
    module.add_function(wrap_pyfunction!(inspect, module)?)?;
    module.add_function(wrap_pyfunction!(verify, module)?)?;
    Ok(())
}
