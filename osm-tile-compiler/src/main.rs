#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

mod cli;
use osm_tile_compiler::{
    BasemapRequest, CompileError, CompileStage, SourcesPolicy, SupplementalDataset, compile_basemap,
};
use serde::Deserialize;
use serde_json::json;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

/// The worker protocol: the request JSON and the JSON lines on stdout. See
/// `docs/formats.md` for when it changes.
const PROTOCOL_VERSION: u32 = 1;

/// Raised when the host closes stdin: the compile stops and cleans up.
static CANCELLED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompilerProcessRequest {
    input_pbf_path: PathBuf,
    sources_dir: PathBuf,
    output_pmtiles_path: PathBuf,
    temp_dir: PathBuf,
    manifest_path: PathBuf,
    /// A GeoJSON or Osmosis `.poly` file limiting the compile to its area.
    #[serde(default)]
    polygon_path: Option<PathBuf>,
    /// Read only the OSM data in and near the polygon [default: no].
    #[serde(default)]
    area_only: bool,
    /// Layers to write [default: all]; not together with `excludeLayers`.
    #[serde(default)]
    layers: Option<Vec<String>>,
    /// Layers to leave out.
    #[serde(default)]
    exclude_layers: Option<Vec<String>>,
    /// `name:<lang>` languages to write, as `--languages` takes them
    /// [default: all the schema lists].
    #[serde(default)]
    languages: Option<Vec<String>>,
    /// Supplemental datasets to compile without, by id (`water_polygons`,
    /// `natural_earth`, `lake_centerlines`); they are not downloaded.
    #[serde(default)]
    omit_sources: Vec<String>,
    /// Whether to write the building levels beside the archive
    /// [default: no].
    #[serde(default)]
    building_levels: bool,
}

fn emit(event: serde_json::Value) -> Result<(), String> {
    let stdout = io::stdout();
    let mut output = stdout.lock();
    serde_json::to_writer(&mut output, &event)
        .map_err(|error| format!("Failed to encode compiler event: {error}"))?;
    output
        .write_all(b"\n")
        .map_err(|error| format!("Failed to write compiler event: {error}"))?;
    output
        .flush()
        .map_err(|error| format!("Failed to flush compiler event: {error}"))
}

fn progress(message: &str, percent: Option<f64>) {
    let _ = emit(json!({
        "protocolVersion": PROTOCOL_VERSION, "event": "progress",
        "stage": message, "percent": percent, "message": message,
    }));
}

/// Why the worker stopped: the error event's `message` and `errorKind`
/// (one of [`CompileError::kind`]'s identifiers).
struct WorkerError {
    kind: &'static str,
    message: String,
}

impl WorkerError {
    fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: "invalid_request",
            message: message.into(),
        }
    }
}

impl From<String> for WorkerError {
    fn from(message: String) -> Self {
        Self {
            kind: "failed",
            message,
        }
    }
}

impl From<CompileError> for WorkerError {
    fn from(error: CompileError) -> Self {
        Self {
            kind: error.kind(),
            message: error.to_string(),
        }
    }
}

fn run() -> Result<(), WorkerError> {
    const USAGE: &str = "usage: osm-tile-compiler [--parent-stdin] --request <request.json>";
    let mut args = std::env::args_os().skip(1).peekable();
    if args.peek().map(|arg| arg.as_os_str()) == Some(std::ffi::OsStr::new("--parent-stdin")) {
        args.next();
        // The native host holds stdin open for the lifetime of this job. EOF
        // also ends the worker if the host crashes or is force-quit: the
        // compile is cancelled, which removes its partial outputs, and the
        // process exits anyway if that takes too long.
        std::thread::spawn(|| {
            let mut byte = [0_u8; 1];
            while let Ok(count) = io::stdin().read(&mut byte) {
                if count == 0 {
                    break;
                }
            }
            CANCELLED.store(true, Ordering::Relaxed);
            std::thread::sleep(Duration::from_secs(10));
            std::process::exit(130);
        });
    }
    if args.next().as_deref() != Some(std::ffi::OsStr::new("--request")) {
        return Err(WorkerError::invalid(USAGE));
    }
    let Some(request_path) = args.next().map(PathBuf::from) else {
        return Err(WorkerError::invalid(USAGE));
    };
    if args.next().is_some() {
        return Err(WorkerError::invalid(USAGE));
    }
    let bytes = std::fs::read(&request_path).map_err(|error| {
        WorkerError::invalid(format!(
            "Failed to read compiler request {request_path:?}: {error}"
        ))
    })?;
    let request: CompilerProcessRequest = serde_json::from_slice(&bytes).map_err(|error| {
        WorkerError::invalid(format!(
            "Invalid compiler request {request_path:?}: {error}"
        ))
    })?;
    let omit_sources = request
        .omit_sources
        .iter()
        .map(|id| {
            SupplementalDataset::from_id(id)
                .ok_or_else(|| WorkerError::invalid(format!("unknown supplemental dataset '{id}'")))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut compile = BasemapRequest::new(
        request.input_pbf_path,
        request.output_pmtiles_path,
        request.sources_dir,
        request.temp_dir,
    );
    compile.sources_policy = SourcesPolicy::FetchAndVerify;
    compile.omit_sources = omit_sources;
    compile.polygon = request.polygon_path;
    compile.area_only = request.area_only;
    compile.layers = request.layers;
    compile.exclude_layers = request.exclude_layers;
    compile.languages = request.languages;
    compile.building_levels = request.building_levels;
    // Each stage is reported once, as before; downloads also with their
    // fraction.
    let mut last = None;
    let output = compile_basemap(
        &compile,
        |report| {
            let downloading = report.stage == CompileStage::FetchSources;
            if last.as_ref() != Some(&report.message) {
                progress(&report.message, report.fraction.filter(|_| downloading));
                last = Some(report.message.clone());
            } else if downloading && report.fraction.is_some() {
                progress(&report.message, report.fraction);
            }
        },
        || CANCELLED.load(Ordering::Relaxed),
    )?;
    std::fs::write(
        &request.manifest_path,
        serde_json::to_vec_pretty(&output.manifest()).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("Failed to write compiler manifest: {e}"))?;
    emit(json!({
        "protocolVersion": PROTOCOL_VERSION,
        "event": "complete",
        "tilesWritten": output.result.tiles,
        "featuresEmitted": output.result.profile_features,
    }))?;
    Ok(())
}

fn main() {
    // The desktop app's worker protocol owns any invocation that starts with
    // its flags; everything else is the command line.
    let worker = std::env::args_os().nth(1).is_some_and(|arg| {
        let arg = arg.to_string_lossy();
        arg.starts_with("--request") || arg.starts_with("--parent-stdin")
    });
    if !worker {
        std::process::exit(cli::main(std::env::args_os().skip(1).collect()));
    }
    // A panic that reaches here would otherwise end the worker without the
    // error event its host waits for; the panic hook still prints the location.
    let result = std::panic::catch_unwind(run).unwrap_or_else(|payload| {
        let message = osm_tile_compiler::panic_message(&*payload);
        Err(format!("The compiler crashed: {message}").into())
    });
    if let Err(WorkerError { kind, message }) = result {
        let _ = emit(json!({
            "protocolVersion": PROTOCOL_VERSION,
            "event": "error",
            "message": message,
            "errorKind": kind,
        }));
        std::process::exit(if CANCELLED.load(Ordering::Relaxed) {
            130
        } else {
            1
        });
    }
}
