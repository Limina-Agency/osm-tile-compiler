//! Roads split where they meet, and how far from a junction each one's
//! markings stop. Positions are world units: z16 tiles of 4096 units, so the
//! plane of every tile at once.

use super::profile::{Class, Level, Profile};
use rustc_hash::FxHashMap;

pub(super) type Point = [f64; 2];

pub(super) struct Way {
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
}

/// What a node adds to the ribbons that meet at it: the ground a rounded
/// corner takes in, or the wedge outside a bend.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Patch {
    pub ring: Vec<Point>,
    pub level: Level,
    pub paved: bool,
}

#[derive(Default)]
pub(super) struct Network {
    pub edges: Vec<Edge>,
    /// Where three or more arms of motor roads meet.
    pub junctions: u64,
    pub patches: Vec<Patch>,
}

struct Arm {
    edge: u32,
    end: usize,
    /// Away from the junction, a unit vector.
    out: Point,
    /// Metres from the way's line to its surface's edge, left and right of `out`.
    half: [f64; 2],
    /// Metres it runs straight.
    straight: f64,
    class: Class,
    level: Level,
    paved: bool,
}

/// Metres a corner is rounded by, and markings kept back from it.
pub(super) const CORNER: f64 = 3.0;
/// The most a junction keeps markings back, metres: past it the arms run side
/// by side and their edges never clear each other.
const MAX_CUT: f64 = 30.0;
/// Metres along an arm its direction is taken over.
const ARM_LENGTH: f64 = 12.0;

/// The ways must be in an order of their own (by id), which fixes the edges'.
pub(super) fn build(ways: &[Way]) -> Network {
    // arms at each node: an end is one, a point within a way two
    let mut uses: FxHashMap<i64, u32> = FxHashMap::default();
    for way in ways.iter().filter(|w| w.profile.class.is_motor()) {
        let last = way.nodes.len() - 1;
        for (i, &node) in way.nodes.iter().enumerate() {
            *uses.entry(node).or_default() += if i == 0 || i == last { 1 } else { 2 };
        }
    }
    let mut network = Network::default();
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
            let edge = network.edges.len() as u32;
            let points = from..to + 1;
            for (end, node) in [(0, way.nodes[from]), (1, way.nodes[to])] {
                if uses[&node] >= 2 {
                    let arm = arm(way, edge, end, &points);
                    let at = if end == 0 { from } else { to };
                    let list = arms.entry(node).or_default();
                    if list.is_empty() {
                        order.push((node, way.points[at], way.scale));
                    }
                    list.push(arm);
                }
            }
            network.edges.push(Edge {
                way: index as u32,
                points,
                cut: [0.0; 2],
            });
            from = to;
        }
    }
    for (node, at, scale) in order {
        let arms = &arms[&node];
        network.patches.extend(wedges(arms, at, scale));
        if uses[&node] < 3 {
            continue;
        }
        for (edge, end, cut) in cuts(arms) {
            network.edges[edge as usize].cut[end] = cut;
        }
        network.junctions += 1;
        network.patches.extend(corners(arms, at, scale));
    }
    network
}

fn arm(way: &Way, edge: u32, end: usize, points: &std::ops::Range<usize>) -> Arm {
    let run = &way.points[points.clone()];
    let outward: Vec<Point> = if end == 0 {
        run.to_vec()
    } else {
        run.iter().rev().copied().collect()
    };
    let start = outward[0];
    let far = outward
        .iter()
        .find(|p| distance(start, **p) >= ARM_LENGTH * way.scale)
        .or(outward.last())
        .copied()
        .unwrap_or(start);
    let length = distance(start, far).max(f64::MIN_POSITIVE);
    let out = [(far[0] - start[0]) / length, (far[1] - start[1]) / length];
    let (left, right) = (
        way.profile.width / 2.0 - way.profile.centre,
        way.profile.width / 2.0 + way.profile.centre,
    );
    Arm {
        edge,
        end,
        out,
        half: if end == 0 {
            [left.max(0.0), right.max(0.0)]
        } else {
            [right.max(0.0), left.max(0.0)]
        },
        straight: straight(&outward, out, way.scale),
        class: way.profile.class,
        level: way.profile.level,
        paved: way.profile.paved,
    }
}

/// How far each arm's markings stop from the node: where its edges clear its
/// neighbours', and the corner's rounding. A driveway does not break the
/// markings of the road it opens onto.
fn cuts(arms: &[Arm]) -> Vec<(u32, usize, f64)> {
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
        if others.len() < 2 {
            cuts.push((arm.edge, arm.end, 0.0));
            continue;
        }
        let mut cut = 0.0_f64;
        for other in others {
            // World y grows southwards, so a positive cross is clockwise: right.
            let cross = arm.out[0] * other.out[1] - arm.out[1] * other.out[0];
            let dot = arm.out[0] * other.out[0] + arm.out[1] * other.out[1];
            let (own, facing) = if cross > 0.0 {
                (arm.half[1], other.half[0])
            } else {
                (arm.half[0], other.half[1])
            };
            let clear = if cross.abs() > 1e-6 {
                (own * dot + facing) / cross.abs()
            } else if dot > 0.0 {
                MAX_CUT
            } else {
                0.0
            };
            cut = cut.max(clear);
        }
        let cut = if cut > 0.0 { cut + CORNER } else { 0.0 };
        cuts.push((arm.edge, arm.end, cut.min(MAX_CUT)));
    }
    cuts
}

/// Metres `points` keep within half a metre of the line `out` from the first.
fn straight(points: &[Point], out: Point, scale: f64) -> f64 {
    let start = points[0];
    let mut reach = 0.0;
    for p in points {
        let (dx, dy) = (p[0] - start[0], p[1] - start[1]);
        let along = dx * out[0] + dy * out[1];
        if (dx * out[1] - dy * out[0]).abs() > 0.5 * scale || along < reach {
            break;
        }
        reach = along;
    }
    reach / scale
}

const DISC_SEGMENTS: u32 = 16;

/// A disc where arms of one level meet at a bend, as wide as the second widest
/// of them: it fills the wedge outside the bend and stands out past no road
/// that goes on.
fn wedges(arms: &[Arm], at: Point, scale: f64) -> Vec<Patch> {
    let mut kinds: Vec<(Level, bool)> = arms.iter().map(|a| (a.level, a.paved)).collect();
    kinds.sort_unstable();
    kinds.dedup();
    let mut patches = Vec::new();
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

/// Angles between two arms, radians, outside which their corner is not rounded:
/// side by side they have none, and nearly in line it is none to speak of.
const CORNER_ANGLES: std::ops::RangeInclusive<f64> = 0.45..=2.8;
/// The furthest from its point a rounded corner may begin, metres.
const MAX_TANGENT: f64 = 12.0;
const CORNER_SEGMENTS: u32 = 5;

/// The rounded corners between each arm and the next one clockwise, where both
/// are on the ground and alike.
fn corners(arms: &[Arm], at: Point, scale: f64) -> Vec<Patch> {
    let angle = |arm: &Arm| arm.out[1].atan2(arm.out[0]);
    let mut order: Vec<&Arm> = arms.iter().filter(|a| a.level == Level::Ground).collect();
    order.sort_by(|a, b| angle(a).total_cmp(&angle(b)).then(a.edge.cmp(&b.edge)));
    let mut corners = Vec::new();
    if order.len() < 2 {
        return corners;
    }
    for (i, a) in order.iter().enumerate() {
        let b = order[(i + 1) % order.len()];
        if a.paved != b.paved {
            continue;
        }
        let between = (angle(b) - angle(a)).rem_euclid(std::f64::consts::TAU);
        if !CORNER_ANGLES.contains(&between) {
            continue;
        }
        let small = a.class == Class::Service || b.class == Class::Service;
        let radius = if small { CORNER / 2.0 } else { CORNER };
        // World y grows southwards, so clockwise is to the right: a's right
        // edge faces b's left.
        let (sin, cos) = between.sin_cos();
        let (ha, hb) = (a.half[1], b.half[0]);
        let (sa, sb) = ((hb + ha * cos) / sin, (ha + hb * cos) / sin);
        let tangent = radius / (between / 2.0).tan();
        if tangent > MAX_TANGENT || sa + tangent > a.straight || sb + tangent > b.straight {
            continue;
        }
        let point = |along: &Arm, s: f64, side: Point, h: f64| {
            [
                at[0] + (along.out[0] * s + side[0] * h) * scale,
                at[1] + (along.out[1] * s + side[1] * h) * scale,
            ]
        };
        let right = [-a.out[1], a.out[0]];
        let left = [b.out[1], -b.out[0]];
        let (from, to) = (
            point(a, sa + tangent, right, ha),
            point(b, sb + tangent, left, hb),
        );
        // the arc's centre lies on the bisector, its radius off both edges
        let centre = [
            from[0] + right[0] * radius * scale,
            from[1] + right[1] * radius * scale,
        ];
        let mut ring = vec![at, from];
        let start = (from[1] - centre[1]).atan2(from[0] - centre[0]);
        let turn = {
            let end = (to[1] - centre[1]).atan2(to[0] - centre[0]);
            let turn = (end - start).rem_euclid(std::f64::consts::TAU);
            if turn > std::f64::consts::PI {
                turn - std::f64::consts::TAU
            } else {
                turn
            }
        };
        for k in 1..CORNER_SEGMENTS {
            let (s, c) = (start + turn * f64::from(k) / f64::from(CORNER_SEGMENTS)).sin_cos();
            ring.push([
                centre[0] + c * radius * scale,
                centre[1] + s * radius * scale,
            ]);
        }
        ring.push(to);
        corners.push(Patch {
            ring,
            level: Level::Ground,
            paved: a.paved,
        });
    }
    corners
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
            let [from, to] = [corner.ring[1], corner.ring[corner.ring.len() - 1]];
            // the edges are 3.5 m off each line, the arc begins 3 m further out
            let ends = [from, to].map(|p| [p[0].abs(), p[1].abs()]);
            assert!(
                ends.iter().any(|p| distance(*p, [6.5, 3.5]) < 1e-9),
                "{ends:?}"
            );
            assert!(
                ends.iter().any(|p| distance(*p, [3.5, 6.5]) < 1e-9),
                "{ends:?}"
            );
            for p in &corner.ring[1..] {
                // within the corner's square, outside the arc's circle
                assert!(p[1] >= 3.5 - 1e-9 && p[0].abs() >= 3.5 - 1e-9);
                assert!(distance([p[0].abs(), p[1]], [6.5, 6.5]) > 3.0 - 1e-9);
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
        // a short arm has no room for one
        let short = [
            way(PRIMARY, &[(1, -100.0, 0.0), (2, 0.0, 0.0), (3, 100.0, 0.0)]),
            way(PRIMARY, &[(2, 0.0, 0.0), (5, 0.0, 5.0)]),
        ];
        assert!(build(&short).patches.is_empty());
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
        let patches = build(&bent).patches;
        assert_eq!(patches.len(), 1);
        // as wide as the narrower road
        assert!((distance(patches[0].ring[0], [50.0, 0.0]) - 3.5).abs() < 1e-9);
    }

    #[test]
    fn a_curved_arm_gets_no_corner() {
        let curve: Vec<(i64, f64, f64)> = (0..10)
            .map(|k| {
                let a = f64::from(k) * 0.3;
                (10 + i64::from(k), 6.0 * a.sin(), 6.0 * (1.0 - a.cos()))
            })
            .collect();
        let ways = [
            way(PRIMARY, &[(1, 0.0, -80.0), (10, 0.0, 0.0), (3, 0.0, 80.0)]),
            way(PRIMARY, &curve),
        ];
        let n = build(&ways);
        assert_eq!(n.junctions, 1);
        let discs = DISC_SEGMENTS as usize;
        assert!(n.patches.iter().all(|p| p.ring.len() == discs));
    }
}
