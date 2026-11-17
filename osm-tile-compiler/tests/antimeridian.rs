//! End-to-end test: compiles a small synthetic PBF on both sides of the
//! antimeridian, around Fiji, whose header box wraps (177 E to 178 W), and
//! checks that the features on each side land in their tiles, and those near
//! 180 in the tiles on both sides.

use osm_tile_compiler::{
    CompileArea, CompileBounds, archive,
    omt::{OpenMapTilesRequest, compile_openmaptiles},
};
#[allow(dead_code)]
mod common;
#[path = "common/mvt.rs"]
mod mvt;

use common::synthetic_sources;
use osmpbfreader::fileformat::{Blob, BlobHeader};
use osmpbfreader::osmformat::{
    DenseNodes, HeaderBBox, HeaderBlock, PrimitiveBlock, PrimitiveGroup, StringTable, Way,
};
use protobuf::Message;
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

/// A PBF frame: big-endian length, `BlobHeader`, uncompressed blob.
fn frame(blob_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut blob = Blob::new();
    blob.set_raw(payload.to_vec());
    blob.set_raw_size(payload.len() as i32);
    let blob = blob.write_to_bytes().unwrap();
    let mut header = BlobHeader::new();
    header.set_type(blob_type.into());
    header.set_datasize(blob.len() as i32);
    let header = header.write_to_bytes().unwrap();
    let mut out = (header.len() as u32).to_be_bytes().to_vec();
    out.extend(header);
    out.extend(blob);
    out
}

/// Interns strings for a block's string table; index 0 is the empty string.
#[derive(Default)]
struct Strings(Vec<String>);

impl Strings {
    fn index(&mut self, s: &str) -> u32 {
        if self.0.is_empty() {
            self.0.push(String::new());
        }
        let at = self.0.iter().position(|t| t == s).unwrap_or_else(|| {
            self.0.push(s.into());
            self.0.len() - 1
        });
        at as u32
    }

    fn block(mut self, group: PrimitiveGroup) -> Vec<u8> {
        self.index("");
        let mut table = StringTable::new();
        table.s = self.0.into_iter().map(String::into_bytes).collect();
        let mut block = PrimitiveBlock::new();
        block.stringtable = Some(table).into();
        block.primitivegroup.push(group);
        block.write_to_bytes().unwrap()
    }
}

type Tags<'a> = &'a [(&'a str, &'a str)];

/// Writes a sorted PBF with dense nodes `(id, lon, lat, tags)` and ways
/// `(id, nodes, tags)`, its header box from `west` to `east`.
fn write_pbf(
    path: &Path,
    [west, south, east, north]: [f64; 4],
    nodes: &[(i64, f64, f64, Tags)],
    ways: &[(i64, &[i64], Tags)],
) {
    let nano = |degrees: f64| (degrees * 1e9).round() as i64;
    let mut bbox = HeaderBBox::new();
    bbox.set_left(nano(west));
    bbox.set_right(nano(east));
    bbox.set_top(nano(north));
    bbox.set_bottom(nano(south));
    let mut header = HeaderBlock::new();
    header.bbox = Some(bbox).into();
    header.required_features = vec!["OsmSchema-V0.6".into(), "DenseNodes".into()];
    header.optional_features = vec!["Sort.Type_then_ID".into()];
    let mut bytes = frame("OSMHeader", &header.write_to_bytes().unwrap());

    let mut strings = Strings::default();
    let mut dense = DenseNodes::new();
    let (mut id, mut lat, mut lon) = (0, 0, 0);
    for &(node, x, y, tags) in nodes {
        // Units of 100 nanodegrees, the default granularity.
        let (x, y) = ((x * 1e7).round() as i64, (y * 1e7).round() as i64);
        dense.id.push(node - id);
        dense.lon.push(x - lon);
        dense.lat.push(y - lat);
        (id, lon, lat) = (node, x, y);
        for (key, value) in tags {
            dense.keys_vals.push(strings.index(key) as i32);
            dense.keys_vals.push(strings.index(value) as i32);
        }
        dense.keys_vals.push(0);
    }
    let mut group = PrimitiveGroup::new();
    group.dense = Some(dense).into();
    bytes.extend(frame("OSMData", &strings.block(group)));

    let mut strings = Strings::default();
    let mut group = PrimitiveGroup::new();
    for &(way, refs, tags) in ways {
        let mut encoded = Way::new();
        encoded.set_id(way);
        for (key, value) in tags {
            encoded.keys.push(strings.index(key));
            encoded.vals.push(strings.index(value));
        }
        let mut last = 0;
        for &node in refs {
            encoded.refs.push(node - last);
            last = node;
        }
        group.ways.push(encoded);
    }
    bytes.extend(frame("OSMData", &strings.block(group)));
    std::fs::write(path, bytes).unwrap();
}

/// Two islands either side of 180: on each a town, a road and a building,
/// and a cafe 0.0001 degrees west of 180.
fn write_fiji(path: &Path) {
    let mut nodes: Vec<(i64, f64, f64, Tags)> = vec![
        (1, 179.5, -17.0, &[("place", "town"), ("name", "West Town")]),
        (
            2,
            -179.5,
            -17.0,
            &[("place", "town"), ("name", "East Town")],
        ),
        (
            3,
            179.9999,
            -16.8,
            &[("amenity", "cafe"), ("name", "Edge Cafe")],
        ),
    ];
    // Roads: 10-11 in the west, 12-13 in the east.
    for (id, lon) in [(10, 179.6), (11, 179.8), (12, -179.8), (13, -179.6)] {
        nodes.push((id, lon, -16.9, &[]));
    }
    // A square building around each (lon, lat) corner list: 20-23, 30-33.
    let square = |lon: f64| {
        [
            (lon, -16.95),
            (lon + 0.001, -16.95),
            (lon + 0.001, -16.949),
            (lon, -16.949),
        ]
    };
    for (first, lon) in [(20, 179.7), (30, -179.7)] {
        for (i, (x, y)) in square(lon).into_iter().enumerate() {
            nodes.push((first + i as i64, x, y, &[]));
        }
    }
    nodes.sort_by_key(|n| n.0);
    let road: Tags = &[("highway", "primary"), ("name", "Coast Road")];
    let building: Tags = &[("building", "yes")];
    write_pbf(
        path,
        [177.0, -19.0, -178.0, -15.0],
        &nodes,
        &[
            (100, &[10, 11], road),
            (101, &[12, 13], road),
            (102, &[20, 21, 22, 23, 20], building),
            (103, &[30, 31, 32, 33, 30], building),
        ],
    );
}

/// The layers of every tile in the archive, keyed by `z/x/y`, with each
/// feature's `name` (quoted, or empty) and geometry.
type Tiles = BTreeMap<(u8, u32, u32), BTreeMap<String, Vec<(String, String)>>>;

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
                        let names = features
                            .iter()
                            .map(|f| {
                                let name = f
                                    .attributes
                                    .split(" name=")
                                    .nth(1)
                                    .map_or("", |rest| rest.split(" name").next().unwrap());
                                (name.to_string(), f.geometry.clone())
                            })
                            .collect();
                        (layer, names)
                    })
                    .collect();
                tiles.insert((coord.z(), coord.x(), coord.y()), layers);
            }
            Ok(())
        })
        .unwrap();
    tiles
}

/// The tile at zoom `z` holding the point.
fn tile_of(z: u8, longitude: f64, latitude: f64) -> (u8, u32, u32) {
    let n = f64::from(1_u32 << z);
    let lat = latitude.to_radians();
    let x = (longitude + 180.0) / 360.0 * n;
    let y = (1.0 - lat.tan().asinh() / std::f64::consts::PI) / 2.0 * n;
    (z, x as u32, y as u32)
}

/// The geometry of the feature named `name` in the tile's `layer`.
fn find<'a>(tiles: &'a Tiles, tile: (u8, u32, u32), layer: &str, name: &str) -> Option<&'a str> {
    tiles
        .get(&tile)?
        .get(layer)?
        .iter()
        .find(|(n, _)| n == &format!("{name:?}"))
        .map(|(_, geometry)| geometry.as_str())
}

fn has(tiles: &Tiles, tile: (u8, u32, u32), layer: &str, name: &str) -> bool {
    find(tiles, tile, layer, name).is_some()
}

fn compile(
    dir: &Path,
    sources: &Path,
    pbf: &Path,
    area: Option<CompileArea>,
) -> std::path::PathBuf {
    // As the CLI and the desktop worker take the bounds from the header.
    let b = osm_pbf_ingest::blob_index::PbfBlobIndex::scan(pbf)
        .unwrap()
        .header()
        .bounds
        .unwrap();
    let bounds = CompileBounds::new(b.north, b.south, b.east, b.west).normalized();
    assert!(bounds.wraps(), "{bounds:?}");
    let output = dir.join("fiji.pmtiles");
    compile_openmaptiles(&OpenMapTilesRequest {
        input_pbf: pbf.to_path_buf(),
        sources: sources.to_path_buf(),
        output: output.clone(),
        temp_dir: dir.join("temp"),
        bounds,
        area,
        center: None,
        archive: Default::default(),
        min_zoom: 0,
        max_zoom: 14,
        threads: 2,
        layers: None,
        languages: None,
        omit_sources: Vec::new(),
    })
    .unwrap();
    output
}

#[test]
fn compiles_an_area_crossing_the_antimeridian() {
    let dir = tempfile::tempdir().unwrap();
    let sources = dir.path().join("sources");
    synthetic_sources(&sources);
    let pbf = dir.path().join("fiji.osm.pbf");
    write_fiji(&pbf);
    let output = compile(&dir.path().join("box"), &sources, &pbf, None);

    let report = archive::inspect(&output).unwrap();
    assert!(report.problems.is_empty(), "{:?}", report.problems);
    // Declared as every longitude, which viewers read; centered on the
    // middle of the wrapped box, 179.5 E, at the zoom that shows all of it
    // (a z6 tile is 5.6 degrees wide).
    assert_eq!(report.bounds, [-180.0, -19.0, 180.0, -15.0]);
    assert_eq!(report.center, (179.5, -17.0, 6));

    let tiles = read_tiles(&output);
    for z in [8, 10, 12, 14] {
        for (lon, town) in [(179.5, "West Town"), (-179.5, "East Town")] {
            let tile = tile_of(z, lon, -17.0);
            assert!(has(&tiles, tile, "place", town), "{town} in {tile:?}");
        }
        for lon in [179.7, -179.7] {
            let tile = tile_of(z, lon, -16.9);
            assert!(
                tiles[&tile].contains_key("transportation"),
                "road in {tile:?}"
            );
        }
    }
    for lon in [179.7, -179.7] {
        let tile = tile_of(14, lon, -16.95);
        assert!(
            tiles[&tile].contains_key("building"),
            "building in {tile:?}"
        );
    }
    // The cafe 0.0001 degrees west of 180 is within the z14 tiles' buffer
    // on the other side too.
    let (z, x, y) = tile_of(14, 179.9999, -16.8);
    assert_eq!(x, (1 << 14) - 1);
    // Inside the tile west of 180, just beyond the left edge of the one east
    // of it (0.0001 degrees is 19 of a z14 tile's 4096 units).
    let point_x = |geometry: &str| geometry.split(' ').nth(1).unwrap().parse::<i64>().unwrap();
    let west = find(&tiles, (z, x, y), "poi", "Edge Cafe").unwrap();
    assert!((4070..4096).contains(&point_x(west)), "{west}");
    let east = find(&tiles, (z, 0, y), "poi", "Edge Cafe").unwrap();
    assert!((-26..0).contains(&point_x(east)), "{east}");
    // Every tile lies within the box, on one side or the other. At z8 the
    // box spans columns 253 to 255 and 0 to 1, and the features, all within
    // half a degree of 180, columns 255 and 0; the places' buffer of a whole
    // tile also puts each town in the next column, across 180 too.
    let columns = tiles
        .keys()
        .filter(|&&(z, _, _)| z == 8)
        .map(|&(_, x, _)| x)
        .collect::<BTreeSet<_>>();
    assert_eq!(columns, BTreeSet::from([0, 1, 254, 255]));
    let (_, _, row) = tile_of(8, 179.5, -17.0);
    assert!(has(&tiles, (8, 0, row), "place", "West Town"));
    assert!(has(&tiles, (8, 255, row), "place", "East Town"));
    for &(z, x, _) in tiles.keys() {
        let (west, east) = (tile_of(z, 177.0, 0.0).1, tile_of(z, -178.0, 0.0).1);
        assert!(x >= west || x <= east, "{z}/{x} is outside the box");
    }

    // Limited further to the west island by a polygon split at 180 (RFC
    // 7946), of which only the western part reaches land: tiles on both
    // sides are written, as the polygon covers them.
    let polygon = r#"{"type":"MultiPolygon","coordinates":[
        [[[179.4,-17.2],[180,-17.2],[180,-16.7],[179.4,-16.7],[179.4,-17.2]]],
        [[[-180,-17.2],[-179.9,-17.2],[-179.9,-16.7],[-180,-16.7],[-180,-17.2]]]]}"#;
    let area = CompileArea::from_geojson(polygon).unwrap();
    assert_eq!(
        area.bounds(),
        CompileBounds::new(-16.7, -17.2, -179.9, 179.4)
    );
    let limited = read_tiles(&compile(
        &dir.path().join("polygon"),
        &sources,
        &pbf,
        Some(area),
    ));
    assert!(limited.keys().all(|tile| tiles.contains_key(tile)));
    assert!(limited.len() < tiles.len());
    let west_town = tile_of(14, 179.5, -17.0);
    assert_eq!(limited[&west_town], tiles[&west_town]);
    assert!(
        limited.contains_key(&(14, 0, y)),
        "the cafe's copy east of 180"
    );
    assert!(!limited.contains_key(&tile_of(14, -179.5, -17.0)));
}
