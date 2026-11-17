//! Topology repair on the final MVT integer grid. Repairing floats and then
//! rounding can introduce self-intersections and zero-area rings again.
use crate::tile_geometry::{TilePoint, TileRingGroup};
use geo::{Area, BoundingRect, Relate};
use geo_types::{LineString, Polygon};
use i_overlay::{
    core::{
        fill_rule::FillRule,
        overlay::{IntOverlayOptions, Overlay, ShapeType},
        overlay_rule::OverlayRule,
        solver::Solver,
    },
    i_float::int::point::IntPoint,
};

pub(super) fn repair_groups(groups: Vec<TileRingGroup>) -> Vec<TileRingGroup> {
    // i_overlay 4.5.2's fragmented splitter can miss an endpoint lying on a
    // nonadjacent edge. AUTO selects that path for large merged tile groups.
    // Keep the entire set and NonZero fill semantics; LIST repairs the actual
    // residential fixture without changing its coverage (see regression below).
    repair_groups_with_solver(groups, Solver::LIST)
}

/// Correct a rare overlay output that associates an otherwise exact hole ring
/// with an unrelated shell. This runs only after the whole snapped result is
/// invalid; valid overlay output keeps its original ring ownership and order.
pub(super) fn reassign_orphan_holes(groups: &mut [TileRingGroup]) -> bool {
    let line = |ring: &[TilePoint]| {
        LineString::from(
            ring.iter()
                .map(|point| (f64::from(point.x), f64::from(point.y)))
                .collect::<Vec<_>>(),
        )
    };
    let shells = groups
        .iter()
        .map(|group| Polygon::new(line(&group.exterior), vec![]))
        .collect::<Vec<_>>();
    let bounds = shells
        .iter()
        .map(BoundingRect::bounding_rect)
        .collect::<Vec<_>>();
    let areas = shells.iter().map(Area::unsigned_area).collect::<Vec<_>>();
    let previous = groups
        .iter_mut()
        .map(|group| std::mem::take(&mut group.holes))
        .collect::<Vec<_>>();
    for (source, holes) in previous.into_iter().enumerate() {
        for hole in holes {
            let polygon = Polygon::new(line(&hole), vec![]);
            let Some(hole_bounds) = polygon.bounding_rect() else {
                return false;
            };
            let covers = |index: usize| {
                bounds[index].is_some_and(|rect| {
                    rect.min().x <= hole_bounds.min().x
                        && rect.min().y <= hole_bounds.min().y
                        && rect.max().x >= hole_bounds.max().x
                        && rect.max().y >= hole_bounds.max().y
                }) && shells[index].relate(&polygon).is_covers()
            };
            let destination = if covers(source) {
                source
            } else {
                let Some(owner) = (0..shells.len())
                    .filter(|&index| index != source && covers(index))
                    .min_by(|&left, &right| areas[left].total_cmp(&areas[right]))
                else {
                    return false;
                };
                owner
            };
            groups[destination].holes.push(hole);
        }
    }
    true
}

fn repair_groups_with_solver(groups: Vec<TileRingGroup>, solver: Solver) -> Vec<TileRingGroup> {
    let capacity = groups
        .iter()
        .map(|g| g.exterior.len() + g.holes.iter().map(Vec::len).sum::<usize>())
        .sum();
    let mut options = IntOverlayOptions::ogc();
    options.preserve_input_collinear = true;
    options.preserve_output_collinear = true;
    let mut overlay = Overlay::new_custom(capacity, options, solver);
    for group in groups {
        for ring in std::iter::once(group.exterior).chain(group.holes) {
            let points = ring
                .into_iter()
                .map(|p| IntPoint::new(p.x, p.y))
                .collect::<Vec<_>>();
            overlay.add_contour(&points, ShapeType::Subject);
        }
    }
    overlay
        .overlay(OverlayRule::Subject, FillRule::NonZero)
        .into_iter()
        .filter_map(|shape| {
            let mut rings = shape.into_iter();
            Some(TileRingGroup {
                exterior: normalize(rings.next()?, true)?,
                holes: rings.filter_map(|ring| normalize(ring, false)).collect(),
            })
        })
        .collect()
}
fn normalize(ring: Vec<IntPoint>, outer: bool) -> Option<Vec<TilePoint>> {
    let mut points = ring
        .into_iter()
        .map(|p| TilePoint::new(p.x, p.y))
        .collect::<Vec<_>>();
    points.dedup();
    if points.first() != points.last() {
        points.push(*points.first()?);
    }
    if points.len() < 4 {
        return None;
    }
    let area = points
        .windows(2)
        .map(|p| p[0].x as i64 * p[1].y as i64 - p[1].x as i64 * p[0].y as i64)
        .sum::<i64>();
    if area == 0 {
        return None;
    }
    if (area > 0) != outer {
        points.reverse();
    }
    Some(points)
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::{Area, Validation};
    use geo_types::{LineString, Polygon};
    fn ring(points: &[(i32, i32)]) -> Vec<TilePoint> {
        points.iter().map(|&(x, y)| TilePoint::new(x, y)).collect()
    }
    fn polygon(group: &TileRingGroup) -> Polygon<f64> {
        let line = |p: &[TilePoint]| {
            LineString::from(
                p.iter()
                    .map(|p| (p.x as f64, p.y as f64))
                    .collect::<Vec<_>>(),
            )
        };
        Polygon::new(
            line(&group.exterior),
            group.holes.iter().map(|h| line(h)).collect(),
        )
    }
    #[test]
    fn actual_landcover_1438482_reassigns_only_orphan_hole_like_pinned_java() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-landcover-1438482-hole-owner.json"
        ))
        .unwrap();
        let mut groups: Vec<TileRingGroup> =
            serde_json::from_value(fixture["input_groups"].clone()).unwrap();
        let expected: Vec<TileRingGroup> =
            serde_json::from_value(fixture["expected_groups"].clone()).unwrap();
        let geometry = |groups: &[TileRingGroup]| {
            geo_types::MultiPolygon(groups.iter().map(polygon).collect::<Vec<_>>())
        };
        assert!(!geometry(&groups).is_valid());
        assert!(reassign_orphan_holes(&mut groups));
        assert!(geometry(&groups).is_valid());
        assert_eq!(groups.len(), 22);
        assert_eq!(geometry(&groups).unsigned_area(), 150_851.5);

        // The overlay already produced the exact rings. Compare the full
        // exterior-to-hole assignment, ignoring only starts and winding.
        fn canonical_ring(ring: &[TilePoint]) -> Vec<(i32, i32)> {
            let points = ring[..ring.len() - 1]
                .iter()
                .map(|p| (p.x, p.y))
                .collect::<Vec<_>>();
            let forward = (0..points.len())
                .map(|i| {
                    points[i..]
                        .iter()
                        .chain(&points[..i])
                        .copied()
                        .collect::<Vec<_>>()
                })
                .min()
                .unwrap();
            let reversed = points.iter().rev().copied().collect::<Vec<_>>();
            let backward = (0..reversed.len())
                .map(|i| {
                    reversed[i..]
                        .iter()
                        .chain(&reversed[..i])
                        .copied()
                        .collect::<Vec<_>>()
                })
                .min()
                .unwrap();
            forward.min(backward)
        }
        let canonical = |groups: &[TileRingGroup]| {
            let mut result = groups
                .iter()
                .map(|g| {
                    let mut holes = g
                        .holes
                        .iter()
                        .map(|h| canonical_ring(h))
                        .collect::<Vec<_>>();
                    holes.sort();
                    (canonical_ring(&g.exterior), holes)
                })
                .collect::<Vec<_>>();
            result.sort();
            result
        };
        assert_eq!(canonical(&groups), canonical(&expected));
        let floats: Vec<crate::coordinate_groups::WorldRingGroup> =
            serde_json::from_value(fixture["float_groups_mvt"].clone()).unwrap();
        let crate::tile_geometry::TileGeometry::Polygon(rendered) =
            super::super::render::snap_float_polygon_groups(floats).unwrap()
        else {
            panic!("expected polygonal snap result");
        };
        assert_eq!(canonical(&rendered), canonical(&expected));
    }
    #[test]
    fn snapped_self_crossing_is_split_into_valid_components() {
        let fixed = repair_groups(vec![TileRingGroup {
            exterior: ring(&[(0, 0), (10, 10), (0, 10), (10, 0), (0, 0)]),
            holes: vec![],
        }]);
        assert!(!fixed.is_empty());
        assert!(fixed.iter().all(|g| polygon(g).is_valid()));
        assert_eq!(
            fixed
                .iter()
                .map(|g| polygon(g).unsigned_area())
                .sum::<f64>(),
            50.0
        );
    }
    #[test]
    fn collapsed_holes_are_removed_after_quantization() {
        let fixed = repair_groups(vec![TileRingGroup {
            exterior: ring(&[(0, 0), (10, 0), (10, 10), (0, 10), (0, 0)]),
            holes: vec![ring(&[(2, 2), (3, 3), (4, 4), (2, 2)])],
        }]);
        assert_eq!(fixed.len(), 1);
        assert!(fixed[0].holes.is_empty());
        assert!(polygon(&fixed[0]).is_valid());
    }
    #[test]
    fn actual_landuse_endpoint_touch_uses_nonfragmented_repair() {
        // opt21 z9/283/173 landuse 18255700, one component of a 761-part
        // residential merge. Vertex (3506,2595) touches the nonadjacent edge
        // (3506,2592)..(3506,2600). Original area: 702.5 MVT units squared.
        let original = TileRingGroup {
            exterior: ring(&[
                (3473, 2601),
                (3470, 2601),
                (3476, 2595),
                (3476, 2605),
                (3479, 2600),
                (3480, 2600),
                (3480, 2597),
                (3482, 2593),
                (3483, 2593),
                (3485, 2595),
                (3485, 2592),
                (3486, 2589),
                (3489, 2589),
                (3489, 2584),
                (3491, 2584),
                (3491, 2585),
                (3491, 2586),
                (3490, 2588),
                (3494, 2588),
                (3494, 2587),
                (3497, 2577),
                (3497, 2564),
                (3498, 2578),
                (3499, 2583),
                (3499, 2585),
                (3499, 2588),
                (3499, 2590),
                (3500, 2595),
                (3502, 2599),
                (3503, 2603),
                (3505, 2599),
                (3505, 2597),
                (3506, 2595),
                (3505, 2595),
                (3503, 2593),
                (3500, 2591),
                (3501, 2589),
                (3503, 2589),
                (3505, 2588),
                (3503, 2586),
                (3502, 2584),
                (3500, 2582),
                (3501, 2579),
                (3504, 2572),
                (3507, 2575),
                (3513, 2580),
                (3515, 2578),
                (3510, 2572),
                (3502, 2564),
                (3501, 2562),
                (3502, 2562),
                (3503, 2563),
                (3511, 2568),
                (3512, 2568),
                (3524, 2566),
                (3515, 2579),
                (3514, 2581),
                (3506, 2591),
                (3506, 2592),
                (3506, 2600),
                (3504, 2602),
                (3504, 2603),
                (3505, 2604),
                (3505, 2605),
                (3506, 2605),
                (3505, 2606),
                (3499, 2612),
                (3499, 2611),
                (3489, 2600),
                (3489, 2599),
                (3490, 2598),
                (3491, 2598),
                (3491, 2597),
                (3491, 2596),
                (3493, 2590),
                (3484, 2599),
                (3487, 2599),
                (3487, 2600),
                (3485, 2606),
                (3474, 2609),
                (3474, 2602),
                (3473, 2601),
            ]),
            holes: vec![],
        };
        assert!(!polygon(&original).is_valid());
        // Force the path that AUTO uses for the original 37,015-coordinate set.
        // This characterizes the pinned 4.5.2 dependency failure on a small fixture.
        let fragmented = repair_groups_with_solver(vec![original.clone()], Solver::FRAG);
        assert!(fragmented.iter().any(|g| !polygon(g).is_valid()));
        let fixed = repair_groups(vec![original]);
        assert_eq!(fixed.len(), 2);
        assert!(fixed.iter().all(|g| polygon(g).is_valid()));
        assert_eq!(
            fixed
                .iter()
                .map(|g| polygon(g).unsigned_area())
                .sum::<f64>(),
            702.5
        );
        assert_eq!(repair_groups(fixed.clone()), fixed);
    }

    #[test]
    fn many_disjoint_contours_preserve_coverage_and_holes() {
        // More than the pinned fragmented-split threshold of 16,000 edges.
        // Validate individual simple components; avoid quadratic all-pairs
        // MultiPolygon validation in this large but disjoint stress fixture.
        let originals = (0..4100)
            .map(|i| {
                let x = (i % 65) * 10;
                let y = (i / 65) * 10;
                TileRingGroup {
                    exterior: ring(&[(x, y), (x + 6, y), (x + 6, y + 6), (x, y + 6), (x, y)]),
                    holes: vec![ring(&[
                        (x + 2, y + 2),
                        (x + 2, y + 4),
                        (x + 4, y + 4),
                        (x + 4, y + 2),
                        (x + 2, y + 2),
                    ])],
                }
            })
            .collect::<Vec<_>>();
        let fixed = repair_groups(originals);
        assert_eq!(fixed.len(), 4100);
        assert!(
            fixed
                .iter()
                .all(|g| g.holes.len() == 1 && polygon(g).is_valid())
        );
        assert_eq!(
            fixed
                .iter()
                .map(|g| polygon(g).unsigned_area())
                .sum::<f64>(),
            4100. * 32.
        );
        assert_eq!(repair_groups(fixed.clone()), fixed);
    }
}
