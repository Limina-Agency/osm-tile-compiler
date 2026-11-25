//! A road's cross-section, read from its tags: how wide, which lanes, what is
//! painted between them, and how far the tags support it.

use super::country::{self, Rules};

/// What a way is drawn as. Motor roads come first, most important first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Class {
    Motorway,
    Trunk,
    Primary,
    Secondary,
    Tertiary,
    Minor,
    LivingStreet,
    Service,
    Cycleway,
    Sidewalk,
    /// Where one walks, and no sidewalk.
    Footway,
    Path,
    Pedestrian,
    Steps,
    Bridleway,
    /// A track of a railway, and of a tram (`rails.rs`).
    Rail,
    Tram,
}

impl Class {
    pub(super) fn is_motor(self) -> bool {
        self <= Self::Service
    }

    /// Whether it is for walking: paths, but for sidewalks and cycleways.
    pub(super) fn is_path(self) -> bool {
        matches!(
            self,
            Self::Footway | Self::Path | Self::Pedestrian | Self::Steps | Self::Bridleway
        )
    }

    pub(super) fn is_rail(self) -> bool {
        matches!(self, Self::Rail | Self::Tram)
    }

    /// Metres across one lane where nothing says otherwise.
    pub(super) fn lane_width(self) -> f64 {
        match self {
            Self::Motorway => 3.75,
            Self::Trunk | Self::Primary => 3.5,
            Self::Secondary => 3.25,
            Self::Tertiary | Self::Service => 3.0,
            Self::Minor => 2.75,
            Self::LivingStreet => 2.5,
            Self::Pedestrian => 5.0,
            Self::Path | Self::Bridleway => 1.5,
            Self::Cycleway | Self::Sidewalk | Self::Footway | Self::Steps => 2.0,
            Self::Rail | Self::Tram => 2.0,
        }
    }
}

/// Where a surface lies: ground roads join up, a bridge or tunnel joins only
/// with others of its `layer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Level {
    Tunnel(i8),
    Ground,
    Bridge(i8),
}

/// How far the tags support the lanes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Confidence {
    /// The class's usual lanes and width.
    Assumed,
    /// Lanes are tagged, the width is not.
    Lanes,
    /// Lanes and width are tagged.
    Full,
}

impl Confidence {
    pub(super) fn id(self) -> u64 {
        match self {
            Self::Assumed => 0,
            Self::Lanes => 1,
            Self::Full => 2,
        }
    }
}

/// What a painted line separates.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Line {
    /// The carriageway's edge.
    Edge,
    /// Lanes of one direction, and those it is not allowed to change between.
    Lane,
    LaneSolid,
    /// The two directions, where crossing over is allowed.
    Centre,
    /// The two directions, where it is not.
    CentreSolid,
    /// Not painted: one of a track's two.
    Rail,
    /// Beside a cycle lane: one not to be crossed, and one that may be.
    Cycle,
    CycleDashed,
    /// Beside a bus lane: solid and wide.
    Bus,
}

impl Line {
    pub(super) fn kind(self) -> &'static str {
        match self {
            Self::Edge => "edge",
            Self::Lane | Self::LaneSolid => "lane",
            Self::Centre | Self::CentreSolid => "centre",
            Self::Rail => "rail",
            Self::Cycle | Self::CycleDashed => "cycle",
            Self::Bus => "bus",
        }
    }

    pub(super) fn dashed(self) -> bool {
        matches!(self, Self::Lane | Self::Centre | Self::CycleDashed)
    }
}

/// Where a lane's arrow points.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(super) enum Turn {
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
}

impl Turn {
    /// Whether it says that its lane ends.
    pub(super) fn merges(self) -> bool {
        matches!(self, Self::MergeLeft | Self::MergeRight)
    }

    /// Whether it is a turn to the left, or to the right.
    pub(super) fn turns(self, left: bool) -> bool {
        match self {
            Self::Left | Self::SlightLeft | Self::SharpLeft => left,
            Self::Right | Self::SlightRight | Self::SharpRight => !left,
            _ => false,
        }
    }

    /// Whether it leads off the road.
    pub(super) fn leaves(self) -> bool {
        !self.merges() && self != Self::Through
    }

    fn read(text: &str) -> Option<Self> {
        Some(match text.trim() {
            "through" => Self::Through,
            "slight_left" => Self::SlightLeft,
            "left" => Self::Left,
            "sharp_left" => Self::SharpLeft,
            "slight_right" => Self::SlightRight,
            "right" => Self::Right,
            "sharp_right" => Self::SharpRight,
            "reverse" => Self::Reverse,
            "merge_to_left" => Self::MergeLeft,
            "merge_to_right" => Self::MergeRight,
            _ => return None,
        })
    }
}

/// A motor road's lanes, metres across as [`Profile`]'s.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Lanes {
    /// The left edge of the leftmost.
    pub left: f64,
    /// Metres across one.
    pub width: f64,
    /// How many run against the way (the left ones, or where traffic keeps
    /// left the right ones), and along it. One lane of a two-way road is
    /// counted along it.
    pub backward: u32,
    pub forward: u32,
    /// Whether traffic goes along the way, and against it.
    pub along: bool,
    pub against: bool,
    /// The arrows of each lane along the way and against it, from the driver's
    /// left, where they are tagged for as many lanes as there are.
    pub turns: [Vec<Vec<Turn>>; 2],
    /// Of each lane, left to right, whether it is for buses; empty if none is.
    pub bus: Vec<bool>,
    /// Whether traffic keeps left.
    pub left_hand: bool,
}

impl Lanes {
    /// Metres across that traffic along the way (`true`) or against it takes.
    pub(super) fn span(&self, along: bool) -> [f64; 2] {
        let at = |lane: f64| self.left + lane * self.width;
        let all = f64::from(self.backward + self.forward);
        let parts = match (self.along, self.against, self.backward) {
            // one lane for both
            (true, true, 0) => all / 2.0,
            (true, true, _) => f64::from(self.middle()),
            // one way
            _ => return [at(0.0), at(all)],
        };
        if along != self.left_hand {
            [at(parts), at(all)]
        } else {
            [at(0.0), at(parts)]
        }
    }

    /// The edge, from the left, between the two directions' lanes.
    pub(super) fn middle(&self) -> u32 {
        if self.left_hand {
            self.forward
        } else {
            self.backward
        }
    }

    /// Which lanes, from the left, traffic along the way (`true`) or against it
    /// takes: of one lane for both, all.
    pub(super) fn of(&self, along: bool) -> std::ops::Range<usize> {
        let (all, middle) = (
            (self.backward + self.forward) as usize,
            self.middle() as usize,
        );
        match (
            self.along && self.against && self.backward > 0,
            along != self.left_hand,
        ) {
            (true, true) => middle..all,
            (true, false) => 0..middle,
            _ => 0..all,
        }
    }

    /// Which way the lane `i` from the left goes, along (1), against (-1) or both
    /// (0), and which of its direction's lanes it is from its drivers' left.
    pub(super) fn lane(&self, i: u32) -> (i8, usize) {
        let count = self.backward + self.forward;
        let against = if self.left_hand {
            i >= self.forward
        } else {
            i < self.backward
        };
        match (self.along, self.against) {
            (true, true) if self.backward == 0 => (0, 0),
            (true, true) if against && self.left_hand => (-1, (count - 1 - i) as usize),
            (true, true) if against => (-1, (self.backward - 1 - i) as usize),
            (true, _) if self.left_hand => (1, i as usize),
            (true, _) => (1, (i - self.backward) as usize),
            (false, _) => (-1, (count - 1 - i) as usize),
        }
    }
}

/// What is painted in a lane for cyclists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Symbol {
    /// A bicycle, in a cycle lane.
    Bicycle,
    /// A bicycle and two chevrons, in a lane cyclists share.
    Sharrow,
}

/// How a parking lane's bays stand to the road, where it is tagged.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Bays {
    Parallel,
    Diagonal,
    Perpendicular,
}

/// What lies on one side of a motor road's lanes, from them outwards: metres
/// across, 0 where there is none.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(super) struct Side {
    /// A cycle lane in the carriageway, and whether its line may be crossed.
    pub cycle: f64,
    pub advisory: bool,
    /// What is painted for cyclists, and whether they ride along the way.
    pub symbol: Option<Symbol>,
    pub along: bool,
    /// Parking in the carriageway, and its bays.
    pub parking: f64,
    pub bays: Option<Bays>,
    /// A hard shoulder.
    pub shoulder: f64,
    /// Out past the kerb (and the parking beside it): a cycle track, then a
    /// sidewalk.
    pub track: f64,
    pub sidewalk: f64,
}

impl Side {
    /// Metres it adds to the carriageway.
    pub(super) fn inside(&self) -> f64 {
        self.cycle + self.parking + self.shoulder
    }
}

/// What says where in its cross-section the way lies.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Placed {
    /// Nothing: in the middle, or where its lanes go on from its neighbours'.
    Unsaid,
    /// `placement` does.
    Said,
    /// `placement=transition`: from where it was to where it will be.
    Transition,
}

/// Metres, across the way and to the right of its direction.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Profile {
    pub class: Class,
    pub level: Level,
    pub paved: bool,
    pub confidence: Confidence,
    /// The surface's whole width, shoulders included, and whether a tag gives it.
    pub width: f64,
    pub measured: bool,
    /// The surface's middle.
    pub centre: f64,
    /// Painted lines, left to right.
    pub lines: Vec<(f64, Line)>,
    /// A motor road's.
    pub lanes: Option<Box<Lanes>>,
    /// Metres of parking beside the surface, on its left and right.
    pub parking: [f64; 2],
    /// What lies beside its lanes, left and right (a motor road's).
    pub sides: [Side; 2],
    /// What a road is paved with, where a style may tell it: `sett`, `cobble`,
    /// `slabs` or `concrete`.
    pub paving: Option<&'static str>,
    pub roundabout: bool,
    pub placed: Placed,
    /// Km/h, tagged or the class's: how gently lanes open and close.
    pub speed: f64,
    /// Its name and its number, as hashes: 0 if it has none.
    pub named: [u64; 2],
    /// How roads look where it is.
    pub rules: &'static Rules,
}

impl Profile {
    /// Moves the whole cross-section right.
    pub(super) fn shift(&mut self, metres: f64) {
        self.centre += metres;
        for line in &mut self.lines {
            line.0 += metres;
        }
        if let Some(lanes) = &mut self.lanes {
            lanes.left += metres;
        }
    }

    /// Narrows the surface to between `edges`, all of it in proportion: its
    /// lanes, its lines and what is beside them in the carriageway.
    pub(super) fn narrow(&mut self, [left, right]: [f64; 2]) {
        let [was_left, was_right] = self.surface();
        let share = (right - left) / (was_right - was_left);
        let at = |x: f64| left + (x - was_left) * share;
        (self.centre, self.width) = ((left + right) / 2.0, right - left);
        for line in &mut self.lines {
            line.0 = at(line.0);
        }
        if let Some(lanes) = &mut self.lanes {
            lanes.left = at(lanes.left);
            lanes.width *= share;
        }
        for side in &mut self.sides {
            side.cycle *= share;
            side.parking *= share;
            side.shoulder *= share;
        }
    }

    /// The surface's edges.
    pub(super) fn surface(&self) -> [f64; 2] {
        [
            self.centre - self.width / 2.0,
            self.centre + self.width / 2.0,
        ]
    }

    /// Metres past the surface's edge that what is beside it reaches, on the
    /// further side.
    pub(super) fn beside(&self) -> f64 {
        let out = |i: usize| self.parking[i] + self.sides[i].track + self.sides[i].sidewalk;
        out(0).max(out(1))
    }

    /// Whether anything of its sides, or a bus lane, is drawn.
    pub(super) fn has_sides(&self) -> bool {
        let side =
            |s: &Side| s.cycle + s.parking + s.track + s.sidewalk > 0.0 || s.symbol.is_some();
        self.sides.iter().any(side) || self.lanes.as_ref().is_some_and(|l| !l.bus.is_empty())
    }
}

/// Metres of hard shoulder right of a motorway's lanes, and left of them.
const SHOULDER: (f64, f64) = (2.5, 0.5);
/// Metres of a tagged shoulder: of a motorway or trunk road, and of another.
const SHOULDERS: (f64, f64) = (2.5, 1.5);
/// Metres across a cycle lane, a cycle track (one way and both), a sidewalk,
/// where no tag says; and the widths of each that are believed.
const CYCLE_LANE: f64 = 1.5;
const TRACK: (f64, f64) = (1.5, 2.0);
const SIDEWALK: f64 = 2.0;
const SIDE_WIDTHS: std::ops::RangeInclusive<f64> = 0.5..=6.0;
const MAX_LANES: u32 = 12;
/// Lane widths a tagged `width` may imply; outside them it is not believed.
const LANE_WIDTHS: std::ops::RangeInclusive<f64> = 2.3..=5.5;

const UNPAVED: [&str; 14] = [
    "unpaved",
    "compacted",
    "dirt",
    "earth",
    "fine_gravel",
    "grass",
    "gravel",
    "ground",
    "mud",
    "pebblestone",
    "rock",
    "sand",
    "grade3",
    "woodchips",
];

/// The profile of a way with these tags, if it is drawn, where the table by
/// country says nothing.
pub(super) fn profile<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Profile> {
    profile_in(get, &country::DEFAULT)
}

/// The profile of a way with these tags in a country of these `rules`.
pub(super) fn profile_in<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
    rules: &'static Rules,
) -> Option<Profile> {
    let highway = get("highway")?;
    if get("area") == Some("yes") {
        return None;
    }
    let link = highway.ends_with("_link");
    let class = match highway.strip_suffix("_link").unwrap_or(highway) {
        "motorway" => Class::Motorway,
        "trunk" => Class::Trunk,
        "primary" => Class::Primary,
        "secondary" => Class::Secondary,
        "tertiary" => Class::Tertiary,
        "residential" | "unclassified" | "road" => Class::Minor,
        "living_street" => Class::LivingStreet,
        "service" => Class::Service,
        "cycleway" => Class::Cycleway,
        "footway" if get("footway") == Some("sidewalk") => Class::Sidewalk,
        "footway" => Class::Footway,
        "path" => Class::Path,
        "pedestrian" => Class::Pedestrian,
        "steps" => Class::Steps,
        "bridleway" => Class::Bridleway,
        _ => return None,
    };
    let level = if class.is_path() {
        path_level(&get, class)?
    } else {
        level(&get)
    };
    let paved = match class {
        // of a path nothing is said of it is trodden
        Class::Path | Class::Bridleway if get("surface").is_none() => false,
        _ => paved(&get),
    };
    let width = get("width").and_then(metres);
    if !class.is_motor() {
        let widest = match class {
            Class::Pedestrian | Class::Steps => 15.0,
            _ => 8.0,
        };
        let width = width.filter(|w| (0.5..=widest).contains(w));
        return Some(Profile {
            class,
            level,
            paved,
            confidence: if width.is_some() {
                Confidence::Full
            } else {
                Confidence::Assumed
            },
            width: width.unwrap_or_else(|| class.lane_width()),
            measured: width.is_some(),
            centre: 0.0,
            lines: Vec::new(),
            lanes: None,
            parking: [0.0; 2],
            sides: Default::default(),
            paving: None,
            roundabout: false,
            placed: Placed::Unsaid,
            speed: 0.0,
            named: [0; 2],
            rules,
        });
    }
    let roundabout = matches!(get("junction"), Some("roundabout" | "circular"));
    let reversed = get("oneway") == Some("-1");
    let oneway = reversed
        || match get("oneway") {
            Some("yes" | "true" | "1") => true,
            Some("no") => false,
            _ => highway == "motorway" || roundabout,
        };
    let count = |key: &str| {
        get(key)
            .and_then(|v| v.trim().parse::<u32>().ok())
            .filter(|n| (1..=MAX_LANES).contains(n))
    };
    let tagged = count("lanes");
    let (forward, backward) = (count("lanes:forward"), count("lanes:backward"));
    let usual = match class {
        _ if link => 1,
        Class::Motorway | Class::Trunk if oneway => 2,
        Class::Service | Class::LivingStreet => 1,
        _ if oneway => 1,
        _ => 2,
    };
    let lanes = tagged
        .or_else(|| Some(forward? + backward?))
        .unwrap_or(usual);
    // Lanes of the way's own direction; one lane of a two-way road is both's.
    let forward = if oneway || lanes == 1 {
        lanes
    } else {
        forward
            .or_else(|| backward.and_then(|b| lanes.checked_sub(b)))
            .filter(|&f| f < lanes)
            .unwrap_or(lanes.div_ceil(2))
    };
    let backward = lanes - forward;
    let left_hand = rules.keeps_left();
    let middle = if left_hand { forward } else { backward };

    let mut sides = sides(&get, class, oneway, reversed, link);
    // a tagged width is the carriageway's, cycle lanes and parking within it too
    let within: f64 = sides.iter().map(|s| s.cycle + s.parking).sum();
    let width = width
        .map(|w| w - within)
        .filter(|w| LANE_WIDTHS.contains(&(w / f64::from(lanes))));
    let known = tagged.is_some() || count("lanes:forward").is_some();
    let confidence = match (known, width) {
        (true, Some(_)) => Confidence::Full,
        (true, None) => Confidence::Lanes,
        (false, _) => Confidence::Assumed,
    };
    let across = width.unwrap_or(class.lane_width() * f64::from(lanes));
    let lane = across / f64::from(lanes);
    let placed = if reversed {
        None
    } else {
        placement(&get, oneway, [backward, forward], lane, left_hand)
    };
    let left = placed.unwrap_or(-across / 2.0);

    let changes = changes(&get, oneway, reversed, [backward, forward], left_hand);
    let mut lines = lines(
        &get,
        class,
        confidence,
        [lanes, middle],
        &changes,
        left,
        lane,
    );
    let shoulders = shoulders(&get, class, oneway, reversed != left_hand, link);
    let count = backward + forward;
    let bus = bus(&get, oneway, reversed, [backward, forward]);
    for (i, side) in sides.iter_mut().enumerate() {
        side.shoulder = shoulders[i].unwrap_or(0.0);
        let at = left + if i == 0 { 0.0 } else { f64::from(count) * lane };
        if side.cycle > 0.0 {
            let line = if side.advisory {
                Line::CycleDashed
            } else {
                Line::Cycle
            };
            set_line(&mut lines, at, line);
        } else if shoulders[i].is_some_and(|s| s > 0.0) && class > Class::Trunk {
            set_line(&mut lines, at, Line::Edge);
        }
    }
    // a bus lane's line is wide where a lane of its own direction is beside it
    let direction = |i: usize| oneway || i >= backward as usize;
    for (i, _) in bus.iter().enumerate().filter(|b| *b.1) {
        for (edge, beside) in [(i, i.wrapping_sub(1)), (i + 1, i + 1)] {
            let other = bus.get(beside).is_some_and(|b| !b);
            if other && direction(beside) == direction(i) {
                set_line(&mut lines, left + edge as f64 * lane, Line::Bus);
            }
        }
    }
    let [left_side, right_side] = sides.map(|s| s.inside());
    let turns = turns(&get, oneway, reversed, [forward, backward]);
    Some(Profile {
        class,
        level,
        paved,
        confidence,
        width: across + left_side + right_side,
        measured: width.is_some(),
        centre: left + across / 2.0 + (right_side - left_side) / 2.0,
        lines,
        lanes: Some(Box::new(Lanes {
            left,
            width: lane,
            backward,
            forward,
            along: !reversed,
            against: reversed || !oneway,
            turns,
            bus,
            left_hand,
        })),
        parking: [parking(&get, "left"), parking(&get, "right")],
        sides,
        paving: paving(&get),
        roundabout,
        placed: match placed {
            Some(_) => Placed::Said,
            None if get("placement") == Some("transition") => Placed::Transition,
            None => Placed::Unsaid,
        },
        speed: speed(&get, class, link),
        named: ["name", "ref"].map(|key| get(key).map_or(0, hash)),
        rules,
    })
}

/// Of a text; not 0.
fn hash(text: &str) -> u64 {
    let mut hasher = rustc_hash::FxHasher::default();
    std::hash::Hash::hash(text, &mut hasher);
    std::hash::Hasher::finish(&hasher).max(1)
}

/// The arrows of the lanes along the way and against it: those tagged for as
/// many lanes as there are.
fn turns<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
    oneway: bool,
    reversed: bool,
    [forward, backward]: [u32; 2],
) -> [Vec<Vec<Turn>>; 2] {
    let read = |key: &str, lanes: u32| -> Vec<Vec<Turn>> {
        let Some(text) = get(key) else {
            return Vec::new();
        };
        let turns: Vec<Vec<Turn>> = text
            .split('|')
            .map(|lane| lane.split(';').filter_map(Turn::read).collect())
            .collect();
        if turns.len() == lanes as usize && turns.iter().any(|t| !t.is_empty()) {
            turns
        } else {
            Vec::new()
        }
    };
    if reversed {
        // a road against its way has its lanes tagged as its traffic sees them
        return [Vec::new(), read("turn:lanes", forward)];
    }
    if oneway {
        return [read("turn:lanes", forward), Vec::new()];
    }
    if backward == 0 {
        return [Vec::new(), Vec::new()];
    }
    [
        read("turn:lanes:forward", forward),
        read("turn:lanes:backward", backward),
    ]
}

/// Metres of parking beside the carriageway, where it has a surface of its own.
fn parking<'a>(get: impl Fn(&str) -> Option<&'a str>, side: &str) -> f64 {
    let of = |suffix: &str| {
        get(&format!("parking:{side}{suffix}")).or_else(|| get(&format!("parking:both{suffix}")))
    };
    if of("") != Some("street_side") {
        return 0.0;
    }
    match of(":orientation") {
        Some("perpendicular") => 5.0,
        Some("diagonal") => 4.5,
        _ => 2.2,
    }
}

/// A side's key, else both sides': `<prefix>:<side><suffix>`.
fn of_side<'a>(
    get: &impl Fn(&str) -> Option<&'a str>,
    prefix: &str,
    side: &str,
    suffix: &str,
) -> Option<&'a str> {
    get(&format!("{prefix}:{side}{suffix}")).or_else(|| get(&format!("{prefix}:both{suffix}")))
}

/// A width a tag gives that is believed.
fn side_width(text: Option<&str>) -> Option<f64> {
    text.and_then(metres).filter(|w| SIDE_WIDTHS.contains(w))
}

/// What lies beside a motor road's lanes, left and right of the way, as its
/// tags say: cycle lanes, tracks and shared lanes (`cycleway*`), parking in the
/// carriageway (`parking:*=lane`) and sidewalks that are not ways of their own
/// (`sidewalk*`). Shoulders are [`shoulders`]'.
fn sides<'a>(
    get: &impl Fn(&str) -> Option<&'a str>,
    class: Class,
    oneway: bool,
    reversed: bool,
    link: bool,
) -> [Side; 2] {
    // of a one-way road, the side on its traffic's right
    let right = usize::from(!reversed);
    let mut out = [Side::default(); 2];
    for (i, side) in ["left", "right"].into_iter().enumerate() {
        let s = &mut out[i];
        let whole = get("cycleway").and_then(|v| match v {
            "lane" | "track" | "shared_lane" if !oneway || i == right => Some((v, false)),
            "opposite_lane" | "opposite_track" if oneway && i != right => {
                Some((v.trim_start_matches("opposite_"), true))
            }
            _ => None,
        });
        let cycle = match of_side(get, "cycleway", side, "") {
            Some("opposite_lane") => Some(("lane", true)),
            Some("opposite_track") => Some(("track", true)),
            Some(v) => Some((v, false)),
            None => whole,
        };
        if let Some((kind, opposite)) = cycle {
            let width = side_width(
                of_side(get, "cycleway", side, ":width").or_else(|| get("cycleway:width")),
            );
            let oneway_said = of_side(get, "cycleway", side, ":oneway");
            s.along = match oneway_said {
                Some("yes") => true,
                Some("-1") => false,
                _ if opposite => reversed,
                _ if oneway => !reversed,
                _ => i == 1,
            };
            match kind {
                "lane" => {
                    s.cycle = width.unwrap_or(CYCLE_LANE);
                    s.symbol = Some(Symbol::Bicycle);
                    let lane =
                        of_side(get, "cycleway", side, ":lane").or_else(|| get("cycleway:lane"));
                    s.advisory = matches!(lane, Some("advisory" | "pictogram"));
                }
                "track" => {
                    let both = oneway_said == Some("no");
                    s.track = width.unwrap_or(if both { TRACK.1 } else { TRACK.0 });
                }
                "shared_lane" => s.symbol = Some(Symbol::Sharrow),
                _ => {}
            }
        }
        let walk = get(&format!("sidewalk:{side}"))
            .or_else(|| get("sidewalk:both"))
            .or_else(|| {
                get("sidewalk").map(|v| match v {
                    "both" | "yes" => "yes",
                    v if v == side => "yes",
                    _ => "no",
                })
            });
        if walk == Some("yes") && class != Class::Motorway && !(link && class == Class::Trunk) {
            let width = of_side(get, "sidewalk", side, ":width").or_else(|| get("sidewalk:width"));
            s.sidewalk = side_width(width).unwrap_or(SIDEWALK);
        }
        if of_side(get, "parking", side, "") == Some("lane") {
            let (across, bays) = match of_side(get, "parking", side, ":orientation") {
                Some("perpendicular") => (5.0, Some(Bays::Perpendicular)),
                Some("diagonal") => (4.5, Some(Bays::Diagonal)),
                Some("parallel") => (2.2, Some(Bays::Parallel)),
                _ => (2.2, None),
            };
            (s.parking, s.bays) = (across, bays);
        }
    }
    out
}

/// Metres of shoulder left and right of the way's lanes: as tagged, or a
/// motorway's usual ones; `None` where nothing says. Where any side is tagged,
/// a side that is not has none, but for the strip a motorway keeps by its middle.
fn shoulders<'a>(
    get: &impl Fn(&str) -> Option<&'a str>,
    class: Class,
    oneway: bool,
    reversed: bool,
    link: bool,
) -> [Option<f64>; 2] {
    let right = usize::from(!reversed);
    let said = ["left", "right"].map(|side| {
        let whole = get("shoulder").map(|v| match v {
            "both" => true,
            "yes" => !oneway || side == ["left", "right"][right],
            v => v == side,
        });
        match of_side(get, "shoulder", side, "") {
            Some("yes") => Some(true),
            Some("no") => Some(false),
            _ => whole,
        }
    });
    let motorway = class == Class::Motorway && !link;
    // the usual ones are as traffic sees them
    let usual = if reversed {
        [SHOULDER.0, SHOULDER.1]
    } else {
        [SHOULDER.1, SHOULDER.0]
    };
    if said.iter().all(Option::is_none) {
        return usual.map(|w| motorway.then_some(w));
    }
    let wide = if class <= Class::Trunk {
        SHOULDERS.0
    } else {
        SHOULDERS.1
    };
    let mut out = [None; 2];
    for (i, side) in ["left", "right"].into_iter().enumerate() {
        let width = of_side(get, "shoulder", side, ":width").or_else(|| get("shoulder:width"));
        out[i] = Some(match said[i] {
            Some(true) => side_width(width).unwrap_or(wide),
            _ if motorway && i != right => usual[i],
            _ => 0.0,
        });
    }
    out
}

/// Of each lane, left to right, whether it is for buses: as `bus:lanes*` or
/// `psv:lanes*` say where they are for as many lanes as there are, else
/// `busway*=lane`, the outer lane of a direction of two lanes or more. Empty
/// if none is.
fn bus<'a>(
    get: &impl Fn(&str) -> Option<&'a str>,
    oneway: bool,
    reversed: bool,
    [backward, forward]: [u32; 2],
) -> Vec<bool> {
    let count = (backward + forward) as usize;
    if count < 2 {
        return Vec::new();
    }
    // from the driver's left
    let read = |key: &str, lanes: u32| -> Option<Vec<bool>> {
        let text = ["bus", "psv"]
            .iter()
            .find_map(|mode| get(&format!("{mode}:{key}")))?;
        let lanes_of: Vec<bool> = text.split('|').map(|v| v.trim() == "designated").collect();
        (lanes_of.len() == lanes as usize).then_some(lanes_of)
    };
    let mut out = vec![false; count];
    if oneway {
        let found = read("lanes", count as u32).or_else(|| read("lanes:forward", count as u32));
        if let Some(mut found) = found {
            if reversed {
                found.reverse();
            }
            out = found;
        }
    } else if backward > 0 {
        if let Some(found) = read("lanes:forward", forward) {
            out[backward as usize..].copy_from_slice(&found);
        }
        if let Some(mut found) = read("lanes:backward", backward) {
            found.reverse();
            out[..backward as usize].copy_from_slice(&found);
        }
    }
    if !out.contains(&true) {
        let right = usize::from(!reversed);
        for (i, side) in ["left", "right"].into_iter().enumerate() {
            let lane = match get(&format!("busway:{side}")).or_else(|| get("busway:both")) {
                Some(v) => v == "lane",
                None => get("busway") == Some("lane") && (!oneway || i == right),
            };
            let room = match (oneway, i) {
                (true, _) => count,
                (false, 0) => backward as usize,
                (false, _) => forward as usize,
            };
            if lane && room >= 2 {
                out[if i == 0 { 0 } else { count - 1 }] = true;
            }
        }
    }
    if out.contains(&true) { out } else { Vec::new() }
}

/// What a road is paved with, if a style may tell it apart.
fn paving<'a>(get: &impl Fn(&str) -> Option<&'a str>) -> Option<&'static str> {
    let surface = get("surface")?;
    Some(match surface.split(':').next()? {
        "sett" => "sett",
        "cobblestone" | "unhewn_cobblestone" => "cobble",
        "paving_stones" => "slabs",
        "concrete" => "concrete",
        _ => return None,
    })
}

/// Puts `line` at `at`, in place of one there, keeping the lines left to right.
fn set_line(lines: &mut Vec<(f64, Line)>, at: f64, line: Line) {
    match lines.iter().position(|l| (l.0 - at).abs() < 1e-6) {
        Some(i) => lines[i].1 = line,
        None => {
            let i = lines.partition_point(|l| l.0 < at);
            lines.insert(i, (at, line));
        }
    }
}

/// Which edges of the lanes, counted from the left one, `change:lanes` has traffic
/// of neither side cross.
fn changes<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
    oneway: bool,
    reversed: bool,
    [backward, forward]: [u32; 2],
    left_hand: bool,
) -> Vec<u32> {
    // between which lanes, from the driver's left: as many as there are, or none
    let read = |key: &str, lanes: u32| -> Vec<u32> {
        let Some(text) = get(key) else {
            return Vec::new();
        };
        // whether it may be left to the left, and to the right
        let may: Vec<[bool; 2]> = text
            .split('|')
            .map(|lane| match lane.trim() {
                "no" => [false, false],
                "not_left" | "only_right" => [false, true],
                "not_right" | "only_left" => [true, false],
                _ => [true, true],
            })
            .collect();
        if may.len() != lanes as usize {
            return Vec::new();
        }
        let between = may.windows(2).zip(1..);
        let solid = between.filter(|(pair, _)| !pair[0][1] && !pair[1][0]);
        solid.map(|(_, i)| i).collect()
    };
    let lanes = backward + forward;
    if oneway {
        let of = read("change:lanes", lanes);
        let of = if of.is_empty() {
            read("change:lanes:forward", lanes)
        } else {
            of
        };
        return of
            .into_iter()
            .map(|i| if reversed { lanes - i } else { i })
            .collect();
    }
    if backward == 0 {
        return Vec::new();
    }
    let ahead = read("change:lanes:forward", forward).into_iter();
    let back = read("change:lanes:backward", backward).into_iter();
    // each direction's from its drivers' left
    if left_hand {
        return ahead.chain(back.map(|i| lanes - i)).collect();
    }
    back.map(|i| backward - i)
        .chain(ahead.map(|i| backward + i))
        .collect()
}

/// The lines of a carriageway whose left edge is at `left`: nothing the tags
/// or the class do not support.
fn lines<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
    class: Class,
    confidence: Confidence,
    [lanes, middle]: [u32; 2],
    solid: &[u32],
    left: f64,
    lane: f64,
) -> Vec<(f64, Line)> {
    let marked = match get("lane_markings") {
        Some("no") => return Vec::new(),
        Some("yes") => true,
        _ => class <= Class::Tertiary || (confidence > Confidence::Assumed && lanes > 2),
    };
    if !marked || (lanes == 1 && class > Class::Trunk) {
        return Vec::new();
    }
    let edged = class <= Class::Trunk;
    let no_passing = matches!(get("overtaking"), Some("no"));
    let mut lines = Vec::new();
    for i in 0..=lanes {
        let line = if i == 0 || i == lanes {
            if !edged {
                continue;
            }
            Line::Edge
        } else if i == middle {
            if no_passing || lanes >= 4 {
                Line::CentreSolid
            } else {
                Line::Centre
            }
        } else if solid.contains(&i) {
            Line::LaneSolid
        } else {
            Line::Lane
        };
        lines.push((left + f64::from(i) * lane, line));
    }
    lines
}

/// Where the carriageway's left edge lies if `placement` says which lane the
/// way runs along: lanes are counted from the left of a one-way road, and from
/// the middle of one whose directions are told apart.
fn placement<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
    oneway: bool,
    [backward, forward]: [u32; 2],
    lane: f64,
    left_hand: bool,
) -> Option<f64> {
    // lanes from the left of those of one direction, as its traffic sees them
    let read = |key: &str, lanes: u32| -> Option<f64> {
        let (side, number) = get(key)?.split_once(':')?;
        let number = number
            .parse::<u32>()
            .ok()
            .filter(|n| (1..=lanes).contains(n))?;
        match side {
            "left_of" => Some(f64::from(number - 1)),
            "middle_of" => Some(f64::from(number) - 0.5),
            "right_of" => Some(f64::from(number)),
            _ => None,
        }
    };
    let edge = if oneway {
        read("placement", forward).or_else(|| read("placement:forward", forward))?
    } else if backward > 0 && left_hand {
        let ahead = read("placement:forward", forward);
        let all = f64::from(backward + forward);
        ahead.or_else(|| Some(all - read("placement:backward", backward)?))?
    } else if backward > 0 {
        let ahead = read("placement:forward", forward).map(|e| f64::from(backward) + e);
        ahead.or_else(|| Some(f64::from(backward) - read("placement:backward", backward)?))?
    } else {
        return None;
    };
    Some(-edge * lane)
}

/// Km/h: `maxspeed`, or what is usual of the class.
fn speed<'a>(get: impl Fn(&str) -> Option<&'a str>, class: Class, link: bool) -> f64 {
    let tagged = get("maxspeed").and_then(|text| {
        let text = text.trim();
        if text.ends_with(":urban") {
            return Some(50.0);
        }
        if text.ends_with(":rural") {
            return Some(90.0);
        }
        let digits = text.find(|c: char| !c.is_ascii_digit());
        let (number, unit) = text.split_at(digits.unwrap_or(text.len()));
        let in_miles = unit.trim() == "mph";
        let number = number.parse::<f64>().ok()?;
        Some(number * if in_miles { 1.609 } else { 1.0 })
    });
    tagged
        .filter(|v| (5.0..=200.0).contains(v))
        .unwrap_or(match class {
            Class::Motorway | Class::Trunk if link => 70.0,
            Class::Motorway => 120.0,
            Class::Trunk => 90.0,
            _ => 50.0,
        })
}

pub(super) fn level<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Level {
    let layer = get("layer").and_then(|v| v.trim().parse::<i8>().ok());
    if get("bridge").is_some_and(|v| v != "no") {
        Level::Bridge(layer.unwrap_or(1))
    } else if get("tunnel").is_some_and(|v| !matches!(v, "no" | "building_passage" | "culvert")) {
        Level::Tunnel(layer.unwrap_or(-1))
    } else {
        Level::Ground
    }
}

/// Where a path with these tags lies, if it is drawn: one within a building, or
/// on a floor that is not the ground's, is not, nor are steps that move or go
/// through the ground. A bridge's steps lead up to it from the ground.
fn path_level<'a>(get: impl Fn(&str) -> Option<&'a str>, class: Class) -> Option<Level> {
    let is = |key: &str| get(key).is_some_and(|v| v != "no");
    let level = level(&get);
    let crossing = ["footway", "path"]
        .iter()
        .any(|key| get(key) == Some("crossing"));
    // `0;1`, `-1-0`
    let floors = |v: &str| v.split([';', '-']).any(|floor| floor.trim() == "0");
    if is("indoor") || (level == Level::Ground && get("level").is_some_and(|v| !floors(v))) {
        return None;
    }
    match (class, level) {
        (Class::Steps, _) if is("conveying") => None,
        (Class::Steps, Level::Tunnel(_)) => None,
        (Class::Steps, _) => Some(Level::Ground),
        // across a bridge it is the road's
        (_, Level::Bridge(_)) if crossing => None,
        _ => Some(level),
    }
}

/// Whether a way with these tags is of a hard surface.
pub(super) fn paved<'a>(get: impl Fn(&str) -> Option<&'a str>) -> bool {
    get("surface").is_none_or(|surface| !UNPAVED.contains(&surface))
}

/// A length in metres: `3.5`, `3.5 m`, or in feet and inches, `12'`, `12'6"`,
/// `6"`, `12 ft`. Other units are not read.
pub(super) fn metres(text: &str) -> Option<f64> {
    let text = text.trim();
    let number = |t: &str| {
        t.trim()
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite() && *v >= 0.0)
    };
    let value = if let Some(feet) = text.strip_suffix("ft") {
        number(feet)? * FOOT
    } else if let Some((feet, inches)) = text.split_once('\'') {
        let inches = match inches.trim() {
            "" => 0.0,
            inches => number(inches.strip_suffix('"')?)?,
        };
        (number(feet)? * 12.0 + inches) * FOOT / 12.0
    } else if let Some(inches) = text.strip_suffix('"') {
        number(inches)? * FOOT / 12.0
    } else {
        number(text.strip_suffix('m').unwrap_or(text))?
    };
    Some(value).filter(|v| v.is_finite() && *v > 0.0)
}

/// Metres a foot.
const FOOT: f64 = 0.3048;

#[cfg(test)]
mod tests {
    use super::*;

    fn read(tags: &[(&str, &'static str)]) -> Option<Profile> {
        profile(|key| tags.iter().find(|(k, _)| *k == key).map(|(_, v)| *v))
    }

    fn kinds(p: &Profile) -> Vec<Line> {
        p.lines.iter().map(|l| l.1).collect()
    }

    #[test]
    fn a_width_in_feet_is_read_as_metres() {
        assert_eq!((metres("3.5"), metres("3.5 m")), (Some(3.5), Some(3.5)));
        let near = |text: &str, m: f64| (metres(text).unwrap() - m).abs() < 1e-9;
        assert!(
            near("12'", 3.6576)
                && near("12 ft", 3.6576)
                && near("12'6\"", 3.81)
                && near("6\"", 0.1524)
        );
        for bad in ["", "ft", "'", "12'x", "-3", "0", "nan", "3 yd"] {
            assert_eq!(metres(bad), None, "{bad}");
        }
        let street = read(&[("highway", "residential"), ("width", "24'")]).unwrap();
        assert!((street.width - 7.3152).abs() < 1e-9);
    }

    #[test]
    fn a_residential_street_without_tags_is_a_surface_with_no_lines() {
        let p = read(&[("highway", "residential")]).unwrap();
        assert_eq!((p.width, p.centre), (5.5, 0.0));
        assert_eq!(p.confidence, Confidence::Assumed);
        assert!(p.lines.is_empty());
        let marked = read(&[("highway", "residential"), ("lane_markings", "yes")]).unwrap();
        assert_eq!(kinds(&marked), [Line::Centre]);
    }

    #[test]
    fn a_tertiary_road_has_a_dashed_centre_unless_it_says_not() {
        assert_eq!(
            kinds(&read(&[("highway", "tertiary")]).unwrap()),
            [Line::Centre]
        );
        let none = read(&[("highway", "tertiary"), ("lane_markings", "no")]).unwrap();
        assert!(none.lines.is_empty());
        let solid = read(&[("highway", "tertiary"), ("overtaking", "no")]).unwrap();
        assert_eq!(kinds(&solid), [Line::CentreSolid]);
    }

    #[test]
    fn a_line_neither_side_may_cross_is_solid() {
        let of = |tags: &[(&str, &'static str)]| {
            let mut all = vec![("highway", "primary"), ("lanes", "3")];
            all.extend(tags);
            kinds(&read(&all).unwrap())
        };
        let (dashed, solid) = (Line::Lane, Line::LaneSolid);
        let one = ("oneway", "yes");
        assert_eq!(
            of(&[one, ("change:lanes", "yes|not_right|not_left")]),
            [dashed, solid]
        );
        // one side may, and as many as there are lanes
        assert_eq!(of(&[one, ("change:lanes", "no|yes|no")]), [dashed, dashed]);
        assert_eq!(of(&[one, ("change:lanes", "no|no")]), [dashed, dashed]);
        assert_eq!(
            of(&[("oneway", "-1"), ("change:lanes", "no|no|yes")]),
            [dashed, solid]
        );
        let two = [
            ("lanes:forward", "1"),
            ("lanes:backward", "2"),
            ("change:lanes:backward", "no|no"),
        ];
        assert_eq!(of(&two), [solid, Line::Centre]);
    }

    #[test]
    fn lanes_split_by_direction() {
        let p = read(&[
            ("highway", "primary"),
            ("lanes", "3"),
            ("lanes:backward", "1"),
        ])
        .unwrap();
        assert_eq!(kinds(&p), [Line::Centre, Line::Lane]);
        assert_eq!(p.lines[0].0, -1.75);
        assert_eq!(p.confidence, Confidence::Lanes);
        // traffic keeps right, so the backward lanes are on the left
        let p = read(&[("highway", "primary"), ("lanes", "4")]).unwrap();
        assert_eq!(kinds(&p), [Line::Lane, Line::CentreSolid, Line::Lane]);
        assert_eq!(p.lines[1].0, 0.0);
    }

    #[test]
    fn nonsense_is_not_believed() {
        let wide = read(&[("highway", "primary"), ("lanes", "2"), ("width", "60")]).unwrap();
        assert_eq!((wide.width, wide.confidence), (7.0, Confidence::Lanes));
        let many = read(&[("highway", "primary"), ("lanes", "400")]).unwrap();
        assert_eq!((many.width, many.confidence), (7.0, Confidence::Assumed));
        let swapped = [
            ("highway", "primary"),
            ("lanes", "2"),
            ("lanes:forward", "5"),
        ];
        assert_eq!(kinds(&read(&swapped).unwrap()), [Line::Centre]);
        assert!(
            read(&[("highway", "primary"), ("width", "NaN")])
                .unwrap()
                .width
                > 0.0
        );
    }

    #[test]
    fn a_motorway_has_edges_and_shoulders() {
        let p = read(&[("highway", "motorway"), ("lanes", "2")]).unwrap();
        assert_eq!(kinds(&p), [Line::Edge, Line::Lane, Line::Edge]);
        assert_eq!((p.width, p.centre), (10.5, 1.0));
        let link = read(&[("highway", "motorway_link"), ("oneway", "yes")]).unwrap();
        assert_eq!(
            (link.width, kinds(&link)),
            (3.75, vec![Line::Edge, Line::Edge])
        );
    }

    #[test]
    fn placement_moves_the_carriageway() {
        let at = |v| {
            read(&[("highway", "motorway"), ("lanes", "3"), ("placement", v)])
                .unwrap()
                .lines[0]
                .0
        };
        assert_eq!(at("right_of:1"), -3.75);
        assert_eq!(at("middle_of:2"), -5.625);
        assert_eq!(at("left_of:1"), 0.0);
        let placed = |tags: &[(&str, &'static str)]| read(tags).unwrap().placed;
        let tags = [("highway", "motorway"), ("placement", "middle_of:1")];
        assert_eq!(placed(&tags), Placed::Said);
        let tags = [("highway", "motorway"), ("placement", "transition")];
        assert_eq!(placed(&tags), Placed::Transition);
        // not a lane there is: as if untagged
        assert_eq!(at("right_of:9"), -5.625);
        assert_eq!(at("transition"), -5.625);
    }

    #[test]
    fn placement_on_a_two_way_road_counts_from_its_middle() {
        let left = |key: &'static str, v: &'static str| {
            let tags = [
                ("highway", "primary"),
                ("lanes", "3"),
                ("lanes:backward", "1"),
                (key, v),
            ];
            read(&tags).unwrap().lanes.unwrap().left
        };
        // the one lane against the way lies left of where the directions part
        assert_eq!(left("placement:forward", "left_of:1"), -3.5);
        assert_eq!(left("placement:forward", "right_of:1"), -7.0);
        assert_eq!(left("placement:backward", "left_of:1"), -3.5);
        assert_eq!(left("placement:backward", "right_of:1"), 0.0);
        // it does not say of which direction
        assert_eq!(left("placement", "right_of:1"), -5.25);
    }

    #[test]
    fn speeds_are_tagged_or_the_classes() {
        let speed = |highway: &'static str, v: &'static str| {
            let tags = [("highway", highway), ("maxspeed", v)];
            read(&tags).unwrap().speed
        };
        assert_eq!(speed("primary", "70"), 70.0);
        assert_eq!(speed("primary", "PL:urban"), 50.0);
        assert!((speed("primary", "50 mph") - 80.45).abs() < 0.01);
        assert_eq!(speed("primary", "none"), 50.0);
        assert_eq!(speed("motorway", "999999"), 120.0);
        assert_eq!(speed("motorway_link", ""), 70.0);
    }

    #[test]
    fn arrows_are_read_for_as_many_lanes_as_there_are() {
        let turns = |tags: &[(&str, &'static str)]| read(tags).unwrap().lanes.unwrap().turns;
        let one = turns(&[
            ("highway", "primary"),
            ("oneway", "yes"),
            ("lanes", "3"),
            ("turn:lanes", "left|through;right|"),
        ]);
        assert_eq!(
            one[0],
            [
                vec![Turn::Left],
                vec![Turn::Through, Turn::Right],
                Vec::new()
            ]
        );
        assert!(one[1].is_empty());
        // two lanes have no three arrows, and a two-way road's are by direction
        let wrong = turns(&[
            ("highway", "primary"),
            ("oneway", "yes"),
            ("lanes", "2"),
            ("turn:lanes", "left|through|right"),
        ]);
        assert!(wrong.iter().all(Vec::is_empty));
        let both = turns(&[
            ("highway", "primary"),
            ("lanes", "3"),
            ("lanes:forward", "2"),
            ("turn:lanes", "left|through|right"),
            ("turn:lanes:forward", "left|through"),
            ("turn:lanes:backward", "none"),
        ]);
        assert_eq!((both[0].len(), both[1].len()), (2, 0));
        let against = turns(&[
            ("highway", "primary"),
            ("oneway", "-1"),
            ("lanes", "2"),
            ("turn:lanes", "left|right"),
        ]);
        assert_eq!((against[0].len(), against[1].len()), (0, 2));
    }

    #[test]
    fn each_direction_has_its_own_span() {
        let lanes = |tags: &[(&str, &'static str)]| read(tags).unwrap().lanes.unwrap();
        let three = lanes(&[
            ("highway", "primary"),
            ("lanes", "3"),
            ("lanes:backward", "1"),
        ]);
        assert_eq!(three.span(true), [-1.75, 5.25]);
        assert_eq!(three.span(false), [-5.25, -1.75]);
        let narrow = lanes(&[("highway", "residential"), ("lanes", "1")]);
        assert_eq!(narrow.span(true), [0.0, 1.375]);
        assert_eq!(narrow.span(false), [-1.375, 0.0]);
        let one_way = lanes(&[("highway", "primary"), ("oneway", "-1"), ("lanes", "2")]);
        assert_eq!((one_way.along, one_way.against), (false, true));
        assert_eq!(one_way.span(false), [-3.5, 3.5]);
    }

    #[test]
    fn parking_beside_a_road_is_a_strip_only_where_it_has_ground_of_its_own() {
        let strips = |tags: &[(&str, &'static str)]| read(tags).unwrap().parking;
        let street = [
            ("highway", "residential"),
            ("parking:both", "street_side"),
            ("parking:right:orientation", "perpendicular"),
        ];
        assert_eq!(strips(&street), [2.2, 5.0]);
        let lane = [("highway", "residential"), ("parking:left", "lane")];
        assert_eq!(strips(&lane), [0.0, 0.0]);
    }

    #[test]
    fn paths_are_as_wide_as_tagged_or_as_their_class_and_never_motor_roads() {
        let of = |tags: &[(&str, &'static str)]| {
            let p = read(tags).unwrap();
            assert!(!p.class.is_motor() && p.class.is_path() && p.lanes.is_none());
            assert!(p.lines.is_empty());
            (p.class, p.width, p.paved)
        };
        assert_eq!(of(&[("highway", "footway")]), (Class::Footway, 2.0, true));
        assert_eq!(of(&[("highway", "path")]), (Class::Path, 1.5, false));
        assert_eq!(
            of(&[("highway", "path"), ("surface", "asphalt"), ("width", "3")]),
            (Class::Path, 3.0, true)
        );
        assert_eq!(
            of(&[("highway", "pedestrian"), ("width", "12")]),
            (Class::Pedestrian, 12.0, true)
        );
        assert_eq!(
            of(&[("highway", "footway"), ("width", "12")]),
            (Class::Footway, 2.0, true)
        );
        assert_eq!(
            of(&[("highway", "footway"), ("surface", "gravel")]),
            (Class::Footway, 2.0, false)
        );
        assert_eq!(of(&[("highway", "steps")]), (Class::Steps, 2.0, true));
        assert_eq!(
            of(&[("highway", "bridleway")]),
            (Class::Bridleway, 1.5, false)
        );
    }

    #[test]
    fn paths_on_other_floors_and_steps_under_ground_are_not_drawn() {
        let level = |tags: &[(&str, &'static str)]| read(tags).map(|p| p.level);
        let footway = ("highway", "footway");
        let steps = ("highway", "steps");
        for hidden in [("indoor", "yes"), ("level", "1"), ("level", "-2;-1")] {
            assert_eq!(level(&[footway, hidden]), None, "{hidden:?}");
        }
        for shown in [("level", "0"), ("level", "-1;0"), ("level", "0-1")] {
            assert_eq!(level(&[footway, shown]), Some(Level::Ground), "{shown:?}");
        }
        assert_eq!(
            level(&[footway, ("tunnel", "yes"), ("level", "-1")]),
            Some(Level::Tunnel(-1))
        );
        assert_eq!(level(&[footway, ("bridge", "yes")]), Some(Level::Bridge(1)));
        let crossing = ("footway", "crossing");
        assert_eq!(level(&[footway, crossing]), Some(Level::Ground));
        assert_eq!(level(&[footway, crossing, ("bridge", "yes")]), None);
        assert_eq!(level(&[steps, ("tunnel", "yes")]), None);
        assert_eq!(level(&[steps, ("conveying", "forward")]), None);
        assert_eq!(level(&[steps, ("bridge", "yes")]), Some(Level::Ground));
    }

    #[test]
    fn levels_and_classes() {
        let level = |tags: &[(&str, &'static str)]| read(tags).unwrap().level;
        assert_eq!(
            level(&[("highway", "service"), ("bridge", "yes")]),
            Level::Bridge(1)
        );
        assert_eq!(
            level(&[("highway", "service"), ("tunnel", "yes"), ("layer", "-2")]),
            Level::Tunnel(-2)
        );
        assert_eq!(
            level(&[("highway", "service"), ("tunnel", "building_passage")]),
            Level::Ground
        );
        assert_eq!(
            level(&[("highway", "service"), ("layer", "1")]),
            Level::Ground
        );
        assert!(read(&[("highway", "track")]).is_none());
        assert!(read(&[("highway", "pedestrian"), ("area", "yes")]).is_none());
        assert_eq!(
            read(&[("highway", "footway"), ("footway", "sidewalk")])
                .unwrap()
                .class,
            Class::Sidewalk
        );
        assert!(
            !read(&[("highway", "residential"), ("surface", "gravel")])
                .unwrap()
                .paved
        );
    }

    #[test]
    fn sides_are_read_as_tagged() {
        let sides = |tags: &[(&str, &'static str)]| read(tags).unwrap().sides;
        // a one-way road's cycle lane is on its traffic's right, one against it on its left
        let one = sides(&[
            ("highway", "residential"),
            ("oneway", "yes"),
            ("cycleway", "lane"),
        ]);
        assert_eq!((one[0].cycle, one[1].cycle, one[1].along), (0.0, 1.5, true));
        let back = sides(&[
            ("highway", "residential"),
            ("oneway", "-1"),
            ("cycleway", "opposite_lane"),
        ]);
        assert_eq!(
            (back[0].cycle, back[1].cycle, back[1].along),
            (0.0, 1.5, true)
        );
        let track = sides(&[
            ("highway", "primary"),
            ("cycleway:both", "track"),
            ("sidewalk", "right"),
            ("sidewalk:right:width", "3"),
        ]);
        let of = |s: &[Side; 2]| (s[0].track, s[1].track, s[0].sidewalk, s[1].sidewalk);
        assert_eq!(of(&track), (1.5, 1.5, 0.0, 3.0));
        let shared = sides(&[("highway", "tertiary"), ("cycleway", "shared_lane")]);
        assert!(
            shared
                .iter()
                .all(|s| s.symbol == Some(Symbol::Sharrow) && s.cycle == 0.0)
        );
        let separate = sides(&[("highway", "residential"), ("sidewalk", "separate")]);
        assert!(separate.iter().all(|s| s.sidewalk == 0.0));
        // a tagged width holds the cycle lanes
        let wide = read(&[
            ("highway", "residential"),
            ("cycleway", "lane"),
            ("width", "8.5"),
        ])
        .unwrap();
        assert_eq!((wide.width, wide.lanes.unwrap().width), (8.5, 2.75));
    }

    #[test]
    fn shoulders_are_a_motorways_unless_tagged() {
        let p = |tags: &[(&str, &'static str)]| read(tags).unwrap();
        let usual = p(&[("highway", "motorway"), ("lanes", "2")]);
        assert_eq!((usual.width, usual.centre), (10.5, 1.0));
        // the strip by the middle stays
        let none = p(&[("highway", "motorway"), ("lanes", "2"), ("shoulder", "no")]);
        assert_eq!((none.width, none.centre), (8.0, -0.25));
        let right = p(&[
            ("highway", "motorway"),
            ("lanes", "2"),
            ("shoulder:right", "yes"),
            ("shoulder:right:width", "3"),
        ]);
        assert_eq!(right.width, 7.5 + 0.5 + 3.0);
        // another road has them only where tagged, and edges where they are
        let both = p(&[("highway", "primary"), ("shoulder", "both")]);
        assert_eq!(both.width, 7.0 + 3.0);
        assert_eq!(kinds(&both), [Line::Edge, Line::Centre, Line::Edge]);
        assert_eq!(p(&[("highway", "primary")]).width, 7.0);
    }

    #[test]
    fn bus_lanes_are_told_by_lane() {
        let bus = |tags: &[(&str, &'static str)]| read(tags).unwrap().lanes.unwrap().bus;
        let both = bus(&[
            ("highway", "primary"),
            ("lanes", "4"),
            ("bus:lanes:forward", "|designated"),
            ("bus:lanes:backward", "|designated"),
        ]);
        assert_eq!(both, [true, false, false, true]);
        let against = bus(&[
            ("highway", "primary"),
            ("oneway", "-1"),
            ("lanes", "2"),
            ("psv:lanes", "designated|"),
        ]);
        assert_eq!(against, [false, true]);
        let busway = bus(&[
            ("highway", "primary"),
            ("lanes", "4"),
            ("busway:right", "lane"),
        ]);
        assert_eq!(busway, [false, false, false, true]);
        // a direction of one lane has no bus lane, nor lanes tagged for fewer
        assert!(bus(&[("highway", "primary"), ("lanes", "2"), ("busway", "lane")]).is_empty());
        let fewer = [
            ("highway", "primary"),
            ("oneway", "yes"),
            ("lanes", "2"),
            ("bus:lanes", "designated"),
        ];
        assert!(bus(&fewer).is_empty());
    }

    #[test]
    fn paving_is_told_of_roads_alone() {
        let paving = |tags: &[(&str, &'static str)]| read(tags).unwrap().paving;
        let street = |surface| paving(&[("highway", "residential"), ("surface", surface)]);
        assert_eq!(street("sett"), Some("sett"));
        assert_eq!(street("unhewn_cobblestone"), Some("cobble"));
        assert_eq!(street("concrete:plates"), Some("concrete"));
        assert_eq!(street("asphalt"), None);
        assert_eq!(paving(&[("highway", "footway"), ("surface", "sett")]), None);
    }
}
