//! The building levels pass, on small archives built here: what each level
//! holds, the tile format the renderer reads, and the same bytes at any number
//! of threads.

use fast_mvt::{MvtCoord, MvtGeomType, MvtLayerBuilder};
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use osm_tile_compiler::{
    BuildingLevelReport, BuildingLevelsReport, building_levels_path, write_building_levels,
    write_building_levels_beside,
};
use pmtiles::{AsyncPmTilesReader, Compression, PmTilesWriter, TileCoord, TileType};
use std::f64::consts::PI;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};
use tempfile::TempDir;

/// The corner of a z11 tile, at about 50 degrees north.
const BASE: (u32, u32) = (9096, 5552);
const WITH_BUILDINGS: &str = r#"{"vector_layers":[{"id":"building"}]}"#;
const RECORD_BYTES: usize = 18;

type Point = (i32, i32);

#[derive(Clone)]
struct Building {
    rings: Vec<Vec<Point>>,
    height: i64,
    colour: Option<&'static str>,
    hidden: bool,
}

type Tile = ((u32, u32), Vec<Building>);

fn building(ring: &[Point]) -> Building {
    Building {
        rings: vec![ring.to_vec()],
        height: 10,
        colour: None,
        hidden: false,
    }
}

fn rectangle(x0: i32, y0: i32, x1: i32, y1: i32) -> Building {
    building(&[(x0, y0), (x1, y0), (x1, y1), (x0, y1)])
}

fn at(dx: u32, dy: u32) -> (u32, u32) {
    (BASE.0 + dx, BASE.1 + dy)
}

/// A gzipped tile with the buildings in its `building` layer.
fn vector_tile(buildings: &[Building]) -> Vec<u8> {
    let mut layer = MvtLayerBuilder::new("building").unwrap();
    for building in buildings {
        let mut feature = layer.feature_of(MvtGeomType::Polygon);
        feature.tag_sint("render_height", building.height).unwrap();
        feature.tag_sint("render_min_height", 0).unwrap();
        if let Some(colour) = building.colour {
            feature.tag_string("colour", colour).unwrap();
        }
        if building.hidden {
            feature.tag_bool("hide_3d", true).unwrap();
        }
        for (index, ring) in building.rings.iter().enumerate() {
            let coords = ring.iter().map(|&(x, y)| MvtCoord { x, y });
            feature.ring(coords, index == 0).unwrap();
        }
        layer = feature.end();
    }
    let mut gzip = GzEncoder::new(Vec::new(), flate2::Compression::default());
    std::io::Write::write_all(&mut gzip, &layer.encode()).unwrap();
    gzip.finish().unwrap()
}

fn write_basemap(path: &Path, tiles: &[Tile], metadata: &str) {
    let mut writer = PmTilesWriter::new(TileType::Mvt)
        .precompressed(Compression::Gzip)
        .min_zoom(14)
        .max_zoom(14)
        .bounds(19.8, 49.9, 20.1, 50.2)
        .center(19.9, 50.0)
        .metadata(metadata)
        .create(File::create(path).unwrap())
        .unwrap();
    for ((x, y), buildings) in tiles {
        let coord = TileCoord::new(14, *x, *y).unwrap();
        writer.add_tile(coord, &vector_tile(buildings)).unwrap();
    }
    writer.finalize().unwrap();
}

struct Levels {
    _dir: TempDir,
    path: PathBuf,
    report: BuildingLevelsReport,
}

impl Levels {
    fn of(tiles: &[Tile]) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let input = dir.path().join("basemap.pmtiles");
        write_basemap(&input, tiles, WITH_BUILDINGS);
        let path = dir.path().join("levels.pmtiles");
        let report = write_building_levels(&input, &path, 2).unwrap();
        Self {
            _dir: dir,
            path,
            report,
        }
    }

    fn level(&self, zoom: u8) -> &BuildingLevelReport {
        let levels = &self.report.levels;
        levels.iter().find(|level| level.zoom == zoom).unwrap()
    }

    /// The boxes of a tile, or None if it was not written.
    async fn boxes(&self, z: u8, x: u32, y: u32) -> Option<Vec<Record>> {
        let reader = AsyncPmTilesReader::new_with_path(&self.path).await.unwrap();
        let coord = TileCoord::new(z, x, y).unwrap();
        let stored = reader.get_tile(coord).await.unwrap()?;
        let mut raw = Vec::new();
        GzDecoder::new(&stored[..]).read_to_end(&mut raw).unwrap();
        Some(records(&raw))
    }
}

/// A box as the renderer reads it: tile units, radians, metres.
#[derive(Debug, Clone, Copy)]
struct Record {
    x: f64,
    y: f64,
    half_u: f64,
    half_v: f64,
    angle: f64,
    raw_angle: u16,
    base: f64,
    top: f64,
    rgb: [u8; 3],
}

impl Record {
    fn corners(&self) -> [[f64; 2]; 4] {
        let (sin, cos) = self.angle.sin_cos();
        [(-1.0, -1.0), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)].map(|(u, v): (f64, f64)| {
            let (u, v) = (u * self.half_u, v * self.half_v);
            [self.x + u * cos - v * sin, self.y + u * sin + v * cos]
        })
    }

    fn area(&self) -> f64 {
        4.0 * self.half_u * self.half_v
    }
}

fn records(raw: &[u8]) -> Vec<Record> {
    let count = u32::from_le_bytes(raw[..4].try_into().unwrap()) as usize;
    assert!(count > 0, "a tile with no boxes is not written");
    assert_eq!(raw.len(), 4 + RECORD_BYTES * count);
    let wide = |column: usize, index: usize| {
        let start = 4 + 2 * column * count + 2 * index;
        u16::from_le_bytes([raw[start], raw[start + 1]])
    };
    let narrow = |column: usize, index: usize| raw[4 + 14 * count + column * count + index];
    let position = |value: u16| f64::from(value) * 8192.0 / 65535.0 - 2048.0;
    (0..count)
        .map(|index| {
            assert_eq!(narrow(3, index), 0, "the reserved byte is zero");
            Record {
                x: position(wide(0, index)),
                y: position(wide(1, index)),
                half_u: f64::from(wide(2, index)) / 8.0,
                half_v: f64::from(wide(3, index)) / 8.0,
                angle: f64::from(wide(4, index)) / 10_000.0 - PI,
                raw_angle: wide(4, index),
                base: f64::from(wide(5, index)) / 10.0,
                top: f64::from(wide(6, index)) / 10.0,
                rgb: [narrow(0, index), narrow(1, index), narrow(2, index)],
            }
        })
        .collect()
}

/// Houses across four tiles, and one tile of a dense block.
fn town() -> Vec<Tile> {
    let mut tiles = Vec::new();
    for (dx, dy) in [(1, 1), (2, 1), (1, 2), (2, 2)] {
        let mut houses = Vec::new();
        for index in 0..144 {
            let (row, column) = (index / 12, index % 12);
            let x = 100 + column * 300 + (index * 37) % 41;
            let y = 100 + row * 300 + (index * 53) % 43;
            houses.push(rectangle(x, y, x + 40, y + 30));
        }
        tiles.push((at(dx, dy), houses));
    }
    let mut block = Vec::new();
    for row in 0..16 {
        for column in 0..16 {
            let (x, y) = (40 + column * 250, 40 + row * 250);
            block.push(rectangle(x, y, x + 200, y + 200));
        }
    }
    tiles.push((at(5, 5), block));
    tiles
}

#[test]
fn the_bytes_do_not_depend_on_the_thread_count() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("basemap.pmtiles");
    write_basemap(&input, &town(), WITH_BUILDINGS);
    let mut outputs = Vec::new();
    for threads in [1, 8] {
        let output = dir.path().join(format!("levels-{threads}.pmtiles"));
        write_building_levels(&input, &output, threads).unwrap();
        outputs.push(std::fs::read(output).unwrap());
    }
    assert!(outputs[0] == outputs[1], "1 and 8 threads differ");
}

#[tokio::test]
async fn a_rectangle_makes_one_box_with_its_corners() {
    let corners = [(1000, 1000), (1300, 1100), (1250, 1250), (950, 1150)];
    let levels = Levels::of(&[(at(0, 0), vec![building(&corners)])]);
    let boxes = levels.boxes(14, BASE.0, BASE.1).await.unwrap();
    assert_eq!(boxes.len(), 1, "{boxes:?}");
    let drawn = boxes[0].corners();
    for (x, y) in corners {
        let nearest = drawn
            .iter()
            .map(|c| (c[0] - f64::from(x)).hypot(c[1] - f64::from(y)))
            .fold(f64::MAX, f64::min);
        assert!(nearest < 0.5, "({x}, {y}) is {nearest} from {drawn:?}");
    }
}

#[tokio::test]
async fn an_l_makes_several_boxes_inside_it() {
    let l = [
        (1000, 1000),
        (1400, 1000),
        (1400, 1100),
        (1100, 1100),
        (1100, 1400),
        (1000, 1400),
    ];
    let levels = Levels::of(&[(at(0, 0), vec![building(&l)])]);
    let boxes = levels.boxes(14, BASE.0, BASE.1).await.unwrap();
    assert!(boxes.len() >= 2, "{boxes:?}");
    // The grid's cell: 1/48 of the long side, which is more than 3 m here.
    let cell = 400.0 / 48.0;
    let within = |p: [f64; 2], x1: f64, y1: f64| {
        let low = 1000.0 - cell;
        p[0] >= low && p[1] >= low && p[0] <= x1 + cell && p[1] <= y1 + cell
    };
    for boxed in &boxes {
        for corner in boxed.corners() {
            let inside = within(corner, 1400.0, 1100.0) || within(corner, 1100.0, 1400.0);
            assert!(inside, "{corner:?} of {boxed:?}");
        }
    }
    let area: f64 = boxes.iter().map(Record::area).sum();
    assert!((area / 70_000.0 - 1.0).abs() < 0.1, "{area}");
}

#[tokio::test]
async fn a_tile_edge_makes_two_boxes_that_meet() {
    // Tiles carry the building out to their buffer of 64 units.
    let levels = Levels::of(&[
        (at(0, 0), vec![rectangle(3900, 1000, 4160, 1200)]),
        (at(1, 0), vec![rectangle(-64, 1000, 204, 1200)]),
    ]);
    let left = levels.boxes(14, BASE.0, BASE.1).await.unwrap();
    let right = levels.boxes(14, BASE.0 + 1, BASE.1).await.unwrap();
    assert_eq!((left.len(), right.len()), (1, 1), "{left:?} {right:?}");
    let xs = |boxed: &Record| boxed.corners().map(|corner| corner[0]);
    let left_edge = xs(&left[0]).into_iter().fold(f64::MIN, f64::max);
    let right_edge = xs(&right[0]).into_iter().fold(f64::MAX, f64::min);
    assert!((left_edge - 4096.0).abs() < 0.5, "{left_edge}");
    assert!(right_edge.abs() < 0.5, "{right_edge}");
    assert!(left[0].area() < 196.5 * 200.5, "{:?}", left[0]);
    assert!(right[0].area() < 204.5 * 200.5, "{:?}", right[0]);
}

#[tokio::test]
async fn hide_3d_features_are_skipped() {
    let mut outline = rectangle(2000, 2000, 2400, 2400);
    outline.hidden = true;
    let mixed = vec![rectangle(1000, 1000, 1200, 1200), outline.clone()];
    let levels = Levels::of(&[(at(0, 0), mixed), (at(1, 0), vec![outline])]);
    let boxes = levels.boxes(14, BASE.0, BASE.1).await.unwrap();
    assert_eq!(boxes.len(), 1, "{boxes:?}");
    assert!(boxes[0].x < 1300.0, "{boxes:?}");
    assert!(levels.boxes(14, BASE.0 + 1, BASE.1).await.is_none());
    assert_eq!(levels.level(14).boxes, 1);
}

#[test]
fn far_levels_keep_the_area_of_a_village_and_a_block() {
    let levels = Levels::of(&town());
    let z12 = levels.level(12).area_kept;
    assert!((0.99..=1.0001).contains(&z12), "{z12}");
    // The cover gain adds area wherever cover is under 1.
    let z11 = levels.level(11).area_kept;
    assert!((0.99..=1.16).contains(&z11), "{z11}");
}

#[tokio::test]
async fn z11_cover_gain_stops_at_full_cover() {
    let levels = Levels::of(&[(at(2, 2), vec![rectangle(0, 0, 4096, 4096)])]);
    let z11 = levels.boxes(11, BASE.0 >> 3, BASE.1 >> 3).await.unwrap();
    assert_eq!(z11.len(), 1, "{z11:?}");
    // 32 cells of 16 units each way, and no wider for the gain.
    assert!((z11[0].half_u - 256.0).abs() < 0.5, "{z11:?}");
    assert!((z11[0].half_v - 256.0).abs() < 0.5, "{z11:?}");
}

#[tokio::test]
async fn a_tall_building_stands_on_in_the_far_levels() {
    let mut tower = rectangle(1284, 1284, 1314, 1314);
    tower.height = 200;
    let buildings = vec![tower, rectangle(1316, 1284, 1340, 1340)];
    let levels = Levels::of(&[(at(1, 1), buildings)]);
    let z12 = levels.boxes(12, BASE.0 >> 2, BASE.1 >> 2).await.unwrap();
    let z11 = levels.boxes(11, BASE.0 >> 3, BASE.1 >> 3).await.unwrap();
    for (boxes, scale) in [(&z12, 4.0), (&z11, 8.0)] {
        let tower = boxes
            .iter()
            .find(|boxed| boxed.top == 200.0)
            .unwrap_or_else(|| panic!("no tower in {boxes:?}"));
        assert!((tower.half_u - 15.0 / scale).abs() < 0.2, "{tower:?}");
        assert!(
            (tower.x - (4096.0 + 1299.0) / scale).abs() < 0.5,
            "{tower:?}"
        );
        assert!(
            (tower.y - (4096.0 + 1299.0) / scale).abs() < 0.5,
            "{tower:?}"
        );
    }
}

#[tokio::test]
async fn the_archive_is_as_the_renderer_expects() {
    let mut painted = rectangle(1000, 1000, 1400, 1400);
    painted.colour = Some("#336699");
    painted.height = 20;
    let buildings = vec![painted, rectangle(2000, 2000, 2400, 2400)];
    let levels = Levels::of(&[(at(0, 0), buildings)]);

    let reader = AsyncPmTilesReader::new_with_path(&levels.path)
        .await
        .unwrap();
    let header = reader.get_header();
    assert_eq!(header.tile_type, TileType::Unknown);
    assert_eq!(header.tile_compression, Compression::Gzip);
    assert_eq!((header.min_zoom, header.max_zoom), (11, 14));
    assert_eq!(
        reader.get_metadata().await.unwrap(),
        r#"{"format":"building-levels","version":1,"source_layer":"building"}"#
    );

    let z14 = levels.boxes(14, BASE.0, BASE.1).await.unwrap();
    assert_eq!(z14.len(), 2, "{z14:?}");
    let (first, second) = (&z14[0], &z14[1]);
    assert_eq!(first.rgb, [0x33, 0x66, 0x99]);
    assert_eq!(second.rgb, [200, 200, 200]);
    assert_eq!((first.base, first.top), (0.0, 20.0));

    for (z, shift) in [(12, 2), (11, 3)] {
        let far = levels.boxes(z, BASE.0 >> shift, BASE.1 >> shift).await;
        let far = far.unwrap_or_else(|| panic!("no tile at z{z}"));
        assert!(far.iter().all(|boxed| boxed.raw_angle == 31416), "{far:?}");
        assert!(far.iter().all(|boxed| boxed.base == 0.0), "{far:?}");
    }
    assert!(levels.boxes(13, BASE.0 >> 1, BASE.1 >> 1).await.is_none());

    for level in &levels.report.levels {
        assert_eq!(level.tiles, 1, "{level:?}");
        let raw = 4 + RECORD_BYTES as u64 * level.boxes;
        assert_eq!(level.raw_bytes, raw, "{level:?}");
        assert!(level.gz_bytes > 0, "{level:?}");
    }
    let zooms = levels.report.levels.iter().map(|level| level.zoom);
    assert_eq!(zooms.collect::<Vec<_>>(), [11, 12, 14]);
}

/// Too thin for a box of its own at z11, with nothing near it to join.
#[tokio::test]
async fn a_lone_house_stays_in_the_far_levels() {
    let levels = Levels::of(&[(at(0, 0), vec![rectangle(1000, 1000, 1020, 1020)])]);
    let z11 = levels.boxes(11, BASE.0 >> 3, BASE.1 >> 3).await;
    assert_eq!(z11.map(|boxes| boxes.len()), Some(1));
    let kept = levels.level(11).area_kept;
    assert!((0.99..1.16).contains(&kept), "{kept}");
}

#[test]
fn levels_go_beside_an_archive_with_buildings_only() {
    let tiles = [(at(0, 0), vec![rectangle(1000, 1000, 1200, 1200)])];

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("region.pmtiles");
    write_basemap(&input, &tiles, r#"{"vector_layers":[{"id":"water"}]}"#);
    assert!(write_building_levels_beside(&input, 2).unwrap().is_none());
    assert!(!building_levels_path(&input).exists());

    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("region.pmtiles");
    write_basemap(&input, &tiles, WITH_BUILDINGS);
    assert!(write_building_levels_beside(&input, 2).unwrap().is_some());
    assert!(building_levels_path(&input).is_file());
}

#[test]
fn an_existing_archive_is_not_replaced() {
    let dir = tempfile::tempdir().unwrap();
    let input = dir.path().join("basemap.pmtiles");
    write_basemap(&input, &town(), WITH_BUILDINGS);
    let output = dir.path().join("levels.pmtiles");
    std::fs::write(&output, b"mine").unwrap();
    let error = write_building_levels(&input, &output, 2).unwrap_err();
    assert!(error.contains("refusing to replace"), "{error}");
    assert_eq!(std::fs::read(&output).unwrap(), b"mine");
}
