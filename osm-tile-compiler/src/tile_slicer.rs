use crate::coordinate_groups::{WorldPoint, WorldRingGroup};
use geo::{BooleanOps, Coord, LineString, Polygon, Rect};

#[derive(Debug, Clone, Copy)]
pub(crate) struct WorldRect {
    pub(crate) min_x: f64,
    pub(crate) max_x: f64,
    pub(crate) min_y: f64,
    pub(crate) max_y: f64,
}

// Segments lying entirely beyond one side of a range by more than this are
// skipped without clipping. The margin keeps the skip clear of the rounding in
// `slice_segment`, which is what decides the segments that remain.
const SEGMENT_APART_MARGIN: f64 = 1e-6;

// True when both ends are below `min` or both above `max` by the margin, so
// `slice_segment` rejects the segment: a zero delta fails its sign
// test on that side, otherwise the entry/exit parameter on that side leaves
// [0, 1] (above 1 or below 0) by far more than rounding can change.
fn segment_apart(a: f64, b: f64, min: f64, max: f64) -> bool {
    let (lo, hi) = (min - SEGMENT_APART_MARGIN, max + SEGMENT_APART_MARGIN);
    (a < lo && b < lo) || (a > hi && b > hi)
}

/// Indices of the segments of `points` that may reach the vertical stripe
/// `min_x..=max_x` of a tile column, ascending. Every other segment is rejected
/// by `slice_segment` for any tile of that column.
pub(crate) fn segments_in_x_range(points: &[WorldPoint], min_x: f64, max_x: f64) -> Vec<u32> {
    (0..points.len().saturating_sub(1))
        .filter(|&i| !segment_apart(points[i].x, points[i + 1].x, min_x, max_x))
        .map(|i| i as u32)
        .collect()
}

/// Clips the polyline to `rect` considering only `segments` (ascending indices
/// from `segments_in_x_range` for a column containing `rect`). Segments that are
/// not listed, or are apart from `rect` in Y, clip to nothing, so they act as
/// the break the full segment walk makes at them. The parts come out with X
/// local to the tile column `column`, as Planetiler slices them.
pub(crate) fn clip_polyline_segments_to_rect(
    points: &[WorldPoint],
    segments: &[u32],
    rect: WorldRect,
    column: f64,
) -> Vec<Vec<WorldPoint>> {
    let mut parts = Vec::new();
    let mut current = Vec::<WorldPoint>::new();
    // Index of the segment following the last one visited.
    let mut expected = 0;
    for &index in segments {
        let index = index as usize;
        let a = points[index];
        let b = points[index + 1];
        if segment_apart(a.y, b.y, rect.min_y, rect.max_y) {
            continue;
        }
        if index != expected {
            // The skipped segments between break the line.
            if current.len() >= 2 {
                parts.push(dedupe_world_line(std::mem::take(&mut current)));
            } else {
                current.clear();
            }
        }
        expected = index + 1;
        if let Some((start, end)) = slice_segment(a, b, rect, column) {
            if current.is_empty() {
                current.push(start);
                if !approx_world_point(start, end) {
                    current.push(end);
                }
            } else if approx_world_point(*current.last().unwrap_or(&start), start) {
                if !approx_world_point(*current.last().unwrap_or(&start), end) {
                    current.push(end);
                }
            } else {
                if current.len() >= 2 {
                    parts.push(dedupe_world_line(current));
                }
                current = vec![start];
                if !approx_world_point(start, end) {
                    current.push(end);
                }
            }
        } else if current.len() >= 2 {
            parts.push(dedupe_world_line(std::mem::take(&mut current)));
        } else {
            current.clear();
        }
    }
    if current.len() >= 2 {
        parts.push(dedupe_world_line(current));
    }
    parts.into_iter().filter(|line| line.len() >= 2).collect()
}

fn world_group_polygon(group: &WorldRingGroup) -> Polygon<f64> {
    let ring = |points: &[WorldPoint]| {
        LineString::new(points.iter().map(|p| Coord { x: p.x, y: p.y }).collect())
    };
    Polygon::new(
        ring(&group.exterior),
        group.holes.iter().map(|h| ring(h)).collect(),
    )
}

/// Polygon inputs retained for repeated tile intersections, in original group order.
pub(crate) struct PreparedPolygonGroups(Vec<Polygon<f64>>);

pub(crate) fn prepare_polygon_groups(groups: &[WorldRingGroup]) -> PreparedPolygonGroups {
    PreparedPolygonGroups(groups.iter().map(world_group_polygon).collect())
}

pub(crate) fn clip_prepared_polygon_groups_to_rect(
    groups: &PreparedPolygonGroups,
    rect: WorldRect,
) -> Vec<WorldRingGroup> {
    let clip = Rect::new(
        Coord {
            x: rect.min_x,
            y: rect.min_y,
        },
        Coord {
            x: rect.max_x,
            y: rect.max_y,
        },
    )
    .to_polygon();
    groups
        .0
        .iter()
        .flat_map(|group| {
            // A polygon-set intersection preserves crossing holes as notches and
            // may split a concave shell into several disconnected components.
            group.intersection(&clip).0.into_iter().map(|polygon| {
                let ring = |line: &LineString<f64>| {
                    line.points()
                        .map(|p| WorldPoint::new(p.x(), p.y()))
                        .collect()
                };
                WorldRingGroup {
                    exterior: ring(polygon.exterior()),
                    holes: polygon.interiors().iter().map(ring).collect(),
                }
            })
        })
        .collect()
}

fn dedupe_world_line(points: Vec<WorldPoint>) -> Vec<WorldPoint> {
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

/// One segment through Planetiler's `TiledGeometry`: `sliceX` in X local to
/// the tile column `column`, then `sliceY` from the X-sliced points, each
/// crossing by `intersectX`/`intersectY` from the segment start. Returns the
/// part inside `rect`, with column-local X: a segment that leaves through an
/// X side and a Y side gets its Y crossing from the X crossing, as in
/// Planetiler, not from the original start.
fn slice_segment(
    a: WorldPoint,
    b: WorldPoint,
    rect: WorldRect,
    column: f64,
) -> Option<(WorldPoint, WorldPoint)> {
    let (a, b) = (
        WorldPoint::new(a.x - column, a.y),
        WorldPoint::new(b.x - column, b.y),
    );
    let intersect_x = |a: WorldPoint, b: WorldPoint, x: f64| {
        let t = (x - a.x) / (b.x - a.x);
        WorldPoint::new(x, a.y + (b.y - a.y) * t)
    };
    let intersect_y = |a: WorldPoint, b: WorldPoint, y: f64| {
        let t = (y - a.y) / (b.y - a.y);
        WorldPoint::new(a.x + (b.x - a.x) * t, y)
    };
    let slice = |a: WorldPoint,
                 b: WorldPoint,
                 axis: fn(WorldPoint) -> f64,
                 lower: f64,
                 upper: f64,
                 cross: &dyn Fn(WorldPoint, WorldPoint, f64) -> WorldPoint| {
        let (av, bv) = (axis(a), axis(b));
        let start = if av < lower {
            (bv > lower).then(|| cross(a, b, lower))?
        } else if av > upper {
            (bv < upper).then(|| cross(a, b, upper))?
        } else {
            a
        };
        let end = if bv < lower {
            cross(a, b, lower)
        } else if bv > upper {
            cross(a, b, upper)
        } else {
            b
        };
        Some((start, end))
    };
    let (a, b) = slice(
        a,
        b,
        |p| p.x,
        rect.min_x - column,
        rect.max_x - column,
        &intersect_x,
    )?;
    slice(a, b, |p| p.y, rect.min_y, rect.max_y, &intersect_y)
}

fn approx_world_point(left: WorldPoint, right: WorldPoint) -> bool {
    (left.x - right.x).abs() < 1e-9 && (left.y - right.y).abs() < 1e-9
}

#[cfg(test)]
mod tests {
    use super::*;

    fn legacy_clip_polygon_groups_to_rect(
        groups: &[WorldRingGroup],
        rect: WorldRect,
    ) -> Vec<WorldRingGroup> {
        let clip = Rect::new(
            Coord {
                x: rect.min_x,
                y: rect.min_y,
            },
            Coord {
                x: rect.max_x,
                y: rect.max_y,
            },
        )
        .to_polygon();
        groups
            .iter()
            .flat_map(|group| {
                // A polygon-set intersection preserves crossing holes as notches and
                // may split a concave shell into several disconnected components.
                world_group_polygon(group)
                    .intersection(&clip)
                    .0
                    .into_iter()
                    .map(|polygon| {
                        let ring = |line: &LineString<f64>| {
                            line.points()
                                .map(|p| WorldPoint::new(p.x(), p.y()))
                                .collect()
                        };
                        WorldRingGroup {
                            exterior: ring(polygon.exterior()),
                            holes: polygon.interiors().iter().map(ring).collect(),
                        }
                    })
            })
            .collect()
    }

    fn old_clip_polyline_to_rect(
        points: &[WorldPoint],
        rect: WorldRect,
        column: f64,
    ) -> Vec<Vec<WorldPoint>> {
        if points.len() < 2 {
            return Vec::new();
        }
        let mut parts = Vec::new();
        let mut current = Vec::<WorldPoint>::new();
        for window in points.windows(2) {
            let a = window[0];
            let b = window[1];
            if let Some((start, end)) = slice_segment(a, b, rect, column) {
                if current.is_empty() {
                    current.push(start);
                    if !approx_world_point(start, end) {
                        current.push(end);
                    }
                } else if approx_world_point(*current.last().unwrap_or(&start), start) {
                    if !approx_world_point(*current.last().unwrap_or(&start), end) {
                        current.push(end);
                    }
                } else {
                    if current.len() >= 2 {
                        parts.push(dedupe_world_line(current));
                    }
                    current = vec![start];
                    if !approx_world_point(start, end) {
                        current.push(end);
                    }
                }
            } else if current.len() >= 2 {
                parts.push(dedupe_world_line(std::mem::take(&mut current)));
            } else {
                current.clear();
            }
        }
        if current.len() >= 2 {
            parts.push(dedupe_world_line(current));
        }
        parts.into_iter().filter(|line| line.len() >= 2).collect()
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

    fn bits(lines: &[Vec<WorldPoint>]) -> Vec<Vec<(u64, u64)>> {
        lines
            .iter()
            .map(|l| l.iter().map(|p| (p.x.to_bits(), p.y.to_bits())).collect())
            .collect()
    }

    // The column prefilter plus per-tile Y skips must reproduce the full segment
    // walk bit for bit, including the breaks made at skipped segments.
    #[test]
    fn column_segment_clipping_matches_full_walk() {
        const BUFFERS: [f64; 3] = [0., 4. / 256., 80. / 256.];
        const NUDGES: [f64; 10] = [0., 0., 1e-7, -1e-7, 5e-7, -5e-7, 1e-6, -1e-6, 2e-6, 1e-5];
        let mut rng = Lcg(0xfeed);
        let mut polylines: Vec<Vec<WorldPoint>> = vec![
            // Axis aligned along tile edges and buffer edges.
            vec![
                WorldPoint::new(1., 0.5),
                WorldPoint::new(1., 6.5),
                WorldPoint::new(3., 6.5),
                WorldPoint::new(3., 1. + 4. / 256.),
                WorldPoint::new(-1., 1. + 4. / 256.),
            ],
            // Degenerate repeated points and a single point.
            vec![
                WorldPoint::new(2.5, 2.5),
                WorldPoint::new(2.5, 2.5),
                WorldPoint::new(2.5, 2.5),
            ],
            vec![WorldPoint::new(2.5, 2.5)],
            vec![],
            // One long diagonal segment across every tile.
            vec![WorldPoint::new(-3., -2.), WorldPoint::new(11., 9.)],
        ];
        for _ in 0..300 {
            let n = 2 + rng.below(14);
            let wide = rng.below(4) == 0;
            let coord = |rng: &mut Lcg| {
                let b = BUFFERS[rng.below(3)];
                let base = match rng.below(6) {
                    0 => 0.,
                    1 => 1.,
                    2 => -b,
                    3 => 1. + b,
                    _ => rng.next() * if wide { 9. } else { 3. },
                };
                let cell = rng.below(7) as f64 - 1.;
                let nudge =
                    NUDGES[rng.below(NUDGES.len())] * if rng.below(2) == 0 { 1. } else { -1. };
                base + cell + nudge
            };
            let mut line = Vec::new();
            for _ in 0..n {
                let p = WorldPoint::new(coord(&mut rng), coord(&mut rng));
                line.push(p);
                // Repeats and axis-aligned continuations.
                match rng.below(5) {
                    0 => line.push(p),
                    1 => line.push(WorldPoint::new(p.x, coord(&mut rng))),
                    2 => line.push(WorldPoint::new(coord(&mut rng), p.y)),
                    _ => {}
                }
            }
            polylines.push(line);
        }
        let (mut nonempty, mut broken) = (0, 0);
        for line in &polylines {
            for buffer in BUFFERS {
                for x in -2..=9 {
                    let (min_x, max_x) = (x as f64 - buffer, x as f64 + 1. + buffer);
                    let segments = segments_in_x_range(line, min_x, max_x);
                    for y in -2..=9 {
                        let rect = WorldRect {
                            min_x,
                            max_x,
                            min_y: y as f64 - buffer,
                            max_y: y as f64 + 1. + buffer,
                        };
                        let expected = old_clip_polyline_to_rect(line, rect, f64::from(x));
                        let actual =
                            clip_polyline_segments_to_rect(line, &segments, rect, f64::from(x));
                        assert_eq!(
                            bits(&actual),
                            bits(&expected),
                            "tile {x},{y} buffer {buffer}: {line:?}"
                        );
                        nonempty += usize::from(!expected.is_empty());
                        broken += usize::from(expected.len() > 1);
                    }
                }
            }
        }
        assert!(nonempty > 2000 && broken > 200, "{nonempty} {broken}");
    }

    #[test]
    fn prepared_polygon_intersections_match_legacy_across_tiles() {
        let ring =
            |coords: &[(f64, f64)]| coords.iter().map(|&(x, y)| WorldPoint::new(x, y)).collect();
        let groups = vec![
            WorldRingGroup {
                exterior: ring(&[
                    (0., 0.),
                    (4., 0.),
                    (4., 1.),
                    (1., 1.),
                    (1., 4.),
                    (0., 4.),
                    (0., 0.),
                ]),
                holes: vec![ring(&[
                    (0.2, 0.2),
                    (0.2, 0.8),
                    (0.8, 0.8),
                    (0.8, 0.2),
                    (0.2, 0.2),
                ])],
            },
            WorldRingGroup {
                exterior: ring(&[(5., 0.), (8., 0.), (8., 3.), (5., 3.), (5., 0.)]),
                holes: vec![ring(&[
                    (5.5, 0.5),
                    (5.5, 2.5),
                    (7.5, 2.5),
                    (7.5, 0.5),
                    (5.5, 0.5),
                ])],
            },
        ];
        let prepared = prepare_polygon_groups(&groups);
        for buffer in [0.0, 0.125] {
            for x in -1..=9 {
                for y in -1..=5 {
                    let rect = WorldRect {
                        min_x: x as f64 - buffer,
                        max_x: x as f64 + 1. + buffer,
                        min_y: y as f64 - buffer,
                        max_y: y as f64 + 1. + buffer,
                    };
                    let expected = legacy_clip_polygon_groups_to_rect(&groups, rect);
                    assert_eq!(
                        clip_prepared_polygon_groups_to_rect(&prepared, rect),
                        expected,
                        "tile {x}/{y}, buffer {buffer}"
                    );
                }
            }
        }
    }
}
