//! Restores exact source vertices after i_overlay's bounded integer-grid repair.
//! The overlay still decides topology and all generated intersections. Only an
//! output grid point with one unambiguous original vertex gets its f64 value
//! back, so coordinates immediately beside an MVT half-grid tie are preserved.
use crate::coordinate_groups::{WorldPoint, WorldRingGroup};
use i_overlay::{
    core::{fill_rule::FillRule, overlay_rule::OverlayRule, solver::Solver},
    float::overlay::{FloatOverlay, OverlayOptions},
    i_float::adapter::FloatPointAdapter,
};
use std::collections::BTreeMap;

/// Repairs one self-crossing shell using its signed-area fill. The caller
/// handles final MVT snapping and validity checks. A result with holes is
/// outside this bounded operation and remains on the caller's fallback.
#[cfg(test)]
fn repair_shells_with_provenance(points: &[WorldPoint]) -> Option<Vec<Vec<WorldPoint>>> {
    let groups = repair_groups_with_provenance(points)?;
    if groups.iter().any(|g| !g.holes.is_empty()) {
        return None;
    }
    Some(groups.into_iter().map(|g| g.exterior).collect())
}

/// Signed-area repair of one shell, retaining any holes created by repair.
/// Restoring source coordinates can affect validity, so the caller must check
/// the complete polygon set before applying shared hot-pixel snap rounding.
pub(super) fn repair_groups_with_provenance(points: &[WorldPoint]) -> Option<Vec<WorldRingGroup>> {
    let origin = *points.first()?;
    let area = points
        .windows(2)
        .map(|edge| {
            (edge[0].x - origin.x) * (edge[1].y - origin.y)
                - (edge[1].x - origin.x) * (edge[0].y - origin.y)
        })
        .sum::<f64>();
    if area == 0. || !area.is_finite() {
        return None;
    }
    repair_contours(
        &[points.to_vec()],
        if area > 0. {
            FillRule::Positive
        } else {
            FillRule::Negative
        },
    )
}

/// Buffer-zero style signed-area repair across shells and their input holes.
/// Orient every contour before overlay so holes subtract from the filled shell.
pub(super) fn repair_polygon_groups_with_provenance(
    groups: &[WorldRingGroup],
) -> Option<Vec<WorldRingGroup>> {
    let mut contours = Vec::new();
    for group in groups {
        for (ring, outer) in
            std::iter::once((&group.exterior, true)).chain(group.holes.iter().map(|h| (h, false)))
        {
            let origin = *ring.first()?;
            let area = ring
                .windows(2)
                .map(|e| {
                    (e[0].x - origin.x) * (e[1].y - origin.y)
                        - (e[1].x - origin.x) * (e[0].y - origin.y)
                })
                .sum::<f64>();
            if area == 0. || !area.is_finite() {
                return None;
            }
            let mut ring = ring.clone();
            if (area > 0.) != outer {
                ring.reverse();
            }
            contours.push(ring);
        }
    }
    repair_contours(&contours, FillRule::Positive)
}

/// Union integer-rendered inputs without discarding retained collinear vertices.
pub(super) fn union_with_provenance(
    polygons: &[geo_types::MultiPolygon<f64>],
) -> Option<geo_types::MultiPolygon<f64>> {
    let points =
        |r: &geo_types::LineString<f64>| r.0.iter().map(|p| WorldPoint::new(p.x, p.y)).collect();
    let groups = polygons
        .iter()
        .flat_map(|m| &m.0)
        .map(|p| WorldRingGroup {
            exterior: points(p.exterior()),
            holes: p.interiors().iter().map(points).collect(),
        })
        .collect::<Vec<_>>();
    let repaired = repair_polygon_groups_with_provenance(&groups)?;
    let line = |r: Vec<WorldPoint>| {
        geo_types::LineString::from(r.into_iter().map(|p| (p.x, p.y)).collect::<Vec<_>>())
    };
    let result = geo_types::MultiPolygon(
        repaired
            .into_iter()
            .map(|g| {
                geo_types::Polygon::new(line(g.exterior), g.holes.into_iter().map(line).collect())
            })
            .collect(),
    );
    use geo::Validation;
    result.is_valid().then_some(result)
}

fn repair_contours(contours: &[Vec<WorldPoint>], fill: FillRule) -> Option<Vec<WorldRingGroup>> {
    let raw = contours
        .iter()
        .map(|r| r.iter().map(|p| [p.x, p.y]).collect::<Vec<_>>())
        .collect::<Vec<_>>();
    if raw.iter().flatten().flatten().any(|v| !v.is_finite()) {
        return None;
    }
    // FloatOverlay constructs the same adapter from the same input bounds.
    let adapter = FloatPointAdapter::<[f64; 2], i32>::with_iter(raw.iter().flatten());
    let mut source_vertices = BTreeMap::<(i32, i32), Option<WorldPoint>>::new();
    for (&source, raw) in contours.iter().flatten().zip(raw.iter().flatten()) {
        let key = adapter.float_to_int(raw);
        source_vertices
            .entry((key.x, key.y))
            .and_modify(|existing| {
                if existing.is_some_and(|vertex| vertex != source) {
                    *existing = None;
                }
            })
            .or_insert(Some(source));
    }
    let mut options = OverlayOptions::ogc();
    options.preserve_input_collinear = true;
    options.preserve_output_collinear = true;
    let shapes = FloatOverlay::with_subj_custom(&raw, options, Solver::LIST)
        .overlay(OverlayRule::Subject, fill);
    if shapes.is_empty() {
        return None;
    }
    shapes
        .into_iter()
        .map(|shape| {
            let restore_ring = |raw: Vec<[f64; 2]>| {
                let mut ring = raw
                    .into_iter()
                    .map(|output| {
                        let key = adapter.float_to_int(&output);
                        source_vertices
                            .get(&(key.x, key.y))
                            .and_then(|source| *source)
                            .unwrap_or_else(|| WorldPoint::new(output[0], output[1]))
                    })
                    .collect::<Vec<_>>();
                if ring.first() != ring.last() {
                    ring.push(*ring.first()?);
                }
                Some(ring)
            };
            let mut rings = shape.into_iter();
            Some(WorldRingGroup {
                exterior: restore_ring(rings.next()?)?,
                holes: rings.map(restore_ring).collect::<Option<Vec<_>>>()?,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preserves_unique_source_vertices_and_generated_intersections() {
        let p = WorldPoint::new;
        let source = vec![
            p(0.499_999_9, 0.),
            p(4000., 3900.),
            p(4000., 0.),
            p(0., 4000.),
            p(0.499_999_9, 0.),
        ];
        let repaired = repair_shells_with_provenance(&source).unwrap();
        assert!(repaired.iter().flatten().any(|point| *point == source[0]));
        assert!(repaired.iter().flatten().any(|point| {
            (point.x - 2_025.438_318_856_567_5).abs() < 1.
                && (point.y - 1_974.561_681_143_432_5).abs() < 1.
        }));
        assert!(repaired.iter().all(|ring| ring.first() == ring.last()));
    }

    #[test]
    fn ambiguous_input_key_does_not_select_an_arbitrary_vertex() {
        let p = WorldPoint::new;
        let source = vec![
            p(0., 0.),
            p(0.000_000_1, 0.),
            p(4000., 4000.),
            p(0., 4000.),
            p(4000., 0.),
            p(0., 0.),
        ];
        let repaired = repair_shells_with_provenance(&source).unwrap();
        assert!(!repaired.iter().flatten().any(|point| *point == source[1]));
    }
}
