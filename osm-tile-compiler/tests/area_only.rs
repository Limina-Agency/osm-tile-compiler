//! End-to-end test of an area-only compile: from a synthetic PBF reaching far
//! beyond a small area, it reads only what lies near the area, yet writes the
//! same zoom-14 tiles as a full read, including a road that crosses the area
//! without a node in it and polygons that surround it.

use osm_tile_compiler::{
    CompileArea, CompileBounds, SupplementalDataset, archive,
    omt::{OpenMapTilesRequest, OpenMapTilesResult, compile_openmaptiles},
};
#[allow(dead_code)]
#[path = "common/mvt.rs"]
mod mvt;
#[path = "common/pbf.rs"]
mod pbf;

use pbf::{Tags, write_pbf};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

/// The area: about half a zoom-14 tile each way, at 20 E 50 N.
const AREA: [f64; 4] = [20.000, 50.000, 20.010, 50.006];

/// Inside the area a cafe; through it a road with its nodes far outside;
/// around it a forest (a closed way) and a lake (a multipolygon of two
/// ways); and half a degree east a town, a motorway and a building.
fn write_extract(path: &Path) {
    let nodes: Vec<(i64, f64, f64, Tags)> = vec![
        (
            1,
            20.005,
            50.003,
            &[("amenity", "cafe"), ("name", "Inside Cafe")],
        ),
        (2, 20.5, 50.0, &[("place", "town"), ("name", "Far Town")]),
        (10, 19.85, 50.003, &[]),
        (11, 20.15, 50.003, &[]),
        (20, 19.9, 49.95, &[]),
        (21, 20.1, 49.95, &[]),
        (22, 20.1, 50.05, &[]),
        (23, 19.9, 50.05, &[]),
        (30, 19.95, 49.97, &[]),
        (31, 20.05, 49.97, &[]),
        (32, 20.05, 50.03, &[]),
        (33, 19.95, 50.03, &[]),
        (40, 20.4, 50.0, &[]),
        (41, 20.401, 50.0, &[]),
        (42, 20.401, 50.001, &[]),
        (43, 20.4, 50.001, &[]),
        (50, 20.45, 49.99, &[]),
        (51, 20.55, 49.99, &[]),
    ];
    write_pbf(
        path,
        [19.8, 49.9, 20.6, 50.1],
        &nodes,
        &[
            (
                101,
                &[10, 11],
                &[("highway", "primary"), ("name", "Long Road")],
            ),
            (102, &[20, 21, 22, 23, 20], &[("landuse", "forest")]),
            (104, &[30, 31, 32], &[]),
            (105, &[32, 33, 30], &[]),
            (106, &[40, 41, 42, 43, 40], &[("building", "yes")]),
            (
                107,
                &[50, 51],
                &[("highway", "motorway"), ("name", "Far Motorway")],
            ),
        ],
        &[(
            200,
            &[(104, "outer"), (105, "outer")],
            &[
                ("type", "multipolygon"),
                ("natural", "water"),
                ("name", "Big Lake"),
            ],
        )],
    );
}

fn compile(dir: &Path, pbf: &Path, area_only: bool) -> (OpenMapTilesResult, PathBuf) {
    let [west, south, east, north] = AREA;
    let area = CompileArea::from_geojson(&format!(
        r#"{{"type":"Polygon","coordinates":[[[{west},{south}],[{east},{south}],[{east},{north}],[{west},{north}],[{west},{south}]]]}}"#
    ))
    .unwrap();
    let output = dir.join("area.pmtiles");
    let result = compile_openmaptiles(&OpenMapTilesRequest {
        input_pbf: pbf.to_path_buf(),
        sources: PathBuf::new(),
        output: output.clone(),
        temp_dir: dir.join("temp"),
        bounds: CompileBounds::new(50.1, 49.9, 20.6, 19.8),
        area: Some(area),
        center: None,
        archive: Default::default(),
        min_zoom: 0,
        max_zoom: 14,
        threads: 2,
        layers: None,
        languages: None,
        omit_sources: SupplementalDataset::ALL.to_vec(),
        area_only,
    })
    .unwrap();
    (result, output)
}

/// Every tile's uncompressed bytes, keyed by `z/x/y`.
fn read_tiles(path: &Path) -> BTreeMap<(u8, u32, u32), Vec<u8>> {
    let mut reader = archive::ArchiveReader::open(path).unwrap();
    let mut tiles = BTreeMap::new();
    reader
        .visit_entries(|reader, entry| {
            let stored = reader.tile(&entry)?;
            let mut tile = Vec::new();
            flate2::read::GzDecoder::new(&stored[..])
                .read_to_end(&mut tile)
                .unwrap();
            for id in entry.tile_id..entry.tile_id + u64::from(entry.run_length) {
                let coord = pmtiles::TileCoord::from(pmtiles::TileId::new(id).unwrap());
                tiles.insert((coord.z(), coord.x(), coord.y()), tile.clone());
            }
            Ok(())
        })
        .unwrap();
    tiles
}

/// Whether the tile names `name` in `layer`.
fn names(tile: &[u8], layer: &str, name: &str) -> bool {
    mvt::decode_tile(tile).iter().any(|(l, features)| {
        l == layer
            && features
                .iter()
                .any(|f| f.attributes.contains(&format!("name={name:?}")))
    })
}

fn has_layer(tile: &[u8], layer: &str) -> bool {
    mvt::decode_tile(tile).iter().any(|(l, _)| l == layer)
}

#[test]
fn area_only_keeps_the_highest_zoom_tiles_of_a_full_read() {
    let dir = tempfile::tempdir().unwrap();
    let pbf = dir.path().join("extract.osm.pbf");
    write_extract(&pbf);
    let (full_result, full) = compile(&dir.path().join("full"), &pbf, false);
    let (result, area_only) = compile(&dir.path().join("only"), &pbf, true);
    let (full, area_only) = (read_tiles(&full), read_tiles(&area_only));
    assert!(
        result.profile_features < full_result.profile_features,
        "{} features, {} in the full read",
        result.profile_features,
        full_result.profile_features
    );

    let zoom = |tiles: &BTreeMap<(u8, u32, u32), Vec<u8>>, z: u8| {
        tiles
            .iter()
            .filter(|((tile_z, _, _), _)| *tile_z == z)
            .map(|(tile, bytes)| (*tile, bytes.clone()))
            .collect::<Vec<_>>()
    };
    let top = zoom(&area_only, 14);
    assert!(!top.is_empty());
    assert_eq!(top, zoom(&full, 14));
    for (_, tile) in &top {
        assert!(names(tile, "transportation_name", "Long Road"), "the road");
        assert!(has_layer(tile, "landcover"), "the forest");
        assert!(has_layer(tile, "water"), "the lake");
    }
    assert!(
        top.iter()
            .any(|(_, tile)| names(tile, "poi", "Inside Cafe"))
    );

    // Zoomed out, the full read shows what lies half a degree away, and the
    // area-only compile does not.
    let far = |tiles: &BTreeMap<(u8, u32, u32), Vec<u8>>| {
        tiles.values().any(|tile| {
            names(tile, "place", "Far Town")
                || names(tile, "transportation_name", "Far Motorway")
                || names(tile, "transportation", "Far Motorway")
        })
    };
    assert!(far(&full));
    assert!(!far(&area_only));
    // Where only the far motorway reached the area's tiles (zooms 4 and
    // 5), the area-only compile writes no tile.
    assert!(area_only.keys().all(|tile| full.contains_key(tile)));
    assert!(area_only.len() < full.len());
}
