//! Vertex hot-pixel noding for valid single rings before precision repair.
//! Hot-pixel intersection adapted from JTS HotPixel (Planetiler 0.10.2 bundle).
//! Copyright (c) 2016 Vivid Solutions. SPDX-License-Identifier: EDL-1.0
//! See LICENSE.jts-edl-1.0.txt. This is not a general intersection noder.
use crate::{coordinate_groups::WorldPoint, tile_geometry::TilePoint};
use geo::{
    Coord,
    algorithm::kernels::{Kernel, Orientation, RobustKernel},
};
use std::collections::BTreeMap;

fn intersects(a: WorldPoint, b: WorldPoint, pixel: TilePoint) -> bool {
    let (p, q) = if a.x > b.x { (b, a) } else { (a, b) };
    let (minx, maxx) = (pixel.x as f64 - 0.5, pixel.x as f64 + 0.5);
    let (miny, maxy) = (pixel.y as f64 - 0.5, pixel.y as f64 + 0.5);
    if p.x >= maxx || q.x < minx || p.y.min(q.y) >= maxy || p.y.max(q.y) < miny {
        return false;
    }
    if p.x == q.x || p.y == q.y {
        return true;
    }
    let orient = |x, y| {
        RobustKernel::orient2d(
            Coord { x: p.x, y: p.y },
            Coord { x: q.x, y: q.y },
            Coord { x, y },
        )
    };
    let ul = orient(minx, maxy);
    if ul == Orientation::Collinear {
        return p.y >= q.y;
    }
    let ur = orient(maxx, maxy);
    if ur == Orientation::Collinear {
        return p.y <= q.y;
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
        return p.y >= q.y;
    }
    ll != lr || lr != ur
}

/// Input must be a valid float ring in MVT units. All potential crossings are
/// therefore vertex hot pixels; proper float intersections require a separate
/// noder and must be handled before this bounded operation.
pub(super) fn node_valid_rings(rings: &[Vec<WorldPoint>]) -> Vec<Vec<TilePoint>> {
    let snap =
        |p: WorldPoint| TilePoint::new((p.x + 0.5).floor() as i32, (p.y + 0.5).floor() as i32);
    let mut pixels = BTreeMap::<i32, Vec<TilePoint>>::new();
    for &point in rings.iter().flatten() {
        let p = snap(point);
        pixels.entry(p.x).or_default().push(p);
    }
    for column in pixels.values_mut() {
        column.sort();
        column.dedup();
    }
    rings
        .iter()
        .map(|points| {
            let mut result = Vec::new();
            for edge in points.windows(2) {
                let (a, b) = (edge[0], edge[1]);
                let (start, end) = (snap(a), snap(b));
                if start == end {
                    continue;
                }
                let mut nodes = vec![start, end];
                let lower = (a.x.min(b.x) - 0.5).ceil() as i32;
                let upper = (a.x.max(b.x) + 0.5).floor() as i32;
                for (_, column) in pixels.range(lower..=upper) {
                    for &pixel in column {
                        if pixel != start && pixel != end && intersects(a, b, pixel) {
                            nodes.push(pixel);
                        }
                    }
                }
                let ascending_x = end.x >= start.x;
                let ascending_y = end.y >= start.y;
                let major_x =
                    (end.x as i64 - start.x as i64).abs() >= (end.y as i64 - start.y as i64).abs();
                nodes.sort_by(|a, b| {
                    let x = if ascending_x {
                        a.x.cmp(&b.x)
                    } else {
                        b.x.cmp(&a.x)
                    };
                    let y = if ascending_y {
                        a.y.cmp(&b.y)
                    } else {
                        b.y.cmp(&a.y)
                    };
                    if major_x { x.then(y) } else { y.then(x) }
                });
                nodes.dedup();
                result.extend(nodes.into_iter().take_while(|p| *p != end));
            }
            result.dedup();
            if let Some(first) = result.first().copied()
                && result.last() != Some(&first)
            {
                result.push(first);
            }
            result
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn hot_pixel_uses_half_open_top_and_right_boundaries() {
        let pixel = TilePoint::new(0, 0);
        let p = WorldPoint::new;
        assert!(intersects(p(-1., -0.5), p(1., -0.5), pixel));
        assert!(!intersects(p(-1., 0.5), p(1., 0.5), pixel));
        assert!(intersects(p(-0.5, -1.), p(-0.5, 1.), pixel));
        assert!(!intersects(p(0.5, -1.), p(0.5, 1.), pixel));
        assert!(!intersects(p(-1., 0.), p(0., 1.), pixel));
        assert!(intersects(p(-1., 0.), p(0., -1.), pixel));
    }
}
