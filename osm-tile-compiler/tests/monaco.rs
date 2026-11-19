//! End-to-end test: compiles `fixtures/monaco.osm.pbf` into PMTiles with small
//! synthetic supplemental inputs (one sea polygon, one out-of-region lake line,
//! an empty Natural Earth database) and compares the archive with the record in
//! `fixtures/monaco-features.json`.

use osm_tile_compiler::{
    Area, BasemapRequest, CompileArea, CompileBounds, CompileResult, LayerSelection,
    OMT_LANGUAGES as LANGUAGES, OMT_LAYERS as LAYERS, compile_basemap,
};
mod common;
#[path = "common/mvt.rs"]
mod mvt;

use common::{monaco_pbf, synthetic_sources};
use mvt::decode_tile;
use pmtiles::{AsyncPmTilesReader, Compression, Header, TileCoord, TileId, TileType};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

fn pbf_bounds(path: &Path) -> CompileBounds {
    let index = osm_pbf_ingest::blob_index::PbfBlobIndex::scan(path).unwrap();
    let b = index
        .header()
        .bounds
        .expect("monaco.osm.pbf has a bounding box");
    CompileBounds::new(b.north, b.south, b.east, b.west)
}

fn compile(
    sources: &Path,
    out: &Path,
    threads: usize,
    area: Option<CompileArea>,
) -> (CompileResult, PathBuf) {
    compile_selected(sources, out, threads, area, LayerSelection::All, None)
}

fn compile_selected(
    sources: &Path,
    out: &Path,
    threads: usize,
    area: Option<CompileArea>,
    layers: LayerSelection,
    languages: Option<Vec<String>>,
) -> (CompileResult, PathBuf) {
    let output = out.join("basemap.pmtiles");
    let mut request = BasemapRequest::new(monaco_pbf(), &output, sources, out.join("temp"));
    request.area = Area::bounds(pbf_bounds(&monaco_pbf()));
    request.area.polygon = area.map(Into::into);
    request.threads = threads;
    request.layers = layers;
    request.languages = languages;
    let result = compile_basemap(&request, |_| {}, || false).unwrap().result;
    (result, output)
}

fn tile_range(bounds: (f64, f64, f64, f64), z: u8) -> (u32, u32, u32, u32) {
    let (west, south, east, north) = bounds;
    let n = f64::from(1_u32 << z);
    let x = |lon: f64| (((lon + 180.0) / 360.0 * n) as u32).min((1 << z) - 1);
    let y = |lat: f64| {
        let lat = lat.to_radians();
        (((1.0 - lat.tan().asinh() / std::f64::consts::PI) / 2.0 * n) as u32).min((1 << z) - 1)
    };
    (x(west), y(north), x(east), y(south))
}

/// First 16 hex digits of the SHA-256 of the lines, each ending in `\n`.
fn digest<'a>(lines: impl Iterator<Item = &'a str>) -> String {
    let mut hasher = Sha256::new();
    for line in lines {
        hasher.update(line.as_bytes());
        hasher.update(b"\n");
    }
    hex::encode(&hasher.finalize()[..8])
}

/// The header fields the compiler decides, so that a change to them shows up.
fn header_record(header: &Header) -> Value {
    let compression = |c: Compression| format!("{c:?}");
    json!({
        "tile_type": format!("{:?}", header.tile_type),
        "tile_compression": compression(header.tile_compression),
        "internal_compression": compression(header.internal_compression()),
        "clustered": header.clustered(),
        "zooms": [header.min_zoom, header.max_zoom],
        "bounds": [header.min_longitude, header.min_latitude, header.max_longitude, header.max_latitude],
        "center": [header.center_longitude, header.center_latitude, header.center_zoom],
        "addressed_tiles": header.n_addressed_tiles().map(|n| n.get()),
        "tile_entries": header.n_tile_entries().map(|n| n.get()),
        "tile_contents": header.n_tile_contents().map(|n| n.get()),
    })
}

/// Differences between two JSON values, one line per changed leaf.
fn json_differences(path: &str, expected: &Value, actual: &Value, out: &mut Vec<String>) {
    match (expected, actual) {
        (Value::Object(old), Value::Object(new)) => {
            let keys = old
                .keys()
                .chain(new.keys())
                .collect::<std::collections::BTreeSet<_>>();
            for key in keys {
                let (old, new) = (old.get(key), new.get(key));
                let path = format!("{path}/{key}");
                match (old, new) {
                    (Some(old), Some(new)) => json_differences(&path, old, new, out),
                    (old, new) => {
                        let show = |v: Option<&Value>| v.map_or("none".into(), Value::to_string);
                        out.push(format!("{path}: {} -> {}", show(old), show(new)));
                    }
                }
            }
        }
        (old, new) if old != new => out.push(format!("{path}: {old} -> {new}")),
        _ => {}
    }
}

const RECORD: &str = "../fixtures/monaco-features.json";

fn read_record() -> Value {
    serde_json::from_str(
        &fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join(RECORD)).unwrap(),
    )
    .unwrap()
}

/// Compares the archive's record with `fixtures/monaco-features.json`: the
/// header and metadata, tile and feature counts, and per tile and layer a
/// digest of the features' identities and attributes and one of their
/// geometries, so any change to the output shows up in review.
/// `UPDATE_MONACO_FEATURES=1` rewrites the file.
fn assert_matches_record(actual: &Value) {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(RECORD);
    if std::env::var_os("UPDATE_MONACO_FEATURES").is_some() {
        let text = serde_json::to_string_pretty(actual).unwrap() + "\n";
        fs::write(&path, text).unwrap();
        return;
    }
    let expected: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let mut differences = Vec::new();
    json_differences("", &expected, actual, &mut differences);
    let geometry_only = !differences.is_empty()
        && differences
            .iter()
            .all(|d| d.starts_with("/tiles/") && d.contains("/geometry:"));
    assert!(
        differences.is_empty(),
        "Monaco output differs from fixtures/monaco-features.json:\n  {}\n{}If the change is \
         intended, rerun with UPDATE_MONACO_FEATURES=1, commit the updated file and explain \
         the change. MONACO_DUMP=<file> writes every feature as text; dump before and after \
         the change and diff the two files to see what changed.",
        differences.join("\n  "),
        if geometry_only {
            "Only geometry changed. The record is made on Linux x86_64, as CI runs; on another \
             platform its math library can round a coordinate differently, so check that the \
             change reproduces on Linux before recording it.\n"
        } else {
            ""
        }
    );
}

/// The compile result's tile statistics agree with the tiles in the archive.
fn assert_tile_stats_match(
    result: &CompileResult,
    tiles_per_zoom: &[u64],
    sizes: &[(u64, String, u64, u64)],
) {
    let stats = &result.tile_stats;
    assert_eq!(stats.tiles(), result.tiles);
    let zooms_with_tiles = (0..=14u8)
        .filter(|&z| tiles_per_zoom[z as usize] > 0)
        .collect::<Vec<_>>();
    assert_eq!(
        stats.zooms.iter().map(|zoom| zoom.zoom).collect::<Vec<_>>(),
        zooms_with_tiles
    );
    for zoom in &stats.zooms {
        let at_zoom = sizes
            .iter()
            .filter(|(_, name, ..)| name.starts_with(&format!("{}/", zoom.zoom)))
            .collect::<Vec<_>>();
        assert_eq!(zoom.tiles, at_zoom.len() as u64, "z{}", zoom.zoom);
        assert_eq!(zoom.bytes, at_zoom.iter().map(|t| t.2).sum::<u64>());
        assert_eq!(zoom.max_bytes, at_zoom.iter().map(|t| t.2).max().unwrap());
        assert!(zoom.p50_bytes <= zoom.p90_bytes && zoom.p90_bytes <= zoom.p99_bytes);
        assert!(zoom.p99_bytes <= zoom.max_bytes);
    }
    let mut expected = sizes.to_vec();
    expected.sort_by(|a, b| b.2.cmp(&a.2).then(a.0.cmp(&b.0)));
    expected.truncate(10);
    assert_eq!(
        stats
            .largest
            .iter()
            .map(|t| (t.tile.clone(), t.bytes))
            .collect::<Vec<_>>(),
        expected
            .iter()
            .map(|t| (t.1.clone(), t.2))
            .collect::<Vec<_>>()
    );
    for (tile, (.., raw)) in stats.largest.iter().zip(&expected) {
        // Each layer is a field of the tile message, so the layers' sizes add
        // up to the uncompressed tile.
        assert_eq!(
            tile.layers.iter().map(|l| l.1).sum::<u64>(),
            *raw,
            "{}",
            tile.tile
        );
    }
    assert_eq!(stats.over_warning, 0);
}

/// What a test reads back from an archive.
struct Archive {
    /// `header_record` of the archive's header.
    header: Value,
    metadata: Value,
    tiles_per_zoom: Vec<u64>,
    /// Feature counts per zoom and layer.
    features: BTreeMap<String, BTreeMap<String, usize>>,
    /// Per tile and layer, the feature count and content digests.
    content: serde_json::Map<String, Value>,
    /// Every feature as one line of text.
    dump: String,
    /// (tile ID, z/x/y, stored bytes, uncompressed bytes) of every tile.
    sizes: Vec<(u64, String, u64, u64)>,
}

/// Decodes every tile within the archive's bounds.
async fn read_archive(path: &Path) -> Archive {
    let reader = AsyncPmTilesReader::new_with_path(path).await.unwrap();
    let header = header_record(reader.get_header());
    let metadata: Value = serde_json::from_str(&reader.get_metadata().await.unwrap()).unwrap();
    let header_bounds = reader.get_header();
    let bounds = (
        header_bounds.min_longitude,
        header_bounds.min_latitude,
        header_bounds.max_longitude,
        header_bounds.max_latitude,
    );
    let mut tiles_per_zoom = Vec::new();
    let mut features = BTreeMap::new();
    let mut content = serde_json::Map::new();
    let mut dump = String::new();
    let mut sizes = Vec::new();
    for z in 0..=14 {
        let (x0, y0, x1, y1) = tile_range(bounds, z);
        let mut tiles = 0;
        let zoom_features = features
            .entry(format!("z{z:02}"))
            .or_insert_with(BTreeMap::new);
        for x in x0..=x1 {
            for y in y0..=y1 {
                let coord = TileCoord::new(z, x, y).unwrap();
                let Some(tile) = reader.get_tile_decompressed(coord).await.unwrap() else {
                    continue;
                };
                tiles += 1;
                let stored = reader.get_tile(coord).await.unwrap().unwrap();
                sizes.push((
                    TileId::from(coord).value(),
                    format!("{z}/{x}/{y}"),
                    stored.len() as u64,
                    tile.len() as u64,
                ));
                let layers = decode_tile(&tile);
                assert!(!layers.is_empty(), "z{z}/{x}/{y} has no layers");
                let mut tile_record = serde_json::Map::new();
                for (name, layer) in layers {
                    *zoom_features.entry(name.clone()).or_insert(0) += layer.len();
                    for (index, feature) in layer.iter().enumerate() {
                        writeln!(
                            dump,
                            "{z}/{x}/{y} {name} #{index} {} | {}",
                            feature.attributes, feature.geometry
                        )
                        .unwrap();
                    }
                    let previous = tile_record.insert(
                        name.clone(),
                        json!({
                            "features": layer.len(),
                            "attributes": digest(layer.iter().map(|f| f.attributes.as_str())),
                            "geometry": digest(layer.iter().map(|f| f.geometry.as_str())),
                        }),
                    );
                    assert!(previous.is_none(), "z{z}/{x}/{y} repeats layer {name}");
                }
                content.insert(format!("{z}/{x}/{y}"), Value::Object(tile_record));
            }
        }
        tiles_per_zoom.push(tiles);
    }
    Archive {
        header,
        metadata,
        tiles_per_zoom,
        features,
        content,
        dump,
        sizes,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn compiles_monaco_into_a_complete_deterministic_archive() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);

    let (result, archive) = compile(&sources, &temp.path().join("four-threads"), 4, None);
    assert!(result.profile_features > 0, "{result:?}");
    // The full profile is what docs/parity.md measures against Planetiler.
    assert!(result.parity_verified);
    // The extract leaves these relations incomplete; a change in the counts
    // shows up here.
    assert_eq!(
        result.warnings,
        [
            "skipped incomplete source geometries: 0 ways with nodes missing from the extract, \
             30 relations whose rings could not be assembled"
        ]
    );

    let Archive {
        header,
        metadata,
        tiles_per_zoom,
        features,
        content,
        dump,
        sizes,
    } = read_archive(&archive).await;
    assert_eq!(header["tile_type"], format!("{:?}", TileType::Mvt));
    assert_eq!(header["zooms"], json!([0, 14]));
    let declared = metadata["vector_layers"]
        .as_array()
        .expect("metadata lists vector_layers")
        .iter()
        .map(|layer| layer["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    for layer in LAYERS {
        assert!(
            declared.contains(&layer),
            "{layer} missing from {declared:?}"
        );
    }
    if let Some(path) = std::env::var_os("MONACO_DUMP") {
        fs::write(path, &dump).unwrap();
    }
    assert_eq!(
        tiles_per_zoom.iter().sum::<u64>(),
        result.tiles,
        "every written tile lies inside the bounds"
    );
    assert_tile_stats_match(&result, &tiles_per_zoom, &sizes);
    let z14 = &features["z14"];
    for layer in [
        "building",
        "housenumber",
        "poi",
        "transportation",
        "transportation_name",
        "water",
    ] {
        assert!(
            z14.get(layer).is_some_and(|&count| count > 0),
            "no {layer} features at z14: {z14:?}"
        );
    }
    assert_matches_record(&json!({
        "header": header,
        "metadata": metadata,
        "tiles_per_zoom": tiles_per_zoom,
        "features": features,
        "tiles": content,
    }));

    // Output order does not depend on the worker count.
    let (_, single) = compile(&sources, &temp.path().join("one-thread"), 1, None);
    assert!(
        fs::read(&archive).unwrap() == fs::read(&single).unwrap(),
        "1-thread and 4-thread archives differ"
    );
}

/// A polygon limits which tiles are written, not what they contain: every
/// tile of the limited archive is the same as in the full one, and those left
/// out are the ones the polygon misses.
#[tokio::test(flavor = "multi_thread")]
async fn a_polygon_writes_only_the_tiles_it_covers() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    // The western part of Monaco.
    let (west, south, east, north) = (7.4031, 43.7208, 7.4215, 43.7597);
    let area = CompileArea::from_geojson(&format!(
        r#"{{"type":"Polygon","coordinates":[[[{west},{south}],[{east},{south}],[{east},{north}],[{west},{north}],[{west},{south}]]]}}"#
    ))
    .unwrap();
    let (result, archive) = compile(&sources, &temp.path().join("polygon"), 2, Some(area));
    let limited = read_archive(&archive).await;
    let record = read_record();
    let full = record["tiles"].as_object().unwrap();

    // The header describes the overlap of the polygon and the extract.
    let pbf = pbf_bounds(&monaco_pbf());
    let bounds = limited.header["bounds"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap())
        .collect::<Vec<_>>();
    let expected_bounds = [pbf.west, pbf.south.max(south), east, pbf.north.min(north)];
    for (actual, expected) in bounds.iter().zip(expected_bounds) {
        assert!((actual - expected).abs() < 1e-6, "{bounds:?}");
    }
    let center = limited.header["center"][0].as_f64().unwrap();
    assert!((center - (pbf.west + east) / 2.0).abs() < 1e-6);

    let polygon_tiles = |z: u8| tile_range((west, south, east, north), z);
    let mut expected = Vec::new();
    for (tile, layers) in full {
        let z_x_y = tile
            .split('/')
            .map(|v| v.parse::<u32>().unwrap())
            .collect::<Vec<_>>();
        let (x0, y0, x1, y1) = polygon_tiles(z_x_y[0] as u8);
        if (x0..=x1).contains(&z_x_y[1]) && (y0..=y1).contains(&z_x_y[2]) {
            expected.push((tile.clone(), layers.clone()));
        }
    }
    let written = limited
        .content
        .iter()
        .map(|(tile, layers)| (tile.clone(), layers.clone()))
        .collect::<Vec<_>>();
    assert!(
        written.len() < full.len(),
        "{} of {}",
        written.len(),
        full.len()
    );
    let sorted = |mut tiles: Vec<(String, Value)>| {
        tiles.sort_by(|a, b| a.0.cmp(&b.0));
        tiles
    };
    assert_eq!(sorted(written), sorted(expected));
    assert_eq!(result.tiles, limited.content.len() as u64);
}

/// The metadata's `vector_layers` entries by layer ID.
fn vector_layers(metadata: &Value) -> BTreeMap<String, Value> {
    metadata["vector_layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|layer| (layer["id"].as_str().unwrap().to_string(), layer.clone()))
        .collect()
}

/// A layer subset writes exactly the full build's content of those layers:
/// in every tile, each selected layer has the same features, attributes and
/// geometry, a tile with none of them is left out, and the metadata declares
/// only the selected layers, as the full build does. The subsets take layers
/// apart from those whose processing they depend on: water names from water
/// polygons (and, leaving out water, the ocean polygons are not read at all),
/// road names from roads, park labels from park outlines, and the ranks of
/// places and peaks from their tile's other points.
#[tokio::test(flavor = "multi_thread")]
async fn selected_layers_match_the_full_build() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let record = read_record();
    let full_tiles = record["tiles"].as_object().unwrap();
    let full_layers = vector_layers(&record["metadata"]);
    let subsets: [&[&str]; 3] = [
        &["water_name", "transportation_name", "park"],
        &[
            "water",
            "waterway",
            "landcover",
            "landuse",
            "building",
            "housenumber",
        ],
        &[
            "mountain_peak",
            "boundary",
            "aeroway",
            "place",
            "poi",
            "aerodrome_label",
        ],
    ];
    let excluded = ["water", "transportation", "poi", "housenumber"];
    let cases = subsets
        .iter()
        .map(|subset| (subset.to_vec(), Some(subset.to_vec()), None))
        .chain([(
            LAYERS
                .iter()
                .copied()
                .filter(|layer| !excluded.contains(layer))
                .collect(),
            None,
            Some(excluded.to_vec()),
        )]);
    for (index, (selected, only, without)) in cases.enumerate() {
        let layers = match (only, without) {
            (Some(only), None) => LayerSelection::only(only),
            (None, Some(without)) => LayerSelection::except(without),
            _ => LayerSelection::All,
        };
        let (result, archive) = compile_selected(
            &sources,
            &temp.path().join(format!("subset-{index}")),
            3,
            None,
            layers,
            None,
        );
        let subset = read_archive(&archive).await;
        let mut expected = serde_json::Map::new();
        for (tile, layers) in full_tiles {
            let kept = layers
                .as_object()
                .unwrap()
                .iter()
                .filter(|(layer, _)| selected.contains(&layer.as_str()))
                .map(|(layer, content)| (layer.clone(), content.clone()))
                .collect::<serde_json::Map<_, _>>();
            if !kept.is_empty() {
                expected.insert(tile.clone(), Value::Object(kept));
            }
        }
        assert!(!expected.is_empty(), "{selected:?}");
        let mut differences = Vec::new();
        json_differences(
            "",
            &Value::Object(expected),
            &Value::Object(subset.content.clone()),
            &mut differences,
        );
        assert!(
            differences.is_empty(),
            "{selected:?} differs from the full build:\n  {}",
            differences.join("\n  ")
        );
        assert_eq!(result.tiles, subset.content.len() as u64);
        let declared = subset.metadata["vector_layers"]
            .as_array()
            .unwrap()
            .iter()
            .map(|layer| layer["id"].as_str().unwrap())
            .collect::<Vec<_>>();
        let in_schema_order = LAYERS
            .iter()
            .copied()
            .filter(|layer| selected.contains(layer))
            .collect::<Vec<_>>();
        assert_eq!(declared, in_schema_order);
        for (layer, entry) in &vector_layers(&subset.metadata) {
            assert_eq!(entry, &full_layers[layer], "{layer}");
        }
    }
}

/// `languages` leaves out the `name:<lang>` attributes of the other
/// languages and keeps every other name attribute.
#[tokio::test(flavor = "multi_thread")]
async fn languages_limit_the_translated_names() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let full_layers = vector_layers(&read_record()["metadata"]);
    let kept = ["en", "it"];
    let (languages_result, archive) = compile_selected(
        &sources,
        &temp.path().join("languages"),
        3,
        None,
        LayerSelection::All,
        Some(kept.iter().map(|code| format!("name:{code}")).collect()),
    );
    // Other languages than Planetiler's default are outside the measurement.
    assert!(!languages_result.parity_verified);
    let limited = read_archive(&archive).await;
    let dropped = |key: &str| {
        key.strip_prefix("name:")
            .is_some_and(|code| LANGUAGES.contains(&code) && !kept.contains(&code))
    };
    // The fields of each layer are the full build's without the dropped
    // languages: the metadata describes what the tiles hold.
    for (layer, entry) in vector_layers(&limited.metadata) {
        let mut expected = full_layers[&layer].clone();
        let fields = expected["fields"].as_object_mut().unwrap();
        fields.retain(|key, _| !dropped(key));
        assert_eq!(entry, expected, "{layer}");
    }
    let keys = limited
        .dump
        .lines()
        .flat_map(|line| line.split(" | ").next().unwrap().split(' '))
        .filter_map(|item| item.split_once('=').map(|(key, _)| key))
        .collect::<std::collections::BTreeSet<_>>();
    assert!(!keys.iter().any(|key| dropped(key)), "{keys:?}");
    for key in [
        "name",
        "name_en",
        "name_de",
        "name_int",
        "name:latin",
        "name:en",
        "name:it",
    ] {
        assert!(keys.contains(key), "{key} missing from {keys:?}");
    }
}
