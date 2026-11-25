//! Junctions as one place.
//!
//! A junction is the nodes where three or more arms of roads meet, driveways apart,
//! that lie so near each other that little is left between their markings: those of
//! a divided road's crossing, or of two roads that meet a third nearly face to face.
//! What lies between them holds no lanes' lines and nothing to stop at. Roundabouts'
//! nodes, and those of roads that part (`merges.rs`), are each their own.
//!
//! At a node a road may go *straight over*: of the ends that meet, two that face
//! each other, of one class or name. They are made one there (`lanes.rs`). What ends
//! at such a road is drawn in to what it has beyond.
//!
//! Between each two neighbouring arms the surface's corner is filled: an arc from
//! edge to edge, or where the edges do not meet, a curve from the one that stands
//! out to the other.

use super::details::{self, Detail, Kind, Sign, Walk};
use super::furniture::{Area, AreaKind};
use super::geometry;
use super::lanes::Shapes;
use super::merges::Merge;
use super::network::{Arm, CORNER, Edge, Network, Patch, Point, Way, clear, distance, leaving};
use super::paint::Block;
use super::profile::{Class, Level, Turn};
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;

fn dot(a: Point, b: Point) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}

/// Clockwise from `a` to `b` if positive (y grows southwards).
fn cross(a: Point, b: Point) -> f64 {
    a[0] * b[1] - a[1] * b[0]
}

fn less(a: Point, b: Point) -> Point {
    [a[0] - b[0], a[1] - b[1]]
}

fn unit(a: Point) -> Point {
    let length = a[0].hypot(a[1]).max(f64::MIN_POSITIVE);
    [a[0] / length, a[1] / length]
}

/// `a` and `by` of the direction `d` from there.
fn onward(a: Point, d: Point, by: f64) -> Point {
    [a[0] + d[0] * by, a[1] + d[1] * by]
}

fn is_road(way: &Way) -> bool {
    way.profile.class.is_motor() && way.profile.class != Class::Service
}

// Straight over.

/// The cosine of the most a road turns that goes straight over, and the radians a
/// road of another class does.
const AHEAD: f64 = 0.866;
const OTHER: f64 = 0.2;
/// Radians that are made up for by one name, and cost by another class.
const NAMED: f64 = 0.2;
const CLASSED: f64 = 0.3;
/// Metres of a junction's mouth, the least and the most.
const MOUTH: [f64; 2] = [4.0, 15.0];
/// Metres what ends at a road keeps from its other edge.
const EDGE: f64 = 0.2;
/// The narrowest it is drawn in to.
const NARROWEST: f64 = 1.5;

/// Two ends (a way, 1 for its end) that are one road.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Over {
    pub ends: [(u32, usize); 2],
    /// Metres from the node each of them is within the junction.
    pub mouth: [f64; 2],
    /// Those each edge of it is within another arm, left and right as met from the
    /// first to the second; all there are where none is beside it, and none where one
    /// that is does not hold its corner.
    pub sides: [[f64; 2]; 2],
    /// Those each is past another arm, where one leads back so that a corner would
    /// stand out beyond it; else 0.
    pub past: [[f64; 2]; 2],
}

/// An end that would stand out past a road that goes straight over.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Within {
    pub end: (u32, usize),
    pub mouth: f64,
    /// The edges its surface is drawn in to there, as [`super::lanes::Section`]'s.
    pub surface: [f64; 2],
}

/// An arm of a road at a node.
struct Ray {
    way: u32,
    /// Its end; none if the way goes on past the node.
    end: Option<usize>,
    /// Away from the node, over some metres and at it.
    out: Point,
    first: Point,
    /// Metres to its surface's edges, left and right of `out`.
    half: [f64; 2],
}

/// The arms at each node where three or more of roads meet, in order.
fn meetings(ways: &[Way]) -> Vec<(i64, Vec<Ray>)> {
    let mut uses: FxHashMap<i64, u32> = FxHashMap::default();
    for way in ways.iter().filter(|w| is_road(w)) {
        let last = way.nodes.len() - 1;
        for (i, &node) in way.nodes.iter().enumerate() {
            *uses.entry(node).or_default() += if i == 0 || i == last { 1 } else { 2 };
        }
    }
    let mut at: FxHashMap<i64, usize> = FxHashMap::default();
    let mut out: Vec<(i64, Vec<Ray>)> = Vec::new();
    for (index, way) in ways.iter().enumerate().filter(|(_, w)| is_road(w)) {
        let last = way.nodes.len() - 1;
        let [left, right] = way.profile.surface();
        for (i, &node) in way.nodes.iter().enumerate() {
            if uses[&node] < 3 {
                continue;
            }
            let place = *at.entry(node).or_insert_with(|| {
                out.push((node, Vec::new()));
                out.len() - 1
            });
            let end = match i {
                0 => Some(0),
                _ if i == last => Some(1),
                _ => None,
            };
            let mut add = |outward: &[Point], half: [f64; 2]| {
                out[place].1.push(Ray {
                    way: index as u32,
                    end,
                    out: leaving(outward, way.scale),
                    first: unit(less(outward[1], outward[0])),
                    half,
                });
            };
            if i > 0 {
                let back: Vec<Point> = way.points[..=i].iter().rev().copied().collect();
                add(&back, [right, -left]);
            }
            if i < last {
                add(&way.points[i..], [-left, right]);
            }
        }
    }
    out
}

/// The roads that go straight over junctions, and what is drawn in to them; none at
/// `merges`.
pub(super) fn over(ways: &[Way], merges: &[Merge]) -> (Vec<Over>, Vec<Within>) {
    let parts: FxHashSet<i64> = merges.iter().map(|m| m.node).collect();
    let found: Vec<(Vec<Over>, Vec<Within>)> = meetings(ways)
        .into_par_iter()
        .filter(|(node, rays)| !parts.contains(node) && rays.iter().any(|r| r.end.is_some()))
        .map(|(_, rays)| straight(ways, &rays))
        .collect();
    let (mut over, mut within) = (Vec::new(), Vec::new());
    for (a, b) in found {
        over.extend(a);
        within.extend(b);
    }
    (over, within)
}

/// Whether traffic comes to the node along an arm, and leaves by it.
fn traffic(way: &Way, end: usize) -> [bool; 2] {
    let lanes = way.profile.lanes.as_ref();
    let [along, against] = lanes.map_or([false; 2], |l| [l.along, l.against]);
    if end == 1 {
        [along, against]
    } else {
        [against, along]
    }
}

fn straight(ways: &[Way], rays: &[Ray]) -> (Vec<Over>, Vec<Within>) {
    let profile = |i: usize| &ways[rays[i].way as usize].profile;
    let mut found: Vec<(f64, usize, usize)> = Vec::new();
    for i in 0..rays.len() {
        for j in i + 1..rays.len() {
            let (a, b) = (&rays[i], &rays[j]);
            let (Some(end), Some(other)) = (a.end, b.end) else {
                continue;
            };
            let ahead = -dot(a.out, b.out);
            let (p, q) = (profile(i), profile(j));
            if ahead < AHEAD || p.level != q.level || p.roundabout || q.roundabout {
                continue;
            }
            let [comes, leaves] = traffic(&ways[a.way as usize], end);
            let [theirs, onward] = traffic(&ways[b.way as usize], other);
            if !(comes && onward || theirs && leaves) || a.way == b.way {
                continue;
            }
            let named = (0..2).any(|k| p.named[k] != 0 && p.named[k] == q.named[k]);
            let alike = p.class == q.class;
            let angle = ahead.min(1.0).acos();
            if !alike && !named && angle > OTHER {
                continue;
            }
            let score = angle + if alike { 0.0 } else { CLASSED } - if named { NAMED } else { 0.0 };
            found.push((score, i, j));
        }
    }
    found.sort_by(|a, b| a.0.total_cmp(&b.0).then((a.1, a.2).cmp(&(b.1, b.2))));
    let mut taken = vec![false; rays.len()];
    let mut pairs: Vec<[usize; 2]> = Vec::new();
    for (_, i, j) in found {
        if !taken[i] && !taken[j] {
            (taken[i], taken[j]) = (true, true);
            pairs.push([i, j]);
        }
    }
    let mouth = |i: usize, with: Option<usize>| {
        let others = (0..rays.len()).filter(|&k| k != i && Some(k) != with);
        let of = |k: usize| (rays[k].out, rays[k].half);
        let cut = others.map(|k| clear(of(i), of(k))).fold(0.0, f64::max);
        (cut + CORNER).clamp(MOUTH[0], MOUTH[1])
    };
    let beside = |i: usize, with: usize, comes: bool| -> [[f64; 2]; 2] {
        let (mut sides, mut past) = ([f64::INFINITY; 2], [0.0_f64; 2]);
        let (out, half) = (rays[i].out, rays[i].half);
        for k in (0..rays.len()).filter(|&k| k != i && k != with) {
            // right of the way out of the node is left of the way to it
            let other = &rays[k];
            let right = cross(out, other.out) > 0.0;
            let side = usize::from(right != comes);
            let within = clear((out, half), (other.out, other.half));
            // its further edge, as the nearer one of one that is all on that side
            let further = if right {
                [-other.half[1], 0.0]
            } else {
                [0.0, -other.half[0]]
            };
            let beyond = clear((out, half), (other.out, further));
            sides[side] = if sides[side].is_finite() {
                sides[side].max(within)
            } else {
                within
            };
            past[side] = past[side].max(beyond.min(within));
        }
        [sides, past]
    };
    let over: Vec<Over> = (pairs.iter())
        .map(|&[i, j]| {
            let end = |i: usize| (rays[i].way, rays[i].end.expect("an end's"));
            let [first, second] = [beside(i, j, true), beside(j, i, false)];
            Over {
                ends: [end(i), end(j)],
                mouth: [mouth(i, Some(j)), mouth(j, Some(i))],
                sides: [first[0], second[0]],
                past: [first[1], second[1]],
            }
        })
        .collect();

    // the most important road that goes on
    let passing = (1..rays.len())
        .filter(|&i| rays[i].end.is_none() && rays[i - 1].end.is_none())
        .filter(|&i| rays[i].way == rays[i - 1].way)
        .map(|i| [i - 1, i]);
    let through = pairs.iter().copied().chain(passing);
    let through = through.min_by_key(|&[i, j]| (profile(i).class, i, j));
    let Some([i, j]) = through else {
        return (over, Vec::new());
    };
    let (a, b) = (&rays[i], &rays[j]);
    let right = [-a.first[1], a.first[0]];
    let within = (0..rays.len())
        .filter(|&k| k != i && k != j && !taken[k])
        .filter(|&k| profile(k).level == profile(i).level && !profile(i).roundabout)
        .filter_map(|k| {
            let ray = &rays[k];
            let end = ray.end?;
            let side = dot(ray.first, right);
            let theirs = [-ray.first[1], ray.first[0]];
            // metres right of the road's line its corners are, and the far edge
            let corners = [-ray.half[0], ray.half[1]].map(|x| x * dot(theirs, right));
            let far = if side > 0.0 {
                EDGE - a.half[0].min(b.half[1])
            } else {
                a.half[1].min(b.half[0]) - EDGE
            };
            let mut half = ray.half;
            let mut changed = false;
            for (half, corner) in half.iter_mut().zip(corners) {
                // past it, on that side of the line
                let past = corner * far > 0.0 && corner.abs() > far.abs();
                if past && (side > 0.0) == (far < 0.0) {
                    *half *= far / corner;
                    changed = true;
                }
            }
            let [left, right] = half;
            (changed && left + right >= NARROWEST).then(|| Within {
                end: (ray.way, end),
                mouth: mouth(k, None),
                surface: if end == 0 {
                    [-left, right]
                } else {
                    [-right, left]
                },
            })
        })
        .collect();
    (over, within)
}

// Nodes that are one junction.

/// Metres that may be left between two nodes' markings for them to be of one
/// junction, the most of each node's that count, and of what lies between.
const LEFT: f64 = 6.0;
const COUNTED: f64 = 14.0;
const APART: f64 = 40.0;
/// The most nodes of one, and metres across them.
const NODES: usize = 12;
const ACROSS: f64 = 70.0;
/// Metres along another node's arm that one's markings are kept back from it.
const REACH: f64 = 30.0;
/// Metres between divided roads' carriageways, where they cross, that are road.
const MEDIAN: f64 = 8.0;

/// What nodes make together.
#[derive(Default)]
pub(super) struct Clusters {
    /// By node, which junction it is of.
    pub of: FxHashMap<i64, u32>,
    pub junctions: u64,
    /// Those of more nodes than one.
    pub clusters: u64,
    pub patches: Vec<Patch>,
}

/// What lies between two nodes: edges one after the other, the nodes, and metres.
struct Between {
    edges: Vec<u32>,
    nodes: [usize; 2],
    length: f64,
}

/// Takes nodes together. Edges within a junction are told so and hold no markings,
/// and those that lead out of it are cut back from all of it. `parts` are the nodes
/// where roads part.
pub(super) fn cluster(
    ways: &[Way],
    edges: &mut [Edge],
    arms: &FxHashMap<i64, Vec<Arm>>,
    order: &[(i64, Point, f64)],
    parts: &FxHashSet<i64>,
) -> Clusters {
    let roads = |node: i64| arms[&node].iter().filter(|a| a.class != Class::Service);
    // the junctions' nodes, each with its place among them
    let nodes: Vec<&(i64, Point, f64)> = order.iter().filter(|o| roads(o.0).count() > 2).collect();
    let place: FxHashMap<i64, usize> = nodes.iter().enumerate().map(|(i, o)| (o.0, i)).collect();
    let alone: Vec<bool> = (nodes.iter())
        .map(|o| parts.contains(&o.0) || roads(o.0).any(|a| a.roundabout))
        .collect();
    let length = |edge: u32| {
        let edge = &edges[edge as usize];
        let way = &ways[edge.way as usize];
        let points = way.points[edge.points.clone()].windows(2);
        points.map(|p| distance(p[0], p[1])).sum::<f64>() / way.scale
    };
    let between = |from: usize, arm: &Arm| -> Option<Between> {
        let mut out = Vec::new();
        let (mut edge, mut end, mut total) = (arm.edge, arm.end, 0.0);
        loop {
            out.push(edge);
            total += length(edge);
            if total > APART || out.len() > NODES {
                return None;
            }
            let e = &edges[edge as usize];
            let far = if end == 0 {
                e.points.end - 1
            } else {
                e.points.start
            };
            let node = ways[e.way as usize].nodes[far];
            if let Some(&to) = place.get(&node) {
                let cuts = [
                    edges[arm.edge as usize].cut[arm.end],
                    edges[edge as usize].cut[1 - end],
                ];
                let held: f64 = cuts.iter().map(|c| c.min(COUNTED)).sum();
                let first = (out[0], arm.end);
                let near = total <= held + LEFT && (from, first) < (to, (edge, 1 - end));
                return (near && to != from && !alone[to]).then_some(Between {
                    edges: out,
                    nodes: [from, to],
                    length: total,
                });
            }
            // on past driveways, as another way
            let list = arms.get(&node)?;
            let mut on = list.iter().filter(|a| a.class != Class::Service);
            let next = on.find(|a| (a.edge, a.end) != (edge, 1 - end))?;
            if next.level != arm.level {
                return None;
            }
            (edge, end) = (next.edge, next.end);
        }
    };
    let mut pieces: Vec<Between> = (0..nodes.len())
        .into_par_iter()
        .filter(|&i| !alone[i])
        .flat_map_iter(|i| roads(nodes[i].0).map(move |arm| (i, arm)))
        .filter_map(|(i, arm)| between(i, arm))
        .collect();
    pieces.sort_by(|a, b| {
        let order = a.length.total_cmp(&b.length);
        order.then((a.nodes, a.edges[0]).cmp(&(b.nodes, b.edges[0])))
    });
    // the shortest first, as long as it does not grow too large
    let mut root: Vec<usize> = (0..nodes.len()).collect();
    fn find(root: &mut [usize], mut i: usize) -> usize {
        while root[i] != i {
            root[i] = root[root[i]];
            i = root[i];
        }
        i
    }
    let mut members: FxHashMap<usize, Vec<usize>> = FxHashMap::default();
    for piece in &pieces {
        let [a, b] = piece.nodes.map(|n| find(&mut root, n));
        if a == b {
            continue;
        }
        let of = |i: usize| members.get(&i).cloned().unwrap_or_else(|| vec![i]);
        let all = [of(a), of(b)].concat();
        let across = |i: usize| {
            let (low, high) = all.iter().fold((f64::MAX, f64::MIN), |(low, high), &n| {
                let v = nodes[n].1[i];
                (low.min(v), high.max(v))
            });
            (high - low) / nodes[a].2
        };
        if all.len() > NODES || across(0).hypot(across(1)) > ACROSS {
            continue;
        }
        let (keep, drop) = (a.min(b), a.max(b));
        root[drop] = keep;
        members.remove(&drop);
        members.insert(keep, all);
    }
    let mut out = Clusters::default();
    let mut number: Vec<u32> = vec![0; nodes.len()];
    for i in 0..nodes.len() {
        let to = find(&mut root, i);
        // the first of one is its root
        if to == i {
            number[i] = out.junctions as u32;
            out.junctions += 1;
        }
        out.of.insert(nodes[i].0, number[to]);
    }
    let pieces: Vec<Between> = pieces
        .into_iter()
        .filter(|p| find(&mut root, p.nodes[0]) == find(&mut root, p.nodes[1]))
        .collect();
    let within = pieces.iter().flat_map(|p| &p.edges);
    let within: Vec<(u32, f64)> = within.map(|&e| (e, length(e))).collect();
    for (edge, length) in within {
        let edge = &mut edges[edge as usize];
        (edge.inner, edge.cut) = (true, [length / 2.0 + 1.0; 2]);
    }
    let mut groups: Vec<(&usize, &Vec<usize>)> = members.iter().collect();
    groups.sort_unstable();
    out.clusters = groups.len() as u64;
    for (_, members) in groups {
        let mut list: Vec<(usize, &Arm)> = Vec::new();
        for &n in members {
            let outer = roads(nodes[n].0).filter(|a| !edges[a.edge as usize].inner);
            list.extend(outer.map(|a| (n, a)));
        }
        for &(n, arm) in &list {
            let mut cut = edges[arm.edge as usize].cut[arm.end];
            for &(m, other) in list.iter().filter(|o| o.0 != n) {
                let beside = cross(arm.out, other.out).abs() < 0.35;
                if cut <= 0.0 || (beside && dot(arm.out, other.out) > 0.0) {
                    continue;
                }
                let scale = nodes[n].2;
                let clear = held([(nodes[n].1, arm), (nodes[m].1, other)], scale);
                if clear > 0.0 {
                    cut = cut.max((clear + CORNER).min(REACH));
                }
            }
            edges[arm.edge as usize].cut[arm.end] = cut;
        }
    }
    out.patches = squares(ways, edges, &pieces, &nodes);
    out
}

/// Metres along the first of two arms that the second's strip lies over it.
fn held([(at, arm), (from, other)]: [(Point, &Arm); 2], scale: f64) -> f64 {
    let strip = |at: Point, arm: &Arm| {
        let right = [-arm.out[1], arm.out[0]];
        let put = |x: f64, y: f64| onward(onward(at, right, x * scale), arm.out, y * scale);
        let [left, right] = arm.half;
        vec![
            put(-left, 0.0),
            put(right, 0.0),
            put(right, REACH),
            put(-left, REACH),
        ]
    };
    let (own, mut theirs) = (strip(at, arm), strip(from, other));
    for k in 0..own.len() {
        let (a, b) = (own[k], own[(k + 1) % own.len()]);
        // on the side the strip's middle is
        let side = |p: Point| cross(less(b, a), less(p, a));
        let inside = side(onward(at, arm.out, REACH / 2.0 * scale)).signum();
        let count = theirs.len();
        let mut kept = Vec::with_capacity(count + 1);
        for i in 0..count {
            let (p, q) = (theirs[i], theirs[(i + 1) % count]);
            let (s, t) = (side(p) * inside, side(q) * inside);
            if s >= 0.0 {
                kept.push(p);
            }
            if (s >= 0.0) != (t >= 0.0) {
                kept.push(onward(p, less(q, p), s / (s - t)));
            }
        }
        theirs = kept;
    }
    let along = theirs.iter().map(|p| dot(less(*p, at), arm.out) / scale);
    along.fold(0.0, f64::max)
}

/// The ground between two divided roads' carriageways, where they cross: four nodes
/// with what lies between them, and little of it open.
fn squares(
    ways: &[Way],
    edges: &[Edge],
    pieces: &[Between],
    nodes: &[&(i64, Point, f64)],
) -> Vec<Patch> {
    let mut beside: BTreeMap<usize, Vec<(usize, usize)>> = BTreeMap::new();
    for (k, piece) in pieces.iter().enumerate() {
        let [a, b] = piece.nodes;
        beside.entry(a).or_default().push((b, k));
        beside.entry(b).or_default().push((a, k));
    }
    let profile = |k: usize| &ways[edges[pieces[k].edges[0] as usize].way as usize].profile;
    let mut out = Vec::new();
    for (&a, next) in &beside {
        for &(b, p) in next.iter().filter(|n| n.0 > a) {
            for &(c, q) in beside[&b].iter().filter(|n| n.0 > a) {
                for &(d, r) in beside[&c].iter().filter(|n| n.0 > b && n.0 != c) {
                    let Some(&(_, s)) = beside[&d].iter().find(|n| n.0 == a) else {
                        continue;
                    };
                    let all = [p, q, r, s];
                    let first = profile(p);
                    let alike = all.iter().all(|&k| {
                        let o = profile(k);
                        (o.level, o.paved) == (Level::Ground, first.paved) && !o.roundabout
                    });
                    // what is open of it, each way
                    let open = |across: [usize; 2], along: [usize; 2]| {
                        let long = along.map(|k| pieces[k].length).iter().sum::<f64>() / 2.0;
                        long - across.map(|k| profile(k).width / 2.0).iter().sum::<f64>()
                    };
                    let open = [open([q, s], [p, r]), open([p, r], [q, s])];
                    if !alike || open.iter().any(|v| *v > MEDIAN) {
                        continue;
                    }
                    let ring = [a, b, c, d].map(|n| nodes[n].1);
                    let turns: Vec<f64> = (0..4)
                        .map(|i| {
                            let [x, y, z] = [i, i + 1, i + 2].map(|k| ring[k % 4]);
                            cross(less(y, x), less(z, y))
                        })
                        .collect();
                    let convex = turns.iter().all(|t| *t > 0.0) || turns.iter().all(|t| *t < 0.0);
                    if let Some(ring) = super::furniture::ring(&ring).filter(|_| convex) {
                        out.push(Patch {
                            ring,
                            level: Level::Ground,
                            paved: first.paved,
                        });
                    }
                }
            }
        }
    }
    out
}

// Corners.

/// Metres along an arm its edges are looked at, and that they are taken to go on
/// back past the node.
const LOOKED: f64 = 30.0;
const BACK: f64 = 1.5;
/// Metres from its point that a corner may begin.
const MAX_TANGENT: f64 = 12.0;
/// A corner's radius where roads lie side by side, and radians from which and to
/// which it is one's own.
const NOSE: f64 = 0.75;
const ANGLES: [f64; 2] = [0.3, 1.2];
/// Of its radius, how much longer a blunt corner is for each right angle more.
const BLUNT: f64 = 1.6;
/// Of a step from one's edge to another's, metres that show, along it for each across
/// and the most.
const STEP: f64 = 0.05;
const EASED: f64 = 3.0;
const MAX_EASED: f64 = 15.0;
/// Metres a corner reaches in under what it lies between.
const UNDER: f64 = 0.3;
/// Metres between two edges that do not meet past which it stays open.
const GAP: f64 = 8.0;
const SEGMENTS: u32 = 6;

/// Metres a corner of roads of this class is rounded by.
fn radius(class: Class) -> f64 {
    match class {
        Class::Service => 1.5,
        Class::LivingStreet => 3.0,
        Class::Minor => 4.0,
        Class::Tertiary => 5.0,
        _ => 6.0,
    }
}

/// A line and the units along it of each point.
struct Side {
    points: Vec<Point>,
    along: Vec<f64>,
    /// Those of them it was made longer by at its start.
    back: f64,
}

impl Side {
    fn new(points: &[Point], scale: f64) -> Self {
        let back = BACK * scale;
        let first = unit(less(points[0], points[1]));
        let mut line = vec![onward(points[0], first, back)];
        line.extend_from_slice(points);
        Self {
            along: geometry::lengths(&line, 1.0, 0.0),
            points: line,
            back,
        }
    }

    fn length(&self) -> f64 {
        self.along[self.along.len() - 1]
    }

    /// The point `s` units along, and the direction there.
    fn at(&self, s: f64) -> (Point, Point) {
        let last = self.points.len() - 1;
        let k = self.along.partition_point(|&a| a <= s).clamp(1, last);
        let d = unit(less(self.points[k], self.points[k - 1]));
        (onward(self.points[k - 1], d, s - self.along[k - 1]), d)
    }

    /// From `from` units along to `to`, `right` units right of it.
    fn under(&self, from: f64, to: f64, right: f64) -> Vec<Point> {
        let within = self.along.iter().copied().filter(|s| from < *s && *s < to);
        [from]
            .into_iter()
            .chain(within)
            .chain([to])
            .map(|s| {
                let (p, d) = self.at(s);
                onward(p, [-d[1], d[0]], right)
            })
            .collect()
    }

    /// Where the two first meet, units along each.
    fn meets(&self, other: &Self) -> Option<(f64, f64)> {
        for (i, p) in self.points.windows(2).enumerate() {
            let d = less(p[1], p[0]);
            let mut first: Option<(f64, f64)> = None;
            for (j, q) in other.points.windows(2).enumerate() {
                let e = less(q[1], q[0]);
                let turn = cross(d, e);
                if turn.abs() < 1e-9 * (dot(d, d) * dot(e, e)).sqrt() {
                    continue;
                }
                let off = less(q[0], p[0]);
                let (t, u) = (cross(off, e) / turn, cross(off, d) / turn);
                if (0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u) {
                    let s = self.along[i] + t * (self.along[i + 1] - self.along[i]);
                    let o = other.along[j] + u * (other.along[j + 1] - other.along[j]);
                    if first.is_none_or(|f| s < f.0) {
                        first = Some((s, o));
                    }
                }
            }
            if first.is_some() {
                return first;
            }
        }
        None
    }
}

/// From `a`, going `from`, to `b`, going `to`; without its ends.
fn curve(a: Point, from: Point, b: Point, to: Point) -> Vec<Point> {
    let chord = distance(a, b);
    let turn = dot(from, to).clamp(-1.0, 1.0).acos();
    // an arc's, or where it goes on as it came an S
    let reach = if turn < 0.3 {
        chord * 0.4
    } else {
        chord / (2.0 * (turn / 2.0).sin()) * 4.0 / 3.0 * (turn / 4.0).tan()
    };
    let (p, q) = (onward(a, from, reach), onward(b, to, -reach));
    (1..SEGMENTS)
        .map(|k| {
            let t = f64::from(k) / f64::from(SEGMENTS);
            let u = 1.0 - t;
            let w = [u * u * u, 3.0 * u * u * t, 3.0 * u * t * t, t * t * t];
            [0, 1].map(|i| a[i] * w[0] + p[i] * w[1] + q[i] * w[2] + b[i] * w[3])
        })
        .collect()
}

/// The edges of an arm's surface going out from the node, left and right.
fn sides(ways: &[Way], shapes: &Shapes, edge: &Edge, end: usize) -> Option<[Vec<Point>; 2]> {
    let way = &ways[edge.way as usize];
    let run = &way.points[edge.points.clone()];
    let units: f64 = run.windows(2).map(|p| distance(p[0], p[1])).sum();
    let over = (units - LOOKED * way.scale).max(0.0);
    let trim = if end == 0 { [0.0, over] } else { [over, 0.0] };
    let points = geometry::trim(run, trim)?;
    let tapered = shapes.tapered(edge.way).is_some();
    let (mut points, lengths) = if tapered {
        let before = geometry::lengths(&way.points[..=edge.points.start], way.scale, 0.0);
        let from = before[before.len() - 1] + trim[0] / way.scale;
        let lengths = geometry::lengths(&points, way.scale, from);
        geometry::stations(&points, &lengths, &shapes.steps(edge.way))
    } else {
        (points, Vec::new())
    };
    points.dedup();
    if points.len() < 2 || (tapered && points.len() != lengths.len()) {
        return None;
    }
    let own = way.profile.surface();
    let surface = |k: usize| match lengths.get(k) {
        Some(&s) => shapes.at(ways, edge.way, s).surface,
        None => own,
    };
    let of = |i: usize| {
        let moved = geometry::displaced(&points, |k| surface(k)[i] * way.scale, [None; 2]);
        let kept = geometry::forwards(&points, &moved);
        let mut line: Vec<Point> = kept.into_iter().map(|k| moved[k]).collect();
        line.dedup();
        line
    };
    let mut out = [of(0), of(1)];
    if end == 1 {
        out.reverse();
        out.iter_mut().for_each(|line| line.reverse());
    }
    out.iter().all(|line| line.len() > 1).then_some(out)
}

/// What lies between each two neighbouring arms at a node and is road; between roads
/// that part (`merged`) is none.
pub(super) fn fills(
    ways: &[Way],
    shapes: &Shapes,
    edges: &[Edge],
    arms: &[Arm],
    (at, scale): (Point, f64),
    merged: bool,
) -> Vec<Patch> {
    let angle = |arm: &Arm| arm.out[1].atan2(arm.out[0]);
    let mut order: Vec<&Arm> = arms.iter().filter(|a| a.level == Level::Ground).collect();
    order.sort_by(|a, b| angle(a).total_cmp(&angle(b)).then(a.edge.cmp(&b.edge)));
    let mut out = Vec::new();
    if order.len() < 2 {
        return out;
    }
    let two = order.iter().filter(|a| a.class != Class::Service).count() == 2;
    let lines: Vec<Option<[Side; 2]>> = (order.iter())
        .map(|arm| {
            let lines = sides(ways, shapes, &edges[arm.edge as usize], arm.end)?;
            Some(lines.map(|line| Side::new(&line, scale)))
        })
        .collect();
    for (i, a) in order.iter().enumerate() {
        let j = (i + 1) % order.len();
        let b = order[j];
        let roads = a.class != Class::Service && b.class != Class::Service;
        let (Some(x), Some(y)) = (&lines[i], &lines[j]) else {
            continue;
        };
        // two roads that follow have their wedge
        if a.paved != b.paved || (roads && (merged || two)) {
            continue;
        }
        // World y grows southwards, so clockwise is to the right: a's right edge
        // faces b's left.
        let between = (angle(b) - angle(a)).rem_euclid(std::f64::consts::TAU);
        let radius = radius(a.class.max(b.class));
        for ring in fill(at, &x[1], &y[0], between, radius, scale) {
            out.push(Patch {
                ring,
                level: Level::Ground,
                paved: a.paved,
            });
        }
    }
    out
}

/// The ground between the right edge of an arm and the left edge of the next one, that
/// leaves the node `at` `between` radians further on.
fn fill(at: Point, a: &Side, b: &Side, between: f64, radius: f64, scale: f64) -> Vec<Vec<Point>> {
    let ring = |from: [f64; 2], to: [f64; 2], whole: bool| -> Vec<Vec<Point>> {
        let (p, p_out) = a.at(to[0]);
        let (q, q_out) = b.at(to[1]);
        // from the node, if not from where they meet; in under both
        let mut ring: Vec<Point> = if whole { vec![at] } else { Vec::new() };
        ring.extend(a.under(from[0], to[0].max(from[0]), -UNDER * scale));
        ring.push(p);
        ring.extend(curve(p, [-p_out[0], -p_out[1]], q, q_out));
        ring.push(q);
        let mut back = b.under(from[1], to[1].max(from[1]), UNDER * scale);
        back.reverse();
        ring.extend(back);
        ring.dedup();
        if ring.len() < 3 {
            return Vec::new();
        }
        // it may cross itself, where one's edge begins behind the other's
        let clean = geometry::union(&vec![vec![ring]]);
        let outer = clean.into_iter().filter_map(|s| s.into_iter().next());
        outer.filter(|r| r.len() > 2).collect()
    };
    if let Some((s, o)) = a.meets(b) {
        let (p, q) = (a.at(s + 1e-6).1, b.at(o + 1e-6).1);
        let angle = cross(p, q).atan2(dot(p, q));
        // what lies over one another has no corner
        if angle <= 0.05 {
            return Vec::new();
        }
        let part = ((angle - ANGLES[0]) / (ANGLES[1] - ANGLES[0])).clamp(0.0, 1.0);
        let radius = NOSE + (radius - NOSE).max(0.0) * part;
        // a blunt one's is longer than a square one's
        let blunt = (angle / std::f64::consts::FRAC_PI_2 - 1.0).clamp(0.0, 0.5);
        let tangent = radius * (1.0 / (angle / 2.0).tan()).max(1.0 + BLUNT * blunt);
        let room = |side: &Side, s: f64| (side.length() - s) * 0.8;
        let tangent = (tangent.min(MAX_TANGENT) * scale)
            .min(room(a, s))
            .min(room(b, o));
        let whole = s < a.back || o < b.back;
        if tangent < STEP * scale && !whole {
            return Vec::new();
        }
        let from = if whole { [a.back, b.back] } else { [s, o] };
        return ring(from, [s + tangent, o + tangent], whole);
    }
    let start = [a.back, b.back];
    let ((p, p_out), (q, q_out)) = (a.at(start[0]), b.at(start[1]));
    let off = less(p, q);
    // metres the one's corner stands out past the other's edge
    let out_of_b = (off[0] * q_out[1] - off[1] * q_out[0]) / scale;
    let out_of_a = (off[0] * p_out[1] - off[1] * p_out[0]) / scale;
    let room = |side: &Side, by: f64| {
        let room = (side.length() - side.back) / scale * 0.8;
        (by * EASED).clamp(2.0, MAX_EASED).min(room) * scale
    };
    let to = match (out_of_a > STEP, out_of_b > STEP) {
        (true, true) if distance(p, q) > GAP * scale => return Vec::new(),
        (true, true) => [NOSE * scale; 2],
        (true, false) => [room(a, out_of_a), 0.0],
        (false, true) => [0.0, room(b, out_of_b)],
        (false, false) if between > std::f64::consts::PI + 0.0045 => [0.0; 2],
        _ => return Vec::new(),
    };
    ring(start, [start[0] + to[0], start[1] + to[1]], true)
}

// Guides.

/// Metres of a bar and of a gap of a guide, and those one is long at the least and
/// at the most.
const GUIDE: f64 = 1.0;
const GUIDES: [f64; 2] = [6.0, 60.0];
/// Radians a turn with guides is off a right angle by, at the most.
const SQUARE: f64 = 0.7;
/// Metres apart that two guides are taken to begin in one place.
const BEGUN: f64 = 2.0;
/// The furthest from a junction, metres, that its signals are.
const SIGNALS: f64 = 35.0;

/// The lines between lanes that turn side by side, over the junctions with signals:
/// as bars.
pub(super) fn guides(ways: &[Way], network: &Network, signs: &FxHashMap<i64, Sign>) -> Vec<Detail> {
    let node = |run: usize, end: usize| {
        let points = &network.runs[run].points;
        let k = if end == 0 {
            points.start
        } else {
            points.end - 1
        };
        ways[network.runs[run].way as usize].nodes[k]
    };
    // the runs that lead out of each junction, and their ends there
    let mut out_of: BTreeMap<u32, Vec<(usize, usize)>> = BTreeMap::new();
    for (i, run) in network.runs.iter().enumerate() {
        let profile = &ways[run.way as usize].profile;
        // on a deck there is no more road than its lanes
        let ground = profile.level == Level::Ground;
        if run.inner || profile.lanes.is_none() || profile.class == Class::Service || !ground {
            continue;
        }
        for end in [0, 1] {
            if let Some(&junction) = network.junction_of.get(&node(i, end))
                && run.cut[end] > 0.0
            {
                out_of.entry(junction).or_default().push((i, end));
            }
        }
    }
    let mut out = Vec::new();
    // where each begins: two of one road may lead to one exit
    let mut begun: Vec<Point> = Vec::new();
    for ends in out_of.values() {
        for &(i, end) in ends {
            let run = &network.runs[i];
            let way = &ways[run.way as usize];
            let turns = match &way.profile.lanes {
                Some(lanes) if traffic(way, end)[0] => &lanes.turns[1 - end],
                _ => continue,
            };
            for left in [true, false] {
                let of = |t: &Vec<Turn>| t.iter().any(|t| t.turns(left));
                let lanes: Vec<usize> = (0..turns.len()).filter(|&k| of(&turns[k])).collect();
                if lanes.len() > 1 && signalled(ways, network, signs, ends) {
                    let bars = turn(ways, network, (i, end), ends, &lanes, left);
                    let scale = way.scale;
                    let near = |p: Point| begun.iter().any(|b| distance(*b, p) < BEGUN * scale);
                    let firsts: Vec<Point> = (bars.iter())
                        .filter(|b| b.block == bars[0].block || !near(b.strokes[0][0]))
                        .map(|b| b.strokes[0][0])
                        .collect();
                    if bars.first().is_some_and(|b| !near(b.strokes[0][0])) {
                        begun.extend(firsts);
                        out.extend(bars);
                    }
                }
            }
        }
    }
    out
}

/// Whether the junction these runs lead out of has signals.
fn signalled(
    ways: &[Way],
    network: &Network,
    signs: &FxHashMap<i64, Sign>,
    ends: &[(usize, usize)],
) -> bool {
    let signal = |node: &i64| matches!(signs.get(node), Some(Sign::Line(Kind::Signal, _)));
    ends.iter().any(|&(i, end)| {
        let run = &network.runs[i];
        let way = &ways[run.way as usize];
        let order: Vec<usize> = if end == 0 {
            run.points.clone().collect()
        } else {
            run.points.clone().rev().collect()
        };
        let mut gone = 0.0;
        order.iter().enumerate().any(|(n, &k)| {
            if n > 0 {
                gone += distance(way.points[order[n - 1]], way.points[k]) / way.scale;
            }
            gone <= SIGNALS + run.cut[end] && signal(&way.nodes[k])
        })
    })
}

/// The guides of `lanes`, from a driver's left, of the run that comes to a junction, to
/// those of the one they turn into.
fn turn(
    ways: &[Way],
    network: &Network,
    (from, end): (usize, usize),
    ends: &[(usize, usize)],
    lanes: &[usize],
    left: bool,
) -> Vec<Detail> {
    // where a run's lanes begin or end at the junction: the place and the way traffic
    // goes there, and between its lanes, from a driver's left, metres right of the line
    let mouth = |run: usize, end: usize, comes: bool| {
        let walk = Walk::new(ways, network, run);
        let cut = network.runs[run].cut[end];
        let s = if end == 0 { cut } else { walk.length() - cut };
        let along = (end == 1) == comes;
        let lanes = details::lanes_at(&walk, s.clamp(0.0, walk.length()), along);
        let between: Vec<f64> = lanes
            .windows(2)
            .map(|l| if along { l[0][1] } else { l[0][0] })
            .collect();
        let (at, d) = (walk.at(s).0, walk.direction(s, 3.0));
        let goes = if along { d } else { [-d[0], -d[1]] };
        (at, d, goes, between)
    };
    let (at, d, goes, between) = mouth(from, end, true);
    let wanted = if left { -1.0 } else { 1.0 } * std::f64::consts::FRAC_PI_2;
    let exit = ends
        .iter()
        .filter(|&&e| e != (from, end))
        .filter(|&&(i, end)| traffic(&ways[network.runs[i].way as usize], end)[1])
        .map(|&(i, end)| {
            let exit = mouth(i, end, false);
            let angle = cross(goes, exit.2).atan2(dot(goes, exit.2));
            ((angle - wanted).abs(), exit)
        })
        .filter(|(off, _)| *off < SQUARE)
        .min_by(|a, b| a.0.total_cmp(&b.0));
    let Some((_, (to, e, on, beyond))) = exit else {
        return Vec::new();
    };
    let way = &ways[network.runs[from].way as usize];
    let scale = way.scale;
    let mut out = Vec::new();
    // which line between its lanes each goes on as: the outermost as the outermost
    for pair in 0..lanes.len() - 1 {
        let (k, theirs) = if left {
            (pair, Some(pair))
        } else {
            let k = lanes.len() - 2 - pair;
            (k, beyond.len().checked_sub(pair + 1))
        };
        let beside = lanes[k + 1] == lanes[k] + 1;
        let (Some(&x), Some(&y)) = (between.get(lanes[k]), theirs.and_then(|t| beyond.get(t)))
        else {
            continue;
        };
        let (p, q) = (
            details::place(at, d, scale, [x, 0.0]),
            details::place(to, e, scale, [y, 0.0]),
        );
        let metres = distance(p, q) / scale;
        if !beside || !(GUIDES[0]..=GUIDES[1]).contains(&metres) {
            continue;
        }
        let mut line = vec![p];
        line.extend(curve(p, goes, q, on));
        line.push(q);
        let side = Side::new(&line, scale);
        let mut s = side.back + GUIDE * scale;
        while s + GUIDE * scale <= side.length() {
            let (a, b) = (side.at(s).0, side.at(s + GUIDE * scale).0);
            let length = distance(a, b) / scale;
            let half = Kind::Guide.width() / 2.0;
            let along = unit(less(b, a));
            out.push(Detail {
                way: network.runs[from].way,
                kind: Kind::Guide,
                confidence: 1,
                at: a,
                block: Block::new(a, along, scale, [-half, half], [0.0, length]),
                strokes: vec![vec![a, b]],
                heads: Vec::new(),
            });
            s += 2.0 * GUIDE * scale;
        }
    }
    out
}

// What is mapped of them.

/// The discs of surface at the ends of roads where one turns round (`turning`, by
/// node, with the metres across it if they are tagged).
pub(super) fn turning(ways: &[Way], turning: &FxHashMap<i64, Option<f64>>) -> Vec<Patch> {
    /// Metres of its radius where no tag says, at the least and at the most.
    const RADIUS: [f64; 2] = [5.0, 8.0];
    const AROUND: u32 = 32;
    let mut drawn: FxHashSet<i64> = FxHashSet::default();
    let mut out = Vec::new();
    for way in ways.iter().filter(|w| w.profile.class.is_motor()) {
        for (node, at) in way.nodes.iter().zip(&way.points) {
            let Some(across) = turning.get(node) else {
                continue;
            };
            if way.profile.level != Level::Ground || !drawn.insert(*node) {
                continue;
            }
            let usual = way.profile.width.clamp(RADIUS[0], RADIUS[1]);
            let radius = across.map_or(usual, |d| d / 2.0) * way.scale;
            out.push(Patch {
                ring: (0..AROUND)
                    .map(|k| {
                        let turn = f64::from(k) / f64::from(AROUND) * std::f64::consts::TAU;
                        [at[0] + radius * turn.cos(), at[1] + radius * turn.sin()]
                    })
                    .collect(),
                level: Level::Ground,
                paved: way.profile.paved,
            });
        }
    }
    out
}

/// The carriageways that are mapped as areas, by where.
pub(super) struct Given<'a> {
    list: Vec<&'a Area>,
    cells: FxHashMap<(i64, i64), Vec<u32>>,
}

/// A stretch of a way whose carriageway is mapped.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Stretch {
    /// Metres along the way.
    pub span: [f64; 2],
    /// Those of it whose lanes it holds: there is no more of the road.
    pub held: Vec<[f64; 2]>,
    /// The edges the way's surface comes to at its start and its end, metres right of
    /// its line; none if they are its own.
    pub edges: [Option<[f64; 2]>; 2],
}

impl Stretch {
    /// The stretches of a way that its own surface is drawn on: those not held, and in
    /// under what is, as its edge may run at an angle the surface's end does not.
    pub(super) fn left(stretches: &[Stretch], way: &Way) -> Vec<[f64; 2]> {
        let whole = super::lanes::length(way);
        let held: Vec<[f64; 2]> = stretches.iter().flat_map(|s| s.held.clone()).collect();
        let under = |at: f64| -> f64 {
            let of = held.iter().find(|s| s[0] <= at && at <= s[1]);
            of.map_or(0.0, |s| (way.profile.width / 2.0).min((s[1] - s[0]) / 2.0))
        };
        let left = super::paint::left(whole, &held).into_iter();
        left.map(|[from, to]| [(from - under(from)).max(0.0), (to + under(to)).min(whole)])
            .collect()
    }
}

/// World units of a cell.
const CELL: f64 = 1024.0;

fn cells(low: Point, high: Point) -> impl Iterator<Item = (i64, i64)> {
    let cell = |v: f64| (v / CELL).floor() as i64;
    let (x, y) = (cell(low[0])..=cell(high[0]), cell(low[1])..=cell(high[1]));
    x.flat_map(move |x| y.clone().map(move |y| (x, y)))
}

fn bounds(points: &[Point]) -> (Point, Point) {
    let of = |f: fn(f64, f64) -> f64, p: Point, q: Point| [f(p[0], q[0]), f(p[1], q[1])];
    let all = points.iter().copied();
    (
        all.clone().fold([f64::MAX; 2], |p, q| of(f64::min, p, q)),
        all.fold([f64::MIN; 2], |p, q| of(f64::max, p, q)),
    )
}

/// Whether a ring goes round `p`.
pub(super) fn inside(ring: &[Point], p: Point) -> bool {
    let mut within = false;
    for i in 0..ring.len() {
        let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
        let crosses = (a[1] > p[1]) != (b[1] > p[1]);
        if crosses && p[0] < a[0] + (b[0] - a[0]) * (p[1] - a[1]) / (b[1] - a[1]) {
            within = !within;
        }
    }
    within
}

impl<'a> Given<'a> {
    pub(super) fn new(areas: &'a [Area]) -> Self {
        let list: Vec<&Area> = areas.iter().filter(|a| a.kind == AreaKind::Road).collect();
        let mut by: FxHashMap<(i64, i64), Vec<u32>> = FxHashMap::default();
        for (i, area) in list.iter().enumerate() {
            let (low, high) = bounds(&area.ring);
            for cell in cells(low, high) {
                by.entry(cell).or_default().push(i as u32);
            }
        }
        Self { list, cells: by }
    }

    pub(super) fn holds(&self, p: Point) -> bool {
        let cell = cells(p, p).next().and_then(|c| self.cells.get(&c));
        cell.into_iter().flatten().any(|&i| {
            let area = self.list[i as usize];
            inside(&area.ring, p) && !area.holes.iter().any(|h| inside(h, p))
        })
    }

    /// The stretches of each way whose carriageway is mapped.
    pub(super) fn stretches(&self, ways: &[Way], shapes: &Shapes) -> FxHashMap<u32, Vec<Stretch>> {
        /// Metres between places looked at, and of a stretch that counts and one that is
        /// held.
        const EVERY: f64 = 1.0;
        const LEAST: [f64; 2] = [3.0, 6.0];
        /// Metres its own edges may be outside of what is mapped.
        const NEAR: f64 = 1.0;
        /// Metres in from where it begins that its edges are looked for, how finely, how
        /// far, and how much further out and in than the way's own they are believed.
        const IN: f64 = 3.0;
        const FINE: f64 = 0.25;
        const FAR: f64 = 25.0;
        const OFF: [f64; 2] = [5.0, 3.5];
        let joined = |list: &mut Vec<[f64; 2]>, [from, to]: [f64; 2]| match list.last_mut() {
            Some(last) if from - last[1] < EVERY / 10.0 => last[1] = to,
            _ => list.push([from, to]),
        };
        let of = |index: u32, way: &Way| -> [Vec<[f64; 2]>; 2] {
            let (mut spans, mut held) = (Vec::new(), Vec::new());
            let mut gone = 0.0;
            for p in way.points.windows(2) {
                let length = distance(p[0], p[1]) / way.scale;
                let (low, high) = bounds(p);
                if cells(low, high).any(|c| self.cells.contains_key(&c)) {
                    let steps = (length / EVERY).ceil().max(1.0);
                    let d = unit(less(p[1], p[0]));
                    let right = [-d[1], d[0]];
                    for k in 0..steps as u32 {
                        let span = [0.0, 1.0].map(|o| gone + (f64::from(k) + o) / steps * length);
                        let middle = (span[0] + span[1]) / 2.0;
                        let at = onward(p[0], d, (middle - gone) * way.scale);
                        if !self.holds(at) {
                            continue;
                        }
                        joined(&mut spans, span);
                        // all of it, as near as that can be told
                        let [left_edge, right_edge] = shapes.at(ways, index, middle).surface;
                        let mut sides = [left_edge + NEAR, right_edge - NEAR].into_iter();
                        if sides.all(|x| self.holds(onward(at, right, x * way.scale))) {
                            joined(&mut held, span);
                        }
                    }
                }
                gone += length;
            }
            let lists = [spans, held].into_iter().zip(LEAST);
            let mut lists = lists.map(|(mut list, least)| {
                list.retain(|s: &[f64; 2]| s[1] - s[0] >= least);
                list
            });
            [(); 2].map(|()| lists.next().expect("there are two"))
        };
        let edges = |index: u32, way: &Way, s: f64, held: bool| -> Option<[f64; 2]> {
            let own = shapes.at(ways, index, s).surface;
            let whole = super::lanes::length(way);
            let trim = [s - 0.1, whole - s - 0.1].map(|m| m.max(0.0) * way.scale);
            let points = geometry::trim(&way.points, trim)?;
            let (a, b) = (points[0], points[points.len() - 1]);
            let d = unit(less(b, a));
            let believed = [0, 1].map(|i| {
                let sign = [-1.0, 1.0][i];
                let mut steps = (1..(FAR / FINE) as u32).map(|k| f64::from(k) * FINE);
                let out = |by: f64| !self.holds(onward(a, [-d[1], d[0]], sign * by * way.scale));
                let further = steps.find(|&by| out(by)).unwrap_or(f64::MAX) - own[i] * sign;
                let in_by = if held { OFF[1] } else { 0.0 };
                if (-in_by..=OFF[0]).contains(&further) {
                    own[i] + further * sign
                } else {
                    own[i]
                }
            });
            let wide = believed[1] - believed[0] >= NARROWEST;
            (wide && believed != own).then_some(believed)
        };
        let motor = |w: &Way| w.profile.class.is_motor() && w.profile.level == Level::Ground;
        (ways.par_iter().enumerate())
            .filter(|(_, w)| !self.list.is_empty() && motor(w))
            .map(|(i, w)| {
                let whole = super::lanes::length(w);
                let [spans, held] = of(i as u32, w);
                let stretches = spans.into_iter().map(|span| {
                    let near = IN.min((span[1] - span[0]) / 2.0);
                    let inner = [span[0] + near, span[1] - near];
                    let ends = [span[0] < LEAST[0], whole - span[1] < LEAST[0]];
                    let within = |s: f64| held.iter().any(|h| h[0] <= s && s <= h[1]);
                    Stretch {
                        edges: [0, 1].map(|e| {
                            let found = edges(i as u32, w, inner[e], within(inner[e]));
                            found.filter(|_| !ends[e])
                        }),
                        held: (held.iter().copied())
                            .filter(|h| span[0] <= h[0] && h[1] <= span[1])
                            .collect(),
                        span,
                    }
                });
                (i as u32, stretches.collect::<Vec<_>>())
            })
            .filter(|(_, s)| !s.is_empty())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::super::details::Marking;
    use super::super::network::{build, build_at};
    use super::super::profile::profile;
    use super::super::{lanes, network};
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

    fn one_way(lanes: &'static str, turns: &'static str) -> Vec<(&'static str, &'static str)> {
        vec![
            ("highway", "primary"),
            ("oneway", "yes"),
            ("lanes", lanes),
            ("turn:lanes", turns),
        ]
    }

    fn surface(ways: &[Way], n: &Network) -> Vec<Vec<Vec<Point>>> {
        let rect = [-1e3, -1e3, 1e3, 1e3];
        let mut ground: Vec<Vec<Vec<Point>>> =
            n.patches.iter().map(|p| vec![p.ring.clone()]).collect();
        for (i, w) in ways.iter().enumerate() {
            let (tapers, taken) = n.shapes.tapers(ways, i as u32, 0.25, rect);
            ground.extend(tapers);
            if let Some(points) = geometry::trim(&w.points, taken) {
                let middle = geometry::offset(&points, w.profile.centre);
                let middle = geometry::extend(&middle, 0.25);
                ground.extend(geometry::ribbon(&middle, w.profile.width));
            }
        }
        let mut out = geometry::union(&ground);
        // less than a tile tells
        geometry::close(&mut out, 1.0, 0.01, (0.0, 0.0));
        out
    }

    /// Eastwards past a road to the south at (0, 0).
    fn straight_over() -> Vec<Way> {
        let mut ways = vec![
            way(
                &one_way("3", "left|through|through"),
                &[(1, -400.0, 0.0), (2, 0.0, 0.0)],
            ),
            way(&one_way("2", ""), &[(2, 0.0, 0.0), (3, 400.0, 0.0)]),
            way(PRIMARY, &[(2, 0.0, 0.0), (5, 0.0, 200.0)]),
        ];
        lanes::place(&mut ways);
        ways
    }

    #[test]
    fn what_goes_straight_over_lines_up_and_has_no_step() {
        let ways = straight_over();
        let (found, within) = over(&ways, &[]);
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].ends, [(0, 1), (1, 0)]);
        assert!(within.is_empty());
        let n = build(&ways);
        let at = |way: u32, s: f64| n.shapes.at(&ways, way, s);
        let (before, after) = (at(0, 400.0), at(1, 0.0));
        for e in 0..2 {
            assert!((before.surface[e] - after.surface[e]).abs() < 1e-9);
        }
        // at the mouths the lanes that go on are in line, and the turn's is whole
        let (p, q) = (at(0, 400.0 - 6.5), at(1, 6.5));
        assert!((p.lanes[1] - q.lanes[0]).abs() < 0.07, "{p:?} {q:?}");
        assert!((p.lanes[3] - q.lanes[2]).abs() < 0.07, "{p:?} {q:?}");
        assert!((p.surface[1] - p.surface[0] - 10.5).abs() < 1e-6);
        let surface = surface(&ways, &n);
        assert_eq!((surface.len(), surface[0].len()), (1, 1));
        // it is drawn in over both mouths
        let (tapers, taken) = n.shapes.tapers(&ways, 0, 0.25, [-1e3, -1e3, 1e3, 1e3]);
        let ring = &geometry::union(&tapers)[0][0];
        for k in 0..ring.len() {
            let (p, q) = (ring[k], ring[(k + 1) % ring.len()]);
            let (along, across) = ((q[0] - p[0]).abs(), (q[1] - p[1]).abs());
            let within = |x: f64| (0.1 - taken[1]..-0.1).contains(&x);
            // the side where none joins it
            let edge = within(p[0]) && within(q[0]) && p[1] < 0.0 && q[1] < 0.0;
            assert!(!edge || across < along * 0.6, "{p:?} {q:?}");
        }
    }

    #[test]
    fn roads_are_one_by_class_or_name_and_by_their_traffic() {
        let ends = |a: &[(&str, &'static str)], b: &[(&str, &'static str)], bend: f64| {
            let ways = [
                way(a, &[(1, -100.0, 0.0), (2, 0.0, 0.0)]),
                way(b, &[(2, 0.0, 0.0), (3, 100.0, bend)]),
                way(PRIMARY, &[(4, 0.0, -100.0), (2, 0.0, 0.0), (5, 0.0, 100.0)]),
            ];
            over(&ways, &[]).0.len()
        };
        let named =
            |class: &'static str, name: &'static str| vec![("highway", class), ("name", name)];
        let (a, b) = (named("secondary", "A"), named("tertiary", "A"));
        assert_eq!(ends(&a, &a, 0.0), 1);
        assert_eq!(ends(&a, &b, 40.0), 1);
        assert_eq!(ends(&a, &named("tertiary", "B"), 40.0), 0);
        assert_eq!(ends(&a, &named("tertiary", "B"), 5.0), 1);
        assert_eq!(ends(&a, &a, 80.0), 0);
        // two that both come to it
        let out = [("highway", "secondary"), ("oneway", "yes")];
        let back = [("highway", "secondary"), ("oneway", "-1")];
        assert_eq!(ends(&out, &out, 0.0), 1);
        assert_eq!(ends(&out, &back, 0.0), 0);
    }

    #[test]
    fn a_road_that_ends_at_one_set_to_the_side_leaves_no_slit() {
        // the way lies left of its lanes, the side road is on its left
        let mut tags = one_way("2", "");
        tags.push(("placement", "left_of:1"));
        let ways = [
            way(&tags, &[(1, -100.0, 0.0), (2, 0.0, 0.0), (3, 100.0, 0.0)]),
            way(PRIMARY, &[(2, 0.0, 0.0), (4, 0.0, -100.0)]),
        ];
        let n = build(&ways);
        let one = surface(&ways, &n);
        assert_eq!((one.len(), one[0].len()), (1, 1));
        tags.pop();
        tags.push(("placement", "left_of:1"));
        let mut off = ways;
        // and further still
        off[0].profile.shift(0.8);
        let n = build(&off);
        let one = surface(&off, &n);
        assert_eq!((one.len(), one[0].len()), (1, 1));
    }

    #[test]
    fn what_ends_at_a_road_does_not_stand_out_past_it() {
        let ways = [
            way(
                &[("highway", "tertiary"), ("lanes", "1"), ("oneway", "yes")],
                &[(1, -100.0, 0.0), (2, 0.0, 0.0), (3, 100.0, 0.0)],
            ),
            way(
                &[("highway", "primary"), ("lanes", "4")],
                &[(2, 0.0, 0.0), (4, 100.0, 60.0)],
            ),
        ];
        let (_, within) = over(&ways, &[]);
        assert_eq!(within.len(), 1);
        let n = build(&ways);
        let at = |s: f64| n.shapes.at(&ways, 1, s).surface;
        // its left corner would be 4 m past the road's line, the road's edge is 1.5
        assert!(at(0.0)[0] > -3.0 && (at(30.0)[0] + 7.0).abs() < 1e-9);
        let one = surface(&ways, &n);
        assert_eq!((one.len(), one[0].len()), (1, 1));
        assert!(one[0][0].iter().all(|p| p[1] > -1.6), "{one:?}");
    }

    /// A divided road, its carriageways `apart` metres, over another road.
    fn divided(apart: f64) -> Vec<Way> {
        vec![
            way(
                &one_way("2", ""),
                &[(1, -100.0, 0.0), (2, 0.0, 0.0), (3, 100.0, 0.0)],
            ),
            way(
                &one_way("2", ""),
                &[(4, 100.0, -apart), (5, 0.0, -apart), (6, -100.0, -apart)],
            ),
            way(
                PRIMARY,
                &[
                    (7, 0.0, -100.0 - apart),
                    (5, 0.0, -apart),
                    (2, 0.0, 0.0),
                    (8, 0.0, 100.0),
                ],
            ),
        ]
    }

    #[test]
    fn a_divided_roads_crossing_is_one_junction() {
        let ways = divided(14.0);
        let n = build(&ways);
        assert_eq!((n.junctions, n.clusters), (1, 1));
        assert_eq!(n.junction_of[&2], n.junction_of[&5]);
        let inner: Vec<_> = n.runs.iter().filter(|r| r.inner).collect();
        assert_eq!(inner.len(), 1);
        assert_eq!(inner[0].points, 1..3);
        assert!(inner[0].cut[0] + inner[0].cut[1] > 14.0);
        // signals at a node within are no line within
        let signs = [(2, Sign::Line(Kind::Signal, details::Facing::Unsaid))];
        let d = details::build(
            &ways,
            &n,
            &signs.into_iter().collect(),
            &FxHashMap::default(),
            &FxHashSet::default(),
            &FxHashSet::default(),
        );
        // from the west, and from the south; nothing at what leads out
        assert_eq!(d.list.len(), 2, "{:?}", d.list);
        for line in &d.list {
            let p = line.strokes[0][0];
            assert!(p[0] < -6.0 || p[1] > 6.0, "{line:?}");
        }
        // far apart they are two
        let n = build(&divided(40.0));
        assert_eq!((n.junctions, n.clusters), (2, 0));
        assert!(n.runs.iter().all(|r| !r.inner));
    }

    #[test]
    fn two_divided_roads_crossing_is_road_but_where_there_is_room() {
        let square = |apart: f64| {
            let mut ways = divided(apart);
            ways.pop();
            let cross = |x: f64, ids: [i64; 2], south: bool| {
                let mut nodes = vec![
                    (ids[0], x, -100.0 - apart),
                    (ids[0] + 2, x, -apart),
                    (ids[1] + 2, x, 0.0),
                    (ids[1], x, 100.0),
                ];
                if !south {
                    nodes.reverse();
                }
                way(&one_way("2", ""), &nodes)
            };
            ways.push(cross(0.0, [20, 30], true));
            ways.push(cross(apart, [40, 50], false));
            // the nodes the carriageways share
            ways[0].nodes = vec![1, 32, 52, 3];
            ways[0].points = vec![[-100.0, 0.0], [0.0, 0.0], [apart, 0.0], [130.0, 0.0]];
            ways[1].nodes = vec![4, 42, 22, 6];
            ways[1].points = vec![
                [130.0, -apart],
                [apart, -apart],
                [0.0, -apart],
                [-100.0, -apart],
            ];
            let n = build(&ways);
            assert_eq!((n.junctions, n.clusters), (1, 1));
            surface(&ways, &n)[0].len() - 1
        };
        // 7 m wide each: 5 m between are road, and 11 stay open
        assert_eq!(square(12.0), 0);
        assert_eq!(square(18.0), 1);
    }

    #[test]
    fn a_roundabouts_nodes_are_each_their_own() {
        let ring: Vec<(i64, f64, f64)> = (0..=16)
            .map(|k| {
                let a = f64::from(k) / 16.0 * std::f64::consts::TAU;
                (10 + i64::from(k % 16), 8.0 * a.cos(), 8.0 * a.sin())
            })
            .collect();
        let round = [("highway", "primary"), ("junction", "roundabout")];
        let ways = [
            way(&round, &ring),
            way(PRIMARY, &[(1, 90.0, 0.0), (10, 8.0, 0.0)]),
            way(PRIMARY, &[(2, 0.0, 90.0), (14, 0.0, 8.0)]),
        ];
        let n = build(&ways);
        assert_eq!((n.junctions, n.clusters), (2, 0));
        // its middle stays open
        assert_eq!(surface(&ways, &n)[0].len(), 2);
    }

    #[test]
    fn guides_lead_lanes_that_turn_side_by_side_over_signals() {
        let ways = [
            way(
                &one_way("4", "left|left|through|right"),
                &[(1, -200.0, 0.0), (2, 0.0, 0.0)],
            ),
            way(
                &[("highway", "primary"), ("lanes", "4")],
                &[(4, 0.0, -200.0), (2, 0.0, 0.0), (5, 0.0, 200.0)],
            ),
        ];
        let guided = |signal: bool| {
            let signs: FxHashMap<i64, Sign> = signal
                .then_some((2, Sign::Line(Kind::Signal, details::Facing::Unsaid)))
                .into_iter()
                .collect();
            let n = build_at(&ways, &|node| signal && node == 2);
            guides(&ways, &n, &signs)
        };
        assert!(guided(false).is_empty());
        let bars = guided(true);
        assert!(bars.len() > 4, "{}", bars.len());
        // from between the two to between the two that go north
        let (first, last) = (&bars[0].strokes[0], &bars[bars.len() - 1].strokes[0]);
        assert!(
            first[0][0] < -8.5 && (first[0][1] + 3.5).abs() < 0.3,
            "{first:?}"
        );
        assert!(
            (last[1][0] - 3.5).abs() < 0.5 && last[1][1] < -7.0,
            "{last:?}"
        );
    }

    #[test]
    fn an_island_stands_in_a_crossing_where_there_is_room() {
        let zebra = Sign::Crossing(Some(Marking::Marked(Kind::Zebra, 2)));
        let of = |tags: &[(&str, &'static str)]| {
            let ways = [way(
                tags,
                &[(1, -100.0, 0.0), (2, 0.0, 0.0), (3, 100.0, 0.0)],
            )];
            let n = build(&ways);
            let d = details::build(
                &ways,
                &n,
                &[(2, zebra)].into_iter().collect(),
                &FxHashMap::default(),
                &[2].into_iter().collect(),
                &FxHashSet::default(),
            );
            (d.refuges, d.list)
        };
        let (islands, list) = of(PRIMARY);
        assert_eq!((islands.len(), list[0].strokes.len()), (1, 2));
        let reach = |i: usize| {
            let v = islands[0].ring.iter().map(move |p| p[i].abs());
            v.fold(0.0, f64::max)
        };
        // 7 m: 2 m of island, and bars either side of it
        assert!((reach(0) - 3.0).abs() < 1e-9 && (reach(1) - 1.0).abs() < 1e-9);
        for stroke in &list[0].strokes {
            assert!(stroke.iter().all(|p| p[1].abs() > 1.2), "{stroke:?}");
        }
        assert!(of(&[("highway", "residential")]).0.is_empty());
        assert!(of(&one_way("3", "")).0.is_empty());
    }

    #[test]
    fn a_mini_roundabout_is_a_disc_and_a_turning_circle_is_road() {
        let ways = [
            way(PRIMARY, &[(1, -100.0, 0.0), (2, 0.0, 0.0), (3, 100.0, 0.0)]),
            way(
                &[("highway", "residential")],
                &[(2, 0.0, 0.0), (4, 0.0, 60.0)],
            ),
        ];
        let n = build(&ways);
        let d = details::build(
            &ways,
            &n,
            &[(2, Sign::Mini)].into_iter().collect(),
            &FxHashMap::default(),
            &FxHashSet::default(),
            &FxHashSet::default(),
        );
        assert_eq!((d.list.len(), d.list[0].kind), (1, Kind::Disc));
        // a quarter of the narrower road's 5.5 m
        let far = d.list[0].heads[0].iter().map(|p| distance(*p, [0.0, 0.0]));
        assert!((far.fold(0.0, f64::max) - 1.375).abs() < 1e-9);
        let discs = turning(&ways, &[(4, None), (9, None)].into_iter().collect());
        assert_eq!(discs.len(), 1);
        assert!((distance(discs[0].ring[0], [0.0, 60.0]) - 5.5).abs() < 1e-9);
        let tagged = turning(&ways, &[(4, Some(20.0))].into_iter().collect());
        assert!((distance(tagged[0].ring[0], [0.0, 60.0]) - 10.0).abs() < 1e-9);
    }

    #[test]
    fn a_carriageway_that_is_mapped_is_told_by_the_ways_in_it() {
        let area = Area {
            kind: AreaKind::Road,
            paved: true,
            ring: vec![[20.0, -9.0], [60.0, -9.0], [60.0, 9.0], [20.0, 9.0]],
            holes: vec![vec![[45.0, -2.0], [45.0, 2.0], [50.0, 2.0], [50.0, -2.0]]],
        };
        let areas = [area];
        let given = Given::new(&areas);
        assert!(given.holds([30.0, 0.0]) && !given.holds([47.0, 0.0]));
        let ways = [
            way(PRIMARY, &[(1, 0.0, 0.0), (2, 100.0, 0.0)]),
            way(PRIMARY, &[(3, 0.0, 30.0), (4, 100.0, 30.0)]),
        ];
        let n = network::build(&ways);
        let found = given.stretches(&ways, &n.shapes);
        assert_eq!(found.len(), 1);
        let near =
            |a: [f64; 2], b: [f64; 2]| (a[0] - b[0]).abs() < 1.1 && (a[1] - b[1]).abs() < 1.1;
        let of = &found[&0];
        assert!(near(of[0].span, [20.0, 45.0]) && near(of[1].span, [50.0, 60.0]));
        // 9 m either side is no edge of a road of 3.5 m
        assert_eq!((of[0].edges[0], of[0].held.len()), (None, 1));
        let left = Stretch::left(of, &ways[0]);
        assert_eq!(left.len(), 3);
        assert!(near(left[0], [0.0, 23.5]) && near(left[2], [56.5, 100.0]));
        // one that is near is come to
        let mut areas = areas;
        areas[0].ring = vec![[20.0, -5.0], [60.0, -5.0], [60.0, 4.0], [20.0, 4.0]];
        let given = Given::new(&areas);
        let mut n = n;
        n.shapes.given = given.stretches(&ways, &n.shapes);
        let at = |s: f64| n.shapes.at(&ways, 0, s).surface;
        assert!(near(at(19.9), [-5.0, 4.0]) && at(7.0) == [-3.5, 3.5]);
        assert!(at(14.0)[0] < -3.6 && at(14.0)[0] > -4.9);
        // and what does not hold its lanes is come to where it is wider, and leaves them
        areas[0].ring = vec![[20.0, -6.0], [60.0, -6.0], [60.0, 1.0], [20.0, 1.0]];
        areas[0].holes.clear();
        let given = Given::new(&areas);
        n.shapes.given = Default::default();
        let of = &given.stretches(&ways, &n.shapes)[&0];
        assert_eq!((of.len(), of[0].held.len()), (1, 0));
        assert!(near(of[0].edges[0].unwrap(), [-6.0, 3.5]));
        assert_eq!(Stretch::left(of, &ways[0]), [[0.0, 100.0]]);
    }
}
