//! Completes geo's polygon validation with the connected-interior check.
//! A cycle in the bipartite graph of rings and distinct touch points disconnects
//! a polygon interior. Using touch-point nodes permits several rings to touch
//! at one shared point without falsely treating that contact as a cycle.
use geo::{
    BoundingRect, Intersects, Line, Rect, Relate, Validation,
    algorithm::coordinate_position::coord_pos_relative_to_ring,
    algorithm::kernels::{Kernel, Orientation, RobustKernel},
    algorithm::{coordinate_position::CoordPos, dimensions::Dimensions},
};
use geo_types::{LineString, MultiPolygon, Polygon};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn is_valid(polygons: &MultiPolygon<f64>) -> bool {
    multi_polygon_valid(polygons) && polygons.0.iter().all(connected_interior)
}

/// Same result as geo's `MultiPolygon::is_valid`: every polygon is valid and no
/// two polygons overlap or share a boundary line. geo relates every pair; only
/// pairs whose bounding rects intersect (touching included) can fail.
fn multi_polygon_valid(polygons: &MultiPolygon<f64>) -> bool {
    if !polygons.0.iter().all(polygon_valid) {
        return false;
    }
    let rects = polygons
        .0
        .iter()
        .map(|polygon| polygon.bounding_rect())
        .collect::<Vec<_>>();
    for (i, a) in rects.iter().enumerate() {
        let Some(a) = a else { continue };
        for (j, b) in rects.iter().enumerate().skip(i + 1) {
            let Some(b) = b else { continue };
            if a.max().x < b.min().x
                || b.max().x < a.min().x
                || a.max().y < b.min().y
                || b.max().y < a.min().y
            {
                continue;
            }
            if polygons_conflict(&polygons.0[i], &polygons.0[j], *a, *b) {
                return false;
            }
        }
    }
    true
}

/// Whether two valid polygons, whose bounding rects meet, overlap or share a
/// boundary line: the `relate` test of geo's `MultiPolygon::is_valid`.
fn polygons_conflict(
    p: &Polygon<f64>,
    q: &Polygon<f64>,
    p_rect: Rect<f64>,
    q_rect: Rect<f64>,
) -> bool {
    polygons_conflict_fast(p, q, p_rect, q_rect).unwrap_or_else(|| {
        let matrix = p.relate(q);
        matrix.get(CoordPos::Inside, CoordPos::Inside) == Dimensions::TwoDimensional
            || matrix.get(CoordPos::OnBoundary, CoordPos::OnBoundary) == Dimensions::OneDimensional
    })
}

/// `polygons_conflict` without geo's graph: rings of different polygons that
/// cross or share a stretch of boundary make the polygons overlap or touch on
/// a line, and when no ring meets another the polygons overlap exactly when
/// one lies in the other (see `holed_polygon_valid`). `None` leaves the pair to
/// geo: rings that only touch at points, and zero-area rings.
fn polygons_conflict_fast(
    p: &Polygon<f64>,
    q: &Polygon<f64>,
    p_rect: Rect<f64>,
    q_rect: Rect<f64>,
) -> Option<bool> {
    let mut rings = non_empty_rings(p);
    let split = rings.len();
    rings.extend(non_empty_rings(q));
    if rings.iter().any(|ring| all_collinear(ring)) {
        return None;
    }
    // Only segments within both bounding rects can meet the other polygon.
    let window = (
        p_rect.min().x.max(q_rect.min().x),
        p_rect.min().y.max(q_rect.min().y),
        p_rect.max().x.min(q_rect.max().x),
        p_rect.max().y.min(q_rect.max().y),
    );
    match sweep_rings(&rings, Some(split), Some(window)) {
        Sweep::Invalid => return Some(true),
        Sweep::Touch => return None,
        Sweep::Clear => {}
    }
    // `inner` has a point (its first) in the area of `outer`.
    let nested = |outer: &Polygon<f64>,
                  outer_rect: Rect<f64>,
                  inner: &Polygon<f64>,
                  inner_rect: Rect<f64>| {
        let point = inner.exterior().0[0];
        outer_rect.min().x <= inner_rect.min().x
            && outer_rect.min().y <= inner_rect.min().y
            && inner_rect.max().x <= outer_rect.max().x
            && inner_rect.max().y <= outer_rect.max().y
            && coord_pos_relative_to_ring(point, outer.exterior()) == CoordPos::Inside
            && !outer
                .interiors()
                .iter()
                .any(|hole| coord_pos_relative_to_ring(point, hole) != CoordPos::Outside)
    };
    Some(nested(p, p_rect, q, q_rect) || nested(q, q_rect, p, p_rect))
}

/// Same result as geo's `Polygon::is_valid`. Hole-free polygons skip geo's
/// quadratic ring self-intersection scan; polygons with holes skip geo's graph
/// based `relate` unless rings touch each other (see `holed_polygon_valid`).
pub(super) fn polygon_valid(polygon: &Polygon<f64>) -> bool {
    if !polygon.interiors().is_empty() {
        return holed_polygon_valid(polygon).unwrap_or_else(|| polygon.is_valid());
    }
    let ring = polygon.exterior();
    if ring.0.is_empty() {
        return true;
    }
    ring_valid(ring) && !ring_self_intersects(ring)
}

/// geo's per-ring checks other than self-intersection: at least four distinct
/// points (consecutive repeats removed) and finite coordinates.
fn ring_valid(ring: &LineString<f64>) -> bool {
    let distinct = 1 + ring.0.windows(2).filter(|w| w[0] != w[1]).count();
    distinct >= 4 && ring.0.iter().all(|c| c.x.is_finite() && c.y.is_finite())
}

/// geo validates a polygon with holes by checking every non-empty ring, then
/// relating the shell to each hole (the hole must be contained, and the rings
/// may share no line) and each pair of holes (no shared area, no shared line).
/// Those relations are decided here when no two rings share a point: with
/// disjoint boundaries a hole is contained exactly when one of its vertices is
/// inside the shell, and two holes share area exactly when one contains the
/// other. Two rings that cross, or share a stretch of boundary, are invalid
/// whatever else holds. `None` leaves the polygon to geo: rings that only touch
/// at points, and rings whose vertices are all collinear (zero area, which
/// geo's checks accept).
fn holed_polygon_valid(polygon: &Polygon<f64>) -> Option<bool> {
    let shell = polygon.exterior();
    if shell.0.is_empty() {
        return Some(true);
    }
    let rings = non_empty_rings(polygon);
    if !rings.iter().all(|ring| ring_valid(ring)) {
        return Some(false);
    }
    if rings.iter().any(|ring| all_collinear(ring)) {
        return None;
    }
    match sweep_rings(&rings, None, None) {
        Sweep::Invalid => return Some(false),
        Sweep::Touch => return None,
        Sweep::Clear => {}
    }
    let bounds = rings
        .iter()
        .map(|ring| {
            ring.0.iter().fold(
                (
                    f64::INFINITY,
                    f64::INFINITY,
                    f64::NEG_INFINITY,
                    f64::NEG_INFINITY,
                ),
                |b, c| (b.0.min(c.x), b.1.min(c.y), b.2.max(c.x), b.3.max(c.y)),
            )
        })
        .collect::<Vec<_>>();
    let within = |inner: usize, outer: usize| {
        let (a, b) = (bounds[inner], bounds[outer]);
        b.0 <= a.0
            && b.1 <= a.1
            && a.2 <= b.2
            && a.3 <= b.3
            && coord_pos_relative_to_ring(rings[inner].0[0], rings[outer]) == CoordPos::Inside
    };
    for hole in 1..rings.len() {
        if !within(hole, 0) {
            return Some(false);
        }
        for other in hole + 1..rings.len() {
            if within(other, hole) || within(hole, other) {
                return Some(false);
            }
        }
    }
    Some(true)
}

/// The shell followed by the holes that have points.
fn non_empty_rings(polygon: &Polygon<f64>) -> Vec<&LineString<f64>> {
    std::iter::once(polygon.exterior())
        .chain(polygon.interiors().iter().filter(|r| !r.0.is_empty()))
        .collect()
}

fn all_collinear(ring: &LineString<f64>) -> bool {
    let first = ring.0[0];
    let Some(&second) = ring.0.iter().find(|c| **c != first) else {
        return true;
    };
    ring.0
        .iter()
        .all(|&c| RobustKernel::orient2d(first, second, c) == Orientation::Collinear)
}

/// geo's `linestring_has_self_intersection`: some pair of segments (i, j) with
/// i != j intersects while `start_i != end_j` and `end_i != start_j`. A pair of
/// segments whose bounding boxes are disjoint cannot intersect (the kernel is
/// exact, so an intersection implies a shared point), so only pairs whose boxes
/// overlap are tested, found by sweeping segments sorted by minimum x.
fn ring_self_intersects(ring: &LineString<f64>) -> bool {
    struct Segment {
        line: Line<f64>,
        min_x: f64,
        max_x: f64,
        min_y: f64,
        max_y: f64,
    }
    let mut segments = ring
        .lines()
        .map(|line| Segment {
            line,
            min_x: line.start.x.min(line.end.x),
            max_x: line.start.x.max(line.end.x),
            min_y: line.start.y.min(line.end.y),
            max_y: line.start.y.max(line.end.y),
        })
        .collect::<Vec<_>>();
    segments.sort_by(|a, b| a.min_x.total_cmp(&b.min_x));
    let hit =
        |a: &Line<f64>, b: &Line<f64>| a.start != b.end && a.end != b.start && a.intersects(b);
    for (i, a) in segments.iter().enumerate() {
        for b in &segments[i + 1..] {
            if b.min_x > a.max_x {
                break;
            }
            if b.min_y <= a.max_y
                && a.min_y <= b.max_y
                && (hit(&a.line, &b.line) || hit(&b.line, &a.line))
            {
                return true;
            }
        }
    }
    false
}

#[derive(PartialEq)]
enum Sweep {
    Clear,
    /// geo rejects: a ring has a self-intersection by geo's definition, or two
    /// rings cross or share a stretch of boundary (which makes two polygons
    /// overlap or touch on a line).
    Invalid,
    /// No rejection found, but segments of two rings share a point.
    Touch,
}

/// `ring_self_intersects` for each of several rings, which also tests segments
/// of different rings for crossing, overlap and touching, by the same sweep.
/// (Kept apart from the single-ring function, which is hot and about 10%
/// slower with the extra bookkeeping.)
///
/// With `split`, `rings` are the rings of two polygons (the first `split` belong
/// to the first), and only pairs from different polygons are tested; segments
/// outside `window` (min x, min y, max x, max y) are ignored.
fn sweep_rings(
    rings: &[&LineString<f64>],
    split: Option<usize>,
    window: Option<(f64, f64, f64, f64)>,
) -> Sweep {
    struct Segment {
        line: Line<f64>,
        ring: u32,
        min_x: f64,
        max_x: f64,
        min_y: f64,
        max_y: f64,
    }
    let mut segments = Vec::with_capacity(rings.iter().map(|r| r.0.len()).sum());
    for (ring, points) in rings.iter().enumerate() {
        for line in points.lines() {
            let segment = Segment {
                line,
                ring: ring as u32,
                min_x: line.start.x.min(line.end.x),
                max_x: line.start.x.max(line.end.x),
                min_y: line.start.y.min(line.end.y),
                max_y: line.start.y.max(line.end.y),
            };
            if window.is_none_or(|w| {
                segment.max_x >= w.0
                    && segment.max_y >= w.1
                    && segment.min_x <= w.2
                    && segment.min_y <= w.3
            }) {
                segments.push(segment);
            }
        }
    }
    segments.sort_by(|a, b| a.min_x.total_cmp(&b.min_x));
    let hit =
        |a: &Line<f64>, b: &Line<f64>| a.start != b.end && a.end != b.start && a.intersects(b);
    let mut touch = false;
    for (i, a) in segments.iter().enumerate() {
        for b in &segments[i + 1..] {
            if b.min_x > a.max_x {
                break;
            }
            if b.min_y > a.max_y || a.min_y > b.max_y {
                continue;
            }
            if split.is_some_and(|k| (a.ring < k as u32) == (b.ring < k as u32)) {
                continue;
            }
            if a.ring == b.ring {
                if hit(&a.line, &b.line) || hit(&b.line, &a.line) {
                    return Sweep::Invalid;
                }
            } else if a.line.intersects(&b.line) {
                // Distinct endpoints on opposite sides of each other's line:
                // the rings cross. Collinear segments sharing more than one
                // point: the boundaries overlap in a line. Either way geo's
                // relation of the rings fails (a hole leaves its shell or
                // another hole, or the boundaries meet on a line).
                let side = |l: &Line<f64>, p| RobustKernel::orient2d(l.start, l.end, p);
                let (a1, a2) = (side(&b.line, a.line.start), side(&b.line, a.line.end));
                let (b1, b2) = (side(&a.line, b.line.start), side(&a.line, b.line.end));
                let crossing = a1 != Orientation::Collinear
                    && a2 != Orientation::Collinear
                    && b1 != Orientation::Collinear
                    && b2 != Orientation::Collinear
                    && a1 != a2
                    && b1 != b2;
                let overlap = a.line.start != a.line.end
                    && b.line.start != b.line.end
                    && b1 == Orientation::Collinear
                    && b2 == Orientation::Collinear
                    && if a.min_x < a.max_x {
                        a.min_x.max(b.min_x) < a.max_x.min(b.max_x)
                    } else {
                        a.min_y.max(b.min_y) < a.max_y.min(b.max_y)
                    };
                if crossing || overlap {
                    return Sweep::Invalid;
                }
                touch = true;
            }
        }
    }
    if touch { Sweep::Touch } else { Sweep::Clear }
}

fn connected_interior(polygon: &Polygon<f64>) -> bool {
    if polygon.interiors().is_empty() {
        return true;
    }
    let rings = std::iter::once(polygon.exterior())
        .chain(polygon.interiors())
        .collect::<Vec<_>>();
    let mut vertices = rings
        .iter()
        .enumerate()
        .flat_map(|(i, r)| r.0.iter().copied().map(move |p| (p, i)))
        .collect::<Vec<_>>();
    vertices.sort_by(|a, b| {
        a.0.x
            .total_cmp(&b.0.x)
            .then(a.0.y.total_cmp(&b.0.y))
            .then(a.1.cmp(&b.1))
    });
    vertices.dedup();
    let bits = |v: f64| if v == 0. { 0 } else { v.to_bits() };
    let mut contacts = BTreeMap::<(u64, u64), BTreeSet<usize>>::new();
    for (index, ring) in rings.iter().enumerate() {
        for edge in ring.0.windows(2) {
            let (a, b) = (edge[0], edge[1]);
            if a == b {
                continue;
            }
            let minx = a.x.min(b.x);
            let maxx = a.x.max(b.x);
            let miny = a.y.min(b.y);
            let maxy = a.y.max(b.y);
            let start = vertices.partition_point(|(p, _)| p.x < minx);
            let end = vertices.partition_point(|(p, _)| p.x <= maxx);
            for &(point, other) in &vertices[start..end] {
                if index == other || point.y < miny || point.y > maxy {
                    continue;
                }
                if RobustKernel::orient2d(a, b, point) == Orientation::Collinear {
                    let rings = contacts.entry((bits(point.x), bits(point.y))).or_default();
                    rings.insert(index);
                    rings.insert(other);
                }
            }
        }
    }
    let mut parent = (0..rings.len() + contacts.len()).collect::<Vec<_>>();
    fn root(parent: &mut [usize], mut node: usize) -> usize {
        while parent[node] != node {
            parent[node] = parent[parent[node]];
            node = parent[node];
        }
        node
    }
    for (point_index, incident) in contacts.values().enumerate() {
        let contact = rings.len() + point_index;
        for &ring in incident {
            let a = root(&mut parent, contact);
            let b = root(&mut parent, ring);
            if a == b {
                return false;
            }
            parent[b] = a;
        }
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo_types::LineString;
    fn ring(p: &[(f64, f64)]) -> LineString<f64> {
        LineString::from(p.to_vec())
    }
    fn square(x: f64, y: f64, size: f64) -> Polygon<f64> {
        Polygon::new(
            ring(&[
                (x, y),
                (x + size, y),
                (x + size, y + size),
                (x, y + size),
                (x, y),
            ]),
            vec![],
        )
    }
    fn assert_matches_geo(polygons: Vec<Polygon<f64>>) {
        for polygon in &polygons {
            assert_eq!(polygon_valid(polygon), polygon.is_valid(), "{polygon:?}");
        }
        let multi = MultiPolygon(polygons);
        assert_eq!(multi_polygon_valid(&multi), multi.is_valid(), "{multi:?}");
    }
    struct Lcg(u64);
    impl Lcg {
        fn next(&mut self, n: u64) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            (self.0 >> 33) % n
        }
    }
    #[test]
    fn polygon_valid_matches_geo_on_fixtures() {
        let fixtures = [
            vec![(0., 0.), (1., 1.)],
            vec![(0., 0.), (1., 1.), (0., 1.)],
            // Bow tie, spike, repeated points, duplicated vertex, collinear.
            vec![(0., 0.), (4., 0.), (0., 2.), (4., 2.), (0., 0.)],
            vec![
                (0., 0.),
                (4., 0.),
                (4., 4.),
                (2., 4.),
                (2., 6.),
                (2., 4.),
                (0., 4.),
                (0., 0.),
            ],
            vec![
                (0., 0.),
                (0., 0.),
                (4., 0.),
                (4., 0.),
                (4., 4.),
                (0., 4.),
                (0., 0.),
            ],
            vec![(0., 0.), (4., 0.), (4., 4.), (4., 0.), (0., 0.)],
            vec![(0., 0.), (4., 0.), (4., 4.), (0., 4.), (4., 0.), (0., 0.)],
            vec![(0., 0.), (2., 0.), (4., 0.), (4., 4.), (0., 4.), (0., 0.)],
            vec![(0., 0.), (4., 0.), (2., 0.), (2., 3.), (0., 0.)],
            vec![(0., 0.), (f64::NAN, 0.), (4., 4.), (0., 4.), (0., 0.)],
            vec![(0., 0.), (f64::INFINITY, 0.), (4., 4.), (0., 4.), (0., 0.)],
        ];
        for points in fixtures {
            assert_matches_geo(vec![Polygon::new(ring(&points), vec![])]);
        }
    }
    #[test]
    fn polygon_valid_matches_geo_on_random_grid_rings() {
        let mut rng = Lcg(99);
        // (grid size, coordinate scale, maximum extra points); the 0.1 scale
        // gives inexact floats and near-collinear triples.
        for (grid, scale, extra) in [(5, 1., 8), (12, 1., 14), (30, 0.1, 20), (4, 0.3, 6)] {
            let (mut valid, mut invalid) = (0, 0);
            for _ in 0..20000 {
                let count = 3 + rng.next(extra) as usize;
                let mut points = (0..count)
                    .map(|_| (rng.next(grid) as f64 * scale, rng.next(grid) as f64 * scale))
                    .collect::<Vec<_>>();
                if rng.next(4) != 0 {
                    points.push(points[0]);
                }
                let polygon = Polygon::new(ring(&points), vec![]);
                let expected = polygon.is_valid();
                assert_eq!(polygon_valid(&polygon), expected, "{polygon:?}");
                if expected {
                    valid += 1;
                } else {
                    invalid += 1;
                }
            }
            assert!(
                valid > 100 && invalid > 100,
                "{valid} valid, {invalid} invalid"
            );
        }
    }
    /// A random ring of 3..=`extra` + 2 points on a `grid` x `grid` lattice of
    /// spacing `scale`, inside the box `lo..hi`. Shapes: axis-aligned rectangle,
    /// star-shaped (simple), or free (may self-cross); sometimes with a repeated
    /// vertex, a collinear midpoint or no closing point.
    fn random_ring(
        rng: &mut Lcg,
        scale: f64,
        (lo, hi, extra, free): (u64, u64, u64, bool),
    ) -> LineString<f64> {
        let pick = |rng: &mut Lcg| (lo + rng.next(hi - lo + 1)) as f64 * scale;
        let mut points = match if free { rng.next(4) } else { rng.next(2) } {
            0 => {
                let (x0, y0, x1, y1) = (pick(rng), pick(rng), pick(rng), pick(rng));
                vec![(x0, y0), (x1, y0), (x1, y1), (x0, y1)]
            }
            1 => {
                // Star-shaped around a centre, vertices sorted by angle.
                let (cx, cy) = (pick(rng), pick(rng));
                let mut pts = (0..3 + rng.next(extra) as usize)
                    .map(|_| (pick(rng), pick(rng)))
                    .collect::<Vec<_>>();
                pts.sort_by(|a, b| {
                    (a.1 - cy)
                        .atan2(a.0 - cx)
                        .total_cmp(&(b.1 - cy).atan2(b.0 - cx))
                });
                pts
            }
            _ => (0..3 + rng.next(extra) as usize)
                .map(|_| (pick(rng), pick(rng)))
                .collect(),
        };
        if rng.next(10) == 0 {
            let i = rng.next(points.len() as u64) as usize;
            points.insert(i, points[i]);
        }
        if rng.next(10) == 0 {
            let i = rng.next(points.len() as u64) as usize;
            let (a, b) = (points[i], points[(i + 1) % points.len()]);
            points.insert(i + 1, ((a.0 + b.0) / 2., (a.1 + b.1) / 2.));
        }
        if rng.next(4) != 0 {
            points.push(points[0]);
        }
        ring(&points)
    }
    /// A `random_ring` that, unless `free`, is usually a valid ring on its own.
    fn tidy_ring(rng: &mut Lcg, scale: f64, args: (u64, u64, u64, bool)) -> LineString<f64> {
        loop {
            let ring = random_ring(rng, scale, args);
            if args.3 || rng.next(12) == 0 || (ring_valid(&ring) && !ring_self_intersects(&ring)) {
                return ring;
            }
        }
    }
    fn random_holed_polygon(rng: &mut Lcg, grid: u64, scale: f64) -> Polygon<f64> {
        let polygon = random_holed_polygon_base(rng, grid, scale);
        if rng.next(5) != 0 || polygon.interiors().is_empty() {
            return polygon;
        }
        // Add a rectangle nested in or around one of the holes (or just beside
        // it), so holes share area, touch or stay apart.
        let (shell, mut holes) = polygon.into_inner();
        let hole = &holes[rng.next(holes.len() as u64) as usize];
        let (mut lo, mut hi) = (
            (f64::INFINITY, f64::INFINITY),
            (f64::NEG_INFINITY, f64::NEG_INFINITY),
        );
        for c in hole.0.iter().filter(|c| c.x.is_finite() && c.y.is_finite()) {
            lo = (lo.0.min(c.x), lo.1.min(c.y));
            hi = (hi.0.max(c.x), hi.1.max(c.y));
        }
        let d = scale * (rng.next(3) as f64 - 1.);
        let (a, b) = (lo.0 + d, lo.1 + d);
        let (c, e) = (hi.0 - d, hi.1 - d);
        holes.push(ring(&[(a, b), (c, b), (c, e), (a, e), (a, b)]));
        Polygon::new(shell, holes)
    }
    fn random_holed_polygon_base(rng: &mut Lcg, grid: u64, scale: f64) -> Polygon<f64> {
        // Two thirds of the polygons use only rectangles and star-shaped rings,
        // which are mostly valid; the rest also use free (self-crossing) rings.
        let free = rng.next(3) == 0;
        if rng.next(3) == 0 {
            // Holes in separate cells of the shell (adjacent cells share lattice
            // lines, so holes still touch each other and the shell sometimes).
            let cells = 1 + rng.next(3);
            let size = grid / cells;
            let holes = (0..cells)
                .map(|i| {
                    let hole = tidy_ring(rng, scale, (0, size, 4, free));
                    let (dx, dy) = (i * size, rng.next(grid - size + 1));
                    let shift = |c: &geo_types::Coord<f64>| geo_types::Coord {
                        x: c.x + dx as f64 * scale,
                        y: c.y + dy as f64 * scale,
                    };
                    LineString::new(hole.0.iter().map(shift).collect())
                })
                .collect();
            return Polygon::new(
                ring(&[
                    (0., 0.),
                    (grid as f64 * scale, 0.),
                    (grid as f64 * scale, grid as f64 * scale),
                    (0., grid as f64 * scale),
                    (0., 0.),
                ]),
                holes,
            );
        }
        let shell = if rng.next(2) == 0 {
            ring(&[
                (0., 0.),
                (grid as f64 * scale, 0.),
                (grid as f64 * scale, grid as f64 * scale),
                (0., grid as f64 * scale),
                (0., 0.),
            ])
        } else {
            tidy_ring(rng, scale, (0, grid, 6, free))
        };
        let holes = (0..1 + rng.next(4))
            .map(|_| {
                // Mostly holes in a sub-box so they sit inside the shell.
                let (lo, hi) = match rng.next(6) {
                    0 => (0, grid),
                    1 => (grid / 3, grid),
                    _ => (grid / 4, grid - grid / 4),
                };
                if rng.next(40) == 0 {
                    ring(&[])
                } else {
                    tidy_ring(rng, scale, (lo, hi, 4, free))
                }
            })
            .collect();
        Polygon::new(shell, holes)
    }
    #[test]
    fn holed_polygon_valid_matches_geo_on_random_polygons() {
        let mut rng = Lcg(2024);
        let mut reasons = BTreeMap::<String, usize>::new();
        // (grid, scale): the 0.1 and 0.3 scales give inexact floats.
        // The offset moves the lattice away from the origin, where scaled and
        // offset coordinates are not exactly representable.
        for (grid, scale, offset) in [
            (6, 1., 0.),
            (10, 1., 0.),
            (24, 0.1, 0.),
            (8, 0.3, 0.),
            (40, 1., 0.),
            (12, 0.1, 123456.7),
            (9, 1. / 3., -77.7),
        ] {
            let (mut valid, mut invalid, mut decided_valid, mut decided_invalid) = (0, 0, 0, 0);
            for _ in 0..30000 {
                let polygon = random_holed_polygon(&mut rng, grid, scale);
                let shift = |ring: &LineString<f64>| {
                    LineString::new(
                        ring.0
                            .iter()
                            .map(|c| geo_types::Coord {
                                x: c.x + offset,
                                y: c.y - offset,
                            })
                            .collect(),
                    )
                };
                let polygon = Polygon::new(
                    shift(polygon.exterior()),
                    polygon.interiors().iter().map(shift).collect(),
                );
                let expected = polygon.is_valid();
                assert_eq!(polygon_valid(&polygon), expected, "{polygon:?}");
                let decided = holed_polygon_valid(&polygon);
                assert!(decided.is_none_or(|d| d == expected), "{polygon:?}");
                match (expected, decided.is_some()) {
                    (true, false) => valid += 1,
                    (false, false) => invalid += 1,
                    (true, true) => {
                        valid += 1;
                        decided_valid += 1;
                    }
                    (false, true) => {
                        invalid += 1;
                        decided_invalid += 1;
                        let reason = format!("{:?}", polygon.validation_errors()[0]);
                        *reasons
                            .entry(reason.split('(').next().unwrap().to_string())
                            .or_default() += 1;
                    }
                }
            }
            eprintln!(
                "grid {grid} scale {scale} offset {offset}: {valid} valid ({decided_valid} decided), \
                 {invalid} invalid ({decided_invalid} decided)"
            );
            assert!(
                decided_valid > 300 && decided_invalid > 300 && valid > 1000 && invalid > 1000,
                "grid {grid} offset {offset}"
            );
        }
        eprintln!("decided invalid by first geo error: {reasons:?}");
        for reason in [
            "InteriorRingNotContainedInExteriorRing",
            "IntersectingRingsOnAnArea",
            "SelfIntersection",
            "TooFewPointsInRing",
        ] {
            assert!(reasons.get(reason).is_some_and(|&n| n > 100), "{reason}");
        }
    }
    #[test]
    fn holed_polygon_valid_matches_geo_on_large_float_stars() {
        let mut rng = Lcg(77);
        let star = |rng: &mut Lcg, cx: f64, cy: f64, radius: f64, n: usize| {
            let mut points = (0..n)
                .map(|i| {
                    let angle = (i as f64 + 0.5 * rng.next(100) as f64 / 100.) / n as f64
                        * std::f64::consts::TAU;
                    let r = radius * (0.7 + 0.3 * rng.next(1000) as f64 / 1000.);
                    (cx + r * angle.cos(), cy + r * angle.sin())
                })
                .collect::<Vec<_>>();
            points.push(points[0]);
            ring(&points)
        };
        let (mut valid, mut invalid, mut decided) = (0, 0, 0);
        for _ in 0..4000 {
            let n = 20 + rng.next(180) as usize;
            let shell = star(&mut rng, 0., 0., 10., n);
            let holes = (0..1 + rng.next(5))
                .map(|_| {
                    let (cx, cy) = (
                        rng.next(1000) as f64 / 100. - 5.,
                        rng.next(1000) as f64 / 100. - 5.,
                    );
                    let radius = 0.2 + rng.next(300) as f64 / 100.;
                    let n = 4 + rng.next(60) as usize;
                    star(&mut rng, cx, cy, radius, n)
                })
                .collect();
            let polygon = Polygon::new(shell, holes);
            let expected = polygon.is_valid();
            assert_eq!(polygon_valid(&polygon), expected, "{polygon:?}");
            decided += usize::from(holed_polygon_valid(&polygon).is_some());
            if expected {
                valid += 1;
            } else {
                invalid += 1;
            }
        }
        assert!(
            valid > 300 && invalid > 300 && decided > 1000,
            "{valid} {invalid} {decided}"
        );
    }
    #[test]
    fn holed_polygon_valid_matches_geo_on_constructed_cases() {
        let square_ring = |x: f64, y: f64, size: f64| {
            ring(&[
                (x, y),
                (x + size, y),
                (x + size, y + size),
                (x, y + size),
                (x, y),
            ])
        };
        let big = || square_ring(0., 0., 10.);
        let cases = vec![
            // Hole inside, outside, around, nested, touching, overlapping.
            vec![square_ring(2., 2., 3.)],
            vec![square_ring(20., 20., 3.)],
            vec![square_ring(-5., -5., 20.)],
            vec![square_ring(2., 2., 6.), square_ring(4., 4., 2.)],
            vec![square_ring(2., 2., 6.), square_ring(1., 1., 8.)],
            vec![square_ring(2., 2., 3.), square_ring(5., 2., 3.)],
            vec![square_ring(2., 2., 3.), square_ring(5., 5., 3.)],
            vec![square_ring(2., 2., 3.), square_ring(4., 4., 3.)],
            vec![square_ring(0., 2., 3.)],
            vec![square_ring(0., 0., 3.)],
            vec![square_ring(7., 7., 3.), square_ring(1., 1., 2.)],
            vec![square_ring(2., 2., 3.), square_ring(2., 2., 3.)],
            vec![ring(&[(2., 2.), (4., 2.), (6., 2.), (2., 2.)])],
            vec![ring(&[(2., 2.), (4., 4.), (6., 6.), (2., 2.)])],
            vec![ring(&[]), square_ring(2., 2., 3.)],
            vec![ring(&[(2., 2.), (3., 3.)])],
            vec![ring(&[
                (2., 2.),
                (f64::NAN, 3.),
                (5., 5.),
                (2., 5.),
                (2., 2.),
            ])],
        ];
        for holes in cases {
            let polygon = Polygon::new(big(), holes);
            assert_eq!(polygon_valid(&polygon), polygon.is_valid(), "{polygon:?}");
        }
        let polygon = Polygon::new(ring(&[]), vec![square_ring(2., 2., 3.)]);
        assert_eq!(polygon_valid(&polygon), polygon.is_valid());
        assert_eq!(holed_polygon_valid(&polygon), Some(true));
    }
    #[test]
    fn multi_polygon_matches_geo_on_constructed_cases() {
        let hole = ring(&[(2., 2.), (2., 8.), (8., 8.), (8., 2.), (2., 2.)]);
        let holed = Polygon::new(shell(), vec![hole]);
        let cases = vec![
            vec![],
            vec![square(0., 0., 10.)],
            // Disjoint, rects disjoint.
            vec![square(0., 0., 10.), square(20., 20., 5.)],
            // Rects disjoint on one axis only.
            vec![square(0., 0., 10.), square(20., 0., 10.)],
            // Corner touch (valid) and edge touch (touch on a line, invalid).
            vec![square(0., 0., 10.), square(10., 10., 5.)],
            vec![square(0., 0., 10.), square(10., 0., 10.)],
            vec![square(0., 0., 10.), square(10., 2., 5.)],
            // Overlapping and identical.
            vec![square(0., 0., 10.), square(5., 5., 10.)],
            vec![square(0., 0., 10.), square(0., 0., 10.)],
            // Nested without touching, and nested inside a hole.
            vec![square(0., 0., 10.), square(2., 2., 3.)],
            vec![holed.clone(), square(4., 4., 2.)],
            vec![holed.clone(), square(2., 2., 6.)],
            vec![holed.clone(), square(8., 8., 1.), square(20., 20., 1.)],
            // One invalid member.
            vec![
                square(0., 0., 1.),
                Polygon::new(
                    ring(&[(0., 0.), (4., 0.), (0., 2.), (4., 2.), (0., 0.)]),
                    vec![],
                ),
            ],
            vec![square(0., 0., 1.), Polygon::new(ring(&[]), vec![])],
        ];
        for case in cases {
            assert_matches_geo(case);
        }
    }
    /// A polygon (half of them with holes) that is geo-valid, apart from a few
    /// invalid ones, shifted by a whole number of lattice steps plus `offset`.
    fn random_member(rng: &mut Lcg, grid: u64, scale: f64, offset: f64) -> Polygon<f64> {
        let (dx, dy) = (
            rng.next(grid + 1) as f64 * scale,
            rng.next(grid + 1) as f64 * scale,
        );
        let shift = |ring: &LineString<f64>| {
            LineString::new(
                ring.0
                    .iter()
                    .map(|c| geo_types::Coord {
                        x: c.x + dx + offset,
                        y: c.y + dy - offset,
                    })
                    .collect(),
            )
        };
        loop {
            let base = if rng.next(2) == 0 {
                random_holed_polygon_base(rng, grid, scale)
            } else {
                Polygon::new(tidy_ring(rng, scale, (0, grid, 6, false)), vec![])
            };
            if rng.next(20) == 0 || base.is_valid() {
                return Polygon::new(
                    shift(base.exterior()),
                    base.interiors().iter().map(shift).collect(),
                );
            }
        }
    }
    #[test]
    fn polygons_conflict_matches_geo_relate() {
        let mut rng = Lcg(31337);
        let mut reasons = BTreeMap::<&str, usize>::new();
        for (grid, scale, offset) in [
            (6, 1., 0.),
            (10, 1., 0.),
            (20, 0.1, 0.),
            (8, 0.3, 0.),
            (12, 0.1, 123456.7),
            (9, 1. / 3., -77.7),
        ] {
            for _ in 0..25000 {
                let p = random_member(&mut rng, grid, scale, offset);
                let q = random_member(&mut rng, grid, scale, offset);
                if !polygon_valid(&p) || !polygon_valid(&q) {
                    continue;
                }
                let (Some(p_rect), Some(q_rect)) = (p.bounding_rect(), q.bounding_rect()) else {
                    continue;
                };
                if p_rect.max().x < q_rect.min().x
                    || q_rect.max().x < p_rect.min().x
                    || p_rect.max().y < q_rect.min().y
                    || q_rect.max().y < p_rect.min().y
                {
                    continue;
                }
                let matrix = p.relate(&q);
                let expected = matrix.get(CoordPos::Inside, CoordPos::Inside)
                    == Dimensions::TwoDimensional
                    || matrix.get(CoordPos::OnBoundary, CoordPos::OnBoundary)
                        == Dimensions::OneDimensional;
                assert_eq!(
                    polygons_conflict(&p, &q, p_rect, q_rect),
                    expected,
                    "{p:?} {q:?}"
                );
                let decided = polygons_conflict_fast(&p, &q, p_rect, q_rect);
                assert!(decided.is_none_or(|d| d == expected), "{p:?} {q:?}");
                *reasons
                    .entry(match (expected, decided.is_some()) {
                        (true, true) => "conflict decided",
                        (false, true) => "separate decided",
                        (true, false) => "conflict by geo",
                        (false, false) => "separate by geo",
                    })
                    .or_default() += 1;
            }
        }
        eprintln!("pairs: {reasons:?}");
        assert!(reasons.values().all(|&n| n > 500), "{reasons:?}");
    }
    #[test]
    fn multi_polygon_matches_geo_on_random_valid_members() {
        let mut rng = Lcg(4242);
        let (mut valid, mut invalid) = (0, 0);
        for (grid, scale, offset) in [
            (6, 1., 0.),
            (10, 1., 0.),
            (20, 0.1, 0.),
            (9, 1. / 3., -77.7),
        ] {
            for _ in 0..8000 {
                let count = 2 + rng.next(3) as usize;
                let multi = MultiPolygon(
                    (0..count)
                        .map(|_| random_member(&mut rng, grid, scale, offset))
                        .collect(),
                );
                let expected = multi.is_valid();
                assert_eq!(multi_polygon_valid(&multi), expected, "{multi:?}");
                if expected {
                    valid += 1;
                } else {
                    invalid += 1;
                }
            }
        }
        assert!(valid > 500 && invalid > 500, "{valid} {invalid}");
    }
    #[test]
    fn multi_polygon_matches_geo_on_random_grid_squares() {
        let mut rng = Lcg(12345);
        for _ in 0..3000 {
            let count = 2 + rng.next(4) as usize;
            let polygons = (0..count)
                .map(|_| {
                    square(
                        rng.next(8) as f64,
                        rng.next(8) as f64,
                        1. + rng.next(4) as f64,
                    )
                })
                .collect();
            assert_matches_geo(polygons);
        }
    }
    fn shell() -> LineString<f64> {
        ring(&[(0., 0.), (10., 0.), (10., 10.), (0., 10.), (0., 0.)])
    }
    #[test]
    fn separate_shell_contacts_disconnect_interior() {
        let polygon = Polygon::new(
            shell(),
            vec![ring(&[(0., 5.), (5., 3.), (10., 5.), (5., 7.), (0., 5.)])],
        );
        assert!(
            polygon.is_valid(),
            "geo omits the connected-interior condition"
        );
        assert!(!is_valid(&MultiPolygon(vec![polygon])));
        let polygon = Polygon::new(
            shell(),
            vec![ring(&[(0., 2.), (2., 3.), (1., 4.), (0., 2.)])],
        );
        assert!(connected_interior(&polygon));
    }
    #[test]
    fn shared_contact_is_one_graph_node() {
        let polygon = Polygon::new(
            shell(),
            vec![
                ring(&[(5., 5.), (2., 4.), (2., 6.), (5., 5.)]),
                ring(&[(5., 5.), (6., 2.), (4., 2.), (5., 5.)]),
                ring(&[(5., 5.), (8., 6.), (8., 4.), (5., 5.)]),
            ],
        );
        assert!(is_valid(&MultiPolygon(vec![polygon])));
    }
}
