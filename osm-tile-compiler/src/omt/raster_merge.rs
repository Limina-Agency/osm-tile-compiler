//! Experiment: polygon dissolve as a morphological closing on a bit raster.
//!
//! `FeatureMerge.bufferUnionUnbuffer` (buffer every polygon out by `d`, union,
//! buffer back in by `d`) is, for round joins, the morphological closing of the
//! union with a disk of radius `d`. On a raster of cell size `c` that is
//! a dilation and an erosion of a bitset, done with word-wide shifts and ORs.
//! The closed raster is traced back into rectilinear rings along cell edges.
//!
//! The output is not vertex-identical to JTS: vertices sit on the cell grid and
//! the input polygons are resampled at cell resolution.

use geo_types::{Coord, LineString, MultiPolygon, Polygon};

/// A bit raster, one `u64` word per 64 cells, row-major, row 0 at `y0`.
struct Bits {
    w: usize,
    h: usize,
    words: usize,
    data: Vec<u64>,
}

impl Bits {
    fn new(w: usize, h: usize) -> Self {
        let words = w.div_ceil(64);
        Self {
            w,
            h,
            words,
            data: vec![0; words * h],
        }
    }
    #[inline]
    fn get(&self, x: isize, y: isize) -> bool {
        if x < 0 || y < 0 || x as usize >= self.w || y as usize >= self.h {
            return false;
        }
        let (x, y) = (x as usize, y as usize);
        self.data[y * self.words + x / 64] >> (x % 64) & 1 == 1
    }
    fn row(&self, y: usize) -> &[u64] {
        &self.data[y * self.words..(y + 1) * self.words]
    }
    /// Sets cells `from..to` of row `y`.
    fn fill(&mut self, y: usize, from: usize, to: usize) {
        if from >= to {
            return;
        }
        let row = &mut self.data[y * self.words..(y + 1) * self.words];
        let (fw, tw) = (from / 64, (to - 1) / 64);
        for (k, word) in row.iter_mut().enumerate().take(tw + 1).skip(fw) {
            let lo = if k == fw { from % 64 } else { 0 };
            let hi = if k == tw { (to - 1) % 64 + 1 } else { 64 };
            let mask = if hi - lo == 64 {
                u64::MAX
            } else {
                ((1u64 << (hi - lo)) - 1) << lo
            };
            *word |= mask;
        }
    }
    fn invert(&mut self) {
        let tail = self.w % 64;
        for y in 0..self.h {
            let row = &mut self.data[y * self.words..(y + 1) * self.words];
            for word in row.iter_mut() {
                *word = !*word;
            }
            if tail != 0 {
                row[self.words - 1] &= (1u64 << tail) - 1;
            }
        }
    }
}

/// `out |= row << k` (towards higher x) and `out |= row >> k`, across words.
#[inline]
fn or_shifted(out: &mut [u64], row: &[u64], k: usize) {
    let n = row.len();
    let (ws, bs) = (k / 64, k % 64);
    // Towards higher x: bit i goes to i + k.
    for i in (ws..n).rev() {
        let mut v = row[i - ws] << bs;
        if bs != 0 && i > ws {
            v |= row[i - ws - 1] >> (64 - bs);
        }
        out[i] |= v;
    }
    // Towards lower x: bit i goes to i - k.
    for i in 0..n.saturating_sub(ws) {
        let mut v = row[i + ws] >> bs;
        if bs != 0 && i + ws + 1 < n {
            v |= row[i + ws + 1] << (64 - bs);
        }
        out[i] |= v;
    }
}

/// Dilation with the digital disk `dx² + dy² <= r²` (cell centers).
fn dilate(src: &Bits, r: f64) -> Bits {
    let rr = r.floor() as usize;
    let widths: Vec<usize> = (0..=rr)
        .map(|dy| ((r * r - (dy * dy) as f64).max(0.).sqrt()).floor() as usize)
        .collect();
    let mut out = Bits::new(src.w, src.h);
    let words = src.words;
    // Horizontal dilations of one source row for every width 0..=rr.
    let mut horiz = vec![vec![0u64; words]; rr + 1];
    for y in 0..src.h {
        let row = src.row(y);
        if row.iter().all(|w| *w == 0) {
            continue;
        }
        horiz[0].copy_from_slice(row);
        for k in 1..=rr {
            let (done, rest) = horiz.split_at_mut(k);
            rest[0].copy_from_slice(&done[k - 1]);
            or_shifted(&mut rest[0], row, k);
        }
        for (dy, &w) in widths.iter().enumerate() {
            let h = &horiz[w];
            for ty in [y as isize - dy as isize, y as isize + dy as isize] {
                if ty < 0 || ty as usize >= src.h {
                    continue;
                }
                let t = &mut out.data[ty as usize * words..(ty as usize + 1) * words];
                for (a, b) in t.iter_mut().zip(h) {
                    *a |= *b;
                }
                if dy == 0 {
                    break;
                }
            }
        }
    }
    out
}

/// Scanline fill of polygons, even-odd within each polygon, cells sampled at
/// their centers. Edges are bucketed by their first row, so each row only
/// visits the edges that cross it.
fn rasterize(polys: &[&Polygon<f64>], bits: &mut Bits, x0: f64, y0: f64, c: f64) {
    struct Edge {
        last: usize,
        x: f64,
        dx: f64,
    }
    let mut xs: Vec<f64> = Vec::new();
    let mut starts: Vec<Vec<Edge>> = Vec::new();
    let mut active: Vec<Edge> = Vec::new();
    for poly in polys {
        starts.iter_mut().for_each(Vec::clear);
        starts.resize_with(bits.h, Vec::new);
        let (mut jlo, mut jhi) = (usize::MAX, 0usize);
        for ring in std::iter::once(poly.exterior()).chain(poly.interiors()) {
            for seg in ring.0.windows(2) {
                let (mut a, mut b) = (seg[0], seg[1]);
                if a.y == b.y {
                    continue;
                }
                if a.y > b.y {
                    std::mem::swap(&mut a, &mut b);
                }
                // Rows whose center y lies in [a.y, b.y).
                let first = ((a.y - y0) / c - 0.5).ceil().max(0.) as usize;
                let last = (((b.y - y0) / c - 0.5).ceil() as isize - 1).min(bits.h as isize - 1);
                if last < first as isize {
                    continue;
                }
                let yc = y0 + (first as f64 + 0.5) * c;
                let slope = (b.x - a.x) / (b.y - a.y);
                starts[first].push(Edge {
                    last: last as usize,
                    x: a.x + (yc - a.y) * slope,
                    dx: slope * c,
                });
                jlo = jlo.min(first);
                jhi = jhi.max(last as usize);
            }
        }
        if jlo == usize::MAX {
            continue;
        }
        active.clear();
        for j in jlo..=jhi {
            active.append(&mut starts[j]);
            xs.clear();
            xs.extend(active.iter().map(|e| e.x));
            xs.sort_unstable_by(f64::total_cmp);
            for pair in xs.chunks_exact(2) {
                let from = ((pair[0] - x0) / c - 0.5).ceil().max(0.) as usize;
                let to = (((pair[1] - x0) / c - 0.5).floor() + 1.).max(0.) as usize;
                bits.fill(j, from, to.min(bits.w));
            }
            active.retain_mut(|e| {
                e.x += e.dx;
                e.last > j
            });
        }
    }
}

/// Traces the boundaries of the set cells into rings along cell edges, with
/// set cells 4-connected. Returns polygons (shell + holes) in grid units.
fn trace(bits: &Bits) -> Vec<(Vec<(i32, i32)>, Vec<Vec<(i32, i32)>>)> {
    let (w, h) = (bits.w as isize, bits.h as isize);
    let cell = |x: isize, y: isize| bits.get(x, y);
    // Horizontal edge (x, y): segment x..x+1 on vertex row y, 0 <= y <= h.
    let mut visited = vec![false; (bits.w + 1) * (bits.h + 1)];
    let hidx = |x: isize, y: isize| (y * (w + 1) + x) as usize;
    let mut shells: Vec<(Vec<(i32, i32)>, [i32; 4], i64)> = Vec::new();
    let mut holes: Vec<(Vec<(i32, i32)>, (f64, f64))> = Vec::new();
    for y in 0..=h {
        for x in 0..w {
            let above = cell(x, y - 1);
            let below = cell(x, y);
            if above == below || visited[hidx(x, y)] {
                continue;
            }
            // Foreground on the left in screen terms: moving right has the
            // set cell above, moving left has it below.
            let (fg, start, dir0) = if above {
                ((x, y - 1), (x, y), (1isize, 0isize))
            } else {
                ((x, y), (x + 1, y), (-1, 0))
            };
            let mut ring = Vec::new();
            let (mut px, mut py) = start;
            let mut dir = dir0;
            let mut area2: i64 = 0;
            loop {
                if dir.1 == 0 {
                    let ex = if dir.0 == 1 { px } else { px - 1 };
                    visited[hidx(ex, py)] = true;
                }
                let (nx, ny) = (px + dir.0, py + dir.1);
                area2 += (px * ny - nx * py) as i64;
                px = nx;
                py = ny;
                // Cells around vertex (px, py).
                let a = cell(px - 1, py - 1);
                let b = cell(px, py - 1);
                let c = cell(px - 1, py);
                let d = cell(px, py);
                let right = b && !d;
                let down = d && !c;
                let left = c && !a;
                let up = a && !b;
                let count = right as u8 + down as u8 + left as u8 + up as u8;
                let next = if count == 2 {
                    // Saddle: turn towards the cell being followed.
                    (dir.1, -dir.0)
                } else if right {
                    (1, 0)
                } else if down {
                    (0, 1)
                } else if left {
                    (-1, 0)
                } else {
                    (0, -1)
                };
                if next != dir {
                    ring.push((px as i32, py as i32));
                }
                dir = next;
                if (px, py) == start && dir == dir0 {
                    break;
                }
            }
            if ring.first() != ring.last() {
                ring.push(ring[0]);
            }
            // Shells come out with negative area, holes positive.
            if area2 < 0 {
                let mut bb = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
                for &(x, y) in &ring {
                    bb = [bb[0].min(x), bb[1].min(y), bb[2].max(x), bb[3].max(y)];
                }
                shells.push((ring, bb, -area2));
            } else {
                // The center of the set cell along the hole's first edge.
                holes.push((ring, (fg.0 as f64 + 0.5, fg.1 as f64 + 0.5)));
            }
        }
    }
    // A hole belongs to the smallest shell around the cell beside it.
    let mut out: Vec<_> = shells
        .iter()
        .map(|(ring, ..)| (ring.clone(), Vec::new()))
        .collect();
    for (ring, (px, py)) in holes {
        let owner = shells
            .iter()
            .enumerate()
            .filter(|(_, (r, bb, _))| {
                px > f64::from(bb[0])
                    && px < f64::from(bb[2])
                    && py > f64::from(bb[1])
                    && py < f64::from(bb[3])
                    && contains(r, px, py)
            })
            .min_by_key(|(_, (.., area))| *area)
            .map(|(i, _)| i);
        if let Some(i) = owner {
            out[i].1.push(ring);
        }
    }
    out
}

/// Even-odd point in ring, for a point off the ring's grid lines.
fn contains(ring: &[(i32, i32)], px: f64, py: f64) -> bool {
    let mut inside = false;
    for w in ring.windows(2) {
        let (ax, ay) = (f64::from(w[0].0), f64::from(w[0].1));
        let (bx, by) = (f64::from(w[1].0), f64::from(w[1].1));
        if (ay > py) != (by > py) && px < ax + (py - ay) * (bx - ax) / (by - ay) {
            inside = !inside;
        }
    }
    inside
}

/// Closing of the union of `cluster` with a disk of radius `distance`, on
/// a raster of cell size `cell`; rings in input units.
pub(super) fn raster_buffer_union_unbuffer(
    cluster: &[MultiPolygon<f64>],
    distance: f64,
    cell: f64,
) -> MultiPolygon<f64> {
    let polys: Vec<&Polygon<f64>> = cluster.iter().flat_map(|m| m.0.iter()).collect();
    let mut min = Coord {
        x: f64::INFINITY,
        y: f64::INFINITY,
    };
    let mut max = Coord {
        x: f64::NEG_INFINITY,
        y: f64::NEG_INFINITY,
    };
    for p in &polys {
        for c in &p.exterior().0 {
            min.x = min.x.min(c.x);
            min.y = min.y.min(c.y);
            max.x = max.x.max(c.x);
            max.y = max.y.max(c.y);
        }
    }
    if !min.x.is_finite() {
        return MultiPolygon(vec![]);
    }
    let margin = distance + 2. * cell;
    let x0 = ((min.x - margin) / cell).floor() * cell;
    let y0 = ((min.y - margin) / cell).floor() * cell;
    let w = ((max.x + margin - x0) / cell).ceil() as usize + 1;
    let h = ((max.y + margin - y0) / cell).ceil() as usize + 1;
    let mut bits = Bits::new(w, h);
    rasterize(&polys, &mut bits, x0, y0, cell);
    let r = distance / cell;
    let mut closed = dilate(&bits, r);
    closed.invert();
    let mut closed = dilate(&closed, r);
    closed.invert();
    let to_line = |ring: Vec<(i32, i32)>| {
        LineString(
            ring.into_iter()
                .map(|(x, y)| Coord {
                    x: x0 + f64::from(x) * cell,
                    y: y0 + f64::from(y) * cell,
                })
                .collect(),
        )
    };
    MultiPolygon(
        trace(&closed)
            .into_iter()
            .map(|(s, hs)| Polygon::new(to_line(s), hs.into_iter().map(to_line).collect()))
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use geo::Area;

    fn square(x: f64, y: f64, s: f64) -> MultiPolygon<f64> {
        MultiPolygon(vec![Polygon::new(
            LineString::from(vec![(x, y), (x + s, y), (x + s, y + s), (x, y + s), (x, y)]),
            vec![],
        )])
    }

    #[test]
    fn closes_gap_between_two_squares() {
        let merged =
            raster_buffer_union_unbuffer(&[square(0., 0., 100.), square(110., 0., 100.)], 8., 1.);
        // The gap fills except for two shallow notches where a disk of
        // radius 8 still dips into its ends (as with a vector closing).
        assert_eq!(merged.0.len(), 1);
        let area = merged.unsigned_area();
        assert!(area <= 210. * 100. && area > 210. * 100. - 40., "{area}");
    }

    #[test]
    fn keeps_far_squares_apart_and_holes() {
        let merged =
            raster_buffer_union_unbuffer(&[square(0., 0., 100.), square(140., 0., 100.)], 8., 1.);
        assert_eq!(merged.0.len(), 2);
        assert!((merged.unsigned_area() - 2. * 100. * 100.).abs() < 1.);
        let ring = MultiPolygon(vec![Polygon::new(
            LineString::from(vec![
                (0., 0.),
                (100., 0.),
                (100., 100.),
                (0., 100.),
                (0., 0.),
            ]),
            vec![LineString::from(vec![
                (30., 30.),
                (70., 30.),
                (70., 70.),
                (30., 70.),
                (30., 30.),
            ])],
        )]);
        let merged = raster_buffer_union_unbuffer(&[ring], 8., 1.);
        assert_eq!(merged.0.len(), 1);
        assert_eq!(merged.0[0].interiors().len(), 1);
        // The hole's four corners fill with quarter disks of radius 8:
        // 4 * (64 - 16 pi) = 55 units squared, coarser on a digital disk.
        let area = merged.unsigned_area();
        assert!((area - (10000. - 1600. + 55.)).abs() < 50., "{area}");
    }
}
