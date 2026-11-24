//! End-to-end tests of configured tile schemas: the built-in Shortbread
//! schema and a small YAML schema of the caller's, compiled from the Monaco
//! fixture, and the requests they refuse.

use osm_tile_compiler::{
    BasemapRequest, CompileError, LayerSelection, Schema, SupplementalDataset, TileSchema, archive,
    compile_basemap, shortbread,
};
#[allow(dead_code)]
mod common;
#[path = "common/mvt.rs"]
mod mvt;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Every feature of every tile, as `layer` and decoded text, keyed by zoom.
fn features(path: &Path) -> BTreeMap<u8, Vec<(String, String)>> {
    let mut reader = archive::ArchiveReader::open(path).unwrap();
    let mut out = BTreeMap::<u8, Vec<(String, String)>>::new();
    reader
        .visit_entries(|reader, entry| {
            let stored = reader.tile(&entry)?;
            let mut tile = Vec::new();
            flate2::read::GzDecoder::new(&stored[..])
                .read_to_end(&mut tile)
                .unwrap();
            let coord = pmtiles::TileCoord::from(pmtiles::TileId::new(entry.tile_id).unwrap());
            for (layer, features) in mvt::decode_tile(&tile) {
                for feature in features {
                    out.entry(coord.z()).or_default().push((
                        layer.clone(),
                        format!("{} {}", feature.attributes, feature.geometry),
                    ));
                }
            }
            Ok(())
        })
        .unwrap();
    out
}

fn metadata(path: &Path) -> serde_json::Value {
    let text = archive::ArchiveReader::open(path)
        .unwrap()
        .metadata()
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

fn request(dir: &Path, name: &str, schema: TileSchema) -> BasemapRequest {
    let sources = dir.join("sources");
    if !sources.exists() {
        common::synthetic_sources(&sources);
    }
    let mut request = BasemapRequest::new(
        common::monaco_pbf(),
        dir.join(name),
        &sources,
        dir.join("temp"),
    );
    request.threads = 2;
    request.schema = schema;
    request
}

fn compile(request: &BasemapRequest) -> PathBuf {
    let output = compile_basemap(request, |_| {}, || false).unwrap();
    let report = archive::inspect(&output.archive).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    output.archive
}

#[test]
fn shortbread_compiles_monaco_with_its_own_layers_and_metadata() {
    let dir = tempfile::tempdir().unwrap();
    let request = request(dir.path(), "shortbread.pmtiles", TileSchema::Shortbread);
    // Shortbread reads the ocean polygons only.
    assert_eq!(
        request.used_sources(),
        vec![SupplementalDataset::WaterPolygons]
    );
    let output = compile_basemap(&request, |_| {}, || false).unwrap();
    assert!(output.result.schema.starts_with("Shortbread"));
    assert!(!output.result.parity_verified);
    let manifest = output.manifest();
    assert_eq!(manifest["schema"], "Shortbread");

    let metadata = metadata(&output.archive);
    assert_eq!(metadata["name"], "Shortbread");
    assert_eq!(metadata["schema"], "Shortbread");
    assert!(
        metadata["attribution"]
            .as_str()
            .unwrap()
            .contains("OpenStreetMap")
    );
    assert!(
        !metadata["attribution"]
            .as_str()
            .unwrap()
            .contains("OpenMapTiles")
    );
    for key in [
        "planetiler_openmaptiles_profile_commit",
        "version",
        "sar_schema",
    ] {
        assert!(metadata.get(key).is_none(), "{key}");
    }
    let ids = metadata["vector_layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|layer| layer["id"].as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    assert_eq!(ids, shortbread().layers());

    let features = features(&output.archive);
    let z14 = &features[&14];
    let has = |layer: &str, text: &str| {
        z14.iter()
            .any(|(l, feature)| l == layer && feature.contains(text))
    };
    assert!(has("streets", "kind=\"primary\""), "streets");
    assert!(has("buildings", ""), "buildings");
    assert!(has("place_labels", "name=\"Monaco\""), "place labels");
    // The synthetic sea polygon.
    assert!(has("ocean", ""), "ocean");
    // No OpenMapTiles layer is written.
    assert!(!z14.iter().any(|(l, _)| l == "transportation" || l == "poi"));
}

const ROADS: &str = r#"
schema_name: Roads
schema_description: Monaco's roads and buildings
attribution: OpenStreetMap contributors
sources:
  osm:
    type: osm
    url: geofabrik:monaco
layers:
- id: road
  features:
  - source: osm
    geometry: line
    min_zoom: 10
    include_when:
      highway: [primary, secondary, tertiary, residential]
    attributes:
    - key: class
      tag_value: highway
    - key: name
      min_zoom: 13
    - key: major
      value: true
      include_when:
        highway: primary
- id: building
  features:
  - source: osm
    geometry: polygon
    min_zoom: 14
    include_when:
      building: __any__
"#;

#[test]
fn a_yaml_schema_writes_what_it_says() {
    let dir = tempfile::tempdir().unwrap();
    let schema = Arc::new(Schema::from_yaml(ROADS).unwrap());
    let mut request = request(dir.path(), "roads.pmtiles", TileSchema::Custom(schema));
    // Nothing supplemental is read, so none is needed.
    assert!(request.used_sources().is_empty());
    request.sources = dir.path().join("no-sources");
    let archive = compile(&request);

    let roads_metadata = metadata(&archive);
    assert_eq!(roads_metadata["name"], "Roads");
    assert_eq!(
        roads_metadata["description"],
        "Monaco's roads and buildings"
    );
    assert_eq!(roads_metadata["attribution"], "OpenStreetMap contributors");
    let layers = roads_metadata["vector_layers"].as_array().unwrap();
    assert_eq!(layers.len(), 2);
    assert_eq!(layers[0]["minzoom"], 10);

    let features = features(&archive);
    assert!(
        !features.contains_key(&9),
        "nothing below the lowest min_zoom"
    );
    let roads = |z: u8| {
        features[&z]
            .iter()
            .filter(|(layer, _)| layer == "road")
            .map(|(_, f)| f.as_str())
            .collect::<Vec<_>>()
    };
    assert!(
        roads(10).iter().all(|f| !f.contains("name=")),
        "names start at 13"
    );
    assert!(roads(14).iter().any(|f| f.contains("name=")));
    assert!(roads(14).iter().any(|f| f.contains("major=true")));
    assert!(
        roads(14)
            .iter()
            .all(|f| f.contains("major=true") == f.contains("class=\"primary\""))
    );
    // Buildings keep every outline: OpenMapTiles' merging of its own
    // `building` layer does not apply to another schema's.
    let buildings = features[&14]
        .iter()
        .filter(|(layer, _)| layer == "building")
        .count();
    assert!(buildings > 1000, "{buildings} buildings");
    assert!(features[&13].iter().all(|(layer, _)| layer != "building"));

    // A selection of the schema's own layers.
    let mut only = request.clone();
    only.output = dir.path().join("only.pmtiles");
    only.layers = LayerSelection::only(["building"]);
    let only = compile(&only);
    let only = metadata(&only);
    assert_eq!(only["vector_layers"].as_array().unwrap().len(), 1);
}

#[test]
fn options_of_the_openmaptiles_schema_are_refused_with_another() {
    let dir = tempfile::tempdir().unwrap();
    let base = request(dir.path(), "x.pmtiles", TileSchema::Shortbread);
    let refused = |change: &dyn Fn(&mut BasemapRequest)| {
        let mut request = base.clone();
        change(&mut request);
        match request.validate() {
            Err(CompileError::InvalidRequest(message)) => message,
            other => panic!("expected a refusal, got {other:?}"),
        }
    };
    assert!(refused(&|r| r.sar = true).contains("Shortbread"));
    assert!(refused(&|r| r.languages = Some(vec!["en".into()])).contains("languages"));
    assert!(refused(&|r| r.building_levels = true).contains("building levels"));
    assert!(refused(&|r| r.search_index = true).contains("search"));
    assert!(refused(&|r| r.merge_cache.write = true).contains("merge"));
    let unknown = refused(&|r| r.layers = LayerSelection::only(["transportation"]));
    assert!(
        unknown.contains("unknown layer 'transportation'"),
        "{unknown}"
    );
    assert!(unknown.contains("streets"), "{unknown}");
    assert!(
        refused(&|r| {
            r.layers = LayerSelection::except(shortbread().layers());
        })
        .contains("no layer")
    );
}

#[test]
fn schema_files_report_their_mistakes() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("broken.yml");
    std::fs::write(&path, "schema_name: x\nsources:\n  osm: {type: osm}\nlayers:\n- id: a\n  features:\n  - source: osm\n    geometry: blob\n")
        .unwrap();
    let error = TileSchema::from_name_or_path(path.to_str().unwrap()).unwrap_err();
    assert!(error.contains("broken.yml"), "{error}");
    assert!(error.contains("blob"), "{error}");
    let missing = TileSchema::from_name_or_path("/no/such/schema.yml").unwrap_err();
    assert!(missing.contains("could not read"), "{missing}");
}
