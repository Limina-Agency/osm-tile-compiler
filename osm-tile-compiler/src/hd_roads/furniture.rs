//! What stands or lies beside a road and is mapped on its own: parking,
//! traffic islands, platforms, squares, and the barriers along it; and
//! carriageways mapped as areas.

use super::geometry;
use super::network::{Point, Way, distance};
use super::profile::{self, Level};
use super::structures::Index;
use osm_pbf_ingest::{NormalizedRelation, RelationMemberType};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AreaKind {
    /// Ground to park on.
    Parking,
    /// One bay of it, outlined.
    Bay,
    /// Kerbed, in the road.
    Island,
    /// A carriageway's outline.
    Road,
    /// Where one waits for a train, a tram or a bus.
    Platform,
    /// To walk on: a footway's outline, and a square's.
    Footway,
    Pedestrian,
    /// A raised table's top, in the carriageway.
    Table,
}

impl AreaKind {
    /// The surface's.
    pub(super) fn surface(self) -> &'static str {
        match self {
            Self::Parking | Self::Bay => "parking",
            Self::Island => "island",
            Self::Road => "road",
            Self::Platform => "platform",
            Self::Footway => "footway",
            Self::Pedestrian => "pedestrian",
            Self::Table => "table",
        }
    }

    /// Whether it is to walk on.
    pub(super) fn is_path(self) -> bool {
        matches!(self, Self::Footway | Self::Pedestrian)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Area {
    pub kind: AreaKind,
    pub paved: bool,
    /// Its ring, closed, wound as a widened line's.
    pub ring: Vec<Point>,
    /// Those of what is cut out of it, wound the other way.
    pub holes: Vec<Vec<Point>>,
}

/// What a closed way with these tags is, if it lies on the ground under the sky.
pub(super) fn area<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<(AreaKind, bool)> {
    let kind = match (get("amenity"), get("area:highway"), get("traffic_calming")) {
        (Some("parking"), ..) => AreaKind::Parking,
        (Some("parking_space"), ..) => AreaKind::Bay,
        (_, Some("traffic_island"), _) => AreaKind::Island,
        (_, _, Some("island")) if get("highway").is_none() => AreaKind::Island,
        (_, _, Some("table")) => AreaKind::Table,
        (_, Some(of), _) if carriageway(of) => AreaKind::Road,
        _ if platform(&get) => AreaKind::Platform,
        _ => walked(&get, get("area") == Some("yes"))?,
    };
    open(&get).then(|| (kind, profile::paved(&get)))
}

/// Whether an `area:highway` of this kind is for motor traffic, and no driveway.
fn carriageway(of: &str) -> bool {
    matches!(
        of.strip_suffix("_link").unwrap_or(of),
        "motorway"
            | "trunk"
            | "primary"
            | "secondary"
            | "tertiary"
            | "unclassified"
            | "residential"
            | "road"
            | "living_street"
    )
}

/// The traffic islands that are mapped, by where.
pub(super) struct Islands(Vec<[f64; 4]>);

impl Islands {
    pub(super) fn new(areas: &[Area]) -> Self {
        let islands = areas.iter().filter(|a| a.kind == AreaKind::Island);
        Self(islands.map(|a| bounds(&a.ring)).collect())
    }

    /// Whether one is where a ring lies.
    pub(super) fn near(&self, ring: &[Point]) -> bool {
        let [west, north, east, south] = bounds(ring);
        let touches = |b: &[f64; 4]| b[0] < east && b[2] > west && b[1] < south && b[3] > north;
        self.0.iter().any(touches)
    }
}

/// A box round points: west, north, east, south.
fn bounds(points: &[Point]) -> [f64; 4] {
    let mut out = [f64::MAX, f64::MAX, f64::MIN, f64::MIN];
    for p in points {
        out = [
            out[0].min(p[0]),
            out[1].min(p[1]),
            out[2].max(p[0]),
            out[3].max(p[1]),
        ];
    }
    out
}

/// What to walk on these tags are of, if a path with them is an `area`.
fn walked<'a>(get: impl Fn(&str) -> Option<&'a str>, area: bool) -> Option<AreaKind> {
    let of = |value: &str| match value {
        "footway" => Some(AreaKind::Footway),
        "pedestrian" => Some(AreaKind::Pedestrian),
        _ => None,
    };
    let outline = get("area:highway").and_then(of);
    outline.or_else(|| get("highway").filter(|_| area).and_then(of))
}

/// Whether what has these tags lies on the ground under the sky.
pub(super) fn open<'a>(get: impl Fn(&str) -> Option<&'a str>) -> bool {
    let within = |value: &str| {
        matches!(
            value,
            "underground"
                | "multi-storey"
                | "rooftop"
                | "roof"
                | "sheds"
                | "carports"
                | "garage_boxes"
        )
    };
    let hidden = get("parking").is_some_and(within)
        || get("location").is_some_and(within)
        || get("building").is_some_and(|v| v != "no")
        || get("indoor").is_some_and(|v| v != "no")
        || get("covered").is_some_and(|v| v != "no")
        || [get("layer"), get("level")]
            .into_iter()
            .flatten()
            .any(|v| v.trim() != "0")
        || profile::level(&get) != Level::Ground;
    !hidden
}

fn platform<'a>(get: impl Fn(&str) -> Option<&'a str>) -> bool {
    match get("railway") {
        Some(railway) => railway == "platform",
        None => get("public_transport") == Some("platform"),
    }
}

/// Metres across a platform that is mapped as a line, and those believed.
const PLATFORM: f64 = 3.0;
const PLATFORMS: std::ops::RangeInclusive<f64> = 1.0..=20.0;

/// Metres across a platform that a way with these tags, not closed, runs along,
/// and whether it is paved. One that is a path too has its path's surface.
pub(super) fn platform_line<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<(f64, bool)> {
    let path = !matches!(get("highway"), None | Some("platform" | "bus_stop"));
    if path || !platform(&get) || !open(&get) {
        return None;
    }
    let width = get("width")
        .and_then(profile::metres)
        .filter(|w| PLATFORMS.contains(w))
        .unwrap_or(PLATFORM);
    Some((width, profile::paved(&get)))
}

/// The areas of a platform along a line.
pub(super) fn along(points: &[Point], width: f64, paved: bool) -> Vec<Area> {
    let shapes = geometry::union(&geometry::ribbon(points, width));
    let area = |shape: Vec<Vec<Point>>| {
        let mut rings = shape.into_iter();
        Some(Area {
            kind: AreaKind::Platform,
            paved,
            ring: rings.next()?,
            holes: rings.collect(),
        })
    };
    shapes.into_iter().filter_map(area).collect()
}

/// What a multipolygon is, whether paved, and its ways and whether each is of its
/// outside.
pub(super) type Multipolygon = (AreaKind, bool, Vec<(i64, bool)>);

/// A multipolygon with these tags. Platforms, carriageways, car parks and what
/// is walked on, alone.
pub(super) fn multipolygon(relation: &NormalizedRelation) -> Option<Multipolygon> {
    let get = |key: &str| relation.tags.get(key);
    if get("type") != Some("multipolygon") {
        return None;
    }
    let (kind, paved) = match area(get) {
        Some((kind @ (AreaKind::Platform | AreaKind::Road | AreaKind::Parking), paved)) => {
            (kind, paved)
        }
        // an area without saying so
        Some((kind, _)) if !kind.is_path() => return None,
        _ => (
            walked(get, true).filter(|_| open(get))?,
            profile::paved(get),
        ),
    };
    let ways = relation
        .members
        .iter()
        .filter(|m| m.member_type == RelationMemberType::Way)
        .map(|m| (m.member_id, m.role != "inner"))
        .collect();
    Some((kind, paved, ways))
}

/// The rings that lines add up to, end to end; what does not close is left out.
pub(super) fn joined(mut lines: Vec<Vec<i64>>) -> Vec<Vec<i64>> {
    let mut out = Vec::new();
    while let Some(mut ring) = lines.pop() {
        while ring[0] != ring[ring.len() - 1] {
            let end = ring[ring.len() - 1];
            let next = |l: &Vec<i64>| l[0] == end || l[l.len() - 1] == end;
            let Some(at) = lines.iter().position(next) else {
                break;
            };
            let mut line = lines.remove(at);
            if line[0] != end {
                line.reverse();
            }
            ring.extend(line.into_iter().skip(1));
        }
        if ring.len() > 3 && ring[0] == ring[ring.len() - 1] {
            out.push(ring);
        }
    }
    out
}

/// `points` closed and wound as a widened line's outline, if they enclose any
/// ground.
pub(super) fn ring(points: &[Point]) -> Option<Vec<Point>> {
    let mut points = points.to_vec();
    points.dedup();
    if points.len() > 1 && points[0] == points[points.len() - 1] {
        points.pop();
    }
    if points.len() < 3 {
        return None;
    }
    let origin = points[0];
    let twice: f64 = (0..points.len())
        .map(|i| {
            let (a, b) = (points[i], points[(i + 1) % points.len()]);
            (a[0] - origin[0]) * (b[1] - origin[1]) - (b[0] - origin[0]) * (a[1] - origin[1])
        })
        .sum();
    if twice == 0.0 {
        return None;
    }
    if twice < 0.0 {
        points.reverse();
    }
    Some(points)
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(super) struct Barrier {
    pub kind: &'static str,
    pub height: f64,
}

/// The barrier a way with these tags is, of those that line roads.
pub(super) fn barrier<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<Barrier> {
    let (kind, usual) = match get("barrier")? {
        "guard_rail" => ("guard_rail", 0.75),
        "jersey_barrier" => ("jersey", 0.9),
        "cable_barrier" => ("cable", 0.75),
        "noise_barrier" => ("noise", 3.0),
        "wall" if matches!(get("wall"), Some("noise_barrier" | "noise_wall")) => ("noise", 3.0),
        _ => return None,
    };
    if get("tunnel").is_some_and(|v| v != "no") {
        return None;
    }
    let height = get("height")
        .and_then(profile::metres)
        .filter(|h| (0.3..=8.0).contains(h))
        .unwrap_or(usual);
    Some(Barrier { kind, height })
}

/// A stretch of a barrier on one thing.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct Stretch {
    pub barrier: Barrier,
    pub points: Vec<Point>,
    /// The group and the piece of it, a bridge or the road leading up to one,
    /// that it stands on; else it stands on the ground.
    pub on: Option<(u32, u32)>,
    /// The tile it is in.
    pub tile: (u32, u32),
}

/// Metres of a barrier that stand on one thing or another.
const STEP: f64 = 5.0;
/// Metres beyond a bridge's edge a barrier along it is its own, and within it
/// its parapet.
const BESIDE: f64 = 1.5;
const PARAPET: f64 = 0.8;

/// The barriers, cut where they come onto a bridge or leave it and at the tiles
/// `tile` tells (by a point). A bridge's parapets are its own: what runs along
/// its edges is left out.
pub(super) fn stretches(
    ways: &[Way],
    index: &Index,
    barriers: &[(Vec<Point>, f64, Barrier)],
    tile: impl Fn(Point) -> (u32, u32),
) -> Vec<Stretch> {
    let mut out: Vec<Stretch> = Vec::new();
    for (points, scale, barrier) in barriers {
        let mut open = false;
        for p in points.windows(2) {
            let length = distance(p[0], p[1]);
            let parts = (length / (STEP * scale)).ceil().max(1.0) as u32;
            let at = |k: u32| {
                let t = f64::from(k) / f64::from(parts);
                [
                    p[0][0] + (p[1][0] - p[0][0]) * t,
                    p[0][1] + (p[1][1] - p[0][1]) * t,
                ]
            };
            for k in 0..parts {
                let (a, b) = (at(k), at(k + 1));
                let middle = [(a[0] + b[0]) / 2.0, (a[1] + b[1]) / 2.0];
                let mut tile = tile(middle);
                let mut on = None;
                if let Some(found) = index.along(ways, a, b, BESIDE) {
                    if !found.approach && !found.rail && found.inside < PARAPET {
                        open = false;
                        continue;
                    }
                    on = Some((found.group, found.piece));
                    tile = found.tile;
                }
                match out.last_mut() {
                    Some(s) if open && (s.on, s.tile) == (on, tile) => s.points.push(b),
                    _ => out.push(Stretch {
                        barrier: *barrier,
                        points: vec![a, b],
                        on,
                        tile,
                    }),
                }
                open = true;
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::profile::profile;
    use super::super::structures;
    use super::*;

    #[test]
    fn areas_are_those_on_the_ground_under_the_sky() {
        let read = |tags: &[(&str, &'static str)]| {
            area(|k| tags.iter().find(|t| t.0 == k).map(|t| t.1)).map(|a| a.0)
        };
        assert_eq!(read(&[("amenity", "parking")]), Some(AreaKind::Parking));
        assert_eq!(
            read(&[("amenity", "parking_space"), ("level", "0")]),
            Some(AreaKind::Bay)
        );
        assert_eq!(
            read(&[("area:highway", "traffic_island")]),
            Some(AreaKind::Island)
        );
        for hidden in [
            ("parking", "underground"),
            ("layer", "-1"),
            ("level", "-2"),
            ("covered", "yes"),
            ("building", "yes"),
        ] {
            assert_eq!(read(&[("amenity", "parking"), hidden]), None, "{hidden:?}");
        }
        // a calmed road is no island
        assert_eq!(
            read(&[("highway", "service"), ("traffic_calming", "island")]),
            None
        );
    }

    #[test]
    fn an_area_adds_to_the_surfaces_it_overlaps_however_it_is_drawn() {
        let square = [
            [0.0, 0.0],
            [20.0, 0.0],
            [20.0, 20.0],
            [0.0, 20.0],
            [0.0, 0.0],
        ];
        let turned: Vec<Point> = square.iter().rev().copied().collect();
        for points in [&square[..], &turned] {
            let mut shapes = geometry::ribbon(&[[10.0, 10.0], [60.0, 10.0]], 6.0);
            shapes.push(vec![ring(points).unwrap()]);
            let union = geometry::union(&shapes);
            assert_eq!((union.len(), union[0].len()), (1, 1));
        }
        assert!(ring(&[[0.0, 0.0], [1.0, 1.0], [0.0, 0.0]]).is_none());
        assert!(ring(&[[0.0, 0.0], [1.0, 1.0], [2.0, 2.0], [0.0, 0.0]]).is_none());
    }

    #[test]
    fn a_platform_is_an_area_or_as_wide_as_its_line_says() {
        let tags = |tags: &[(&'static str, &'static str)]| {
            let tags = tags.to_vec();
            move |key: &str| tags.iter().find(|t| t.0 == key).map(|t| t.1)
        };
        let railway = ("railway", "platform");
        let of_bus = ("public_transport", "platform");
        let kind = |t: &[(&'static str, &'static str)]| area(tags(t)).map(|a| a.0);
        for platform in [railway, of_bus] {
            assert_eq!(kind(&[platform]), Some(AreaKind::Platform));
            assert_eq!(kind(&[platform, ("level", "1")]), None);
            assert_eq!(platform_line(tags(&[platform])), Some((3.0, true)));
        }
        assert_eq!(kind(&[of_bus, ("railway", "proposed")]), None);
        let line = |t| platform_line(tags(&[railway, t]));
        assert_eq!(line(("width", "4.5")), Some((4.5, true)));
        assert_eq!(line(("width", "450")), Some((3.0, true)));
        assert_eq!(line(("surface", "gravel")), Some((3.0, false)));
        assert_eq!(line(("highway", "footway")), None);
        assert_eq!(line(("covered", "yes")), None);
        let areas = along(&[[0.0, 0.0], [50.0, 0.0]], 4.0, true);
        assert_eq!(areas.len(), 1);
        let ring = ring(&areas[0].ring).unwrap();
        assert_eq!(ring, areas[0].ring, "wound as others are");
    }

    #[test]
    fn squares_are_paths_that_are_areas() {
        let read = |tags: &[(&str, &'static str)]| {
            area(|k| tags.iter().find(|t| t.0 == k).map(|t| t.1)).map(|a| a.0)
        };
        let square = ("highway", "pedestrian");
        let area = ("area", "yes");
        assert_eq!(read(&[square, area]), Some(AreaKind::Pedestrian));
        assert_eq!(
            read(&[("highway", "footway"), area]),
            Some(AreaKind::Footway)
        );
        for outline in ["footway", "pedestrian"] {
            let kind = read(&[("area:highway", outline)]).unwrap();
            assert_eq!((kind.surface(), kind.is_path()), (outline, true));
        }
        // a street round a block, and what is not on the ground
        assert_eq!(read(&[square]), None);
        assert_eq!(read(&[square, area, ("layer", "1")]), None);
        assert_eq!(read(&[("highway", "service"), area]), None);
    }

    #[test]
    fn a_ring_is_put_together_from_its_lines() {
        let rings = joined(vec![
            vec![1, 2, 3],
            vec![5, 4, 3],
            vec![5, 6, 1],
            vec![7, 8, 9, 7],
            vec![20, 21],
        ]);
        assert_eq!(rings.len(), 2, "{rings:?}");
        let mut all = rings[1].clone();
        all.sort_unstable();
        all.dedup();
        assert_eq!((rings[0].len(), all), (4, vec![1, 2, 3, 4, 5, 6]));
    }

    #[test]
    fn a_barrier_is_the_grounds_the_bridges_or_its_parapet() {
        let way = |id: i64, tags: &[(&str, &'static str)], nodes: &[(i64, f64, f64)]| Way {
            id,
            nodes: nodes.iter().map(|n| n.0).collect(),
            points: nodes.iter().map(|n| [n.1, n.2]).collect(),
            profile: profile(|k| tags.iter().find(|t| t.0 == k).map(|t| t.1)).unwrap(),
            scale: 1.0,
        };
        let ways = [
            way(
                1,
                &[("highway", "primary"), ("lanes", "4"), ("bridge", "yes")],
                &[(1, 0.0, 0.0), (2, 100.0, 0.0)],
            ),
            way(
                2,
                &[("highway", "primary"), ("lanes", "4")],
                &[(2, 100.0, 0.0), (3, 300.0, 0.0), (4, 500.0, 0.0)],
            ),
        ];
        let groups = structures::build(&ways, &[]);
        let index = Index::new(&groups, &ways, |_| (7, 7));
        let rail = Barrier {
            kind: "guard_rail",
            height: 0.75,
        };
        let cut = |points: Vec<Point>| stretches(&ways, &index, &[(points, 1.0, rail)], |_| (1, 1));
        // down the middle: on the bridge, on its approach, then on the ground
        let middle = cut(vec![[-40.0, 0.2], [400.0, 0.2]]);
        let on: Vec<_> = middle.iter().map(|s| (s.on, s.tile)).collect();
        assert_eq!(
            on,
            [
                (None, (1, 1)),
                (Some((0, 0)), (7, 7)),
                (Some((0, 1)), (7, 7)),
                (None, (1, 1))
            ]
        );
        // along the edge the bridge has its parapet
        let edge = cut(vec![[-40.0, 7.0], [90.0, 7.0]]);
        assert_eq!(edge.len(), 1);
        assert_eq!(edge[0].points.last(), Some(&[0.0, 7.0]));
        // across it, under it: on the ground
        let under = cut(vec![[50.0, -40.0], [50.0, 40.0]]);
        assert_eq!((under.len(), under[0].on), (1, None));
    }
}
