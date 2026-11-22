//! HD roads: a PMTiles archive of vector tiles beside a basemap, holding roads
//! at their true width, read from an OSM extract. It is a pass of its own, so
//! it changes nothing in the basemap.
//!
//! Every tile is z16 (a unit is about 10 cm at 50 degrees), and renderers
//! overzoom it. Two layers:
//!
//! - `surface`, polygons: the carriageways, `kind` `road`, `sidewalk` or
//!   `cycleway`, as the union of the roads of one `level` (`ground`, `bridge`
//!   or `tunnel`, with its `layer`), corners rounded where roads meet, and
//!   `paved`.
//! - `marking`, lines: the painted lines, `kind` `edge`, `lane` or `centre`,
//!   `dashed`, `level`, `layer`, and `confidence` (0 the class's usual lanes,
//!   1 tagged lanes, 2 tagged lanes and width). They stop short of junctions.
//!
//! One layer more, in z14 tiles of 65536 units:
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
//!
//! Traffic keeps right. Tiles are worked out one by one from the roads that
//! reach them, in order, so the bytes do not depend on the number of threads.

mod geometry;
mod network;
mod profile;
mod structures;

use crate::control::{CompileError, Control};
use crate::omt::PartialArchive;
use fast_mvt::{MvtCoord, MvtGeomType, MvtLayerBuilder};
use flate2::write::GzEncoder;
use geometry::Rect;
use network::{Network, Point, Way};
use osm_pbf_ingest::{
    IngestBuildProfile, NodeBatch, Pass2Options, Pass2Sink, PrepareOptions, RelationBatch,
    WayBatch, prepare_ingest_index_with_options, run_pass2_with_options,
};
use pmtiles::{Compression, PmTilesWriter, TileCoord, TileId, TileType};
use profile::{Class, Level, Profile, profile};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;
use structures::{Group, Kind, Obstacle};

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
    let (ways, obstacles, incomplete_ways) = read_ways(pbf, temp, threads, control)?;
    let read_seconds = start.elapsed().as_secs_f64();
    let network = network::build(&ways);
    let groups = structures::build(&ways, &obstacles);
    let tiles = tile_contents(&ways, &network);
    let encoded = tiles
        .par_iter()
        .map(|(&(x, y), content)| {
            control.check()?;
            encode(&ways, &network, (x, y), content)
        })
        .collect::<Result<Vec<_>, String>>()?;
    control.check()?;
    let mut report = HdRoadsReport {
        ways: ways.len() as u64,
        incomplete_ways,
        junctions: network.junctions,
        profiles: distinct_profiles(&ways),
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
    for (id, raw, bytes) in structure_tiles(&ways, &groups)? {
        report.raw_bytes += raw;
        report.gz_bytes += bytes.len() as u64;
        written.push((id, bytes));
    }
    report.groups = groups.len() as u64;
    report.pieces = groups.iter().map(|g| g.pieces.len() as u64).sum();
    report.crossings = groups.iter().map(|g| g.crossings.len() as u64).sum();
    report.tiles = written.len() as u64;
    written.sort_unstable_by_key(|tile| tile.0);
    write_archive(output, &ways, &written)?;
    report.seconds = start.elapsed().as_secs_f64();
    Ok(report)
}

fn distinct_profiles(ways: &[Way]) -> u64 {
    let mut seen: Vec<String> = ways.iter().map(|w| format!("{:?}", w.profile)).collect();
    seen.sort_unstable();
    seen.dedup();
    seen.len() as u64
}

// Reading.

#[derive(Default)]
struct Roads {
    roads: Vec<(i64, Vec<i64>, Profile)>,
    obstacles: Vec<(i64, Vec<i64>, Kind)>,
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
            if let Some(profile) = profile(|key| way.tags.get(key)) {
                self.roads.push((way.way_id, nodes, profile));
            } else if let Some(kind) = obstacle(|key| way.tags.get(key)) {
                self.obstacles.push((way.way_id, nodes, kind));
            }
        }
        Ok(())
    }
}

/// The extract's roads by id, what lies near its bridges, and how many roads
/// lack a node.
fn read_ways(
    pbf: &Path,
    temp: &Path,
    threads: usize,
    control: &Control,
) -> Result<(Vec<Way>, Vec<Obstacle>, u64), String> {
    let profile = IngestBuildProfile::DesktopFull;
    let index = prepare_ingest_index_with_options(
        pbf,
        profile,
        temp,
        PrepareOptions::new().worker_threads(threads),
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
                ["highway", "railway", "waterway"]
                    .iter()
                    .any(|key| tags.contains_key(key))
            }),
        &mut roads,
        |_| {},
        || control.is_cancelled(),
    )?;
    control.check()?;
    let Roads {
        mut roads,
        mut obstacles,
    } = roads;
    roads.sort_by_key(|road| road.0);
    roads.dedup_by_key(|road| road.0);
    let mut nodes = index.node_store.cursor();
    let mut ways = Vec::with_capacity(roads.len());
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
    for (id, nodes, profile) in roads {
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
    obstacles.sort_by_key(|o| o.0);
    obstacles.dedup_by_key(|o| o.0);
    let near = structures::bridge_cells(&ways);
    let obstacles = obstacles
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
    Ok((ways, obstacles, incomplete))
}

fn world(lat: f64, lng: f64) -> Point {
    let lat = lat.clamp(-85.05, 85.05).to_radians();
    [
        (lng / 360.0 + 0.5) * WORLD,
        (0.5 - lat.tan().asinh() / (2.0 * std::f64::consts::PI)) * WORLD,
    ]
}

// Tiles.

#[derive(Default)]
struct Content {
    ways: Vec<u32>,
    edges: Vec<u32>,
    patches: Vec<u32>,
}

/// What reaches each tile, in order.
fn tile_contents(ways: &[Way], network: &Network) -> BTreeMap<(u32, u32), Content> {
    let mut tiles: BTreeMap<(u32, u32), Content> = BTreeMap::new();
    for (i, way) in ways.iter().enumerate() {
        let reach = way_reach(way);
        for tile in touched(&way.points, reach) {
            tiles.entry(tile).or_default().ways.push(i as u32);
        }
    }
    for (i, edge) in network.edges.iter().enumerate() {
        let way = &ways[edge.way as usize];
        if way.profile.lines.is_empty() {
            continue;
        }
        for tile in touched(&way.points[edge.points.clone()], way_reach(way)) {
            tiles.entry(tile).or_default().edges.push(i as u32);
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
    (p.width / 2.0 + p.centre.abs()) * way.scale + BUFFER
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

fn encode(
    ways: &[Way],
    network: &Network,
    (x, y): (u32, u32),
    content: &Content,
) -> Result<Option<Encoded>, String> {
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

    let mut markings = MvtLayerBuilder::new("marking").map_err(failed)?;
    let mut marking_count = 0;
    for &index in &content.edges {
        let edge = &network.edges[index as usize];
        let way = &ways[edge.way as usize];
        let cut = edge.cut.map(|metres| metres * way.scale);
        let Some(middle) = geometry::trim(&way.points[edge.points.clone()], cut) else {
            continue;
        };
        let length: f64 = middle
            .windows(2)
            .map(|p| network::distance(p[0], p[1]))
            .sum();
        if length < MIN_MARKING * way.scale {
            continue;
        }
        let (name, layer) = level_name(way.profile.level);
        for &(at, line) in &way.profile.lines {
            let moved = geometry::offset(&middle, at * way.scale);
            for part in geometry::clip(&moved, rect) {
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

/// The groups' tiles: id, bytes before gzip, and gzipped.
fn structure_tiles(ways: &[Way], groups: &[Group]) -> Result<Vec<(u64, u64, Vec<u8>)>, String> {
    let across = EXTENT * f64::from(1_u32 << (ZOOM - STRUCTURE_ZOOM));
    let mut tiles: BTreeMap<(u32, u32), Vec<usize>> = BTreeMap::new();
    for (i, group) in groups.iter().enumerate() {
        let p = group.pieces[0].points[0];
        let tile = ((p[0] / across) as u32, (p[1] / across) as u32);
        tiles.entry(tile).or_default().push(i);
    }
    let failed = |e: fast_mvt::MvtError| format!("Failed to encode a tile of structures: {e}");
    let extent = fast_mvt::MvtExtent::new(STRUCTURE_EXTENT).expect("an extent is not 0");
    let mut out = Vec::with_capacity(tiles.len());
    for ((x, y), members) in tiles {
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
        let raw = layer.encode();
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
            {"id": "structure", "minzoom": STRUCTURE_ZOOM, "maxzoom": STRUCTURE_ZOOM, "fields": {}},
        ],
    })
    .to_string()
}

fn write_archive(output: &Path, ways: &[Way], tiles: &[(u64, Vec<u8>)]) -> Result<(), String> {
    let failed = |e: pmtiles::PmtError| format!("Failed to write {}: {e}", output.display());
    let mut box_ = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for p in ways.iter().flat_map(|w| &w.points) {
        box_ = [
            box_[0].min(p[0]),
            box_[1].min(p[1]),
            box_[2].max(p[0]),
            box_[3].max(p[1]),
        ];
    }
    if ways.is_empty() {
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
