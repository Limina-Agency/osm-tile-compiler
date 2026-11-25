//! HD roads: a PMTiles archive of vector tiles beside a basemap, holding roads
//! at their true width, read from an OSM extract. It is a pass of its own, so
//! it changes nothing in the basemap.
//!
//! What is drawn on the ground is in z16 tiles (a unit is about 10 cm at 50
//! degrees), which renderers overzoom. Two layers:
//!
//! - `surface`, polygons: the carriageways, `kind` `road`, `sidewalk` or
//!   `cycleway`, as the union of the roads of one `level` (`ground`, `bridge`
//!   or `tunnel`, with its `layer`), corners rounded where roads meet, a disc
//!   where one ends in a turning circle (as wide as its `diameter`, else 10 to
//!   16 m), and `paved`; a road paved with sett, cobbles, slabs or concrete says
//!   so in `paved_by` (`sett`, `cobble`, `slabs`, `concrete`). A road whose width
//!   no tag gives is narrowed, as far as its lanes may be, where paths mapped
//!   beside it would lie under it (`hd_roads/sides.rs`). A carriageway mapped
//!   as an area (`area:highway`) is that area, not
//!   what its way is widened to. Beside them `parking`, and in them `island`, what
//!   is kerbed off, the middles of crossings with one too (two-way roads of 6 m at
//!   the least). In them too, on the ground, what the tags put beside the lanes
//!   (`hd_roads/sides.rs`): `cycle_lane`, `bus_lane` (one of the lanes),
//!   `parking_lane` and `table` (a raised table's top, across the road); and out
//!   past their edge `sidewalk` and `cycleway` strips, where no way of their own
//!   is mapped beside the road, joined with those ways' own.
//!   Paths are `footway`, `path` (bridleways too), `pedestrian` and `steps`,
//!   the last on the ground alone: 2, 1.5, 5 and 2 m wide if no tag says, and
//!   only what no `road` of their level takes. `footway` and `pedestrian` are
//!   areas and squares too, which are whole, and take from the paths of
//!   another kind that run in them. Tracks, and paths within buildings or on
//!   other floors, are not there.
//!   Under them `ballast` (3.2 m a track) and a tram's own `trackbed` (2.8 m),
//!   0.8 m less where the gauge is under 1.2 m, tracks side by side as one; and
//!   `platform`. Under all of them, ground that is no road's
//!   (`hd_roads/grounds.rs`), with its `material` (`hard`, `rubber`, `turf`,
//!   `grass`, `clay`, `sand`, `earth` or `plain`, what no tag says nor is usual):
//!   `pitch`, `track` (to run on: a loop is 4 lanes of 1.22 m if no tag says),
//!   `runway` (45 m wide if no tag says, a tenth of its length at most),
//!   `taxiway` (23 m, a taxilane 15, on soft ground 15), `apron`, `helipad`,
//!   in that order, and `yard`, under them, a filling station's or services'.
//!   A smaller pitch or track is cut out of a larger one it lies on.
//! - `marking`, lines: the painted lines, `kind` `edge`, `lane`, `centre`,
//!   `cycle` (beside a cycle lane) or `bus` (beside a bus lane, to be drawn 0.25 m),
//!   `dashed`, `level`, `layer`, and `confidence` (0 the class's usual lanes,
//!   1 tagged lanes, 2 tagged lanes and width); `colour` `yellow` where the
//!   country's are (else white), and of a dashed one `dash`, `<on>-<off>` metres,
//!   where they are not 2 and 4. They stop short of junctions
//!   and crossings, and of whatever else is painted; within a junction are none.
//!   Then, to be drawn at the width given here, and dashed from
//!   the line's start with bars of 0.5 m and gaps of 0.5 m where it says so:
//!   `zebra` (4 m, dashed: the bars; the line runs across the road), `dots`
//!   (0.5 m, dashed: the two rows cyclists cross between, and studs), `crossing`
//!   (0.3 m, the two lines of one), `stop` (0.5 m, or its `width`), `signal`
//!   (0.4 m: where traffic stops at signals; `dashed` where it is bars),
//!   `give_way` (0.5 m, dashed; one or two lines), `give_way-tri` (polygons: the
//!   triangles where traffic gives way, pointing at it), `merge` (0.24 m: a bar, 2 m
//!   long, of the line between roads that merge or part, and a `centre` by there
//!   is a bar too), `gore` (0.4 m: a chevron between them, up to the nose), `guide`
//!   (0.12 m: a bar, 1 m long, of the line over a junction with signals between lanes
//!   that turn side by side) and
//!   `arrow` (0.25 m), whose heads are `arrow-head`, polygons, as is the `disc` of a
//!   mini roundabout; `bicycle` and `sharrow` (0.1 m) in cycle lanes and lanes
//!   cyclists share, `bus-text` (0.15 m: "BUS") in bus lanes, and `hump`, polygons:
//!   the triangles on the ramps of humps, cushions and tables. Their `confidence` is 2 where the thing
//!   itself is tagged, 1 where another tag implies it, 0 where it is usual.
//!   `bay` is the outline of a parking space, a line between the bays of a
//!   strip of a car park that says how many and which way they lie, or a line
//!   across parking in the carriageway where how it is parked is tagged; that
//!   parking's edge is `parking-edge` (0.12 m, dashed 1 m on and 1 m off from
//!   where it begins), `kerb` (0.15 m) the carriageway's edge where a sidewalk or
//!   track strip is beside it, and
//!   `wheelchair` a symbol on a bay for wheelchairs (0.15 m), upright to who drives
//!   in. Painted on ground of `surface`'s: `pitch`, a court's and a track's
//!   lines (0.1 m, `dashed` 0.15 m on and off where it says so, and whole);
//!   `runway`, a runway's middle line, 30 m dashes 20 m or more apart, and the
//!   numbers its `ref` gives, if they are of its direction (0.9 m); `runway-bar`,
//!   polygons, its thresholds' bars and where one aims; `taxi`, a taxiway's
//!   middle line (0.3 m, yellow), and `helipad`, a helipad's circle and H (0.4 m).
//!   Where a carriageway of the ground is, none of these is. And tracks', with their `gauge`
//!   (millimetres): `rail`, a line along each of the two (to be drawn 0.12 m
//!   wide), and `sleeper`, one down the middle (2.6 m wide, 1.8 under a gauge
//!   of 1200, dashed 0.25 m on and 0.35 m off); a tram in a street has its
//!   rails alone, and no surface. A road that crosses lies over ballast and
//!   sleepers, and under the rails. `step` is the treads of steps: a line down
//!   their middle, to be drawn `width` metres wide (one of 1, 1.5, 2, 3 and 5,
//!   the widest they are no narrower than) and dashed 0.1 m on and 0.22 off.
//!
//! Three layers in z15 tiles of 8192 units (the same units):
//!
//! - `road`, lines: the motor roads' ways but for service roads, not to be
//!   drawn but asked of: `way`, `class`, `level`, `layer`, and `lanes`, metres and counts
//!   `<left>,<lane>,<against>,<along>,<traffic>[,l]`: the left edge of the leftmost
//!   lane right of the line, a lane's width, the lanes against the way (the
//!   left ones, or with `l`, where traffic keeps left, the right ones) and along
//!   it, and which way traffic goes (`+`, `-` or `=`).
//!   They reach as far out of their tile as their surface does.
//! - `fence`, lines on the ground: `kind` `fence`, `hedge`, `wall`,
//!   `retaining_wall`, `city_wall` or `handrail`, and `height` (metres, tagged or
//!   usual: 1.5, 1.5, 1.8, 1.5, 5 and 1). Cut at the tile's edges, and where a
//!   carriageway of the ground runs or a bridge's edge, whose parapet they are.
//! - `thing`, points: `kind` `lamp`, `signal`, `stop`, `give_way`, `sign`,
//!   `bollard`, `lift_gate`, `gate`, `bus_stop`, `shelter`, `mirror`, `hydrant`,
//!   `bench`, `bin`, `cabinet` or `column`, and `turn`, degrees clockwise from
//!   north: where it faces, a lamp leans or, from the post by the road, a gate
//!   closes; without one no tag and no road says. `size` is a lamp's height or a
//!   gate's length (metres). On a bridge, the z14 tile `x`, `y` its `group` is in
//!   and its `piece`. A thing mapped up to 1.5 m within a carriageway stands beside it, as
//!   widths are mostly usual ones, and one further in is left out; on a bridge it
//!   stands by its edge. Signs and signals stand on the side traffic keeps to of the lines painted for
//!   them, and gates on motor roads of the ground alone. Hydrants are pillars.
//!
//! Four layers more, in z14 tiles of 65536 units:
//!
//! - `structure`: what a bridge's heights are worked out from (they need
//!   terrain, so they are not here). Each group of bridges that join or cross
//!   is whole in one tile, the first's, and may reach out of it. Lines are the
//!   bridges' pieces: `group`, `piece` (its number in the group), `way`,
//!   `layer`, `class` (a road's, a path's, or a track's `rail` or `tram`), `width` and
//!   `centre` (metres, of the surface across the line), `paved` (of a track,
//!   whether it lies in a road), `lines` (`<metres>:<kind>[-]`, painted lines,
//!   or a track's rails, `-` dashed), `dash` (of each line the metres of a
//!   dash's period, as below, gone at the piece's start; if absent none are)
//!   and `pattern` (as a marking's `dash`, where it is not 2 and 4),
//!   `gaps` (`<from>-<to>,..`, metres along the piece that hold none of them),
//!   `node0` and `node1` (pieces join where these are equal), `anchor0` and
//!   `anchor1` (whether it meets the ground there), and `approach` (a road on
//!   the ground leading up to a bridge, to be raised to it if need be, or a
//!   footbridge's path, or, as `steps`, the flight that goes up to it), and
//!   `lead` (a motor road's, last in its group, 60 m on from where a piece meets
//!   the ground, onward past junctions as straight as it goes: to be raised as an
//!   approach up to a step in the ground it climbs, the end of a structure the
//!   tags leave short, and left out where there is none; its number, the same of
//!   each way's piece of it, those one after another as it goes on, and
//!   `lead_back` where a piece is followed from its end). Where its
//!   cross-section is another at its start, or its end: `width0`, `centre0` and
//!   `lines0` (metres alone, as `lines`), which over `taper0` metres turn into
//!   those above, eased at both ends; `width1` and so on. `bare0`, and `bare1`: of
//!   each line the metres from the piece's start, and end, that it is not painted.
//!   Points are where a piece
//!   passes over something: `group`, `over` and `at` (metres along), `kind`
//!   (`road`, `path`, `rail`, `water` or `bridge`), `clearance` and `half`
//!   (metres), and of a bridge of the group `under` and `under_at`.
//! - `paint`, polygons: what else is painted on those pieces, `kind` as a
//!   marking's, on the `piece` of the `group`, convex rings; and a tram's
//!   `rail` on a road's bridge.
//! - `barrier`, lines: `kind` `guard_rail`, `jersey`, `cable` or `noise`,
//!   `height` (metres), and where it stands on one, the `group` and `piece`.
//!   What runs along a bridge's edge is left to its parapet.
//! - `portal`, lines: where a road's or a track's tunnel comes out, `way`, `width`, `centre`
//!   and `inward` (whether the line, in the way's direction, goes into it).
//!
//! Where ways follow each other their lanes do (`hd_roads/lanes.rs`): each lies
//! where its lanes go on from the one before, and lanes open and close over a
//! taper; where a road parts or two merge each arm begins as its part of the wide
//! one (`hd_roads/merges.rs`). A road that goes straight over a junction is one as
//! well, and nodes of one junction are taken together (`hd_roads/junctions.rs`).
//! Dashes of lanes' lines are as the country's (2 m with 4 m after where its
//! `dash` says nothing), and go on from way to way and over the tiles' edges: a dashed line begins where a dash does, which may
//! be up to 6 m out of its tile, and what a way's end leaves of a dash is a line
//! of its own.
//!
//! How roads look is the country's, by a table (`hd_roads/country.rs`): which
//! side traffic keeps to, a crossing at signals whose marking is not tagged
//! (a zebra, two lines, or studs), what is painted where traffic gives way and
//! at signals, stop lines' width, dashes, and yellow lines. The country is where
//! a way's middle is, by the extract's own boundaries; elsewhere it is the
//! table's default, which keeps right. Where traffic keeps left everything of
//! lanes is the other way about but what tags say as drivers see it.
//!
//! Tiles are worked out one by one from the roads that
//! reach them, in order, so the bytes do not depend on the number of threads.

mod country;
mod details;
mod furniture;
mod geometry;
mod grounds;
mod junctions;
mod lanes;
mod merges;
mod network;
mod paint;
mod profile;
mod rails;
mod sides;
mod structures;
mod things;

use crate::control::{CompileError, Control};
use crate::output::{OutputFormat, PartialArchive};
use details::{Detail, Details, Kind, Marking, Sign};
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
use profile::{Class, Level, Profile, profile, profile_in};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;
use structures::{Group, Index, Obstacle, Portal};
use things::{Fence, Kept, Mapped, Things};

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
/// And paths' (about 0.5 m²).
const MIN_PATH_AREA: u64 = 54;
/// Metres across steps' treads.
const TREADS: [f64; 5] = [1.0, 1.5, 2.0, 3.0, 5.0];
/// Metres across a turning circle that are believed.
const TURNING: std::ops::RangeInclusive<f64> = 6.0..=40.0;
/// Ground within a road, of under so many square metres, or so many and as narrow on
/// average, that is road.
const OPEN: f64 = 3.0;
const NARROW: (f64, f64) = (15.0, 0.5);
const GZIP_LEVEL: u32 = 6;

/// What [`write_hd_roads`] wrote.
#[derive(Debug, Clone, PartialEq, Default)]
#[non_exhaustive]
pub struct HdRoadsReport {
    /// Ways drawn, but for paths, and those.
    pub ways: u64,
    pub paths: u64,
    /// Ways left out because a node of theirs is not in the extract.
    pub incomplete_ways: u64,
    /// Where three or more arms of roads meet, driveways apart: nodes that are one
    /// junction as one, and how many of them are of more than one.
    pub junctions: u64,
    pub clusters: u64,
    /// Distinct cross-sections.
    pub profiles: u64,
    pub tiles: u64,
    pub surfaces: u64,
    pub markings: u64,
    /// Bridges that join or cross, as one; their pieces, and what they cross.
    pub groups: u64,
    pub pieces: u64,
    pub crossings: u64,
    /// Crossings, lines traffic stops at, lanes' arrows and whatever else is painted
    /// but lanes' lines.
    pub details: u64,
    /// Parking, parking spaces, platforms, traffic islands, crossings' islands,
    /// carriageways and squares.
    pub areas: u64,
    /// Those of them that are crossings' islands, and carriageways.
    pub refuges: u64,
    pub carriageways: u64,
    /// Discs at roads' ends where one turns round.
    pub turning_circles: u64,
    /// Roads narrowed to the paths mapped beside them.
    pub fitted: u64,
    /// Stretches of barriers.
    pub barriers: u64,
    /// Railways' and trams' tracks that are drawn.
    pub tracks: u64,
    /// Stretches of fences, hedges and walls, and lamps, signs and other things
    /// that stand beside roads.
    pub fences: u64,
    pub things: u64,
    /// Pitches, and those whose lines are drawn.
    pub pitches: u64,
    pub lined: u64,
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
    let mut source = read(pbf, temp, threads, control)?;
    let read_seconds = start.elapsed().as_secs_f64();
    let profiles = distinct_profiles(&source.ways);
    lanes::place(&mut source.ways);
    let fitted = sides::fit(&mut source.ways[..source.roads]);
    sides::separate(&mut source.ways[..source.roads]);
    let Source {
        ways: all,
        roads,
        tracks,
        carried,
        obstacles,
        areas,
        barriers,
        fences,
        mapped,
        incomplete_ways,
        ..
    } = &source;
    let ways = &all[..*roads];
    let signal = |node| matches!(source.signs.get(&node), Some(Sign::Line(Kind::Signal, _)));
    let mut network = network::build_at(ways, &signal);
    let given = junctions::Given::new(areas);
    network.shapes.given = given.stretches(ways, &network.shapes);
    // settled with the rest of what is painted
    let beside = sides::painted(ways, &network, &source.calming);
    network.paint.extend(beside);
    let mut details = details::build(
        ways,
        &network,
        &source.signs,
        &source.crossings,
        &source.islands,
        &source.fast,
    );
    // a corner of what is mapped is there as mapped
    network.patches.retain(|patch| {
        let sum = (patch.ring.iter()).fold([0.0; 2], |s, p| [s[0] + p[0], s[1] + p[1]]);
        let count = patch.ring.len() as f64;
        patch.level != Level::Ground || !given.holds([sum[0] / count, sum[1] / count])
    });
    let turning = junctions::turning(ways, &source.turning);
    let turning_circles = turning.len() as u64;
    network.patches.extend(turning);
    // nor is a crossing's island that is mapped
    let mut refuges = std::mem::take(&mut details.refuges);
    let islands = furniture::Islands::new(areas);
    refuges.retain(|r| !islands.near(&r.ring));
    let refuge_count = refuges.len() as u64;
    let tables = sides::tables(ways, &network, &source.calming);
    let areas: &[Area] = &[&areas[..], &refuges, &tables].concat();
    let tracks = rails::build(all, *roads, tracks, *carried);
    // with the tracks that have bridges of their own
    let ways_and_tracks = &all[..all.len() - carried];
    let groups = structures::build(ways_and_tracks, obstacles);
    let gaps = paint::piece_gaps(all, &network, &details, &groups);
    let index = Index::new(&groups, all, structure_tile);
    let painted: Vec<((u32, u32), &Detail)> = details
        .list
        .iter()
        .filter_map(|d| Some((index.on(all, d.way, d.at)?, d)))
        .collect();
    let rails = tracks.painted(all, &index);
    let portals = structures::portals(ways_and_tracks);
    let stretches = furniture::stretches(all, &index, barriers, structure_tile);
    let things = things::build(ways, all, &index, &details, mapped, fences);
    let grounds = grounds::build(&source.grounds, areas, ways);
    let scene = Scene {
        ways,
        network: &network,
        details: &details,
        areas,
        tracks: &tracks,
        scales: all,
        ends: path_ends(ways),
        things: &things,
        grounds: &grounds,
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
    let paths = ways.iter().filter(|w| w.profile.class.is_path()).count() as u64;
    let mut report = HdRoadsReport {
        ways: ways.len() as u64 - paths,
        paths,
        incomplete_ways: *incomplete_ways,
        junctions: network.junctions,
        clusters: network.clusters,
        refuges: refuge_count,
        carriageways: (areas.iter().filter(|a| a.kind == AreaKind::Road)).count() as u64,
        turning_circles,
        fitted,
        profiles,
        details: details.list.len() as u64,
        areas: areas.len() as u64,
        barriers: stretches.len() as u64,
        tracks: (all.len() - roads) as u64,
        pitches: grounds.pitches,
        lined: grounds.lined,
        read_seconds,
        ..Default::default()
    };
    let mut written = Vec::with_capacity(encoded.len());
    let mut beside: BTreeMap<(u32, u32), Kept> = BTreeMap::new();
    for (&(x, y), mut tile) in tiles.keys().zip(encoded) {
        for (i, p) in std::mem::take(&mut tile.kept.points) {
            let of = |v: f64| (v / f64::from(ROAD_EXTENT)) as u32;
            let kept = beside.entry((of(p[0]), of(p[1]))).or_default();
            kept.points.push((i, p));
        }
        if !tile.kept.lines.is_empty() {
            let kept = beside.entry((x / 2, y / 2)).or_default();
            kept.lines.append(&mut tile.kept.lines);
        }
        if tile.bytes.is_empty() {
            continue;
        }
        report.surfaces += tile.surfaces;
        report.markings += tile.markings;
        report.raw_bytes += tile.raw_bytes;
        report.gz_bytes += tile.bytes.len() as u64;
        written.push((tile.id, tile.bytes));
    }
    let standing = Standing {
        groups: &groups,
        gaps: &gaps,
        painted: &painted,
        rails: &rails,
        barriers: &stretches,
        portals: &portals,
    };
    for (id, raw, bytes) in structure_tiles(all, &network, &standing)? {
        report.raw_bytes += raw;
        report.gz_bytes += bytes.len() as u64;
        written.push((id, bytes));
    }
    let stood = thing_layers(&things, beside, &groups)?;
    for (_, fences, things) in stood.values() {
        report.fences += fences;
        report.things += things;
    }
    for (id, raw, bytes) in road_tiles(ways, &network, &stood, control)? {
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
        let profile = Profile {
            speed: 0.0,
            named: [0; 2],
            ..way.profile.clone()
        };
        let profile = format!("{profile:?}");
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
    obstacles: Read<structures::Kind>,
    crossings: Read<Marking>,
    /// Roads whose arrows are longer.
    fast: FxHashSet<i64>,
    /// Whether paved, too.
    areas: Read<(AreaKind, bool)>,
    barriers: Read<Barrier>,
    fences: Read<Fence>,
    tracks: Read<rails::Track>,
    grounds: Read<grounds::Ground>,
    /// Platforms that are lines: metres across, and whether paved.
    platforms: Read<(f64, bool)>,
    /// The nodes of tables that are lines, across roads.
    tables: Vec<i64>,
    /// The ways that outline platforms, squares and countries.
    outlines: Arc<FxHashSet<i64>>,
    outlined: FxHashMap<i64, Vec<i64>>,
    /// Whether a country of the extract keeps left, and if so the roads'
    /// profiles there.
    left_hand: bool,
    left: FxHashMap<i64, Profile>,
}

/// What is read of the extract, each by id.
struct Source {
    /// The roads, how many they are, and the tracks, the last `carried` of which
    /// are on roads' bridges.
    ways: Vec<Way>,
    roads: usize,
    tracks: Vec<rails::Track>,
    carried: usize,
    /// What lies near bridges.
    obstacles: Vec<Obstacle>,
    areas: Vec<Area>,
    /// Points, world units a metre, and which.
    barriers: Vec<(Vec<Point>, f64, Barrier)>,
    fences: Vec<(Vec<Point>, f64, Fence)>,
    grounds: Vec<grounds::Mapped>,
    /// Nodes, where they are, world units a metre, and what stands there.
    mapped: Vec<(i64, Point, f64, Mapped)>,
    /// By node.
    signs: FxHashMap<i64, Sign>,
    crossings: FxHashMap<i64, Marking>,
    /// The nodes of crossings with an island.
    islands: FxHashSet<i64>,
    /// Those roads' ends are discs about, with the metres across if tagged.
    turning: FxHashMap<i64, Option<f64>>,
    /// What slows traffic, by node.
    calming: FxHashMap<i64, sides::Calming>,
    /// By way.
    fast: FxHashSet<i64>,
    /// Roads that lack a node.
    incomplete_ways: u64,
}

/// What a way that is no road of the archive is to a bridge over it.
fn obstacle<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<structures::Kind> {
    use structures::Kind;
    if get("tunnel").is_some_and(|v| v != "no") || get("bridge").is_some_and(|v| v != "no") {
        return None;
    }
    match (get("railway"), get("waterway"), get("highway")) {
        (Some("tram"), ..) => Some(Kind::Tram),
        (Some("rail" | "light_rail" | "subway" | "narrow_gauge"), ..) => Some(Kind::Rail),
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
            if self.outlines.contains(&way.way_id) {
                self.outlined.insert(way.way_id, nodes.clone());
            }
            if let Some(track) = rails::track(get) {
                self.tracks.push((way.way_id, nodes.clone(), track));
            }
            if nodes[0] != nodes[nodes.len() - 1]
                && let Some(platform) = furniture::platform_line(get)
            {
                self.platforms.push((way.way_id, nodes.clone(), platform));
            }
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
            if let Some(fence) = things::fence(get) {
                self.fences.push((way.way_id, nodes.clone(), fence));
            }
            let closed = nodes[0] == nodes[nodes.len() - 1];
            if get("traffic_calming") == Some("table") && !closed {
                self.tables.extend(&nodes);
            }
            if let Some(ground) = grounds::ground(get, closed) {
                self.grounds.push((way.way_id, nodes.clone(), ground));
            }
            if closed && let Some(area) = furniture::area(get) {
                self.areas.push((way.way_id, nodes.clone(), area));
            }
            if let Some(profile) = profile(get) {
                if details::fast(get) {
                    self.fast.insert(way.way_id);
                }
                if self.left_hand
                    && let Some(left) = profile_in(get, &country::KEEPS_LEFT)
                {
                    self.left.insert(way.way_id, left);
                }
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
                    Some(
                        "crossing"
                            | "stop"
                            | "give_way"
                            | "traffic_signals"
                            | "mini_roundabout"
                            | "turning_circle"
                    )
                ) || things::mapped(|key| tags.get(key)).is_some()
                    || sides::calming(|key| tags.get(key)).is_some()
            }),
        |_| {},
        || control.is_cancelled(),
    )?;
    control.check()?;
    let multipolygons: Vec<furniture::Multipolygon> = index
        .supported_relations
        .iter()
        .filter_map(furniture::multipolygon)
        .collect();
    let ground_polygons: Vec<_> = (index.supported_relations.iter())
        .filter_map(grounds::multipolygon)
        .collect();
    let boundaries: Vec<country::Boundary> = (index.supported_relations.iter())
        .filter_map(country::boundary)
        .collect();
    let outlines: Arc<FxHashSet<i64>> = Arc::new(
        (multipolygons.iter().flat_map(|p| &p.2))
            .chain(ground_polygons.iter().flat_map(|p| &p.1))
            .map(|w| w.0)
            .chain(boundaries.iter().flat_map(|b| b.ways.iter().copied()))
            .collect(),
    );
    let mut roads = Roads {
        outlines: outlines.clone(),
        left_hand: (boundaries.iter()).any(|b| country::rules(&b.code).keeps_left()),
        ..Default::default()
    };
    run_pass2_with_options(
        pbf,
        profile,
        Pass2Options::new()
            .worker_threads(threads)
            .skip_nodes()
            .skip_relations()
            .keep_ways_where(Arc::new(move |way| {
                let tags = &way.tags;
                let keys = ["highway", "railway", "waterway", "area:highway"];
                keys.iter().any(|key| tags.contains_key(key))
                    || tags.contains_key("traffic_calming")
                    || furniture::barrier(|key| tags.get(key)).is_some()
                    || things::fence(|key| tags.get(key)).is_some()
                    || matches!(tags.get("amenity"), Some("parking" | "parking_space"))
                    || grounds::ground(|key| tags.get(key), true).is_some()
                    || tags.get("public_transport") == Some("platform")
                    || outlines.contains(&way.way_id)
            })),
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
    let mut mapped: Vec<(i64, Point, f64, Mapped)> = index
        .tagged_nodes
        .iter()
        .filter_map(|node| {
            let mapped = things::mapped(|key| node.tags.get(key))?;
            let (lat, lng) = (node.lat, node.lng);
            Some((node.node_id, world(lat, lng), units(lat), mapped))
        })
        .collect();
    mapped.sort_by_key(|m| m.0);
    mapped.dedup_by_key(|m| m.0);
    let tagged = |key: &str, value: &str| {
        let nodes = index.tagged_nodes.iter();
        let (key, value) = (key.to_owned(), value.to_owned());
        nodes.filter(move |node| node.tags.get(&key) == Some(&value))
    };
    let islands: FxHashSet<i64> = tagged("crossing:island", "yes")
        .map(|n| n.node_id)
        .collect();
    let turning: FxHashMap<i64, Option<f64>> = tagged("highway", "turning_circle")
        .map(|node| {
            let across = node.tags.get("diameter").and_then(profile::metres);
            let across = across.filter(|d| TURNING.contains(d));
            (node.node_id, across)
        })
        .collect();
    let mut calming: FxHashMap<i64, sides::Calming> = (roads.tables.iter())
        .map(|&node| (node, sides::Calming::Table))
        .collect();
    calming.extend(
        index
            .tagged_nodes
            .iter()
            .filter_map(|node| Some((node.node_id, sides::calming(|key| node.tags.get(key))?))),
    );
    let mut crossings: FxHashMap<i64, Marking> = FxHashMap::default();
    for (_, nodes, marking) in by_id(roads.crossings) {
        for node in nodes {
            crossings.entry(node).or_insert(marking);
        }
    }
    let found = by_id(roads.roads);
    let mut nodes = index.node_store.cursor();
    let countries = {
        let mut cursor = index.node_store.cursor();
        country::countries(&boundaries, &roads.outlined, |ids| {
            ids.iter()
                .map(|&id| cursor.get(id).map(|(lat, lng)| world(lat, lng)))
                .collect()
        })
    };
    let mut ways = Vec::with_capacity(found.len());
    let mut incomplete = 0;
    let mut locate = |ids: &[i64]| -> Option<(Vec<Point>, f64)> {
        let located: Vec<(f64, f64)> =
            ids.iter().map(|&id| nodes.get(id)).collect::<Option<_>>()?;
        let latitude = located.iter().map(|p| p.0).sum::<f64>() / located.len() as f64;
        let scale = units(latitude);
        Some((
            located.iter().map(|&(lat, lng)| world(lat, lng)).collect(),
            scale,
        ))
    };
    for (id, nodes, mut profile) in found {
        match locate(&nodes) {
            Some((points, scale)) => {
                // as its country keeps, where its middle is
                let rules = countries.at(points[points.len() / 2]);
                if rules.keeps_left()
                    && let Some(left) = roads.left.remove(&id)
                {
                    profile = left;
                }
                profile.rules = rules;
                ways.push(Way {
                    id,
                    nodes,
                    points,
                    profile,
                    scale,
                })
            }
            None => incomplete += 1,
        }
    }
    let drawn = ways.len();
    let mut tracks = Vec::new();
    for (id, nodes, track) in by_id(roads.tracks) {
        if let Some((points, scale)) = locate(&nodes) {
            let profile = track.profile();
            let way = Way {
                id,
                nodes,
                points,
                profile,
                scale,
            };
            tracks.push((way, track));
        }
    }
    let carried = rails::on_roads_bridges(&ways, &mut tracks);
    let mut of = carried.iter();
    let (last, own): (Vec<_>, Vec<_>) = tracks
        .into_iter()
        .partition(|_| *of.next().expect("one a track"));
    let carried = last.len();
    let (rails, tracks): (Vec<Way>, Vec<_>) = own.into_iter().chain(last).unzip();
    ways.extend(rails);
    let mut areas: Vec<Area> = by_id(roads.areas)
        .into_iter()
        .filter_map(|(_, nodes, (kind, paved))| {
            let ring = furniture::ring(&locate(&nodes)?.0)?;
            let holes = Vec::new();
            Some(Area {
                kind,
                paved,
                ring,
                holes,
            })
        })
        .collect();
    for (_, nodes, (width, paved)) in by_id(roads.platforms) {
        if let Some((points, scale)) = locate(&nodes) {
            areas.extend(furniture::along(&points, width * scale, paved));
        }
    }
    for (kind, paved, members) in multipolygons {
        let mut rings = [false, true].map(|outer| {
            let of = members.iter().filter(|m| m.1 == outer);
            let lines = of.filter_map(|m| roads.outlined.get(&m.0).cloned());
            let rings = furniture::joined(lines.collect()).into_iter();
            rings
                .filter_map(|nodes| furniture::ring(&locate(&nodes)?.0))
                .collect::<Vec<_>>()
        });
        let outer = std::mem::take(&mut rings[1]);
        let [mut holes, _] = rings;
        holes.iter_mut().for_each(|ring| ring.reverse());
        for ring in outer {
            // the first takes them all, as the surfaces are joined
            let holes = std::mem::take(&mut holes);
            areas.push(Area {
                kind,
                paved,
                ring,
                holes,
            });
        }
    }
    let grounds = grounds::located(
        by_id(roads.grounds),
        ground_polygons,
        &roads.outlined,
        &mut locate,
    );
    let barriers = by_id(roads.barriers)
        .into_iter()
        .filter_map(|(_, nodes, barrier)| {
            let (points, scale) = locate(&nodes)?;
            Some((points, scale, barrier))
        })
        .collect();
    let fences = by_id(roads.fences)
        .into_iter()
        .filter_map(|(_, nodes, fence)| {
            let (points, scale) = locate(&nodes)?;
            Some((points, scale, fence))
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
        roads: drawn,
        tracks,
        carried,
        obstacles,
        areas,
        barriers,
        fences,
        grounds,
        mapped,
        signs,
        crossings,
        islands,
        turning,
        calming,
        fast: roads.fast,
        incomplete_ways: incomplete,
    })
}

/// Whether a way's line at `edge` is yellow where it is: the line between its
/// directions, an edge, or a one-way carriageway's edge on its traffic's inner side.
fn yellow(way: &Way, edge: usize, line: profile::Line) -> bool {
    use country::Colour::Yellow;
    let rules = way.profile.rules;
    let Some(lanes) = &way.profile.lanes else {
        return false;
    };
    let all = (lanes.backward + lanes.forward) as usize;
    let inner = match (lanes.along, lanes.against) {
        (true, false) => Some(if lanes.left_hand { all } else { 0 }),
        (false, true) => Some(if lanes.left_hand { 0 } else { all }),
        _ => None,
    };
    match line {
        profile::Line::Centre | profile::Line::CentreSolid => rules.centre == Yellow,
        profile::Line::Edge if inner == Some(edge) => rules.inner == Yellow,
        profile::Line::Edge => rules.edge == Yellow,
        _ => false,
    }
}

/// World units a metre at a latitude.
fn units(lat: f64) -> f64 {
    WORLD / (EQUATOR_M * lat.to_radians().cos().max(0.05))
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
    tracks: &'a rails::Tracks<'a>,
    /// The ways the tracks number, the roads and theirs, for their scales.
    scales: &'a [Way],
    /// Metres each way that is a path runs on past its start and its end.
    ends: Vec<[f64; 2]>,
    things: &'a Things,
    grounds: &'a grounds::Grounds,
}

#[derive(Default)]
struct Content {
    ways: Vec<u32>,
    runs: Vec<u32>,
    patches: Vec<u32>,
    details: Vec<u32>,
    areas: Vec<u32>,
    strips: Vec<u32>,
    fences: Vec<u32>,
    things: Vec<u32>,
    grounds: Vec<u32>,
    painted: Vec<u32>,
}

/// Metres each path runs on past its ends, by way: where it goes on as another
/// as far as their outer edges meet, half their width at most.
fn path_ends(ways: &[Way]) -> Vec<[f64; 2]> {
    // at each end's node: the directions paths leave it in, and whether one passes
    let mut at: FxHashMap<i64, (Vec<Point>, bool)> = FxHashMap::default();
    let paths = || (ways.iter().enumerate()).filter(|(_, w)| w.profile.class.is_path());
    let out = |way: &Way, end: usize| -> Point {
        let last = way.points.len() - 1;
        let [a, b] = [[0, 1], [last, last - 1]][end].map(|i| way.points[i]);
        let length = network::distance(a, b).max(f64::MIN_POSITIVE);
        [(b[0] - a[0]) / length, (b[1] - a[1]) / length]
    };
    let nodes = |way: &Way| [way.nodes[0], way.nodes[way.nodes.len() - 1]];
    for (_, way) in paths() {
        for (end, node) in nodes(way).into_iter().enumerate() {
            at.entry(node).or_default().0.push(out(way, end));
        }
    }
    for (_, way) in paths() {
        for node in &way.nodes[1..way.nodes.len() - 1] {
            if let Some(found) = at.get_mut(node) {
                found.1 = true;
            }
        }
    }
    let mut ends = vec![[OVERLAP; 2]; ways.len()];
    for (index, way) in paths() {
        for (end, node) in nodes(way).into_iter().enumerate() {
            let half = way.profile.width / 2.0;
            let by = match &at[&node] {
                // onto one that passes it is within it
                (_, true) => 0.0,
                (found, _) if found.len() == 2 => {
                    let dot = found[0][0] * found[1][0] + found[0][1] * found[1][1];
                    half * ((1.0 + dot).max(0.0) / (1.0 - dot).max(1e-9))
                        .sqrt()
                        .min(1.0)
                }
                (found, _) if found.len() > 2 => half / 2.0,
                _ => 0.0,
            };
            ends[index][end] = by.max(OVERLAP);
        }
    }
    ends
}

/// What reaches each tile, in order.
fn tile_contents(scene: &Scene) -> BTreeMap<(u32, u32), Content> {
    let Scene { ways, network, .. } = scene;
    let mut tiles: BTreeMap<(u32, u32), Content> = BTreeMap::new();
    for (i, way) in ways.iter().enumerate() {
        let reach = way_reach(way, network.shapes.reach(ways, i as u32));
        for tile in touched(&way.points, reach) {
            tiles.entry(tile).or_default().ways.push(i as u32);
        }
    }
    for (i, run) in network.runs.iter().enumerate() {
        let way = &ways[run.way as usize];
        let p = &way.profile;
        if p.lines.is_empty() && p.parking == [0.0; 2] && !p.has_sides() {
            continue;
        }
        let reach = way_reach(way, network.shapes.reach(ways, run.way));
        for tile in touched(&way.points[run.points.clone()], reach) {
            tiles.entry(tile).or_default().runs.push(i as u32);
        }
    }
    for (i, detail) in scene.details.list.iter().enumerate() {
        // past its strokes an arrow's head
        let reach = (detail.kind.width() + 2.0) * ways[detail.way as usize].scale + BUFFER;
        // a disc has no strokes
        let points = match detail.strokes.is_empty() {
            true => detail.heads.concat(),
            false => detail.strokes.concat(),
        };
        for tile in touched(&points, reach) {
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
    for (i, strip) in scene.tracks.strips.iter().enumerate() {
        let reach = strip.reach() * scene.scales[strip.way as usize].scale + BUFFER;
        for tile in touched(&strip.points, reach) {
            tiles.entry(tile).or_default().strips.push(i as u32);
        }
    }
    for (i, patch) in network.patches.iter().enumerate() {
        for tile in touched(&patch.ring, BUFFER) {
            tiles.entry(tile).or_default().patches.push(i as u32);
        }
    }
    for (i, line) in scene.things.lines.iter().enumerate() {
        for tile in touched(&line.points, 0.0) {
            tiles.entry(tile).or_default().fences.push(i as u32);
        }
    }
    for (i, surface) in scene.grounds.surfaces.iter().enumerate() {
        for tile in touched(&surface.corners, BUFFER) {
            tiles.entry(tile).or_default().grounds.push(i as u32);
        }
    }
    for (i, line) in scene.grounds.lines.iter().enumerate() {
        for tile in touched(&line.points, BUFFER) {
            tiles.entry(tile).or_default().painted.push(i as u32);
        }
    }
    for (i, thing) in scene.things.points.iter().enumerate() {
        let of = |v: f64| (v / EXTENT) as u32;
        let tile = (of(thing.at[0][0]), of(thing.at[0][1]));
        tiles.entry(tile).or_default().things.push(i as u32);
    }
    tiles
}

/// World units from a way's line that its surface, `surface` metres out, what
/// is beside it and a tile's buffer reach.
fn way_reach(way: &Way, surface: f64) -> f64 {
    (surface + way.profile.beside()) * way.scale + BUFFER
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
    /// None where nothing is drawn on the ground.
    bytes: Vec<u8>,
    raw_bytes: u64,
    surfaces: u64,
    markings: u64,
    kept: Kept,
}

/// What is joined into one surface: its level, kind, whether paved, and with
/// what if a style may tell.
type SurfaceKey = (Level, &'static str, bool, Option<&'static str>);

fn kind(class: Class) -> &'static str {
    match class {
        Class::Sidewalk => "sidewalk",
        Class::Cycleway => "cycleway",
        Class::Footway => "footway",
        Class::Path | Class::Bridleway => "path",
        Class::Pedestrian => "pedestrian",
        Class::Steps => "steps",
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

fn encode(scene: &Scene, (x, y): (u32, u32), content: &Content) -> Result<Encoded, String> {
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
    let mut paths: BTreeMap<SurfaceKey, Vec<Vec<Vec<Point>>>> = BTreeMap::new();
    let mut squares: Vec<(SurfaceKey, Vec<Vec<Point>>)> = Vec::new();
    for &index in &content.ways {
        let way = &ways[index as usize];
        let p = &way.profile;
        // what is not mapped as an area, of one that is in part
        let given = network.shapes.given.get(&index);
        let (mut shapes, taken) = match given {
            Some(given) => {
                let left = junctions::Stretch::left(given, way).into_iter();
                let strip = |span| (network.shapes).strip(ways, index, span, OVERLAP, rect);
                (left.flat_map(strip).collect(), [0.0; 2])
            }
            None => network.shapes.tapers(ways, index, OVERLAP, rect),
        };
        let reach = (p.width / 2.0 + p.centre.abs()) * way.scale;
        let wide: Rect = [
            rect[0] - reach,
            rect[1] - reach,
            rect[2] + reach,
            rect[3] + reach,
        ];
        // between its tapers, if anything is
        let constant = match given {
            Some(_) => None,
            None if taken == [0.0; 2] => Some(way.points.clone()),
            None => geometry::trim(&way.points, taken.map(|metres| metres * way.scale)),
        };
        let (of, overlap) = if p.class.is_path() {
            (&mut paths, scene.ends[index as usize])
        } else {
            (&mut groups, [OVERLAP; 2])
        };
        if let Some(points) = constant {
            let middle = geometry::offset(&points, p.centre * way.scale);
            let middle = geometry::extend_ends(&middle, overlap.map(|m| m * way.scale));
            for run in geometry::near(&middle, wide) {
                shapes.extend(geometry::ribbon(&run, p.width * way.scale));
            }
        }
        of.entry((p.level, kind(p.class), p.paved, p.paving))
            .or_default()
            .extend(shapes);
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
                if across <= 0.0 {
                    continue;
                }
                let parking = (groups.entry((Level::Ground, "parking", true, None))).or_default();
                if network.shapes.tapered(run.way).is_some() {
                    let out = side * across;
                    let trim = span.map(|units| units / way.scale);
                    parking.extend(network.beside(ways, index as usize, trim, side, out));
                } else {
                    let at = p.centre + side * (p.width / 2.0 + across / 2.0);
                    parking.extend(geometry::ribbon(
                        &geometry::offset(&middle, at * way.scale),
                        across * way.scale,
                    ));
                }
            }
        }
    }
    for &index in &content.areas {
        let area = &scene.areas[index as usize];
        let key = (Level::Ground, area.kind.surface(), area.paved, None);
        let mut shape = vec![area.ring.clone()];
        shape.extend(area.holes.iter().cloned());
        if area.kind.is_path() {
            squares.push((key, shape.clone()));
        }
        groups.entry(key).or_default().push(shape);
    }
    for &index in &content.strips {
        let strip = &scene.tracks.strips[index as usize];
        let Some(bed) = strip.bed else { continue };
        let scale = scene.scales[strip.way as usize].scale;
        let reach = strip.reach() * scale;
        let wide: Rect = [
            rect[0] - reach,
            rect[1] - reach,
            rect[2] + reach,
            rect[3] + reach,
        ];
        let shapes = (groups.entry((strip.level, bed.kind(), false, None))).or_default();
        let middle = geometry::extend(&strip.points, OVERLAP * scale);
        for run in geometry::near(&middle, wide) {
            shapes.extend(geometry::ribbon(&run, bed.width(strip.gauge) * scale));
            let between = scene.tracks.between(strip, &run);
            shapes.extend(between.into_iter().map(|ring| vec![ring]));
        }
    }
    for &index in &content.patches {
        let patch = &network.patches[index as usize];
        groups
            .entry((patch.level, kind(Class::Primary), patch.paved, None))
            .or_default()
            .push(vec![patch.ring.clone()]);
    }
    for &index in &content.runs {
        let gaps = &scene.details.gaps[index as usize];
        for (kind, within, shapes) in sides::strips(ways, network, index as usize, gaps, rect) {
            let of = if within { &mut groups } else { &mut paths };
            of.entry((Level::Ground, kind, true, None))
                .or_default()
                .extend(shapes);
        }
    }
    for (key, ribbons) in paths {
        let (level, name, ..) = key;
        let roads = (groups.iter()).filter(|(k, _)| (k.0, k.1) == (level, "road"));
        let mut taken: Vec<Vec<Vec<Point>>> = roads.flat_map(|r| r.1).cloned().collect();
        if level == Level::Ground && name != "steps" {
            let others = squares.iter().filter(|s| s.0 != key);
            taken.extend(others.map(|s| s.1.clone()));
        }
        let left = geometry::without(&ribbons, &taken);
        groups.entry(key).or_default().extend(left);
    }
    let mut surfaces = MvtLayerBuilder::new("surface").map_err(failed)?;
    let mut surface_count = 0;
    let mut carriageways = Vec::new();
    let scale = content
        .ways
        .first()
        .map_or(1.0, |&w| ways[w as usize].scale);
    for ((level, kind, paved, paving), ribbons) in groups {
        let mut shapes = geometry::union(&ribbons);
        if (level, kind) == (Level::Ground, "road") {
            geometry::close(&mut shapes, scale, OPEN, NARROW);
        }
        let (name, layer) = level_name(level);
        // a road leaves specks of a path along its edge
        let speck = match kind {
            "footway" | "path" | "pedestrian" | "steps" => MIN_PATH_AREA,
            _ => MIN_AREA,
        };
        for polygon in geometry::to_tile(&shapes, rect, origin, speck) {
            let mut feature = surfaces.feature_of(MvtGeomType::Polygon);
            feature.tag_string("kind", kind).map_err(failed)?;
            feature.tag_string("level", name).map_err(failed)?;
            feature
                .tag_sint("layer", i64::from(layer))
                .map_err(failed)?;
            feature.tag_bool("paved", paved).map_err(failed)?;
            if let Some(paving) = paving {
                feature.tag_string("paved_by", paving).map_err(failed)?;
            }
            for (i, ring) in polygon.iter().enumerate() {
                let points = ring.iter().map(|p| MvtCoord { x: p[0], y: p[1] });
                feature.ring(points, i == 0).map_err(failed)?;
            }
            surfaces = feature.end();
            surface_count += 1;
        }
        if (level, kind) == (Level::Ground, "road") {
            carriageways.push(shapes);
        }
    }
    let grounds = scene.grounds;
    let (layer, count) =
        (grounds.draw_surfaces(&content.grounds, rect, origin, surfaces)).map_err(failed)?;
    (surfaces, surface_count) = (layer, surface_count + count);
    let tile: Rect = [origin[0], origin[1], origin[0] + EXTENT, origin[1] + EXTENT];
    let things = scene.things;
    let kept = things.kept(&content.things, &content.fences, tile, &carriageways);

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
        let rules = way.profile.rules;
        let pattern = rules.pattern();
        for span in spans(run, &scene.details.gaps[index as usize], way) {
            for &(at, line) in &way.profile.lines {
                let edge = lanes::edge(&way.profile, at);
                let yellow = yellow(way, edge, line);
                let trim = span.map(|units| units / way.scale);
                let Some(drawn) = network.line(ways, index as usize, edge, trim) else {
                    continue;
                };
                // less what else is painted there
                let parts: Vec<Vec<Point>> = if line.dashed() {
                    let (points, lengths, phase) =
                        network.dashes.laid(index, edge, drawn, pattern[1]);
                    let kept = scene
                        .details
                        .taken
                        .kept(ways, run.way, &points, MIN_MARKING);
                    (kept.into_iter())
                        .filter_map(|[from, to]| {
                            let whole = (lengths[lengths.len() - 1] - lengths[0]) * way.scale;
                            let points = geometry::trim(&points, [from, whole - to])?;
                            let gone = from / way.scale;
                            let lengths = geometry::lengths(&points, way.scale, lengths[0] + gone);
                            let phase = (phase + gone) % pattern[1];
                            Some(geometry::clip_dashed(
                                &points, &lengths, rect, phase, pattern,
                            ))
                        })
                        .flatten()
                        .collect()
                } else {
                    let left = scene
                        .details
                        .taken
                        .cut(ways, run.way, &drawn.points, MIN_MARKING);
                    left.iter().flat_map(|l| geometry::clip(l, rect)).collect()
                };
                for part in parts {
                    let mut points: Vec<MvtCoord> = part.iter().map(local).collect();
                    points.dedup();
                    if points.len() < 2 {
                        continue;
                    }
                    let mut feature = markings.feature_of(MvtGeomType::LineString);
                    feature.tag_string("kind", line.kind()).map_err(failed)?;
                    feature.tag_bool("dashed", line.dashed()).map_err(failed)?;
                    if yellow {
                        feature.tag_string("colour", "yellow").map_err(failed)?;
                    }
                    if let Some(dash) = rules.dash_name().filter(|_| line.dashed()) {
                        feature.tag_string("dash", dash).map_err(failed)?;
                    }
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
    for &index in &content.runs {
        let gaps = &scene.details.gaps[index as usize];
        for (kind, dashed, part) in sides::lines(ways, network, index as usize, gaps, rect) {
            let mut points: Vec<MvtCoord> = part.iter().map(local).collect();
            points.dedup();
            if points.len() < 2 {
                continue;
            }
            let mut feature = markings.feature_of(MvtGeomType::LineString);
            feature.tag_string("kind", kind).map_err(failed)?;
            feature.tag_bool("dashed", dashed).map_err(failed)?;
            feature.tag_string("level", "ground").map_err(failed)?;
            feature.tag_sint("layer", 0).map_err(failed)?;
            feature.tag_uint("confidence", 2).map_err(failed)?;
            feature.line(points).map_err(failed)?;
            markings = feature.end();
            marking_count += 1;
        }
    }
    // whole in every tile they reach, so that their dashes go on over the edge
    for &index in &content.details {
        let detail = &scene.details.list[index as usize];
        let rules = ways[detail.way as usize].profile.rules;
        let (name, layer) = level_name(ways[detail.way as usize].profile.level);
        // the line of triangles is not painted
        let strokes = match detail.kind {
            Kind::Triangles => &[][..],
            _ => &detail.strokes[..],
        };
        let lines = strokes.iter().map(|s| (MvtGeomType::LineString, &s[..]));
        let heads = detail.heads.iter().map(|h| (MvtGeomType::Polygon, &h[..]));
        for (geometry, points) in lines.chain(heads) {
            let mut points: Vec<MvtCoord> = points.iter().map(local).collect();
            points.dedup();
            let head = geometry == MvtGeomType::Polygon;
            if points.len() < if head { 3 } else { 2 } {
                continue;
            }
            let kind = match detail.kind {
                Kind::Arrow if head => "arrow-head",
                kind => kind.name(),
            };
            let mut feature = markings.feature_of(geometry);
            feature.tag_string("kind", kind).map_err(failed)?;
            match detail.kind {
                // each bar is a line of its own
                Kind::Centre => {
                    feature.tag_bool("dashed", false).map_err(failed)?;
                }
                Kind::Signal | Kind::SignalDashed => {
                    let dashed = detail.kind == Kind::SignalDashed;
                    feature.tag_bool("dashed", dashed).map_err(failed)?;
                }
                Kind::Stop if rules.stop != country::DEFAULT.stop => {
                    feature.tag_double("width", rules.stop).map_err(failed)?;
                }
                _ => {}
            }
            if detail.kind == Kind::Centre && rules.centre == country::Colour::Yellow {
                feature.tag_string("colour", "yellow").map_err(failed)?;
            }
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
    for &index in &content.strips {
        let strip = &scene.tracks.strips[index as usize];
        let scale = scene.scales[strip.way as usize].scale;
        let (name, layer) = level_name(strip.level);
        // a sleeper is as usual: no tag says
        let confidence = [0, if strip.tagged { 2 } else { 0 }];
        let half = strip.gauge / 2.0;
        let sleepers = strip.bed.map(|_| ("sleeper", 0.0));
        for (kind, at) in sleepers
            .into_iter()
            .chain([("rail", -half), ("rail", half)])
        {
            let moved = geometry::offset(&strip.points, at * scale);
            for part in geometry::clip(&moved, rect) {
                let mut points: Vec<MvtCoord> = part.iter().map(local).collect();
                points.dedup();
                if points.len() < 2 {
                    continue;
                }
                let mut feature = markings.feature_of(MvtGeomType::LineString);
                feature.tag_string("kind", kind).map_err(failed)?;
                feature.tag_string("level", name).map_err(failed)?;
                feature
                    .tag_sint("layer", i64::from(layer))
                    .map_err(failed)?;
                feature
                    .tag_uint("confidence", confidence[usize::from(kind == "rail")])
                    .map_err(failed)?;
                feature
                    .tag_uint("gauge", (strip.gauge * 1000.0).round() as u64)
                    .map_err(failed)?;
                feature.line(points).map_err(failed)?;
                markings = feature.end();
                marking_count += 1;
            }
        }
    }
    for &index in &content.ways {
        let way = &ways[index as usize];
        let p = &way.profile;
        let Some(&width) = TREADS.iter().rfind(|w| **w <= p.width + 1e-6) else {
            continue;
        };
        if p.class != Class::Steps {
            continue;
        }
        for part in geometry::clip(&way.points, rect) {
            let mut points: Vec<MvtCoord> = part.iter().map(local).collect();
            points.dedup();
            if points.len() < 2 {
                continue;
            }
            let mut feature = markings.feature_of(MvtGeomType::LineString);
            feature.tag_string("kind", "step").map_err(failed)?;
            feature.tag_string("level", "ground").map_err(failed)?;
            feature.tag_sint("layer", 0).map_err(failed)?;
            feature.tag_uint("confidence", 2).map_err(failed)?;
            feature.tag_double("width", width).map_err(failed)?;
            feature.line(points).map_err(failed)?;
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
    let (layer, count) = grounds
        .draw_lines(&content.painted, rect, origin, &carriageways, markings)
        .map_err(failed)?;
    (markings, marking_count) = (layer, marking_count + count);

    let mut raw = Vec::new();
    for layer in [surfaces, markings] {
        if layer.num_features() > 0 {
            raw.extend_from_slice(&layer.encode());
        }
    }
    let bytes = if raw.is_empty() {
        Vec::new()
    } else {
        gzip(&raw)?
    };
    let coord = TileCoord::new(ZOOM, x, y).map_err(|e| e.to_string())?;
    Ok(Encoded {
        id: TileId::from(coord).value(),
        bytes,
        raw_bytes: raw.len() as u64,
        surfaces: surface_count,
        markings: marking_count,
        kept,
    })
}

fn gzip(raw: &[u8]) -> Result<Vec<u8>, String> {
    let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
    encoder
        .write_all(raw)
        .and_then(|()| encoder.finish())
        .map_err(|e| format!("Failed to gzip a tile: {e}"))
}

// The roads to be asked of, and what stands beside them.

/// The zoom of the tiles they are in, and its units across a tile.
const ROAD_ZOOM: u8 = 15;
const ROAD_EXTENT: u32 = 8192;

/// By tile, the layers of what stands in it, and how many fences and things.
type Stood = BTreeMap<(u32, u32), (Vec<u8>, u64, u64)>;

fn thing_layers(
    things: &Things,
    standing: BTreeMap<(u32, u32), Kept>,
    groups: &[Group],
) -> Result<Stood, String> {
    // the groups' numbers in their tiles, as `structure_tiles` counts them
    let mut counts: FxHashMap<(u32, u32), u64> = FxHashMap::default();
    let numbers: Vec<((u32, u32), u64)> = groups
        .iter()
        .map(|group| {
            let tile = structure_tile(group.pieces[0].points[0]);
            let count = counts.entry(tile).or_default();
            *count += 1;
            (tile, *count - 1)
        })
        .collect();
    let failed = |e: fast_mvt::MvtError| format!("Failed to encode a tile of things: {e}");
    let extent = fast_mvt::MvtExtent::new(ROAD_EXTENT).expect("an extent is not 0");
    let encode = |((x, y), mut kept): ((u32, u32), Kept)| {
        let across = f64::from(ROAD_EXTENT);
        let origin = [f64::from(x) * across, f64::from(y) * across];
        let coord = |p: &Point| MvtCoord {
            x: (p[0] - origin[0]).round() as i32,
            y: (p[1] - origin[1]).round() as i32,
        };
        let tenths = |metres: f64| (metres * 10.0).round() / 10.0;
        let mut lines = MvtLayerBuilder::new("fence").map_err(failed)?;
        lines.extent(extent);
        for (index, points) in things.laid(std::mem::take(&mut kept.lines)) {
            let mut points: Vec<MvtCoord> = points.iter().map(coord).collect();
            points.dedup();
            if points.len() < 2 {
                continue;
            }
            let fence = things.lines[index as usize].fence;
            let mut f = lines.feature_of(MvtGeomType::LineString);
            f.tag_string("kind", fence.kind).map_err(failed)?;
            f.tag_double("height", tenths(fence.height))
                .map_err(failed)?;
            f.line(points).map_err(failed)?;
            lines = f.end();
        }
        let mut points = MvtLayerBuilder::new("thing").map_err(failed)?;
        points.extent(extent);
        kept.points
            .sort_by_key(|(i, _)| (things.points[*i as usize].kind, *i));
        for (index, at) in &kept.points {
            let thing = &things.points[*index as usize];
            let mut f = points.feature_of(MvtGeomType::Point);
            f.tag_string("kind", thing.kind.name()).map_err(failed)?;
            if let Some(turn) = thing.turn {
                f.tag_uint("turn", u64::from(turn)).map_err(failed)?;
            }
            if let Some(size) = thing.size {
                f.tag_double("size", tenths(size)).map_err(failed)?;
            }
            if let Some((group, piece)) = thing.on {
                let ((x, y), number) = numbers[group as usize];
                f.tag_uint("x", u64::from(x)).map_err(failed)?;
                f.tag_uint("y", u64::from(y)).map_err(failed)?;
                f.tag_uint("group", number).map_err(failed)?;
                f.tag_uint("piece", u64::from(piece)).map_err(failed)?;
            }
            f.points([coord(at)]).map_err(failed)?;
            points = f.end();
        }
        let (fences, count) = (lines.num_features() as u64, points.num_features() as u64);
        let mut raw = Vec::new();
        for layer in [lines, points] {
            if layer.num_features() > 0 {
                raw.extend_from_slice(&layer.encode());
            }
        }
        Ok((!raw.is_empty()).then_some(((x, y), (raw, fences, count))))
    };
    let encoded = standing
        .into_par_iter()
        .map(encode)
        .collect::<Result<Vec<_>, String>>()?;
    Ok(encoded.into_iter().flatten().collect())
}

/// The tiles of the motor roads' ways, with the layers of what stood: id, bytes
/// before gzip, and gzipped.
fn road_tiles(
    ways: &[Way],
    network: &Network,
    stood: &Stood,
    control: &Control,
) -> Result<Vec<(u64, u64, Vec<u8>)>, String> {
    let across = f64::from(ROAD_EXTENT);
    let mut tiles: BTreeMap<(u32, u32), Vec<u32>> = BTreeMap::new();
    for (i, way) in ways.iter().enumerate() {
        // driveways and parking aisles are half the ways, and have no lanes to tell
        if way.profile.lanes.is_none() || way.profile.class == Class::Service {
            continue;
        }
        let reach = way_reach(way, network.shapes.reach(ways, i as u32));
        let mut of: Vec<(u32, u32)> = touched(&way.points, reach)
            .into_iter()
            .map(|(x, y)| (x / 2, y / 2))
            .collect();
        of.sort_unstable();
        of.dedup();
        for tile in of {
            tiles.entry(tile).or_default().push(i as u32);
        }
    }
    for &tile in stood.keys() {
        tiles.entry(tile).or_default();
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
            let reach = way_reach(way, network.shapes.reach(ways, index));
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
                    "{:.2},{:.2},{},{},{traffic}{}",
                    lanes.left,
                    lanes.width,
                    lanes.backward,
                    lanes.forward,
                    if lanes.left_hand { ",l" } else { "" }
                );
                f.tag_string("lanes", text).map_err(failed)?;
                f.line(points).map_err(failed)?;
                layer = f.end();
            }
        }
        let mut raw = Vec::new();
        if layer.num_features() > 0 {
            raw.extend_from_slice(&layer.encode());
        }
        if let Some((layers, ..)) = stood.get(&(x, y)) {
            raw.extend_from_slice(layers);
        }
        if raw.is_empty() {
            return Ok(None);
        }
        let bytes = gzip(&raw)?;
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
        Class::Footway => "footway",
        Class::Path => "path",
        Class::Pedestrian => "pedestrian",
        Class::Steps => "steps",
        Class::Bridleway => "bridleway",
        Class::Rail => "rail",
        Class::Tram => "tram",
    }
}

/// World units across a tile of structures, and the tile a point is in.
const STRUCTURE_ACROSS: f64 = EXTENT * (1_u32 << (ZOOM - STRUCTURE_ZOOM)) as f64;

fn structure_tile(p: Point) -> (u32, u32) {
    let of = |v: f64| (v / STRUCTURE_ACROSS) as u32;
    (of(p[0]), of(p[1]))
}

/// What is not on the ground: the groups of bridges, by piece what holds no
/// lines, what is painted and the rails on them; barriers and portals.
#[derive(Clone, Copy)]
struct Standing<'a> {
    groups: &'a [Group],
    gaps: &'a [Vec<Vec<[f64; 2]>>],
    painted: &'a [((u32, u32), &'a Detail)],
    rails: &'a [((u32, u32), Vec<Point>)],
    barriers: &'a [Stretch],
    portals: &'a [Portal],
}

/// The tiles of the groups, what is painted on their pieces and the barriers:
/// id, bytes before gzip, and gzipped.
fn structure_tiles(
    ways: &[Way],
    network: &Network,
    standing: &Standing,
) -> Result<Vec<(u64, u64, Vec<u8>)>, String> {
    let Standing {
        groups,
        gaps,
        painted,
        rails,
        barriers,
        portals,
    } = *standing;
    let across = STRUCTURE_ACROSS;
    let mut paint: BTreeMap<u32, Vec<(u32, &Detail)>> = BTreeMap::new();
    for &((group, piece), detail) in painted {
        paint.entry(group).or_default().push((piece, detail));
    }
    let mut rings: BTreeMap<u32, Vec<(u32, &[Point])>> = BTreeMap::new();
    for ((group, piece), ring) in rails {
        rings.entry(*group).or_default().push((*piece, ring));
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
                let changes = network.piece(ways, piece);
                for (taper, names) in changes.tapers.into_iter().zip([
                    ["width0", "centre0", "taper0", "lines0"],
                    ["width1", "centre1", "taper1", "lines1"],
                ]) {
                    let Some(taper) = taper else { continue };
                    let [left, right] = taper.to.surface;
                    let metres = |v: f64| (v * 100.0).round() / 100.0;
                    f.tag_double(names[0], metres(right - left))
                        .map_err(failed)?;
                    f.tag_double(names[1], metres((left + right) / 2.0))
                        .map_err(failed)?;
                    f.tag_double(names[2], metres(taper.length))
                        .map_err(failed)?;
                    let at =
                        |(at, _): &(f64, _)| format!("{:.2}", taper.to.lanes[lanes::edge(p, *at)]);
                    let lines: Vec<String> = p.lines.iter().map(at).collect();
                    f.tag_string(names[3], lines.join(",")).map_err(failed)?;
                }
                for (bare, name) in changes.bare.into_iter().zip(["bare0", "bare1"]) {
                    let Some(bare) = bare else { continue };
                    let bare: Vec<String> = bare.iter().map(|v| format!("{v:.1}")).collect();
                    f.tag_string(name, bare.join(",")).map_err(failed)?;
                }
                if let Some(gone) = changes.gone {
                    let gone: Vec<String> = gone.iter().map(|v| format!("{v:.1}")).collect();
                    f.tag_string("dash", gone.join(",")).map_err(failed)?;
                    if let Some(pattern) = p.rules.dash_name() {
                        f.tag_string("pattern", pattern).map_err(failed)?;
                    }
                }
                let without: Vec<String> = gaps[index][i]
                    .iter()
                    .map(|g| format!("{:.2}-{:.2}", g[0].max(0.0), g[1].max(0.0)))
                    .collect();
                if !without.is_empty() {
                    f.tag_string("gaps", without.join(",")).map_err(failed)?;
                }
                f.tag_sint("node0", piece.nodes[0]).map_err(failed)?;
                f.tag_sint("node1", piece.nodes[1]).map_err(failed)?;
                f.tag_bool("anchor0", piece.anchor[0]).map_err(failed)?;
                f.tag_bool("anchor1", piece.anchor[1]).map_err(failed)?;
                f.tag_bool("approach", piece.approach).map_err(failed)?;
                if let Some(lead) = piece.lead {
                    f.tag_uint("lead", u64::from(lead.chain) + 1)
                        .map_err(failed)?;
                    f.tag_bool("lead_back", lead.back).map_err(failed)?;
                }
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
            for (piece, ring) in rings.get(&(index as u32)).into_iter().flatten() {
                let mut f = paints.feature_of(MvtGeomType::Polygon);
                f.tag_uint("group", number as u64).map_err(failed)?;
                f.tag_uint("piece", u64::from(*piece)).map_err(failed)?;
                f.tag_string("kind", "rail").map_err(failed)?;
                f.ring(ring.iter().map(coord), true).map_err(failed)?;
                paints = f.end();
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
             "fields": {"kind": text, "level": text, "layer": "Number", "paved": "Boolean", "material": text}},
            {"id": "marking", "minzoom": ZOOM, "maxzoom": ZOOM,
             "fields": {"kind": text, "dashed": "Boolean", "level": text, "layer": "Number", "confidence": "Number", "gauge": "Number", "width": "Number", "colour": text, "dash": text}},
            {"id": "road", "minzoom": ROAD_ZOOM, "maxzoom": ROAD_ZOOM,
             "fields": {"way": "Number", "class": text, "level": text, "layer": "Number", "lanes": text}},
            {"id": "fence", "minzoom": ROAD_ZOOM, "maxzoom": ROAD_ZOOM,
             "fields": {"kind": text, "height": "Number"}},
            {"id": "thing", "minzoom": ROAD_ZOOM, "maxzoom": ROAD_ZOOM,
             "fields": {"kind": text, "turn": "Number", "size": "Number", "x": "Number", "y": "Number", "group": "Number", "piece": "Number"}},
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
    let fences = source.fences.iter().flat_map(|f| &f.0);
    let mapped = source.mapped.iter().map(|m| &m.1);
    let grounds = (source.grounds.iter()).flat_map(|g| g.shapes.iter().flatten().flatten());
    let lines = roads
        .chain(areas)
        .chain(barriers)
        .chain(fences)
        .chain(grounds);
    for p in lines.chain(mapped) {
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
    let partial = PartialArchive::create(output, OutputFormat::PmTiles)?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_path_runs_on_to_the_outside_of_a_bend_and_no_further() {
        let way = |id: i64, nodes: &[(i64, f64, f64)]| Way {
            id,
            nodes: nodes.iter().map(|n| n.0).collect(),
            points: nodes.iter().map(|n| [n.1, n.2]).collect(),
            profile: profile(|k| (k == "highway").then_some("footway")).unwrap(),
            scale: 1.0,
        };
        let ways = [
            way(1, &[(1, 0.0, 0.0), (2, 10.0, 0.0)]),
            way(2, &[(2, 10.0, 0.0), (3, 10.0, 10.0)]),
            way(3, &[(3, 10.0, 10.0), (4, 10.0, 15.0), (5, 10.0, 20.0)]),
            way(4, &[(4, 10.0, 15.0), (6, 20.0, 15.0)]),
        ];
        let ends = path_ends(&ways);
        // a right angle of two 2 m wide, straight on, and free; onto one that passes
        assert_eq!(ends[0], [OVERLAP, 1.0]);
        assert_eq!(ends[1], [1.0, OVERLAP]);
        assert_eq!(ends[2], [OVERLAP; 2]);
        assert_eq!(ends[3], [OVERLAP; 2]);
        let bend: Vec<_> = (0..2)
            .flat_map(|i| geometry::ribbon(&geometry::extend_ends(&ways[i].points, ends[i]), 2.0))
            .collect();
        let shape = geometry::union(&bend);
        // an L: its outer corner is filled
        assert_eq!((shape.len(), shape[0].len()), (1, 1));
        assert!(
            shape[0][0]
                .iter()
                .any(|p| network::distance(*p, [11.0, -1.0]) < 1e-3)
        );
    }
}
