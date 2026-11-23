//! What lies beside a motor road's lanes (`profile.rs`'s `Side`s): cycle lanes,
//! bus lanes and parking in the carriageway, cycle tracks and sidewalks out past
//! its kerb, what is painted for cyclists and buses, and humps and tables.
//! Each is placed by the road's cross-section where it is (`lanes.rs`), so it
//! opens and closes with its lanes, and stops where their lines stop.
//!
//! Within the carriageway a side is, from the lanes out: the cycle lane, the
//! parking, the shoulder. Out past the surface's edge: the parking beside it,
//! the cycle track, the sidewalk. A track or sidewalk that is mapped as a way of
//! its own beside the road is not drawn again, and a road whose width no tag
//! gives is narrowed to keep clear of the paths mapped beside it.
//!
//! Humps, cushions and tables have one look, whatever the country: on each
//! ramp traffic comes up, a row of white triangles across its lanes, pointing
//! the way it goes. A table's top is tinted across the surface.

use super::details::{Detail, Kind, Walk, place};
use super::furniture::{Area, AreaKind, ring};
use super::geometry::{self, Rect};
use super::lanes::Section;
use super::network::{Network, Point, Way, distance};
use super::paint::Block;
use super::profile::{Bays, Class, Level, Symbol};
use super::spans;
use i_overlay::i_shape::base::data::Shapes;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

/// What slows traffic at a node, as `traffic_calming` says.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Calming {
    Bump,
    Hump,
    Cushion,
    Table,
}

impl Calming {
    /// Metres either way along the road from the node to where its ramps'
    /// triangles end.
    fn half(self) -> f64 {
        match self {
            Self::Bump => 0.3,
            Self::Cushion => 1.0,
            Self::Hump => 1.5,
            Self::Table => 3.0,
        }
    }
}

/// What a node with these tags slows traffic with, if it is drawn.
pub(super) fn calming<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Calming> {
    Some(match get("traffic_calming")? {
        "bump" => Calming::Bump,
        "hump" => Calming::Hump,
        "cushion" => Calming::Cushion,
        "table" => Calming::Table,
        _ => return None,
    })
}

/// Metres from a strip's middle within which a way of its own stands for it (a
/// cycleway for a track, a sidewalk for a sidewalk), and the share of the strip
/// that must be so near one.
const TRACK_NEAR: f64 = 3.0;
const SIDEWALK_NEAR: f64 = 4.0;
const NEAR_SHARE: f64 = 0.25;
/// Metres between the places a strip is looked at.
const SAMPLE: f64 = 5.0;
/// World units of the cells ways are looked up by: about 6 m at 50 degrees.
const CELL: f64 = 64.0;

/// By cell, segments: their way and their first point.
type Cells = FxHashMap<(i64, i64), Vec<(u32, u32)>>;

/// Leaves out the tracks and sidewalks of `ways` that are mapped as ways of
/// their own, cycleways and `footway=sidewalk`: how many of each.
pub(super) fn separate(ways: &mut [Way]) -> [u64; 2] {
    let cell = |v: f64| (v / CELL).floor() as i64;
    // by cell, the segments of cycleways, and of sidewalks, that pass
    let mut cells: [Cells; 2] = Default::default();
    for (index, way) in ways.iter().enumerate() {
        let of = match way.profile.class {
            Class::Cycleway => 0,
            Class::Sidewalk => 1,
            _ => continue,
        };
        for (k, p) in way.points.windows(2).enumerate() {
            let mut seen: Vec<(i64, i64)> = samples(p, CELL / 2.0)
                .into_iter()
                .map(|q| (cell(q[0]), cell(q[1])))
                .collect();
            seen.dedup();
            for c in seen {
                cells[of]
                    .entry(c)
                    .or_default()
                    .push((index as u32, k as u32));
            }
        }
    }
    let ways_ref: &[Way] = ways;
    let near = |of: usize, q: Point, reach: f64| -> bool {
        let r = reach + CELL;
        let (x, y) = (
            cell(q[0] - r)..=cell(q[0] + r),
            cell(q[1] - r)..=cell(q[1] + r),
        );
        x.flat_map(|x| y.clone().map(move |y| (x, y)))
            .filter_map(|c| cells[of].get(&c))
            .flatten()
            .any(|&(w, k)| {
                let points = &ways_ref[w as usize].points;
                to_segment(q, points[k as usize], points[k as usize + 1]) <= reach
            })
    };
    let mut gone: Vec<(usize, usize, usize)> = Vec::new();
    for (index, way) in ways_ref.iter().enumerate() {
        let p = &way.profile;
        if !p.class.is_motor() {
            continue;
        }
        for (i, s) in p.sides.iter().enumerate() {
            let sign = if i == 0 { -1.0 } else { 1.0 };
            let edge = p.width / 2.0 + p.parking[i];
            let strips = [
                (0, edge, s.track, TRACK_NEAR),
                (1, edge + s.track, s.sidewalk, SIDEWALK_NEAR),
            ];
            for (of, from, across, reach) in strips {
                if across <= 0.0 {
                    continue;
                }
                let at = (p.centre + sign * (from + across / 2.0)) * way.scale;
                let middle = geometry::offset(&way.points, at);
                let places = (middle.windows(2))
                    .flat_map(|p| samples(p, SAMPLE * way.scale))
                    .collect::<Vec<_>>();
                let found = (places.iter())
                    .filter(|q| near(of, **q, reach * way.scale))
                    .count();
                if found > 0 && found as f64 >= NEAR_SHARE * places.len() as f64 {
                    gone.push((index, i, of));
                }
            }
        }
    }
    let mut counts = [0; 2];
    for (index, i, of) in gone {
        let side = &mut ways[index].profile.sides[i];
        if of == 0 {
            side.track = 0.0;
        } else {
            side.sidewalk = 0.0;
        }
        counts[of] += 1;
    }
    counts
}

/// The narrowest a lane is made to fit paths beside it: metres, and a share of
/// its class's usual width.
const FIT_LANE: (f64, f64) = (2.25, 0.8);
/// Metres between the places a road is looked at, and from its ends where it is
/// not (there paths turn in to cross).
const FIT_SAMPLE: f64 = 4.0;
const FIT_ENDS: f64 = 6.0;
/// The cosine of the angle within which a path runs beside a road.
const PARALLEL: f64 = 0.94;
/// Metres from a road's line within which a path is on it, not beside it.
const ON_LINE: f64 = 1.0;
/// Of a road's places, the share a path must be beside it at, and the share
/// whose path's edge the road keeps clear of.
const FIT_SHARE: f64 = 0.3;
const FIT_CLEAR: f64 = 0.75;

/// Narrows the motor roads of the ground whose width no tag gives, where paths
/// mapped beside them (sidewalks, footways, cycleways, paths) lie within it: the
/// road's edge on that side goes back to the paths' near edge, as far as its
/// lanes may narrow. The class's usual width is a guess; where the paths are is
/// what is mapped. How many are narrowed.
pub(super) fn fit(ways: &mut [Way]) -> u64 {
    let cell = |v: f64| (v / CELL).floor() as i64;
    let mut cells: Cells = FxHashMap::default();
    for (index, way) in ways.iter().enumerate() {
        let path = matches!(
            way.profile.class,
            Class::Sidewalk | Class::Footway | Class::Cycleway | Class::Path | Class::Bridleway
        );
        if !path || way.profile.level != Level::Ground {
            continue;
        }
        for (k, p) in way.points.windows(2).enumerate() {
            let mut seen: Vec<(i64, i64)> = samples(p, CELL / 2.0)
                .into_iter()
                .map(|q| (cell(q[0]), cell(q[1])))
                .collect();
            seen.dedup();
            for c in seen {
                cells.entry(c).or_default().push((index as u32, k as u32));
            }
        }
    }
    let ways_ref: &[Way] = ways;
    let narrowed: Vec<(usize, [f64; 2])> = (ways_ref.par_iter().enumerate())
        .filter_map(|(index, way)| Some((index, fitted(ways_ref, &cells, way)?)))
        .collect();
    for &(index, edges) in &narrowed {
        ways[index].profile.narrow(edges);
    }
    narrowed.len() as u64
}

/// A road's edges, fitted to the paths beside it, if they move.
fn fitted(ways: &[Way], cells: &Cells, way: &Way) -> Option<[f64; 2]> {
    let p = &way.profile;
    let lanes = p.lanes.as_ref()?;
    if !p.class.is_motor() || p.measured || p.level != Level::Ground {
        return None;
    }
    let cell = |v: f64| (v / CELL).floor() as i64;
    let [left, right] = p.surface();
    // metres out from the line to each edge
    let edges = [-left, right];
    let length: f64 = (way.points.windows(2))
        .map(|w| distance(w[0], w[1]))
        .sum::<f64>()
        / way.scale;
    let (mut places, mut limits) = (0usize, [Vec::new(), Vec::new()]);
    let mut from = 0.0;
    for w in way.points.windows(2) {
        let (a, b) = (w[0], w[1]);
        let long = distance(a, b);
        if long <= 0.0 {
            continue;
        }
        let along = [(b[0] - a[0]) / long, (b[1] - a[1]) / long];
        // right of the way's direction, y growing southwards
        let normal = [-along[1], along[0]];
        for q in samples(w, FIT_SAMPLE * way.scale) {
            let s = from + distance(a, q) / way.scale;
            if s < FIT_ENDS || s > length - FIT_ENDS {
                continue;
            }
            places += 1;
            let reach = (edges[0].max(edges[1]) + FIT_REACH_PATH) * way.scale;
            let mut nearest = [f64::INFINITY; 2];
            for x in cell(q[0] - reach)..=cell(q[0] + reach) {
                for y in cell(q[1] - reach)..=cell(q[1] + reach) {
                    for &(w, k) in cells.get(&(x, y)).into_iter().flatten() {
                        let path = &ways[w as usize];
                        let (c, d) = (path.points[k as usize], path.points[k as usize + 1]);
                        let span = distance(c, d);
                        let cos = ((d[0] - c[0]) * along[0] + (d[1] - c[1]) * along[1]) / span;
                        if span <= 0.0 || cos.abs() < PARALLEL {
                            continue;
                        }
                        let foot = nearest_on(q, c, d);
                        let away = [foot[0] - q[0], foot[1] - q[1]];
                        let off = distance(q, foot) / way.scale;
                        if off < ON_LINE || off * way.scale > reach {
                            continue;
                        }
                        let i = usize::from(away[0] * normal[0] + away[1] * normal[1] > 0.0);
                        // to its near edge
                        let clear = off - path.profile.width / 2.0;
                        nearest[i] = nearest[i].min(clear);
                    }
                }
            }
            for i in 0..2 {
                if nearest[i] < edges[i] {
                    limits[i].push(nearest[i]);
                }
            }
        }
        from += long / way.scale;
    }
    if places == 0 {
        return None;
    }
    let mut out = edges;
    for i in 0..2 {
        let found = &mut limits[i];
        if (found.len() as f64) < FIT_SHARE * places as f64 {
            continue;
        }
        found.sort_by(f64::total_cmp);
        // clear of the paths at that share of the places they are found at
        let k = ((1.0 - FIT_CLEAR) * (found.len() - 1) as f64).round() as usize;
        out[i] = found[k];
    }
    if out == edges {
        return None;
    }
    // as narrow as its lanes may be, given back to each side as it was taken
    let lane = FIT_LANE.0.max(FIT_LANE.1 * p.class.lane_width());
    let least = (edges[0] + edges[1]) * (lane / lanes.width).min(1.0);
    let short = least - (out[0] + out[1]);
    if short > 0.0 {
        let taken = [edges[0] - out[0], edges[1] - out[1]];
        let all = taken[0] + taken[1];
        for i in 0..2 {
            out[i] += short * taken[i] / all;
        }
    }
    let moved = (edges[0] - out[0]).max(edges[1] - out[1]);
    (moved > 0.1).then_some([-out[0], out[1]])
}

/// Metres past a road's further edge that a path's line is looked for.
const FIT_REACH_PATH: f64 = 2.0;

/// The point of a segment nearest `q`.
fn nearest_on(q: Point, a: Point, b: Point) -> Point {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let square = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if square > 0.0 {
        (((q[0] - a[0]) * ab[0] + (q[1] - a[1]) * ab[1]) / square).clamp(0.0, 1.0)
    } else {
        0.0
    };
    [a[0] + ab[0] * t, a[1] + ab[1] * t]
}

/// Points along a segment `every` units apart, its start among them, its end not.
fn samples(p: &[Point], every: f64) -> Vec<Point> {
    let (a, b) = (p[0], p[1]);
    let length = distance(a, b);
    let n = (length / every).ceil().max(1.0) as usize;
    (0..n)
        .map(|k| {
            let t = k as f64 / n as f64;
            [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
        })
        .chain(std::iter::once(b))
        .collect()
}

fn to_segment(q: Point, a: Point, b: Point) -> f64 {
    let ab = [b[0] - a[0], b[1] - a[1]];
    let square = ab[0] * ab[0] + ab[1] * ab[1];
    let t = if square > 0.0 {
        (((q[0] - a[0]) * ab[0] + (q[1] - a[1]) * ab[1]) / square).clamp(0.0, 1.0)
    } else {
        0.0
    };
    distance(q, [a[0] + ab[0] * t, a[1] + ab[1] * t])
}

/// Metres across under which a strip is not drawn.
const NARROWEST: f64 = 0.05;
/// Metres of paint and of the period of the dashed edge of parking.
const PARKING_DASH: [f64; 2] = [1.0, 2.0];
/// Metres a parallel bay is long, a diagonal or perpendicular one wide.
const BAY: [f64; 2] = [6.0, 2.5];

/// A run's cross-section along it: places, and at each, its section.
struct Along {
    points: Vec<Point>,
    sections: Vec<Section>,
    scale: f64,
}

/// That of a run without `trim` metres at its ends.
fn along(ways: &[Way], network: &Network, run: usize, trim: [f64; 2]) -> Option<Along> {
    let edge = &network.runs[run];
    let w = &ways[edge.way as usize];
    let points = geometry::trim(&w.points[edge.points.clone()], trim.map(|m| m * w.scale))?;
    let lengths = geometry::lengths(&points, w.scale, network.run_from[run] + trim[0]);
    let steps = network.shapes.steps(edge.way);
    let (points, lengths) = geometry::stations(&points, &lengths, &steps);
    let sections = (lengths.iter())
        .map(|&s| network.shapes.at(ways, edge.way, s))
        .collect();
    Some(Along {
        points,
        sections,
        scale: w.scale,
    })
}

impl Along {
    /// The ground between two lines of each section, metres right of the
    /// way's, of the segments that reach `rect`.
    fn band(&self, rect: Rect, edges: impl Fn(&Section) -> [f64; 2]) -> Shapes<Point> {
        let of: Vec<[f64; 2]> = self.sections.iter().map(edges).collect();
        let wide = |k: usize| of[k][1] - of[k][0] >= NARROWEST;
        let mut out = Vec::new();
        if !(0..of.len()).any(wide) {
            return out;
        }
        let sides =
            [0, 1].map(|i| geometry::displaced(&self.points, |k| of[k][i] * self.scale, [None; 2]));
        for k in 0..self.points.len() - 1 {
            let corners = [sides[0][k], sides[0][k + 1], sides[1][k], sides[1][k + 1]];
            if !(wide(k) || wide(k + 1)) || !reaches(&corners, rect) {
                continue;
            }
            out.extend(geometry::between(
                &sides[0][k..=k + 1],
                &sides[1][k..=k + 1],
            ));
        }
        out
    }

    /// The line at a place in each section.
    fn line(&self, at: impl Fn(&Section) -> f64) -> Vec<Point> {
        let by: Vec<f64> = self.sections.iter().map(|s| at(s) * self.scale).collect();
        let moved = geometry::displaced(&self.points, |k| by[k], [None; 2]);
        let kept = geometry::forwards(&self.points, &moved);
        kept.into_iter().map(|k| moved[k]).collect()
    }
}

/// Whether points' box meets `rect`.
fn reaches(points: &[Point], rect: Rect) -> bool {
    (0..2).all(|i| {
        points.iter().any(|p| p[i] >= rect[i]) && points.iter().any(|p| p[i] <= rect[i + 2])
    })
}

/// Metres right of the way's line from `from` to `to` metres outwards of the
/// lanes, on the left (`side` 0) or right, within the surface.
fn inside(section: &Section, side: usize, [from, to]: [f64; 2]) -> [f64; 2] {
    if side == 1 {
        let (lane, edge) = (section.lanes[section.lanes.len() - 1], section.surface[1]);
        [(lane + from).min(edge), (lane + to).min(edge)]
    } else {
        let (lane, edge) = (section.lanes[0], section.surface[0]);
        [(lane - to).max(edge), (lane - from).max(edge)]
    }
}

/// The same, outwards of the surface's edge.
fn outside(section: &Section, side: usize, [from, to]: [f64; 2]) -> [f64; 2] {
    let edge = section.surface[side];
    if side == 1 {
        [edge + from, edge + to]
    } else {
        [edge - to, edge - from]
    }
}

/// The strips of a run on the ground that reach `rect`: by surface, whether it
/// lies within the carriageway (else past it), and the ground. Within it they
/// stop where its lanes' lines do; past it they run whole, to be cut by what
/// is carriageway.
pub(super) fn strips(
    ways: &[Way],
    network: &Network,
    run: usize,
    gaps: &[[f64; 2]],
    rect: Rect,
) -> Vec<(&'static str, bool, Shapes<Point>)> {
    let edge = &network.runs[run];
    let way = &ways[edge.way as usize];
    let p = &way.profile;
    let Some(lanes) = p
        .lanes
        .as_ref()
        .filter(|_| p.level == Level::Ground && !edge.inner && p.has_sides())
    else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for span in spans(edge, gaps, way) {
        let Some(along) = along(ways, network, run, span.map(|u| u / way.scale)) else {
            continue;
        };
        for (i, s) in p.sides.iter().enumerate() {
            let cycle = along.band(rect, |section| inside(section, i, [0.0, s.cycle]));
            let parked = [s.cycle, s.cycle + s.parking];
            let parking = along.band(rect, |section| inside(section, i, parked));
            out.push(("cycle_lane", true, cycle));
            out.push(("parking_lane", true, parking));
        }
        for (i, _) in lanes.bus.iter().enumerate().filter(|b| *b.1) {
            let bus = along.band(rect, |section| [section.lanes[i], section.lanes[i + 1]]);
            out.push(("bus_lane", true, bus));
        }
    }
    if let Some(along) = along(ways, network, run, [0.0; 2]) {
        for (i, s) in p.sides.iter().enumerate() {
            let from = p.parking[i];
            let track = [from, from + s.track];
            let walk = [track[1], track[1] + s.sidewalk];
            out.push((
                "cycleway",
                false,
                along.band(rect, |c| outside(c, i, track)),
            ));
            out.push(("sidewalk", false, along.band(rect, |c| outside(c, i, walk))));
        }
    }
    out.retain(|s| !s.2.is_empty());
    out
}

/// The lines a run's sides add on the ground, within `rect`: the kerb inside
/// a track or sidewalk, the dashed edge of parking in the carriageway, and its
/// bays where their way is tagged. By kind, whether dashed, and the points.
pub(super) fn lines(
    ways: &[Way],
    network: &Network,
    run: usize,
    gaps: &[[f64; 2]],
    rect: Rect,
) -> Vec<(&'static str, bool, Vec<Point>)> {
    let edge = &network.runs[run];
    let way = &ways[edge.way as usize];
    let p = &way.profile;
    let beside = p
        .sides
        .iter()
        .any(|s| s.parking + s.track + s.sidewalk > 0.0);
    if p.lanes.is_none() || p.level != Level::Ground || edge.inner || !beside {
        return Vec::new();
    }
    let mut out = Vec::new();
    // a kerb stops where the junction's markings do, and runs past crossings
    for span in spans(edge, &[], way) {
        let Some(along) = along(ways, network, run, span.map(|u| u / way.scale)) else {
            continue;
        };
        for (i, s) in p.sides.iter().enumerate() {
            if s.track + s.sidewalk <= 0.0 {
                continue;
            }
            let kerb = along.line(|c| outside(c, i, [p.parking[i]; 2])[0]);
            out.extend(
                geometry::clip(&kerb, rect)
                    .into_iter()
                    .map(|l| ("kerb", false, l)),
            );
        }
    }
    let walk = Walk::new(ways, network, run);
    for span in spans(edge, gaps, way) {
        let trim = span.map(|u| u / way.scale);
        let Some(along) = along(ways, network, run, trim) else {
            continue;
        };
        for (i, s) in p.sides.iter().enumerate() {
            if s.parking <= 0.0 {
                continue;
            }
            let line = along.line(|c| inside(c, i, [s.cycle; 2])[0]);
            if line.len() > 1 {
                let lengths = geometry::lengths(&line, way.scale, 0.0);
                out.extend(
                    geometry::clip_dashed(&line, &lengths, rect, 0.0, PARKING_DASH)
                        .into_iter()
                        .map(|l| ("parking-edge", true, l)),
                );
            }
            let Some(bays) = s.bays else { continue };
            let (every, slant) = match bays {
                Bays::Parallel => (BAY[0], 0.0),
                Bays::Perpendicular => (BAY[1], 0.0),
                Bays::Diagonal => (BAY[1] * std::f64::consts::SQRT_2, 1.0),
            };
            let [from, to] = [trim[0], walk.length() - trim[1]];
            let mut at = from + every;
            while at < to - every * 0.5 {
                let section = network.at(ways, run, at);
                let across = inside(&section, i, [s.cycle, s.cycle + s.parking]);
                let (point, direction) = (walk.at(at).0, walk.direction(at, 2.0));
                let depth = (across[1] - across[0]) * slant;
                let ends = [[across[0], 0.0], [across[1], depth]]
                    .map(|q| place(point, direction, way.scale, q));
                out.extend(
                    geometry::clip(&ends, rect)
                        .into_iter()
                        .map(|l| ("bay", false, l)),
                );
                at += every;
            }
        }
    }
    out
}

/// Metres between bicycles painted, and between "BUS" written; the least room
/// a run must have for one.
const BICYCLE_EVERY: f64 = 50.0;
const BUS_EVERY: f64 = 80.0;
const ROOM: f64 = 20.0;
/// Metres either side of a symbol that its lane keeps.
const MARGIN: f64 = 0.2;
/// Of humps' triangles: metres across one, between two, and along.
const TRIANGLE: [f64; 3] = [0.6, 0.3, 1.0];

/// The places along a stretch from `from` to `to` metres that something
/// painted `every` metres goes, about its middle.
fn every(from: f64, to: f64, every: f64) -> Vec<f64> {
    let room = to - from;
    if room < ROOM {
        return Vec::new();
    }
    let n = ((room - ROOM) / every).floor() as usize + 1;
    let first = from + (room - (n - 1) as f64 * every) / 2.0;
    (0..n).map(|k| first + k as f64 * every).collect()
}

/// What is painted beside the lanes and across them, to be settled with the
/// rest: bicycles in cycle lanes and shared ones, "BUS" in bus lanes, and
/// triangles on the ramps of `calming`'s humps (by node). Nothing within a
/// junction.
pub(super) fn painted(
    ways: &[Way],
    network: &Network,
    calming: &FxHashMap<i64, Calming>,
) -> Vec<Detail> {
    let mut out = Vec::new();
    let mut drawn: FxHashSet<i64> = FxHashSet::default();
    for (index, run) in network.runs.iter().enumerate() {
        let way = &ways[run.way as usize];
        let p = &way.profile;
        let Some(lanes) = &p.lanes else { continue };
        if run.inner {
            continue;
        }
        let walk = Walk::new(ways, network, index);
        let length = walk.length();
        let [from, to] = [run.cut[0], length - run.cut[1]];
        for (i, side) in p.sides.iter().enumerate() {
            let Some(symbol) = side.symbol else { continue };
            for at in every(from, to, BICYCLE_EVERY) {
                let section = network.at(ways, index, at);
                let n = section.lanes.len() - 1;
                let across = match symbol {
                    Symbol::Bicycle => inside(&section, i, [0.0, side.cycle]),
                    // in the outer lane, towards its edge
                    Symbol::Sharrow if i == 1 => [section.lanes[n - 1], section.lanes[n]],
                    Symbol::Sharrow => [section.lanes[0], section.lanes[1]],
                };
                let kind = match symbol {
                    Symbol::Bicycle => Kind::Bicycle,
                    Symbol::Sharrow => Kind::Sharrow,
                };
                out.extend(symbol_at(run.way, &walk, at, across, side.along, kind));
            }
        }
        let backward = lanes.backward as usize;
        for (i, _) in lanes.bus.iter().enumerate().filter(|b| *b.1) {
            let along = if lanes.along && lanes.against {
                i >= backward
            } else {
                lanes.along
            };
            for at in every(from, to, BUS_EVERY) {
                let section = network.at(ways, index, at);
                let across = [section.lanes[i], section.lanes[i + 1]];
                out.extend(symbol_at(run.way, &walk, at, across, along, Kind::BusText));
            }
        }
        let nodes = &way.nodes[run.points.clone()];
        let places = geometry::lengths(&way.points[run.points.clone()], way.scale, 0.0);
        let last = nodes.len() - 1;
        for (k, node) in nodes.iter().enumerate() {
            let Some(&kind) = calming.get(node) else {
                continue;
            };
            let at = places[k];
            let end = k == 0 || k == last;
            if at < from - 1e-6 || at > to + 1e-6 || (end && network.junction_nodes.contains(node))
            {
                continue;
            }
            if drawn.insert(*node) {
                out.extend(hump(run.way, &walk, network.at(ways, index, at), at, kind));
            }
        }
    }
    out
}

/// A bicycle, a sharrow or "BUS" at `at` metres along a run, upright to whom
/// comes along the way (`along`) or against it, between `across` metres right
/// of the way's line.
fn symbol_at(
    way: u32,
    walk: &Walk,
    at: f64,
    across: [f64; 2],
    along: bool,
    kind: Kind,
) -> Option<Detail> {
    let strokes = match kind {
        Kind::Bicycle => bicycle(false),
        Kind::Sharrow => bicycle(true),
        _ => letters(),
    };
    let points = strokes.iter().flatten();
    let low = points
        .clone()
        .fold([f64::MAX; 2], |m, q| [m[0].min(q[0]), m[1].min(q[1])]);
    let high = points.fold([f64::MIN; 2], |m, q| [m[0].max(q[0]), m[1].max(q[1])]);
    let half = kind.width() / 2.0;
    let wide = high[0] - low[0] + 2.0 * half;
    let fits = ((across[1] - across[0] - 2.0 * MARGIN) / wide).min(1.0);
    // a narrower one would not read
    if fits < 0.6 {
        return None;
    }
    let (point, mut direction) = (walk.at(at).0, walk.direction(at, 2.0));
    if !along {
        direction = [-direction[0], -direction[1]];
    }
    let x = (across[0] + across[1]) / 2.0 * if along { 1.0 } else { -1.0 };
    let middle = [(low[0] + high[0]) / 2.0, (low[1] + high[1]) / 2.0];
    let fit = |q: [f64; 2]| [x + (q[0] - middle[0]) * fits, q[1] - middle[1]];
    let scale = walk.way().scale;
    let span = [x - wide * fits / 2.0, x + wide * fits / 2.0];
    let length = (high[1] - low[1]) / 2.0 + half;
    Some(Detail {
        way,
        kind,
        confidence: 2,
        at: point,
        block: Block::new(point, direction, scale, span, [-length, length]),
        strokes: strokes
            .into_iter()
            .map(|s| {
                s.into_iter()
                    .map(|q| place(point, direction, scale, fit(q)))
                    .collect()
            })
            .collect(),
        heads: Vec::new(),
    })
}

/// A bicycle as seen from the side, laid on the road as an approaching rider
/// sees it upright, and elongated along it; with two chevrons ahead, a sharrow.
/// Metres right and ahead.
fn bicycle(chevrons: bool) -> Vec<Vec<[f64; 2]>> {
    // across, and up its side seen from beside
    const LONG: f64 = 2.2;
    let to = |[h, v]: [f64; 2]| [h * 0.9, v * LONG];
    let wheel = |h: f64| -> Vec<[f64; 2]> {
        (0..=12)
            .map(|k| {
                let (s, c) = (f64::from(k) / 12.0 * std::f64::consts::TAU).sin_cos();
                to([h + 0.22 * c, 0.3 + 0.22 * s])
            })
            .collect()
    };
    let line = |points: &[[f64; 2]]| points.iter().map(|&q| to(q)).collect::<Vec<_>>();
    let mut out = vec![
        wheel(-0.33),
        wheel(0.33),
        line(&[[-0.33, 0.3], [-0.02, 0.3], [0.22, 0.6], [0.33, 0.3]]),
        line(&[[-0.33, 0.3], [-0.12, 0.62], [-0.02, 0.3]]),
        line(&[[-0.12, 0.58], [0.22, 0.6]]),
        line(&[[-0.2, 0.66], [-0.04, 0.66]]),
        line(&[[0.22, 0.6], [0.2, 0.72], [0.29, 0.73]]),
    ];
    if chevrons {
        for y in [0.6, 1.0] {
            let y = 0.75 * LONG + y;
            out.push(vec![[-0.35, y], [0.0, y + 0.35], [0.35, y]]);
        }
    }
    out
}

/// "BUS", its letters side by side across the lane and long along it. Metres
/// right and ahead.
fn letters() -> Vec<Vec<[f64; 2]>> {
    const WIDE: f64 = 0.5;
    const TALL: f64 = 2.4;
    const GAP: f64 = 0.3;
    let b: &[&[[f64; 2]]] = &[
        &[
            [0.0, 0.0],
            [0.0, 1.0],
            [0.7, 1.0],
            [0.9, 0.9],
            [0.9, 0.62],
            [0.7, 0.52],
            [0.0, 0.52],
        ],
        &[
            [0.7, 0.52],
            [1.0, 0.4],
            [1.0, 0.12],
            [0.75, 0.0],
            [0.0, 0.0],
        ],
    ];
    let u: &[&[[f64; 2]]] = &[&[
        [0.0, 1.0],
        [0.0, 0.25],
        [0.2, 0.0],
        [0.8, 0.0],
        [1.0, 0.25],
        [1.0, 1.0],
    ]];
    let s: &[&[[f64; 2]]] = &[&[
        [1.0, 0.88],
        [0.8, 1.0],
        [0.2, 1.0],
        [0.0, 0.85],
        [0.0, 0.62],
        [0.2, 0.52],
        [0.8, 0.48],
        [1.0, 0.38],
        [1.0, 0.15],
        [0.8, 0.0],
        [0.2, 0.0],
        [0.0, 0.12],
    ]];
    let mut out = Vec::new();
    for (i, letter) in [b, u, s].into_iter().enumerate() {
        let left = i as f64 * (WIDE + GAP);
        for stroke in letter {
            out.push(
                stroke
                    .iter()
                    .map(|q| [left + q[0] * WIDE, q[1] * TALL])
                    .collect(),
            );
        }
    }
    out
}

/// The triangles on the ramps of a hump `at` metres along a run, a row for
/// each direction traffic comes up it in, across its lanes.
fn hump(way: u32, walk: &Walk, section: Section, at: f64, kind: Calming) -> Vec<Detail> {
    let w = walk.way();
    let Some(lanes) = &w.profile.lanes else {
        return Vec::new();
    };
    let point = walk.at(at).0;
    let forwards = walk.direction(at, 3.0);
    let scale = w.scale;
    let [base, gap, long] = TRIANGLE;
    let mut out = Vec::new();
    for along in [true, false] {
        if !(if along { lanes.along } else { lanes.against }) {
            continue;
        }
        let span = section.span(lanes, along);
        let direction = if along {
            forwards
        } else {
            [-forwards[0], -forwards[1]]
        };
        // as its traffic sees it
        let [left, right] = if along { span } else { [-span[1], -span[0]] };
        let n = ((right - left + gap) / (base + gap)).floor();
        if n < 1.0 {
            continue;
        }
        let first = (left + right) / 2.0 - (n * base + (n - 1.0) * gap) / 2.0;
        let y = -kind.half() - long;
        let heads = (0..n as usize)
            .map(|k| {
                let x = first + k as f64 * (base + gap);
                [[x, y], [x + base, y], [x + base / 2.0, y + long]]
                    .map(|q| place(point, direction, scale, q))
                    .to_vec()
            })
            .collect();
        out.push(Detail {
            way,
            kind: Kind::Hump,
            confidence: 2,
            at: point,
            block: Block::new(point, direction, scale, [left, right], [y, y + long]),
            strokes: Vec::new(),
            heads,
        });
    }
    out
}

/// The tops of `calming`'s tables on roads of the ground, across the surface
/// and as long as each.
pub(super) fn tables(
    ways: &[Way],
    network: &Network,
    calming: &FxHashMap<i64, Calming>,
) -> Vec<Area> {
    let everywhere: Rect = [f64::MIN, f64::MIN, f64::MAX, f64::MAX];
    let mut out = Vec::new();
    let mut drawn: FxHashSet<i64> = FxHashSet::default();
    for (index, run) in network.runs.iter().enumerate() {
        let way = &ways[run.way as usize];
        if way.profile.lanes.is_none() || way.profile.level != Level::Ground || run.inner {
            continue;
        }
        let nodes = &way.nodes[run.points.clone()];
        let places = geometry::lengths(&way.points[run.points.clone()], way.scale, 0.0);
        let (last, length) = (nodes.len() - 1, places[places.len() - 1]);
        for (k, node) in nodes.iter().enumerate() {
            if calming.get(node) != Some(&Calming::Table) {
                continue;
            }
            let at = places[k];
            let end = k == 0 || k == last;
            let [from, to] = [run.cut[0], length - run.cut[1]];
            if at < from - 1e-6 || at > to + 1e-6 || (end && network.junction_nodes.contains(node))
            {
                continue;
            }
            if !drawn.insert(*node) {
                continue;
            }
            let half = Calming::Table.half();
            let trim = [(at - half).max(0.0), (length - at - half).max(0.0)];
            let Some(along) = along(ways, network, index, trim) else {
                continue;
            };
            for shape in along.band(everywhere, |c| c.surface) {
                if let Some(ring) = ring(&shape[0]) {
                    out.push(Area {
                        kind: AreaKind::Table,
                        paved: true,
                        ring,
                        holes: Vec::new(),
                    });
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::network;
    use super::super::profile::profile;
    use super::*;

    fn way(id: i64, tags: &[(&str, &'static str)], nodes: &[(i64, f64, f64)]) -> Way {
        let scale = 10.0;
        Way {
            id,
            nodes: nodes.iter().map(|n| n.0).collect(),
            points: nodes.iter().map(|n| [n.1 * scale, n.2 * scale]).collect(),
            profile: profile(|key| tags.iter().find(|(k, _)| *k == key).map(|(_, v)| *v)).unwrap(),
            scale,
        }
    }

    fn area(shapes: &Shapes<Point>, scale: f64) -> f64 {
        let ring = |r: &Vec<Point>| {
            let twice: f64 = (0..r.len())
                .map(|i| {
                    let (a, b) = (r[i], r[(i + 1) % r.len()]);
                    a[0] * b[1] - b[0] * a[1]
                })
                .sum();
            twice.abs() / 2.0
        };
        shapes.iter().map(|s| ring(&s[0])).sum::<f64>() / (scale * scale)
    }

    const EVERYWHERE: Rect = [-1e9, -1e9, 1e9, 1e9];

    #[test]
    fn a_cycle_lane_is_a_strip_between_the_lanes_and_the_kerb() {
        let tags = [
            ("highway", "secondary"),
            ("cycleway:right", "lane"),
            ("cycleway:right:lane", "advisory"),
        ];
        let ways = vec![way(1, &tags, &[(1, 0.0, 0.0), (2, 100.0, 0.0)])];
        let n = network::build(&ways);
        let strips = strips(&ways, &n, 0, &[], EVERYWHERE);
        let lane: Vec<_> = strips.iter().filter(|s| s.0 == "cycle_lane").collect();
        assert_eq!(lane.len(), 1);
        assert!(lane[0].1);
        // 1.5 m along 100 m, no less than the ends cut
        let metres = area(&lane[0].2, 10.0);
        assert!((140.0..=150.5).contains(&metres), "{metres}");
        // it lies right of the lanes, within the surface
        let p = &ways[0].profile;
        assert_eq!(p.width, 6.5 + 1.5);
        let line = p
            .lines
            .iter()
            .find(|l| l.1 == super::super::profile::Line::CycleDashed);
        assert_eq!(line.map(|l| l.0), Some(3.25));
        assert!(strips.iter().all(|s| s.0 != "sidewalk"));
    }

    #[test]
    fn a_bus_lane_is_tinted_and_has_its_word() {
        let tags = [
            ("highway", "primary"),
            ("oneway", "yes"),
            ("lanes", "2"),
            ("bus:lanes", "|designated"),
        ];
        let ways = vec![way(1, &tags, &[(1, 0.0, 0.0), (2, 200.0, 0.0)])];
        let p = &ways[0].profile;
        assert_eq!(p.lanes.as_ref().unwrap().bus, [false, true]);
        // a primary road has no edges painted, and its one line is the bus lane's
        assert_eq!(p.lines, [(0.0, super::super::profile::Line::Bus)]);
        let n = network::build(&ways);
        let strips = strips(&ways, &n, 0, &[], EVERYWHERE);
        let bus = strips.iter().find(|s| s.0 == "bus_lane").unwrap();
        assert!((area(&bus.2, 10.0) - 3.5 * 200.0).abs() < 5.0);
        let words: Vec<Detail> = painted(&ways, &n, &FxHashMap::default());
        assert_eq!(words.len(), 3);
        assert!(
            words
                .iter()
                .all(|d| d.kind == Kind::BusText && d.strokes.len() == 4)
        );
        // within the right lane
        for d in &words {
            assert!(
                d.strokes
                    .iter()
                    .flatten()
                    .all(|q| q[1] > 0.0 && q[1] < 35.0)
            );
        }
    }

    #[test]
    fn nothing_is_drawn_where_no_tag_says() {
        let ways = vec![way(
            1,
            &[("highway", "residential")],
            &[(1, 0.0, 0.0), (2, 100.0, 0.0)],
        )];
        let n = network::build(&ways);
        assert!(!ways[0].profile.has_sides());
        assert!(strips(&ways, &n, 0, &[], EVERYWHERE).is_empty());
        assert!(lines(&ways, &n, 0, &[], EVERYWHERE).is_empty());
        assert!(painted(&ways, &n, &FxHashMap::default()).is_empty());
    }

    #[test]
    fn a_road_of_no_width_is_narrowed_to_the_paths_beside_it() {
        let sidewalk = [("highway", "footway"), ("footway", "sidewalk")];
        let line = |id: i64, y: f64| [(id * 10, 0.0, y), (id * 10 + 1, 100.0, y)];
        // a street of 5.5 m, its sidewalks' lines 2.6 m right and 3.5 m left
        let mut ways = vec![
            way(1, &[("highway", "residential")], &line(1, 0.0)),
            way(2, &sidewalk, &line(2, 2.6)),
            way(3, &sidewalk, &line(3, -3.5)),
        ];
        assert_eq!(ways[0].profile.surface(), [-2.75, 2.75]);
        assert_eq!(fit(&mut ways), 1);
        let p = &ways[0].profile;
        // back to 2.5 m and 1.6 m, short of two lanes of 2.25 m: the 0.4 m given
        // back as each side gave up
        let [left, right] = p.surface();
        let back = 0.4 * 0.25 / 1.4;
        assert!(
            (left + 2.5 + back).abs() < 1e-9 && (right - left - 4.5).abs() < 1e-9,
            "{left} {right}"
        );
        let lanes = p.lanes.as_ref().unwrap();
        assert!((lanes.left - left).abs() < 1e-9 && (lanes.width - 2.25).abs() < 1e-9);
        assert!(p.lines.iter().all(|l| (left..=right).contains(&l.0)));
        // nor where its width is tagged, a path crosses or runs on its line
        let tagged = [("highway", "residential"), ("width", "5.5")];
        let crossing = [(40, 50.0, -10.0), (41, 50.0, 10.0)];
        let mut others = vec![
            way(1, &tagged, &line(1, 0.0)),
            way(2, &sidewalk, &line(2, 2.6)),
            way(5, &[("highway", "residential")], &line(5, 300.0)),
            way(4, &sidewalk, &crossing.map(|(n, x, y)| (n, x, y + 300.0))),
            way(6, &sidewalk, &line(6, 300.5)),
        ];
        assert_eq!(fit(&mut others), 0);
    }

    #[test]
    fn a_sidewalk_mapped_on_its_own_is_not_drawn_again() {
        let road = [("highway", "residential"), ("sidewalk", "both")];
        let sidewalk = [("highway", "footway"), ("footway", "sidewalk")];
        let mut ways = vec![
            way(1, &road, &[(1, 0.0, 0.0), (2, 100.0, 0.0)]),
            // 4.5 m right of the line: where the strip's middle is
            way(2, &sidewalk, &[(3, 0.0, 4.5), (4, 100.0, 4.5)]),
        ];
        assert_eq!(separate(&mut ways), [0, 1]);
        let sides = ways[0].profile.sides;
        assert_eq!((sides[0].sidewalk, sides[1].sidewalk), (2.0, 0.0));
        let n = network::build(&ways[..1]);
        let strips = strips(&ways[..1], &n, 0, &[], EVERYWHERE);
        let walks: Vec<_> = strips.iter().filter(|s| s.0 == "sidewalk").collect();
        assert_eq!(walks.len(), 1);
        assert!(!walks[0].1);
        // left of it, from the surface's edge out
        let ys = walks[0].2.iter().flat_map(|s| &s[0]).map(|q| q[1] / 10.0);
        let (low, high) = ys.fold((f64::MAX, f64::MIN), |m, y| (m.0.min(y), m.1.max(y)));
        assert!(
            (low + 4.75).abs() < 1e-6 && (high + 2.75).abs() < 1e-6,
            "{low} {high}"
        );
        let kerbs = lines(&ways[..1], &n, 0, &[], EVERYWHERE);
        assert_eq!(kerbs.len(), 1);
        assert_eq!(kerbs[0].0, "kerb");
    }

    #[test]
    fn a_hump_has_triangles_on_each_ramp_and_none_at_a_junction() {
        let road = [("highway", "residential")];
        let ways = vec![
            way(1, &road, &[(1, 0.0, 0.0), (2, 50.0, 0.0), (3, 100.0, 0.0)]),
            way(2, &road, &[(3, 100.0, 0.0), (4, 150.0, 0.0)]),
            way(3, &road, &[(3, 100.0, 0.0), (5, 100.0, 50.0)]),
        ];
        let n = network::build(&ways);
        let calming: FxHashMap<i64, Calming> = [(2, Calming::Hump), (3, Calming::Table)]
            .into_iter()
            .collect();
        let humps = painted(&ways, &n, &calming);
        assert_eq!(humps.len(), 2);
        // three of 0.6 m and 0.3 m between to a lane of 2.75 m
        assert!(
            humps
                .iter()
                .all(|d| d.kind == Kind::Hump && d.heads.len() == 3)
        );
        // one row before the node either way
        let xs: Vec<f64> = humps.iter().map(|d| d.heads[0][2][0] / 10.0).collect();
        assert!(
            xs.iter().any(|x| *x < 50.0) && xs.iter().any(|x| *x > 50.0),
            "{xs:?}"
        );
        assert!(tables(&ways, &n, &calming).is_empty());
        let raised: FxHashMap<i64, Calming> = [(2, Calming::Table)].into_iter().collect();
        let tops = tables(&ways, &n, &raised);
        let ring: Shapes<Point> = tops.iter().map(|a| vec![a.ring.clone()]).collect();
        assert!((area(&ring, 10.0) - 6.0 * 5.5).abs() < 0.5);
    }

    #[test]
    fn parking_in_the_carriageway_has_a_dashed_edge_and_bays_only_where_tagged() {
        let tags = [
            ("highway", "residential"),
            ("parking:right", "lane"),
            ("parking:right:orientation", "perpendicular"),
            ("parking:left", "lane"),
        ];
        let ways = vec![way(1, &tags, &[(1, 0.0, 0.0), (2, 100.0, 0.0)])];
        assert_eq!(ways[0].profile.width, 5.5 + 5.0 + 2.2);
        let n = network::build(&ways);
        let lines = lines(&ways, &n, 0, &[], EVERYWHERE);
        let bays = lines.iter().filter(|l| l.0 == "bay").count();
        assert_eq!(bays, 39);
        assert!(lines.iter().any(|l| l.0 == "parking-edge" && l.1));
        let strips = strips(&ways, &n, 0, &[], EVERYWHERE);
        assert_eq!(strips.iter().filter(|s| s.0 == "parking_lane").count(), 2);
    }
}
