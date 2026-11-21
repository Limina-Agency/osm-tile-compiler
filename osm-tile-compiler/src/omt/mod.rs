//! Native Rust port of the OpenMapTiles 3.16 profile pinned by Planetiler 0.10.2.
//! This profile is experimental: see the parity ledger before treating output
//! as interchangeable with Planetiler. No Java or external GIS process is used.
#[cfg(test)]
mod f64_arrangement;
#[cfg(test)]
mod f64_buffer;
mod float_repair;
mod ingest;
mod input_area;
mod interior_point;
mod java_hashmap;
mod jts_buffer;
mod jts_intersection;
mod labels;
mod land;
mod line_merge;
mod pipeline;
mod polygon_validity;
mod raster_merge;
mod record_codec;
mod render;
mod repair;
mod selection;
mod simplify;
mod snap_round;
mod spill;
mod store;
mod supplements;
mod transport;
mod types;
mod water;

use crate::compile_area::{Region, TileCoverage};
use crate::control::{CompileStage, Control};
use crate::sources::SupplementalDataset;
use crate::{ArchiveCenter, CompileArea, CompileBounds, archive::ArchiveOptions};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::{sync::Arc, time::Duration, time::Instant};

pub use labels::LANGUAGES;
pub(crate) use selection::{resolve_languages, select_layers};

/// The OpenMapTiles schema version the archives follow, written as the
/// metadata's `version` as OpenMapTiles does.
pub const OMT_SCHEMA_VERSION: &str = "3.16.0";
/// The OpenMapTiles commit the profile ports, as pinned by Planetiler 0.10.2.
pub const PROFILE_REVISION: &str = "91516fdf477915b1a985015811049b9b96c7f87e";
/// The layers of the OpenMapTiles schema, in the order the archives hold
/// them.
pub const LAYERS: [&str; 16] = [
    "water",
    "waterway",
    "landcover",
    "landuse",
    "mountain_peak",
    "park",
    "boundary",
    "aeroway",
    "transportation",
    "building",
    "water_name",
    "transportation_name",
    "place",
    "housenumber",
    "poi",
    "aerodrome_label",
];

/// The compile [`crate::compile_basemap`] runs once the request's datasets
/// are in place and its area is read. All inputs are local. Supplemental
/// archives are the pinned Planetiler inputs.
#[derive(Debug, Clone)]
pub(crate) struct OpenMapTilesRequest {
    pub input_pbf: PathBuf,
    pub sources: PathBuf,
    pub output: PathBuf,
    pub temp_dir: PathBuf,
    /// The box to compile; one whose west is greater than its east crosses
    /// the antimeridian. Longitudes past ±180 are moved into range (see
    /// [`CompileBounds::normalized`]).
    pub bounds: CompileBounds,
    /// Limits the compile to the tiles this area covers, within `bounds`.
    pub area: Option<CompileArea>,
    /// Where viewers start [default: the middle of the area or bounds, at the
    /// highest zoom that shows all of it].
    pub center: Option<ArchiveCenter>,
    /// Metadata and tile compression of the archive.
    pub archive: ArchiveOptions,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub threads: usize,
    /// The layers to write, from [`LAYERS`] [default: all of them]. Each one
    /// is identical to the same layer of a full build; the archive's metadata
    /// lists only these. [`select_layers`] makes this list from either the
    /// layers to write or those to leave out.
    pub layers: Option<Vec<String>>,
    /// The `name:<lang>` attributes to write, as Planetiler's `--languages`
    /// takes them (see [`resolve_languages`]) [default: every language in
    /// [`LANGUAGES`]]. `name`, `name_en`, `name_de`, `name_int`, `name:latin`
    /// and `name:nonlatin` are written regardless.
    pub languages: Option<Vec<String>>,
    /// Supplemental datasets to compile without; `sources` need not hold
    /// them. The archive's metadata (`omitted_sources`) and the result list
    /// them, and [`SupplementalDataset::without_warning`] says what the map
    /// lacks.
    pub omit_sources: Vec<SupplementalDataset>,
    /// Reads only the OSM objects in and near the area (`bounds`, within
    /// `area`'s box), so a small area compiles from a large extract in
    /// seconds. The tiles at `max_zoom` keep the content a full read gives
    /// them; lower zooms show only what lies near the area.
    pub area_only: bool,
}
/// Whether a compile with these options is one the parity measurement covers
/// (docs/parity.md): Planetiler's default languages, every supplemental
/// dataset, a full read of the extract and zoom 14 at the top. A selection of
/// layers is covered, since each layer is the same as in a full build.
fn parity_verified(request: &OpenMapTilesRequest) -> bool {
    request.languages.is_none()
        && request.omit_sources.is_empty()
        && !request.area_only
        && request.max_zoom == 14
}

/// What a compile wrote into the archive, and how long it took.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub struct CompileResult {
    /// The schema and its version, such as `OpenMapTiles 3.16.0 (native
    /// experimental port)`.
    pub schema: String,
    /// The profile's source revision ([`PROFILE_REVISION`]).
    pub profile_revision: String,
    /// Whether this profile's output is verified against its reference
    /// compiler (Planetiler for OpenMapTiles): `true` for the compiles the
    /// parity measurement covers (`docs/parity.md`): Planetiler's default
    /// languages, every supplemental dataset, a full read of the extract and
    /// zoom 14 at the top.
    pub parity_verified: bool,
    /// Source features read: OSM objects and supplemental records.
    pub sources_processed: u64,
    /// Features the profile made from them.
    pub profile_features: u64,
    /// Tiles written.
    pub tiles: u64,
    pub supplement_seconds: f64,
    pub osm_seconds: f64,
    pub sort_encode_write_seconds: f64,
    pub total_seconds: f64,
    /// Problems that did not stop the compile, such as source geometries that
    /// could not be assembled.
    #[serde(default)]
    pub warnings: Vec<String>,
    /// Sizes of the tiles written, per zoom and layer, and the largest
    /// tiles. Tiles above `TileStats::warning_bytes` also produce a warning.
    #[serde(default)]
    pub tile_stats: crate::tile_stats::TileStats,
    /// The [ids](SupplementalDataset::id) of the supplemental datasets the
    /// compile was asked to leave out, as the archive's metadata lists them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omitted_sources: Vec<String>,
}

/// The profile's state. Supplements fill it serially; afterwards workers only
/// read it and report what each OSM source adds as a `Contribution`.
#[derive(Default)]
struct Profile {
    water: water::State,
    labels: labels::State,
    transport: transport::State,
}

#[derive(Default)]
struct Contribution {
    water: water::Contribution,
    labels: labels::Contribution,
    transport: transport::Contribution,
}

impl Contribution {
    fn is_empty(&self) -> bool {
        self.water.is_empty() && self.labels.is_empty() && self.transport.is_empty()
    }
}

impl Profile {
    /// Evaluates a source serially, applying what it contributes at once.
    fn process(&mut self, source: &types::Source, features: &mut Vec<types::Feature>) {
        land::process(source, features);
        transport::process(source, features);
        self.water.process(source, features);
        self.labels.process(source, features);
    }

    /// Evaluates an OSM source; the layers append in the order land, transport,
    /// water, labels.
    fn evaluate(
        &self,
        source: &types::Source,
        features: &mut Vec<types::Feature>,
        contribution: &mut Contribution,
    ) {
        land::process(source, features);
        transport::evaluate(source, features, &mut contribution.transport);
        self.water
            .evaluate(source, features, &mut contribution.water);
        self.labels
            .evaluate(source, features, &mut contribution.labels);
    }

    /// Replays one source's contribution; call in source order.
    fn apply(&mut self, contribution: Contribution) {
        self.water.apply(contribution.water);
        self.labels.apply(contribution.labels);
        self.transport.apply(contribution.transport);
    }

    /// Features that need every OSM source: lakes, river members, transit
    /// stops, country borders.
    fn finish(&mut self, deferred: &mut Vec<types::Feature>) {
        self.water.finish(deferred);
        self.labels.finish(deferred);
        self.transport.finish(deferred);
    }
}

/// The archive while it is written: `<output>.<pid>.partial`, locked until it
/// is moved into place. Concurrent compiles to one output never share it, and
/// a later compile can tell one a killed compile left (unlocked) from one
/// being written. Dropped before `persist` succeeds, it is removed.
pub(crate) struct PartialArchive {
    path: PathBuf,
    file: Option<File>,
    finished: bool,
}

impl PartialArchive {
    pub(crate) fn create(output: &Path) -> Result<Self, String> {
        remove_abandoned_partials(output);
        let mut name = output.as_os_str().to_owned();
        name.push(format!(".{}.partial", std::process::id()));
        let path = PathBuf::from(name);
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("Failed to create {}: {e}", path.display()))?;
        file.try_lock()
            .map_err(|e| format!("Failed to lock {}: {e}", path.display()))?;
        Ok(Self {
            path,
            file: Some(file),
            finished: false,
        })
    }

    pub(crate) fn file(&self) -> &File {
        self.file
            .as_ref()
            .expect("the archive is open until persist")
    }

    /// Moves the finished archive to `output`. If that fails, it is left
    /// where it is and the error names it.
    pub(crate) fn persist(mut self, output: &Path) -> Result<(), String> {
        if output.exists() {
            return Err(format!(
                "refusing to replace existing archive {}",
                output.display()
            ));
        }
        self.finished = true;
        // Unix renames the file while it is still locked.
        let mut moved = std::fs::rename(&self.path, output);
        if moved.is_err() {
            // Windows cannot rename an open file, and virus scanners briefly
            // hold new ones.
            drop(self.file.take());
            for attempt in 0..10 {
                moved = std::fs::rename(&self.path, output);
                if !moved
                    .as_ref()
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100 << attempt.min(4)));
            }
        }
        moved.map_err(|e| {
            format!(
                "Failed to move the finished archive {} to {}: {e}",
                self.path.display(),
                output.display()
            )
        })
    }
}

impl Drop for PartialArchive {
    fn drop(&mut self) {
        if !self.finished {
            drop(self.file.take());
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Removes the `<output>.<pid>.partial` files that no compile holds locked:
/// those left by killed compiles.
fn remove_abandoned_partials(output: &Path) {
    let Some(name) = output.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let pid = file_name
            .to_str()
            .and_then(|file| file.strip_prefix(name))
            .and_then(|rest| rest.strip_prefix('.'))
            .and_then(|rest| rest.strip_suffix(".partial"));
        if !pid.is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())) {
            continue;
        }
        let path = entry.path();
        let Ok(file) = File::options().write(true).open(&path) else {
            continue;
        };
        if file.try_lock().is_ok() {
            drop(file);
            let _ = std::fs::remove_file(&path);
        }
    }
}

/// The middle of the bounds, or a point inside the area when the middle is
/// not, at the highest zoom that shows all of the bounds.
fn default_center(
    bounds: CompileBounds,
    area: Option<&CompileArea>,
    min_zoom: u8,
    max_zoom: u8,
) -> ArchiveCenter {
    let (mut longitude, mut latitude) = (bounds.center_longitude(), bounds.center_latitude());
    if let Some(area) = area
        && !area.contains(longitude, latitude)
        && let Some((x, y)) = area.interior_point()
        && bounds.contains(x, y)
    {
        (longitude, latitude) = (x, y);
    }
    ArchiveCenter {
        longitude,
        latitude,
        zoom: bounds.fit_zoom().clamp(min_zoom, max_zoom),
    }
}

/// Checks the request's settings without reading anything.
pub(crate) fn validate(request: &OpenMapTilesRequest) -> Result<(), String> {
    if request.min_zoom > request.max_zoom || request.max_zoom > 14 {
        return Err("OpenMapTiles port supports zooms 0 through 14".into());
    }
    if request.threads == 0 {
        return Err("threads must be positive".into());
    }
    if request.archive.gzip_level > 9 {
        return Err(format!(
            "the gzip level must be 0 to 9, got {}",
            request.archive.gzip_level
        ));
    }
    selection::Selection::new(request.layers.as_deref(), request.languages.as_deref())?;
    check_bounds(request.bounds)?;
    if let Some(center) = request.center {
        check_center(
            center,
            crate::ZoomRange::new(request.min_zoom, request.max_zoom).map_err(|e| e.to_string())?,
        )?;
    }
    Ok(())
}

/// Checks that `bounds` are finite and, once normalized, a box Web Mercator
/// can tile.
pub(crate) fn check_bounds(bounds: CompileBounds) -> Result<(), String> {
    let b = bounds;
    if ![b.north, b.south, b.east, b.west]
        .iter()
        .all(|v| v.is_finite())
    {
        return Err("invalid Web Mercator bounds: not finite".into());
    }
    let b = b.normalized();
    if b.north <= b.south || b.east == b.west || b.north > 85.051129 || b.south < -85.051129 {
        return Err(format!(
            "invalid Web Mercator bounds {} {} {} {}: they need S < N within ±85.051129 and W \
             different from E (W > E crosses the antimeridian)",
            b.west, b.south, b.east, b.north
        ));
    }
    Ok(())
}

/// Checks that `center` is a point Web Mercator shows, at a zoom written.
pub(crate) fn check_center(center: ArchiveCenter, zoom: crate::ZoomRange) -> Result<(), String> {
    if !(center.longitude.abs() <= 180.0 && center.latitude.abs() <= 85.051129)
        || !zoom.contains(center.zoom)
    {
        return Err(format!(
            "the center must be a longitude, a latitude within Web Mercator's and a zoom from \
             {} to {}",
            zoom.min(),
            zoom.max()
        ));
    }
    Ok(())
}

pub(crate) fn compile(
    request: &OpenMapTilesRequest,
    control: &Control,
    mut progress: impl FnMut(&str),
) -> Result<CompileResult, String> {
    validate(request)?;
    let selection =
        selection::Selection::new(request.layers.as_deref(), request.languages.as_deref())?;
    let b = request.bounds.normalized();
    let bounds = match &request.area {
        Some(area) => b
            .intersection(area.bounds())
            .ok_or("the area lies outside the bounds")?,
        None => b,
    };
    let center = match request.center {
        Some(center) => center,
        None => default_center(
            bounds,
            request.area.as_ref(),
            request.min_zoom,
            request.max_zoom,
        ),
    };
    let region = Region {
        bounds,
        coverage: request
            .area
            .as_ref()
            .map(|area| Arc::new(TileCoverage::new(area, request.min_zoom, request.max_zoom))),
    };
    if request.output.exists() {
        return Err(format!(
            "refusing to replace existing archive {}",
            request.output.display()
        ));
    }
    std::fs::create_dir_all(&request.temp_dir).map_err(|e| e.to_string())?;
    if let Some(parent) = request.output.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let start = Instant::now();
    let mut store = store::Store::new(&request.temp_dir)?;
    let mut pipeline = pipeline::Pipeline::new(
        &store,
        request.threads,
        region,
        request.min_zoom,
        request.max_zoom,
        Arc::new(selection.clone()),
        control.flag(),
    )?;
    let mut profile = Profile::default();
    let mut sources_processed = 0;
    let mut profile_features = 0;
    // Supplement sources run here, in order, because they fill the state that
    // OSM sources read; their ordinals are plain counters, below every OSM
    // source's. Reuse the small output buffer across source callbacks.
    let mut features = Vec::with_capacity(8);
    let mut process = |source: types::Source| -> Result<(), String> {
        profile.process(&source, &mut features);
        profile_features += features.len() as u64;
        pipeline.push_source(sources_processed, &mut features)?;
        sources_processed += 1;
        Ok(())
    };
    let mut warnings = Vec::new();
    let omitted = |dataset| request.omit_sources.contains(&dataset);
    let message = "Reading supplemental sources";
    progress(message);
    control.stage(CompileStage::ReadSources, message)?;
    supplements::read_sources(
        &request.sources,
        &request.temp_dir,
        bounds,
        supplements::Read {
            // The ocean polygons only make water features.
            water_polygons: selection.has_layer("water")
                && !omitted(SupplementalDataset::WaterPolygons),
            natural_earth: !omitted(SupplementalDataset::NaturalEarth),
            lake_centerlines: !omitted(SupplementalDataset::LakeCenterlines),
        },
        control,
        &mut process,
        &mut warnings,
    )?;
    let supplement_seconds = start.elapsed().as_secs_f64();
    control.stage(CompileStage::IndexOsm, "Indexing the OSM extract")?;
    let osm_start = Instant::now();
    // Workers assemble and evaluate OSM sources against the now read-only
    // profile, then everything they accumulated is replayed in source order.
    let profile = Arc::new(profile);
    ingest::read_osm(
        &request.input_pbf,
        &request.temp_dir,
        request.threads,
        &mut pipeline,
        &profile,
        control,
        request
            .area_only
            .then(|| input_area::InputArea::new(bounds, request.max_zoom)),
        &mut progress,
        &mut warnings,
    )?;
    let mut profile = Arc::into_inner(profile).ok_or("Render workers still hold the profile")?;
    for (_, contribution) in pipeline.take_contributions() {
        profile.apply(contribution);
    }
    let osm_seconds = osm_start.elapsed().as_secs_f64();
    control.check()?;
    let mut deferred = Vec::new();
    profile.finish(&mut deferred);
    profile_features += deferred.len() as u64;
    // The deferred features follow every source.
    pipeline.push_source(u64::MAX, &mut deferred)?;
    let totals = pipeline.finish(&mut store)?;
    sources_processed += totals.sources;
    profile_features += totals.features;
    drop(pipeline);
    let finish_start = Instant::now();
    let message = "Sorting, encoding and writing PMTiles";
    progress(message);
    control.stage(CompileStage::WriteTiles, message)?;
    // The archive is written beside the output and moved into place, so a
    // failed or interrupted compile never leaves a partial archive at the
    // output path, where it would block the next attempt.
    let partial = PartialArchive::create(&request.output)?;
    let omitted_sources = SupplementalDataset::ALL
        .into_iter()
        .filter(|dataset| omitted(*dataset))
        .map(SupplementalDataset::id)
        .collect::<Vec<_>>();
    let header = store::ArchiveHeader {
        bounds,
        center,
        options: &request.archive,
        min_zoom: request.min_zoom,
        max_zoom: request.max_zoom,
        layers: &selection.layers(),
        omitted_sources: &omitted_sources,
    };
    let tile_stats = store.finish(partial.file(), &header, request.threads, control)?;
    control.check()?;
    partial.persist(&request.output)?;
    warnings.extend(tile_stats.warning());
    Ok(CompileResult {
        schema: format!("OpenMapTiles {OMT_SCHEMA_VERSION} (native experimental port)"),
        profile_revision: PROFILE_REVISION.into(),
        parity_verified: parity_verified(request),
        sources_processed,
        profile_features,
        tiles: tile_stats.tiles(),
        supplement_seconds,
        osm_seconds,
        sort_encode_write_seconds: finish_start.elapsed().as_secs_f64(),
        total_seconds: start.elapsed().as_secs_f64(),
        warnings,
        tile_stats,
        omitted_sources: omitted_sources.into_iter().map(String::from).collect(),
    })
}
