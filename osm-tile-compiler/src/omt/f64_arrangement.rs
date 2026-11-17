//! Test-only, bounded f64 polygonization experiment for JTS buffer curves.
//! The production merge path remains unchanged while this is qualified against
//! captured Planetiler input. No fixed precision grid is used for topology.

use std::collections::{BTreeMap, BTreeSet};

use geo::{
    InteriorPoint, Line,
    algorithm::{
        kernels::{Kernel, Orientation, RobustKernel},
        line_intersection::{LineIntersection, line_intersection},
    },
};
use geo_types::{Coord, LineString, MultiPolygon, Polygon};

type Point = [f64; 2];
type Key = (u64, u64);

#[derive(Clone, Copy)]
struct Segment {
    a: Point,
    b: Point,
}

fn key(point: Point) -> Key {
    let canonical = |value: f64| {
        if value == 0.0 {
            0.0_f64.to_bits()
        } else {
            value.to_bits()
        }
    };
    (canonical(point[0]), canonical(point[1]))
}

fn line(segment: Segment) -> Line<f64> {
    Line::new(
        Coord {
            x: segment.a[0],
            y: segment.a[1],
        },
        Coord {
            x: segment.b[0],
            y: segment.b[1],
        },
    )
}

fn along_segment(segment: Segment, a: Point, b: Point) -> std::cmp::Ordering {
    let dx = segment.b[0] - segment.a[0];
    let dy = segment.b[1] - segment.a[1];
    let (first, second) = if dx.abs() >= dy.abs() { (0, 1) } else { (1, 0) };
    // SegmentPointComparator orders each axis by the segment's octant. On a
    // diagonal with decreasing y, two nodes can round to the same x while
    // retaining distinct y bits; reversing both axes together misorders them.
    let ordered = |axis: usize| {
        let comparison = a[axis].total_cmp(&b[axis]);
        if segment.b[axis] >= segment.a[axis] {
            comparison
        } else {
            comparison.reverse()
        }
    };
    ordered(first).then_with(|| ordered(second))
}

fn direction_order(origin: Point, a: Point, b: Point) -> std::cmp::Ordering {
    let vector = |point: Point| [point[0] - origin[0], point[1] - origin[1]];
    let va = vector(a);
    let vb = vector(b);
    let quadrant = |v: Point| match (v[0] >= 0.0, v[1] >= 0.0) {
        (true, true) => 0,
        (false, true) => 1,
        (false, false) => 2,
        (true, false) => 3,
    };
    quadrant(va).cmp(&quadrant(vb)).then_with(|| {
        let coord = |p: Point| Coord { x: p[0], y: p[1] };
        match RobustKernel::orient2d(coord(origin), coord(a), coord(b)) {
            Orientation::CounterClockwise => std::cmp::Ordering::Less,
            Orientation::Clockwise => std::cmp::Ordering::Greater,
            Orientation::Collinear => va[0].hypot(va[1]).total_cmp(&vb[0].hypot(vb[1])),
        }
    })
}

fn winding(curves: &[Vec<Point>], point: Point) -> i32 {
    let mut total = 0;
    for ring in curves {
        for index in 0..ring.len() {
            let a = ring[index];
            let b = ring[(index + 1) % ring.len()];
            let turn = (b[0] - a[0]) * (point[1] - a[1]) - (point[0] - a[0]) * (b[1] - a[1]);
            if a[1] <= point[1] && point[1] < b[1] && turn > 0.0 {
                total += 1;
            } else if b[1] <= point[1] && point[1] < a[1] && turn < 0.0 {
                total -= 1;
            }
        }
    }
    total
}

/// Polygonize noded raw curves and retain positive-winding bounded faces.
/// Returns `None` when bounds, finite coordinates, or graph consistency fail.
pub(super) fn positive_faces(curves: &[Vec<Point>]) -> Option<MultiPolygon<f64>> {
    let mut segments = Vec::new();
    for ring in curves {
        if ring.len() < 3 {
            return None;
        }
        for i in 0..ring.len() {
            let a = ring[i];
            let b = ring[(i + 1) % ring.len()];
            if !a.iter().chain(b.iter()).all(|v| v.is_finite()) {
                return None;
            }
            if a != b {
                segments.push(Segment { a, b });
            }
        }
    }
    if segments.len() > 8192 {
        return None;
    }
    graph_faces(curves, &segments, node_segments(&segments), true)
}

fn node_segments(segments: &[Segment]) -> Vec<Vec<Point>> {
    let mut cuts = segments.iter().map(|s| vec![s.a, s.b]).collect::<Vec<_>>();
    for i in 0..segments.len() {
        let a = segments[i];
        for j in i + 1..segments.len() {
            let b = segments[j];
            if a.a[0].min(a.b[0]) > b.a[0].max(b.b[0])
                || b.a[0].min(b.b[0]) > a.a[0].max(a.b[0])
                || a.a[1].min(a.b[1]) > b.a[1].max(b.b[1])
                || b.a[1].min(b.b[1]) > a.a[1].max(a.b[1])
            {
                continue;
            }
            let Some(intersection) = line_intersection(line(a), line(b)) else {
                continue;
            };
            let points: Vec<Point> = match intersection {
                LineIntersection::SinglePoint {
                    intersection,
                    is_proper,
                } => {
                    let fallback = [intersection.x, intersection.y];
                    let point = if is_proper {
                        let within = |p: Point, edge: Segment| {
                            p[0] >= edge.a[0].min(edge.b[0])
                                && p[0] <= edge.a[0].max(edge.b[0])
                                && p[1] >= edge.a[1].min(edge.b[1])
                                && p[1] <= edge.a[1].max(edge.b[1])
                        };
                        super::jts_intersection::intersection(a.a, a.b, b.a, b.b)
                            .filter(|&p| within(p, a) && within(p, b))
                            .unwrap_or(fallback)
                    } else {
                        fallback
                    };
                    vec![point]
                }
                LineIntersection::Collinear { intersection } => vec![
                    [intersection.start.x, intersection.start.y],
                    [intersection.end.x, intersection.end.y],
                ],
            };
            cuts[i].extend(&points);
            cuts[j].extend(points);
        }
    }
    cuts
}

/// Compare face extraction against a pinned JTS noding oracle without
/// conflating its segment intersections with this prototype's own noder.
#[cfg(test)]
fn positive_faces_from_noded_edges(
    curves: &[Vec<Point>],
    noded_edges: &[Vec<Point>],
    positive_winding: bool,
) -> Option<MultiPolygon<f64>> {
    let segments = noded_edges
        .iter()
        .flat_map(|edge| {
            edge.windows(2).map(|pair| Segment {
                a: pair[0],
                b: pair[1],
            })
        })
        .filter(|edge| edge.a != edge.b)
        .collect::<Vec<_>>();
    let cuts = node_segments(&segments);
    graph_faces(curves, &segments, cuts, positive_winding)
}

fn graph_faces(
    curves: &[Vec<Point>],
    segments: &[Segment],
    cuts: Vec<Vec<Point>>,
    positive_winding: bool,
) -> Option<MultiPolygon<f64>> {
    let mut vertices = Vec::<Point>::new();
    let mut lookup = BTreeMap::<Key, usize>::new();
    let mut undirected = BTreeSet::<(usize, usize)>::new();
    for (segment, mut points) in segments.iter().copied().zip(cuts) {
        points.sort_by(|a, b| along_segment(segment, *a, *b));
        points.dedup_by(|a, b| key(*a) == key(*b));
        for pair in points.windows(2) {
            let mut ids = [0; 2];
            for (index, &point) in pair.iter().enumerate() {
                ids[index] = *lookup.entry(key(point)).or_insert_with(|| {
                    let id = vertices.len();
                    vertices.push(point);
                    id
                });
            }
            if ids[0] != ids[1] {
                undirected.insert((ids[0].min(ids[1]), ids[0].max(ids[1])));
            }
        }
    }
    if undirected.len() > 32_768 {
        return None;
    }
    let mut edges = Vec::<(usize, usize)>::with_capacity(undirected.len() * 2);
    let mut outgoing = vec![Vec::<usize>::new(); vertices.len()];
    for (a, b) in undirected {
        let index = edges.len();
        edges.extend([(a, b), (b, a)]);
        outgoing[a].push(index);
        outgoing[b].push(index + 1);
    }
    for (vertex, list) in outgoing.iter_mut().enumerate() {
        list.sort_by(|&a, &b| {
            direction_order(vertices[vertex], vertices[edges[a].1], vertices[edges[b].1])
                .then(a.cmp(&b))
        });
    }
    let mut visited = vec![false; edges.len()];
    let mut faces = Vec::new();
    #[cfg(test)]
    let mut diagnostics = BTreeMap::<i32, (usize, f64)>::new();
    for start in 0..edges.len() {
        if visited[start] {
            continue;
        }
        let mut current = start;
        let mut points = Vec::new();
        loop {
            if visited[current] {
                if current != start {
                    return None;
                }
                break;
            }
            visited[current] = true;
            let (from, to) = edges[current];
            points.push(vertices[from]);
            let twin = current ^ 1;
            let options = &outgoing[to];
            let position = options.iter().position(|&edge| edge == twin)?;
            current = options[(position + options.len() - 1) % options.len()];
            if points.len() > edges.len() {
                return None;
            }
        }
        // A face walk may touch itself at a graph vertex. JTS's polygon
        // builder treats each resulting loop separately; sampling the entire
        // self-touching walk can keep a spur with the opposite depth.
        let mut pending = vec![points];
        while let Some(mut points) = pending.pop() {
            let mut seen = BTreeMap::new();
            let repeated = points.iter().enumerate().find_map(|(index, &point)| {
                seen.insert(key(point), index).map(|first| (first, index))
            });
            if let Some((first, second)) = repeated {
                let attached = points[first..second].to_vec();
                let rest = points[second..]
                    .iter()
                    .chain(&points[..first])
                    .copied()
                    .collect();
                pending.push(rest);
                pending.push(attached);
                continue;
            }
            if points.len() < 3 {
                continue;
            }
            let area = points
                .iter()
                .enumerate()
                .map(|(i, a)| {
                    let b = points[(i + 1) % points.len()];
                    a[0] * b[1] - b[0] * a[1]
                })
                .sum::<f64>();
            #[cfg(test)]
            if std::env::var_os("OMT_F64_ARRANGEMENT_OUTPUT").is_some() && area < -1000.0 {
                let min_x = points.iter().map(|p| p[0]).fold(f64::INFINITY, f64::min);
                let min_y = points.iter().map(|p| p[1]).fold(f64::INFINITY, f64::min);
                let max_x = points
                    .iter()
                    .map(|p| p[0])
                    .fold(f64::NEG_INFINITY, f64::max);
                let max_y = points
                    .iter()
                    .map(|p| p[1])
                    .fold(f64::NEG_INFINITY, f64::max);
                eprintln!(
                    "negative face area={} bounds={min_x},{min_y},{max_x},{max_y}",
                    area / 2.
                );
            }
            if area <= 0.0 {
                continue;
            }
            points.push(points[0]);
            let ring = LineString::from(points.iter().map(|p| (p[0], p[1])).collect::<Vec<_>>());
            let polygon = Polygon::new(ring, vec![]);
            let interior = polygon.interior_point()?;
            let depth = winding(curves, [interior.x(), interior.y()]);
            #[cfg(test)]
            if std::env::var_os("OMT_F64_ARRANGEMENT_OUTPUT").is_some()
                && depth == -1
                && interior.x() > 3719.0
                && interior.x() < 3813.0
                && interior.y() > 1240.0
                && interior.y() < 1342.0
            {
                use geo::Area;
                if polygon.unsigned_area() > 50.0 {
                    eprintln!(
                        "negative-depth region area={} sample={:?}",
                        polygon.unsigned_area(),
                        interior
                    );
                }
            }
            #[cfg(test)]
            if std::env::var_os("OMT_F64_ARRANGEMENT_OUTPUT").is_some() {
                use geo::{Area, BoundingRect};
                if polygon.unsigned_area() > 150.0 {
                    eprintln!(
                        "large face depth={depth} area={} bounds={:?} sample={:?}",
                        polygon.unsigned_area(),
                        polygon.bounding_rect(),
                        interior
                    );
                }
            }
            #[cfg(test)]
            if std::env::var_os("OMT_F64_ARRANGEMENT_OUTPUT").is_some() && depth == 0 {
                use geo::{Area, BoundingRect};
                if polygon.unsigned_area() > 20.0 {
                    eprintln!(
                        "zero-depth face area={} bounds={:?} sample={:?}",
                        polygon.unsigned_area(),
                        polygon.bounding_rect(),
                        interior
                    );
                }
            }
            #[cfg(test)]
            {
                use geo::Area;
                let entry = diagnostics.entry(depth).or_default();
                entry.0 += 1;
                entry.1 += polygon.unsigned_area();
            }
            if (positive_winding && depth > 0) || (!positive_winding && depth < 0) {
                faces.push(polygon);
            }
        }
    }
    #[cfg(test)]
    if std::env::var_os("OMT_F64_ARRANGEMENT_OUTPUT").is_some() {
        eprintln!("face depth diagnostics: {diagnostics:?}");
    }
    Some(MultiPolygon(faces))
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Area;

    fn decode_bits(value: &serde_json::Value) -> Vec<Vec<Point>> {
        let bits: Vec<Vec<[String; 2]>> = serde_json::from_value(value.clone()).unwrap();
        bits.into_iter()
            .map(|ring| {
                ring.into_iter()
                    .map(|point| {
                        point.map(|word| f64::from_bits(u64::from_str_radix(&word, 16).unwrap()))
                    })
                    .collect()
            })
            .collect()
    }

    fn fixture_curves() -> Vec<Vec<Point>> {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-landuse-18255700-buffer-cluster.json"
        ))
        .unwrap();
        decode_bits(&fixture["native_erosion_raw_curves_bits"])
    }

    #[test]
    fn point_touch_keeps_two_faces() {
        let curves = vec![
            vec![[0., 0.], [2., 0.], [2., 2.], [0., 2.]],
            vec![[2., 2.], [4., 2.], [4., 4.], [2., 4.]],
        ];
        let result = positive_faces(&curves).unwrap();
        assert_eq!(result.0.len(), 2);
        assert_eq!(result.unsigned_area(), 8.);
    }

    #[test]
    fn fixture_json_preserves_critical_curve_vertex_bits() {
        let curves = fixture_curves();
        assert_eq!(curves[12][1][0].to_bits(), 0x40ac_e800_0000_0001);
        assert_eq!(curves[12][1][1].to_bits(), 0x4093_c800_0000_0046);
    }

    #[test]
    fn pinned_cluster_noded_faces_exclude_opposite_depth_spur() {
        let stage: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-landuse-18255700-erosion-ieee-bits.json"
        ))
        .unwrap();
        let curves = decode_bits(&stage["negative_buffer_raw_curves"]);
        let segments = curves
            .iter()
            .flat_map(|curve| {
                (0..curve.len()).map(|i| Segment {
                    a: curve[i],
                    b: curve[(i + 1) % curve.len()],
                })
            })
            .collect::<Vec<_>>();
        let faces = graph_faces(&curves, &segments, node_segments(&segments), false).unwrap();
        assert_eq!(faces.0.len(), 12);
        assert!((faces.unsigned_area() - 4558.654439814203).abs() < 1e-7);
    }

    #[test]
    #[ignore]
    fn exact_java_raw_curve_noding_cuts() {
        let stage: serde_json::Value = serde_json::from_slice(
            &std::fs::read(std::env::var("OMT_BUFFER_JAVA_BITS").unwrap()).unwrap(),
        )
        .unwrap();
        let curves = stage["negative_buffer_raw_curves"]
            .as_array()
            .unwrap()
            .iter()
            .map(|curve| {
                decode_bits(&serde_json::json!([curve["coordinates"]]))
                    .into_iter()
                    .next()
                    .unwrap()
            })
            .collect::<Vec<_>>();
        let segments = curves
            .iter()
            .flat_map(|curve| {
                curve.windows(2).map(|points| Segment {
                    a: points[0],
                    b: points[1],
                })
            })
            .collect::<Vec<_>>();
        let cuts = node_segments(&segments);
        let faces = graph_faces(&curves, &segments, cuts.clone(), false).unwrap();
        eprintln!(
            "exact-bit JTS-curve faces={} area={}",
            faces.0.len(),
            faces.unsigned_area()
        );
        let encode = |point: Point| {
            [
                format!("{:016x}", point[0].to_bits()),
                format!("{:016x}", point[1].to_bits()),
            ]
        };
        let output = cuts
            .iter()
            .map(|points| points.iter().copied().map(encode).collect::<Vec<_>>())
            .collect::<Vec<_>>();
        std::fs::write(
            std::env::var("OMT_F64_NODE_CUTS_OUTPUT").unwrap(),
            serde_json::to_vec(&serde_json::json!({ "cuts": output, "faces": faces })).unwrap(),
        )
        .unwrap();
    }

    #[test]
    #[ignore]
    fn captured_jts_erosion_curves() {
        let curves = fixture_curves();
        let faces = positive_faces(&curves).unwrap();
        eprintln!("faces={} area={}", faces.0.len(), faces.unsigned_area());
        std::fs::write(
            std::env::var("OMT_F64_ARRANGEMENT_OUTPUT").unwrap(),
            serde_json::to_vec(&faces).unwrap(),
        )
        .unwrap();
    }

    #[test]
    #[ignore]
    fn pinned_jts_noded_edges_face_oracle() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-landuse-18255700-jts-noded-erosion-bits.json"
        ))
        .unwrap();
        let curves = decode_bits(&fixture["raw_curves_bits"]);
        let edges = decode_bits(&fixture["noded_edges_bits"]);
        let faces = positive_faces_from_noded_edges(&curves, &edges, false).unwrap();
        eprintln!(
            "JTS-noded faces={} area={}",
            faces.0.len(),
            faces.unsigned_area()
        );
        let groups = faces
            .0
            .iter()
            .map(|polygon| {
                let points = |ring: &LineString<f64>| {
                    ring.0
                        .iter()
                        .map(|point| crate::coordinate_groups::WorldPoint::new(point.x, point.y))
                        .collect::<Vec<_>>()
                };
                crate::coordinate_groups::WorldRingGroup {
                    exterior: points(polygon.exterior()),
                    holes: polygon.interiors().iter().map(points).collect(),
                }
            })
            .collect();
        let snapped = super::super::render::snap_float_polygon_groups(groups).unwrap();
        std::fs::write(
            std::env::var("OMT_F64_ARRANGEMENT_OUTPUT").unwrap(),
            serde_json::to_vec(&serde_json::json!({ "faces": faces, "snapped": snapped })).unwrap(),
        )
        .unwrap();
    }
}
