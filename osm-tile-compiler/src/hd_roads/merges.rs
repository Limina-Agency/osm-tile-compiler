//! Where a road parts into two, or two merge into one: which lane is which, the
//! nose where their surfaces part, and what is painted from the node to there.
//!
//! Three ends meet (driveways apart), and of the two on one side of the third the
//! lanes of the left one are its left ones, those of the right one its right ones,
//! each for the same direction: a merge and a diverge of one-way roads, and a
//! two-way road that parts into two carriageways. Where the lanes do not add up
//! two tags may tell why: a lane whose arrow merges ends there, and one with
//! arrows for both roads goes on as a lane of each. Any other is left *plain*:
//! only the surfaces are drawn in to one another.
//!
//! The wide road stays as it is, and the arms begin as its parts and are brought
//! to their own lines over as long as these take to be apart by the arms' widths
//! (`lanes.rs` shapes them).

use super::details::{Detail, Kind};
use super::geometry;
use super::lanes::{ALIKE, Across, Links, Section, Shapes};
use super::network::{Patch, Point, Way, distance};
use super::paint::Block;
use super::profile::{Class, Level, Line, Turn};
use rayon::prelude::*;
use rustc_hash::FxHashMap;

/// Metres along an end its direction is taken over.
const FACING: f64 = 25.0;
/// The most the arms may lie apart, radians, where lanes are told and where they
/// are not; and the cosine of the most either may turn off.
const TOLD: f64 = std::f64::consts::PI / 3.0;
const ANGLE: f64 = std::f64::consts::PI / 6.0;
const AHEAD: f64 = 0.5;
/// Metres over which a plain one's surfaces are drawn in, at most.
pub(super) const PLAIN: f64 = 15.0;
/// The narrowest a surface is drawn in to.
const NARROWEST: f64 = 1.5;
/// Metres along the arms that are looked at, and between places looked at.
const REACH: f64 = 200.0;
const STEP: f64 = 1.0;
/// Metres arms may overlap by as they part.
const OVERLAP: f64 = 0.1;
/// Metres between the arms' lanes at which each has its own line again.
const PART: f64 = 0.6;
/// Metres between the arms' surfaces at the nose, of fast roads and others, and
/// along the arms that it may be off.
const NOSE: [f64; 2] = [2.5, 1.2];
const TO_NOSE: f64 = 100.0;
/// Metres the ground between them reaches in under each.
const UNDER: f64 = 0.4;
/// Metres of a merge line's dash and gap.
const MERGE: [f64; 2] = [2.0, 2.0];
/// Metres of a solid line in one part.
const PARTS: f64 = 3.0;
/// A gore's: metres across where it begins, along for one to be painted, and from
/// one chevron to the next; and what is kept from the lines beside it.
const GORE: f64 = 1.4;
const GORE_LENGTH: f64 = 8.0;
const CHEVRONS: usize = 3;
const BESIDE: f64 = 0.3;

/// Three ends of ways, (way, 1 for its end), at a node.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Merge {
    pub node: i64,
    /// The wide road's, the left arm's and the right arm's, seen from the first.
    pub ends: [(u32, usize); 3],
    /// Which of the lanes of each, from left to right seen so, go on; `None` if
    /// that cannot be told.
    pub kept: Option<[Vec<bool>; 3]>,
    /// How many of the wide road's go on as a lane of each arm.
    pub shared: usize,
}

/// Whether a way is met against itself, seen from the wide road, where its end
/// `end` is the wide one's (`part` 0) or an arm's.
pub(super) fn against(end: usize, part: usize) -> bool {
    (end == 0) == (part == 0)
}

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

fn middle(a: Point, b: Point) -> Point {
    [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0]
}

/// A way's points from its end `end` on.
fn outward(way: &Way, end: usize) -> Vec<Point> {
    let mut points = way.points.clone();
    if end == 1 {
        points.reverse();
    }
    points
}

/// The way an end leaves its node.
fn facing(way: &Way, end: usize) -> Point {
    let points = outward(way, end);
    let mut left = FACING * way.scale;
    for p in points.windows(2) {
        let length = distance(p[0], p[1]);
        if length >= left {
            let t = left / length;
            let d = less(p[1], p[0]);
            let at = [p[0][0] + d[0] * t, p[0][1] + d[1] * t];
            return unit(less(at, points[0]));
        }
        left -= length;
    }
    unit(less(points[points.len() - 1], points[0]))
}

/// The point next to a way's end.
fn next(way: &Way, end: usize) -> Point {
    let last = way.points.len() - 1;
    way.points[if end == 0 { 1 } else { last - 1 }]
}

/// A node, the ends of ways there, and how many arms meet.
type Node = (i64, Vec<(u32, usize)>, u32);

/// The nodes ways end at, in order.
fn nodes(ways: &[Way]) -> Vec<Node> {
    let roads = || {
        let motor = |w: &&Way| w.profile.class.is_motor() && w.profile.class != Class::Service;
        (ways.iter().enumerate()).filter(move |(_, w)| motor(w))
    };
    let mut at: FxHashMap<i64, usize> = FxHashMap::default();
    let mut out: Vec<Node> = Vec::new();
    for (i, way) in roads() {
        for (end, node) in [(0, way.nodes[0]), (1, way.nodes[way.nodes.len() - 1])] {
            let place = *at.entry(node).or_insert_with(|| {
                out.push((node, Vec::new(), 0));
                out.len() - 1
            });
            out[place].1.push((i as u32, end));
        }
    }
    for (_, way) in roads() {
        let last = way.nodes.len() - 1;
        for (i, node) in way.nodes.iter().enumerate() {
            if let Some(&place) = at.get(node) {
                out[place].2 += if i == 0 || i == last { 1 } else { 2 };
            }
        }
    }
    out
}

/// Where roads part or merge, in an order of its own.
pub(super) fn find(ways: &[Way]) -> Vec<Merge> {
    let nodes = nodes(ways);
    let of = nodes.into_par_iter();
    of.filter_map(|(node, ends, arms)| {
        let [a, b, c] = ends[..] else { return None };
        if arms != 3 || a.0 == b.0 || b.0 == c.0 || a.0 == c.0 {
            return None;
        }
        merge(ways, node, [a, b, c])
    })
    .collect()
}

fn merge(ways: &[Way], node: i64, ends: [(u32, usize); 3]) -> Option<Merge> {
    let way = |x: usize| &ways[ends[x].0 as usize];
    let out = [0, 1, 2].map(|x: usize| facing(way(x), ends[x].1));
    // the two that lie nearest to each other are the arms
    let (wide, angle) = (0..3)
        .map(|x| (x, dot(out[(x + 1) % 3], out[(x + 2) % 3]).clamp(-1.0, 1.0)))
        .max_by(|a, b| a.1.total_cmp(&b.1))
        .map(|(x, cos)| (x, cos.acos()))?;
    let (mut left, mut right) = ((wide + 1) % 3, (wide + 2) % 3);
    if cross(out[left], out[right]) < 0.0 {
        (left, right) = (right, left);
    }
    if [left, right]
        .iter()
        .any(|&x| -dot(out[wide], out[x]) < AHEAD)
    {
        return None;
    }
    let ends = [wide, left, right].map(|x| ends[x]);
    if angle > TOLD {
        return None;
    }
    let found = Merge {
        node,
        ends,
        kept: None,
        shared: 0,
    };
    let across = found.across(ways)?;
    let told = told(&across);
    // the arms of one left plain are one-way
    let one_way = |e: &(u32, usize)| {
        let lanes = ways[e.0 as usize].profile.lanes.as_ref();
        lanes.is_some_and(|l| l.along != l.against)
    };
    if told.is_none() && (angle > ANGLE || !ends[1..].iter().all(one_way)) {
        return None;
    }
    let (kept, shared) = told.map_or((None, 0), |(kept, shared)| (Some(kept), shared));
    Some(Merge {
        kept,
        shared,
        ..found
    })
}

/// Which lanes of a road and of the arms it parts into go on, and how many as a
/// lane of both.
fn told(across: &[Across; 3]) -> Option<([Vec<bool>; 3], usize)> {
    if across.iter().any(|x| x.lanes.iter().any(|l| l.0 == 0)) {
        return None;
    }
    let directions = |x: usize, kept: &[bool]| -> Vec<i8> {
        let lanes = across[x].lanes.iter().zip(kept);
        lanes.filter(|(_, k)| **k).map(|(l, _)| l.0).collect()
    };
    let adds_up = |kept: &[Vec<bool>; 3]| {
        let [wide, mut left, right] = [0, 1, 2].map(|x: usize| directions(x, &kept[x]));
        let any = !left.is_empty() || !right.is_empty();
        left.extend(right);
        any && left == wide
    };
    let all = [0, 1, 2].map(|x: usize| vec![true; across[x].lanes.len()]);
    if adds_up(&all) {
        return Some((all, 0));
    }
    // a lane whose traffic comes to the node and merges ends there, which may be
    // all an arm has
    let less = [0, 1, 2].map(|x: usize| -> Vec<bool> {
        let comes = if x == 0 { 1 } else { -1 };
        let lanes = across[x].lanes.iter();
        let merges = |t: &[Turn]| t.iter().any(|t| t.merges());
        lanes.map(|l| l.0 != comes || !merges(l.1)).collect()
    });
    if less != all && adds_up(&less) {
        return Some((less, 0));
    }
    // traffic may take one of its lanes to either
    let count = [0, 1, 2].map(|x: usize| across[x].lanes.len());
    let shared = (count[1] + count[2]).checked_sub(count[0])?;
    let ahead = across.iter().all(|x| x.lanes.iter().all(|l| l.0 == 1));
    let room = count[1].max(count[2]) <= count[0];
    if !ahead || shared == 0 || !room {
        return None;
    }
    let either = &across[0].lanes[count[0] - count[2]..count[1]];
    let both = !either.is_empty() && either.iter().all(|l| l.1.len() > 1);
    both.then_some((all, shared))
}

/// The point a part's lines go on to, and from which edge on to which other one.
type Beyond = (Option<Point>, Option<(usize, Point)>);

impl Merge {
    /// The three as seen from the wide one.
    fn across<'a>(&self, ways: &'a [Way]) -> Option<[Across<'a>; 3]> {
        let of = |x: usize| {
            let (way, end) = self.ends[x];
            Across::new(&ways[way as usize].profile, against(end, x))
        };
        Some([of(0)?, of(1)?, of(2)?])
    }

    /// How many of the wide road's lanes that go on are the left arm's, and the
    /// right arm's.
    fn counts(&self) -> [usize; 2] {
        let kept = self.kept.as_ref();
        [1, 2].map(|x: usize| kept.map_or(0, |k| k[x].iter().filter(|k| **k).count()))
    }

    /// The section of each at the node, as seen from the wide road.
    pub(super) fn sections(&self, ways: &[Way]) -> [Section; 3] {
        let across = self.across(ways).expect("motor roads'");
        let own = |x: usize| across[x].section.clone();
        let Some(kept) = &self.kept else {
            return self.drawn_in([own(0), own(1), own(2)]);
        };
        let wide = &across[0].section;
        let last = wide.lanes.len() - 1;
        let shoulders = [
            wide.surface[0] - wide.lanes[0],
            wide.surface[1] - wide.lanes[last],
        ];
        // the edges of its lanes that go on
        let mut edges: Vec<f64> = Vec::new();
        for i in (0..last).filter(|&i| kept[0][i]) {
            let at = edges.last().copied().unwrap_or(wide.lanes[i]);
            if edges.is_empty() {
                edges.push(at);
            }
            edges.push(at + wide.lanes[i + 1] - wide.lanes[i]);
        }
        let n = edges.len() - 1;
        let [a, b] = self.counts();
        [(0, 0, n), (1, 0, a), (2, n - b, n)].map(|(x, from, to): (usize, usize, usize)| {
            let mut lanes = vec![edges[from]];
            let mut at = from;
            for k in &kept[x] {
                at += usize::from(*k);
                lanes.push(edges[at]);
            }
            let shoulder = |outer: bool, s: f64| if outer { s } else { 0.0 };
            Section {
                surface: [
                    edges[from] + shoulder(from == 0, shoulders[0]),
                    edges[to] + shoulder(to == n, shoulders[1]),
                ],
                lanes,
            }
        })
    }

    /// Their surfaces, none wider than what it meets.
    fn drawn_in(&self, own: [Section; 3]) -> [Section; 3] {
        let arms = [
            own[1].surface[0].min(own[2].surface[0]),
            own[1].surface[1].max(own[2].surface[1]),
        ];
        let wide = own[0].surface;
        let mut x = 0;
        own.map(|mut section| {
            let within = if x == 0 { arms } else { wide };
            x += 1;
            let [left, right] = section.surface;
            let (left, right) = (left.max(within[0]), right.min(within[1]));
            if right - left >= NARROWEST {
                section.surface = [left, right];
            }
            section
        })
    }

    /// How far right of where they are on the first way the lanes both have are on
    /// the second, of the wide road and each arm.
    pub(super) fn apart(&self, ways: &[Way]) -> Vec<([(u32, bool); 2], f64)> {
        let Some(kept) = &self.kept else {
            return Vec::new();
        };
        let across = self.across(ways).expect("motor roads'");
        let of = |x: usize| -> Vec<usize> { (0..kept[x].len()).filter(|&i| kept[x][i]).collect() };
        let wide = of(0);
        let met = |x: usize| (self.ends[x].0, against(self.ends[x].1, x));
        [1, 2]
            .into_iter()
            .filter(|&x| !of(x).is_empty())
            .map(|x| {
                let arm = of(x);
                let from = if x == 1 { 0 } else { wide.len() - arm.len() };
                let lanes = wide[from..from + arm.len()].iter().copied();
                let apart = across[x].mean(arm.iter().copied()) - across[0].mean(lanes);
                ([met(0), met(x)], apart)
            })
            .collect()
    }

    /// Whether the line at each edge of a part's lanes goes on over the node, in
    /// the order of its way.
    pub(super) fn on(&self, part: usize, against: bool) -> Vec<bool> {
        let Some(kept) = &self.kept else {
            return Vec::new();
        };
        let kept = &kept[part];
        let count = kept.iter().filter(|k| **k).count();
        // of those that are one there the first, and of the last the last
        let mut on = vec![true];
        let mut at = 0;
        for (i, k) in kept.iter().enumerate() {
            let before = at;
            at += usize::from(*k);
            let last = i + 1 == kept.len();
            on.push(if at == count { last } else { at != before });
        }
        if against {
            on.reverse();
        }
        on
    }

    /// Metres from the end that the line at each edge of a plain one's lanes
    /// begins: those out of the surface `to` there are none over `length`.
    pub(super) fn out_of(&self, base: &Section, to: &Section, length: f64) -> Option<Vec<f64>> {
        let out = |v: &f64| *v < to.surface[0] - ALIKE || *v > to.surface[1] + ALIKE;
        let lined = |v: &f64| if out(v) { length } else { 0.0 };
        (self.kept.is_none()).then(|| base.lanes.iter().map(lined).collect())
    }

    /// Where each part's lines go on to past the node: those of the wide road to
    /// two points, from an edge on to the second.
    pub(super) fn beyond(&self, ways: &[Way]) -> [Beyond; 3] {
        let Some(kept) = &self.kept else {
            return [(None, None); 3];
        };
        let point = |x: usize| next(&ways[self.ends[x].0 as usize], self.ends[x].1);
        let (left, right) = (point(1), point(2));
        // the wide road's edges from the left to where the arms part
        let edges = kept[0].len();
        let lanes = (0..edges).filter(|&i| kept[0][i]).take(self.counts()[0]);
        let parts = lanes.last().map_or(0, |i| i + 1);
        let wide = if against(self.ends[0].1, 0) {
            (Some(right), Some((edges - parts, left)))
        } else {
            (Some(left), Some((parts + 1, right)))
        };
        [wide, (Some(point(0)), None), (Some(point(0)), None)]
    }

    /// Metres over which the arms, which at the node have the sections `to`, may
    /// come to their own lines and not overlap: as their lines part.
    pub(super) fn ease(&self, ways: &[Way], links: &Links, to: &[Section; 3]) -> f64 {
        /// Metres tried, in order.
        const TRIED: [f64; 14] = [
            6.0, 8.0, 11.0, 15.0, 20.0, 27.0, 36.0, 48.0, 64.0, 85.0, 110.0, 140.0, 170.0, 200.0,
        ];
        if self.kept.is_none() {
            return 0.0;
        }
        const EVERY: f64 = 2.0;
        let arms = [1, 2].map(|x: usize| {
            let walk = Walk::new(ways, links, self.ends[x]);
            let (points, lengths) = walk.stations(EVERY);
            let own: Vec<f64> = lengths
                .iter()
                .map(|&s| {
                    let (way, reversed, _) = walk.at(s);
                    let own = Section::of(&ways[way as usize].profile);
                    inner(&if reversed { own.mirrored() } else { own }, x)[0]
                })
                .collect();
            let shift = inner(&to[x], x)[0] - own[0];
            let normals: Vec<Point> = (0..points.len())
                .map(|k| {
                    let d = tangent(&points, k);
                    [-d[1], d[0]]
                })
                .collect();
            (points, own, shift, normals, walk.scale)
        });
        let count = arms[0].0.len().min(arms[1].0.len());
        let least = |length: f64| -> f64 {
            let steps = ((length / EVERY) as usize).min(count - 1);
            (1..=steps)
                .map(|k| {
                    let t = (EVERY * k as f64 / length).min(1.0);
                    let left = 1.0 - t * t * (3.0 - 2.0 * t);
                    let [a, b] = [0, 1].map(|x: usize| {
                        let (points, own, shift, normals, scale) = &arms[x];
                        let by = (own[k] + shift * left) * scale;
                        [
                            points[k][0] + normals[k][0] * by,
                            points[k][1] + normals[k][1] * by,
                        ]
                    });
                    let (n, m) = (arms[0].3[k], arms[1].3[k]);
                    dot(less(b, a), unit([n[0] + m[0], n[1] + m[1]])) / arms[0].4
                })
                .fold(f64::INFINITY, f64::min)
        };
        let mut best = (f64::NEG_INFINITY, TRIED[0]);
        for length in TRIED {
            let least = least(length);
            if least >= -OVERLAP {
                return length;
            }
            if least > best.0 + 0.02 {
                best = (least, length);
            }
        }
        best.1
    }
}

/// The edges towards the other arm of an arm's lanes and of its surface, as seen
/// from the wide road.
fn inner(section: &Section, part: usize) -> [f64; 2] {
    if part == 1 {
        [section.lanes[section.lanes.len() - 1], section.surface[1]]
    } else {
        [section.lanes[0], section.surface[0]]
    }
}

/// The direction of a line at one of its points.
fn tangent(points: &[Point], k: usize) -> Point {
    let last = points.len() - 1;
    unit(less(points[(k + 1).min(last)], points[k.saturating_sub(1)]))
}

/// The ways from an end on, as they follow each other.
struct Walk {
    points: Vec<Point>,
    lengths: Vec<f64>,
    /// Each way, the end it is come to, and the metres it begins at and is long.
    ways: Vec<(u32, usize, f64, f64)>,
    scale: f64,
}

impl Walk {
    fn new(ways: &[Way], links: &Links, from: (u32, usize)) -> Self {
        let scale = ways[from.0 as usize].scale;
        let mut out = Self {
            points: Vec::new(),
            lengths: Vec::new(),
            ways: Vec::new(),
            scale,
        };
        let mut at = Some(from);
        let mut begin = 0.0;
        while let Some((way, end)) = at.filter(|e| !out.ways.iter().any(|w| w.0 == e.0)) {
            let points = outward(&ways[way as usize], end);
            let lengths = geometry::lengths(&points, scale, begin);
            let whole = lengths[lengths.len() - 1] - begin;
            out.ways.push((way, end, begin, whole));
            let skip = usize::from(!out.points.is_empty());
            out.points.extend(points.into_iter().skip(skip));
            out.lengths.extend(lengths.into_iter().skip(skip));
            begin += whole;
            if begin >= REACH {
                break;
            }
            at = links.at(way, 1 - end);
        }
        out
    }

    /// Points every so many metres, and what is gone at each.
    fn stations(&self, every: f64) -> (Vec<Point>, Vec<f64>) {
        let whole = self.lengths[self.lengths.len() - 1].min(REACH);
        let count = ((whole / every) as usize).max(1);
        let places: Vec<f64> = (0..=count).map(|k| (k as f64 * every).min(whole)).collect();
        let mut points = Vec::with_capacity(places.len());
        let mut k = 1;
        for s in &places {
            while k + 1 < self.lengths.len() && self.lengths[k] < *s {
                k += 1;
            }
            let span = (self.lengths[k] - self.lengths[k - 1]).max(f64::MIN_POSITIVE);
            let t = ((s - self.lengths[k - 1]) / span).clamp(0.0, 1.0);
            let d = less(self.points[k], self.points[k - 1]);
            let from = self.points[k - 1];
            points.push([from[0] + d[0] * t, from[1] + d[1] * t]);
        }
        (points, places)
    }

    /// The way at `s` metres, whether it is met against itself, and the metres
    /// along it.
    fn at(&self, s: f64) -> (u32, bool, f64) {
        let k = self.ways.partition_point(|w| w.2 <= s).max(1) - 1;
        let (way, end, from, whole) = self.ways[k];
        let gone = (s - from).clamp(0.0, whole);
        (way, end == 1, if end == 0 { gone } else { whole - gone })
    }
}

/// An arm as it is shaped: every step, its line, the inner edges of its lanes and
/// of its surface, and its way.
struct Side {
    walk: Walk,
    line: Vec<Point>,
    lane: Vec<Point>,
    surface: Vec<Point>,
    ways: Vec<u32>,
}

impl Side {
    fn new(ways: &[Way], shapes: &Shapes, merge: &Merge, part: usize) -> Self {
        let walk = Walk::new(ways, &shapes.links, merge.ends[part]);
        let (line, lengths) = walk.stations(STEP);
        let edges: Vec<(u32, [f64; 2])> = lengths
            .iter()
            .map(|&s| {
                let (way, reversed, along) = walk.at(s);
                let section = shapes.at(ways, way, along);
                let section = if reversed {
                    section.mirrored()
                } else {
                    section
                };
                (way, inner(&section, part))
            })
            .collect();
        let wide = next(&ways[merge.ends[0].0 as usize], merge.ends[0].1);
        let moved = |x: usize| {
            let by = |k: usize| edges[k].1[x] * walk.scale;
            geometry::displaced(&line, by, [Some(wide), None])
        };
        let (lane, surface) = (moved(0), moved(1));
        Self {
            lane,
            surface,
            ways: edges.iter().map(|e| e.0).collect(),
            walk,
            line,
        }
    }
}

/// Of each point of `a` the nearest of `b`, not far along, and units it is right
/// of it.
fn beside(a: &[Point], b: &[Point]) -> Vec<(Point, f64)> {
    const NEAR: usize = 12;
    (0..a.len())
        .map(|k| {
            let from = k.saturating_sub(NEAR).min(b.len() - 1);
            let to = (k + NEAR).min(b.len() - 1);
            let mut best = (f64::MAX, b[from]);
            for p in b[from..=to].windows(2) {
                let d = less(p[1], p[0]);
                let t = dot(less(a[k], p[0]), d) / dot(d, d).max(f64::MIN_POSITIVE);
                let t = t.clamp(0.0, 1.0);
                let q = [p[0][0] + d[0] * t, p[0][1] + d[1] * t];
                let off = distance(a[k], q);
                if off < best.0 {
                    best = (off, q);
                }
            }
            let side = cross(tangent(a, k), less(best.1, a[k]));
            (best.1, distance(a[k], best.1) * side.signum())
        })
        .collect()
}

/// What merges add to the roads that meet at them.
#[derive(Default)]
pub(super) struct Laid {
    /// The ground between arms, to the nose.
    pub patches: Vec<Patch>,
    pub paint: Vec<Detail>,
    /// Ways' ends, the edge of their lanes towards the other arm, and the metres
    /// from there that it has no line.
    begin: Vec<(u32, usize, usize, f64)>,
}

/// Lays what lies between the arms, and has their lines begin where they part.
pub(super) fn lay(ways: &[Way], shapes: &mut Shapes) -> Laid {
    let each: Vec<Laid> = (shapes.merges.par_iter())
        .filter(|m| m.kept.is_some())
        .map(|m| m.lay(ways, shapes))
        .collect();
    let mut out = Laid::default();
    for laid in each {
        out.patches.extend(laid.patches);
        out.paint.extend(laid.paint);
        for (way, end, edge, from) in laid.begin {
            shapes.begin(way, end, edge, from);
        }
    }
    out
}

impl Merge {
    fn lay(&self, ways: &[Way], shapes: &Shapes) -> Laid {
        let mut out = Laid::default();
        let across = self.across(ways).expect("motor roads'");
        let [left, right] = [1, 2].map(|x: usize| Side::new(ways, shapes, self, x));
        let scale = left.walk.scale;
        let count = left.line.len().min(right.line.len());
        let lanes: Vec<(Point, f64)> = beside(&left.lane[..count], &right.lane)
            .into_iter()
            .map(|(p, gap)| (p, gap / scale))
            .collect();
        let surfaces: Vec<(Point, f64)> = beside(&left.surface[..count], &right.surface)
            .into_iter()
            .map(|(p, gap)| (p, gap / scale))
            .collect();
        let last = count - 1;
        // those that come together again have half each
        let again = distance(
            left.line[left.line.len() - 1],
            right.line[right.line.len() - 1],
        ) < 5.0 * scale;
        let end = if again { last / 2 } else { last };
        let parted = (1..=end).find(|&k| lanes[k].1 >= PART);
        let part = parted.unwrap_or(end);
        for (x, side) in [(1, &left), (2, &right)] {
            for &(way, end, from, whole) in &side.walk.ways {
                let metres = (part as f64 * STEP - from).min(whole);
                let edges = Section::of(&ways[way as usize].profile).lanes.len();
                if metres <= 0.0 || edges == 0 {
                    continue;
                }
                // the right edge of the left arm, met along itself
                let first = (x == 2) == (end == 0);
                let edge = if first { 0 } else { edges - 1 };
                out.begin.push((way, end, edge, metres));
            }
        }
        let alike = |k: usize| {
            let [a, b] = [&left, &right].map(|s| &ways[s.ways[k] as usize].profile);
            (a.level, a.paved) == (b.level, b.paved)
        };
        let profiles = self.ends.map(|e| &ways[e.0 as usize].profile);
        let fast = profiles.iter().all(|p| p.class <= Class::Trunk);
        let wide = NOSE[usize::from(!fast)];
        let reach = ((TO_NOSE / STEP) as usize).min(end);
        let nose = (1..=reach)
            .find(|&k| surfaces[k].1 >= wide)
            .unwrap_or(reach);
        for k in 0..nose {
            let gap = surfaces[k].1.max(surfaces[k + 1].1);
            if !alike(k) || !alike(k + 1) || !(-0.3..wide + 2.0).contains(&gap) {
                continue;
            }
            // in under both, whose edges are not as finely drawn
            let sides = [k, k + 1].map(|k| {
                let (a, b) = (left.surface[k], surfaces[k].0);
                let d = tangent(&left.surface, k);
                let under = [-d[1] * UNDER * scale, d[0] * UNDER * scale];
                [
                    [a[0] - under[0], a[1] - under[1]],
                    [b[0] + under[0], b[1] + under[1]],
                ]
            });
            let [a, b] = [0, 1].map(|x: usize| sides.map(|s| s[x]));
            let between = geometry::between(&a, &b);
            out.patches
                .extend(between.into_iter().flatten().map(|ring| Patch {
                    ring,
                    level: ways[left.ways[k] as usize].profile.level,
                    paved: profiles[1].paved,
                }));
        }

        // on a bridge if either is
        let of = |k: usize| {
            let bridge =
                |s: &Side| matches!(ways[s.ways[k] as usize].profile.level, Level::Bridge(_));
            let side = if bridge(&right) && !bridge(&left) {
                &right
            } else {
                &left
            };
            (side.ways[k], side.line[k])
        };
        let confidence = profiles[0].confidence.id() as u8;
        let middles: Vec<Point> = (0..count)
            .map(|k| middle(left.lane[k], lanes[k].0))
            .collect();
        let edge = {
            let [a, b] = self.counts();
            let on = self.kept.as_ref().map(|k| &k[0]);
            let edges: Vec<usize> = on.map_or_else(Vec::new, |on| {
                let kept = (0..on.len()).filter(|&i| on[i]);
                let mut edges = vec![0];
                edges.extend(kept.map(|i| i + 1));
                edges
            });
            let n = edges.len() - 1;
            (n - b..=a).find_map(|e| across[0].lines[edges[e]])
        };
        let bars: Option<(Kind, f64, f64, f64)> = match edge {
            Some(Line::Lane | Line::LaneSolid) => Some((Kind::Merge, 1.0, MERGE[0], MERGE[1])),
            Some(Line::Centre) => {
                let [on, every] = profiles[0].rules.pattern();
                Some((Kind::Centre, every - on - 2.0, on, every - on))
            }
            Some(Line::CentreSolid) => Some((Kind::Centre, 0.0, PARTS, 0.0)),
            _ => None,
        };
        if let Some((kind, from, on, off)) = bars {
            let (on_steps, every) = ((on / STEP) as usize, ((on + off) / STEP) as usize);
            let mut k = (from / STEP) as usize;
            // what is left of a solid one is one more
            while k < part {
                let to = (k + on_steps).min(part);
                let whole = to - k == on_steps || (off == 0.0 && parted.is_some());
                // both arms have a lane that is shared
                let over = self.shared > 0 && lanes[k].1 < -PART;
                if whole && !over {
                    let (p, q) = (middles[k], middles[to]);
                    let length = distance(p, q) / scale;
                    let half = kind.width() / 2.0;
                    let (way, at) = of(k);
                    let block =
                        Block::new(p, unit(less(q, p)), scale, [-half, half], [0.0, length]);
                    out.paint.push(Detail {
                        way,
                        kind,
                        confidence,
                        at,
                        block,
                        strokes: vec![vec![p, q]],
                        heads: Vec::new(),
                    });
                }
                k += every;
            }
        }

        if !fast || parted.is_none() || edge.is_none() {
            return out;
        }
        // ahead of the nose, where there is ground between the lanes
        let deck = |k: usize| ways[of(k).0 as usize].profile.level != Level::Ground;
        let holds = |k: usize| {
            lanes[k].1 >= GORE && alike(k) && (!deck(k) || surfaces[k].1 <= 0.0) && k <= nose
        };
        let Some(begin) = (part..=nose).find(|&k| holds(k)) else {
            return out;
        };
        let ends = (begin..=nose).take_while(|&k| holds(k)).last();
        let ends = ends.unwrap_or(begin);
        if ((ends - begin) as f64) * STEP < GORE_LENGTH {
            return out;
        }
        for k in (begin..ends).step_by(CHEVRONS) {
            // at half a right angle, its point towards the node
            let gap = lanes[k].1;
            let foot = k + (gap / 2.0 / STEP).round().max(1.0) as usize;
            if foot > ends {
                break;
            }
            let (a, b) = (left.lane[foot], lanes[foot].0);
            let across = unit(less(b, a));
            let kept = BESIDE * scale;
            let ends = [
                [a[0] + across[0] * kept, a[1] + across[1] * kept],
                [b[0] - across[0] * kept, b[1] - across[1] * kept],
            ];
            let long = foot.max((k + CHEVRONS).min(last));
            let (p, q) = (middles[k], middles[long]);
            let half = gap / 2.0 - BESIDE / 2.0;
            let length = distance(p, q) / scale;
            let (way, at) = of(k);
            out.paint.push(Detail {
                way,
                kind: Kind::Gore,
                confidence: 0,
                at,
                block: Block::new(p, unit(less(q, p)), scale, [-half, half], [0.0, length]),
                strokes: vec![vec![ends[0], middles[k], ends[1]]],
                heads: Vec::new(),
            });
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::super::lanes;
    use super::super::network::{self, Network};
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

    fn one_way(
        class: &'static str,
        lanes: &'static str,
        turns: &'static str,
    ) -> Vec<(&'static str, &'static str)> {
        vec![
            ("highway", class),
            ("oneway", "yes"),
            ("lanes", lanes),
            ("turn:lanes", turns),
        ]
    }

    const FROM: [(i64, f64, f64); 2] = [(1, -500.0, 0.0), (2, 0.0, 0.0)];
    const LEFT: [(i64, f64, f64); 2] = [(2, 0.0, 0.0), (3, 600.0, -18.0)];
    const RIGHT: [(i64, f64, f64); 3] = [(2, 0.0, 0.0), (5, 120.0, 9.0), (4, 500.0, 60.0)];

    /// A motorway of three lanes that gives one to a link.
    fn diverge() -> Vec<Way> {
        let mut ways = vec![
            way(&one_way("motorway", "3", ""), &FROM),
            way(&one_way("motorway", "2", ""), &LEFT),
            way(&one_way("motorway_link", "1", ""), &RIGHT),
        ];
        lanes::place(&mut ways);
        ways
    }

    /// The same with traffic the other way.
    fn turned(mut ways: Vec<Way>) -> Vec<Way> {
        for way in &mut ways {
            way.nodes.reverse();
            way.points.reverse();
        }
        ways
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
        geometry::union(&ground)
    }

    #[test]
    fn each_arm_begins_as_its_part_of_the_wide_road() {
        for reversed in [false, true] {
            let ways = if reversed {
                turned(diverge())
            } else {
                diverge()
            };
            let found = find(&ways);
            assert_eq!(found.len(), 1);
            let m = &found[0];
            let (left, right) = (m.ends[1].0, m.ends[2].0);
            assert_eq!((m.ends[0].0, left, right), (0, 1, 2));
            assert_eq!(m.kept.as_ref().unwrap().each_ref().map(Vec::len), [3, 2, 1]);
            let n = network::build(&ways);
            let at = |way: usize| {
                let whole = lanes::length(&ways[way]);
                let end = m.ends[way].1;
                let s = n
                    .shapes
                    .at(&ways, way as u32, if end == 0 { 0.0 } else { whole });
                if against(end, way) { s.mirrored() } else { s }
            };
            let [wide, left, right] = [at(0), at(1), at(2)];
            let near = |a: f64, b: f64| assert!((a - b).abs() < 1e-9, "{a} {b}");
            for e in 0..3 {
                near(left.lanes[e], wide.lanes[e]);
            }
            near(right.lanes[0], wide.lanes[2]);
            near(right.lanes[1], wide.lanes[3]);
            near(left.surface[0], wide.surface[0]);
            near(right.surface[1], wide.surface[1]);
            // nothing is kept back, and the lines go on over the node
            assert!(n.runs.iter().all(|r| r.cut == [0.0; 2]));
            let line = |way: usize, edge: usize| {
                let mut points = n.line(&ways, way, edge, [0.0; 2]).unwrap().points;
                if m.ends[way].1 == 1 {
                    points.reverse();
                }
                points
            };
            let edge = |of: usize, lanes: usize| if reversed { lanes - of } else { of };
            for (way, a, b, lanes) in [(1, 0, 0, 2), (1, 1, 1, 2), (2, 3, 1, 1)] {
                let (p, q) = (line(0, edge(a, 3))[0], line(way, edge(b, lanes))[0]);
                assert!(distance(p, q) < 1e-6, "{way} {a}: {p:?} {q:?}");
            }
            // the arms' inner lines begin where they part
            let (a, b) = (line(1, edge(2, 2))[0], line(2, edge(0, 1))[0]);
            assert!(a[0] > 4.0 && (a[0] - b[0]).abs() < 1.5, "{a:?} {b:?}");
            assert!((0.4..1.2).contains(&(b[1] - a[1])), "{a:?} {b:?}");
            // and up to there is the line between, less than its gap apart
            let of = |kind: Kind| n.paint.iter().filter(move |d| d.kind == kind);
            let mut to = 0.0;
            for d in of(Kind::Merge) {
                assert!(d.strokes[0][0][0] - to <= MERGE[1] + 0.1);
                to = d.strokes[0][1][0];
            }
            assert!((a[0] - to) < 4.0 && to < a[0] + 0.5, "{to} {a:?}");
            // ahead of the nose chevrons, between the lines
            assert!(of(Kind::Gore).count() > 3);
            for d in of(Kind::Gore) {
                let s = &d.strokes[0];
                assert!(s[1][0] > a[0] && s[1][0] < s[0][0] && s[0][1] < s[2][1]);
            }
        }
    }

    #[test]
    fn the_surface_is_one_with_no_square_end() {
        for ways in [diverge(), turned(diverge())] {
            let n = network::build(&ways);
            let surface = surface(&ways, &n);
            assert_eq!((surface.len(), surface[0].len()), (1, 1));
            let ring = &surface[0][0];
            for k in 0..ring.len() {
                let (p, q) = (ring[k], ring[(k + 1) % ring.len()]);
                let (along, across) = ((q[0] - p[0]).abs(), (q[1] - p[1]).abs());
                // but for its ends and the nose
                let within = p[0] > -499.0 && p[0] < 490.0 && q[0] < 490.0;
                let nose = across < NOSE[0] + 0.5 && p[0] > 20.0;
                assert!(
                    !within || nose || across < 0.1 || across < along * 0.5,
                    "{p:?} {q:?}"
                );
            }
        }
    }

    #[test]
    fn arms_do_not_overlap_as_they_part() {
        let ways = diverge();
        let n = network::build(&ways);
        let m = &n.shapes.merges[0];
        let [left, right] = [1, 2].map(|x: usize| Side::new(&ways, &n.shapes, m, x));
        for (k, (_, gap)) in beside(&left.lane, &right.lane).into_iter().enumerate() {
            assert!(gap > -2.0 * OVERLAP, "{k}: {gap}");
        }
        // and are on their own lines in the end
        assert!(
            (n.shapes.at(&ways, 2, 300.0).lanes[0] - ways[2].profile.lanes.as_ref().unwrap().left)
                .abs()
                < 1e-9
        );
    }

    #[test]
    fn a_two_way_road_parts_by_direction() {
        let one = [("highway", "primary"), ("oneway", "yes"), ("lanes", "1")];
        let ways = [
            way(
                &[("highway", "primary")],
                &[(1, -300.0, 0.0), (2, 0.0, 0.0)],
            ),
            way(&one, &[(3, 150.0, -20.0), (2, 0.0, 0.0)]),
            way(&one, &[(2, 0.0, 0.0), (4, 150.0, 20.0)]),
        ];
        let n = network::build(&ways);
        let m = &n.shapes.merges[0];
        assert_eq!(m.ends, [(0, 1), (1, 1), (2, 0)]);
        assert!(n.runs.iter().all(|r| r.cut == [0.0; 2]));
        // its middle line goes on to where they part, and no further
        let bars: Vec<_> = n.paint.iter().collect();
        assert!(
            bars.iter()
                .all(|d| d.kind == Kind::Centre && d.strokes[0][1][0] < 30.0)
        );
        assert_eq!(surface(&ways, &n).len(), 1);
        // traffic on the left arm the other way: no lane of it is the road's
        let mut other = ways;
        other[1].points.reverse();
        other[1].nodes.reverse();
        assert!(find(&other)[0].kept.is_none());
    }

    #[test]
    fn where_traffic_keeps_left_a_road_parts_the_other_way() {
        use super::super::country;
        use super::super::profile::profile_in;
        let left = |t: &[(&str, &'static str)], n: &[(i64, f64, f64)]| Way {
            profile: profile_in(
                |k| t.iter().find(|x| x.0 == k).map(|x| x.1),
                country::rules("GB"),
            )
            .unwrap(),
            ..way(t, n)
        };
        let one = [("highway", "primary"), ("oneway", "yes"), ("lanes", "1")];
        let two = [("highway", "primary")];
        // the northern arm takes traffic away, the southern brings it
        let ways = [
            left(&two, &[(1, -300.0, 0.0), (2, 0.0, 0.0)]),
            left(&one, &[(2, 0.0, 0.0), (3, 150.0, -20.0)]),
            left(&one, &[(4, 150.0, 20.0), (2, 0.0, 0.0)]),
        ];
        let found = find(&ways);
        assert!(found[0].kept.is_some());
        let n = network::build(&ways);
        assert!(
            n.paint
                .iter()
                .all(|d| d.kind == Kind::Centre && d.strokes[0][1][0] < 30.0)
        );
        assert_eq!(surface(&ways, &n).len(), 1);
        // drawn as where traffic keeps right, none of it is told
        let mut other = ways;
        for w in &mut other[1..] {
            w.points.reverse();
            w.nodes.reverse();
        }
        assert!(find(&other)[0].kept.is_none());
    }

    #[test]
    fn lanes_that_do_not_add_up_are_told_by_their_arrows_or_left() {
        let of = |wide, left, right, reversed: bool| {
            let ways = vec![
                way(
                    &one_way("primary", ["1", "2", "3"][lanes_in(wide) - 1], wide),
                    &FROM,
                ),
                way(
                    &one_way("primary", ["1", "2", "3"][lanes_in(left) - 1], left),
                    &LEFT,
                ),
                way(
                    &one_way("primary", ["1", "2", "3"][lanes_in(right) - 1], right),
                    &RIGHT,
                ),
            ];
            let ways = if reversed { turned(ways) } else { ways };
            let m = find(&ways).remove(0);
            (
                m.kept.map(|k| k.map(|k| k.iter().filter(|k| **k).count())),
                m.shared,
            )
        };
        fn lanes_in(turns: &str) -> usize {
            turns.split('|').count()
        }
        // one lane leads to both
        let shared = of("through|through;right", "|", "", false);
        assert_eq!(shared, (Some([2, 2, 1]), 1));
        // the link's lane ends
        let ends = of("|", "|", "merge_to_left", true);
        assert_eq!(ends, (Some([2, 2, 0]), 0));
        // nothing says
        assert_eq!(of("|", "|", "", false), (None, 0));
        assert_eq!(of("|", "|", "", true), (None, 0));
    }

    #[test]
    fn a_plain_one_is_drawn_in_and_keeps_its_lines_back() {
        let ways = vec![
            way(&one_way("primary", "4", ""), &FROM),
            way(&one_way("primary", "2", ""), &LEFT),
            way(&one_way("primary", "1", ""), &RIGHT),
        ];
        let n = network::build(&ways);
        assert!(n.shapes.merges[0].kept.is_none() && n.paint.is_empty());
        assert!(n.runs[1].cut[0] > 20.0 && n.runs[2].cut[0] > 20.0);
        assert_eq!(n.shapes.at(&ways, 0, 500.0).surface, [-3.5, 3.5]);
        assert_eq!(n.shapes.at(&ways, 0, 480.0).surface, [-7.0, 7.0]);
    }
}
