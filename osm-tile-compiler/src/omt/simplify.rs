//! Closed-ring simplification for the pinned OMT renderer.
//!
//! Adapted from Planetiler's JTS-derived DouglasPeuckerSimplifier, embedded
//! in Planetiler 0.10.2 (core 0e5588c4). Copyright (c) 2016 Vivid Solutions.
//! SPDX-License-Identifier: EDL-1.0
//! See LICENSE.jts-edl-1.0.txt. The iterative traversal preserves the original
//! sequence, first-farthest ties and forced-point allocation of that algorithm.
use crate::coordinate_groups::{WorldPoint, WorldRingGroup};

pub(super) fn polygons(groups: Vec<WorldRingGroup>, tolerance: f64) -> Vec<WorldRingGroup> {
    groups
        .into_iter()
        .filter_map(|group| {
            Some(WorldRingGroup {
                exterior: ring(group.exterior, tolerance)?,
                holes: group
                    .holes
                    .into_iter()
                    .filter_map(|hole| ring(hole, tolerance))
                    .collect(),
            })
        })
        .collect()
}

fn ring(mut points: Vec<WorldPoint>, tolerance: f64) -> Option<Vec<WorldPoint>> {
    if points.len() < 4 {
        return None;
    }
    if points.len() == 4 || tolerance < 0. {
        return Some(points);
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    let mut pending = vec![(0, points.len() - 1, 2_i32)];
    let tolerance_squared = tolerance * tolerance;
    while let Some((first, last, forced)) = pending.pop() {
        let a = points[first];
        let b = points[last];
        let dx = b.x - a.x;
        let dy = b.y - a.y;
        let mut maximum = if forced > 0 { -1. } else { tolerance_squared };
        let mut selected = None;
        for (index, p) in points.iter().enumerate().take(last).skip(first + 1) {
            let (mut x, mut y) = (a.x, a.y);
            if dx != 0. || dy != 0. {
                let t = ((p.x - x) * dx + (p.y - y) * dy) / (dx * dx + dy * dy);
                if t > 1. {
                    x = b.x;
                    y = b.y;
                } else if t > 0. {
                    x += dx * t;
                    y += dy * t;
                }
            }
            let (px, py) = (p.x - x, p.y - y);
            let distance = px * px + py * py;
            if distance > maximum {
                maximum = distance;
                selected = Some(index);
            }
        }
        if let Some(index) = selected {
            keep[index] = true;
            if index - first > 1 {
                pending.push((first, index, forced - 1));
            }
            if last - index > 1 {
                pending.push((index, last, forced - 2));
            }
        }
    }
    let mut index = 0;
    points.retain(|_| {
        let retained = keep[index];
        index += 1;
        retained
    });
    // The reference GeometryTransformer discards a ring that is no longer
    // a LinearRing. Do not restore the original unsimplified polygon here.
    (points.len() >= 4).then_some(points)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn coarse_tolerance_keeps_forced_vertices_without_restoring_original() {
        let p = WorldPoint::new;
        let square = vec![p(0., 0.), p(1., 0.), p(1., 1.), p(0., 1.), p(0., 0.)];
        assert_eq!(
            ring(square, 100.).unwrap(),
            vec![p(0., 0.), p(1., 0.), p(1., 1.), p(0., 0.)]
        );
    }

    #[test]
    fn zero_tolerance_removes_unforced_collinear_vertices() {
        let p = WorldPoint::new;
        let input = vec![
            p(0., 0.),
            p(1., 0.),
            p(2., 0.),
            p(2., 2.),
            p(0., 2.),
            p(0., 0.),
        ];
        assert_eq!(
            ring(input, 0.).unwrap(),
            vec![p(0., 0.), p(2., 0.), p(2., 2.), p(0., 2.), p(0., 0.)]
        );
    }
}
