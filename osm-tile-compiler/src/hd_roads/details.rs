//! What is painted on a road besides its lanes' lines: crossings, the lines
//! traffic stops at, and arrows. Only what the tags say is there, and of what
//! would lie on one another only the first of crossings, lines traffic stops
//! at, gores, arrows, merges' lines (`merges.rs`), guides (`junctions.rs`) and
//! lanes' (`paint.rs`).

use super::country::{GiveWay, Rules, Signalled};
use super::furniture::{Area, AreaKind, ring};
use super::junctions;
use super::lanes::Section;
use super::network::{Network, Point, Way, distance};
use super::paint::{Block, Taken};
use super::profile::{Class, Lanes, Turn};
use rustc_hash::{FxHashMap, FxHashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    Zebra,
    /// The two rows of dots cyclists cross between.
    Dots,
    /// Two lines across the road.
    Lines,
    Stop,
    /// Where traffic stops at a signal, and where that is a line of bars.
    Signal,
    SignalDashed,
    GiveWay,
    /// Where traffic gives way, as triangles: the line across is not painted.
    Triangles,
    Arrow,
    /// A chevron between roads that part.
    Gore,
    /// A bar of the line between them, and of one between directions.
    Merge,
    Centre,
    /// A bar of the line between lanes that turn side by side.
    Guide,
    /// A mini roundabout's middle.
    Disc,
    /// What is painted in a cycle lane, and in a lane cyclists share.
    Bicycle,
    Sharrow,
    /// "BUS", in a bus lane.
    BusText,
    /// The triangles on a hump's ramp.
    Hump,
}

impl Kind {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Zebra => "zebra",
            Self::Dots => "dots",
            Self::Lines => "crossing",
            Self::Stop => "stop",
            Self::Signal | Self::SignalDashed => "signal",
            Self::GiveWay => "give_way",
            Self::Triangles => "give_way-tri",
            Self::Arrow => "arrow",
            Self::Gore => "gore",
            Self::Merge => "merge",
            Self::Centre => "centre",
            Self::Guide => "guide",
            Self::Disc => "disc",
            Self::Bicycle => "bicycle",
            Self::Sharrow => "sharrow",
            Self::BusText => "bus-text",
            Self::Hump => "hump",
        }
    }

    /// Metres across its strokes.
    pub(super) fn width(self) -> f64 {
        match self {
            Self::Zebra => ZEBRA,
            Self::Dots | Self::Stop | Self::GiveWay => 0.5,
            Self::Signal | Self::SignalDashed | Self::Gore => 0.4,
            Self::Triangles => TRIANGLE[1],
            Self::Lines => 0.3,
            Self::Arrow => 0.25,
            Self::Merge => 0.24,
            Self::Centre | Self::Guide => 0.12,
            Self::Bicycle | Self::Sharrow => 0.1,
            Self::BusText => 0.15,
            Self::Disc | Self::Hump => 0.0,
        }
    }

    /// Metres of paint and of gap along its strokes, if they are dashed.
    pub(super) fn dash(self) -> Option<(f64, f64)> {
        match self {
            Self::Zebra | Self::Dots | Self::GiveWay | Self::SignalDashed => Some((BAR, BAR)),
            Self::Triangles => Some((TRIANGLE[0], TRIANGLE[2])),
            _ => None,
        }
    }

    /// What gives way to what: the lower stays.
    pub(super) fn rank(self) -> u8 {
        match self {
            Self::Zebra | Self::Dots | Self::Lines => 0,
            Self::Stop | Self::Signal | Self::SignalDashed | Self::GiveWay | Self::Triangles => 1,
            Self::Gore | Self::Disc | Self::Hump => 2,
            Self::Arrow => 3,
            Self::Merge | Self::Centre => 4,
            Self::Guide => 5,
            Self::Bicycle | Self::Sharrow | Self::BusText => 6,
        }
    }

    /// Whether it is one of many in a row, left out where it gives way, whole.
    pub(super) fn in_a_row(self) -> bool {
        matches!(self, Self::Gore | Self::Merge | Self::Centre | Self::Guide)
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
    /// A mini roundabout.
    Mini,
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
    /// What it takes of the road, gaps between its bars too.
    pub block: Block,
    pub strokes: Vec<Vec<Point>>,
    /// Convex rings: an arrow's heads, a disc.
    pub heads: Vec<Vec<Point>>,
}

#[derive(Default)]
pub(super) struct Details {
    pub list: Vec<Detail>,
    /// The stretches of each of the network's runs, metres along it, that hold
    /// no lanes' lines. Its ends' cuts are not among them.
    pub gaps: Vec<Vec<[f64; 2]>>,
    /// Where lanes' lines give way to the list.
    pub taken: Taken,
    /// The islands in the middle of crossings.
    pub refuges: Vec<Area>,
}

/// Metres of a bar and of the gap between bars.
const BAR: f64 = 0.5;
/// Metres across a triangle where traffic gives way, from its base to its tip, and
/// between two (P-13's and MUTCD's yield line's are near these).
const TRIANGLE: [f64; 3] = [0.5, 0.6, 0.3];
/// Metres either side of a double line's middle that its lines are.
const DOUBLE: f64 = 0.45;
/// Metres a zebra takes along the road, the dots' and lines' from row to row.
const ZEBRA: f64 = 4.0;
const ROWS: f64 = 2.5;
/// Metres lines keep from a crossing, traffic's from where it stops to one, and
/// how near a junction (beyond its cut) a crossing takes the lines between.
const CLEAR: f64 = 0.6;
const STOP_BACK: f64 = 1.5;
const TO_JUNCTION: f64 = 12.0;
/// Metres across a road with an island in the middle of a crossing, at the least; and
/// left of it either side of the island, the metres this is across, and along.
const REFUGE_ROAD: f64 = 6.0;
const BESIDE_REFUGE: f64 = 2.5;
const REFUGE: [f64; 2] = [1.0, 2.0];
const REFUGE_LENGTH: f64 = 6.0;
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
/// How much longer arrows are, and further apart, on a fast road; km/h up to
/// which one is not.
const FAST: f64 = 1.5;
const SLOW: f64 = 60.0;
/// Metres an arrow keeps from its lane's edges, so that in a bend it does not
/// take its lines.
const ARROW_ROOM: f64 = 0.4;
/// Metres across the narrowest lane that has one.
const NARROWEST: f64 = 2.2;
/// Metres on, along plain road, that something must need the lanes for a run's
/// arrows to lead up to it.
const AHEAD: f64 = 150.0;

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
        "mini_roundabout" => return Some(Sign::Mini),
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

/// Whether traffic may go faster than `SLOW` where the limit is tagged.
pub(super) fn fast<'a>(get: impl Fn(&str) -> Option<&'a str>) -> bool {
    let Some(limit) = get("maxspeed").map(str::trim) else {
        return false;
    };
    if let Some((_, zone)) = limit.split_once(':') {
        return matches!(zone, "rural" | "trunk" | "motorway");
    }
    let (number, mile) = match limit.strip_suffix("mph") {
        Some(number) => (number, 1.609_344),
        None => (limit.strip_suffix("km/h").unwrap_or(limit), 1.0),
    };
    limit == "none"
        || number
            .trim()
            .parse::<f64>()
            .is_ok_and(|v| v * mile > SLOW && v.is_finite())
}

/// Metres right of the way's line that traffic along a run (`along`) or against
/// it takes, `s` metres along the run.
fn span_at(walk: &Walk, s: f64, along: bool) -> [f64; 2] {
    walk.section(s).span(lanes_of(walk.way), along)
}

/// Metres right of the way's line of the surface's edges, `s` metres along the
/// run.
fn surface_at(walk: &Walk, s: f64) -> [f64; 2] {
    walk.section(s).surface
}

/// Metres right of the way's line of each lane of one direction, from its
/// drivers' left, `s` metres along the run.
pub(super) fn lanes_at(walk: &Walk, s: f64, along: bool) -> Vec<[f64; 2]> {
    let lanes = lanes_of(walk.way);
    let section = walk.section(s);
    let of = lanes.of(along);
    let mut out: Vec<[f64; 2]> = of
        .map(|i| [section.lanes[i], section.lanes[i + 1]])
        .collect();
    if !along {
        out.reverse();
    }
    out
}

fn lanes_of(way: &Way) -> &Lanes {
    way.profile.lanes.as_ref().expect("a motor road's")
}

/// A run as it is walked: its points and the metres along it of each.
pub(super) struct Walk<'a> {
    ways: &'a [Way],
    network: &'a Network,
    index: usize,
    way: &'a Way,
    points: &'a [Point],
    along: Vec<f64>,
}

impl<'a> Walk<'a> {
    pub(super) fn new(ways: &'a [Way], network: &'a Network, index: usize) -> Self {
        let run = &network.runs[index];
        let way = &ways[run.way as usize];
        let points = &way.points[run.points.clone()];
        let mut along = vec![0.0];
        for p in points.windows(2) {
            along.push(along[along.len() - 1] + distance(p[0], p[1]) / way.scale);
        }
        Self {
            ways,
            network,
            index,
            way,
            points,
            along,
        }
    }

    fn section(&self, s: f64) -> Section {
        self.network.at(self.ways, self.index, s)
    }

    pub(super) fn way(&self) -> &'a Way {
        self.way
    }

    pub(super) fn length(&self) -> f64 {
        self.along[self.along.len() - 1]
    }

    /// The point `at` metres along, and the direction there.
    pub(super) fn at(&self, at: f64) -> (Point, Point) {
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
    pub(super) fn direction(&self, at: f64, reach: f64) -> Point {
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
pub(super) fn place(at: Point, direction: Point, scale: f64, [x, y]: [f64; 2]) -> Point {
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
/// are marked; `islands`, those of crossings with an island; `fast`, the ids of the
/// ways on which arrows are longer.
pub(super) fn build(
    ways: &[Way],
    network: &Network,
    signs: &FxHashMap<i64, Sign>,
    crossings: &FxHashMap<i64, Marking>,
    islands: &FxHashSet<i64>,
    fast: &FxHashSet<i64>,
) -> Details {
    let onward = Onward::new(ways);
    let rings: FxHashSet<i64> = ways
        .iter()
        .filter(|w| w.profile.roundabout)
        .flat_map(|w| w.nodes.iter().copied())
        .collect();
    let mut details = Details::default();
    // a node two runs share is drawn once
    let mut drawn: FxHashSet<i64> = FxHashSet::default();
    for (index, run) in network.runs.iter().enumerate() {
        let way = &ways[run.way as usize];
        let nodes = &way.nodes[run.points.clone()];
        let walk = Walk::new(ways, network, index);
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
            // in a junction it says not across which arm
            if (k == 0 && junction(0)) || (k == last && junction(1)) {
                continue;
            }
            let at = walk.along[k];
            let refuge = islands.contains(node).then(|| refuge(&walk, at)).flatten();
            let Some(Marking::Marked(kind, confidence)) =
                marked.map(|m| signalled(m, way.profile.rules))
            else {
                // an island is there, marked or not
                if let Some((area, _)) = refuge {
                    let half = REFUGE_LENGTH / 2.0 + CLEAR;
                    gaps.push([at - half, at + half]);
                    if drawn.insert(*node) {
                        details.refuges.push(area);
                    }
                }
                continue;
            };
            let mut half = match kind {
                Kind::Zebra => ZEBRA / 2.0,
                _ => (ROWS + kind.width()) / 2.0,
            };
            if refuge.is_some() {
                half = half.max(REFUGE_LENGTH / 2.0);
            }
            // two mapped where there is room for one
            if across.iter().any(|c| (c.at - at).abs() < c.half + half) {
                continue;
            }
            across.push(Crossing { at, half });
            if drawn.insert(*node) {
                let mut crossing = crossing(run.way, &walk, at, kind, confidence);
                if let Some((area, block)) = refuge {
                    super::paint::trim(&mut crossing, &[block], way.scale);
                    details.refuges.push(area);
                }
                details.list.push(crossing);
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
            // within a junction nothing stops
            if run.inner {
                continue;
            }
            let at = walk.along[k];
            let at_junction = [k == 0 && junction(0), k == last && junction(1)];
            if at_junction[0] || at_junction[1] {
                // signals at the junction itself are every arm's that leads to it
                let side = usize::from(at_junction[0]);
                if kind == Kind::Signal && [lanes.along, lanes.against][side] {
                    stops[side].push((at, kind, 1));
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
                && !run.inner
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
                let (kind, rows) = looks(kind, way.profile.rules);
                details.list.push(line(
                    run.way,
                    &walk,
                    place_at,
                    side == 0,
                    (kind, confidence),
                    rows,
                ));
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
            let last = if side == 0 {
                run.points.end - 1
            } else {
                run.points.start
            };
            if run.inner || turns.is_empty() || !onward.needs_lanes(run.way, last, side == 0) {
                continue;
            }
            let size = if fast.contains(&way.id) { FAST } else { 1.0 };
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
                let tail = tip - ARROW * size;
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
                    let middle = flip((tail + tip) / 2.0);
                    details
                        .list
                        .extend(arrows(run.way, &walk, middle, side == 0, size, turns));
                }
                tip -= ARROW_EVERY * size;
            }
        }
        for gap in &mut gaps {
            *gap = [gap[0].max(0.0), gap[1].min(length)];
        }
        gaps.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
        details.gaps.push(gaps);
    }
    details.list.extend(network.paint.iter().cloned());
    details.list.extend(discs(ways, signs));
    details.list.extend(junctions::guides(ways, network, signs));
    (details.list, details.taken) = super::paint::settle(ways, details.list);
    for detail in &mut details.list {
        if detail.kind == Kind::Triangles {
            let scale = ways[detail.way as usize].scale;
            kept_triangles(detail, scale);
        }
    }
    details
}

/// A crossing's marking as it looks where it is: at signals, where the marking is
/// not tagged, the country's.
fn signalled(marking: Marking, rules: &Rules) -> Marking {
    match marking {
        Marking::Marked(Kind::Zebra, 0) => Marking::Marked(
            match rules.signalled {
                Signalled::Zebra => Kind::Zebra,
                Signalled::Lines => Kind::Lines,
                Signalled::Studs => Kind::Dots,
            },
            0,
        ),
        other => other,
    }
}

/// A line traffic stops at as it looks where it is, and metres along the road of
/// each of its lines.
fn looks(kind: Kind, rules: &Rules) -> (Kind, &'static [f64]) {
    match (kind, rules.give_way) {
        (Kind::Signal, _) if rules.signal_dashed => (Kind::SignalDashed, &[0.0]),
        (Kind::GiveWay, GiveWay::Triangles) => (Kind::Triangles, &[0.0]),
        (Kind::GiveWay, GiveWay::Double) => (Kind::GiveWay, &[-DOUBLE, DOUBLE]),
        _ => (kind, &[0.0]),
    }
}

/// The triangles of a line where traffic gives way that are left where it was
/// cut: those whose base lies on what is left of its line.
fn kept_triangles(detail: &mut Detail, scale: f64) {
    let near = |p: Point| {
        detail.strokes.iter().any(|s| {
            let (a, b) = (s[0], s[s.len() - 1]);
            let length = distance(a, b);
            let along = [(b[0] - a[0]) / length, (b[1] - a[1]) / length];
            let t = (p[0] - a[0]) * along[0] + (p[1] - a[1]) * along[1];
            let off = ((p[0] - a[0]) * along[1] - (p[1] - a[1]) * along[0]).abs();
            length > 0.0
                && t > -0.05 * scale
                && t < length + 0.05 * scale
                && off < TRIANGLE[1] * scale
        })
    };
    let heads = std::mem::take(&mut detail.heads);
    detail.heads = heads
        .into_iter()
        .filter(|h| near([(h[0][0] + h[1][0]) / 2.0, (h[0][1] + h[1][1]) / 2.0]))
        .collect();
}

/// Where motor roads meet, and so where traffic may go on.
struct Onward<'a> {
    ways: &'a [Way],
    /// By node, the ways and which of their points it is.
    at: FxHashMap<i64, Vec<(u32, u32)>>,
}

impl<'a> Onward<'a> {
    fn new(ways: &'a [Way]) -> Self {
        let motor = || {
            ways.iter()
                .enumerate()
                .filter(|(_, w)| w.profile.lanes.is_some())
        };
        let mut uses: FxHashMap<i64, u32> = FxHashMap::default();
        for (_, way) in motor() {
            for &node in &way.nodes {
                *uses.entry(node).or_default() += 1;
            }
        }
        let mut at: FxHashMap<i64, Vec<(u32, u32)>> = FxHashMap::default();
        for (index, way) in motor() {
            for (k, node) in way.nodes.iter().enumerate() {
                if uses[node] > 1 {
                    let here = (index as u32, k as u32);
                    at.entry(*node).or_default().push(here);
                }
            }
        }
        Self { ways, at }
    }

    /// Where traffic that comes to the point `k` of a way may go on to: ways,
    /// their points and whether along them.
    fn ways_on(&self, way: u32, k: usize, along: bool) -> Vec<(u32, usize, bool)> {
        let alone = [(way, k as u32)];
        let node = self.ways[way as usize].nodes[k];
        let mut out = Vec::new();
        for &(other, j) in self.at.get(&node).map_or(&alone[..], |v| &v[..]) {
            let (road, j) = (&self.ways[other as usize], j as usize);
            let lanes = lanes_of(road);
            for (goes, ahead) in [
                (lanes.along && j + 1 < road.nodes.len(), true),
                (lanes.against && j > 0, false),
            ] {
                // not back where it came from
                if goes && (other, j, ahead) != (way, k, !along) {
                    out.push((other, j, ahead));
                }
            }
        }
        out
    }

    /// Whether what lies on from the point `k` of a way needs its lanes: where
    /// traffic may part, or the lanes or their turns are others. Where a way
    /// leads on alone, as one does that leaves a junction, nothing does.
    fn needs_lanes(&self, way: u32, k: usize, along: bool) -> bool {
        let of = |way: u32, along: bool| {
            let lanes = lanes_of(&self.ways[way as usize]);
            let span = lanes.span(along);
            let count = ((span[1] - span[0]) / lanes.width).round() as u32;
            (count, &lanes.turns[usize::from(!along)])
        };
        let (count, turns) = of(way, along);
        let merge = |t: &Turn| matches!(t, Turn::MergeLeft | Turn::MergeRight);
        if turns.iter().flatten().any(merge) {
            return true;
        }
        let (mut here, mut plain) = ((way, k, along), 0.0);
        // a step a point: a road goes round in fewer
        for _ in 0..1000 {
            // none, at the edge of what is known
            let [next] = self.ways_on(here.0, here.1, here.2)[..] else {
                return true;
            };
            let (lanes, theirs) = of(next.0, next.2);
            if lanes != count || (!theirs.is_empty() && theirs != turns) {
                return true;
            }
            let road = &self.ways[next.0 as usize];
            let to = if next.2 { next.1 + 1 } else { next.1 - 1 };
            if theirs.is_empty() {
                plain += distance(road.points[next.1], road.points[to]) / road.scale;
                if plain > AHEAD {
                    return false;
                }
            }
            here = (next.0, to, next.2);
        }
        false
    }
}

/// A crossing square over its road: its strokes run across, so a zebra's bars
/// lie along the road.
fn crossing(way: u32, walk: &Walk, at: f64, kind: Kind, confidence: u8) -> Detail {
    let edges = surface_at(walk, at);
    let (across, centre) = (edges[1] - edges[0], (edges[0] + edges[1]) / 2.0);
    let (point, direction) = (walk.at(at).0, walk.direction(at, 3.0));
    // bars at both edges, whole
    let width = match kind.dash() {
        Some((on, off)) => {
            let bars = ((across + off) / (on + off) + 1e-6).floor().max(1.0);
            bars * (on + off) - off
        }
        None => across,
    };
    let stroke = |y: f64| {
        [centre - width / 2.0, centre + width / 2.0]
            .map(|x| place(point, direction, walk.way.scale, [x, y]))
            .to_vec()
    };
    let strokes = match kind {
        Kind::Zebra => vec![stroke(0.0)],
        _ => vec![stroke(-ROWS / 2.0), stroke(ROWS / 2.0)],
    };
    let half = match kind {
        Kind::Zebra => ZEBRA / 2.0,
        _ => (ROWS + kind.width()) / 2.0,
    };
    let span = [centre - width / 2.0, centre + width / 2.0];
    Detail {
        way,
        kind,
        confidence,
        at: point,
        block: Block::new(point, direction, walk.way.scale, span, [-half, half]),
        strokes,
        heads: Vec::new(),
    }
}

/// The island in the middle of a crossing, between the directions, where the road has
/// the room; and what it takes of the crossing.
fn refuge(walk: &Walk, at: f64) -> Option<(Area, Block)> {
    let lanes = lanes_of(walk.way);
    let edges = surface_at(walk, at);
    let across = edges[1] - edges[0];
    // a one-way road's is beside it
    if across < REFUGE_ROAD || !lanes.along || !lanes.against {
        return None;
    }
    // between the directions
    let centre = span_at(walk, at, !lanes.left_hand)[0];
    let width = (across - 2.0 * BESIDE_REFUGE).clamp(REFUGE[0], REFUGE[1]);
    let (point, direction) = (walk.at(at).0, walk.direction(at, 3.0));
    let (half, long) = (width / 2.0, (REFUGE_LENGTH - width) / 2.0);
    let mut points = Vec::new();
    for (from, sign) in [(0.0, 1.0), (std::f64::consts::PI, -1.0)] {
        points.extend((0..=6).map(|k| {
            let (s, c) = (from + f64::from(k) / 6.0 * std::f64::consts::PI).sin_cos();
            let put = [centre + c * half, long * sign + s * half];
            place(point, direction, walk.way.scale, put)
        }));
    }
    let area = Area {
        kind: AreaKind::Island,
        paved: true,
        ring: ring(&points)?,
        holes: Vec::new(),
    };
    let span = [centre - half, centre + half];
    let ahead = [-REFUGE_LENGTH / 2.0, REFUGE_LENGTH / 2.0];
    let block = Block::new(point, direction, walk.way.scale, span, ahead);
    Some((area, block))
}

/// The middles of mini roundabouts.
fn discs(ways: &[Way], signs: &FxHashMap<i64, Sign>) -> Vec<Detail> {
    /// Metres of the radius, of the narrowest road's width, at the least and at the
    /// most.
    const OF_WIDTH: f64 = 0.25;
    const RADIUS: [f64; 2] = [0.75, 2.0];
    const AROUND: u32 = 24;
    // by node, the first road and the narrowest one's width
    let mut found: FxHashMap<i64, (u32, Point, f64)> = FxHashMap::default();
    let mut order: Vec<i64> = Vec::new();
    for (index, way) in ways.iter().enumerate() {
        if !way.profile.class.is_motor() {
            continue;
        }
        for (node, at) in way.nodes.iter().zip(&way.points) {
            if signs.get(node) == Some(&Sign::Mini) {
                let width = way.profile.width;
                let seen = found.entry(*node).or_insert_with(|| {
                    order.push(*node);
                    (index as u32, *at, width)
                });
                seen.2 = seen.2.min(width);
            }
        }
    }
    let disc = |node: &i64| {
        let (way, at, width) = found[node];
        let scale = ways[way as usize].scale;
        let radius = (width * OF_WIDTH).clamp(RADIUS[0], RADIUS[1]);
        let put = |k: u32| {
            let (s, c) = (f64::from(k) / f64::from(AROUND) * std::f64::consts::TAU).sin_cos();
            place(at, [0.0, -1.0], scale, [c * radius, s * radius])
        };
        let span = [-radius, radius];
        Detail {
            way,
            kind: Kind::Disc,
            confidence: 2,
            at,
            block: Block::new(at, [0.0, -1.0], scale, span, span),
            strokes: Vec::new(),
            heads: vec![(0..AROUND).map(put).collect()],
        }
    };
    order.iter().map(disc).collect()
}

/// A line across the lanes of one direction, or lines `rows` metres along the road
/// from `at`; triangles are whole and point at the traffic that comes.
fn line(
    way: u32,
    walk: &Walk,
    at: f64,
    along: bool,
    (kind, confidence): (Kind, u8),
    rows: &[f64],
) -> Detail {
    let (point, direction) = (walk.at(at).0, walk.direction(at, 3.0));
    let mut span = span_at(walk, at, along);
    let scale = walk.way.scale;
    let mut heads = Vec::new();
    if kind == Kind::Triangles {
        let [base, tall, gap] = TRIANGLE;
        let count = ((span[1] - span[0] + gap) / (base + gap) + 1e-6)
            .floor()
            .max(1.0);
        let width = count * (base + gap) - gap;
        let middle = (span[0] + span[1]) / 2.0;
        span = [middle - width / 2.0, middle + width / 2.0];
        // the base ahead of the tip, as traffic goes
        let y = if along { tall / 2.0 } else { -tall / 2.0 };
        for k in 0..count as u32 {
            let x = span[0] + f64::from(k) * (base + gap);
            heads.push(
                [[x, y], [x + base, y], [x + base / 2.0, -y]]
                    .map(|q| place(point, direction, scale, q))
                    .to_vec(),
            );
        }
    }
    let strokes = rows
        .iter()
        .map(|&y| {
            span.map(|x| place(point, direction, scale, [x, y]))
                .to_vec()
        })
        .collect();
    let half = kind.width() / 2.0;
    let ahead = [rows[0] - half, rows[rows.len() - 1] + half];
    Detail {
        way,
        kind,
        confidence,
        at: point,
        block: Block::new(point, direction, scale, span, ahead),
        strokes,
        heads,
    }
}

/// The arrows of one direction's lanes, side by side about `middle`, `size`
/// times the usual length.
fn arrows(
    way: u32,
    walk: &Walk,
    middle: f64,
    along: bool,
    size: f64,
    turns: &[Vec<Turn>],
) -> Vec<Detail> {
    let point = walk.at(middle).0;
    let mut direction = walk.direction(middle, ARROW * size / 2.0);
    let lanes = lanes_at(walk, middle, along);
    if lanes.len() != turns.len() {
        return Vec::new();
    }
    if !along {
        direction = [-direction[0], -direction[1]];
    }
    let mut out = Vec::new();
    for (turns, edges) in turns.iter().zip(lanes) {
        let (strokes, heads) = arrow(turns);
        let lane = edges[1] - edges[0];
        // none in a lane that is opening yet, or closing
        if strokes.is_empty() || lane < NARROWEST {
            continue;
        }
        let x = (edges[0] + edges[1]) / 2.0 * if along { 1.0 } else { -1.0 };
        // as wide as it is long, and within its lane
        let half = Kind::Arrow.width() / 2.0;
        let reach = strokes
            .iter()
            .flatten()
            .map(|q| q[0].abs() + half)
            .chain(heads.iter().flatten().map(|q| q[0].abs()))
            .fold(half, f64::max);
        let wide = size.min((lane / 2.0 - ARROW_ROOM).max(half) / reach);
        let fit = |q: [f64; 2]| [x + q[0] * wide, (q[1] - ARROW / 2.0) * size];
        let put = |q: [f64; 2]| place(point, direction, walk.way.scale, fit(q));
        let mut span = [x - half, x + half];
        for q in strokes.iter().flatten().map(|q| (fit(*q)[0], half)) {
            span = [span[0].min(q.0 - q.1), span[1].max(q.0 + q.1)];
        }
        for q in heads.iter().flatten().map(|q| fit(*q)[0]) {
            span = [span[0].min(q), span[1].max(q)];
        }
        let length = ARROW * size / 2.0;
        out.push(Detail {
            way,
            kind: Kind::Arrow,
            confidence: 2,
            at: point,
            block: Block::new(point, direction, walk.way.scale, span, [-length, length]),
            strokes: strokes
                .into_iter()
                .map(|s| s.into_iter().map(put).collect())
                .collect(),
            heads: heads.into_iter().map(|h| h.map(put).to_vec()).collect(),
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
                // under one that turns left, which it would cross
                let low = if turns.len() > 1 && !through {
                    1.9
                } else {
                    0.0
                };
                let turned: Vec<[f64; 2]> = (0..=6)
                    .map(|k| {
                        let (s, c) = (f64::from(k) / 6.0 * std::f64::consts::PI).sin_cos();
                        [-0.45 + 0.45 * c, 3.6 - low + 0.45 * s]
                    })
                    .collect();
                let mut stroke = vec![[0.0, 0.0]];
                stroke.extend(turned);
                stroke.push([-0.9, 3.0 - low]);
                strokes.push(stroke);
                heads.push([[-1.25, 3.1 - low], [-0.55, 3.1 - low], [-0.9, 2.0 - low]]);
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
        // the line of triangles is not painted
        let strokes = if self.kind == Kind::Triangles {
            &[][..]
        } else {
            &self.strokes[..]
        };
        for stroke in strokes {
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
        out.extend(self.heads.iter().cloned());
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
            &FxHashSet::default(),
            &FxHashSet::default(),
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
    fn where_traffic_keeps_left_it_stops_and_turns_on_the_left() {
        use super::super::country;
        use super::super::profile::profile_in;
        let tags = [
            ("highway", "primary"),
            ("lanes", "4"),
            ("lanes:forward", "2"),
            ("turn:lanes:forward", "through|right"),
            ("turn:lanes:backward", "left|through"),
        ];
        for code in ["PL", "GB"] {
            let rules = country::rules(code);
            let make = |t: &[(&str, &'static str)], n: &[(i64, f64, f64)]| Way {
                profile: profile_in(|k| t.iter().find(|x| x.0 == k).map(|x| x.1), rules).unwrap(),
                ..way(t, n)
            };
            let ways = [
                make(
                    &tags,
                    &[
                        (1, -100.0, 0.0),
                        (2, -20.0, 0.0),
                        (3, -14.0, 0.0),
                        (4, 0.0, 0.0),
                    ],
                ),
                make(&tags, &[(4, 0.0, 0.0), (5, 100.0, 0.0)]),
                make(PRIMARY, &[(6, 0.0, -100.0), (4, 0.0, 0.0), (7, 0.0, 100.0)]),
            ];
            let left = rules.keeps_left();
            // traffic going east stops across the north half, keeping left
            let stop = details(&ways, &[(3, Sign::Line(Kind::Stop, Facing::Along))]);
            let line = &of(&stop, Kind::Stop)[0].strokes[0];
            let half = if left { [-7.0, 0.0] } else { [0.0, 7.0] };
            assert_eq!(line, &vec![[-14.0, half[0]], [-14.0, half[1]]], "{code}");
            // a give way is two lines in Britain, triangles in Poland
            let give = details(&ways, &[(3, Sign::Line(Kind::GiveWay, Facing::Along))]);
            if left {
                let lines = &of(&give, Kind::GiveWay)[0].strokes;
                assert_eq!(
                    lines.iter().map(|l| l[0][0]).collect::<Vec<_>>(),
                    [-14.45, -13.55]
                );
            } else {
                let tri = of(&give, Kind::Triangles)[0];
                assert_eq!(tri.heads.len(), 9);
                // tips to the west, at the traffic that comes
                assert!(
                    tri.heads
                        .iter()
                        .all(|h| h[2][0] < h[0][0] && h[0][0] == h[1][0])
                );
                assert!(tri.heads.iter().all(|h| (0.0..7.0).contains(&h[2][1])));
            }
            // the arrows of traffic going east, from its drivers' left
            let d = details(&ways, &[]);
            let east: Vec<&Detail> = of(&d, Kind::Arrow)
                .into_iter()
                .filter(|a| a.way == 0 && a.heads[0][2][0] > a.strokes[0][0][0])
                .collect();
            assert!(!east.is_empty());
            for a in east {
                let (tail, tip) = (a.strokes[0][0], a.heads[0][2]);
                let turns = (tip[1] - tail[1]).abs() > 0.5;
                // the right turn's is the inner lane where traffic keeps left
                let lane = match (left, turns) {
                    (false, false) => 1.75,
                    (false, true) => 5.25,
                    (true, false) => -5.25,
                    (true, true) => -1.75,
                };
                assert!((tail[1] - lane).abs() < 0.3, "{code} {a:?}");
                // a right turn points right, whichever side traffic keeps to
                assert!(!turns || tip[1] > tail[1]);
            }
        }
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
        let across = first.strokes[0][0][1];
        assert_eq!(first.strokes[0][0][0], -6.5 - 6.0 - 5.0);
        assert_eq!(first.heads[0][2], [-6.5 - 6.0, across]);
        // the lanes go on as the road beyond has them, from here on
        assert!((-1.75..-0.4).contains(&across), "{across}");
        // the left turn points north, which is up
        assert!(first.heads[1][2][1] < across - 0.5);
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
    fn arrows_are_longer_on_a_fast_road_and_keep_to_their_lanes() {
        let of = |limit: &'static str| fast(|k| (k == "maxspeed").then_some(limit));
        for limit in ["70", "90 km/h", "40 mph", "PL:rural", "none"] {
            assert!(of(limit), "{limit}");
        }
        for limit in ["60", "50 km/h", "35 mph", "PL:urban", "walk", "inf", ""] {
            assert!(!of(limit), "{limit}");
        }
        assert!(!fast(|_| None));
        let tags = [
            ("highway", "secondary"),
            ("oneway", "yes"),
            ("lanes", "2"),
            ("width", "5.4"),
            ("turn:lanes", "left|reverse"),
        ];
        let ways = junction(&tags);
        let network = network::build(&ways);
        let arrows = |fast: &[i64]| {
            let fast = fast.iter().copied().collect();
            let d = build(
                &ways,
                &network,
                &FxHashMap::default(),
                &FxHashMap::default(),
                &FxHashSet::default(),
                &fast,
            );
            let tails: Vec<f64> = d.list.iter().map(|d| d.strokes[0][0][0]).collect();
            for d in &d.list {
                // the left lane's 2.7 m, or the right's
                let points = d.strokes.iter().flatten().chain(d.heads.iter().flatten());
                let (low, high) =
                    points.fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(p[1]), b.max(p[1])));
                let lane = if d.strokes[0][0][1] < 0.0 { -2.7 } else { 0.0 };
                assert!(low > lane + 0.35 && high < lane + 2.35, "{low} {high}");
            }
            tails
        };
        let (slow, quick) = (arrows(&[]), arrows(&[1]));
        assert_eq!((slow.len(), quick.len()), (6, 4));
        assert!((slow[0] - quick[0] - 2.5).abs() < 1e-9);
        assert!((quick[0] - quick[2] - 45.0).abs() < 1e-9);
    }

    #[test]
    fn no_arrows_lead_to_nothing() {
        let tags = |turns: &'static str| {
            [
                ("highway", "primary"),
                ("oneway", "yes"),
                ("lanes", "2"),
                ("turn:lanes", turns),
            ]
        };
        let plain = [("highway", "primary"), ("oneway", "yes"), ("lanes", "2")];
        // past a junction, and on along a plain road
        let on = |turns, then: &[(&str, &'static str)], length: f64| {
            let mut ways = vec![
                way(&tags(turns), &[(1, 0.0, 0.0), (2, 60.0, 0.0)]),
                way(then, &[(2, 60.0, 0.0), (3, 60.0 + length, 0.0)]),
                way(then, &[(3, 60.0 + length, 0.0), (4, 900.0, 0.0)]),
            ];
            ways.extend([
                way(PRIMARY, &[(6, 900.0, -90.0), (4, 900.0, 0.0)]),
                way(PRIMARY, &[(4, 900.0, 0.0), (7, 900.0, 90.0)]),
            ]);
            // one that only comes in
            ways.push(way(&plain, &[(8, 10.0, 60.0), (2, 60.0, 0.0)]));
            let d = details(&ways, &[]);
            d.list.iter().filter(|d| d.kind == Kind::Arrow).count()
        };
        assert_eq!(on("left|through", &plain, 200.0), 0);
        // a lane that ends says so
        assert!(on("through|merge_to_left", &plain, 200.0) > 0);
        // on ones alike until there is a junction, or where lanes change
        assert!(on("left|through", &tags("left|through"), 200.0) > 2);
        let three = [("highway", "primary"), ("oneway", "yes"), ("lanes", "3")];
        assert!(on("left|through", &three, 200.0) > 0);
        assert!(on("left|through", &tags("through|through"), 200.0) > 0);
        // and where the junction is near
        let near = [
            way(&tags("left|through"), &[(1, 0.0, 0.0), (2, 60.0, 0.0)]),
            way(&plain, &[(2, 60.0, 0.0), (4, 100.0, 0.0)]),
            way(
                PRIMARY,
                &[(6, 100.0, -90.0), (4, 100.0, 0.0), (7, 100.0, 90.0)],
            ),
        ];
        assert_eq!(of(&details(&near, &[]), Kind::Arrow).len(), 2 * 2);
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
        // a turn back stays under the turn left beside it
        let (strokes, heads) = arrow(&[Reverse, Left]);
        let top = |p: &[[f64; 2]]| p.iter().map(|p| p[1]).fold(f64::MIN, f64::max);
        let low = |p: &[[f64; 2]]| p.iter().map(|p| p[1]).fold(f64::MAX, f64::min);
        assert!(top(&strokes[0]) + 0.3 < low(&heads[1]) && low(&heads[0]) >= 0.0);
    }
}
