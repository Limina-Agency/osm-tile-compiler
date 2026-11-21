//! Polygon buffer, union and grid snapping in f64 that follow the code paths
//! of the pinned JTS 1.20 and Planetiler 0.10.2 that
//! `FeatureMerge.mergeNearbyPolygons` takes, closely enough to reproduce its
//! output vertex for vertex in almost every cluster.
//!
//! - Buffer (`BufferOp`, mitre joins): offset curves port
//!   `BufferCurveSetBuilder`, `OffsetCurveBuilder`, `OffsetSegmentGenerator`
//!   and `BufferInputLineSimplifier`, each ring keeping its own start and
//!   direction. Noding follows `MCIndexNoder` with `RobustLineIntersector`,
//!   whose proper intersections are the double-double
//!   `CGAlgorithmsDD.intersection`. `BufferBuilder` is followed on JTS's own
//!   graph, with nodes only where the noder put them: `EdgeEndStar` order
//!   (and its replacement of an end by a collinear one), `BufferSubgraph`
//!   depth propagation from `RightmostEdgeFinder`, result edges, and
//!   `PolygonBuilder`'s maximal and minimal rings. Where JTS throws a
//!   `TopologyException`, the buffer is retried as `BufferOp` does: snap
//!   rounded on a grid of 12 and fewer significant digits.
//! - Union (`Geometry.union()`, the classic overlay by default): the
//!   `CascadedPolygonUnion` STR-tree and binary unions, each through
//!   `SnapIfNeededOverlayOp`, so a pair whose noding fails is snapped with
//!   `CommonBitsRemover` and `GeometrySnapper` and overlaid again.
//! - Snapping (`GeoUtils.snapAndFixPolygon`): pointwise rounding, or JTS's
//!   `GeometryPrecisionReducer` (a snap-rounding union) where that is invalid,
//!   of the input or of its zero buffer where the input is invalid too.
//! - Zero-distance buffers (`mergeOverlappingPolygons`, for landcover, park and
//!   water) with `BufferBuilder`'s noding validation.
//! - Java's `Math.hypot` and `Math.atan2` (fdlibm) bit for bit, and correctly
//!   rounded `Math.sin` and `Math.cos`, where the platform functions round
//!   differently.
//!
//! Rings come out in JTS orientation, clockwise shells and counter-clockwise
//! holes, and with JTS's start vertex, which later simplification depends on.
//! Coordinates are MVT units, 16 per tile pixel; scaling by a power of two
//! leaves every operation bit for bit equal to JTS's in pixels.
//!
//! Adapted from JTS (Copyright (c) 2016 Martin Davis and Vivid Solutions,
//! SPDX: EDL-1.0) and Planetiler (Apache-2.0). See
//! `src/omt/LICENSE.jts-edl-1.0.txt` and `src/omt/LICENSE.planetiler`.

use rustc_hash::{FxHashMap as HashMap, FxHashSet as HashSet};

use geo::Orientation;
use geo::kernels::{Kernel, RobustKernel};
use geo_types::{Coord, LineString, MultiPolygon, Polygon};

type Point = [f64; 2];

const MITRE_LIMIT: f64 = 5.0;
const SIMPLIFY_FACTOR: f64 = 0.01;
const OFFSET_SEGMENT_SEPARATION_FACTOR: f64 = 0.05;
const INSIDE_TURN_VERTEX_SNAP_DISTANCE_FACTOR: f64 = 1.0e-3;
const CURVE_VERTEX_SNAP_DISTANCE_FACTOR: f64 = 1.0e-6;
/// Bounds the quadratic parts (hole assignment, nested components).
const MAX_SEGMENTS: usize = 1 << 18;

/// `BufferOp(geometry, JOIN_MITRE).getResultGeometry(distance)` for polygons,
/// in MVT units (16 per tile pixel, the unit Planetiler's JTS works in).
/// `None` when the input is outside the bounds this implementation supports.
pub(super) fn buffer(input: &MultiPolygon<f64>, distance: f64) -> Option<MultiPolygon<f64>> {
    if !distance.is_finite() {
        return None;
    }
    if let Some(result) = buffer_with(input, distance, None) {
        return Some(result);
    }
    // BufferOp.bufferReducedPrecision: snap-rounded noding on a grid scaled
    // to the envelope, from 12 significant digits down.
    let env_max = input
        .0
        .iter()
        .flat_map(|p| {
            p.exterior()
                .0
                .iter()
                .chain(p.interiors().iter().flat_map(|r| &r.0))
        })
        .map(|c| (c.x / PIXEL).abs().max((c.y / PIXEL).abs()))
        .fold(0.0, f64::max);
    let expand = if distance > 0.0 {
        distance / PIXEL
    } else {
        0.0
    };
    let env_digits = ((env_max + 2.0 * expand).ln() / 10f64.ln() + 1.0) as i32;
    for digits in (0..=12).rev() {
        let scale = format!("1e{}", digits - env_digits).parse::<f64>().ok()?;
        if let Some(result) = buffer_with(input, distance, Some(scale)) {
            return Some(result);
        }
    }
    None
}

/// MVT units per tile pixel.
const PIXEL: f64 = 16.0;

fn buffer_with(
    input: &MultiPolygon<f64>,
    distance: f64,
    scale: Option<f64>,
) -> Option<MultiPolygon<f64>> {
    let mut curves = Vec::new();
    for polygon in &input.0 {
        add_polygon(polygon, distance, scale, &mut curves);
    }
    let noded = match scale {
        None => node_float(&curves)?,
        Some(scale) => node_snap_round(&curves, scale, true)?,
    };
    // BufferBuilder checks the noding of a zero-distance buffer with a
    // FastNodingValidator and throws where it is incomplete.
    if distance == 0.0 && !noding_valid(&noded)? {
        return None;
    }
    build(noded, Mode::Buffer)
}

/// `FastNodingValidator.checkValid` on noded substrings: through
/// `NodingIntersectionFinder`, no two segments may meet inside either one,
/// and two segments that are not adjacent in one string may share a vertex
/// only where it ends both strings. `None` beyond the size bound.
fn noding_valid(noded: &[Noded]) -> Option<bool> {
    struct Segment {
        a: Point,
        b: Point,
        string: usize,
        index: usize,
        a_end: bool,
        b_end: bool,
    }
    let mut segments = Vec::new();
    for (string, edge) in noded.iter().enumerate() {
        let n = edge.points.len();
        for (index, pair) in edge.points.windows(2).enumerate() {
            segments.push(Segment {
                a: pair[0],
                b: pair[1],
                string,
                index,
                a_end: index == 0,
                b_end: index + 2 == n,
            });
        }
    }
    if segments.len() > MAX_SEGMENTS {
        return None;
    }
    let min_x = |s: &Segment| s.a[0].min(s.b[0]);
    let mut order = (0..segments.len()).collect::<Vec<_>>();
    order.sort_by(|&i, &j| min_x(&segments[i]).total_cmp(&min_x(&segments[j])));
    for (position, &i) in order.iter().enumerate() {
        let s = &segments[i];
        let max_x = s.a[0].max(s.b[0]);
        for &j in &order[position + 1..] {
            let t = &segments[j];
            if min_x(t) > max_x {
                break;
            }
            // LineIntersector.isInteriorIntersection
            let interior = intersections(s.a, s.b, t.a, t.b)
                .iter()
                .any(|p| (*p != s.a && *p != s.b) || (*p != t.a && *p != t.b));
            if interior {
                return Some(false);
            }
            if s.string == t.string && s.index.abs_diff(t.index) <= 1 {
                continue;
            }
            for (p, p_end) in [(s.a, s.a_end), (s.b, s.b_end)] {
                for (q, q_end) in [(t.a, t.a_end), (t.b, t.b_end)] {
                    if !(p_end && q_end) && p == q {
                        return Some(false);
                    }
                }
            }
        }
    }
    Some(true)
}

/// `PrecisionModel(scale).makePrecise` on tile-pixel ordinates.
fn precise(p: Point, scale: Option<f64>) -> Point {
    match scale {
        None => p,
        Some(scale) => p.map(|v| java_round(v / PIXEL * scale) / scale * PIXEL),
    }
}

/// `Math.round` as a double.
fn java_round(v: f64) -> f64 {
    (v + 0.5).floor()
}

/// `FeatureMerge.bufferUnionUnbuffer` for one proximity cluster: each polygon
/// buffered by `distance`, the union, and the union buffered back. A large
/// cluster that lost too much area falls back to unbuffering the buffered
/// collection, or to the plain union.
pub(super) fn buffer_union_unbuffer(
    cluster: &[MultiPolygon<f64>],
    distance: f64,
    drop_below: f64,
) -> Option<MultiPolygon<f64>> {
    use geo::Area;
    let expanded = cluster
        .iter()
        .map(|g| buffer(g, distance))
        .collect::<Option<Vec<_>>>()?;
    if drop_below > 0.0
        && distance > 1.0
        && eroded_hull_area(&expanded, distance - 0.8) <= drop_below
    {
        return Some(MultiPolygon(vec![]));
    }
    let mut merged = buffer(&union(&expanded)?, -distance)?;
    let largest = cluster
        .iter()
        .map(Area::unsigned_area)
        .fold(0_f64, f64::max);
    // 100 tile pixels squared.
    if largest > 100.0 * PIXEL * PIXEL && merged.unsigned_area() < largest * 0.95 {
        let collection = MultiPolygon(expanded.into_iter().flat_map(|g| g.0).collect());
        let alternate = buffer(&collection, -distance)?;
        if alternate.unsigned_area() < largest * 0.95 {
            return union(cluster);
        }
        if alternate.unsigned_area() > merged.unsigned_area() {
            merged = alternate;
        }
    }
    Some(merged)
}

/// An upper bound on the area of anything the unbuffered union of `expanded`
/// can cover: the area of the convex hull of the buffered polygons, eroded by
/// `amount`. The negative mitre buffer of a region lies within its erosion by
/// a disc (a mitre or bevel join removes more than a round one), and the
/// erosion of a region lies within the erosion of its convex hull. `amount`
/// is the buffer distance less the slack for rounding to the grid (0.71) and
/// for the input simplification (0.01 of the distance).
fn eroded_hull_area(expanded: &[MultiPolygon<f64>], amount: f64) -> f64 {
    let mut points = Vec::<Point>::new();
    for g in expanded {
        for p in &g.0 {
            points.extend(p.exterior().0.iter().map(point));
        }
    }
    if points.len() < 3
        || points
            .iter()
            .any(|p| !p[0].is_finite() || !p[1].is_finite())
    {
        return f64::INFINITY;
    }
    points.sort_unstable_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    points.dedup();
    let cross = |o: Point, a: Point, b: Point| {
        (a[0] - o[0]) * (b[1] - o[1]) - (a[1] - o[1]) * (b[0] - o[0])
    };
    // Andrew's monotone chain: counter-clockwise, without collinear points.
    let mut hull = Vec::<Point>::with_capacity(points.len() + 1);
    for reverse in [false, true] {
        let start = hull.len();
        for i in 0..points.len() {
            let p = points[if reverse { points.len() - 1 - i } else { i }];
            while hull.len() >= start + 2
                && cross(hull[hull.len() - 2], hull[hull.len() - 1], p) <= 0.0
            {
                hull.pop();
            }
            hull.push(p);
        }
        hull.pop();
    }
    if hull.len() < 3 {
        return 0.0;
    }
    let mut polygon = hull.clone();
    let mut next = Vec::with_capacity(polygon.len() + 2);
    for i in 0..hull.len() {
        let (a, b) = (hull[i], hull[(i + 1) % hull.len()]);
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let length = dx.hypot(dy);
        let side = |c: Point| ((c[0] - a[0]) * -dy + (c[1] - a[1]) * dx) / length - amount;
        next.clear();
        for j in 0..polygon.len() {
            let (p, q) = (polygon[j], polygon[(j + 1) % polygon.len()]);
            let (sp, sq) = (side(p), side(q));
            if sp >= 0.0 {
                next.push(p);
            }
            if (sp >= 0.0) != (sq >= 0.0) {
                let t = sp / (sp - sq);
                next.push([p[0] + t * (q[0] - p[0]), p[1] + t * (q[1] - p[1])]);
            }
        }
        std::mem::swap(&mut polygon, &mut next);
        if polygon.len() < 3 {
            return 0.0;
        }
    }
    let mut twice = 0.0;
    for j in 0..polygon.len() {
        let (p, q) = (polygon[j], polygon[(j + 1) % polygon.len()]);
        twice += p[0] * q[1] - q[0] * p[1];
    }
    twice.abs() / 2.0
}

/// `Geometry.union()` of a collection of valid polygons: JTS's classic
/// `UnaryUnionOp`, which unions them through `CascadedPolygonUnion`'s
/// STR-tree in binary steps. Each step nodes only its two operands.
pub(super) fn union(inputs: &[MultiPolygon<f64>]) -> Option<MultiPolygon<f64>> {
    let polygons = inputs
        .iter()
        .flat_map(|m| m.0.iter().cloned())
        .collect::<Vec<_>>();
    if polygons.is_empty() {
        return Some(MultiPolygon(vec![]));
    }
    let tree = str_tree(polygons.iter().map(envelope_of).collect());
    Some(union_tree(&tree, &polygons)?.unwrap_or_else(|| MultiPolygon(vec![])))
}

/// Planetiler's `GeoUtils.snapAndFixPolygon` onto the MVT grid (one unit):
/// pointwise rounding when that stays valid, otherwise JTS's
/// `GeometryPrecisionReducer`, an OverlayNG union of the snap-rounded rings,
/// of the input or, where the input is invalid, of its zero buffer.
/// Rings come out with integer coordinates.
pub(super) fn snap_and_fix(input: &MultiPolygon<f64>) -> Option<MultiPolygon<f64>> {
    // GeoUtils.PointwiseRounder: a ring of fewer than four points after
    // dropping repeated rounded points disappears, and with its shell the
    // whole polygon.
    let round_ring = |ring: &LineString<f64>| {
        if ring.0.len() < 4 {
            return None;
        }
        let mut points = Vec::<Point>::with_capacity(ring.0.len());
        for c in &ring.0 {
            let p = [java_round(c.x), java_round(c.y)];
            if points.last() != Some(&p) {
                points.push(p);
            }
        }
        (points.len() >= 4).then_some(points)
    };
    let line = |ring: Vec<Point>| {
        LineString::new(
            ring.into_iter()
                .map(|p| Coord { x: p[0], y: p[1] })
                .collect(),
        )
    };
    let pointwise = MultiPolygon(
        input
            .0
            .iter()
            .filter_map(|p| {
                let shell = round_ring(p.exterior())?;
                let holes = p
                    .interiors()
                    .iter()
                    .filter_map(round_ring)
                    .map(line)
                    .collect();
                Some(Polygon::new(line(shell), holes))
            })
            .collect(),
    );
    if jts_valid(&pointwise) {
        return Some(pointwise);
    }
    // An invalid input is first repaired with GeoUtils.fixPolygon, a zero
    // buffer, so that, for example, a hole the erosion left outside its
    // shell does not reach the precision reducer.
    let repaired;
    let input = if jts_valid(input) {
        input
    } else {
        repaired = buffer(input, 0.0)?;
        &repaired
    };
    // PrecisionReducer.reducePrecision: OverlayNG with a SnapRoundingNoder
    // at the target precision.
    let mut curves = Vec::new();
    for polygon in &input.0 {
        let rings = std::iter::once((polygon.exterior(), true))
            .chain(polygon.interiors().iter().map(|ring| (ring, false)));
        for (ring, shell) in rings {
            let points = clean(ring);
            if points.len() < 4 {
                continue;
            }
            curves.push(area_curve(points, shell));
        }
    }
    build(node_snap_round(&curves, PIXEL, false)?, Mode::Overlay)
}

/// JTS `Geometry.isValid` for the polygons `snap_and_fix` checks. The geo
/// rules of `polygon_validity` skip segment pairs that share an end, so they
/// accept a ring that turns back on itself at a vertex (a collinear ring among
/// them), which JTS reports as a self-intersection.
fn jts_valid(polygons: &MultiPolygon<f64>) -> bool {
    let doubles_back = |ring: &LineString<f64>| {
        let points = &ring.0;
        let n = points.len().saturating_sub(1);
        (0..n).any(|i| {
            let (a, b, c) = (
                point(&points[(i + n - 1) % n]),
                point(&points[i]),
                point(&points[i + 1]),
            );
            let (u, v) = ([a[0] - b[0], a[1] - b[1]], [c[0] - b[0], c[1] - b[1]]);
            orientation(a, b, c) == Orientation::Collinear && u[0] * v[0] + u[1] * v[1] > 0.0
        })
    };
    super::polygon_validity::is_valid(polygons)
        && !polygons
            .0
            .iter()
            .flat_map(|p| std::iter::once(p.exterior()).chain(p.interiors()))
            .any(doubles_back)
}

type Envelope = ([f64; 2], [f64; 2]);

fn envelope_of(polygon: &Polygon<f64>) -> Envelope {
    let (mut min, mut max) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    for c in &polygon.exterior().0 {
        for (axis, v) in [c.x, c.y].into_iter().enumerate() {
            min[axis] = min[axis].min(v);
            max[axis] = max[axis].max(v);
        }
    }
    (min, max)
}

enum Tree {
    Item(usize),
    Node(Vec<Tree>, Envelope),
}

impl Tree {
    fn envelope(&self, items: &[Envelope]) -> Envelope {
        match self {
            Tree::Item(i) => items[*i],
            Tree::Node(_, e) => *e,
        }
    }
}

/// `STRtree(4)` packing, as `itemsTree()` returns it.
fn str_tree(items: Vec<Envelope>) -> Tree {
    const CAPACITY: usize = 4;
    let mut level = (0..items.len()).map(Tree::Item).collect::<Vec<_>>();
    loop {
        let centre = |t: &Tree, axis: usize| {
            let (min, max) = t.envelope(&items);
            (min[axis] + max[axis]) / 2.0
        };
        let min_leaf_count = level.len().div_ceil(CAPACITY);
        let slice_count = (min_leaf_count as f64).sqrt().ceil() as usize;
        level.sort_by(|a, b| centre(a, 0).total_cmp(&centre(b, 0)));
        let slice_capacity = level.len().div_ceil(slice_count);
        let mut parents = Vec::new();
        let mut rest = level.into_iter();
        for _ in 0..slice_count {
            let mut slice = rest.by_ref().take(slice_capacity).collect::<Vec<_>>();
            slice.sort_by(|a, b| centre(a, 1).total_cmp(&centre(b, 1)));
            let mut slice = slice.into_iter().peekable();
            while slice.peek().is_some() {
                let children = slice.by_ref().take(CAPACITY).collect::<Vec<_>>();
                let mut env = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
                for child in &children {
                    let (min, max) = child.envelope(&items);
                    for axis in 0..2 {
                        env.0[axis] = env.0[axis].min(min[axis]);
                        env.1[axis] = env.1[axis].max(max[axis]);
                    }
                }
                parents.push(Tree::Node(children, env));
            }
        }
        if parents.len() == 1 {
            return parents.pop().expect("one root");
        }
        level = parents;
    }
}

/// `CascadedPolygonUnion.unionTree`.
fn union_tree(tree: &Tree, polygons: &[Polygon<f64>]) -> Option<Option<MultiPolygon<f64>>> {
    let Tree::Node(children, _) = tree else {
        let Tree::Item(i) = tree else { unreachable!() };
        return Some(Some(MultiPolygon(vec![polygons[*i].clone()])));
    };
    let geometries = children
        .iter()
        .map(|child| match child {
            Tree::Item(i) => Some(Some(MultiPolygon(vec![polygons[*i].clone()]))),
            node => union_tree(node, polygons),
        })
        .collect::<Option<Vec<_>>>()?;
    binary_union(&geometries)
}

/// `CascadedPolygonUnion.binaryUnion`.
fn binary_union(geometries: &[Option<MultiPolygon<f64>>]) -> Option<Option<MultiPolygon<f64>>> {
    match geometries.len() {
        0 => Some(None),
        1 => Some(geometries[0].clone()),
        2 => union_pair(geometries[0].as_ref(), geometries[1].as_ref()),
        n => {
            let mid = n / 2;
            let a = binary_union(&geometries[..mid])?;
            let b = binary_union(&geometries[mid..])?;
            union_pair(a.as_ref(), b.as_ref())
        }
    }
}

fn union_pair(
    a: Option<&MultiPolygon<f64>>,
    b: Option<&MultiPolygon<f64>>,
) -> Option<Option<MultiPolygon<f64>>> {
    let (a, b) = match (a, b) {
        (None, None) => return Some(None),
        (Some(g), None) | (None, Some(g)) => return Some(Some(g.clone())),
        (Some(a), Some(b)) => (a, b),
    };
    // SnapIfNeededOverlayOp: the plain overlay, then SnapOverlayOp.
    if let Some(result) = overlay_union(a, b) {
        return Some(Some(result));
    }
    Some(Some(snap_overlay_union(a, b)?))
}

fn overlay_union(a: &MultiPolygon<f64>, b: &MultiPolygon<f64>) -> Option<MultiPolygon<f64>> {
    let mut curves = Vec::new();
    for polygon in a.0.iter().chain(&b.0) {
        let rings = std::iter::once((polygon.exterior(), true))
            .chain(polygon.interiors().iter().map(|ring| (ring, false)));
        for (ring, shell) in rings {
            let points = clean(ring);
            if points.len() < 4 {
                continue;
            }
            curves.push(area_curve(points, shell));
        }
    }
    polygonize(curves, Mode::Overlay)
}

fn map_rings(g: &MultiPolygon<f64>, f: impl Fn(&[Point]) -> Vec<Point>) -> MultiPolygon<f64> {
    let line = |r: &LineString<f64>| {
        let points = r.0.iter().map(point).collect::<Vec<_>>();
        LineString::new(
            f(&points)
                .into_iter()
                .map(|p| Coord { x: p[0], y: p[1] })
                .collect(),
        )
    };
    MultiPolygon(
        g.0.iter()
            .map(|p| Polygon::new(line(p.exterior()), p.interiors().iter().map(line).collect()))
            .collect(),
    )
}

/// `SnapOverlayOp.overlayOp(a, b, UNION)`: common bits removed, each operand
/// snapped to the other's vertices (`GeometrySnapper.snap`), overlay, and the
/// common bits added back.
fn snap_overlay_union(a: &MultiPolygon<f64>, b: &MultiPolygon<f64>) -> Option<MultiPolygon<f64>> {
    let coords = || {
        a.0.iter().chain(&b.0).flat_map(|p| {
            p.exterior()
                .0
                .iter()
                .chain(p.interiors().iter().flat_map(|r| &r.0))
                .map(point)
        })
    };
    let common = [0, 1].map(|axis| common_bits(coords().map(|p| p[axis])));
    let shift = |sign: f64| {
        move |ring: &[Point]| {
            ring.iter()
                .map(|p| [p[0] + sign * common[0], p[1] + sign * common[1]])
                .collect::<Vec<_>>()
        }
    };
    let (a, b) = if common == [0.0, 0.0] {
        (a.clone(), b.clone())
    } else {
        (map_rings(a, shift(-1.0)), map_rings(b, shift(-1.0)))
    };
    // GeometrySnapper.computeOverlaySnapTolerance
    let tolerance = |g: &MultiPolygon<f64>| {
        let (mut min, mut max) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
        for p in &g.0 {
            let (lo, hi) = envelope_of(p);
            for axis in 0..2 {
                min[axis] = min[axis].min(lo[axis]);
                max[axis] = max[axis].max(hi[axis]);
            }
        }
        (max[1] - min[1]).min(max[0] - min[0]) * 1e-9
    };
    let tolerance = tolerance(&a).min(tolerance(&b));
    let a = snap_to(&a, &b, tolerance);
    let b = snap_to(&b, &a, tolerance);
    let result = overlay_union(&a, &b)?;
    Some(if common == [0.0, 0.0] {
        result
    } else {
        map_rings(&result, shift(1.0))
    })
}

/// `CommonBits`: the common sign, exponent and leading mantissa bits.
fn common_bits(values: impl Iterator<Item = f64>) -> f64 {
    let mut common = None::<(u64, u64)>;
    for v in values {
        let bits = v.to_bits();
        let Some((value, sign_exp)) = common else {
            common = Some((bits, bits >> 52));
            continue;
        };
        if bits >> 52 != sign_exp {
            common = Some((0, sign_exp));
            continue;
        }
        let mut count = 0;
        for i in (0..=52).rev() {
            if (value >> i) & 1 != (bits >> i) & 1 {
                break;
            }
            count += 1;
        }
        let count = count.min(52);
        let low = 64 - (12 + count);
        let mask = if low >= 64 { 0 } else { !((1u64 << low) - 1) };
        common = Some((value & mask, sign_exp));
    }
    f64::from_bits(common.map_or(0, |(value, _)| value))
}

/// `GeometrySnapper(source).snapTo(target, tolerance)`.
fn snap_to(
    source: &MultiPolygon<f64>,
    target: &MultiPolygon<f64>,
    tolerance: f64,
) -> MultiPolygon<f64> {
    // extractTargetCoordinates: a sorted set.
    let mut targets = target
        .0
        .iter()
        .flat_map(|p| {
            p.exterior()
                .0
                .iter()
                .chain(p.interiors().iter().flat_map(|r| &r.0))
                .map(point)
        })
        .collect::<Vec<_>>();
    targets.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    targets.dedup();
    map_rings(source, |ring| snap_line(ring, &targets, tolerance))
}

/// `LineStringSnapper.snapTo` without snapping to source vertices.
fn snap_line(source: &[Point], targets: &[Point], tolerance: f64) -> Vec<Point> {
    let mut coords = source.to_vec();
    let closed = coords.len() > 1 && coords.first() == coords.last();
    // snapVertices
    let end = if closed {
        coords.len() - 1
    } else {
        coords.len()
    };
    for i in 0..end {
        let p = coords[i];
        let mut snap = None;
        for &t in targets {
            if p == t {
                break;
            }
            if distance(p, t) < tolerance {
                snap = Some(t);
                break;
            }
        }
        if let Some(t) = snap {
            coords[i] = t;
            if i == 0 && closed {
                let last = coords.len() - 1;
                coords[last] = t;
            }
        }
    }
    // snapSegments
    let distinct = if targets.first() == targets.last() && targets.len() > 1 {
        targets.len() - 1
    } else {
        targets.len()
    };
    for &t in &targets[..distinct] {
        let mut best = f64::MAX;
        let mut index = None;
        for i in 0..coords.len().saturating_sub(1) {
            let (p0, p1) = (coords[i], coords[i + 1]);
            if p0 == t || p1 == t {
                index = None;
                best = -1.0;
                break;
            }
            let d = point_to_segment(t, p0, p1);
            if d < tolerance && d < best {
                best = d;
                index = Some(i);
            }
        }
        if best < 0.0 {
            continue;
        }
        if let Some(i) = index {
            // CoordinateList.add(i + 1, t, false)
            if coords[i] != t && coords.get(i + 1) != Some(&t) {
                coords.insert(i + 1, t);
            }
        }
    }
    coords
}

/// A polygon ring as an overlay input edge: the interior side of a
/// counter-clockwise shell or a clockwise hole is its left.
fn area_curve(points: Vec<Point>, shell: bool) -> Curve {
    let ccw = signed_area(&points) > 0.0;
    Curve {
        points,
        delta: if ccw == shell { 1 } else { -1 },
    }
}

fn point(c: &Coord<f64>) -> Point {
    [c.x, c.y]
}

/// `CoordinateArrays.removeRepeatedOrInvalidPoints`.
fn clean(ring: &LineString<f64>) -> Vec<Point> {
    let mut points = Vec::with_capacity(ring.0.len());
    for p in &ring.0 {
        let p = point(p);
        if p.iter().all(|v| v.is_finite()) && points.last() != Some(&p) {
            points.push(p);
        }
    }
    points
}

/// Twice the signed area; positive for counter-clockwise rings.
fn signed_area(ring: &[Point]) -> f64 {
    ring.windows(2)
        .map(|e| e[0][0] * e[1][1] - e[1][0] * e[0][1])
        .sum()
}

fn orientation(a: Point, b: Point, c: Point) -> Orientation {
    let coord = |p: Point| Coord { x: p[0], y: p[1] };
    RobustKernel::orient2d(coord(a), coord(b), coord(c))
}

/// `Coordinate.distance`.
fn distance(a: Point, b: Point) -> f64 {
    java_hypot(a[0] - b[0], a[1] - b[1])
}

/// Java's `Math.sin` and `Math.cos`, as nearly as can be had: HotSpot's
/// intrinsics round almost all results correctly, so this returns the
/// correctly rounded values, from a double-double evaluation (about 100 bits)
/// after reduction by pi/2. On 200,000 random angles they differ from JDK 21
/// in 0.14% of results, the platform functions in 0.21%. `(sin, cos)`; beyond
/// 1e6 radians, the platform functions.
fn java_sin_cos(x: f64) -> (f64, f64) {
    if !x.is_finite() || x.abs() > 1.0e6 {
        return (x.sin(), x.cos());
    }
    // Double-double arithmetic: (hi, lo) with |lo| <= ulp(hi) / 2.
    fn two_sum(a: f64, b: f64) -> (f64, f64) {
        let s = a + b;
        let v = s - a;
        (s, (a - (s - v)) + (b - v))
    }
    fn add(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
        let (s, e) = two_sum(a.0, b.0);
        let (s, e) = two_sum(s, e + a.1 + b.1);
        (s, e)
    }
    fn mul(a: (f64, f64), b: (f64, f64)) -> (f64, f64) {
        let p = a.0 * b.0;
        let e = a.0.mul_add(b.0, -p);
        two_sum(p, e + a.0 * b.1 + a.1 * b.0)
    }
    fn div(a: (f64, f64), b: f64) -> (f64, f64) {
        let q = a.0 / b;
        let r = add(a, (-(q * b), -(q.mul_add(b, -(q * b)))));
        two_sum(q, r.0 / b)
    }
    // pi / 2 as the sum of three doubles.
    const P: [f64; 3] = [
        f64::from_bits(0x3FF9_21FB_5444_2D18),
        f64::from_bits(0x3C91_A626_3314_5C07),
        f64::from_bits(0xB91F_1976_B7ED_8FBC),
    ];
    let k = (x * std::f64::consts::FRAC_2_PI).round();
    let mut r = (x, 0.0);
    for part in P {
        let p = k * part;
        r = add(r, (-p, -(k.mul_add(part, -p))));
    }
    let r2 = mul(r, r);
    // Taylor series of sin and cos at r, |r| <= pi / 4 (plus rounding).
    let (mut sin, mut term): ((f64, f64), (f64, f64)) = (r, r);
    let mut n = 1.0;
    while term.0.abs() > 1.0e-40 {
        term = div(mul(term, r2), -((n + 1.0) * (n + 2.0)));
        sin = add(sin, term);
        n += 2.0;
    }
    let (mut cos, mut term): ((f64, f64), (f64, f64)) = ((1.0, 0.0), (1.0, 0.0));
    let mut n = 0.0;
    while term.0.abs() > 1.0e-40 {
        term = div(mul(term, r2), -((n + 1.0) * (n + 2.0)));
        cos = add(cos, term);
        n += 2.0;
    }
    let (sin, cos) = (sin.0 + sin.1, cos.0 + cos.1);
    match (k as i64).rem_euclid(4) {
        0 => (sin, cos),
        1 => (cos, -sin),
        2 => (-sin, -cos),
        _ => (-cos, sin),
    }
}

/// Java's `Math.atan2`, which is `StrictMath.atan2`: fdlibm's `e_atan2.c`,
/// bit for bit. The platform `atan2` rounds many results differently.
fn java_atan2(y: f64, x: f64) -> f64 {
    const PI_O_4: f64 = f64::from_bits(0x3FE9_21FB_5444_2D18);
    const PI_O_2: f64 = f64::from_bits(0x3FF9_21FB_5444_2D18);
    const PI: f64 = f64::from_bits(0x4009_21FB_5444_2D18);
    const PI_LO: f64 = f64::from_bits(0x3CA1_A626_3314_5C07);
    const TINY: f64 = 1.0e-300;
    if x.is_nan() || y.is_nan() {
        return x + y;
    }
    if x == 1.0 {
        return java_atan(y);
    }
    // 2 * sign(x) + sign(y)
    let m = usize::from(y.is_sign_negative()) | (usize::from(x.is_sign_negative()) << 1);
    if y == 0.0 {
        return match m {
            0 | 1 => y,
            2 => PI + TINY,
            _ => -PI - TINY,
        };
    }
    if x == 0.0 {
        return if y < 0.0 {
            -PI_O_2 - TINY
        } else {
            PI_O_2 + TINY
        };
    }
    if x.is_infinite() {
        return if y.is_infinite() {
            [
                PI_O_4 + TINY,
                -PI_O_4 - TINY,
                3.0 * PI_O_4 + TINY,
                -3.0 * PI_O_4 - TINY,
            ][m]
        } else {
            [0.0, -0.0, PI + TINY, -PI - TINY][m]
        };
    }
    if y.is_infinite() {
        return if y < 0.0 {
            -PI_O_2 - TINY
        } else {
            PI_O_2 + TINY
        };
    }
    let high = |v: f64| ((v.to_bits() >> 32) as i32) & 0x7fff_ffff;
    let k = (high(y) - high(x)) >> 20;
    let z = if k > 60 {
        PI_O_2 + 0.5 * PI_LO
    } else if x < 0.0 && k < -60 {
        0.0
    } else {
        java_atan((y / x).abs())
    };
    match m {
        0 => z,
        1 => -z,
        2 => PI - (z - PI_LO),
        _ => (z - PI_LO) - PI,
    }
}

/// fdlibm's `s_atan.c`, as `StrictMath.atan`.
fn java_atan(x: f64) -> f64 {
    const ATANHI: [f64; 4] = [
        f64::from_bits(0x3FDD_AC67_0561_BB4F),
        f64::from_bits(0x3FE9_21FB_5444_2D18),
        f64::from_bits(0x3FEF_730B_D281_F69B),
        f64::from_bits(0x3FF9_21FB_5444_2D18),
    ];
    const ATANLO: [f64; 4] = [
        f64::from_bits(0x3C7A_2B7F_222F_65E2),
        f64::from_bits(0x3C81_A626_3314_5C07),
        f64::from_bits(0x3C70_0788_7AF0_CBBD),
        f64::from_bits(0x3C91_A626_3314_5C07),
    ];
    const AT: [f64; 11] = [
        f64::from_bits(0x3FD5_5555_5555_550D),
        f64::from_bits(0xBFC9_9999_9998_EBC4),
        f64::from_bits(0x3FC2_4924_9200_83FF),
        f64::from_bits(0xBFBC_71C6_FE23_1671),
        f64::from_bits(0x3FB7_45CD_C54C_206E),
        f64::from_bits(0xBFB3_B0F2_AF74_9A6D),
        f64::from_bits(0x3FB1_0D66_A0D0_3D51),
        f64::from_bits(0xBFAD_DE2D_52DE_FD9A),
        f64::from_bits(0x3FA9_7B4B_2476_0DEB),
        f64::from_bits(0xBFA2_B444_2C6A_6C2F),
        f64::from_bits(0x3F90_AD3A_E322_DA11),
    ];
    let hx = (x.to_bits() >> 32) as i32;
    let ix = hx & 0x7fff_ffff;
    let mut x = x;
    let id;
    if ix >= 0x4410_0000 {
        // |x| >= 2^66
        if x.is_nan() {
            return x + x;
        }
        return if hx > 0 {
            ATANHI[3] + ATANLO[3]
        } else {
            -ATANHI[3] - ATANLO[3]
        };
    }
    if ix < 0x3fdc_0000 {
        // |x| < 0.4375
        if ix < 0x3e20_0000 {
            return x;
        }
        id = None;
    } else {
        x = x.abs();
        if ix < 0x3ff3_0000 {
            if ix < 0x3fe6_0000 {
                id = Some(0);
                x = (2.0 * x - 1.0) / (2.0 + x);
            } else {
                id = Some(1);
                x = (x - 1.0) / (x + 1.0);
            }
        } else if ix < 0x4003_8000 {
            id = Some(2);
            x = (x - 1.5) / (1.0 + 1.5 * x);
        } else {
            id = Some(3);
            x = -1.0 / x;
        }
    }
    let z = x * x;
    let w = z * z;
    let s1 = z * (AT[0] + w * (AT[2] + w * (AT[4] + w * (AT[6] + w * (AT[8] + w * AT[10])))));
    let s2 = w * (AT[1] + w * (AT[3] + w * (AT[5] + w * (AT[7] + w * AT[9]))));
    match id {
        None => x - x * (s1 + s2),
        Some(id) => {
            let z = ATANHI[id] - ((x * (s1 + s2) - ATANLO[id]) - x);
            if hx < 0 { -z } else { z }
        }
    }
}

/// Java's `Math.hypot`, which is `StrictMath.hypot`: the fdlibm algorithm of
/// JDK 21's `FdLibm.Hypot`, bit for bit. The platform `hypot` rounds some
/// results differently in the last bit.
fn java_hypot(x: f64, y: f64) -> f64 {
    let (mut a, mut b) = (x.abs(), y.abs());
    if !a.is_finite() || !b.is_finite() {
        return if a == f64::INFINITY || b == f64::INFINITY {
            f64::INFINITY
        } else {
            a + b
        };
    }
    if b > a {
        std::mem::swap(&mut a, &mut b);
    }
    let high = |v: f64| (v.to_bits() >> 32) as i32;
    let with_high = |high: i32| f64::from_bits(u64::from(high as u32) << 32);
    let (mut ha, mut hb) = (high(a), high(b));
    if ha - hb > 0x3c0_0000 {
        return a + b;
    }
    let mut k = 0;
    if a > f64::from_bits(0x5f30_0000_ffff_ffff) {
        // Scale a and b by 2^-600.
        ha -= 0x2580_0000;
        hb -= 0x2580_0000;
        a *= f64::from_bits(0x1a7 << 52);
        b *= f64::from_bits(0x1a7 << 52);
        k += 600;
    }
    if b < f64::from_bits(0x20b << 52) {
        if b < f64::MIN_POSITIVE {
            if b == 0.0 {
                return a;
            }
            let t1 = f64::from_bits(0x7fd << 52);
            b *= t1;
            a *= t1;
            k -= 1022;
        } else {
            // Scale a and b by 2^600.
            ha += 0x2580_0000;
            hb += 0x2580_0000;
            a *= f64::from_bits(0x657 << 52);
            b *= f64::from_bits(0x657 << 52);
            k -= 600;
        }
    }
    let mut w = a - b;
    if w > b {
        let t1 = with_high(ha);
        let t2 = a - t1;
        w = (t1 * t1 - (b * (-b) - t2 * (a + t1))).sqrt();
    } else {
        a += a;
        let y1 = with_high(hb);
        let y2 = b - y1;
        let t1 = with_high(ha + 0x0010_0000);
        let t2 = a - t1;
        w = (t1 * y1 - (w * (-w) - (t1 * y2 + t2 * b))).sqrt();
    }
    if k != 0 {
        f64::from_bits(((1023 + k) as u64) << 52) * w
    } else {
        w
    }
}

/// `Distance.pointToSegment`.
fn point_to_segment(p: Point, a: Point, b: Point) -> f64 {
    if a == b {
        return distance(p, a);
    }
    let len2 = (b[0] - a[0]) * (b[0] - a[0]) + (b[1] - a[1]) * (b[1] - a[1]);
    let r = ((p[0] - a[0]) * (b[0] - a[0]) + (p[1] - a[1]) * (b[1] - a[1])) / len2;
    if r <= 0.0 {
        return distance(p, a);
    }
    if r >= 1.0 {
        return distance(p, b);
    }
    let s = ((a[1] - p[1]) * (b[0] - a[0]) - (a[0] - p[0]) * (b[1] - a[1])) / len2;
    s.abs() * len2.sqrt()
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Side {
    Left,
    Right,
}

impl Side {
    fn opposite(self) -> Self {
        match self {
            Side::Left => Side::Right,
            Side::Right => Side::Left,
        }
    }
}

/// `BufferCurveSetBuilder.addPolygon`.
fn add_polygon(polygon: &Polygon<f64>, distance: f64, scale: Option<f64>, curves: &mut Vec<Curve>) {
    let (offset, side) = if distance < 0.0 {
        (-distance, Side::Right)
    } else {
        (distance, Side::Left)
    };
    let shell = clean(polygon.exterior());
    if distance < 0.0 && eroded_completely(polygon.exterior(), distance) {
        return;
    }
    if distance <= 0.0 && shell.len() < 3 {
        return;
    }
    // Shell: exterior on the left of a clockwise ring.
    add_ring_side(&shell, offset, side, false, scale, curves);
    for hole in polygon.interiors() {
        let points = clean(hole);
        if distance > 0.0 && eroded_completely(hole, -distance) {
            continue;
        }
        add_ring_side(&points, offset, side.opposite(), true, scale, curves);
    }
}

/// `BufferCurveSetBuilder.addRingSide`. `cw_left_interior` is whether the
/// polygon interior lies on the left of the ring when it is clockwise.
fn add_ring_side(
    coords: &[Point],
    offset: f64,
    mut side: Side,
    cw_left_interior: bool,
    scale: Option<f64>,
    curves: &mut Vec<Curve>,
) {
    if offset == 0.0 && coords.len() < 3 {
        return;
    }
    let mut left_interior = cw_left_interior;
    // Orientation.isCCWArea.
    if coords.len() >= 3 && signed_area(coords) > 0.0 {
        left_interior = !left_interior;
        side = side.opposite();
    }
    let Some(curve) = ring_curve(coords, side, offset, scale) else {
        return;
    };
    if ring_curve_inverted(coords, offset, &curve) || curve.len() < 2 {
        return;
    }
    // Depth increases by one across the curve towards its interior side.
    curves.push(Curve {
        points: curve,
        delta: if left_interior { 1 } else { -1 },
    });
}

/// `BufferCurveSetBuilder.isErodedCompletely`, on the uncleaned ring.
fn eroded_completely(ring: &LineString<f64>, buffer_distance: f64) -> bool {
    let coords = ring.0.iter().map(point).collect::<Vec<_>>();
    if coords.len() < 4 {
        return buffer_distance < 0.0;
    }
    if coords.len() == 4 {
        // isTriangleErodedCompletely, through Triangle.inCentre.
        let (a, b, c) = (coords[0], coords[1], coords[2]);
        let (len0, len1, len2) = (distance(b, c), distance(a, c), distance(a, b));
        let circum = len0 + len1 + len2;
        let centre = [
            (len0 * a[0] + len1 * b[0] + len2 * c[0]) / circum,
            (len0 * a[1] + len1 * b[1] + len2 * c[1]) / circum,
        ];
        return point_to_segment(centre, a, b) < buffer_distance.abs();
    }
    let (mut min, mut max) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
    for p in &coords {
        for axis in 0..2 {
            min[axis] = min[axis].min(p[axis]);
            max[axis] = max[axis].max(p[axis]);
        }
    }
    let smallest = (max[1] - min[1]).min(max[0] - min[0]);
    buffer_distance < 0.0 && 2.0 * buffer_distance.abs() > smallest
}

/// `BufferCurveSetBuilder.isRingCurveInverted`.
fn ring_curve_inverted(input: &[Point], offset: f64, curve: &[Point]) -> bool {
    if offset == 0.0 || input.len() <= 3 || input.len() >= 9 || curve.len() > 4 * input.len() {
        return false;
    }
    // hasPointOnBuffer
    let tolerance = 0.99 * offset.abs();
    let to_input = |p: Point| {
        input
            .windows(2)
            .map(|s| point_to_segment(p, s[0], s[1]))
            .fold(f64::INFINITY, f64::min)
    };
    for i in 0..curve.len() - 1 {
        let v = curve[i];
        if to_input(v) > tolerance {
            return false;
        }
        let next = curve[if i < curve.len() - 1 { i + 1 } else { 0 }];
        let mid = [(v[0] + next[0]) / 2.0, (v[1] + next[1]) / 2.0];
        if to_input(mid) > tolerance {
            return false;
        }
    }
    true
}

/// `OffsetCurveBuilder.getRingCurve` for a closed ring and positive offset.
fn ring_curve(input: &[Point], side: Side, offset: f64, scale: Option<f64>) -> Option<Vec<Point>> {
    if input.len() <= 2 || input.first() != input.last() {
        return None;
    }
    if offset == 0.0 {
        return Some(input.iter().map(|&p| precise(p, scale)).collect());
    }
    // computeRingBufferCurve
    let mut tolerance = offset * SIMPLIFY_FACTOR;
    if side == Side::Right {
        tolerance = -tolerance;
    }
    let simplified = simplify(input, tolerance);
    let n = simplified.len() - 1;
    if n < 2 {
        return None;
    }
    let mut generator =
        SegmentGenerator::new(offset, side, scale, simplified[n - 1], simplified[0]);
    // A mitre join adds at most two points per vertex.
    generator.points.reserve(2 * n + 2);
    for (i, &p) in simplified.iter().enumerate().skip(1) {
        generator.add_next_segment(p, i != 1);
    }
    generator.close_ring();
    Some(generator.points)
}

/// `BufferInputLineSimplifier.simplify`. A negative tolerance deletes shallow
/// clockwise concavities, a positive one counter-clockwise ones.
fn simplify(points: &[Point], signed_tolerance: f64) -> Vec<Point> {
    let n = points.len();
    let is_ring = n >= 4 && points[0] == points[n - 1];
    let tolerance = signed_tolerance.abs();
    let concave = if signed_tolerance < 0.0 {
        Orientation::Clockwise
    } else {
        Orientation::CounterClockwise
    };
    let mut deleted = vec![false; n];
    let next = |deleted: &[bool], index: usize| {
        let mut next = index + 1;
        while next < n && deleted[next] {
            next += 1;
        }
        next
    };
    loop {
        let mut index = if is_ring { 0 } else { 1 };
        let mut mid = next(&deleted, index);
        let mut last = next(&deleted, mid);
        let mut changed = false;
        while last < n {
            let (p0, p1, p2) = (points[index], points[mid], points[last]);
            // isDeletable, including the pinned isShallowSampled call, which
            // measures the sampled vertices against the segment p0-p1.
            let deletable =
                orientation(p0, p1, p2) == concave && point_to_segment(p1, p0, p2) < tolerance && {
                    let step = ((last - index) / 10).max(1);
                    (index..last)
                        .step_by(step)
                        .all(|i| point_to_segment(points[i], p0, p1) < tolerance)
                };
            if deletable {
                deleted[mid] = true;
                changed = true;
                index = last;
            } else {
                index = mid;
            }
            mid = next(&deleted, index);
            last = next(&deleted, mid);
        }
        if !changed {
            break;
        }
    }
    points
        .iter()
        .zip(&deleted)
        .filter_map(|(p, d)| (!d).then_some(*p))
        .collect()
}

/// `OffsetSegmentGenerator` with `JOIN_MITRE` and the default mitre limit.
struct SegmentGenerator {
    distance: f64,
    side: Side,
    scale: Option<f64>,
    s0: Point,
    s1: Point,
    s2: Point,
    offset0: (Point, Point),
    offset1: (Point, Point),
    points: Vec<Point>,
}

impl SegmentGenerator {
    fn new(distance: f64, side: Side, scale: Option<f64>, s1: Point, s2: Point) -> Self {
        Self {
            distance,
            side,
            scale,
            s0: s1,
            s1,
            s2,
            offset0: (s1, s2),
            offset1: offset_segment(s1, s2, side, distance),
            points: Vec::new(),
        }
    }

    /// `OffsetSegmentString.addPt`, dropping near-duplicate vertices.
    fn add(&mut self, p: Point) {
        let p = precise(p, self.scale);
        if let Some(&last) = self.points.last()
            && distance(p, last) < self.distance * CURVE_VERTEX_SNAP_DISTANCE_FACTOR
        {
            return;
        }
        self.points.push(p);
    }

    fn close_ring(&mut self) {
        if let (Some(&first), Some(&last)) = (self.points.first(), self.points.last())
            && first != last
        {
            self.points.push(first);
        }
    }

    fn add_next_segment(&mut self, p: Point, add_start_point: bool) {
        self.s0 = self.s1;
        self.s1 = self.s2;
        self.s2 = p;
        self.offset0 = offset_segment(self.s0, self.s1, self.side, self.distance);
        self.offset1 = offset_segment(self.s1, self.s2, self.side, self.distance);
        if self.s1 == self.s2 {
            return;
        }
        let turn = orientation(self.s0, self.s1, self.s2);
        let outside = (turn == Orientation::Clockwise && self.side == Side::Left)
            || (turn == Orientation::CounterClockwise && self.side == Side::Right);
        if turn == Orientation::Collinear {
            // addCollinear: only a reversal (two intersection points) adds a
            // bevelled end cap.
            if intersections(self.s0, self.s1, self.s1, self.s2).len() >= 2 {
                if add_start_point {
                    self.add(self.offset0.1);
                }
                self.add(self.offset1.0);
            }
        } else if outside {
            self.add_outside_turn();
        } else {
            self.add_inside_turn();
        }
    }

    fn add_outside_turn(&mut self) {
        if distance(self.offset0.1, self.offset1.0)
            < self.distance * OFFSET_SEGMENT_SEPARATION_FACTOR
        {
            // Use the endpoint of the longer segment.
            let point = if distance(self.s0, self.s1) > distance(self.s1, self.s2) {
                self.offset0.1
            } else {
                self.offset1.0
            };
            self.add(point);
            return;
        }
        // addMitreJoin
        let limit = MITRE_LIMIT * self.distance;
        let corner = self.s1;
        let (o0, o1) = (self.offset0, self.offset1);
        if let Some(p) = super::jts_intersection::intersection(o0.0, o0.1, o1.0, o1.1)
            && distance(p, corner) <= limit
        {
            self.add(p);
            return;
        }
        if point_to_segment(corner, o0.1, o1.0) >= limit {
            self.add(o0.1);
            self.add(o1.0);
            return;
        }
        // addLimitedMitreJoin
        let angle = |a: Point, b: Point| java_atan2(b[1] - a[1], b[0] - a[0]);
        let interior = {
            let delta = angle(corner, self.s2) - angle(corner, self.s0);
            if delta <= -std::f64::consts::PI {
                delta + 2.0 * std::f64::consts::PI
            } else if delta > std::f64::consts::PI {
                delta - 2.0 * std::f64::consts::PI
            } else {
                delta
            }
        };
        let bisector = normalize_angle(angle(corner, self.s0) + interior / 2.0);
        // The bevel midpoint, back along the bisector of the interior angle.
        let mid = project(corner, -limit, bisector);
        let bevel = normalize_angle(bisector + std::f64::consts::FRAC_PI_2);
        let bevel0 = project(mid, self.distance, bevel);
        let bevel1 = project(mid, self.distance, bevel + std::f64::consts::PI);
        match (
            line_segment(o0.0, o0.1, bevel0, bevel1),
            line_segment(o1.0, o1.1, bevel0, bevel1),
        ) {
            (Some(a), Some(b)) => {
                self.add(a);
                self.add(b);
            }
            _ => {
                self.add(o0.1);
                self.add(o1.0);
            }
        }
    }

    fn add_inside_turn(&mut self) {
        let (o0, o1) = (self.offset0, self.offset1);
        if let Some(&p) = intersections(o0.0, o0.1, o1.0, o1.1).first() {
            self.add(p);
            return;
        }
        if distance(o0.1, o1.0) < self.distance * INSIDE_TURN_VERTEX_SNAP_DISTANCE_FACTOR {
            self.add(o0.1);
            return;
        }
        // closingSegLengthFactor is 1 for mitre joins.
        let s1 = self.s1;
        self.add(o0.1);
        self.add([(o0.1[0] + s1[0]) / 2.0, (o0.1[1] + s1[1]) / 2.0]);
        self.add([(o1.0[0] + s1[0]) / 2.0, (o1.0[1] + s1[1]) / 2.0]);
        self.add(o1.0);
    }
}

/// `OffsetSegmentGenerator.computeOffsetSegment`.
fn offset_segment(p0: Point, p1: Point, side: Side, distance: f64) -> (Point, Point) {
    let sign = if side == Side::Left { 1.0 } else { -1.0 };
    let dx = p1[0] - p0[0];
    let dy = p1[1] - p0[1];
    let len = java_hypot(dx, dy);
    let ux = sign * distance * dx / len;
    let uy = sign * distance * dy / len;
    ([p0[0] - uy, p0[1] + ux], [p1[0] - uy, p1[1] + ux])
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

/// `OffsetSegmentGenerator.project`, through `Angle.cosSnap` and `sinSnap`,
/// which take a sine or cosine below 5e-16 as zero.
fn project(p: Point, distance: f64, angle: f64) -> Point {
    let snap = |v: f64| if v.abs() < 5.0e-16 { 0.0 } else { v };
    let (sin, cos) = java_sin_cos(angle);
    [p[0] + distance * snap(cos), p[1] + distance * snap(sin)]
}

/// `Intersection.lineSegment`.
fn line_segment(line0: Point, line1: Point, seg0: Point, seg1: Point) -> Option<Point> {
    let first = orientation(line0, line1, seg0);
    if first == Orientation::Collinear {
        return Some(seg0);
    }
    let second = orientation(line0, line1, seg1);
    if second == Orientation::Collinear {
        return Some(seg1);
    }
    if first == second {
        return None;
    }
    super::jts_intersection::intersection(line0, line1, seg0, seg1)
}

/// Up to two intersection points, held inline.
#[derive(Clone, Copy)]
struct Hits {
    len: usize,
    points: [Point; 2],
}

impl Hits {
    const NONE: Hits = Hits {
        len: 0,
        points: [[0.0; 2]; 2],
    };
    fn one(p: Point) -> Self {
        Hits {
            len: 1,
            points: [p, [0.0; 2]],
        }
    }
    fn two(a: Point, b: Point) -> Self {
        Hits {
            len: 2,
            points: [a, b],
        }
    }
}

impl std::ops::Deref for Hits {
    type Target = [Point];
    fn deref(&self) -> &[Point] {
        &self.points[..self.len]
    }
}

/// `RobustLineIntersector.computeIntersect`: no point, one point, or the two
/// ends of a collinear overlap, in JTS's order.
fn intersections(p1: Point, p2: Point, q1: Point, q2: Point) -> Hits {
    let within = |a: Point, b: Point, p: Point| {
        p[0] >= a[0].min(b[0])
            && p[0] <= a[0].max(b[0])
            && p[1] >= a[1].min(b[1])
            && p[1] <= a[1].max(b[1])
    };
    if p1[0].max(p2[0]) < q1[0].min(q2[0])
        || q1[0].max(q2[0]) < p1[0].min(p2[0])
        || p1[1].max(p2[1]) < q1[1].min(q2[1])
        || q1[1].max(q2[1]) < p1[1].min(p2[1])
    {
        return Hits::NONE;
    }
    let same = |a: Orientation, b: Orientation| a != Orientation::Collinear && a == b;
    let pq1 = orientation(p1, p2, q1);
    let pq2 = orientation(p1, p2, q2);
    if same(pq1, pq2) {
        return Hits::NONE;
    }
    let qp1 = orientation(q1, q2, p1);
    let qp2 = orientation(q1, q2, p2);
    if same(qp1, qp2) {
        return Hits::NONE;
    }
    let collinear = Orientation::Collinear;
    if pq1 == collinear && pq2 == collinear && qp1 == collinear && qp2 == collinear {
        // computeCollinearIntersection
        let (q1p, q2p) = (within(p1, p2, q1), within(p1, p2, q2));
        let (p1q, p2q) = (within(q1, q2, p1), within(q1, q2, p2));
        let pair = |a: Point, b: Point, single: bool| {
            if single {
                Hits::one(a)
            } else {
                Hits::two(a, b)
            }
        };
        return if q1p && q2p {
            Hits::two(q1, q2)
        } else if p1q && p2q {
            Hits::two(p1, p2)
        } else if q1p && p1q {
            pair(q1, p1, q1 == p1 && !q2p && !p2q)
        } else if q1p && p2q {
            pair(q1, p2, q1 == p2 && !q2p && !p1q)
        } else if q2p && p1q {
            pair(q2, p1, q2 == p1 && !q1p && !p2q)
        } else if q2p && p2q {
            pair(q2, p2, q2 == p2 && !q1p && !p1q)
        } else {
            Hits::NONE
        };
    }
    if [pq1, pq2, qp1, qp2].contains(&collinear) {
        let p = if p1 == q1 || p1 == q2 {
            p1
        } else if p2 == q1 || p2 == q2 {
            p2
        } else if pq1 == collinear {
            q1
        } else if pq2 == collinear {
            q2
        } else if qp1 == collinear {
            p1
        } else {
            p2
        };
        return Hits::one(p);
    }
    // RobustLineIntersector.intersection
    let point = match super::jts_intersection::intersection(p1, p2, q1, q2) {
        Some(p) if within(p1, p2, p) && within(q1, q2, p) => p,
        _ => {
            let mut nearest = p1;
            let mut best = point_to_segment(p1, q1, q2);
            for (p, a, b) in [(p2, q1, q2), (q1, p1, p2), (q2, p1, p2)] {
                let d = point_to_segment(p, a, b);
                if d < best {
                    best = d;
                    nearest = p;
                }
            }
            nearest
        }
    };
    Hits::one(point)
}

/// `SegmentPointComparator` order of two nodes along a segment.
fn along(a: Point, b: Point, p: Point, q: Point) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    if p == q {
        return Ordering::Equal;
    }
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let sign = |u: f64, v: f64| u.partial_cmp(&v).unwrap_or(Ordering::Equal);
    let (xs, ys) = (sign(p[0], q[0]), sign(p[1], q[1]));
    let flip = |o: Ordering, negative: bool| if negative { o.reverse() } else { o };
    let (major, minor) = if dx.abs() >= dy.abs() {
        (flip(xs, dx < 0.0), flip(ys, dy < 0.0))
    } else {
        (flip(ys, dy < 0.0), flip(xs, dx < 0.0))
    };
    major.then(minor)
}

fn key(p: Point) -> (u64, u64) {
    let canonical = |v: f64| if v == 0.0 { 0 } else { v.to_bits() };
    (canonical(p[0]), canonical(p[1]))
}

/// A closed curve to node, in the order and direction JTS adds it. Crossing
/// it from right to left adds `delta` to the depth (`Label` depth delta).
struct Curve {
    points: Vec<Point>,
    delta: i64,
}

/// A noded edge: the coordinates from one node to the next.
struct Noded {
    points: Vec<Point>,
    delta: i64,
}

/// Which JTS polygon builder input order a result follows.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// `BufferBuilder`: subgraphs from the lowest node, in depth-first order.
    Buffer,
    /// `OverlayOp`: noded edges in input order.
    Overlay,
}

fn polygonize(curves: Vec<Curve>, mode: Mode) -> Option<MultiPolygon<f64>> {
    build(node_float(&curves)?, mode)
}

/// MCIndexNoder with IntersectionAdder: the noded substrings of each curve.
fn node_float(curves: &[Curve]) -> Option<Vec<Noded>> {
    struct Segment {
        a: Point,
        b: Point,
        curve: usize,
        index: usize,
    }
    let mut segments = Vec::with_capacity(
        curves
            .iter()
            .map(|c| c.points.len().saturating_sub(1))
            .sum(),
    );
    let mut curve_len = Vec::with_capacity(curves.len());
    let mut first_segment = Vec::with_capacity(curves.len());
    for (c, curve) in curves.iter().enumerate() {
        first_segment.push(segments.len());
        if curve.points.first() != curve.points.last() || curve.points.len() < 4 {
            curve_len.push(0);
            continue;
        }
        curve_len.push(curve.points.len() - 1);
        for (index, pair) in curve.points.windows(2).enumerate() {
            segments.push(Segment {
                a: pair[0],
                b: pair[1],
                curve: c,
                index,
            });
        }
    }
    if segments.len() > MAX_SEGMENTS {
        return None;
    }
    let mut nodes = vec![Vec::<Point>::new(); segments.len()];
    let min_x = |s: &Segment| s.a[0].min(s.b[0]);
    let mut order = (0..segments.len()).collect::<Vec<_>>();
    order.sort_by(|&i, &j| min_x(&segments[i]).total_cmp(&min_x(&segments[j])));
    for (position, &i) in order.iter().enumerate() {
        let s = &segments[i];
        let max_x = s.a[0].max(s.b[0]);
        for &j in &order[position + 1..] {
            let t = &segments[j];
            if min_x(t) > max_x {
                break;
            }
            let found = intersections(s.a, s.b, t.a, t.b);
            if found.is_empty() {
                continue;
            }
            // IntersectionAdder.isTrivialIntersection
            if found.len() == 1 && s.curve == t.curve {
                let (lo, hi) = (s.index.min(t.index), s.index.max(t.index));
                let last = curve_len[s.curve] - 1;
                if hi - lo == 1 || (lo == 0 && hi == last) {
                    continue;
                }
            }
            nodes[i].extend_from_slice(&found);
            nodes[j].extend_from_slice(&found);
        }
    }
    let mut output = Vec::with_capacity(curves.len());
    for (c, curve) in curves.iter().enumerate() {
        let n = curve_len[c];
        if n == 0 {
            continue;
        }
        let points = &curve.points;
        // A vertex is a node where an intersection falls on it, besides the
        // two ends of the string.
        let mut node_vertex = vec![false; n + 1];
        node_vertex[0] = true;
        node_vertex[n] = true;
        for i in 0..n {
            for &p in &nodes[first_segment[c] + i] {
                if p == points[i] {
                    node_vertex[i] = true;
                } else if p == points[i + 1] {
                    node_vertex[i + 1] = true;
                }
            }
        }
        let inserted: usize = (0..n).map(|i| nodes[first_segment[c] + i].len()).sum();
        let mut sequence = Vec::with_capacity(n + 1 + inserted);
        sequence.push((points[0], true, true));
        for i in 0..n {
            let (a, b) = (points[i], points[i + 1]);
            let mut inner = nodes[first_segment[c] + i]
                .iter()
                .copied()
                .filter(|&p| p != a && p != b)
                .collect::<Vec<_>>();
            inner.sort_by(|p, q| along(a, b, *p, *q));
            inner.dedup_by(|p, q| key(*p) == key(*q));
            sequence.extend(inner.into_iter().map(|p| (p, true, false)));
            sequence.push((b, node_vertex[i + 1], true));
        }
        split_at_nodes(&sequence, curve.delta, &mut output);
    }
    Some(output)
}

/// `SegmentNodeList.addCollapsedNodes`: the vertices that become nodes
/// because the string collapses onto itself there. That is a vertex between
/// two equal vertices (`findCollapsesFromExistingVertices`), or the single
/// vertex between two consecutive equal nodes (`findCollapsesFromInsertedNodes`).
/// Indices are into `sequence`, in which both kinds are computed from the
/// nodes as given; each collapsed vertex's flag in `is_node` is set.
fn mark_collapsed_vertices(sequence: &[(Point, bool, bool)], is_node: &mut [bool]) {
    // Consecutive nodes with equal points and exactly one vertex between.
    let mut previous_node = None::<usize>;
    let mut between = 0usize;
    let mut between_vertex = 0usize;
    for (i, &(p, node, vertex)) in sequence.iter().enumerate() {
        if node {
            if let Some(a) = previous_node
                && sequence[a].0 == p
                && between == 1
            {
                is_node[between_vertex] = true;
            }
            previous_node = Some(i);
            between = 0;
        } else if vertex && previous_node.is_some() {
            between += 1;
            between_vertex = i;
        }
    }
    // Three consecutive vertices whose first and last are equal.
    let (mut first, mut second) = (None::<usize>, None::<usize>);
    for (i, &(p, _, vertex)) in sequence.iter().enumerate() {
        if !vertex {
            continue;
        }
        if let (Some(t0), Some(t1)) = (first, second)
            && sequence[t0].0 == p
        {
            is_node[t1] = true;
        }
        first = second;
        second = Some(i);
    }
}

/// `SegmentNodeList.addSplitEdges`: substrings between consecutive nodes.
/// Each entry is a point, whether it is a node, and whether it is a vertex
/// of the string (rather than a node inserted inside a segment).
fn split_at_nodes(sequence: &[(Point, bool, bool)], delta: i64, output: &mut Vec<Noded>) {
    let Some(&(first, ..)) = sequence.first() else {
        return;
    };
    let mut is_node = sequence
        .iter()
        .map(|&(_, node, _)| node)
        .collect::<Vec<_>>();
    mark_collapsed_vertices(sequence, &mut is_node);
    // Room for the points from `from` up to the next node.
    let span = |from: usize| {
        is_node[from + 1..]
            .iter()
            .position(|&n| n)
            .map_or(sequence.len() - from, |k| k + 2)
    };
    let mut current = Vec::with_capacity(span(0));
    current.push(first);
    for (i, &(p, ..)) in sequence.iter().enumerate().skip(1) {
        if current.last() != Some(&p) {
            current.push(p);
        }
        if is_node[i] {
            if current.len() >= 2 {
                output.push(Noded {
                    points: std::mem::take(&mut current),
                    delta,
                });
            } else {
                current.clear();
            }
            if current.capacity() == 0 && i + 1 < sequence.len() {
                current.reserve_exact(span(i));
            }
            current.push(p);
        }
    }
}

/// `SnapRoundingNoder` on a grid of `scale` per tile pixel, for curves in MVT
/// units; returns the noded substrings in MVT units. With `prescaled`, it is
/// BufferOp's `ScaledNoder(SnapRoundingNoder(PrecisionModel(1)), scale)`,
/// which rounds the input coordinates onto the grid before noding.
fn node_snap_round(curves: &[Curve], scale: f64, prescaled: bool) -> Option<Vec<Noded>> {
    let to_grid = |p: Point| {
        p.map(|v| {
            let v = v / PIXEL * scale;
            if prescaled { java_round(v) } else { v }
        })
    };
    let strings = curves
        .iter()
        .map(|c| {
            let mut points = c.points.iter().map(|&p| to_grid(p)).collect::<Vec<_>>();
            points.dedup();
            (points, c.delta)
        })
        .filter(|(c, _)| c.len() >= 2)
        .collect::<Vec<_>>();
    struct Segment {
        string: usize,
        index: usize,
    }
    let segments = strings
        .iter()
        .enumerate()
        .flat_map(|(string, (points, _))| {
            (0..points.len() - 1).map(move |index| Segment { string, index })
        })
        .collect::<Vec<_>>();
    if segments.len() > MAX_SEGMENTS {
        return None;
    }
    let ends = |s: &Segment| {
        let points = &strings[s.string].0;
        (points[s.index], points[s.index + 1])
    };
    // SnapRoundingIntersectionAdder through an MCIndexNoder whose overlap
    // tolerance is the nearness tolerance.
    const NEARNESS: f64 = 1.0 / 100.0;
    let mut nodes = vec![Vec::<Point>::new(); segments.len()];
    let mut pixels = Vec::<Point>::new();
    let min_x = |s: &Segment| {
        let (a, b) = ends(s);
        a[0].min(b[0])
    };
    let mut order = (0..segments.len()).collect::<Vec<_>>();
    order.sort_by(|&i, &j| min_x(&segments[i]).total_cmp(&min_x(&segments[j])));
    for (position, &i) in order.iter().enumerate() {
        let (p0, p1) = ends(&segments[i]);
        let max_x = p0[0].max(p1[0]) + NEARNESS;
        let (lo_y, hi_y) = (p0[1].min(p1[1]) - NEARNESS, p0[1].max(p1[1]) + NEARNESS);
        for &j in &order[position + 1..] {
            if min_x(&segments[j]) > max_x {
                break;
            }
            let (q0, q1) = ends(&segments[j]);
            if q0[1].max(q1[1]) < lo_y || q0[1].min(q1[1]) > hi_y {
                continue;
            }
            let found = intersections(p0, p1, q0, q1);
            let interior = found
                .iter()
                .any(|p| (*p != p0 && *p != p1) || (*p != q0 && *p != q1));
            if !found.is_empty() && interior {
                pixels.extend_from_slice(&found);
                nodes[i].extend_from_slice(&found);
                nodes[j].extend_from_slice(&found);
                continue;
            }
            // processNearVertex
            let mut near =
                |p: Point, target: usize, a: Point, b: Point, nodes: &mut Vec<Vec<Point>>| {
                    if distance(p, a) < NEARNESS || distance(p, b) < NEARNESS {
                        return;
                    }
                    if point_to_segment(p, a, b) < NEARNESS {
                        pixels.push(p);
                        nodes[target].push(p);
                    }
                };
            near(p0, j, q0, q1, &mut nodes);
            near(p1, j, q0, q1, &mut nodes);
            near(q0, i, p0, p1, &mut nodes);
            near(q1, i, p0, p1, &mut nodes);
        }
    }
    let round = |p: Point| p.map(java_round);
    // HotPixelIndex: a pixel added more than once, or for an intersection,
    // is a node.
    let mut counts = HashMap::<(u64, u64), usize>::default();
    for &p in strings.iter().flat_map(|(points, _)| points) {
        *counts.entry(key(round(p))).or_default() += 1;
    }
    let mut node_pixels = counts
        .iter()
        .filter(|(_, n)| **n > 1)
        .map(|(k, _)| *k)
        .collect::<HashSet<_>>();
    node_pixels.extend(pixels.iter().map(|&p| key(round(p))));
    let mut hot = pixels.iter().map(|&p| round(p)).collect::<Vec<_>>();
    hot.extend(
        strings
            .iter()
            .flat_map(|(points, _)| points)
            .map(|&p| round(p)),
    );
    hot.sort_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    hot.dedup();
    // computeSegmentSnaps
    let mut by_string = vec![Vec::new(); strings.len()];
    for (segment, mut extra) in segments.iter().zip(nodes) {
        let (a, b) = ends(segment);
        extra.push(a);
        extra.push(b);
        extra.sort_by(|p, q| along(a, b, *p, *q));
        extra.dedup();
        by_string[segment.string].push(extra);
    }
    let mut snapped_strings = Vec::new();
    for pieces in by_string {
        // getNodedCoordinates
        let mut noded = Vec::<Point>::new();
        for piece in pieces {
            for p in piece {
                if noded.last() != Some(&p) {
                    noded.push(p);
                }
            }
        }
        // (point, node, vertex): the rounded noded coordinates are the
        // vertices of the snapped string, hot pixels its inserted nodes.
        let mut snapped = Vec::<(Point, bool, bool)>::new();
        let mut current = None;
        for pair in noded.windows(2) {
            let (p0, p1) = (pair[0], pair[1]);
            let (r0, r1) = (round(p0), round(p1));
            if snapped.is_empty() {
                snapped.push((r0, true, true));
                current = Some(r0);
            }
            // The segment collapsed onto the current vertex.
            if Some(r1) == current {
                continue;
            }
            // snapSegment: hot pixels the unrounded segment passes through
            // become nodes.
            let mut through = Vec::new();
            let lo = p0[0].min(p1[0]) - 1.0;
            let hi = p0[0].max(p1[0]) + 1.0;
            let start = hot.partition_point(|h| h[0] < lo);
            for &pixel in hot[start..].iter().take_while(|h| h[0] <= hi) {
                if pixel != r0 && pixel != r1 && hot_pixel_intersects(pixel, p0, p1) {
                    through.push(pixel);
                }
            }
            let from = current?;
            through.sort_by(|p, q| along(from, r1, *p, *q));
            through.dedup();
            for pixel in through {
                node_pixels.insert(key(pixel));
                if snapped.last().map(|e| e.0) != Some(pixel) {
                    snapped.push((pixel, true, false));
                }
            }
            match snapped.last_mut() {
                // A node at the next vertex lies on that vertex.
                Some(last) if last.0 == r1 => last.2 = true,
                _ => snapped.push((r1, false, true)),
            }
            current = Some(r1);
        }
        snapped_strings.push(snapped);
    }
    let mut output = Vec::new();
    for (mut snapped, (_, delta)) in snapped_strings.into_iter().zip(&strings) {
        if snapped.len() < 2 {
            continue;
        }
        // addVertexNodeSnaps, and the string's own end.
        let last = snapped.len() - 1;
        for (i, entry) in snapped.iter_mut().enumerate() {
            if i == last || node_pixels.contains(&key(entry.0)) {
                entry.1 = true;
            }
        }
        let sequence = snapped
            .into_iter()
            .map(|(p, node, vertex)| (p.map(|v| v / scale * PIXEL), node, vertex))
            .collect::<Vec<_>>();
        split_at_nodes(&sequence, *delta, &mut output);
    }
    Some(output)
}

/// `HotPixel.intersects` for a unit pixel centred on an integer point; its
/// top and right sides are open.
fn hot_pixel_intersects(centre: Point, p0: Point, p1: Point) -> bool {
    let (p, q) = if p0[0] > p1[0] { (p1, p0) } else { (p0, p1) };
    let (minx, maxx) = (centre[0] - 0.5, centre[0] + 0.5);
    let (miny, maxy) = (centre[1] - 0.5, centre[1] + 0.5);
    if p[0].min(q[0]) >= maxx
        || p[0].max(q[0]) < minx
        || p[1].min(q[1]) >= maxy
        || p[1].max(q[1]) < miny
    {
        return false;
    }
    if p[0] == q[0] || p[1] == q[1] {
        return true;
    }
    let orient = |x: f64, y: f64| orientation(p, q, [x, y]);
    let ul = orient(minx, maxy);
    if ul == Orientation::Collinear {
        return p[1] >= q[1];
    }
    let ur = orient(maxx, maxy);
    if ur == Orientation::Collinear {
        return p[1] <= q[1];
    }
    if ul != ur {
        return true;
    }
    let ll = orient(minx, miny);
    if ll == Orientation::Collinear || ll != ul {
        return true;
    }
    let lr = orient(maxx, miny);
    if lr == Orientation::Collinear {
        return p[1] >= q[1];
    }
    ll != lr || lr != ur
}

/// JTS `EdgeEnd` order around a node: by quadrant from north-east, then
/// counter-clockwise.
fn edge_end_order(origin: Point, a: Point, b: Point) -> std::cmp::Ordering {
    let quadrant = |p: Point| {
        let (dx, dy) = (p[0] - origin[0], p[1] - origin[1]);
        match (dx >= 0.0, dy >= 0.0) {
            (true, true) => 0,
            (false, true) => 1,
            (false, false) => 2,
            (true, false) => 3,
        }
    };
    quadrant(a)
        .cmp(&quadrant(b))
        .then_with(|| match orientation(origin, a, b) {
            Orientation::CounterClockwise => std::cmp::Ordering::Less,
            Orientation::Clockwise => std::cmp::Ordering::Greater,
            Orientation::Collinear => std::cmp::Ordering::Equal,
        })
}

/// Builds polygons from noded edges as JTS's `BufferBuilder` and `OverlayOp`
/// do with `PolygonBuilder`: edges merged when equal, depths propagated
/// around the nodes from the outside of each connected subgraph, the
/// directed edges with depth >= 1 on their right and <= 0 on their left
/// linked into maximal and then minimal rings, and holes assigned to shells.
/// Rings keep JTS's start and orientation. `None` where JTS would throw.
fn build(noded: Vec<Noded>, mode: Mode) -> Option<MultiPolygon<f64>> {
    // insertUniqueEdge
    let mut edges = Vec::<(Vec<Point>, i64)>::with_capacity(noded.len());
    // Each edge is looked up by its coordinates in whichever direction
    // orders first, with the direction it was inserted in: by a hash of
    // that sequence of keys, then comparing the sequences of the edges
    // that share the hash (chained through `same_hash`).
    let mut lookup = HashMap::<u64, usize>::default();
    lookup.reserve(noded.len());
    let mut stored_reversed = Vec::<bool>::with_capacity(noded.len());
    let mut same_hash = Vec::<usize>::with_capacity(noded.len());
    fn canonical(points: &[Point], reversed: bool) -> impl Iterator<Item = (u64, u64)> + '_ {
        let n = points.len();
        (0..n).map(move |i| key(points[if reversed { n - 1 - i } else { i }]))
    }
    for Noded { points, delta } in noded {
        if points.len() < 2 || (points.len() == 2 && points[0] == points[1]) {
            continue;
        }
        let reversed = points
            .iter()
            .rev()
            .map(|&p| key(p))
            .lt(points.iter().map(|&p| key(p)));
        let hash = {
            use std::hash::{Hash, Hasher};
            let mut hasher = rustc_hash::FxHasher::default();
            points.len().hash(&mut hasher);
            for k in canonical(&points, reversed) {
                k.hash(&mut hasher);
            }
            hasher.finish()
        };
        let mut found = None;
        let mut e = lookup.get(&hash).copied().unwrap_or(usize::MAX);
        while e != usize::MAX {
            if edges[e].0.len() == points.len()
                && canonical(&edges[e].0, stored_reversed[e]).eq(canonical(&points, reversed))
            {
                found = Some(e);
                break;
            }
            e = same_hash[e];
        }
        match found {
            Some(e) => {
                if stored_reversed[e] == reversed {
                    edges[e].1 += delta;
                } else {
                    edges[e].1 -= delta;
                }
            }
            None => {
                let index = edges.len();
                same_hash.push(lookup.insert(hash, index).unwrap_or(usize::MAX));
                stored_reversed.push(reversed);
                edges.push((points, delta));
            }
        }
    }
    if edges.is_empty() {
        return Some(MultiPolygon(vec![]));
    }
    let overlay = match mode {
        Mode::Overlay => Some(overlay_depths(&edges)?),
        // The segment graph also rejects an edge with no two distinct
        // coordinates.
        Mode::Buffer => {
            if edges
                .iter()
                .any(|(points, _)| points.windows(2).all(|w| key(w[0]) == key(w[1])))
            {
                return None;
            }
            None
        }
    };
    // JTS directed edges: 2e along the edge, 2e + 1 against it. A JTS
    // result edge has the interior on its right. Appends the points of `d`,
    // without the first unless `first`.
    let extend_edge_points = |out: &mut Vec<Point>, d: usize, first: bool| {
        let points = &edges[d / 2].0;
        let skip = usize::from(!first);
        if d.is_multiple_of(2) {
            out.extend_from_slice(&points[skip..]);
        } else {
            out.extend(points.iter().rev().skip(skip).copied());
        }
    };
    let start_of = |d: usize| {
        key(edges[d / 2].0[if d.is_multiple_of(2) {
            0
        } else {
            edges[d / 2].0.len() - 1
        }])
    };
    let second_of = |d: usize| {
        let points = &edges[d / 2].0;
        if d.is_multiple_of(2) {
            points[1]
        } else {
            points[points.len() - 2]
        }
    };
    // Node stars of the JTS graph.
    let mut nodes = HashMap::<(u64, u64), usize>::default();
    nodes.reserve(edges.len() * 2);
    let mut node_points = Vec::<Point>::new();
    let mut star = Vec::<Vec<usize>>::new();
    let mut node_index = Vec::with_capacity(edges.len() * 2);
    for d in 0..edges.len() * 2 {
        let k = start_of(d);
        let n = *nodes.entry(k).or_insert_with(|| {
            let points = &edges[d / 2].0;
            node_points.push(if d.is_multiple_of(2) {
                points[0]
            } else {
                points[points.len() - 1]
            });
            star.push(Vec::new());
            star.len() - 1
        });
        node_index.push(n);
        // EdgeEndStar keeps its ends in a TreeMap: an end in the same
        // direction as an earlier one replaces it.
        let origin = node_points[n];
        let list = &mut star[n];
        match list.binary_search_by(|&e| edge_end_order(origin, second_of(e), second_of(d))) {
            Ok(i) => list[i] = d,
            Err(i) => list.insert(i, d),
        }
    }
    let node_of = |d: usize| node_index[d];
    // JTS edge depth delta of a directed edge: left minus right.
    let edge_delta = |d: usize| {
        if d.is_multiple_of(2) {
            edges[d / 2].1
        } else {
            -edges[d / 2].1
        }
    };
    // createSubgraphs over the node map, sorted by coordinate; each
    // BufferSubgraph collects its nodes' stars depth-first. Subgraphs are
    // then processed by decreasing rightmost x.
    let subgraphs = if mode == Mode::Buffer {
        let mut sorted = (0..node_points.len()).collect::<Vec<_>>();
        sorted.sort_by(|&a, &b| {
            node_points[a][0]
                .total_cmp(&node_points[b][0])
                .then(node_points[a][1].total_cmp(&node_points[b][1]))
        });
        let mut visited = vec![false; node_points.len()];
        let mut subgraphs = Vec::<(f64, Vec<usize>)>::new();
        for &n in &sorted {
            if visited[n] {
                continue;
            }
            let mut list = Vec::new();
            let mut rightmost = f64::NEG_INFINITY;
            let mut stack = vec![n];
            while let Some(node) = stack.pop() {
                visited[node] = true;
                for &d in &star[node] {
                    list.push(d);
                    for p in &edges[d / 2].0 {
                        rightmost = rightmost.max(p[0]);
                    }
                    let other = node_of(d ^ 1);
                    if !visited[other] {
                        stack.push(other);
                    }
                }
            }
            subgraphs.push((rightmost, list));
        }
        subgraphs.sort_by(|a, b| b.0.total_cmp(&a.0));
        subgraphs
    } else {
        vec![]
    };
    // (left, right) depths of each JTS directed edge.
    let mut depths = overlay.unwrap_or_else(|| vec![(None, None); edges.len() * 2]);
    if mode == Mode::Buffer {
        for (index, (_, list)) in subgraphs.iter().enumerate() {
            let start = rightmost_edge(list, &edges, &star, &node_of, &second_of)?;
            // SubgraphDepthLocater: the depth the subgraphs already
            // processed give the rightmost coordinate.
            let p = edges[start / 2]
                .0
                .iter()
                .copied()
                .max_by(|a, b| a[0].total_cmp(&b[0]))?;
            let outside = stabbed_depth(p, &subgraphs[..index], &edges, &depths);
            propagate_depths(start, outside, &star, &node_of, &edge_delta, &mut depths)?;
        }
    }
    let in_result = |d: usize| {
        let (l, r) = depths[d];
        r.is_some_and(|r| r >= 1) && l.is_some_and(|l| l <= 0)
    };
    // DirectedEdgeStar.linkResultDirectedEdges
    let mut next = vec![usize::MAX; edges.len() * 2];
    for list in &star {
        let mut first_out = None;
        let mut incoming = None;
        for out in list
            .iter()
            .copied()
            .filter(|&d| in_result(d) || in_result(d ^ 1))
        {
            let sym = out ^ 1;
            if first_out.is_none() && in_result(out) {
                first_out = Some(out);
            }
            match incoming {
                None => {
                    if in_result(sym) {
                        incoming = Some(sym);
                    }
                }
                Some(i) => {
                    if in_result(out) {
                        next[i] = out;
                        incoming = None;
                    }
                }
            }
        }
        if let Some(i) = incoming {
            next[i] = first_out?;
        }
    }
    // The order PolygonBuilder meets the directed edges in.
    let order = match mode {
        Mode::Overlay => (0..edges.len() * 2).collect::<Vec<_>>(),
        Mode::Buffer => subgraphs.into_iter().flat_map(|(_, list)| list).collect(),
    };
    // buildMaximalEdgeRings
    let mut ring_of = vec![usize::MAX; edges.len() * 2];
    let mut maximal = Vec::<Vec<usize>>::new();
    for &d in &order {
        if !in_result(d) || ring_of[d] != usize::MAX {
            continue;
        }
        let mut ring = Vec::new();
        let mut e = d;
        loop {
            if e == usize::MAX || ring_of[e] != usize::MAX {
                // "Directed Edge visited twice during ring-building".
                return None;
            }
            ring_of[e] = maximal.len();
            ring.push(e);
            e = next[e];
            if e == d {
                break;
            }
        }
        maximal.push(ring);
    }
    let points_of = |ring: &[usize]| {
        let len = 1 + ring
            .iter()
            .map(|&d| edges[d / 2].0.len() - 1)
            .sum::<usize>();
        let mut points = Vec::with_capacity(len);
        for (i, &d) in ring.iter().enumerate() {
            extend_edge_points(&mut points, d, i == 0);
        }
        points
    };
    // A JTS hole is counter-clockwise.
    let is_hole = |points: &[Point]| signed_area(points) > 0.0;
    let mut shells = Vec::<(Vec<Point>, Vec<Vec<Point>>)>::new();
    let mut free_holes = Vec::<Vec<Point>>::new();
    // The maximal ring that last visited each node.
    let mut visited_by = vec![usize::MAX; node_points.len()];
    for (index, ring) in maximal.iter().enumerate() {
        let mut node_visited_twice = false;
        for &d in ring {
            let n = node_of(d);
            if visited_by[n] == index {
                node_visited_twice = true;
                break;
            }
            visited_by[n] = index;
        }
        if !node_visited_twice {
            let points = points_of(ring);
            if is_hole(&points) {
                free_holes.push(points);
            } else {
                shells.push((points, vec![]));
            }
            continue;
        }
        // linkMinimalDirectedEdges: clockwise around each node, over the
        // edges of this maximal ring.
        let mut next_min = HashMap::<usize, usize>::default();
        let mut done = HashSet::default();
        for &d in ring {
            let n = node_of(d);
            if !done.insert(n) {
                continue;
            }
            let mut first_out = None;
            let mut incoming = None;
            for out in star[n]
                .iter()
                .copied()
                .filter(|&e| in_result(e) || in_result(e ^ 1))
                .rev()
            {
                let sym = out ^ 1;
                if first_out.is_none() && ring_of[out] == index {
                    first_out = Some(out);
                }
                match incoming {
                    None => {
                        if ring_of[sym] == index {
                            incoming = Some(sym);
                        }
                    }
                    Some(i) => {
                        if ring_of[out] == index {
                            next_min.insert(i, out);
                            incoming = None;
                        }
                    }
                }
            }
            if let Some(i) = incoming {
                next_min.insert(i, first_out?);
            }
        }
        // buildMinimalRings from the maximal ring's start.
        let mut assigned = HashSet::default();
        let mut minimal = Vec::new();
        for &d in ring {
            if assigned.contains(&d) {
                continue;
            }
            let mut min_ring = Vec::new();
            let mut e = d;
            loop {
                if !assigned.insert(e) {
                    return None;
                }
                min_ring.push(e);
                e = *next_min.get(&e)?;
                if e == d {
                    break;
                }
            }
            minimal.push(points_of(&min_ring));
        }
        // findShell: at most one shell, which keeps the other rings as holes.
        let shell_count = minimal.iter().filter(|r| !is_hole(r)).count();
        if shell_count > 1 {
            // "found two shells in MinimalEdgeRing list".
            return None;
        }
        if shell_count == 1 {
            let shell = minimal.iter().position(|r| !is_hole(r))?;
            let shell_ring = minimal.remove(shell);
            shells.push((shell_ring, minimal));
        } else {
            free_holes.extend(minimal);
        }
    }
    // placeFreeHoles with findEdgeRingContaining.
    let envelope = |ring: &[Point]| {
        let (mut min, mut max) = ([f64::INFINITY; 2], [f64::NEG_INFINITY; 2]);
        for p in ring {
            for axis in 0..2 {
                min[axis] = min[axis].min(p[axis]);
                max[axis] = max[axis].max(p[axis]);
            }
        }
        (min, max)
    };
    let contains = |(amin, amax): Envelope, (bmin, bmax): Envelope| {
        amin[0] <= bmin[0] && amin[1] <= bmin[1] && amax[0] >= bmax[0] && amax[1] >= bmax[1]
    };
    let shell_envelopes = shells.iter().map(|(s, _)| envelope(s)).collect::<Vec<_>>();
    for hole in free_holes {
        let hole_envelope = envelope(&hole);
        let mut best: Option<usize> = None;
        for (s, shell_envelope) in shell_envelopes.iter().enumerate() {
            if !contains(*shell_envelope, hole_envelope) {
                continue;
            }
            let shell = &shells[s].0;
            let Some(test) = hole.iter().find(|p| !shell.contains(p)) else {
                continue;
            };
            if !in_ring_or_boundary(*test, shell) {
                continue;
            }
            if best.is_none_or(|b| contains(shell_envelopes[b], *shell_envelope)) {
                best = Some(s);
            }
        }
        // "unable to assign hole to a shell".
        shells[best?].1.push(hole);
    }
    let line = |ring: Vec<Point>| {
        LineString::new(
            ring.into_iter()
                .map(|p| Coord { x: p[0], y: p[1] })
                .collect(),
        )
    };
    Some(MultiPolygon(
        shells
            .into_iter()
            .map(|(shell, holes)| Polygon::new(line(shell), holes.into_iter().map(line).collect()))
            .collect(),
    ))
}

/// The (left, right) depths of each JTS directed edge of an overlay's
/// edges, propagated around a graph of their segments.
fn overlay_depths(edges: &[(Vec<Point>, i64)]) -> Option<Vec<(Option<i64>, Option<i64>)>> {
    // Depths are propagated on the segments, whose every vertex is a node.
    let point_count: usize = edges.iter().map(|(points, _)| points.len()).sum();
    let mut vertices = Vec::<Point>::with_capacity(point_count);
    let mut ids = HashMap::<(u64, u64), usize>::default();
    ids.reserve(point_count);
    let mut id = |p: Point, vertices: &mut Vec<Point>| {
        *ids.entry(key(p)).or_insert_with(|| {
            vertices.push(p);
            vertices.len() - 1
        })
    };
    let mut segment_index = HashMap::<(usize, usize), usize>::default();
    segment_index.reserve(point_count);
    let mut segments = Vec::<((usize, usize), i64)>::with_capacity(point_count);
    // The first segment of each edge, as a half-edge in the edge's direction.
    let mut edge_first = Vec::with_capacity(edges.len());
    for (points, delta) in edges {
        let mut first = None;
        for pair in points.windows(2) {
            let (u, v) = (id(pair[0], &mut vertices), id(pair[1], &mut vertices));
            if u == v {
                continue;
            }
            let k = (u.min(v), u.max(v));
            let s = *segment_index.entry(k).or_insert_with(|| {
                segments.push((k, 0));
                segments.len() - 1
            });
            segments[s].1 += if u < v { *delta } else { -*delta };
            first.get_or_insert(2 * s + usize::from(u > v));
        }
        edge_first.push(first?);
    }
    let from = |h: usize| {
        let (a, b) = segments[h / 2].0;
        if h.is_multiple_of(2) { a } else { b }
    };
    let to = |h: usize| from(h ^ 1);
    // Depth on the left minus depth on the right.
    let delta = |h: usize| {
        if h.is_multiple_of(2) {
            segments[h / 2].1
        } else {
            -segments[h / 2].1
        }
    };
    // The half-edges out of each vertex, in one vector: those of vertex v
    // are `out_edges[out_start[v]..out_start[v + 1]]`, in star order.
    let mut out_start = vec![0usize; vertices.len() + 1];
    for h in 0..segments.len() * 2 {
        out_start[from(h) + 1] += 1;
    }
    for v in 0..vertices.len() {
        out_start[v + 1] += out_start[v];
    }
    let mut out_edges = vec![0usize; segments.len() * 2];
    let mut fill = out_start[..vertices.len()].to_vec();
    for h in 0..segments.len() * 2 {
        let v = from(h);
        out_edges[fill[v]] = h;
        fill[v] += 1;
    }
    for vertex in 0..vertices.len() {
        let origin = vertices[vertex];
        out_edges[out_start[vertex]..out_start[vertex + 1]].sort_by(|&g, &h| {
            edge_end_order(origin, vertices[to(g)], vertices[to(h)]).then(g.cmp(&h))
        });
    }
    let outgoing = |v: usize| &out_edges[out_start[v]..out_start[v + 1]];
    // Connected components (BufferBuilder.createSubgraphs).
    let mut parent = (0..vertices.len()).collect::<Vec<_>>();
    fn root(parent: &mut [usize], mut i: usize) -> usize {
        while parent[i] != i {
            parent[i] = parent[parent[i]];
            i = parent[i];
        }
        i
    }
    for ((a, b), _) in &segments {
        let (ra, rb) = (root(&mut parent, *a), root(&mut parent, *b));
        parent[ra] = rb;
    }
    // (root, vertex) of every vertex with an edge, by root and then vertex:
    // the components in order of their roots.
    let mut by_root = (0..vertices.len())
        .filter(|&v| !outgoing(v).is_empty())
        .map(|v| (root(&mut parent, v), v))
        .collect::<Vec<_>>();
    by_root.sort_unstable();
    let segment_roots = segments
        .iter()
        .map(|((a, _), _)| root(&mut parent, *a))
        .collect::<Vec<_>>();
    // Depth on the right of each half-edge; the left is right + delta.
    let mut right = vec![None::<i64>; segments.len() * 2];
    // Shared by the components' searches, which stay each in its own.
    let mut visited = vec![false; vertices.len()];
    for members in by_root.chunk_by(|a, b| a.0 == b.0) {
        let component = &members[0].0;
        // The rightmost vertex; the sector around it containing +x is
        // outside this component, at the depth the other components give
        // that point (SubgraphDepthLocater).
        let start = members.iter().map(|&(_, v)| v).max_by(|&a, &b| {
            vertices[a][0]
                .total_cmp(&vertices[b][0])
                .then(vertices[a][1].total_cmp(&vertices[b][1]))
        })?;
        let p = vertices[start];
        let mut outside = 0;
        for (((a, b), d), r) in segments.iter().zip(&segment_roots) {
            if r == component {
                continue;
            }
            let (u, v) = (vertices[*a], vertices[*b]);
            if u[1] <= p[1] {
                if v[1] > p[1] && orientation(u, v, p) == Orientation::CounterClockwise {
                    outside += d;
                }
            } else if v[1] <= p[1] && orientation(u, v, p) == Orientation::Clockwise {
                outside -= d;
            }
        }
        // BufferSubgraph.computeDepths: breadth-first over the nodes, each
        // node's star walked counter-clockwise from an edge of known depth.
        let first = outgoing(start)[0];
        right[first] = Some(outside);
        right[first ^ 1] = Some(outside + delta(first));
        let mut queue = std::collections::VecDeque::from([start]);
        visited[start] = true;
        while let Some(node) = queue.pop_front() {
            let star = outgoing(node);
            let seed = (0..star.len()).find(|&i| right[star[i]].is_some())?;
            // DirectedEdgeStar.computeDepths
            let target = right[star[seed]]?;
            let mut current = target + delta(star[seed]);
            for offset in 1..star.len() {
                let h = star[(seed + offset) % star.len()];
                // A depth already known from another node must agree; when
                // it does not, the noded curves cross somewhere unnoded and
                // JTS's ring building fails, so the caller retries snapped.
                if right[h].is_some_and(|known| known != current) {
                    return None;
                }
                right[h] = Some(current);
                // copySymDepths: the twin's right is this edge's left.
                right[h ^ 1] = Some(current + delta(h));
                current += delta(h);
            }
            if current != target {
                // TopologyException("depth mismatch").
                return None;
            }
            for &h in star {
                let next = to(h);
                if !visited[next] {
                    visited[next] = true;
                    queue.push_back(next);
                }
            }
        }
    }
    let mut depths = vec![(None::<i64>, None::<i64>); edges.len() * 2];
    for (d, entry) in depths.iter_mut().enumerate() {
        let h = edge_first[d / 2] ^ (d % 2);
        *entry = (right[h].map(|r| r + delta(h)), right[h]);
    }
    Some(depths)
}

/// `RightmostEdgeFinder.findEdge`: a directed edge of the subgraph whose
/// right side is outside it.
/// JTS `SubgraphDepthLocater.getDepth`: the left depth of the least segment,
/// by `DepthSegment.compareTo`, among the upward segments of the forward
/// directed edges of earlier subgraphs that the ray from `p` towards +x
/// crosses. For valid input this is the winding depth at `p`; for invalid
/// input JTS's choice decides, and `Collections.min` over a comparator that
/// is not transitive depends on the order the segments are met in, which is
/// kept here.
fn stabbed_depth(
    p: Point,
    processed: &[(f64, Vec<usize>)],
    edges: &[(Vec<Point>, i64)],
    depths: &[(Option<i64>, Option<i64>)],
) -> i64 {
    // JTS initialises unset directed-edge depths to -999.
    const UNSET: i64 = -999;
    let mut least: Option<([Point; 2], i64)> = None;
    for (_, list) in processed {
        let (low, high) = list
            .iter()
            .flat_map(|&d| &edges[d / 2].0)
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), q| {
                (lo.min(q[1]), hi.max(q[1]))
            });
        if p[1] < low || p[1] > high {
            continue;
        }
        for &d in list.iter().filter(|&&d| d % 2 == 0) {
            for pair in edges[d / 2].0.windows(2) {
                let flipped = pair[0][1] > pair[1][1];
                let seg = if flipped {
                    [pair[1], pair[0]]
                } else {
                    [pair[0], pair[1]]
                };
                if seg[0][0].max(seg[1][0]) < p[0]
                    || seg[0][1] == seg[1][1]
                    || p[1] < seg[0][1]
                    || p[1] > seg[1][1]
                    || orientation(seg[0], seg[1], p) == Orientation::Clockwise
                {
                    continue;
                }
                let (left, right) = depths[d];
                let depth = if flipped { right } else { left }.unwrap_or(UNSET);
                match &least {
                    Some((best, _)) if depth_segment_cmp(seg, *best).is_ge() => {}
                    _ => least = Some((seg, depth)),
                }
            }
        }
    }
    least.map_or(0, |(_, depth)| depth)
}

/// `SubgraphDepthLocater.DepthSegment.compareTo` on upward segments.
fn depth_segment_cmp(a: [Point; 2], b: [Point; 2]) -> std::cmp::Ordering {
    let min = |s: [Point; 2], k: usize| s[0][k].min(s[1][k]);
    let max = |s: [Point; 2], k: usize| s[0][k].max(s[1][k]);
    if min(a, 0) >= max(b, 0)
        || max(a, 0) <= min(b, 0)
        || min(a, 1) >= max(b, 1)
        || max(a, 1) <= min(b, 1)
    {
        // LineSegment.compareTo: p0, then p1, each by x then y.
        let coordinate = |p: Point, q: Point| p[0].total_cmp(&q[0]).then(p[1].total_cmp(&q[1]));
        return coordinate(a[0], b[0]).then(coordinate(a[1], b[1]));
    }
    // LineSegment.orientationIndex.
    let index = |s: [Point; 2], q: Point| match orientation(s[0], s[1], q) {
        Orientation::CounterClockwise => 1,
        Orientation::Clockwise => -1,
        Orientation::Collinear => 0,
    };
    let orientation_index = |s: [Point; 2], t: [Point; 2]| {
        let (o0, o1) = (index(s, t[0]), index(s, t[1]));
        if o0 >= 0 && o1 >= 0 {
            o0.max(o1)
        } else if o0 <= 0 && o1 <= 0 {
            o0.min(o1)
        } else {
            0
        }
    };
    let mut result = orientation_index(a, b);
    if result == 0 {
        result = -orientation_index(b, a);
    }
    result.cmp(&0)
}

fn rightmost_edge(
    list: &[usize],
    edges: &[(Vec<Point>, i64)],
    star: &[Vec<usize>],
    node_of: &impl Fn(usize) -> usize,
    second_of: &impl Fn(usize) -> Point,
) -> Option<usize> {
    let mut best: Option<(usize, usize, Point)> = None;
    // checkForRightmostCoordinate over the forward directed edges.
    let check = |d: usize, best: &mut Option<(usize, usize, Point)>| {
        let points = &edges[d / 2].0;
        for (i, &p) in points[..points.len() - 1].iter().enumerate() {
            if best.is_none_or(|(_, _, c)| p[0] > c[0]) {
                *best = Some((d, i, p));
            }
        }
    };
    for &d in list {
        if d.is_multiple_of(2) {
            check(d, &mut best);
        }
    }
    let (mut de, mut index, coord) = best?;
    let points = |d: usize| &edges[d / 2].0;
    if index == 0 {
        // findRightmostEdgeAtNode: DirectedEdgeStar.getRightmostEdge.
        let ends = &star[node_of(de)];
        let northern = |d: usize| second_of(d)[1] >= edges_start(edges, d)[1];
        let first = *ends.first()?;
        let last = *ends.last()?;
        de = if ends.len() == 1 || (northern(first) && northern(last)) {
            first
        } else if !northern(first) && !northern(last) {
            last
        } else if second_of(first)[1] != edges_start(edges, first)[1] {
            first
        } else if second_of(last)[1] != edges_start(edges, last)[1] {
            last
        } else {
            // "found two horizontal edges incident on node".
            return None;
        };
        if de % 2 == 1 {
            de ^= 1;
            index = points(de).len() - 1;
        }
    } else {
        // findRightmostEdgeAtVertex
        let pts = points(de);
        let (prev, next) = (pts[index - 1], pts[index + 1]);
        let turn = orientation(coord, next, prev);
        if (prev[1] < coord[1] && next[1] < coord[1] && turn == Orientation::CounterClockwise)
            || (prev[1] > coord[1] && next[1] > coord[1] && turn == Orientation::Clockwise)
        {
            index -= 1;
        }
    }
    // getRightmostSide, on the edge's own coordinates.
    let side_of = |i: isize| -> Option<bool> {
        let pts = points(de);
        if i < 0 || i as usize + 1 >= pts.len() {
            return None;
        }
        let (a, b) = (pts[i as usize], pts[i as usize + 1]);
        if a[1] == b[1] {
            return None;
        }
        // true for Position.LEFT
        Some(a[1] >= b[1])
    };
    let left = side_of(index as isize).or_else(|| side_of(index as isize - 1));
    Some(if left == Some(true) { de ^ 1 } else { de })
}

fn edges_start(edges: &[(Vec<Point>, i64)], d: usize) -> Point {
    let points = &edges[d / 2].0;
    if d.is_multiple_of(2) {
        points[0]
    } else {
        points[points.len() - 1]
    }
}

/// `BufferSubgraph.computeDepth`: the right of `start` is at `outside`; the
/// depths spread breadth-first over the nodes. `None` where JTS throws.
fn propagate_depths(
    start: usize,
    outside: i64,
    star: &[Vec<usize>],
    node_of: &impl Fn(usize) -> usize,
    edge_delta: &impl Fn(usize) -> i64,
    depths: &mut [(Option<i64>, Option<i64>)],
) -> Option<()> {
    // DirectedEdge.setDepth refuses a different depth than one assigned.
    fn set(slot: &mut Option<i64>, value: i64) -> Option<()> {
        if slot.is_some_and(|v| v != value) {
            return None;
        }
        *slot = Some(value);
        Some(())
    }
    // setEdgeDepths(RIGHT, depth)
    let set_right = |d: usize, depth: i64, depths: &mut [(Option<i64>, Option<i64>)]| {
        set(&mut depths[d].1, depth)?;
        set(&mut depths[d].0, depth + edge_delta(d))
    };
    let copy_sym = |d: usize, depths: &mut [(Option<i64>, Option<i64>)]| {
        let (l, r) = depths[d];
        if let Some(r) = r {
            set(&mut depths[d ^ 1].0, r)?;
        }
        if let Some(l) = l {
            set(&mut depths[d ^ 1].1, l)?;
        }
        Some(())
    };
    set_right(start, outside, depths)?;
    copy_sym(start, depths)?;
    let mut visited = vec![false; depths.len()];
    visited[start] = true;
    let first = node_of(start);
    let mut seen = HashSet::from_iter([first]);
    let mut queue = std::collections::VecDeque::from([first]);
    while let Some(n) = queue.pop_front() {
        let list = &star[n];
        // computeNodeDepth
        let index = list.iter().position(|&d| visited[d] || visited[d ^ 1])?;
        let de = list[index];
        let start_depth = depths[de].0?;
        let target = depths[de].1?;
        let mut current = start_depth;
        for &d in list[index + 1..].iter().chain(&list[..index]) {
            set_right(d, current, depths)?;
            current = depths[d].0?;
        }
        if current != target {
            // "depth mismatch".
            return None;
        }
        for &d in list {
            visited[d] = true;
            copy_sym(d, depths)?;
        }
        for &d in list {
            if visited[d ^ 1] {
                continue;
            }
            let other = node_of(d ^ 1);
            if seen.insert(other) {
                queue.push_back(other);
            }
        }
    }
    Some(())
}

/// `PointLocation.isInRing`: inside or on the boundary.
fn in_ring_or_boundary(p: Point, ring: &[Point]) -> bool {
    for pair in ring.windows(2) {
        if orientation(pair[0], pair[1], p) == Orientation::Collinear
            && p[0] >= pair[0][0].min(pair[1][0])
            && p[0] <= pair[0][0].max(pair[1][0])
            && p[1] >= pair[0][1].min(pair[1][1])
            && p[1] <= pair[0][1].max(pair[1][1])
        {
            return true;
        }
    }
    in_ring(p, ring)
}

/// Strict point-in-ring by crossing number with exact orientation.
fn in_ring(p: Point, ring: &[Point]) -> bool {
    let mut winding = 0;
    for pair in ring.windows(2) {
        let (a, b) = (pair[0], pair[1]);
        if a[1] <= p[1] {
            if b[1] > p[1] && orientation(a, b, p) == Orientation::CounterClockwise {
                winding += 1;
            }
        } else if b[1] <= p[1] && orientation(a, b, p) == Orientation::Clockwise {
            winding -= 1;
        }
    }
    winding != 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Area;

    #[test]
    fn zero_buffer_takes_depth_from_jts_least_stabbed_segment() {
        // Pomorskie 14/8986/5261, relation 13340268 (grassland around
        // ponds), in MVT units. The ray from the westmost pond's rightmost
        // point crosses the two ponds east of it; JTS's DepthSegment order
        // picks the lower pond's edge, so even this valid polygon loses that
        // pond in buffer(0), as Planetiler's tile does after its repair.
        let ring = |points: &[(f64, f64)]| LineString::from(points.to_vec());
        let shell = ring(&[
            (2857.625, 1438.1875),
            (3774.625, 1376.375),
            (3896.5, 1363.5625),
            (4083.9375, 1543.0),
            (4160.0, 1614.2778705284),
            (4160.0, 2758.317262135446),
            (3896.8125, 2491.3125),
            (3633.9375, 3125.375),
            (3347.9375, 3792.0625),
            (3168.9375, 3768.375),
            (2706.0625, 3696.0),
            (2678.25, 4028.4375),
            (2373.625, 4000.625),
            (1775.125, 3869.1875),
            (1855.5625, 3266.125),
            (1897.3125, 2922.8125),
            (2732.375, 3037.25),
            (2775.625, 2536.1875),
            (2809.6875, 2036.6875),
            (2857.625, 1438.1875),
        ]);
        let west = ring(&[
            (1897.3125, 3301.6875),
            (1823.0625, 3864.5625),
            (2627.1875, 3985.1875),
            (2689.0625, 3437.75),
            (1897.3125, 3301.6875),
        ]);
        let upper = ring(&[
            (3536.5, 3215.0625),
            (2760.1875, 3106.8125),
            (2732.375, 3382.0625),
            (3418.9375, 3493.375),
            (3536.5, 3215.0625),
        ]);
        let lower = ring(&[
            (2732.375, 3428.4375),
            (2720.0, 3660.4375),
            (3326.1875, 3747.0),
            (3403.5, 3530.5),
            (2732.375, 3428.4375),
        ]);
        let input = MultiPolygon(vec![Polygon::new(shell, vec![west, upper, lower])]);
        let out = buffer(&input, 0.0).unwrap();
        assert_eq!(out.0.len(), 1);
        let mut starts = out.0[0]
            .interiors()
            .iter()
            .map(|h| (h.0[0].x, h.0[0].y))
            .collect::<Vec<_>>();
        starts.sort_by(|a, b| a.0.total_cmp(&b.0));
        assert_eq!(starts, vec![(2732.375, 3428.4375), (3536.5, 3215.0625)]);
    }

    fn square(x: f64, y: f64, size: f64) -> MultiPolygon<f64> {
        MultiPolygon(vec![Polygon::new(
            LineString::from(vec![
                (x, y),
                (x + size, y),
                (x + size, y + size),
                (x, y + size),
                (x, y),
            ]),
            vec![],
        )])
    }

    #[test]
    fn math_matches_java_where_the_platform_rounds_differently() {
        // (x, y, Math.hypot) from JDK 21, where the platform hypot differs.
        for (x, y, expected) in [
            (0x409D20CE81BFD15C, 0x409773DDE188B804, 0x40A2B2B5D50B3B22),
            (0x3FE81F779F726A20, 0x3F129F6FE47E5DD3, 0x3FE81F77A13E784F),
            (0x40802B8000000000, 0xC0427C80F82A8AC9, 0x4080360DC8F4914F),
            (0xBFF35ECFA73DE2AE, 0x3F3010FD6EC58417, 0x3FF35ECFADE79AFD),
            (0x3FC5A6988E1CDB7C, 0xBF392EA4A6F1173C, 0x3FC5A69C37614E1B),
            (0x40AC48A000000000, 0x40945A95DD718AE6, 0x40AE0F138D897288),
        ] {
            let (x, y) = (f64::from_bits(x), f64::from_bits(y));
            assert_eq!(super::java_hypot(x, y).to_bits(), expected);
            // Scaling to MVT units keeps it exact.
            assert_eq!(
                super::java_hypot(16.0 * x, 16.0 * y),
                16.0 * f64::from_bits(expected)
            );
        }
        // (y, x, Math.atan2), where the platform atan2 differs.
        for (y, x, expected) in [
            (0x4038EABDE93DA016, 0xC02E568B0F784180, 0x4000F0F206C35424),
            (0x4039B351F9C76D06, 0xC02F99C1BF6AD158, 0x4000F9E44F0F2E2E),
            (0xC035105C59770668, 0xC03216475EA6D78F, 0xC0023E10445EB866),
            (0x40388FFDB9E56EA7, 0x4022C10C44C2155D, 0x3FF34C396C688064),
            (0xBFFCD8184ABED924, 0x403A3EE25B7242FF, 0xBFB18E7838A5EB30),
            (0x403CB71B23256F5A, 0xC046972B4D84BAB2, 0x40049A7DDF738BBA),
        ] {
            assert_eq!(
                super::java_atan2(f64::from_bits(y), f64::from_bits(x)).to_bits(),
                expected
            );
        }
        // (angle, Math.sin, Math.cos) of limited mitre joins in landuse tile
        // 9/286/174, where the platform sin is a unit in the last place off.
        for (angle, sin, cos) in [
            (
                0x40020F5A6F6B6867_u64,
                0x3FE8BF3804B54771_u64,
                0xBFE449A319F347EE_u64,
            ),
            (0x4006C7DDF98360A0, 0x3FD28BD2D0610673, 0xBFEEA07F75210D10),
            (0xC003BC88E8FE52C9, 0xBFE3FC2E9AEB7C8F, 0xBFE8FDEED1A23958),
        ] {
            let (s, c) = super::java_sin_cos(f64::from_bits(angle));
            assert_eq!((s.to_bits(), c.to_bits()), (sin, cos));
        }
    }

    #[test]
    fn mitred_square_buffer_and_erosion() {
        let grown = buffer(&square(0., 0., 10.), 1.).unwrap();
        assert_eq!(grown.0.len(), 1);
        assert!((grown.unsigned_area() - 144.).abs() < 1e-9);
        let shrunk = buffer(&square(0., 0., 10.), -1.).unwrap();
        assert!((shrunk.unsigned_area() - 64.).abs() < 1e-9);
        assert!(buffer(&square(0., 0., 10.), -6.).unwrap().0.is_empty());
    }

    #[test]
    fn union_of_overlapping_and_disjoint_squares() {
        let merged = union(&[square(0., 0., 2.), square(1., 1., 2.), square(5., 5., 1.)]).unwrap();
        assert_eq!(merged.0.len(), 2);
        assert!((merged.unsigned_area() - 8.).abs() < 1e-12);
    }

    #[test]
    fn union_keeps_hole_and_jts_orientation() {
        let frame = MultiPolygon(vec![Polygon::new(
            LineString::from(vec![(0., 0.), (6., 0.), (6., 6.), (0., 6.), (0., 0.)]),
            vec![LineString::from(vec![
                (2., 2.),
                (2., 4.),
                (4., 4.),
                (4., 2.),
                (2., 2.),
            ])],
        )]);
        let merged = union(&[frame, square(10., 10., 1.)]).unwrap();
        let polygon = merged.0.iter().find(|p| p.interiors().len() == 1).unwrap();
        assert!(signed_area(&clean(polygon.exterior())) < 0.);
        assert!(signed_area(&clean(&polygon.interiors()[0])) > 0.);
        assert!((merged.unsigned_area() - 33.).abs() < 1e-12);
    }
}
