//! OMT line-stroke pairing at junctions.
//!
//! The pinned Planetiler `LoopLineMerger.strokeMerge` pairs incident edges in
//! descending angle order after simplification and duplicate-edge removal.
//! Graph stages are adapted from Planetiler 0.10.2 `LoopLineMerger.java`
//! (Apache-2.0); line DP follows its adapted JTS simplifier (EDL-1.0).
//! See `LICENSE.planetiler` and `LICENSE.jts-edl-1.0.txt` in this directory.

use crate::tile_geometry::TilePoint;
use rustc_hash::{FxHashMap, FxHashSet};
use std::cmp::{Ordering, Reverse};
#[cfg(test)]
use std::collections::BTreeMap;
use std::collections::{BTreeSet, BinaryHeap};

#[derive(Clone, Copy)]
struct QueueItem {
    priority: f64,
    distance: f64,
    sequence: usize,
    index: usize,
    forward: bool,
}

impl PartialEq for QueueItem {
    fn eq(&self, other: &Self) -> bool {
        self.priority.total_cmp(&other.priority) == Ordering::Equal
            && self.sequence == other.sequence
            && self.index == other.index
            && self.forward == other.forward
    }
}
impl Eq for QueueItem {}
impl PartialOrd for QueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for QueueItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.priority
            .total_cmp(&other.priority)
            .then_with(|| self.sequence.cmp(&other.sequence))
            .then_with(|| self.index.cmp(&other.index))
            .then_with(|| self.forward.cmp(&other.forward))
    }
}

#[cfg(test)]
#[derive(Clone, Copy)]
struct Incident {
    index: usize,
    at_start: bool,
}

fn angle(path: &[TilePoint], at_start: bool) -> f64 {
    let (origin, next) = if at_start {
        (path[0], path[1])
    } else {
        (path[path.len() - 1], path[path.len() - 2])
    };
    (f64::from(next.y) - f64::from(origin.y)).atan2(f64::from(next.x) - f64::from(origin.x))
}

fn separation(a: f64, b: f64) -> f64 {
    let mut delta = a - b;
    if delta > std::f64::consts::PI {
        delta -= 2. * std::f64::consts::PI;
    } else if delta < -std::f64::consts::PI {
        delta += 2. * std::f64::consts::PI;
    }
    delta.abs()
}

fn node_and_deduplicate(paths: &mut Vec<Vec<TilePoint>>) {
    for path in paths.iter_mut() {
        path.dedup();
    }
    paths.retain(|path| path.len() >= 2);
    // Only looked up, never iterated, so the map's order cannot matter.
    let mut counts = FxHashMap::<TilePoint, usize>::default();
    for point in paths.iter().flatten() {
        *counts.entry(*point).or_default() += 1;
    }
    let mut segments = Vec::new();
    for path in paths.drain(..) {
        let mut start = 0;
        for i in 1..path.len() - 1 {
            if counts[&path[i]] > 1 {
                segments.push(path[start..=i].to_vec());
                start = i;
            }
        }
        segments.push(path[start..].to_vec());
    }
    // Membership only: segments keep their input order.
    let mut unique = FxHashSet::default();
    for segment in segments {
        if segment.len() < 2 {
            continue;
        }
        // Java Node.addEdge compares directed coordinates at the start node.
        // For a closed loop both orientations start at that same node, so a
        // reversed loop remains a distinct input edge until graph merging.
        // Vec ordering is the iterators' lexicographic ordering.
        let key = if segment.first() == segment.last() || segment.iter().le(segment.iter().rev()) {
            segment.clone()
        } else {
            segment.iter().rev().copied().collect()
        };
        if unique.insert(key) {
            paths.push(segment);
        }
    }
}

/// Pair the straightest incident paths at each junction while preserving the
/// original direction of one path, as Planetiler's `mergeTwoEdges` does.
/// Input paths must already be snapped and simplified. The helper splits paths
/// at shared interior coordinates and drops exact duplicate edges before
/// choosing strokes. Production uses the full directed graph below.
#[cfg(test)]
pub(super) fn stroke_merge(paths: &mut Vec<Vec<TilePoint>>) {
    node_and_deduplicate(paths);
    let mut slots: Vec<Option<Vec<TilePoint>>> = paths.drain(..).map(Some).collect();
    let mut nodes = Vec::new();
    let mut seen = BTreeSet::new();
    let mut ends: BTreeMap<TilePoint, Vec<usize>> = BTreeMap::new();
    for (index, path) in slots
        .iter()
        .enumerate()
        .filter_map(|(i, p)| p.as_ref().map(|p| (i, p)))
    {
        if path.len() < 2 {
            continue;
        }
        for point in [path[0], *path.last().unwrap()] {
            ends.entry(point).or_default().push(index);
            if seen.insert(point) {
                nodes.push(point);
            }
        }
    }
    for node in nodes {
        let mut incident = Vec::new();
        let mut touching_loop = false;
        for &index in ends.get(&node).into_iter().flatten() {
            let Some(path) = slots[index].as_ref() else {
                continue;
            };
            if path.len() < 2 {
                continue;
            }
            // A loop has both orientations at the same node. The full pinned
            // graph models that explicitly; leave the whole junction untouched
            // instead of pairing the other arms without the loop.
            if path[0] == *path.last().unwrap() {
                touching_loop = true;
                continue;
            }
            if path[0] == node {
                incident.push(Incident {
                    index,
                    at_start: true,
                });
            } else if *path.last().unwrap() == node {
                incident.push(Incident {
                    index,
                    at_start: false,
                });
            }
        }
        if touching_loop || incident.len() < 2 {
            continue;
        }
        let mut pairs = Vec::new();
        for i in 0..incident.len() {
            for j in i + 1..incident.len() {
                let a = incident[i];
                let b = incident[j];
                let first = slots[a.index].as_ref().unwrap();
                let second = slots[b.index].as_ref().unwrap();
                pairs.push((
                    separation(angle(first, a.at_start), angle(second, b.at_start)),
                    i,
                    j,
                ));
            }
        }
        // Java's stable ascending sort followed by reverse traversal chooses
        // the last enumerated pair when angular differences tie.
        pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
        let mut used = BTreeSet::new();
        for (_, i, j) in pairs.into_iter().rev() {
            let left = incident[i];
            let right = incident[j];
            if !used.insert(left.index) {
                continue;
            }
            if !used.insert(right.index) {
                used.remove(&left.index);
                continue;
            }
            let (a, b) = if left.at_start {
                (right, left)
            } else {
                (left, right)
            };
            let mut incoming = slots[a.index].take().unwrap();
            let mut outgoing = slots[b.index].take().unwrap();
            if a.at_start {
                incoming.reverse();
            }
            if !b.at_start {
                outgoing.reverse();
            }
            incoming.extend(outgoing.into_iter().skip(1));
            let next = slots.len();
            for endpoint in [incoming[0], *incoming.last().unwrap()] {
                ends.entry(endpoint).or_default().push(next);
            }
            slots.push(Some(incoming));
        }
    }
    *paths = slots.into_iter().flatten().collect();
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct DirectedEdge {
    edge: usize,
    forward: bool,
}

struct GraphNode {
    point: TilePoint,
    incident: Vec<DirectedEdge>,
}

struct GraphEdge {
    from: usize,
    to: usize,
    points: Vec<TilePoint>,
    length: f64,
    alive: bool,
}

#[derive(Default)]
struct LineGraph {
    nodes: Vec<GraphNode>,
    node_index: FxHashMap<TilePoint, usize>,
    edges: Vec<GraphEdge>,
}

impl LineGraph {
    fn node(&mut self, point: TilePoint) -> usize {
        if let Some(&index) = self.node_index.get(&point) {
            return index;
        }
        let index = self.nodes.len();
        self.nodes.push(GraphNode {
            point,
            incident: Vec::new(),
        });
        self.node_index.insert(point, index);
        index
    }

    fn add(&mut self, points: Vec<TilePoint>, length: f64) {
        let from = self.node(points[0]);
        let to = self.node(*points.last().unwrap());
        let index = self.edges.len();
        self.edges.push(GraphEdge {
            from,
            to,
            points,
            length,
            alive: true,
        });
        self.nodes[from].incident.push(DirectedEdge {
            edge: index,
            forward: true,
        });
        if from != to {
            self.nodes[to].incident.push(DirectedEdge {
                edge: index,
                forward: false,
            });
        }
    }

    fn other(&self, directed: DirectedEdge) -> usize {
        let edge = &self.edges[directed.edge];
        if directed.forward { edge.to } else { edge.from }
    }

    /// `angle(&self.oriented(directed), true)`, without the copy.
    fn start_angle(&self, directed: DirectedEdge) -> f64 {
        angle(&self.edges[directed.edge].points, directed.forward)
    }

    /// Whether the two directed edges run through the same points in order.
    fn same_oriented(&self, a: DirectedEdge, b: DirectedEdge) -> bool {
        let (pa, pb) = (&self.edges[a.edge].points, &self.edges[b.edge].points);
        if pa.len() != pb.len() {
            return false;
        }
        match (a.forward, b.forward) {
            (true, true) => pa == pb,
            (false, false) => pa.iter().rev().eq(pb.iter().rev()),
            (true, false) => pa.iter().eq(pb.iter().rev()),
            (false, true) => pa.iter().rev().eq(pb.iter()),
        }
    }

    fn oriented(&self, directed: DirectedEdge) -> Vec<TilePoint> {
        let edge = &self.edges[directed.edge];
        if directed.forward {
            edge.points.clone()
        } else {
            edge.points.iter().rev().copied().collect()
        }
    }

    fn remove(&mut self, index: usize) {
        if !self.edges[index].alive {
            return;
        }
        let edge = &mut self.edges[index];
        edge.alive = false;
        let from = edge.from;
        let to = edge.to;
        self.nodes[from].incident.retain(|d| d.edge != index);
        if from != to {
            self.nodes[to].incident.retain(|d| d.edge != index);
        }
    }

    fn merge_two(&mut self, first: DirectedEdge, second: DirectedEdge) -> usize {
        // Pinned mergeTwoEdges chooses the non-main edge as the incoming side,
        // retaining a main input's direction when one is available.
        let (a, b) = if first.forward {
            (second, first)
        } else {
            (first, second)
        };
        let from = self.other(a);
        let to = self.other(b);
        let length = self.edges[a.edge].length + self.edges[b.edge].length;
        let mut points = self.oriented(a);
        points.reverse();
        points.extend(self.oriented(b).into_iter().skip(1));
        self.remove(a.edge);
        self.remove(b.edge);
        debug_assert_eq!(points[0], self.nodes[from].point);
        debug_assert_eq!(*points.last().unwrap(), self.nodes[to].point);
        self.add(points, length);
        self.edges.len() - 1
    }

    fn degree_two_at(&mut self, node: usize) -> Option<usize> {
        let incident = &self.nodes[node].incident;
        if incident.len() != 2 {
            return None;
        }
        let (a, b) = (incident[0], incident[1]);
        if self.edges[a.edge].from == self.edges[a.edge].to
            || self.edges[b.edge].from == self.edges[b.edge].to
        {
            return None;
        }
        Some(self.merge_two(a, b))
    }

    fn degree_two_merge(&mut self) {
        for node in 0..self.nodes.len() {
            self.degree_two_at(node);
        }
    }

    fn shortest_path_except(
        &self,
        start: usize,
        end: usize,
        exclude: usize,
        max_length: f64,
    ) -> f64 {
        if start == exclude {
            return f64::INFINITY;
        }
        // Only looked up, never iterated.
        let mut best = FxHashMap::<usize, f64>::default();
        let mut pending = BinaryHeap::<Reverse<QueueItem>>::new();
        let mut sequence = 0;
        let heuristic = |node: usize| {
            let a = self.nodes[node].point;
            let b = self.nodes[end].point;
            (f64::from(a.x) - f64::from(b.x)).hypot(f64::from(a.y) - f64::from(b.y))
        };
        pending.push(Reverse(QueueItem {
            priority: heuristic(start),
            distance: 0.,
            sequence,
            index: start,
            forward: true,
        }));
        // The queue carries f=g+h; best stores g. This follows the pinned
        // A* bounded search, including its first-return behavior at `end`.
        while let Some(Reverse(candidate)) = pending.pop() {
            let node = candidate.index;
            let distance = candidate.distance;
            if node == end {
                return distance;
            }
            for directed in &self.nodes[node].incident {
                let neighbor = self.other(*directed);
                if neighbor == exclude {
                    continue;
                }
                let next = distance + self.edges[directed.edge].length;
                if next < *best.get(&neighbor).unwrap_or(&f64::INFINITY) {
                    best.insert(neighbor, next);
                    let lower_bound = next + heuristic(neighbor);
                    if lower_bound <= max_length {
                        sequence += 1;
                        pending.push(Reverse(QueueItem {
                            priority: lower_bound,
                            distance: next,
                            sequence,
                            index: neighbor,
                            forward: true,
                        }));
                    }
                }
            }
        }
        f64::INFINITY
    }

    fn break_short_loops(&mut self, maximum: f64) {
        for node in 0..self.nodes.len() {
            if self.nodes[node].incident.len() <= 1 {
                continue;
            }
            let currents = self.nodes[node].incident.clone();
            for current in currents {
                if !self.edges[current.edge].alive {
                    continue;
                }
                let mut loops = Vec::new();
                for &other in &self.nodes[node].incident {
                    let length = self.edges[other.edge].length;
                    let alternate = self.shortest_path_except(
                        self.other(other),
                        self.other(current),
                        node,
                        maximum - length,
                    );
                    let distance = length + alternate;
                    if distance <= maximum {
                        loops.push((other.edge, distance));
                    }
                }
                if loops.len() > 1 {
                    // Java stream().min keeps the first edge on equal costs.
                    let mut best = loops[0];
                    for &candidate in &loops[1..] {
                        if candidate.1 < best.1 {
                            best = candidate;
                        }
                    }
                    let keep = best.0;
                    for (edge, _) in loops {
                        if edge != keep {
                            self.remove(edge);
                        }
                    }
                }
            }
        }
    }

    fn is_short_stub(&self, index: usize, threshold: f64) -> bool {
        let edge = &self.edges[index];
        edge.alive
            && edge.length < threshold
            && (self.nodes[edge.from].incident.len() == 1
                || self.nodes[edge.to].incident.len() == 1
                || edge.from == edge.to)
    }

    fn remove_short_stubs(&mut self, threshold: f64) {
        let mut pending = BinaryHeap::<Reverse<QueueItem>>::new();
        let mut sequence = 0;
        let enqueue = |pending: &mut BinaryHeap<Reverse<QueueItem>>,
                       sequence: &mut usize,
                       directed: DirectedEdge,
                       graph: &Self| {
            if graph.is_short_stub(directed.edge, threshold) {
                pending.push(Reverse(QueueItem {
                    priority: graph.edges[directed.edge].length,
                    distance: 0.,
                    sequence: *sequence,
                    index: directed.edge,
                    forward: directed.forward,
                }));
                *sequence += 1;
            }
        };
        for node in &self.nodes {
            for directed in &node.incident {
                enqueue(&mut pending, &mut sequence, *directed, self);
            }
        }
        while let Some(Reverse(candidate)) = pending.pop() {
            let index = candidate.index;
            if !self.edges[index].alive {
                continue;
            }
            let edge = &self.edges[index];
            let (from, to) = if candidate.forward {
                (edge.from, edge.to)
            } else {
                (edge.to, edge.from)
            };
            if self.is_short_stub(index, threshold) {
                self.remove(index);
            }
            for node in [from, to] {
                if let Some(merged) = self.degree_two_at(node) {
                    enqueue(
                        &mut pending,
                        &mut sequence,
                        DirectedEdge {
                            edge: merged,
                            forward: true,
                        },
                        self,
                    );
                }
                if self.nodes[node].incident.len() == 1 {
                    let next = self.nodes[node].incident[0];
                    enqueue(&mut pending, &mut sequence, next, self);
                }
                if node == from && from == to {
                    break;
                }
            }
        }
    }

    fn remove_short_edges(&mut self, threshold: f64) {
        for node in 0..self.nodes.len() {
            let incident = self.nodes[node].incident.clone();
            for directed in incident {
                if self.edges[directed.edge].alive && self.edges[directed.edge].length < threshold {
                    self.remove(directed.edge);
                }
            }
        }
    }

    fn simplify(&mut self, tolerance: f64) {
        for edge in &mut self.edges {
            if edge.alive {
                simplify_path(&mut edge.points, tolerance);
            }
        }
        for index in 0..self.edges.len() {
            if self.edges[index].alive
                && (self.edges[index].points.len() < 2
                    || (self.edges[index].points.len() == 2
                        && self.edges[index].points[0] == self.edges[index].points[1]))
            {
                self.remove(index);
            }
        }
    }

    fn remove_duplicates(&mut self) {
        for node in 0..self.nodes.len() {
            let incident = self.nodes[node].incident.clone();
            for i in 0..incident.len() {
                let a = incident[i];
                if !self.edges[a.edge].alive {
                    continue;
                }
                for &b in &incident[i + 1..] {
                    if self.edges[b.edge].alive
                        && self.other(a) == self.other(b)
                        && self.same_oriented(a, b)
                    {
                        self.remove(b.edge);
                    }
                }
            }
        }
    }

    fn stroke_merge(&mut self) {
        for node in 0..self.nodes.len() {
            let incident = self.nodes[node].incident.clone();
            if incident.len() < 2 {
                continue;
            }
            let mut pairs = Vec::new();
            for i in 0..incident.len() {
                for j in i + 1..incident.len() {
                    let a = incident[i];
                    let b = incident[j];
                    if a.edge == b.edge {
                        continue;
                    }
                    pairs.push((separation(self.start_angle(a), self.start_angle(b)), i, j));
                }
            }
            pairs.sort_by(|a, b| a.0.total_cmp(&b.0));
            let mut used = BTreeSet::new();
            for (_, i, j) in pairs.into_iter().rev() {
                let a = incident[i];
                let b = incident[j];
                if !used.contains(&a.edge) && !used.contains(&b.edge) {
                    self.merge_two(a, b);
                    used.insert(a.edge);
                    used.insert(b.edge);
                }
            }
        }
    }

    fn output(&self) -> Vec<Vec<TilePoint>> {
        let mut result = Vec::new();
        for node in &self.nodes {
            for directed in &node.incident {
                if directed.forward && self.edges[directed.edge].alive {
                    result.push(self.edges[directed.edge].points.clone());
                }
            }
        }
        result
    }
}

fn path_length(points: &[TilePoint]) -> f64 {
    points
        .windows(2)
        .map(|pair| {
            let dx = f64::from(pair[1].x) - f64::from(pair[0].x);
            let dy = f64::from(pair[1].y) - f64::from(pair[0].y);
            dx.hypot(dy)
        })
        .sum()
}

fn simplify_path(points: &mut Vec<TilePoint>, tolerance: f64) {
    if points.len() <= 2 {
        return;
    }
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    let mut pending = vec![(0, points.len() - 1)];
    while let Some((first, last)) = pending.pop() {
        let a = points[first];
        let b = points[last];
        let dx = f64::from(b.x) - f64::from(a.x);
        let dy = f64::from(b.y) - f64::from(a.y);
        let mut maximum = tolerance * tolerance;
        let mut selected = None;
        for (index, point) in points.iter().enumerate().take(last).skip(first + 1) {
            let (mut x, mut y) = (f64::from(a.x), f64::from(a.y));
            if dx != 0. || dy != 0. {
                let t = ((f64::from(point.x) - x) * dx + (f64::from(point.y) - y) * dy)
                    / (dx * dx + dy * dy);
                if t > 1. {
                    x = f64::from(b.x);
                    y = f64::from(b.y);
                } else if t > 0. {
                    x += dx * t;
                    y += dy * t;
                }
            }
            let distance = (f64::from(point.x) - x).powi(2) + (f64::from(point.y) - y).powi(2);
            if distance > maximum {
                maximum = distance;
                selected = Some(index);
            }
        }
        if let Some(index) = selected {
            keep[index] = true;
            if index > first + 1 {
                pending.push((first, index));
            }
            if last > index + 1 {
                pending.push((index, last));
            }
        }
    }
    let mut index = 0;
    points.retain(|_| {
        let retain = keep[index];
        index += 1;
        retain
    });
}

/// Pinned graph stages on already snapped 1/16-pixel coordinates. Applicable
/// when `minLength` is zero (transportation z8+) or at most one grid unit
/// (boundary): loop/stub/min-length phases cannot remove a nondegenerate edge.
/// Collapsed edges are removed after DP.
pub(super) fn merge_graph_without_pruning(paths: &mut Vec<Vec<TilePoint>>, tolerance: f64) {
    merge_graph_with_pruning(paths, tolerance, 0.);
}

/// Full pinned stage order for postprocessing lines on the final MVT grid.
/// `minimum` is in 1/16-pixel grid units; edges retain their snapped pre-DP
/// lengths through joining and simplification, including the final filter.
pub(super) fn merge_graph_with_pruning(
    paths: &mut Vec<Vec<TilePoint>>,
    tolerance: f64,
    minimum: f64,
) {
    node_and_deduplicate(paths);
    let mut graph = LineGraph::default();
    for path in paths.drain(..) {
        let length = path_length(&path);
        if length > 0. {
            graph.add(path, length);
        }
    }
    graph.degree_two_merge();
    if minimum > 0. {
        graph.break_short_loops(minimum);
        graph.degree_two_merge();
        graph.remove_short_stubs(minimum.min(8.));
    }
    graph.simplify(tolerance);
    graph.remove_duplicates();
    graph.degree_two_merge();
    graph.stroke_merge();
    graph.degree_two_merge();
    if minimum > 0. {
        graph.remove_short_edges(minimum);
    }
    *paths = graph.output();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn points(raw: &serde_json::Value) -> Vec<TilePoint> {
        raw.as_array()
            .unwrap()
            .iter()
            .map(|point| {
                let p = point.as_array().unwrap();
                TilePoint::new(p[0].as_i64().unwrap() as i32, p[1].as_i64().unwrap() as i32)
            })
            .collect()
    }

    #[test]
    fn actual_boundary_three_way_stroke_matches_pinned_java() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-line-stroke-boundary-399309380.json"
        ))
        .unwrap();
        let mut paths: Vec<_> = fixture["input_paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(points)
            .collect();
        stroke_merge(&mut paths);
        let expected: Vec<_> = fixture["pinned_java_output_tolerance_0"]
            .as_array()
            .unwrap()
            .iter()
            .map(points)
            .collect();
        assert_eq!(paths.len(), expected.len());
        for expected_path in expected {
            assert!(paths.contains(&expected_path));
        }
    }

    #[test]
    fn interior_four_way_and_tied_straight_pairs_match_pinned_java() {
        let mut paths = vec![
            vec![
                TilePoint::new(0, 0),
                TilePoint::new(10, 0),
                TilePoint::new(20, 0),
            ],
            vec![TilePoint::new(10, 0), TilePoint::new(10, 10)],
            vec![TilePoint::new(10, 0), TilePoint::new(10, -10)],
        ];
        stroke_merge(&mut paths);
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&vec![
            TilePoint::new(0, 0),
            TilePoint::new(10, 0),
            TilePoint::new(20, 0)
        ]));
        assert!(paths.contains(&vec![
            TilePoint::new(10, -10),
            TilePoint::new(10, 0),
            TilePoint::new(10, 10)
        ]));
    }

    #[test]
    fn exact_reverse_duplicate_removed_and_loop_kept_closed() {
        let straight = vec![TilePoint::new(0, 0), TilePoint::new(4, 0)];
        let loop_path = vec![
            TilePoint::new(10, 0),
            TilePoint::new(12, 0),
            TilePoint::new(12, 2),
            TilePoint::new(10, 0),
        ];
        let mut paths = vec![
            straight.clone(),
            straight.iter().rev().copied().collect(),
            loop_path.clone(),
        ];
        stroke_merge(&mut paths);
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&straight));
        assert!(paths.contains(&loop_path));
    }

    #[test]
    fn closed_loop_suppresses_pairing_of_other_arms_at_same_node() {
        let node = TilePoint::new(0, 0);
        let mut paths = vec![
            vec![node, TilePoint::new(2, 0), TilePoint::new(2, 2), node],
            vec![node, TilePoint::new(10, 0)],
            vec![node, TilePoint::new(-10, 0)],
        ];
        let unchanged = paths.clone();
        stroke_merge(&mut paths);
        assert_eq!(paths, unchanged);
    }

    #[test]
    fn reversed_closed_loop_and_shared_branch_match_pinned_java() {
        let p = TilePoint::new;
        let forward = vec![p(0, 0), p(16, 0), p(16, 16), p(0, 16), p(0, 0)];
        let reverse = vec![p(0, 0), p(0, 16), p(16, 16), p(16, 0), p(0, 0)];
        let branch = vec![p(0, 0), p(-16, 0)];
        for (mut input, expected) in [
            (vec![forward.clone()], forward.clone()),
            (
                vec![forward.clone(), reverse.clone()],
                vec![p(0, 16), p(0, 0), p(16, 0), p(16, 16), p(0, 16)],
            ),
            (
                vec![forward.clone(), branch.clone()],
                vec![p(-16, 0), p(0, 0), p(16, 0), p(16, 16), p(0, 16), p(0, 0)],
            ),
            (
                vec![forward, reverse, branch],
                vec![p(0, 0), p(0, 16), p(16, 16), p(16, 0), p(0, 0), p(-16, 0)],
            ),
        ] {
            merge_graph_without_pruning(&mut input, 0.);
            assert_eq!(input, vec![expected]);
        }
    }

    #[test]
    fn boundary_graph_matches_pinned_three_arm_oracle() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-line-stroke-boundary-399309380.json"
        ))
        .unwrap();
        let mut paths: Vec<_> = fixture["input_paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(points)
            .collect();
        merge_graph_without_pruning(&mut paths, 0.);
        let expected: Vec<_> = fixture["pinned_java_output_tolerance_0"]
            .as_array()
            .unwrap()
            .iter()
            .map(points)
            .collect();
        assert_eq!(paths.len(), expected.len());
        for expected_path in expected {
            assert!(paths.contains(&expected_path));
        }
    }

    #[test]
    fn boundary_graph_nodes_shared_interior_before_simplifying() {
        let mut paths = vec![
            vec![
                TilePoint::new(0, 0),
                TilePoint::new(10, 0),
                TilePoint::new(20, 0),
            ],
            vec![TilePoint::new(10, 0), TilePoint::new(10, 10)],
            vec![TilePoint::new(10, 0), TilePoint::new(10, -10)],
        ];
        merge_graph_without_pruning(&mut paths, 0.);
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&vec![
            TilePoint::new(0, 0),
            TilePoint::new(10, 0),
            TilePoint::new(20, 0)
        ]));
        assert!(paths.contains(&vec![
            TilePoint::new(10, -10),
            TilePoint::new(10, 0),
            TilePoint::new(10, 10)
        ]));
    }

    #[test]
    fn boundary_graph_pairs_straight_arms_even_when_loop_touches_node() {
        let node = TilePoint::new(0, 0);
        let loop_path = vec![node, TilePoint::new(1, 2), TilePoint::new(2, 0), node];
        let straight = vec![TilePoint::new(-5, 0), node, TilePoint::new(5, 0)];
        let mut paths = vec![
            loop_path.clone(),
            vec![TilePoint::new(-5, 0), node],
            vec![node, TilePoint::new(5, 0)],
        ];
        merge_graph_without_pruning(&mut paths, 0.);
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&loop_path));
        assert!(paths.contains(&straight));
    }

    #[test]
    fn actual_transportation_three_way_matches_pinned_java() {
        // Actual z9/284/173 transportation ID 249717770 from opt33: native
        // kept three arms; the reference joins the west and short east arm.
        // Planetiler 0.10.2 LoopLineMerger with tolerance 0.1 px, no minimum
        // length, and stroke merging yields exactly the two paths below.
        let node = TilePoint::new(3068, 2189);
        let long = vec![TilePoint::new(3070, 2190), node, TilePoint::new(3045, 2173)];
        let short = vec![node, TilePoint::new(3069, 2191)];
        let mut paths = vec![
            vec![TilePoint::new(3070, 2190), node],
            vec![TilePoint::new(3045, 2173), node],
            short.clone(),
        ];
        merge_graph_without_pruning(&mut paths, 1.6);
        assert_eq!(paths.len(), 2);
        assert!(paths.contains(&long));
        assert!(paths.contains(&short));
    }

    #[test]
    fn pinned_short_hair_is_removed_then_trunk_rejoined() {
        // Planetiler 0.10.2: min=loop=1 px, stub=0.5 px, tolerance=0.1 px.
        // All values here are in the 1/16-pixel graph grid.
        let node = TilePoint::new(0, 0);
        let mut paths = vec![
            vec![TilePoint::new(-160, 0), node],
            vec![node, TilePoint::new(160, 0)],
            vec![node, TilePoint::new(0, 4)],
        ];
        merge_graph_with_pruning(&mut paths, 1.6, 16.);
        assert_eq!(
            paths,
            vec![vec![TilePoint::new(-160, 0), TilePoint::new(160, 0)]]
        );
    }

    #[test]
    fn pinned_short_alternate_loop_keeps_direct_route() {
        // Oracle isolates LoopLineMerger's loop phase with loopMin=2 px.
        let mut graph = LineGraph::default();
        for path in [
            vec![TilePoint::new(-80, 0), TilePoint::new(0, 0)],
            vec![TilePoint::new(0, 0), TilePoint::new(16, 0)],
            vec![
                TilePoint::new(0, 0),
                TilePoint::new(8, 4),
                TilePoint::new(16, 0),
            ],
            vec![TilePoint::new(16, 0), TilePoint::new(96, 0)],
        ] {
            graph.add(path.clone(), path_length(&path));
        }
        graph.degree_two_merge();
        graph.break_short_loops(32.);
        graph.degree_two_merge();
        graph.simplify(1.6);
        graph.remove_duplicates();
        graph.degree_two_merge();
        graph.stroke_merge();
        graph.degree_two_merge();
        let paths = graph.output();
        assert_eq!(
            paths,
            vec![vec![TilePoint::new(-80, 0), TilePoint::new(96, 0)]]
        );
    }

    #[test]
    fn pinned_final_length_uses_pre_dp_edge_length() {
        // Original two legs total sqrt(2) px, simplified line is 1 px.
        // Pinned Java retains it under a 1.2 px minimum.
        let mut paths = vec![vec![
            TilePoint::new(0, 0),
            TilePoint::new(8, 8),
            TilePoint::new(16, 0),
        ]];
        merge_graph_with_pruning(&mut paths, 16., 19.2);
        assert_eq!(
            paths,
            vec![vec![TilePoint::new(0, 0), TilePoint::new(16, 0)]]
        );
    }

    #[test]
    #[ignore = "requires external env-gated regional capture"]
    fn captured_boundary_graph_summary() {
        let path = std::env::var("OSM_TILE_COMPILER_CAPTURE_FILE").unwrap();
        let capture: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap();
        let scale = 2_f64.powi(capture["scale_exp"].as_u64().unwrap() as i32);
        let mut paths: Vec<_> = capture["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|path| {
                path.as_array()
                    .unwrap()
                    .iter()
                    .map(|p| {
                        TilePoint::new(
                            (p["x"].as_i64().unwrap() as f64 / scale + 0.5).floor() as i32,
                            (p["y"].as_i64().unwrap() as f64 / scale + 0.5).floor() as i32,
                        )
                    })
                    .collect()
            })
            .collect();
        merge_graph_without_pruning(&mut paths, 1.6);
        if let Ok(output) = std::env::var("OSM_TILE_COMPILER_GRAPH_DUMP") {
            std::fs::write(output, serde_json::to_vec(&paths).unwrap()).unwrap();
        }
        println!(
            "graph paths={} points={} sizes={:?}",
            paths.len(),
            paths.iter().map(Vec::len).sum::<usize>(),
            paths.iter().map(Vec::len).collect::<Vec<_>>()
        );
    }
}
