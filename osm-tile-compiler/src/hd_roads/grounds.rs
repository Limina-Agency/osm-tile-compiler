//! Ground that is painted and no road's: pitches and their lines, running
//! tracks, aerodromes, bays nobody mapped one by one, and the yards of filling
//! stations, and of services and rest areas that say what they are made of. It
//! lies under roads and paths.
//!
//! A pitch's lines are drawn where its outline is nearly a rectangle (it fills
//! [`RECTANGLE`] of the least one round it) and as large as the sport's rules
//! let the lines be, 8% off at most as outlines are ([`OFF`]), with the free
//! ground round them that outlines often take in. They are in its middle, at
//! their size in the rules, or less if the outline is. Tennis and basketball
//! courts side by side too, where only one number of them fits. Football's are
//! the outline itself, with the areas a full pitch's from 90 m and in proportion
//! to the length from 50, as the rules for the young are; under 50 m only its
//! middle is marked, and only of what is hard or artificial. Of several sports
//! the first that fits. Nothing is drawn where a pitch holds a smaller one.
//!
//! What a pitch is made of, where no tag says, is what most of its sport's that
//! say are of. A runway is painted if it is tagged as paved; a taxiway or an
//! apron that does not say is as the nearest runway is.
//!
//! Bays are a car park's long side divided, where its `capacity` and
//! `orientation` are tagged, it is a strip of one row, and that comes to bays as
//! wide and long as they are (slanted ones lean one way or the other, which no
//! tag says).

use super::furniture::{self, Area, AreaKind};
use super::geometry::{self, Rect};
use super::network::{Point, Way, distance};
use super::{profile, structures, things};
use fast_mvt::{MvtCoord, MvtError, MvtGeomType, MvtLayerBuilder};
use i_overlay::i_shape::base::data::Shapes;
use osm_pbf_ingest::{NormalizedRelation, RelationMemberType};
use rustc_hash::FxHashMap;
use std::collections::BTreeMap;
use std::f64::consts::{FRAC_PI_2, PI, TAU};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum Material {
    /// Asphalt, concrete, acrylic.
    Hard,
    Rubber,
    /// Artificial.
    Turf,
    Grass,
    Clay,
    Sand,
    Earth,
    /// No tag says, and nothing usual.
    Plain,
}

impl Material {
    fn name(self) -> &'static str {
        match self {
            Self::Hard => "hard",
            Self::Rubber => "rubber",
            Self::Turf => "turf",
            Self::Grass => "grass",
            Self::Clay => "clay",
            Self::Sand => "sand",
            Self::Earth => "earth",
            Self::Plain => "plain",
        }
    }

    fn paved(self) -> bool {
        matches!(self, Self::Hard | Self::Rubber)
    }

    /// That of a `surface`.
    fn of(surface: &str) -> Option<Self> {
        let first = surface.split([';', ',', ':']).next().unwrap_or_default();
        Some(match first.trim() {
            "asphalt" | "concrete" | "paved" | "paving_stones" | "acrylic" | "decoturf"
            | "hard" | "sett" | "metal" | "wood" => Self::Hard,
            "tartan" | "rubber" | "epdm" | "polyurethane" | "poliuretan" | "synthetic"
            | "plastic" => Self::Rubber,
            "artificial_turf" | "artificial" | "astroturf" | "artificial_grass" => Self::Turf,
            "grass" | "grass_paver" => Self::Grass,
            "clay" => Self::Clay,
            "sand" => Self::Sand,
            "ground" | "dirt" | "earth" | "mud" | "compacted" | "fine_gravel" | "gravel"
            | "unpaved" | "woodchips" => Self::Earth,
            _ => return None,
        })
    }

    /// That of most pitches of a sport that say.
    fn usual(sport: &str) -> Self {
        match sport {
            "soccer" | "football" | "rugby" | "rugby_union" | "american_football" => Self::Grass,
            "basketball" | "volleyball" | "skateboard" | "roller_skating" => Self::Hard,
            "handball" | "multi" | "running" => Self::Rubber,
            "tennis" => Self::Clay,
            "beachvolleyball" | "equestrian" | "athletics" => Self::Sand,
            "padel" | "field_hockey" => Self::Turf,
            _ => Self::Plain,
        }
    }
}

/// Sports whose lines are drawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Sport {
    Soccer,
    Basketball,
    Tennis,
    Volleyball,
    BeachVolleyball,
    Handball,
}

impl Sport {
    fn of(value: &str) -> Option<Self> {
        Some(match value {
            "soccer" => Self::Soccer,
            "basketball" => Self::Basketball,
            "tennis" => Self::Tennis,
            "volleyball" => Self::Volleyball,
            "beachvolleyball" => Self::BeachVolleyball,
            "handball" => Self::Handball,
            _ => return None,
        })
    }
}

/// What ground a way or a multipolygon is.
#[derive(Debug, Clone, PartialEq)]
pub(super) enum Ground {
    Pitch {
        material: Material,
        sports: Vec<Sport>,
    },
    Track {
        material: Material,
        /// Metres, tagged.
        width: Option<f64>,
        lanes: Option<u32>,
        /// Whether it says it is an area, or not one.
        area: Option<bool>,
    },
    Runway {
        material: Option<Material>,
        width: Option<f64>,
        /// Its two ends' names.
        names: Option<[String; 2]>,
        painted: bool,
    },
    Taxiway {
        material: Option<Material>,
        /// Metres across, tagged or usual on hard ground.
        width: Option<f64>,
        usual: f64,
    },
    Apron {
        material: Option<Material>,
    },
    Helipad {
        material: Option<Material>,
    },
    Yard {
        material: Material,
    },
    /// A car park's one row of bays, which lie across its length or along it.
    Strip {
        capacity: u32,
        across: bool,
    },
    /// A bay for who uses a wheelchair.
    Disabled,
}

/// What a way with these tags is, if it lies on the ground under the sky.
pub(super) fn ground<'a>(get: impl Fn(&str) -> Option<&'a str>, closed: bool) -> Option<Ground> {
    // most ways are told by their keys
    let keys = ["leisure", "aeroway", "amenity", "highway"];
    if keys.iter().all(|key| get(key).is_none()) {
        return None;
    }
    let said = get("surface").and_then(Material::of);
    let metres = |key: &str, least: f64, most: f64| {
        let value = get(key).and_then(profile::metres);
        value.filter(|v| (least..=most).contains(v))
    };
    let found = match (get("leisure"), get("aeroway")) {
        (Some("pitch"), _) if closed => {
            let sports: Vec<&str> = get("sport").unwrap_or_default().split(';').collect();
            let first = sports[0].trim();
            if [
                "table_tennis",
                "chess",
                "climbing",
                "shooting",
                "paintball",
                "ski_jumping",
            ]
            .contains(&first)
            {
                // a table, a wall, a wood
                return None;
            }
            Ground::Pitch {
                material: said.unwrap_or(Material::usual(first)),
                sports: sports.iter().filter_map(|s| Sport::of(s.trim())).collect(),
            }
        }
        (Some("track"), _) if matches!(get("sport"), Some("athletics" | "running")) => {
            let lanes = get("lanes").and_then(|v| v.trim().parse().ok());
            Ground::Track {
                material: said.unwrap_or(Material::Rubber),
                width: metres("width", 1.0, 15.0),
                lanes: lanes.filter(|n| (1..=10).contains(n)),
                area: get("area").map(|v| v == "yes"),
            }
        }
        (_, Some("runway")) => {
            let ends = get("ref").and_then(|v| v.split_once('/'));
            Ground::Runway {
                material: said,
                width: metres("width", 5.0, 150.0),
                names: ends.map(|(a, b)| [a, b].map(|v| v.trim().to_owned())),
                painted: get("runway").is_none(),
            }
        }
        (_, Some(value @ ("taxiway" | "taxilane"))) => Ground::Taxiway {
            material: said,
            width: metres("width", 3.0, 80.0),
            usual: if value == "taxiway" { 23.0 } else { 15.0 },
        },
        (_, Some("apron")) if closed => Ground::Apron { material: said },
        (_, Some("helipad")) if closed => Ground::Helipad { material: said },
        _ if !closed => return None,
        _ => match (get("amenity"), get("highway")) {
            (Some("fuel"), _) => Ground::Yard {
                material: said.unwrap_or(Material::Hard),
            },
            // a whole site, its grass and trees too: what it is made of only where it says
            (_, Some("services" | "rest_area")) => Ground::Yard { material: said? },
            (Some("parking"), _) => Ground::Strip {
                capacity: get("capacity").and_then(|v| v.trim().parse().ok())?,
                across: match get("orientation")? {
                    "perpendicular" => true,
                    "parallel" => false,
                    _ => return None,
                },
            },
            (Some("parking_space"), _) if get("parking_space") == Some("disabled") => {
                Ground::Disabled
            }
            _ => return None,
        },
    };
    let gone = ["disused", "abandoned"]
        .iter()
        .any(|key| get(key) == Some("yes"));
    (!gone && furniture::open(&get)).then_some(found)
}

/// The ground a multipolygon is, and its ways and whether each is of its outside.
pub(super) fn multipolygon(relation: &NormalizedRelation) -> Option<(Ground, Vec<(i64, bool)>)> {
    let get = |key: &str| relation.tags.get(key);
    if get("type") != Some("multipolygon") {
        return None;
    }
    let mut ground = ground(get, true)?;
    match &mut ground {
        Ground::Strip { .. } | Ground::Disabled => return None,
        Ground::Track { area, .. } => *area = Some(true),
        _ => {}
    }
    let ways = relation
        .members
        .iter()
        .filter(|m| m.member_type == RelationMemberType::Way)
        .map(|m| (m.member_id, m.role != "inner"))
        .collect();
    Some((ground, ways))
}

/// A ground where it is.
#[derive(Debug, Clone)]
pub(super) struct Mapped {
    pub ground: Ground,
    /// The nodes it begins and ends at, and its line; a multipolygon has none.
    pub ends: [i64; 2],
    pub line: Vec<Point>,
    /// What it encloses, if it does: rings wound as a widened line's, and those
    /// of holes the other way.
    pub shapes: Shapes<Point>,
    /// World units a metre.
    pub scale: f64,
}

/// A surface's, in the order they lie on each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Kind {
    Yard,
    Pitch,
    Track,
    Apron,
    Taxiway,
    Runway,
    Helipad,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::Yard => "yard",
            Self::Pitch => "pitch",
            Self::Track => "track",
            Self::Apron => "apron",
            Self::Taxiway => "taxiway",
            Self::Runway => "runway",
            Self::Helipad => "helipad",
        }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Surface {
    kind: Kind,
    material: Material,
    shapes: Shapes<Point>,
    /// The corners of the box it is in.
    pub corners: [Point; 2],
}

/// What a line is painted as.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Paint {
    Pitch,
    /// A dashed line of one.
    PitchDashed,
    Runway,
    /// A bar of one: a ring.
    RunwayBar,
    Taxi,
    Helipad,
    Bay,
    Wheelchair,
}

impl Paint {
    fn name(self) -> &'static str {
        match self {
            Self::Pitch | Self::PitchDashed => "pitch",
            Self::Runway => "runway",
            Self::RunwayBar => "runway-bar",
            Self::Taxi => "taxi",
            Self::Helipad => "helipad",
            Self::Bay => "bay",
            Self::Wheelchair => "wheelchair",
        }
    }

    /// As a marking's: whether a tag says it, or only implies it.
    fn confidence(self) -> u64 {
        if self == Self::Wheelchair { 2 } else { 1 }
    }
}

#[derive(Debug, Clone)]
pub(super) struct Line {
    paint: Paint,
    pub points: Vec<Point>,
}

#[derive(Default)]
pub(super) struct Grounds {
    pub surfaces: Vec<Surface>,
    pub lines: Vec<Line>,
    /// Pitches, and those with their lines.
    pub pitches: u64,
    pub lined: u64,
}

// Shapes.

/// The least rectangle round an outline.
#[derive(Debug, Clone, Copy)]
struct Fit {
    centre: Point,
    /// Along its longer side, of length 1.
    along: Point,
    /// Metres long and across.
    size: [f64; 2],
    /// How much of it the outline fills.
    fill: f64,
}

/// Of a ring, closed or not.
fn area(ring: &[Point]) -> f64 {
    let o = ring[0];
    let twice: f64 = (0..ring.len())
        .map(|i| {
            let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
            (a[0] - o[0]) * (b[1] - o[1]) - (b[0] - o[0]) * (a[1] - o[1])
        })
        .sum();
    (twice / 2.0).abs()
}

/// The convex hull of points, in order.
fn hull(points: &[Point]) -> Vec<Point> {
    let mut sorted = points.to_vec();
    sorted.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    sorted.dedup();
    let chain = |points: &mut dyn Iterator<Item = Point>| {
        let mut out: Vec<Point> = Vec::new();
        for p in points {
            while out.len() >= 2 {
                let (a, b) = (out[out.len() - 2], out[out.len() - 1]);
                if (b[0] - a[0]) * (p[1] - a[1]) - (b[1] - a[1]) * (p[0] - a[0]) > 0.0 {
                    break;
                }
                out.pop();
            }
            out.push(p);
        }
        out.pop();
        out
    };
    let mut out = chain(&mut sorted.iter().copied());
    out.extend(chain(&mut sorted.iter().rev().copied()));
    out
}

fn fit(ring: &[Point], scale: f64) -> Option<Fit> {
    let hull = hull(ring);
    if hull.len() < 3 {
        return None;
    }
    let mut best: Option<(f64, Fit)> = None;
    for i in 0..hull.len() {
        let (a, b) = (hull[i], hull[(i + 1) % hull.len()]);
        let length = distance(a, b);
        let u = [(b[0] - a[0]) / length, (b[1] - a[1]) / length];
        let span = |d: Point| {
            hull.iter()
                .map(|p| (p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1])
                .fold([f64::MAX, f64::MIN], |s, v| [s[0].min(v), s[1].max(v)])
        };
        let v = [-u[1], u[0]];
        let [x, y] = [span(u), span(v)];
        let (w, h) = (x[1] - x[0], y[1] - y[0]);
        if best.as_ref().is_some_and(|b| b.0 <= w * h) || w * h <= 0.0 {
            continue;
        }
        let (mx, my) = ((x[0] + x[1]) / 2.0, (y[0] + y[1]) / 2.0);
        best = Some((
            w * h,
            Fit {
                centre: [a[0] + u[0] * mx + v[0] * my, a[1] + u[1] * mx + v[1] * my],
                along: if w >= h { u } else { v },
                size: [w.max(h) / scale, w.min(h) / scale],
                fill: area(ring) / (w * h),
            },
        ));
    }
    best.map(|b| b.1)
}

/// Draws in metres, from a point and along two axes there.
struct Pen<'a> {
    origin: Point,
    /// World units a metre along each axis.
    axes: [Point; 2],
    lines: &'a mut Vec<Line>,
}

impl<'a> Pen<'a> {
    /// With axes along `along`, of length 1, and to its right.
    fn new(origin: Point, along: Point, scale: f64, lines: &'a mut Vec<Line>) -> Self {
        let axes = [along, [-along[1], along[0]]].map(|a| [a[0] * scale, a[1] * scale]);
        Self {
            origin,
            axes,
            lines,
        }
    }

    fn at(&self, [x, y]: Point) -> Point {
        let [u, v] = self.axes;
        [
            self.origin[0] + u[0] * x + v[0] * y,
            self.origin[1] + u[1] * x + v[1] * y,
        ]
    }

    fn line(&mut self, paint: Paint, points: &[Point]) {
        let points = points.iter().map(|p| self.at(*p)).collect();
        self.lines.push(Line { paint, points });
    }

    fn rect(&mut self, paint: Paint, [x0, y0]: Point, [x1, y1]: Point) {
        let ring = [[x0, y0], [x1, y0], [x1, y1], [x0, y1], [x0, y0]];
        self.line(paint, &ring);
    }

    fn arc(&mut self, paint: Paint, centre: Point, radius: f64, from: f64, to: f64) {
        self.line(paint, &arc(centre, radius, from, to));
    }
}

/// Points round a circle, from an angle to another (radians, of the axes).
fn arc(centre: Point, radius: f64, from: f64, to: f64) -> Vec<Point> {
    /// Metres the chords are within.
    const WITHIN: f64 = 0.03;
    let step = 2.0 * (1.0 - WITHIN / radius.max(2.0 * WITHIN)).acos();
    let parts = ((to - from).abs() / step).ceil().max(1.0) as u32;
    (0..=parts)
        .map(|i| {
            let angle = from + (to - from) * f64::from(i) / f64::from(parts);
            let (sin, cos) = angle.sin_cos();
            [centre[0] + radius * cos, centre[1] + radius * sin]
        })
        .collect()
}

// Courts.

/// How much of its rectangle a pitch fills at least.
const RECTANGLE: f64 = 0.92;
/// How far off its size an outline may be.
const OFF: f64 = 0.08;

/// A court whose size the rules set.
struct Court {
    /// Metres long and across, and the least it may be, in parts of that.
    size: [f64; 2],
    least: [f64; 2],
    /// Metres of free ground beyond each end and each side.
    free: [f64; 2],
    /// Where courts side by side are told: metres long, and of each across.
    row: Option<[[f64; 2]; 2]>,
}

fn court(sport: Sport) -> Option<Court> {
    let of = |size, least, free, row| {
        Some(Court {
            size,
            least,
            free,
            row,
        })
    };
    match sport {
        Sport::Soccer => None,
        Sport::Basketball => of(
            [28.0, 15.0],
            [26.0 / 28.0, 14.0 / 15.0],
            [3.0, 3.0],
            Some([[24.7, 34.0], [15.0, 20.0]]),
        ),
        Sport::Tennis => of(
            [23.77, 10.97],
            [1.0, 1.0],
            [8.0, 4.6],
            Some([[34.0, 40.0], [14.6, 18.6]]),
        ),
        Sport::Volleyball => of([18.0, 9.0], [1.0, 1.0], [6.5, 5.0], None),
        Sport::BeachVolleyball => of([16.0, 8.0], [1.0, 1.0], [6.0, 6.0], None),
        Sport::Handball => of([40.0, 20.0], [0.95, 0.9], [4.0, 3.0], None),
    }
}

/// Courts of a sport that fit a rectangle: how many side by side, and whether
/// they lie along its longer side. None if none or several numbers do.
fn courts(court: &Court, [long, short]: [f64; 2]) -> Option<(u32, bool)> {
    let one = (0..2).all(|i| {
        let least = court.size[i] * court.least[i] * (1.0 - OFF);
        (least..=court.size[i] + 2.0 * court.free[i]).contains(&[long, short][i])
    });
    let mut found: Vec<(u32, bool)> = one.then_some((1, true)).into_iter().collect();
    if let Some([length, across]) = court.row {
        for (along, row, lies) in [(long, short, true), (short, long, false)] {
            if (length[0]..=length[1]).contains(&along) {
                let fits = |n: &u32| (across[0]..=across[1]).contains(&(row / f64::from(*n)));
                found.extend((2..=8).filter(fits).map(|n| (n, lies)));
            }
        }
    }
    match found[..] {
        [only] => Some(only),
        _ => None,
    }
}

/// How football's lines go on a rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Football {
    /// Metres long and across, and what its areas are in parts of a full pitch's.
    Areas([f64; 2], f64),
    /// Metres long and across, and its circle's radius.
    Middle([f64; 2], f64),
}

fn football([long, short]: [f64; 2], material: Material) -> Option<Football> {
    let off = OFF / 2.0;
    let within =
        |v: f64, least: f64, most: f64| v >= least * (1.0 - off) && v <= most * (1.0 + off);
    let shape = long / short;
    if within(long, 90.0, 120.0) && within(short, 45.0, 90.0) && shape >= 1.1 {
        let size = [long.min(120.0), short.min(90.0)];
        return Some(Football::Areas(size, 1.0));
    }
    if (50.0..90.0).contains(&long)
        && (28.0..=60.0).contains(&short)
        && (1.2..=2.2).contains(&shape)
    {
        // the penalty area's 40.32 m within the touchlines
        let parts = (long / 105.0).min((short - 4.0) / 40.32);
        return Some(Football::Areas([long, short], parts));
    }
    let lasting = !matches!(
        material,
        Material::Grass | Material::Earth | Material::Sand | Material::Plain
    );
    let small = (25.0..50.0).contains(&long)
        && (15.0..=30.0).contains(&short)
        && (1.3..=2.4).contains(&shape);
    (small && lasting).then_some(Football::Middle([long, short], 3.0))
}

fn draw_football(pen: &mut Pen, found: Football) {
    use Paint::Pitch;
    let ([long, short], radius) = match found {
        Football::Areas(size, parts) => (size, 9.15 * parts),
        Football::Middle(size, radius) => (size, radius),
    };
    // a line's own width belongs to what it bounds
    let (x, y) = (long / 2.0 - 0.06, short / 2.0 - 0.06);
    pen.rect(Pitch, [-x, -y], [x, y]);
    pen.line(Pitch, &[[0.0, -y], [0.0, y]]);
    pen.arc(Pitch, [0.0, 0.0], radius, 0.0, TAU);
    let Football::Areas(_, parts) = found else {
        return;
    };
    for end in [-1.0, 1.0] {
        for (deep, half) in [(16.5, 20.16), (5.5, 9.16)] {
            let (inner, half) = (end * (x - deep * parts), half * parts);
            pen.line(
                Pitch,
                &[
                    [end * x, -half],
                    [inner, -half],
                    [inner, half],
                    [end * x, half],
                ],
            );
        }
        // what of the circle about the penalty mark is beyond the area
        let half = (5.5_f64 / 9.15).acos();
        let towards = if end > 0.0 { PI } else { 0.0 };
        let mark = [end * (x - 11.0 * parts), 0.0];
        pen.arc(Pitch, mark, radius, towards - half, towards + half);
        for side in [-1.0, 1.0] {
            let from = match (end > 0.0, side > 0.0) {
                (false, false) => 0.0,
                (true, false) => FRAC_PI_2,
                (true, true) => PI,
                (false, true) => 3.0 * FRAC_PI_2,
            };
            let corner = [end * x, side * y];
            pen.arc(Pitch, corner, 1.0, from, from + FRAC_PI_2);
        }
    }
}

/// The lines of a court about the pen's origin, at the size in the rules.
fn draw_court(pen: &mut Pen, sport: Sport) {
    use Paint::{Pitch, PitchDashed};
    let Some(Court { size, .. }) = court(sport) else {
        return;
    };
    let (x, y) = (size[0] / 2.0, size[1] / 2.0);
    pen.rect(Pitch, [-x, -y], [x, y]);
    match sport {
        Sport::Soccer | Sport::BeachVolleyball => {}
        Sport::Volleyball => {
            for at in [-3.0, 0.0, 3.0] {
                pen.line(Pitch, &[[at, -y], [at, y]]);
            }
        }
        Sport::Tennis => {
            for side in [-4.115, 4.115] {
                pen.line(Pitch, &[[-x, side], [x, side]]);
            }
            for at in [-6.4, 6.4] {
                pen.line(Pitch, &[[at, -4.115], [at, 4.115]]);
            }
            pen.line(Pitch, &[[-6.4, 0.0], [6.4, 0.0]]);
        }
        Sport::Basketball => {
            pen.line(Pitch, &[[0.0, -y], [0.0, y]]);
            pen.arc(Pitch, [0.0, 0.0], 1.8, 0.0, TAU);
            for end in [-1.0, 1.0] {
                let towards = if end > 0.0 { PI } else { 0.0 };
                let throw = end * (x - 5.8);
                let key = [
                    [end * x, -2.45],
                    [throw, -2.45],
                    [throw, 2.45],
                    [end * x, 2.45],
                ];
                pen.line(Pitch, &key);
                let half = [towards - FRAC_PI_2, towards + FRAC_PI_2];
                pen.arc(Pitch, [throw, 0.0], 1.8, half[0], half[1]);
                // 6.75 m from the ring, and 0.9 m from the sides where that is nearer
                let ring = [end * (x - 1.575), 0.0];
                let angle = ((y - 0.9) / 6.75).asin();
                let curve = arc(ring, 6.75, towards - angle, towards + angle);
                let mut three = vec![[end * x, curve[0][1]]];
                three.extend(&curve);
                three.push([end * x, curve[curve.len() - 1][1]]);
                pen.line(Pitch, &three);
            }
        }
        Sport::Handball => {
            pen.line(Pitch, &[[0.0, -y], [0.0, y]]);
            for end in [-1.0, 1.0] {
                let towards = if end > 0.0 { PI } else { 0.0 };
                // about the goal's posts, 3 m apart
                let round = |radius: f64, most: f64| {
                    let turn = if end > 0.0 { -1.0 } else { 1.0 };
                    let mut line = arc([end * x, -1.5], radius, towards - turn * most, towards);
                    line.extend(arc([end * x, 1.5], radius, towards, towards + turn * most));
                    line
                };
                pen.line(Pitch, &round(6.0, FRAC_PI_2));
                // the free throw line goes out of the sides
                pen.line(PitchDashed, &round(9.0, ((y - 1.5) / 9.0).asin()));
                let seven = end * (x - 7.0);
                pen.line(Pitch, &[[seven, -0.5], [seven, 0.5]]);
            }
        }
    }
}

/// Draws a pitch's lines if they fit it.
fn draw_pitch(
    fit: &Fit,
    sports: &[Sport],
    material: Material,
    scale: f64,
    lines: &mut Vec<Line>,
) -> bool {
    if fit.fill < RECTANGLE {
        return false;
    }
    for &sport in sports {
        let Some(court) = court(sport) else {
            let Some(found) = football(fit.size, material) else {
                continue;
            };
            draw_football(&mut Pen::new(fit.centre, fit.along, scale, lines), found);
            return true;
        };
        let Some((number, lies)) = courts(&court, fit.size) else {
            continue;
        };
        let across = [-fit.along[1], fit.along[0]];
        let (along, beside) = if lies {
            (fit.along, across)
        } else {
            (across, fit.along)
        };
        let [length, row] = if lies {
            fit.size
        } else {
            [fit.size[1], fit.size[0]]
        };
        let each = row / f64::from(number);
        for i in 0..number {
            let off = (f64::from(i) + 0.5) * each - row / 2.0;
            let centre = [
                fit.centre[0] + beside[0] * off * scale,
                fit.centre[1] + beside[1] * off * scale,
            ];
            let mut pen = Pen::new(centre, along, scale, lines);
            for (axis, of) in pen
                .axes
                .iter_mut()
                .zip([length, each].iter().zip(court.size))
            {
                let by = (of.0 / of.1).min(1.0);
                *axis = axis.map(|v| v * by);
            }
            draw_court(&mut pen, sport);
        }
        return true;
    }
    false
}

// Tracks.

/// Metres across a lane, and how many lanes a track has if no tag says.
const LANE: f64 = 1.22;
const LANES: u32 = 4;

/// Draws a track: its surface, and its lanes' lines where a tag says how many.
fn track(grounds: &mut Grounds, mapped: &Mapped, material: Material) {
    let Ground::Track {
        width, lanes, area, ..
    } = mapped.ground
    else {
        return;
    };
    let (line, scale) = (&mapped.line, mapped.scale);
    let length = geometry::lengths(line, scale, 0.0)
        .last()
        .copied()
        .unwrap_or_default();
    let closed = !mapped.shapes.is_empty();
    // a loop encloses what is no track
    let round = closed && !line.is_empty() && length >= 100.0 && {
        let within = self::area(line) / (scale * scale);
        4.0 * PI * within / (length * length) >= 0.4
    };
    if closed && !round && area != Some(false) {
        grounds.add(Kind::Track, material, mapped.shapes.clone());
        let fitted = mapped.shapes.first().and_then(|s| fit(&s[0], scale));
        // lines only where its lanes are said
        let (Some(fit), Some(lanes), 1) = (fitted, lanes, mapped.shapes.len()) else {
            return;
        };
        let each = fit.size[1] / f64::from(lanes);
        if fit.fill >= RECTANGLE && (0.9..=1.5).contains(&each) {
            let mut pen = Pen::new(fit.centre, fit.along, scale, &mut grounds.lines);
            let half = fit.size.map(|v| v / 2.0);
            for i in 0..=lanes {
                let at = f64::from(i) * each - half[1];
                pen.line(Paint::Pitch, &[[-half[0], at], [half[0], at]]);
            }
        }
        return;
    }
    if line.len() < 2 || length < 10.0 || (closed && area == Some(true)) {
        // an area with the ground a loop goes round
        return;
    }
    let width = match (width, lanes) {
        (Some(width), Some(n)) if !(0.9..=1.6).contains(&(width / f64::from(n))) => {
            f64::from(n) * LANE
        }
        (Some(width), _) => width,
        (None, n) => f64::from(n.unwrap_or(LANES)) * LANE,
    };
    grounds.add(Kind::Track, material, ribbon(line, width, scale));
    let Some(n) = lanes else { return };
    for i in 0..=n {
        let at = width * (f64::from(i) / f64::from(n) - 0.5);
        grounds.lines.push(Line {
            paint: Paint::Pitch,
            points: geometry::offset(line, at * scale),
        });
    }
}

fn ribbon(line: &[Point], width: f64, scale: f64) -> Shapes<Point> {
    geometry::union(&geometry::ribbon(line, width * scale))
}

// Aerodromes.

/// Metres long a runway is at least that is painted.
const PAINTED: f64 = 300.0;

/// How many bars a threshold has on a runway so wide.
fn bars(width: f64) -> u32 {
    match width {
        w if w >= 60.0 => 16,
        w if w >= 45.0 => 12,
        w if w >= 30.0 => 8,
        w if w >= 23.0 => 6,
        w if w >= 18.0 => 4,
        _ => 0,
    }
}

/// On a runway so long: metres from its end to where one aims, and of the two
/// bars there their length and width and the ground between them.
fn aim(length: f64) -> [f64; 4] {
    match length {
        l if l < 800.0 => [150.0, 30.0, 4.0, 6.0],
        l if l < 1200.0 => [250.0, 30.0, 6.0, 9.0],
        l if l < 2400.0 => [300.0, 45.0, 6.0, 18.0],
        _ => [400.0, 45.0, 6.0, 18.0],
    }
}

/// A character's strokes, in a box 1 wide and 1 high.
fn strokes(c: char) -> &'static [&'static [Point]] {
    const ROUND: &[Point] = &[
        [0.3, 0.0],
        [0.0, 0.15],
        [0.0, 0.85],
        [0.3, 1.0],
        [0.7, 1.0],
        [1.0, 0.85],
        [1.0, 0.15],
        [0.7, 0.0],
        [0.3, 0.0],
    ];
    const MIDDLE: &[Point] = &[[0.0, 0.5], [1.0, 0.5]];
    match c {
        '0' => &[ROUND],
        '1' => &[&[[0.2, 0.8], [0.5, 1.0], [0.5, 0.0]]],
        '2' => &[&[
            [0.0, 0.85],
            [0.3, 1.0],
            [0.7, 1.0],
            [1.0, 0.85],
            [1.0, 0.6],
            [0.0, 0.0],
            [1.0, 0.0],
        ]],
        '3' => &[
            &[[0.0, 1.0], [1.0, 1.0], [1.0, 0.0], [0.0, 0.0]],
            &[[0.3, 0.5], [1.0, 0.5]],
        ],
        '4' => &[
            &[[0.6, 1.0], [0.0, 0.3], [1.0, 0.3]],
            &[[0.75, 0.6], [0.75, 0.0]],
        ],
        '5' => &[&[
            [1.0, 1.0],
            [0.0, 1.0],
            [0.0, 0.55],
            [1.0, 0.55],
            [1.0, 0.0],
            [0.0, 0.0],
        ]],
        '6' => &[&[
            [1.0, 1.0],
            [0.0, 1.0],
            [0.0, 0.0],
            [1.0, 0.0],
            [1.0, 0.5],
            [0.0, 0.5],
        ]],
        '7' => &[&[[0.0, 1.0], [1.0, 1.0], [0.4, 0.0]]],
        '8' => &[ROUND, MIDDLE],
        '9' => &[&[
            [1.0, 0.5],
            [0.0, 0.5],
            [0.0, 1.0],
            [1.0, 1.0],
            [1.0, 0.0],
            [0.0, 0.0],
        ]],
        'L' => &[&[[0.0, 1.0], [0.0, 0.0], [1.0, 0.0]]],
        'R' => &[
            &[[0.0, 0.0], [0.0, 1.0], [1.0, 1.0], [1.0, 0.5], [0.0, 0.5]],
            &[[0.5, 0.5], [1.0, 0.0]],
        ],
        'C' => &[&[[1.0, 1.0], [0.0, 1.0], [0.0, 0.0], [1.0, 0.0]]],
        _ => &[],
    }
}

/// The number of a runway's end, and its letter.
fn name(text: &str) -> Option<(u32, Option<char>)> {
    let digits = text.trim_end_matches(['L', 'R', 'C']);
    let letter = text[digits.len()..].chars().next();
    let number: u32 = digits.parse().ok()?;
    let one = text.len() - digits.len() <= 1 && digits.len() <= 2;
    (one && (1..=36).contains(&number)).then_some((number, letter))
}

/// The names of a runway's start and end, if they are of its direction, `heading`
/// (degrees clockwise from north) from its start.
fn named(names: &[String; 2], heading: f64) -> [Option<(u32, Option<char>)>; 2] {
    /// Degrees a name's is off: it is rounded, and from magnetic north.
    const WITHIN: f64 = 25.0;
    let read = [name(&names[0]), name(&names[1])];
    let off = |n: Option<(u32, Option<char>)>, heading: f64| {
        n.is_some_and(|(n, _)| {
            let off = (f64::from(n) * 10.0 - heading).rem_euclid(360.0);
            off.min(360.0 - off) <= WITHIN
        })
    };
    let back = heading + 180.0;
    if off(read[0], heading) && off(read[1], back) {
        read
    } else if off(read[1], heading) && off(read[0], back) {
        [read[1], read[0]]
    } else {
        [None; 2]
    }
}

/// Paints a runway, but for an end where it goes on as another.
fn paint_runway(
    lines: &mut Vec<Line>,
    line: &[Point],
    width: f64,
    names: Option<&[String; 2]>,
    goes_on: [bool; 2],
    scale: f64,
) {
    /// Metres from the end to the bars, along them, and on to the number; a
    /// character's height and width; and between two.
    const TO_BARS: f64 = 6.0;
    const BARS: f64 = 30.0;
    const BEYOND: f64 = 12.0;
    const CHARACTER: [f64; 2] = [12.0, 4.5];
    const APART: f64 = 2.5;
    /// Metres of a bar across, and between two.
    const BAR: f64 = 1.8;
    /// Metres of the middle line's dashes, and at least between them.
    const DASH: f64 = 30.0;
    const GAP: f64 = 20.0;
    let length = geometry::lengths(line, scale, 0.0)
        .last()
        .copied()
        .unwrap_or_default();
    // where its ends are, and which way it goes from them
    let frame = |end: usize, from: f64| -> Option<(Point, Point)> {
        let mut points = line.to_vec();
        if end == 1 {
            points.reverse();
        }
        let rest = geometry::trim(&points, [from * scale, 0.0])?;
        let ahead = geometry::trim(&rest, [60.0 * scale, 0.0])?;
        let long = distance(rest[0], ahead[0]);
        Some((
            rest[0],
            [
                (ahead[0][0] - rest[0][0]) / long,
                (ahead[0][1] - rest[0][1]) / long,
            ],
        ))
    };
    let Some((_, d)) = frame(0, 0.0) else { return };
    // north is up, where y is less
    let heading = d[0].atan2(-d[1]).to_degrees().rem_euclid(360.0);
    let names = names.map_or([None; 2], |n| named(n, heading));
    let mut from = [0.0; 2];
    for end in 0..2 {
        if goes_on[end] {
            continue;
        }
        let Some((at, towards)) = frame(end, 0.0) else {
            continue;
        };
        let mut pen = Pen::new(at, towards, scale, &mut *lines);
        let mut y = 0.0;
        let each = bars(width) / 2;
        for i in 0..each {
            let inner = BAR + f64::from(i) * 2.0 * BAR;
            for side in [-1.0, 1.0] {
                let across = [side * inner, side * (inner + BAR)];
                pen.rect(
                    Paint::RunwayBar,
                    [TO_BARS, across[0]],
                    [TO_BARS + BARS, across[1]],
                );
            }
        }
        if each > 0 {
            y = TO_BARS + BARS;
        }
        if let Some((number, letter)) = names[end] {
            let number = format!("{number:02}");
            // read from the approach, the letter is below
            for text in [letter.map(String::from), Some(number)]
                .into_iter()
                .flatten()
            {
                y += BEYOND;
                let count = text.len() as f64;
                let wide = count * CHARACTER[1] + (count - 1.0) * APART;
                for (i, c) in text.chars().enumerate() {
                    let left = i as f64 * (CHARACTER[1] + APART) - wide / 2.0;
                    for stroke in strokes(c) {
                        let points: Vec<Point> = stroke
                            .iter()
                            .map(|p| [y + p[1] * CHARACTER[0], left + p[0] * CHARACTER[1]])
                            .collect();
                        pen.line(Paint::Runway, &points);
                    }
                }
                y += CHARACTER[0];
            }
        }
        if y > 0.0 {
            from[end] = y + BEYOND;
        }
        let [far, long, wide, between] = aim(length);
        if between + 2.0 * wide + 3.0 <= width
            && 2.0 * (far + long) < length
            && let Some((at, towards)) = frame(end, far)
        {
            let mut pen = Pen::new(at, towards, scale, lines);
            for side in [-1.0, 1.0] {
                let inner = side * between / 2.0;
                let outer = side * (between / 2.0 + wide);
                pen.rect(Paint::RunwayBar, [0.0, inner], [long, outer]);
            }
        }
    }
    let room = length - from[0] - from[1];
    let number = ((room + GAP) / (DASH + GAP)).floor();
    let spare = room - (number * (DASH + GAP) - GAP);
    for i in 0..number.max(0.0) as u32 {
        let start = from[0] + spare / 2.0 + f64::from(i) * (DASH + GAP);
        let ends = [start, length - start - DASH].map(|m| m * scale);
        if let Some(points) = geometry::trim(line, ends) {
            lines.push(Line {
                paint: Paint::Runway,
                points,
            });
        }
    }
}

/// Paints a helipad's circle and its H.
fn paint_helipad(lines: &mut Vec<Line>, fit: &Fit, scale: f64) {
    if fit.fill < 0.7 || !(8.0..=60.0).contains(&fit.size[1]) {
        return;
    }
    let radius = (0.35 * fit.size[1]).min(8.0);
    let mut pen = Pen::new(fit.centre, fit.along, scale, lines);
    pen.arc(Paint::Helipad, [0.0, 0.0], radius, 0.0, TAU);
    let (x, y) = (radius * 0.5, radius * 0.3);
    for side in [-y, y] {
        pen.line(Paint::Helipad, &[[-x, side], [x, side]]);
    }
    pen.line(Paint::Helipad, &[[0.0, -y], [0.0, y]]);
}

// Car parks.

/// Metres across and along a bay: of those across their strip, and along it.
const ACROSS: [[f64; 2]; 2] = [[2.2, 3.2], [3.8, 7.0]];
const ALONG: [[f64; 2]; 2] = [[4.6, 7.5], [1.6, 3.2]];
/// How much of its rectangle a strip fills at least.
const STRIP: f64 = 0.9;

/// Draws the lines between a strip's bays, and at its ends.
fn draw_bays(lines: &mut Vec<Line>, fit: &Fit, capacity: u32, across: bool, scale: f64) {
    let [step, side] = if across { ACROSS } else { ALONG };
    let each = fit.size[0] / f64::from(capacity.max(1));
    let fits = (step[0]..=step[1]).contains(&each) && (side[0]..=side[1]).contains(&fit.size[1]);
    if capacity < 2 || fit.fill < STRIP || !fits {
        return;
    }
    let half = fit.size[1] / 2.0;
    let mut pen = Pen::new(fit.centre, fit.along, scale, lines);
    for i in 0..=capacity {
        let at = f64::from(i) * each - fit.size[0] / 2.0;
        pen.line(Paint::Bay, &[[at, -half], [at, half]]);
    }
}

/// Draws a wheelchair on a bay, upright to who drives in from `road`.
fn draw_wheelchair(lines: &mut Vec<Line>, fit: &Fit, road: Point, scale: f64) {
    if fit.size[1] < 1.8 || fit.size[0] < 3.0 {
        return;
    }
    let away = [fit.centre[0] - road[0], fit.centre[1] - road[1]];
    let forward = away[0] * fit.along[0] + away[1] * fit.along[1] >= 0.0;
    let up = if forward {
        fit.along
    } else {
        fit.along.map(|v| -v)
    };
    let mut pen = Pen::new(fit.centre, up, scale, lines);
    // up, and right: the head, the back, the seat and the leg
    let sitter = [
        [0.75, -0.15],
        [0.45, -0.15],
        [0.05, -0.1],
        [0.0, 0.3],
        [-0.55, 0.5],
    ];
    pen.line(Paint::Wheelchair, &sitter);
    // the wheel, open towards where the leg is
    let wheel = arc([-0.25, -0.1], 0.45, FRAC_PI_2 * 0.9, FRAC_PI_2 * 4.0);
    pen.line(Paint::Wheelchair, &wheel);
}

/// The nearest point of the roads, by the cells they lie in.
struct Near<'a> {
    ways: &'a [Way],
    cells: FxHashMap<(i64, i64), Vec<(u32, u32)>>,
}

impl<'a> Near<'a> {
    /// Roads about the points alone.
    fn new(ways: &'a [Way], points: &[Point]) -> Self {
        let mut cells: FxHashMap<(i64, i64), Vec<(u32, u32)>> = FxHashMap::default();
        for p in points {
            for cell in around(*p) {
                cells.entry(cell).or_default();
            }
        }
        for (i, way) in ways.iter().enumerate() {
            if way.profile.class.is_path() || cells.is_empty() {
                continue;
            }
            for (k, cell) in structures::cells(&way.points) {
                if let Some(found) = cells.get_mut(&cell) {
                    found.push((i as u32, k as u32));
                }
            }
        }
        Self { ways, cells }
    }

    fn to(&self, p: Point) -> Option<Point> {
        let mut best: Option<(f64, Point)> = None;
        for &(way, k) in around(p).iter().filter_map(|c| self.cells.get(c)).flatten() {
            let points = &self.ways[way as usize].points;
            let (a, b) = (points[k as usize], points[k as usize + 1]);
            let d = [b[0] - a[0], b[1] - a[1]];
            let long = (d[0] * d[0] + d[1] * d[1]).max(f64::MIN_POSITIVE);
            let t = (((p[0] - a[0]) * d[0] + (p[1] - a[1]) * d[1]) / long).clamp(0.0, 1.0);
            let at = [a[0] + d[0] * t, a[1] + d[1] * t];
            let far = distance(at, p);
            if best.is_none_or(|b| far < b.0) {
                best = Some((far, at));
            }
        }
        best.map(|b| b.1)
    }
}

/// The cells within half of one of a point.
fn around(p: Point) -> [(i64, i64); 4] {
    let half = structures::CELL / 2.0;
    [[-half, -half], [half, -half], [-half, half], [half, half]]
        .map(|d| structures::cell([p[0] + d[0], p[1] + d[1]]))
}

// Putting it together.

/// What of the grounds read is where: `ways` of their nodes, `polygons` of
/// their members, those of which `outlined` holds, and `locate` puts nodes on
/// the map, with world units a metre there.
pub(super) fn located(
    ways: Vec<(i64, Vec<i64>, Ground)>,
    polygons: Vec<(Ground, Vec<(i64, bool)>)>,
    outlined: &FxHashMap<i64, Vec<i64>>,
    mut locate: impl FnMut(&[i64]) -> Option<(Vec<Point>, f64)>,
) -> Vec<Mapped> {
    let mut out = Vec::new();
    for (_, nodes, ground) in ways {
        let Some((line, scale)) = locate(&nodes) else {
            continue;
        };
        let ends = [nodes[0], nodes[nodes.len() - 1]];
        let shapes = match furniture::ring(&line) {
            Some(ring) if ends[0] == ends[1] => vec![vec![ring]],
            _ => Vec::new(),
        };
        out.push(Mapped {
            ground,
            ends,
            line,
            shapes,
            scale,
        });
    }
    for (ground, members) in polygons {
        let mut scale = 0.0;
        let mut rings = [false, true].map(|outer| {
            let of = members.iter().filter(|m| m.1 == outer);
            let lines = of.filter_map(|m| outlined.get(&m.0).cloned());
            let mut rings = Vec::new();
            for nodes in furniture::joined(lines.collect()) {
                let Some((points, at)) = locate(&nodes) else {
                    continue;
                };
                scale = at;
                rings.extend(furniture::ring(&points));
            }
            rings
        });
        let outer = std::mem::take(&mut rings[1]);
        let [mut holes, _] = rings;
        holes.iter_mut().for_each(|ring| ring.reverse());
        let mut shapes: Shapes<Point> = outer.into_iter().map(|ring| vec![ring]).collect();
        // the first takes them all, as the shapes are joined
        match shapes.first_mut() {
            Some(first) => first.extend(holes),
            None => continue,
        }
        out.push(Mapped {
            ground,
            ends: [0; 2],
            line: Vec::new(),
            shapes,
            scale,
        });
    }
    out
}

impl Grounds {
    fn add(&mut self, kind: Kind, material: Material, shapes: Shapes<Point>) {
        let mut corners = [[f64::MAX; 2], [f64::MIN; 2]];
        for p in shapes.iter().flatten().flatten() {
            for i in 0..2 {
                corners[0][i] = corners[0][i].min(p[i]);
                corners[1][i] = corners[1][i].max(p[i]);
            }
        }
        if corners[0][0] <= corners[1][0] {
            self.surfaces.push(Surface {
                kind,
                material,
                shapes,
                corners,
            });
        }
    }
}

fn size(shapes: &Shapes<Point>) -> f64 {
    let ring = |r: &Vec<Point>| {
        let mut closed = r.clone();
        closed.push(r[0]);
        area(&closed)
    };
    let of = |shape: &Vec<Vec<Point>>| ring(&shape[0]) - shape[1..].iter().map(ring).sum::<f64>();
    shapes.iter().map(of).sum()
}

fn overlaps(a: &[Point; 2], b: &[Point; 2]) -> bool {
    (0..2).all(|i| a[0][i] < b[1][i] && b[0][i] < a[1][i])
}

/// The pitches and tracks each one overlaps the box of, smaller ones first.
fn fields(surfaces: &[Surface]) -> Vec<(usize, Vec<usize>)> {
    let sizes: Vec<f64> = surfaces.iter().map(|s| size(&s.shapes)).collect();
    let field = |s: &Surface| matches!(s.kind, Kind::Pitch | Kind::Track);
    let mut cells: FxHashMap<(i64, i64), Vec<usize>> = FxHashMap::default();
    let of = |s: &Surface| -> Vec<(i64, i64)> {
        let [a, b] = s.corners.map(structures::cell);
        (a.0..=b.0)
            .flat_map(|x| (a.1..=b.1).map(move |y| (x, y)))
            .collect()
    };
    for (i, s) in surfaces.iter().enumerate().filter(|(_, s)| field(s)) {
        for cell in of(s) {
            cells.entry(cell).or_default().push(i);
        }
    }
    let mut out = Vec::new();
    for (i, s) in surfaces.iter().enumerate().filter(|(_, s)| field(s)) {
        let smaller = |&j: &usize| (sizes[j], j) < (sizes[i], i);
        let mut found: Vec<usize> = of(s)
            .iter()
            .flat_map(|cell| &cells[cell])
            .copied()
            .filter(smaller)
            .filter(|&j| overlaps(&s.corners, &surfaces[j].corners))
            .collect();
        found.sort_unstable();
        found.dedup();
        if !found.is_empty() {
            out.push((i, found));
        }
    }
    out
}

/// What is drawn of the grounds. `areas` hold the bays that are mapped.
pub(super) fn build(mapped: &[Mapped], areas: &[Area], ways: &[Way]) -> Grounds {
    let mut grounds = Grounds::default();
    let runways: Vec<&Mapped> = mapped
        .iter()
        .filter(|m| matches!(m.ground, Ground::Runway { .. }))
        .collect();
    // a taxiway's is the nearest runway's that says
    let nearest = |m: &Mapped| -> Option<Material> {
        /// Metres within an aerodrome.
        const WITHIN: f64 = 4000.0;
        let from = *(m.line.first()).or(m.shapes.first().and_then(|s| s[0].first()))?;
        let found = runways.iter().filter_map(|r| {
            let Ground::Runway {
                material: Some(material),
                ..
            } = r.ground
            else {
                return None;
            };
            let far = r.line.iter().map(|p| distance(*p, from)).reduce(f64::min)?;
            (far < WITHIN * m.scale).then_some((far, material))
        });
        found.min_by(|a, b| a.0.total_cmp(&b.0)).map(|f| f.1)
    };
    let mut bays: FxHashMap<(i64, i64), Vec<Point>> = FxHashMap::default();
    for bay in areas.iter().filter(|a| a.kind == AreaKind::Bay) {
        let middle = middle(&bay.ring);
        bays.entry(structures::cell(middle))
            .or_default()
            .push(middle);
    }
    let wheelchairs: Vec<Point> = mapped
        .iter()
        .filter(|m| m.ground == Ground::Disabled)
        .filter_map(|m| Some(middle(&m.shapes.first()?[0])))
        .collect();
    let near = Near::new(ways, &wheelchairs);
    let mut taxiways = Vec::new();
    // each pitch's surface, and its lines
    let mut lined: Vec<(usize, Vec<Line>)> = Vec::new();
    for m in mapped {
        let closed = !m.shapes.is_empty();
        let fitted = || match &m.shapes[..] {
            [only] => {
                let mut ring = only[0].clone();
                ring.push(ring[0]);
                fit(&ring, m.scale)
            }
            _ => None,
        };
        match &m.ground {
            Ground::Pitch { material, sports } => {
                grounds.pitches += 1;
                let mut lines = Vec::new();
                let drawn = fitted()
                    .is_some_and(|fit| draw_pitch(&fit, sports, *material, m.scale, &mut lines));
                if drawn {
                    lined.push((grounds.surfaces.len(), lines));
                }
                grounds.add(Kind::Pitch, *material, m.shapes.clone());
            }
            Ground::Track { material, .. } => track(&mut grounds, m, *material),
            Ground::Runway {
                material,
                width,
                names,
                painted,
            } => {
                let kept = material.unwrap_or(Material::Plain);
                if closed {
                    grounds.add(Kind::Runway, kept, m.shapes.clone());
                    continue;
                }
                let length = geometry::lengths(&m.line, m.scale, 0.0);
                let length = length[length.len() - 1];
                // a tenth of its length at most, as model aircraft's are short
                let width = width.unwrap_or(45.0_f64.min(length / 10.0));
                grounds.add(Kind::Runway, kept, ribbon(&m.line, width, m.scale));
                let hard = *material == Some(Material::Hard);
                if hard && *painted && length >= PAINTED && width >= 15.0 {
                    let goes_on = m.ends.map(|node| {
                        let mut others = runways.iter().filter(|r| !std::ptr::eq(**r, m));
                        others.any(|r| r.ends.contains(&node))
                    });
                    let names = names.as_ref();
                    paint_runway(&mut grounds.lines, &m.line, width, names, goes_on, m.scale);
                }
            }
            Ground::Taxiway {
                material,
                width,
                usual,
            } => {
                let material = material.or_else(|| nearest(m));
                let kept = material.unwrap_or(Material::Plain);
                if closed {
                    grounds.add(Kind::Taxiway, kept, m.shapes.clone());
                    continue;
                }
                let hard = kept == Material::Hard;
                let width = width.unwrap_or(if hard { *usual } else { usual.min(15.0) });
                grounds.add(Kind::Taxiway, kept, ribbon(&m.line, width, m.scale));
                if hard {
                    taxiways.push(m);
                }
            }
            Ground::Apron { material } => {
                let material = material.or_else(|| nearest(m)).unwrap_or(Material::Plain);
                grounds.add(Kind::Apron, material, m.shapes.clone());
            }
            Ground::Helipad { material } => {
                /// Square metres of one that says nothing and is not a field.
                const PAD: f64 = 1500.0;
                let small = size(&m.shapes) < PAD * m.scale * m.scale;
                let usual = if small {
                    Material::Hard
                } else {
                    Material::Grass
                };
                let material = material.unwrap_or(usual);
                grounds.add(Kind::Helipad, material, m.shapes.clone());
                if let (true, Some(fit)) = (material.paved(), fitted()) {
                    paint_helipad(&mut grounds.lines, &fit, m.scale);
                }
            }
            Ground::Yard { material } => grounds.add(Kind::Yard, *material, m.shapes.clone()),
            Ground::Strip { capacity, across } => {
                let shapes = &m.shapes;
                let held = structures::cells(&m.line)
                    .filter_map(|(_, cell)| bays.get(&cell))
                    .flatten()
                    .any(|p| things::within(shapes, *p));
                if let (false, Some(fit)) = (held, fitted()) {
                    draw_bays(&mut grounds.lines, &fit, *capacity, *across, m.scale);
                }
            }
            Ground::Disabled => {
                let Some(fit) = fitted() else { continue };
                if let Some(road) = near.to(fit.centre) {
                    draw_wheelchair(&mut grounds.lines, &fit, road, m.scale);
                }
            }
        }
    }
    // taxiways' lines end where a runway begins
    let runways: Vec<Shapes<Point>> = grounds
        .surfaces
        .iter()
        .filter(|s| s.kind == Kind::Runway)
        .map(|s| s.shapes.clone())
        .collect();
    for m in taxiways {
        for points in outside(&m.line, &runways) {
            grounds.lines.push(Line {
                paint: Paint::Taxi,
                points,
            });
        }
    }
    // a smaller field is on a larger one, and not under its lines
    let mut cut = Vec::new();
    for (i, smaller) in fields(&grounds.surfaces) {
        let s = &grounds.surfaces[i];
        let whole = size(&s.shapes);
        let holds = |&j: &usize| {
            let other = &grounds.surfaces[j];
            let [a, b] = other.corners;
            let middle = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
            size(&other.shapes) >= 0.05 * whole && things::within(&s.shapes, middle)
        };
        if smaller.iter().any(holds) {
            lined.retain(|l| l.0 != i);
        }
        let taken: Shapes<Point> = smaller
            .iter()
            .map(|&j| &grounds.surfaces[j])
            .filter(|o| (o.kind, o.material) != (s.kind, s.material))
            .flat_map(|o| o.shapes.clone())
            .collect();
        if !taken.is_empty() {
            cut.push((i, geometry::without(&s.shapes, &taken)));
        }
    }
    for (i, shapes) in cut {
        grounds.surfaces[i].shapes = shapes;
    }
    grounds.lined = lined.len() as u64;
    grounds.lines.extend(lined.into_iter().flat_map(|l| l.1));
    grounds
}

fn middle(ring: &[Point]) -> Point {
    let sum = ring
        .iter()
        .fold([0.0; 2], |s, p| [s[0] + p[0], s[1] + p[1]]);
    sum.map(|v| v / ring.len() as f64)
}

/// The parts of a line that none of `taken` covers.
fn outside(points: &[Point], taken: &[Shapes<Point>]) -> Vec<Vec<Point>> {
    let mut out: Vec<Vec<Point>> = Vec::new();
    let mut open = false;
    for p in points.windows(2) {
        let (a, b) = (p[0], p[1]);
        let mut cuts = vec![0.0];
        cuts.extend(
            taken
                .iter()
                .flat_map(|shapes| things::crossings(shapes, a, b)),
        );
        cuts.retain(|t| (0.0..1.0).contains(t));
        cuts.push(1.0);
        cuts.sort_by(f64::total_cmp);
        let at = |t: f64| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
        for c in cuts.windows(2) {
            if c[1] <= c[0] {
                continue;
            }
            let middle = at((c[0] + c[1]) / 2.0);
            if taken.iter().any(|shapes| things::within(shapes, middle)) {
                open = false;
                continue;
            }
            match out.last_mut() {
                Some(run) if open => run.push(at(c[1])),
                _ => out.push(vec![at(c[0]), at(c[1])]),
            }
            open = true;
        }
    }
    out
}

// Tiles.

/// Metres of a line that are not worth painting.
const STUB: f64 = 0.3;

impl Grounds {
    /// Adds to a tile's `surface` layer those that reach it, and says how many.
    pub(super) fn draw_surfaces(
        &self,
        which: &[u32],
        rect: Rect,
        origin: Point,
        mut layer: MvtLayerBuilder,
    ) -> Result<(MvtLayerBuilder, u64), MvtError> {
        let mut groups: BTreeMap<(Kind, Material), Shapes<Point>> = BTreeMap::new();
        for &i in which {
            let s = &self.surfaces[i as usize];
            let shapes = s.shapes.iter().cloned();
            groups
                .entry((s.kind, s.material))
                .or_default()
                .extend(shapes);
        }
        let mut count = 0;
        for ((kind, material), shapes) in groups {
            let shapes = geometry::union(&shapes);
            for polygon in geometry::to_tile(&shapes, rect, origin, super::MIN_AREA) {
                let mut feature = layer.feature_of(MvtGeomType::Polygon);
                feature.tag_string("kind", kind.name())?;
                feature.tag_string("level", "ground")?;
                feature.tag_sint("layer", 0)?;
                feature.tag_bool("paved", material.paved())?;
                feature.tag_string("material", material.name())?;
                for (i, ring) in polygon.iter().enumerate() {
                    let points = ring.iter().map(|p| MvtCoord { x: p[0], y: p[1] });
                    feature.ring(points, i == 0)?;
                }
                layer = feature.end();
                count += 1;
            }
        }
        Ok((layer, count))
    }

    /// Adds to a tile's `marking` layer the lines that reach it, but where a
    /// carriageway of `roads` is, and says how many.
    pub(super) fn draw_lines(
        &self,
        which: &[u32],
        rect: Rect,
        origin: Point,
        roads: &[Shapes<Point>],
        mut layer: MvtLayerBuilder,
    ) -> Result<(MvtLayerBuilder, u64), MvtError> {
        let mut count = 0;
        for &i in which {
            let line = &self.lines[i as usize];
            let bar = line.paint == Paint::RunwayBar;
            let dashed = line.paint == Paint::PitchDashed;
            let parts = if bar {
                let on = |p: &Point| roads.iter().any(|shapes| things::within(shapes, *p));
                if line.points.iter().any(on) {
                    continue;
                }
                // its last point is its first
                vec![line.points[..line.points.len() - 1].to_vec()]
            } else if dashed {
                // whole, so that its dashes go on over the tile's edge
                outside(&line.points, roads)
            } else {
                let within = geometry::clip(&line.points, rect);
                within.iter().flat_map(|l| outside(l, roads)).collect()
            };
            for part in parts {
                let long: f64 = part.windows(2).map(|p| distance(p[0], p[1])).sum();
                let mut points: Vec<MvtCoord> = part
                    .iter()
                    .map(|p| MvtCoord {
                        x: (p[0] - origin[0]).round() as i32,
                        y: (p[1] - origin[1]).round() as i32,
                    })
                    .collect();
                points.dedup();
                // about ten units a metre
                if points.len() < if bar { 3 } else { 2 } || long < STUB * 10.0 {
                    continue;
                }
                let of = if bar {
                    MvtGeomType::Polygon
                } else {
                    MvtGeomType::LineString
                };
                let mut feature = layer.feature_of(of);
                feature.tag_string("kind", line.paint.name())?;
                if line.paint.name() == "pitch" {
                    feature.tag_bool("dashed", dashed)?;
                }
                feature.tag_string("level", "ground")?;
                feature.tag_sint("layer", 0)?;
                feature.tag_uint("confidence", line.paint.confidence())?;
                if bar {
                    feature.ring(points, true)?;
                } else {
                    feature.line(points)?;
                }
                layer = feature.end();
                count += 1;
            }
        }
        Ok((layer, count))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn read(tags: &[(&str, &'static str)], closed: bool) -> Option<Ground> {
        ground(|k| tags.iter().find(|t| t.0 == k).map(|t| t.1), closed)
    }

    /// A rectangle `long` by `short` metres about `centre`, turned `angle`.
    fn rectangle(centre: Point, [long, short]: [f64; 2], angle: f64) -> Vec<Point> {
        let (sin, cos) = angle.sin_cos();
        let corners = [[-1.0, -1.0], [1.0, -1.0], [1.0, 1.0], [-1.0, 1.0]];
        corners
            .map(|[x, y]| {
                let (x, y) = (x * long / 2.0, y * short / 2.0);
                [centre[0] + x * cos - y * sin, centre[1] + x * sin + y * cos]
            })
            .to_vec()
    }

    fn pitch(ring: Vec<Point>, material: Material, sports: Vec<Sport>) -> Mapped {
        let ring = furniture::ring(&ring).unwrap();
        let mut line = ring.clone();
        line.push(ring[0]);
        Mapped {
            ground: Ground::Pitch { material, sports },
            ends: [1, 1],
            line,
            shapes: vec![vec![ring]],
            scale: 1.0,
        }
    }

    #[test]
    fn a_pitch_is_made_of_what_its_tag_or_its_sport_says() {
        let material = |tags: &[(&str, &'static str)]| match read(tags, true) {
            Some(Ground::Pitch { material, .. }) => Some(material),
            _ => None,
        };
        let pitch = ("leisure", "pitch");
        assert_eq!(
            material(&[pitch, ("sport", "soccer")]),
            Some(Material::Grass)
        );
        assert_eq!(
            material(&[pitch, ("sport", "tennis")]),
            Some(Material::Clay)
        );
        assert_eq!(
            material(&[pitch, ("sport", "tennis"), ("surface", "tartan")]),
            Some(Material::Rubber)
        );
        assert_eq!(material(&[pitch]), Some(Material::Plain));
        // a table, a roof, a line
        assert_eq!(material(&[pitch, ("sport", "table_tennis")]), None);
        assert_eq!(material(&[pitch, ("covered", "yes")]), None);
        assert_eq!(read(&[pitch], false), None);
        let Some(Ground::Pitch { sports, .. }) =
            read(&[pitch, ("sport", "soccer; basketball;chess")], true)
        else {
            panic!("a pitch");
        };
        assert_eq!(sports, [Sport::Soccer, Sport::Basketball]);
        // tracks to run on alone
        assert!(read(&[("leisure", "track"), ("sport", "running")], false).is_some());
        assert!(read(&[("leisure", "track"), ("sport", "cycling")], false).is_none());
    }

    #[test]
    fn aerodromes_and_car_parks_are_read_as_tagged() {
        let Some(Ground::Runway { names, width, .. }) = read(
            &[("aeroway", "runway"), ("ref", "07/25"), ("width", "60")],
            false,
        ) else {
            panic!("a runway");
        };
        assert_eq!(
            (names.unwrap(), width),
            (["07".into(), "25".into()], Some(60.0))
        );
        let strip = |o: &'static str| {
            let tags = [
                ("amenity", "parking"),
                ("capacity", "12"),
                ("orientation", o),
            ];
            read(&tags, true)
        };
        assert_eq!(
            strip("perpendicular"),
            Some(Ground::Strip {
                capacity: 12,
                across: true
            })
        );
        assert_eq!(strip("diagonal"), None);
        assert_eq!(read(&[("amenity", "parking")], true), None);
        let disabled = [("amenity", "parking_space"), ("parking_space", "disabled")];
        assert_eq!(read(&disabled, true), Some(Ground::Disabled));
        let fuel = [("amenity", "fuel"), ("building", "roof")];
        assert_eq!(read(&fuel, true), None);
        assert_eq!(read(&[("highway", "services")], true), None);
        assert!(matches!(
            read(&[("highway", "rest_area"), ("surface", "asphalt")], true),
            Some(Ground::Yard { .. })
        ));
        assert!(matches!(
            read(&[("amenity", "fuel")], true),
            Some(Ground::Yard { .. })
        ));
    }

    #[test]
    fn the_least_rectangle_round_a_turned_one_is_itself() {
        let mut ring = rectangle([100.0, 50.0], [40.0, 20.0], 0.4);
        ring.insert(
            1,
            [
                ring[0][0] * 0.5 + ring[1][0] * 0.5,
                ring[0][1] * 0.5 + ring[1][1] * 0.5,
            ],
        );
        let fit = fit(&ring, 1.0).unwrap();
        assert!(
            (fit.size[0] - 40.0).abs() < 1e-6 && (fit.size[1] - 20.0).abs() < 1e-6,
            "{fit:?}"
        );
        assert!((fit.fill - 1.0).abs() < 1e-9);
        assert!(distance(fit.centre, [100.0, 50.0]) < 1e-6);
        assert!((fit.along[1].atan2(fit.along[0]).rem_euclid(PI) - 0.4).abs() < 1e-6);
    }

    #[test]
    fn courts_fit_or_are_left_out() {
        let tennis = court(Sport::Tennis).unwrap();
        assert_eq!(courts(&tennis, [24.0, 11.0]), Some((1, true)));
        assert_eq!(courts(&tennis, [36.0, 18.0]), Some((1, true)));
        // three side by side, along the short side
        assert_eq!(courts(&tennis, [50.0, 36.0]), Some((3, false)));
        // two either way
        assert_eq!(courts(&tennis, [36.0, 35.0]), None);
        assert_eq!(courts(&tennis, [15.0, 8.0]), None);
        let basketball = court(Sport::Basketball).unwrap();
        assert_eq!(courts(&basketball, [28.0, 15.0]), Some((1, true)));
        assert_eq!(courts(&basketball, [45.0, 40.0]), None);
        assert_eq!(
            football([105.0, 68.0], Material::Grass),
            Some(Football::Areas([105.0, 68.0], 1.0))
        );
        assert!(
            matches!(football([60.0, 40.0], Material::Grass), Some(Football::Areas(_, p)) if p < 0.6)
        );
        assert_eq!(football([40.0, 20.0], Material::Grass), None);
        assert!(matches!(
            football([40.0, 20.0], Material::Turf),
            Some(Football::Middle(..))
        ));
        assert_eq!(football([200.0, 60.0], Material::Grass), None);
    }

    #[test]
    fn a_pitchs_lines_are_within_it() {
        for (size, sports) in [
            ([105.0, 68.0], vec![Sport::Soccer]),
            ([30.0, 17.0], vec![Sport::Soccer, Sport::Basketball]),
            ([36.0, 18.0], vec![Sport::Tennis]),
            ([24.0, 13.0], vec![Sport::Volleyball]),
            ([40.0, 20.0], vec![Sport::Handball]),
        ] {
            let ring = rectangle([500.0, 300.0], size, 1.1);
            let m = pitch(ring.clone(), Material::Rubber, sports.clone());
            let grounds = build(&[m], &[], &[]);
            assert_eq!(grounds.lined, 1, "{sports:?}");
            let inside = |p: &Point| {
                let fit = fit(&ring, 1.0).unwrap();
                let d = [p[0] - fit.centre[0], p[1] - fit.centre[1]];
                let u = d[0] * fit.along[0] + d[1] * fit.along[1];
                let v = -d[0] * fit.along[1] + d[1] * fit.along[0];
                u.abs() <= size[0] / 2.0 + 1e-6 && v.abs() <= size[1] / 2.0 + 1e-6
            };
            assert!(grounds.lines.len() >= 3);
            for line in &grounds.lines {
                assert!(line.points.iter().all(inside), "{sports:?} {line:?}");
            }
        }
        // not a rectangle, and too large
        let mut ring = rectangle([0.0, 0.0], [105.0, 68.0], 0.0);
        ring[2] = [20.0, 34.0];
        assert_eq!(
            build(
                &[pitch(ring, Material::Grass, vec![Sport::Soccer])],
                &[],
                &[]
            )
            .lined,
            0
        );
        let ring = rectangle([0.0, 0.0], [160.0, 68.0], 0.0);
        assert_eq!(
            build(
                &[pitch(ring, Material::Grass, vec![Sport::Soccer])],
                &[],
                &[]
            )
            .lined,
            0
        );
    }

    #[test]
    fn a_pitch_on_another_cuts_it_and_takes_its_lines() {
        let big = pitch(
            rectangle([0.0, 0.0], [105.0, 68.0], 0.0),
            Material::Grass,
            vec![Sport::Soccer],
        );
        let small = pitch(
            rectangle([20.0, 0.0], [28.0, 15.0], 0.0),
            Material::Hard,
            vec![Sport::Basketball],
        );
        let grounds = build(&[big, small], &[], &[]);
        assert_eq!((grounds.pitches, grounds.lined), (2, 1));
        let left = size(&grounds.surfaces[0].shapes);
        assert!((left - (105.0 * 68.0 - 28.0 * 15.0)).abs() < 1.0, "{left}");
    }

    #[test]
    fn a_runway_is_painted_from_its_names() {
        assert_eq!(name("09R"), Some((9, Some('R'))));
        assert_eq!(name("36"), Some((36, None)));
        assert_eq!(name("H1"), None);
        let names = ["07".to_owned(), "25".to_owned()];
        assert_eq!(named(&names, 72.0), [Some((7, None)), Some((25, None))]);
        assert_eq!(named(&names, 250.0), [Some((25, None)), Some((7, None))]);
        assert_eq!(named(&names, 160.0), [None, None]);
        // eastwards, 2500 m long and 45 wide
        let line = [[0.0, 0.0], [1200.0, 0.0], [2500.0, 0.0]];
        let mut lines = Vec::new();
        paint_runway(
            &mut lines,
            &line,
            45.0,
            Some(&["09".into(), "27".into()]),
            [false; 2],
            1.0,
        );
        let count = |paint: Paint| lines.iter().filter(|l| l.paint == paint).count();
        // twelve bars at each end, and two where one aims
        assert_eq!(count(Paint::RunwayBar), 2 * (12 + 2));
        let dashes: Vec<&Line> = lines
            .iter()
            .filter(|l| l.points.len() == 2 && l.paint == Paint::Runway && l.points[0][1] == 0.0)
            .collect();
        assert!(dashes.len() > 40);
        // the numbers, and before them nothing on the middle line
        let first = dashes
            .iter()
            .map(|l| l.points[0][0])
            .fold(f64::MAX, f64::min);
        let numbers = lines
            .iter()
            .filter(|l| l.paint == Paint::Runway && l.points[0][1] != 0.0);
        let reach = numbers
            .flat_map(|l| &l.points)
            .map(|p| p[0])
            .filter(|x| *x < 1250.0)
            .fold(0.0, f64::max);
        assert!(reach > 50.0 && first > reach + 11.9, "{reach} {first}");
        for dash in dashes {
            let long = distance(dash.points[0], dash.points[1]);
            assert!((long - 30.0).abs() < 1e-6);
        }
        // where it goes on as another, no threshold and no aiming point there
        let mut lines = Vec::new();
        paint_runway(&mut lines, &line, 45.0, None, [true, false], 1.0);
        assert_eq!(
            lines.iter().filter(|l| l.paint == Paint::RunwayBar).count(),
            12 + 2
        );
    }

    #[test]
    fn bays_are_drawn_only_where_the_strip_holds_them() {
        let ring = rectangle([0.0, 0.0], [25.0, 5.0], 0.3);
        let strip = fit(&ring, 1.0).unwrap();
        let mut lines = Vec::new();
        draw_bays(&mut lines, &strip, 10, true, 1.0);
        assert_eq!(lines.len(), 11);
        assert!(
            lines
                .iter()
                .all(|l| (distance(l.points[0], l.points[1]) - 5.0).abs() < 1e-6)
        );
        // 5 m a bay, and along it
        for (capacity, across) in [(5, true), (10, false)] {
            let mut lines = Vec::new();
            draw_bays(&mut lines, &strip, capacity, across, 1.0);
            assert!(lines.is_empty());
        }
        let ring = rectangle([0.0, 0.0], [5.0, 2.5], 0.0);
        let mut lines = Vec::new();
        draw_wheelchair(&mut lines, &fit(&ring, 1.0).unwrap(), [-20.0, 0.0], 1.0);
        // its head is away from the road
        assert_eq!(lines.len(), 2);
        assert!(lines[0].points[0][0] > 0.5);
    }

    #[test]
    fn a_line_is_cut_where_it_is_covered() {
        let square = vec![vec![rectangle([0.0, 0.0], [10.0, 10.0], 0.0)]];
        let parts = outside(&[[-20.0, 0.0], [20.0, 0.0]], std::slice::from_ref(&square));
        assert_eq!(
            parts,
            [
                vec![[-20.0, 0.0], [-5.0, 0.0]],
                vec![[5.0, 0.0], [20.0, 0.0]]
            ]
        );
        assert_eq!(outside(&[[-1.0, 0.0], [1.0, 0.0]], &[square]).len(), 0);
    }
}
