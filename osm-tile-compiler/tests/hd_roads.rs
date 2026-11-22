//! The HD roads pass over the Monaco fixture: the same bytes at any number of
//! threads, polygons that are valid as written, and what the tiles hold.

#[allow(dead_code)]
mod common;

use common::monaco_pbf;
use fast_mvt::{MvtGeometry, MvtReaderRef, MvtValueRef};
use flate2::read::GzDecoder;
use geo::{CoordsIter, MapCoords, Validation};
use geo_types::{Coord, Geometry, Polygon};
use osm_tile_compiler::{HD_ROADS_FORMAT, HD_ROADS_VERSION, HdRoadsReport, write_hd_roads};
use pmtiles::{AsyncPmTilesReader, TileCoord};
use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};

fn compile(directory: &Path, name: &str, threads: usize) -> (HdRoadsReport, PathBuf) {
    let output = directory.join(format!("{name}.pmtiles"));
    let temp = directory.join(format!("{name}.tmp"));
    fs::create_dir(&temp).unwrap();
    let report = write_hd_roads(&monaco_pbf(), &output, &temp, threads, || false).unwrap();
    (report, output)
}

#[test]
fn the_bytes_do_not_depend_on_the_threads() {
    let temp = tempfile::tempdir().unwrap();
    let (one, a) = compile(temp.path(), "one", 1);
    let (four, b) = compile(temp.path(), "four", 4);
    assert!(fs::read(a).unwrap() == fs::read(b).unwrap());
    assert_eq!((one.tiles, one.markings), (four.tiles, four.markings));
    assert!(
        one.ways > 500 && one.surfaces > 50 && one.markings > 100,
        "{one:?}"
    );
}

#[test]
fn an_existing_archive_is_never_replaced_and_a_cancel_leaves_none() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("roads.pmtiles");
    fs::write(&output, b"kept").unwrap();
    assert!(write_hd_roads(&monaco_pbf(), &output, temp.path(), 2, || false).is_err());
    assert_eq!(fs::read(&output).unwrap(), b"kept");
    let other = temp.path().join("cancelled.pmtiles");
    assert!(write_hd_roads(&monaco_pbf(), &other, temp.path(), 2, || true).is_err());
    assert!(!other.exists());
}

/// Every polygon of a geometry, in f64, and every point of it.
fn polygons(geometry: &MvtGeometry) -> Vec<Polygon<f64>> {
    let polygon = |p: &Polygon<i32>| {
        p.map_coords(|c| Coord {
            x: f64::from(c.x),
            y: f64::from(c.y),
        })
    };
    match geometry {
        Geometry::Polygon(p) => vec![polygon(p)],
        Geometry::MultiPolygon(m) => m.0.iter().map(polygon).collect(),
        other => panic!("a surface that is {other:?}"),
    }
}

#[tokio::test]
async fn every_polygon_is_valid_and_every_marking_is_within_its_tile() {
    let temp = tempfile::tempdir().unwrap();
    let (report, archive) = compile(temp.path(), "roads", 4);
    let reader = AsyncPmTilesReader::new_with_path(&archive).await.unwrap();
    let metadata: serde_json::Value =
        serde_json::from_str(&reader.get_metadata().await.unwrap()).unwrap();
    assert_eq!(metadata["format"], HD_ROADS_FORMAT);
    assert_eq!(metadata["version"], HD_ROADS_VERSION);
    let header = reader.get_header();
    let n = f64::from(1_u32 << 16);
    let x = |lon: f64| ((lon + 180.0) / 360.0 * n) as u32;
    let y =
        |lat: f64| ((1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n) as u32;
    let (mut tiles, mut surfaces, mut markings) = (0, 0, 0);
    let mut kinds: BTreeMap<String, u64> = BTreeMap::new();
    for tx in x(header.min_longitude) - 1..=x(header.max_longitude) + 1 {
        for ty in y(header.max_latitude) - 1..=y(header.min_latitude) + 1 {
            let coord = TileCoord::new(16, tx, ty).unwrap();
            let Some(bytes) = reader.get_tile(coord).await.unwrap() else {
                continue;
            };
            tiles += 1;
            let mut raw = Vec::new();
            GzDecoder::new(&bytes[..]).read_to_end(&mut raw).unwrap();
            let tile = MvtReaderRef::new(&raw).unwrap();
            for layer in tile.layers() {
                assert_eq!(layer.extent(), 4096);
                for feature in layer.features() {
                    let properties: BTreeMap<_, _> =
                        feature.properties_vec().unwrap().into_iter().collect();
                    let Some(MvtValueRef::String(kind)) = properties.get("kind") else {
                        panic!("a feature without a kind");
                    };
                    *kinds.entry(format!("{} {kind}", layer.name())).or_default() += 1;
                    let geometry = feature.geometry().unwrap();
                    // a crossing, a line to stop at and an arrow are whole
                    let whole = ["zebra", "dots", "crossing", "stop", "signal"].contains(kind)
                        || ["give_way", "arrow", "arrow-head"].contains(kind);
                    let past = if whole { 800 } else { 64 };
                    let reach = |v: i32| (-past..=4096 + past).contains(&v);
                    assert!(
                        geometry.coords_iter().all(|c| reach(c.x) && reach(c.y)),
                        "{kind} {geometry:?}"
                    );
                    if layer.name() == "surface" {
                        surfaces += 1;
                        for polygon in polygons(&geometry) {
                            assert!(
                                polygon.is_valid(),
                                "{tx}/{ty} {kind}: {:?}",
                                polygon.check_validation()
                            );
                        }
                    } else {
                        markings += 1;
                        let head = matches!(geometry, Geometry::Polygon(_));
                        assert!(head || matches!(geometry, Geometry::LineString(_)));
                        assert_eq!(head, *kind == "arrow-head");
                    }
                }
            }
        }
    }
    // z15 holds the roads' ways, to be asked of
    let n = f64::from(1_u32 << 15);
    let x = |lon: f64| ((lon + 180.0) / 360.0 * n) as u32;
    let y =
        |lat: f64| ((1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n) as u32;
    let mut roads = 0;
    for tx in x(header.min_longitude) - 1..=x(header.max_longitude) + 1 {
        for ty in y(header.max_latitude) - 1..=y(header.min_latitude) + 1 {
            let coord = TileCoord::new(15, tx, ty).unwrap();
            let Some(bytes) = reader.get_tile(coord).await.unwrap() else {
                continue;
            };
            tiles += 1;
            let mut raw = Vec::new();
            GzDecoder::new(&bytes[..]).read_to_end(&mut raw).unwrap();
            let tile = MvtReaderRef::new(&raw).unwrap();
            for layer in tile.layers() {
                assert_eq!((layer.name(), layer.extent()), ("road", 8192));
                for feature in layer.features() {
                    let properties: BTreeMap<_, _> =
                        feature.properties_vec().unwrap().into_iter().collect();
                    let Some(MvtValueRef::String(lanes)) = properties.get("lanes") else {
                        panic!("a road without lanes");
                    };
                    assert_eq!(lanes.split(',').count(), 5, "{lanes}");
                    assert!(properties.contains_key("way"));
                    roads += 1;
                }
            }
        }
    }
    // z14 holds the structures, whole groups in one tile
    let n = f64::from(1_u32 << 14);
    let x = |lon: f64| ((lon + 180.0) / 360.0 * n) as u32;
    let y =
        |lat: f64| ((1.0 - lat.to_radians().tan().asinh() / std::f64::consts::PI) / 2.0 * n) as u32;
    let (mut pieces, mut crossings, mut groups) = (0, 0, 0);
    let (mut barriers, mut portals) = (0, 0);
    for tx in x(header.min_longitude) - 1..=x(header.max_longitude) + 1 {
        for ty in y(header.max_latitude) - 1..=y(header.min_latitude) + 1 {
            let coord = TileCoord::new(14, tx, ty).unwrap();
            let Some(bytes) = reader.get_tile(coord).await.unwrap() else {
                continue;
            };
            tiles += 1;
            let mut raw = Vec::new();
            GzDecoder::new(&bytes[..]).read_to_end(&mut raw).unwrap();
            let tile = MvtReaderRef::new(&raw).unwrap();
            // the pieces of each group, and the pieces its crossings name
            let mut seen: BTreeMap<u64, (Vec<u64>, Vec<u64>)> = BTreeMap::new();
            let mut named: Vec<(u64, u64)> = Vec::new();
            for layer in tile.layers() {
                assert_eq!(layer.extent(), 65_536);
                for feature in layer.features() {
                    let properties: BTreeMap<_, _> =
                        feature.properties_vec().unwrap().into_iter().collect();
                    let uint = |key: &str| match properties.get(key) {
                        Some(MvtValueRef::UInt(v)) => Some(*v),
                        _ => None,
                    };
                    if layer.name() == "portal" {
                        portals += 1;
                        continue;
                    }
                    if layer.name() != "structure" {
                        assert!(["paint", "barrier"].contains(&layer.name()));
                        barriers += u64::from(layer.name() == "barrier");
                        let on = uint("group").zip(uint("piece"));
                        assert!(on.is_some() || layer.name() == "barrier");
                        named.extend(on);
                        continue;
                    }
                    let group = seen.entry(uint("group").unwrap()).or_default();
                    match feature.geometry().unwrap() {
                        Geometry::LineString(_) => {
                            pieces += 1;
                            group.0.push(uint("piece").unwrap());
                        }
                        Geometry::Point(_) => {
                            crossings += 1;
                            group.1.extend(uint("over"));
                            group.1.extend(uint("under"));
                        }
                        other => panic!("a structure that is {other:?}"),
                    }
                }
            }
            for (group, piece) in named {
                assert!(piece < seen[&group].0.len() as u64);
            }
            groups += seen.len() as u64;
            for (numbers, named) in seen.values() {
                let count = numbers.len() as u64;
                assert!(numbers.iter().copied().eq(0..count));
                assert!(named.iter().all(|n| *n < count));
            }
        }
    }
    assert!(pieces > 10 && crossings > 5, "{pieces} {crossings}");
    assert!(roads > report.ways / 3 && portals > 5, "{roads} {portals}");
    assert_eq!(
        (groups, pieces, crossings, barriers),
        (
            report.groups,
            report.pieces,
            report.crossings,
            report.barriers
        )
    );
    assert_eq!(
        (tiles, surfaces, markings),
        (report.tiles, report.surfaces, report.markings)
    );
    for kind in [
        "surface road",
        "surface sidewalk",
        "marking lane",
        "marking centre",
        "marking zebra",
    ] {
        assert!(kinds.get(kind).is_some_and(|&n| n > 0), "{kind}: {kinds:?}");
    }
}
