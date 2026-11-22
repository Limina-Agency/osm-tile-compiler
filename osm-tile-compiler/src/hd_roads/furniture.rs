//! What stands or lies beside a road and is mapped on its own: parking,
//! traffic islands, and the barriers along it.

use super::network::{Point, Way, distance};
use super::profile::{self, Level};
use super::structures::Index;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum AreaKind {
    /// Ground to park on.
    Parking,
    /// One bay of it, outlined.
    Bay,
    /// Kerbed, in the road.
    Island,
}

#[derive(Debug, Clone, PartialEq)]
pub(super) struct Area {
    pub kind: AreaKind,
    pub paved: bool,
    /// Its ring, closed, wound as a widened line's.
    pub ring: Vec<Point>,
}

/// What a closed way with these tags is, if it lies on the ground under the sky.
pub(super) fn area<'a>(get: impl Fn(&str) -> Option<&'a str>) -> Option<(AreaKind, bool)> {
    let kind = match (get("amenity"), get("area:highway"), get("traffic_calming")) {
        (Some("parking"), ..) => AreaKind::Parking,
        (Some("parking_space"), ..) => AreaKind::Bay,
        (_, Some("traffic_island"), _) => AreaKind::Island,
        (_, _, Some("island")) if get("highway").is_none() => AreaKind::Island,
        _ => return None,
    };
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
    (!hidden).then(|| (kind, profile::paved(&get)))
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
                    if !found.approach && found.inside < PARAPET {
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
    use super::super::geometry;
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
