//! End-to-end test of the opt-in `sar` layer: a small synthetic PBF in the
//! Tatras with one feature of every class, compiled with and without it. The
//! layer holds what the schema says at the zooms it says, the metadata names
//! its schema, and the OpenMapTiles layers come out the same either way.

use osm_tile_compiler::{
    Area, BasemapRequest, CompileBounds, LayerSelection, SAR_LAYER, SAR_SCHEMA_VERSION, archive,
    compile_basemap,
};
#[allow(dead_code)]
mod common;
#[path = "common/mvt.rs"]
mod mvt;
#[path = "common/pbf.rs"]
mod pbf;

use common::synthetic_sources;
use pbf::{Tags, write_pbf};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

const BOX: [f64; 4] = [19.95, 49.20, 20.05, 49.26];

/// One node of every point class, a hut drawn as a building outline, a
/// power line, a cliff, graded and ungraded paths, and a plain hotel and
/// road for the OpenMapTiles layers.
fn write_tatras(path: &Path) {
    let points: [(f64, f64, Tags); 12] = [
        (
            20.000,
            49.230,
            &[
                ("tourism", "alpine_hut"),
                ("name", "Murowaniec"),
                ("ele", "1500"),
                ("capacity", "120"),
            ],
        ),
        (20.002, 49.231, &[("tourism", "wilderness_hut")]),
        (
            20.004,
            49.232,
            &[("amenity", "shelter"), ("shelter_type", "weather_shelter")],
        ),
        (
            20.006,
            49.233,
            &[("natural", "spring"), ("drinking_water", "yes")],
        ),
        (
            20.008,
            49.234,
            &[("emergency", "mountain_rescue"), ("name", "TOPR")],
        ),
        (
            20.010,
            49.235,
            &[("emergency", "access_point"), ("ref", "TT-07")],
        ),
        (20.012, 49.236, &[("emergency", "phone")]),
        (
            20.014,
            49.237,
            &[("aeroway", "helipad"), ("operator", "LPR")],
        ),
        (20.016, 49.238, &[("natural", "cave_entrance")]),
        (20.018, 49.239, &[("tourism", "hotel"), ("name", "Hotel")]),
        (20.020, 49.240, &[("emergency", "fire_hydrant")]),
        (20.022, 49.241, &[("man_made", "water_well")]),
    ];
    let mut nodes: Vec<(i64, f64, f64, Tags)> = points
        .iter()
        .enumerate()
        .map(|(i, &(lon, lat, tags))| (1 + i as i64, lon, lat, tags))
        .collect();
    // Plain nodes for the ways, from 100: pairs along a line each.
    let lines: [(f64, f64); 5] = [
        (49.210, 19.96), // power line
        (49.212, 19.96), // cliff
        (49.214, 19.96), // graded path
        (49.216, 19.96), // ungraded path
        (49.218, 19.96), // road
    ];
    for (i, (lat, lon)) in lines.iter().enumerate() {
        nodes.push((100 + 2 * i as i64, *lon, *lat, &[]));
        nodes.push((101 + 2 * i as i64, lon + 0.05, *lat, &[]));
    }
    // A building hut, 200-203.
    for (i, (lon, lat)) in [
        (20.030, 49.250),
        (20.031, 49.250),
        (20.031, 49.251),
        (20.030, 49.251),
    ]
    .into_iter()
    .enumerate()
    {
        nodes.push((200 + i as i64, lon, lat, &[]));
    }
    write_pbf(
        path,
        BOX,
        &nodes,
        &[
            (
                1000,
                &[100, 101],
                &[("power", "line"), ("voltage", "110000")],
            ),
            (1001, &[102, 103], &[("natural", "cliff")]),
            (
                1002,
                &[104, 105],
                &[
                    ("highway", "path"),
                    ("sac_scale", "alpine_hiking"),
                    ("name", "Orla Perć"),
                ],
            ),
            (1003, &[106, 107], &[("highway", "path")]),
            (
                1004,
                &[108, 109],
                &[("highway", "secondary"), ("name", "Droga")],
            ),
            (
                1005,
                &[200, 201, 202, 203, 200],
                &[("building", "hut"), ("name", "Bacówka")],
            ),
        ],
        &[],
    );
}

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

fn compile(dir: &Path, name: &str, configure: impl FnOnce(&mut BasemapRequest)) -> PathBuf {
    let sources = dir.join("sources");
    if !sources.exists() {
        synthetic_sources(&sources);
    }
    let pbf = dir.join("tatras.osm.pbf");
    if !pbf.exists() {
        write_tatras(&pbf);
    }
    let output = dir.join(name);
    let mut request = BasemapRequest::new(&pbf, &output, &sources, dir.join("temp"));
    let [west, south, east, north] = BOX;
    request.area = Area::bounds(CompileBounds::new(north, south, east, west));
    request.threads = 2;
    configure(&mut request);
    compile_basemap(&request, |_| {}, || false).unwrap();
    output
}

fn metadata(path: &Path) -> serde_json::Value {
    let text = archive::ArchiveReader::open(path)
        .unwrap()
        .metadata()
        .unwrap();
    serde_json::from_str(&text).unwrap()
}

fn layer_ids(metadata: &serde_json::Value) -> Vec<String> {
    metadata["vector_layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|layer| layer["id"].as_str().unwrap().to_string())
        .collect()
}

/// The `sar` features at zoom `z`, over every tile.
fn sar_at(tiles: &Tiles, z: u8) -> Vec<&String> {
    tiles
        .iter()
        .filter(|((zoom, _, _), _)| *zoom == z)
        .flat_map(|(_, layers)| layers.get(SAR_LAYER).into_iter().flatten())
        .collect()
}

fn count(features: &[&String], pattern: &str) -> usize {
    features.iter().filter(|f| f.contains(pattern)).count()
}

#[test]
fn the_sar_layer_is_opt_in_and_leaves_the_basemap_alone() {
    let dir = tempfile::tempdir().unwrap();
    let plain = compile(dir.path(), "plain.pmtiles", |_| {});
    let with_sar = compile(dir.path(), "sar.pmtiles", |request| request.sar = true);
    let excluded = compile(dir.path(), "excluded.pmtiles", |request| {
        request.sar = true;
        request.layers = LayerSelection::except([SAR_LAYER]);
    });
    let only = compile(dir.path(), "only.pmtiles", |request| {
        request.layers = LayerSelection::only([SAR_LAYER]);
    });
    for archive in [&plain, &with_sar, &excluded, &only] {
        let report = archive::inspect(archive).unwrap();
        assert!(report.problems.is_empty(), "{:?}", report.problems);
    }

    // Without the flag, or with it left out, nothing changes: the same bytes.
    assert_eq!(
        std::fs::read(&plain).unwrap(),
        std::fs::read(&excluded).unwrap()
    );
    let plain_metadata = metadata(&plain);
    assert!(plain_metadata.get("sar_schema").is_none());
    assert!(!layer_ids(&plain_metadata).iter().any(|id| id == SAR_LAYER));

    // With it, the metadata names the layer, last, and its schema.
    let sar_metadata = metadata(&with_sar);
    assert_eq!(sar_metadata["sar_schema"], SAR_SCHEMA_VERSION);
    let ids = layer_ids(&sar_metadata);
    assert_eq!(ids.last().map(String::as_str), Some(SAR_LAYER));
    assert_eq!(ids[..ids.len() - 1], layer_ids(&plain_metadata)[..]);
    let sar_layer = sar_metadata["vector_layers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|layer| layer["id"] == SAR_LAYER)
        .unwrap();
    assert_eq!(sar_layer["minzoom"], 10);
    assert_eq!(sar_layer["maxzoom"], 14);
    for field in [
        "class",
        "subclass",
        "name",
        "ele",
        "capacity",
        "ref",
        "sac_scale",
    ] {
        assert!(sar_layer["fields"].get(field).is_some(), "{field}");
    }

    // Every OpenMapTiles layer of every tile is the same as without it.
    let plain_tiles = read_tiles(&plain);
    let mut sar_tiles = read_tiles(&with_sar);
    for layers in sar_tiles.values_mut() {
        layers.remove(SAR_LAYER);
    }
    sar_tiles.retain(|_, layers| !layers.is_empty());
    assert_eq!(plain_tiles, sar_tiles);

    let tiles = read_tiles(&with_sar);
    let z14 = sar_at(&tiles, 14);
    for subclass in [
        "alpine_hut",
        "wilderness_hut",
        "shelter",
        "hut",
        "spring",
        "water_well",
        "mountain_rescue",
        "access_point",
        "phone",
        "helipad",
        "cave_entrance",
        "power_line",
        "cliff",
        "path",
    ] {
        assert!(
            count(&z14, &format!(" subclass=\"{subclass}\"")) >= 1,
            "{subclass} at z14: {z14:#?}"
        );
    }
    // The hotel, the hydrant, the plain path and the road are not in it.
    assert_eq!(count(&z14, "Hotel"), 0);
    assert_eq!(count(&z14, "fire_hydrant"), 0);
    assert_eq!(count(&z14, "Droga"), 0);
    assert_eq!(count(&z14, " subclass=\"path\""), count(&z14, "Orla Perć"));
    // Attributes, as the schema writes them.
    for pattern in [
        " capacity=120",
        " ele=1500",
        " drinking_water=1",
        " ref=\"TT-07\"",
        " operator=\"LPR\"",
        " voltage=110000",
        " sac_scale=\"alpine_hiking\"",
        " shelter_type=\"weather_shelter\"",
        " class=\"emergency\"",
        " class=\"landing\"",
        " class=\"hazard\"",
        " class=\"trail\"",
    ] {
        assert!(count(&z14, pattern) >= 1, "{pattern}: {z14:#?}");
    }

    // Zooms: named huts, rescue and landing from 10; power lines from 11;
    // trails, water and unnamed shelters from 12; access points and cliffs
    // from 13; phones and caves at 14. Points near a tile edge also appear
    // in the next tile's buffer, so these count at least one.
    let at = |z| sar_at(&tiles, z);
    let z10 = at(10);
    for name in ["Murowaniec", "TOPR", "Bacówka"] {
        assert!(count(&z10, name) >= 1, "{name} at z10: {z10:#?}");
    }
    assert!(count(&z10, "helipad") >= 1);
    assert_eq!(count(&z10, "power_line"), 0);
    assert!(count(&at(11), "power_line") >= 1);
    assert_eq!(count(&at(11), "spring"), 0);
    assert!(count(&at(12), "spring") >= 1);
    assert!(count(&at(12), "Orla Perć") >= 1);
    assert_eq!(count(&at(12), "access_point"), 0);
    assert!(count(&at(13), "access_point") >= 1);
    assert!(count(&at(13), "cliff") >= 1);
    assert_eq!(count(&at(13), "phone"), 0);
    assert_eq!(count(&at(13), "cave_entrance"), 0);
    assert!(tiles.keys().all(|(z, _, _)| *z <= 14));
    assert!(sar_at(&tiles, 9).is_empty());

    // Only `sar`, asked for by name.
    let only_metadata = metadata(&only);
    assert_eq!(layer_ids(&only_metadata), [SAR_LAYER]);
    assert_eq!(only_metadata["sar_schema"], SAR_SCHEMA_VERSION);
    let only_tiles = read_tiles(&only);
    assert!(
        only_tiles
            .values()
            .all(|layers| layers.keys().all(|layer| layer == SAR_LAYER))
    );
    assert_eq!(sar_at(&only_tiles, 14), z14);
}
