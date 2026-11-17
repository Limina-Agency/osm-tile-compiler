//! Experimental exact-coordinate provenance for the existing polygon buffer.
//!
//! Portions of the offset-join and shallow-concavity simplifier adapt JTS
//! 1.20.0 `OffsetSegmentGenerator` and `BufferInputLineSimplifier`
//! (Copyright 2016 Martin Davis and Vivid Solutions), SPDX:
//! EDL-1.0. See `src/omt/LICENSE.jts-edl-1.0.txt` and THIRD_PARTY_NOTICES.
//! This Rust adaptation changes curve storage and restores coordinates after
//! the existing i_overlay topology pass.
//!
//! `geo::Buffer` delegates topology to i_overlay's i32 adapter. This helper
//! independently constructs f64 mitred offset curves from the original input
//! and restores only unambiguous output vertices/intersections. It does not
//! replace the topology engine. Callers must compare its result with pinned
//! JTS fixtures before enabling it for production merging.

use geo::{Orient, orient::Direction};
use geo_types::{Coord, LineString, MultiPolygon, Polygon};
use i_overlay::{
    core::{fill_rule::FillRule, overlay_rule::OverlayRule, solver::Solver},
    float::overlay::{FloatOverlay, OverlayOptions},
    i_float::adapter::FloatPointAdapter,
    i_float::float::rect::FloatRect,
};
use std::collections::BTreeMap;

const MITRE_LIMIT: f64 = 5.0;
const MAX_SEGMENTS: usize = 8_192;
const JTS_SIMPLIFY_FACTOR: f64 = 0.01;

type Point = [f64; 2];
type Key = (i32, i32);

/// Mirrors JTS's ring-side simplification before a polygon buffer. For
/// geo's default orientation, a positive polygon buffer uses RIGHT for both
/// shell and holes, and erosion uses LEFT for both. JTS therefore passes a
/// signed tolerance of `-distance * 0.01` to the line simplifier.
#[allow(dead_code)]
pub(super) fn simplify_for_jts_buffer(
    input: &MultiPolygon<f64>,
    distance: f64,
) -> Option<MultiPolygon<f64>> {
    if !distance.is_finite() || distance == 0.0 {
        return None;
    }
    let oriented = input.orient(Direction::Default);
    let tolerance = -distance * JTS_SIMPLIFY_FACTOR;
    let mut polygons = Vec::with_capacity(oriented.0.len());
    for polygon in &oriented.0 {
        let exterior = simplify_ring(polygon.exterior(), tolerance)?;
        let holes = polygon
            .interiors()
            .iter()
            .map(|ring| simplify_ring(ring, tolerance))
            .collect::<Option<Vec<_>>>()?;
        polygons.push(Polygon::new(exterior, holes));
    }
    Some(MultiPolygon(polygons))
}

fn simplify_ring(ring: &LineString<f64>, signed_tolerance: f64) -> Option<LineString<f64>> {
    let points = ring.0.iter().map(|p| [p.x, p.y]).collect::<Vec<_>>();
    if points.len() < 4 || points.len() > MAX_SEGMENTS || points.first() != points.last() {
        return None;
    }
    let target_ccw = signed_tolerance > 0.0;
    let tolerance = signed_tolerance.abs();
    let mut deleted = vec![false; points.len()];
    let next = |start: usize, deleted: &[bool]| -> usize {
        let mut index = start + 1;
        while index < deleted.len() && deleted[index] {
            index += 1;
        }
        index
    };
    loop {
        let mut index = 0;
        let mut mid = next(index, &deleted);
        let mut last = next(mid, &deleted);
        let mut changed = false;
        while last < points.len() {
            let turn = cross(
                sub(points[mid], points[index]),
                sub(points[last], points[mid]),
            );
            let concave = if target_ccw { turn > 0.0 } else { turn < 0.0 };
            let shallow = distance_to_segment(points[mid], points[index], points[last]) < tolerance;
            let stride = ((last - index) / 10).max(1);
            let sampled = (index..last)
                .step_by(stride)
                .all(|i| distance_to_segment(points[i], points[index], points[last]) < tolerance);
            if concave && shallow && sampled {
                deleted[mid] = true;
                changed = true;
                index = last;
            } else {
                index = mid;
            }
            mid = next(index, &deleted);
            last = next(mid, &deleted);
        }
        if !changed {
            break;
        }
    }
    let result = points
        .into_iter()
        .enumerate()
        .filter_map(|(i, p)| (!deleted[i]).then_some(Coord { x: p[0], y: p[1] }))
        .collect::<Vec<_>>();
    (result.len() >= 4 && result.first() == result.last()).then(|| LineString::new(result))
}

#[derive(Default)]
struct Provenance {
    vertices: BTreeMap<Key, Option<Point>>,
    segments: Vec<(Point, Point)>,
}

/// Experimental in-process buffer path. The f64 JTS-style offset curves are
/// noded by i_overlay, then unique original curve vertices/intersections are
/// restored. The integer overlay still controls topology; this must remain
/// test-only until same-input Java fixtures qualify it.
#[allow(dead_code)]
pub(super) fn buffer_from_raw_curves(
    input: &MultiPolygon<f64>,
    distance: f64,
) -> Option<MultiPolygon<f64>> {
    let curves = raw_offset_curves(input, distance)?;
    #[cfg(test)]
    if distance < 0.0
        && let Ok(path) = std::env::var("OMT_BUFFER_NATIVE_RAW_CURVES_OUTPUT")
    {
        let bits = curves
            .iter()
            .map(|ring| {
                ring.iter()
                    .map(|point| {
                        [
                            format!("{:016x}", point[0].to_bits()),
                            format!("{:016x}", point[1].to_bits()),
                        ]
                    })
                    .collect::<Vec<_>>()
            })
            .collect::<Vec<_>>();
        std::fs::write(path, serde_json::to_vec(&bits).ok()?).ok()?;
    }
    let curve_adapter = FloatPointAdapter::<Point, f64>::with_iter(curves.iter().flatten());
    let mut provenance = Provenance::default();
    for curve in &curves {
        add_curve_provenance(curve, &curve_adapter, &mut provenance);
    }
    let mut options = OverlayOptions::ogc();
    options.preserve_input_collinear = true;
    options.preserve_output_collinear = true;
    let shapes = FloatOverlay::with_subj_custom(&curves, options, Solver::LIST).overlay(
        OverlayRule::Subject,
        if distance < 0.0 {
            FillRule::Positive
        } else {
            FillRule::NonZero
        },
    );
    let mut polygons = Vec::with_capacity(shapes.len());
    for shape in shapes {
        let mut contours = shape
            .into_iter()
            .map(|path| {
                let mut coords = path
                    .into_iter()
                    .map(|p| Coord { x: p[0], y: p[1] })
                    .collect::<Vec<_>>();
                if coords.first() != coords.last() {
                    coords.push(*coords.first()?);
                }
                Some(restore_ring(
                    &LineString::new(coords),
                    &curve_adapter,
                    &provenance,
                ))
            })
            .collect::<Option<Vec<_>>>()?;
        if contours.is_empty() {
            continue;
        }
        let exterior = contours.remove(0);
        polygons.push(Polygon::new(exterior, contours));
    }
    Some(MultiPolygon(polygons))
}

fn raw_offset_curves(input: &MultiPolygon<f64>, distance: f64) -> Option<Vec<Vec<Point>>> {
    if !distance.is_finite() || distance == 0.0 {
        return None;
    }
    let oriented = input.orient(Direction::Default);
    let source = oriented
        .0
        .iter()
        .flat_map(|polygon| std::iter::once(polygon.exterior()).chain(polygon.interiors()))
        .flat_map(|ring| ring.0.iter().map(|p| [p.x, p.y]))
        .collect::<Vec<_>>();
    if source.len() < 4
        || source.len() > MAX_SEGMENTS
        || source.iter().flatten().any(|v| !v.is_finite())
    {
        return None;
    }
    let mut rect = FloatRect::with_iter(source.iter())?;
    rect.add_offset(2.0 * 1.1 * MITRE_LIMIT * distance.abs());
    let source_adapter = FloatPointAdapter::<Point, f64>::new(rect);
    let mut discarded = Provenance::default();
    let mut curves = Vec::<Vec<Point>>::new();
    for polygon in &oriented.0 {
        curves.push(add_ring(
            polygon.exterior(),
            distance,
            &source_adapter,
            &mut discarded,
        )?);
        for hole in polygon.interiors() {
            curves.push(add_ring(hole, distance, &source_adapter, &mut discarded)?);
        }
    }
    Some(curves)
}

/// Restores f64 offset-curve vertices and uniquely determined intersections
/// on a polygon already buffered by geo/i_overlay. `None` means the input is
/// outside this bounded operation; the original buffered geometry is then the
/// appropriate fallback. `distance` is in the input coordinate units.
#[allow(dead_code)]
pub(super) fn restore_buffer_coordinates(
    input: &MultiPolygon<f64>,
    distance: f64,
    buffered: MultiPolygon<f64>,
) -> Option<MultiPolygon<f64>> {
    if !distance.is_finite() || distance == 0.0 {
        return None;
    }
    let oriented = input.orient(Direction::Default);
    let source = oriented
        .0
        .iter()
        .flat_map(|polygon| std::iter::once(polygon.exterior()).chain(polygon.interiors()))
        .flat_map(|ring| ring.0.iter().map(|p| [p.x, p.y]))
        .collect::<Vec<_>>();
    if source.len() < 4
        || source.len() > MAX_SEGMENTS
        || source.iter().flatten().any(|v| !v.is_finite())
    {
        return None;
    }
    // OutlineSolver::prepare in i_overlay 4.5.2 adds the outer and inner
    // miter builder offsets. For MITRE_LIMIT=5 each is 5.5*|distance|.
    let mut rect = FloatRect::with_iter(source.iter())?;
    rect.add_offset(2.0 * 1.1 * MITRE_LIMIT * distance.abs());
    let adapter = FloatPointAdapter::<Point, f64>::new(rect);
    let mut provenance = Provenance::default();
    for polygon in &oriented.0 {
        add_ring(polygon.exterior(), distance, &adapter, &mut provenance)?;
        for hole in polygon.interiors() {
            add_ring(hole, distance, &adapter, &mut provenance)?;
        }
    }
    if provenance.segments.len() > MAX_SEGMENTS * 2 {
        return None;
    }
    let mut polygons = Vec::with_capacity(buffered.0.len());
    for polygon in buffered.0 {
        let exterior = restore_ring(polygon.exterior(), &adapter, &provenance);
        let holes = polygon
            .interiors()
            .iter()
            .map(|ring| restore_ring(ring, &adapter, &provenance))
            .collect();
        polygons.push(Polygon::new(exterior, holes));
    }
    Some(MultiPolygon(polygons))
}

fn add_ring(
    ring: &LineString<f64>,
    distance: f64,
    adapter: &FloatPointAdapter<Point, f64>,
    provenance: &mut Provenance,
) -> Option<Vec<Point>> {
    let mut vertices = ring.0.iter().map(|p| [p.x, p.y]).collect::<Vec<_>>();
    if vertices.first() == vertices.last() {
        vertices.pop();
    }
    if vertices.len() < 3 || vertices.len() > MAX_SEGMENTS {
        return None;
    }
    let area = vertices
        .iter()
        .enumerate()
        .map(|(i, &a)| cross(a, vertices[(i + 1) % vertices.len()]))
        .sum::<f64>();
    if area == 0.0 || !area.is_finite() {
        return None;
    }
    let mut curve = Vec::with_capacity(vertices.len() * 2);
    for i in 0..vertices.len() {
        let previous = vertices[(i + vertices.len() - 1) % vertices.len()];
        let current = vertices[i];
        let next = vertices[(i + 1) % vertices.len()];
        let incoming = unit(sub(current, previous))?;
        let outgoing = unit(sub(next, current))?;
        // A right-hand offset expands an oriented CCW shell and contracts an
        // oriented CW hole for positive distance.
        let (offset_previous, a) = offset_segment(previous, current, distance)?;
        let (b, offset_next) = offset_segment(current, next, distance)?;
        // JTS 1.20 OffsetSegmentGenerator distinguishes turns by offset side.
        let turn = cross(incoming, outgoing) * distance.signum();
        let radius = distance.abs();
        if turn == 0.0 {
            if dot(incoming, outgoing) < 0.0 {
                curve.extend([a, b]);
            } else {
                curve.push(a);
            }
        } else if turn > 0.0 {
            if length(sub(a, b)) < radius * 0.05 {
                curve.push(
                    if length(sub(current, previous)) > length(sub(next, current)) {
                        a
                    } else {
                        b
                    },
                );
                continue;
            }
            let intersection =
                super::jts_intersection::intersection(offset_previous, a, b, offset_next);
            if let Some(point) =
                intersection.filter(|p| length(sub(*p, current)) <= MITRE_LIMIT * radius)
            {
                curve.push(point);
            } else if distance_to_segment(current, a, b) >= MITRE_LIMIT * radius {
                curve.extend([a, b]);
            } else {
                // OffsetSegmentGenerator.addLimitedMitreJoin constructs the
                // clipping line from the angle bisector, then intersects its
                // segment with each offset line. In particular, constructing
                // it from the mitre intersection loses a bit at grid ties.
                let angle0 = (previous[1] - current[1]).atan2(previous[0] - current[0]);
                let angle1 = (next[1] - current[1]).atan2(next[0] - current[0]);
                let mut oriented = angle1 - angle0;
                if oriented <= -std::f64::consts::PI {
                    oriented += 2.0 * std::f64::consts::PI;
                } else if oriented > std::f64::consts::PI {
                    oriented -= 2.0 * std::f64::consts::PI;
                }
                let bisector = normalize_angle(angle0 + oriented / 2.0);
                let midpoint = project(current, -MITRE_LIMIT * radius, bisector);
                let perpendicular = normalize_angle(bisector + std::f64::consts::FRAC_PI_2);
                let clip_a = project(midpoint, radius, perpendicular);
                let clip_b = project(midpoint, radius, perpendicular + std::f64::consts::PI);
                match (
                    jts_line_segment(offset_previous, a, clip_a, clip_b),
                    jts_line_segment(b, offset_next, clip_a, clip_b),
                ) {
                    (Some(first), Some(second)) => curve.extend([first, second]),
                    _ => curve.extend([a, b]),
                }
            }
        } else {
            let shifted_previous = sub(a, mul(incoming, length(sub(current, previous))));
            let shifted_next = add(b, mul(outgoing, length(sub(next, current))));
            if let Some(point) = segment_cross(shifted_previous, a, b, shifted_next) {
                curve.push(point);
            } else if length(sub(a, b)) < radius * 0.001 {
                curve.push(a);
            } else {
                curve.extend([a, mul(add(a, current), 0.5), mul(add(b, current), 0.5), b]);
            }
        }
    }
    if curve.len() < 3 {
        return None;
    }
    add_curve_provenance(&curve, adapter, provenance);
    Some(curve)
}

fn add_curve_provenance(
    curve: &[Point],
    adapter: &FloatPointAdapter<Point, f64>,
    provenance: &mut Provenance,
) {
    for (i, &point) in curve.iter().enumerate() {
        let key = key(adapter, point);
        provenance
            .vertices
            .entry(key)
            .and_modify(|candidate| {
                if candidate.is_some_and(|old| old != point) {
                    *candidate = None;
                }
            })
            .or_insert(Some(point));
        let next = curve[(i + 1) % curve.len()];
        if point != next {
            provenance.segments.push((point, next));
        }
    }
}

fn restore_ring(
    ring: &LineString<f64>,
    adapter: &FloatPointAdapter<Point, f64>,
    provenance: &Provenance,
) -> LineString<f64> {
    LineString::new(
        ring.0
            .iter()
            .map(|p| {
                let output = [p.x, p.y];
                let key = key(adapter, output);
                let restored = provenance
                    .vertices
                    .get(&key)
                    .and_then(|value| *value)
                    .or_else(|| unique_intersection(key, adapter, &provenance.segments))
                    .unwrap_or(output);
                Coord {
                    x: restored[0],
                    y: restored[1],
                }
            })
            .collect(),
    )
}

fn unique_intersection(
    output_key: Key,
    adapter: &FloatPointAdapter<Point, f64>,
    segments: &[(Point, Point)],
) -> Option<Point> {
    // Retain only segments whose quantized bounds can contain the output. This
    // grid-derived bound is not a coordinate-shifting tolerance.
    let nearby = segments
        .iter()
        .copied()
        .filter(|&(a, b)| {
            let ka = key(adapter, a);
            let kb = key(adapter, b);
            output_key.0 >= ka.0.min(kb.0).saturating_sub(1)
                && output_key.0 <= ka.0.max(kb.0).saturating_add(1)
                && output_key.1 >= ka.1.min(kb.1).saturating_sub(1)
                && output_key.1 <= ka.1.max(kb.1).saturating_add(1)
        })
        .collect::<Vec<_>>();
    if nearby.len() > 16 {
        return None;
    }
    let mut candidate = None;
    for (i, &(a, b)) in nearby.iter().enumerate() {
        for &(c, d) in nearby.iter().skip(i + 1) {
            let r = sub(b, a);
            let s = sub(d, c);
            let denominator = cross(r, s);
            if denominator == 0.0 {
                continue;
            }
            let t = cross(sub(c, a), s) / denominator;
            let u = cross(sub(c, a), r) / denominator;
            if !(0.0..=1.0).contains(&t) || !(0.0..=1.0).contains(&u) {
                continue;
            }
            let point = add(a, mul(r, t));
            if key(adapter, point) != output_key {
                continue;
            }
            match candidate {
                None => candidate = Some(point),
                Some(old) if old == point => {}
                Some(_) => return None,
            }
        }
    }
    candidate
}

fn key(adapter: &FloatPointAdapter<Point, f64>, point: Point) -> Key {
    let grid = adapter.float_to_int(&point);
    (grid.x, grid.y)
}
fn add(a: Point, b: Point) -> Point {
    [a[0] + b[0], a[1] + b[1]]
}
fn sub(a: Point, b: Point) -> Point {
    [a[0] - b[0], a[1] - b[1]]
}
fn mul(a: Point, scale: f64) -> Point {
    [a[0] * scale, a[1] * scale]
}
fn cross(a: Point, b: Point) -> f64 {
    a[0] * b[1] - a[1] * b[0]
}
fn dot(a: Point, b: Point) -> f64 {
    a[0] * b[0] + a[1] * b[1]
}
fn length(a: Point) -> f64 {
    a[0].hypot(a[1])
}
fn unit(a: Point) -> Option<Point> {
    let norm = length(a);
    (norm > 0.0 && norm.is_finite()).then(|| mul(a, 1.0 / norm))
}
fn offset_segment(start: Point, end: Point, distance: f64) -> Option<(Point, Point)> {
    // JTS OffsetSegmentGenerator.computeOffsetSegment evaluates these operations
    // in this order: side * distance * delta / length, then translates both
    // endpoints. Unit-vector multiplication changes low bits at grid ties.
    let dx = end[0] - start[0];
    let dy = end[1] - start[1];
    let length = dx.hypot(dy);
    if length == 0.0 || !length.is_finite() {
        return None;
    }
    let scale = -distance;
    let ux = scale * dx / length;
    let uy = scale * dy / length;
    Some(([start[0] - uy, start[1] + ux], [end[0] - uy, end[1] + ux]))
}
fn distance_to_segment(point: Point, start: Point, end: Point) -> f64 {
    let edge = sub(end, start);
    let length_sq = dot(edge, edge);
    if length_sq == 0.0 {
        return length(sub(point, start));
    }
    let t = (dot(sub(point, start), edge) / length_sq).clamp(0.0, 1.0);
    length(sub(point, add(start, mul(edge, t))))
}
fn segment_cross(a: Point, b: Point, c: Point, d: Point) -> Option<Point> {
    let r = sub(b, a);
    let s = sub(d, c);
    let denominator = cross(r, s);
    if denominator == 0.0 {
        return None;
    }
    let t = cross(sub(c, a), s) / denominator;
    let u = cross(sub(c, a), r) / denominator;
    ((0.0..=1.0).contains(&t) && (0.0..=1.0).contains(&u)).then(|| add(a, mul(r, t)))
}
fn normalize_angle(mut angle: f64) -> f64 {
    while angle > std::f64::consts::PI {
        angle -= 2.0 * std::f64::consts::PI;
    }
    while angle <= -std::f64::consts::PI {
        angle += 2.0 * std::f64::consts::PI;
    }
    angle
}

fn project(point: Point, distance: f64, angle: f64) -> Point {
    let cosine = angle.cos();
    let sine = angle.sin();
    let cosine = if cosine.abs() < 5e-16 { 0.0 } else { cosine };
    let sine = if sine.abs() < 5e-16 { 0.0 } else { sine };
    [point[0] + distance * cosine, point[1] + distance * sine]
}

fn jts_line_segment(line_start: Point, line_end: Point, start: Point, end: Point) -> Option<Point> {
    use geo::{
        Coord,
        kernels::{Kernel, RobustKernel},
    };
    let orient = |point: Point| {
        RobustKernel::orient2d(
            Coord {
                x: line_start[0],
                y: line_start[1],
            },
            Coord {
                x: line_end[0],
                y: line_end[1],
            },
            Coord {
                x: point[0],
                y: point[1],
            },
        )
    };
    let first = orient(start);
    if first == geo::Orientation::Collinear {
        return Some(start);
    }
    let second = orient(end);
    if second == geo::Orientation::Collinear {
        return Some(end);
    }
    if first == second {
        return None;
    }
    super::jts_intersection::intersection(line_start, line_end, start, end)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tile_geometry::TileGeometry;
    use geo::{
        Buffer, MapCoordsInPlace,
        algorithm::buffer::{BufferStyle, LineJoin},
        unary_union,
    };

    fn square() -> MultiPolygon<f64> {
        MultiPolygon(vec![Polygon::new(
            LineString::from(vec![
                (0., 0.),
                (100., 0.),
                (100., 100.),
                (0., 100.),
                (0., 0.),
            ]),
            vec![],
        )])
    }

    #[test]
    fn square_dilation_recovers_exact_mitred_corners() {
        let original = square();
        let buffered = original.buffer_with_style(
            BufferStyle::new(0.5).line_join(LineJoin::Miter(2. * (1_f64 / 5.).asin())),
        );
        let restored = restore_buffer_coordinates(&original, 0.5, buffered).unwrap();
        let ring = &restored.0[0].exterior().0;
        for (x, y) in [(-0.5, -0.5), (100.5, -0.5), (100.5, 100.5), (-0.5, 100.5)] {
            assert!(
                ring.iter().any(|p| p.x == x && p.y == y),
                "missing ({x},{y}): {ring:?}"
            );
        }
    }

    #[test]
    fn direct_raw_curve_square_and_hole_topology() {
        use geo::Area;
        let shell = square();
        let direct = buffer_from_raw_curves(&shell, 0.5).unwrap();
        assert_eq!(direct.0.len(), 1, "{direct:?}");
        assert!((direct.unsigned_area() - 10201.).abs() < 1e-6, "{direct:?}");
    }

    #[test]
    fn square_erosion_recovers_exact_mitred_corners() {
        let original = square();
        let buffered = original.buffer_with_style(
            BufferStyle::new(-0.5).line_join(LineJoin::Miter(2. * (1_f64 / 5.).asin())),
        );
        let restored = restore_buffer_coordinates(&original, -0.5, buffered).unwrap();
        let ring = &restored.0[0].exterior().0;
        for (x, y) in [(0.5, 0.5), (99.5, 0.5), (99.5, 99.5), (0.5, 99.5)] {
            assert!(
                ring.iter().any(|p| p.x == x && p.y == y),
                "missing ({x},{y}): {ring:?}"
            );
        }
    }

    #[test]
    fn positive_buffer_contracts_hole_while_expanding_shell() {
        let original = MultiPolygon(vec![Polygon::new(
            square().0[0].exterior().clone(),
            vec![LineString::from(vec![
                (25., 25.),
                (25., 75.),
                (75., 75.),
                (75., 25.),
                (25., 25.),
            ])],
        )]);
        let buffered = original.buffer_with_style(
            BufferStyle::new(0.5).line_join(LineJoin::Miter(2. * (1_f64 / 5.).asin())),
        );
        let restored = restore_buffer_coordinates(&original, 0.5, buffered).unwrap();
        assert_eq!(restored.0.len(), 1);
        let hole = &restored.0[0].interiors()[0].0;
        for (x, y) in [(25.5, 25.5), (74.5, 25.5), (74.5, 74.5), (25.5, 74.5)] {
            assert!(
                hole.iter().any(|p| p.x == x && p.y == y),
                "missing ({x},{y}): {hole:?}"
            );
        }
    }

    #[test]
    fn clipped_mitre_provenance_is_bounded() {
        let original = MultiPolygon(vec![Polygon::new(
            LineString::from(vec![(0., 0.), (100., 0.), (0.1, 0.01), (0., 0.)]),
            vec![],
        )]);
        let buffered = original.buffer_with_style(
            BufferStyle::new(0.5).line_join(LineJoin::Miter(2. * (1_f64 / 5.).asin())),
        );
        let restored = restore_buffer_coordinates(&original, 0.5, buffered);
        assert!(restored.is_some());
    }

    /// One-off same-input qualification against the pinned Java cluster
    /// oracle. No production code depends on external benchmark fixtures.
    #[test]
    #[ignore]
    fn captured_cluster_stage_oracle() {
        let records: Vec<super::super::render::Record> = serde_json::from_slice(
            &std::fs::read(std::env::var("OMT_BUFFER_FIXTURE").unwrap()).unwrap(),
        )
        .unwrap();
        let clusters: serde_json::Value = serde_json::from_slice(
            &std::fs::read(std::env::var("OMT_BUFFER_CLUSTERS").unwrap()).unwrap(),
        )
        .unwrap();
        let index: usize = std::env::var("OMT_BUFFER_CLUSTER")
            .unwrap()
            .parse()
            .unwrap();
        let distance: f64 = std::env::var("OMT_BUFFER_DISTANCE")
            .unwrap()
            .parse()
            .unwrap();
        let members = clusters[index]["members"].as_array().unwrap();
        let mut positive_fallbacks = Vec::new();
        let mut buffered = Vec::new();
        let style =
            |amount| BufferStyle::new(amount).line_join(LineJoin::Miter(2. * (1_f64 / 5.).asin()));
        for member in members {
            let record = &records[member.as_u64().unwrap() as usize];
            let TileGeometry::Polygon(groups) = &record.geometry else {
                panic!("polygon expected")
            };
            let polygon = MultiPolygon(
                groups
                    .iter()
                    .map(|group| {
                        let line = |points: &[crate::tile_geometry::TilePoint]| {
                            LineString::from(
                                points
                                    .iter()
                                    .map(|p| (f64::from(p.x), f64::from(p.y)))
                                    .collect::<Vec<_>>(),
                            )
                        };
                        Polygon::new(
                            line(&group.exterior),
                            group.holes.iter().map(|h| line(h)).collect(),
                        )
                    })
                    .collect(),
            );
            let simplified = simplify_for_jts_buffer(&polygon, distance).unwrap_or(polygon);
            let approximated = simplified.buffer_with_style(style(distance));
            let restored = buffer_from_raw_curves(&simplified, distance).unwrap_or_else(|| {
                positive_fallbacks.push(member.as_u64().unwrap() as usize);
                approximated
            });
            buffered.push(restored);
        }
        let unioned = unary_union(&buffered);
        let simplified_union =
            simplify_for_jts_buffer(&unioned, -distance).unwrap_or(unioned.clone());
        let approximated = simplified_union.buffer_with_style(style(-distance));
        let contracted = buffer_from_raw_curves(&simplified_union, -distance);
        let erosion_fallback = contracted.is_none();
        let contracted = contracted.unwrap_or(approximated);
        let serialize = |shape: &MultiPolygon<f64>| {
            shape
                .0
                .iter()
                .map(|polygon| {
                    std::iter::once(polygon.exterior())
                        .chain(polygon.interiors())
                        .map(|ring| ring.0.iter().map(|p| [p.x, p.y]).collect::<Vec<_>>())
                        .collect::<Vec<_>>()
                })
                .collect::<Vec<_>>()
        };
        let polygons = serialize(&contracted);
        let float_groups = contracted
            .0
            .iter()
            .map(|p| {
                let world_ring = |ring: &LineString<f64>| {
                    ring.0
                        .iter()
                        .map(|c| crate::coordinate_groups::WorldPoint::new(c.x, c.y))
                        .collect::<Vec<_>>()
                };
                crate::coordinate_groups::WorldRingGroup {
                    exterior: world_ring(p.exterior()),
                    holes: p.interiors().iter().map(world_ring).collect(),
                }
            })
            .collect::<Vec<_>>();
        let snapped = match super::super::render::snap_float_polygon_groups(float_groups).unwrap() {
            TileGeometry::Polygon(groups) => groups,
            _ => panic!("polygon expected after snap"),
        };
        let filtered = super::super::store::filter_tile_polygon_areas(snapped.clone(), 256., 256.);
        std::fs::write(
            std::env::var("OMT_BUFFER_OUTPUT").unwrap(),
            serde_json::to_vec(&serde_json::json!({
                "buffered": buffered.iter().map(&serialize).collect::<Vec<_>>(),
                "unioned": serialize(&unioned),
                "contracted": &polygons,
                "snapped": &snapped,
                "filtered": &filtered,
                "positive_fallbacks": &positive_fallbacks,
                "erosion_fallback": erosion_fallback
            }))
            .unwrap(),
        )
        .unwrap();
        eprintln!(
            "cluster={index} members={} positive_fallbacks={} erosion_fallback={erosion_fallback} polygons={}",
            members.len(),
            positive_fallbacks.len(),
            polygons.len()
        );
    }

    /// Feed Planetiler's contracted floating-point geometry directly to the
    /// native snap backend to isolate buffering from precision reduction.
    #[test]
    #[ignore]
    fn java_contracted_snap_oracle() {
        let float_groups: Vec<crate::coordinate_groups::WorldRingGroup> = serde_json::from_slice(
            &std::fs::read(std::env::var("OMT_BUFFER_JAVA_CONTRACTED").unwrap()).unwrap(),
        )
        .unwrap();
        let snapped = super::super::render::snap_float_polygon_groups(float_groups).unwrap();
        std::fs::write(
            std::env::var("OMT_BUFFER_JAVA_SNAPPED_OUTPUT").unwrap(),
            serde_json::to_vec(&snapped).unwrap(),
        )
        .unwrap();
    }

    /// Isolates the erosion offset stage with Planetiler's exact union input.
    #[test]
    #[ignore]
    fn java_unioned_contract_oracle() {
        let groups: Vec<crate::coordinate_groups::WorldRingGroup> = serde_json::from_slice(
            &std::fs::read(std::env::var("OMT_BUFFER_JAVA_UNIONED").unwrap()).unwrap(),
        )
        .unwrap();
        let mut input = MultiPolygon(
            groups
                .iter()
                .map(|group| {
                    let line = |points: &[crate::coordinate_groups::WorldPoint]| {
                        LineString::from(points.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>())
                    };
                    Polygon::new(
                        line(&group.exterior),
                        group.holes.iter().map(|hole| line(hole)).collect(),
                    )
                })
                .collect(),
        );
        let pixel_domain = std::env::var_os("OMT_BUFFER_PIXEL_DOMAIN").is_some();
        if pixel_domain {
            input.map_coords_in_place(|coordinate| Coord {
                x: coordinate.x / 16.0,
                y: coordinate.y / 16.0,
            });
        }
        let distance = if pixel_domain { -0.25 } else { -4.0 };
        let simplified = simplify_for_jts_buffer(&input, distance).unwrap();
        let contracted = buffer_from_raw_curves(&simplified, distance).unwrap();
        let output = contracted
            .0
            .iter()
            .map(|polygon| {
                let points = |line: &LineString<f64>| {
                    line.0
                        .iter()
                        .map(|p| crate::coordinate_groups::WorldPoint::new(p.x, p.y))
                        .collect::<Vec<_>>()
                };
                crate::coordinate_groups::WorldRingGroup {
                    exterior: points(polygon.exterior()),
                    holes: polygon.interiors().iter().map(points).collect(),
                }
            })
            .collect::<Vec<_>>();
        std::fs::write(
            std::env::var("OMT_BUFFER_JAVA_UNIONED_OUTPUT").unwrap(),
            serde_json::to_vec(&output).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn pinned_cluster_erosion_curves_match_ieee_bits() {
        let stage: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-landuse-18255700-erosion-ieee-bits.json"
        ))
        .unwrap();
        let decode = |ring: &serde_json::Value| {
            ring.as_array()
                .unwrap()
                .iter()
                .map(|point| {
                    let point = point.as_array().unwrap();
                    (
                        f64::from_bits(
                            u64::from_str_radix(point[0].as_str().unwrap(), 16).unwrap(),
                        ),
                        f64::from_bits(
                            u64::from_str_radix(point[1].as_str().unwrap(), 16).unwrap(),
                        ),
                    )
                })
                .collect::<Vec<_>>()
        };
        let mut input = MultiPolygon(
            stage["unioned"]["polygons"]
                .as_array()
                .unwrap()
                .iter()
                .map(|polygon| {
                    Polygon::new(
                        LineString::from(decode(&polygon["exterior"])),
                        polygon["holes"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|hole| LineString::from(decode(hole)))
                            .collect(),
                    )
                })
                .collect(),
        );
        input.map_coords_in_place(|coordinate| Coord {
            x: coordinate.x / 16.0,
            y: coordinate.y / 16.0,
        });
        let simplified = simplify_for_jts_buffer(&input, -0.25).unwrap();
        let curves = raw_offset_curves(&simplified, -0.25).unwrap();
        let expected = stage["negative_buffer_raw_curves"].as_array().unwrap();
        assert_eq!(curves.len(), 13);
        assert_eq!(curves.len(), expected.len());
        for (curve, expected) in curves.iter().zip(expected) {
            let actual = curve
                .iter()
                .map(|point| ((point[0] * 16.0).to_bits(), (point[1] * 16.0).to_bits()))
                .collect::<Vec<_>>();
            let expected = decode(expected)
                .into_iter()
                .map(|(x, y)| (x.to_bits(), y.to_bits()))
                .collect::<Vec<_>>();
            assert_eq!(actual.len(), expected.len());
            assert!(
                (0..actual.len()).any(|shift| {
                    (0..actual.len()).all(|i| actual[i] == expected[(i + shift) % actual.len()])
                }) || (0..actual.len()).any(|shift| {
                    (0..actual.len())
                        .all(|i| actual[i] == expected[(shift + actual.len() - i) % actual.len()])
                }),
                "JTS offset curve differs at IEEE-754 precision"
            );
        }
    }

    #[test]
    #[ignore]
    fn bit_exact_java_union_curve_oracle() {
        let stage: serde_json::Value = serde_json::from_slice(
            &std::fs::read(std::env::var("OMT_BUFFER_JAVA_BITS").unwrap()).unwrap(),
        )
        .unwrap();
        let decode = |ring: &serde_json::Value| {
            ring.as_array()
                .unwrap()
                .iter()
                .map(|point| {
                    let point = point.as_array().unwrap();
                    (
                        f64::from_bits(
                            u64::from_str_radix(point[0].as_str().unwrap(), 16).unwrap(),
                        ),
                        f64::from_bits(
                            u64::from_str_radix(point[1].as_str().unwrap(), 16).unwrap(),
                        ),
                    )
                })
                .collect::<Vec<_>>()
        };
        let mut input = MultiPolygon(
            stage["unioned"]["polygons"]
                .as_array()
                .unwrap()
                .iter()
                .map(|polygon| {
                    Polygon::new(
                        LineString::from(decode(&polygon["exterior"])),
                        polygon["holes"]
                            .as_array()
                            .unwrap()
                            .iter()
                            .map(|hole| LineString::from(decode(hole)))
                            .collect(),
                    )
                })
                .collect(),
        );
        let pixel_domain = std::env::var_os("OMT_BUFFER_PIXEL_DOMAIN").is_some();
        if pixel_domain {
            input.map_coords_in_place(|coordinate| Coord {
                x: coordinate.x / 16.0,
                y: coordinate.y / 16.0,
            });
        }
        let distance = if pixel_domain { -0.25 } else { -4.0 };
        let simplified = simplify_for_jts_buffer(&input, distance).unwrap();
        let contracted = buffer_from_raw_curves(&simplified, distance).unwrap();
        assert!(!contracted.0.is_empty());
    }

    #[test]
    fn exact_java_contracted_float_snaps_to_same_integer_rings() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-landuse-18255700-buffer-cluster.json"
        ))
        .unwrap();
        let float_groups: Vec<crate::coordinate_groups::WorldRingGroup> =
            serde_json::from_value(fixture["java_contracted_mvt"].clone()).unwrap();
        let expected: Vec<crate::coordinate_groups::WorldRingGroup> =
            serde_json::from_value(fixture["java_snapped_mvt"].clone()).unwrap();
        let TileGeometry::Polygon(actual) =
            super::super::render::snap_float_polygon_groups(float_groups).unwrap()
        else {
            panic!("snapped Java polygon must remain a polygon")
        };
        let canonical_ring = |mut ring: Vec<(i32, i32)>| {
            assert_eq!(ring.first(), ring.last());
            ring.pop();
            let mut choices = Vec::with_capacity(ring.len() * 2);
            for direction in [ring.clone(), ring.into_iter().rev().collect()] {
                for start in 0..direction.len() {
                    choices.push(
                        direction[start..]
                            .iter()
                            .chain(&direction[..start])
                            .copied()
                            .collect::<Vec<_>>(),
                    );
                }
            }
            choices.into_iter().min().unwrap()
        };
        let mut actual = actual
            .iter()
            .map(|group| {
                let exterior = canonical_ring(group.exterior.iter().map(|p| (p.x, p.y)).collect());
                let mut holes = group
                    .holes
                    .iter()
                    .map(|h| canonical_ring(h.iter().map(|p| (p.x, p.y)).collect()))
                    .collect::<Vec<_>>();
                holes.sort();
                (exterior, holes)
            })
            .collect::<Vec<_>>();
        let mut expected = expected
            .iter()
            .map(|group| {
                let convert = |p: &crate::coordinate_groups::WorldPoint| {
                    assert_eq!(p.x.fract(), 0.0);
                    assert_eq!(p.y.fract(), 0.0);
                    (p.x as i32, p.y as i32)
                };
                let exterior = canonical_ring(group.exterior.iter().map(convert).collect());
                let mut holes = group
                    .holes
                    .iter()
                    .map(|h| canonical_ring(h.iter().map(convert).collect()))
                    .collect::<Vec<_>>();
                holes.sort();
                (exterior, holes)
            })
            .collect::<Vec<_>>();
        actual.sort();
        expected.sort();
        assert_eq!(actual, expected);
    }
}
