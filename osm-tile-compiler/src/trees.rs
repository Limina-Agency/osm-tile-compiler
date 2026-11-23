//! Trees: a PMTiles archive of the trees mapped one by one (`natural=tree` nodes) in an OSM
//! extract, for renderers that stand a model at each. The OpenMapTiles schema has none.
//!
//! The archive is PMTiles v3 with tile type unknown, gzipped per tile, z14 only. Each tile is
//! a little-endian `u32` count and then columns of that many values: `x`, `y` (the place in
//! the tile, in 65536ths of it from its north-west corner) and `height` (decimetres) as
//! `u16`, then `crown` (its width, decimetres), `kind`, `genus` and `source` as `u8`, 10 bytes
//! a tree. 0 is unknown in `height`, `crown`, `kind`'s fields and `genus`. Trees keep the
//! extract's order, which is by node ID in the extracts everyone publishes. A tile with no
//! trees is not written.
//!
//! Nothing is guessed: a tree whose tags say nothing is a place alone, and a renderer decides
//! what stands there.

use crate::control::{CompileError, Control};
use crate::omt::PartialArchive;
use flate2::write::GzEncoder;
use osm_pbf_ingest::{
    IngestBuildProfile, NodeBatch, NormalizedNode, Pass2Options, Pass2Sink, RelationBatch,
    WayBatch, run_pass2_with_options,
};
use pmtiles::{Compression, PmTilesWriter, TileCoord, TileId, TileType};
use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// The tree archives' metadata `format`.
pub const TREES_FORMAT: &str = "trees";
/// Their metadata `version`. See `docs/formats.md` for when it changes.
pub const TREES_VERSION: u32 = 1;
/// Spelled out so the bytes stay fixed; a test below checks it against the
/// two constants above.
const METADATA: &str =
    r#"{"format":"trees","version":1,"attribution":"© OpenStreetMap contributors"}"#;
const ZOOM: u8 = 14;
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
    /// 0: mapped as a node.
    source: u8,
}

/// What [`write_trees`] wrote.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct TreesReport {
    pub tiles: u64,
    pub trees: u64,
    /// Trees whose tags give a leaf type, or a genus.
    pub with_kind: u64,
    /// Trees whose tags give a height.
    pub with_height: u64,
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

/// Reads the mapped trees of the OSM extract `input` and writes them to the new archive
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
    let mut options = Pass2Options::new().keep_ways(|_| false);
    options.worker_threads = Some(threads);
    options.skip_relations = true;
    run_pass2_with_options(
        input,
        IngestBuildProfile::DesktopFull,
        options,
        &mut sink,
        |_| {},
        || control.check().is_err(),
    )?;
    control.check()?;
    if sink.tiles.is_empty() {
        return Err(format!("{} has no natural=tree nodes", input.display()));
    }
    let mut report = TreesReport {
        tiles: sink.tiles.len() as u64,
        trees: 0,
        with_kind: 0,
        with_height: 0,
        raw_bytes: 0,
        gz_bytes: 0,
        seconds: 0.0,
    };
    let failed = |e: pmtiles::PmtError| format!("Failed to write {}: {e}", output.display());
    let partial = PartialArchive::create(output)?;
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
    for (id, trees) in &sink.tiles {
        control.check()?;
        let raw = encode_tile(trees);
        let mut gz = GzEncoder::new(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
        let bytes = gz
            .write_all(&raw)
            .and_then(|()| gz.finish())
            .map_err(|e| format!("Failed to compress a tile: {e}"))?;
        report.trees += trees.len() as u64;
        report.with_kind += trees.iter().filter(|t| t.kind != 0 || t.genus != 0).count() as u64;
        report.with_height += trees.iter().filter(|t| t.height != 0).count() as u64;
        report.raw_bytes += raw.len() as u64;
        report.gz_bytes += bytes.len() as u64;
        let coord = TileCoord::from(TileId::new(*id).map_err(failed)?);
        writer.add_tile(coord, &bytes).map_err(failed)?;
    }
    writer.finalize().map_err(failed)?;
    partial.persist(output)?;
    report.seconds = start.elapsed().as_secs_f64();
    Ok(report)
}

/// Trees by their tile's PMTiles ID.
struct Trees {
    tiles: BTreeMap<u64, Vec<Tree>>,
    bounds: [f64; 4],
}

impl Default for Trees {
    fn default() -> Self {
        Self {
            tiles: BTreeMap::new(),
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
        let ((tx, ty), (x, y)) = place(node.lng, node.lat);
        let coord = TileCoord::new(ZOOM, tx, ty)
            .map_err(|e| format!("Node {} is in no tile: {e}", node.node_id))?;
        let tag = |key: &str| node.tags.get(key);
        self.tiles
            .entry(TileId::from(coord).value())
            .or_default()
            .push(Tree {
                x,
                y,
                height: tag("height").or(tag("est_height")).map_or(0, decimetres),
                crown: tag("diameter_crown")
                    .map_or(0, decimetres)
                    .min(u16::from(u8::MAX)) as u8,
                kind: kind(tag("leaf_type"), tag("leaf_cycle")),
                genus: tag("genus")
                    .or(tag("species"))
                    .or(tag("taxon"))
                    .map_or(0, genus),
                source: 0,
            });
        let b = &mut self.bounds;
        *b = [
            b[0].min(node.lng),
            b[1].min(node.lat),
            b[2].max(node.lng),
            b[3].max(node.lat),
        ];
        Ok(())
    }
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
fn decimetres(text: &str) -> u16 {
    let text = text.trim();
    let text = text.strip_suffix('m').unwrap_or(text).trim_end();
    match text.replace(',', ".").parse::<f64>() {
        Ok(m) if m > 0.0 && m <= 130.0 => (m * 10.0).round().max(1.0) as u16,
        _ => 0,
    }
}

fn kind(leaf_type: Option<&str>, leaf_cycle: Option<&str>) -> u8 {
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
fn genus(name: &str) -> u8 {
    let first = name.split_whitespace().next().unwrap_or("").to_lowercase();
    TREE_GENERA
        .iter()
        .position(|g| *g == first)
        .map_or(0, |i| i as u8 + 1)
}

fn encode_tile(trees: &[Tree]) -> Vec<u8> {
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
        let bytes = encode_tile(&[t(0x0101), t(0x0202)]);
        assert_eq!(
            bytes,
            [
                2, 0, 0, 0, 1, 1, 2, 2, 2, 0, 2, 0, 4, 3, 4, 3, 5, 5, 6, 6, 7, 7, 0, 0
            ]
        );
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
        assert_eq!(report.trees, again.trees);
        assert_eq!(report.raw_bytes, report.trees * 10 + report.tiles * 4);
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
