//! The request envelope and the request kinds it dispatches to. Each kind
//! parses its request when the job starts, so a malformed one fails
//! `otc_job_start` at once, and returns the work the job runs.
//!
//! The `compile_basemap` request below is a JSON mirror of
//! `osm_tile_compiler::BasemapRequest`, mapped onto it field by field and
//! checked with `BasemapRequest::validate` before the job starts. Once the
//! request types get serde support with these names (roadmap item A9), the
//! mirror can go.

use crate::job::{JobError, Progress, Work};
use crate::{archive_jobs, fetch_jobs, update_job};
use osm_tile_compiler::{
    ArchiveCenter, Area, AreaPolygon, BasemapRequest, CompileBounds, CompileError, LayerSelection,
    OutputFormat, ReadScope, SourcesPolicy, StylePackEvent, StylePackRequest, SupplementalDataset,
    ZoomRange, compile_basemap, write_style_pack,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

/// The request every `otc_job_start` takes.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Envelope {
    /// The C ABI version the host was written against; it must be
    /// [`crate::ABI_VERSION`].
    abi_version: u32,
    /// What to run, such as `compile_basemap`.
    kind: String,
    /// The kind's own request.
    #[serde(default)]
    request: Value,
}

/// The kinds this library runs, in the order the docs list them.
pub const KINDS: &[&str] = &[
    "compile_basemap",
    "write_style_pack",
    "extract",
    "update",
    "diff",
    "apply",
    "inspect",
    "verify",
    "verify_delta",
    "fetch_sources",
    "fetch_extract",
];

/// Parses an envelope and its request: the kind and the work to run.
pub fn parse(json: &str) -> Result<(&'static str, Work), JobError> {
    let envelope: Envelope = serde_json::from_str(json)
        .map_err(|e| JobError::invalid_request(format!("Invalid request envelope: {e}")))?;
    if envelope.abi_version != crate::ABI_VERSION {
        return Err(JobError::invalid_request(format!(
            "The request is for C ABI version {}; this library speaks version {}",
            envelope.abi_version,
            crate::ABI_VERSION
        )));
    }
    match envelope.kind.as_str() {
        "compile_basemap" => Ok(("compile_basemap", compile_basemap_work(envelope.request)?)),
        "write_style_pack" => Ok(("write_style_pack", write_style_pack_work(envelope.request)?)),
        "extract" => Ok(("extract", archive_jobs::extract_work(envelope.request)?)),
        "update" => Ok(("update", update_job::update_work(envelope.request)?)),
        "diff" => Ok(("diff", archive_jobs::diff_work(envelope.request)?)),
        "apply" => Ok(("apply", archive_jobs::apply_work(envelope.request)?)),
        "inspect" => Ok((
            "inspect",
            archive_jobs::archive_report_work("inspect", envelope.request)?,
        )),
        "verify" => Ok((
            "verify",
            archive_jobs::archive_report_work("verify", envelope.request)?,
        )),
        "verify_delta" => Ok((
            "verify_delta",
            archive_jobs::verify_delta_work(envelope.request)?,
        )),
        "fetch_sources" => Ok((
            "fetch_sources",
            fetch_jobs::fetch_sources_work(envelope.request)?,
        )),
        "fetch_extract" => Ok((
            "fetch_extract",
            fetch_jobs::fetch_extract_work(envelope.request)?,
        )),
        other => Err(JobError::invalid_request(format!(
            "Unknown request kind '{other}'; this library runs: {}",
            KINDS.join(", ")
        ))),
    }
}

impl From<CompileError> for JobError {
    fn from(error: CompileError) -> Self {
        let stage = match &error {
            CompileError::Failed { stage, .. } => stage.map(|stage| stage.id()),
            _ => None,
        };
        Self {
            kind: error.kind(),
            message: error.message().to_string(),
            stage,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
enum SourcesPolicyJson {
    UsePresent,
    FetchMissing,
    FetchAndVerify,
}

/// A box in degrees; one whose west is greater than its east crosses the
/// antimeridian.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BoundsJson {
    north: f64,
    south: f64,
    east: f64,
    west: f64,
}

impl BoundsJson {
    /// The box, refused as the command line's `--bounds` refuses it.
    pub(crate) fn checked(self) -> Result<CompileBounds, JobError> {
        let Self {
            north,
            south,
            east,
            west,
        } = self;
        if ![north, south, east, west].iter().all(|v| v.is_finite())
            || west == east
            || south >= north
            || west.abs() > 180.0
            || east.abs() > 180.0
        {
            return Err(JobError::invalid_request(format!(
                "bounds must be within -180..180 with south < north and west different \
                 from east (west > east crosses the antimeridian), got north {north}, \
                 south {south}, east {east}, west {west}"
            )));
        }
        Ok(CompileBounds::new(north, south, east, west))
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CenterJson {
    longitude: f64,
    latitude: f64,
    zoom: u8,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchiveJson {
    name: Option<String>,
    description: Option<String>,
    attribution: Option<String>,
    gzip_level: Option<u32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ZoomJson {
    pub(crate) min: Option<u8>,
    pub(crate) max: Option<u8>,
}

/// Which layers to write: `{"only": [...]}` or `{"except": [...]}`; left
/// out, all of them.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
enum LayersJson {
    Only(Vec<String>),
    Except(Vec<String>),
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum ReadScopeJson {
    /// Read the whole extract.
    #[default]
    WholeExtract,
    /// Read only the OSM data in and near the area.
    NearArea,
}

/// The area to compile [default: the PBF header's box, read whole].
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct AreaJson {
    /// A box; one whose west is greater than its east crosses the
    /// antimeridian.
    bounds: Option<BoundsJson>,
    /// A GeoJSON or Osmosis `.poly` file limiting the compile to the tiles
    /// its polygon covers.
    polygon: Option<PathBuf>,
    #[serde(default)]
    read: ReadScopeJson,
}

/// `compile_basemap`'s request. Every field but the four paths is optional
/// and defaults as `BasemapRequest::new` does. Its shape follows the request
/// types (`ZoomRange`, `LayerSelection`, `Area`).
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompileBasemapJson {
    input_pbf: PathBuf,
    /// More extracts to merge with `inputPbf` [default: none].
    #[serde(default)]
    extra_inputs: Vec<PathBuf>,
    output: PathBuf,
    /// "pmtiles" or "mbtiles" [default: by the output's extension].
    format: Option<String>,
    sources: PathBuf,
    temp_dir: PathBuf,
    sources_policy: Option<SourcesPolicyJson>,
    #[serde(default)]
    omit_sources: Vec<String>,
    #[serde(default)]
    area: AreaJson,
    center: Option<CenterJson>,
    #[serde(default)]
    archive: ArchiveJson,
    #[serde(default)]
    zoom: ZoomJson,
    threads: Option<usize>,
    layers: Option<LayersJson>,
    languages: Option<Vec<String>>,
    #[serde(default)]
    building_levels: bool,
    /// Whether to also write the search-and-rescue layer `sar`.
    #[serde(default)]
    sar: bool,
    /// Whether the compile keeps checkpoints when interrupted, and continues
    /// from them (`BasemapRequest::resume`).
    #[serde(default)]
    resume: bool,
    /// Layers of other vector data (`BasemapRequest::data_layers`): each
    /// with a `layer`, a `source` (`path`, and optionally `format`, `table`,
    /// `crs`, `csv`, `idAttribute`), and optionally `minZoom`, `maxZoom`,
    /// `attributes`, `excludeAttributes`, `renameAttributes` and
    /// `attribution`.
    #[serde(default)]
    data_layers: Vec<osm_tile_compiler::DataLayer>,
}

fn compile_basemap_request(request: Value) -> Result<BasemapRequest, JobError> {
    let json: CompileBasemapJson = serde_json::from_value(request)
        .map_err(|e| JobError::invalid_request(format!("Invalid compile_basemap request: {e}")))?;
    let mut request = BasemapRequest::new(json.input_pbf, json.output, json.sources, json.temp_dir);
    request.extra_inputs = json.extra_inputs;
    if let Some(id) = json.format {
        request.format = Some(OutputFormat::from_id(&id).ok_or_else(|| {
            JobError::invalid_request(format!(
                "Unknown output format '{id}': it is pmtiles or mbtiles"
            ))
        })?);
    }
    if let Some(policy) = json.sources_policy {
        request.sources_policy = match policy {
            SourcesPolicyJson::UsePresent => SourcesPolicy::UsePresent,
            SourcesPolicyJson::FetchMissing => SourcesPolicy::FetchMissing,
            SourcesPolicyJson::FetchAndVerify => SourcesPolicy::FetchAndVerify,
        };
    }
    request.omit_sources = json
        .omit_sources
        .iter()
        .map(|id| {
            SupplementalDataset::from_id(id).ok_or_else(|| {
                JobError::invalid_request(format!("Unknown supplemental dataset '{id}'"))
            })
        })
        .collect::<Result<_, _>>()?;
    let mut area = Area::whole_extract();
    area.bounds = json
        .area
        .bounds
        .map(|b| CompileBounds::new(b.north, b.south, b.east, b.west));
    area.polygon = json.area.polygon.map(AreaPolygon::File);
    area.read = match json.area.read {
        ReadScopeJson::WholeExtract => ReadScope::WholeExtract,
        ReadScopeJson::NearArea => ReadScope::NearArea,
    };
    request.area = area;
    request.center = json
        .center
        .map(|c| ArchiveCenter::new(c.longitude, c.latitude, c.zoom));
    let (archive, given) = (&mut request.archive, json.archive);
    if let Some(name) = given.name {
        archive.name = name;
    }
    if let Some(description) = given.description {
        archive.description = description;
    }
    if let Some(attribution) = given.attribution {
        archive.attribution = attribution;
    }
    archive.gzip_level = given.gzip_level.unwrap_or(archive.gzip_level);
    let zoom = ZoomRange::default();
    request.zoom = ZoomRange::new(
        json.zoom.min.unwrap_or(zoom.min()),
        json.zoom.max.unwrap_or(zoom.max()),
    )?;
    request.threads = json.threads.unwrap_or(request.threads);
    request.layers = match json.layers {
        None => LayerSelection::All,
        Some(LayersJson::Only(layers)) => LayerSelection::only(layers),
        Some(LayersJson::Except(layers)) => LayerSelection::except(layers),
    };
    request.languages = json.languages;
    request.building_levels = json.building_levels;
    request.sar = json.sar;
    request.resume = json.resume;
    request.data_layers = json.data_layers;
    // Everything that needs no reading or network fails the start, not the job.
    request.validate()?;
    Ok(request)
}

fn compile_basemap_work(request: Value) -> Result<Work, JobError> {
    let request = compile_basemap_request(request)?;
    Ok(Box::new(move |reporter| {
        let output = compile_basemap(
            &request,
            |progress| {
                reporter.report(Progress {
                    stage: Some(progress.stage.id()),
                    message: progress.message.clone(),
                    fraction: progress.fraction,
                    overall: Some(progress.overall),
                })
            },
            || reporter.is_cancelled(),
        )?;
        // The worker's manifest, plus where each output went.
        let mut result = output.manifest();
        result["archive"] = json!(output.archive);
        result["archiveFormat"] = json!(output.format.id());
        result["buildingLevels"] = json!(output.building_levels.as_ref().map(|(path, _)| path));
        result["resumedFrom"] = json!(output.resumed_from.map(|stage| stage.id()));
        result["bounds"] = json!({
            "north": output.bounds.north,
            "south": output.bounds.south,
            "east": output.bounds.east,
            "west": output.bounds.west,
        });
        Ok(result)
    }))
}

/// `write_style_pack`'s request, a mirror of
/// `osm_tile_compiler::StylePackRequest`. Only `directory` and `archiveUrl`
/// are required.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct StylePackJson {
    directory: PathBuf,
    archive_url: String,
    archive_file: Option<PathBuf>,
    languages: Option<Vec<String>>,
    glyphs: Option<String>,
    sprite: Option<String>,
    base_url: Option<String>,
    #[serde(default)]
    sar: bool,
}

fn style_pack_request(request: Value) -> Result<StylePackRequest, JobError> {
    let json: StylePackJson = serde_json::from_value(request)
        .map_err(|e| JobError::invalid_request(format!("Invalid write_style_pack request: {e}")))?;
    let mut request = StylePackRequest::new(json.directory, json.archive_url);
    request.archive_file = json.archive_file;
    request.languages = json.languages;
    request.base_url = json.base_url;
    request.sar = json.sar;
    if let Some(glyphs) = json.glyphs {
        request.glyphs = glyphs;
    }
    if let Some(sprite) = json.sprite {
        request.sprite = sprite;
    }
    request.validate()?;
    Ok(request)
}

fn write_style_pack_work(request: Value) -> Result<Work, JobError> {
    let request = style_pack_request(request)?;
    Ok(Box::new(move |reporter| {
        let report = write_style_pack(
            &request,
            |event| match event {
                StylePackEvent::ReadingArchive => reporter.report(Progress {
                    stage: None,
                    message: "Reading the archive's labels".into(),
                    fraction: None,
                    overall: Some(0.0),
                }),
                StylePackEvent::Fetched { done, total, .. } => {
                    let fraction = done as f64 / total.max(1) as f64;
                    reporter.report(Progress {
                        stage: None,
                        message: format!("Fetched {done} of {total} files"),
                        fraction: Some(fraction),
                        overall: Some(fraction),
                    })
                }
                _ => {}
            },
            || reporter.is_cancelled(),
        )?;
        serde_json::to_value(report).map_err(|e| JobError::internal(e.to_string()))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn error(json: &str) -> JobError {
        match parse(json) {
            Ok(_) => panic!("{json} parsed"),
            Err(error) => error,
        }
    }

    #[test]
    fn the_envelope_is_checked() {
        assert!(
            error("not json")
                .message
                .starts_with("Invalid request envelope")
        );
        assert!(
            error(r#"{"abiVersion":2,"kind":"compile_basemap"}"#)
                .message
                .contains("C ABI version 2")
        );
        assert!(
            error(r#"{"abiVersion":1,"kind":"nope"}"#)
                .message
                .contains("Unknown request kind 'nope'")
        );
        assert!(
            error(r#"{"abiVersion":1,"kind":"compile_basemap","extra":1}"#)
                .message
                .contains("unknown field `extra`")
        );
        assert_eq!(error("{}").kind, "invalid_request");
    }

    #[test]
    fn compile_basemap_requests_map_onto_basemap_request() {
        let request = compile_basemap_request(json!({
            "inputPbf": "/in.osm.pbf",
            "output": "/out.pmtiles",
            "sources": "/sources",
            "tempDir": "/tmp",
            "sourcesPolicy": "fetch_missing",
            "omitSources": ["water_polygons"],
            "area": {
                "bounds": { "north": 2.0, "south": 1.0, "east": 4.0, "west": 3.0 },
                "read": "near_area",
            },
            "zoom": { "min": 2, "max": 12 },
            "layers": { "except": ["poi"] },
            "threads": 3,
            "archive": { "name": "Test" },
            "languages": ["en"],
            "buildingLevels": true,
            "sar": true,
        }))
        .unwrap();
        assert_eq!(request.input_pbf, PathBuf::from("/in.osm.pbf"));
        assert_eq!(request.sources_policy, SourcesPolicy::FetchMissing);
        assert_eq!(
            request.omit_sources,
            vec![SupplementalDataset::from_id("water_polygons").unwrap()]
        );
        assert_eq!(request.area.bounds.unwrap().west, 3.0);
        assert_eq!(request.area.read, ReadScope::NearArea);
        assert_eq!(request.layers, LayerSelection::except(["poi"]));
        assert_eq!(request.zoom, ZoomRange::new(2, 12).unwrap());
        assert_eq!(request.threads, 3);
        assert_eq!(request.archive.name, "Test");
        assert_eq!(request.archive.gzip_level, 6);
        assert_eq!(request.languages, Some(vec!["en".to_string()]));
        assert!(request.building_levels);
        assert!(request.sar);
    }

    #[test]
    fn compile_basemap_requests_take_data_layers() {
        let dir = tempfile::tempdir().unwrap();
        let csv = dir.path().join("stations.csv");
        std::fs::write(&csv, "lon,lat,name\n1,2,a\n").unwrap();
        let request = compile_basemap_request(json!({
            "inputPbf": "/in.osm.pbf",
            "output": dir.path().join("out.pmtiles"),
            "sources": "/sources",
            "tempDir": "/tmp",
            "omitSources": ["water_polygons", "natural_earth", "lake_centerlines"],
            "dataLayers": [{
                "layer": "stations",
                "source": { "path": csv, "idAttribute": "name" },
                "minZoom": 12,
                "renameAttributes": { "name": "label" },
            }],
        }))
        .unwrap();
        let layer = &request.data_layers[0];
        assert_eq!(layer.layer.as_str(), "stations");
        assert_eq!(layer.min_zoom, Some(12));
        assert_eq!(layer.source.id_attribute.as_deref(), Some("name"));
        assert_eq!(layer.rename_attributes["name"], "label");
        let missing = compile_basemap_request(json!({
            "inputPbf": "/in.osm.pbf",
            "output": dir.path().join("out.pmtiles"),
            "sources": "/sources",
            "tempDir": "/tmp",
            "dataLayers": [{ "layer": "x", "source": { "path": "/nowhere.csv" } }],
        }))
        .unwrap_err();
        assert!(
            missing.message.contains("does not exist"),
            "{}",
            missing.message
        );
    }

    #[test]
    fn style_pack_requests_map_onto_style_pack_request() {
        let request = style_pack_request(json!({
            "directory": "/nowhere/pack",
            "archiveUrl": "asset://berlin.pmtiles",
            "languages": ["uk"],
            "glyphs": "https://example.com/{fontstack}/{range}.pbf",
            "baseUrl": "asset://pack",
        }))
        .unwrap();
        assert_eq!(request.directory, PathBuf::from("/nowhere/pack"));
        assert_eq!(request.languages, Some(vec!["uk".to_string()]));
        assert_eq!(request.base_url.as_deref(), Some("asset://pack"));
        assert_eq!(request.sprite, osm_tile_compiler::DEFAULT_SPRITE_URL);
        let refused = |request: Value| style_pack_request(request).unwrap_err().message;
        assert!(refused(json!({"directory": "/x"})).contains("archiveUrl"));
        assert!(
            refused(json!({"directory": "/x", "archiveUrl": "a", "languages": ["xx"]}))
                .contains("unknown language 'xx'")
        );
        assert!(
            refused(json!({"directory": "/x", "archiveUrl": "a", "glyphs": "g.pbf"}))
                .contains("{fontstack}")
        );
    }

    #[test]
    fn bad_compile_basemap_requests_fail_at_start() {
        let base = r#""inputPbf":"/a","output":"/b","sources":"/c","tempDir":"/d""#;
        let parse_request = |extra: &str| {
            error(&format!(
                r#"{{"abiVersion":1,"kind":"compile_basemap","request":{{{base}{extra}}}}}"#
            ))
            .message
        };
        assert!(
            parse_request(r#","omitSources":["x"]"#).contains("Unknown supplemental dataset 'x'")
        );
        assert!(parse_request(r#","colour":"red""#).contains("unknown field `colour`"));
        assert!(parse_request(r#","sourcesPolicy":"always""#).contains("unknown variant"));
        assert!(parse_request(r#","zoom":{"min":9,"max":3}"#).contains("zooms must satisfy"));
        // BasemapRequest::validate runs at start too.
        assert!(parse_request(r#","layers":{"only":["roads"]}"#).contains("roads"));
        assert!(parse_request(r#","threads":0"#).contains("threads must be positive"));
        assert!(parse_request(r#","area":{"read":"near_area"}"#).contains("bounds or a polygon"));
        assert!(
            error(r#"{"abiVersion":1,"kind":"compile_basemap"}"#)
                .message
                .contains("Invalid compile_basemap request")
        );
    }
}
