//! Trees: a PMTiles archive of the trees mapped one by one (`natural=tree` nodes), the rows of
//! trees (`natural=tree_row`) and hedges (`barrier=hedge`) in an OSM extract, and of where its
//! woods, scrub and orchards are, for renderers that stand a model at each tree and grow the
//! rest. The OpenMapTiles schema has none of the first, and no leaf types of the second.
//!
//! The archive is PMTiles v3 with tile type unknown, gzipped per tile, z14 only. Each tile is
//! a little-endian `u32` count and then columns of that many values: `x`, `y` (the place in
//! the tile, in 65536ths of it from its north-west corner) and `height` (decimetres) as
//! `u16`, then `crown` (its width, decimetres), `kind`, `genus` and `source` as `u8`, 10 bytes
//! a tree. 0 is unknown in `height`, `crown`, `kind`'s fields and `genus`; `source` is 0 for a
//! tree's own node and 1 for one of a row. Trees keep the extract's order, which is by ID in
//! the extracts everyone publishes, rows after nodes. Then the stands: a `u8` count, each stand's
//! `cover` (1 wood, 2 scrub, 3 orchard, 4 swamp, 5 grass, 6 heath, 7 field, 8 reeds, 9 lawn), `kind`, `genus` and height (metres), and,
//! unless there are none, 512 x 512 `u8` cells by rows from the north-west, each 0 or a stand's
//! number from 1 (`stands.rs`). Then, unless there are none, the lines: a `u32` count and columns
//! of that many: `points` and `height` (decimetres) as `u16`, then `what` (1 a hedge, 2 a row of
//! trees), `kind`, `genus` and `form` (a hedge's: 0 unknown, 1 clipped, 2 grown freely) as `u8`,
//! and then every line's points, first their `x` and then their `y` as `u16`, in the trees' units.
//! A line is cut where it leaves its tile, each piece in the tile it crosses. A tile with neither
//! trees, stands nor lines is not written.
//!
//! Nothing is guessed: a tree whose tags say nothing is a place alone, a line a line, and a
//! renderer decides what stands there and how far apart. Version 1 had no lines, and a row's
//! trees as trees 8 m apart, `source` 1.

use crate::control::{CompileError, Control};
use crate::output::{OutputFormat, PartialArchive};
use crate::stands::{self, Painted, Row, Stands, WaysOnly};
use flate2::write::GzEncoder;
use osm_pbf_ingest::{
    IngestBuildProfile, NodeBatch, NormalizedNode, Pass2Options, Pass2Sink, RelationBatch,
    WayBatch, run_pass2_with_options,
};
use pmtiles::{Compression, PmTilesWriter, TileCoord, TileId, TileType};
use rayon::prelude::*;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The tree archives' metadata `format`.
pub const TREES_FORMAT: &str = "trees";
/// Their metadata `version`. See `docs/formats.md` for when it changes.
pub const TREES_VERSION: u32 = 2;
/// Spelled out so the bytes stay fixed; a test below checks it against the
/// two constants above.
const METADATA: &str =
    r#"{"format":"trees","version":2,"attribution":"© OpenStreetMap contributors"}"#;
pub(crate) const ZOOM: u8 = 14;
const GZIP_LEVEL: u32 = 6;
const MAX_LATITUDE: f64 = 85.051_128_779_806_59;

/// The genera `genus` counts from 1. New ones go at the end.
pub const TREE_GENERA: [&str; 40] = [
    "acer",
    "aesculus",
    "ailanthus",
    "alnus",
    "betula",
    "carpinus",
    "castanea",
    "catalpa",
    "cedrus",
    "celtis",
    "corylus",
    "crataegus",
    "cupressus",
    "eucalyptus",
    "fagus",
    "fraxinus",
    "ginkgo",
    "gleditsia",
    "juglans",
    "larix",
    "liquidambar",
    "liriodendron",
    "magnolia",
    "malus",
    "morus",
    "olea",
    "picea",
    "abies",
    "pinus",
    "platanus",
    "populus",
    "prunus",
    "pseudotsuga",
    "pyrus",
    "quercus",
    "robinia",
    "salix",
    "sorbus",
    "tilia",
    "ulmus",
];

/// What stands at a tree's node, as the tile holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Tree {
    x: u16,
    y: u16,
    height: u16,
    crown: u8,
    /// Leaf type in the low three bits (1 broadleaved, 2 needleleaved, 3 palm, 4 mixed, 5
    /// leafless), leaf cycle in the next two (1 deciduous, 2 evergreen, 3 between them).
    kind: u8,
    genus: u8,
    /// 0: mapped as a node; 1: in a row (version 1 alone).
    source: u8,
}

/// A hedge's or a row's line in a tile, as the tile holds it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Line {
    /// 1 a hedge, 2 a row of trees.
    what: u8,
    /// Decimetres, 0 unknown; `kind` and `genus` as a tree's.
    height: u16,
    kind: u8,
    genus: u8,
    /// A hedge's: 0 unknown, 1 clipped, 2 grown freely.
    form: u8,
    /// In the tile, as a tree's place.
    points: Vec<[u16; 2]>,
}

/// What [`write_trees`] wrote.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct TreesReport {
    pub tiles: u64,
    pub trees: u64,
    /// Rows of trees and hedges, and their kilometres.
    pub rows: u64,
    pub row_km: f64,
    pub hedges: u64,
    pub hedge_km: f64,
    /// Trees whose tags give a leaf type, or a genus.
    pub with_kind: u64,
    /// Trees whose tags give a height.
    pub with_height: u64,
    /// Tiles with stands, and the stands' area, square kilometres.
    pub stand_tiles: u64,
    pub stand_km2: f64,
    /// Areas left out because the extract does not hold all of them.
    pub incomplete: u64,
    pub raw_bytes: u64,
    pub gz_bytes: u64,
    pub seconds: f64,
}

/// Where a basemap's trees go by convention: beside it, named after it, so `basemap.pmtiles`
/// has `basemap-trees.pmtiles`.
pub fn trees_path(archive: &Path) -> PathBuf {
    let stem = archive
        .file_stem()
        .map_or_else(|| "basemap".into(), |s| s.to_string_lossy());
    archive.with_file_name(format!("{stem}-trees.pmtiles"))
}

/// Reads the mapped trees and the stands of the OSM extract `input` and writes them to the new archive
/// `output`, stopping when `should_cancel` returns true. The bytes are the same for any
/// number of `threads`. An existing `output` is never replaced, and whatever the outcome no
/// partial archive is left.
pub fn write_trees(
    input: &Path,
    output: &Path,
    threads: usize,
    should_cancel: impl Fn() -> bool,
) -> Result<TreesReport, CompileError> {
    if threads == 0 {
        return Err(CompileError::InvalidRequest(
            "threads must be positive".into(),
        ));
    }
    if output.exists() {
        return Err(CompileError::InvalidRequest(format!(
            "refusing to replace existing archive {}",
            output.display()
        )));
    }
    crate::control::run(
        threads,
        false,
        |_| {},
        should_cancel,
        |control| write(input, output, threads, control),
    )
}

fn write(
    input: &Path,
    output: &Path,
    threads: usize,
    control: &Control,
) -> Result<TreesReport, String> {
    let start = Instant::now();
    let mut sink = Trees::default();
    let cancelled = || control.check().is_err();
    let profile = IngestBuildProfile::DesktopFull;
    let mut options = Pass2Options::new().keep_ways(|_| false);
    options.worker_threads = Some(threads);
    options.skip_relations = true;
    run_pass2_with_options(input, profile, options, &mut sink, |_| {}, cancelled)?;
    control.check()?;
    let mut report = TreesReport {
        tiles: 0,
        trees: 0,
        rows: 0,
        row_km: 0.0,
        hedges: 0,
        hedge_km: 0.0,
        with_kind: 0,
        with_height: 0,
        stand_tiles: 0,
        stand_km2: 0.0,
        incomplete: 0,
        raw_bytes: 0,
        gz_bytes: 0,
        seconds: 0.0,
    };
    let failed = |e: pmtiles::PmtError| format!("Failed to write {}: {e}", output.display());
    let painted = {
        // a large extract's node positions
        let temp = Temp::create(output)?;
        let mut stands = Stands::prepare(input, &temp.0, threads, cancelled)?;
        let options = stands.options(threads);
        run_pass2_with_options(
            input,
            profile,
            options,
            &mut WaysOnly(&mut stands),
            |_| {},
            cancelled,
        )?;
        control.check()?;
        for row in std::mem::take(&mut stands.rows) {
            let km = length(&row.line) / 1000.0;
            if row.hedge {
                (report.hedges, report.hedge_km) = (report.hedges + 1, report.hedge_km + km);
            } else {
                (report.rows, report.row_km) = (report.rows + 1, report.row_km + km);
            }
            sink.line(&row)?;
        }
        report.incomplete = stands.incomplete_areas();
        let places = std::mem::take(&mut sink.places);
        stands.paint(places, || control.check())?
    };
    for ((x, y), p) in painted {
        let coord = TileCoord::new(ZOOM, x, y).map_err(|e| format!("No tile {x} {y}: {e}"))?;
        report.stand_tiles += 1;
        report.stand_km2 += p.covered() as f64 * stands::cell_m2(y) / 1e6;
        sink.reach(x, y);
        sink.stands.insert(TileId::from(coord).value(), p);
    }
    if sink.tiles.is_empty() && sink.stands.is_empty() && sink.lines.is_empty() {
        return Err(format!(
            "{} has no trees, woods, orchards or hedges",
            input.display()
        ));
    }
    let partial = PartialArchive::create(output, OutputFormat::PmTiles)?;
    let [west, south, east, north] = sink.bounds;
    let mut writer = PmTilesWriter::new(TileType::Unknown)
        .precompressed(Compression::Gzip)
        .min_zoom(ZOOM)
        .max_zoom(ZOOM)
        .bounds(west, south, east, north)
        .center((west + east) / 2.0, (south + north) / 2.0)
        .center_zoom(ZOOM)
        .metadata(METADATA)
        .create(partial.file())
        .map_err(failed)?;
    let mut ids: Vec<u64> = sink
        .tiles
        .keys()
        .chain(sink.stands.keys())
        .chain(sink.lines.keys())
        .copied()
        .collect();
    ids.sort_unstable();
    ids.dedup();
    report.tiles = ids.len() as u64;
    // few enough at a time that their cells, uncompressed, are not all held
    for ids in ids.chunks(64 * threads.max(1)) {
        control.check()?;
        let tiles: Vec<(usize, Vec<u8>)> = ids
            .par_iter()
            .map(|id| {
                let trees = sink.tiles.get(id).map_or(&[][..], Vec::as_slice);
                let lines = sink.lines.get(id).map_or(&[][..], Vec::as_slice);
                let raw = encode_tile(trees, sink.stands.get(id), lines);
                let mut gz = GzEncoder::new(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
                gz.write_all(&raw)
                    .and_then(|()| gz.finish())
                    .map(|bytes| (raw.len(), bytes))
                    .map_err(|e| format!("Failed to compress a tile: {e}"))
            })
            .collect::<Result<_, String>>()?;
        for (id, (raw, bytes)) in ids.iter().zip(tiles) {
            let trees = sink.tiles.get(id).map_or(&[][..], Vec::as_slice);
            let count = |of: fn(&Tree) -> bool| trees.iter().filter(|t| of(t)).count() as u64;
            report.trees += trees.len() as u64;
            report.with_kind += count(|t| t.kind != 0 || t.genus != 0);
            report.with_height += count(|t| t.height != 0);
            report.raw_bytes += raw as u64;
            report.gz_bytes += bytes.len() as u64;
            let coord = TileCoord::from(TileId::new(*id).map_err(failed)?);
            writer.add_tile(coord, &bytes).map_err(failed)?;
        }
    }
    writer.finalize().map_err(failed)?;
    partial.persist(output)?;
    report.seconds = start.elapsed().as_secs_f64();
    Ok(report)
}

/// A directory beside the archive for as long as it is written.
struct Temp(PathBuf);

impl Temp {
    fn create(output: &Path) -> Result<Self, String> {
        let mut name = output.as_os_str().to_owned();
        name.push(format!(".{}.tmp", std::process::id()));
        let path = PathBuf::from(name);
        std::fs::create_dir_all(&path)
            .map_err(|e| format!("Failed to create {}: {e}", path.display()))?;
        Ok(Self(path))
    }
}

impl Drop for Temp {
    fn drop(&mut self) {
        // best effort, as the compile's
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Trees by their tile's PMTiles ID.
struct Trees {
    tiles: BTreeMap<u64, Vec<Tree>>,
    stands: BTreeMap<u64, Painted>,
    lines: BTreeMap<u64, Vec<Line>>,
    /// The mapped trees, in the stands' units.
    places: Vec<[i32; 2]>,
    bounds: [f64; 4],
}

impl Default for Trees {
    fn default() -> Self {
        Self {
            tiles: BTreeMap::new(),
            stands: BTreeMap::new(),
            lines: BTreeMap::new(),
            places: Vec::new(),
            bounds: [180.0, 90.0, -180.0, -90.0],
        }
    }
}

impl Pass2Sink for Trees {
    fn on_relation_batch(&mut self, _: RelationBatch) -> Result<(), String> {
        Ok(())
    }

    fn on_way_batch(&mut self, _: WayBatch) -> Result<(), String> {
        Ok(())
    }

    fn on_node_batch(&mut self, batch: NodeBatch) -> Result<(), String> {
        for node in &batch.nodes {
            self.node(node)?;
        }
        Ok(())
    }
}

impl Trees {
    fn node(&mut self, node: &NormalizedNode) -> Result<(), String> {
        if node.tags.get("natural") != Some("tree") || node.lat.abs() > MAX_LATITUDE {
            return Ok(());
        }
        self.places.push(stands::unit(node.lng, node.lat));
        self.push(node.lng, node.lat, tagged(&node.tags, 0))
    }

    /// `tree` at a place.
    fn push(&mut self, longitude: f64, latitude: f64, tree: Tree) -> Result<(), String> {
        let ((tx, ty), (x, y)) = place(longitude, latitude);
        let coord = TileCoord::new(ZOOM, tx, ty)
            .map_err(|e| format!("{longitude} {latitude} is in no tile: {e}"))?;
        self.tiles
            .entry(TileId::from(coord).value())
            .or_default()
            .push(Tree { x, y, ..tree });
        self.hold([longitude, latitude, longitude, latitude]);
        Ok(())
    }

    fn hold(&mut self, b: [f64; 4]) {
        let a = &mut self.bounds;
        *a = [
            a[0].min(b[0]),
            a[1].min(b[1]),
            a[2].max(b[2]),
            a[3].max(b[3]),
        ];
    }

    /// Holds a tile in the bounds.
    fn reach(&mut self, x: u32, y: u32) {
        let n = f64::from(1u32 << ZOOM);
        let lon = |x: u32| f64::from(x) / n * 360.0 - 180.0;
        let lat = |y: u32| latitude(f64::from(y) / n);
        self.hold([lon(x), lat(y + 1), lon(x + 1), lat(y)]);
    }

    /// A row's or a hedge's line, in each tile it crosses.
    fn line(&mut self, row: &Row) -> Result<(), String> {
        let t = tagged(&row.tags, 0);
        let form = if row.hedge { form(&row.tags) } else { 0 };
        for ((x, y), points) in clip(&row.line) {
            let coord = TileCoord::new(ZOOM, x, y).map_err(|e| format!("No tile {x} {y}: {e}"))?;
            self.lines
                .entry(TileId::from(coord).value())
                .or_default()
                .push(Line {
                    what: if row.hedge { 1 } else { 2 },
                    height: t.height,
                    kind: t.kind,
                    genus: t.genus,
                    form,
                    points,
                });
            self.reach(x, y);
        }
        Ok(())
    }
}

/// A line's metres.
fn length(line: &[[i32; 2]]) -> f64 {
    line.windows(2)
        .map(|e| {
            let d = f64::from(e[1][0] - e[0][0]).hypot(f64::from(e[1][1] - e[0][1]));
            d / stands::units(1.0, e[0][1] + (e[1][1] - e[0][1]) / 2)
        })
        .sum()
}

/// A hedge's form, where its tags say: clipped, or grown freely.
fn form(tags: &osm_pbf_ingest::Tags) -> u8 {
    match tags.get("hedge").or(tags.get("shape")) {
        Some("clipped" | "trimmed" | "cut" | "box" | "topiary") => 1,
        Some("hedge_bank" | "hedgerow" | "natural" | "untrimmed" | "unclipped" | "wild") => 2,
        _ => 0,
    }
}

/// A line's piece in a z14 tile: the tile's `(x, y)` and its points in the tile's 65536ths.
type Piece = ((u32, u32), Vec<[u16; 2]>);

/// A line's pieces in the z14 tiles it crosses.
fn clip(line: &[[i32; 2]]) -> Vec<Piece> {
    let side = f64::from(stands::TILE);
    // by their tiles, in units
    let mut pieces: Vec<(_, Vec<[f64; 2]>)> = Vec::new();
    for e in line.windows(2) {
        let [a, b] = [e[0], e[1]].map(|p| p.map(f64::from));
        // where it crosses a tile's edge
        let mut ts = vec![0.0, 1.0];
        for c in 0..2 {
            let (lo, hi) = (a[c].min(b[c]), a[c].max(b[c]));
            let mut k = (lo / side).floor() + 1.0;
            while k * side < hi {
                ts.push((k * side - a[c]) / (b[c] - a[c]));
                k += 1.0;
            }
        }
        ts.sort_by(f64::total_cmp);
        let at = |t: f64| [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
        for w in ts.windows(2) {
            if w[1] <= w[0] {
                continue;
            }
            let m = at((w[0] + w[1]) * 0.5);
            let tile = ((m[0] / side).floor() as i64, (m[1] / side).floor() as i64);
            let (p, q) = (at(w[0]), at(w[1]));
            match pieces.last_mut() {
                Some((t, points)) if *t == tile && points.last() == Some(&p) => points.push(q),
                _ => pieces.push((tile, vec![p, q])),
            }
        }
    }
    let tiles = 1i64 << ZOOM;
    pieces
        .into_iter()
        .filter(|((x, y), _)| (0..tiles).contains(x) && (0..tiles).contains(y))
        .filter_map(|((x, y), points)| {
            let origin = [x as f64 * side, y as f64 * side];
            let mut out: Vec<[u16; 2]> = points
                .iter()
                .map(|p| {
                    [0, 1].map(|c| {
                        ((p[c] - origin[c]) / side * 65536.0)
                            .round()
                            .clamp(0.0, 65535.0) as u16
                    })
                })
                .collect();
            out.dedup();
            (out.len() > 1).then_some(((x as u32, y as u32), out))
        })
        .collect()
}

/// What a tree's, or its row's, tags say of it.
fn tagged(tags: &osm_pbf_ingest::Tags, source: u8) -> Tree {
    let tag = |key: &str| tags.get(key);
    Tree {
        x: 0,
        y: 0,
        height: tag("height").or(tag("est_height")).map_or(0, decimetres),
        crown: tag("diameter_crown")
            .map_or(0, decimetres)
            .min(u16::from(u8::MAX)) as u8,
        kind: kind(tag("leaf_type"), tag("leaf_cycle")),
        genus: tag("genus")
            .or(tag("species"))
            .or(tag("taxon"))
            .map_or(0, genus),
        source,
    }
}

/// The latitude of a share of the world's height.
pub(crate) fn latitude(v: f64) -> f64 {
    (std::f64::consts::PI * (1.0 - 2.0 * v))
        .sinh()
        .atan()
        .to_degrees()
}

/// The z14 tile a place is in, and where in it.
fn place(longitude: f64, latitude: f64) -> ((u32, u32), (u16, u16)) {
    let n = f64::from(1u32 << ZOOM);
    let sin = latitude.to_radians().sin();
    let u = (longitude + 180.0) / 360.0 * n;
    let v = (0.5 - ((1.0 + sin) / (1.0 - sin)).ln() / (4.0 * std::f64::consts::PI)) * n;
    let split = |c: f64| {
        let c = c.clamp(0.0, n - 1e-9);
        (c.floor() as u32, ((c - c.floor()) * 65536.0) as u16)
    };
    let ((tx, x), (ty, y)) = (split(u), split(v));
    ((tx, ty), (x, y))
}

/// A length's tag in decimetres: metres, with or without the unit. 0 where it is in another
/// unit, is not a number or could not be a tree's.
pub(crate) fn decimetres(text: &str) -> u16 {
    let text = text.trim();
    let text = text.strip_suffix('m').unwrap_or(text).trim_end();
    match text.replace(',', ".").parse::<f64>() {
        Ok(m) if m > 0.0 && m <= 130.0 => (m * 10.0).round().max(1.0) as u16,
        _ => 0,
    }
}

pub(crate) fn kind(leaf_type: Option<&str>, leaf_cycle: Option<&str>) -> u8 {
    let leaf = match leaf_type {
        Some("broadleaved") => 1,
        Some("needleleaved") => 2,
        Some("palm") => 3,
        Some("mixed") => 4,
        Some("leafless") => 5,
        _ => 0,
    };
    let cycle = match leaf_cycle {
        Some("deciduous") => 1,
        Some("evergreen") => 2,
        Some("semi_deciduous" | "semi_evergreen" | "mixed") => 3,
        _ => 0,
    };
    leaf | cycle << 3
}

/// A genus' number, from its name or a species' or taxon's, whose first word it is.
pub(crate) fn genus(name: &str) -> u8 {
    let first = name.split_whitespace().next().unwrap_or("").to_lowercase();
    TREE_GENERA
        .iter()
        .position(|g| *g == first)
        .map_or(0, |i| i as u8 + 1)
}

fn encode_tile(trees: &[Tree], stands: Option<&Painted>, lines: &[Line]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + trees.len() * 10);
    out.extend((trees.len() as u32).to_le_bytes());
    for column in [|t: &Tree| t.x, |t: &Tree| t.y, |t: &Tree| t.height] {
        out.extend(trees.iter().flat_map(|t| column(t).to_le_bytes()));
    }
    for column in [
        |t: &Tree| t.crown,
        |t: &Tree| t.kind,
        |t: &Tree| t.genus,
        |t: &Tree| t.source,
    ] {
        out.extend(trees.iter().map(column));
    }
    match stands {
        Some(s) => s.encode(&mut out),
        None => out.push(0),
    }
    if !lines.is_empty() {
        out.extend((lines.len() as u32).to_le_bytes());
        for column in [|l: &Line| l.points.len() as u16, |l: &Line| l.height] {
            out.extend(lines.iter().flat_map(|l| column(l).to_le_bytes()));
        }
        for column in [
            |l: &Line| l.what,
            |l: &Line| l.kind,
            |l: &Line| l.genus,
            |l: &Line| l.form,
        ] {
            out.extend(lines.iter().map(column));
        }
        for c in 0..2 {
            out.extend(
                lines
                    .iter()
                    .flat_map(|l| l.points.iter().flat_map(move |p| p[c].to_le_bytes())),
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn metadata_names_the_format_and_version() {
        let m: serde_json::Value = serde_json::from_str(METADATA).unwrap();
        assert_eq!(m["format"], TREES_FORMAT);
        assert_eq!(m["version"], TREES_VERSION);
    }

    #[test]
    fn genera_are_told_by_their_first_word() {
        let distinct: HashSet<_> = TREE_GENERA.iter().collect();
        assert_eq!(distinct.len(), TREE_GENERA.len());
        assert_eq!(genus("Tilia"), genus("Tilia cordata 'Greenspire'"));
        assert_eq!(TREE_GENERA[usize::from(genus("tilia")) - 1], "tilia");
        assert_eq!(genus("klon"), 0);
        assert_eq!(genus(""), 0);
    }

    #[test]
    fn lengths_are_metres_or_nothing() {
        assert_eq!(decimetres("12"), 120);
        assert_eq!(decimetres("12.5 m"), 125);
        assert_eq!(decimetres("7,5m"), 75);
        assert_eq!(decimetres("0.02"), 1);
        for none in ["", "0", "-3", "40 ft", "40'", "tall", "900", "NaN"] {
            assert_eq!(decimetres(none), 0, "{none}");
        }
    }

    #[test]
    fn kinds_pack_leaf_and_cycle() {
        assert_eq!(kind(None, None), 0);
        assert_eq!(kind(Some("needleleaved"), Some("evergreen")), 2 | 2 << 3);
        assert_eq!(kind(Some("broadleaved"), None), 1);
        assert_eq!(kind(Some("odd"), Some("deciduous")), 1 << 3);
    }

    #[test]
    fn places_fall_in_their_tiles() {
        // Krakow's main square
        let ((tx, ty), (x, y)) = place(19.937_3, 50.061_7);
        assert_eq!((tx, ty), (9099, 5552));
        let lon = (f64::from(tx) + (f64::from(x) + 0.5) / 65536.0) / 16384.0 * 360.0 - 180.0;
        assert!((lon - 19.937_3).abs() < 1e-6);
        let v = (f64::from(ty) + (f64::from(y) + 0.5) / 65536.0) / 16384.0;
        let lat = (std::f64::consts::PI * (1.0 - 2.0 * v))
            .sinh()
            .atan()
            .to_degrees();
        assert!((lat - 50.061_7).abs() < 1e-6);
        assert_eq!(place(180.0, 0.0).0.0, 16383);
    }

    #[test]
    fn tiles_are_a_count_and_columns() {
        let t = |x: u16| Tree {
            x,
            y: 2,
            height: 0x0304,
            crown: 5,
            kind: 6,
            genus: 7,
            source: 0,
        };
        let bytes = encode_tile(&[t(0x0101), t(0x0202)], None, &[]);
        assert_eq!(
            bytes,
            [
                2, 0, 0, 0, 1, 1, 2, 2, 2, 0, 2, 0, 4, 3, 4, 3, 5, 5, 6, 6, 7, 7, 0, 0, 0
            ]
        );
    }

    #[test]
    fn lines_follow_the_stands_as_columns_then_points() {
        let hedge = Line {
            what: 1,
            height: 0x0203,
            kind: 4,
            genus: 5,
            form: 2,
            points: vec![[1, 2], [0x0304, 0x0506], [7, 8]],
        };
        let row = Line {
            what: 2,
            height: 0,
            kind: 0,
            genus: 0,
            form: 0,
            points: vec![[9, 10], [11, 12]],
        };
        let bytes = encode_tile(&[], None, &[hedge, row]);
        assert_eq!(
            bytes,
            [
                0, 0, 0, 0, 0, 2, 0, 0, 0, 3, 0, 2, 0, 3, 2, 0, 0, 1, 2, 4, 0, 5, 0, 2, 0, 1, 0, 4,
                3, 7, 0, 9, 0, 11, 0, 2, 0, 6, 5, 8, 0, 10, 0, 12, 0
            ]
        );
        // a tile without lines is as version 1's
        assert_eq!(encode_tile(&[], None, &[]), [0, 0, 0, 0, 0]);
    }

    #[test]
    fn lines_are_cut_at_their_tiles_edges() {
        let side = stands::TILE;
        let (x, y) = (9099 * side, 5552 * side);
        // from a quarter into one tile to a quarter into the next east, and on south into a third
        let line = [
            [x + side / 4, y + side / 2],
            [x + side + side / 4, y + side / 2],
            [x + side + side / 4, y + side + side / 8],
        ];
        let pieces = clip(&line);
        assert_eq!(
            pieces,
            [
                ((9099, 5552), vec![[16384, 32768], [65535, 32768]]),
                (
                    (9100, 5552),
                    vec![[0, 32768], [16384, 32768], [16384, 65535]]
                ),
                ((9100, 5553), vec![[16384, 0], [16384, 8192]]),
            ]
        );
        // along an edge, a line is in one of its tiles alone
        let along = clip(&[[x, y + 10], [x, y + 100]]);
        assert_eq!(along.len(), 1);
        assert!(clip(&[[x + 5, y + 5], [x + 5, y + 5]]).is_empty());
    }

    #[test]
    fn hedges_say_how_they_are_kept_and_rows_their_trees() {
        let mut tags = osm_pbf_ingest::Tags::new();
        assert_eq!(form(&tags), 0);
        tags.insert("hedge", "hedge_bank");
        assert_eq!(form(&tags), 2);
        let mut t = Trees::default();
        tags.insert("height", "1.5");
        tags.insert("leaf_type", "needleleaved");
        let a = stands::unit(19.937_3, 50.061_7);
        let line: std::sync::Arc<[[i32; 2]]> = std::sync::Arc::from([a, [a[0] + 400, a[1]]]);
        assert!((length(&line) - 400.0 / stands::units(1.0, a[1])).abs() < 1e-6);
        t.line(&Row {
            tags,
            line: line.clone(),
            hedge: true,
        })
        .unwrap();
        t.line(&Row {
            tags: osm_pbf_ingest::Tags::new(),
            line,
            hedge: false,
        })
        .unwrap();
        let lines: Vec<&Line> = t.lines.values().flatten().collect();
        assert_eq!(lines.len(), 2);
        assert_eq!(
            (lines[0].what, lines[0].height, lines[0].kind, lines[0].form),
            (1, 15, 2, 2)
        );
        assert_eq!((lines[1].what, lines[1].height, lines[1].form), (2, 0, 0));
        assert!(t.tiles.is_empty(), "no trees of rows");
    }

    /// Monaco's fixture: every tree once, and the same bytes for any number of threads.
    #[test]
    fn writes_monacos_trees() {
        let input = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/monaco.osm.pbf");
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.pmtiles"), dir.path().join("b.pmtiles"));
        let report = write_trees(&input, &a, 1, || false).unwrap();
        let again = write_trees(&input, &b, 4, || false).unwrap();
        assert!(report.trees > 100, "{report:?}");
        // its hedges and its row, as lines
        assert!(report.hedges > 0 && report.rows > 0, "{report:?}");
        assert_eq!(report.trees, again.trees);
        assert_eq!(
            (report.rows, report.hedges, report.stand_tiles),
            (again.rows, again.hedges, again.stand_tiles)
        );
        assert!(
            report.stand_tiles > 0 && report.stand_km2 > 0.0,
            "{report:?}"
        );
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        assert!(matches!(
            write_trees(&input, &a, 1, || false),
            Err(CompileError::InvalidRequest(_))
        ));
    }

    #[test]
    fn a_cancelled_write_leaves_nothing() {
        let input = Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/monaco.osm.pbf");
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path().join("a.pmtiles");
        assert!(write_trees(&input, &out, 1, || true).is_err());
        assert_eq!(std::fs::read_dir(dir.path()).unwrap().count(), 0);
    }
}
