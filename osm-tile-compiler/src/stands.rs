//! Stands: where an extract's woods, scrub, orchards, grass, heath and fields are, as a grid of cells a tile, for a
//! renderer to grow trees in (`trees.rs` writes them). A cell holds what its middle lies in,
//! the smaller area over the larger, and nothing where a road, water, a building or a cleared
//! strip runs, so that no tree is grown there.

use osm_pbf_ingest::{
    IngestBuildProfile, NormalizedWay, Pass2Options, Pass2Sink, PrepareOptions,
    PreparedIngestIndex, RelationMemberType, Tags, TagsRef, WayBatch,
    prepare_ingest_index_with_options,
};
use rayon::prelude::*;
use std::collections::{BTreeMap, HashMap};
use std::path::Path;
use std::sync::Arc;

/// Cells along a tile's side: 3.06 m at 50 degrees north.
pub(crate) const SIDE: usize = 512;
/// Fixed-point units along a cell's side.
const CELL: i32 = 16;
/// Units along a tile's side.
const TILE: i32 = SIDE as i32 * CELL;
/// Units round the world, at z14: 2^27.
const WORLD: f64 = (1u32 << super::trees::ZOOM) as f64 * TILE as f64;
const EARTH_METRES: f64 = 40_075_016.685_578_49;
const MAX_LATITUDE: f64 = 85.051_128_779_806_59;
/// Metres round a mapped tree where none is grown.
const CLEAR_OF_TREE: f64 = 2.5;

/// What grows in an area. 0 is unknown but in `cover`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub(crate) struct Stand {
    /// 1 wood or forest, 2 scrub, 3 orchard, 4 swamp, 5 grass (meadow, grassland), 6 heath (and bog),
    /// 7 field (farmland), 8 reeds (reedbed, marsh, fen), 9 lawn (mown grass, parks, gardens).
    pub cover: u8,
    /// As a tree's; a field's the way its rows run (`field_way`, the high four bits) and its crop's
    /// own number (the low four).
    pub kind: u8,
    pub genus: u8,
    /// Metres.
    pub height: u8,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum What {
    Stand(Stand),
    /// Nothing grows, whatever else is there.
    Clear,
    /// The same, this many metres to either side of a line.
    Strip(f64),
}

/// What an area or a line with these tags is here. `get` looks a key up.
fn what<'a>(get: impl Fn(&str) -> Option<&'a str>, closed: bool) -> Option<What> {
    let is = |key: &str, values: &[&str]| get(key).is_some_and(|v| values.contains(&v));
    if closed {
        let cover = if is("landuse", &["forest"])
            || is("natural", &["wood"])
            || is("landcover", &["trees"])
        {
            1
        } else if is("natural", &["scrub"]) {
            2
        } else if is("landuse", &["orchard"]) {
            3
        } else if is("wetland", &["swamp"]) {
            4
        } else if is("landuse", &["meadow"]) || is("natural", &["grassland"]) {
            5
        } else if is("natural", &["heath"]) || is("wetland", &["bog"]) {
            6
        } else if is("landuse", &["farmland"]) {
            7
        } else if is("wetland", &["reedbed", "marsh", "fen"]) {
            8
        } else if is("landuse", &["grass", "village_green", "recreation_ground"]) || is("leisure", &["park", "garden", "common"]) {
            9
        } else {
            0
        };
        if cover > 0 {
            return Some(What::Stand(Stand {
                cover,
                kind: super::trees::kind(get("leaf_type"), get("leaf_cycle")),
                genus: get("genus")
                    .or(get("species"))
                    .or(get("taxon"))
                    .or(get("trees"))
                    .map_or(0, super::trees::genus),
                height: get("height")
                    .map_or(0, |h| (super::trees::decimetres(h) / 10).min(255) as u8),
            }));
        }
        if is("natural", &["water", "bare_rock", "scree", "glacier"])
            || is("landuse", &["reservoir", "basin", "quarry"])
            || is("waterway", &["riverbank"])
            || is("amenity", &["parking"])
            || is("leisure", &["pitch", "swimming_pool"])
            || is("piste:type", &["downhill"])
            || is("aeroway", &["runway", "taxiway", "apron"])
            || get("building").is_some_and(|b| b != "no")
        {
            return Some(What::Clear);
        }

    }
    if get("tunnel").is_some_and(|t| t != "no") {
        return None;
    }
    let half = if let Some(highway) = get("highway") {
        match highway {
            "motorway" | "trunk" => 8.0,
            "motorway_link" | "trunk_link" | "primary" | "primary_link" => 6.0,
            "secondary" | "secondary_link" | "tertiary" | "tertiary_link" => 5.0,
            "unclassified" | "residential" => 4.0,
            "service" | "living_street" | "pedestrian" | "road" => 3.0,
            "track" => 2.2,
            "path" | "footway" | "cycleway" | "bridleway" | "steps" => 1.4,
            _ => return None,
        }
    } else if let Some(railway) = get("railway") {
        match railway {
            "rail" | "light_rail" => 5.0,
            "narrow_gauge" | "tram" | "funicular" => 4.0,
            _ => return None,
        }
    } else if let Some(waterway) = get("waterway") {
        match waterway {
            "river" | "canal" => 4.0,
            "stream" | "drain" | "ditch" => 1.6,
            _ => return None,
        }
    } else if let Some(power) = get("power") {
        match power {
            "line" => 15.0,
            "minor_line" => 4.0,
            _ => return None,
        }
    } else if is(
        "aerialway",
        &["cable_car", "gondola", "chair_lift", "drag_lift", "t-bar"],
    ) {
        6.0
    } else if is("man_made", &["cutline"]) {
        3.0
    } else if is("piste:type", &["downhill"]) {
        10.0
    } else {
        return None;
    };
    Some(What::Strip(half))
}

fn closed_area(way: &NormalizedWay) -> bool {
    way.is_closed && way.node_ids.len() > 3
}

/// A place in units.
pub(crate) fn unit(longitude: f64, latitude: f64) -> [i32; 2] {
    let sin = latitude
        .clamp(-MAX_LATITUDE, MAX_LATITUDE)
        .to_radians()
        .sin();
    let x = (longitude + 180.0) / 360.0 * WORLD;
    let y = (0.5 - ((1.0 + sin) / (1.0 - sin)).ln() / (4.0 * std::f64::consts::PI)) * WORLD;
    [x, y].map(|c| c.clamp(0.0, WORLD - 1.0).round() as i32)
}

/// Metres in units at a latitude's `y`.
pub(crate) fn units(metres: f64, y: i32) -> f64 {
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * f64::from(y) / WORLD))
        .sinh()
        .atan();
    metres * WORLD / (EARTH_METRES * lat.cos())
}

/// Degrees east and north of a place in units.
pub(crate) fn place(p: [i32; 2]) -> [f64; 2] {
    let [u, v] = p.map(|c| f64::from(c) / WORLD);
    [u * 360.0 - 180.0, super::trees::latitude(v)]
}

/// A cell's square metres in row `y` of tiles.
pub(crate) fn cell_m2(y: u32) -> f64 {
    (f64::from(CELL) / units(1.0, (y as i32) * TILE + TILE / 2)).powi(2)
}

/// The way a field's rows run, as its edges do the most: 16 ways over half a turn from east
/// towards north, as `way * 16` (0 to 240).
fn field_way(pieces: &[Arc<[[i32; 2]]>]) -> u8 {
    let mut along = [0f64; 16];
    for piece in pieces {
        for w in piece.windows(2) {
            // y runs south
            let (dx, dy) = (f64::from(w[1][0] - w[0][0]), -f64::from(w[1][1] - w[0][1]));
            let length = dx.hypot(dy);
            if length > 0.0 {
                let way = dy.atan2(dx).rem_euclid(std::f64::consts::PI) / std::f64::consts::PI;
                along[(way * 16.0).round() as usize % 16] += length;
            }
        }
    }
    let most = (0..16).max_by(|&a, &b| along[a].total_cmp(&along[b])).unwrap_or(0);
    most as u8 * 16
}

/// An area's or a line's pieces.
struct Feature {
    what: What,
    pieces: Vec<Arc<[[i32; 2]]>>,
    bounds: [i32; 4],
}

impl Feature {
    fn new(mut what: What, pieces: Vec<Arc<[[i32; 2]]>>) -> Self {
        if let What::Stand(s) = &mut what
            && s.cover == 7
        {
            // its crop's number from where it is, the same each time it is read
            let first = pieces.first().and_then(|p| p.first()).copied().unwrap_or([0, 0]);
            let own = (first[0] as u32).wrapping_mul(0x9e37_79b9) ^ (first[1] as u32).wrapping_mul(0x85eb_ca6b);
            s.kind = field_way(&pieces) | (own >> 28) as u8;
        }
        let mut bounds = [i32::MAX, i32::MAX, i32::MIN, i32::MIN];
        for p in pieces.iter().flat_map(|p| p.iter()) {
            bounds = [
                bounds[0].min(p[0]),
                bounds[1].min(p[1]),
                bounds[2].max(p[0]),
                bounds[3].max(p[1]),
            ];
        }
        Self {
            what,
            pieces,
            bounds,
        }
    }

    /// Rows of tiles it may touch, `reach` units past its bounds.
    fn rows(&self, reach: i32) -> std::ops::RangeInclusive<i32> {
        (self.bounds[1] - reach).div_euclid(TILE)..=(self.bounds[3] + reach).div_euclid(TILE)
    }
}

/// What a relation with an area of ours has of its members.
struct Gathered {
    what: What,
    ways: usize,
    pieces: Vec<Arc<[[i32; 2]]>>,
    /// Each end's node, for telling whether the rings close.
    ends: HashMap<i64, u32>,
    broken: bool,
}

impl Gathered {
    /// Whether all of it was there: a ring with an end open would leak over its rows.
    fn whole(&self) -> bool {
        !self.broken && self.pieces.len() >= self.ways && self.ends.values().all(|n| n % 2 == 0)
    }
}

/// A row of trees, a place a tree.
pub(crate) struct Row {
    pub tags: Tags,
    pub line: Arc<[[i32; 2]]>,
}

/// The areas and lines of an extract that the grid is painted with.
pub(crate) struct Stands {
    index: PreparedIngestIndex,
    /// Relations with an area, and those a way is a member of.
    relations: Vec<Gathered>,
    members: HashMap<i64, Vec<u32>>,
    features: Vec<Feature>,
    pub rows: Vec<Row>,
    /// Areas left out because the extract does not hold all of them.
    incomplete: u64,
}

impl Stands {
    /// Reads the extract's relations and node positions; `temp` holds a large extract's.
    pub fn prepare(
        input: &Path,
        temp: &Path,
        threads: usize,
        cancelled: impl Fn() -> bool,
    ) -> Result<Self, String> {
        let index = prepare_ingest_index_with_options(
            input,
            IngestBuildProfile::DesktopFull,
            temp,
            PrepareOptions::new().worker_threads(threads),
            |_| {},
            cancelled,
        )?;
        let (mut relations, mut members) = (Vec::new(), HashMap::new());
        for r in &index.supported_relations {
            if r.tags.get("type") != Some("multipolygon") {
                continue;
            }
            let Some(what) = what(|k| r.tags.get(k), true).filter(|w| !matches!(w, What::Strip(_)))
            else {
                continue;
            };
            let at = relations.len() as u32;
            let mut ways = 0;
            for m in &r.members {
                if m.member_type == RelationMemberType::Way {
                    members.entry(m.member_id).or_insert_with(Vec::new).push(at);
                    ways += 1;
                }
            }
            relations.push(Gathered {
                what,
                ways,
                pieces: Vec::new(),
                ends: HashMap::new(),
                broken: false,
            });
        }
        Ok(Self {
            index,
            relations,
            members,
            features: Vec::new(),
            rows: Vec::new(),
            incomplete: 0,
        })
    }

    /// The options to read the ways we need with.
    pub fn options(&self, threads: usize) -> Pass2Options {
        let members: std::collections::HashSet<i64> = self.members.keys().copied().collect();
        Pass2Options::new()
            .worker_threads(threads)
            .skip_relations()
            .keep_ways_where(Arc::new(move |way| {
                members.contains(&way.way_id) || wanted(&way.tags)
            }))
    }

    pub fn ways(&mut self, batch: &WayBatch) {
        for way in &batch.ways {
            self.way(way);
        }
    }

    fn way(&mut self, way: &NormalizedWay) {
        let mut cursor = self.index.node_store.cursor();
        let line: Option<Arc<[[i32; 2]]>> = way
            .node_ids
            .iter()
            .map(|&id| cursor.get(id).map(|(lat, lng)| unit(lng, lat)))
            .collect();
        for &r in self.members.get(&way.way_id).into_iter().flatten() {
            let r = &mut self.relations[r as usize];
            match (&line, way.node_ids.first(), way.node_ids.last()) {
                (Some(line), Some(&a), Some(&b)) => {
                    r.pieces.push(line.clone());
                    if a != b {
                        *r.ends.entry(a).or_default() += 1;
                        *r.ends.entry(b).or_default() += 1;
                    }
                }
                _ => r.broken = true,
            }
        }
        let closed = closed_area(way);
        let what = what(|k| way.tags.get(k), closed);
        let row = way.tags.get("natural") == Some("tree_row");
        if what.is_none() && !row {
            return;
        }
        let Some(line) = line.filter(|l| l.len() > 1) else {
            self.incomplete += 1;
            return;
        };
        if row {
            self.rows.push(Row {
                tags: way.tags.clone(),
                line: line.clone(),
            });
        }
        if let Some(what) = what {
            self.features.push(Feature::new(what, vec![line]));
        }
    }

    /// Areas the extract does not hold all of.
    pub fn incomplete_areas(&self) -> u64 {
        self.incomplete + self.relations.iter().filter(|r| !r.whole()).count() as u64
    }

    /// Paints the grids: each tile's that has any stand, encoded for it (`encode`), by `(x, y)`.
    /// `trees`, the mapped trees' places, are kept clear.
    pub fn paint(
        mut self,
        trees: Vec<[i32; 2]>,
        check: impl Fn() -> Result<(), String> + Sync,
    ) -> Result<BTreeMap<(u32, u32), Painted>, String> {
        drop(self.members);
        drop(self.index);
        for r in self.relations {
            if r.whole() && !r.pieces.is_empty() {
                self.features.push(Feature::new(r.what, r.pieces));
            }
        }
        for t in trees {
            self.features.push(Feature::new(
                What::Strip(CLEAR_OF_TREE),
                vec![Arc::from([t, t])],
            ));
        }
        // the larger under the smaller, and what is always clear over both
        let mut order: Vec<usize> = (0..self.features.len()).collect();
        let key = |f: &Feature| {
            let [x0, y0, x1, y1] = f.bounds.map(i64::from);
            (
                matches!(f.what, What::Clear | What::Strip(_)),
                std::cmp::Reverse((x1 - x0) * (y1 - y0)),
            )
        };
        order.sort_by_key(|&i| key(&self.features[i]));
        let features: Vec<Feature> = {
            let mut own: Vec<Option<Feature>> = self.features.into_iter().map(Some).collect();
            order.into_iter().filter_map(|i| own[i].take()).collect()
        };
        let mut rows: BTreeMap<i32, Vec<u32>> = BTreeMap::new();
        for f in &features {
            if matches!(f.what, What::Stand(_)) {
                for row in f.rows(0) {
                    rows.entry(row).or_default();
                }
            }
        }
        for (i, f) in features.iter().enumerate() {
            for row in f.rows(f.reach()) {
                if let Some(list) = rows.get_mut(&row) {
                    list.push(i as u32);
                }
            }
        }
        let painted: Vec<Vec<((u32, u32), Painted)>> = rows
            .into_par_iter()
            .map(|(row, list)| {
                check()?;
                Ok(paint_row(row, list.iter().map(|&i| &features[i as usize])))
            })
            .collect::<Result<_, String>>()?;
        Ok(painted.into_iter().flatten().collect())
    }
}

/// Whether a way is an area, a line or a row of ours.
fn wanted(tags: &TagsRef<'_>) -> bool {
    tags.get("natural") == Some("tree_row") || what(|k| tags.get(k), true).is_some()
}

impl Feature {
    /// How far it reaches past its bounds, in units.
    fn reach(&self) -> i32 {
        match self.what {
            What::Strip(half) => units(half, self.bounds[1])
                .max(units(half, self.bounds[3]))
                .ceil() as i32,
            _ => 0,
        }
    }
}

/// A tile's stands and its cells, each 0 or a stand's number from 1.
#[derive(Debug, PartialEq)]
pub(crate) struct Painted {
    pub stands: Vec<Stand>,
    /// The cells in runs, each its length times 256 and its value: a region's grids are held at
    /// once.
    runs: Vec<u32>,
}

impl Painted {
    /// Cells that hold a stand.
    pub fn covered(&self) -> usize {
        self.runs
            .iter()
            .filter(|r| **r & 255 != 0)
            .map(|r| (r >> 8) as usize)
            .sum()
    }

    /// The count of stands, four bytes each, then the cells by rows from the north-west.
    pub fn encode(&self, out: &mut Vec<u8>) {
        out.push(self.stands.len() as u8);
        for s in &self.stands {
            out.extend([s.cover, s.kind, s.genus, s.height]);
        }
        self.cells(out);
    }

    fn cells(&self, out: &mut Vec<u8>) {
        for r in &self.runs {
            out.resize(out.len() + (r >> 8) as usize, *r as u8);
        }
    }
}

/// The grids of a row of tiles.
struct Grids {
    top: i32,
    tiles: BTreeMap<i32, Vec<u16>>,
    stands: Vec<Stand>,
}

impl Grids {
    /// Paints the cells from `x0` to `x1` (world cells, not `x1`) of `row`.
    fn span(&mut self, row: usize, x0: i32, x1: i32, what: What, number: u16) {
        let side = SIDE as i32;
        let mut x = x0;
        while x < x1 {
            let tile = x.div_euclid(side);
            let end = ((tile + 1) * side).min(x1);
            let range = (x - tile * side) as usize..(end - tile * side) as usize;
            x = end;
            let grid = if matches!(what, What::Stand(_)) {
                self.tiles
                    .entry(tile)
                    .or_insert_with(|| vec![0; SIDE * SIDE])
            } else {
                match self.tiles.get_mut(&tile) {
                    Some(g) => g,
                    None => continue,
                }
            };
            for c in &mut grid[row * SIDE..][..SIDE][range] {
                *c = match (what, *c) {
                    // a wood's leaf type stays where a smaller wood in it has none
                    (What::Stand(new), old) if old > 0 => {
                        let o = self.stands[usize::from(old) - 1];
                        if new.cover == o.cover && new.kind == 0 && new.genus == 0 {
                            old
                        } else {
                            number
                        }
                    }
                    _ => number,
                };
            }
        }
    }

    fn fill(&mut self, f: &Feature, number: u16, crossings: &mut [Vec<f64>]) {
        crossings.iter_mut().for_each(Vec::clear);
        for piece in &f.pieces {
            for e in piece.windows(2) {
                let ([ax, ay], [bx, by]) = (e[0], e[1]);
                if ay == by {
                    continue;
                }
                // the rows whose middles it crosses
                let row = |y: i32| (y - self.top - CELL / 2).div_euclid(CELL) + 1;
                let (lo, hi) = (row(ay.min(by)).max(0), row(ay.max(by)).min(SIDE as i32));
                for r in lo..hi {
                    let y = f64::from(self.top + r * CELL + CELL / 2);
                    let t = (y - f64::from(ay)) / f64::from(by - ay);
                    crossings[r as usize].push(f64::from(ax) + t * f64::from(bx - ax));
                }
            }
        }
        let cell = f64::from(CELL);
        for (r, xs) in crossings.iter_mut().enumerate() {
            xs.sort_by(f64::total_cmp);
            for pair in xs.as_chunks::<2>().0 {
                let [x0, x1] = [pair[0], pair[1]].map(|x| ((x / cell) - 0.5).ceil() as i32);
                if x0 < x1 {
                    self.span(r, x0, x1, f.what, number);
                }
            }
        }
    }

    /// Clears the cells whose middles are within `half` units of the line.
    fn strip(&mut self, f: &Feature, half: f64) {
        let cell = f64::from(CELL);
        for piece in &f.pieces {
            for e in piece.windows(2) {
                let [a, b] = [e[0], e[1]].map(|p| [f64::from(p[0]), f64::from(p[1] - self.top)]);
                let d = [b[0] - a[0], b[1] - a[1]];
                let long = (d[0] * d[0] + d[1] * d[1]).max(1e-9);
                let lo = ((a[1].min(b[1]) - half) / cell - 0.5).ceil().max(0.0) as i32;
                let hi = ((a[1].max(b[1]) + half) / cell - 0.5).floor() as i32;
                for r in lo..=hi.min(SIDE as i32 - 1) {
                    let y = (f64::from(r) + 0.5) * cell;
                    // where the row is nearest the line, and as far each way as is in reach
                    let x0 = ((a[0].min(b[0]) - half) / cell - 0.5).ceil() as i32;
                    let x1 = ((a[0].max(b[0]) + half) / cell - 0.5).floor() as i32;
                    let within = |c: i32| {
                        let x = (f64::from(c) + 0.5) * cell;
                        let t = (((x - a[0]) * d[0] + (y - a[1]) * d[1]) / long).clamp(0.0, 1.0);
                        (x - a[0] - d[0] * t).hypot(y - a[1] - d[1] * t) <= half
                    };
                    // the cells within are one run, round where the line is nearest
                    let near = if d[1].abs() > 1e-9 {
                        a[0] + d[0] * ((y - a[1]) / d[1]).clamp(0.0, 1.0)
                    } else {
                        (a[0] + b[0]) * 0.5
                    };
                    let mid = ((near / cell - 0.5).round() as i32).clamp(x0.min(x1), x1.max(x0));
                    let (mut first, mut last) = (mid, mid);
                    while first > x0 && within(first - 1) {
                        first -= 1;
                    }
                    while last < x1 && within(last + 1) {
                        last += 1;
                    }
                    if first < mid || last > mid || within(mid) {
                        if !within(first) {
                            first += 1;
                        }
                        if first <= last {
                            self.span(r as usize, first, last + 1, What::Clear, 0);
                        }
                    }
                }
            }
        }
    }
}

fn paint_row<'a>(
    row: i32,
    features: impl Iterator<Item = &'a Feature>,
) -> Vec<((u32, u32), Painted)> {
    let mut g = Grids {
        top: row * TILE,
        tiles: BTreeMap::new(),
        stands: Vec::new(),
    };
    let mut crossings = vec![Vec::new(); SIDE];
    for f in features {
        match f.what {
            What::Stand(s) => {
                let number = g.stands.iter().position(|t| *t == s).unwrap_or_else(|| {
                    g.stands.push(s);
                    g.stands.len() - 1
                });
                // more than a row of tiles could have: the last number stands for the rest
                let number = number.min(usize::from(u16::MAX) - 1) as u16 + 1;
                g.fill(f, number, &mut crossings);
            }
            What::Clear => g.fill(f, 0, &mut crossings),
            What::Strip(half) => {
                let half = units(half, f.bounds[1] / 2 + f.bounds[3] / 2);
                g.strip(f, half);
            }
        }
    }
    let Grids { tiles, stands, .. } = g;
    tiles
        .into_iter()
        .filter(|(x, cells)| {
            (0..1 << super::trees::ZOOM).contains(x) && cells.iter().any(|c| *c != 0)
        })
        .map(|(x, cells)| ((x as u32, row as u32), tile(&cells, &stands)))
        .collect()
}

/// A tile's own stands, numbered in their order (past 255, as the first of their cover).
fn tile(cells: &[u16], all: &[Stand]) -> Painted {
    let mut seen: Vec<Stand> = {
        let mut used = vec![false; all.len() + 1];
        cells.iter().for_each(|c| used[usize::from(*c)] = true);
        all.iter()
            .zip(&used[1..])
            .filter(|(_, u)| **u)
            .map(|(s, _)| *s)
            .collect()
    };
    seen.sort_unstable();
    seen.dedup();
    seen.truncate(255);
    let number: Vec<u8> = std::iter::once(0)
        .chain(all.iter().map(|s| {
            let at = seen
                .iter()
                .position(|t| t == s)
                .or_else(|| seen.iter().position(|t| t.cover == s.cover));
            at.map_or(0, |i| i as u8 + 1)
        }))
        .collect();
    let runs = cells
        .chunk_by(|a, b| a == b)
        .map(|run| (run.len() as u32) << 8 | u32::from(number[usize::from(run[0])]))
        .collect();
    Painted { stands: seen, runs }
}

/// Discards the nodes pass 2 would stream: it has none here.
pub(crate) struct WaysOnly<'a>(pub &'a mut Stands);

impl Pass2Sink for WaysOnly<'_> {
    fn on_relation_batch(&mut self, _: osm_pbf_ingest::RelationBatch) -> Result<(), String> {
        Ok(())
    }

    fn on_way_batch(&mut self, batch: WayBatch) -> Result<(), String> {
        self.0.ways(&batch);
        Ok(())
    }

    fn on_node_batch(&mut self, _: osm_pbf_ingest::NodeBatch) -> Result<(), String> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(list: &[(&str, &str)]) -> Tags {
        let mut t = Tags::new();
        list.iter().for_each(|(k, v)| t.insert(k, v));
        t
    }

    fn told(list: &[(&str, &str)], closed: bool) -> Option<What> {
        let t = tags(list);
        what(|k| t.get(k), closed)
    }

    fn wood(kind: u8) -> What {
        What::Stand(Stand {
            cover: 1,
            kind,
            genus: 0,
            height: 0,
        })
    }

    /// A ring of cells `x0..x1` by `y0..y1` of the tile row 10's tile 20.
    fn square(x0: i32, y0: i32, x1: i32, y1: i32) -> Arc<[[i32; 2]]> {
        let at = |x: i32, y: i32| [20 * TILE + x * CELL, 10 * TILE + y * CELL];
        Arc::from([at(x0, y0), at(x1, y0), at(x1, y1), at(x0, y1), at(x0, y0)])
    }

    fn painted(features: &[Feature]) -> Vec<((u32, u32), Painted)> {
        paint_row(10, features.iter())
    }

    fn cell(p: &Painted, x: usize, y: usize) -> u8 {
        let mut cells = Vec::new();
        p.cells(&mut cells);
        cells[y * SIDE + x]
    }

    #[test]
    fn tags_tell_stands_from_what_clears_them() {
        assert_eq!(told(&[("natural", "wood")], true), Some(wood(0)));
        assert_eq!(
            told(
                &[("landuse", "forest"), ("leaf_type", "needleleaved")],
                true
            ),
            Some(wood(2))
        );
        assert!(
            matches!(told(&[("landuse", "orchard"), ("trees", "malus")], true), Some(What::Stand(s)) if s.cover == 3 && s.genus > 0)
        );
        assert_eq!(told(&[("building", "yes")], true), Some(What::Clear));
        assert_eq!(told(&[("building", "no")], true), None);
        let cover = |tags: &[(&str, &str)]| match told(tags, true) {
            Some(What::Stand(s)) => s.cover,
            _ => 0,
        };
        assert_eq!(cover(&[("landuse", "meadow")]), 5);
        assert_eq!(cover(&[("natural", "grassland")]), 5);
        assert_eq!(cover(&[("natural", "heath")]), 6);
        assert_eq!(cover(&[("wetland", "bog")]), 6);
        assert_eq!(cover(&[("landuse", "farmland")]), 7);
        assert_eq!(cover(&[("wetland", "marsh")]), 8);
        assert_eq!(cover(&[("leisure", "park")]), 9);
        assert_eq!(cover(&[("landuse", "grass")]), 9);
        assert_eq!(told(&[("natural", "wood")], false), None);
        assert!(matches!(told(&[("highway", "track")], false), Some(What::Strip(m)) if m < 3.0));
        assert_eq!(
            told(&[("highway", "motorway"), ("tunnel", "yes")], false),
            None
        );
        assert_eq!(told(&[("highway", "proposed")], false), None);
    }

    #[test]
    fn a_fields_rows_run_along_its_longest_edges() {
        // 200 by 40 units, its long sides running east; then turned a quarter (north)
        let east: Arc<[[i32; 2]]> = Arc::from([[0, 0], [200, 0], [200, 40], [0, 40], [0, 0]]);
        assert_eq!(field_way(&[east]), 0);
        let north: Arc<[[i32; 2]]> = Arc::from([[0, 0], [40, 0], [40, -200], [0, -200], [0, 0]]);
        assert_eq!(field_way(&[north]), 8 * 16);
        // north-east: y runs south
        let diagonal: Arc<[[i32; 2]]> = Arc::from([[0, 0], [100, -100], [110, -90], [10, 10], [0, 0]]);
        assert_eq!(field_way(&[diagonal]), 4 * 16);
    }

    #[test]
    fn areas_fill_the_cells_whose_middles_they_hold() {
        // across two tiles, with a hole
        let p = painted(&[Feature::new(
            wood(0),
            vec![square(500, 4, 520, 8), square(505, 5, 507, 7)],
        )]);
        assert_eq!(
            p.iter().map(|t| t.0).collect::<Vec<_>>(),
            [(20, 10), (21, 10)]
        );
        assert_eq!(p[0].1.covered() + p[1].1.covered(), 20 * 4 - 2 * 2);
        assert_eq!(cell(&p[0].1, 499, 4), 0);
        assert_eq!(cell(&p[0].1, 500, 4), 1);
        assert_eq!(cell(&p[0].1, 505, 5), 0);
        assert_eq!(cell(&p[1].1, 7, 7), 1);
        assert_eq!(cell(&p[1].1, 8, 7), 0);
    }

    #[test]
    fn the_smaller_area_wins_and_keeps_a_known_leaf_type() {
        let p = painted(&[
            Feature::new(wood(2), vec![square(0, 0, 100, 100)]),
            Feature::new(wood(0), vec![square(10, 10, 20, 20)]),
            Feature::new(wood(1), vec![square(30, 30, 40, 40)]),
            Feature::new(What::Clear, vec![square(50, 50, 60, 60)]),
        ]);
        let p = &p[0].1;
        let kind = |x, y| p.stands[usize::from(cell(p, x, y)) - 1].kind;
        assert_eq!((kind(5, 5), kind(15, 15), kind(35, 35)), (2, 2, 1));
        assert_eq!(cell(p, 55, 55), 0);
    }

    #[test]
    fn strips_clear_their_width() {
        let at = |x: i32, y: i32| [20 * TILE + x * CELL, 10 * TILE + y * CELL];
        let y = 10 * TILE;
        let metres = 3.0 / units(1.0, y) * f64::from(CELL);
        for (a, b) in [
            ((0, 50), (100, 50)),
            ((50, 0), (50, 100)),
            ((0, 0), (100, 100)),
        ] {
            let p = painted(&[
                Feature::new(wood(0), vec![square(0, 0, 100, 100)]),
                Feature::new(
                    What::Strip(metres),
                    vec![Arc::from([at(a.0, a.1), at(b.0, b.1)])],
                ),
            ]);
            let p = &p[0].1;
            let d = (f64::from(b.0 - a.0), f64::from(b.1 - a.1));
            let mut cells = Vec::new();
            p.cells(&mut cells);
            for (x, y) in (0..100).flat_map(|x| (0..100).map(move |y| (x, y))) {
                let (px, py) = (
                    x as f64 + 0.5 - f64::from(a.0),
                    y as f64 + 0.5 - f64::from(a.1),
                );
                let off = (px * d.1 - py * d.0).abs() / d.0.hypot(d.1);
                // away from the rounded ends
                if off < 2.9 {
                    assert_eq!(cells[y * SIDE + x], 0, "{x} {y} of {a:?}");
                }
                if off > 3.1 {
                    assert_eq!(cells[y * SIDE + x], 1, "{x} {y} of {a:?}");
                }
            }
        }
    }

    #[test]
    fn places_and_metres_agree() {
        let [a, b] = [unit(19.9373, 50.0617), unit(19.9373, 50.0707)];
        let metres = 0.009f64.to_radians() * EARTH_METRES / std::f64::consts::TAU;
        let got = f64::from(a[1] - b[1]);
        assert!((got - units(metres, a[1])).abs() < got * 0.001);
        assert_eq!([a[0] / TILE, a[1] / TILE], [9099, 5552]);
    }

    #[test]
    fn tiles_encode_their_stands_then_cells() {
        let p = painted(&[Feature::new(wood(2), vec![square(1, 0, 2, 1)])]);
        let mut out = Vec::new();
        p[0].1.encode(&mut out);
        assert_eq!(out[..8], [1, 1, 2, 0, 0, 0, 1, 0]);
        assert_eq!(out.len(), 5 + SIDE * SIDE);
    }
}
