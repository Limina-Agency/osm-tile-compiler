//! Lanes that go on from one way to the next: where each way lies in its
//! cross-section, and how that changes along it.
//!
//! Two ways *follow* each other where the two ends are all that meets at a node
//! (driveways apart). There a lane of one is a lane of the other or it ends, by
//! direction, in this order:
//!
//! 1. of the lanes of the one with more, those whose arrows merge end, and those
//!    with arrows that lead off the road open;
//! 2. where `placement` is tagged on both, those that are in the same place go on;
//! 3. arrows that are the same on both go on;
//! 4. else the outermost (the right one, as its traffic sees it, or the left
//!    one where traffic keeps left) opens or ends.
//!
//! Where no lane can be told to be another's (one lane for both directions
//! that becomes two, one-way roads that face each other) none goes on, and the
//! wider surface is drawn in to the narrower.
//!
//! From this [`place`] moves each way's cross-section (its profile) so that what
//! goes on lines up, and [`shapes`] says what of it is left to be made up near
//! the ways' ends. Where three or more meet a road that parts, or two that merge,
//! are looked at (`merges.rs`), and the roads that go straight over (`junctions.rs`):
//! their lanes line up too, and what one's surface has that the other's has not
//! opens or ends between the junction's mouths. At any other end the cross-section
//! is the way's own.

use super::geometry::{self, Rect};
use super::junctions::{self, Stretch};
use super::merges::{self, Merge};
use super::network::{Edge, Point, Way, distance};
use super::profile::{Class, Lanes, Level, Line, Placed, Profile, Turn};
use rayon::prelude::*;
use rustc_hash::FxHashMap;

/// Metres of paint of a dashed line, and from one dash to the next, where the
/// table by country says nothing (`Rules::pattern`).
#[cfg(test)]
pub(super) const DASH: [f64; 2] = [2.0, 6.0];
/// Metres cross-sections may differ by where ways meet.
pub(super) const ALIKE: f64 = 0.06;
/// Metres across that a lane must have opened to for its line to be drawn.
const LINED: f64 = 1.5;
/// Parts a taper is drawn in.
const STEPS: u32 = 8;
/// Metres of a whole lane a taper is as long for as its speed says.
const LANE: f64 = 3.5;
/// The shortest a taper is, if the way has the room.
const SHORTEST: f64 = 6.0;

/// The least metres an edge in a junction comes to the other road's over.
const INNER: f64 = 1.0;
/// Metres a surface that is wider in a junction begins to widen over, of each.
const WIDENS: f64 = 2.5;

/// Metres over which a way's surface comes to one that is mapped.
const GIVEN: f64 = 12.0;

/// From 0 to 1, gently at both ends.
fn ease(t: f64) -> f64 {
    t * t * (3.0 - 2.0 * t)
}

fn apart(a: [f64; 2], b: [f64; 2]) -> f64 {
    (a[0] - b[0]).abs().max((a[1] - b[1]).abs())
}

/// Metres over which a lane opens or ends at `speed`.
fn taper(speed: f64) -> f64 {
    30.0 + (speed - 50.0).clamp(0.0, 70.0) / 70.0 * 90.0
}

/// A way's cross-section at a place along it, metres right of its line.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Section {
    /// The surface's edges.
    pub surface: [f64; 2],
    /// The lanes' edges from left to right, one more than there are lanes; a
    /// lane that is not there yet, or no longer, has no width.
    pub lanes: Vec<f64>,
}

impl Section {
    /// The section of a way where nothing of it changes.
    pub(super) fn of(profile: &Profile) -> Self {
        let lanes = profile.lanes.as_ref().map_or_else(Vec::new, |l| {
            (0..=l.backward + l.forward)
                .map(|i| l.left + f64::from(i) * l.width)
                .collect()
        });
        Self {
            surface: profile.surface(),
            lanes,
        }
    }

    /// What traffic along the way (`true`) or against it takes, as
    /// [`Lanes::span`]: of one lane for both, its half.
    pub(super) fn span(&self, lanes: &Lanes, along: bool) -> [f64; 2] {
        let (first, last) = (self.lanes[0], self.lanes[self.lanes.len() - 1]);
        let parts = match (lanes.along, lanes.against, lanes.backward) {
            (true, true, 0) => (first + last) / 2.0,
            (true, true, _) => self.lanes[lanes.middle() as usize],
            _ => return [first, last],
        };
        if along != lanes.left_hand {
            [parts, last]
        } else {
            [first, parts]
        }
    }

    pub(super) fn mirrored(mut self) -> Self {
        self.lanes.reverse();
        self.lanes.iter_mut().for_each(|v| *v = -*v);
        self.surface = [-self.surface[1], -self.surface[0]];
        self
    }

    /// It `by` metres further right.
    fn moved(&self, by: f64) -> Self {
        Self {
            surface: self.surface.map(|v| v + by),
            lanes: self.lanes.iter().map(|v| v + by).collect(),
        }
    }

    /// The most anything in it is from where it is in `other`.
    pub(super) fn apart(&self, other: &Self) -> f64 {
        let pairs = self.lanes.iter().zip(&other.lanes);
        let pairs = pairs.chain(self.surface.iter().zip(&other.surface));
        pairs.map(|(a, b)| (a - b).abs()).fold(0.0, f64::max)
    }

    /// The section `t` of the way from `self` to `other`.
    fn mix(&self, other: &Self, t: f64) -> Self {
        let mix = |a: f64, b: f64| a + (b - a) * t;
        Self {
            surface: [0, 1].map(|i| mix(self.surface[i], other.surface[i])),
            lanes: (self.lanes.iter().zip(&other.lanes))
                .map(|(a, b)| mix(*a, *b))
                .collect(),
        }
    }
}

/// What a way's end has to meet.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Meet {
    /// Metres over which its section turns into `to`; 0 where it is the same.
    pub length: f64,
    /// Those of them, at its end, within which it is `to` already: a junction's
    /// mouth.
    pub from: f64,
    pub across: Option<Mouth>,
    /// The section there.
    pub to: Section,
    /// The edge of the other way's lanes that each edge of its lanes goes on as.
    pub onto: Vec<Option<u8>>,
    /// Metres from the end that the line at each edge of its lanes begins.
    pub lined: Vec<f64>,
}

/// What a junction's mouth makes of a way's surface: its edges at the end, and the
/// metres over which each comes to be there, half of what there is between the mouths;
/// or, if so, those from the end by which it is, all there is.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Mouth {
    pub surface: [f64; 2],
    pub within: [f64; 2],
    pub whole: [Option<f64>; 2],
}

/// Of two ends that go straight over, as met from the first to the second: their
/// sections, their surfaces' edges at the node, and of each edge the metres from it by
/// which it is drawn in to the other's, if it is.
type Mouths = ([Section; 2], [[f64; 2]; 2], [[Option<f64>; 2]; 2]);

/// How a way differs from its profile.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct Shape {
    /// Metres along the whole way.
    pub length: f64,
    /// At its start and its end.
    pub meets: [Option<Box<Meet>>; 2],
    /// The point the way that follows goes on to, where it goes on that way.
    pub beyond: [Option<Point>; 2],
    /// Where a road parts, the first edge of its lanes that goes on to another
    /// point, and that.
    pub parts: [Option<(usize, Point)>; 2],
}

/// Those of the ways that follow another.
#[derive(Default)]
pub(super) struct Shapes {
    list: Vec<Shape>,
    /// By way, its place in the list and 1, or 0.
    of: Vec<u32>,
    pub links: Links,
    pub merges: Vec<Merge>,
    /// By way, the stretches of it whose carriageway is mapped, in order.
    pub given: FxHashMap<u32, Vec<Stretch>>,
}

impl Shapes {
    /// That of a way, if it had none a new one.
    fn add(&mut self, way: u32, length: f64) -> &mut Shape {
        if self.of[way as usize] == 0 {
            self.list.push(Shape {
                length,
                ..Default::default()
            });
            self.of[way as usize] = self.list.len() as u32;
        }
        &mut self.list[self.of[way as usize] as usize - 1]
    }

    /// Has the line at an edge of a way begin no nearer to an end than `from`.
    pub(super) fn begin(&mut self, way: u32, end: usize, edge: usize, from: f64) {
        let place = self.of[way as usize] as usize;
        let shape = place.checked_sub(1).map(|k| &mut self.list[k]);
        if let Some(meet) = shape.and_then(|s| s.meets[end].as_mut()) {
            meet.lined[edge] = meet.lined[edge].max(from);
        }
    }

    pub(super) fn of(&self, way: u32) -> Option<&Shape> {
        let place = *self.of.get(way as usize)?;
        (place > 0).then(|| &self.list[place as usize - 1])
    }

    /// The way that tapers, if this one does.
    pub(super) fn tapered(&self, way: u32) -> Option<&Shape> {
        self.of(way)
            .filter(|s| s.meets.iter().flatten().any(|m| m.length > 0.0))
    }

    /// The section of a way `s` metres along it.
    pub(super) fn at(&self, ways: &[Way], way: u32, s: f64) -> Section {
        let base = Section::of(&ways[way as usize].profile);
        let mut out = base.clone();
        let shape = self.tapered(way);
        let length = shape.map_or(0.0, |s| s.length);
        let meets = shape.into_iter().flat_map(|s| &s.meets);
        for (end, meet) in meets.enumerate() {
            let Some(meet) = meet.as_ref().filter(|m| m.length > 0.0) else {
                continue;
            };
            let from = if end == 0 { s } else { length - s };
            if from < meet.length {
                let eased = (meet.length - meet.from).max(f64::MIN_POSITIVE);
                let t = ((from - meet.from) / eased).clamp(0.0, 1.0);
                let there = 1.0 - ease(t);
                // what each end adds to it: they do not overlap
                let add = |out: &mut f64, to: f64, base: f64| *out += (to - base) * there;
                for i in 0..2 {
                    add(&mut out.surface[i], meet.to.surface[i], base.surface[i]);
                }
                for i in 0..base.lanes.len() {
                    add(&mut out.lanes[i], meet.to.lanes[i], base.lanes[i]);
                }
                for i in 0..2 {
                    let Some(mouth) = meet.across.as_ref().filter(|m| from < m.within[i]) else {
                        continue;
                    };
                    let by = mouth.surface[i] - meet.to.surface[i];
                    let past = mouth.whole[i].unwrap_or(0.0);
                    let long = (mouth.within[i] - past).max(f64::MIN_POSITIVE);
                    let left = ((mouth.within[i] - from) / long).clamp(0.0, 1.0);
                    out.surface[i] += by
                        * match mouth.whole[i] {
                            Some(_) => ease(left),
                            // half of what there is from this mouth to the other one
                            None => 2.0 * ease(left / 2.0),
                        };
                }
            }
        }
        self.near_given(way, s, &mut out);
        out
    }

    /// The section of a way whose carriageway is mapped in part, near to there.
    fn near_given(&self, way: u32, s: f64, section: &mut Section) {
        for given in self.given.get(&way).into_iter().flatten() {
            let off = [given.span[0] - s, s - given.span[1]];
            for (off, edges) in off.into_iter().zip(&given.edges) {
                let Some(edges) = edges.filter(|_| off.abs() < GIVEN) else {
                    continue;
                };
                let there = 1.0 - ease(off.abs() / GIVEN);
                for (edge, to) in section.surface.iter_mut().zip(edges) {
                    *edge += (to - *edge) * there;
                }
            }
        }
    }

    /// Metres from a way's end that a junction's mouth begins at, if its section is
    /// made to meet what goes straight over.
    pub(super) fn mouth(&self, way: u32, end: usize) -> f64 {
        let meet = self.of(way).and_then(|s| s.meets[end].as_ref());
        meet.map_or(0.0, |m| m.from)
    }

    /// Where along a way its section changes, metres, in order.
    pub(super) fn steps(&self, way: u32) -> Vec<f64> {
        let mut out = Vec::new();
        if let Some(shape) = self.tapered(way) {
            for (end, meet) in shape.meets.iter().enumerate() {
                let Some(meet) = meet.as_ref().filter(|m| m.length > 0.0) else {
                    continue;
                };
                let mut add = |a: f64, b: f64, steps: u32| {
                    out.extend((0..=steps).map(|k| {
                        let from = a + (b - a) * f64::from(k) / f64::from(steps);
                        if end == 0 { from } else { shape.length - from }
                    }));
                };
                if let Some(mouth) = &meet.across {
                    for (within, whole) in mouth.within.into_iter().zip(mouth.whole) {
                        add(whole.unwrap_or(0.0), within, STEPS / 2);
                    }
                }
                if meet.length > meet.from {
                    add(meet.from, meet.length, STEPS);
                }
            }
        }
        for given in self.given.get(&way).into_iter().flatten() {
            let mut add = |from: f64, by: f64| {
                let steps = (0..=STEPS).map(|k| f64::from(k) / f64::from(STEPS));
                out.extend(steps.map(|t| from + by * t));
            };
            for (at, edges) in given.span.into_iter().zip(&given.edges) {
                if edges.is_some() {
                    add(at, -GIVEN);
                    add(at, GIVEN);
                }
            }
        }
        out.sort_unstable_by(f64::total_cmp);
        out
    }

    /// Metres from a way's line that its surface reaches, anywhere along it.
    pub(super) fn reach(&self, ways: &[Way], way: u32) -> f64 {
        let p = &ways[way as usize].profile;
        let mut reach = p.width / 2.0 + p.centre.abs();
        for meet in self
            .of(way)
            .into_iter()
            .flat_map(|s| s.meets.iter().flatten())
        {
            let across = meet.across.as_ref().map(|m| m.surface);
            for [left, right] in [meet.to.surface].into_iter().chain(across) {
                reach = reach.max(left.abs()).max(right.abs());
            }
        }
        let given = self.given.get(&way).into_iter().flatten();
        for edge in given.flat_map(|g| g.edges).flatten().flatten() {
            reach = reach.max(edge.abs());
        }
        reach
    }

    /// The ground a way takes between its own constant part and its ends, and
    /// the constant part: metres at its start and end it does not have.
    pub(super) fn tapers(
        &self,
        ways: &[Way],
        way: u32,
        overlap: f64,
        rect: Rect,
    ) -> (Vec<Vec<Vec<Point>>>, [f64; 2]) {
        let mut out = Vec::new();
        let mut taken = [0.0; 2];
        let Some(shape) = self.tapered(way) else {
            return (out, taken);
        };
        for (end, meet) in shape.meets.iter().enumerate() {
            let Some(meet) = meet.as_ref().filter(|m| m.length > 0.0) else {
                continue;
            };
            taken[end] = meet.length;
            let span = if end == 0 {
                [0.0, meet.length]
            } else {
                [shape.length - meet.length, shape.length]
            };
            out.extend(self.strip(ways, way, span, overlap, rect));
        }
        (out, taken)
    }

    /// The ground a way takes from one place along it to another, metres.
    pub(super) fn strip(
        &self,
        ways: &[Way],
        way: u32,
        span: [f64; 2],
        overlap: f64,
        rect: Rect,
    ) -> Vec<Vec<Vec<Point>>> {
        let w = &ways[way as usize];
        let whole = length(w);
        let trim = [span[0], whole - span[1]].map(|m| (m * w.scale).max(0.0));
        let Some(points) = geometry::trim(&w.points, trim) else {
            return Vec::new();
        };
        let lengths = geometry::lengths(&points, w.scale, span[0].max(0.0));
        let (points, lengths) = geometry::stations(&points, &lengths, &self.steps(way));
        let long = geometry::extend(&points, overlap * w.scale);
        let sections: Vec<[f64; 2]> = (lengths.iter())
            .map(|&s| self.at(ways, way, s).surface)
            .collect();
        let sides =
            [0, 1].map(|i| geometry::displaced(&long, |k| sections[k][i] * w.scale, [None; 2]));
        let reaches = |k: usize| {
            let of = [sides[0][k], sides[0][k + 1], sides[1][k], sides[1][k + 1]];
            (0..2).all(|i| {
                of.iter().any(|p| p[i] >= rect[i]) && of.iter().any(|p| p[i] <= rect[i + 2])
            })
        };
        let mut out = Vec::new();
        for k in (0..long.len() - 1).filter(|&k| reaches(k)) {
            out.extend(geometry::between(
                &sides[0][k..=k + 1],
                &sides[1][k..=k + 1],
            ));
        }
        out
    }
}

// What follows what.

/// No way follows there.
const NONE: u32 = u32::MAX;

/// By way, which way's end (the way twice, and 1 for its end) is at its start
/// and its end.
#[derive(Default)]
pub(super) struct Links(Vec<[u32; 2]>);

impl Links {
    pub(super) fn at(&self, way: u32, end: usize) -> Option<(u32, usize)> {
        let to = self.0[way as usize][end];
        (to != NONE).then_some((to / 2, (to % 2) as usize))
    }

    /// The ways in chains, each with whether it runs against its chain: from a
    /// free end, or of those in a ring from the first.
    fn chains(&self) -> Vec<Vec<(u32, bool)>> {
        let mut seen = vec![false; self.0.len()];
        let mut out = Vec::new();
        for ring in [false, true] {
            for way in 0..self.0.len() as u32 {
                let links = self.0[way as usize];
                if seen[way as usize] || links == [NONE; 2] {
                    continue;
                }
                if !ring && links[0] != NONE && links[1] != NONE {
                    continue;
                }
                let mut chain = Vec::new();
                let (mut at, mut reversed) = (way, links[0] != NONE && !ring);
                while !seen[at as usize] {
                    seen[at as usize] = true;
                    chain.push((at, reversed));
                    let Some((next, end)) = self.at(at, usize::from(!reversed)) else {
                        break;
                    };
                    (at, reversed) = (next, end == 1);
                }
                out.push(chain);
            }
        }
        out
    }
}

pub(super) fn links(ways: &[Way]) -> Links {
    // arms of roads and of driveways at each node, and the ends among them
    #[derive(Default)]
    struct Arms {
        roads: u32,
        service: u32,
        ends: [u32; 2],
        found: u8,
    }
    let mut arms: FxHashMap<i64, Arms> = FxHashMap::default();
    let is_service = |way: &Way| way.profile.class == Class::Service;
    let motor = || {
        ways.iter()
            .enumerate()
            .filter(|(_, w)| w.profile.class.is_motor())
    };
    for (_, way) in motor() {
        let last = way.nodes.len() - 1;
        for node in [way.nodes[0], way.nodes[last]] {
            arms.entry(node).or_default();
        }
    }
    for (_, way) in motor() {
        let last = way.nodes.len() - 1;
        for (i, node) in way.nodes.iter().enumerate() {
            let Some(arms) = arms.get_mut(node) else {
                continue;
            };
            let count = if i == 0 || i == last { 1 } else { 2 };
            if is_service(way) {
                arms.service += count;
            } else {
                arms.roads += count;
            }
        }
    }
    let alone = |arms: &Arms, way: &Way| match (arms.roads, arms.service) {
        (2, _) => !is_service(way),
        (1, 1) | (0, 2) => true,
        _ => false,
    };
    let mut links = Links(vec![[NONE; 2]; ways.len()]);
    for (i, way) in motor() {
        let last = way.nodes.len() - 1;
        for (end, node) in [(0, way.nodes[0]), (1, way.nodes[last])] {
            let arms = arms.get_mut(&node).expect("put in above");
            if !alone(arms, way) || way.nodes[0] == way.nodes[last] || arms.found > 1 {
                continue;
            }
            arms.ends[usize::from(arms.found)] = i as u32 * 2 + end;
            arms.found += 1;
            if arms.found == 2 {
                let [a, b] = arms.ends;
                links.0[(a / 2) as usize][(a % 2) as usize] = b;
                links.0[(b / 2) as usize][(b % 2) as usize] = a;
            }
        }
    }
    links
}

// Which lane is which.

/// A way's cross-section as it is met going one way along it.
pub(super) struct Across<'a> {
    pub section: Section,
    /// The lanes from left to right: along (1), against (-1) or both (0), and
    /// their arrows.
    pub lanes: Vec<(i8, &'a [Turn])>,
    /// What is painted at each edge.
    pub lines: Vec<Option<Line>>,
    placed: bool,
    /// Whether traffic keeps left: its outermost lanes are then its left ones.
    left_hand: bool,
}

impl<'a> Across<'a> {
    pub(super) fn new(profile: &'a Profile, reversed: bool) -> Option<Self> {
        let l = profile.lanes.as_ref()?;
        let count = l.backward + l.forward;
        let mut lanes: Vec<(i8, &[Turn])> = (0..count)
            .map(|i| {
                // from the driver's left
                let (direction, k) = l.lane(i);
                let turns = match direction {
                    1 => l.turns[0].get(k),
                    -1 => l.turns[1].get(k),
                    _ => None,
                };
                (direction, turns.map_or(&[][..], |t| &t[..]))
            })
            .collect();
        let mut section = Section::of(profile);
        let mut lines = vec![None; section.lanes.len()];
        for (at, line) in &profile.lines {
            lines[edge(profile, *at)] = Some(*line);
        }
        if reversed {
            section = section.mirrored();
            lines.reverse();
            lanes.reverse();
            lanes.iter_mut().for_each(|l| l.0 = -l.0);
        }
        Some(Self {
            section,
            lanes,
            lines,
            placed: profile.placed == Placed::Said,
            left_hand: l.left_hand,
        })
    }

    /// The mean middle of some of its lanes, metres right.
    pub(super) fn mean(&self, lanes: impl Iterator<Item = usize>) -> f64 {
        let (count, sum) = lanes.fold((0.0, 0.0), |(n, sum), i| (n + 1.0, sum + self.middle(i)));
        sum / f64::max(count, 1.0)
    }

    fn middle(&self, lane: usize) -> f64 {
        (self.section.lanes[lane] + self.section.lanes[lane + 1]) / 2.0
    }
}

/// Which edge of a profile's lanes is `at`.
pub(super) fn edge(profile: &Profile, at: f64) -> usize {
    profile.lanes.as_ref().map_or(0, |l| {
        let edge = ((at - l.left) / l.width).round().max(0.0);
        (edge as usize).min((l.backward + l.forward) as usize)
    })
}

/// Which lanes of `a` and of the way that follows, `b`, go on as each other,
/// from left to right; `None` if no lane can be told to.
fn kept(a: &Across, b: &Across) -> Option<[Vec<bool>; 2]> {
    let single = |x: &Across| x.lanes.iter().any(|l| l.0 == 0);
    if single(a) || single(b) {
        let one = a.lanes.len() == 1 && b.lanes.len() == 1;
        return one.then(|| [vec![true], vec![true]]);
    }
    let mut out = [vec![false; a.lanes.len()], vec![false; b.lanes.len()]];
    for direction in [-1, 1] {
        // from the inside out
        let of = |x: &Across| -> Vec<usize> {
            let lanes = x.lanes.iter().enumerate();
            let mut of: Vec<usize> = lanes
                .filter(|(_, l)| l.0 == direction)
                .map(|(i, _)| i)
                .collect();
            if (direction < 0) != x.left_hand {
                of.reverse();
            }
            of
        };
        let (of_a, of_b) = (of(a), of(b));
        // as traffic goes
        let flip = direction < 0;
        let (up, down, from, to) = if flip {
            (&of_b, &of_a, b, a)
        } else {
            (&of_a, &of_b, a, b)
        };
        let (keep_up, keep_down) = choose((from, up), (to, down));
        let (i, j) = if flip { (1, 0) } else { (0, 1) };
        keep_up.into_iter().for_each(|k| out[i][k] = true);
        keep_down.into_iter().for_each(|k| out[j][k] = true);
    }
    out[0].contains(&true).then_some(out)
}

/// Of the lanes of one direction, in order, as many as there are on both sides:
/// those it costs the least to take as the same.
fn choose(
    (from, up): (&Across, &[usize]),
    (to, down): (&Across, &[usize]),
) -> (Vec<usize>, Vec<usize>) {
    let ends = up.len() > down.len();
    if up.len() == down.len() {
        return (up.to_vec(), down.to_vec());
    }
    let (long, short) = if ends { (up, down) } else { (down, up) };
    let leaves = |t: &[Turn]| !t.is_empty() && t.iter().all(|t| t.leaves());
    let merges = |t: &[Turn]| t.iter().any(|t| t.merges());
    let drop = |i: usize| {
        let turns = if ends { from } else { to }.lanes[long[i]].1;
        let said = match ends {
            true if merges(turns) => 0.0,
            true if leaves(turns) => 0.6,
            false if leaves(turns) => 0.0,
            _ => 1.0,
        };
        said + 0.02 * (long.len() - 1 - i) as f64
    };
    let same = |i: usize, k: usize| {
        let (u, d) = if ends {
            (long[i], short[k])
        } else {
            (short[k], long[i])
        };
        let (before, after) = (from.lanes[u].1, to.lanes[d].1);
        let mut cost = 0.0;
        if merges(before) && !merges(after) {
            cost += 2.0;
        }
        if !before.is_empty() && !after.is_empty() {
            let common = before.iter().any(|t| after.contains(t));
            cost += if common { 0.0 } else { 0.5 };
        } else if leaves(after) || leaves(before) {
            cost += 0.3;
        }
        if from.placed && to.placed {
            cost += (from.middle(u) - to.middle(d)).abs();
        }
        cost
    };
    // the least that the first i of the long ones cost with k of them taken
    let (n, m) = (long.len(), short.len());
    let mut least = vec![vec![f64::INFINITY; m + 1]; n + 1];
    least[0][0] = 0.0;
    for i in 1..=n {
        for k in 0..=m.min(i) {
            let left = least[i - 1][k] + drop(i - 1);
            let taken = if k > 0 {
                least[i - 1][k - 1] + same(i - 1, k - 1)
            } else {
                f64::INFINITY
            };
            least[i][k] = left.min(taken);
        }
    }
    let mut taken = Vec::with_capacity(m);
    let mut k = m;
    for i in (1..=n).rev() {
        if k > 0 && least[i - 1][k - 1] + same(i - 1, k - 1) <= least[i - 1][k] + drop(i - 1) {
            k -= 1;
            taken.push(long[i - 1]);
        }
    }
    if ends {
        (taken, short.to_vec())
    } else {
        (short.to_vec(), taken)
    }
}

// Placing.

/// How much a way is kept in the middle of its line: the longer it is the surer
/// it was drawn there, and one with more lanes than its neighbours is mostly
/// where a line drawn for them goes on.
fn pull(way: &Way, length: f64, wider: bool) -> f64 {
    match way.profile.placed {
        Placed::Said => return 1e6,
        Placed::Transition => return 1e-3,
        Placed::Unsaid => {}
    }
    let pull = 0.003 + (length / 500.0).powi(2);
    if wider { pull * 0.15 } else { pull }
}

pub(super) fn length(way: &Way) -> f64 {
    let points = way.points.windows(2);
    points.map(|p| distance(p[0], p[1])).sum::<f64>() / way.scale
}

/// Two ways whose lanes should line up, each with whether it is met against
/// itself: how far right of where they are on the first those of the second are,
/// and how much that counts.
struct Tie {
    ways: [(u32, bool); 2],
    apart: f64,
    weight: f64,
}

/// Metres of a way that is half tied to what it meets at a junction: a longer one
/// makes up for it near the node.
const TIED: f64 = 60.0;
/// How much less what goes straight over a junction is: there lanes may shift.
const OVER: f64 = 0.2;

/// Moves the ways' cross-sections so that lanes go on from way to way: those
/// with a `placement` stay, the others are kept near the middle of their lines.
pub(super) fn place(ways: &mut [Way]) {
    let links = links(ways);
    let chains = links.chains();
    let merges = merges::find(ways);
    let of = |(way, reversed): (u32, bool)| {
        Across::new(&ways[way as usize].profile, reversed).expect("a motor road's")
    };
    let mut ties: Vec<Tie> = (chains.par_iter())
        .flat_map_iter(|chain| {
            chain.windows(2).map(|pair| {
                let (a, b) = (of(pair[0]), of(pair[1]));
                let middle = |x: &Across| (x.section.surface[0] + x.section.surface[1]) / 2.0;
                let apart = match kept(&a, &b) {
                    Some(kept) => shared(&b, &kept[1]) - shared(&a, &kept[0]),
                    None => middle(&b) - middle(&a),
                };
                Tie {
                    ways: [pair[0], pair[1]],
                    apart,
                    weight: 1.0,
                }
            })
        })
        .collect();
    let across = |(a, b): (u32, u32)| {
        let short = length(&ways[a as usize]).min(length(&ways[b as usize]));
        1.0 / (1.0 + (short / TIED).powi(2))
    };
    for merge in &merges {
        for (ways, apart) in merge.apart(ways) {
            let weight = across((ways[0].0, ways[1].0));
            ties.push(Tie {
                ways,
                apart,
                weight,
            });
        }
    }
    for over in junctions::over(ways, &merges).0 {
        let [a, b] = over.ends;
        let pair = [(a.0, a.1 == 0), (b.0, b.1 == 1)];
        let (x, y) = (of(pair[0]), of(pair[1]));
        // what it is not told of may as well be where it is
        if let Some(kept) = kept(&x, &y) {
            ties.push(Tie {
                ways: pair,
                apart: shared(&y, &kept[1]) - shared(&x, &kept[0]),
                weight: across((a.0, b.0)) * OVER,
            });
        }
    }
    // ways that follow in a chain, and have more lanes than one beside them
    let mut wider = vec![false; ways.len()];
    let lanes = |way: u32| {
        let lanes = ways[way as usize].profile.lanes.as_ref();
        lanes.map_or(0, |l| l.backward + l.forward)
    };
    for pair in chains.iter().flat_map(|chain| chain.windows(2)) {
        let (a, b) = (pair[0].0, pair[1].0);
        wider[a as usize] |= lanes(a) > lanes(b);
        wider[b as usize] |= lanes(b) > lanes(a);
    }
    // those tied to one another, in order
    let mut root: Vec<u32> = (0..ways.len() as u32).collect();
    fn find(root: &mut [u32], mut i: u32) -> u32 {
        while root[i as usize] != i {
            root[i as usize] = root[root[i as usize] as usize];
            i = root[i as usize];
        }
        i
    }
    for tie in &ties {
        let [a, b] = tie.ways.map(|w| find(&mut root, w.0));
        root[a.max(b) as usize] = a.min(b);
    }
    let mut groups: FxHashMap<u32, Vec<Tie>> = FxHashMap::default();
    for tie in ties {
        let at = find(&mut root, tie.ways[0].0);
        groups.entry(at).or_default().push(tie);
    }
    let moved: Vec<Vec<(u32, f64)>> = (groups.par_iter())
        .map(|(_, ties)| moves(ways, ties, &wider))
        .collect();
    for (way, by) in moved.into_iter().flatten() {
        ways[way as usize].profile.shift(by);
    }
}

/// The mean middle of the lanes of a way that go on.
fn shared(x: &Across, kept: &[bool]) -> f64 {
    x.mean((0..kept.len()).filter(|&i| kept[i]))
}

/// How far each of the ways of `ties` that is not where it should be moves.
fn moves(ways: &[Way], ties: &[Tie], wider: &[bool]) -> Vec<(u32, f64)> {
    if ties.iter().all(|t| t.apart.abs() < ALIKE) {
        return Vec::new();
    }
    let mut of: Vec<u32> = ties.iter().flat_map(|t| t.ways.map(|w| w.0)).collect();
    of.sort_unstable();
    of.dedup();
    let place = |way: u32| of.binary_search(&way).expect("every tie's is there");
    let n = of.len();
    // least squares of what does not line up and of what is off the middle
    let mut diagonal: Vec<f64> = (of.iter())
        .map(|&w| {
            let way = &ways[w as usize];
            pull(way, length(way), wider[w as usize])
        })
        .collect();
    let mut right = vec![0.0; n];
    let mut terms: Vec<(usize, usize, f64)> = Vec::with_capacity(ties.len());
    for tie in ties {
        let [i, j] = tie.ways.map(|w| place(w.0));
        let [p, q] = tie.ways.map(|w| if w.1 { -1.0 } else { 1.0 });
        diagonal[i] += tie.weight;
        diagonal[j] += tie.weight;
        right[i] += tie.weight * p * tie.apart;
        right[j] -= tie.weight * q * tie.apart;
        terms.push((i, j, -tie.weight * p * q));
    }
    // by conjugate gradients, each scaled by its diagonal
    let mut by = vec![0.0; n];
    let mut rest = right;
    let mut to: Vec<f64> = (0..n).map(|i| rest[i] / diagonal[i]).collect();
    let dot = |a: &[f64], b: &[f64]| a.iter().zip(b).map(|(a, b)| a * b).sum::<f64>();
    let scaled = |rest: &[f64]| -> f64 { (0..n).map(|i| rest[i] * rest[i] / diagonal[i]).sum() };
    let mut off = scaled(&rest);
    for _ in 0..(2 * n + 10).min(2000) {
        if off < 1e-12 {
            break;
        }
        let mut moved: Vec<f64> = (0..n).map(|i| to[i] * diagonal[i]).collect();
        for &(i, j, c) in &terms {
            moved[i] += c * to[j];
            moved[j] += c * to[i];
        }
        let step = off / dot(&to, &moved);
        for i in 0..n {
            by[i] += step * to[i];
            rest[i] -= step * moved[i];
        }
        let left = scaled(&rest);
        for i in 0..n {
            to[i] = rest[i] / diagonal[i] + left / off * to[i];
        }
        off = left;
    }
    (0..n)
        .filter_map(|i| {
            let p = &ways[of[i] as usize].profile;
            if p.placed == Placed::Said {
                return None;
            }
            let by = by[i].clamp(-p.width / 2.0, p.width / 2.0);
            (by.abs() >= ALIKE).then_some((of[i], by))
        })
        .collect()
}

// Shaping.

/// What two ways that follow each other make of the place they meet, each as
/// met going from the first to the second.
struct Joint {
    ways: [(u32, usize); 2],
    to: [Section; 2],
    onto: [Vec<Option<u8>>; 2],
    /// Whether the line at each edge goes on over the joint.
    on: [Vec<bool>; 2],
}

/// What they make of it; of two with the same lanes the `first` stays as it is.
fn meet(ways: [(u32, usize); 2], [a, b]: [&Across; 2], first: usize) -> Joint {
    let sections = [&a.section, &b.section];
    let Some(kept) = kept(a, b) else {
        // the wider is drawn in to the narrower
        let width = |s: &Section| s.surface[1] - s.surface[0];
        let narrow = usize::from(width(sections[1]) < width(sections[0]));
        let [left, right] = sections[narrow].surface;
        let wide = sections[1 - narrow];
        let draw = |x: f64| left + (x - wide.surface[0]) / width(wide).max(1e-9) * (right - left);
        let drawn = Section {
            surface: [left, right],
            lanes: wide.lanes.iter().map(|x| draw(*x)).collect(),
        };
        let to = if narrow == 0 {
            [sections[0].clone(), drawn]
        } else {
            [drawn, sections[1].clone()]
        };
        let each = |x: &Section| x.lanes.len();
        return Joint {
            ways,
            to,
            onto: sections.map(|s| vec![None; each(s)]),
            on: sections.map(|s| vec![true; each(s)]),
        };
    };
    let count = kept[0].iter().filter(|k| **k).count();
    // which edge of the lanes both have each edge is
    let of: [Vec<usize>; 2] = [0, 1].map(|x: usize| {
        let mut of = vec![0];
        of.extend(kept[x].iter().scan(0, |n, k| {
            *n += usize::from(*k);
            Some(*n)
        }));
        of
    });
    let whole = [0, 1].map(|x: usize| kept[x].iter().all(|k| *k));
    let shoulder = |x: usize| {
        let s = sections[x];
        let last = s.lanes.len() - 1;
        [s.surface[0] - s.lanes[0], s.surface[1] - s.lanes[last]]
    };
    let (edges, shoulders) = match whole {
        [false, false] => {
            // neither has only those: between the two
            let lane = |x: usize, k: usize| {
                let i = (0..kept[x].len()).filter(|&i| kept[x][i]).nth(k);
                let i = i.expect("as many are kept as are counted");
                (sections[x].lanes[i], sections[x].lanes[i + 1])
            };
            let mut middle = 0.0;
            let mut widths = Vec::with_capacity(count);
            for k in 0..count {
                let (p, q) = (lane(0, k), lane(1, k));
                middle += (p.0 + p.1 + q.0 + q.1) / 4.0 / count as f64;
                widths.push((p.1 - p.0 + q.1 - q.0) / 2.0);
            }
            let mut at = middle - widths.iter().sum::<f64>() / 2.0;
            let mut edges = vec![at];
            for width in widths {
                at += width;
                edges.push(at);
            }
            let (p, q) = (shoulder(0), shoulder(1));
            (edges, [(p[0] + q[0]) / 2.0, (p[1] + q[1]) / 2.0])
        }
        _ => {
            let x = match whole {
                [true, true] => first,
                [true, false] => 0,
                _ => 1,
            };
            (sections[x].lanes.clone(), shoulder(x))
        }
    };
    let surface = [edges[0] + shoulders[0], edges[count] + shoulders[1]];
    let to = [0, 1].map(|x: usize| Section {
        surface,
        lanes: of[x].iter().map(|&k| edges[k]).collect(),
    });
    // of the edges that are one there, the one whose line goes on: the surface's
    // own at its edges, else the first painted alike
    let lines = [&a.lines, &b.lines];
    let mut pick = vec![[0, 0]; count + 1];
    for (k, pick) in pick.iter_mut().enumerate() {
        let are = [0, 1].map(|x: usize| -> Vec<usize> {
            (0..of[x].len()).filter(|&i| of[x][i] == k).collect()
        });
        // every edge of those kept is one of each
        *pick = [0, 1].map(|x: usize| {
            let last = k == count;
            are[x][if last { are[x].len() - 1 } else { 0 }]
        });
        let alike = are[0]
            .iter()
            .flat_map(|&i| are[1].iter().map(move |&j| [i, j]))
            .find(|[i, j]| lines[0][*i].is_some() && lines[0][*i] == lines[1][*j]);
        if let Some(alike) = alike.filter(|_| k > 0 && k < count) {
            *pick = alike;
        }
    }
    let onto = [0, 1].map(|x: usize| {
        let each = of[x].iter().map(|&k| Some(pick[k][1 - x] as u8));
        each.collect()
    });
    let on = [0, 1].map(|x: usize| {
        let each = of[x].iter().enumerate().map(|(i, &k)| pick[k][x] == i);
        each.collect()
    });
    Joint { ways, to, onto, on }
}

/// How each way that follows another is shaped for it.
pub(super) fn shapes(ways: &[Way]) -> Shapes {
    let links = links(ways);
    let merges = merges::find(ways);
    let bridge = |w: &Way| matches!(w.profile.level, Level::Bridge(_));
    let pairs: Vec<[(u32, usize); 2]> = (0..ways.len() as u32)
        .flat_map(|way| [(way, 0), (way, 1)])
        .filter_map(|a| {
            let b = links.at(a.0, a.1)?;
            (a.0 < b.0).then_some([a, b])
        })
        .collect();
    let (over, within) = junctions::over(ways, &merges);
    let at_nodes = merges.iter().flat_map(|m| m.ends);
    let at_nodes = at_nodes.chain(over.iter().flat_map(|o| o.ends));
    let at_nodes = at_nodes.chain(within.iter().map(|w| w.end));
    let long: FxHashMap<u32, f64> = (pairs.iter().flatten().copied())
        .chain(at_nodes)
        .map(|(way, _)| (way, length(&ways[way as usize])))
        .collect();
    let mut joints: Vec<Joint> = pairs
        .par_iter()
        .map(|&[(a, end_a), (b, end_b)]| {
            let of = |way: u32, reversed| {
                Across::new(&ways[way as usize].profile, reversed).expect("a motor road's")
            };
            let (x, y) = (of(a, end_a == 0), of(b, end_b == 1));
            // of two with the same lanes the one that stays as it is: a bridge,
            // which is drawn as one width, or the shorter
            let first = match (bridge(&ways[a as usize]), bridge(&ways[b as usize])) {
                (true, false) => 0,
                (false, true) => 1,
                _ => usize::from(long[&b] < long[&a]),
            };
            meet([(a, end_a), (b, end_b)], [&x, &y], first)
        })
        .collect();
    let own = |way: u32, reversed: bool| {
        let section = Section::of(&ways[way as usize].profile);
        if reversed {
            section.mirrored()
        } else {
            section
        }
    };
    let turned = |joint: &Joint, x: usize| (joint.ways[x].1 == 0) == (x == 0);
    // which ends taper, for the room each has
    let mut needs: FxHashMap<(u32, usize), f64> = FxHashMap::default();
    for joint in &joints {
        for x in 0..2 {
            let apart = joint.to[x].apart(&own(joint.ways[x].0, turned(joint, x)));
            if apart > ALIKE {
                needs.insert(joint.ways[x], apart);
            }
        }
    }
    // and those at merges
    let parted: Vec<[Section; 3]> = merges.iter().map(|m| m.sections(ways)).collect();
    let own_of = |end: (u32, usize), x: usize| own(end.0, merges::against(end.1, x));
    for (merge, to) in merges.iter().zip(&parted) {
        for (x, to) in to.iter().enumerate() {
            let apart = to.apart(&own_of(merge.ends[x], x));
            if apart > ALIKE {
                needs.insert(merge.ends[x], apart);
            }
        }
    }
    let crossed = crossed(ways);
    // metres from a way's end to the first junction along it, or its other end
    let reach = |way: u32, end: usize| {
        let w = &ways[way as usize];
        let last = w.points.len() - 1;
        let order: Vec<usize> = if end == 0 {
            (0..=last).collect()
        } else {
            (0..=last).rev().collect()
        };
        let mut to = 0.0;
        for k in 1..order.len() {
            to += distance(w.points[order[k - 1]], w.points[order[k]]) / w.scale;
            if k < last && crossed.contains_key(&w.nodes[order[k]]) {
                break;
            }
        }
        to.min(long[&way])
    };
    // and roads that go straight over: the lanes that go on are moved to line up, by
    // what each has the room for
    let middle = |s: &Section| (s.surface[0] + s.surface[1]) / 2.0;
    let across: Vec<Mouths> = (over.iter())
        .map(|over| {
            let [a, b] = over.ends;
            let against = [a.1 == 0, b.1 == 1];
            let of = |x: usize| {
                let profile = &ways[over.ends[x].0 as usize].profile;
                Across::new(profile, against[x]).expect("a motor road's")
            };
            let (x, y) = (of(0), of(1));
            let apart = match kept(&x, &y) {
                Some(kept) => shared(&y, &kept[1]) - shared(&x, &kept[0]),
                None => middle(&y.section) - middle(&x.section),
            };
            let apart = if apart.abs() < ALIKE { 0.0 } else { apart };
            let room = [0, 1].map(|x: usize| {
                let (way, end) = over.ends[x];
                let other = bridge(&ways[over.ends[1 - x].0 as usize]);
                let deck = bridge(&ways[way as usize]) && !other;
                if deck { 0.0 } else { reach(way, end) }
            });
            let part = room[0] / (room[0] + room[1]).max(f64::MIN_POSITIVE);
            let to = [
                x.section.moved(apart * part),
                y.section.moved(-apart * (1.0 - part)),
            ];
            let mut whole = [[None; 2]; 2];
            let mut surface = [to[0].surface, to[1].surface];
            for i in 0..2 {
                let [p, q] = [0, 1].map(|x: usize| to[x].surface[i]);
                let wide = usize::from((q > p) == (i == 1));
                // what stands out past an arm is drawn in before, and of a corner that
                // shows the junction fills the rest
                let past = over.past[wide][i];
                if past > 0.0 && (p - q).abs() > ALIKE {
                    whole[wide][i] = Some(past);
                    surface[wide][i] = to[1 - wide].surface[i];
                } else if over.sides.iter().all(|s| s[i] >= INNER) {
                    (surface[0][i], surface[1][i]) = ((p + q) / 2.0, (p + q) / 2.0);
                }
            }
            (to, surface, whole)
        })
        .collect();
    for (over, (to, surface, _)) in over.iter().zip(&across) {
        for x in 0..2 {
            let end = over.ends[x];
            let own = own(end.0, merges::against(end.1, x));
            let apart = to[x].apart(&own).max(apart(surface[x], to[x].surface));
            if apart > ALIKE {
                needs.insert(end, apart);
            }
        }
    }
    for within in &within {
        let own = Section::of(&ways[within.end.0 as usize].profile);
        needs.insert(within.end, apart(within.surface, own.surface));
    }
    let wanted = |way: u32, apart: f64| {
        let full = taper(ways[way as usize].profile.speed);
        (full * (apart / LANE).clamp(0.25, 2.0)).max(SHORTEST)
    };
    // of what is free at both ends, half, else nearly all
    let free = |needs: &FxHashMap<(u32, usize), f64>, (way, end): (u32, usize)| {
        let taken = needs.contains_key(&(way, 1 - end));
        reach(way, end) * if taken { 0.5 } else { 0.95 }
    };
    let room = |way: u32, end: usize, shared: bool| {
        let to = reach(way, end);
        let w = &ways[way as usize];
        let last = w.points.len() - 1;
        let other = w.nodes[if end == 0 { last } else { 0 }];
        // what goes on after it, or ends there, takes none of it
        let free = !shared && !needs.contains_key(&(way, 1 - end)) && !crossed.contains_key(&other);
        let share = if free && to >= long[&way] { 0.95 } else { 0.5 };
        to * share
    };
    let mut lengths: FxHashMap<(u32, usize), f64> = FxHashMap::default();
    for (merge, to) in merges.iter().zip(&parted) {
        let ease = merge.ease(ways, &links, to);
        for (x, &(way, end)) in merge.ends.iter().enumerate() {
            let Some(&apart) = needs.get(&(way, end)) else {
                continue;
            };
            let full = match (merge.kept.is_some(), x) {
                (false, _) => wanted(way, apart).min(merges::PLAIN),
                (true, 0) => wanted(way, apart),
                (true, _) => wanted(way, apart).max(ease),
            };
            lengths.insert((way, end), full.min(free(&needs, (way, end))));
        }
    }
    // of those the part that is a junction's mouth
    let mut mouths: FxHashMap<(u32, usize), f64> = FxHashMap::default();
    for (over, (to, surface, _)) in over.iter().zip(&across) {
        for x in 0..2 {
            let (way, end) = over.ends[x];
            if !needs.contains_key(&(way, end)) {
                continue;
            }
            let moved = to[x].apart(&own(way, merges::against(end, x)));
            let free = free(&needs, (way, end));
            // one that only widens may begin to before the mouth
            let wide = [
                to[x].surface[0] - surface[x][0],
                surface[x][1] - to[x].surface[1],
            ];
            let mut mouth = over.mouth[x];
            if wide[0].min(wide[1]) > -ALIKE {
                mouth = mouth.max(WIDENS * wide[0].max(wide[1]));
            }
            let (mouth, length) = if moved > ALIKE {
                let mouth = mouth.min(free * 0.5);
                (mouth, mouth + wanted(way, moved).min(free - mouth))
            } else {
                (mouth.min(free), mouth.min(free))
            };
            lengths.insert((way, end), length);
            mouths.insert((way, end), mouth / length.max(f64::MIN_POSITIVE));
        }
    }
    for within in &within {
        let length = within.mouth.min(free(&needs, within.end));
        lengths.insert(within.end, length);
        mouths.insert(within.end, 1.0);
    }
    for joint in &mut joints {
        let taper = [0, 1].map(|x: usize| needs.get(&joint.ways[x]).copied());
        for x in 0..2 {
            let Some(apart) = taper[x] else { continue };
            let (way, end) = joint.ways[x];
            let full = wanted(way, apart);
            let mut length = full.min(room(way, end, false));
            lengths.insert((way, end), length);
            // a way too short for it leaves some to the one it meets, and a bridge
            // all to a road on the ground
            let (other, other_end) = joint.ways[1 - x];
            let mut more = full / 2.0 - length;
            if bridge(&ways[way as usize]) && !bridge(&ways[other as usize]) {
                let all = full.min(room(other, other_end, true));
                if all >= SHORTEST {
                    (length, more) = (0.0, all);
                }
            }
            if taper[1 - x].is_some() {
                continue;
            }
            let more = more.min(room(other, other_end, true));
            if more <= 0.0 || more < length.min(1.0) {
                continue;
            }
            lengths.insert((way, end), length);
            let t = more / (more + length);
            let section = own(way, turned(joint, x));
            let to = joint.to[x].mix(&section, t);
            let base = own(other, turned(joint, 1 - x));
            let lanes: Vec<f64> = joint.onto[1 - x]
                .iter()
                .enumerate()
                .map(|(i, j)| match j {
                    Some(j) => to.lanes[usize::from(*j)],
                    None => base.lanes[i],
                })
                .collect();
            joint.to[1 - x] = Section {
                surface: to.surface,
                lanes,
            };
            joint.to[x] = to;
            lengths.insert((other, other_end), more);
        }
    }
    // what the two ends of a way leave each other
    for way in long.keys() {
        let [p, q] = [0, 1].map(|end| lengths.get(&(*way, end)).copied().unwrap_or(0.0));
        let over = (p + q) / long[way].max(f64::MIN_POSITIVE);
        for end in [0, 1] {
            if let Some(length) = lengths.get_mut(&(*way, end)).filter(|_| over > 1.0) {
                *length /= over;
            }
        }
    }

    let mut out = Shapes {
        of: vec![0; ways.len()],
        ..Default::default()
    };
    for joint in joints {
        let inner = |x: usize| {
            let (way, end) = joint.ways[x];
            let w = &ways[way as usize];
            let last = w.points.len() - 1;
            if end == 0 {
                w.points[1]
            } else {
                w.points[last - 1]
            }
        };
        let at = {
            let (way, end) = joint.ways[0];
            let w = &ways[way as usize];
            w.points[if end == 0 { 0 } else { w.points.len() - 1 }]
        };
        let (p, q) = (inner(0), inner(1));
        let turn = (at[0] - p[0]) * (q[0] - at[0]) + (at[1] - p[1]) * (q[1] - at[1]);
        let straight = turn > STRAIGHT * distance(p, at) * distance(at, q);
        let Joint {
            ways: ends,
            to,
            onto,
            on,
        } = joint;
        let parts = to.into_iter().zip(onto).zip(on);
        for (x, ((mut to, mut onto), mut on)) in parts.enumerate() {
            let (way, end) = ends[x];
            let profile = &ways[way as usize].profile;
            let base = Section::of(profile);
            let mirrored = (end == 0) == (x == 0);
            if mirrored {
                to = to.mirrored();
                onto.reverse();
                on.reverse();
            }
            let (other, other_end) = ends[1 - x];
            if (other_end == 0) == (x == 1) {
                let other = Section::of(&ways[other as usize].profile);
                let last = (other.lanes.len() - 1) as u8;
                onto.iter_mut().flatten().for_each(|j| *j = last - *j);
            }
            let length = lengths.get(&(way, end)).copied().unwrap_or(0.0);
            let lined = lined(&base, &to, &on, length);
            let shape = out.add(way, long[&way]);
            shape.beyond[end] = straight.then(|| [q, p][x]);
            shape.meets[end] = Some(Box::new(Meet {
                length,
                from: 0.0,
                across: None,
                to,
                onto,
                lined,
            }));
        }
    }
    for (over, (to, surface, whole)) in over.iter().zip(across) {
        for (x, mut to) in to.into_iter().enumerate() {
            let (way, end) = over.ends[x];
            let length = lengths.get(&(way, end)).copied().unwrap_or(0.0);
            if length <= 0.0 {
                continue;
            }
            let from = length * mouths[&(way, end)];
            let mut across = Mouth {
                surface: surface[x],
                within: over.sides[x].map(|m| m.clamp(INNER, from.max(INNER))),
                whole: whole[x],
            };
            if merges::against(end, x) {
                to = to.mirrored();
                across.surface = [-surface[x][1], -surface[x][0]];
                across.within.reverse();
                across.whole.reverse();
            }
            let edges = to.lanes.len();
            let shape = out.add(way, long[&way]);
            shape.meets[end] = Some(Box::new(Meet {
                length,
                from,
                across: Some(across),
                to,
                onto: vec![None; edges],
                lined: vec![0.0; edges],
            }));
        }
    }
    for within in within {
        let (way, end) = within.end;
        let length = lengths[&(way, end)];
        let to = Section::of(&ways[way as usize].profile);
        let edges = to.lanes.len();
        let shape = out.add(way, long[&way]);
        shape.meets[end] = Some(Box::new(Meet {
            length,
            from: length,
            across: Some(Mouth {
                surface: within.surface,
                within: [length; 2],
                whole: [None; 2],
            }),
            to,
            onto: vec![None; edges],
            lined: vec![0.0; edges],
        }));
    }
    for (merge, to) in merges.iter().zip(parted) {
        let onward = merge.beyond(ways);
        for (x, to) in to.into_iter().enumerate() {
            let (way, end) = merge.ends[x];
            let against = merges::against(end, x);
            let mut to = if against { to.mirrored() } else { to };
            let length = lengths.get(&(way, end)).copied().unwrap_or(0.0);
            let base = Section::of(&ways[way as usize].profile);
            if length <= 0.0 {
                to = base.clone();
            }
            let on = merge.on(x, against);
            let plain = merge.out_of(&base, &to, length);
            let lined = plain.unwrap_or_else(|| lined(&base, &to, &on, length));
            let onto = vec![None; base.lanes.len()];
            let shape = out.add(way, long[&way]);
            (shape.beyond[end], shape.parts[end]) = onward[x];
            shape.meets[end] = Some(Box::new(Meet {
                length,
                from: 0.0,
                across: None,
                to,
                onto,
                lined,
            }));
        }
    }
    out.links = links;
    out.merges = merges;
    out
}

/// The cosine of the turn past which lines meet where they end, not on a mitre.
const STRAIGHT: f64 = 0.25;

/// Metres from the end at which the line at each edge begins: one that does not
/// go on, where the lane it parts off is wide enough.
fn lined(base: &Section, to: &Section, on: &[bool], length: f64) -> Vec<f64> {
    let narrow = |t: f64, i: usize| {
        let eased = ease(t);
        let at = |k: usize| to.lanes[k] + (base.lanes[k] - to.lanes[k]) * eased;
        let beside = [i.checked_sub(1), Some(i + 1).filter(|k| *k < on.len())];
        beside.into_iter().flatten().any(|k| {
            let whole = (base.lanes[k] - base.lanes[i]).abs();
            (at(k) - at(i)).abs() < LINED.min(whole * 0.9)
        })
    };
    (0..on.len())
        .map(|i| {
            if on[i] || length <= 0.0 {
                return 0.0;
            }
            let steps = 4 * STEPS;
            let first = (0..steps).find(|k| !narrow(f64::from(*k) / f64::from(steps), i));
            length * f64::from(first.unwrap_or(steps)) / f64::from(steps)
        })
        .collect()
}

/// The nodes where three or more arms meet, driveways apart.
fn crossed(ways: &[Way]) -> FxHashMap<i64, u32> {
    let mut uses: FxHashMap<i64, u32> = FxHashMap::default();
    for way in ways {
        if !way.profile.class.is_motor() || way.profile.class == Class::Service {
            continue;
        }
        let last = way.nodes.len() - 1;
        for (i, &node) in way.nodes.iter().enumerate() {
            *uses.entry(node).or_default() += if i == 0 || i == last { 1 } else { 2 };
        }
    }
    uses.retain(|_, n| *n > 2);
    uses
}

// Lines.

/// The line at an edge of the lanes along a run.
pub(super) struct Drawn {
    pub points: Vec<Point>,
    /// Metres of each along the run's whole line, and all of that.
    pub lengths: Vec<f64>,
    pub whole: f64,
}

impl Shapes {
    /// The line at `edge` along `run` (which begins `from` metres along its way)
    /// without `trim` metres at the run's start and end. It runs as its way does.
    pub(super) fn line(
        &self,
        ways: &[Way],
        run: &Edge,
        from: f64,
        edge: usize,
        trim: [f64; 2],
    ) -> Option<Drawn> {
        let way = &ways[run.way as usize];
        let shape = self.of(run.way);
        let points = &way.points[run.points.clone()];
        let lengths = geometry::lengths(points, way.scale, from);
        let mut span = [from + trim[0], lengths[lengths.len() - 1] - trim[1]];
        let mut beyond = [None; 2];
        if let Some(shape) = shape {
            let towards = |end: usize| match shape.parts[end] {
                Some((first, point)) if edge >= first => Some(point),
                _ => shape.beyond[end],
            };
            if run.points.start == 0 {
                beyond[0] = towards(0);
            }
            if run.points.end == way.points.len() {
                beyond[1] = towards(1);
            }
            if let Some(meet) = &shape.meets[0] {
                span[0] = span[0].max(meet.lined[edge]);
            }
            if let Some(meet) = &shape.meets[1] {
                span[1] = span[1].min(shape.length - meet.lined[edge]);
            }
        }
        if span[1] <= span[0] {
            return None;
        }
        let mut extra = self.steps(run.way);
        extra.extend(span);
        extra.sort_unstable_by(f64::total_cmp);
        let (points, at) = geometry::stations(points, &lengths, &extra);
        let moved = if self.tapered(run.way).is_some() {
            let by: Vec<f64> = (at.iter())
                .map(|&s| self.at(ways, run.way, s).lanes[edge] * way.scale)
                .collect();
            geometry::displaced(&points, |k| by[k], beyond)
        } else {
            let by = Section::of(&way.profile).lanes[edge] * way.scale;
            geometry::displaced(&points, |_| by, beyond)
        };
        let kept = geometry::forwards(&points, &moved);
        let moved: Vec<Point> = kept.iter().map(|&k| moved[k]).collect();
        let along = geometry::lengths(&moved, way.scale, 0.0);
        let within = |k: &usize| (span[0] - 1e-2..=span[1] + 1e-2).contains(&at[kept[*k]]);
        let of: Vec<usize> = (0..moved.len()).filter(within).collect();
        (of.len() > 1).then(|| Drawn {
            points: of.iter().map(|&k| moved[k]).collect(),
            lengths: of.iter().map(|&k| along[k]).collect(),
            whole: along[along.len() - 1],
        })
    }
}

/// The dashes of the runs: which way along each they are laid, and where in
/// their period its lines begin.
#[derive(Default)]
pub(super) struct Dashes {
    of: FxHashMap<u32, Laid>,
}

/// Whether a run's are laid against its way, and by edge of its lanes the metres
/// of the period gone where the run begins (as they are laid).
type Laid = (bool, Vec<f64>);

impl Dashes {
    /// A line of a run as its dashes are laid: its points, the metres along it of
    /// each, and those of the `period` gone at the first.
    pub(super) fn laid(
        &self,
        run: u32,
        edge: usize,
        drawn: Drawn,
        period: f64,
    ) -> (Vec<Point>, Vec<f64>, f64) {
        let Drawn {
            mut points,
            mut lengths,
            whole,
        } = drawn;
        let mut gone = 0.0;
        if let Some((reversed, at)) = self.of.get(&run) {
            gone = at[edge];
            if *reversed {
                points.reverse();
                lengths.reverse();
                lengths.iter_mut().for_each(|s| *s = whole - *s);
            }
        }
        let phase = (gone + lengths[0]) % period;
        (points, lengths, phase)
    }

    /// The metres of a period gone `at` metres along a run's line at `edge`, of
    /// `whole` metres, taken along its way, whose dashes are `[on, period]`.
    pub(super) fn gone(&self, run: u32, edge: usize, whole: f64, at: f64, dash: [f64; 2]) -> f64 {
        match self.of.get(&run) {
            None => at % dash[1],
            Some((false, gone)) => (gone[edge] + at) % dash[1],
            // the dash that ends where one laid the other way begins
            Some((true, gone)) => (dash[0] - gone[edge] - (whole - at)).rem_euclid(dash[1]),
        }
    }

    pub(super) fn new(ways: &[Way], runs: &[Edge], from: &[f64], shapes: &Shapes) -> Self {
        // each way's runs
        let mut at: Vec<[u32; 2]> = vec![[0; 2]; ways.len()];
        for (i, run) in runs.iter().enumerate() {
            let at = &mut at[run.way as usize];
            if at[1] == 0 {
                at[0] = i as u32;
            }
            at[1] = i as u32 + 1;
        }
        let mut chains = shapes.links.chains();
        for way in 0..ways.len() as u32 {
            let [first, after] = at[way as usize];
            if shapes.links.0[way as usize] == [NONE; 2] && after > first + 1 {
                chains.push(vec![(way, false)]);
            }
        }
        let dashed = |way: u32| {
            ways[way as usize]
                .profile
                .lines
                .iter()
                .any(|l| l.1.dashed())
        };
        let found: Vec<Vec<(u32, Laid)>> = chains
            .par_iter()
            .map(|chain| {
                let mut out = Vec::new();
                if !chain.iter().any(|c| dashed(c.0)) {
                    return out;
                }
                // what is gone at each edge where the way before ended
                let mut before: Vec<f64> = Vec::new();
                for &(way, reversed) in chain {
                    let p = &ways[way as usize].profile;
                    let edges = Section::of(p).lanes.len();
                    let onto = (shapes.of(way))
                        .and_then(|s| s.meets[usize::from(reversed)].as_ref())
                        .map(|m| &m.onto);
                    let mut gone: Vec<f64> = (0..edges)
                        .map(|i| {
                            let j = onto.and_then(|o| o[i]);
                            j.and_then(|j| before.get(usize::from(j)).copied())
                                .unwrap_or(0.0)
                        })
                        .collect();
                    let [first, after] = at[way as usize];
                    let count = after - first;
                    for k in 0..count {
                        let index = if reversed { after - 1 - k } else { first + k };
                        let run = &runs[index as usize];
                        if dashed(way) {
                            out.push((index, (reversed, gone.clone())));
                        }
                        let points = &ways[way as usize].points[run.points.clone()];
                        let plain = geometry::lengths(points, ways[way as usize].scale, 0.0);
                        let plain = plain[plain.len() - 1];
                        for (i, gone) in gone.iter_mut().enumerate() {
                            let painted = p.lines.iter().any(|l| edge(p, l.0) == i);
                            let line = if painted {
                                let from = from[index as usize];
                                shapes.line(ways, run, from, i, [0.0; 2])
                            } else {
                                None
                            };
                            // an opening lane's own is as long as if it had none
                            let long = line.map_or(plain, |l| l.whole);
                            *gone = (*gone + long) % p.rules.pattern()[1];
                        }
                    }
                    before = gone;
                }
                out
            })
            .collect();
        Self {
            of: found.into_iter().flatten().collect(),
        }
    }
}

/// How a piece of a structure differs from its way's profile.
pub(super) struct Changes {
    /// At its start and its end the section, and metres until it is the way's.
    pub tapers: [Option<Taper>; 2],
    /// Of each of its way's lines the metres of a dash's period gone at its
    /// start, if any.
    pub gone: Option<Vec<f64>>,
    /// Of each the metres from the start and from the end that it is not painted,
    /// if any.
    pub bare: [Option<Vec<f64>>; 2],
}

pub(super) struct Taper {
    pub length: f64,
    pub to: Section,
}

impl Shapes {
    /// The section of a way between two places along it, metres, at each if it
    /// is not the way's own.
    pub(super) fn ends(&self, ways: &[Way], way: u32, span: [f64; 2]) -> [Option<Taper>; 2] {
        let Some(shape) = self.tapered(way) else {
            return [None, None];
        };
        let base = Section::of(&ways[way as usize].profile);
        [0, 1].map(|end: usize| {
            let meet = shape.meets[end].as_ref()?;
            let from = if end == 0 {
                span[0]
            } else {
                shape.length - span[1]
            };
            let to = self.at(ways, way, span[end]);
            let length = (meet.length - from).min(span[1] - span[0]);
            (length > 0.0 && to.apart(&base) > ALIKE).then_some(Taper { length, to })
        })
    }

    /// Of each line of a way between two places along it, metres, how far from
    /// each it begins, if any does not there.
    pub(super) fn bare(&self, ways: &[Way], way: u32, span: [f64; 2]) -> [Option<Vec<f64>>; 2] {
        let p = &ways[way as usize].profile;
        [0, 1].map(|end: usize| {
            let shape = self.of(way)?;
            let meet = shape.meets[end].as_ref()?;
            let from = if end == 0 {
                span[0]
            } else {
                shape.length - span[1]
            };
            let of = |line: &(f64, Line)| {
                let begins = meet.lined[edge(p, line.0)];
                (begins - from).clamp(0.0, (span[1] - span[0]).max(0.0))
            };
            let bare: Vec<f64> = p.lines.iter().map(of).collect();
            bare.iter().any(|m| *m >= 0.05).then_some(bare)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::network;
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

    /// One-way roads of these lanes and arrows, one after the other.
    fn one_way(parts: &[(&'static str, &'static str, f64)]) -> Vec<Way> {
        let mut x = 0.0;
        let mut ways = Vec::new();
        for (i, (lanes, turns, long)) in parts.iter().enumerate() {
            let tags = [
                ("highway", "primary"),
                ("oneway", "yes"),
                ("lanes", *lanes),
                ("turn:lanes", *turns),
            ];
            let i = i as i64;
            ways.push(way(&tags, &[(i, x, 0.0), (i + 1, x + long, 0.0)]));
            x += long;
        }
        ways
    }

    fn left(way: &Way) -> f64 {
        way.profile.lanes.as_ref().unwrap().left
    }

    /// The places of `b`'s lanes that go on as one of `a`'s.
    fn go_on(a: &Way, b: &Way) -> Vec<bool> {
        let of = |w| Across::new(w, false).unwrap();
        kept(&of(&a.profile), &of(&b.profile)).unwrap()[1].clone()
    }

    #[test]
    fn turn_lanes_open_on_their_side_and_merging_ones_end() {
        let opens = |before: &'static str, after: &'static str| {
            let count = |t: &str| ["1", "2", "3", "4"][t.split('|').count() - 1];
            let w = one_way(&[(count(before), before, 90.0), (count(after), after, 90.0)]);
            go_on(&w[0], &w[1])
        };
        assert_eq!(opens("|", "left|through|through"), [false, true, true]);
        assert_eq!(opens("|", "through|through|right"), [true, true, false]);
        assert_eq!(
            opens("|", "left|through|through|right"),
            [false, true, true, false]
        );
        // what is on both goes on
        assert_eq!(
            opens("left|through", "left|through|right"),
            [true, true, false]
        );
        // nothing says: the outermost
        assert_eq!(opens("|", "||"), [true, true, false]);
        let ends = one_way(&[("3", "|merge_to_right|", 90.0), ("2", "", 90.0)]);
        assert_eq!(go_on(&ends[1], &ends[0]), [true, true, false]);
        let a = Across::new(&ends[0].profile, false).unwrap();
        let b = Across::new(&ends[1].profile, false).unwrap();
        assert_eq!(kept(&a, &b).unwrap()[0], [true, false, true]);
    }

    #[test]
    fn where_traffic_keeps_left_its_outermost_lane_is_the_left_one() {
        use super::super::country;
        use super::super::profile::profile_in;
        let keep_left = |mut ways: Vec<Way>, tags: &[&[(&str, &'static str)]]| {
            for (w, t) in ways.iter_mut().zip(tags) {
                let get = |k: &str| t.iter().find(|x| x.0 == k).map(|x| x.1);
                w.profile = profile_in(get, country::rules("GB")).unwrap();
            }
            ways
        };
        let one = |lanes: &'static str, turns: &'static str| {
            vec![
                ("highway", "primary"),
                ("oneway", "yes"),
                ("lanes", lanes),
                ("turn:lanes", turns),
            ]
        };
        let opens = |before: &'static str, after: &'static str| {
            let count = |t: &str| ["1", "2", "3", "4"][t.split('|').count() - 1];
            let tags = [one(count(before), before), one(count(after), after)];
            let w = one_way(&[(count(before), before, 90.0), (count(after), after, 90.0)]);
            let w = keep_left(w, &[&tags[0], &tags[1]]);
            go_on(&w[0], &w[1])
        };
        assert_eq!(opens("|", "||"), [false, true, true]);
        // what the arrows say is as their drivers see them, either side
        assert_eq!(opens("|", "left|through|through"), [false, true, true]);
        assert_eq!(opens("|", "through|through|right"), [true, true, false]);
        // a two-way road's lanes along it are its left ones
        let two = [("highway", "primary"), ("lanes", "2")];
        let three = [
            ("highway", "primary"),
            ("lanes", "3"),
            ("lanes:forward", "2"),
            ("lanes:backward", "1"),
            ("turn:lanes:forward", "through|right"),
        ];
        let a = way(&two, &[(1, 0.0, 0.0), (2, 300.0, 0.0)]);
        let b = way(&three, &[(2, 300.0, 0.0), (3, 400.0, 0.0)]);
        let ways = keep_left(vec![a, b], &[&two, &three]);
        // the right turn opens between the directions
        assert_eq!(go_on(&ways[0], &ways[1]), [true, false, true]);
        let lanes = ways[1].profile.lanes.as_ref().unwrap();
        assert_eq!(
            (lanes.lane(0), lanes.lane(1), lanes.lane(2)),
            ((1, 0), (1, 1), (-1, 0))
        );
        assert_eq!(lanes.span(true), [-5.25, 1.75]);
        let across = Across::new(&ways[1].profile, false).unwrap();
        assert_eq!(across.lanes[1].1, [Turn::Right]);
        assert_eq!(across.lines[2], Some(Line::Centre));
    }

    #[test]
    fn a_two_way_roads_lanes_go_on_by_direction_whichever_way_it_is_drawn() {
        let two = [("highway", "primary"), ("lanes", "2")];
        let three = [
            ("highway", "primary"),
            ("lanes", "3"),
            ("lanes:forward", "2"),
            ("lanes:backward", "1"),
            ("turn:lanes:forward", "left|through"),
        ];
        let a = way(&two, &[(1, 0.0, 0.0), (2, 300.0, 0.0)]);
        let b = way(&three, &[(2, 300.0, 0.0), (3, 400.0, 0.0)]);
        assert_eq!(go_on(&a, &b), [true, false, true]);
        // the left turn opens between the directions: neither way moves
        let mut ways = [a, b];
        place(&mut ways);
        assert_eq!([left(&ways[0]), left(&ways[1])], [-3.5, -5.25]);
        // drawn the other way the one lane of its own is against it
        let back = [
            ("highway", "primary"),
            ("lanes", "3"),
            ("lanes:forward", "1"),
            ("lanes:backward", "2"),
        ];
        let c = way(&back, &[(3, 400.0, 0.0), (2, 300.0, 0.0)]);
        let mut ways = [way(&two, &[(1, 0.0, 0.0), (2, 300.0, 0.0)]), c];
        place(&mut ways);
        // its right lane opens, on its own left: the rest is in line
        let shapes = shapes(&ways);
        let (p, q) = (shapes.at(&ways, 0, 300.0), shapes.at(&ways, 1, 100.0));
        assert!((p.lanes[1] + q.lanes[2]).abs() < 0.07, "{p:?} {q:?}");
        assert!((left(&ways[1]) - -7.0).abs() < 0.1, "{}", left(&ways[1]));
    }

    #[test]
    fn a_short_way_moves_a_long_one_stays_and_a_placement_stays() {
        let mut ways = one_way(&[("2", "", 800.0), ("3", "through|through|right", 60.0)]);
        place(&mut ways);
        assert!((left(&ways[0]) - -3.5).abs() < 0.06);
        assert!((left(&ways[1]) - -3.5).abs() < 0.06, "{}", left(&ways[1]));
        // of two as long each is in its middle, and the lanes move over the taper
        let mut ways = one_way(&[("2", "", 8000.0), ("3", "through|through|right", 8000.0)]);
        place(&mut ways);
        assert!((left(&ways[1]) - -5.25).abs() < 0.1);
        let mut ways = one_way(&[("2", "", 800.0), ("3", "through|through|right", 60.0)]);
        ways[1].profile.placed = Placed::Said;
        place(&mut ways);
        assert_eq!(left(&ways[1]), -5.25);
    }

    #[test]
    fn errors_do_not_add_up_along_a_chain() {
        // a lane opens on the right, and ends on the left, again and again
        let mut parts = Vec::new();
        for _ in 0..20 {
            parts.push(("2", "", 400.0));
            parts.push(("3", "merge_to_right||", 400.0));
        }
        let mut ways = one_way(&parts);
        place(&mut ways);
        for w in &ways {
            assert!(w.profile.centre.abs() < 4.0, "{}", w.profile.centre);
        }
    }

    #[test]
    fn ways_meet_with_the_same_section_and_taper_within_their_room() {
        for (parts, speed) in [
            (vec![("2", "", 200.0), ("3", "||right", 200.0)], 50.0),
            (vec![("2", "", 500.0), ("3", "||right", 400.0)], 120.0),
            (
                vec![("3", "", 100.0), ("1", "", 8.0), ("4", "", 100.0)],
                50.0,
            ),
            (
                vec![("2", "", 100.0), ("4", "", 5.0), ("2", "", 100.0)],
                90.0,
            ),
        ] {
            let mut ways = one_way(&parts);
            ways.iter_mut().for_each(|w| w.profile.speed = speed);
            place(&mut ways);
            let shapes = shapes(&ways);
            for i in 1..ways.len() {
                let before = shapes.at(&ways, i as u32 - 1, parts[i - 1].2);
                let after = shapes.at(&ways, i as u32, 0.0);
                for e in 0..2 {
                    let step = before.surface[e] - after.surface[e];
                    assert!(step.abs() < ALIKE, "{parts:?} {before:?} {after:?}");
                }
                let shape = shapes.of(i as u32).unwrap();
                let taken: f64 = shape.meets.iter().flatten().map(|m| m.length).sum();
                assert!(taken <= parts[i].2 + 1e-9);
            }
        }
        // half a lane at half of 30 m in town, and more of a fast road
        let length = |speed| {
            let mut ways = one_way(&[("2", "", 500.0), ("3", "||right", 400.0)]);
            ways[1].profile.speed = speed;
            place(&mut ways);
            let shapes = shapes(&ways);
            let meet = shapes.of(1).unwrap().meets[0].clone().unwrap();
            (meet.length, shapes.at(&ways, 1, meet.length / 2.0))
        };
        let (town, section) = length(50.0);
        assert!((28.0..31.0).contains(&town), "{town}");
        assert!((section.lanes[3] - section.lanes[2] - 1.75).abs() < 0.05);
        assert!((115.0..125.0).contains(&length(120.0).0));
        // a driveway at the joint is no junction, another road is
        let mut ways = one_way(&[("2", "", 200.0), ("3", "||right", 200.0)]);
        ways.push(way(
            &[("highway", "service")],
            &[(1, 200.0, 0.0), (9, 200.0, 50.0)],
        ));
        assert!(shapes_of(&ways).tapered(1).is_some());
        ways[2] = way(
            &[("highway", "residential")],
            &[(1, 200.0, 0.0), (9, 200.0, 50.0)],
        );
        let over = shapes_of(&ways);
        assert!(over.links.at(1, 0).is_none());
        assert!(over.of(1).unwrap().meets[0].as_ref().unwrap().from > 0.0);
    }

    fn shapes_of(ways: &[Way]) -> Shapes {
        shapes(ways)
    }

    #[test]
    fn a_way_too_short_for_its_taper_leaves_some_to_the_next() {
        let mut ways = one_way(&[("2", "", 300.0), ("3", "||right", 8.0)]);
        place(&mut ways);
        let shapes = shapes(&ways);
        let of = |way: u32, end: usize| shapes.of(way).unwrap().meets[end].clone().unwrap();
        assert!(of(0, 1).length > 5.0 && of(1, 0).length < 8.0);
        // its outer lane is as wide as the two are there
        let (before, after) = (shapes.at(&ways, 0, 300.0), shapes.at(&ways, 1, 0.0));
        assert!((before.lanes[2] - after.lanes[3]).abs() < 1e-6);
        assert!((before.lanes[1] - after.lanes[1]).abs() < 1e-6);
        assert!((1.0..3.4).contains(&(after.lanes[3] - after.lanes[2])));
    }

    #[test]
    fn a_taper_is_one_surface_with_no_step() {
        let mut ways = one_way(&[("2", "", 200.0), ("3", "||right", 100.0), ("2", "", 200.0)]);
        // with a bend in it
        ways[1].nodes.insert(1, 50);
        ways[1].points.insert(1, [215.0, 2.0]);
        place(&mut ways);
        let shapes = shapes(&ways);
        let rect = [-1e3, -1e3, 1e3, 1e3];
        let mut ground = Vec::new();
        for (i, w) in ways.iter().enumerate() {
            let (tapers, taken) = shapes.tapers(&ways, i as u32, 0.25, rect);
            ground.extend(tapers);
            if let Some(points) = geometry::trim(&w.points, taken) {
                let middle = geometry::offset(&points, w.profile.centre);
                let middle = geometry::extend(&middle, 0.25);
                ground.extend(geometry::ribbon(&middle, w.profile.width));
            }
        }
        let surface = geometry::union(&ground);
        assert_eq!((surface.len(), surface[0].len()), (1, 1));
        let tile = geometry::to_tile(&surface, [0.0, -50.0, 500.0, 50.0], [0.0; 2], 1);
        assert_eq!((tile.len(), tile[0].len()), (1, 1));
        // no edge of it runs across the road but at its ends
        let ring = &surface[0][0];
        for k in 0..ring.len() {
            let (p, q) = (ring[k], ring[(k + 1) % ring.len()]);
            let within = p[0] > 1.0 && p[0] < 499.0;
            let (along, across) = ((q[0] - p[0]).abs(), (q[1] - p[1]).abs());
            assert!(
                !within || across < 0.15 || across < along * 0.6,
                "{p:?} {q:?}"
            );
        }
    }

    #[test]
    fn what_cannot_go_on_is_drawn_in_and_rings_come_round() {
        let narrow = way(
            &[("highway", "residential"), ("lanes", "1")],
            &[(1, 0.0, 0.0), (2, 100.0, 0.0)],
        );
        let wide = way(
            &[("highway", "residential")],
            &[(2, 100.0, 0.0), (3, 200.0, 0.0)],
        );
        let ways = [narrow, wide];
        let shapes = shapes(&ways);
        let at = shapes.at(&ways, 1, 0.0);
        assert_eq!(at.surface, shapes.at(&ways, 0, 100.0).surface);
        assert!(at.lanes[1].abs() < 1e-9);
        // two ways that are a ring, and two that face each other
        let mut ring = one_way(&[("1", "", 90.0), ("2", "", 90.0)]);
        ring[1].nodes[1] = 0;
        ring[1].points[1] = [0.0, 0.0];
        ring[1].points.insert(1, [90.0, 60.0]);
        ring[1].nodes.insert(1, 77);
        place(&mut ring);
        assert!(ring.iter().all(|w| w.profile.centre.is_finite()));
        let chains = links(&ring).chains();
        assert_eq!(chains, [vec![(0, false), (1, false)]]);
        let mut facing = one_way(&[("2", "", 90.0), ("3", "", 90.0)]);
        facing[1].points.reverse();
        facing[1].nodes.reverse();
        let across = |w, reversed| Across::new(w, reversed).unwrap();
        let (a, b) = (
            across(&facing[0].profile, false),
            across(&facing[1].profile, true),
        );
        assert!(kept(&a, &b).is_none());
        place(&mut facing);
        assert!(facing.iter().all(|w| w.profile.centre == 0.0));
    }

    #[test]
    fn a_sections_spans_are_its_lanes_where_nothing_changes() {
        for tags in [
            &[("highway", "primary"), ("lanes", "3")][..],
            &[("highway", "residential"), ("lanes", "1")],
            &[("highway", "primary"), ("oneway", "-1"), ("lanes", "2")],
        ] {
            let p = profile(|k| tags.iter().find(|t| t.0 == k).map(|t| t.1)).unwrap();
            let lanes = p.lanes.as_ref().unwrap();
            let section = Section::of(&p);
            for along in [true, false] {
                assert_eq!(section.span(lanes, along), lanes.span(along));
            }
        }
    }

    #[test]
    fn lines_meet_where_ways_bend() {
        let tags = [("highway", "primary"), ("lanes", "4")];
        let ways = [
            way(&tags, &[(1, 0.0, 0.0), (2, 60.0, 0.0)]),
            way(&tags, &[(3, 110.0, 30.0), (2, 60.0, 0.0)]),
        ];
        let n = network::build(&ways);
        let ends = |run: usize, edge: usize| n.line(&ways, run, edge, [0.0; 2]).unwrap().points;
        for edge in 1..4 {
            let (a, b) = (ends(0, edge), ends(1, 4 - edge));
            let (a, b) = (a[a.len() - 1], b[b.len() - 1]);
            assert!(distance(a, b) < 1e-9, "{a:?} {b:?}");
        }
    }

    #[test]
    fn a_lanes_line_begins_where_it_is_a_lane() {
        let mut ways = one_way(&[("2", "", 300.0), ("3", "||right", 200.0)]);
        place(&mut ways);
        let n = network::build(&ways);
        let from = |edge: usize| n.line(&ways, 1, edge, [0.0; 2]).unwrap().points[0][0];
        // the line between the lanes that go on is there from the start
        assert_eq!(from(1), 300.0);
        assert!((308.0..320.0).contains(&from(2)), "{}", from(2));
    }

    #[test]
    fn dashes_go_on_from_way_to_way_and_tile_to_tile() {
        // the second is drawn the other way, and the third goes on from it
        let tags = [("highway", "primary")];
        let ways = [
            way(&tags, &[(1, 0.0, 0.0), (2, 64.0, 0.0)]),
            way(&tags, &[(3, 101.0, 0.0), (2, 64.0, 0.0)]),
            way(&tags, &[(3, 101.0, 0.0), (4, 300.0, 0.0)]),
        ];
        let n = network::build(&ways);
        let mut painted: Vec<[f64; 2]> = Vec::new();
        for (i, run) in n.runs.iter().enumerate() {
            let drawn = n.line(&ways, i, 1, [0.0; 2]).unwrap();
            let (points, lengths, phase) = n.dashes.laid(i as u32, 1, drawn, DASH[1]);
            for tile in 0..4 {
                // as a renderer draws it: dashes from the part's start, cut at the tile
                let (west, east) = (f64::from(tile) * 80.0, f64::from(tile + 1) * 80.0);
                let rect = [west - 1.0, -9.0, east + 1.0, 9.0];
                for part in geometry::clip_dashed(&points, &lengths, rect, phase, DASH) {
                    let (a, b) = (part[0][0], part[part.len() - 1][0]);
                    assert!(a >= west - 1.0 - DASH[1] && b <= east + 1.0);
                    let mut at = a;
                    while at < b {
                        let dash = [at.max(west), (at + DASH[0]).min(b).min(east)];
                        if dash[1] > dash[0] + 1e-6 {
                            painted.push(dash);
                        }
                        at += DASH[1];
                    }
                }
            }
            assert_eq!(run.way, i as u32);
        }
        painted.sort_by(|a, b| a[0].total_cmp(&b[0]));
        let mut whole: Vec<[f64; 2]> = Vec::new();
        for p in painted {
            match whole.last_mut() {
                Some(last) if p[0] - last[1] < 1e-6 => last[1] = p[1],
                _ => whole.push(p),
            }
        }
        for (k, dash) in whole.iter().enumerate() {
            let from = k as f64 * DASH[1];
            let to = from + DASH[0];
            assert!((dash[0] - from).abs() < 0.21, "{k} {dash:?}");
            assert!((dash[1] - to).abs() < 0.21, "{k} {dash:?}");
        }
        assert_eq!(whole.len(), 50);
        // and where a piece of the second begins it is told as laid along it
        let gone = n.dashes.gone(1, 1, 37.0, 0.0, DASH);
        assert!((gone - (2.0_f64 - 101.0).rem_euclid(6.0)).abs() < 1e-9);
    }
}
