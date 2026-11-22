//! Lines beside a line, cut to length and to a tile, and the surfaces that
//! widened lines add up to.

use super::network::{Point, distance};
use i_overlay::core::fill_rule::FillRule;
use i_overlay::core::overlay::{IntOverlayOptions, Overlay};
use i_overlay::core::overlay_rule::OverlayRule;
use i_overlay::float::single::SingleFloatOverlay;
use i_overlay::i_float::int::point::IntPoint;
use i_overlay::i_shape::base::data::Shapes;
use i_overlay::mesh::float::stroke::offset::StrokeOffset;
use i_overlay::mesh::float::style::{LineJoin, StrokeStyle};

/// A rectangle: west, north, east, south, y growing southwards.
pub(super) type Rect = [f64; 4];

/// The longest a corner reaches past the offset, in offsets.
const MITRE: f64 = 2.0;
/// A bend sharper than this interior angle (radians) is cut off, not drawn to
/// its point.
const SHARPEST: f64 = 0.6;

/// `points` moved `by` to the right of their direction (y grows southwards).
/// Where the inside of a bend is tighter than the offset, the part that would
/// run backwards is left out.
pub(super) fn offset(points: &[Point], by: f64) -> Vec<Point> {
    if by == 0.0 || points.len() < 2 {
        return points.to_vec();
    }
    let normals: Vec<Point> = points
        .windows(2)
        .map(|p| {
            let length = distance(p[0], p[1]).max(f64::MIN_POSITIVE);
            [-(p[1][1] - p[0][1]) / length, (p[1][0] - p[0][0]) / length]
        })
        .collect();
    let closed = points.len() > 3 && points[0] == points[points.len() - 1];
    let corner = |at: Point, a: Point, b: Point| {
        let sum = [a[0] + b[0], a[1] + b[1]];
        let square = sum[0] * sum[0] + sum[1] * sum[1];
        // the mitre is sum * 2 / |sum|², as long as 2 / |sum|
        let reach = if square > 4.0 / (MITRE * MITRE) {
            2.0 / square
        } else {
            MITRE / square.sqrt().max(1e-9)
        };
        [at[0] + sum[0] * reach * by, at[1] + sum[1] * reach * by]
    };
    let last = normals.len();
    let moved: Vec<Point> = (0..points.len())
        .map(|i| {
            let before = if i > 0 {
                normals[i - 1]
            } else if closed {
                normals[last - 1]
            } else {
                normals[0]
            };
            let after = if i < last {
                normals[i]
            } else if closed {
                normals[0]
            } else {
                normals[last - 1]
            };
            corner(points[i], before, after)
        })
        .collect();
    let mut out = vec![moved[0]];
    for i in 1..moved.len() {
        let previous = out[out.len() - 1];
        let along = [
            points[i][0] - points[i - 1][0],
            points[i][1] - points[i - 1][1],
        ];
        let step = [moved[i][0] - previous[0], moved[i][1] - previous[1]];
        if along[0] * step[0] + along[1] * step[1] > 0.0 {
            out.push(moved[i]);
        }
    }
    out
}

/// `points` without their first and last lengths, if anything is left.
pub(super) fn trim(points: &[Point], [start, end]: [f64; 2]) -> Option<Vec<Point>> {
    let total: f64 = points.windows(2).map(|p| distance(p[0], p[1])).sum();
    let (from, to) = (start, total - end);
    if to <= from {
        return None;
    }
    let mut out = Vec::with_capacity(points.len());
    let mut at = 0.0;
    for p in points.windows(2) {
        let length = distance(p[0], p[1]);
        let next = at + length;
        if length > 0.0 && next > from && at < to {
            let lerp = |s: f64| {
                let t = ((s - at) / length).clamp(0.0, 1.0);
                [
                    p[0][0] + (p[1][0] - p[0][0]) * t,
                    p[0][1] + (p[1][1] - p[0][1]) * t,
                ]
            };
            if out.is_empty() {
                out.push(lerp(from.max(at)));
            }
            out.push(lerp(to.min(next)));
        }
        at = next;
    }
    (out.len() > 1).then_some(out)
}

/// The parts of `points` within `rect`.
pub(super) fn clip(points: &[Point], rect: Rect) -> Vec<Vec<Point>> {
    let mut out: Vec<Vec<Point>> = Vec::new();
    let mut open = false;
    for p in points.windows(2) {
        let Some((a, b, leaves)) = clip_segment(p[0], p[1], rect) else {
            open = false;
            continue;
        };
        match out.last_mut() {
            Some(run) if open => run.push(b),
            _ => out.push(vec![a, b]),
        }
        open = !leaves;
    }
    out
}

/// The segment within `rect`, and whether it was cut at its end.
fn clip_segment(a: Point, b: Point, rect: Rect) -> Option<(Point, Point, bool)> {
    let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
    let (mut t0, mut t1) = (0.0_f64, 1.0_f64);
    for (p, q) in [
        (-dx, a[0] - rect[0]),
        (dx, rect[2] - a[0]),
        (-dy, a[1] - rect[1]),
        (dy, rect[3] - a[1]),
    ] {
        if p == 0.0 {
            if q < 0.0 {
                return None;
            }
        } else if p < 0.0 {
            t0 = t0.max(q / p);
        } else {
            t1 = t1.min(q / p);
        }
    }
    (t0 < t1).then(|| {
        let at = |t: f64| [a[0] + dx * t, a[1] + dy * t];
        (at(t0), if t1 < 1.0 { at(t1) } else { b }, t1 < 1.0)
    })
}

/// The runs of whole segments of `points` that come within `rect`.
pub(super) fn near(points: &[Point], rect: Rect) -> Vec<Vec<Point>> {
    let mut out: Vec<Vec<Point>> = Vec::new();
    let mut open = false;
    for p in points.windows(2) {
        if clip_segment(p[0], p[1], rect).is_none() {
            open = false;
            continue;
        }
        if !open {
            out.push(vec![p[0]]);
            open = true;
        }
        out.last_mut().expect("a run was opened").push(p[1]);
    }
    out
}

/// `points` lengthened `by` at both ends; a closed line as it is.
pub(super) fn extend(points: &[Point], by: f64) -> Vec<Point> {
    let mut out = points.to_vec();
    let last = out.len() - 1;
    if last == 0 || out[0] == out[last] {
        return out;
    }
    let beyond = |end: Point, before: Point| {
        let length = distance(end, before).max(f64::MIN_POSITIVE);
        [
            end[0] + (end[0] - before[0]) / length * by,
            end[1] + (end[1] - before[1]) / length * by,
        ]
    };
    (out[0], out[last]) = (
        beyond(points[0], points[1]),
        beyond(points[last], points[last - 1]),
    );
    out
}

/// A line widened to `width`, its ends square.
pub(super) fn ribbon(points: &[Point], width: f64) -> Shapes<Point> {
    let closed = points.len() > 3 && points[0] == points[points.len() - 1];
    let points = if closed {
        &points[..points.len() - 1]
    } else {
        points
    };
    let style = StrokeStyle::new(width).line_join(LineJoin::Miter(SHARPEST));
    points.stroke_as::<i32>(style, closed)
}

pub(super) fn union(shapes: &Shapes<Point>) -> Shapes<Point> {
    let none: Shapes<Point> = Vec::new();
    shapes.overlay_as::<i32>(&none, OverlayRule::Subject, FillRule::NonZero)
}

/// `shapes` within `rect` on the whole-unit grid of a tile whose corner is
/// `origin`: polygons that are valid as written. Outer rings are clockwise on
/// screen, as vector tiles have them; specks under `min_area` are left out.
pub(super) fn to_tile(
    shapes: &Shapes<Point>,
    rect: Rect,
    origin: Point,
    min_area: u64,
) -> Vec<Vec<Vec<[i32; 2]>>> {
    let frame: Shapes<Point> = vec![vec![vec![
        [rect[0], rect[1]],
        [rect[2], rect[1]],
        [rect[2], rect[3]],
        [rect[0], rect[3]],
    ]]];
    let clipped = shapes.overlay_as::<i32>(&frame, OverlayRule::Intersect, FillRule::NonZero);
    let rounded: Vec<Vec<IntPoint<i32>>> = clipped
        .iter()
        .flatten()
        .map(|ring| {
            ring.iter()
                .map(|p| {
                    IntPoint::new(
                        (p[0] - origin[0]).round() as i32,
                        (p[1] - origin[1]).round() as i32,
                    )
                })
                .collect()
        })
        .collect();
    if rounded.is_empty() {
        return Vec::new();
    }
    let options = IntOverlayOptions {
        min_output_area: min_area,
        ogc: true,
        ..Default::default()
    };
    let none: Vec<Vec<IntPoint<i32>>> = Vec::new();
    Overlay::<i32>::from_subj_and_clip_custom(&rounded, &none, options, Default::default())
        .overlay(OverlayRule::Subject, FillRule::NonZero)
        .into_iter()
        .map(|shape| {
            shape
                .into_iter()
                .map(|ring| ring.into_iter().map(|p| [p.x, p.y]).collect())
                .collect()
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offset_goes_right_of_the_direction() {
        // eastwards, y south: right is south
        assert_eq!(
            offset(&[[0.0, 0.0], [10.0, 0.0]], 2.0),
            [[0.0, 2.0], [10.0, 2.0]]
        );
        let bend = offset(&[[0.0, 0.0], [10.0, 0.0], [10.0, 10.0]], 2.0);
        assert!(distance(bend[1], [8.0, 2.0]) < 1e-9, "{bend:?}");
    }

    #[test]
    fn a_tight_bend_does_not_run_backwards() {
        let line = [
            [0.0, 0.0],
            [10.0, 0.0],
            [11.0, 1.0],
            [11.0, 2.0],
            [10.0, 3.0],
            [0.0, 3.0],
        ];
        let moved = offset(&line, 5.0);
        assert!(moved.len() < line.len());
        assert!(moved.iter().flatten().all(|v| v.is_finite()));
        let hairpin = offset(&[[0.0, 0.0], [10.0, 0.0], [0.0, 0.0]], 3.0);
        assert!(
            hairpin
                .iter()
                .flatten()
                .all(|v| v.is_finite() && v.abs() < 1e3)
        );
    }

    #[test]
    fn trimming_keeps_the_middle() {
        let line = [[0.0, 0.0], [10.0, 0.0], [20.0, 0.0]];
        assert_eq!(
            trim(&line, [4.0, 5.0]).unwrap(),
            [[4.0, 0.0], [10.0, 0.0], [15.0, 0.0]]
        );
        assert_eq!(
            trim(&line, [12.0, 0.0]).unwrap(),
            [[12.0, 0.0], [20.0, 0.0]]
        );
        assert!(trim(&line, [12.0, 9.0]).is_none());
    }

    #[test]
    fn clipping_splits_where_a_line_leaves_and_comes_back() {
        let line = [
            [-5.0, 5.0],
            [5.0, 5.0],
            [15.0, 5.0],
            [15.0, 8.0],
            [5.0, 8.0],
        ];
        let parts = clip(&line, [0.0, 0.0, 10.0, 10.0]);
        assert_eq!(
            parts,
            [
                vec![[0.0, 5.0], [5.0, 5.0], [10.0, 5.0]],
                vec![[10.0, 8.0], [5.0, 8.0]]
            ]
        );
    }

    #[test]
    fn a_crossing_becomes_one_valid_surface() {
        let a = ribbon(&[[-50.0, 0.0], [50.0, 0.0]], 8.0);
        let b = ribbon(&[[0.0, -50.0], [0.0, 50.0]], 8.0);
        let cross = union(&a.into_iter().chain(b).collect());
        assert_eq!(cross.len(), 1);
        let tile = to_tile(&cross, [-30.0, -30.0, 30.0, 30.0], [-30.0, -30.0], 1);
        assert_eq!((tile.len(), tile[0].len(), tile[0][0].len()), (1, 1, 12));
    }

    #[test]
    fn pieces_end_to_end_leave_no_crack() {
        // in world units far from the origin, where f64 has few digits to spare
        let at = |t: f64| [1.49e8 + t * 0.83, 9.1e7 - t * 0.557];
        let shapes: Shapes<Point> = (0..40)
            .flat_map(|k| {
                let k = f64::from(k);
                ribbon(&extend(&[at(k * 61.3), at((k + 1.0) * 61.3)], 2.5), 73.1)
            })
            .collect();
        let (low, high) = (at(0.0), at(40.0 * 61.3));
        let rect = [
            low[0] - 100.0,
            high[1] - 100.0,
            high[0] + 100.0,
            low[1] + 100.0,
        ];
        let tile = to_tile(&union(&shapes), rect, [rect[0], rect[1]], 1);
        assert_eq!((tile.len(), tile[0].len()), (1, 1));
        let ring = [[0.0, 0.0], [1.0, 0.0], [1.0, 1.0], [0.0, 0.0]];
        assert_eq!(extend(&ring, 2.0), ring);
    }
}
