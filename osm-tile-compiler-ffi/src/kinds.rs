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
use osm_tile_compiler::{
    ArchiveCenter, Area, AreaPolygon, BasemapRequest, CompileBounds, CompileError, LayerSelection,
    ReadScope, SourcesPolicy, SupplementalDataset, ZoomRange, compile_basemap,
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
pub const KINDS: &[&str] = &["compile_basemap"];

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

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct BoundsJson {
    north: f64,
    south: f64,
    east: f64,
    west: f64,
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
struct ZoomJson {
    min: Option<u8>,
    max: Option<u8>,
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
    output: PathBuf,
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
}

fn compile_basemap_request(request: Value) -> Result<BasemapRequest, JobError> {
    let json: CompileBasemapJson = serde_json::from_value(request)
        .map_err(|e| JobError::invalid_request(format!("Invalid compile_basemap request: {e}")))?;
    let mut request = BasemapRequest::new(json.input_pbf, json.output, json.sources, json.temp_dir);
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
        result["buildingLevels"] = json!(output.building_levels.as_ref().map(|(path, _)| path));
        result["bounds"] = json!({
            "north": output.bounds.north,
            "south": output.bounds.south,
            "east": output.bounds.east,
            "west": output.bounds.west,
        });
        Ok(result)
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
