//! Bridges as groups to be given heights together: their pieces, where they
//! meet the ground, and what each passes over. The heights themselves need
//! terrain and are left to whoever draws them.

use super::network::{Point, Way, distance};
use super::profile::{Class, Level};
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::BTreeMap;

/// What a bridge passes over that is not a road of the archive.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Kind {
    Path,
    Rail,
    /// A river or canal.
    River,
    /// A stream or ditch.
    Stream,
}

#[derive(Debug, Clone)]
pub(super) struct Obstacle {
    pub nodes: Vec<i64>,
    pub points: Vec<Point>,
    pub kind: Kind,
}

/// A stretch of a bridge between two nodes it shares with others, or its ends;
/// or of the road that leads up to one.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Piece {
    pub way: u32,
    pub points: Vec<Point>,
    pub nodes: [i64; 2],
    /// Whether it meets the ground at its start and its end.
    pub anchor: [bool; 2],
    /// A road on the ground that leads up to a bridge, and may be raised to it.
    pub approach: bool,
}

/// Where a piece passes over something.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Crossing {
    /// The piece over, of its group, and metres along it.
    pub over: u32,
    pub at: f64,
    pub point: Point,
    /// What is under: `road`, `path`, `rail`, `water` or `bridge`.
    pub kind: &'static str,
    /// The piece under and metres along it, if it is a bridge of the group.
    pub under: Option<(u32, f64)>,
    /// Metres the one over must clear the one under by.
    pub clearance: f64,
    /// Metres from the crossing along the one over that the one under takes.
    pub half: f64,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Group {
    pub pieces: Vec<Piece>,
    pub crossings: Vec<Crossing>,
}

/// Metres of clearance and of half width, over a road and over obstacles.
const OVER_ROAD: f64 = 5.0;
fn over(kind: Kind) -> (&'static str, f64, f64) {
    match kind {
        Kind::Path => ("path", 3.0, 1.5),
        Kind::Rail => ("rail", 6.5, 2.5),
        Kind::River => ("water", 2.0, 6.0),
        Kind::Stream => ("water", 0.7, 1.0),
    }
}

/// Metres a road is followed from a bridge's end, for raising it, a railway,
/// which is less steep, a path, which is steeper, and steps.
const APPROACH: f64 = 160.0;
const RAIL_APPROACH: f64 = 320.0;
const PATH_APPROACH: f64 = 80.0;
const STEPS_APPROACH: f64 = 40.0;
/// Metres of path between two flights of steps that lead up as one.
const LANDING: f64 = 8.0;
/// Metres round a node two ways share within which they do not cross.
const SHARED: f64 = 3.0;
/// World units of a cell segments are looked up by: about 100 m at 50 degrees.
pub(super) const CELL: f64 = 1024.0;

fn layer(level: Level) -> Option<i8> {
    match level {
        Level::Bridge(layer) => Some(layer),
        _ => None,
    }
}

/// The cell of a point.
pub(super) fn cell(p: Point) -> (i64, i64) {
    ((p[0] / CELL).floor() as i64, (p[1] / CELL).floor() as i64)
}

/// The cells of `points`' segments.
pub(super) fn cells(points: &[Point]) -> impl Iterator<Item = (usize, (i64, i64))> + '_ {
    let cell = |v: f64| (v / CELL).floor() as i64;
    points.windows(2).enumerate().flat_map(move |(i, p)| {
        let (x0, x1) = (cell(p[0][0].min(p[1][0])), cell(p[0][0].max(p[1][0])));
        let (y0, y1) = (cell(p[0][1].min(p[1][1])), cell(p[0][1].max(p[1][1])));
        (x0..=x1).flat_map(move |x| (y0..=y1).map(move |y| (i, (x, y))))
    })
}

/// The cells any bridge lies in: an obstacle elsewhere is under none.
pub(super) fn bridge_cells(ways: &[Way]) -> FxHashSet<(i64, i64)> {
    ways.iter()
        .filter(|w| layer(w.profile.level).is_some())
        .flat_map(|w| cells(&w.points).map(|c| c.1))
        .collect()
}

/// The groups, in the order of their first pieces' ways.
pub(super) fn build(ways: &[Way], obstacles: &[Obstacle]) -> Vec<Group> {
    let mut pieces = pieces(ways);
    let mut groups = Groups::new(pieces.len());
    let mut ends: BTreeMap<i64, Vec<(usize, usize)>> = BTreeMap::new();
    for (i, piece) in pieces.iter().enumerate() {
        for end in 0..2 {
            ends.entry(piece.nodes[end]).or_default().push((i, end));
        }
    }
    let ground: FxHashSet<i64> = ways
        .iter()
        .filter(|w| layer(w.profile.level).is_none())
        .filter(|w| w.profile.class.is_motor() || w.profile.class.is_rail())
        .flat_map(|w| w.nodes.iter().copied())
        .collect();
    for (node, at) in &ends {
        let anchored = at.len() == 1 || ground.contains(node);
        for &(i, end) in at {
            pieces[i].anchor[end] = anchored;
            // on the ground too: side by side there, they must know of each other
            groups.join(at[0].0, i);
        }
    }
    for (a, b) in side_by_side(ways, &pieces) {
        groups.join(a, b);
    }
    let crossings = crossings(ways, obstacles, &pieces);

    let roads = approaches(ways, &pieces, APPROACH, |w| w.profile.class.is_motor(), &[]);
    // a track is not raised off the roads that cross it
    let crossed: Vec<i64> = ways
        .iter()
        .filter(|w| w.profile.class.is_motor() && w.profile.level == Level::Ground)
        .flat_map(|w| w.nodes.iter().copied())
        .collect();
    let rails = approaches(
        ways,
        &pieces,
        RAIL_APPROACH,
        |w| w.profile.class.is_rail(),
        &crossed,
    );
    // nor is a path, off the roads and the tracks; steps lead up where no path does
    let crossed: Vec<i64> = ways
        .iter()
        .filter(|w| w.profile.level == Level::Ground)
        .filter(|w| w.profile.class.is_motor() || w.profile.class.is_rail())
        .flat_map(|w| w.nodes.iter().copied())
        .collect();
    let footbridges: Vec<Piece> = pieces
        .iter()
        .filter(|p| ways[p.way as usize].profile.class.is_path())
        .cloned()
        .collect();
    let walked = |w: &Way| !w.profile.class.is_motor() && !w.profile.class.is_rail();
    let steps = |w: &Way| w.profile.class == Class::Steps;
    let paths = approaches(
        ways,
        &footbridges,
        PATH_APPROACH,
        |w| walked(w) && !steps(w),
        &crossed,
    );
    let flight = |w: &Way| {
        let length = || {
            w.points
                .windows(2)
                .map(|p| distance(p[0], p[1]))
                .sum::<f64>()
        };
        steps(w) || w.profile.level != Level::Ground || length() <= LANDING * w.scale
    };
    let mut stops = crossed;
    let on = ways.iter().filter(|w| walked(w) && !flight(w));
    stops.extend(on.flat_map(|w| w.nodes.iter().copied()));
    let mut stairs = approaches(
        ways,
        &footbridges,
        STEPS_APPROACH,
        |w| walked(w) && flight(w),
        &stops,
    );
    stairs.retain(|p| steps(&ways[p.way as usize]));
    for approach in roads.into_iter().chain(rails).chain(paths).chain(stairs) {
        let index = pieces.len();
        groups.add();
        for end in 0..2 {
            if let (false, Some(at)) = (approach.anchor[end], ends.get(&approach.nodes[end])) {
                for &(i, _) in at {
                    groups.join(i, index);
                }
            }
        }
        pieces.push(approach);
    }
    for c in &crossings {
        if let Some((under, _)) = c.under {
            groups.join(c.over as usize, under as usize);
        }
    }
    // by the group's first piece, pieces and crossings in their order
    let mut place: Vec<(usize, u32)> = Vec::with_capacity(pieces.len());
    let mut index: FxHashMap<usize, usize> = FxHashMap::default();
    let mut out: Vec<Group> = Vec::new();
    for (i, piece) in pieces.into_iter().enumerate() {
        let root = groups.find(i);
        let group = *index.entry(root).or_insert_with(|| {
            out.push(Group::default());
            out.len() - 1
        });
        place.push((group, out[group].pieces.len() as u32));
        out[group].pieces.push(piece);
    }
    for mut c in crossings {
        let (group, over) = place[c.over as usize];
        c.over = over;
        c.under = c.under.map(|(p, at)| (place[p as usize].1, at));
        out[group].crossings.push(c);
    }
    out
}

/// Where a tunnel comes out onto a road on the ground: its way, the point, and
/// the points into it from there.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Portal {
    pub way: u32,
    pub points: Vec<Point>,
}

/// Metres of a tunnel from its portal that tell which way it goes in.
const INTO: f64 = 25.0;

/// The portals of motor roads' tunnels, in the ways' order.
pub(super) fn portals(ways: &[Way]) -> Vec<Portal> {
    let motor = || ways.iter().filter(|w| w.profile.class.is_motor());
    let ends = |level: fn(Level) -> bool| -> FxHashSet<i64> {
        motor()
            .filter(|w| level(w.profile.level))
            .flat_map(|w| [w.nodes[0], w.nodes[w.nodes.len() - 1]])
            .collect()
    };
    let ground = ends(|l| l == Level::Ground);
    let mut tunnels: FxHashMap<i64, u32> = FxHashMap::default();
    for way in motor().filter(|w| matches!(w.profile.level, Level::Tunnel(_))) {
        for node in [way.nodes[0], way.nodes[way.nodes.len() - 1]] {
            *tunnels.entry(node).or_default() += 1;
        }
    }
    let mut out = Vec::new();
    for (index, way) in ways.iter().enumerate() {
        if !way.profile.class.is_motor() || !matches!(way.profile.level, Level::Tunnel(_)) {
            continue;
        }
        for end in 0..2 {
            let node = if end == 0 {
                way.nodes[0]
            } else {
                way.nodes[way.nodes.len() - 1]
            };
            if !ground.contains(&node) || tunnels[&node] != 1 {
                continue;
            }
            let mut inward: Vec<Point> = way.points.clone();
            if end == 1 {
                inward.reverse();
            }
            let mut points = vec![inward[0]];
            let mut length = 0.0;
            for p in inward.windows(2) {
                length += distance(p[0], p[1]) / way.scale;
                points.push(p[1]);
                if length >= INTO {
                    break;
                }
            }
            // in the way's own direction, for what is to its left and right
            if end == 1 {
                points.reverse();
            }
            out.push(Portal {
                way: index as u32,
                points,
            });
        }
    }
    out
}

/// The bridges, split where others join them.
fn pieces(ways: &[Way]) -> Vec<Piece> {
    let bridges = || {
        ways.iter()
            .enumerate()
            .filter(|(_, w)| layer(w.profile.level).is_some())
    };
    let mut uses: FxHashMap<i64, u32> = FxHashMap::default();
    for (_, way) in bridges() {
        for &node in &way.nodes {
            *uses.entry(node).or_default() += 1;
        }
    }
    let mut pieces = Vec::new();
    for (index, way) in bridges() {
        let last = way.nodes.len() - 1;
        let mut from = 0;
        for to in 1..=last {
            if to < last && uses[&way.nodes[to]] < 2 {
                continue;
            }
            pieces.push(Piece {
                way: index as u32,
                points: way.points[from..=to].to_vec(),
                nodes: [way.nodes[from], way.nodes[to]],
                anchor: [false; 2],
                approach: false,
            });
            from = to;
        }
    }
    pieces
}

/// Metres between the edges of a track's bridge and of another's beside it
/// that are one bridge, and between places at which it is looked for.
const ONE_BRIDGE: f64 = 0.5;
const SIDE_STEP: f64 = 20.0;

/// The tracks' bridges and what of their layer runs along them, edge to edge or
/// in one place: a line's two tracks, or a tram in a road.
fn side_by_side(ways: &[Way], pieces: &[Piece]) -> Vec<(usize, usize)> {
    let mut at: FxHashMap<(i64, i64), Vec<(usize, usize)>> = FxHashMap::default();
    for (i, piece) in pieces.iter().enumerate() {
        for (segment, cell) in cells(&piece.points) {
            at.entry(cell).or_default().push((i, segment));
        }
    }
    let mut out = Vec::new();
    for (i, piece) in pieces.iter().enumerate() {
        let way = &ways[piece.way as usize];
        if !way.profile.class.is_rail() {
            continue;
        }
        for p in piece.points.windows(2) {
            let length = distance(p[0], p[1]).max(f64::MIN_POSITIVE);
            let d = [(p[1][0] - p[0][0]) / length, (p[1][1] - p[0][1]) / length];
            let parts = (length / (SIDE_STEP * way.scale)).ceil().max(1.0) as u32;
            for k in 0..parts {
                let t = length * (f64::from(k) + 0.5) / f64::from(parts);
                let q = [p[0][0] + d[0] * t, p[0][1] + d[1] * t];
                let (x, y) = cell(q);
                let near = (-1..=1).flat_map(|i| (-1..=1).map(move |j| (x + i, y + j)));
                for &(j, segment) in near.filter_map(|c| at.get(&c)).flatten() {
                    let other = &ways[pieces[j].way as usize];
                    if i == j || other.profile.level != way.profile.level {
                        continue;
                    }
                    let (a, b) = (pieces[j].points[segment], pieces[j].points[segment + 1]);
                    let length = distance(a, b).max(f64::MIN_POSITIVE);
                    let e = [(b[0] - a[0]) / length, (b[1] - a[1]) / length];
                    let r = [q[0] - a[0], q[1] - a[1]];
                    let t = r[0] * e[0] + r[1] * e[1];
                    let right = (r[1] * e[0] - r[0] * e[1]) / way.scale - other.profile.centre;
                    let reach = (way.profile.width + other.profile.width) / 2.0 + ONE_BRIDGE;
                    if (0.0..=length).contains(&t)
                        && right.abs() < reach
                        && (d[0] * e[1] - d[1] * e[0]).abs() < ALONG
                    {
                        out.push((i, j));
                    }
                }
            }
        }
    }
    out
}

/// The roads on the ground that lead up to the bridges' ends, of the ways
/// `motor` accepts: from each one's, as far as the road goes on alone, to
/// another bridge or one of the nodes `stops`, `reach` metres at most.
fn approaches(
    ways: &[Way],
    pieces: &[Piece],
    reach: f64,
    motor: impl Fn(&Way) -> bool,
    stops: &[i64],
) -> Vec<Piece> {
    // where each node of a motor road that another shares lies on them
    let mut uses: FxHashMap<i64, u32> = FxHashMap::default();
    for way in ways.iter().filter(|w| motor(w)) {
        let last = way.nodes.len() - 1;
        for (i, &node) in way.nodes.iter().enumerate() {
            *uses.entry(node).or_default() += if i == 0 || i == last { 1 } else { 2 };
        }
    }
    for node in stops {
        if let Some(uses) = uses.get_mut(node) {
            *uses += 2;
        }
    }
    let mut at: FxHashMap<i64, Vec<(usize, usize)>> = FxHashMap::default();
    for (index, way) in ways.iter().enumerate().filter(|(_, w)| motor(w)) {
        for (i, &node) in way.nodes.iter().enumerate() {
            if uses[&node] > 1 && (i == 0 || i + 1 == way.nodes.len()) {
                at.entry(node).or_default().push((index, i));
            }
        }
    }
    let bridge_ends: FxHashSet<i64> = pieces
        .iter()
        .flat_map(|p| (0..2).filter(|&e| p.anchor[e]).map(|e| p.nodes[e]))
        .collect();
    // the one way that goes on from `node`, off `from`
    let onward = |node: i64, from: usize| -> Option<(usize, usize)> {
        if uses.get(&node) != Some(&2) {
            return None;
        }
        let arms = at.get(&node)?;
        let mut others = arms.iter().filter(|a| a.0 != from);
        let next = *others.next()?;
        (others.next().is_none() && arms.len() == 2).then_some(next)
    };
    let mut out: Vec<Piece> = Vec::new();
    let mut seen: FxHashSet<(i64, i64)> = FxHashSet::default();
    for piece in pieces {
        for end in 0..2 {
            let way = &ways[piece.way as usize];
            if !piece.anchor[end] || !motor(way) {
                continue;
            }
            let start = piece.nodes[end];
            let Some((mut index, mut k)) = onward(start, piece.way as usize) else {
                continue;
            };
            let (first, onwards) = (index, k == 0);
            let (mut points, mut node, mut length) = (Vec::new(), start, 0.0);
            let mut to_bridge = false;
            'road: while ways[index].profile.level == Level::Ground {
                let road = &ways[index];
                let last = road.nodes.len() - 1;
                let forward = k == 0;
                if points.is_empty() {
                    points.push(road.points[k]);
                }
                loop {
                    if (forward && k == last) || (!forward && k == 0) {
                        break;
                    }
                    k = if forward { k + 1 } else { k - 1 };
                    length += distance(points[points.len() - 1], road.points[k]) / road.scale;
                    points.push(road.points[k]);
                    node = road.nodes[k];
                    if length > reach || uses[&node] > 2 || node == start {
                        break 'road;
                    }
                }
                if bridge_ends.contains(&node) {
                    to_bridge = true;
                    break;
                }
                let Some(next) = onward(node, index) else {
                    break;
                };
                (index, k) = next;
            }
            let ends = (start.min(node), start.max(node));
            if points.len() < 2 || node == start || (to_bridge && !seen.insert(ends)) {
                continue;
            }
            // along its way, whichever way it was followed: its surface and lines
            // lie to the way's left and right
            let mut nodes = [start, node];
            let mut anchor = [false, !to_bridge];
            if !onwards {
                points.reverse();
                nodes.reverse();
                anchor.reverse();
            }
            out.push(Piece {
                way: first as u32,
                points,
                nodes,
                anchor,
                approach: true,
            });
        }
    }
    out
}

/// By cell, segments: group, piece and place in it.
type Segments = FxHashMap<(i64, i64), Vec<(u32, u32, u32)>>;

/// What lies along the groups' pieces.
pub(super) struct Index<'a> {
    groups: &'a [Group],
    /// The tile each group is in.
    tiles: Vec<(u32, u32)>,
    at: Segments,
}

/// A piece something lies on.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Found {
    pub group: u32,
    pub piece: u32,
    pub approach: bool,
    /// Whether it is a railway's, whose edge has no parapet.
    pub rail: bool,
    /// Metres within the surface's nearer edge; below nothing, beyond it.
    pub inside: f64,
    pub tile: (u32, u32),
}

/// A bridge a point lies on: its way, the direction it goes there, and metres
/// right of its line.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Deck {
    pub found: Found,
    pub way: u32,
    pub along: Point,
    pub right: f64,
}

/// The sine of the angle to a piece beyond which a line crosses it and does
/// not run along it.
const ALONG: f64 = 0.4;

impl<'a> Index<'a> {
    pub(super) fn new(
        groups: &'a [Group],
        ways: &[Way],
        tile: impl Fn(Point) -> (u32, u32),
    ) -> Self {
        let mut at = Segments::default();
        for (g, group) in groups.iter().enumerate() {
            for (i, piece) in group.pieces.iter().enumerate() {
                let way = &ways[piece.way as usize];
                let reach = (way.profile.width / 2.0 + way.profile.centre.abs() + 3.0) * way.scale;
                for (k, p) in piece.points.windows(2).enumerate() {
                    let cell = |v: f64| (v / CELL).floor() as i64;
                    let low = |i: usize| cell(p[0][i].min(p[1][i]) - reach);
                    let high = |i: usize| cell(p[0][i].max(p[1][i]) + reach);
                    for x in low(0)..=high(0) {
                        for y in low(1)..=high(1) {
                            let entry = (g as u32, i as u32, k as u32);
                            at.entry((x, y)).or_default().push(entry);
                        }
                    }
                }
            }
        }
        let tiles = groups.iter().map(|g| tile(g.pieces[0].points[0])).collect();
        Self { groups, tiles, at }
    }

    /// The pieces' segments about `p` that `p` lies beside, with metres right of
    /// them.
    fn beside(&self, ways: &'a [Way], p: Point) -> impl Iterator<Item = (u32, u32, Point, f64)> {
        let cell = |v: f64| (v / CELL).floor() as i64;
        let near = self.at.get(&(cell(p[0]), cell(p[1])));
        near.into_iter().flatten().filter_map(move |&(g, i, k)| {
            let piece = &self.groups[g as usize].pieces[i as usize];
            let (a, b) = (piece.points[k as usize], piece.points[k as usize + 1]);
            let length = distance(a, b).max(f64::MIN_POSITIVE);
            let d = [(b[0] - a[0]) / length, (b[1] - a[1]) / length];
            let q = [p[0] - a[0], p[1] - a[1]];
            let t = q[0] * d[0] + q[1] * d[1];
            let right = (q[1] * d[0] - q[0] * d[1]) / ways[piece.way as usize].scale;
            (-1e-6..=length + 1e-6)
                .contains(&t)
                .then_some((g, i, d, right))
        })
    }

    /// The piece whose line the point of a way's line is on.
    pub(super) fn on(&self, ways: &'a [Way], way: u32, p: Point) -> Option<(u32, u32)> {
        let bridge = layer(ways[way as usize].profile.level).is_some();
        self.beside(ways, p)
            .filter(|&(g, i, _, right)| {
                let piece = &self.groups[g as usize].pieces[i as usize];
                let class = ways[piece.way as usize].profile.class;
                right.abs() < 0.2
                    && piece.approach != bridge
                    && (piece.approach || piece.way == way)
                    && !class.is_rail()
                    && !class.is_path()
            })
            .map(|(g, i, ..)| (g, i))
            .min()
    }

    /// The bridge, but a railway's, whose surface a point lies on or within `beyond`
    /// metres of its edge: the highest, and of those the one it is furthest within.
    pub(super) fn deck(&self, ways: &'a [Way], p: Point, beyond: f64) -> Option<Deck> {
        self.beside(ways, p)
            .filter_map(|(group, piece, along, right)| {
                let of = &self.groups[group as usize].pieces[piece as usize];
                let p = &ways[of.way as usize].profile;
                let inside = p.width / 2.0 - (right - p.centre).abs();
                let layer = layer(p.level).filter(|_| !of.approach && !p.class.is_rail())?;
                let found = Found {
                    group,
                    piece,
                    approach: false,
                    rail: false,
                    inside,
                    tile: self.tiles[group as usize],
                };
                let deck = Deck {
                    found,
                    way: of.way,
                    along,
                    right,
                };
                (inside >= -beyond).then_some((layer, deck))
            })
            .reduce(|a, b| {
                let more = (b.0, b.1.found.inside) > (a.0, a.1.found.inside);
                if more { b } else { a }
            })
            .map(|found| found.1)
    }

    /// The piece the line from `a` to `b` runs along, the one it is furthest
    /// within, on it or within `beyond` metres of its edge.
    pub(super) fn along(&self, ways: &'a [Way], a: Point, b: Point, beyond: f64) -> Option<Found> {
        let length = distance(a, b).max(f64::MIN_POSITIVE);
        let along = [(b[0] - a[0]) / length, (b[1] - a[1]) / length];
        let middle = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
        self.beside(ways, middle)
            .filter(|(_, _, d, _)| (along[0] * d[1] - along[1] * d[0]).abs() < ALONG)
            .filter_map(|(group, piece, _, right)| {
                let of = &self.groups[group as usize].pieces[piece as usize];
                let p = &ways[of.way as usize].profile;
                let inside = p.width / 2.0 - (right - p.centre).abs();
                // what lines a road stands on no path beside it
                let beside = of.approach && p.class.is_path();
                (inside >= -beyond && !beside).then_some(Found {
                    group,
                    piece,
                    approach: of.approach,
                    rail: p.class.is_rail(),
                    inside,
                    tile: self.tiles[group as usize],
                })
            })
            .reduce(|a, b| if b.inside > a.inside { b } else { a })
    }

    /// The piece of a road of this level that the line from `a` to `b` runs
    /// along and lies on, or within `beyond` metres of its edge.
    pub(super) fn carrying(
        &self,
        ways: &'a [Way],
        a: Point,
        b: Point,
        level: Level,
        beyond: f64,
    ) -> Option<(u32, u32)> {
        let length = distance(a, b).max(f64::MIN_POSITIVE);
        let along = [(b[0] - a[0]) / length, (b[1] - a[1]) / length];
        let middle = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
        self.beside(ways, middle)
            .filter(|(_, _, d, _)| (along[0] * d[1] - along[1] * d[0]).abs() < ALONG)
            .filter_map(|(group, piece, _, right)| {
                let of = &self.groups[group as usize].pieces[piece as usize];
                let p = &ways[of.way as usize].profile;
                let inside = p.width / 2.0 - (right - p.centre).abs();
                (p.class.is_motor() && p.level == level && inside >= -beyond)
                    .then_some((inside, (group, piece)))
            })
            .reduce(|a, b| if b.0 > a.0 { b } else { a })
            .map(|found| found.1)
    }
}

/// What a piece must clear a bridge of this class by.
fn over_bridge(class: Class) -> f64 {
    match class {
        Class::Rail => over(Kind::Rail).1,
        class if class.is_path() => over(Kind::Path).1,
        _ => OVER_ROAD,
    }
}

/// Where each piece passes over what is lower, pieces numbered over all groups.
fn crossings(ways: &[Way], obstacles: &[Obstacle], pieces: &[Piece]) -> Vec<Crossing> {
    let mut at: FxHashMap<(i64, i64), Vec<(u32, u32)>> = FxHashMap::default();
    for (i, piece) in pieces.iter().enumerate() {
        for (segment, cell) in cells(&piece.points) {
            at.entry(cell).or_default().push((i as u32, segment as u32));
        }
    }
    let mut found: Vec<Crossing> = Vec::new();
    let mut seen: FxHashSet<(u32, u32, usize, usize)> = FxHashSet::default();
    let mut look = |source: usize,
                    points: &[Point],
                    nodes: &dyn Fn(usize) -> i64,
                    own: &dyn Fn(u32) -> bool,
                    make: &dyn Fn(u32, f64) -> Option<Crossing>| {
        for (segment, cell) in cells(points) {
            let Some(near) = at.get(&cell) else { continue };
            for &(i, s) in near {
                if own(i) || !seen.insert((i, s, source, segment)) {
                    continue;
                }
                let piece = &pieces[i as usize];
                let (a, b) = (piece.points[s as usize], piece.points[s as usize + 1]);
                let (c, d) = (points[segment], points[segment + 1]);
                let Some((t, u, sine)) = meet(a, b, c, d) else {
                    continue;
                };
                let way = &ways[piece.way as usize];
                let p = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
                // where they share a node they meet, and do not cross
                let shared = [segment, segment + 1].iter().any(|&k| {
                    distance(points[k], p) < SHARED * way.scale && way.nodes.contains(&nodes(k))
                });
                if shared {
                    continue;
                }
                let along = |points: &[Point], k: usize, p: Point| {
                    (points[..=k]
                        .windows(2)
                        .map(|q| distance(q[0], q[1]))
                        .sum::<f64>()
                        + distance(points[k], p))
                        / way.scale
                };
                let under = along(
                    points,
                    segment,
                    [c[0] + (d[0] - c[0]) * u, c[1] + (d[1] - c[1]) * u],
                );
                if let Some(mut crossing) = make(i, under) {
                    crossing.at = along(&piece.points, s as usize, p);
                    crossing.point = p;
                    // at an angle it takes more of the bridge
                    crossing.half /= sine.max(0.2);
                    found.push(crossing);
                }
            }
        }
    };
    let crossing = |over: u32, kind, under, clearance, half| Crossing {
        over,
        at: 0.0,
        point: [0.0; 2],
        kind,
        under,
        clearance,
        half,
    };
    let mut source = 0;
    for way in ways.iter() {
        // tracks on the ground are obstacles
        if way.profile.level == Level::Ground && !way.profile.class.is_rail() {
            let (kind, clearance) = if way.profile.class.is_motor() {
                ("road", OVER_ROAD)
            } else {
                ("path", over(Kind::Path).1)
            };
            let mut half = way.profile.width / 2.0;
            if way.profile.class.is_path() {
                half = half.max(over(Kind::Path).2);
            }
            look(
                source,
                &way.points,
                &|k| way.nodes[k],
                &|_| false,
                &|i, _| Some(crossing(i, kind, None, clearance, half)),
            );
        }
        source += 1;
    }
    for obstacle in obstacles {
        let (kind, clearance, half) = over(obstacle.kind);
        look(
            source,
            &obstacle.points,
            &|k| obstacle.nodes[k],
            &|_| false,
            &|i, _| Some(crossing(i, kind, None, clearance, half)),
        );
        source += 1;
    }
    for (j, piece) in pieces.iter().enumerate() {
        let lower = &ways[piece.way as usize];
        let half = lower.profile.width / 2.0;
        // a piece's own nodes are known only at its ends
        let last = piece.points.len() - 1;
        let node = |k: usize| {
            if k == 0 {
                piece.nodes[0]
            } else if k == last {
                piece.nodes[1]
            } else {
                i64::MIN
            }
        };
        look(
            source,
            &piece.points,
            &node,
            &|i| i as usize == j,
            &|i, at| {
                let over = ways[pieces[i as usize].way as usize].profile.level;
                let clearance = over_bridge(lower.profile.class);
                (layer(over) > layer(lower.profile.level))
                    .then(|| crossing(i, "bridge", Some((j as u32, at)), clearance, half))
            },
        );
        source += 1;
    }
    found.sort_by(|a, b| a.over.cmp(&b.over).then(a.at.total_cmp(&b.at)));
    found
}

/// Where along each of two segments they cross, and the sine of their angle.
fn meet(a: Point, b: Point, c: Point, d: Point) -> Option<(f64, f64, f64)> {
    let (r, s) = ([b[0] - a[0], b[1] - a[1]], [d[0] - c[0], d[1] - c[1]]);
    let cross = r[0] * s[1] - r[1] * s[0];
    if cross.abs() < 1e-12 {
        return None;
    }
    let sine = cross.abs() / (distance(a, b) * distance(c, d)).max(f64::MIN_POSITIVE);
    let q = [c[0] - a[0], c[1] - a[1]];
    let t = (q[0] * s[1] - q[1] * s[0]) / cross;
    let u = (q[0] * r[1] - q[1] * r[0]) / cross;
    ((0.0..=1.0).contains(&t) && (0.0..1.0).contains(&u)).then_some((t, u, sine))
}

/// Which pieces go together.
struct Groups(Vec<usize>);

impl Groups {
    fn new(n: usize) -> Self {
        Self((0..n).collect())
    }

    fn add(&mut self) {
        self.0.push(self.0.len());
    }

    fn find(&mut self, mut i: usize) -> usize {
        while self.0[i] != i {
            self.0[i] = self.0[self.0[i]];
            i = self.0[i];
        }
        i
    }

    /// The lower index is kept, so that the order things join in is no matter.
    fn join(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        self.0[a.max(b)] = a.min(b);
    }
}

#[cfg(test)]
mod tests {
    use super::super::profile::profile;
    use super::*;

    fn way(id: i64, tags: &[(&str, &'static str)], nodes: &[(i64, f64, f64)]) -> Way {
        Way {
            id,
            nodes: nodes.iter().map(|n| n.0).collect(),
            points: nodes.iter().map(|n| [n.1, n.2]).collect(),
            profile: profile(|k| tags.iter().find(|t| t.0 == k).map(|t| t.1)).unwrap(),
            scale: 1.0,
        }
    }

    const ROAD: &[(&str, &str)] = &[("highway", "primary")];
    const BRIDGE: &[(&str, &str)] = &[("highway", "primary"), ("bridge", "yes")];
    const HIGHER: &[(&str, &str)] = &[("highway", "primary"), ("bridge", "yes"), ("layer", "2")];

    #[test]
    fn a_bridge_over_a_road_and_a_river() {
        let ways = [
            way(1, ROAD, &[(1, -50.0, 0.0), (2, 0.0, 0.0)]),
            way(2, BRIDGE, &[(2, 0.0, 0.0), (3, 60.0, 0.0), (4, 100.0, 0.0)]),
            way(3, ROAD, &[(4, 100.0, 0.0), (5, 150.0, 0.0)]),
            way(4, ROAD, &[(6, 30.0, -50.0), (7, 30.0, 50.0)]),
        ];
        let river = Obstacle {
            nodes: vec![8, 9],
            points: vec![[80.0, -50.0], [80.0, 50.0]],
            kind: Kind::River,
        };
        let groups = build(&ways, &[river]);
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        let seen: Vec<_> = g.pieces.iter().map(|p| (p.approach, p.anchor)).collect();
        // the road either side of it leads up to it
        assert_eq!(
            seen,
            [
                (false, [true, true]),
                (true, [true, false]),
                (true, [false, true])
            ]
        );
        assert_eq!((g.pieces[1].nodes, g.pieces[2].nodes), ([1, 2], [4, 5]));
        let seen: Vec<_> = g
            .crossings
            .iter()
            .map(|c| (c.kind, c.at, c.clearance, c.under))
            .collect();
        assert_eq!(
            seen,
            [("road", 30.0, OVER_ROAD, None), ("water", 80.0, 2.0, None)]
        );
    }

    fn rail(id: i64, tags: &[(&str, &'static str)], nodes: &[(i64, f64, f64)]) -> Way {
        let get = |k: &str| tags.iter().find(|t| t.0 == k).map(|t| t.1);
        let track = super::super::rails::track(get).unwrap();
        Way {
            profile: track.profile(),
            ..way(id, ROAD, nodes)
        }
    }

    const TRACK: &[(&str, &str)] = &[("railway", "rail")];
    const TRACKS: &[(&str, &str)] = &[("railway", "rail"), ("bridge", "yes")];

    #[test]
    fn a_railways_bridge_is_led_up_to_by_its_track_as_far_as_a_road_crosses() {
        let ways = [
            // over the bridge, under it, and across the track
            way(1, HIGHER, &[(1, 20.0, -90.0), (2, 20.0, 90.0)]),
            way(2, ROAD, &[(3, 80.0, -90.0), (4, 80.0, 90.0)]),
            way(
                3,
                ROAD,
                &[(5, 250.0, -90.0), (6, 250.0, 0.0), (7, 250.0, 90.0)],
            ),
            rail(
                4,
                TRACK,
                &[(8, -300.0, 0.0), (9, -250.0, 0.0), (10, 0.0, 0.0)],
            ),
            rail(5, TRACKS, &[(10, 0.0, 0.0), (11, 100.0, 0.0)]),
            rail(
                6,
                TRACK,
                &[(11, 100.0, 0.0), (6, 250.0, 0.0), (12, 400.0, 0.0)],
            ),
        ];
        let groups = build(&ways, &[]);
        assert_eq!(groups.len(), 1);
        let g = &groups[0];
        let of_track: Vec<_> = g
            .pieces
            .iter()
            .filter(|p| ways[p.way as usize].profile.class == Class::Rail)
            .map(|p| (p.approach, p.nodes, p.anchor))
            .collect();
        // farther than a road, whose approach is half as long
        assert_eq!(
            of_track,
            [
                (false, [10, 11], [true, true]),
                (true, [8, 10], [true, false]),
                (true, [11, 6], [false, true])
            ]
        );
        let seen: Vec<_> = g.crossings.iter().map(|c| (c.kind, c.clearance)).collect();
        assert_eq!(seen, [("bridge", 6.5), ("road", OVER_ROAD)]);
    }

    #[test]
    fn tracks_side_by_side_are_one_group() {
        let at = |y: f64| {
            let ways = [
                rail(1, TRACKS, &[(1, 0.0, 0.0), (2, 100.0, 0.0)]),
                rail(2, TRACKS, &[(3, 100.0, y), (4, 0.0, y)]),
            ];
            build(&ways, &[]).len()
        };
        assert_eq!((at(4.5), at(9.0)), (1, 2));
        // roads' are the renderer's to tell
        let roads = [0.0, 4.5].map(|y| way(1, BRIDGE, &[(1, 0.0, y), (2, 100.0, y)]));
        let [a, mut b] = roads;
        b.nodes = vec![3, 4];
        assert_eq!(build(&[a, b], &[]).len(), 2);
    }

    #[test]
    fn three_levels_are_one_group_in_order() {
        let ways = [
            way(1, ROAD, &[(1, 0.0, -90.0), (2, 0.0, 90.0)]),
            way(2, BRIDGE, &[(3, -90.0, 1.0), (4, 90.0, 1.0)]),
            way(3, HIGHER, &[(5, -90.0, -89.0), (6, 90.0, 91.0)]),
        ];
        let groups = build(&ways, &[]);
        assert_eq!(groups.len(), 1);
        let kinds: Vec<_> = groups[0]
            .crossings
            .iter()
            .map(|c| (c.over, c.kind, c.under.map(|u| u.0)))
            .collect();
        assert_eq!(
            kinds,
            [(0, "road", None), (1, "road", None), (1, "bridge", Some(0))]
        );
    }

    #[test]
    fn bridges_that_join_are_one_group_and_steps_anchor_none() {
        let ways = [
            way(1, ROAD, &[(1, -50.0, 0.0), (2, 0.0, 0.0)]),
            way(2, BRIDGE, &[(2, 0.0, 0.0), (3, 50.0, 0.0)]),
            way(3, BRIDGE, &[(3, 50.0, 0.0), (4, 100.0, 0.0)]),
            way(4, BRIDGE, &[(3, 50.0, 0.0), (5, 50.0, 80.0)]),
        ];
        let steps = Obstacle {
            nodes: vec![3, 9],
            points: vec![[50.0, 0.0], [52.0, -9.0]],
            kind: Kind::Path,
        };
        let groups = build(&ways, &[steps]);
        assert_eq!(groups.len(), 1);
        let bridges = groups[0].pieces.iter().filter(|p| !p.approach);
        let anchors: Vec<_> = bridges.map(|p| p.anchor).collect();
        assert_eq!(anchors, [[true, false], [false, true], [false, true]]);
        assert!(groups[0].crossings.is_empty(), "{:?}", groups[0].crossings);
    }

    #[test]
    fn bridges_that_meet_on_the_ground_are_together_and_each_is_anchored() {
        let ways = [
            way(1, BRIDGE, &[(1, 0.0, 0.0), (2, 50.0, 0.0)]),
            way(2, BRIDGE, &[(2, 50.0, 0.0), (3, 100.0, 0.0)]),
            way(3, ROAD, &[(2, 50.0, 0.0), (4, 50.0, 50.0)]),
        ];
        let groups = build(&ways, &[]);
        assert_eq!(groups.len(), 1);
        let bridges = groups[0].pieces.iter().filter(|p| !p.approach);
        assert!(bridges.map(|p| p.anchor).eq([[true, true]; 2]));
    }

    #[test]
    fn a_tunnel_has_a_portal_where_it_meets_the_ground_and_none_within() {
        let tunnel = &[("highway", "primary"), ("tunnel", "yes")][..];
        let ways = [
            way(
                1,
                &[("highway", "primary")],
                &[(1, -100.0, 0.0), (2, 0.0, 0.0)],
            ),
            way(2, tunnel, &[(2, 0.0, 0.0), (3, 60.0, 0.0)]),
            way(
                3,
                tunnel,
                &[(3, 60.0, 0.0), (4, 90.0, 0.0), (5, 120.0, 0.0)],
            ),
            way(
                4,
                &[("highway", "primary")],
                &[(5, 120.0, 0.0), (6, 200.0, 0.0)],
            ),
            // a driveway that ends in one, with no road beyond
            way(
                5,
                &[("highway", "service"), ("tunnel", "yes")],
                &[(7, 0.0, 50.0), (8, 9.0, 50.0)],
            ),
        ];
        let found = portals(&ways);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].points, [[0.0, 0.0], [60.0, 0.0]]);
        assert_eq!(found[1].points, [[90.0, 0.0], [120.0, 0.0]]);
    }

    const FOOT: &[(&str, &str)] = &[("highway", "footway")];
    const FOOTBRIDGE: &[(&str, &str)] = &[("highway", "footway"), ("bridge", "yes")];
    const STEPS: &[(&str, &str)] = &[("highway", "steps")];

    #[test]
    fn a_footbridge_clears_the_road_and_is_led_up_to_by_its_path_and_its_steps() {
        let ways = [
            // the path beyond is crossed by a road, which is not raised
            way(1, FOOT, &[(1, -200.0, 0.0), (2, -50.0, 0.0), (3, 0.0, 0.0)]),
            way(2, FOOTBRIDGE, &[(3, 0.0, 0.0), (4, 60.0, 0.0)]),
            way(3, STEPS, &[(4, 60.0, 0.0), (5, 70.0, 0.0)]),
            way(4, FOOT, &[(5, 70.0, 0.0), (6, 200.0, 0.0)]),
            way(5, ROAD, &[(7, 30.0, -50.0), (8, 30.0, 50.0)]),
            way(
                6,
                ROAD,
                &[(9, -50.0, -50.0), (2, -50.0, 0.0), (10, -50.0, 50.0)],
            ),
        ];
        let groups = build(&ways, &[]);
        assert_eq!(groups.len(), 1);
        let seen: Vec<_> = groups[0]
            .pieces
            .iter()
            .map(|p| (p.way, p.approach, p.nodes, p.anchor))
            .collect();
        assert_eq!(
            seen,
            [
                (1, false, [3, 4], [true, true]),
                (0, true, [2, 3], [true, false]),
                (2, true, [4, 5], [false, true])
            ]
        );
        let seen: Vec<_> = groups[0]
            .crossings
            .iter()
            .map(|c| (c.kind, c.at, c.clearance))
            .collect();
        assert_eq!(seen, [("road", 30.0, OVER_ROAD)]);
    }

    #[test]
    fn where_a_path_goes_on_beside_the_steps_it_is_the_path_that_leads_up() {
        let ways = [
            way(1, FOOTBRIDGE, &[(1, 0.0, 0.0), (2, 60.0, 0.0)]),
            way(2, STEPS, &[(2, 60.0, 0.0), (3, 60.0, 9.0)]),
            way(3, FOOT, &[(2, 60.0, 0.0), (4, 110.0, 0.0)]),
        ];
        let groups = build(&ways, &[]);
        let led: Vec<_> = groups[0].pieces.iter().filter(|p| p.approach).collect();
        assert_eq!((led.len(), led[0].way), (1, 2));
        // a sidewalk's bridge is led up to by none
        let sidewalk = &[
            ("highway", "footway"),
            ("footway", "sidewalk"),
            ("bridge", "yes"),
        ];
        let [_, steps, path] = ways;
        let ways = [
            way(1, sidewalk, &[(1, 0.0, 0.0), (2, 60.0, 0.0)]),
            steps,
            path,
        ];
        assert_eq!(build(&ways, &[])[0].pieces.len(), 1);
    }

    #[test]
    fn equal_layers_have_no_order() {
        let ways = [
            way(1, BRIDGE, &[(1, -50.0, 0.0), (2, 50.0, 0.0)]),
            way(2, BRIDGE, &[(3, 0.0, -50.0), (4, 0.0, 50.0)]),
        ];
        let groups = build(&ways, &[]);
        assert_eq!(groups.len(), 2);
        assert!(groups.iter().all(|g| g.crossings.is_empty()));
    }

    #[test]
    fn a_road_between_two_bridges_leads_to_both() {
        let ways = [
            way(1, BRIDGE, &[(1, 0.0, 0.0), (2, 50.0, 0.0)]),
            way(2, ROAD, &[(2, 50.0, 0.0), (3, 80.0, 0.0)]),
            way(3, ROAD, &[(3, 80.0, 0.0), (4, 100.0, 0.0)]),
            way(4, BRIDGE, &[(4, 100.0, 0.0), (5, 150.0, 0.0)]),
        ];
        let groups = build(&ways, &[]);
        assert_eq!(groups.len(), 1);
        let approaches: Vec<_> = groups[0].pieces.iter().filter(|p| p.approach).collect();
        assert_eq!(approaches.len(), 1);
        assert_eq!(
            (approaches[0].nodes, approaches[0].anchor),
            ([2, 4], [false, false])
        );
        assert_eq!(
            approaches[0].points,
            [[50.0, 0.0], [80.0, 0.0], [100.0, 0.0]]
        );
    }

    #[test]
    fn an_approach_stops_at_a_junction_and_at_its_length() {
        let ways = [
            way(1, BRIDGE, &[(1, 0.0, 0.0), (2, 50.0, 0.0)]),
            way(2, ROAD, &[(2, 50.0, 0.0), (3, 80.0, 0.0), (4, 900.0, 0.0)]),
            way(3, ROAD, &[(3, 80.0, 0.0), (5, 80.0, 90.0)]),
            way(
                4,
                ROAD,
                &[
                    (1, 0.0, 0.0),
                    (6, -100.0, 0.0),
                    (7, -200.0, 0.0),
                    (8, -300.0, 0.0),
                ],
            ),
        ];
        let groups = build(&ways, &[]);
        let far: Vec<_> = groups[0]
            .pieces
            .iter()
            .filter(|p| p.approach)
            .map(|p| p.nodes)
            .collect();
        assert_eq!(far, [[1, 7], [2, 3]]);
    }
}
