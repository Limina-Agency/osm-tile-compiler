//! End-to-end test of data layers: Monaco compiled with a GeoJSON layer of
//! its own, a CSV layer in Web Mercator, and CSV points added to the
//! profile's `poi` layer. The layers hold their features at their zooms
//! with the attributes kept, the metadata lists them and credits them, and
//! the OpenMapTiles layers they leave alone come out the same.

use osm_tile_compiler::{
    BasemapRequest, CompileError, DataInput, DataLayer, LayerSelection, TileFormat, archive,
    compile_basemap,
};
#[allow(dead_code)]
mod common;
#[path = "common/mvt.rs"]
mod mvt;

use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// Every tile's layers, keyed by `z/x/y`, each feature as decoded text.
type Tiles = BTreeMap<(u8, u32, u32), BTreeMap<String, Vec<String>>>;

fn read_tiles(path: &Path) -> Tiles {
    let mut reader = archive::ArchiveReader::open(path).unwrap();
    let mut tiles = Tiles::new();
    reader
        .visit_entries(|reader, entry| {
            let stored = reader.tile(&entry)?;
            let mut tile = Vec::new();
            flate2::read::GzDecoder::new(&stored[..])
                .read_to_end(&mut tile)
                .unwrap();
            for id in entry.tile_id..entry.tile_id + u64::from(entry.run_length) {
                let coord = pmtiles::TileCoord::from(pmtiles::TileId::new(id).unwrap());
                let layers = mvt::decode_tile(&tile)
                    .into_iter()
                    .map(|(layer, features)| {
                        let features = features
                            .into_iter()
                            .map(|f| format!("{} {}", f.attributes, f.geometry))
                            .collect();
                        (layer, features)
                    })
                    .collect();
                tiles.insert((coord.z(), coord.x(), coord.y()), layers);
            }
            Ok(())
        })
        .unwrap();
    tiles
}

fn metadata(path: &Path) -> serde_json::Value {
    let text = archive::ArchiveReader::open(path)
        .unwrap()
        .metadata()
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The features of `layer` at zoom `z`, over every tile.
fn layer_at<'a>(tiles: &'a Tiles, layer: &str, z: u8) -> Vec<&'a String> {
    tiles
        .iter()
        .filter(|((zoom, _, _), _)| *zoom == z)
        .flat_map(|(_, layers)| layers.get(layer).into_iter().flatten())
        .collect()
}

/// Monaco's districts as two polygons, one with a hole, and a line.
const DISTRICTS: &str = r#"{"type": "FeatureCollection", "features": [
  {"type": "Feature", "id": 1, "properties": {"name": "Monte-Carlo", "population": 3500, "secret": "x"},
   "geometry": {"type": "Polygon", "coordinates": [[[7.415, 43.735], [7.43, 43.735], [7.43, 43.745], [7.415, 43.745], [7.415, 43.735]],
     [[7.42, 43.738], [7.42, 43.742], [7.425, 43.742], [7.425, 43.738], [7.42, 43.738]]]}},
  {"type": "Feature", "id": 2, "properties": {"name": "Fontvieille", "population": 3600},
   "geometry": {"type": "Polygon", "coordinates": [[[7.41, 43.725], [7.42, 43.725], [7.42, 43.73], [7.41, 43.73], [7.41, 43.725]]]}},
  {"type": "Feature", "id": 3, "properties": {"name": "Boulevard"},
   "geometry": {"type": "LineString", "coordinates": [[7.41, 43.73], [7.43, 43.74]]}},
  {"type": "Feature", "id": 4, "properties": {"name": "Paris"},
   "geometry": {"type": "Point", "coordinates": [2.35, 48.85]}}
]}"#;

/// Web Mercator metres of a longitude and latitude.
fn mercator(lon: f64, lat: f64) -> (f64, f64) {
    const R: f64 = 6_378_137.0;
    (
        lon.to_radians() * R,
        (std::f64::consts::FRAC_PI_4 + lat.to_radians() / 2.0)
            .tan()
            .ln()
            * R,
    )
}

fn write_inputs(dir: &Path) -> (PathBuf, PathBuf, PathBuf) {
    let districts = dir.join("districts.geojson");
    std::fs::write(&districts, DISTRICTS).unwrap();
    let stations = dir.join("stations.csv");
    let (x1, y1) = mercator(7.4197, 43.7389);
    let (x2, y2) = mercator(7.4256, 43.7401);
    std::fs::write(
        &stations,
        format!("x,y,label,kind\n{x1},{y1},Monaco-Ville,charger\n{x2},{y2},Port,charger\n"),
    )
    .unwrap();
    let extra_poi = dir.join("extra-poi.csv");
    std::fs::write(
        &extra_poi,
        "lat,lon,name,class,subclass\n43.7384,7.4246,Kiosk,shop,kiosk\n",
    )
    .unwrap();
    (districts, stations, extra_poi)
}

fn request(dir: &Path, name: &str) -> BasemapRequest {
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
    request
}

fn data_layers(dir: &Path) -> Vec<DataLayer> {
    let (districts, stations, extra_poi) = write_inputs(dir);
    let mut districts = DataLayer::new("districts", DataInput::new(districts)).zooms(10, 14);
    districts.exclude_attributes = vec!["secret".into()];
    districts.attribution = Some("© Monaco districts".into());
    let mut stations_input = DataInput::new(stations);
    stations_input.crs = Some("EPSG:3857".into());
    let mut stations = DataLayer::new("stations", stations_input).zooms(12, 14);
    stations
        .rename_attributes
        .insert("label".into(), "name".into());
    stations.attributes = Some(vec!["label".into()]);
    let poi = DataLayer::new("poi", DataInput::new(extra_poi)).zooms(14, 14);
    vec![districts, stations, poi]
}

#[test]
fn data_layers_compile_beside_the_basemap() {
    let dir = tempfile::tempdir().unwrap();
    let plain_path = dir.path().join("plain.pmtiles");
    compile_basemap(&request(dir.path(), "plain.pmtiles"), |_| {}, || false).unwrap();
    let mut with_data = request(dir.path(), "data.pmtiles");
    with_data.data_layers = data_layers(dir.path());
    let output = compile_basemap(&with_data, |_| {}, || false).unwrap();
    let data_path = dir.path().join("data.pmtiles");
    let report = archive::inspect(&data_path).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);

    // What was read.
    let reports = &output.result.data_layers;
    assert_eq!(reports.len(), 3);
    assert_eq!(reports[0].layer, "districts");
    assert_eq!(reports[0].format, "geojson");
    assert_eq!(reports[0].read.features, 3);
    assert_eq!(reports[0].read.outside_bounds, 1, "Paris is outside");
    assert_eq!(reports[1].read.crs, "EPSG:3857");
    assert_eq!(reports[1].read.features, 2);
    assert_eq!(reports[2].read.features, 1);
    assert!(
        !output.result.parity_verified,
        "the poi layer gained a feature"
    );

    // The metadata lists the new layers after the profile's, with their
    // zooms and fields, says where they came from, and credits them.
    let meta = metadata(&data_path);
    let layers = meta["vector_layers"].as_array().unwrap();
    let ids = layers
        .iter()
        .map(|l| l["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids[ids.len() - 2..], ["districts", "stations"]);
    assert_eq!(ids.iter().filter(|id| **id == "poi").count(), 1);
    let districts = layers.iter().find(|l| l["id"] == "districts").unwrap();
    assert_eq!(districts["minzoom"], 10);
    assert_eq!(districts["maxzoom"], 14);
    assert_eq!(districts["fields"]["population"], "Number");
    assert_eq!(districts["fields"]["name"], "String");
    assert!(districts["fields"].get("secret").is_none());
    let stations = layers.iter().find(|l| l["id"] == "stations").unwrap();
    assert_eq!(
        stations["fields"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        ["name"]
    );
    assert_eq!(meta["data_layers"][0]["file"], "districts.geojson");
    assert_eq!(meta["data_layers"][1]["format"], "csv");
    assert!(
        meta["attribution"]
            .as_str()
            .unwrap()
            .ends_with("© Monaco districts")
    );

    // The tiles: each layer only at its zooms, with what it keeps.
    let tiles = read_tiles(&data_path);
    assert!(layer_at(&tiles, "districts", 9).is_empty());
    let z14 = layer_at(&tiles, "districts", 14);
    assert!(z14.iter().any(|f| f.contains("Monte-Carlo")), "{z14:?}");
    assert!(z14.iter().any(|f| f.contains("Boulevard")));
    assert!(!z14.iter().any(|f| f.contains("secret")));
    assert!(!z14.iter().any(|f| f.contains("Paris")));
    let z10 = layer_at(&tiles, "districts", 10);
    assert!(z10.iter().any(|f| f.contains("Fontvieille")), "{z10:?}");
    assert!(layer_at(&tiles, "stations", 11).is_empty());
    let stations = layer_at(&tiles, "stations", 14);
    assert_eq!(stations.len(), 2, "{stations:?}");
    assert!(
        stations
            .iter()
            .all(|f| f.contains("name=") && !f.contains("kind"))
    );
    assert!(
        layer_at(&tiles, "poi", 14)
            .iter()
            .any(|f| f.contains("Kiosk"))
    );
    assert!(
        !layer_at(&tiles, "poi", 13)
            .iter()
            .any(|f| f.contains("Kiosk"))
    );

    // Every other profile layer is the same as without data layers.
    let plain = read_tiles(&plain_path);
    for (tile, layers) in &plain {
        for (layer, features) in layers {
            if layer != "poi" {
                assert_eq!(
                    tiles.get(tile).and_then(|t| t.get(layer)),
                    Some(features),
                    "{tile:?} {layer}"
                );
            }
        }
    }
}

#[test]
fn data_layers_compile_to_mlt() {
    let dir = tempfile::tempdir().unwrap();
    let mut request = request(dir.path(), "data-mlt.pmtiles");
    request.data_layers = data_layers(dir.path());
    request.archive.tile_format = TileFormat::Mlt;
    request.layers = LayerSelection::only(["water", "poi"]);
    compile_basemap(&request, |_| {}, || false).unwrap();
    let report = archive::inspect(&dir.path().join("data-mlt.pmtiles")).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
}

#[test]
fn data_layers_are_checked_before_the_compile() {
    let dir = tempfile::tempdir().unwrap();
    let mut request = request(dir.path(), "refused.pmtiles");
    request.data_layers = data_layers(dir.path());
    // `poi` is not written, so points cannot be added to it.
    request.layers = LayerSelection::only(["water"]);
    let error = request.validate().unwrap_err();
    assert!(matches!(error, CompileError::InvalidRequest(_)), "{error}");
    assert!(error.to_string().contains("poi"), "{error}");
    request.layers = LayerSelection::All;
    request.data_layers[0].source.path = dir.path().join("missing.geojson");
    assert!(request.validate().is_err());
}
