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
    let moved = displaced(points, |_| by, [None; 2]);
    let kept = forwards(points, &moved);
    kept.into_iter().map(|i| moved[i]).collect()
}

/// `points` each moved its own way right. Past an end the line goes on to
/// `beyond`, if to anywhere: the end is moved as any other bend.
pub(super) fn displaced(
    points: &[Point],
    by: impl Fn(usize) -> f64,
    beyond: [Option<Point>; 2],
) -> Vec<Point> {
    let normal = |a: Point, b: Point| {
        let length = distance(a, b).max(f64::MIN_POSITIVE);
        [-(b[1] - a[1]) / length, (b[0] - a[0]) / length]
    };
    let normals: Vec<Point> = points.windows(2).map(|p| normal(p[0], p[1])).collect();
    let closed = points.len() > 3 && points[0] == points[points.len() - 1];
    let corner = |at: Point, a: Point, b: Point, by: f64| {
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
    (0..points.len())
        .map(|i| {
            let before = if i > 0 {
                normals[i - 1]
            } else if let Some(p) = beyond[0] {
                normal(p, points[0])
            } else if closed {
                normals[last - 1]
            } else {
                normals[0]
            };
            let after = if i < last {
                normals[i]
            } else if let Some(p) = beyond[1] {
                normal(points[last], p)
            } else if closed {
                normals[0]
            } else {
                normals[last - 1]
            };
            corner(points[i], before, after, by(i))
        })
        .collect()
}

/// Which of the `moved` points do not run backwards.
pub(super) fn forwards(points: &[Point], moved: &[Point]) -> Vec<usize> {
    let mut out = vec![0];
    for i in 1..moved.len() {
        let previous = moved[out[out.len() - 1]];
        let along = [
            points[i][0] - points[i - 1][0],
            points[i][1] - points[i - 1][1],
        ];
        let step = [moved[i][0] - previous[0], moved[i][1] - previous[1]];
        if along[0] * step[0] + along[1] * step[1] > 0.0 {
            out.push(i);
        }
    }
    out
}

/// Metres along `points` of each, of `scale` units, the first at `from`.
pub(super) fn lengths(points: &[Point], scale: f64, from: f64) -> Vec<f64> {
    let mut at = from;
    let mut out = vec![at];
    for p in points.windows(2) {
        at += distance(p[0], p[1]) / scale;
        out.push(at);
    }
    out
}

/// `points` at the `lengths` given with further ones put in at those of
/// `extra` (in order), with the length of each.
pub(super) fn stations(points: &[Point], lengths: &[f64], extra: &[f64]) -> (Vec<Point>, Vec<f64>) {
    /// Metres from another one a point is not put in.
    const APART: f64 = 1e-3;
    let mut out = (vec![points[0]], vec![lengths[0]]);
    let mut extra = extra.iter().copied().peekable();
    for k in 1..points.len() {
        let (a, b) = (lengths[k - 1], lengths[k]);
        while let Some(s) = extra.next_if(|&s| s < b - APART) {
            if s > out.1[out.1.len() - 1] + APART {
                let t = (s - a) / (b - a);
                out.0.push([
                    points[k - 1][0] + (points[k][0] - points[k - 1][0]) * t,
                    points[k - 1][1] + (points[k][1] - points[k - 1][1]) * t,
                ]);
                out.1.push(s);
            }
        }
        out.0.push(points[k]);
        out.1.push(b);
    }
    out
}

/// The ground between two lines of as many points, wound as [`ribbon`]'s.
pub(super) fn between(left: &[Point], right: &[Point]) -> Shapes<Point> {
    let twice = |ring: &[Point]| -> f64 {
        let o = ring[0];
        (1..ring.len() - 1)
            .map(|i| {
                let (a, b) = (ring[i], ring[i + 1]);
                (a[0] - o[0]) * (b[1] - o[1]) - (b[0] - o[0]) * (a[1] - o[1])
            })
            .sum()
    };
    let mut out: Shapes<Point> = Vec::new();
    let mut add = |mut ring: Vec<Point>| {
        let area = twice(&ring);
        if area < 0.0 {
            ring.reverse();
        }
        if area != 0.0 {
            out.push(vec![ring]);
        }
    };
    for k in 1..left.len().min(right.len()) {
        let [a, b, c, d] = [left[k - 1], left[k], right[k], right[k - 1]];
        let turns = [[a, b, c], [b, c, d], [c, d, a], [d, a, b]].map(|t| twice(&t));
        if turns.iter().all(|t| *t > 0.0) || turns.iter().all(|t| *t < 0.0) {
            add(vec![a, b, c, d]);
        } else {
            // folded inside a bend, or with a side of no length
            add(vec![a, b, c]);
            add(vec![a, c, d]);
        }
    }
    out
}

/// The parts of a dashed line within `rect` that begin where a dash does:
/// `phase` metres of the period `every` are gone at its start, and its points
/// are at `lengths` metres. A dash the line before left off is a part of its
/// own.
pub(super) fn clip_dashed(
    points: &[Point],
    lengths: &[f64],
    rect: Rect,
    phase: f64,
    [dash, every]: [f64; 2],
) -> Vec<Vec<Point>> {
    /// Metres of paint too short to draw.
    const SPECK: f64 = 0.2;
    let last = lengths.len() - 1;
    let (start, end) = (lengths[0], lengths[last]);
    let at = |s: f64| -> Point {
        let k = lengths.partition_point(|&l| l < s).clamp(1, last);
        let span = (lengths[k] - lengths[k - 1]).max(f64::MIN_POSITIVE);
        let t = ((s - lengths[k - 1]) / span).clamp(0.0, 1.0);
        let (a, b) = (points[k - 1], points[k]);
        [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t]
    };
    let part = |from: f64, to: f64| -> Vec<Point> {
        let mut out = vec![at(from)];
        let within = (0..=last).filter(|&k| lengths[k] > from && lengths[k] < to);
        out.extend(within.map(|k| points[k]));
        out.push(at(to));
        out
    };
    let mut stretches: Vec<[f64; 2]> = Vec::new();
    for k in 1..=last {
        let (a, b) = (points[k - 1], points[k]);
        let Some((p, q, _)) = clip_segment(a, b, rect) else {
            continue;
        };
        let units = distance(a, b).max(f64::MIN_POSITIVE);
        let of = |p: Point| {
            lengths[k - 1] + (lengths[k] - lengths[k - 1]) * (distance(a, p) / units).min(1.0)
        };
        let (from, to) = (of(p), of(q));
        match stretches.last_mut() {
            Some(s) if from - s[1] < 1e-6 => s[1] = to,
            _ => stretches.push([from, to]),
        }
    }
    // where the first dash to begin in it begins, from the start
    let first = if phase < 1e-6 { 0.0 } else { every - phase };
    let mut out = Vec::new();
    let mut done = f64::NEG_INFINITY;
    for [from, to] in stretches {
        let mut begins = start + first + ((from - start - first) / every).floor() * every;
        if begins < start - 1e-6 {
            let left = (start + dash - phase).min(end);
            if from <= left && left - start > SPECK {
                out.push(part(start, left));
            }
            begins += every;
        }
        let begins = begins.max(done);
        if to - begins > SPECK {
            out.push(part(begins.max(start), to));
            done = begins + ((to - begins) / every).ceil() * every;
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

/// From and to where of the segment, 0 to 1, it lies within `rect`.
pub(super) fn span_within(a: Point, b: Point, rect: Rect) -> Option<[f64; 2]> {
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
    (t0 < t1).then_some([t0, t1])
}

/// The segment within `rect`, and whether it was cut at its end.
fn clip_segment(a: Point, b: Point, rect: Rect) -> Option<(Point, Point, bool)> {
    let [t0, t1] = span_within(a, b, rect)?;
    let at = |t: f64| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
    Some((at(t0), if t1 < 1.0 { at(t1) } else { b }, t1 < 1.0))
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
    extend_ends(points, [by; 2])
}

/// `points` lengthened at its start and its end; a closed line as it is.
pub(super) fn extend_ends(points: &[Point], by: [f64; 2]) -> Vec<Point> {
    let mut out = points.to_vec();
    let last = out.len() - 1;
    if last == 0 || out[0] == out[last] {
        return out;
    }
    let beyond = |end: Point, before: Point, by: f64| {
        let length = distance(end, before).max(f64::MIN_POSITIVE);
        [
            end[0] + (end[0] - before[0]) / length * by,
            end[1] + (end[1] - before[1]) / length * by,
        ]
    };
    (out[0], out[last]) = (
        beyond(points[0], points[1], by[0]),
        beyond(points[last], points[last - 1], by[1]),
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

/// Fills what is open within `shapes`, of `scale` units a metre, of under `open`
/// square metres, or of under `narrow.0` and `narrow.1` metres across on average.
pub(super) fn close(shapes: &mut Shapes<Point>, scale: f64, open: f64, narrow: (f64, f64)) {
    let remains = |ring: &Vec<Point>| {
        let (mut twice, mut round) = (0.0, 0.0);
        let o = ring[0];
        for i in 0..ring.len() {
            let (a, b) = (ring[i], ring[(i + 1) % ring.len()]);
            twice += (a[0] - o[0]) * (b[1] - o[1]) - (b[0] - o[0]) * (a[1] - o[1]);
            round += distance(a, b) / scale;
        }
        let area = twice.abs() / 2.0 / (scale * scale);
        area >= open && (area >= narrow.0 || 2.0 * area / round >= narrow.1)
    };
    for shape in shapes {
        let mut first = true;
        shape.retain(|ring| std::mem::take(&mut first) || remains(ring));
    }
}

/// What of `shapes` none of `taken` covers.
pub(super) fn without(shapes: &Shapes<Point>, taken: &Shapes<Point>) -> Shapes<Point> {
    shapes.overlay_as::<i32>(taken, OverlayRule::Difference, FillRule::NonZero)
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
    fn a_path_across_a_road_is_what_lies_either_side_of_it() {
        let path = ribbon(&[[0.0, -50.0], [0.0, 50.0]], 2.0);
        let road = ribbon(&[[-50.0, 0.0], [50.0, 0.0]], 8.0);
        let left = without(&path, &road);
        assert_eq!(left.len(), 2);
        assert!(left.iter().flatten().flatten().all(|p| p[1].abs() > 3.99));
        // along one, nothing
        assert!(without(&ribbon(&[[-40.0, 1.0], [40.0, 1.0]], 2.0), &road).is_empty());
        assert_eq!(without(&path, &Vec::new()).len(), 1);
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
