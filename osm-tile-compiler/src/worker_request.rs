//! The compile request of the worker protocol (`--request <request.json>`),
//! which `compile --request` also reads. `docs/formats.md` says when its
//! fields change.
use osm_tile_compiler::{
    BasemapRequest, LayerSelection, ReadScope, SourcesPolicy, SupplementalDataset,
};
use serde::Deserialize;
use std::path::{Path, PathBuf};

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CompilerProcessRequest {
    input_pbf_path: PathBuf,
    /// More extracts to merge with `inputPbfPath` [default: none].
    #[serde(default)]
    extra_input_pbf_paths: Vec<PathBuf>,
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
    /// Whether to also write the search-and-rescue layer `sar`
    /// [default: no].
    #[serde(default)]
    sar: bool,
}

/// Reads the request at `path`: the compile it asks for, and where to write
/// its manifest. Every failure is the request's.
pub fn read(path: &Path) -> Result<(BasemapRequest, PathBuf), String> {
    let bytes = std::fs::read(path)
        .map_err(|error| format!("Failed to read compiler request {path:?}: {error}"))?;
    let request: CompilerProcessRequest = serde_json::from_slice(&bytes)
        .map_err(|error| format!("Invalid compiler request {path:?}: {error}"))?;
    let omit_sources = request
        .omit_sources
        .iter()
        .map(|id| {
            SupplementalDataset::from_id(id)
                .ok_or_else(|| format!("unknown supplemental dataset '{id}'"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    let mut compile = BasemapRequest::new(
        request.input_pbf_path,
        request.output_pmtiles_path,
        request.sources_dir,
        request.temp_dir,
    );
    compile.extra_inputs = request.extra_input_pbf_paths;
    compile.sources_policy = SourcesPolicy::FetchAndVerify;
    compile.omit_sources = omit_sources;
    // `areaOnly` without a polygon reads the whole extract, as before.
    let read = if request.area_only && request.polygon_path.is_some() {
        ReadScope::NearArea
    } else {
        ReadScope::WholeExtract
    };
    compile.area.polygon = request.polygon_path.map(Into::into);
    compile.area.read = read;
    compile.layers = match (request.layers, request.exclude_layers) {
        (Some(_), Some(_)) => {
            return Err("give either the layers to write or those to leave out, not both".into());
        }
        (Some(only), None) => LayerSelection::only(only),
        (None, Some(except)) => LayerSelection::except(except),
        (None, None) => LayerSelection::All,
    };
    compile.languages = request.languages;
    compile.building_levels = request.building_levels;
    compile.sar = request.sar;
    Ok((compile, request.manifest_path))
}
