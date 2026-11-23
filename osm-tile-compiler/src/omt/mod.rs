//! Native Rust port of the OpenMapTiles 3.16 profile pinned by Planetiler 0.10.2.
//! This profile is experimental: see the parity ledger before treating output
//! as interchangeable with Planetiler. No Java or external GIS process is used.
#[cfg(test)]
mod f64_arrangement;
#[cfg(test)]
mod f64_buffer;
mod float_repair;
mod ingest;
pub(crate) use store::GENERATOR;
mod input_area;
mod interior_point;
mod java_hashmap;
mod jts_buffer;
mod jts_intersection;
mod labels;
mod land;
mod line_merge;
mod merge_cache;
mod pipeline;
mod polygon_validity;
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
use crate::output::PartialArchive;
use crate::sources::SupplementalDataset;
use crate::{ArchiveCenter, CompileArea, CompileBounds, archive::ArchiveOptions};
pub use merge_cache::{MergeCacheOptions, MergeCacheStats, merge_cache_path};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::{sync::Arc, time::Instant};

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
    /// The memory the tile sort may hold [default: half of what is available
    /// when rendering starts]; see [`crate::BasemapRequest::sort_memory`].
    pub sort_memory: Option<u64>,
    /// The archive format to write.
    pub format: crate::OutputFormat,
    /// The merge cache to write and reuse.
    pub merge_cache: merge_cache::MergeCacheOptions,
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

/// How current the OSM data of a compile is, from the extract's header. The
/// archive's metadata carries it as `osm_data_timestamp`,
/// `osm_replication_sequence` and `osm_replication_url`; each is left out
/// when the header doesn't say.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct OsmDataInfo {
    /// The moment the data is current to, in seconds since the Unix epoch:
    /// the header's `osmosis_replication_timestamp`, which Geofabrik sets and
    /// `update` advances with every change file it applies.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replication_timestamp: Option<i64>,
    /// The replication sequence number the data is current to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replication_sequence_number: Option<i64>,
    /// The replication server whose diffs continue the data.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub replication_base_url: Option<String>,
}

impl OsmDataInfo {
    fn from_header(header: &osm_pbf_ingest::blob_index::PbfFileHeader) -> Self {
        Self {
            replication_timestamp: header.replication_timestamp,
            replication_sequence_number: header.replication_sequence_number,
            replication_base_url: header.replication_base_url.clone(),
        }
    }

    /// The replication timestamp as an RFC 3339 UTC time, such as
    /// `2026-11-18T20:21:22Z`.
    pub fn timestamp_rfc3339(&self) -> Option<String> {
        self.replication_timestamp.map(rfc3339)
    }
}

/// Seconds since the Unix epoch as an RFC 3339 UTC time.
pub(crate) fn rfc3339(seconds: i64) -> String {
    let days = seconds.div_euclid(86_400);
    let second_of_day = seconds.rem_euclid(86_400);
    // Days to a civil date, after Howard Hinnant's `civil_from_days`.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        second_of_day / 3600,
        second_of_day / 60 % 60,
        second_of_day % 60
    )
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
    /// What the merge cache did, when the compile used one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub merge_cache: Option<MergeCacheStats>,
    /// How current the OSM data is, as the archive's metadata records it.
    #[serde(default)]
    pub osm_data: OsmDataInfo,
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
        pipeline::sort_budget(
            request.threads,
            request.sort_memory,
            std::fs::metadata(&request.input_pbf)
                .ok()
                .map(|metadata| metadata.len()),
        ),
        region,
        (request.min_zoom, request.max_zoom),
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
    // Opened first, so that an index that cannot be used fails at once.
    let mut cache = merge_cache::MergeCache::open(
        &request.merge_cache,
        &request.temp_dir,
        request.archive.gzip_level,
        &mut warnings,
    )?;
    let omitted = |dataset| request.omit_sources.contains(&dataset);
    let message = "Reading supplemental sources";
    progress(message);
    control.stage(CompileStage::ReadSources, message)?;
    let sources_stage = crate::stage_profile::Stage::start("read_sources");
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
    drop(sources_stage);
    let supplement_seconds = start.elapsed().as_secs_f64();
    control.stage(CompileStage::IndexOsm, "Indexing the OSM extract")?;
    let osm_start = Instant::now();
    // Workers assemble and evaluate OSM sources against the now read-only
    // profile, then everything they accumulated is replayed in source order.
    let profile = Arc::new(profile);
    let osm_header = ingest::read_osm(
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
    let drain_stage = crate::stage_profile::Stage::start("render_drain");
    let mut deferred = Vec::new();
    profile.finish(&mut deferred);
    profile_features += deferred.len() as u64;
    // The deferred features follow every source.
    pipeline.push_source(u64::MAX, &mut deferred)?;
    let totals = pipeline.finish(&mut store)?;
    sources_processed += totals.sources;
    profile_features += totals.features;
    drop(pipeline);
    drop(drain_stage);
    let finish_start = Instant::now();
    let message = format!("Sorting, encoding and writing {}", request.format.name());
    progress(&message);
    control.stage(CompileStage::WriteTiles, &message)?;
    // The archive is written beside the output and moved into place, so a
    // failed or interrupted compile never leaves a partial archive at the
    // output path, where it would block the next attempt.
    let partial = PartialArchive::create(&request.output, request.format)?;
    let omitted_sources = SupplementalDataset::ALL
        .into_iter()
        .filter(|dataset| omitted(*dataset))
        .map(SupplementalDataset::id)
        .collect::<Vec<_>>();
    let osm_data = osm_header
        .as_ref()
        .map(OsmDataInfo::from_header)
        .unwrap_or_default();
    let header = store::ArchiveHeader {
        bounds,
        center,
        options: &request.archive,
        min_zoom: request.min_zoom,
        max_zoom: request.max_zoom,
        layers: &selection.layers(),
        omitted_sources: &omitted_sources,
        osm_data: &osm_data,
    };
    let tile_stats =
        store.finish_cached(&partial, &header, request.threads, control, cache.as_mut())?;
    control.check()?;
    partial.persist(&request.output)?;
    let merge_cache = match cache {
        Some(mut cache) => {
            let mut index_bytes = 0;
            if let Some(writer) = cache.writer.take() {
                // Written beside the archive, then moved over any index an
                // earlier archive of this name left.
                let path = merge_cache_path(&request.output);
                let mut partial = path.as_os_str().to_owned();
                partial.push(format!(".{}.partial", std::process::id()));
                let partial = PathBuf::from(partial);
                let written = writer
                    .finish(&partial, request.archive.gzip_level, &request.output)
                    .and_then(|bytes| {
                        let _ = std::fs::remove_file(&path);
                        std::fs::rename(&partial, &path)
                            .map(|()| bytes)
                            .map_err(|e| format!("Failed to write {}: {e}", path.display()))
                    });
                match written {
                    Ok(bytes) => index_bytes = bytes,
                    Err(error) => {
                        let _ = std::fs::remove_file(&partial);
                        warnings.push(format!("the merge cache was not written: {error}"));
                    }
                }
            }
            Some(cache.counters.stats(index_bytes))
        }
        None => None,
    };
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
        merge_cache,
        osm_data,
    })
}

#[cfg(test)]
mod osm_data_tests {
    use super::*;

    #[test]
    fn replication_timestamps_print_as_rfc3339_utc() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        // Monaco's fixture header.
        assert_eq!(rfc3339(1_619_036_506), "2021-04-21T20:21:46Z");
        // A leap day and the last second of a year.
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_798_761_599), "2026-12-31T23:59:59Z");
        assert_eq!(rfc3339(-1), "1969-12-31T23:59:59Z");
    }

    #[test]
    fn metadata_records_the_osm_data_only_when_the_header_gives_it() {
        let bounds = CompileBounds::new(43.76, 43.72, 7.45, 7.40);
        let mut header = store::ArchiveHeader::for_test(bounds, 0, 14);
        let unknown = store::metadata_for_test(&header);
        assert_eq!(unknown["generator"], store::GENERATOR);
        for key in [
            "osm_data_timestamp",
            "osm_replication_sequence",
            "osm_replication_url",
        ] {
            assert!(unknown.get(key).is_none(), "{key} without a header");
        }
        let known = OsmDataInfo {
            replication_timestamp: Some(1_619_036_506),
            replication_sequence_number: Some(2947),
            replication_base_url: Some(
                "https://download.geofabrik.de/europe/monaco-updates".into(),
            ),
        };
        header.osm_data = &known;
        let metadata = store::metadata_for_test(&header);
        assert_eq!(metadata["osm_data_timestamp"], "2021-04-21T20:21:46Z");
        assert_eq!(metadata["osm_replication_sequence"], 2947);
        assert_eq!(
            metadata["osm_replication_url"],
            "https://download.geofabrik.de/europe/monaco-updates"
        );
    }
}
