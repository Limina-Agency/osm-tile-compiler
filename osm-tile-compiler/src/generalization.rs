use crate::coordinate_groups::{WorldGeometry, WorldPoint, WorldRingGroup};

/// Resolved geometry policy, independent of source-layer classification.
/// Lengths are in 4096-extent tile units; polygon area is in squared tile
/// units. Hole area is already in world tile-coordinate units to preserve
/// the Tactical threshold's floating-point evaluation order.
#[derive(Debug, Clone, Copy)]
pub(crate) struct GeometryPolicy {
    pub simplification_tolerance: f64,
    pub min_line_length: f64,
    pub min_polygon_area: f64,
    pub min_hole_area_world: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct GeneralizationStats {
    pub features_dropped_by_generalization: usize,
    pub line_features_dropped_small: usize,
    pub polygon_features_dropped_small: usize,
}

#[derive(Debug, Clone, PartialEq)]
pub struct GeneralizationOutcomeWorld {
    pub geometry: Option<WorldGeometry>,
    pub stats: GeneralizationStats,
}

pub(crate) fn generalize_world_geometry_with_policy(
    rules: GeometryPolicy,
    geometry: WorldGeometry,
) -> GeneralizationOutcomeWorld {
    let mut stats = GeneralizationStats::default();
    let tolerance = rules.simplification_tolerance / 4096.0;
    let min_line_length = rules.min_line_length / 4096.0;

    let geometry = match geometry {
        WorldGeometry::Point(points) => {
            if points.is_empty() {
                stats.features_dropped_by_generalization += 1;
                None
            } else {
                Some(WorldGeometry::Point(points))
            }
        }
        WorldGeometry::Line(lines) => {
            let input_count = lines.len();
            let lines = lines
                .into_iter()
                .filter_map(|line| generalize_world_line(line, tolerance, min_line_length))
                .collect::<Vec<_>>();
            stats.line_features_dropped_small += input_count.saturating_sub(lines.len());
            if lines.is_empty() {
                stats.features_dropped_by_generalization += 1;
                None
            } else {
                Some(WorldGeometry::Line(lines))
            }
        }
        WorldGeometry::Polygon(groups) => {
            let min_area = rules.min_polygon_area / (4096.0 * 4096.0);
            let total_area = groups.iter().map(world_group_area).sum::<f64>();
            if groups.is_empty() || total_area < min_area {
                stats.features_dropped_by_generalization += 1;
                stats.polygon_features_dropped_small += 1;
                None
            } else {
                Some(WorldGeometry::Polygon(
                    groups
                        .into_iter()
                        .filter_map(|group| {
                            generalize_world_polygon(group, tolerance, rules.min_hole_area_world)
                        })
                        .collect(),
                ))
            }
        }
    };

    GeneralizationOutcomeWorld { geometry, stats }
}

fn generalize_world_polygon(
    group: WorldRingGroup,
    tolerance: f64,
    min_hole_area: f64,
) -> Option<WorldRingGroup> {
    let exterior = simplify_world_ring(group.exterior, tolerance)?;
    let holes = group
        .holes
        .into_iter()
        .filter(|ring| world_ring_area(ring) >= min_hole_area)
        .filter_map(|ring| simplify_world_ring(ring, tolerance))
        .collect();
    Some(WorldRingGroup { exterior, holes })
}

fn simplify_world_ring(ring: Vec<WorldPoint>, tolerance: f64) -> Option<Vec<WorldPoint>> {
    let mut ring = dedupe_adjacent_world(ring);
    if ring
        .first()
        .zip(ring.last())
        .is_some_and(|(first, last)| !approx_world_point(*first, *last))
        && let Some(first) = ring.first().copied()
    {
        ring.push(first);
    }
    if ring.len() < 4 || tolerance <= 0.0 {
        return (ring.len() >= 4).then_some(ring);
    }

    let original = ring.clone();
    ring.pop();
    let anchor = ring[0];
    let split = ring
        .iter()
        .enumerate()
        .skip(1)
        .max_by(|(_, left), (_, right)| {
            world_distance_sq(anchor, **left).total_cmp(&world_distance_sq(anchor, **right))
        })
        .map(|(index, _)| index)?;

    let first_arc = simplify_world_polyline(&ring[..=split], tolerance);
    let mut second_arc = ring[split..].to_vec();
    second_arc.push(anchor);
    let second_arc = simplify_world_polyline(&second_arc, tolerance);
    let mut simplified = first_arc;
    simplified.extend(second_arc.into_iter().skip(1));
    if simplified.len() < 4 || world_ring_area(&simplified) <= 0.0 {
        Some(original)
    } else {
        Some(simplified)
    }
}

fn world_group_area(group: &WorldRingGroup) -> f64 {
    (world_ring_area(&group.exterior)
        - group
            .holes
            .iter()
            .map(|ring| world_ring_area(ring))
            .sum::<f64>())
    .max(0.0)
}

fn world_ring_area(ring: &[WorldPoint]) -> f64 {
    ring.windows(2)
        .map(|points| points[0].x * points[1].y - points[1].x * points[0].y)
        .sum::<f64>()
        .abs()
        * 0.5
}

fn world_distance_sq(left: WorldPoint, right: WorldPoint) -> f64 {
    let dx = left.x - right.x;
    let dy = left.y - right.y;
    dx * dx + dy * dy
}

fn generalize_world_line(
    line: Vec<WorldPoint>,
    tolerance: f64,
    min_length: f64,
) -> Option<Vec<WorldPoint>> {
    let line = dedupe_adjacent_world(line);
    if line.len() < 2 {
        return None;
    }
    let simplified = if tolerance > 0.0 {
        simplify_world_polyline(&line, tolerance)
    } else {
        line
    };
    if simplified.len() < 2 || world_polyline_length(&simplified) < min_length {
        None
    } else {
        Some(simplified)
    }
}

fn simplify_world_polyline(points: &[WorldPoint], tolerance: f64) -> Vec<WorldPoint> {
    if points.len() <= 2 {
        return points.to_vec();
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    simplify_world_range(
        points,
        0,
        points.len() - 1,
        tolerance * tolerance,
        &mut keep,
    );
    points
        .iter()
        .zip(keep)
        .filter_map(|(point, keep)| keep.then_some(*point))
        .collect()
}

fn simplify_world_range(
    points: &[WorldPoint],
    start: usize,
    end: usize,
    tolerance_sq: f64,
    keep: &mut [bool],
) {
    if end <= start + 1 {
        return;
    }
    let start_point = points[start];
    let end_point = points[end];
    let mut max_distance = 0.0;
    let mut max_index = None;
    for (index, point) in points.iter().enumerate().take(end).skip(start + 1) {
        let distance = perpendicular_distance_sq_world(*point, start_point, end_point);
        if distance > max_distance {
            max_distance = distance;
            max_index = Some(index);
        }
    }
    if max_distance > tolerance_sq
        && let Some(index) = max_index
    {
        keep[index] = true;
        simplify_world_range(points, start, index, tolerance_sq, keep);
        simplify_world_range(points, index, end, tolerance_sq, keep);
    }
}

fn perpendicular_distance_sq_world(point: WorldPoint, start: WorldPoint, end: WorldPoint) -> f64 {
    perpendicular_distance_sq(point.x, point.y, start.x, start.y, end.x, end.y)
}

fn perpendicular_distance_sq(px: f64, py: f64, sx: f64, sy: f64, ex: f64, ey: f64) -> f64 {
    let dx = ex - sx;
    let dy = ey - sy;
    if dx == 0.0 && dy == 0.0 {
        let ddx = px - sx;
        let ddy = py - sy;
        return ddx * ddx + ddy * ddy;
    }
    let t = (((px - sx) * dx) + ((py - sy) * dy)) / (dx * dx + dy * dy);
    let t = t.clamp(0.0, 1.0);
    let proj_x = sx + t * dx;
    let proj_y = sy + t * dy;
    let ddx = px - proj_x;
    let ddy = py - proj_y;
    ddx * ddx + ddy * ddy
}

fn dedupe_adjacent_world(points: Vec<WorldPoint>) -> Vec<WorldPoint> {
    let mut deduped = Vec::with_capacity(points.len());
    for point in points {
        if deduped
            .last()
            .copied()
            .map(|last: WorldPoint| approx_world_point(last, point))
            .unwrap_or(false)
        {
            continue;
        }
        deduped.push(point);
    }
    deduped
}

fn approx_world_point(left: WorldPoint, right: WorldPoint) -> bool {
    (left.x - right.x).abs() < 1e-9 && (left.y - right.y).abs() < 1e-9
}

fn world_polyline_length(points: &[WorldPoint]) -> f64 {
    points
        .windows(2)
        .map(|window| {
            let dx = window[1].x - window[0].x;
            let dy = window[1].y - window[0].y;
            (dx * dx + dy * dy).sqrt()
        })
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn custom_policy() -> GeometryPolicy {
        GeometryPolicy {
            simplification_tolerance: 0.0,
            min_line_length: 0.0,
            min_polygon_area: 0.0,
            min_hole_area_world: 0.0,
        }
    }

    #[test]
    fn explicit_policy_controls_line_simplification_and_length() {
        let points = [(100.0, 100.0), (150.0, 101.0), (200.0, 100.0)];
        let world = WorldGeometry::Line(vec![
            points
                .iter()
                .map(|(x, y)| WorldPoint::new(x / 4096.0, y / 4096.0))
                .collect(),
        ]);
        let mut policy = custom_policy();
        policy.simplification_tolerance = 2.0;
        let world_result = generalize_world_geometry_with_policy(policy, world.clone());
        assert_eq!(
            world_result.geometry,
            Some(WorldGeometry::Line(vec![vec![
                WorldPoint::new(100.0 / 4096.0, 100.0 / 4096.0),
                WorldPoint::new(200.0 / 4096.0, 100.0 / 4096.0),
            ]]))
        );
        policy.min_line_length = 200.0;
        let world_result = generalize_world_geometry_with_policy(policy, world);
        assert!(world_result.geometry.is_none());
        assert_eq!(world_result.stats.line_features_dropped_small, 1);
    }

    #[test]
    fn explicit_polygon_and_hole_thresholds_are_independent() {
        fn square(x: f64, size: f64) -> Vec<WorldPoint> {
            vec![
                WorldPoint::new(x, x),
                WorldPoint::new(x + size, x),
                WorldPoint::new(x + size, x + size),
                WorldPoint::new(x, x + size),
                WorldPoint::new(x, x),
            ]
        }
        let large_hole = square(0.4, 0.2);
        let geometry = WorldGeometry::Polygon(vec![WorldRingGroup {
            exterior: square(0.0, 1.0),
            holes: vec![square(0.1, 0.01), large_hole.clone()],
        }]);
        let mut policy = custom_policy();
        policy.min_hole_area_world = 0.001;
        let result = generalize_world_geometry_with_policy(policy, geometry.clone());
        let Some(WorldGeometry::Polygon(groups)) = result.geometry else {
            panic!("polygon retained")
        };
        assert_eq!(groups[0].holes, vec![large_hole]);
        policy.min_polygon_area = 4096.0 * 4096.0;
        let result = generalize_world_geometry_with_policy(policy, geometry);
        assert!(result.geometry.is_none(), "net area excludes holes");
        assert_eq!(result.stats.polygon_features_dropped_small, 1);
    }
}
