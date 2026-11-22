//! Schema-neutral rendering for the OpenMapTiles profile.
use super::{
    store::{AttrMap, AttrValue},
    types::Feature,
};
use crate::{
    compile_area::Region,
    coordinate_groups::*,
    generalization::{GeometryPolicy, generalize_world_geometry_with_policy},
    tile_geometry::{TileGeometry, TilePoint, TileRingGroup},
    tile_slicer::*,
};
use pmtiles::{TileCoord, TileId};
use serde::{Deserialize, Serialize};
use std::borrow::Cow;

// Pinned TiledGeometry.NEIGHBOR_BUFFER_EPS, in 256-pixel tile units. Points
// just beyond a nominal buffer edge are still sliced into the neighboring tile.
const POINT_NEIGHBOR_BUFFER_EPS: f64 = 0.1 / 4096.0;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(super) struct Record {
    pub tile_id: u64,
    pub zoom: u8,
    pub id: u64,
    pub layer: Cow<'static, str>,
    pub geometry: TileGeometry,
    pub attrs: AttrMap,
    pub sort_key: i64,
    pub grid_size: f64,
    pub grid_cell: Option<(i32, i32)>,
    pub grid_limit: usize,
    pub rank: bool,
}

fn attrs(feature: &Feature, z: u8) -> AttrMap {
    feature
        .attrs
        .iter()
        .filter(|(k, _)| feature.attr_minzoom.get(*k).is_none_or(|min| z >= *min))
        .filter_map(|(k, v)| {
            let value = match v {
                serde_json::Value::String(v) => AttrValue::String(v.clone()),
                serde_json::Value::Bool(v) => AttrValue::Bool(*v),
                serde_json::Value::Number(v) => {
                    if let Some(i) = v.as_i64() {
                        AttrValue::Int(i)
                    } else if let Some(i) = v.as_u64() {
                        AttrValue::UInt(i)
                    } else {
                        AttrValue::Double(v.as_f64()?)
                    }
                }
                _ => return None,
            };
            Some((Cow::Owned(k.clone()), value))
        })
        .collect()
}

fn bounds(geometry: &WorldGeometry) -> Option<(f64, f64, f64, f64)> {
    match geometry {
        WorldGeometry::Point(p) => world_bounds_for_lines(std::slice::from_ref(p)),
        WorldGeometry::Line(p) => world_bounds_for_lines(p),
        WorldGeometry::Polygon(p) => world_bounds_for_polygons(p),
    }
}

fn tile_point(p: WorldPoint, x: i32, y: i32, scale_exp: u8) -> TilePoint {
    let extent = 4096.0 * (1u32 << scale_exp) as f64;
    // Java Math.round rounds a negative tie toward positive infinity.
    TilePoint::new(
        ((p.x - x as f64) * extent + 0.5).floor() as i32,
        ((p.y - y as f64) * extent + 0.5).floor() as i32,
    )
}
fn ring_area(r: &[WorldPoint]) -> f64 {
    let Some(origin) = r.first() else { return 0.0 };
    r.windows(2)
        .map(|p| {
            (p[0].x - origin.x) * (p[1].y - origin.y) - (p[1].x - origin.x) * (p[0].y - origin.y)
        })
        .sum::<f64>()
        .abs()
        / 2.0
}
fn snap_ring(r: &[WorldPoint], x: i32, y: i32, outer: bool) -> Option<Vec<TilePoint>> {
    let mut ring = r
        .iter()
        .map(|p| tile_point(*p, x, y, 0))
        .collect::<Vec<_>>();
    ring.dedup();
    if ring.first() != ring.last() {
        ring.push(*ring.first()?);
    }
    if ring.len() < 4 {
        return None;
    }
    let area = ring
        .windows(2)
        .map(|p| p[0].x as i64 * p[1].y as i64 - p[1].x as i64 * p[0].y as i64)
        .sum::<i64>();
    if area == 0 {
        return None;
    }
    if (area > 0) != outer {
        ring.reverse();
    }
    Some(ring)
}
fn tile_polygons(groups: Vec<WorldRingGroup>, x: i32, y: i32) -> Option<TileGeometry> {
    let groups = groups
        .into_iter()
        .filter_map(|g| {
            Some(TileRingGroup {
                exterior: snap_ring(&g.exterior, x, y, true)?,
                holes: g
                    .holes
                    .iter()
                    .filter_map(|h| snap_ring(h, x, y, false))
                    .collect(),
            })
        })
        .collect::<Vec<_>>();
    let groups = super::repair::repair_groups(groups);
    (!groups.is_empty()).then_some(TileGeometry::Polygon(groups))
}

fn simple_ring_valid(points: &[WorldPoint]) -> bool {
    let line = geo_types::LineString::from(points.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>());
    points.len() >= 4
        && super::polygon_validity::polygon_valid(&geo_types::Polygon::new(line, vec![]))
}

// Pinned snapAndFixPolygon retains stripe-clipped, pointwise-rounded
// coordinates when the entire result is valid,
// including holes and separate components. Validate the whole polygon set so
// overlaps or touching holes still take the topology-repair fallback.
fn clip_valid_groups(
    column: &[ColumnGroup],
    rect: WorldRect,
    y: i32,
) -> Option<Option<TileGeometry>> {
    let clip = |ring: &ColumnRing| {
        column_ring_y(ring, rect)
            .into_iter()
            .map(|p| WorldPoint::new(p.x, p.y - y as f64))
            .collect::<Vec<_>>()
    };
    let clipped = column
        .iter()
        .filter_map(|g| {
            let exterior = clip(&g.exterior);
            (!exterior.is_empty()).then(|| WorldRingGroup {
                exterior,
                holes: g.holes.iter().map(clip).filter(|h| !h.is_empty()).collect(),
            })
        })
        .collect::<Vec<_>>();
    if clipped.is_empty() {
        return Some(None);
    }
    let scale = |ring: Vec<WorldPoint>| {
        ring.into_iter()
            .map(|p| WorldPoint::new(p.x * 4096., p.y * 4096.))
            .collect()
    };
    snap_tile_slice(
        clipped
            .into_iter()
            .map(|g| WorldRingGroup {
                exterior: scale(g.exterior),
                holes: g.holes.into_iter().map(scale).collect(),
            })
            .collect(),
    )
}

/// One tile's slice of a polygon onto the MVT grid, as FeatureRenderer does
/// with `GeoUtils.snapAndFixPolygon`: pointwise rounding when that stays
/// valid, otherwise the port of JTS's repair and precision reduction, whose
/// empty result (`Some(None)`) is final, as it is in Planetiler: a slice that
/// only runs along the tile's buffer edge and back, say, leaves nothing to
/// write. `None` when neither applies, for the caller's own fallbacks.
/// Coordinates are MVT units.
fn snap_tile_slice(float_groups: Vec<WorldRingGroup>) -> Option<Option<TileGeometry>> {
    if let Some(snapped) = pointwise_snap(&float_groups) {
        return Some(Some(snapped));
    }
    let line = |r: &[WorldPoint]| {
        geo_types::LineString::from(r.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>())
    };
    let input = geo_types::MultiPolygon(
        float_groups
            .iter()
            .map(|g| {
                geo_types::Polygon::new(
                    line(&g.exterior),
                    g.holes.iter().map(|h| line(h)).collect(),
                )
            })
            .collect(),
    );
    if let Some(fixed) = super::jts_buffer::snap_and_fix(&input) {
        let groups = super::store::from_grid_polygons(fixed);
        return Some((!groups.is_empty()).then_some(TileGeometry::Polygon(groups)));
    }
    node_valid_float_groups(float_groups).map(Some)
}

/// The pointwise-rounded groups, when every ring survives and the whole set
/// stays valid.
fn pointwise_snap(float_groups: &[WorldRingGroup]) -> Option<TileGeometry> {
    let snap_mvt_ring = |ring: &[WorldPoint], outer| {
        // Power-of-two scaling preserves the original floating-point coordinates.
        let world = ring
            .iter()
            .map(|p| WorldPoint::new(p.x / 4096., p.y / 4096.))
            .collect::<Vec<_>>();
        snap_ring(&world, 0, 0, outer)
    };
    let snapped = float_groups
        .iter()
        .map(|g| {
            Some(TileRingGroup {
                exterior: snap_mvt_ring(&g.exterior, true)?,
                holes: g
                    .holes
                    .iter()
                    .map(|h| snap_mvt_ring(h, false))
                    .collect::<Option<Vec<_>>>()?,
            })
        })
        .collect::<Option<Vec<_>>>();
    if let Some(snapped) = snapped {
        let line = |r: &[TilePoint]| {
            geo_types::LineString::from(
                r.iter()
                    .map(|p| (p.x as f64, p.y as f64))
                    .collect::<Vec<_>>(),
            )
        };
        let geometry = geo_types::MultiPolygon(
            snapped
                .iter()
                .map(|g| {
                    geo_types::Polygon::new(
                        line(&g.exterior),
                        g.holes.iter().map(|h| line(h)).collect(),
                    )
                })
                .collect(),
        );
        if super::polygon_validity::is_valid(&geometry) {
            return Some(TileGeometry::Polygon(snapped));
        }
    }
    None
}

/// Pinned pointwise-valid shortcut followed by topology-preserving grid
/// reduction, without the JTS repair of `snap_tile_slice`, for tests that
/// check that reduction. All input coordinates are in MVT units.
#[cfg(test)]
pub(super) fn snap_float_polygon_groups(float_groups: Vec<WorldRingGroup>) -> Option<TileGeometry> {
    pointwise_snap(&float_groups).or_else(|| node_valid_float_groups(float_groups))
}

// Match the pinned TiledGeometry X-stripe then Y-stripe clipping. Unlike a
// Boolean overlay, this retains input vertices between crossings. Restrict it
// to one shell whose clipped, snapped result is valid. Like upstream,
// input crossings outside this tile do not require repairing the whole ring.
fn clip_stripe(points: &[WorldPoint], lower: f64, upper: f64, vertical: bool) -> Vec<WorldPoint> {
    let axis = |p: WorldPoint| if vertical { p.x } else { p.y };
    let mut out = Vec::new();
    for edge in points.windows(2) {
        let (a, b) = (edge[0], edge[1]);
        let (av, bv) = (axis(a), axis(b));
        let intersection = |boundary| {
            let t = (boundary - av) / (bv - av);
            if vertical {
                WorldPoint::new(boundary, a.y + (b.y - a.y) * t)
            } else {
                WorldPoint::new(a.x + (b.x - a.x) * t, boundary)
            }
        };
        if av < lower {
            if bv > lower {
                out.push(intersection(lower));
            }
        } else if av > upper {
            if bv < upper {
                out.push(intersection(upper));
            }
        } else {
            out.push(a);
        }
        if bv < lower && av >= lower {
            out.push(intersection(lower));
        }
        if bv > upper && av <= upper {
            out.push(intersection(upper));
        }
    }
    if let Some(last) = points.last().copied()
        && axis(last) >= lower
        && axis(last) <= upper
    {
        out.push(last);
    }
    out.dedup();
    if let Some(first) = out.first().copied()
        && out.last() != Some(&first)
    {
        out.push(first);
    }
    out
}

/// A ring after the X stripe of one tile column, in column-local X. The stripe
/// depends only on the column, so it is computed once and every tile of the
/// column applies just its own Y stripe. `min_y`/`max_y` bound the stripe output.
struct ColumnRing {
    points: Vec<WorldPoint>,
    min_y: f64,
    max_y: f64,
}

struct ColumnGroup {
    exterior: ColumnRing,
    holes: Vec<ColumnRing>,
}

fn column_ring(ring: &[WorldPoint], min_x: f64, max_x: f64, x: i32) -> ColumnRing {
    let local = ring
        .iter()
        .map(|p| WorldPoint::new(p.x - x as f64, p.y))
        .collect::<Vec<_>>();
    let points = clip_stripe(&local, min_x - x as f64, max_x - x as f64, true);
    let (mut min_y, mut max_y) = (f64::INFINITY, f64::NEG_INFINITY);
    for p in &points {
        if p.y.is_nan() {
            // The stripe keeps NaN points; never treat such a ring as apart.
            (min_y, max_y) = (f64::NEG_INFINITY, f64::INFINITY);
            break;
        }
        min_y = min_y.min(p.y);
        max_y = max_y.max(p.y);
    }
    ColumnRing {
        points,
        min_y,
        max_y,
    }
}

/// X stripes of every ring for the tile column `x` whose X range is
/// `min_x..=max_x`. A group whose bounds (`min_x, max_x, ..`) lie strictly
/// outside the stripe has an empty exterior stripe, hence is dropped by the
/// callers, so its rings are not clipped. Local X is monotone in X, which makes
/// the comparison exact.
fn polygon_column(
    groups: &[WorldRingGroup],
    group_bounds: &[Option<(f64, f64, f64, f64)>],
    min_x: f64,
    max_x: f64,
    x: i32,
) -> Vec<ColumnGroup> {
    let empty = || ColumnRing {
        points: Vec::new(),
        min_y: f64::INFINITY,
        max_y: f64::NEG_INFINITY,
    };
    let xf = x as f64;
    groups
        .iter()
        .zip(group_bounds)
        .map(|(g, bounds)| {
            if bounds.is_some_and(|b| b.1 - xf < min_x - xf || b.0 - xf > max_x - xf) {
                return ColumnGroup {
                    exterior: empty(),
                    holes: Vec::new(),
                };
            }
            ColumnGroup {
                exterior: column_ring(&g.exterior, min_x, max_x, x),
                holes: g
                    .holes
                    .iter()
                    .map(|h| column_ring(h, min_x, max_x, x))
                    .collect(),
            }
        })
        .collect()
}

fn column_ring_apart(ring: &ColumnRing, rect: WorldRect) -> bool {
    ring.max_y < rect.min_y || ring.min_y > rect.max_y
}

/// The stripes of a column's rings for one tile, in world coordinates. Empty
/// stripes are dropped, and so are groups left with no ring.
fn tile_stripes(column: &[ColumnGroup], rect: WorldRect, x: i32) -> Vec<WorldRingGroup> {
    let world = |ring: &ColumnRing| {
        column_ring_y(ring, rect)
            .into_iter()
            .map(|p| WorldPoint::new(p.x + x as f64, p.y))
            .collect::<Vec<_>>()
    };
    column
        .iter()
        .map(|g| WorldRingGroup {
            exterior: world(&g.exterior),
            holes: g
                .holes
                .iter()
                .map(world)
                .filter(|h| !h.is_empty())
                .collect(),
        })
        .filter(|g| !g.exterior.is_empty() || !g.holes.is_empty())
        .collect()
}

/// Whether the Y stripe of a column ring for one tile is empty, decided edge
/// by edge exactly as `clip_stripe` does but without building the stripe:
/// every edge stays on one side of the stripe and so does the last point.
fn column_ring_misses(ring: &ColumnRing, rect: WorldRect) -> bool {
    let (lower, upper) = (rect.min_y, rect.max_y);
    column_ring_apart(ring, rect)
        || ring.points.windows(2).all(|edge| {
            let (a, b) = (edge[0].y, edge[1].y);
            a < lower && b <= lower || a > upper && b >= upper
        }) && ring
            .points
            .last()
            .is_none_or(|p| p.y < lower || p.y > upper)
}

/// The Y stripe of a column ring for one tile. A ring whose Y bounds lie
/// strictly outside the stripe has no point in range and no crossing, so its
/// stripe is empty.
fn column_ring_y(ring: &ColumnRing, rect: WorldRect) -> Vec<WorldPoint> {
    if column_ring_apart(ring, rect) {
        return Vec::new();
    }
    clip_stripe(&ring.points, rect.min_y, rect.max_y, false)
}

// Valid float polygons can acquire intersections during grid rounding. Share
// all vertex hot pixels, including vertices from subsequently collapsed holes.
fn node_valid_float_groups(mut float_groups: Vec<WorldRingGroup>) -> Option<TileGeometry> {
    let line = |r: &[WorldPoint]| {
        geo_types::LineString::from(r.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>())
    };
    let geometry = geo_types::MultiPolygon(
        float_groups
            .iter()
            .map(|g| {
                geo_types::Polygon::new(
                    line(&g.exterior),
                    g.holes.iter().map(|h| line(h)).collect(),
                )
            })
            .collect(),
    );
    if !super::polygon_validity::is_valid(&geometry) {
        let repaired = super::float_repair::repair_polygon_groups_with_provenance(&float_groups)?;
        let valid = geo_types::MultiPolygon(
            repaired
                .iter()
                .map(|g| {
                    geo_types::Polygon::new(
                        line(&g.exterior),
                        g.holes.iter().map(|h| line(h)).collect(),
                    )
                })
                .collect(),
        );
        if !super::polygon_validity::is_valid(&valid) {
            return None;
        }
        float_groups = repaired;
    }
    for group in &mut float_groups {
        for (ring, outer) in std::iter::once((&mut group.exterior, true))
            .chain(group.holes.iter_mut().map(|h| (h, false)))
        {
            let origin = *ring.first()?;
            let area = ring
                .windows(2)
                .map(|e| {
                    (e[0].x - origin.x) * (e[1].y - origin.y)
                        - (e[1].x - origin.x) * (e[0].y - origin.y)
                })
                .sum::<f64>();
            if (area > 0.) != outer {
                ring.reverse();
            }
        }
    }
    let rings = float_groups
        .iter()
        .flat_map(|g| std::iter::once(&g.exterior).chain(&g.holes))
        .cloned()
        .collect::<Vec<_>>();
    let mut noded = super::snap_round::node_valid_rings(&rings).into_iter();
    let mut groups = super::repair::repair_groups(
        float_groups
            .into_iter()
            .map(|g| TileRingGroup {
                exterior: noded.next().expect("noded exterior"),
                holes: (0..g.holes.len())
                    .map(|_| noded.next().expect("noded hole"))
                    .collect(),
            })
            .collect(),
    );
    let valid = |groups: &[TileRingGroup]| {
        let result = geo_types::MultiPolygon(
            groups
                .iter()
                .map(|g| {
                    let line = |r: &[TilePoint]| {
                        geo_types::LineString::from(
                            r.iter()
                                .map(|p| (p.x as f64, p.y as f64))
                                .collect::<Vec<_>>(),
                        )
                    };
                    geo_types::Polygon::new(
                        line(&g.exterior),
                        g.holes.iter().map(|h| line(h)).collect(),
                    )
                })
                .collect(),
        );
        super::polygon_validity::is_valid(&result)
    };
    if groups.is_empty() {
        return None;
    }
    if !valid(&groups) && (!super::repair::reassign_orphan_holes(&mut groups) || !valid(&groups)) {
        // A bounded repair may still produce an unsupported topology. Preserve
        // the caller's fallback rather than emitting invalid MVT polygons.
        return None;
    }
    Some(TileGeometry::Polygon(groups))
}

/// One shell's slice for one tile, snapped as `snap_tile_slice` does:
/// `Some(None)` when Planetiler's repair leaves nothing, `None` when this
/// path cannot decide and the caller's fallbacks apply.
fn clip_valid_shell(ring: &ColumnRing, rect: WorldRect, y: i32) -> Option<Option<TileGeometry>> {
    let clipped = column_ring_y(ring, rect);
    let mut snapped = clipped
        .iter()
        .map(|p| {
            TilePoint::new(
                (p.x * 4096. + 0.5).floor() as i32,
                ((p.y - y as f64) * 4096. + 0.5).floor() as i32,
            )
        })
        .collect::<Vec<_>>();
    snapped.dedup();
    let check = snapped
        .iter()
        .map(|p| WorldPoint::new(p.x as f64, p.y as f64))
        .collect::<Vec<_>>();
    let area = snapped
        .windows(2)
        .map(|p| p[0].x as i64 * p[1].y as i64 - p[1].x as i64 * p[0].y as i64)
        .sum::<i64>();
    if !simple_ring_valid(&check) || area == 0 {
        if clipped.is_empty() {
            return None;
        }
        let local = clipped
            .iter()
            .map(|p| WorldPoint::new(p.x * 4096., (p.y - y as f64) * 4096.))
            .collect::<Vec<_>>();
        // GeoUtils.snapAndFixPolygon: the rounded ring is invalid, so JTS
        // repairs the slice and reduces its precision.
        let input = geo_types::MultiPolygon(vec![geo_types::Polygon::new(
            geo_types::LineString::from(local.iter().map(|p| (p.x, p.y)).collect::<Vec<_>>()),
            vec![],
        )]);
        if let Some(fixed) = super::jts_buffer::snap_and_fix(&input) {
            let groups = super::store::from_grid_polygons(fixed);
            return Some((!groups.is_empty()).then_some(TileGeometry::Polygon(groups)));
        }
        let float_groups = if simple_ring_valid(&local) {
            vec![WorldRingGroup {
                exterior: local,
                holes: vec![],
            }]
        } else {
            super::float_repair::repair_groups_with_provenance(&local)?
        };
        return node_valid_float_groups(float_groups).map(Some);
    }
    if area < 0 {
        snapped.reverse();
    }
    Some(Some(TileGeometry::Polygon(vec![TileRingGroup {
        exterior: snapped,
        holes: vec![],
    }])))
}

// Restrict the pre-snap repair to a single contained polygon whose shell has
// proper crossings both before and after rounding. These crossings prove both
// inputs invalid without a quadratic full-polygon validity check. Other invalid
// configurations and polygons requiring clipping retain the existing path.
fn repair_contained_crossing_polygon(
    groups: &[WorldRingGroup],
    rect: WorldRect,
    x: i32,
    y: i32,
) -> Option<Vec<WorldRingGroup>> {
    use geo::Line;
    use geo::algorithm::{line_intersection::LineIntersection, sweep::Intersections};
    use i_overlay::{
        core::{fill_rule::FillRule, overlay_rule::OverlayRule, solver::Solver},
        float::overlay::{FloatOverlay, OverlayOptions},
    };
    let [group] = groups else { return None };
    // Preexisting holes need separate JTS qualification; this path repairs
    // crossings in a shell, which may itself produce holes.
    if !group.holes.is_empty() {
        return None;
    }
    if !std::iter::once(&group.exterior)
        .chain(&group.holes)
        .flatten()
        .all(|p| p.x > rect.min_x && p.x < rect.max_x && p.y > rect.min_y && p.y < rect.max_y)
    {
        return None;
    }
    let crossing = |points: &[[f64; 2]]| {
        Intersections::from_iter(
            points
                .windows(2)
                .filter(|p| p[0] != p[1])
                .map(|p| Line::from([(p[0][0], p[0][1]), (p[1][0], p[1][1])])),
        )
        .any(|(_, _, intersection)| {
            matches!(
                intersection,
                LineIntersection::SinglePoint {
                    is_proper: true,
                    ..
                }
            )
        })
    };
    let snapped = group
        .exterior
        .iter()
        .map(|p| {
            let p = tile_point(*p, x, y, 0);
            [p.x as f64, p.y as f64]
        })
        .collect::<Vec<_>>();
    if !crossing(&snapped) {
        return None;
    }
    let local = |ring: &[WorldPoint]| {
        ring.iter()
            .map(|p| [(p.x - x as f64) * 4096., (p.y - y as f64) * 4096.])
            .collect::<Vec<_>>()
    };
    let shell = local(&group.exterior);
    if !crossing(&shell) {
        return None;
    }
    // The reference slicer leaves a contained ring intact, then snapAndFixPolygon
    // applies buffer(0) to invalid float input before precision reduction. The
    // ordinary Boolean intersection would already change this ring's topology.
    // JTS BufferCurveSetBuilder uses Orientation.isCCWArea, not the local
    // extremal-vertex winding used by geo::Buffer. The latter can select an
    // empty/wrong lobe of a self-crossing ring (OSM way 724352898).
    let origin = shell[0];
    let area2 = shell
        .windows(2)
        .map(|p| {
            (p[0][0] - origin[0]) * (p[1][1] - origin[1])
                - (p[1][0] - origin[0]) * (p[0][1] - origin[1])
        })
        .sum::<f64>();
    if area2 == 0. {
        return None;
    }
    let fill = if area2 > 0. {
        FillRule::Positive
    } else {
        FillRule::Negative
    };
    let mut options = OverlayOptions::ogc();
    options.preserve_input_collinear = true;
    options.preserve_output_collinear = true;
    let repaired = FloatOverlay::with_subj_custom(&shell, options, Solver::LIST)
        .overlay(OverlayRule::Subject, fill);
    let world_ring = |r: Vec<[f64; 2]>| {
        let mut points = r
            .into_iter()
            .map(|p| WorldPoint::new(x as f64 + p[0] / 4096., y as f64 + p[1] / 4096.))
            .collect::<Vec<_>>();
        if points.first() != points.last()
            && let Some(first) = points.first().copied()
        {
            points.push(first);
        }
        points
    };
    Some(
        repaired
            .into_iter()
            .filter_map(|shape| {
                let mut rings = shape.into_iter();
                Some(WorldRingGroup {
                    exterior: world_ring(rings.next()?),
                    holes: rings.map(world_ring).collect(),
                })
            })
            .collect(),
    )
}

// Pinned GeoUtils.labelGridId: wrap X only, floor BEFORE MVT quantization.
fn label_grid_cell(point: WorldPoint, zoom: u8, pixels: f64) -> Option<(i32, i32)> {
    let size = pixels / 256.0;
    if size < 1.0 / 4096.0 {
        return None;
    }
    let width = f64::from(1u32 << zoom);
    let mut x = point.x % width;
    if x < 0.0 {
        x += width;
    }
    Some(((x / size).floor() as i32, (point.y / size).floor() as i32))
}

// Zoom-independent world coordinates in [0, 1]; `scale_world` applies 2^zoom.
// Multiplying the projected value by a power of two is the last operation at
// every zoom, so projecting once is bit-identical to projecting per zoom.
fn feature_unit_geometry(feature: &Feature) -> Option<WorldGeometry> {
    if let Some(point) = feature.world_point {
        return Some(WorldGeometry::Point(vec![WorldPoint::new(
            point.x, point.y,
        )]));
    }
    let world = if feature.osm_outline_projection {
        super::interior_point::projected_osm(&feature.geometry)
    } else {
        // External inputs use GeoUtils' floating projection, including its
        // finite polar clamp. They must not pass through the OSM node store.
        super::interior_point::projected(&feature.geometry)
    };
    let ring =
        |r: &geo_types::LineString<f64>| r.0.iter().map(|p| WorldPoint::new(p.x, p.y)).collect();
    let polygon = |p: &geo_types::Polygon<f64>| WorldRingGroup {
        exterior: ring(p.exterior()),
        holes: p.interiors().iter().map(ring).collect(),
    };
    match &world {
        geo_types::Geometry::Point(p) => {
            Some(WorldGeometry::Point(vec![WorldPoint::new(p.x(), p.y())]))
        }
        geo_types::Geometry::MultiPoint(p) => Some(WorldGeometry::Point(
            p.0.iter().map(|p| WorldPoint::new(p.x(), p.y())).collect(),
        )),
        geo_types::Geometry::LineString(line) => Some(WorldGeometry::Line(vec![ring(line)])),
        geo_types::Geometry::MultiLineString(lines) => {
            Some(WorldGeometry::Line(lines.0.iter().map(ring).collect()))
        }
        geo_types::Geometry::Polygon(p) => Some(WorldGeometry::Polygon(vec![polygon(p)])),
        geo_types::Geometry::MultiPolygon(p) => {
            Some(WorldGeometry::Polygon(p.0.iter().map(polygon).collect()))
        }
        _ => None,
    }
}

fn scale_world(unit: &WorldGeometry, zoom: u8) -> WorldGeometry {
    let scale = f64::from(1u32 << zoom);
    let ring = |r: &Vec<WorldPoint>| -> Vec<WorldPoint> {
        r.iter()
            .map(|p| WorldPoint::new(p.x * scale, p.y * scale))
            .collect()
    };
    match unit {
        WorldGeometry::Point(points) => WorldGeometry::Point(ring(points)),
        WorldGeometry::Line(lines) => WorldGeometry::Line(lines.iter().map(ring).collect()),
        WorldGeometry::Polygon(groups) => WorldGeometry::Polygon(
            groups
                .iter()
                .map(|g| WorldRingGroup {
                    exterior: ring(&g.exterior),
                    holes: g.holes.iter().map(ring).collect(),
                })
                .collect(),
        ),
    }
}

// TiledGeometry slices the original world, then the copies crossing its east
// and west edges. Keep these in one feature so polygon snapping can node their
// shared boundaries. The attribute point count and label grid use the source.
fn wrap_world_copies(world: &mut WorldGeometry, width: f64, buffer: f64) {
    if let WorldGeometry::Point(points) = world {
        for p in points {
            p.x = p.x.rem_euclid(width);
        }
    }
    let Some((left, right, _, _)) = bounds(world) else {
        return;
    };
    let shifts = [
        (right + buffer >= width).then_some(-width),
        (left - buffer < 0.).then_some(width),
    ];
    let translate = |ring: &[WorldPoint], shift: f64| {
        ring.iter()
            .map(|p| WorldPoint::new(p.x + shift, p.y))
            .collect::<Vec<_>>()
    };
    match world {
        WorldGeometry::Point(points) => {
            let original = points.len();
            for shift in shifts.into_iter().flatten() {
                points.extend(translate(&points[..original], shift));
            }
        }
        WorldGeometry::Line(lines) => {
            let original = lines.len();
            for shift in shifts.into_iter().flatten() {
                let copies = lines[..original]
                    .iter()
                    .map(|r| translate(r, shift))
                    .collect::<Vec<_>>();
                lines.extend(copies);
            }
        }
        WorldGeometry::Polygon(groups) => {
            let original = groups.len();
            for shift in shifts.into_iter().flatten() {
                let copies = groups[..original]
                    .iter()
                    .map(|g| WorldRingGroup {
                        exterior: translate(&g.exterior, shift),
                        holes: g.holes.iter().map(|r| translate(r, shift)).collect(),
                    })
                    .collect::<Vec<_>>();
                groups.extend(copies);
            }
        }
    }
}

const BOUNDS_APART_MARGIN: f64 = 1e-6;

// The X half of `bounds_apart`, for a whole tile column.
fn bounds_apart_x(b: (f64, f64, f64, f64), min_x: f64, max_x: f64) -> bool {
    b.1 < min_x - BOUNDS_APART_MARGIN || b.0 > max_x + BOUNDS_APART_MARGIN
}

// True when a bounding box (min_x, max_x, min_y, max_y) lies beyond the tile
// rect by more than float noise, so no ring of it can reach the tile.
fn bounds_apart(b: (f64, f64, f64, f64), rect: WorldRect) -> bool {
    bounds_apart_x(b, rect.min_x, rect.max_x)
        || b.3 < rect.min_y - BOUNDS_APART_MARGIN
        || b.2 > rect.max_y + BOUNDS_APART_MARGIN
}

/// The winding Planetiler slices and repairs: `extractGroups` turns every
/// ring counter-clockwise by `Area.ofRingSigned`, and `reassemblePolygon`
/// reverses the holes again after slicing. A hole fully inside a tile comes
/// out the same either way, start point included. Where the JTS repair has
/// to decide an invalid slice, its edge order, and with it the depth its
/// non-transitive `DepthSegment` order picks, follows these windings.
fn orient_like_planetiler(groups: &mut [WorldRingGroup]) {
    // JTS Area.ofRingSigned: positive for a clockwise ring.
    let signed_area = |ring: &[WorldPoint]| {
        if ring.len() < 3 {
            return 0.;
        }
        let x0 = ring[0].x;
        let mut sum = 0.;
        for i in 1..ring.len() - 1 {
            sum += (ring[i].x - x0) * (ring[i - 1].y - ring[i + 1].y);
        }
        sum / 2.
    };
    for group in groups {
        if signed_area(&group.exterior) > 0. {
            group.exterior.reverse();
        }
        for hole in &mut group.holes {
            if signed_area(hole) <= 0. {
                hole.reverse();
            }
        }
    }
}

fn polygon_tile(
    groups: &[WorldRingGroup],
    column: &[ColumnGroup],
    rect: WorldRect,
    x: i32,
    y: i32,
) -> Option<TileGeometry> {
    // No ring reaches the tile's rows within this column, so the stripe clip
    // is empty and the contained-shell repair needs a point inside the tile.
    // The intersection below is empty too: the column rings keep the winding
    // number of every point in the column, and a ring with no edge in the
    // tile's rows cannot wind around the tile, whose row it would cross.
    // Holes count: in invalid input one can lie outside its shell.
    if column.iter().all(|g| {
        column_ring_misses(&g.exterior, rect) && g.holes.iter().all(|h| column_ring_misses(h, rect))
    }) {
        return None;
    }
    let clipped = if groups.len() == 1 && groups[0].holes.is_empty() {
        clip_valid_shell(&column[0].exterior, rect, y)
    } else {
        clip_valid_groups(column, rect, y)
    };
    // A slice the JTS repair decided, empty included, is final, as in
    // Planetiler; the fallbacks below are only for what it declined.
    if let Some(decided) = clipped {
        return decided;
    }
    repair_contained_crossing_polygon(groups, rect, x, y)
        .and_then(|repaired| tile_polygons(repaired, x, y))
        .or_else(|| {
            // Floating repair can collapse a degenerate input. Rather than
            // lose a feature the JTS port could not decide, intersect the
            // rings' stripes for this tile, not the whole polygon: the stripes
            // keep the winding number of every point of the tile, so the
            // region is the same, at a cost bounded by what reaches the tile
            // rather than by the polygon's size.
            let stripes = tile_stripes(column, rect, x);
            tile_polygons(
                clip_prepared_polygon_groups_to_rect(&prepare_polygon_groups(&stripes), rect),
                x,
                y,
            )
        })
}

pub(super) fn emit_with_record_sink(
    feature: Feature,
    region: &Region,
    minzoom: u8,
    maxzoom: u8,
    sink: &mut impl FnMut(Record) -> Result<(), String>,
) -> Result<(), String> {
    let unit_geometry = std::cell::OnceCell::new();
    let unit_source = std::cell::OnceCell::new();
    for z in feature.min_zoom.max(minzoom)..=feature.max_zoom.min(maxzoom) {
        let size = if z == maxzoom {
            feature.min_size_at_max_zoom
        } else {
            feature.min_size
        };
        let tolerance = if z == maxzoom {
            feature.tolerance_at_max_zoom
        } else {
            feature.tolerance
        };
        let policy = GeometryPolicy {
            simplification_tolerance: tolerance * 16.0,
            min_line_length: size * 16.0,
            min_polygon_area: (size * 16.0).powi(2),
            min_hole_area_world: (size / 256.0).powi(2),
        };
        if let Some(original) = &feature.min_size_source_geometry {
            let Some(unit) = unit_source.get_or_init(|| geometry_to_world(original, 0)) else {
                continue;
            };
            let world = scale_world(unit, z);
            if generalize_world_geometry_with_policy(policy, world)
                .geometry
                .is_none()
            {
                continue;
            }
        }
        // Poi.setMinPixelSizeBelowZoom(13, 80) uses an inclusive
        // ZoomFunction.maxZoom(13, 80) at the pinned profile revision.
        if z <= 13
            && feature.attrs.contains_key("__university_min_size")
            && let Some(original) = &feature.min_size_source_geometry
        {
            // WithGeometry.size() is zero for puntal sources. Geometry
            // generalization itself deliberately leaves points untouched.
            if matches!(
                original,
                geo_types::Geometry::Point(_) | geo_types::Geometry::MultiPoint(_)
            ) {
                continue;
            }
            let Some(unit) = unit_source.get_or_init(|| geometry_to_world(original, 0)) else {
                continue;
            };
            let world = scale_world(unit, z);
            let p = GeometryPolicy {
                min_line_length: 80.0 * 16.0,
                min_polygon_area: (80.0_f64 * 16.0).powi(2),
                ..policy
            };
            if generalize_world_geometry_with_policy(p, world)
                .geometry
                .is_none()
            {
                continue;
            }
        }
        let Some(unit) = unit_geometry.get_or_init(|| feature_unit_geometry(&feature)) else {
            continue;
        };
        let world = scale_world(unit, z);
        let simplify_policy = GeometryPolicy {
            min_line_length: 0.0,
            min_polygon_area: 0.0,
            min_hole_area_world: 0.0,
            ..policy
        };
        let mut world = match world {
            WorldGeometry::Polygon(groups) => {
                WorldGeometry::Polygon(super::simplify::polygons(groups, tolerance / 256.))
            }
            other => {
                let Some(world) =
                    generalize_world_geometry_with_policy(simplify_policy, other).geometry
                else {
                    continue;
                };
                world
            }
        };
        let num_points = match &world {
            WorldGeometry::Point(p) => p.len(),
            WorldGeometry::Line(lines) => lines.iter().map(Vec::len).sum(),
            WorldGeometry::Polygon(groups) => groups
                .iter()
                .map(|g| g.exterior.len() + g.holes.iter().map(Vec::len).sum::<usize>())
                .sum(),
        };
        // The upstream group extractor filters each simplified shell, not the
        // sum of several disconnected islands.
        if let WorldGeometry::Polygon(groups) = &mut world {
            groups.retain(|g| ring_area(&g.exterior) >= (size / 256.0).powi(2));
            for group in groups.iter_mut() {
                group
                    .holes
                    .retain(|h| ring_area(h) >= (size / 256.0).powi(2));
            }
            orient_like_planetiler(groups);
        } else if let WorldGeometry::Line(lines) = &mut world {
            lines.retain(|line| {
                line.windows(2)
                    .map(|p| (p[0].x - p[1].x).hypot(p[0].y - p[1].y))
                    .sum::<f64>()
                    >= size / 256.0
            });
        }
        let label_point = match &world {
            WorldGeometry::Point(points) if points.len() == 1 => Some(points[0]),
            _ => None,
        };
        let buffer = feature.buffer_pixels / 256.0;
        wrap_world_copies(
            &mut world,
            f64::from(1u32 << z),
            buffer + POINT_NEIGHBOR_BUFFER_EPS,
        );
        let Some((left, right, top, bottom)) = bounds(&world) else {
            continue;
        };
        let nw = lng_lat_to_world(region.bounds.west, region.bounds.north, z);
        let se = lng_lat_to_world(region.bounds.east, region.bounds.south, z);
        let max = (1_i32 << z) - 1;
        let slice_buffer = if matches!(&world, WorldGeometry::Point(_)) {
            buffer + POINT_NEIGHBOR_BUFFER_EPS
        } else {
            buffer
        };
        let minx = ((left - slice_buffer).floor() as i32).max(0);
        let maxx = ((right + slice_buffer).floor() as i32).min(max);
        // One run of columns, or two for bounds crossing the antimeridian;
        // the world copies above put a feature near ±180 in both.
        let columns = region
            .bounds
            .tile_columns(z)
            .flat_map(|(first, last)| first.max(minx)..=last.min(maxx));
        let miny = ((top - slice_buffer).floor() as i32)
            .max(nw.y.floor() as i32)
            .max(0);
        let maxy = ((bottom + slice_buffer).floor() as i32)
            .min(se.y.floor() as i32)
            .min(max);
        let mut attributes = attrs(&feature, z);
        if attributes.contains_key("_numpoints") {
            attributes.insert("_numpoints".into(), AttrValue::Int(num_points as i64));
        }
        let grid_size = feature
            .grid_size_by_zoom
            .iter()
            .find(|(upper, _)| z <= *upper)
            .map_or(feature.grid_size, |(_, v)| *v);
        let grid_limit = feature
            .grid_limits_by_zoom
            .iter()
            .find(|(upper, _)| z <= *upper)
            .map_or(feature.grid_limit, |(_, v)| *v);
        let effective_grid_size = if z <= feature.grid_max_zoom {
            grid_size
        } else {
            0.0
        };
        let grid_cell = label_point.and_then(|p| label_grid_cell(p, z, effective_grid_size));
        let group_bounds = match &world {
            WorldGeometry::Polygon(groups) => groups
                .iter()
                .map(|g| world_bounds_for_polygons(std::slice::from_ref(g)))
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        for x in columns {
            // Work shared by every tile of the column: X stripes of polygon
            // rings, or the line segments that can reach the column.
            let (min_x, max_x) = (x as f64 - slice_buffer, x as f64 + 1.0 + slice_buffer);
            let mut column = Vec::new();
            let mut segments = Vec::new();
            match &world {
                WorldGeometry::Polygon(groups) => {
                    // Every tile of the column would be skipped below.
                    if group_bounds
                        .iter()
                        .all(|b| b.is_some_and(|b| bounds_apart_x(b, min_x, max_x)))
                    {
                        continue;
                    }
                    column = polygon_column(groups, &group_bounds, min_x, max_x, x);
                }
                WorldGeometry::Line(lines) => {
                    segments = lines
                        .iter()
                        .map(|line| segments_in_x_range(line, min_x, max_x))
                        .collect::<Vec<_>>();
                    if segments.iter().all(Vec::is_empty) {
                        continue;
                    }
                }
                WorldGeometry::Point(_) => {}
            }
            for y in miny..=maxy {
                if !region.covers(z, x as u32, y as u32) {
                    continue;
                }
                let rect = WorldRect {
                    min_x: x as f64 - slice_buffer,
                    max_x: x as f64 + 1.0 + slice_buffer,
                    min_y: y as f64 - slice_buffer,
                    max_y: y as f64 + 1.0 + slice_buffer,
                };
                let geometry = match &world {
                    WorldGeometry::Point(points) => {
                        let points = points
                            .iter()
                            .filter(|p| {
                                p.x >= rect.min_x
                                    && p.x <= rect.max_x
                                    && p.y >= rect.min_y
                                    && p.y <= rect.max_y
                            })
                            .map(|p| tile_point(*p, x, y, 0))
                            .collect::<Vec<_>>();
                        (!points.is_empty()).then_some(TileGeometry::Point(points))
                    }
                    WorldGeometry::Line(lines) => {
                        let scale_exp = (14 - z).min(17);
                        let clipped = lines
                            .iter()
                            .zip(&segments)
                            .flat_map(|(line, segments)| {
                                clip_polyline_segments_to_rect(line, segments, rect, f64::from(x))
                            })
                            .collect::<Vec<_>>();
                        if clipped.is_empty() {
                            continue;
                        }
                        // Planetiler emits a sliced line even when every part
                        // collapses once rounded to the tile grid. Its empty
                        // geometry is dropped only when the tile is encoded,
                        // after line merging, where it still counts as a
                        // member of its group and can be the one whose ID the
                        // merged line keeps.
                        let paths = clipped
                            .into_iter()
                            .map(|line| {
                                let mut points = line
                                    .into_iter()
                                    // X is already local to the column.
                                    .map(|p| tile_point(p, 0, y, scale_exp))
                                    .collect::<Vec<_>>();
                                points.dedup();
                                points
                            })
                            .filter(|p| p.len() > 1)
                            .collect::<Vec<_>>();
                        Some(TileGeometry::Line { scale_exp, paths })
                    }
                    WorldGeometry::Polygon(groups) => {
                        if group_bounds
                            .iter()
                            .all(|b| b.is_some_and(|b| bounds_apart(b, rect)))
                        {
                            continue;
                        }
                        polygon_tile(groups, &column, rect, x, y)
                    }
                };
                if let Some(geometry) = geometry {
                    let coord = TileCoord::new(z, x as u32, y as u32).map_err(|e| e.to_string())?;
                    sink(Record {
                        tile_id: TileId::from(coord).value(),
                        zoom: z,
                        id: feature.id,
                        layer: Cow::Owned(feature.layer.clone()),
                        geometry,
                        attrs: attributes.clone(),
                        sort_key: feature.sort_key,
                        grid_size: if z <= feature.grid_max_zoom {
                            grid_size
                        } else {
                            0.0
                        },
                        grid_cell,
                        grid_limit,
                        rank: feature.rank,
                    })?;
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;
    // The tile-local column of `groups` for the column of `rect`.
    fn column_of(groups: &[WorldRingGroup], rect: WorldRect, x: i32) -> Vec<ColumnGroup> {
        let bounds = groups
            .iter()
            .map(|g| world_bounds_for_polygons(std::slice::from_ref(g)))
            .collect::<Vec<_>>();
        polygon_column(groups, &bounds, rect.min_x, rect.max_x, x)
    }
    fn clip_groups(
        groups: &[WorldRingGroup],
        rect: WorldRect,
        x: i32,
        y: i32,
    ) -> Option<TileGeometry> {
        clip_valid_groups(&column_of(groups, rect, x), rect, y).flatten()
    }
    #[test]
    fn valid_float_holes_share_hot_pixels_when_integer_snap_is_invalid() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-valid-hole-snap.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let points = |r: &serde_json::Value| {
                r.as_array()
                    .unwrap()
                    .iter()
                    .map(|p| {
                        WorldPoint::new(
                            p[0].as_f64().unwrap() / 4096.,
                            p[1].as_f64().unwrap() / 4096.,
                        )
                    })
                    .collect()
            };
            let group = WorldRingGroup {
                exterior: points(&case["exterior"]),
                holes: case["holes"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(points)
                    .collect(),
            };
            let TileGeometry::Polygon(actual) = clip_groups(
                &[group],
                WorldRect {
                    min_x: -1. / 64.,
                    max_x: 1. + 1. / 64.,
                    min_y: -1. / 64.,
                    max_y: 1. + 1. / 64.,
                },
                0,
                0,
            )
            .unwrap() else {
                panic!("expected polygon")
            };
            let expected = case["expected_rings_mvt"].as_array().unwrap();
            assert_eq!(actual.len(), expected.len(), "{}", case["name"]);
            let decode = |r: &serde_json::Value| {
                r.as_array()
                    .unwrap()
                    .iter()
                    .map(|p| {
                        TilePoint::new(p[0].as_f64().unwrap() as i32, p[1].as_f64().unwrap() as i32)
                    })
                    .collect::<Vec<_>>()
            };
            for (a, e) in actual.iter().zip(expected) {
                assert_eq!(
                    canonical_fixture_ring(&a.exterior),
                    canonical_fixture_ring(&decode(&e["exterior"])),
                    "{}",
                    case["name"]
                );
                let holes = e["holes"].as_array().unwrap();
                assert_eq!(a.holes.len(), holes.len());
                for (a, e) in a.holes.iter().zip(holes) {
                    assert_eq!(
                        canonical_fixture_ring(a),
                        canonical_fixture_ring(&decode(e))
                    );
                }
            }
        }
    }
    #[test]
    fn actual_relation_with_holes_matches_reference_after_full_render() {
        use geo_types::{Geometry, LineString, Polygon};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-landcover-relation-20659308.json"
        ))
        .unwrap();
        let members = fixture["source"]["members"].as_array().unwrap();
        let ring = |m: &serde_json::Value| {
            LineString::from(
                m["way"]["coordinates_lonlat"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
                    .collect::<Vec<_>>(),
            )
        };
        let source = super::super::types::Source {
            id: 206593083,
            osm_id: 20659308,
            kind: super::super::types::SourceKind::Relation,
            source_layer: "osm".into(),
            tags: serde_json::from_value(fixture["source"]["tags"].clone()).unwrap(),
            geometry: Geometry::Polygon(Polygon::new(
                ring(&members[0]),
                members[1..].iter().map(ring).collect(),
            )),
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::land::process(&source, &mut features);
        let feature = features
            .into_iter()
            .find(|f| f.layer == "landcover" && f.min_zoom <= 9 && f.max_zoom >= 9)
            .unwrap();
        let mut records = Vec::new();
        emit_with_record_sink(
            feature,
            &crate::CompileBounds::new(51., 49., 22., 18.).into(),
            9,
            14,
            &mut |r| {
                records.push(r);
                Ok(())
            },
        )
        .unwrap();
        let target = TileId::from(TileCoord::new(9, 284, 173).unwrap()).value();
        let record = records.iter().find(|r| r.tile_id == target).unwrap();
        let TileGeometry::Polygon(actual) = &record.geometry else {
            panic!("expected polygon");
        };
        let expected = fixture["java_stages"]["snapped_rings_mvt"]
            .as_array()
            .unwrap();
        assert_eq!(actual.len(), expected.len());
        let int_ring = |r: &serde_json::Value| {
            r.as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    TilePoint::new(p[0].as_f64().unwrap() as i32, p[1].as_f64().unwrap() as i32)
                })
                .collect::<Vec<_>>()
        };
        for (a, e) in actual.iter().zip(expected) {
            assert_eq!(
                canonical_fixture_ring(&a.exterior),
                canonical_fixture_ring(&int_ring(&e["exterior"]))
            );
            assert_eq!(a.holes.len(), e["holes"].as_array().unwrap().len());
        }
    }
    #[test]
    fn actual_relation_with_invalid_float_hole_matches_pinned_snap() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-landcover-relation-20659308.json"
        ))
        .unwrap();
        let decode = |r: &serde_json::Value| {
            r.as_array()
                .unwrap()
                .iter()
                .map(|p| WorldPoint::new(p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
                .collect::<Vec<_>>()
        };
        let groups = fixture["java_stages"]["float_rings_mvt"]
            .as_array()
            .unwrap()
            .iter()
            .map(|g| WorldRingGroup {
                exterior: decode(&g["exterior"]),
                holes: g["holes"].as_array().unwrap().iter().map(decode).collect(),
            })
            .collect();
        let TileGeometry::Polygon(actual) = snap_float_polygon_groups(groups).unwrap() else {
            panic!("expected polygon");
        };
        let expected = fixture["java_stages"]["snapped_rings_mvt"]
            .as_array()
            .unwrap();
        assert_eq!(actual.len(), expected.len());
        let int_ring = |r: &serde_json::Value| {
            decode(r)
                .iter()
                .map(|p| TilePoint::new(p.x as i32, p.y as i32))
                .collect::<Vec<_>>()
        };
        for (a, e) in actual.iter().zip(expected) {
            assert_eq!(
                canonical_fixture_ring(&a.exterior),
                canonical_fixture_ring(&int_ring(&e["exterior"]))
            );
            let holes = e["holes"].as_array().unwrap();
            assert_eq!(a.holes.len(), holes.len());
            for (a, e) in a.holes.iter().zip(holes) {
                assert_eq!(
                    canonical_fixture_ring(a),
                    canonical_fixture_ring(&int_ring(e))
                );
            }
        }
    }
    #[test]
    fn repaired_farmland_hole_matches_pinned_reference() {
        use geo_types::{Geometry, LineString, Polygon};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-landcover-way748863343.json"
        ))
        .unwrap();
        let points = fixture["coordinates_lonlat"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
            .collect::<Vec<_>>();
        let source = super::super::types::Source {
            id: 7488633432,
            osm_id: 748863343,
            kind: super::super::types::SourceKind::Way,
            source_layer: "osm".into(),
            tags: serde_json::from_value(fixture["tags"].clone()).unwrap(),
            geometry: Geometry::Polygon(Polygon::new(LineString::from(points), vec![])),
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::land::process(&source, &mut features);
        let feature = features
            .into_iter()
            .find(|f| f.layer == "landcover" && f.min_zoom <= 9 && f.max_zoom >= 9)
            .unwrap();
        let mut records = Vec::new();
        emit_with_record_sink(
            feature,
            &crate::CompileBounds::new(51., 49., 22., 18.).into(),
            9,
            14,
            &mut |r| {
                records.push(r);
                Ok(())
            },
        )
        .unwrap();
        let target = TileId::from(TileCoord::new(9, 284, 173).unwrap()).value();
        let record = records.iter().find(|r| r.tile_id == target).unwrap();
        let TileGeometry::Polygon(groups) = &record.geometry else {
            panic!("expected polygon")
        };
        let expected = fixture["expected_reference_rings_mvt"].as_array().unwrap();
        assert_eq!(groups.len(), expected.len());
        let decode = |r: &serde_json::Value| {
            r.as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    TilePoint::new(p[0].as_f64().unwrap() as i32, p[1].as_f64().unwrap() as i32)
                })
                .collect::<Vec<_>>()
        };
        for (actual, expected) in groups.iter().zip(expected) {
            assert_eq!(
                canonical_fixture_ring(&actual.exterior),
                canonical_fixture_ring(&decode(&expected["exterior"]))
            );
            let holes = expected["holes"].as_array().unwrap();
            assert_eq!(actual.holes.len(), holes.len());
            for (a, e) in actual.holes.iter().zip(holes) {
                assert_eq!(
                    canonical_fixture_ring(a),
                    canonical_fixture_ring(&decode(e))
                );
            }
        }
    }
    #[test]
    fn contained_hole_preserves_source_vertices_below_half_grid_ties() {
        let ring = |coords: &[(f64, f64)]| {
            coords
                .iter()
                .map(|&(x, y)| WorldPoint::new(x / 4096., y / 4096.))
                .collect::<Vec<_>>()
        };
        let group = WorldRingGroup {
            exterior: ring(&[
                (0.499_999_9, 100.),
                (4000., 100.),
                (4000., 4000.),
                (0.499_999_9, 4000.),
                (0.499_999_9, 100.),
            ]),
            holes: vec![ring(&[
                (1_000.499_999_9, 1000.),
                (1_000.499_999_9, 2000.),
                (2000., 2000.),
                (2000., 1000.),
                (1_000.499_999_9, 1000.),
            ])],
        };
        let rect = WorldRect {
            min_x: -1. / 64.,
            max_x: 1. + 1. / 64.,
            min_y: -1. / 64.,
            max_y: 1. + 1. / 64.,
        };
        assert!(clip_groups(&[group.clone(), group.clone()], rect, 0, 0).is_some());
        let TileGeometry::Polygon(clipped) = clip_groups(
            std::slice::from_ref(&group),
            WorldRect {
                max_x: 0.75,
                ..rect
            },
            0,
            0,
        )
        .unwrap() else {
            panic!("expected polygon")
        };
        assert_eq!(clipped[0].exterior.iter().map(|p| p.x).max(), Some(3072));
        assert_eq!(clipped[0].holes[0].iter().map(|p| p.x).min(), Some(1000));
        let result = clip_groups(&[group], rect, 0, 0).unwrap();
        let TileGeometry::Polygon(groups) = result else {
            panic!("expected polygon")
        };
        assert_eq!(groups[0].exterior.iter().map(|p| p.x).min(), Some(0));
        assert_eq!(groups[0].holes[0].iter().map(|p| p.x).min(), Some(1000));
        assert_eq!(groups[0].exterior.len(), 5);
        assert_eq!(groups[0].holes[0].len(), 5);
    }
    // Tiles whose rect is apart from every group's bounds are skipped by the
    // tile loop; the full cascade must return nothing for each of them.
    #[test]
    fn polygon_cascade_is_empty_for_tiles_apart_from_every_group() {
        let ring = |pts: &[(f64, f64)]| {
            pts.iter()
                .map(|&(x, y)| WorldPoint::new(x, y))
                .collect::<Vec<_>>()
        };
        let square = |x: f64, y: f64, s: f64| {
            ring(&[(x, y), (x + s, y), (x + s, y + s), (x, y + s), (x, y)])
        };
        let group = |exterior| WorldRingGroup {
            exterior,
            holes: vec![],
        };
        let cases = vec![
            vec![group(square(100.2, 200.3, 1.4))],
            // Exactly on tile edges, so bounds touch neighbouring rects.
            vec![group(square(100., 200., 2.))],
            // Bow tie (self-crossing shell).
            vec![group(ring(&[
                (100.1, 200.1),
                (101.9, 200.1),
                (100.1, 201.9),
                (101.9, 201.9),
                (100.1, 200.1),
            ]))],
            // Shell with a hole, and two separate islands.
            vec![WorldRingGroup {
                exterior: square(100.1, 200.1, 1.8),
                holes: vec![square(100.5, 200.5, 0.5)],
            }],
            vec![
                group(square(98.5, 200.2, 0.6)),
                group(square(103.2, 203.1, 0.4)),
            ],
            // Thin diagonal sliver.
            vec![group(ring(&[
                (98.0, 198.0),
                (104.0, 204.0),
                (104.0, 204.001),
                (98.0, 198.001),
                (98.0, 198.0),
            ]))],
        ];
        for groups in cases {
            let bounds = groups
                .iter()
                .map(|g| world_bounds_for_polygons(std::slice::from_ref(g)))
                .collect::<Vec<_>>();
            let (mut apart, mut reached) = (0, 0);
            for buffer in [0., 4. / 256., 80. / 256.] {
                for x in 94..=108 {
                    for y in 194..=208 {
                        let rect = WorldRect {
                            min_x: x as f64 - buffer,
                            max_x: x as f64 + 1. + buffer,
                            min_y: y as f64 - buffer,
                            max_y: y as f64 + 1. + buffer,
                        };
                        let geometry =
                            polygon_tile(&groups, &column_of(&groups, rect, x), rect, x, y);
                        if bounds
                            .iter()
                            .all(|b| b.is_some_and(|b| bounds_apart(b, rect)))
                        {
                            apart += 1;
                            assert!(geometry.is_none(), "tile {x},{y} buffer {buffer}");
                        } else if geometry.is_some() {
                            reached += 1;
                        }
                    }
                }
            }
            assert!(
                apart > 100 && reached > 0,
                "{apart} apart, {reached} reached"
            );
        }
    }
    // The previous per-tile implementation: the whole ring is X- then Y-stripe
    // clipped for every tile.
    fn old_clip_ring_stripes(ring: &[WorldPoint], rect: WorldRect, x: i32) -> Vec<WorldPoint> {
        let local = ring
            .iter()
            .map(|p| WorldPoint::new(p.x - x as f64, p.y))
            .collect::<Vec<_>>();
        let clipped = clip_stripe(&local, rect.min_x - x as f64, rect.max_x - x as f64, true);
        clip_stripe(&clipped, rect.min_y, rect.max_y, false)
    }
    fn old_clip_valid_groups(
        groups: &[WorldRingGroup],
        rect: WorldRect,
        x: i32,
        y: i32,
    ) -> Option<Option<TileGeometry>> {
        if groups.is_empty() || (groups.len() == 1 && groups[0].holes.is_empty()) {
            return None;
        }
        let clip = |ring: &[WorldPoint]| {
            old_clip_ring_stripes(ring, rect, x)
                .into_iter()
                .map(|p| WorldPoint::new(p.x, p.y - y as f64))
                .collect::<Vec<_>>()
        };
        let clipped = groups
            .iter()
            .map(|g| WorldRingGroup {
                exterior: clip(&g.exterior),
                holes: g
                    .holes
                    .iter()
                    .map(|h| clip(h))
                    .filter(|h| !h.is_empty())
                    .collect(),
            })
            .filter(|g| !g.exterior.is_empty())
            .collect::<Vec<_>>();
        if clipped.is_empty() {
            return Some(None);
        }
        let scale = |ring: Vec<WorldPoint>| {
            ring.into_iter()
                .map(|p| WorldPoint::new(p.x * 4096., p.y * 4096.))
                .collect()
        };
        snap_tile_slice(
            clipped
                .into_iter()
                .map(|g| WorldRingGroup {
                    exterior: scale(g.exterior),
                    holes: g.holes.into_iter().map(scale).collect(),
                })
                .collect(),
        )
    }
    // `clip_valid_shell` on the unclipped ring: the column stripe is applied
    // here per tile instead of being reused.
    fn old_clip_valid_shell(
        ring: &[WorldPoint],
        rect: WorldRect,
        x: i32,
        y: i32,
    ) -> Option<Option<TileGeometry>> {
        let column = column_ring(ring, rect.min_x, rect.max_x, x);
        let reference = old_clip_ring_stripes(ring, rect, x);
        assert_eq!(column_ring_y(&column, rect), reference);
        clip_valid_shell(&column, rect, y)
    }
    fn old_polygon_tile(
        groups: &[WorldRingGroup],
        prepared: &std::cell::OnceCell<PreparedPolygonGroups>,
        rect: WorldRect,
        x: i32,
        y: i32,
    ) -> Option<TileGeometry> {
        let simple_shell =
            (groups.len() == 1 && groups[0].holes.is_empty()).then(|| &groups[0].exterior);
        // A slice the JTS repair decided, empty included, is final.
        if let Some(decided) = old_clip_valid_groups(groups, rect, x, y)
            .or_else(|| simple_shell.and_then(|ring| old_clip_valid_shell(ring, rect, x, y)))
        {
            return decided;
        }
        repair_contained_crossing_polygon(groups, rect, x, y)
            .and_then(|repaired| tile_polygons(repaired, x, y))
            .or_else(|| {
                let prepared = prepared.get_or_init(|| prepare_polygon_groups(groups));
                tile_polygons(clip_prepared_polygon_groups_to_rect(prepared, rect), x, y)
            })
    }
    // Deterministic LCG so the cases are reproducible.
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self) -> f64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 11) as f64 / (1u64 << 53) as f64
        }
        fn below(&mut self, n: usize) -> usize {
            (self.next() * n as f64) as usize % n
        }
    }
    // Memoizing the X stripe per tile column must reproduce the per-tile result
    // for every tile, including tiles the loop would skip as apart.
    #[test]
    fn column_memoized_polygon_tiles_match_per_tile_clipping() {
        let mut rng = Lcg(0x5eed);
        let (cx, cy) = (100.0, 200.0);
        let mut cases: Vec<Vec<WorldRingGroup>> = Vec::new();
        let ring = |pts: &[(f64, f64)]| {
            pts.iter()
                .map(|&(x, y)| WorldPoint::new(x, y))
                .collect::<Vec<_>>()
        };
        let rect_ring = |x0: f64, y0: f64, x1: f64, y1: f64| {
            ring(&[(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)])
        };
        // Tile-boundary aligned shells, with holes touching the boundaries too.
        cases.push(vec![WorldRingGroup {
            exterior: rect_ring(cx, cy, cx + 3., cy + 3.),
            holes: vec![rect_ring(cx + 1., cy + 1., cx + 2., cy + 2.)],
        }]);
        cases.push(vec![WorldRingGroup {
            exterior: rect_ring(cx + 0.25, cy + 0.25, cx + 3.25, cy + 2.75),
            holes: vec![
                rect_ring(cx + 1., cy + 0.5, cx + 1.5, cy + 2.5),
                rect_ring(cx + 2., cy + 0.5, cx + 2.75, cy + 1.),
            ],
        }]);
        // Concave L shape and a spike crossing several columns.
        cases.push(vec![WorldRingGroup {
            exterior: ring(&[
                (cx, cy),
                (cx + 4., cy),
                (cx + 4., cy + 0.5),
                (cx + 0.5, cy + 0.5),
                (cx + 0.5, cy + 4.),
                (cx, cy + 4.),
                (cx, cy),
            ]),
            holes: vec![],
        }]);
        cases.push(vec![WorldRingGroup {
            exterior: ring(&[
                (cx + 0.1, cy + 0.1),
                (cx + 4.7, cy + 2.05),
                (cx + 0.1, cy + 0.2),
                (cx + 0.1, cy + 0.1),
            ]),
            holes: vec![],
        }]);
        for case in 0..60 {
            let groups = 1 + rng.below(3);
            let mut out = Vec::new();
            for _ in 0..groups {
                let (mx, my) = (cx + rng.next() * 4., cy + rng.next() * 4.);
                let n = 3 + rng.below(10);
                let radius = 0.2 + rng.next() * 2.;
                let mut angles = (0..n)
                    .map(|_| rng.next() * std::f64::consts::TAU)
                    .collect::<Vec<_>>();
                // Mostly star-shaped (simple) rings; every third case keeps the
                // random order so shells can self-cross.
                if case % 3 != 0 {
                    angles.sort_by(f64::total_cmp);
                }
                let mut pts = angles
                    .iter()
                    .map(|a| {
                        let r = radius * (0.3 + rng.next());
                        let (mut x, mut y) = (mx + r * a.cos(), my + r * a.sin());
                        // Pull some vertices onto the quarter-tile and buffer grid.
                        if rng.below(3) == 0 {
                            x = (x * 4.).round() / 4.;
                        }
                        if rng.below(3) == 0 {
                            y = (y * 4.).round() / 4. + 4. / 256.;
                        }
                        WorldPoint::new(x, y)
                    })
                    .collect::<Vec<_>>();
                pts.push(pts[0]);
                if rng.below(2) == 0 {
                    pts.reverse();
                }
                let mut holes = Vec::new();
                if rng.below(3) == 0 {
                    let h = radius * 0.15;
                    holes.push(rect_ring(mx - h, my - h, mx + h, my + h));
                }
                out.push(WorldRingGroup {
                    exterior: pts,
                    holes,
                });
            }
            cases.push(out);
        }
        let (mut reached, mut skipped_columns) = (0, 0);
        for groups in &cases {
            let bounds = groups
                .iter()
                .map(|g| world_bounds_for_polygons(std::slice::from_ref(g)))
                .collect::<Vec<_>>();
            for buffer in [0., 4. / 256., 80. / 256.] {
                let prepared_old = std::cell::OnceCell::new();
                for x in (cx as i32 - 2)..=(cx as i32 + 7) {
                    let (min_x, max_x) = (x as f64 - buffer, x as f64 + 1. + buffer);
                    let column_apart = bounds
                        .iter()
                        .all(|b| b.is_some_and(|b| bounds_apart_x(b, min_x, max_x)));
                    let column = polygon_column(groups, &bounds, min_x, max_x, x);
                    for y in (cy as i32 - 2)..=(cy as i32 + 7) {
                        let rect = WorldRect {
                            min_x,
                            max_x,
                            min_y: y as f64 - buffer,
                            max_y: y as f64 + 1. + buffer,
                        };
                        let expected = old_polygon_tile(groups, &prepared_old, rect, x, y);
                        if column_apart {
                            assert!(expected.is_none(), "column {x}, buffer {buffer}");
                            skipped_columns += 1;
                            continue;
                        }
                        let actual = polygon_tile(groups, &column, rect, x, y);
                        assert_eq!(actual, expected, "tile {x},{y} buffer {buffer}: {groups:?}");
                        reached += usize::from(actual.is_some());
                    }
                }
            }
        }
        assert!(
            reached > 500 && skipped_columns > 500,
            "{reached} {skipped_columns}"
        );
    }
    // A polygon whose every ring lies beside the tile has no tile geometry, and
    // skipping the fallbacks must not change that; a hole reaching into the tile
    // (invalid input) or a ring touching the tile edge must still take them.
    #[test]
    fn polygon_beside_tile_matches_full_cascade() {
        let ring = |x0: f64, y0: f64, x1: f64, y1: f64| {
            [(x0, y0), (x1, y0), (x1, y1), (x0, y1), (x0, y0)]
                .map(|(x, y)| WorldPoint::new(x, y))
                .to_vec()
        };
        let group = |exterior, holes| WorldRingGroup { exterior, holes };
        let (x, y, buffer) = (10, 20, 4. / 256.);
        let rect = WorldRect {
            min_x: x as f64 - buffer,
            max_x: x as f64 + 1. + buffer,
            min_y: y as f64 - buffer,
            max_y: y as f64 + 1. + buffer,
        };
        let cases = [
            (vec![group(ring(30., 30., 31., 31.), vec![])], true),
            (vec![group(ring(30., 20., 31., 21.), vec![])], true),
            (vec![group(ring(10.2, 18., 10.6, 19.9), vec![])], true),
            (
                vec![
                    group(ring(30., 30., 31., 31.), vec![ring(30.2, 30.2, 30.5, 30.5)]),
                    group(ring(5., 5., 6., 6.), vec![]),
                ],
                true,
            ),
            (
                vec![group(
                    ring(30., 30., 31., 31.),
                    vec![ring(10.2, 20.2, 10.6, 20.6)],
                )],
                false,
            ),
            (
                vec![group(ring(10.2, 18., 10.6, rect.min_y), vec![])],
                false,
            ),
            (vec![group(ring(9., 18., 10.6, 19.99), vec![])], false),
            (vec![], true),
        ];
        for (groups, apart) in cases {
            let column = column_of(&groups, rect, x);
            let actual = polygon_tile(&groups, &column, rect, x, y);
            let expected = old_polygon_tile(&groups, &std::cell::OnceCell::new(), rect, x, y);
            assert_eq!(actual, expected, "{groups:?}");
            assert_eq!(
                column.iter().all(|g| column_ring_apart(&g.exterior, rect)
                    && g.holes.iter().all(|h| column_ring_apart(h, rect))),
                apart,
                "{groups:?}"
            );
            if apart {
                assert!(actual.is_none(), "{groups:?}");
            }
        }
    }
    #[test]
    fn contained_shell_preserves_source_below_half_grid_tie() {
        let points = [
            (0.499_999_9, 100.),
            (200., 100.),
            (200., 300.),
            (0.499_999_9, 300.),
            (0.499_999_9, 100.),
        ]
        .map(|(x, y)| WorldPoint::new(x / 4096., y / 4096.));
        let rect = WorldRect {
            min_x: -1. / 64.,
            max_x: 1. + 1. / 64.,
            min_y: -1. / 64.,
            max_y: 1. + 1. / 64.,
        };
        let column = column_ring(&points, rect.min_x, rect.max_x, 0);
        let geometry = clip_valid_shell(&column, rect, 0).flatten().unwrap();
        let TileGeometry::Polygon(groups) = geometry else {
            panic!("expected polygon")
        };
        assert_eq!(
            canonical_fixture_ring(&groups[0].exterior),
            canonical_fixture_ring(
                &[(0, 100), (200, 100), (200, 300), (0, 300), (0, 100)]
                    .map(|(x, y)| TilePoint::new(x, y))
            )
        );
    }
    fn canonical_fixture_ring(points: &[TilePoint]) -> Vec<TilePoint> {
        let mut ring = points.to_vec();
        if ring.first() == ring.last() {
            ring.pop();
        }
        let mut candidates = Vec::new();
        for _ in 0..2 {
            for start in 0..ring.len() {
                candidates.push(
                    ring[start..]
                        .iter()
                        .chain(&ring[..start])
                        .copied()
                        .collect::<Vec<_>>(),
                );
            }
            ring.reverse();
        }
        candidates.into_iter().min().unwrap_or_default()
    }
    #[test]
    fn natural_earth_ocean_matches_pinned_polar_and_world_edge_render() {
        let fixture: serde_json::Value =
            serde_json::from_str(include_str!("../../tests/fixtures/omt-ne110-ocean.json"))
                .unwrap();
        let geometry: geojson::Geometry =
            serde_json::from_value(fixture["geometry"].clone()).unwrap();
        let source = super::super::types::Source {
            id: 3319220,
            osm_id: 331922,
            kind: super::super::types::SourceKind::Supplement,
            source_layer: "ne_110m_ocean".into(),
            tags: BTreeMap::new(),
            geometry: geometry.try_into().unwrap(),
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::water::State::default().process(&source, &mut features);
        assert_eq!(features.len(), 1);
        let mut records = Vec::new();
        emit_with_record_sink(
            features.remove(0),
            &crate::CompileBounds::new(50.53221, 49.02835, 21.43328, 18.87522).into(),
            0,
            14,
            &mut |r| {
                records.push(r);
                Ok(())
            },
        )
        .unwrap();
        if let Ok(path) = std::env::var("OMT_OCEAN_CAPTURE_OUTPUT") {
            std::fs::write(path, serde_json::to_vec(&records).unwrap()).unwrap();
        }
        let canonical = |groups: &[TileRingGroup]| {
            let mut polygons = groups
                .iter()
                .map(|g| {
                    let mut holes = g
                        .holes
                        .iter()
                        .map(|h| canonical_fixture_ring(h))
                        .collect::<Vec<_>>();
                    holes.sort();
                    (canonical_fixture_ring(&g.exterior), holes)
                })
                .collect::<Vec<_>>();
            polygons.sort();
            polygons
        };
        for case in fixture["reference_cases"].as_array().unwrap() {
            let tile = case["tile"].as_array().unwrap();
            let id = TileId::from(
                TileCoord::new(
                    tile[0].as_u64().unwrap() as u8,
                    tile[1].as_u64().unwrap() as u32,
                    tile[2].as_u64().unwrap() as u32,
                )
                .unwrap(),
            )
            .value();
            let record = records.iter().find(|r| r.tile_id == id).unwrap();
            let TileGeometry::Polygon(actual) = &record.geometry else {
                panic!("expected polygon")
            };
            let ring = |r: &serde_json::Value| {
                r.as_array()
                    .unwrap()
                    .iter()
                    .map(|p| {
                        TilePoint::new(p[0].as_f64().unwrap() as i32, p[1].as_f64().unwrap() as i32)
                    })
                    .collect()
            };
            let expected = case["geometry"]["coordinates"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    let p = p.as_array().unwrap();
                    TileRingGroup {
                        exterior: ring(&p[0]),
                        holes: p[1..].iter().map(ring).collect(),
                    }
                })
                .collect::<Vec<_>>();
            assert!(
                canonical(actual) == canonical(&expected),
                "ocean mismatch at {}: {} native vs {} reference components",
                case["tile"],
                actual.len(),
                expected.len()
            );
        }
    }
    #[test]
    fn actual_disconnected_hole_contacts_match_pinned_snap() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-disconnected-hole-relations.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let decode = |r: &serde_json::Value| {
                r.as_array()
                    .unwrap()
                    .iter()
                    .map(|p| WorldPoint::new(p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
                    .collect::<Vec<_>>()
            };
            let groups = case["float_rings_mvt"]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| WorldRingGroup {
                    exterior: decode(&g["exterior"]),
                    holes: g["holes"].as_array().unwrap().iter().map(decode).collect(),
                })
                .collect();
            let TileGeometry::Polygon(actual) = snap_float_polygon_groups(groups)
                .unwrap_or_else(|| panic!("repair failed for {}", case["relation_id"]))
            else {
                panic!("polygon expected")
            };
            let canonical = |groups: &[TileRingGroup]| {
                let mut polygons = groups
                    .iter()
                    .map(|g| {
                        let mut holes = g
                            .holes
                            .iter()
                            .map(|h| canonical_fixture_ring(h))
                            .collect::<Vec<_>>();
                        holes.sort();
                        (canonical_fixture_ring(&g.exterior), holes)
                    })
                    .collect::<Vec<_>>();
                polygons.sort();
                polygons
            };
            let integer_ring = |r: &serde_json::Value| {
                decode(r)
                    .iter()
                    .map(|p| TilePoint::new(p.x as i32, p.y as i32))
                    .collect()
            };
            let expected = case["snapped_rings_mvt"]
                .as_array()
                .unwrap()
                .iter()
                .map(|g| TileRingGroup {
                    exterior: integer_ring(&g["exterior"]),
                    holes: g["holes"]
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(integer_ring)
                        .collect(),
                })
                .collect::<Vec<_>>();
            assert_eq!(
                canonical(&actual),
                canonical(&expected),
                "relation {}",
                case["relation_id"]
            );
        }
    }
    #[test]
    fn actual_contained_farmland_matches_reference_snap_repair() {
        use geo_types::{Geometry, LineString, Polygon};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-contained-farmland-753058966.json"
        ))
        .unwrap();
        let points = fixture["coordinates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
            .collect::<Vec<_>>();
        let source = super::super::types::Source {
            id: 7530589662,
            osm_id: 753058966,
            kind: super::super::types::SourceKind::Way,
            source_layer: "osm".into(),
            tags: BTreeMap::from([("landuse".into(), "farmland".into())]),
            geometry: Geometry::Polygon(Polygon::new(LineString::from(points), vec![])),
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::land::process(&source, &mut features);
        let feature = features
            .into_iter()
            .find(|f| f.layer == "landcover" && f.min_zoom <= 9 && f.max_zoom >= 9)
            .unwrap();
        let tile = TileId::from(TileCoord::new(9, 284, 173).unwrap()).value();
        let mut records = Vec::new();
        emit_with_record_sink(
            feature,
            &crate::CompileBounds::new(51., 49., 22., 18.).into(),
            9,
            14,
            &mut |record| {
                records.push(record);
                Ok(())
            },
        )
        .unwrap();
        let record = records.iter().find(|r| r.tile_id == tile).unwrap();
        let TileGeometry::Polygon(groups) = &record.geometry else {
            panic!("expected polygon")
        };
        assert_eq!(groups.len(), 1);
        let expected = fixture["expected_ring"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| TilePoint::new(p[0].as_i64().unwrap() as i32, p[1].as_i64().unwrap() as i32))
            .collect::<Vec<_>>();
        assert_eq!(
            canonical_fixture_ring(&groups[0].exterior),
            canonical_fixture_ring(&expected)
        );
    }

    #[test]
    fn actual_farmland_stripe_preserves_reference_edge_vertex() {
        use geo_types::{Geometry, LineString, Polygon};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-farmland-stripe-780650215.json"
        ))
        .unwrap();
        let points = fixture["coordinates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
            .collect::<Vec<_>>();
        let source = super::super::types::Source {
            id: 7806502152,
            osm_id: 780650215,
            kind: super::super::types::SourceKind::Way,
            source_layer: "osm".into(),
            tags: BTreeMap::from([("landuse".into(), "farmland".into())]),
            geometry: Geometry::Polygon(Polygon::new(LineString::from(points), vec![])),
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::land::process(&source, &mut features);
        let feature = features
            .into_iter()
            .find(|f| f.layer == "landcover" && f.min_zoom <= 9 && f.max_zoom >= 9)
            .unwrap();
        let tile = TileId::from(TileCoord::new(9, 284, 173).unwrap()).value();
        let mut records = Vec::new();
        emit_with_record_sink(
            feature,
            &crate::CompileBounds::new(51., 49., 22., 18.).into(),
            9,
            14,
            &mut |record| {
                records.push(record);
                Ok(())
            },
        )
        .unwrap();
        let record = records.iter().find(|r| r.tile_id == tile).unwrap();
        let TileGeometry::Polygon(groups) = &record.geometry else {
            panic!("expected polygon")
        };
        assert_eq!(groups.len(), 1);
        let expected = fixture["expected_ring"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| TilePoint::new(p[0].as_i64().unwrap() as i32, p[1].as_i64().unwrap() as i32))
            .collect::<Vec<_>>();
        assert_eq!(
            canonical_fixture_ring(&groups[0].exterior),
            canonical_fixture_ring(&expected)
        );
    }

    #[test]
    fn actual_farmland_second_stripe_preserves_reference_edge_vertex() {
        use geo_types::{Geometry, LineString, Polygon};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-farmland-stripe-1375094601.json"
        ))
        .unwrap();
        let points = fixture["coordinates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
            .collect::<Vec<_>>();
        let source = super::super::types::Source {
            id: 13750946012,
            osm_id: 1375094601,
            kind: super::super::types::SourceKind::Way,
            source_layer: "osm".into(),
            tags: BTreeMap::from([("landuse".into(), "farmland".into())]),
            geometry: Geometry::Polygon(Polygon::new(LineString::from(points), vec![])),
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::land::process(&source, &mut features);
        let feature = features
            .into_iter()
            .find(|f| f.layer == "landcover" && f.min_zoom <= 10 && f.max_zoom >= 10)
            .unwrap();
        let tile = TileId::from(TileCoord::new(10, 571, 346).unwrap()).value();
        let mut records = Vec::new();
        emit_with_record_sink(
            feature,
            &crate::CompileBounds::new(51., 49., 22., 18.).into(),
            10,
            14,
            &mut |record| {
                records.push(record);
                Ok(())
            },
        )
        .unwrap();
        let record = records.iter().find(|r| r.tile_id == tile).unwrap();
        let TileGeometry::Polygon(groups) = &record.geometry else {
            panic!("expected polygon")
        };
        assert_eq!(groups.len(), 1);
        let expected = fixture["expected_ring"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| TilePoint::new(p[0].as_i64().unwrap() as i32, p[1].as_i64().unwrap() as i32))
            .collect::<Vec<_>>();
        assert_eq!(
            canonical_fixture_ring(&groups[0].exterior),
            canonical_fixture_ring(&expected)
        );
    }

    #[test]
    fn actual_water_retains_vertex_that_becomes_collinear_after_snapping() {
        use geo_types::{Geometry, LineString, Polygon};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-water-collinear-833626865.json"
        ))
        .unwrap();
        let points = fixture["coordinates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
            .collect::<Vec<_>>();
        let source = super::super::types::Source {
            id: 8336268652,
            osm_id: 833626865,
            kind: super::super::types::SourceKind::Way,
            source_layer: "osm".into(),
            tags: BTreeMap::from([("natural".into(), "water".into())]),
            geometry: Geometry::Polygon(Polygon::new(LineString::from(points), vec![])),
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::water::State::default().process(&source, &mut features);
        let feature = features
            .into_iter()
            .find(|f| f.layer == "water" && f.min_zoom == 12)
            .unwrap();
        let tile = TileId::from(TileCoord::new(12, 2272, 1389).unwrap()).value();
        let mut records = Vec::new();
        emit_with_record_sink(
            feature,
            &crate::CompileBounds::new(51., 49., 21., 18.).into(),
            12,
            14,
            &mut |record| {
                records.push(record);
                Ok(())
            },
        )
        .unwrap();
        let record = records.iter().find(|r| r.tile_id == tile).unwrap();
        let TileGeometry::Polygon(groups) = &record.geometry else {
            panic!("expected polygon")
        };
        assert_eq!(groups.len(), 1);
        let expected = fixture["expected_ring"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| TilePoint::new(p[0].as_i64().unwrap() as i32, p[1].as_i64().unwrap() as i32))
            .collect::<Vec<_>>();
        assert_eq!(
            canonical_fixture_ring(&groups[0].exterior),
            canonical_fixture_ring(&expected)
        );
    }

    #[test]
    fn pustynia_outline_matches_reference_71_vertex_quantized_oracle() {
        let geometry = geo_types::Geometry::Polygon(geo_types::Polygon::new(
            geo_types::LineString::from(vec![
                (19.4849711, 50.3476841),
                (19.4872463, 50.3481032),
                (19.4876647, 50.3472953),
                (19.4915271, 50.3473638),
                (19.4932089, 50.347693),
                (19.4947266, 50.3479826),
                (19.4950062, 50.3480966),
                (19.4951735, 50.3486001),
                (19.4957436, 50.3485159),
                (19.496039, 50.3483844),
                (19.4967335, 50.348503),
                (19.4974456, 50.3483732),
                (19.4981344, 50.3480238),
                (19.4991216, 50.3475898),
                (19.4997053, 50.3474806),
                (19.4997553, 50.3472762),
                (19.4996349, 50.3471363),
                (19.4997253, 50.347017),
                (19.5001391, 50.3468961),
                (19.5005057, 50.3468304),
                (19.5014169, 50.346523),
                (19.5018933, 50.3465936),
                (19.5024022, 50.346511),
                (19.5035189, 50.3464787),
                (19.5035168, 50.3461052),
                (19.5037777, 50.3460737),
                (19.5047815, 50.3460571),
                (19.5063243, 50.3463179),
                (19.5065156, 50.3461499),
                (19.5073323, 50.3460513),
                (19.5083502, 50.3462201),
                (19.5084445, 50.3460703),
                (19.5092895, 50.345864),
                (19.5100748, 50.34576),
                (19.5109761, 50.3456161),
                (19.5113075, 50.3454426),
                (19.5117946, 50.3453996),
                (19.512207, 50.3453053),
                (19.5128134, 50.3450564),
                (19.5127501, 50.3449668),
                (19.5131377, 50.3448652),
                (19.5132946, 50.3447212),
                (19.5143109, 50.3446531),
                (19.5146596, 50.3444913),
                (19.5148393, 50.3445074),
                (19.5150501, 50.3445388),
                (19.5153733, 50.3447759),
                (19.5161653, 50.345121),
                (19.5165492, 50.3450226),
                (19.516817, 50.345233),
                (19.5179583, 50.3454077),
                (19.5186875, 50.3454643),
                (19.5196977, 50.3459192),
                (19.5205549, 50.3452941),
                (19.5210933, 50.3448038),
                (19.5232586, 50.3452577),
                (19.5251751, 50.3455322),
                (19.5270117, 50.3457669),
                (19.5282564, 50.3461071),
                (19.5283336, 50.3465133),
                (19.5288416, 50.3467093),
                (19.5295317, 50.3466099),
                (19.5301786, 50.3464383),
                (19.5305652, 50.3465112),
                (19.5308382, 50.3467545),
                (19.5313266, 50.3469502),
                (19.5318895, 50.3469591),
                (19.5319198, 50.3469423),
                (19.5321375, 50.3468223),
                (19.5322288, 50.34666),
                (19.5329969, 50.3462007),
                (19.5336184, 50.3459258),
                (19.5338545, 50.3458027),
                (19.5350959, 50.3458448),
                (19.5362562, 50.345814),
                (19.5369204, 50.345797),
                (19.5381904, 50.3457295),
                (19.5388766, 50.3453797),
                (19.5387257, 50.3452247),
                (19.540208, 50.3448872),
                (19.5424003, 50.3459437),
                (19.5439278, 50.346742),
                (19.5450381, 50.3470147),
                (19.5458897, 50.3475967),
                (19.5467326, 50.3479637),
                (19.548135, 50.3482083),
                (19.5487061, 50.3478688),
                (19.5500775, 50.3480383),
                (19.5512094, 50.3483533),
                (19.5516045, 50.3484195),
                (19.5517664, 50.3483528),
                (19.5516131, 50.3475053),
                (19.5507137, 50.3474598),
                (19.5500925, 50.3473709),
                (19.5499288, 50.3470139),
                (19.5498754, 50.3457743),
                (19.5505686, 50.3448227),
                (19.5501232, 50.3447309),
                (19.5504827, 50.3444433),
                (19.5498616, 50.3438922),
                (19.5487141, 50.3434843),
                (19.548665, 50.3425776),
                (19.5486628, 50.3425368),
                (19.5469296, 50.3427387),
                (19.5461368, 50.3426228),
                (19.5458179, 50.3428031),
                (19.5444696, 50.3429299),
                (19.5428337, 50.342437),
                (19.5420494, 50.3420717),
                (19.5422853, 50.3413116),
                (19.5426938, 50.3404079),
                (19.5429222, 50.3391725),
                (19.5430581, 50.33842),
                (19.5426603, 50.3375575),
                (19.5418985, 50.3375729),
                (19.540866, 50.3365434),
                (19.5419295, 50.335526),
                (19.541714, 50.3347845),
                (19.5407118, 50.3343745),
                (19.5400834, 50.3337068),
                (19.5421519, 50.3334851),
                (19.5414626, 50.3326778),
                (19.5400471, 50.3319932),
                (19.5387496, 50.3322941),
                (19.5380059, 50.3324235),
                (19.5364423, 50.3326956),
                (19.534225, 50.3331008),
                (19.5339479, 50.3331514),
                (19.5327021, 50.3333691),
                (19.5326458, 50.3337782),
                (19.5325811, 50.3342455),
                (19.5326998, 50.3355846),
                (19.5321317, 50.3357897),
                (19.5309349, 50.3359769),
                (19.5294251, 50.3355596),
                (19.5278594, 50.3353635),
                (19.5273028, 50.3349601),
                (19.5264619, 50.3345807),
                (19.5253687, 50.3345957),
                (19.5236878, 50.3342688),
                (19.5227156, 50.3340468),
                (19.5214783, 50.3336319),
                (19.5203735, 50.333518),
                (19.5197025, 50.3335922),
                (19.518922, 50.3336785),
                (19.517649, 50.3333278),
                (19.5153994, 50.333608),
                (19.5153181, 50.3336182),
                (19.5143464, 50.3349668),
                (19.5131432, 50.3355762),
                (19.5121262, 50.3360686),
                (19.5116689, 50.3362837),
                (19.5114947, 50.3363656),
                (19.5108276, 50.336629),
                (19.5091139, 50.3370351),
                (19.5076098, 50.337495),
                (19.5064343, 50.3376834),
                (19.5057413, 50.3377946),
                (19.5042029, 50.3378449),
                (19.5021733, 50.3382881),
                (19.5008191, 50.3386714),
                (19.5005758, 50.3387403),
                (19.4987645, 50.3390263),
                (19.4965305, 50.3387756),
                (19.4950251, 50.3385787),
                (19.4944925, 50.3376073),
                (19.4933042, 50.3371393),
                (19.4925666, 50.3368486),
                (19.49072, 50.3362141),
                (19.4885002, 50.3363104),
                (19.4862859, 50.3362429),
                (19.4852468, 50.335528),
                (19.4834278, 50.3347715),
                (19.4833586, 50.3347428),
                (19.4814009, 50.3352816),
                (19.4849711, 50.3476841),
            ]),
            vec![],
        ));
        let source = super::super::types::Source {
            id: 4237118102,
            osm_id: 423711810,
            kind: super::super::types::SourceKind::Way,
            source_layer: String::new(),
            tags: BTreeMap::new(),
            geometry: geometry.clone(),
            relations: vec![],
        };
        for (quantized, expected) in [
            (
                false,
                vec![
                    (2907, 3682),
                    (2895, 3677),
                    (2916, 3564),
                    (2929, 3560),
                    (2932, 3567),
                    (2954, 3567),
                    (2973, 3561),
                    (2976, 3555),
                    (2985, 3556),
                    (3002, 3566),
                    (3002, 3570),
                    (3012, 3574),
                    (3024, 3575),
                    (3026, 3578),
                    (3052, 3577),
                    (3089, 3593),
                    (3098, 3587),
                    (3113, 3584),
                    (3118, 3580),
                    (3127, 3590),
                    (3161, 3581),
                    (3168, 3578),
                    (3172, 3573),
                    (3180, 3575),
                    (3190, 3570),
                    (3201, 3581),
                    (3226, 3582),
                    (3229, 3586),
                    (3238, 3589),
                    (3276, 3561),
                    (3284, 3559),
                    (3287, 3562),
                    (3305, 3558),
                    (3304, 3565),
                    (3296, 3567),
                    (3294, 3581),
                    (3298, 3593),
                    (3288, 3602),
                    (3287, 3611),
                    (3263, 3607),
                    (3249, 3615),
                    (3255, 3648),
                    (3252, 3656),
                    (3248, 3656),
                    (3242, 3665),
                    (3248, 3675),
                    (3247, 3681),
                    (3241, 3685),
                    (3237, 3691),
                    (3249, 3693),
                    (3245, 3701),
                    (3237, 3707),
                    (3194, 3694),
                    (3194, 3674),
                    (3184, 3671),
                    (3166, 3676),
                    (3158, 3683),
                    (3152, 3683),
                    (3129, 3692),
                    (3114, 3692),
                    (3107, 3695),
                    (3093, 3692),
                    (3087, 3680),
                    (3067, 3665),
                    (3048, 3657),
                    (3028, 3654),
                    (2997, 3643),
                    (2975, 3647),
                    (2972, 3656),
                    (2950, 3668),
                    (2924, 3668),
                ],
            ),
            (
                true,
                vec![
                    (2916, 3564),
                    (2929, 3560),
                    (2932, 3567),
                    (2954, 3567),
                    (2973, 3561),
                    (2976, 3555),
                    (2985, 3556),
                    (3002, 3566),
                    (3002, 3570),
                    (3012, 3574),
                    (3024, 3575),
                    (3026, 3578),
                    (3052, 3577),
                    (3089, 3593),
                    (3098, 3587),
                    (3113, 3584),
                    (3118, 3580),
                    (3127, 3590),
                    (3161, 3581),
                    (3168, 3578),
                    (3172, 3573),
                    (3180, 3575),
                    (3190, 3570),
                    (3201, 3581),
                    (3226, 3582),
                    (3229, 3586),
                    (3238, 3589),
                    (3276, 3561),
                    (3284, 3559),
                    (3287, 3562),
                    (3305, 3558),
                    (3304, 3565),
                    (3296, 3567),
                    (3294, 3581),
                    (3298, 3593),
                    (3288, 3602),
                    (3287, 3611),
                    (3263, 3607),
                    (3249, 3615),
                    (3255, 3648),
                    (3252, 3656),
                    (3248, 3656),
                    (3242, 3665),
                    (3248, 3675),
                    (3247, 3681),
                    (3237, 3691),
                    (3249, 3693),
                    (3245, 3701),
                    (3237, 3707),
                    (3194, 3694),
                    (3194, 3674),
                    (3184, 3671),
                    (3166, 3676),
                    (3158, 3683),
                    (3152, 3683),
                    (3129, 3692),
                    (3114, 3692),
                    (3107, 3695),
                    (3093, 3692),
                    (3087, 3680),
                    (3067, 3665),
                    (3048, 3657),
                    (3028, 3654),
                    (2997, 3643),
                    (2975, 3647),
                    (2972, 3656),
                    (2950, 3668),
                    (2924, 3668),
                    (2907, 3682),
                    (2895, 3677),
                ],
            ),
        ] {
            let mut f = Feature::new(&source, "park", geometry.clone(), 9, 9);
            f.min_size = 2.;
            f.osm_outline_projection = quantized;
            let mut records = vec![];
            emit_with_record_sink(
                f,
                &crate::CompileBounds::new(51., 49., 21., 18.).into(),
                0,
                14,
                &mut |record| {
                    records.push(record);
                    Ok(())
                },
            )
            .unwrap();
            let tile = TileId::from(TileCoord::new(9, 283, 172).unwrap()).value();
            let record = records.iter().find(|r| r.tile_id == tile).unwrap();
            let TileGeometry::Polygon(groups) = &record.geometry else {
                panic!("polygon")
            };
            assert_eq!(groups.len(), 1);
            assert!(groups[0].holes.is_empty());
            let ring = &groups[0].exterior;
            assert_eq!(ring.first(), ring.last());
            // Counts include closure: raw72, quantized/reference71.
            assert_eq!(ring.len(), if quantized { 71 } else { 72 });
            let actual = ring[..ring.len() - 1]
                .iter()
                .map(|p| (p.x, p.y))
                .collect::<Vec<_>>();
            // Ignore only cyclic start and winding; preserve every edge.
            fn cyclic(ring: Vec<(i32, i32)>) -> Vec<(i32, i32)> {
                let normalize = |r: Vec<(i32, i32)>| {
                    (0..r.len())
                        .map(|i| r[i..].iter().chain(&r[..i]).copied().collect::<Vec<_>>())
                        .min()
                        .unwrap()
                };
                let forward = normalize(ring.clone());
                let backward = normalize(ring.into_iter().rev().collect());
                forward.min(backward)
            }
            assert_eq!(cyclic(actual), cyclic(expected), "quantized={quantized}");
        }
    }

    #[test]
    fn grid_cell_precedes_quantization_at_roczyny_boundary() {
        // Source-ID 19120068671: z14/9070/5568 encodes y=-13; at z9
        // y is just below tile174, but its MVT y rounds to4096 in tile173.
        let point = WorldPoint::new(282.0 + 5995.0 / 4096.0, 174.0 - 13.0 / (4096.0 * 32.0));
        assert_eq!(tile_point(point, 282, 173, 0).y, 4096);
        assert_eq!(label_grid_cell(point, 9, 128.0), Some((566, 347)));
        assert_eq!(
            label_grid_cell(WorldPoint::new(point.x, 174.0), 9, 128.0),
            Some((566, 348))
        );
        // Buffered copies quantize locally; source grid identity stays global.
        assert_eq!(tile_point(point, 282, 174, 0).y, 0);
        assert_eq!(label_grid_cell(point, 9, 128.0), Some((566, 347)));
    }
    #[test]
    fn grid_wraps_longitude_but_not_polar_y_and_disables_tiny_grids() {
        assert_eq!(
            label_grid_cell(WorldPoint::new(-0.25, -0.1), 2, 128.0),
            Some((7, -1))
        );
        assert_eq!(
            label_grid_cell(WorldPoint::new(4.0, 4.1), 2, 128.0),
            Some((0, 8))
        );
        assert_eq!(label_grid_cell(WorldPoint::new(-0.0, 0.0), 2, 0.0), None);
        assert_eq!(label_grid_cell(WorldPoint::new(0.0, 0.0), 2, 0.01), None);
        assert_eq!(
            label_grid_cell(WorldPoint::new(0.0, 0.0), 2, 0.0625),
            Some((0, 0))
        );
    }

    use super::*;
    #[test]
    fn profile_buffers_are_not_clamped_to_tactical_four_pixels() {
        assert_eq!(
            tile_point(WorldPoint::new(-0.5, 1.5), 0, 0, 0),
            TilePoint::new(-2048, 6144)
        );
    }
    #[test]
    fn point_neighbor_epsilon_keeps_two_pbf_places_at_buffer_edge() {
        use geo_types::{Geometry, Point};

        // Actual MaÅ‚opolskie PBF nodes: each is less than 0.1/4096 tile units
        // beyond a full-tile label buffer. Pinned TiledGeometry.slicePoint
        // includes both neighboring tiles, even though MVT rounds to the edge.
        for (id, zoom, lon, lat, tile, expected) in [
            (
                19120071511,
                9,
                19.9178885,
                50.2893464,
                (285, 174),
                TilePoint::new(-2754, -4096),
            ),
            (
                30097390051,
                11,
                19.5745606,
                49.8379803,
                (1136, 694),
                TilePoint::new(-2632, 8192),
            ),
        ] {
            let geometry = Geometry::Point(Point::new(lon, lat));
            let source = super::super::types::Source {
                id,
                osm_id: (id / 10) as i64,
                kind: super::super::types::SourceKind::Node,
                source_layer: String::new(),
                tags: BTreeMap::new(),
                geometry: geometry.clone(),
                relations: vec![],
            };
            let mut feature = Feature::new(&source, "place", geometry, zoom, zoom);
            feature.buffer_pixels = 256.0;
            let mut records = Vec::new();
            emit_with_record_sink(
                feature,
                &crate::CompileBounds::new(51., 49., 21., 18.).into(),
                zoom,
                zoom,
                &mut |record| {
                    records.push(record);
                    Ok(())
                },
            )
            .unwrap();
            let target = TileId::from(TileCoord::new(zoom, tile.0, tile.1).unwrap()).value();
            let record = records
                .iter()
                .find(|record| record.tile_id == target)
                .unwrap();
            assert_eq!(record.geometry, TileGeometry::Point(vec![expected]));
        }
    }
    #[test]
    fn negative_rounding_matches_java() {
        assert_eq!(
            tile_point(WorldPoint::new(-0.5 / 4096.0, 0.0), 0, 0, 0).x,
            0
        );
    }
    #[test]
    fn lines_preserve_intermediate_precision() {
        assert_ne!(
            tile_point(WorldPoint::new(0.000001, 0.0), 0, 0, 14),
            tile_point(WorldPoint::new(0.000002, 0.0), 0, 0, 14)
        );
    }
    #[test]
    fn original_way_line_uses_packed_nodes_before_buffer_clipping() {
        use geo_types::{Geometry, LineString};

        // OSM way 217375755, z14/9065/5555. The first simplified segment
        // crosses the four-pixel buffer edge at y=4160. Pinned Java's packed
        // node-store coordinates intersect at x=1571.4740 (rounds to 1571);
        // raw longitude/latitude projection intersects at x=1571.5073 (1572).
        let geometry = Geometry::LineString(LineString::from(vec![
            (19.1904668, 50.0078014),
            (19.1905716, 50.0074803),
            (19.1906850, 50.0071325),
            (19.1904390, 50.0070993),
            (19.1903277, 50.0074405),
            (19.1902313, 50.0077359),
        ]));
        let mut source = super::super::types::Source {
            id: 2173757552,
            osm_id: 217375755,
            kind: super::super::types::SourceKind::Way,
            source_layer: "osm".into(),
            tags: BTreeMap::new(),
            geometry: geometry.clone(),
            relations: vec![],
        };
        let tile = TileId::from(TileCoord::new(14, 9065, 5555).unwrap()).value();
        for (original, x) in [(false, 1572), (true, 1571)] {
            let mut feature = if original {
                Feature::new_original_geometry(&source, "transportation", geometry.clone(), 14, 14)
            } else {
                Feature::new(&source, "transportation", geometry.clone(), 14, 14)
            };
            assert_eq!(feature.osm_outline_projection, original);
            feature.buffer_pixels = 4.0;
            feature.min_size = 0.0;
            let mut records = Vec::new();
            emit_with_record_sink(
                feature,
                &crate::CompileBounds::new(51., 49., 21., 18.).into(),
                14,
                14,
                &mut |record| {
                    records.push(record);
                    Ok(())
                },
            )
            .unwrap();
            let record = records
                .iter()
                .find(|record| record.tile_id == tile)
                .unwrap();
            let TileGeometry::Line { paths, scale_exp } = &record.geometry else {
                panic!("expected line")
            };
            assert_eq!(*scale_exp, 0);
            assert_eq!(
                paths[0],
                vec![TilePoint::new(1554, 4078), TilePoint::new(x, 4160)]
            );
            assert_eq!(
                paths[1],
                vec![TilePoint::new(1524, 4160), TilePoint::new(1510, 4097)]
            );
        }
        source.kind = super::super::types::SourceKind::Supplement;
        assert!(
            !Feature::new_original_geometry(&source, "transportation", geometry, 14, 14)
                .osm_outline_projection
        );
    }

    #[test]
    fn osm_landcover_way_uses_packed_nodes_before_simplification() {
        use geo_types::{Geometry, LineString, Polygon};

        // OSM way 1512258402 (landuse=grass), z14/9097/5552. The source
        // PBF's eleven nodes produce eight MVT ring coordinates with raw
        // projection, but nine with Planetiler 0.10.2's packed node store.
        // PBF SHA-256: 4c3a6b117c6d1e79f7fbaae4efc8d8565866a3ea1c075488fb04746acec1c971
        // JAR SHA-256: f310bd0413e2e4512b27f4046d418664e8e1d3bf31603c2a70e23de06c167e4d
        let geometry = Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (19.9061518, 50.0625012),
                (19.9062426, 50.0624859),
                (19.9061918, 50.0623511),
                (19.9061919, 50.0623465),
                (19.9062473, 50.0623377),
                (19.9063973, 50.0623964),
                (19.9064374, 50.0625028),
                (19.9062217, 50.0625354),
                (19.9062022, 50.0625216),
                (19.9061799, 50.0625108),
                (19.9061518, 50.0625012),
            ]),
            vec![],
        ));
        let source = super::super::types::Source {
            id: 15122584022,
            osm_id: 1512258402,
            kind: super::super::types::SourceKind::Way,
            source_layer: "osm".into(),
            tags: BTreeMap::from([("landuse".into(), "grass".into())]),
            geometry,
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::land::process(&source, &mut features);
        let feature = features
            .into_iter()
            .find(|f| f.layer == "landcover" && f.min_zoom == 14)
            .unwrap();
        assert!(feature.osm_outline_projection);
        let tile = TileId::from(TileCoord::new(14, 9097, 5552).unwrap()).value();
        let ring_for = |feature: Feature| {
            let mut records = Vec::new();
            emit_with_record_sink(
                feature,
                &crate::CompileBounds::new(51., 49., 21., 18.).into(),
                14,
                14,
                &mut |record| {
                    records.push(record);
                    Ok(())
                },
            )
            .unwrap();
            let record = records.iter().find(|r| r.tile_id == tile).unwrap();
            let TileGeometry::Polygon(groups) = &record.geometry else {
                panic!("landcover must remain a polygon")
            };
            assert_eq!(groups.len(), 1);
            assert!(groups[0].holes.is_empty());
            let ring = &groups[0].exterior;
            assert_eq!(ring.first(), ring.last());
            ring[..ring.len() - 1]
                .iter()
                .map(|p| (p.x, p.y))
                .collect::<Vec<_>>()
        };
        let cyclic = |ring: Vec<(i32, i32)>| {
            let normalize = |r: Vec<(i32, i32)>| {
                (0..r.len())
                    .map(|i| r[i..].iter().chain(&r[..i]).copied().collect::<Vec<_>>())
                    .min()
                    .unwrap()
            };
            normalize(ring.clone()).min(normalize(ring.into_iter().rev().collect()))
        };
        let mut raw = feature.clone();
        raw.osm_outline_projection = false;
        assert_eq!(
            cyclic(ring_for(raw)),
            cyclic(vec![
                (3896, 491),
                (3913, 495),
                (3903, 536),
                (3913, 538),
                (3941, 521),
                (3949, 490),
                (3909, 481),
            ])
        );
        assert_eq!(
            cyclic(ring_for(feature)),
            cyclic(vec![
                (3896, 491),
                (3913, 495),
                (3903, 536),
                (3913, 538),
                (3941, 521),
                (3949, 490),
                (3909, 481),
                (3901, 488),
            ])
        );
    }

    #[test]
    fn osm_wood_ring_keeps_reference_vertex_under_omt_simplification() {
        use geo_types::{Geometry, LineString, Polygon};

        // OSM way 288999590, natural=wood. z14/9097/5552 contains the
        // entire valid ring; no clipping or polygon repair is involved.
        // Planetiler 0.10.2's whole-ring forced-point DP keeps (584,3672),
        // which the shared split-ring simplifier omitted.
        // PBF SHA-256: 4c3a6b117c6d1e79f7fbaae4efc8d8565866a3ea1c075488fb04746acec1c971
        // JAR SHA-256: f310bd0413e2e4512b27f4046d418664e8e1d3bf31603c2a70e23de06c167e4d
        let geometry = Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (19.8895137, 50.0515837),
                (19.888969, 50.0515654),
                (19.8883845, 50.0515443),
                (19.888119, 50.051536),
                (19.8882057, 50.0517074),
                (19.8887477, 50.0517416),
                (19.8887994, 50.0517788),
                (19.8894911, 50.0517726),
                (19.8894869, 50.0520057),
                (19.8898527, 50.0519879),
                (19.8899374, 50.0519752),
                (19.8899184, 50.0519496),
                (19.8898579, 50.0518203),
                (19.8898418, 50.0517695),
                (19.8897948, 50.0516209),
                (19.8897801, 50.0516009),
                (19.8897472, 50.0515832),
                (19.8895137, 50.0515837),
            ]),
            vec![],
        ));
        let source = super::super::types::Source {
            id: 2889995902,
            osm_id: 288999590,
            kind: super::super::types::SourceKind::Way,
            source_layer: "osm".into(),
            tags: BTreeMap::from([("natural".into(), "wood".into())]),
            geometry,
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::land::process(&source, &mut features);
        let feature = features
            .into_iter()
            .find(|f| f.layer == "landcover" && f.min_zoom == 14)
            .unwrap();
        assert!(feature.osm_outline_projection);
        assert_eq!(feature.tolerance_at_max_zoom, 0.0625);
        let tile = TileId::from(TileCoord::new(14, 9097, 5552).unwrap()).value();
        let mut records = Vec::new();
        emit_with_record_sink(
            feature,
            &crate::CompileBounds::new(51., 49., 21., 18.).into(),
            14,
            14,
            &mut |record| {
                records.push(record);
                Ok(())
            },
        )
        .unwrap();
        let record = records.iter().find(|r| r.tile_id == tile).unwrap();
        let TileGeometry::Polygon(groups) = &record.geometry else {
            panic!("landcover must remain a polygon")
        };
        assert_eq!(groups.len(), 1);
        assert!(groups[0].holes.is_empty());
        let ring = &groups[0].exterior;
        assert_eq!(ring.first(), ring.last());
        let actual = ring[..ring.len() - 1]
            .iter()
            .map(|p| (p.x, p.y))
            .collect::<Vec<_>>();
        let expected = vec![
            (794, 3661),
            (584, 3672),
            (534, 3675),
            (550, 3625),
            (651, 3615),
            (661, 3604),
            (790, 3606),
            (789, 3538),
            (857, 3543),
            (873, 3547),
            (870, 3555),
            (858, 3592),
            (846, 3650),
            (844, 3656),
            (838, 3661),
        ];
        fn cyclic(ring: Vec<(i32, i32)>) -> Vec<(i32, i32)> {
            let normalize = |r: Vec<(i32, i32)>| {
                (0..r.len())
                    .map(|i| r[i..].iter().chain(&r[..i]).copied().collect::<Vec<_>>())
                    .min()
                    .unwrap()
            };
            normalize(ring.clone()).min(normalize(ring.into_iter().rev().collect()))
        }
        assert_eq!(cyclic(actual), cyclic(expected));
    }

    #[test]
    fn farmland_at_zoom_ten_uses_inclusive_landcover_tolerance() {
        use geo_types::{Geometry, LineString, Polygon};

        // OSM way 1529655825 (landuse=farmland), z10/569/345. Planetiler's
        // setPixelToleranceBelowZoom(10, 0.25) includes zoom 10. At 0.1 the
        // simplified ring keeps (2588,1679), which the reference omits.
        // PBF SHA-256: 4c3a6b117c6d1e79f7fbaae4efc8d8565866a3ea1c075488fb04746acec1c971
        // JAR SHA-256: f310bd0413e2e4512b27f4046d418664e8e1d3bf31603c2a70e23de06c167e4d
        let geometry = Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (20.2619313, 50.4177905),
                (20.2618454, 50.417788),
                (20.2617127, 50.4178303),
                (20.2599786, 50.4182789),
                (20.2568324, 50.4190556),
                (20.2567922, 50.4191009),
                (20.2568445, 50.4194513),
                (20.256945, 50.4201101),
                (20.2572361, 50.4228444),
                (20.2597493, 50.4221036),
                (20.261195, 50.4216815),
                (20.2628379, 50.4213431),
                (20.2636345, 50.4211662),
                (20.2636693, 50.4211517),
                (20.2636921, 50.4211107),
                (20.2636881, 50.421044),
                (20.2633703, 50.4204374),
                (20.2631839, 50.4200947),
                (20.2629545, 50.4196709),
                (20.262579, 50.4189745),
                (20.262292, 50.4184566),
                (20.2619641, 50.417829),
                (20.2619313, 50.4177905),
            ]),
            vec![],
        ));
        let source = super::super::types::Source {
            id: 15296558252,
            osm_id: 1529655825,
            kind: super::super::types::SourceKind::Way,
            source_layer: "osm".into(),
            tags: BTreeMap::from([("landuse".into(), "farmland".into())]),
            geometry,
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::land::process(&source, &mut features);
        let feature = features
            .into_iter()
            .find(|f| f.layer == "landcover" && f.min_zoom == 10)
            .unwrap();
        assert_eq!(feature.tolerance, 0.25);
        let mut records = Vec::new();
        emit_with_record_sink(
            feature,
            &crate::CompileBounds::new(51., 49., 21., 18.).into(),
            10,
            14,
            &mut |record| {
                records.push(record);
                Ok(())
            },
        )
        .unwrap();
        let tile = TileId::from(TileCoord::new(10, 569, 345).unwrap()).value();
        let record = records.iter().find(|r| r.tile_id == tile).unwrap();
        let TileGeometry::Polygon(groups) = &record.geometry else {
            panic!("landcover must remain a polygon")
        };
        assert_eq!(groups.len(), 1);
        assert!(groups[0].holes.is_empty());
        let ring = &groups[0].exterior;
        assert_eq!(ring.first(), ring.last());
        let actual = ring[..ring.len() - 1]
            .iter()
            .map(|p| (p.x, p.y))
            .collect::<Vec<_>>();
        let expected = vec![(2597, 1750), (2537, 1727), (2542, 1658), (2617, 1689)];
        let normalize = |r: Vec<(i32, i32)>| {
            (0..r.len())
                .map(|i| r[i..].iter().chain(&r[..i]).copied().collect::<Vec<_>>())
                .min()
                .unwrap()
        };
        let canonical =
            |r: Vec<(i32, i32)>| normalize(r.clone()).min(normalize(r.into_iter().rev().collect()));
        assert_eq!(canonical(actual), canonical(expected));
    }

    fn assert_polygon_reference(geometry: TileGeometry, fixture_json: &str) {
        let fixture: serde_json::Value = serde_json::from_str(fixture_json).unwrap();
        let expected = &fixture["jts_buffer_zero_then_precision_reduce"];
        let parse = |value: &serde_json::Value| {
            value
                .as_array()
                .unwrap()
                .iter()
                .map(|p| (p[0].as_i64().unwrap() as i32, p[1].as_i64().unwrap() as i32))
                .collect::<Vec<_>>()
        };
        let canonical = |mut ring: Vec<(i32, i32)>| {
            assert_eq!(ring.first(), ring.last());
            ring.pop();
            let rotations = |r: &Vec<(i32, i32)>| {
                (0..r.len())
                    .map(|i| r[i..].iter().chain(&r[..i]).copied().collect::<Vec<_>>())
                    .min()
                    .unwrap()
            };
            let forward = rotations(&ring);
            ring.reverse();
            forward.min(rotations(&ring))
        };
        let TileGeometry::Polygon(groups) = geometry else {
            panic!("polygon expected")
        };
        assert_eq!(groups.len(), 1);
        assert_eq!(
            groups[0].holes.len(),
            expected["holes"].as_array().unwrap().len()
        );
        let points = |r: &[TilePoint]| r.iter().map(|p| (p.x, p.y)).collect();
        assert_eq!(
            canonical(points(&groups[0].exterior)),
            canonical(parse(&expected["exterior"]))
        );
        let mut actual_holes = groups[0]
            .holes
            .iter()
            .map(|h| canonical(points(h)))
            .collect::<Vec<_>>();
        let mut expected_holes = expected["holes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| canonical(parse(h)))
            .collect::<Vec<_>>();
        actual_holes.sort();
        expected_holes.sort();
        assert_eq!(actual_holes, expected_holes);
    }

    fn assert_meadow_reference(geometry: TileGeometry) {
        assert_polygon_reference(
            geometry,
            include_str!("../../tests/fixtures/omt-float-repair-meadow-1525089255.json"),
        );
    }

    #[test]
    fn contained_crossing_repair_matches_pinned_meadow_and_preserves_scope() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-float-repair-meadow-1525089255.json"
        ))
        .unwrap();
        let group = WorldRingGroup {
            exterior: fixture["simplified_tile_float_exterior"]
                .as_array()
                .unwrap()
                .iter()
                .map(|p| {
                    WorldPoint::new(
                        572. + p[0].as_f64().unwrap() / 4096.,
                        349. + p[1].as_f64().unwrap() / 4096.,
                    )
                })
                .collect(),
            holes: vec![],
        };
        let rect = WorldRect {
            min_x: 572.,
            max_x: 573.,
            min_y: 349.,
            max_y: 350.,
        };
        let fixed = repair_contained_crossing_polygon(std::slice::from_ref(&group), rect, 572, 349)
            .unwrap();
        assert_meadow_reference(tile_polygons(fixed, 572, 349).unwrap());
        let mut reversed = group.clone();
        reversed.exterior.reverse();
        let fixed = repair_contained_crossing_polygon(&[reversed], rect, 572, 349).unwrap();
        assert_meadow_reference(tile_polygons(fixed, 572, 349).unwrap());
        // Multipart overlap and a polygon crossing the tile boundary keep the
        // established clipping path; this is not a general contained bypass.
        assert!(
            repair_contained_crossing_polygon(&[group.clone(), group.clone()], rect, 572, 349)
                .is_none()
        );
        let with_hole = WorldRingGroup {
            holes: vec![group.exterior.clone()],
            ..group.clone()
        };
        assert!(repair_contained_crossing_polygon(&[with_hole], rect, 572, 349).is_none());
        let cut = WorldRect {
            max_x: 572. + 1100. / 4096.,
            ..rect
        };
        assert!(repair_contained_crossing_polygon(&[group], cut, 572, 349).is_none());
        let simple = WorldRingGroup {
            exterior: vec![
                WorldPoint::new(572.1, 349.1),
                WorldPoint::new(572.9, 349.1),
                WorldPoint::new(572.9, 349.9),
                WorldPoint::new(572.1, 349.9),
                WorldPoint::new(572.1, 349.1),
            ],
            holes: vec![],
        };
        assert!(repair_contained_crossing_polygon(&[simple], rect, 572, 349).is_none());
        // A sub-grid crossing collapses on rounding, so the float fallback must
        // not run merely because the floating shell crosses itself.
        let collapsed = WorldRingGroup {
            exterior: [
                (10., 10.),
                (10.2, 10.2),
                (10., 10.2),
                (10.2, 10.),
                (10., 10.),
            ]
            .into_iter()
            .map(|(x, y)| WorldPoint::new(572. + x / 4096., 349. + y / 4096.))
            .collect(),
            holes: vec![],
        };
        assert!(repair_contained_crossing_polygon(&[collapsed], rect, 572, 349).is_none());
    }

    #[test]
    fn osm_meadow_float_repair_matches_reference_after_full_render() {
        use super::super::types::{Source, SourceKind};
        let coords = include_str!("../../tests/fixtures/omt-meadow-1525089255.csv")
            .lines()
            .map(|line| {
                let (x, y) = line.split_once(',').unwrap();
                (x.parse::<f64>().unwrap(), y.parse::<f64>().unwrap())
            })
            .collect::<Vec<_>>();
        let source = Source {
            id: 15250892552,
            osm_id: 1525089255,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: BTreeMap::from([("landuse".into(), "meadow".into())]),
            geometry: geo_types::Polygon::new(geo_types::LineString::from(coords), vec![]).into(),
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::land::process(&source, &mut features);
        let feature = features
            .into_iter()
            .find(|f| f.layer == "landcover" && f.min_zoom == 10)
            .unwrap();
        let tile = TileId::from(TileCoord::new(10, 572, 349).unwrap()).value();
        let mut found = false;
        emit_with_record_sink(
            feature,
            &crate::CompileBounds::new(51., 49., 22., 18.).into(),
            10,
            14,
            &mut |record| {
                if record.zoom == 10 && record.tile_id == tile {
                    assert_meadow_reference(record.geometry);
                    found = true;
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(found);
    }

    #[test]
    fn university_size_override_includes_zoom_13_for_actual_way() {
        use super::super::types::{Source, SourceKind};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-poi-university-425831539.json"
        ))
        .unwrap();
        let coords = fixture["coordinates_lonlat"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
            .collect::<Vec<_>>();
        let source = Source {
            id: 4258315392,
            osm_id: 425831539,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: serde_json::from_value(fixture["tags"].clone()).unwrap(),
            geometry: geo_types::Polygon::new(geo_types::LineString::from(coords), vec![]).into(),
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::labels::process(&source, &mut features);
        let feature = features.into_iter().find(|f| f.layer == "poi").unwrap();
        let mut records = Vec::new();
        emit_with_record_sink(
            feature,
            &crate::CompileBounds::new(51., 49., 22., 18.).into(),
            10,
            14,
            &mut |record| {
                records.push(record);
                Ok(())
            },
        )
        .unwrap();
        assert!(!records.iter().any(|r| r.zoom <= 13));
        assert!(records.iter().any(|r| r.zoom == 14));
    }

    #[test]
    fn original_osm_nodes_use_packed_world_coordinates_at_poi_buffer_edges() {
        use super::super::types::{Source, SourceKind};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-poi-node-buffer-edges.json"
        ))
        .unwrap();
        for case in fixture["cases"].as_array().unwrap() {
            let id = case["id"].as_u64().unwrap();
            let source = Source {
                id: id * 10 + 1,
                osm_id: id as i64,
                kind: SourceKind::Node,
                source_layer: "osm".into(),
                tags: serde_json::from_value(case["tags"].clone()).unwrap(),
                geometry: geo_types::Point::new(
                    case["lon"].as_f64().unwrap(),
                    case["lat"].as_f64().unwrap(),
                )
                .into(),
                relations: vec![],
            };
            let mut features = Vec::new();
            super::super::labels::process(&source, &mut features);
            let feature = features.into_iter().find(|f| f.layer == "poi").unwrap();
            assert!(feature.world_point.is_some());
            let mut tiles = Vec::new();
            emit_with_record_sink(
                feature,
                &crate::CompileBounds::new(51., 49., 22., 18.).into(),
                14,
                14,
                &mut |record| {
                    tiles.push(record.tile_id);
                    Ok(())
                },
            )
            .unwrap();
            for tile in case["absent_neighbor_tiles"].as_array().unwrap() {
                let target = TileId::from(
                    TileCoord::new(
                        14,
                        tile[0].as_u64().unwrap() as u32,
                        tile[1].as_u64().unwrap() as u32,
                    )
                    .unwrap(),
                )
                .value();
                assert!(!tiles.contains(&target), "node {id} leaked into {tile}");
            }
            assert!(!tiles.is_empty(), "node {id} disappeared entirely");
        }
    }

    #[test]
    fn packed_cliff_source_falls_below_zoom_13_length_threshold() {
        use super::super::types::{Source, SourceKind};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-mountain-cliff-237075292.json"
        ))
        .unwrap();
        let coords = fixture["coordinates_lonlat"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| (p[0].as_f64().unwrap(), p[1].as_f64().unwrap()))
            .collect::<Vec<_>>();
        let source = Source {
            id: 2370752922,
            osm_id: 237075292,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: serde_json::from_value(fixture["tags"].clone()).unwrap(),
            geometry: geo_types::LineString::from(coords).into(),
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::labels::process(&source, &mut features);
        let feature = features
            .into_iter()
            .find(|f| f.layer == "mountain_peak")
            .unwrap();
        assert!(feature.osm_outline_projection);
        let target = TileId::from(TileCoord::new(14, 9091, 5544).unwrap()).value();
        let mut records = Vec::new();
        emit_with_record_sink(
            feature,
            &crate::CompileBounds::new(51., 49., 22., 18.).into(),
            13,
            14,
            &mut |record| {
                records.push(record);
                Ok(())
            },
        )
        .unwrap();
        assert!(!records.iter().any(|r| r.zoom == 13));
        assert!(records.iter().any(|r| r.zoom == 14 && r.tile_id == target));
    }

    #[test]
    fn osm_farmland_signed_area_repair_preserves_reference_feature() {
        use super::super::types::{Source, SourceKind};
        let coords = include_str!("../../tests/fixtures/omt-farmland-724352898.csv")
            .lines()
            .map(|line| {
                let (x, y) = line.split_once(',').unwrap();
                (x.parse::<f64>().unwrap(), y.parse::<f64>().unwrap())
            })
            .collect::<Vec<_>>();
        let source = Source {
            id: 7243528982,
            osm_id: 724352898,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: BTreeMap::from([("landuse".into(), "farmland".into())]),
            geometry: geo_types::Polygon::new(geo_types::LineString::from(coords), vec![]).into(),
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::land::process(&source, &mut features);
        let feature = features
            .into_iter()
            .find(|f| f.layer == "landcover" && f.min_zoom <= 9 && f.max_zoom >= 9)
            .unwrap();
        let tile = TileId::from(TileCoord::new(9, 284, 173).unwrap()).value();
        let mut found = false;
        emit_with_record_sink(
            feature,
            &crate::CompileBounds::new(51., 49., 22., 18.).into(),
            9,
            14,
            &mut |record| {
                if record.zoom == 9 && record.tile_id == tile {
                    assert_polygon_reference(
                        record.geometry,
                        include_str!(
                            "../../tests/fixtures/omt-float-repair-farmland-724352898.json"
                        ),
                    );
                    found = true;
                }
                Ok(())
            },
        )
        .unwrap();
        assert!(found);
    }

    #[test]
    fn short_ferry_is_filtered_through_zoom_ten() {
        use super::super::types::{Source, SourceKind};
        let source = Source {
            id: 12,
            osm_id: 1,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: BTreeMap::from([("route".into(), "ferry".into())]),
            geometry: geo_types::LineString::from(vec![(19., 50.), (19.005, 50.)]).into(),
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::transport::process(&source, &mut features);
        for (zoom, archive_max) in [(9, 14), (10, 14), (11, 14), (14, 14), (9, 9), (10, 10)] {
            let mut records = Vec::new();
            for feature in features.iter().filter(|f| f.layer == "transportation") {
                emit_with_record_sink(
                    feature.clone(),
                    &crate::CompileBounds::new(51., 49., 21., 18.).into(),
                    zoom,
                    archive_max,
                    &mut |record| {
                        if record.zoom == zoom {
                            records.push(record);
                        }
                        Ok(())
                    },
                )
                .unwrap();
            }
            assert_eq!(records.is_empty(), zoom <= 10, "zoom {zoom}");
        }
    }

    #[test]
    fn osm_housenumber_world_points_keep_pinned_half_grid_ties() {
        use super::super::types::{Source, SourceKind};
        use geo_types::{Geometry, LineString, Polygon};

        // Actual PBF ways and pinned Planetiler 0.10.2 coordinates. Both JTS
        // interior points have Y exactly halfway between MVT grid units.
        for (way, ring, tile, expected, roundtripped) in [
            (
                844238790,
                vec![
                    (19.8253652, 50.2717642),
                    (19.8251953, 50.2717929),
                    (19.8252157, 50.2718422),
                    (19.8252708, 50.2718328),
                    (19.8252762, 50.2718460),
                    (19.8253910, 50.2718266),
                    (19.8253856, 50.2718134),
                    (19.8253652, 50.2717642),
                ],
                (9094, 5537),
                TilePoint::new(1110, 1020),
                TilePoint::new(1110, 1019),
            ),
            (
                524500494,
                vec![
                    (20.4114145, 49.5652196),
                    (20.4115917, 49.5652544),
                    (20.4116535, 49.5651218),
                    (20.4114249, 49.5650770),
                    (20.4113980, 49.5651348),
                    (20.4114493, 49.5651449),
                    (20.4114145, 49.5652196),
                ],
                (9120, 5587),
                TilePoint::new(3897, 804),
                TilePoint::new(3897, 803),
            ),
        ] {
            let source = Source {
                id: way * 10 + 2,
                osm_id: way as i64,
                kind: SourceKind::Way,
                source_layer: String::new(),
                tags: [("addr:housenumber".into(), "1".into())].into(),
                geometry: Geometry::Polygon(Polygon::new(LineString::from(ring), vec![])),
                relations: vec![],
            };
            let mut features = Vec::new();
            super::super::labels::process(&source, &mut features);
            let feature = features
                .into_iter()
                .find(|f| f.layer == "housenumber")
                .unwrap();
            let target = TileId::from(TileCoord::new(14, tile.0, tile.1).unwrap()).value();
            for (world_point, wanted) in [(feature.world_point, expected), (None, roundtripped)] {
                let mut candidate = feature.clone();
                candidate.world_point = world_point;
                let mut records = Vec::new();
                emit_with_record_sink(
                    candidate,
                    &crate::CompileBounds::new(51., 49., 21., 18.).into(),
                    14,
                    14,
                    &mut |record| {
                        records.push(record);
                        Ok(())
                    },
                )
                .unwrap();
                let record = records.iter().find(|r| r.tile_id == target).unwrap();
                assert_eq!(record.geometry, TileGeometry::Point(vec![wanted]));
            }
        }
    }
}
