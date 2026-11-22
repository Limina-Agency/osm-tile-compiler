//! A road's cross-section, read from its tags: how wide, which lanes, what is
//! painted between them, and how far the tags support it.

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
}

impl Class {
    pub(super) fn is_motor(self) -> bool {
        self <= Self::Service
    }

    /// Metres across one lane where nothing says otherwise.
    fn lane_width(self) -> f64 {
        match self {
            Self::Motorway => 3.75,
            Self::Trunk | Self::Primary => 3.5,
            Self::Secondary => 3.25,
            Self::Tertiary | Self::Service => 3.0,
            Self::Minor => 2.75,
            Self::LivingStreet => 2.5,
            Self::Cycleway | Self::Sidewalk => 2.0,
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
    /// Lanes of one direction.
    Lane,
    /// The two directions, where crossing over is allowed.
    Centre,
    /// The two directions, where it is not.
    CentreSolid,
}

impl Line {
    pub(super) fn kind(self) -> &'static str {
        match self {
            Self::Edge => "edge",
            Self::Lane => "lane",
            Self::Centre | Self::CentreSolid => "centre",
        }
    }

    pub(super) fn dashed(self) -> bool {
        matches!(self, Self::Lane | Self::Centre)
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
    /// How many run against the way (the left ones), and along it. One lane of
    /// a two-way road is counted along it.
    pub backward: u32,
    pub forward: u32,
    /// Whether traffic goes along the way, and against it.
    pub along: bool,
    pub against: bool,
    /// The arrows of each lane along the way and against it, from the driver's
    /// left, where they are tagged for as many lanes as there are.
    pub turns: [Vec<Vec<Turn>>; 2],
}

impl Lanes {
    /// Metres across that traffic along the way (`true`) or against it takes.
    pub(super) fn span(&self, along: bool) -> [f64; 2] {
        let at = |lane: f64| self.left + lane * self.width;
        let (backward, all) = (
            f64::from(self.backward),
            f64::from(self.backward + self.forward),
        );
        match (self.along, self.against, self.backward) {
            // one lane for both, or one way
            (true, true, 0) if along => [at(all / 2.0), at(all)],
            (true, true, 0) => [at(0.0), at(all / 2.0)],
            (true, true, _) if along => [at(backward), at(all)],
            (true, true, _) => [at(0.0), at(backward)],
            _ => [at(0.0), at(all)],
        }
    }
}

/// Metres, across the way and to the right of its direction.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Profile {
    pub class: Class,
    pub level: Level,
    pub paved: bool,
    pub confidence: Confidence,
    /// The surface's whole width, shoulders included.
    pub width: f64,
    /// The surface's middle.
    pub centre: f64,
    /// Painted lines, left to right.
    pub lines: Vec<(f64, Line)>,
    /// A motor road's.
    pub lanes: Option<Box<Lanes>>,
    /// Metres of parking beside the surface, on its left and right.
    pub parking: [f64; 2],
    pub roundabout: bool,
}

/// Metres of hard shoulder right of a motorway's lanes, and left of them.
const SHOULDER: (f64, f64) = (2.5, 0.5);
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

/// The profile of a way with these tags, if it is drawn. Traffic keeps right.
pub(super) fn profile<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Profile> {
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
        _ => return None,
    };
    let level = level(&get);
    let paved = paved(&get);
    let width = get("width").and_then(metres);
    if !class.is_motor() {
        let width = width.filter(|w| (0.5..=8.0).contains(w));
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
            centre: 0.0,
            lines: Vec::new(),
            lanes: None,
            parking: [0.0; 2],
            roundabout: false,
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

    let width = width.filter(|w| LANE_WIDTHS.contains(&(w / f64::from(lanes))));
    let known = tagged.is_some() || count("lanes:forward").is_some();
    let confidence = match (known, width) {
        (true, Some(_)) => Confidence::Full,
        (true, None) => Confidence::Lanes,
        (false, _) => Confidence::Assumed,
    };
    let across = width.unwrap_or(class.lane_width() * f64::from(lanes));
    let lane = across / f64::from(lanes);
    let left = placement(&get, oneway && !reversed, lanes, lane).unwrap_or(-across / 2.0);

    let lines = lines(&get, class, confidence, [backward, forward], left, lane);
    let (right_shoulder, left_shoulder) = if class == Class::Motorway && !link {
        if reversed {
            (SHOULDER.1, SHOULDER.0)
        } else {
            SHOULDER
        }
    } else {
        (0.0, 0.0)
    };
    let turns = turns(&get, oneway, reversed, [forward, backward]);
    Some(Profile {
        class,
        level,
        paved,
        confidence,
        width: across + left_shoulder + right_shoulder,
        centre: left + across / 2.0 + (right_shoulder - left_shoulder) / 2.0,
        lines,
        lanes: Some(Box::new(Lanes {
            left,
            width: lane,
            backward,
            forward,
            along: !reversed,
            against: reversed || !oneway,
            turns,
        })),
        parking: [parking(&get, "left"), parking(&get, "right")],
        roundabout,
    })
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

/// The lines of a carriageway whose left edge is at `left`: nothing the tags
/// or the class do not support.
fn lines<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
    class: Class,
    confidence: Confidence,
    [backward, forward]: [u32; 2],
    left: f64,
    lane: f64,
) -> Vec<(f64, Line)> {
    let lanes = backward + forward;
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
        } else if i == backward {
            if no_passing || lanes >= 4 {
                Line::CentreSolid
            } else {
                Line::Centre
            }
        } else {
            Line::Lane
        };
        lines.push((left + f64::from(i) * lane, line));
    }
    lines
}

/// Where the carriageway's left edge lies if `placement` says which lane the
/// way runs along; lanes are counted from the left of a one-way road.
fn placement<'a>(
    get: impl Fn(&str) -> Option<&'a str>,
    forward: bool,
    lanes: u32,
    lane: f64,
) -> Option<f64> {
    if !forward {
        return None;
    }
    let (side, number) = get("placement")?.split_once(':')?;
    let number = number
        .parse::<u32>()
        .ok()
        .filter(|n| (1..=lanes).contains(n))?;
    let edge = match side {
        "left_of" => f64::from(number - 1),
        "middle_of" => f64::from(number) - 0.5,
        "right_of" => f64::from(number),
        _ => return None,
    };
    Some(-edge * lane)
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

/// Whether a way with these tags is of a hard surface.
pub(super) fn paved<'a>(get: impl Fn(&str) -> Option<&'a str>) -> bool {
    get("surface").is_none_or(|surface| !UNPAVED.contains(&surface))
}

/// A length in metres: `3.5`, `3.5 m`. Other units are not read.
pub(super) fn metres(text: &str) -> Option<f64> {
    let text = text.trim();
    let text = text.strip_suffix('m').unwrap_or(text).trim_end();
    text.parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
}

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
        // not a lane there is: as if untagged
        assert_eq!(at("right_of:9"), -5.625);
        assert_eq!(at("transition"), -5.625);
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
        assert!(read(&[("highway", "footway")]).is_none());
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
}
