//! HD roads: a PMTiles archive of vector tiles beside a basemap, holding roads
//! at their true width, read from an OSM extract. It is a pass of its own, so
//! it changes nothing in the basemap.
//!
//! What is drawn on the ground is in z16 tiles (a unit is about 10 cm at 50
//! degrees), which renderers overzoom. Two layers:
//!
//! - `surface`, polygons: the carriageways, `kind` `road`, `sidewalk` or
//!   `cycleway`, as the union of the roads of one `level` (`ground`, `bridge`
//!   or `tunnel`, with its `layer`), corners rounded where roads meet, and
//!   `paved`. Beside them `parking`, and in them `island`, what is kerbed off.
//! - `marking`, lines: the painted lines, `kind` `edge`, `lane` or `centre`,
//!   `dashed`, `level`, `layer`, and `confidence` (0 the class's usual lanes,
//!   1 tagged lanes, 2 tagged lanes and width). They stop short of junctions
//!   and crossings. Then, to be drawn at the width given here, and dashed from
//!   the line's start with bars of 0.5 m and gaps of 0.5 m where it says so:
//!   `zebra` (4 m, dashed: the bars; the line runs across the road), `dots`
//!   (0.5 m, dashed: the two rows cyclists cross between), `crossing` (0.3 m,
//!   the two lines of one), `stop` (0.5 m), `signal` (0.4 m: where traffic
//!   stops at signals), `give_way` (0.5 m, dashed) and `arrow` (0.25 m), whose
//!   heads are `arrow-head`, polygons. Their `confidence` is 2 where the thing
//!   itself is tagged, 1 where another tag implies it, 0 where it is usual.
//!   `bay` is the outline of a parking space.
//!
//! One layer in z15 tiles of 8192 units (the same units):
//!
//! - `road`, lines: the motor roads' ways but for service roads, not to be
//!   drawn but asked of: `way`, `class`, `level`, `layer`, and `lanes`, metres and counts
//!   `<left>,<lane>,<against>,<along>,<traffic>`: the left edge of the leftmost
//!   lane right of the line, a lane's width, the lanes against the way (the
//!   left ones) and along it, and which way traffic goes (`+`, `-` or `=`).
//!   They reach as far out of their tile as their surface does.
//!
//! Four layers more, in z14 tiles of 65536 units:
//!
//! - `structure`: what a bridge's heights are worked out from (they need
//!   terrain, so they are not here). Each group of bridges that join or cross
//!   is whole in one tile, the first's, and may reach out of it. Lines are the
//!   bridges' pieces: `group`, `piece` (its number in the group), `way`,
//!   `layer`, `class`, `width` and `centre` (metres, of the surface across the
//!   line), `paved`, `lines` (`<metres>:<kind>[-]`, painted lines, `-` dashed),
//!   `node0` and `node1` (pieces join where these are equal), `anchor0` and
//!   `anchor1` (whether it meets the ground there), and `approach` (a road on
//!   the ground leading up to a bridge, to be raised to it if need be). Points are where a piece
//!   passes over something: `group`, `over` and `at` (metres along), `kind`
//!   (`road`, `path`, `rail`, `water` or `bridge`), `clearance` and `half`
//!   (metres), and of a bridge of the group `under` and `under_at`.
//! - `paint`, polygons: what else is painted on those pieces, `kind` as a
//!   marking's, on the `piece` of the `group`, convex rings.
//! - `barrier`, lines: `kind` `guard_rail`, `jersey`, `cable` or `noise`,
//!   `height` (metres), and where it stands on one, the `group` and `piece`.
//!   What runs along a bridge's edge is left to its parapet.
//! - `portal`, lines: where a tunnel comes out, `way`, `width`, `centre`
//!   and `inward` (whether the line, in the way's direction, goes into it).
//!
//! Traffic keeps right. Tiles are worked out one by one from the roads that
//! reach them, in order, so the bytes do not depend on the number of threads.

mod details;
mod furniture;
mod geometry;
mod network;
mod profile;
mod structures;

use crate::control::{CompileError, Control};
use crate::omt::PartialArchive;
use details::{Detail, Details, Marking, Sign};
use fast_mvt::{MvtCoord, MvtGeomType, MvtLayerBuilder};
use flate2::write::GzEncoder;
use furniture::{Area, AreaKind, Barrier, Stretch};
use geometry::Rect;
use network::{Network, Point, Way};
use osm_pbf_ingest::{
    IngestBuildProfile, NodeBatch, Pass2Options, Pass2Sink, PrepareOptions, RelationBatch,
    WayBatch, prepare_ingest_index_with_options, run_pass2_with_options,
};
use pmtiles::{Compression, PmTilesWriter, TileCoord, TileId, TileType};
use profile::{Class, Level, Profile, profile};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;
use structures::{Group, Index, Kind, Obstacle, Portal};

/// The `format` of an HD roads archive's metadata.
pub const HD_ROADS_FORMAT: &str = "hd-roads";
/// The `version` of an HD roads archive's metadata.
pub const HD_ROADS_VERSION: u32 = 1;

const ZOOM: u8 = 16;
const EXTENT: f64 = 4096.0;
/// World units across the world at [`ZOOM`].
const WORLD: f64 = EXTENT * (1_u64 << ZOOM) as f64;
const EQUATOR_M: f64 = 40_075_016.686;
/// Units a tile's geometry reaches past its edge.
const BUFFER: f64 = 64.0;
/// Metres a surface runs on past its way's ends, so that the next one's meets
/// it with no crack.
const OVERLAP: f64 = 0.25;
/// Markings shorter than this many metres are left out.
const MIN_MARKING: f64 = 2.0;
/// Surfaces under this many square units (about 0.15 m² at 50 degrees).
const MIN_AREA: u64 = 16;
const GZIP_LEVEL: u32 = 6;

/// What [`write_hd_roads`] wrote.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct HdRoadsReport {
    /// Ways drawn.
    pub ways: u64,
    /// Ways left out because a node of theirs is not in the extract.
    pub incomplete_ways: u64,
    pub junctions: u64,
    /// Distinct cross-sections.
    pub profiles: u64,
    pub tiles: u64,
    pub surfaces: u64,
    pub markings: u64,
    /// Bridges that join or cross, as one; their pieces, and what they cross.
    pub groups: u64,
    pub pieces: u64,
    pub crossings: u64,
    /// Crossings, lines traffic stops at and lanes' arrows.
    pub details: u64,
    /// Parking, parking spaces and traffic islands.
    pub areas: u64,
    /// Stretches of barriers.
    pub barriers: u64,
    /// Bytes of the tiles before gzip.
    pub raw_bytes: u64,
    pub gz_bytes: u64,
    /// Seconds reading the extract, and in all.
    pub read_seconds: f64,
    pub seconds: f64,
}

/// Where the HD roads of `archive` go: beside it, named after it.
pub fn hd_roads_path(archive: &Path) -> PathBuf {
    let stem = archive
        .file_stem()
        .map_or_else(|| "basemap".into(), |s| s.to_string_lossy());
    archive.with_file_name(format!("{stem}-roads.pmtiles"))
}

/// Reads the roads of the extract `pbf` and writes them to the new archive
/// `output`, with scratch files in the directory `temp`. The bytes are the
/// same for any number of `threads`. An existing `output` is never replaced.
/// Stops when `should_cancel` returns true, which it is asked every 50 ms or
/// so; whatever the outcome, no partial archive is left at `output`.
pub fn write_hd_roads(
    pbf: &Path,
    output: &Path,
    temp: &Path,
    threads: usize,
    should_cancel: impl Fn() -> bool,
) -> Result<HdRoadsReport, CompileError> {
    if output.exists() {
        return Err(CompileError::InvalidRequest(format!(
            "refusing to replace existing archive {}",
            output.display()
        )));
    }
    crate::control::run(
        threads,
        false,
        |_| {},
        should_cancel,
        |control| write(pbf, output, temp, threads, control),
    )
}

fn write(
    pbf: &Path,
    output: &Path,
    temp: &Path,
    threads: usize,
    control: &Control,
) -> Result<HdRoadsReport, String> {
    let start = Instant::now();
    let source = read(pbf, temp, threads, control)?;
    let read_seconds = start.elapsed().as_secs_f64();
    let Source {
        ways,
        obstacles,
        areas,
        barriers,
        incomplete_ways,
        ..
    } = &source;
    let network = network::build(ways);
    let details = details::build(ways, &network, &source.signs, &source.crossings);
    let groups = structures::build(ways, obstacles);
    let index = Index::new(&groups, ways, structure_tile);
    let painted: Vec<((u32, u32), &Detail)> = details
        .list
        .iter()
        .filter_map(|d| Some((index.on(ways, d.way, d.at)?, d)))
        .collect();
    let portals = structures::portals(ways);
    let stretches = furniture::stretches(ways, &index, barriers, structure_tile);
    let scene = Scene {
        ways,
        network: &network,
        details: &details,
        areas,
    };
    let tiles = tile_contents(&scene);
    let encoded = tiles
        .par_iter()
        .map(|(&(x, y), content)| {
            control.check()?;
            encode(&scene, (x, y), content)
        })
        .collect::<Result<Vec<_>, String>>()?;
    control.check()?;
    let mut report = HdRoadsReport {
        ways: ways.len() as u64,
        incomplete_ways: *incomplete_ways,
        junctions: network.junctions,
        profiles: distinct_profiles(ways),
        details: details.list.len() as u64,
        areas: areas.len() as u64,
        barriers: stretches.len() as u64,
        read_seconds,
        ..Default::default()
    };
    let mut written = Vec::with_capacity(encoded.len());
    for tile in encoded.into_iter().flatten() {
        report.surfaces += tile.surfaces;
        report.markings += tile.markings;
        report.raw_bytes += tile.raw_bytes;
        report.gz_bytes += tile.bytes.len() as u64;
        written.push((tile.id, tile.bytes));
    }
    for (id, raw, bytes) in structure_tiles(ways, &groups, &painted, &stretches, &portals)? {
        report.raw_bytes += raw;
        report.gz_bytes += bytes.len() as u64;
        written.push((id, bytes));
    }
    for (id, raw, bytes) in road_tiles(ways, control)? {
        report.raw_bytes += raw;
        report.gz_bytes += bytes.len() as u64;
        written.push((id, bytes));
    }
    report.groups = groups.len() as u64;
    report.pieces = groups.iter().map(|g| g.pieces.len() as u64).sum();
    report.crossings = groups.iter().map(|g| g.crossings.len() as u64).sum();
    report.tiles = written.len() as u64;
    written.sort_unstable_by_key(|tile| tile.0);
    write_archive(output, &source, &written)?;
    report.seconds = start.elapsed().as_secs_f64();
    Ok(report)
}

fn distinct_profiles(ways: &[Way]) -> u64 {
    let mut seen: FxHashSet<String> = FxHashSet::default();
    for way in ways {
        let profile = format!("{:?}", way.profile);
        if !seen.contains(&profile) {
            seen.insert(profile);
        }
    }
    seen.len() as u64
}

// Reading.

/// Ways by id, with their nodes and what they are.
type Read<T> = Vec<(i64, Vec<i64>, T)>;

/// The ways of the extract that matter, by what they are.
#[derive(Default)]
struct Roads {
    roads: Read<Profile>,
    obstacles: Read<Kind>,
    crossings: Read<Marking>,
    /// Whether paved, too.
    areas: Read<(AreaKind, bool)>,
    barriers: Read<Barrier>,
}

/// What is read of the extract, each by id.
struct Source {
    ways: Vec<Way>,
    /// What lies near bridges.
    obstacles: Vec<Obstacle>,
    areas: Vec<Area>,
    /// Points, world units a metre, and which.
    barriers: Vec<(Vec<Point>, f64, Barrier)>,
    /// By node.
    signs: FxHashMap<i64, Sign>,
    crossings: FxHashMap<i64, Marking>,
    /// Roads that lack a node.
    incomplete_ways: u64,
}

/// What a way that is no road of the archive is to a bridge over it.
fn obstacle<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Kind> {
    if get("tunnel").is_some_and(|v| v != "no") || get("bridge").is_some_and(|v| v != "no") {
        return None;
    }
    match (get("railway"), get("waterway"), get("highway")) {
        (Some("rail" | "tram" | "light_rail" | "subway" | "narrow_gauge"), ..) => Some(Kind::Rail),
        (_, Some("river" | "canal"), _) => Some(Kind::River),
        (_, Some("stream" | "ditch" | "drain"), _) => Some(Kind::Stream),
        (_, _, Some("footway" | "path" | "pedestrian" | "track" | "bridleway" | "steps")) => {
            Some(Kind::Path)
        }
        _ => None,
    }
}

impl Pass2Sink for Roads {
    fn on_relation_batch(&mut self, _: RelationBatch) -> Result<(), String> {
        Ok(())
    }

    fn on_node_batch(&mut self, _: NodeBatch) -> Result<(), String> {
        Ok(())
    }

    fn on_way_batch(&mut self, batch: WayBatch) -> Result<(), String> {
        for way in batch.ways {
            let mut nodes = way.node_ids;
            nodes.dedup();
            if nodes.len() < 2 {
                continue;
            }
            let get = |key: &str| way.tags.get(key);
            if ["footway", "path", "cycleway"]
                .iter()
                .any(|key| get(key) == Some("crossing"))
                && let Some(marking @ Marking::Marked(..)) = details::marking(get)
            {
                self.crossings.push((way.way_id, nodes.clone(), marking));
            }
            if let Some(barrier) = furniture::barrier(get) {
                self.barriers.push((way.way_id, nodes.clone(), barrier));
            }
            if nodes[0] == nodes[nodes.len() - 1]
                && let Some(area) = furniture::area(get)
            {
                self.areas.push((way.way_id, nodes.clone(), area));
            }
            if let Some(profile) = profile(get) {
                self.roads.push((way.way_id, nodes, profile));
            } else if let Some(kind) = obstacle(get) {
                self.obstacles.push((way.way_id, nodes, kind));
            }
        }
        Ok(())
    }
}

fn by_id<T>(mut ways: Read<T>) -> Read<T> {
    ways.sort_by_key(|way| way.0);
    ways.dedup_by_key(|way| way.0);
    ways
}

fn read(pbf: &Path, temp: &Path, threads: usize, control: &Control) -> Result<Source, String> {
    let profile = IngestBuildProfile::DesktopFull;
    let index = prepare_ingest_index_with_options(
        pbf,
        profile,
        temp,
        PrepareOptions::new()
            .worker_threads(threads)
            .keep_nodes(|tags| {
                matches!(
                    tags.get("highway"),
                    Some("crossing" | "stop" | "give_way" | "traffic_signals")
                )
            }),
        |_| {},
        || control.is_cancelled(),
    )?;
    control.check()?;
    let mut roads = Roads::default();
    run_pass2_with_options(
        pbf,
        profile,
        Pass2Options::new()
            .worker_threads(threads)
            .skip_nodes()
            .skip_relations()
            .keep_ways(|tags| {
                // fences are most of the barriers, and are none of these
                let keys = ["highway", "railway", "waterway", "area:highway"];
                keys.iter().any(|key| tags.contains_key(key))
                    || tags.contains_key("traffic_calming")
                    || furniture::barrier(|key| tags.get(key)).is_some()
                    || matches!(tags.get("amenity"), Some("parking" | "parking_space"))
            }),
        &mut roads,
        |_| {},
        || control.is_cancelled(),
    )?;
    control.check()?;
    let signs: FxHashMap<i64, Sign> = index
        .tagged_nodes
        .iter()
        .filter_map(|node| Some((node.node_id, details::sign(|key| node.tags.get(key))?)))
        .collect();
    let mut crossings: FxHashMap<i64, Marking> = FxHashMap::default();
    for (_, nodes, marking) in by_id(roads.crossings) {
        for node in nodes {
            crossings.entry(node).or_insert(marking);
        }
    }
    let found = by_id(roads.roads);
    let mut nodes = index.node_store.cursor();
    let mut ways = Vec::with_capacity(found.len());
    let mut incomplete = 0;
    let mut locate = |ids: &[i64]| -> Option<(Vec<Point>, f64)> {
        let located: Vec<(f64, f64)> =
            ids.iter().map(|&id| nodes.get(id)).collect::<Option<_>>()?;
        let latitude = located.iter().map(|p| p.0).sum::<f64>() / located.len() as f64;
        let scale = WORLD / (EQUATOR_M * latitude.to_radians().cos().max(0.05));
        Some((
            located.iter().map(|&(lat, lng)| world(lat, lng)).collect(),
            scale,
        ))
    };
    for (id, nodes, profile) in found {
        match locate(&nodes) {
            Some((points, scale)) => ways.push(Way {
                id,
                nodes,
                points,
                profile,
                scale,
            }),
            None => incomplete += 1,
        }
    }
    let areas = by_id(roads.areas)
        .into_iter()
        .filter_map(|(_, nodes, (kind, paved))| {
            let ring = furniture::ring(&locate(&nodes)?.0)?;
            Some(Area { kind, paved, ring })
        })
        .collect();
    let barriers = by_id(roads.barriers)
        .into_iter()
        .filter_map(|(_, nodes, barrier)| {
            let (points, scale) = locate(&nodes)?;
            Some((points, scale, barrier))
        })
        .collect();
    let near = structures::bridge_cells(&ways);
    let obstacles = by_id(roads.obstacles)
        .into_iter()
        .filter_map(|(_, nodes, kind)| {
            let (points, _) = locate(&nodes)?;
            let reaches = structures::cells(&points).any(|(_, cell)| near.contains(&cell));
            reaches.then_some(Obstacle {
                nodes,
                points,
                kind,
            })
        })
        .collect();
    Ok(Source {
        ways,
        obstacles,
        areas,
        barriers,
        signs,
        crossings,
        incomplete_ways: incomplete,
    })
}

fn world(lat: f64, lng: f64) -> Point {
    let lat = lat.clamp(-85.05, 85.05).to_radians();
    [
        (lng / 360.0 + 0.5) * WORLD,
        (0.5 - lat.tan().asinh() / (2.0 * std::f64::consts::PI)) * WORLD,
    ]
}

// Tiles.

/// What lies on the ground, all of it.
struct Scene<'a> {
    ways: &'a [Way],
    network: &'a Network,
    details: &'a Details,
    areas: &'a [Area],
}

#[derive(Default)]
struct Content {
    ways: Vec<u32>,
    runs: Vec<u32>,
    patches: Vec<u32>,
    details: Vec<u32>,
    areas: Vec<u32>,
}

/// What reaches each tile, in order.
fn tile_contents(scene: &Scene) -> BTreeMap<(u32, u32), Content> {
    let Scene { ways, network, .. } = scene;
    let mut tiles: BTreeMap<(u32, u32), Content> = BTreeMap::new();
    for (i, way) in ways.iter().enumerate() {
        let reach = way_reach(way);
        for tile in touched(&way.points, reach) {
            tiles.entry(tile).or_default().ways.push(i as u32);
        }
    }
    for (i, run) in network.runs.iter().enumerate() {
        let way = &ways[run.way as usize];
        if way.profile.lines.is_empty() && way.profile.parking == [0.0; 2] {
            continue;
        }
        for tile in touched(&way.points[run.points.clone()], way_reach(way)) {
            tiles.entry(tile).or_default().runs.push(i as u32);
        }
    }
    for (i, detail) in scene.details.list.iter().enumerate() {
        // past its strokes an arrow's head
        let reach = (detail.kind.width() + 2.0) * ways[detail.way as usize].scale + BUFFER;
        for tile in touched(&detail.strokes.concat(), reach) {
            tiles.entry(tile).or_default().details.push(i as u32);
        }
    }
    for (i, area) in scene.areas.iter().enumerate() {
        // those within it too
        let low = area
            .ring
            .iter()
            .copied()
            .reduce(|a, b| [a[0].min(b[0]), a[1].min(b[1])]);
        let high = area
            .ring
            .iter()
            .copied()
            .reduce(|a, b| [a[0].max(b[0]), a[1].max(b[1])]);
        let box_ = [low, high].map(|p| p.expect("a ring has points"));
        for tile in touched(&box_, BUFFER) {
            tiles.entry(tile).or_default().areas.push(i as u32);
        }
    }
    for (i, patch) in network.patches.iter().enumerate() {
        for tile in touched(&patch.ring, BUFFER) {
            tiles.entry(tile).or_default().patches.push(i as u32);
        }
    }
    tiles
}

/// World units from a way's line that its surface and a tile's buffer reach.
fn way_reach(way: &Way) -> f64 {
    let p = &way.profile;
    let parking = p.parking[0].max(p.parking[1]);
    (p.width / 2.0 + p.centre.abs() + parking) * way.scale + BUFFER
}

/// The tiles within `reach` of the line's segments, each once, in order.
fn touched(points: &[Point], reach: f64) -> Vec<(u32, u32)> {
    let last = (1_u32 << ZOOM) - 1;
    let tile = |v: f64| ((v / EXTENT).floor().max(0.0) as u32).min(last);
    let mut out = Vec::new();
    for p in points.windows(2) {
        let (x0, x1) = (p[0][0].min(p[1][0]) - reach, p[0][0].max(p[1][0]) + reach);
        let (y0, y1) = (p[0][1].min(p[1][1]) - reach, p[0][1].max(p[1][1]) + reach);
        for x in tile(x0)..=tile(x1) {
            for y in tile(y0)..=tile(y1) {
                out.push((x, y));
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

struct Encoded {
    id: u64,
    bytes: Vec<u8>,
    raw_bytes: u64,
    surfaces: u64,
    markings: u64,
}

/// What is joined into one surface.
type SurfaceKey = (Level, &'static str, bool);

fn kind(class: Class) -> &'static str {
    match class {
        Class::Sidewalk => "sidewalk",
        Class::Cycleway => "cycleway",
        _ => "road",
    }
}

fn level_name(level: Level) -> (&'static str, i8) {
    match level {
        Level::Ground => ("ground", 0),
        Level::Bridge(layer) => ("bridge", layer),
        Level::Tunnel(layer) => ("tunnel", layer),
    }
}

/// The stretches of a run that hold its lanes' lines, world units along it.
fn spans(run: &network::Edge, gaps: &[[f64; 2]], way: &Way) -> Vec<[f64; 2]> {
    let points = &way.points[run.points.clone()];
    let length: f64 = points
        .windows(2)
        .map(|p| network::distance(p[0], p[1]))
        .sum();
    let [start, end] = run.cut.map(|metres| metres * way.scale);
    let mut out = Vec::with_capacity(gaps.len() + 1);
    let mut from = start;
    for gap in gaps {
        out.push([from, gap[0] * way.scale]);
        from = from.max(gap[1] * way.scale);
    }
    out.push([from, length - end]);
    out.retain(|s| s[1] - s[0] >= MIN_MARKING * way.scale);
    // what is left to trim from either end
    out.iter().map(|s| [s[0], length - s[1]]).collect()
}

fn encode(scene: &Scene, (x, y): (u32, u32), content: &Content) -> Result<Option<Encoded>, String> {
    let Scene { ways, network, .. } = scene;
    let origin = [f64::from(x) * EXTENT, f64::from(y) * EXTENT];
    let rect: Rect = [
        origin[0] - BUFFER,
        origin[1] - BUFFER,
        origin[0] + EXTENT + BUFFER,
        origin[1] + EXTENT + BUFFER,
    ];
    let failed = |e: fast_mvt::MvtError| format!("Failed to encode a tile of roads: {e}");

    let mut groups: BTreeMap<SurfaceKey, Vec<Vec<Vec<Point>>>> = BTreeMap::new();
    for &index in &content.ways {
        let way = &ways[index as usize];
        let p = &way.profile;
        let reach = (p.width / 2.0 + p.centre.abs()) * way.scale;
        let wide: Rect = [
            rect[0] - reach,
            rect[1] - reach,
            rect[2] + reach,
            rect[3] + reach,
        ];
        let middle = geometry::offset(&way.points, p.centre * way.scale);
        let middle = geometry::extend(&middle, OVERLAP * way.scale);
        for run in geometry::near(&middle, wide) {
            groups
                .entry((p.level, kind(p.class), p.paved))
                .or_default()
                .extend(geometry::ribbon(&run, p.width * way.scale));
        }
    }
    for &index in &content.runs {
        let run = &network.runs[index as usize];
        let way = &ways[run.way as usize];
        let p = &way.profile;
        if p.parking == [0.0; 2] || p.level != Level::Ground {
            continue;
        }
        for span in spans(run, &scene.details.gaps[index as usize], way) {
            let Some(middle) = geometry::trim(&way.points[run.points.clone()], span) else {
                continue;
            };
            for (side, across) in [(-1.0, p.parking[0]), (1.0, p.parking[1])] {
                if across > 0.0 {
                    let at = p.centre + side * (p.width / 2.0 + across / 2.0);
                    groups
                        .entry((Level::Ground, "parking", true))
                        .or_default()
                        .extend(geometry::ribbon(
                            &geometry::offset(&middle, at * way.scale),
                            across * way.scale,
                        ));
                }
            }
        }
    }
    for &index in &content.areas {
        let area = &scene.areas[index as usize];
        let kind = match area.kind {
            AreaKind::Parking | AreaKind::Bay => "parking",
            AreaKind::Island => "island",
        };
        groups
            .entry((Level::Ground, kind, area.paved))
            .or_default()
            .push(vec![area.ring.clone()]);
    }
    for &index in &content.patches {
        let patch = &network.patches[index as usize];
        groups
            .entry((patch.level, kind(Class::Primary), patch.paved))
            .or_default()
            .push(vec![patch.ring.clone()]);
    }
    let mut surfaces = MvtLayerBuilder::new("surface").map_err(failed)?;
    let mut surface_count = 0;
    for ((level, kind, paved), ribbons) in groups {
        let shapes = geometry::union(&ribbons);
        let (name, layer) = level_name(level);
        for polygon in geometry::to_tile(&shapes, rect, origin, MIN_AREA) {
            let mut feature = surfaces.feature_of(MvtGeomType::Polygon);
            feature.tag_string("kind", kind).map_err(failed)?;
            feature.tag_string("level", name).map_err(failed)?;
            feature
                .tag_sint("layer", i64::from(layer))
                .map_err(failed)?;
            feature.tag_bool("paved", paved).map_err(failed)?;
            for (i, ring) in polygon.iter().enumerate() {
                let points = ring.iter().map(|p| MvtCoord { x: p[0], y: p[1] });
                feature.ring(points, i == 0).map_err(failed)?;
            }
            surfaces = feature.end();
            surface_count += 1;
        }
    }

    let local = |p: &Point| MvtCoord {
        x: (p[0] - origin[0]).round() as i32,
        y: (p[1] - origin[1]).round() as i32,
    };
    let mut markings = MvtLayerBuilder::new("marking").map_err(failed)?;
    let mut marking_count = 0;
    for &index in &content.runs {
        let run = &network.runs[index as usize];
        let way = &ways[run.way as usize];
        let (name, layer) = level_name(way.profile.level);
        for span in spans(run, &scene.details.gaps[index as usize], way) {
            let Some(middle) = geometry::trim(&way.points[run.points.clone()], span) else {
                continue;
            };
            for &(at, line) in &way.profile.lines {
                let moved = geometry::offset(&middle, at * way.scale);
                for part in geometry::clip(&moved, rect) {
                    let mut points: Vec<MvtCoord> = part.iter().map(local).collect();
                    points.dedup();
                    if points.len() < 2 {
                        continue;
                    }
                    let mut feature = markings.feature_of(MvtGeomType::LineString);
                    feature.tag_string("kind", line.kind()).map_err(failed)?;
                    feature.tag_bool("dashed", line.dashed()).map_err(failed)?;
                    feature.tag_string("level", name).map_err(failed)?;
                    feature
                        .tag_sint("layer", i64::from(layer))
                        .map_err(failed)?;
                    feature
                        .tag_uint("confidence", way.profile.confidence.id())
                        .map_err(failed)?;
                    feature.line(points).map_err(failed)?;
                    markings = feature.end();
                    marking_count += 1;
                }
            }
        }
    }
    // whole in every tile they reach, so that their dashes go on over the edge
    for &index in &content.details {
        let detail = &scene.details.list[index as usize];
        let (name, layer) = level_name(ways[detail.way as usize].profile.level);
        let lines = detail
            .strokes
            .iter()
            .map(|s| (MvtGeomType::LineString, &s[..]));
        let heads = detail.heads.iter().map(|h| (MvtGeomType::Polygon, &h[..]));
        for (geometry, points) in lines.chain(heads) {
            let mut points: Vec<MvtCoord> = points.iter().map(local).collect();
            points.dedup();
            let head = geometry == MvtGeomType::Polygon;
            if points.len() < if head { 3 } else { 2 } {
                continue;
            }
            let kind = if head {
                "arrow-head"
            } else {
                detail.kind.name()
            };
            let mut feature = markings.feature_of(geometry);
            feature.tag_string("kind", kind).map_err(failed)?;
            feature.tag_string("level", name).map_err(failed)?;
            feature
                .tag_sint("layer", i64::from(layer))
                .map_err(failed)?;
            feature
                .tag_uint("confidence", u64::from(detail.confidence))
                .map_err(failed)?;
            if head {
                feature.ring(points, true).map_err(failed)?;
            } else {
                feature.line(points).map_err(failed)?;
            }
            markings = feature.end();
            marking_count += 1;
        }
    }
    for &index in &content.areas {
        let area = &scene.areas[index as usize];
        if area.kind != AreaKind::Bay {
            continue;
        }
        let mut outline = area.ring.clone();
        outline.push(outline[0]);
        for part in geometry::clip(&outline, rect) {
            let mut points: Vec<MvtCoord> = part.iter().map(local).collect();
            points.dedup();
            if points.len() < 2 {
                continue;
            }
            let mut feature = markings.feature_of(MvtGeomType::LineString);
            feature.tag_string("kind", "bay").map_err(failed)?;
            feature.tag_string("level", "ground").map_err(failed)?;
            feature.tag_sint("layer", 0).map_err(failed)?;
            feature.tag_uint("confidence", 2).map_err(failed)?;
            feature.line(points).map_err(failed)?;
            markings = feature.end();
            marking_count += 1;
        }
    }

    let mut raw = Vec::new();
    for layer in [surfaces, markings] {
        if layer.num_features() > 0 {
            raw.extend_from_slice(&layer.encode());
        }
    }
    if raw.is_empty() {
        return Ok(None);
    }
    let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
    encoder
        .write_all(&raw)
        .and_then(|()| encoder.finish())
        .map_err(|e| format!("Failed to gzip a tile: {e}"))
        .and_then(|bytes| {
            let coord = TileCoord::new(ZOOM, x, y).map_err(|e| e.to_string())?;
            Ok(Some(Encoded {
                id: TileId::from(coord).value(),
                bytes,
                raw_bytes: raw.len() as u64,
                surfaces: surface_count,
                markings: marking_count,
            }))
        })
}

// The roads to be asked of.

/// The zoom of the tiles they are in, and its units across a tile.
const ROAD_ZOOM: u8 = 15;
const ROAD_EXTENT: u32 = 8192;

/// The tiles of the motor roads' ways: id, bytes before gzip, and gzipped.
fn road_tiles(ways: &[Way], control: &Control) -> Result<Vec<(u64, u64, Vec<u8>)>, String> {
    let across = f64::from(ROAD_EXTENT);
    let mut tiles: BTreeMap<(u32, u32), Vec<u32>> = BTreeMap::new();
    for (i, way) in ways.iter().enumerate() {
        // driveways and parking aisles are half the ways, and have no lanes to tell
        if way.profile.lanes.is_none() || way.profile.class == Class::Service {
            continue;
        }
        let mut of: Vec<(u32, u32)> = touched(&way.points, way_reach(way))
            .into_iter()
            .map(|(x, y)| (x / 2, y / 2))
            .collect();
        of.sort_unstable();
        of.dedup();
        for tile in of {
            tiles.entry(tile).or_default().push(i as u32);
        }
    }
    let failed = |e: fast_mvt::MvtError| format!("Failed to encode a tile of roads: {e}");
    let extent = fast_mvt::MvtExtent::new(ROAD_EXTENT).expect("an extent is not 0");
    let encode = |(&(x, y), members): (&(u32, u32), &Vec<u32>)| {
        control.check()?;
        let origin = [f64::from(x) * across, f64::from(y) * across];
        let mut layer = MvtLayerBuilder::new("road").map_err(failed)?;
        layer.extent(extent);
        for &index in members {
            let way = &ways[index as usize];
            let Some(lanes) = &way.profile.lanes else {
                continue;
            };
            let traffic = match (lanes.along, lanes.against) {
                (true, true) => "=",
                (true, false) => "+",
                (false, _) => "-",
            };
            let (name, level) = level_name(way.profile.level);
            let reach = way_reach(way);
            let wide: Rect = [
                origin[0] - reach,
                origin[1] - reach,
                origin[0] + across + reach,
                origin[1] + across + reach,
            ];
            for part in geometry::clip(&way.points, wide) {
                let mut points: Vec<MvtCoord> = part
                    .iter()
                    .map(|p| MvtCoord {
                        x: (p[0] - origin[0]).round() as i32,
                        y: (p[1] - origin[1]).round() as i32,
                    })
                    .collect();
                points.dedup();
                if points.len() < 2 {
                    continue;
                }
                let mut f = layer.feature_of(MvtGeomType::LineString);
                f.tag_sint("way", way.id).map_err(failed)?;
                f.tag_string("class", class_name(way.profile.class))
                    .map_err(failed)?;
                f.tag_string("level", name).map_err(failed)?;
                f.tag_sint("layer", i64::from(level)).map_err(failed)?;
                let text = format!(
                    "{:.2},{:.2},{},{},{traffic}",
                    lanes.left, lanes.width, lanes.backward, lanes.forward
                );
                f.tag_string("lanes", text).map_err(failed)?;
                f.line(points).map_err(failed)?;
                layer = f.end();
            }
        }
        if layer.num_features() == 0 {
            return Ok(None);
        }
        let raw = layer.encode();
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
        let bytes = encoder
            .write_all(&raw)
            .and_then(|()| encoder.finish())
            .map_err(|e| format!("Failed to gzip a tile: {e}"))?;
        let coord = TileCoord::new(ROAD_ZOOM, x, y).map_err(|e| e.to_string())?;
        Ok(Some((TileId::from(coord).value(), raw.len() as u64, bytes)))
    };
    let encoded = tiles
        .par_iter()
        .map(encode)
        .collect::<Result<Vec<_>, String>>()?;
    Ok(encoded.into_iter().flatten().collect())
}

// Structures.

/// The zoom of the tiles structures are in, and its units across a tile.
const STRUCTURE_ZOOM: u8 = 14;
const STRUCTURE_EXTENT: u32 = 65_536;

fn class_name(class: Class) -> &'static str {
    match class {
        Class::Motorway => "motorway",
        Class::Trunk => "trunk",
        Class::Primary => "primary",
        Class::Secondary => "secondary",
        Class::Tertiary => "tertiary",
        Class::Minor => "minor",
        Class::LivingStreet => "living_street",
        Class::Service => "service",
        Class::Cycleway => "cycleway",
        Class::Sidewalk => "sidewalk",
    }
}

/// World units across a tile of structures, and the tile a point is in.
const STRUCTURE_ACROSS: f64 = EXTENT * (1_u32 << (ZOOM - STRUCTURE_ZOOM)) as f64;

fn structure_tile(p: Point) -> (u32, u32) {
    let of = |v: f64| (v / STRUCTURE_ACROSS) as u32;
    (of(p[0]), of(p[1]))
}

/// The tiles of the groups, what is painted on their pieces and the barriers:
/// id, bytes before gzip, and gzipped.
fn structure_tiles(
    ways: &[Way],
    groups: &[Group],
    painted: &[((u32, u32), &Detail)],
    barriers: &[Stretch],
    portals: &[Portal],
) -> Result<Vec<(u64, u64, Vec<u8>)>, String> {
    let across = STRUCTURE_ACROSS;
    let mut paint: BTreeMap<u32, Vec<(u32, &Detail)>> = BTreeMap::new();
    for &((group, piece), detail) in painted {
        paint.entry(group).or_default().push((piece, detail));
    }
    // each tile's groups, and its barriers
    type Held<'a> = (Vec<usize>, Vec<&'a Stretch>, Vec<&'a Portal>);
    let mut tiles: BTreeMap<(u32, u32), Held> = BTreeMap::new();
    for (i, group) in groups.iter().enumerate() {
        let tile = structure_tile(group.pieces[0].points[0]);
        tiles.entry(tile).or_default().0.push(i);
    }
    for stretch in barriers {
        tiles.entry(stretch.tile).or_default().1.push(stretch);
    }
    for portal in portals {
        let tile = structure_tile(portal.points[0]);
        tiles.entry(tile).or_default().2.push(portal);
    }
    let failed = |e: fast_mvt::MvtError| format!("Failed to encode a tile of structures: {e}");
    let extent = fast_mvt::MvtExtent::new(STRUCTURE_EXTENT).expect("an extent is not 0");
    let mut out = Vec::with_capacity(tiles.len());
    for ((x, y), (members, barriers, portals)) in tiles {
        let origin = [f64::from(x) * across, f64::from(y) * across];
        let coord = |p: &Point| {
            let unit =
                |v: f64, o: f64| ((v - o) / across * f64::from(STRUCTURE_EXTENT)).round() as i32;
            MvtCoord {
                x: unit(p[0], origin[0]),
                y: unit(p[1], origin[1]),
            }
        };
        let mut layer = MvtLayerBuilder::new("structure").map_err(failed)?;
        layer.extent(extent);
        for (number, &index) in members.iter().enumerate() {
            let group = &groups[index];
            for (i, piece) in group.pieces.iter().enumerate() {
                let way = &ways[piece.way as usize];
                let p = &way.profile;
                let (_, level) = level_name(p.level);
                let lines: Vec<String> = p
                    .lines
                    .iter()
                    .map(|(at, line)| {
                        let dashed = if line.dashed() { "-" } else { "" };
                        format!("{at:.2}:{}{dashed}", line.kind())
                    })
                    .collect();
                let mut f = layer.feature_of(MvtGeomType::LineString);
                f.tag_uint("group", number as u64).map_err(failed)?;
                f.tag_uint("piece", i as u64).map_err(failed)?;
                f.tag_sint("way", way.id).map_err(failed)?;
                f.tag_sint("layer", i64::from(level)).map_err(failed)?;
                f.tag_string("class", class_name(p.class)).map_err(failed)?;
                f.tag_double("width", p.width).map_err(failed)?;
                f.tag_double("centre", p.centre).map_err(failed)?;
                f.tag_bool("paved", p.paved).map_err(failed)?;
                f.tag_string("lines", lines.join(",")).map_err(failed)?;
                f.tag_sint("node0", piece.nodes[0]).map_err(failed)?;
                f.tag_sint("node1", piece.nodes[1]).map_err(failed)?;
                f.tag_bool("anchor0", piece.anchor[0]).map_err(failed)?;
                f.tag_bool("anchor1", piece.anchor[1]).map_err(failed)?;
                f.tag_bool("approach", piece.approach).map_err(failed)?;
                f.line(piece.points.iter().map(coord)).map_err(failed)?;
                layer = f.end();
            }
            for crossing in &group.crossings {
                let mut f = layer.feature_of(MvtGeomType::Point);
                f.tag_uint("group", number as u64).map_err(failed)?;
                f.tag_uint("over", u64::from(crossing.over))
                    .map_err(failed)?;
                f.tag_double("at", crossing.at).map_err(failed)?;
                f.tag_string("kind", crossing.kind).map_err(failed)?;
                f.tag_double("clearance", crossing.clearance)
                    .map_err(failed)?;
                f.tag_double("half", crossing.half).map_err(failed)?;
                if let Some((under, at)) = crossing.under {
                    f.tag_uint("under", u64::from(under)).map_err(failed)?;
                    f.tag_double("under_at", at).map_err(failed)?;
                }
                f.points([coord(&crossing.point)]).map_err(failed)?;
                layer = f.end();
            }
        }
        let mut paints = MvtLayerBuilder::new("paint").map_err(failed)?;
        paints.extent(extent);
        for (number, &index) in members.iter().enumerate() {
            for (piece, detail) in paint.get(&(index as u32)).into_iter().flatten() {
                for ring in detail.rings(ways[detail.way as usize].scale) {
                    let mut f = paints.feature_of(MvtGeomType::Polygon);
                    f.tag_uint("group", number as u64).map_err(failed)?;
                    f.tag_uint("piece", u64::from(*piece)).map_err(failed)?;
                    f.tag_string("kind", detail.kind.name()).map_err(failed)?;
                    f.ring(ring.iter().map(coord), true).map_err(failed)?;
                    paints = f.end();
                }
            }
        }
        let mut lines = MvtLayerBuilder::new("barrier").map_err(failed)?;
        lines.extent(extent);
        for stretch in barriers {
            let mut f = lines.feature_of(MvtGeomType::LineString);
            f.tag_string("kind", stretch.barrier.kind).map_err(failed)?;
            f.tag_double("height", stretch.barrier.height)
                .map_err(failed)?;
            if let Some((group, piece)) = stretch.on {
                let number = members.binary_search(&(group as usize));
                let number = number.expect("a group is in its tile");
                f.tag_uint("group", number as u64).map_err(failed)?;
                f.tag_uint("piece", u64::from(piece)).map_err(failed)?;
            }
            f.line(stretch.points.iter().map(coord)).map_err(failed)?;
            lines = f.end();
        }
        let mut mouths = MvtLayerBuilder::new("portal").map_err(failed)?;
        mouths.extent(extent);
        for portal in portals {
            let way = &ways[portal.way as usize];
            let mut f = mouths.feature_of(MvtGeomType::LineString);
            f.tag_sint("way", way.id).map_err(failed)?;
            f.tag_double("width", way.profile.width).map_err(failed)?;
            f.tag_double("centre", way.profile.centre).map_err(failed)?;
            f.tag_bool("inward", portal.points[0] == way.points[0])
                .map_err(failed)?;
            f.line(portal.points.iter().map(coord)).map_err(failed)?;
            mouths = f.end();
        }
        let mut raw = Vec::new();
        for layer in [layer, paints, lines, mouths] {
            if layer.num_features() > 0 {
                raw.extend_from_slice(&layer.encode());
            }
        }
        let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
        let bytes = encoder
            .write_all(&raw)
            .and_then(|()| encoder.finish())
            .map_err(|e| format!("Failed to gzip a tile: {e}"))?;
        let coord = TileCoord::new(STRUCTURE_ZOOM, x, y).map_err(|e| e.to_string())?;
        out.push((TileId::from(coord).value(), raw.len() as u64, bytes));
    }
    Ok(out)
}

// The archive.

fn metadata() -> String {
    let text = "String";
    serde_json::json!({
        "format": HD_ROADS_FORMAT,
        "version": HD_ROADS_VERSION,
        "attribution": "<a href=\"https://www.openstreetmap.org/copyright\">© OpenStreetMap contributors</a>",
        "vector_layers": [
            {"id": "surface", "minzoom": ZOOM, "maxzoom": ZOOM,
             "fields": {"kind": text, "level": text, "layer": "Number", "paved": "Boolean"}},
            {"id": "marking", "minzoom": ZOOM, "maxzoom": ZOOM,
             "fields": {"kind": text, "dashed": "Boolean", "level": text, "layer": "Number", "confidence": "Number"}},
            {"id": "road", "minzoom": ROAD_ZOOM, "maxzoom": ROAD_ZOOM,
             "fields": {"way": "Number", "class": text, "level": text, "layer": "Number", "lanes": text}},
            {"id": "structure", "minzoom": STRUCTURE_ZOOM, "maxzoom": STRUCTURE_ZOOM, "fields": {}},
            {"id": "paint", "minzoom": STRUCTURE_ZOOM, "maxzoom": STRUCTURE_ZOOM, "fields": {}},
            {"id": "barrier", "minzoom": STRUCTURE_ZOOM, "maxzoom": STRUCTURE_ZOOM, "fields": {}},
            {"id": "portal", "minzoom": STRUCTURE_ZOOM, "maxzoom": STRUCTURE_ZOOM, "fields": {}},
        ],
    })
    .to_string()
}

fn write_archive(output: &Path, source: &Source, tiles: &[(u64, Vec<u8>)]) -> Result<(), String> {
    let failed = |e: pmtiles::PmtError| format!("Failed to write {}: {e}", output.display());
    let mut box_ = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    let roads = source.ways.iter().flat_map(|w| &w.points);
    let areas = source.areas.iter().flat_map(|a| &a.ring);
    let barriers = source.barriers.iter().flat_map(|b| &b.0);
    for p in roads.chain(areas).chain(barriers) {
        box_ = [
            box_[0].min(p[0]),
            box_[1].min(p[1]),
            box_[2].max(p[0]),
            box_[3].max(p[1]),
        ];
    }
    if box_[0] > box_[2] {
        box_ = [0.0, 0.0, WORLD, WORLD];
    }
    let lng = |x: f64| (x / WORLD - 0.5) * 360.0;
    let lat = |y: f64| {
        ((0.5 - y / WORLD) * 2.0 * std::f64::consts::PI)
            .sinh()
            .atan()
            .to_degrees()
    };
    let (west, south, east, north) = (lng(box_[0]), lat(box_[3]), lng(box_[2]), lat(box_[1]));
    let partial = PartialArchive::create(output)?;
    let mut writer = PmTilesWriter::new(TileType::Mvt)
        .precompressed(Compression::Gzip)
        .min_zoom(STRUCTURE_ZOOM)
        .max_zoom(ZOOM)
        .bounds(west, south, east, north)
        .center((west + east) / 2.0, (south + north) / 2.0)
        .center_zoom(ZOOM)
        .metadata(metadata())
        .create(partial.file())
        .map_err(failed)?;
    for (id, bytes) in tiles {
        let coord = TileCoord::from(TileId::new(*id).map_err(failed)?);
        writer.add_tile(coord, bytes).map_err(failed)?;
    }
    writer.finalize().map_err(failed)?;
    partial.persist(output)
}
