#[cfg(test)]
use geo_types::{Coord, MultiPolygon};
use geo_types::{Geometry, LineString, Point, Polygon};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize, Default)]
pub struct WorldPoint {
    pub x: f64,
    pub y: f64,
}

impl WorldPoint {
    pub const fn new(x: f64, y: f64) -> Self {
        Self { x, y }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct WorldRingGroup {
    pub exterior: Vec<WorldPoint>,
    pub holes: Vec<Vec<WorldPoint>>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum WorldGeometry {
    Point(Vec<WorldPoint>),
    Line(Vec<Vec<WorldPoint>>),
    Polygon(Vec<WorldRingGroup>),
}

pub fn geometry_to_world(geometry: &Geometry<f64>, zoom: u8) -> Option<WorldGeometry> {
    match geometry {
        Geometry::Point(point) => Some(WorldGeometry::Point(vec![lng_lat_point_to_world(
            point, zoom,
        )])),
        Geometry::LineString(line) => Some(WorldGeometry::Line(vec![line_to_world(line, zoom)])),
        Geometry::MultiLineString(lines) => Some(WorldGeometry::Line(
            lines
                .0
                .iter()
                .map(|line| line_to_world(line, zoom))
                .collect(),
        )),
        Geometry::Polygon(polygon) => Some(WorldGeometry::Polygon(vec![polygon_to_world(
            polygon, zoom,
        )])),
        Geometry::MultiPolygon(polygons) => Some(WorldGeometry::Polygon(
            polygons
                .0
                .iter()
                .map(|polygon| polygon_to_world(polygon, zoom))
                .collect(),
        )),
        _ => None,
    }
}

pub fn line_to_world(line: &LineString<f64>, zoom: u8) -> Vec<WorldPoint> {
    line.points()
        .map(|point| lng_lat_to_world(point.x(), point.y(), zoom))
        .collect()
}

pub fn polygon_to_world(polygon: &Polygon<f64>, zoom: u8) -> WorldRingGroup {
    WorldRingGroup {
        exterior: line_to_world(polygon.exterior(), zoom),
        holes: polygon
            .interiors()
            .iter()
            .map(|ring| line_to_world(ring, zoom))
            .collect(),
    }
}

pub fn lng_lat_point_to_world(point: &Point<f64>, zoom: u8) -> WorldPoint {
    lng_lat_to_world(point.x(), point.y(), zoom)
}

pub fn lng_lat_to_world(lng: f64, lat: f64, zoom: u8) -> WorldPoint {
    use std::f64::consts::PI;

    let scale = f64::from(1u32 << zoom);
    let x = ((lng + 180.0) / 360.0) * scale;
    let lat_rad = lat.to_radians();
    let y = ((1.0 - (lat_rad.tan() + 1.0 / lat_rad.cos()).ln() / PI) / 2.0) * scale;
    WorldPoint::new(x, y)
}

pub fn world_bounds_for_lines(lines: &[Vec<WorldPoint>]) -> Option<(f64, f64, f64, f64)> {
    world_bounds_for_points(lines.iter().flatten())
}

fn world_bounds_for_points<'a>(
    points: impl Iterator<Item = &'a WorldPoint>,
) -> Option<(f64, f64, f64, f64)> {
    let mut min_x = f64::MAX;
    let mut max_x = f64::MIN;
    let mut min_y = f64::MAX;
    let mut max_y = f64::MIN;
    let mut found = false;
    for point in points {
        min_x = min_x.min(point.x);
        max_x = max_x.max(point.x);
        min_y = min_y.min(point.y);
        max_y = max_y.max(point.y);
        found = true;
    }
    found.then_some((min_x, max_x, min_y, max_y))
}

pub fn world_bounds_for_polygons(groups: &[WorldRingGroup]) -> Option<(f64, f64, f64, f64)> {
    world_bounds_for_points(groups.iter().flat_map(|group| {
        std::iter::once(&group.exterior)
            .chain(group.holes.iter())
            .flatten()
    }))
}

#[cfg(test)]
pub fn world_to_geo_polygons(groups: &[WorldRingGroup], zoom: u8) -> Geometry<f64> {
    let scale = f64::from(1u32 << zoom);
    let polygons = groups
        .iter()
        .map(|group| {
            let exterior = LineString::new(
                group
                    .exterior
                    .iter()
                    .map(|point| world_to_lng_lat_coord(*point, scale))
                    .collect(),
            );
            let holes = group
                .holes
                .iter()
                .map(|ring| {
                    LineString::new(
                        ring.iter()
                            .map(|point| world_to_lng_lat_coord(*point, scale))
                            .collect(),
                    )
                })
                .collect();
            Polygon::new(exterior, holes)
        })
        .collect::<Vec<_>>();
    match polygons.len() {
        0 => Geometry::MultiPolygon(MultiPolygon(vec![])),
        1 => Geometry::Polygon(polygons.into_iter().next().expect("single polygon")),
        _ => Geometry::MultiPolygon(MultiPolygon(polygons)),
    }
}

#[cfg(test)]
fn world_to_lng_lat_coord(point: WorldPoint, scale: f64) -> Coord<f64> {
    use std::f64::consts::PI;

    let lng = (point.x / scale) * 360.0 - 180.0;
    let merc_y = PI * (1.0 - 2.0 * point.y / scale);
    let lat = merc_y.sinh().atan().to_degrees();
    Coord { x: lng, y: lat }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn polygon_bounds_include_all_components_and_holes() {
        // Bounds must retain the previous all-coordinate behavior even for
        // malformed input whose hole extends beyond its shell.
        let groups = vec![
            WorldRingGroup::default(),
            WorldRingGroup {
                exterior: vec![WorldPoint::new(2.0, 3.0), WorldPoint::new(5.0, 6.0)],
                holes: vec![vec![], vec![WorldPoint::new(-7.0, 8.0)]],
            },
            WorldRingGroup {
                exterior: vec![WorldPoint::new(9.0, -10.0)],
                holes: vec![],
            },
        ];
        assert_eq!(
            world_bounds_for_polygons(&groups),
            Some((-7.0, 9.0, -10.0, 8.0))
        );
        assert_eq!(world_bounds_for_polygons(&[]), None);
        assert_eq!(
            world_bounds_for_polygons(&[WorldRingGroup::default()]),
            None
        );
    }

    #[test]
    fn converts_polygon_to_world_and_back() {
        let polygon = Geometry::Polygon(Polygon::new(
            LineString::new(vec![
                Coord { x: 19.0, y: 50.0 },
                Coord { x: 19.1, y: 50.0 },
                Coord { x: 19.1, y: 50.1 },
                Coord { x: 19.0, y: 50.1 },
                Coord { x: 19.0, y: 50.0 },
            ]),
            vec![],
        ));
        let world = geometry_to_world(&polygon, 10).unwrap();
        let WorldGeometry::Polygon(groups) = world else {
            panic!("expected polygon");
        };
        let round_trip = world_to_geo_polygons(&groups, 10);
        let Geometry::Polygon(round_polygon) = round_trip else {
            panic!("expected polygon");
        };
        assert_eq!(round_polygon.exterior().0.len(), 5);
    }
}
