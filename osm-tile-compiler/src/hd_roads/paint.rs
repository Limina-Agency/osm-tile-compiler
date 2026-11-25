//! One place holds one painted thing: crossings, lines traffic stops at, gores,
//! arrows, merges' lines, guides and lanes' lines, in that order, each cut or left out
//! where one before it lies, on its own way or on another whose surface it shares.

use super::details::{Detail, Details, Kind};
use super::network::{Network, Point, Way, distance};
use super::profile::Level;
use super::structures::Group;
use super::{geometry, spans};
use rustc_hash::FxHashMap;

/// Metres kept between one painted thing and another, and between an arrow and
/// the lines beside it.
const APART: f64 = 0.3;
const BESIDE: f64 = 0.1;
/// Of a crossing or a line, cut, the least that is kept: metres, and the share
/// of the whole.
const LEAST: f64 = 1.5;
const LEAST_SHARE: f64 = 0.4;
/// Metres from a node two ways share within which their surfaces are one,
/// whatever their levels.
const JOINED: f64 = 30.0;
/// World units of a cell blocks are looked up by: about 25 m at 50 degrees.
const CELL: f64 = 256.0;
/// Metres across a lane's line, the widest.
const LINE: f64 = 0.15;
/// The most metres a junction keeps lines back on a deck: where more, roads
/// part side by side, and their lines go on.
const JUNCTION: f64 = 15.0;

/// A rectangle of the ground, world units.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Block {
    middle: Point,
    /// A unit vector; right of it is `[-y, x]`.
    along: Point,
    /// Either way along, and right.
    half: [f64; 2],
}

fn dot(a: Point, b: Point) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}

fn less(a: Point, b: Point) -> Point {
    [a[0] - b[0], a[1] - b[1]]
}

/// What is left of `[0, length]` without `taken`, which are in order.
pub(super) fn left(length: f64, taken: &[[f64; 2]]) -> Vec<[f64; 2]> {
    let mut out = Vec::new();
    let mut from = 0.0;
    for t in taken {
        if t[0] > from {
            out.push([from, t[0]]);
        }
        from = from.max(t[1]);
    }
    if length > from {
        out.push([from, length]);
    }
    out
}

/// In order, those that touch as one.
fn joined(mut stretches: Vec<[f64; 2]>) -> Vec<[f64; 2]> {
    stretches.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    let mut out: Vec<[f64; 2]> = Vec::with_capacity(stretches.len());
    for s in stretches {
        match out.last_mut() {
            Some(last) if s[0] <= last[1] => last[1] = last[1].max(s[1]),
            _ => out.push(s),
        }
    }
    out
}

impl Block {
    /// Metres right of and ahead of `at`, facing `direction`.
    pub(super) fn new(
        at: Point,
        direction: Point,
        scale: f64,
        right: [f64; 2],
        ahead: [f64; 2],
    ) -> Self {
        let x = (right[0] + right[1]) / 2.0 * scale;
        let y = (ahead[0] + ahead[1]) / 2.0 * scale;
        Self {
            middle: [
                at[0] + direction[0] * y - direction[1] * x,
                at[1] + direction[1] * y + direction[0] * x,
            ],
            along: direction,
            half: [
                (ahead[1] - ahead[0]) / 2.0 * scale,
                (right[1] - right[0]) / 2.0 * scale,
            ],
        }
    }

    fn axes(&self) -> [Point; 2] {
        [self.along, [-self.along[1], self.along[0]]]
    }

    /// The furthest it reaches along the unit vector `axis` from its middle.
    fn reach(&self, axis: Point) -> f64 {
        let [a, b] = self.axes();
        self.half[0] * dot(a, axis).abs() + self.half[1] * dot(b, axis).abs()
    }

    fn touches(&self, other: &Self, apart: f64) -> bool {
        let between = less(other.middle, self.middle);
        self.axes()
            .into_iter()
            .chain(other.axes())
            .all(|axis| dot(between, axis).abs() < self.reach(axis) + other.reach(axis) + apart)
    }

    /// The part of the segment from `p` to `q`, `half` either side of it, that
    /// comes within `apart`: units from `p`.
    fn within(&self, p: Point, q: Point, half: f64, apart: f64) -> Option<[f64; 2]> {
        let length = distance(p, q);
        if length <= 0.0 {
            return None;
        }
        let d = [(q[0] - p[0]) / length, (q[1] - p[1]) / length];
        let normal = [-d[1], d[0]];
        let from = less(p, self.middle);
        let (mut t0, mut t1) = (0.0_f64, length);
        for (axis, reach) in self.axes().into_iter().zip(self.half) {
            let reach = reach + half * dot(normal, axis).abs() + apart;
            let (at, speed) = (dot(from, axis), dot(d, axis));
            if speed.abs() < 1e-9 {
                if at.abs() >= reach {
                    return None;
                }
                continue;
            }
            let (a, b) = ((-reach - at) / speed, (reach - at) / speed);
            t0 = t0.max(a.min(b));
            t1 = t1.min(a.max(b));
        }
        (t0 < t1).then_some([t0, t1])
    }
}

/// What is painted, by where.
#[derive(Default)]
pub(super) struct Taken {
    blocks: Vec<(u32, Kind, Block)>,
    cells: FxHashMap<(i64, i64), Vec<u32>>,
}

fn cells(low: Point, high: Point) -> impl Iterator<Item = (i64, i64)> {
    let cell = |v: f64| (v / CELL).floor() as i64;
    let (x, y) = (cell(low[0])..=cell(high[0]), cell(low[1])..=cell(high[1]));
    x.flat_map(move |x| y.clone().map(move |y| (x, y)))
}

/// Whether the surfaces of two ways are one at `p`.
fn one_surface(ways: &[Way], a: u32, b: u32, p: Point) -> bool {
    let (a, b) = (&ways[a as usize], &ways[b as usize]);
    a.profile.level == b.profile.level
        || a.nodes
            .iter()
            .zip(&a.points)
            .any(|(node, q)| distance(*q, p) < JOINED * a.scale && b.nodes.contains(node))
}

impl Taken {
    fn add(&mut self, way: u32, kind: Kind, block: Block) {
        let reach = block.half[0].hypot(block.half[1]);
        let m = block.middle;
        for cell in cells([m[0] - reach, m[1] - reach], [m[0] + reach, m[1] + reach]) {
            let index = self.blocks.len() as u32;
            self.cells.entry(cell).or_default().push(index);
        }
        self.blocks.push((way, kind, block));
    }

    /// The blocks about a box that lie on the surface of `way`.
    fn near<'a>(
        &'a self,
        ways: &'a [Way],
        way: u32,
        low: Point,
        high: Point,
    ) -> impl Iterator<Item = (Kind, &'a Block)> {
        let mut found: Vec<u32> = cells(low, high)
            .filter_map(|cell| self.cells.get(&cell))
            .flatten()
            .copied()
            .collect();
        found.sort_unstable();
        found.dedup();
        found.into_iter().filter_map(move |index| {
            let (other, kind, block) = &self.blocks[index as usize];
            one_surface(ways, way, *other, block.middle).then_some((*kind, block))
        })
    }

    /// The stretches of a line of `way`, `width` metres across, that something
    /// painted of those kinds takes: units along it, in order.
    fn stretches(
        &self,
        ways: &[Way],
        way: u32,
        points: &[Point],
        width: f64,
        kinds: impl Fn(Kind) -> bool,
    ) -> Vec<[f64; 2]> {
        let scale = ways[way as usize].scale;
        let half = width / 2.0 * scale;
        let mut out = Vec::new();
        let mut at = 0.0;
        for p in points.windows(2) {
            let bound = |of: fn(f64, f64) -> f64, by: f64| {
                [of(p[0][0], p[1][0]) + by, of(p[0][1], p[1][1]) + by]
            };
            let reach = half + APART * scale;
            // most of the world has no paint near
            let near = self.near(ways, way, bound(f64::min, -reach), bound(f64::max, reach));
            for (kind, block) in near.filter(|b| kinds(b.0)) {
                let apart = match kind {
                    Kind::Gore => 0.0,
                    Kind::Arrow | Kind::Merge | Kind::Centre | Kind::Guide => BESIDE,
                    Kind::Bicycle | Kind::Sharrow | Kind::BusText => BESIDE,
                    _ => APART,
                };
                if let Some(t) = block.within(p[0], p[1], half, apart * scale) {
                    out.push([at + t[0], at + t[1]]);
                }
            }
            at += distance(p[0], p[1]);
        }
        joined(out)
    }

    /// What is left of a lanes' line of `way`, the stretches `least` metres long:
    /// world units along it.
    pub(super) fn kept(
        &self,
        ways: &[Way],
        way: u32,
        points: &[Point],
        least: f64,
    ) -> Vec<[f64; 2]> {
        let taken = self.stretches(ways, way, points, LINE, |_| true);
        let length: f64 = points.windows(2).map(|p| distance(p[0], p[1])).sum();
        left(length, &taken)
            .into_iter()
            .filter(|s| s[1] - s[0] >= least * ways[way as usize].scale)
            .collect()
    }

    /// Those stretches of it.
    pub(super) fn cut(
        &self,
        ways: &[Way],
        way: u32,
        points: &[Point],
        least: f64,
    ) -> Vec<Vec<Point>> {
        let length: f64 = points.windows(2).map(|p| distance(p[0], p[1])).sum();
        self.kept(ways, way, points, least)
            .into_iter()
            .filter_map(|s| geometry::trim(points, [s[0], length - s[1]]))
            .collect()
    }
}

/// `list` with nothing on what goes before it, and what it takes. The order is
/// by rank, then the more important road, then as they are, so it is the same
/// every time.
pub(super) fn settle(ways: &[Way], list: Vec<Detail>) -> (Vec<Detail>, Taken) {
    let mut order: Vec<usize> = (0..list.len()).collect();
    order.sort_by_key(|&i| {
        let d = &list[i];
        (d.kind.rank(), ways[d.way as usize].profile.class, d.way, i)
    });
    let mut taken = Taken::default();
    let mut list: Vec<Option<Detail>> = list.into_iter().map(Some).collect();
    for i in order {
        let Some(mut detail) = list[i].take() else {
            continue;
        };
        let scale = ways[detail.way as usize].scale;
        let reach = detail.block.half[0].hypot(detail.block.half[1]) + APART * scale;
        let m = detail.block.middle;
        let on: Vec<Block> = taken
            .near(
                ways,
                detail.way,
                [m[0] - reach, m[1] - reach],
                [m[0] + reach, m[1] + reach],
            )
            .filter(|(kind, _)| !(detail.kind.in_a_row() && *kind == detail.kind))
            .map(|(_, block)| *block)
            .filter(|b| b.touches(&detail.block, APART * scale))
            .collect();
        let whole = matches!(
            detail.kind,
            Kind::Arrow | Kind::Disc | Kind::Bicycle | Kind::Sharrow | Kind::BusText | Kind::Hump
        ) || detail.kind.in_a_row();
        if !on.is_empty() && (whole || !trim(&mut detail, &on, scale)) {
            continue;
        }
        taken.add(detail.way, detail.kind, detail.block);
        list[i] = Some(detail);
    }
    (list.into_iter().flatten().collect(), taken)
}

/// Cuts a crossing or a line where it would lie on `blocks`; false if too
/// little is left.
pub(super) fn trim(detail: &mut Detail, blocks: &[Block], scale: f64) -> bool {
    let half = detail.kind.width() / 2.0 * scale;
    let across = detail.block.axes()[1];
    let mut whole = 0.0;
    let mut kept: Vec<Vec<Point>> = Vec::new();
    let (mut sum, mut span) = (0.0, [f64::MAX, f64::MIN]);
    for stroke in &detail.strokes {
        let (p, q) = (stroke[0], stroke[stroke.len() - 1]);
        let length = distance(p, q);
        whole += length;
        let taken = blocks
            .iter()
            .filter_map(|b| b.within(p, q, half, APART * scale))
            .collect();
        for mut part in left(length, &joined(taken)) {
            if let Some((on, off)) = detail.kind.dash() {
                // whole bars, where they were
                let every = (on + off) * scale;
                part[0] = ((part[0] / every) - 1e-6).ceil() * every;
                part[1] = ((part[1] - on * scale) / every + 1e-6).floor() * every + on * scale;
            }
            if part[1] - part[0] < LEAST * scale - 1e-6 {
                continue;
            }
            sum += part[1] - part[0];
            let at = |t: f64| {
                let t = t / length;
                [p[0] + (q[0] - p[0]) * t, p[1] + (q[1] - p[1]) * t]
            };
            let ends = part.map(at);
            for end in ends {
                let x = dot(less(end, detail.block.middle), across);
                span = [span[0].min(x), span[1].max(x)];
            }
            kept.push(ends.to_vec());
        }
    }
    if kept.is_empty() || sum < LEAST_SHARE * whole {
        return false;
    }
    detail.strokes = kept;
    let x = (span[0] + span[1]) / 2.0;
    let m = detail.block.middle;
    detail.block.middle = [m[0] + across[0] * x, m[1] + across[1] * x];
    detail.block.half[1] = (span[1] - span[0]) / 2.0;
    true
}

/// The stretches of each group's pieces, metres along them, that hold no
/// lanes' lines: those of the runs they are, and where something is painted.
pub(super) fn piece_gaps(
    ways: &[Way],
    network: &Network,
    details: &Details,
    groups: &[Group],
) -> Vec<Vec<Vec<[f64; 2]>>> {
    // by its ends, each segment of the pieces: where it is, metres along
    // its piece, and whether it runs back along it
    type Segment = (usize, usize, f64, bool);
    let bits = |p: Point| [p[0].to_bits(), p[1].to_bits()];
    let mut segments: FxHashMap<([u64; 2], [u64; 2]), Vec<Segment>> = FxHashMap::default();
    let mut out: Vec<Vec<Vec<[f64; 2]>>> = Vec::with_capacity(groups.len());
    for (g, group) in groups.iter().enumerate() {
        for (i, piece) in group.pieces.iter().enumerate() {
            let way = &ways[piece.way as usize];
            if way.profile.lines.is_empty() || way.profile.class.is_rail() {
                continue;
            }
            let mut at = 0.0;
            for p in piece.points.windows(2) {
                let (a, b) = (bits(p[0]), bits(p[1]));
                let to = at + distance(p[0], p[1]) / way.scale;
                segments.entry((a, b)).or_default().push((g, i, at, false));
                segments.entry((b, a)).or_default().push((g, i, to, true));
                at = to;
            }
        }
        out.push(vec![Vec::new(); group.pieces.len()]);
    }
    for (index, run) in network.runs.iter().enumerate() {
        let way = &ways[run.way as usize];
        let points = &way.points[run.points.clone()];
        if way.profile.lanes.is_none() {
            continue;
        }
        let mut none: Option<Vec<[f64; 2]>> = None;
        let mut at = 0.0;
        for p in points.windows(2) {
            let from = at;
            at += distance(p[0], p[1]) / way.scale;
            let Some(found) = segments.get(&(bits(p[0]), bits(p[1]))) else {
                continue;
            };
            let none = none.get_or_insert_with(|| {
                let length = points.windows(2).map(|p| distance(p[0], p[1])).sum::<f64>();
                let mut run = run.clone();
                if !run.inner {
                    run.cut = run.cut.map(|c| if c > JUNCTION { 0.0 } else { c });
                }
                let drawn: Vec<[f64; 2]> = spans(&run, &details.gaps[index], way)
                    .into_iter()
                    .map(|s| [s[0], length - s[1]])
                    .collect();
                left(length, &drawn)
                    .into_iter()
                    .map(|s| s.map(|v| v / way.scale))
                    .collect()
            });
            for &(g, i, start, back) in found {
                let piece = &groups[g].pieces[i];
                let own = if piece.approach {
                    way.profile.level == Level::Ground
                } else {
                    piece.way == run.way
                };
                for gap in none.iter().filter(|_| own) {
                    let (a, b) = (gap[0].max(from) - from, gap[1].min(at) - from);
                    if b > a {
                        out[g][i].push(if back {
                            [start - b, start - a]
                        } else {
                            [start + a, start + b]
                        });
                    }
                }
            }
        }
    }
    for (group, gaps) in groups.iter().zip(&mut out) {
        for (piece, gaps) in group.pieces.iter().zip(gaps) {
            let way = &ways[piece.way as usize];
            for &(at, _) in &way.profile.lines {
                let line = geometry::offset(&piece.points, at * way.scale);
                // a gap is in all its lines, and those that go by a merge's go on
                let whole = |kind: Kind| !kind.in_a_row();
                let found = (details.taken).stretches(ways, piece.way, &line, LINE, whole);
                for s in found {
                    let ends = s.map(|s| station(&piece.points, point_at(&line, s)) / way.scale);
                    gaps.push([ends[0].min(ends[1]), ends[0].max(ends[1])]);
                }
            }
            // a stretch between two that is too short to draw is one with them
            for gap in gaps.iter_mut() {
                gap[1] += super::MIN_MARKING;
            }
            *gaps = joined(std::mem::take(gaps));
            for gap in gaps.iter_mut() {
                gap[1] -= super::MIN_MARKING;
            }
            gaps.retain(|g| g[1] - g[0] > 0.05);
        }
    }
    out
}

/// The point `at` units along a line.
fn point_at(points: &[Point], at: f64) -> Point {
    let mut left = at;
    for p in points.windows(2) {
        let length = distance(p[0], p[1]);
        if left <= length && length > 0.0 {
            let t = left / length;
            return [
                p[0][0] + (p[1][0] - p[0][0]) * t,
                p[0][1] + (p[1][1] - p[0][1]) * t,
            ];
        }
        left -= length;
    }
    points[points.len() - 1]
}

/// Units along a line of the point of it nearest to `p`.
fn station(points: &[Point], p: Point) -> f64 {
    let (mut best, mut at) = ((f64::MAX, 0.0), 0.0);
    for s in points.windows(2) {
        let length = distance(s[0], s[1]);
        let d = less(s[1], s[0]);
        let t = (dot(less(p, s[0]), d) / (length * length).max(f64::MIN_POSITIVE)).clamp(0.0, 1.0);
        let off = distance(p, [s[0][0] + d[0] * t, s[0][1] + d[1] * t]);
        if off < best.0 {
            best = (off, at + length * t);
        }
        at += length;
    }
    best.1
}

#[cfg(test)]
mod tests {
    use super::super::details::{Marking, Sign, build};
    use super::super::profile::profile;
    use super::super::{network, structures};
    use super::*;
    use rustc_hash::FxHashSet;

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

    fn details(ways: &[Way], signs: &[(i64, Sign)]) -> (Network, Details) {
        let network = network::build(ways);
        let details = build(
            ways,
            &network,
            &signs.iter().copied().collect(),
            &FxHashMap::default(),
            &FxHashSet::default(),
            &FxHashSet::default(),
        );
        (network, details)
    }

    const ROAD: &[(&str, &str)] = &[("highway", "primary"), ("lanes", "2")];
    const BRIDGE: &[(&str, &str)] = &[
        ("highway", "primary"),
        ("lanes", "2"),
        ("bridge", "yes"),
        ("layer", "1"),
    ];
    const ARROWS: &[(&str, &str)] = &[
        ("highway", "secondary"),
        ("oneway", "yes"),
        ("lanes", "2"),
        ("turn:lanes", "left|through"),
    ];
    const ZEBRA: Sign = Sign::Crossing(Some(Marking::Marked(Kind::Zebra, 2)));

    #[test]
    fn a_block_knows_what_comes_near_it() {
        // 10 m across a road that goes east, 4 along it
        let b = Block::new([0.0, 0.0], [1.0, 0.0], 1.0, [-5.0, 5.0], [-2.0, 2.0]);
        // a line along the road, 0.15 wide
        let t = b.within([-10.0, 4.0], [10.0, 4.0], 0.075, 0.3).unwrap();
        assert!((t[0] - 7.7).abs() < 1e-9 && (t[1] - 12.3).abs() < 1e-9);
        // beside it
        assert!(b.within([-10.0, 5.4], [10.0, 5.4], 0.075, 0.3).is_none());
        assert!(b.within([-10.0, 5.37], [10.0, 5.37], 0.075, 0.3).is_some());
        let turned = |x: f64| {
            let d = 0.5_f64.sqrt();
            Block::new([x, 0.0], [d, d], 1.0, [-1.0, 1.0], [-1.0, 1.0])
        };
        assert!(b.touches(&turned(3.3), 0.0) && !b.touches(&turned(3.5), 0.0));
        assert!(b.touches(&turned(3.5), 0.3));
    }

    #[test]
    fn zebras_side_by_side_share_the_road_in_whole_bars() {
        // two carriageways whose surfaces overlap by 2 m
        let ways = [
            way(ROAD, &[(1, -50.0, 0.0), (2, 0.0, 0.0), (3, 50.0, 0.0)]),
            way(ROAD, &[(4, -50.0, 5.0), (5, 0.0, 5.0), (6, 50.0, 5.0)]),
        ];
        let (_, d) = details(&ways, &[(2, ZEBRA), (5, ZEBRA)]);
        assert_eq!(d.list.len(), 2);
        assert_eq!(d.list[0].strokes, [vec![[0.0, -3.25], [0.0, 3.25]]]);
        // bars were at 1.75, 2.75, 3.75 and on
        assert_eq!(d.list[1].strokes, [vec![[0.0, 3.75], [0.0, 8.25]]]);
        // two in one place are one
        let twice = [
            way(ROAD, &[(1, -50.0, 0.0), (2, 0.0, 0.0), (3, 50.0, 0.0)]),
            way(ROAD, &[(4, -50.0, 1.0), (5, 0.0, 1.0), (6, 50.0, 1.0)]),
        ];
        assert_eq!(details(&twice, &[(2, ZEBRA), (5, ZEBRA)]).1.list.len(), 1);
    }

    #[test]
    fn arrows_give_way_to_another_ways_crossing_of_their_level() {
        let cross = [(6, 0.0, -100.0), (4, 0.0, 0.0), (7, 0.0, 100.0)];
        let beside = [(8, -90.0, 3.0), (9, -15.0, 3.0), (10, -8.0, 3.0)];
        let arrows = |tags: &[(&str, &'static str)]| {
            let ways = [
                way(ARROWS, &[(1, -100.0, 0.0), (4, 0.0, 0.0)]),
                way(ROAD, &cross),
                way(tags, &beside),
            ];
            let (_, d) = details(&ways, &[(9, ZEBRA)]);
            assert_eq!(d.list.iter().filter(|d| d.kind == Kind::Zebra).count(), 1);
            d.list.iter().filter(|d| d.kind == Kind::Arrow).count()
        };
        // the right lane's nearest, and not the left lane's clear of it
        assert_eq!((arrows(ROAD), arrows(BRIDGE)), (5, 6));
    }

    #[test]
    fn a_lanes_line_stops_at_what_is_painted_on_it() {
        let ways = [
            way(ROAD, &[(1, -50.0, 0.0), (2, 50.0, 0.0)]),
            way(ROAD, &[(4, -50.0, 5.0), (5, 0.0, 5.0), (6, 50.0, 5.0)]),
        ];
        let (_, d) = details(&ways, &[(5, ZEBRA)]);
        let line = [[-50.0, 2.5], [50.0, 2.5]];
        let parts = d.taken.cut(&ways, 0, &line, 2.0);
        let ends: Vec<f64> = parts.iter().flatten().map(|p| p[0]).collect();
        let want = [-50.0, -2.3, 2.3, 50.0];
        assert_eq!(ends.len(), want.len());
        assert!(ends.iter().zip(want).all(|(a, b)| (a - b).abs() < 1e-9));
        // its own middle line is clear of it, and is whole
        let own = [[-50.0, 0.0], [50.0, 0.0]];
        assert_eq!(d.taken.cut(&ways, 0, &own, 2.0), [own.to_vec()]);
        // the parts too short to draw are none
        let short = [[-3.0, 2.5], [50.0, 2.5]];
        assert_eq!(d.taken.cut(&ways, 0, &short, 2.0).len(), 1);
    }

    #[test]
    fn a_bridge_and_the_road_up_to_it_hold_no_lines_under_paint() {
        // the road is drawn towards the bridge, which is drawn towards it
        let ways = [
            way(ROAD, &[(1, -80.0, 0.0), (2, -30.0, 0.0), (3, 0.0, 0.0)]),
            way(BRIDGE, &[(5, 100.0, 0.0), (4, 60.0, 0.0), (3, 0.0, 0.0)]),
        ];
        let (network, d) = details(
            &ways,
            &[
                (2, ZEBRA),
                (
                    4,
                    Sign::Line(Kind::Stop, super::super::details::Facing::Along),
                ),
            ],
        );
        let groups = structures::build(&ways, &[]);
        let gaps = piece_gaps(&ways, &network, &d, &groups);
        assert_eq!(groups[0].pieces.len(), 2);
        let close = |got: &[[f64; 2]], want: &[[f64; 2]]| {
            got.len() == want.len()
                && got
                    .iter()
                    .flatten()
                    .zip(want.iter().flatten())
                    .all(|(a, b)| (a - b).abs() < 1e-6)
        };
        // a stop line half a metre wide, 40 m from the bridge's start, with
        // 0.3 either side of it
        assert!(close(&gaps[0][0], &[[39.45, 40.55]]), "{gaps:?}");
        assert!(groups[0].pieces[1].approach);
        assert!(close(&gaps[0][1], &[[47.4, 52.6]]), "{gaps:?}");
    }
}
