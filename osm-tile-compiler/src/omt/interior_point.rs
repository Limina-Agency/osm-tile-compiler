//! World-Mercator point placement matching the reference's JTS 1.20.0.
//!
//! Interior-point algorithms adapted from JTS InteriorPointArea/Line/Point,
//! Copyright (c) 2016 Vivid Solutions, under Eclipse Distribution License 1.0.
//! SPDX-License-Identifier: EDL-1.0
//! Modified for osm-tile-compiler: native Rust geometry traversal and Mercator adapters.
//! Source: org/locationtech/jts/algorithm/*.java embedded in Planetiler 0.10.2.
//! Convexity and projection follow Planetiler GeoUtils at core 0e5588c4.
use geo::{Centroid, MapCoords};
use geo_types::{Coord, Geometry, LineString, Point, Polygon};
use std::f64::consts::PI;

fn inverse(p: Coord<f64>) -> Point<f64> {
    let n = PI - 2.0 * PI * p.y;
    Point::new(
        p.x * 360.0 - 180.0,
        (0.5 * (n.exp() - (-n).exp())).atan().to_degrees(),
    )
}
pub(super) fn world_to_latlon(p: Coord<f64>) -> Point<f64> {
    inverse(p)
}
fn project(p: Coord<f64>) -> Coord<f64> {
    let max_lat = inverse(Coord { x: 0.5, y: -0.1 }).y();
    let min_lat = inverse(Coord { x: 0.5, y: 1.1 }).y();
    let sin = p.y.to_radians().sin();
    Coord {
        x: (p.x + 180.0) / 360.0,
        y: if p.y <= min_lat {
            1.1
        } else if p.y >= max_lat {
            -0.1
        } else {
            0.5 - 0.25 * ((1.0 + sin) / (1.0 - sin)).ln() / PI
        },
    }
}
pub(super) fn projected(geometry: &Geometry<f64>) -> Geometry<f64> {
    geometry.map_coords(project)
}

// Pinned OSM node-store representation: GeoUtils.encodeFlatLocation/decodeWorld.
// Apply only to original OSM coordinates before geometry operations, never to
// generated label points or external dataset geometries.
pub(super) fn projected_osm(geometry: &Geometry<f64>) -> Geometry<f64> {
    const HALF_WORLD: f64 = 1_073_741_824.0;
    geometry.map_coords(|p| {
        let p = project(p);
        let quantize = |v: f64| {
            let packed = ((v + 1.0) * HALF_WORLD) as i64;
            ((packed as u64 & 0xffff_ffff) as f64 / HALF_WORLD) - 1.0
        };
        Coord {
            x: quantize(p.x),
            y: quantize(p.y),
        }
    })
}

#[cfg(test)]
pub fn osm_point_then_centroid_if_convex(geometry: &Geometry<f64>) -> Option<Point<f64>> {
    osm_point_then_centroid_if_convex_world(geometry).map(inverse)
}

pub(super) fn osm_centroid_world(geometry: &Geometry<f64>) -> Option<Coord<f64>> {
    projected_osm(geometry).centroid().map(|p| p.0)
}

pub(super) fn osm_point_world(geometry: &Geometry<f64>) -> Option<Coord<f64>> {
    interior(&projected_osm(geometry)).map(|p| p.0)
}

pub(super) fn osm_point_then_centroid_if_convex_world(
    geometry: &Geometry<f64>,
) -> Option<Coord<f64>> {
    select_point_then_centroid_world(&projected_osm(geometry))
}

/// Geometry centroid computed in the same normalized Mercator plane as JTS.
pub fn centroid(geometry: &Geometry<f64>) -> Option<Point<f64>> {
    projected(geometry).centroid().map(|p| inverse(p.0))
}

/// Planetiler's centroidIfConvex policy: only a single approximately convex,
/// hole-free polygon uses its centroid; other polygons use pointOnSurface.
/// Retained as an independent two-step policy oracle for the shared-work tests.
#[cfg(test)]
pub fn centroid_if_convex(geometry: &Geometry<f64>) -> Option<Point<f64>> {
    let world = projected(geometry);
    let world_point = if prefers_centroid(&world) {
        world.centroid()
    } else {
        interior(&world)
    }?;
    Some(inverse(world_point.0))
}

fn prefers_centroid(world: &Geometry<f64>) -> bool {
    match world {
        Geometry::Polygon(p) => p.interiors().is_empty() && is_convex(p.exterior()),
        Geometry::MultiPolygon(_) => false,
        _ => true,
    }
}

/// Preserve the profile's point-on-surface eligibility/fallback, while sharing
/// projection and interior-point work with the subsequent centroid policy.
pub fn point_then_centroid_if_convex(geometry: &Geometry<f64>) -> Option<Point<f64>> {
    select_point_then_centroid(&projected(geometry))
}

fn select_point_then_centroid(world: &Geometry<f64>) -> Option<Point<f64>> {
    select_point_then_centroid_world(world).map(inverse)
}

fn select_point_then_centroid_world(world: &Geometry<f64>) -> Option<Coord<f64>> {
    let surface = interior(world)?;
    let selected = if prefers_centroid(world) {
        world.centroid().unwrap_or(surface)
    } else {
        surface
    };
    Some(selected.0)
}

/// JTS pointOnSurface: widest polygon scanline, closest interior line vertex,
/// or closest input point to the centroid. Strict ties preserve source order.
pub fn point(geometry: &Geometry<f64>) -> Option<Point<f64>> {
    interior(&projected(geometry)).map(|p| inverse(p.0))
}

fn interior(geometry: &Geometry<f64>) -> Option<Point<f64>> {
    let mut polygons = Vec::new();
    let mut lines = Vec::new();
    let mut points = Vec::new();
    collect(geometry, &mut polygons, &mut lines, &mut points);
    if !polygons.is_empty() {
        let mut best = None;
        let mut widest = -1.0;
        for polygon in polygons {
            if let Some((candidate, width)) = polygon_point(polygon)
                && width > widest
            {
                widest = width;
                best = Some(candidate);
            }
        }
        return best.map(Point::from);
    }
    let center = geometry.centroid()?;
    let mut closest = None;
    let mut distance = f64::MAX;
    let mut consider = |p: Coord<f64>| {
        let d = (p.x - center.x()).hypot(p.y - center.y());
        if d < distance {
            distance = d;
            closest = Some(Point::from(p));
        }
    };
    if !lines.is_empty() {
        let has_interior = lines.iter().any(|line| line.0.len() > 2);
        for line in lines {
            if has_interior {
                for p in line.0.iter().skip(1).take(line.0.len().saturating_sub(2)) {
                    consider(*p);
                }
            } else if let (Some(first), Some(last)) = (line.0.first(), line.0.last()) {
                consider(*first);
                consider(*last);
            }
        }
    } else {
        for p in points {
            consider(p);
        }
    }
    closest
}

fn collect<'a>(
    geometry: &'a Geometry<f64>,
    polygons: &mut Vec<&'a Polygon<f64>>,
    lines: &mut Vec<&'a LineString<f64>>,
    points: &mut Vec<Coord<f64>>,
) {
    match geometry {
        Geometry::Polygon(p) if !p.exterior().0.is_empty() => polygons.push(p),
        Geometry::MultiPolygon(p) => {
            polygons.extend(p.0.iter().filter(|p| !p.exterior().0.is_empty()))
        }
        Geometry::LineString(line) if !line.0.is_empty() => lines.push(line),
        Geometry::MultiLineString(multi) => {
            lines.extend(multi.0.iter().filter(|line| !line.0.is_empty()))
        }
        Geometry::Point(p) => points.push(p.0),
        Geometry::MultiPoint(multi) => points.extend(multi.0.iter().map(|p| p.0)),
        Geometry::GeometryCollection(collection) => {
            for part in &collection.0 {
                collect(part, polygons, lines, points);
            }
        }
        _ => {}
    }
}

fn polygon_point(polygon: &Polygon<f64>) -> Option<(Coord<f64>, f64)> {
    let first = *polygon.exterior().0.first()?;
    let mut low = f64::INFINITY;
    let mut high = f64::NEG_INFINITY;
    for p in &polygon.exterior().0 {
        if !p.x.is_finite() || !p.y.is_finite() {
            return None;
        }
        low = low.min(p.y);
        high = high.max(p.y);
    }
    let center = (low + high) / 2.0;
    // Shrink the central interval to its closest vertex ordinates, including holes.
    for ring in std::iter::once(polygon.exterior()).chain(polygon.interiors()) {
        for p in &ring.0 {
            if !p.x.is_finite() || !p.y.is_finite() {
                return None;
            }
            if p.y <= center && p.y > low {
                low = p.y;
            } else if p.y > center && p.y < high {
                high = p.y;
            }
        }
    }
    let scan_y = (high + low) / 2.0;
    let mut crossings = Vec::new();
    for ring in std::iter::once(polygon.exterior()).chain(polygon.interiors()) {
        for edge in ring.0.windows(2) {
            let (a, b) = (edge[0], edge[1]);
            if (a.y > scan_y && b.y > scan_y)
                || (a.y < scan_y && b.y < scan_y)
                || a.y == b.y
                || (a.y == scan_y && b.y < scan_y)
                || (b.y == scan_y && a.y < scan_y)
            {
                continue;
            }
            // Preserve JTS's slope/division evaluation order rather than t*dx.
            crossings.push(if a.x == b.x {
                a.x
            } else {
                a.x + (scan_y - a.y) / ((b.y - a.y) / (b.x - a.x))
            });
        }
    }
    // JTS signals an odd crossing count as a robustness failure. Do not invent
    // a point by pairing a truncated list for malformed geometry.
    if crossings.len() % 2 != 0 {
        return None;
    }
    crossings.sort_by(f64::total_cmp);
    let mut best = first;
    let mut width = 0.0;
    for pair in crossings.as_chunks::<2>().0 {
        let candidate_width = pair[1] - pair[0];
        if candidate_width > width {
            width = candidate_width;
            best = Coord {
                x: (pair[0] + pair[1]) / 2.0,
                y: scan_y,
            };
        }
    }
    Some((best, width))
}

fn is_convex(ring: &LineString<f64>) -> bool {
    let points = &ring.0;
    let size = points.len();
    if size <= 3 {
        return false;
    }
    let first = points[0];
    let Some(mut i) = (1..size).find(|i| points[*i] != first) else {
        return false;
    };
    let mut current = points[i];
    let mut dx1 = current.x - first.x;
    let mut dy1 = current.y - first.y;
    let (mut neg, mut pos) = (1e-20_f64, 1e-20_f64);
    while i <= size + 1 {
        let next = points[if i < size { i } else { i + 1 - size }];
        let dx2 = next.x - current.x;
        let dy2 = next.y - current.y;
        let z = dx1 * dy2 - dy1 * dx2;
        let mut extended = false;
        if z < 0.0 && z.abs() > neg {
            neg = z.abs();
            extended = true;
        } else if z > 0.0 && z.abs() > pos {
            pos = z.abs();
            extended = true;
        }
        if (i == 10 || (i > 10 && extended)) && neg.min(pos) / neg.max(pos) > 1e-3 {
            return false;
        }
        current = next;
        dx1 = dx2;
        dy1 = dy2;
        i += 1;
    }
    neg.min(pos) / neg.max(pos) < 1e-3
}

#[cfg(test)]
mod tests {
    #[test]
    fn actual_osm_rings_match_pinned_java_label_oracle_after_source_quantization() {
        type Case = (u64, Vec<(f64, f64)>, (i64, i64));
        let cases: Vec<Case> = vec![
            (
                131582600,
                vec![
                    (19.9023053, 50.0658312),
                    (19.902097, 50.0658338),
                    (19.9020982, 50.0658475),
                    (19.9021219, 50.0661463),
                    (19.9021503, 50.0661457),
                    (19.9023198, 50.0661437),
                    (19.9023053, 50.0658312),
                ],
                (37264473, 22740468),
            ),
            (
                219293626,
                vec![
                    (19.187933, 50.014341),
                    (19.187936, 50.014274),
                    (19.187989, 50.014275),
                    (19.187991, 50.014234),
                    (19.188095, 50.014236),
                    (19.188092, 50.014344),
                    (19.187933, 50.014341),
                ],
                (37131337, 22755470),
            ),
            (
                385998590,
                vec![
                    (19.4904711, 50.267324),
                    (19.4904668, 50.2673449),
                    (19.4904127, 50.2673403),
                    (19.4904171, 50.2673194),
                    (19.4903562, 50.2673141),
                    (19.4903741, 50.2672281),
                    (19.4906156, 50.2672496),
                    (19.4905891, 50.2673603),
                    (19.4905787, 50.2674001),
                    (19.4905324, 50.2673953),
                    (19.4905456, 50.2673496),
                    (19.490526, 50.2673288),
                    (19.4904711, 50.267324),
                ],
                (37187722, 22681890),
            ),
            (
                1181818130,
                vec![
                    (19.2021938, 50.0252181),
                    (19.2021241, 50.0252353),
                    (19.2023062, 50.0253928),
                    (19.2023504, 50.0253532),
                    (19.2021938, 50.0252181),
                ],
                (37133989, 22752281),
            ),
        ];
        for (id, coords, expected) in cases {
            let geometry = Geometry::Polygon(Polygon::new(LineString::from(coords), vec![]));
            let label = osm_point_then_centroid_if_convex(&geometry).unwrap();
            let p = project(label.0);
            assert_eq!(
                (
                    (p.x * 67108864.0 + 0.5).floor() as i64,
                    (p.y * 67108864.0 + 0.5).floor() as i64
                ),
                expected,
                "OSM way {id}"
            );
        }
    }

    use super::*;
    use geo_types::{MultiPoint, MultiPolygon};
    fn square(x: f64, y: f64, w: f64, h: f64) -> Polygon<f64> {
        Polygon::new(
            LineString::from(vec![(x, y), (x + w, y), (x + w, y + h), (x, y + h), (x, y)]),
            vec![],
        )
    }
    #[test]
    fn shared_projection_preserves_two_step_label_policy() {
        let hole = Polygon::new(
            square(0., 40., 10., 10.).exterior().clone(),
            vec![square(4., 42., 2., 6.).exterior().clone()],
        );
        let cases = vec![
            Geometry::Point(Point::new(19., 50.)),
            Geometry::MultiPoint(MultiPoint::from(vec![(0., 0.), (2., 0.), (10., 0.)])),
            Geometry::Polygon(square(0., 40., 10., 10.)),
            Geometry::Polygon(hole),
            Geometry::Polygon(Polygon::new(
                LineString::from(vec![(0., 0.), (6., 0.), (0., 6.), (0., 0.)]),
                vec![],
            )),
            Geometry::Polygon(Polygon::new(
                LineString::from(vec![
                    (0., 0.),
                    (5., 0.),
                    (2., 2.),
                    (5., 5.),
                    (0., 5.),
                    (0., 0.),
                ]),
                vec![],
            )),
            Geometry::MultiPolygon(MultiPolygon(vec![
                square(0., 40., 2., 2.),
                square(8., 40., 5., 1.),
            ])),
            Geometry::MultiPolygon(MultiPolygon(vec![])),
            Geometry::Polygon(Polygon::new(LineString::new(vec![]), vec![])),
            Geometry::Polygon(Polygon::new(
                LineString::from(vec![(0., 0.), (0., 0.), (0., 0.)]),
                vec![],
            )),
            Geometry::Polygon(Polygon::new(
                LineString::from(vec![(0., 0.), (f64::NAN, 1.), (1., 1.), (0., 0.)]),
                vec![],
            )),
        ];
        for geometry in cases {
            let previous = point(&geometry).map(|p| centroid_if_convex(&geometry).unwrap_or(p));
            assert_eq!(point_then_centroid_if_convex(&geometry), previous);
        }
    }
    #[test]
    fn multipolygon_selects_widest_scanline_not_largest_area() {
        let geometry = Geometry::MultiPolygon(MultiPolygon(vec![
            square(0., 0., 1., 10.),
            square(10., 0., 4., 1.),
        ]));
        let p = point(&geometry).unwrap();
        assert!((p.x() - 12.).abs() < 1e-10);
        assert!(p.y() > 0.49 && p.y() < 0.51);
    }
    #[test]
    fn holes_select_nearest_safe_scanline_and_leftmost_tied_span() {
        let polygon = Polygon::new(
            square(0., 0., 10., 10.).exterior().clone(),
            vec![square(4., 2., 2., 6.).exterior().clone()],
        );
        // Work directly in normalized coordinates to exercise exact JTS tie rules.
        let (p, width) = polygon_point(&polygon).unwrap();
        assert_eq!(p, Coord { x: 2., y: 5. });
        assert_eq!(width, 4.);
    }
    #[test]
    fn safe_scanline_avoids_central_vertex_ordinate() {
        let polygon = Polygon::new(
            LineString::from(vec![
                (0., 0.),
                (10., 0.),
                (10., 10.),
                (5., 5.),
                (0., 10.),
                (0., 0.),
            ]),
            vec![],
        );
        let (p, _) = polygon_point(&polygon).unwrap();
        assert_eq!(p.y, 7.5);
        assert_eq!(p.x, 1.25);
    }
    #[test]
    fn geographic_scanline_uses_mercator_not_latitude_average() {
        let p = point(&Geometry::Polygon(square(0., 50., 10., 20.))).unwrap();
        let expected = inverse(Coord {
            x: 0.5,
            y: (project(Coord { x: 0., y: 50. }).y + project(Coord { x: 0., y: 70. }).y) / 2.,
        })
        .y();
        assert!((p.y() - expected).abs() < 1e-10);
        assert!((p.y() - 60.).abs() > 1.0);
    }
    #[test]
    fn lines_and_multipoints_return_existing_vertices() {
        let line = Geometry::LineString(LineString::from(vec![(0., 0.), (2., 0.), (10., 0.)]));
        assert!((point(&line).unwrap().x() - 2.).abs() < 1e-10);
        let points = Geometry::MultiPoint(MultiPoint::from(vec![(0., 0.), (2., 0.), (10., 0.)]));
        assert!((point(&points).unwrap().x() - 2.).abs() < 1e-10);
    }
    #[test]
    fn convex_centroid_differs_from_scanline_for_triangle() {
        let polygon = Geometry::Polygon(Polygon::new(
            LineString::from(vec![(0., 0.), (6., 0.), (0., 6.), (0., 0.)]),
            vec![],
        ));
        let c = centroid_if_convex(&polygon).unwrap();
        let p = point(&polygon).unwrap();
        assert!((c.x() - 2.).abs() < 1e-8);
        assert!((p.x() - 1.5).abs() < 1e-8);
        assert!(point(&Geometry::MultiPolygon(MultiPolygon(vec![]))).is_none());
    }
}
