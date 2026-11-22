//! What is painted on a road besides its lanes' lines: crossings, the lines
//! traffic stops at, and arrows. Only what the tags say is there.

use super::network::{Network, Point, Way, distance};
use super::profile::{Class, Turn};
use rustc_hash::{FxHashMap, FxHashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Zebra,
    /// The two rows of dots cyclists cross between.
    Dots,
    /// Two lines across the road.
    Lines,
    Stop,
    /// Where traffic stops at a signal.
    Signal,
    GiveWay,
    Arrow,
}

impl Kind {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Zebra => "zebra",
            Self::Dots => "dots",
            Self::Lines => "crossing",
            Self::Stop => "stop",
            Self::Signal => "signal",
            Self::GiveWay => "give_way",
            Self::Arrow => "arrow",
        }
    }

    /// Metres across its strokes.
    pub(super) fn width(self) -> f64 {
        match self {
            Self::Zebra => ZEBRA,
            Self::Dots | Self::Stop | Self::GiveWay => 0.5,
            Self::Signal => 0.4,
            Self::Lines => 0.3,
            Self::Arrow => 0.25,
        }
    }

    /// Metres of paint and of gap along its strokes, if they are dashed.
    pub(super) fn dash(self) -> Option<(f64, f64)> {
        matches!(self, Self::Zebra | Self::Dots | Self::GiveWay).then_some((BAR, BAR))
    }
}

/// How a way's or a node's tags say a crossing is marked.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Marking {
    Unmarked,
    /// As what, and how surely: 2 the marking is tagged, 1 that there is one,
    /// 0 it has signals.
    Marked(Kind, u8),
}

/// Which way along its way a node's sign faces traffic.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Facing {
    Along,
    Against,
    Both,
    Unsaid,
}

/// What a node of a road says is there.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sign {
    /// `None` if its marking is not tagged.
    Crossing(Option<Marking>),
    Line(Kind, Facing),
}

/// One painted thing, in world units.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Detail {
    pub way: u32,
    pub kind: Kind,
    /// As [`Marking::Marked`]'s.
    pub confidence: u8,
    /// Where on its way's line it is.
    pub at: Point,
    pub strokes: Vec<Vec<Point>>,
    /// Triangles.
    pub heads: Vec<[Point; 3]>,
}

#[derive(Default)]
pub(super) struct Details {
    pub list: Vec<Detail>,
    /// The stretches of each of the network's runs, metres along it, that hold
    /// no lanes' lines. Its ends' cuts are not among them.
    pub gaps: Vec<Vec<[f64; 2]>>,
}

/// Metres of a bar and of the gap between bars.
const BAR: f64 = 0.5;
/// Metres a zebra takes along the road, the dots' and lines' from row to row.
const ZEBRA: f64 = 4.0;
const ROWS: f64 = 2.5;
/// Metres lines keep from a crossing, traffic's from where it stops to one, and
/// how near a junction (beyond its cut) a crossing takes the lines between.
const CLEAR: f64 = 0.6;
const STOP_BACK: f64 = 1.5;
const TO_JUNCTION: f64 = 12.0;
/// Metres past a crossing a stop may be mapped that belongs before it.
const PAST: f64 = 10.0;
/// Metres apart two lines are one.
const ONE_LINE: f64 = 5.0;
/// The furthest from a junction, metres, a sign is taken to face it.
const NEAR_JUNCTION: f64 = 35.0;
/// Metres from what it leads up to an arrow ends, an arrow's length, from one
/// to the next, and how many there are.
const ARROW_BACK: f64 = 6.0;
const ARROW: f64 = 5.0;
const ARROW_EVERY: f64 = 30.0;
const ARROWS: u32 = 3;

/// How the tags of a crossing, node or way, say it is marked.
pub(super) fn marking<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Marking> {
    if let Some(value) = get("crossing:markings") {
        let has = |start: &str| value.split(';').any(|v| v.trim().starts_with(start));
        return Some(if has("zebra") || has("ladder") {
            Marking::Marked(Kind::Zebra, 2)
        } else if has("dots") {
            Marking::Marked(Kind::Dots, 2)
        } else if has("lines") {
            Marking::Marked(Kind::Lines, 2)
        } else if value == "yes" {
            Marking::Marked(Kind::Zebra, 1)
        } else {
            // none, or of a kind not drawn
            Marking::Unmarked
        });
    }
    match (get("crossing"), get("crossing_ref")) {
        (Some("unmarked" | "informal" | "no"), _) => Some(Marking::Unmarked),
        (_, Some("zebra")) | (Some("zebra" | "marked" | "uncontrolled"), _) => {
            Some(Marking::Marked(Kind::Zebra, 1))
        }
        (Some("traffic_signals"), _) => Some(Marking::Marked(Kind::Zebra, 0)),
        _ => None,
    }
}

/// What a node with these tags says is there.
pub(super) fn sign<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Sign> {
    let kind = match get("highway")? {
        "crossing" => return Some(Sign::Crossing(marking(get))),
        "stop" => Kind::Stop,
        "give_way" => Kind::GiveWay,
        "traffic_signals" => Kind::Signal,
        _ => return None,
    };
    let facing = match get("traffic_signals:direction").or_else(|| get("direction")) {
        Some("forward") => Facing::Along,
        Some("backward") => Facing::Against,
        Some("both") => Facing::Both,
        _ => Facing::Unsaid,
    };
    Some(Sign::Line(kind, facing))
}

/// A run as it is walked: its points and the metres along it of each.
struct Walk<'a> {
    way: &'a Way,
    points: &'a [Point],
    along: Vec<f64>,
}

impl<'a> Walk<'a> {
    fn new(way: &'a Way, points: std::ops::Range<usize>) -> Self {
        let points = &way.points[points];
        let mut along = vec![0.0];
        for p in points.windows(2) {
            along.push(along[along.len() - 1] + distance(p[0], p[1]) / way.scale);
        }
        Self { way, points, along }
    }

    fn length(&self) -> f64 {
        self.along[self.along.len() - 1]
    }

    /// The point `at` metres along, and the direction there.
    fn at(&self, at: f64) -> (Point, Point) {
        let last = self.points.len() - 1;
        let k = self.along.partition_point(|&a| a < at).clamp(1, last);
        let (a, b) = (self.points[k - 1], self.points[k]);
        let length = (self.along[k] - self.along[k - 1]).max(f64::MIN_POSITIVE);
        let t = ((at - self.along[k - 1]) / length).clamp(0.0, 1.0);
        let units = distance(a, b).max(f64::MIN_POSITIVE);
        (
            [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t],
            [(b[0] - a[0]) / units, (b[1] - a[1]) / units],
        )
    }

    /// The direction over a few metres about `at`: a node in a bend has none of
    /// its own.
    fn direction(&self, at: f64, reach: f64) -> Point {
        let (a, b) = (
            self.at((at - reach).max(0.0)).0,
            self.at((at + reach).min(self.length())).0,
        );
        let units = distance(a, b);
        if units > 0.0 {
            [(b[0] - a[0]) / units, (b[1] - a[1]) / units]
        } else {
            self.at(at).1
        }
    }
}

/// `x` metres right and `y` ahead of `at`, facing `direction` (y grows
/// southwards).
fn place(at: Point, direction: Point, scale: f64, [x, y]: [f64; 2]) -> Point {
    [
        at[0] + (direction[0] * y - direction[1] * x) * scale,
        at[1] + (direction[1] * y + direction[0] * x) * scale,
    ]
}

/// A crossing on a run.
#[derive(Clone, Copy)]
struct Crossing {
    at: f64,
    /// Metres either way along the road it takes.
    half: f64,
}

/// `signs` by node; `crossings`, the nodes of ways that cross, with how they
/// are marked.
pub(super) fn build(
    ways: &[Way],
    network: &Network,
    signs: &FxHashMap<i64, Sign>,
    crossings: &FxHashMap<i64, Marking>,
) -> Details {
    let rings: FxHashSet<i64> = ways
        .iter()
        .filter(|w| w.profile.roundabout)
        .flat_map(|w| w.nodes.iter().copied())
        .collect();
    let mut details = Details::default();
    // a node two runs share is drawn once
    let mut drawn: FxHashSet<i64> = FxHashSet::default();
    for run in &network.runs {
        let way = &ways[run.way as usize];
        let nodes = &way.nodes[run.points.clone()];
        let walk = Walk::new(way, run.points.clone());
        let length = walk.length();
        let last = nodes.len() - 1;
        let Some(lanes) = &way.profile.lanes else {
            details.gaps.push(Vec::new());
            continue;
        };
        // an end of a run that is a junction, and not just another way's
        let junction = |end: usize| {
            run.cut[end] > 0.0
                && network
                    .junction_nodes
                    .contains(&nodes[if end == 0 { 0 } else { last }])
        };
        let mut gaps: Vec<[f64; 2]> = Vec::new();

        let mut across: Vec<Crossing> = Vec::new();
        for (k, node) in nodes.iter().enumerate() {
            let marked = match signs.get(node) {
                Some(Sign::Crossing(Some(marking))) => Some(*marking),
                Some(Sign::Line(..)) => None,
                _ => crossings.get(node).copied(),
            };
            let Some(Marking::Marked(kind, confidence)) = marked else {
                continue;
            };
            // in a junction it says not across which arm
            if (k == 0 && junction(0)) || (k == last && junction(1)) {
                continue;
            }
            let at = walk.along[k];
            let half = match kind {
                Kind::Zebra => ZEBRA / 2.0,
                _ => (ROWS + kind.width()) / 2.0,
            };
            // two mapped where there is room for one
            if across.iter().any(|c| (c.at - at).abs() < c.half + half) {
                continue;
            }
            across.push(Crossing { at, half });
            if drawn.insert(*node) {
                details
                    .list
                    .push(crossing(run.way, &walk, at, kind, confidence));
            }
        }
        for c in &across {
            let mut gap = [c.at - c.half - CLEAR, c.at + c.half + CLEAR];
            if junction(0) && gap[0] < run.cut[0] + TO_JUNCTION {
                gap[0] = 0.0;
            }
            if junction(1) && gap[1] > length - run.cut[1] - TO_JUNCTION {
                gap[1] = length;
            }
            gaps.push(gap);
        }

        // where traffic stops, along the way and against it: metres along, what
        let mut stops: [Vec<(f64, Kind, u8)>; 2] = Default::default();
        for (k, node) in nodes.iter().enumerate() {
            let Some(&Sign::Line(kind, facing)) = signs.get(node) else {
                continue;
            };
            let at = walk.along[k];
            let at_junction = [k == 0 && junction(0), k == last && junction(1)];
            if at_junction[0] || at_junction[1] {
                // signals at the junction itself are every arm's
                if kind == Kind::Signal {
                    stops[usize::from(at_junction[0])].push((at, kind, 1));
                }
                continue;
            }
            // an end two runs share is the one's whose traffic comes to it
            let reaches = [k > 0, k < last];
            let toward = |along: bool| {
                let end = if along { length - at } else { at };
                junction(usize::from(along)) && end <= NEAR_JUNCTION
            };
            let faces = match facing {
                Facing::Along => [true, false],
                Facing::Against => [false, true],
                Facing::Both => [true, true],
                Facing::Unsaid if lanes.along != lanes.against => [lanes.along, lanes.against],
                Facing::Unsaid => match (toward(true), toward(false)) {
                    (true, true) if at * 2.0 > length => [true, false],
                    (true, true) => [false, true],
                    other => [other.0, other.1],
                },
            };
            for side in 0..2 {
                if faces[side] && reaches[side] && [lanes.along, lanes.against][side] {
                    stops[side].push((at, kind, 2));
                }
            }
        }
        // who enters a roundabout gives way
        for (side, end) in [(0, 1), (1, 0)] {
            let node = nodes[if end == 0 { 0 } else { last }];
            let comes = [lanes.along, lanes.against][side];
            let signed = stops[side].iter().any(|s| {
                let d = if side == 0 { length - s.0 } else { s.0 };
                d <= NEAR_JUNCTION
            });
            if comes
                && !signed
                && junction(end)
                && rings.contains(&node)
                && !way.profile.roundabout
                && way.profile.class < Class::Service
            {
                stops[side].push((if side == 0 { length } else { 0.0 }, Kind::GiveWay, 1));
            }
        }
        // the first line each direction's arrows lead up to, if any, and every
        // line, metres from where its traffic comes into the run
        let mut line_at: [Option<f64>; 2] = [None, None];
        let mut lines_at: [Vec<f64>; 2] = Default::default();
        for side in 0..2 {
            // of two lines in one place the first goes
            let order = |kind| {
                [Kind::Stop, Kind::Signal, Kind::GiveWay]
                    .iter()
                    .position(|k| *k == kind)
            };
            stops[side].sort_by_key(|s| order(s.1));
            for &(at, kind, confidence) in &stops[side] {
                // as traffic there sees it: metres from where it comes into the run
                let flip = |s: f64| if side == 0 { s } else { length - s };
                let end_cut = run.cut[1 - side];
                let mut s = flip(at);
                if junction(1 - side) {
                    s = s.min(length - end_cut);
                }
                let mut before: Vec<Crossing> = across.clone();
                before.sort_by(|a, b| flip(b.at).total_cmp(&flip(a.at)));
                for c in before {
                    let (near, far) = (flip(c.at) - c.half, flip(c.at) + c.half);
                    if s > near - STOP_BACK && s < far + PAST {
                        s = near - STOP_BACK;
                    }
                }
                if s < 2.0 || lines_at[side].iter().any(|l| (l - s).abs() < ONE_LINE) {
                    continue;
                }
                lines_at[side].push(s);
                let place_at = flip(s);
                details
                    .list
                    .push(line(run.way, &walk, place_at, side == 0, kind, confidence));
                if junction(1 - side) && length - s < end_cut + NEAR_JUNCTION {
                    line_at[side] = Some(line_at[side].map_or(s, |o: f64| o.min(s)));
                    if side == 0 {
                        gaps.push([place_at, length]);
                    } else {
                        gaps.push([0.0, place_at]);
                    }
                }
            }
        }

        for side in 0..2 {
            let turns = &lanes.turns[side];
            if turns.is_empty() {
                continue;
            }
            let flip = |s: f64| if side == 0 { s } else { length - s };
            let mut end = length - run.cut[1 - side];
            if let Some(stop) = line_at[side] {
                end = end.min(stop);
            }
            for c in &across {
                let at = flip(c.at);
                if junction(1 - side)
                    && at + c.half + CLEAR > length - run.cut[1 - side] - TO_JUNCTION
                {
                    end = end.min(at - c.half - CLEAR);
                }
            }
            let mut tip = end - ARROW_BACK;
            for _ in 0..ARROWS {
                let tail = tip - ARROW;
                if tail < run.cut[side] + 2.0 {
                    break;
                }
                let clear = across.iter().all(|c| {
                    let at = flip(c.at);
                    at + c.half + CLEAR < tail || at - c.half - CLEAR > tip
                }) && lines_at[side]
                    .iter()
                    .all(|l| *l < tail - CLEAR || *l > tip + CLEAR);
                if clear {
                    let middle = flip(tail + ARROW / 2.0);
                    details
                        .list
                        .extend(arrows(run.way, &walk, middle, side == 0, turns));
                }
                tip -= ARROW_EVERY;
            }
        }
        for gap in &mut gaps {
            *gap = [gap[0].max(0.0), gap[1].min(length)];
        }
        gaps.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
        details.gaps.push(gaps);
    }
    details
}

/// A crossing square over its road: its strokes run across, so a zebra's bars
/// lie along the road.
fn crossing(way: u32, walk: &Walk, at: f64, kind: Kind, confidence: u8) -> Detail {
    let p = &walk.way.profile;
    let (point, direction) = (walk.at(at).0, walk.direction(at, 3.0));
    // bars at both edges, whole
    let width = match kind.dash() {
        Some((on, off)) => {
            let bars = ((p.width + off) / (on + off) + 1e-6).floor().max(1.0);
            bars * (on + off) - off
        }
        None => p.width,
    };
    let stroke = |y: f64| {
        [p.centre - width / 2.0, p.centre + width / 2.0]
            .map(|x| place(point, direction, walk.way.scale, [x, y]))
            .to_vec()
    };
    let strokes = match kind {
        Kind::Zebra => vec![stroke(0.0)],
        _ => vec![stroke(-ROWS / 2.0), stroke(ROWS / 2.0)],
    };
    Detail {
        way,
        kind,
        confidence,
        at: point,
        strokes,
        heads: Vec::new(),
    }
}

/// A line across the lanes of one direction.
fn line(way: u32, walk: &Walk, at: f64, along: bool, kind: Kind, confidence: u8) -> Detail {
    let (point, direction) = (walk.at(at).0, walk.direction(at, 3.0));
    let lanes = walk.way.profile.lanes.as_ref().expect("a motor road's");
    let span = lanes.span(along);
    let stroke = span.map(|x| place(point, direction, walk.way.scale, [x, 0.0]));
    Detail {
        way,
        kind,
        confidence,
        at: point,
        strokes: vec![stroke.to_vec()],
        heads: Vec::new(),
    }
}

/// The arrows of one direction's lanes, side by side about `middle`.
fn arrows(way: u32, walk: &Walk, middle: f64, along: bool, turns: &[Vec<Turn>]) -> Vec<Detail> {
    let point = walk.at(middle).0;
    let mut direction = walk.direction(middle, ARROW / 2.0);
    let lanes = walk.way.profile.lanes.as_ref().expect("a motor road's");
    let span = lanes.span(along);
    if !along {
        direction = [-direction[0], -direction[1]];
    }
    let mut out = Vec::new();
    for (k, lane) in turns.iter().enumerate() {
        let (strokes, heads) = arrow(lane);
        if strokes.is_empty() {
            continue;
        }
        // lanes are from the driver's left
        let off = (k as f64 + 0.5) * lanes.width;
        let x = if along {
            span[0] + off
        } else {
            -(span[1] - off)
        };
        let put = |q: [f64; 2]| {
            place(
                point,
                direction,
                walk.way.scale,
                [x + q[0], q[1] - ARROW / 2.0],
            )
        };
        out.push(Detail {
            way,
            kind: Kind::Arrow,
            confidence: 2,
            at: point,
            strokes: strokes
                .into_iter()
                .map(|s| s.into_iter().map(put).collect())
                .collect(),
            heads: heads.into_iter().map(|h| h.map(put)).collect(),
        });
    }
    out
}

type Strokes = Vec<Vec<[f64; 2]>>;
type Heads = Vec<[[f64; 2]; 3]>;

/// An arrow's strokes and heads, metres right and ahead of its tail.
fn arrow(turns: &[Turn]) -> (Strokes, Heads) {
    let (mut strokes, mut heads) = (Strokes::new(), Heads::new());
    let through = turns.contains(&Turn::Through);
    if through {
        strokes.push(vec![[0.0, 0.0], [0.0, 3.6]]);
        heads.push([[-0.4, 3.5], [0.4, 3.5], [0.0, ARROW]]);
    }
    // beside an arrow ahead the others leave it low
    let rise = if through { 1.6 } else { 2.6 };
    for turn in turns {
        // degrees from ahead, left
        let angle: f64 = match turn {
            Turn::Through => continue,
            Turn::MergeLeft => 25.0,
            Turn::SlightLeft => 40.0,
            Turn::Left => 90.0,
            Turn::SharpLeft => 135.0,
            Turn::MergeRight => -25.0,
            Turn::SlightRight => -40.0,
            Turn::Right => -90.0,
            Turn::SharpRight => -135.0,
            Turn::Reverse => {
                let turned: Vec<[f64; 2]> = (0..=6)
                    .map(|k| {
                        let (s, c) = (f64::from(k) / 6.0 * std::f64::consts::PI).sin_cos();
                        [-0.45 + 0.45 * c, 3.6 + 0.45 * s]
                    })
                    .collect();
                let mut stroke = vec![[0.0, 0.0]];
                stroke.extend(turned);
                stroke.push([-0.9, 3.0]);
                strokes.push(stroke);
                heads.push([[-1.25, 3.1], [-0.55, 3.1], [-0.9, 2.0]]);
                continue;
            }
        };
        let (s, c) = angle.to_radians().sin_cos();
        let out = [-s, c];
        // ahead, then round to where it points
        let (from, bend) = ([0.0, rise], [0.0, rise + 0.6]);
        let end = [bend[0] + out[0] * 0.6, bend[1] + out[1] * 0.6];
        let mut stroke = vec![[0.0, 0.0]];
        stroke.extend((0..=4).map(|k| {
            let t = f64::from(k) / 4.0;
            let mix = |i: usize| {
                from[i] * (1.0 - t) * (1.0 - t) + 2.0 * bend[i] * t * (1.0 - t) + end[i] * t * t
            };
            [mix(0), mix(1)]
        }));
        strokes.push(stroke);
        let side = [out[1], -out[0]];
        let base = [end[0] - out[0] * 0.1, end[1] - out[1] * 0.1];
        heads.push([
            [base[0] - side[0] * 0.45, base[1] - side[1] * 0.45],
            [base[0] + side[0] * 0.45, base[1] + side[1] * 0.45],
            [end[0] + out[0] * 0.8, end[1] + out[1] * 0.8],
        ]);
    }
    (strokes, heads)
}

impl Detail {
    /// Its paint as convex rings: strokes widened and cut to their dashes.
    pub(super) fn rings(&self, scale: f64) -> Vec<Vec<Point>> {
        let half = self.kind.width() / 2.0 * scale;
        let mut out: Vec<Vec<Point>> = Vec::new();
        for stroke in &self.strokes {
            let dash = self.kind.dash().map(|(on, off)| (on * scale, off * scale));
            for p in stroke.windows(2) {
                let length = distance(p[0], p[1]);
                if length <= 0.0 {
                    continue;
                }
                let along = [(p[1][0] - p[0][0]) / length, (p[1][1] - p[0][1]) / length];
                let right = [-along[1] * half, along[0] * half];
                let mut bar = |from: f64, to: f64| {
                    let at = |s: f64, side: f64| {
                        [
                            p[0][0] + along[0] * s + right[0] * side,
                            p[0][1] + along[1] * s + right[1] * side,
                        ]
                    };
                    out.push(vec![
                        at(from, -1.0),
                        at(to, -1.0),
                        at(to, 1.0),
                        at(from, 1.0),
                    ]);
                };
                let Some((on, off)) = dash else {
                    bar(0.0, length);
                    continue;
                };
                let mut s = 0.0;
                while s < length {
                    bar(s, (s + on).min(length));
                    s += on + off;
                }
            }
        }
        out.extend(self.heads.iter().map(|h| h.to_vec()));
        out
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

    fn said(tags: &[(&str, &'static str)]) -> Option<Marking> {
        marking(|k| tags.iter().find(|t| t.0 == k).map(|t| t.1))
    }

    fn details(ways: &[Way], signs: &[(i64, Sign)]) -> Details {
        let network = network::build(ways);
        build(
            ways,
            &network,
            &signs.iter().copied().collect(),
            &FxHashMap::default(),
        )
    }

    const PRIMARY: &[(&str, &str)] = &[("highway", "primary"), ("lanes", "2")];
    const ZEBRA_SIGN: Sign = Sign::Crossing(Some(Marking::Marked(Kind::Zebra, 2)));

    /// Eastwards to a junction at (0, 0), with a crossing road.
    fn junction(first: &[(&str, &'static str)]) -> Vec<Way> {
        vec![
            way(
                first,
                &[
                    (1, -100.0, 0.0),
                    (2, -20.0, 0.0),
                    (3, -14.0, 0.0),
                    (4, 0.0, 0.0),
                ],
            ),
            way(PRIMARY, &[(4, 0.0, 0.0), (5, 100.0, 0.0)]),
            way(PRIMARY, &[(6, 0.0, -100.0), (4, 0.0, 0.0), (7, 0.0, 100.0)]),
        ]
    }

    fn of(d: &Details, kind: Kind) -> Vec<&Detail> {
        d.list.iter().filter(|d| d.kind == kind).collect()
    }

    #[test]
    fn a_crossing_is_marked_only_where_the_tags_say() {
        use Marking::{Marked, Unmarked};
        assert_eq!(said(&[]), None);
        assert_eq!(said(&[("crossing", "unmarked")]), Some(Unmarked));
        assert_eq!(
            said(&[("crossing", "uncontrolled"), ("crossing:markings", "no")]),
            Some(Unmarked)
        );
        assert_eq!(
            said(&[("crossing", "marked")]),
            Some(Marked(Kind::Zebra, 1))
        );
        assert_eq!(
            said(&[("crossing", "traffic_signals")]),
            Some(Marked(Kind::Zebra, 0))
        );
        assert_eq!(
            said(&[
                ("crossing", "traffic_signals"),
                ("crossing:markings", "dots;zebra")
            ]),
            Some(Marked(Kind::Zebra, 2))
        );
        assert_eq!(
            said(&[("crossing:markings", "dots")]),
            Some(Marked(Kind::Dots, 2))
        );
        assert_eq!(said(&[("crossing:markings", "surface")]), Some(Unmarked));
    }

    #[test]
    fn a_zebra_is_square_over_its_road_and_stops_its_lines() {
        let d = details(&junction(PRIMARY), &[(2, ZEBRA_SIGN)]);
        let zebras = of(&d, Kind::Zebra);
        assert_eq!(zebras.len(), 1);
        // seven bars and the gaps between, in 7 m
        assert_eq!(zebras[0].strokes, [vec![[-20.0, -3.25], [-20.0, 3.25]]]);
        assert_eq!(zebras[0].rings(1.0).len(), 7);
        // and the lines between it and the junction are left out
        assert_eq!(d.gaps[0], [[100.0 - 20.0 - 2.6, 100.0]]);
        assert!(d.gaps[1].is_empty());
        // in the junction itself nobody knows across what
        assert!(
            details(&junction(PRIMARY), &[(4, ZEBRA_SIGN)])
                .list
                .is_empty()
        );
    }

    #[test]
    fn a_crossing_where_two_ways_meet_is_drawn_once() {
        let ways = [
            way(PRIMARY, &[(1, -50.0, 0.0), (2, 0.0, 0.0)]),
            way(PRIMARY, &[(2, 0.0, 0.0), (3, 50.0, 0.0)]),
        ];
        let d = details(&ways, &[(2, ZEBRA_SIGN)]);
        assert_eq!(d.list.len(), 1);
        assert_eq!(d.gaps, [vec![[47.4, 50.0]], vec![[0.0, 2.6]]]);
    }

    #[test]
    fn a_stop_lies_before_the_junction_and_before_a_crossing() {
        let stop = |facing| Sign::Line(Kind::Stop, facing);
        let lone = details(&junction(PRIMARY), &[(3, stop(Facing::Along))]);
        // on the driver's side, where it is mapped
        assert_eq!(lone.list[0].strokes, [vec![[-14.0, 0.0], [-14.0, 3.5]]]);
        let both = details(
            &junction(PRIMARY),
            &[(2, ZEBRA_SIGN), (3, stop(Facing::Along))],
        );
        let x = of(&both, Kind::Stop)[0].strokes[0][0][0];
        assert_eq!(x, -20.0 - 2.0 - STOP_BACK);
        // of two in one place there is one
        let road = [
            way(
                PRIMARY,
                &[
                    (1, -100.0, 0.0),
                    (2, -20.0, 0.0),
                    (3, -17.0, 0.0),
                    (4, 0.0, 0.0),
                ],
            ),
            way(PRIMARY, &[(6, 0.0, -100.0), (4, 0.0, 0.0), (7, 0.0, 100.0)]),
        ];
        let give_way = Sign::Line(Kind::GiveWay, Facing::Along);
        let two = details(&road, &[(2, give_way), (3, stop(Facing::Along))]);
        assert_eq!((two.list.len(), two.list[0].kind), (1, Kind::Stop));
        let zebras = details(&road, &[(2, ZEBRA_SIGN), (3, ZEBRA_SIGN)]);
        assert_eq!(zebras.list.len(), 1);
        // unsaid, the junction near by says
        let unsaid = details(&junction(PRIMARY), &[(3, stop(Facing::Unsaid))]);
        assert_eq!(unsaid.list, lone.list);
        // and far from any, nothing does
        let far = [way(
            PRIMARY,
            &[(1, 0.0, 0.0), (2, 300.0, 0.0), (3, 600.0, 0.0)],
        )];
        assert!(details(&far, &[(2, stop(Facing::Unsaid))]).list.is_empty());
        // facing away from the junction it is the other side's
        let away = details(&junction(PRIMARY), &[(2, stop(Facing::Against))]);
        assert_eq!(away.list[0].strokes, [vec![[-20.0, -3.5], [-20.0, 0.0]]]);
    }

    #[test]
    fn signals_at_a_junction_stop_every_arm() {
        let d = details(
            &junction(PRIMARY),
            &[(4, Sign::Line(Kind::Signal, Facing::Unsaid))],
        );
        assert_eq!(of(&d, Kind::Signal).len(), 4);
        for s in of(&d, Kind::Signal) {
            let x = s.strokes[0][0][0].abs().max(s.strokes[0][0][1].abs());
            assert!((x - 6.5).abs() < 1e-9, "{s:?}");
        }
    }

    #[test]
    fn who_enters_a_roundabout_gives_way_once() {
        let ring: Vec<(i64, f64, f64)> = (0..=24)
            .map(|k| {
                let a = f64::from(k) / 24.0 * std::f64::consts::TAU;
                (10 + i64::from(k % 24), 20.0 * a.cos(), 20.0 * a.sin())
            })
            .collect();
        let round = [("highway", "primary"), ("junction", "roundabout")];
        let ways = [
            way(&round, &ring),
            way(PRIMARY, &[(1, 120.0, 0.0), (2, 35.0, 0.0), (10, 20.0, 0.0)]),
            way(
                &[("highway", "service")],
                &[(22, -20.0, 0.0), (3, -90.0, 0.0)],
            ),
        ];
        let d = details(&ways, &[]);
        assert_eq!(d.list.len(), 1);
        assert_eq!((d.list[0].way, d.list[0].kind), (1, Kind::GiveWay));
        let signed = details(&ways, &[(2, Sign::Line(Kind::GiveWay, Facing::Along))]);
        assert_eq!(signed.list.len(), 1);
        assert_eq!(signed.list[0].confidence, 2);
    }

    #[test]
    fn arrows_stand_in_their_lanes_and_lead_up_to_the_junction() {
        let tags = [
            ("highway", "primary"),
            ("oneway", "yes"),
            ("lanes", "2"),
            ("turn:lanes", "left;through|"),
        ];
        let d = details(&junction(&tags), &[]);
        let arrows = of(&d, Kind::Arrow);
        assert_eq!(arrows.len(), 3);
        let first = arrows[0];
        // the left lane's, 6 m before the cut
        assert_eq!((first.strokes.len(), first.heads.len()), (2, 2));
        assert_eq!(first.strokes[0][0], [-6.5 - 6.0 - 5.0, -1.75]);
        assert_eq!(first.heads[0][2], [-6.5 - 6.0, -1.75]);
        // the left turn points north, which is up
        assert!(first.heads[1][2][1] < -1.75 - 0.5);
        // clear of a crossing, and none where the road is too short for one
        let crossed = details(&junction(&tags), &[(2, ZEBRA_SIGN)]);
        assert!(of(&crossed, Kind::Arrow).iter().all(|a| {
            let x = [a.strokes[0][0][0], a.heads[0][2][0]];
            x[1] < -22.6 || x[0] > -17.4
        }));
        let against = [
            ("highway", "primary"),
            ("oneway", "-1"),
            ("lanes", "2"),
            ("turn:lanes", "left|through"),
        ];
        let d = details(&junction(&against), &[]);
        let arrows = of(&d, Kind::Arrow);
        // drivers go west: their left lane is the southern one
        assert!(arrows[0].heads[0][2][1] > 0.0, "{:?}", arrows[0]);
    }

    #[test]
    fn every_turn_has_an_arrow_within_its_lane() {
        use Turn::*;
        for turn in [
            Through,
            SlightLeft,
            Left,
            SharpLeft,
            SlightRight,
            Right,
            SharpRight,
            Reverse,
            MergeLeft,
            MergeRight,
        ] {
            let (strokes, heads) = arrow(&[turn]);
            assert_eq!((strokes.len(), heads.len()), (1, 1));
            let points = strokes.iter().flatten().chain(heads.iter().flatten());
            for p in points {
                assert!(
                    p[0].abs() <= 1.45 && (0.0..=ARROW).contains(&p[1]),
                    "{turn:?} {p:?}"
                );
            }
        }
        assert!(arrow(&[]).0.is_empty());
    }
}
