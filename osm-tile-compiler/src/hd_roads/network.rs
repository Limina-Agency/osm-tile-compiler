//! Roads split where they meet, and how far from a junction each one's
//! markings stop. Positions are world units: z16 tiles of 4096 units, so the
//! plane of every tile at once.

use super::details::Detail;
use super::geometry;
use super::junctions;
use super::lanes::{self, Changes, Dashes, Drawn, Section, Shapes, shapes};
use super::merges::{self, Merge};
use super::profile::{Class, Level, Profile};
use super::structures::Piece;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};

pub(super) type Point = [f64; 2];

pub(super) struct Way {
    pub id: i64,
    pub nodes: Vec<i64>,
    pub points: Vec<Point>,
    pub profile: Profile,
    /// World units a metre here.
    pub scale: f64,
}

/// A stretch of a way between two junctions or ends.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Edge {
    pub way: u32,
    /// The way's points it runs over, both ends included.
    pub points: std::ops::Range<usize>,
    /// Metres at its start and its end that hold no markings.
    pub cut: [f64; 2],
    /// Whether it lies within a junction, between two of its nodes.
    pub inner: bool,
}

/// What a node adds to the ribbons that meet at it: the ground a rounded
/// corner takes in, or a step from one road's edge to another's, the wedge outside
/// a bend, or what lies between roads that part.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Patch {
    pub ring: Vec<Point>,
    pub level: Level,
    pub paved: bool,
}

#[derive(Default)]
pub(super) struct Network {
    /// Between junctions and ends, for the tests.
    #[cfg(test)]
    pub edges: Vec<Edge>,
    /// The edges of a way joined where nothing keeps their markings apart: past
    /// driveways, a road's markings go on.
    pub runs: Vec<Edge>,
    /// Where three or more arms of motor roads meet, driveways too.
    pub junction_nodes: FxHashSet<i64>,
    /// Those of roads, by which junction they are of.
    pub junction_of: FxHashMap<i64, u32>,
    /// How many of these there are, and how many are of more nodes than one.
    pub junctions: u64,
    pub clusters: u64,
    pub patches: Vec<Patch>,
    /// How the ways' cross-sections change along them.
    pub shapes: Shapes,
    /// What is painted where roads part and merge.
    pub paint: Vec<Detail>,
    /// Metres along its way that each run begins.
    pub run_from: Vec<f64>,
    pub dashes: Dashes,
}

impl Network {
    /// The cross-section of a run `s` metres along it.
    pub(super) fn at(&self, ways: &[Way], run: usize, s: f64) -> Section {
        let way = self.runs[run].way;
        self.shapes.at(ways, way, self.run_from[run] + s)
    }

    /// The line at an edge of a run's lanes, without `trim` metres at its ends.
    pub(super) fn line(
        &self,
        ways: &[Way],
        run: usize,
        edge: usize,
        trim: [f64; 2],
    ) -> Option<Drawn> {
        let from = self.run_from[run];
        self.shapes.line(ways, &self.runs[run], from, edge, trim)
    }

    /// How a piece differs from its way's profile: a bridge's, or of a road that
    /// leads up to one its first way's part.
    pub(super) fn piece(&self, ways: &[Way], piece: &Piece) -> Changes {
        let way = &ways[piece.way as usize];
        let along = geometry::lengths(&way.points, way.scale, 0.0);
        let place = |node: i64, p: Point| {
            let mut found = way.nodes.iter().zip(&way.points).zip(&along);
            found
                .find(|((n, q), _)| **n == node && **q == p)
                .map(|f| *f.1)
        };
        let ends = [0, piece.points.len() - 1];
        let length = geometry::lengths(&piece.points, way.scale, 0.0);
        let length = length[length.len() - 1];
        let [start, end] = [0, 1].map(|e: usize| place(piece.nodes[e], piece.points[ends[e]]));
        let whole = along[along.len() - 1];
        // the part of it that is the way's
        let span = match (start, end) {
            (Some(a), Some(b)) => [a, b],
            (Some(a), None) => [a, whole],
            (None, Some(b)) => [0.0, b],
            (None, None) => [0.0, whole],
        };
        let mut tapers = self.shapes.ends(ways, piece.way, span);
        // where it goes on as another way its own end is not the way's
        for (taper, at) in tapers.iter_mut().zip([start, end]) {
            if at.is_none() {
                *taper = None;
            }
        }
        let gone = |line: &(f64, _)| {
            let edge = lanes::edge(&way.profile, line.0);
            let (at, back) = match (start, end) {
                (Some(at), _) => (at, 0.0),
                (_, Some(at)) => (at, length),
                _ => return 0.0,
            };
            let period = way.profile.rules.pattern()[1];
            (self.gone(ways, piece.way, edge, at) - back).rem_euclid(period)
        };
        let gone: Vec<f64> = way.profile.lines.iter().map(gone).collect();
        let any = way.profile.lines.iter().zip(&gone);
        let any = any.fold(false, |any, (l, g)| any || (l.1.dashed() && *g >= 0.05));
        let mut bare = self.shapes.bare(ways, piece.way, span);
        for (bare, at) in bare.iter_mut().zip([start, end]) {
            if at.is_none() {
                *bare = None;
            }
        }
        Changes {
            tapers,
            gone: any.then_some(gone),
            bare,
        }
    }

    /// The metres of a dash's period gone `s` metres along a way, as if its
    /// dashes were laid along it.
    fn gone(&self, ways: &[Way], way: u32, edge: usize, s: f64) -> f64 {
        let runs = self.runs.partition_point(|r| r.way < way);
        let of = (runs..self.runs.len()).take_while(|&i| self.runs[i].way == way);
        let Some(run) = of
            .take_while(|&i| i == runs || self.run_from[i] <= s)
            .last()
        else {
            return 0.0;
        };
        let trim = [s - self.run_from[run], 0.0];
        let (line, at) = match self.line(ways, run, edge, trim) {
            Some(line) => {
                let first = line.lengths[0];
                (line, first)
            }
            // at its very end
            None => match self.line(ways, run, edge, [0.0; 2]) {
                Some(line) => {
                    let whole = line.whole;
                    (line, whole)
                }
                None => return 0.0,
            },
        };
        let dash = ways[way as usize].profile.rules.pattern();
        self.dashes.gone(run as u32, edge, line.whole, at, dash)
    }

    /// The ground beside a run's surface, right of it (`side` 1) or left (-1), out
    /// to `out` metres right of its edge.
    pub(super) fn beside(
        &self,
        ways: &[Way],
        run: usize,
        trim: [f64; 2],
        side: f64,
        out: f64,
    ) -> Vec<Vec<Vec<Point>>> {
        let Edge { way, points, .. } = &self.runs[run];
        let w = &ways[*way as usize];
        let Some(points) = geometry::trim(
            &w.points[points.clone()],
            trim.map(|metres| metres * w.scale),
        ) else {
            return Vec::new();
        };
        let lengths = geometry::lengths(&points, w.scale, self.run_from[run] + trim[0]);
        let (points, lengths) = geometry::stations(&points, &lengths, &self.shapes.steps(*way));
        let surface = |k: usize| {
            let surface = self.shapes.at(ways, *way, lengths[k]).surface;
            surface[usize::from(side > 0.0)]
        };
        let [near, far] = [0.0, out]
            .map(|by| geometry::displaced(&points, |k| (surface(k) + by) * w.scale, [None; 2]));
        geometry::between(&near, &far)
    }
}

#[derive(Clone)]
pub(super) struct Arm {
    pub edge: u32,
    pub end: usize,
    /// Away from the junction, a unit vector.
    pub out: Point,
    /// Metres from the way's line to its surface's edge, left and right of `out`.
    pub half: [f64; 2],
    pub class: Class,
    pub level: Level,
    pub paved: bool,
    pub roundabout: bool,
    /// How many lanes it has, if traffic goes but one way.
    pub one_way: Option<u32>,
}

/// Metres markings are kept back from where an arm's edges clear its neighbours'.
pub(super) const CORNER: f64 = 3.0;
/// The most a junction keeps markings back, metres: past it the arms run side
/// by side and their edges never clear each other.
const MAX_CUT: f64 = 30.0;
/// Metres along an arm its direction is taken over.
const ARM_LENGTH: f64 = 12.0;

/// The ways must be in an order of their own (by id), which fixes the edges'.
#[cfg(test)]
pub(super) fn build(ways: &[Way]) -> Network {
    build_at(ways, &|_| false)
}

/// `signals`, whether there are some at a node.
pub(super) fn build_at(ways: &[Way], signals: &(impl Fn(i64) -> bool + Sync)) -> Network {
    // arms at each node: an end is one, a point within a way two
    let mut uses: FxHashMap<i64, u32> = FxHashMap::default();
    for way in ways.iter().filter(|w| w.profile.class.is_motor()) {
        let last = way.nodes.len() - 1;
        for (i, &node) in way.nodes.iter().enumerate() {
            *uses.entry(node).or_default() += if i == 0 || i == last { 1 } else { 2 };
        }
    }
    let mut shapes = shapes(ways);
    let laid = merges::lay(ways, &mut shapes);
    let merges: FxHashMap<i64, Merge> = (shapes.merges.iter())
        .filter(|m| m.kept.is_some())
        .map(|m| (m.node, m.clone()))
        .collect();
    let parts: FxHashSet<i64> = shapes.merges.iter().map(|m| m.node).collect();
    let mut network = Network {
        shapes,
        patches: laid.patches,
        paint: laid.paint,
        ..Default::default()
    };
    let mut edges: Vec<Edge> = Vec::new();
    let mut arms: FxHashMap<i64, Vec<Arm>> = FxHashMap::default();
    let mut order = Vec::new();
    for (index, way) in ways.iter().enumerate() {
        let last = way.nodes.len() - 1;
        if !way.profile.class.is_motor() {
            continue;
        }
        let mut from = 0;
        for to in 1..=last {
            if to < last && uses[&way.nodes[to]] < 3 {
                continue;
            }
            let edge = edges.len() as u32;
            let points = from..to + 1;
            for (end, node) in [(0, way.nodes[from]), (1, way.nodes[to])] {
                if uses[&node] >= 2 {
                    let at = if end == 0 { from } else { to };
                    let section = network.shapes.tapered(index as u32).map(|shape| {
                        // at a junction as its mouth has it
                        let s = if at == 0 {
                            network.shapes.mouth(index as u32, 0)
                        } else if at == last {
                            shape.length - network.shapes.mouth(index as u32, 1)
                        } else {
                            let before = geometry::lengths(&way.points[..=at], way.scale, 0.0);
                            before[at]
                        };
                        network.shapes.at(ways, index as u32, s).surface
                    });
                    let surface = section.unwrap_or(way.profile.surface());
                    let arm = arm(way, surface, edge, end, &points);
                    let list = arms.entry(node).or_default();
                    if list.is_empty() {
                        order.push((node, way.points[at], way.scale));
                    }
                    list.push(arm);
                }
            }
            edges.push(Edge {
                way: index as u32,
                points,
                cut: [0.0; 2],
                inner: false,
            });
            from = to;
        }
    }
    // each node's patches, and its arms' cuts
    type Met = (Vec<Patch>, Vec<(u32, usize, f64)>);
    let met: Vec<Met> = (order.par_iter())
        .map(|&(node, at, scale)| {
            let arms = &arms[&node];
            let merge = merges.get(&node);
            let mut patches = match merge {
                Some(merge) => parted(ways, &edges, arms, merge, at, scale),
                None => wedges(arms, at, scale),
            };
            if uses[&node] < 3 {
                return (patches, Vec::new());
            }
            let shapes = &network.shapes;
            let fills = junctions::fills(ways, shapes, &edges, arms, (at, scale), merge.is_some());
            patches.extend(fills);
            (patches, cuts(arms, merge.is_some(), signals(node)))
        })
        .collect();
    for ((node, ..), (patches, cuts)) in order.iter().zip(met) {
        network.patches.extend(patches);
        for (edge, end, cut) in cuts {
            edges[edge as usize].cut[end] = cut;
        }
        if uses[node] >= 3 {
            network.junction_nodes.insert(*node);
        }
    }
    let one = junctions::cluster(ways, &mut edges, &arms, &order, &parts);
    network.patches.extend(one.patches);
    network.junctions = one.junctions;
    network.clusters = one.clusters;
    network.junction_of = one.of;
    network.runs = runs(&edges);
    network.run_from = run_from(ways, &network.runs);
    network.dashes = Dashes::new(ways, &network.runs, &network.run_from, &network.shapes);
    #[cfg(test)]
    {
        network.edges = edges;
    }
    network
}

fn runs(edges: &[Edge]) -> Vec<Edge> {
    let mut out: Vec<Edge> = Vec::with_capacity(edges.len());
    for edge in edges {
        match out.last_mut() {
            Some(run)
                if run.way == edge.way
                    && run.points.end == edge.points.start + 1
                    && run.cut[1] == 0.0
                    && edge.cut[0] == 0.0
                    && !(run.inner || edge.inner) =>
            {
                run.points.end = edge.points.end;
                run.cut[1] = edge.cut[1];
            }
            _ => out.push(edge.clone()),
        }
    }
    out
}

/// Metres along its way that each run begins.
fn run_from(ways: &[Way], runs: &[Edge]) -> Vec<f64> {
    let mut out: Vec<f64> = Vec::with_capacity(runs.len());
    for (i, run) in runs.iter().enumerate() {
        let way = &ways[run.way as usize];
        let (mut from, mut after) = (0, 0.0);
        if let Some(last) = i.checked_sub(1).filter(|&k| runs[k].way == run.way) {
            (from, after) = (runs[last].points.start, out[last]);
        }
        let between = &way.points[from..=run.points.start];
        let lengths = geometry::lengths(between, way.scale, after);
        out.push(lengths[lengths.len() - 1]);
    }
    out
}

fn arm(
    way: &Way,
    surface: [f64; 2],
    edge: u32,
    end: usize,
    points: &std::ops::Range<usize>,
) -> Arm {
    let run = &way.points[points.clone()];
    let outward: Vec<Point> = if end == 0 {
        run.to_vec()
    } else {
        run.iter().rev().copied().collect()
    };
    let out = leaving(&outward, way.scale);
    let (left, right) = (-surface[0], surface[1]);
    Arm {
        edge,
        end,
        out,
        half: if end == 0 {
            [left.max(0.0), right.max(0.0)]
        } else {
            [right.max(0.0), left.max(0.0)]
        },
        class: way.profile.class,
        level: way.profile.level,
        paved: way.profile.paved,
        roundabout: way.profile.roundabout,
        one_way: (way.profile.lanes.as_ref())
            .filter(|l| l.along != l.against)
            .map(|l| l.forward + l.backward),
    }
}

/// The direction a line leaves its first point in.
pub(super) fn leaving(outward: &[Point], scale: f64) -> Point {
    let start = outward[0];
    let far = outward
        .iter()
        .find(|p| distance(start, **p) >= ARM_LENGTH * scale)
        .or(outward.last())
        .copied()
        .unwrap_or(start);
    let length = distance(start, far).max(f64::MIN_POSITIVE);
    [(far[0] - start[0]) / length, (far[1] - start[1]) / length]
}

/// Metres along an arm (which way it leaves, and its halves) that it takes to
/// clear another.
pub(super) fn clear((out, half): (Point, [f64; 2]), (other, theirs): (Point, [f64; 2])) -> f64 {
    // World y grows southwards, so a positive cross is clockwise: right.
    let cross = out[0] * other[1] - out[1] * other[0];
    let dot = out[0] * other[0] + out[1] * other[1];
    let (own, facing) = if cross > 0.0 {
        (half[1], theirs[0])
    } else {
        (half[0], theirs[1])
    };
    if cross.abs() > 1e-6 {
        (own * dot + facing) / cross.abs()
    } else if dot > 0.0 {
        MAX_CUT
    } else {
        0.0
    }
}

/// How far each arm's markings stop from the node: where its edges clear its
/// neighbours', and the corner's rounding. A driveway does not break the
/// markings of the road it opens onto, nor, where there are no `signals`, a small
/// road those of a one-way road it joins; and those of roads that part (`merged`)
/// go on.
fn cuts(arms: &[Arm], merged: bool, signals: bool) -> Vec<(u32, usize, f64)> {
    let mut cuts = Vec::with_capacity(arms.len());
    for (i, arm) in arms.iter().enumerate() {
        let others: Vec<&Arm> = arms
            .iter()
            .enumerate()
            .filter(|(j, other)| {
                *j != i && (arm.class == Class::Service || other.class != Class::Service)
            })
            .map(|(_, other)| other)
            .collect();
        let goes_on = merged && arm.class != Class::Service;
        if others.len() < 2 || goes_on || (!signals && passes(arm, &others)) {
            cuts.push((arm.edge, arm.end, 0.0));
            continue;
        }
        let mut cut = 0.0_f64;
        for other in others {
            cut = cut.max(clear((arm.out, arm.half), (other.out, other.half)));
        }
        let cut = if cut > 0.0 { cut + CORNER } else { 0.0 };
        cuts.push((arm.edge, arm.end, cut.min(MAX_CUT)));
    }
    cuts
}

/// Whether an arm is of a one-way road that goes on past a small one at its side.
fn passes(arm: &Arm, others: &[&Arm]) -> bool {
    let [a, b] = others else { return false };
    let on = |o: &Arm| {
        let ahead = arm.out[0] * o.out[0] + arm.out[1] * o.out[1] < -0.9;
        ahead && (o.class, o.level, o.one_way) == (arm.class, arm.level, arm.one_way)
    };
    let small = |o: &Arm| matches!(o.class, Class::Minor | Class::LivingStreet);
    arm.one_way.is_some()
        && !arm.roundabout
        && arm.class <= Class::Tertiary
        && ((on(a) && small(b)) || (on(b) && small(a)))
}

const DISC_SEGMENTS: u32 = 16;

/// The wedge outside a bend where two roads follow each other, as far out as
/// both are there: what a mitre takes in, or an arc of a sharper one.
fn bend(a: &Arm, b: &Arm, at: Point, scale: f64) -> Option<Patch> {
    let ahead = [-a.out[0], -a.out[1]];
    let cross = ahead[0] * b.out[1] - ahead[1] * b.out[0];
    let dot = ahead[0] * b.out[0] + ahead[1] * b.out[1];
    // turning right, the wedge is on the left
    let (side, half) = if cross > 0.0 {
        (1.0, a.half[1].min(b.half[0]))
    } else {
        (-1.0, a.half[0].min(b.half[1]))
    };
    if half <= 0.0 {
        return None;
    }
    let off = |d: Point| [d[1] * side, -d[0] * side];
    let (from, to) = (off(ahead), off(b.out));
    let place = |d: Point, by: f64| [at[0] + d[0] * by * scale, at[1] + d[1] * by * scale];
    let mut ring = vec![at, place(from, half)];
    if dot > 0.5 {
        let sum = [from[0] + to[0], from[1] + to[1]];
        let reach = 2.0 / (sum[0] * sum[0] + sum[1] * sum[1]);
        ring.push(place(sum, reach * half));
    } else {
        let (start, turn) = (from[1].atan2(from[0]), cross.atan2(dot));
        ring.extend((1..WEDGE_SEGMENTS).map(|k| {
            let (s, c) = (start + turn * f64::from(k) / f64::from(WEDGE_SEGMENTS)).sin_cos();
            place([c, s], half)
        }));
    }
    ring.push(place(to, half));
    // wound as the ribbons are
    if side < 0.0 {
        ring.reverse();
    }
    Some(Patch {
        ring,
        level: a.level,
        paved: a.paved,
    })
}

const WEDGE_SEGMENTS: u32 = 8;

/// The wedges outside the bends from a road that parts to each of its arms, out
/// to its edges and to where the arms meet.
fn parted(
    ways: &[Way],
    edges: &[Edge],
    arms: &[Arm],
    merge: &Merge,
    at: Point,
    scale: f64,
) -> Vec<Patch> {
    // as they leave the node: their ends are square to that
    let [wide, left, right] = merge.ends.map(|(way, end)| {
        let arm = arms
            .iter()
            .find(|a| (edges[a.edge as usize].way, a.end) == (way, end))?;
        let points = &ways[way as usize].points;
        let last = points.len() - 1;
        let inner = points[if end == 0 { 1 } else { last - 1 }];
        let length = distance(inner, at).max(f64::MIN_POSITIVE);
        Some(Arm {
            out: [(inner[0] - at[0]) / length, (inner[1] - at[1]) / length],
            ..arm.clone()
        })
    });
    let wedge = |arm: Option<Arm>| {
        let (a, b) = (wide.as_ref()?, arm?);
        let alike = (a.level, a.paved) == (b.level, b.paved);
        let opposite = a.out[0] * b.out[0] + a.out[1] * b.out[1] < -0.99999;
        (alike && !opposite)
            .then(|| bend(a, &b, at, scale))
            .flatten()
    };
    [wedge(left), wedge(right)].into_iter().flatten().collect()
}

/// Where but two roads meet, the wedge outside their bend. Else, where the arms are
/// not of one kind and on the ground (there junctions are filled), of each a disc
/// as wide as the second widest of them: it fills the wedge outside a bend and
/// stands out past no road that goes on.
fn wedges(arms: &[Arm], at: Point, scale: f64) -> Vec<Patch> {
    let roads: Vec<&Arm> = arms.iter().filter(|a| a.class != Class::Service).collect();
    let two = match (&roads[..], arms) {
        ([a, b], _) => Some((*a, *b)),
        (_, [a, b]) => Some((a, b)),
        _ => None,
    };
    if let Some((a, b)) = two {
        let alike = (a.level, a.paved) == (b.level, b.paved);
        let opposite = a.out[0] * b.out[0] + a.out[1] * b.out[1] < -0.99999;
        let found = (alike && !opposite).then(|| bend(a, b, at, scale));
        return found.flatten().into_iter().collect();
    }
    let mut kinds: Vec<(Level, bool)> = arms.iter().map(|a| (a.level, a.paved)).collect();
    kinds.sort_unstable();
    kinds.dedup();
    let mut patches = Vec::new();
    if let [(Level::Ground, _)] = kinds[..] {
        return patches;
    }
    for (level, paved) in kinds {
        let mut alike: Vec<&Arm> = arms
            .iter()
            .filter(|a| (a.level, a.paved) == (level, paved))
            .collect();
        let half = |a: &Arm| a.half[0].min(a.half[1]);
        alike.sort_by(|a, b| half(b).total_cmp(&half(a)).then(a.edge.cmp(&b.edge)));
        let [a, b, ..] = alike[..] else { continue };
        // within a quarter of a degree, under a unit at a wide road's edge
        let opposite = a.out[0] * b.out[0] + a.out[1] * b.out[1] < -0.99999;
        if opposite || half(b) <= 0.0 {
            continue;
        }
        let radius = half(b) * scale;
        let ring = (0..DISC_SEGMENTS)
            .map(|k| {
                let turn = f64::from(k) / f64::from(DISC_SEGMENTS) * std::f64::consts::TAU;
                [at[0] + radius * turn.cos(), at[1] + radius * turn.sin()]
            })
            .collect();
        patches.push(Patch { ring, level, paved });
    }
    patches
}

pub(super) fn distance(a: Point, b: Point) -> f64 {
    (a[0] - b[0]).hypot(a[1] - b[1])
}

#[cfg(test)]
mod tests {
    use super::super::profile::profile;
    use super::*;

    /// A way of `nodes` at metre positions (a unit a metre).
    fn way(tags: &[(&str, &'static str)], nodes: &[(i64, f64, f64)]) -> Way {
        Way {
            id: nodes[0].0,
            nodes: nodes.iter().map(|n| n.0).collect(),
            points: nodes.iter().map(|n| [n.1, n.2]).collect(),
            profile: profile(|k| tags.iter().find(|t| t.0 == k).map(|t| t.1)).unwrap(),
            scale: 1.0,
        }
    }

    const PRIMARY: &[(&str, &str)] = &[("highway", "primary")];
    const SERVICE: &[(&str, &str)] = &[("highway", "service")];

    #[test]
    fn a_crossing_cuts_every_arm_by_the_other_roads_half() {
        let ways = [
            way(PRIMARY, &[(1, -100.0, 0.0), (2, 0.0, 0.0), (3, 100.0, 0.0)]),
            way(
                &[("highway", "tertiary")],
                &[(4, 0.0, -100.0), (2, 0.0, 0.0), (5, 0.0, 100.0)],
            ),
        ];
        let n = build(&ways);
        assert_eq!(n.edges.len(), 4);
        assert_eq!(n.junctions, 1);
        assert_eq!(n.patches.len(), 4);
        let cut = |i: usize| n.edges[i].cut;
        assert_eq!(cut(0), [0.0, 3.0 + CORNER]);
        assert_eq!(cut(1), [3.0 + CORNER, 0.0]);
        assert_eq!(cut(2), [0.0, 3.5 + CORNER]);
    }

    #[test]
    fn a_driveway_leaves_the_roads_markings_alone() {
        let ways = [
            way(PRIMARY, &[(1, -100.0, 0.0), (2, 0.0, 0.0), (3, 100.0, 0.0)]),
            way(SERVICE, &[(2, 0.0, 0.0), (4, 0.0, 50.0)]),
        ];
        let n = build(&ways);
        assert!(n.edges[..2].iter().all(|e| e.cut == [0.0, 0.0]));
        // past it they are one run
        assert_eq!(n.runs.len(), 2);
        assert_eq!(
            (n.runs[0].points.clone(), n.runs[0].cut),
            (0..3, [0.0, 0.0])
        );
        assert!(n.edges[2].cut[0] > 0.0);
    }

    #[test]
    fn ways_that_only_continue_are_not_a_junction() {
        let ways = [
            way(PRIMARY, &[(1, 0.0, 0.0), (2, 50.0, 0.0)]),
            way(PRIMARY, &[(2, 50.0, 0.0), (3, 90.0, 0.0)]),
        ];
        let n = build(&ways);
        assert_eq!(n.junctions, 0);
        assert!(n.patches.is_empty());
    }

    #[test]
    fn a_roundabout_is_split_where_roads_join_it() {
        let ring = [
            (1, 0.0, 10.0),
            (2, 10.0, 0.0),
            (3, 0.0, -10.0),
            (4, -10.0, 0.0),
            (1, 0.0, 10.0),
        ];
        let ways = [
            way(&[("highway", "primary"), ("junction", "roundabout")], &ring),
            way(PRIMARY, &[(2, 10.0, 0.0), (9, 90.0, 0.0)]),
        ];
        let n = build(&ways);
        assert_eq!(n.edges.iter().filter(|e| e.way == 0).count(), 2);
        assert_eq!(n.junctions, 1);
    }

    #[test]
    fn arms_side_by_side_are_cut_no_further_than_the_limit() {
        let ways = [
            way(PRIMARY, &[(1, -100.0, 0.0), (2, 0.0, 0.0)]),
            way(PRIMARY, &[(2, 0.0, 0.0), (3, 200.0, 1.0)]),
            way(PRIMARY, &[(2, 0.0, 0.0), (4, 200.0, -1.0)]),
        ];
        let n = build(&ways);
        assert!(
            n.edges
                .iter()
                .all(|e| e.cut[0] <= MAX_CUT && e.cut[1] <= MAX_CUT)
        );
        assert_eq!(n.edges[1].cut[0], MAX_CUT);
    }

    #[test]
    fn a_corner_touches_both_edges_and_stays_between_the_arms() {
        let ways = [
            way(PRIMARY, &[(1, -100.0, 0.0), (2, 0.0, 0.0), (3, 100.0, 0.0)]),
            way(PRIMARY, &[(2, 0.0, 0.0), (5, 0.0, 100.0)]),
        ];
        let n = build(&ways);
        // two corners: none along the straight side
        assert_eq!(n.patches.len(), 2);
        for corner in &n.patches {
            // the edges are 3.5 m off each line, the arc begins 6 m further out
            for end in [[9.5, 3.5], [3.5, 9.5]] {
                let mut points = corner.ring.iter();
                assert!(
                    points.any(|p| distance([p[0].abs(), p[1]], end) < 1e-6),
                    "{corner:?}"
                );
            }
            for p in &corner.ring {
                // within the corner's square, or a little under the arms, and outside the
                // arc's circle
                assert!(p[1] >= 3.5 - 0.31 && p[0].abs() >= 3.5 - 0.31);
                let beside = p[1] < 3.4 || p[0].abs() < 3.4;
                assert!(beside || distance([p[0].abs(), p[1]], [9.5, 9.5]) > 6.0 - 0.01);
            }
        }
        // wound as the ribbons are, so that the union adds it: with each arm's
        // ribbon it leaves no hole
        let mut shapes: Vec<Vec<Vec<Point>>> =
            n.patches.iter().map(|c| vec![c.ring.clone()]).collect();
        for w in &ways {
            shapes.extend(super::super::geometry::ribbon(&w.points, w.profile.width));
        }
        let surface = super::super::geometry::union(&shapes);
        assert_eq!((surface.len(), surface[0].len()), (1, 1));
        // a short arm has room for less
        let short = [
            way(PRIMARY, &[(1, -100.0, 0.0), (2, 0.0, 0.0), (3, 100.0, 0.0)]),
            way(PRIMARY, &[(2, 0.0, 0.0), (5, 0.0, 5.0)]),
        ];
        let reach = build(&short).patches;
        let far = reach.iter().flat_map(|p| &p.ring).map(|p| p[1]);
        assert!((3.6..5.1).contains(&far.fold(0.0, f64::max)));
    }

    #[test]
    fn a_bend_between_ways_is_filled_and_a_widening_has_no_bulge() {
        let wide = &[("highway", "primary"), ("lanes", "4")][..];
        let straight = [
            way(PRIMARY, &[(1, 0.0, 0.0), (2, 50.0, 0.0)]),
            way(wide, &[(2, 50.0, 0.0), (3, 90.0, 0.0)]),
        ];
        assert!(build(&straight).patches.is_empty());
        let bent = [
            way(PRIMARY, &[(1, 0.0, 0.0), (2, 50.0, 0.0)]),
            way(wide, &[(2, 50.0, 0.0), (3, 80.0, 30.0)]),
        ];
        let n = build(&bent);
        assert_eq!(n.patches.len(), 1);
        // turning right the wedge is on the left, out to where the edges would
        // meet, and fills the bend with no hole
        let ring = &n.patches[0].ring;
        assert!(ring.iter().any(|p| distance(*p, [50.0, -3.5]) < 1e-9));
        let reach = 3.5 / (std::f64::consts::PI / 8.0).cos();
        let far = ring.iter().map(|p| distance(*p, [50.0, 0.0]));
        assert!((far.fold(0.0, f64::max) - reach).abs() < 1e-9, "{ring:?}");
        let mut shapes = vec![vec![ring.clone()]];
        shapes.extend(geometry::ribbon(&bent[0].points, 7.0));
        shapes.extend(geometry::ribbon(&bent[1].points, 7.0));
        let surface = geometry::union(&shapes);
        assert_eq!((surface.len(), surface[0].len()), (1, 1));
    }

    #[test]
    fn a_curved_arm_is_rounded_along_its_edges() {
        let curve: Vec<(i64, f64, f64)> = (0..10)
            .map(|k| {
                let a = f64::from(k) * 0.15;
                (10 + i64::from(k), 30.0 * a.sin(), 30.0 * (1.0 - a.cos()))
            })
            .collect();
        let ways = [
            way(PRIMARY, &[(1, 0.0, -80.0), (10, 0.0, 0.0), (3, 0.0, 80.0)]),
            way(PRIMARY, &curve),
        ];
        let n = build(&ways);
        assert_eq!(n.junctions, 1);
        assert_eq!(n.patches.len(), 2);
        let mut shapes: Vec<Vec<Vec<Point>>> =
            n.patches.iter().map(|c| vec![c.ring.clone()]).collect();
        for w in &ways {
            shapes.extend(geometry::ribbon(&w.points, w.profile.width));
        }
        let surface = geometry::union(&shapes);
        assert_eq!((surface.len(), surface[0].len()), (1, 1));
    }

    #[test]
    fn a_small_road_leaves_a_one_way_roads_markings_alone() {
        let one_way = &[("highway", "primary"), ("oneway", "yes"), ("lanes", "2")][..];
        let side = |class: &'static str| {
            let ways = [
                way(one_way, &[(1, -100.0, 0.0), (2, 0.0, 0.0)]),
                way(one_way, &[(2, 0.0, 0.0), (3, 100.0, 0.0)]),
                way(&[("highway", class)], &[(2, 0.0, 0.0), (4, 0.0, 60.0)]),
            ];
            let n = build(&ways);
            let signalled = build_at(&ways, &|node| node == 2);
            assert!(signalled.edges[0].cut[1] > 5.0);
            (n.edges[0].cut[1], n.edges[1].cut[0], n.edges[2].cut[0])
        };
        let small = side("residential");
        assert!(small.0 == 0.0 && small.1 == 0.0 && small.2 > 5.0);
        let large = side("tertiary");
        assert!(large.0 > 5.0 && large.1 > 5.0 && large.2 > 5.0);
    }
}
