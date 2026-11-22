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
                    let reach = |v: i32| (-64..=4160).contains(&v);
                    assert!(geometry.coords_iter().all(|c| reach(c.x) && reach(c.y)));
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
                        assert!(matches!(geometry, Geometry::LineString(_)));
                    }
                }
            }
        }
    }
    assert_eq!(
        (tiles, surfaces, markings),
        (report.tiles, report.surfaces, report.markings)
    );
    for kind in [
        "surface road",
        "surface sidewalk",
        "marking lane",
        "marking centre",
    ] {
        assert!(kinds.get(kind).is_some_and(|&n| n > 0), "{kind}: {kinds:?}");
    }
}
