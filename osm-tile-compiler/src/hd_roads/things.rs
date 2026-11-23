//! What stands beside roads: fences, hedges and walls, and lamps, signs, signals
//! and other street furniture.

use super::details::{self, Details};
use super::furniture;
use super::geometry::{self, Rect};
use super::network::{Point, Way, distance};
use super::profile::{self, Class, Level};
use super::structures::Index;
use i_overlay::i_shape::base::data::Shapes;
use rustc_hash::{FxHashMap, FxHashSet};

/// A fence, hedge or wall, and metres it stands high.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Fence {
    pub kind: &'static str,
    pub height: f64,
}

/// Metres high the lowest that is drawn is.
const LOWEST: f64 = 0.2;

/// The fence a way with these tags is, if it stands on the ground in the open.
pub(super) fn fence<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Fence> {
    let (kind, usual, highest) = match get("barrier")? {
        "fence" => ("fence", 1.5, 8.0),
        "hedge" => ("hedge", 1.5, 8.0),
        "wall" => ("wall", 1.8, 8.0),
        "retaining_wall" => ("retaining_wall", 1.5, 8.0),
        "city_wall" => ("city_wall", 5.0, 15.0),
        "handrail" => ("handrail", 1.0, 1.5),
        _ => return None,
    };
    // those along roads, which are lines of their own
    let of_roads = furniture::barrier(&get).is_some() || get("wall") == Some("jersey_barrier");
    if of_roads || !in_the_open(&get) || said(&get, "bridge") || said(&get, "building") {
        return None;
    }
    if get("layer").is_some_and(|v| v.trim() != "0") {
        return None;
    }
    let height = match get("height").and_then(profile::metres) {
        Some(height) if height < LOWEST => return None,
        Some(height) if height <= highest => height,
        _ => usual,
    };
    Some(Fence { kind, height })
}

fn said<'a>(get: impl Fn(&str) -> Option<&'a str>, key: &str) -> bool {
    get(key).is_some_and(|v| v != "no")
}

fn in_the_open<'a>(get: impl Fn(&str) -> Option<&'a str>) -> bool {
    let within = |v: &str| matches!(v, "underground" | "indoor" | "roof" | "air" | "wall");
    !get("location").is_some_and(within)
        && !["indoor", "covered", "tunnel"]
            .iter()
            .any(|key| said(&get, key))
        && get("level").is_none_or(|v| v.trim() == "0")
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Kind {
    Lamp,
    Signal,
    Stop,
    GiveWay,
    Sign,
    Bollard,
    LiftGate,
    Gate,
    BusStop,
    Shelter,
    Mirror,
    Hydrant,
    Bench,
    Bin,
    Cabinet,
    Column,
}

impl Kind {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Lamp => "lamp",
            Self::Signal => "signal",
            Self::Stop => "stop",
            Self::GiveWay => "give_way",
            Self::Sign => "sign",
            Self::Bollard => "bollard",
            Self::LiftGate => "lift_gate",
            Self::Gate => "gate",
            Self::BusStop => "bus_stop",
            Self::Shelter => "shelter",
            Self::Mirror => "mirror",
            Self::Hydrant => "hydrant",
            Self::Bench => "bench",
            Self::Bin => "bin",
            Self::Cabinet => "cabinet",
            Self::Column => "column",
        }
    }

    /// Whether, within a bridge's outline, it is the bridge's.
    fn of_bridges(self) -> bool {
        matches!(self, Self::Lamp | Self::Sign)
    }
}

/// Which way a node says it faces.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Faces {
    /// Degrees clockwise from north.
    Degrees(f64),
    /// Traffic along its way, or against it.
    Along,
    Against,
    Unsaid,
}

/// A node to be stood beside its road.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Mapped {
    kind: Kind,
    faces: Faces,
    height: Option<f64>,
    layer: Option<i8>,
    /// A lamp on a mast that does not lean out, and a stop with a shelter.
    straight: bool,
    shelter: bool,
}

/// What a node with these tags is, if it stands in the open. Lines to stop at
/// have their signs where they are painted ([`build`]).
pub(super) fn mapped<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Mapped> {
    let sign = || {
        // a sign that is a stop's is drawn with its line
        let line = matches!(get("highway"), Some("stop" | "give_way"));
        (get("traffic_sign").is_some_and(|v| v != "no") && !line).then_some(Kind::Sign)
    };
    let pillar = || get("fire_hydrant:type") == Some("pillar");
    let kind = match (get("highway"), get("barrier"), get("amenity")) {
        (Some("street_lamp"), ..) => Kind::Lamp,
        (Some("bus_stop"), ..) => Kind::BusStop,
        (Some("traffic_mirror"), ..) => Kind::Mirror,
        (_, Some("bollard"), _) => Kind::Bollard,
        (_, Some("lift_gate"), _) => Kind::LiftGate,
        (_, Some("gate" | "swing_gate" | "sliding_gate"), _) => Kind::Gate,
        (_, _, Some("bench")) => Kind::Bench,
        (_, _, Some("waste_basket")) => Kind::Bin,
        _ if get("emergency") == Some("fire_hydrant") => pillar().then_some(Kind::Hydrant)?,
        _ if get("man_made") == Some("street_cabinet") => Kind::Cabinet,
        _ if get("advertising") == Some("column") => Kind::Column,
        _ => sign()?,
    };
    if !in_the_open(&get) {
        return None;
    }
    let height = get("height").and_then(profile::metres);
    if kind == Kind::Lamp {
        // what hangs, or lies, has no mast
        let hung = |v: &str| {
            matches!(
                v,
                "wall"
                    | "wall_mounted"
                    | "ground"
                    | "bench"
                    | "bollard"
                    | "suspended"
                    | "ceiling"
                    | "wire"
                    | "roof"
                    | "building"
                    | "utility_pole"
                    | "power_pole"
            )
        };
        let mounted = get("lamp_mount").is_some_and(hung);
        let supported = get("support").is_some_and(|v| v != "ground" && hung(v));
        if mounted || supported || get("height").is_some_and(|v| v.trim() == "0") {
            return None;
        }
        if height.is_some_and(|h| h < 2.0) {
            return None;
        }
    }
    let keys = [
        "traffic_sign:direction",
        "direction",
        "traffic_signals:direction",
    ];
    let faces = match keys.iter().find_map(|key| get(key)).map(str::trim) {
        Some("forward") => Faces::Along,
        Some("backward") => Faces::Against,
        Some(text) => degrees(text).map_or(Faces::Unsaid, Faces::Degrees),
        None => Faces::Unsaid,
    };
    Some(Mapped {
        kind,
        faces,
        height: height.filter(|h| *h <= 20.0),
        layer: get("layer").and_then(|v| v.trim().parse().ok()),
        straight: get("lamp_mount").is_some_and(|v| v.starts_with("straight")),
        shelter: get("shelter") == Some("yes"),
    })
}

/// A direction in degrees or of the compass.
fn degrees(text: &str) -> Option<f64> {
    let winds = [
        "N", "NNE", "NE", "ENE", "E", "ESE", "SE", "SSE", "S", "SSW", "SW", "WSW", "W", "WNW",
        "NW", "NNW",
    ];
    if let Some(wind) = winds.iter().position(|w| *w == text) {
        return Some(wind as f64 * 22.5);
    }
    let number = text.trim_end_matches('°').parse::<f64>().ok()?;
    (0.0..=360.0).contains(&number).then_some(number)
}

/// A thing in its place.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Thing {
    pub kind: Kind,
    /// Where it stands: the first of these that no carriageway takes.
    pub at: Vec<Point>,
    /// Degrees clockwise from north it faces, or a gate's bar leads.
    pub turn: Option<u32>,
    /// Metres: a lamp's height, or what a gate closes.
    pub size: Option<f64>,
    /// The group and the piece of it, of a bridge, that it stands on.
    pub on: Option<(u32, u32)>,
}

/// A line on the ground, less what runs along bridges.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Line {
    pub fence: Fence,
    pub points: Vec<Point>,
    pub scale: f64,
}

/// Part of a line: from a place on it to another, each the number of a segment
/// and how far along that, 0 to 1.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Run {
    pub line: u32,
    pub from: (u32, f64),
    pub to: (u32, f64),
}

#[derive(Default)]
pub(super) struct Things {
    pub points: Vec<Thing>,
    pub lines: Vec<Line>,
}

/// What of the things in a tile no carriageway takes.
#[derive(Debug, Default)]
pub(super) struct Kept {
    /// Which, and where.
    pub points: Vec<(u32, Point)>,
    pub lines: Vec<Run>,
}

/// Metres from a road's edge within which a thing is taken to be the road's, and
/// a path's.
const BESIDE: f64 = 8.0;
/// Metres from one that a lamp leans over it.
const LEANS: f64 = 5.0;
/// Metres within a carriageway, whose width is mostly the usual one, a thing is
/// taken to stand beside it, and how far out it then stands.
const WITHIN: f64 = 1.5;
const OUT: f64 = 0.4;
/// Metres outside its road's edge a sign stands, and a post of a gate.
const SIGN_OUT: f64 = 0.8;
const GATE_OUT: f64 = 0.2;
/// The widest a gate closes.
const WIDEST: f64 = 14.0;
/// Metres beyond a bridge's edge something is taken to stand on it, how far in
/// from it, and how far in from it that then is.
const BRIDGE_OUT: f64 = 1.2;
const BRIDGE_IN: f64 = 2.5;
const ON_EDGE: f64 = 0.15;
/// A lamp's height, beside a road and elsewhere.
const LAMP: f64 = 8.0;
const LOW_LAMP: f64 = 4.5;
/// Metres a shelter's middle stands behind its stop.
const BEHIND: f64 = 1.3;
/// Degrees a turn is rounded to.
const TURN: f64 = 5.0;
/// World units of a cell ways are looked up by.
const CELL: f64 = 1024.0;

/// By cell, segments: way and place in it.
struct Near<'a> {
    ways: &'a [Way],
    at: FxHashMap<(i64, i64), Vec<(u32, u32)>>,
}

/// A road or path a point lies by.
#[derive(Clone, Copy)]
struct Beside {
    way: u32,
    /// Its direction there.
    along: Point,
    /// Metres right of its line, and outside its nearer edge (within it, below
    /// nothing); whether that is the right one.
    out: f64,
    right: bool,
}

fn cell(v: f64) -> i64 {
    (v / CELL).floor() as i64
}

fn scaled(p: Point, v: Point, by: f64) -> Point {
    [p[0] + v[0] * by, p[1] + v[1] * by]
}

/// What lies right of a direction, y growing southwards.
fn rightwards(d: Point) -> Point {
    [-d[1], d[0]]
}

fn back(d: Point) -> Point {
    [-d[0], -d[1]]
}

fn turn(d: Point) -> u32 {
    let degrees = d[0].atan2(-d[1]).to_degrees();
    ((degrees / TURN).round() * TURN).rem_euclid(360.0) as u32
}

impl<'a> Near<'a> {
    fn new(ways: &'a [Way]) -> Self {
        let mut at: FxHashMap<(i64, i64), Vec<(u32, u32)>> = FxHashMap::default();
        for (i, way) in ways.iter().enumerate() {
            let p = &way.profile;
            if p.level != Level::Ground || p.class.is_rail() {
                continue;
            }
            let reach = (p.width / 2.0 + p.centre.abs() + BESIDE) * way.scale;
            for (k, s) in way.points.windows(2).enumerate() {
                let low = |v: usize| cell(s[0][v].min(s[1][v]) - reach);
                let high = |v: usize| cell(s[0][v].max(s[1][v]) + reach);
                for x in low(0)..=high(0) {
                    for y in low(1)..=high(1) {
                        at.entry((x, y)).or_default().push((i as u32, k as u32));
                    }
                }
            }
        }
        Self { ways, at }
    }

    /// The way of those wanted whose surface `p` is nearest.
    fn beside(&self, p: Point, wanted: impl Fn(Class) -> bool) -> Option<Beside> {
        let mut best: Option<Beside> = None;
        for &(i, k) in self.at.get(&(cell(p[0]), cell(p[1])))? {
            let way = &self.ways[i as usize];
            if !wanted(way.profile.class) {
                continue;
            }
            let (a, b) = (way.points[k as usize], way.points[k as usize + 1]);
            let length = distance(a, b).max(f64::MIN_POSITIVE);
            let d = [(b[0] - a[0]) / length, (b[1] - a[1]) / length];
            let q = [p[0] - a[0], p[1] - a[1]];
            let t = q[0] * d[0] + q[1] * d[1];
            // past its end by what it lies outside of it
            let past = (-t).max(t - length).max(0.0) / way.scale;
            let right = (q[1] * d[0] - q[0] * d[1]) / way.scale;
            let off = right - way.profile.centre;
            let out = off.abs() - way.profile.width / 2.0;
            let out = if past > 0.0 {
                out.max(0.0).hypot(past)
            } else {
                out
            };
            if out <= BESIDE && best.is_none_or(|b| out < b.out) {
                best = Some(Beside {
                    way: i,
                    along: d,
                    out,
                    right: off >= 0.0,
                });
            }
        }
        best
    }

    fn scale(&self, found: &Beside) -> f64 {
        self.ways[found.way as usize].scale
    }
}

impl Beside {
    /// Towards the road.
    fn inward(&self) -> Point {
        let right = rightwards(self.along);
        if self.right { back(right) } else { right }
    }

    /// The direction traffic goes on the side it lies on.
    fn traffic(&self, way: &Way) -> Option<Point> {
        let lanes = way.profile.lanes.as_ref()?;
        Some(match (lanes.along, lanes.against) {
            (true, true) if self.right != lanes.left_hand => self.along,
            (true, true) | (false, _) => back(self.along),
            _ => self.along,
        })
    }
}

/// The things of the nodes `mapped`, of the lines painted where traffic stops, and
/// the `lines`.
pub(super) fn build(
    ways: &[Way],
    all: &[Way],
    index: &Index,
    details: &Details,
    mapped: &[(i64, Point, f64, Mapped)],
    lines: &[(Vec<Point>, f64, Fence)],
) -> Things {
    let near = Near::new(ways);
    let wanted: FxHashSet<i64> = mapped.iter().map(|m| m.0).collect();
    // where on a motor road of the ground nodes lie
    let mut on_way: FxHashMap<i64, (u32, u32)> = FxHashMap::default();
    for (i, way) in ways.iter().enumerate() {
        if !way.profile.class.is_motor() || way.profile.level != Level::Ground {
            continue;
        }
        for (k, node) in way.nodes.iter().enumerate() {
            if wanted.contains(node) {
                on_way.entry(*node).or_insert((i as u32, k as u32));
            }
        }
    }
    let mut out = Things::default();
    for (node, p, scale, mapped) in mapped {
        let on = on_way.get(node).copied();
        stand(&near, all, index, *p, *scale, mapped, on, &mut out.points);
    }
    signed(all, index, details, &mut out.points);
    for (points, scale, fence) in lines {
        for points in off_bridges(all, index, points, *scale) {
            out.lines.push(Line {
                fence: *fence,
                points,
                scale: *scale,
            });
        }
    }
    out
}

#[allow(clippy::too_many_arguments)]
fn stand(
    near: &Near,
    all: &[Way],
    index: &Index,
    p: Point,
    scale: f64,
    mapped: &Mapped,
    on_way: Option<(u32, u32)>,
    out: &mut Vec<Thing>,
) {
    let kind = mapped.kind;
    let mut thing = Thing {
        kind,
        at: vec![p],
        turn: match mapped.faces {
            Faces::Degrees(d) => Some(((d / TURN).round() * TURN).rem_euclid(360.0) as u32),
            _ => None,
        },
        size: None,
        on: None,
    };
    if matches!(kind, Kind::Gate | Kind::LiftGate) {
        if let Some((way, k)) = on_way
            && let Some(gate) = gate(&near.ways[way as usize], k as usize, thing)
        {
            out.push(gate);
        }
        return;
    }
    if mapped.layer.is_some_and(|l| l < 0) {
        return;
    }
    if kind.of_bridges() && on_way.is_none() && mapped.layer != Some(0) {
        match index.deck(all, p, BRIDGE_OUT) {
            Some(deck) => {
                let piece = &all[deck.way as usize];
                let class = piece.profile.class;
                // in its middle, on it or under it: nothing tells
                if deck.found.inside > BRIDGE_IN && class.is_motor() {
                    return;
                }
                let beside = Beside {
                    way: deck.way,
                    along: deck.along,
                    out: -deck.found.inside,
                    right: deck.right >= piece.profile.centre,
                };
                let inward = beside.inward();
                let by = (ON_EDGE - deck.found.inside).max(0.0);
                thing.at = vec![scaled(p, inward, by * scale)];
                thing.on = Some((deck.found.group, deck.found.piece));
                if kind == Kind::Lamp {
                    if class.is_motor() && !mapped.straight {
                        thing.turn = Some(thing.turn.unwrap_or(turn(inward)));
                    }
                    let usual = if class.is_motor() { LAMP } else { LOW_LAMP };
                    thing.size = Some(mapped.height.unwrap_or(usual));
                } else {
                    let traffic = beside.traffic(piece).map(|d| turn(back(d)));
                    thing.turn = thing.turn.or(traffic);
                    if thing.turn.is_none() {
                        return;
                    }
                }
                out.push(thing);
                return;
            }
            // on something that is not drawn
            None if mapped.layer.is_some_and(|l| l > 0) => return,
            None => {}
        }
    } else if mapped.layer.is_some_and(|l| l > 0) {
        return;
    }
    let road = near.beside(p, Class::is_motor);
    let any = || near.beside(p, |class| !class.is_rail());
    if let Some(road) = road.filter(|r| (-WITHIN..0.0).contains(&r.out)) {
        let by = (OUT - road.out) * near.scale(&road);
        thing.at.push(scaled(p, back(road.inward()), by));
    }
    match kind {
        Kind::Lamp => {
            let road = road.filter(|r| r.out <= LEANS);
            if let Some(road) = &road
                && !mapped.straight
            {
                thing.turn = thing.turn.or(Some(turn(road.inward())));
            } else if mapped.straight {
                thing.turn = None;
            }
            let usual = if road.is_some() { LAMP } else { LOW_LAMP };
            thing.size = Some(mapped.height.unwrap_or(usual));
        }
        Kind::Sign => {
            // on its way it stands beside those it is for, on the side they keep to
            if let Some((way, k)) = on_way {
                let way = &near.ways[way as usize];
                let Some(lanes) = &way.profile.lanes else {
                    return;
                };
                let along = match (mapped.faces, lanes.along, lanes.against) {
                    (Faces::Along, ..) => true,
                    (Faces::Against, ..) => false,
                    (_, along, against) if along != against => along,
                    _ => return,
                };
                let d = direction(way, k as usize);
                let [lo, hi] = way.profile.surface();
                let x = if along != lanes.left_hand {
                    hi + SIGN_OUT
                } else {
                    lo - SIGN_OUT
                };
                let traffic = if along { d } else { back(d) };
                thing.at = vec![scaled(p, rightwards(d), x * way.scale)];
                thing.turn = thing.turn.or(Some(turn(back(traffic))));
            } else if thing.turn.is_none() {
                let Some(road) = road else { return };
                let way = &near.ways[road.way as usize];
                let traffic = match mapped.faces {
                    Faces::Along => Some(road.along),
                    Faces::Against => Some(back(road.along)),
                    _ => road.traffic(way),
                };
                thing.turn = traffic.map(|d| turn(back(d)));
                if thing.turn.is_none() {
                    return;
                }
            }
        }
        Kind::BusStop => {
            if let Some(road) = &road {
                let inward = road.inward();
                thing.turn = thing.turn.or(Some(turn(inward)));
                if mapped.shelter {
                    let behind = |at: &Point| scaled(*at, back(inward), BEHIND * scale);
                    out.push(Thing {
                        kind: Kind::Shelter,
                        at: thing.at.iter().map(behind).collect(),
                        turn: Some(turn(inward)),
                        ..thing.clone()
                    });
                }
            }
        }
        Kind::Mirror => {
            thing.turn = thing.turn.or(road.map(|r| turn(r.inward())));
            if thing.turn.is_none() {
                return;
            }
        }
        Kind::Bench | Kind::Cabinet => {
            thing.turn = thing.turn.or_else(|| any().map(|r| turn(r.inward())));
        }
        _ => thing.turn = None,
    }
    out.push(thing);
}

/// The way's direction at one of its points.
fn direction(way: &Way, k: usize) -> Point {
    let (a, b) = (
        way.points[k.saturating_sub(1)],
        way.points[(k + 1).min(way.points.len() - 1)],
    );
    let length = distance(a, b).max(f64::MIN_POSITIVE);
    [(b[0] - a[0]) / length, (b[1] - a[1]) / length]
}

/// A gate across a road: a post on one side, and how far and which way it closes.
fn gate(way: &Way, k: usize, thing: Thing) -> Option<Thing> {
    let [lo, hi] = way.profile.surface();
    let right = rightwards(direction(way, k));
    // a bar's post stands right of the way, a gate's hinge left
    let (x, to) = if thing.kind == Kind::LiftGate {
        (hi + GATE_OUT, back(right))
    } else {
        (lo - GATE_OUT, right)
    };
    let size = hi - lo + 2.0 * GATE_OUT;
    (size <= WIDEST).then(|| Thing {
        at: vec![scaled(way.points[k], right, x * way.scale)],
        turn: Some(turn(to)),
        size: Some(size),
        ..thing
    })
}

/// The signs and signals of the lines painted where traffic stops: a stop's and
/// a give way's where they are tagged, a signal's also at the arms of the
/// junction that has them.
fn signed(all: &[Way], index: &Index, details: &Details, out: &mut Vec<Thing>) {
    for d in &details.list {
        let kind = match d.kind {
            details::Kind::Signal | details::Kind::SignalDashed => Kind::Signal,
            details::Kind::Stop if d.confidence == 2 => Kind::Stop,
            details::Kind::GiveWay | details::Kind::Triangles if d.confidence == 2 => Kind::GiveWay,
            _ => continue,
        };
        let way = &all[d.way as usize];
        let (Some(lanes), Some([a, b])) = (
            &way.profile.lanes,
            d.strokes
                .first()
                .and_then(|s| <[Point; 2]>::try_from(&s[..]).ok()),
        ) else {
            continue;
        };
        let across = distance(a, b);
        if across <= 0.0 {
            continue;
        }
        let right = [(b[0] - a[0]) / across, (b[1] - a[1]) / across];
        let ahead = [right[1], -right[0]];
        let middle = [(a[0] + b[0]) / 2.0 - d.at[0], (a[1] + b[1]) / 2.0 - d.at[1]];
        let middle = (middle[0] * right[0] + middle[1] * right[1]) / way.scale;
        let along = match (lanes.along, lanes.against) {
            (true, true) => {
                let off = |along: bool| {
                    let [lo, hi] = lanes.span(along);
                    (middle - (lo + hi) / 2.0).abs()
                };
                off(true) <= off(false)
            }
            (along, _) => along,
        };
        let [lo, hi] = way.profile.surface();
        // on the side its traffic keeps to
        let on_right = along != lanes.left_hand;
        let at = |out: f64| {
            let x = if on_right { hi + out } else { lo - out };
            scaled(d.at, right, x * way.scale)
        };
        let (at, on) = match way.profile.level {
            Level::Ground => (vec![at(SIGN_OUT), at(2.0 * SIGN_OUT)], None),
            Level::Bridge(_) => match index.on(all, d.way, d.at) {
                Some(on) => (vec![at(-ON_EDGE)], Some(on)),
                None => continue,
            },
            Level::Tunnel(_) => continue,
        };
        out.push(Thing {
            kind,
            at,
            turn: Some(turn(if along { back(ahead) } else { ahead })),
            size: None,
            on,
        });
    }
}

/// Metres of a line that stand on one thing or another, and beyond a bridge's
/// edge it is the bridge's.
const STEP: f64 = 5.0;
const ALONG: f64 = 1.5;

/// The line less what runs along bridges: their parapets stand for it.
fn off_bridges(all: &[Way], index: &Index, points: &[Point], scale: f64) -> Vec<Vec<Point>> {
    let mut out: Vec<Vec<Point>> = Vec::new();
    let mut open = false;
    for p in points.windows(2) {
        let parts = (distance(p[0], p[1]) / (STEP * scale)).ceil().max(1.0) as u32;
        let at = |k: u32| {
            let t = f64::from(k) / f64::from(parts);
            [
                p[0][0] + (p[1][0] - p[0][0]) * t,
                p[0][1] + (p[1][1] - p[0][1]) * t,
            ]
        };
        for k in 0..parts {
            let (a, b) = (at(k), at(k + 1));
            if index.along(all, a, b, ALONG).is_some_and(|f| !f.approach) {
                open = false;
                continue;
            }
            match out.last_mut() {
                // no more points than the line has
                Some(line) if open => {
                    if k > 0 {
                        line.pop();
                    }
                    line.push(b);
                }
                _ => out.push(vec![a, b]),
            }
            open = true;
        }
    }
    out
}

/// Whether a point lies within shapes none of which overlaps another.
pub(super) fn within(shapes: &Shapes<Point>, p: Point) -> bool {
    let mut inside = false;
    for ring in shapes.iter().flatten() {
        let mut before = ring[ring.len() - 1];
        for &q in ring {
            if (q[1] > p[1]) != (before[1] > p[1])
                && p[0] < q[0] + (p[1] - q[1]) / (before[1] - q[1]) * (before[0] - q[0])
            {
                inside = !inside;
            }
            before = q;
        }
    }
    inside
}

/// Metres of a line's part that are not worth standing.
const STUB: f64 = 0.5;

impl Things {
    /// What of the things in a tile, its points and the lines that reach it, none
    /// of the carriageways there, `roads`, takes. Lines are cut at the tile's edges.
    pub(super) fn kept(
        &self,
        points: &[u32],
        lines: &[u32],
        tile: Rect,
        roads: &[Shapes<Point>],
    ) -> Kept {
        let taken = |p: Point| roads.iter().any(|shapes| within(shapes, p));
        let stands = |&i: &u32| {
            let thing = &self.points[i as usize];
            let at = match thing.on {
                Some(_) => thing.at.first(),
                None => thing.at.iter().find(|p| !taken(**p)),
            };
            at.map(|p| (i, *p))
        };
        let mut kept = Kept {
            points: points.iter().filter_map(stands).collect(),
            lines: Vec::new(),
        };
        for &i in lines {
            let line = &self.lines[i as usize];
            for (k, p) in line.points.windows(2).enumerate() {
                let (a, b) = (p[0], p[1]);
                let Some([from, to]) = geometry::span_within(a, b, tile) else {
                    continue;
                };
                let mut cuts = vec![from];
                cuts.extend(roads.iter().flat_map(|shapes| crossings(shapes, a, b)));
                cuts.retain(|t| (from..to).contains(t));
                cuts.push(to);
                cuts.sort_by(f64::total_cmp);
                for c in cuts.windows(2) {
                    let t = (c[0] + c[1]) / 2.0;
                    if c[1] <= c[0] || taken(scaled(a, [b[0] - a[0], b[1] - a[1]], t)) {
                        continue;
                    }
                    let run = Run {
                        line: i,
                        from: (k as u32, c[0]),
                        to: (k as u32, c[1]),
                    };
                    join(&mut kept.lines, run);
                }
            }
        }
        kept
    }

    /// The runs of one tile joined where they go on from one another, as points;
    /// stubs left out.
    pub(super) fn laid(&self, mut runs: Vec<Run>) -> Vec<(u32, Vec<Point>)> {
        runs.sort_by(|a, b| {
            (a.line, a.from.0)
                .cmp(&(b.line, b.from.0))
                .then(a.from.1.total_cmp(&b.from.1))
        });
        let mut joined: Vec<Run> = Vec::new();
        for run in runs {
            join(&mut joined, run);
        }
        let points = |run: Run| {
            let line = &self.lines[run.line as usize];
            let at = |(k, t): (u32, f64)| {
                let (a, b) = (line.points[k as usize], line.points[k as usize + 1]);
                if t >= 1.0 {
                    b
                } else {
                    scaled(a, [b[0] - a[0], b[1] - a[1]], t)
                }
            };
            let mut points = vec![at(run.from)];
            points.extend((run.from.0..run.to.0).map(|k| line.points[k as usize + 1]));
            points.push(at(run.to));
            points.dedup();
            let length: f64 = points.windows(2).map(|p| distance(p[0], p[1])).sum();
            (length >= STUB * line.scale).then_some((run.line, points))
        };
        joined.into_iter().filter_map(points).collect()
    }
}

/// Adds a run to those before it, one with the last if it goes on from that.
fn join(runs: &mut Vec<Run>, run: Run) {
    const SAME: f64 = 1e-6;
    match runs.last_mut() {
        Some(last)
            if last.line == run.line
                && ((last.to.0 == run.from.0 && (last.to.1 - run.from.1).abs() < SAME)
                    || (last.to.0 + 1 == run.from.0
                        && last.to.1 > 1.0 - SAME
                        && run.from.1 < SAME)) =>
        {
            last.to = run.to;
        }
        _ => runs.push(run),
    }
}

/// How far along the segment, 0 to 1, it crosses the shapes' rings.
pub(super) fn crossings(shapes: &Shapes<Point>, a: Point, b: Point) -> Vec<f64> {
    let d = [b[0] - a[0], b[1] - a[1]];
    let mut out = Vec::new();
    for ring in shapes.iter().flatten() {
        let mut before = ring[ring.len() - 1];
        for &q in ring {
            let (p, e) = (before, [q[0] - before[0], q[1] - before[1]]);
            before = q;
            let cross = d[0] * e[1] - d[1] * e[0];
            if cross == 0.0 {
                continue;
            }
            let r = [p[0] - a[0], p[1] - a[1]];
            let t = (r[0] * e[1] - r[1] * e[0]) / cross;
            let s = (r[0] * d[1] - r[1] * d[0]) / cross;
            if (0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&s) {
                out.push(t);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::network;
    use super::super::profile::profile;
    use super::super::structures;
    use super::*;

    type Tags<'a> = &'a [(&'a str, &'static str)];

    fn get<'a>(tags: Tags<'a>) -> impl Fn(&str) -> Option<&'static str> + 'a {
        move |key| tags.iter().find(|t| t.0 == key).map(|t| t.1)
    }

    /// A way of `nodes` at metre positions (a unit a metre).
    fn way(tags: Tags, nodes: &[(i64, f64, f64)]) -> Way {
        Way {
            id: nodes[0].0,
            nodes: nodes.iter().map(|n| n.0).collect(),
            points: nodes.iter().map(|n| [n.1, n.2]).collect(),
            profile: profile(get(tags)).unwrap(),
            scale: 1.0,
        }
    }

    /// Nodes' things: id, where, and tags.
    fn stood(ways: &[Way], nodes: &[(i64, Point, Tags)], signs: &[(i64, details::Sign)]) -> Things {
        let network = network::build(ways);
        let signs = signs.iter().copied().collect();
        let details = details::build(
            ways,
            &network,
            &signs,
            &Default::default(),
            &Default::default(),
            &Default::default(),
        );
        let groups = structures::build(ways, &[]);
        let index = Index::new(&groups, ways, |_| (7, 7));
        let mapped: Vec<_> = nodes
            .iter()
            .filter_map(|(id, p, tags)| Some((*id, *p, 1.0, mapped(get(tags))?)))
            .collect();
        build(ways, ways, &index, &details, &mapped, &[])
    }

    /// Eastwards, 7 m wide, traffic both ways.
    fn street() -> Way {
        let tags = [("highway", "primary"), ("lanes", "2")];
        way(&tags, &[(1, 0.0, 0.0), (2, 50.0, 0.0), (3, 100.0, 0.0)])
    }

    const LAMP: Tags = &[("highway", "street_lamp")];

    #[test]
    fn fences_are_those_on_the_ground_in_the_open_at_their_height() {
        let read = |tags: Tags| fence(get(tags)).map(|f| (f.kind, f.height));
        let of = ("barrier", "fence");
        assert_eq!(read(&[of]), Some(("fence", 1.5)));
        assert_eq!(read(&[of, ("height", "2.2 m")]), Some(("fence", 2.2)));
        assert_eq!(read(&[of, ("height", "31")]), Some(("fence", 1.5)));
        assert_eq!(read(&[of, ("height", "0.1")]), None, "said to be a kerb");
        let city = ("barrier", "city_wall");
        assert_eq!(read(&[city, ("height", "12")]), Some(("city_wall", 12.0)));
        for left in [
            ("layer", "1"),
            ("level", "-1"),
            ("location", "underground"),
            ("tunnel", "yes"),
            ("bridge", "yes"),
            ("building", "yes"),
        ] {
            assert_eq!(read(&[of, left]), None, "{left:?}");
        }
        // a barrier of its own
        assert_eq!(
            read(&[("barrier", "wall"), ("wall", "noise_barrier")]),
            None
        );
        assert_eq!(read(&[("barrier", "guard_rail")]), None);
    }

    #[test]
    fn a_lamp_leans_over_the_road_it_stands_by() {
        let t = stood(
            &[street()],
            &[
                (10, [50.0, 4.5], LAMP),
                (11, [50.0, -4.5], &[LAMP[0], ("height", "6")]),
                (12, [50.0, 4.5], &[LAMP[0], ("lamp_mount", "straight_mast")]),
                (13, [50.0, 40.0], LAMP),
                (14, [50.0, 4.5], &[LAMP[0], ("lamp_mount", "wall")]),
            ],
            &[],
        );
        let of: Vec<_> = t.points.iter().map(|p| (p.turn, p.size)).collect();
        assert_eq!(
            of,
            [
                (Some(0), Some(8.0)),
                (Some(180), Some(6.0)),
                (None, Some(8.0)),
                (None, Some(4.5))
            ]
        );
    }

    #[test]
    fn what_stands_in_a_carriageway_is_moved_to_its_edge_or_left_out() {
        let street = street();
        let t = stood(
            std::slice::from_ref(&street),
            &[
                (10, [50.0, 3.0], LAMP),
                (11, [50.0, 0.5], LAMP),
                (12, [50.0, 6.0], LAMP),
            ],
            &[],
        );
        let surface = geometry::ribbon(&street.points, street.profile.width);
        let tile = [0.0, 0.0, 4096.0, 4096.0];
        let kept = t.kept(&[0, 1, 2], &[], tile, &[surface]);
        assert_eq!(kept.points.len(), 2);
        assert!((kept.points[0].1[1] - 3.9).abs() < 1e-9);
        assert_eq!(kept.points[1], (2, [50.0, 6.0]));
    }

    #[test]
    fn signs_face_those_they_are_for() {
        let sign = ("traffic_sign", "maxspeed");
        let t = stood(
            &[street()],
            &[
                // right of the way, left of it, as it says, and on it
                (10, [50.0, 5.0], &[sign]),
                (11, [50.0, -5.0], &[sign]),
                (12, [50.0, 5.0], &[sign, ("direction", "N")]),
                (2, [50.0, 0.0], &[sign, ("direction", "backward")]),
                (2, [50.0, 0.0], &[sign]),
                (13, [50.0, 60.0], &[sign]),
            ],
            &[],
        );
        let of: Vec<_> = t.points.iter().map(|p| (p.at[0][1], p.turn)).collect();
        let half = 3.5 + SIGN_OUT;
        assert_eq!(
            of,
            [
                (5.0, Some(270)),
                (-5.0, Some(90)),
                (5.0, Some(0)),
                (-half, Some(90))
            ]
        );
    }

    #[test]
    fn lines_to_stop_at_have_their_signs_at_the_right() {
        let stop = |facing| details::Sign::Line(details::Kind::Stop, facing);
        let mut ways = vec![street()];
        ways.push(way(
            &[("highway", "primary")],
            &[(4, 100.0, 60.0), (3, 100.0, 0.0), (5, 100.0, -60.0)],
        ));
        let t = stood(&ways, &[], &[(2, stop(details::Facing::Along))]);
        assert_eq!(t.points.len(), 1, "{:?}", t.points);
        let sign = &t.points[0];
        assert_eq!((sign.kind, sign.turn), (Kind::Stop, Some(270)));
        assert_eq!(sign.at[0], [50.0, 3.5 + SIGN_OUT]);
    }

    #[test]
    fn gates_are_those_of_motor_roads_and_close_them() {
        let path = way(
            &[("highway", "footway")],
            &[(6, 0.0, 90.0), (7, 50.0, 90.0), (8, 99.0, 90.0)],
        );
        let t = stood(
            &[street(), path],
            &[
                (2, [50.0, 0.0], &[("barrier", "gate")]),
                (3, [100.0, 0.0], &[("barrier", "lift_gate")]),
                (7, [50.0, 90.0], &[("barrier", "gate")]),
                (9, [50.0, 30.0], &[("barrier", "gate")]),
            ],
            &[],
        );
        let of: Vec<_> = t.points.iter().map(|p| (p.kind, p.turn)).collect();
        assert_eq!(of, [(Kind::Gate, Some(180)), (Kind::LiftGate, Some(0))]);
        assert_eq!(t.points[0].at[0], [50.0, -3.5 - GATE_OUT]);
        assert_eq!(t.points[0].size, Some(7.0 + 2.0 * GATE_OUT));
    }

    #[test]
    fn a_shelter_stands_behind_its_stop() {
        let stop: Tags = &[("highway", "bus_stop"), ("shelter", "yes")];
        let t = stood(&[street()], &[(10, [50.0, 5.0], stop)], &[]);
        let of: Vec<_> = t.points.iter().map(|p| (p.kind, p.at[0])).collect();
        assert_eq!(
            of,
            [
                (Kind::Shelter, [50.0, 5.0 + BEHIND]),
                (Kind::BusStop, [50.0, 5.0])
            ]
        );
        // a hydrant in the ground is not seen, nor is a hidden thing
        let no: [Tags; 3] = [
            &[("emergency", "fire_hydrant")],
            &[("amenity", "bench"), ("indoor", "yes")],
            &[("amenity", "bench"), ("level", "1")],
        ];
        assert!(no.iter().all(|tags| mapped(get(tags)).is_none()));
    }

    #[test]
    fn a_lamp_at_a_bridges_edge_stands_on_it_and_a_fence_there_is_its_parapet() {
        let tags = [("highway", "primary"), ("lanes", "2"), ("bridge", "yes")];
        let ways = [
            way(&tags, &[(1, 0.0, 0.0), (2, 100.0, 0.0)]),
            way(
                &[("highway", "primary"), ("lanes", "2")],
                &[(2, 100.0, 0.0), (3, 300.0, 0.0)],
            ),
        ];
        let t = stood(
            &ways,
            &[
                (10, [50.0, 4.2], LAMP),
                (11, [50.0, 6.0], LAMP),
                (12, [50.0, 0.0], LAMP),
                (13, [50.0, 4.2], &[LAMP[0], ("layer", "0")]),
                (14, [50.0, 90.0], &[LAMP[0], ("layer", "1")]),
            ],
            &[],
        );
        let of: Vec<_> = t.points.iter().map(|p| (p.at[0][1], p.on)).collect();
        assert_eq!(of.len(), 3, "{of:?}");
        assert!((of[0].0 - (3.5 - ON_EDGE)).abs() < 1e-9);
        assert_eq!((of[0].1, of[1], of[2].1), (Some((0, 0)), (6.0, None), None));
        let groups = structures::build(&ways, &[]);
        let index = Index::new(&groups, &ways, |_| (7, 7));
        let off = off_bridges(&ways, &index, &[[-40.0, 3.6], [200.0, 3.6]], 1.0);
        assert_eq!(off.len(), 2, "{off:?}");
        assert_eq!((off[0].len(), off[1].len()), (2, 2));
        assert_eq!((off[0][1], off[1][0]), ([0.0, 3.6], [100.0, 3.6]));
    }

    #[test]
    fn a_fence_is_cut_where_a_road_runs_and_whole_over_tiles() {
        let square = |x: f64| vec![vec![[x, -5.0], [x, 5.0], [x + 4.0, 5.0], [x + 4.0, -5.0]]];
        let fence = Fence {
            kind: "fence",
            height: 1.5,
        };
        let line = Line {
            fence,
            points: vec![[0.0, 1.0], [10.0, 1.0], [30.0, 1.0]],
            scale: 1.0,
        };
        let things = Things {
            points: Vec::new(),
            lines: vec![line],
        };
        let roads = [vec![square(8.0)], vec![square(20.0)]];
        let mut runs = Vec::new();
        for x in [0.0, 16.0] {
            let tile = [x, 0.0, x + 16.0, 16.0];
            runs.extend(things.kept(&[], &[0], tile, &roads).lines);
        }
        runs.reverse();
        let laid: Vec<_> = things.laid(runs).into_iter().map(|l| l.1).collect();
        let x = |line: &Vec<Point>| line.iter().map(|p| p[0].round()).collect::<Vec<_>>();
        let x: Vec<_> = laid.iter().map(x).collect();
        assert_eq!(x, [vec![0.0, 8.0], vec![12.0, 20.0], vec![24.0, 30.0]]);
    }
}
