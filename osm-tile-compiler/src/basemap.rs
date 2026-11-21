//! One call that does a whole compile, for apps that run it in their own
//! process, and for this crate's command line and worker.

use crate::building_levels::{BuildingLevelsReport, building_levels_path, has_building_layer};
use crate::control::{self, CompileError, CompileProgress, CompileStage, Control, RemoveOnDrop};
use crate::omt::{self, CompileResult, OpenMapTilesRequest};
use crate::request::{Area, LayerSelection, ReadScope, ZoomRange};
use crate::sources::{self, CachedInput, FetchEvent, FetchMode, SupplementalDataset};
use crate::{ArchiveCenter, CompileBounds, OutputFormat, archive::ArchiveOptions};
use serde_json::json;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// What to do about the supplemental datasets the compile reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum SourcesPolicy {
    /// Use what `sources` holds, unchecked; fail if a dataset not omitted is
    /// missing.
    UsePresent,
    /// Download the datasets missing from `sources`; keep the others
    /// unchecked.
    FetchMissing,
    /// Check every dataset against its recorded hash and download any that
    /// is missing or changed. Checking reads the 1.5 GB again.
    FetchAndVerify,
}

/// A compile of one PBF extract into a basemap.
///
/// Make one with [`BasemapRequest::new`] and set the fields that differ from
/// its defaults; more fields may be added in later versions, each with a
/// default that keeps today's behaviour.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct BasemapRequest {
    /// The `.osm.pbf` extract, read in place.
    pub input_pbf: PathBuf,
    /// The archive to write; it must not exist yet. Building levels, when
    /// asked for, go beside it ([`building_levels_path`]).
    pub output: PathBuf,
    /// The archive format [default: the one `output`'s extension names, see
    /// [`OutputFormat::from_path`]].
    pub format: Option<OutputFormat>,
    /// The directory of the supplemental datasets: downloads go there, and
    /// the first compile that reads one extracts it there too.
    pub sources: PathBuf,
    pub sources_policy: SourcesPolicy,
    /// Datasets to compile without: they are neither fetched nor read. The
    /// archive's metadata (`omitted_sources`) and the output list them; see
    /// [`SupplementalDataset::without_warning`] for what the map lacks.
    pub omit_sources: Vec<SupplementalDataset>,
    /// Scratch space. The compile works in a directory of its own inside it
    /// and removes that directory when it ends, however it ends.
    pub temp_dir: PathBuf,
    /// The part of the extract to compile, and how much of the extract to
    /// read for it [default: the whole extract, read in full].
    pub area: Area,
    /// Where viewers start [default: the middle of the area].
    pub center: Option<ArchiveCenter>,
    pub archive: ArchiveOptions,
    /// The zooms to write [default: 0 to 14].
    pub zoom: ZoomRange,
    /// The compile's threads. Its work runs on a rayon pool of this many
    /// threads; the PBF reader and the tile renderers start this many of
    /// their own as well, so up to about twice as many are busy at a time
    /// while the PBF is read.
    pub threads: usize,
    /// The layers to write [default: all].
    pub layers: LayerSelection,
    /// The `name:<lang>` languages to write, as [`resolve_languages`] takes
    /// them [default: all of [`OMT_LANGUAGES`]]. `name`, `name_en`,
    /// `name_de`, `name_int`, `name:latin` and `name:nonlatin` are written
    /// regardless.
    ///
    /// [`resolve_languages`]: crate::resolve_languages
    /// [`OMT_LANGUAGES`]: crate::OMT_LANGUAGES
    pub languages: Option<Vec<String>>,
    /// Whether to write the building levels beside the archive (when it
    /// has buildings at zoom 14) [default: no]. They serve renderers that
    /// draw far buildings as boxes; see [`write_building_levels`].
    ///
    /// [`write_building_levels`]: crate::write_building_levels
    pub building_levels: bool,
}

impl BasemapRequest {
    /// A full compile at zooms 0 to 14 on every core, with the supplemental
    /// datasets `sources` holds, and no building levels.
    pub fn new(
        input_pbf: impl Into<PathBuf>,
        output: impl Into<PathBuf>,
        sources: impl Into<PathBuf>,
        temp_dir: impl Into<PathBuf>,
    ) -> Self {
        Self {
            input_pbf: input_pbf.into(),
            output: output.into(),
            format: None,
            sources: sources.into(),
            sources_policy: SourcesPolicy::UsePresent,
            omit_sources: Vec::new(),
            temp_dir: temp_dir.into(),
            area: Area::default(),
            center: None,
            archive: ArchiveOptions::default(),
            zoom: ZoomRange::default(),
            threads: std::thread::available_parallelism().map_or(4, |n| n.get()),
            layers: LayerSelection::All,
            languages: None,
            building_levels: false,
        }
    }

    /// The archive format the compile writes: `format`, or the one the
    /// output's extension names.
    pub fn output_format(&self) -> OutputFormat {
        self.format
            .unwrap_or_else(|| OutputFormat::from_path(&self.output))
    }

    /// The datasets the compile reads.
    pub fn used_sources(&self) -> Vec<SupplementalDataset> {
        SupplementalDataset::ALL
            .into_iter()
            .filter(|dataset| !self.omit_sources.contains(dataset))
            .collect()
    }

    /// Checks everything about the request that needs no reading or
    /// network: the layers, languages, threads, gzip level, bounds, center
    /// and area, that building levels are asked for only with PMTiles, and
    /// that no output would be replaced.
    /// [`compile_basemap`] starts with the same checks, so this is for
    /// callers that want to refuse a request before starting it.
    pub fn validate(&self) -> Result<(), CompileError> {
        self.check().map(|_| ())
    }

    /// [`validate`](Self::validate), returning the layers as the profile
    /// takes them.
    fn check(&self) -> Result<Option<Vec<String>>, CompileError> {
        let invalid = CompileError::InvalidRequest;
        let layers = self.layers.names().map_err(invalid)?;
        if let Some(languages) = &self.languages {
            omt::resolve_languages(languages).map_err(invalid)?;
        }
        if self.threads == 0 {
            return Err(invalid("threads must be positive".into()));
        }
        if self.archive.gzip_level > 9 {
            return Err(invalid(format!(
                "the gzip level must be 0 to 9, got {}",
                self.archive.gzip_level
            )));
        }
        if let Some(bounds) = self.area.bounds {
            omt::check_bounds(bounds).map_err(invalid)?;
        }
        if let Some(center) = self.center {
            omt::check_center(center, self.zoom).map_err(invalid)?;
        }
        if self.building_levels && self.output_format() != OutputFormat::PmTiles {
            return Err(invalid(format!(
                "building levels are written beside PMTiles archives only, not {}",
                self.output_format().name()
            )));
        }
        if self.area.read == ReadScope::NearArea && !self.area.is_limited() {
            return Err(invalid(
                "reading only near the area needs bounds or a polygon".into(),
            ));
        }
        let levels = building_levels_path(&self.output);
        let archives = [Some(&self.output), self.building_levels.then_some(&levels)];
        for archive in archives.into_iter().flatten() {
            if archive.exists() {
                return Err(invalid(format!(
                    "refusing to replace existing archive {}",
                    archive.display()
                )));
            }
        }
        Ok(layers)
    }
}

/// What a compile wrote.
#[derive(Debug)]
#[non_exhaustive]
pub struct BasemapOutput {
    pub result: CompileResult,
    /// The archive: the request's `output`.
    pub archive: PathBuf,
    /// The archive's format.
    pub format: OutputFormat,
    /// The building levels and what they hold, if written.
    pub building_levels: Option<(PathBuf, BuildingLevelsReport)>,
    /// The area compiled.
    pub bounds: CompileBounds,
    /// Where each supplemental dataset read came from, when the request
    /// fetched or verified them; empty for [`SourcesPolicy::UsePresent`].
    pub supplemental_inputs: Vec<CachedInput>,
    /// The datasets left out, as the request asked.
    pub omitted_sources: Vec<SupplementalDataset>,
}

/// The manifest's `format`.
pub const MANIFEST_FORMAT: &str = "basemap-manifest";
/// The manifest's `formatVersion`. See `docs/formats.md` for when it changes.
pub const MANIFEST_VERSION: u32 = 1;

impl BasemapOutput {
    /// The manifest the desktop worker writes beside its output. `version`
    /// is the OpenMapTiles schema's; `formatVersion` is the manifest's own.
    /// `omittedSources` is there only when a dataset was left out.
    pub fn manifest(&self) -> serde_json::Value {
        let mut manifest = json!({
            "format": MANIFEST_FORMAT,
            "formatVersion": MANIFEST_VERSION,
            "schema": "OpenMapTiles",
            "version": omt::OMT_SCHEMA_VERSION,
            "result": self.result,
            "supplementalInputs": self.supplemental_inputs,
        });
        if !self.omitted_sources.is_empty() {
            manifest["omittedSources"] = json!(self.omitted_sources);
        }
        manifest
    }
}

/// Reads the PBF header's bounding box, normalized (a box crossing the
/// antimeridian has west above east).
pub fn pbf_bounds(input: &Path) -> Result<CompileBounds, CompileError> {
    read_pbf_bounds(input).map_err(CompileError::InvalidRequest)
}

fn read_pbf_bounds(input: &Path) -> Result<CompileBounds, String> {
    let index = osm_pbf_ingest::blob_index::PbfBlobIndex::scan(input)
        .map_err(|e| format!("Failed to inspect {}: {e}", input.display()))?;
    let b = index.header().bounds.ok_or_else(|| {
        format!(
            "{} has no bounding box in its header; the compile needs one",
            input.display()
        )
    })?;
    Ok(CompileBounds::new(b.north, b.south, b.east, b.west).normalized())
}

/// Compiles `request.input_pbf` into a basemap: fetches the supplemental
/// datasets as the request says, compiles, and writes the building levels
/// beside the archive if the request asks for them.
///
/// `progress` and `should_cancel` are called on the calling thread, which
/// waits for the compile while the work runs on a rayon pool of
/// `request.threads` threads built for it. `progress` gets every stage as it
/// starts and then the stage's fraction at most every 200 ms; `should_cancel`
/// is asked every 50 ms and right after each progress report. Once it
/// returns true the compile stops within about a second in every stage
/// (downloads: see [`sources::fetch_sources`]) and returns
/// [`CompileError::Cancelled`].
///
/// A cancelled or failed compile leaves neither the archive nor the building
/// levels, and removes its scratch directory. Downloads already complete,
/// and extracted datasets, stay in `sources` for the next compile.
///
/// The library prints nothing, never exits the process, and installs no
/// allocator; paths are used as given, so pass absolute ones.
///
/// That clean-up relies on unwinding: a panic in the compile is caught and
/// returned as [`CompileError::Failed`]. In a build with `panic = "abort"`,
/// common in mobile release profiles, a panic ends the app instead. Its
/// scratch directory (`<temp_dir>/compile-<pid>-<n>`) then stays until the
/// app clears `temp_dir`, for example at launch, before starting a compile;
/// the next compile to the same output removes the abandoned partial archive.
pub fn compile_basemap(
    request: &BasemapRequest,
    progress: impl FnMut(&CompileProgress),
    should_cancel: impl Fn() -> bool,
) -> Result<BasemapOutput, CompileError> {
    // Everything that needs no reading or network is checked first.
    let layers = request.check()?;
    control::run(
        request.threads,
        request.building_levels,
        progress,
        should_cancel,
        |control| compile(request, layers, control),
    )
}

fn compile(
    request: &BasemapRequest,
    layers: Option<Vec<String>>,
    control: &Control,
) -> Result<BasemapOutput, String> {
    let bounds = match request.area.bounds {
        Some(bounds) => bounds,
        None => read_pbf_bounds(&request.input_pbf)?,
    };
    let area = request
        .area
        .polygon
        .as_ref()
        .map(|polygon| polygon.load().map_err(|e| e.to_string()))
        .transpose()?;
    let mut compile_request = OpenMapTilesRequest {
        input_pbf: request.input_pbf.clone(),
        sources: request.sources.clone(),
        output: request.output.clone(),
        temp_dir: PathBuf::new(),
        bounds,
        area,
        center: request.center,
        archive: request.archive.clone(),
        min_zoom: request.zoom.min(),
        max_zoom: request.zoom.max(),
        threads: request.threads,
        layers,
        languages: request.languages.clone(),
        omit_sources: request.omit_sources.clone(),
        area_only: request.area.read == ReadScope::NearArea,
        format: request.output_format(),
    };
    omt::validate(&compile_request)?;
    let used = request.used_sources();
    let supplemental_inputs = fetch(request, &used, control)?;
    let missing = used
        .iter()
        .filter(|dataset| !dataset.is_present(&request.sources))
        .map(|dataset| dataset.file_name())
        .collect::<Vec<_>>();
    if !missing.is_empty() {
        return Err(format!(
            "{} is missing supplemental datasets: {}",
            request.sources.display(),
            missing.join(", ")
        ));
    }
    let scratch = Scratch::create(&request.temp_dir)?;
    compile_request.temp_dir = scratch.path.clone();
    // The archive is moved into place only when complete; it and the
    // levels are removed again if a later step fails or is cancelled.
    let archive_guard = RemoveOnDrop::file(request.output.clone());
    let result = omt::compile(&compile_request, control, |_| {});
    let result = match result {
        Ok(result) => result,
        Err(error) => {
            // The archive was never put in place: nothing to remove.
            archive_guard.keep();
            return Err(error);
        }
    };
    let mut building_levels = None;
    let mut levels_guard = None;
    if request.building_levels && has_building_layer(&request.output)? {
        control.stage(CompileStage::BuildingLevels, "Writing building levels")?;
        let path = building_levels_path(&request.output);
        let report =
            crate::building_levels::write_levels(&request.output, &path, request.threads, control)?;
        levels_guard = Some(RemoveOnDrop::file(path.clone()));
        building_levels = Some((path, report));
    }
    control.check()?;
    drop(scratch);
    archive_guard.keep();
    if let Some(guard) = levels_guard {
        guard.keep();
    }
    Ok(BasemapOutput {
        result,
        archive: request.output.clone(),
        format: request.output_format(),
        building_levels,
        bounds,
        supplemental_inputs,
        omitted_sources: SupplementalDataset::ALL
            .into_iter()
            .filter(|dataset| request.omit_sources.contains(dataset))
            .collect(),
    })
}

/// Fetches the datasets as the request's policy says, in the
/// `FetchSources` stage.
fn fetch(
    request: &BasemapRequest,
    used: &[SupplementalDataset],
    control: &Control,
) -> Result<Vec<CachedInput>, String> {
    let mode = match request.sources_policy {
        SourcesPolicy::UsePresent => return Ok(Vec::new()),
        SourcesPolicy::FetchMissing => FetchMode::OnlyMissing,
        SourcesPolicy::FetchAndVerify => FetchMode::Verify,
    };
    if used.is_empty() {
        return Ok(Vec::new());
    }
    control.stage(CompileStage::FetchSources, "Checking supplemental datasets")?;
    // The fraction: bytes checked or received over the datasets' sizes,
    // each counted at its approximate size until its real one is known.
    let sizes = used
        .iter()
        .map(|dataset| AtomicU64::new(dataset.approximate_bytes()))
        .collect::<Vec<_>>();
    let finished = AtomicU64::new(0);
    let index = |dataset| used.iter().position(|d| *d == dataset).unwrap_or(0);
    let report = |dataset: SupplementalDataset, bytes: u64, total: Option<u64>| {
        let i = index(dataset);
        if let Some(total) = total.filter(|total| *total > 0) {
            sizes[i].store(total, Ordering::Relaxed);
        }
        let all = sizes
            .iter()
            .map(|size| size.load(Ordering::Relaxed))
            .sum::<u64>();
        control.set_total(all);
        control.set_done(finished.load(Ordering::Relaxed) + bytes);
    };
    let megabytes = |bytes: u64| bytes / 1_000_000;
    sources::fetch(
        &request.sources,
        used,
        mode,
        |event| match event {
            FetchEvent::Checking {
                dataset,
                bytes,
                total,
            } => {
                control.message(format!("Checking {}", dataset.file_name()));
                report(dataset, bytes, Some(total));
            }
            FetchEvent::Downloading { dataset, .. } => {
                control.message(format!("Downloading {}", dataset.file_name()));
                report(dataset, 0, None);
            }
            FetchEvent::Received {
                dataset,
                bytes,
                total,
            } => {
                control.message(match total {
                    Some(total) => format!(
                        "Downloading {} ({} of {} MB)",
                        dataset.file_name(),
                        megabytes(bytes),
                        megabytes(total)
                    ),
                    None => format!(
                        "Downloading {} ({} MB)",
                        dataset.file_name(),
                        megabytes(bytes)
                    ),
                });
                report(dataset, bytes, total);
            }
            FetchEvent::Ready { dataset, bytes, .. } => {
                sizes[index(dataset)].store(bytes, Ordering::Relaxed);
                finished.fetch_add(bytes, Ordering::Relaxed);
                report(dataset, 0, None);
            }
        },
        &|| control.is_cancelled(),
    )
}

/// The compile's own scratch directory inside the request's, removed with
/// everything in it when dropped. The request's directory is removed too
/// if the compile made it and it is empty.
struct Scratch {
    path: PathBuf,
    parent: Option<PathBuf>,
}

impl Scratch {
    fn create(temp_dir: &Path) -> Result<Self, String> {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let made_parent = !temp_dir.exists();
        std::fs::create_dir_all(temp_dir)
            .map_err(|e| format!("Failed to create {}: {e}", temp_dir.display()))?;
        loop {
            let path = temp_dir.join(format!(
                "compile-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            match std::fs::create_dir(&path) {
                Ok(()) => {
                    return Ok(Self {
                        path,
                        parent: made_parent.then(|| temp_dir.to_path_buf()),
                    });
                }
                Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(e) => return Err(format!("Failed to create {}: {e}", path.display())),
            }
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
        if let Some(parent) = &self.parent {
            let _ = std::fs::remove_dir(parent);
        }
    }
}
