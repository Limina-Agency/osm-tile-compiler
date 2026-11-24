//! End-to-end test of `--tile-format mlt`: Monaco compiled to MLT holds, tile
//! for tile and feature for feature, what Monaco compiled to MVT does, and
//! the archive declares MLT.

use osm_tile_compiler::archive::{ArchiveReader, inspect};
use osm_tile_compiler::{Area, BasemapRequest, CompileBounds, TileFormat, compile_basemap};
use std::collections::BTreeMap;
use std::io::Read;
use std::path::{Path, PathBuf};

mod common;
#[allow(dead_code)]
#[path = "../src/mlt/decode.rs"]
mod decode;

use common::{monaco_pbf, synthetic_sources};

fn pbf_bounds(path: &Path) -> CompileBounds {
    let index = osm_pbf_ingest::blob_index::PbfBlobIndex::scan(path).unwrap();
    let b = index.header().bounds.unwrap();
    CompileBounds::new(b.north, b.south, b.east, b.west)
}

fn compile(dir: &Path, sources: &Path, format: TileFormat, file: &str) -> PathBuf {
    compile_with(dir, sources, format, file, |_| {})
}

fn compile_with(
    dir: &Path,
    sources: &Path,
    format: TileFormat,
    file: &str,
    edit: impl FnOnce(&mut BasemapRequest),
) -> PathBuf {
    let output = dir.join(file);
    let mut request = BasemapRequest::new(monaco_pbf(), &output, sources, dir.join("temp"));
    request.area = Area::bounds(pbf_bounds(&monaco_pbf()));
    request.threads = 2;
    request.archive.tile_format = format;
    edit(&mut request);
    compile_basemap(&request, |_| {}, || false).unwrap();
    output
}

/// Every tile of the archive, by tile id, gunzipped.
fn tiles(path: &Path) -> (u8, BTreeMap<u64, Vec<u8>>) {
    let mut reader = ArchiveReader::open(path).unwrap();
    let tile_type = reader.header.tile_type;
    let mut tiles = BTreeMap::new();
    reader
        .visit_entries(|reader, entry| {
            let stored = reader.tile(&entry)?;
            let mut tile = Vec::new();
            flate2::read::GzDecoder::new(&stored[..])
                .read_to_end(&mut tile)
                .map_err(|e| e.to_string())?;
            for i in 0..u64::from(entry.run_length) {
                tiles.insert(entry.tile_id + i, tile.clone());
            }
            Ok(())
        })
        .unwrap();
    (tile_type, tiles)
}

#[test]
fn monaco_in_mlt_holds_the_mvt_features() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let mvt = compile(temp.path(), &sources, TileFormat::Mvt, "monaco.pmtiles");
    let mlt = compile(temp.path(), &sources, TileFormat::Mlt, "monaco-mlt.pmtiles");

    let (mvt_type, mvt_tiles) = tiles(&mvt);
    let (mlt_type, mlt_tiles) = tiles(&mlt);
    assert_eq!((mvt_type, mlt_type), (1, 6));
    assert_eq!(
        mvt_tiles.keys().collect::<Vec<_>>(),
        mlt_tiles.keys().collect::<Vec<_>>()
    );
    let mut features = 0;
    for (id, mvt) in &mvt_tiles {
        let mlt = &mlt_tiles[id];
        decode::same_as_mvt(mlt, mvt).unwrap_or_else(|e| panic!("tile {id}: {e}"));
        features += decode::decode(mlt)
            .unwrap()
            .iter()
            .map(|layer| layer.features.len())
            .sum::<usize>();
    }
    assert!(features > 5_000, "{features} features");
    let (mvt_bytes, mlt_bytes) = (
        std::fs::metadata(&mvt).unwrap().len(),
        std::fs::metadata(&mlt).unwrap().len(),
    );
    eprintln!("Monaco: MVT {mvt_bytes} bytes, MLT {mlt_bytes} bytes, {features} features");

    // verify decodes every MLT tile, and finds the layers the metadata lists.
    let report = inspect(&mlt).unwrap();
    assert_eq!(report.problems, Vec::<String>::new());
    assert_eq!(report.tile_type, "mlt");
    assert_eq!(report.metadata["format"], "mlt");
    let mvt_report = inspect(&mvt).unwrap();
    // Each zoom has the same layers in the same number of tiles.
    let layers = |report: &osm_tile_compiler::archive::ArchiveReport| {
        report
            .tile_stats
            .zooms
            .iter()
            .map(|zoom| {
                let tiles = zoom
                    .layers
                    .iter()
                    .map(|(name, layer)| (name.clone(), layer.tiles));
                (zoom.zoom, zoom.tiles, tiles.collect::<Vec<_>>())
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(layers(&report), layers(&mvt_report));
}

#[test]
fn mlt_mbtiles_declare_their_format() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let output = compile(temp.path(), &sources, TileFormat::Mlt, "monaco.mbtiles");
    let db = rusqlite::Connection::open(&output).unwrap();
    let format: String = db
        .query_row(
            "SELECT value FROM metadata WHERE name = 'format'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(format, "mlt");
    let tile: Vec<u8> = db
        .query_row(
            "SELECT tile_data FROM tiles WHERE zoom_level = 14 LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut raw = Vec::new();
    flate2::read::GzDecoder::new(&tile[..])
        .read_to_end(&mut raw)
        .unwrap();
    assert!(!decode::decode(&raw).unwrap().is_empty());
}

/// The search index and the building levels read an MLT basemap as the MVT
/// one it was transcoded from: both come out byte for byte the same, but for
/// the index naming its source archive.
#[test]
fn search_index_and_building_levels_read_mlt_basemaps() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let both = |request: &mut BasemapRequest| {
        request.building_levels = true;
        request.search_index = true;
    };
    let mvt = compile_with(temp.path(), &sources, TileFormat::Mvt, "a.pmtiles", both);
    let mlt = compile_with(temp.path(), &sources, TileFormat::Mlt, "b.pmtiles", both);

    let levels = |archive: &Path| std::fs::read(osm_tile_compiler::building_levels_path(archive));
    let (mvt_levels, mlt_levels) = (levels(&mvt).unwrap(), levels(&mlt).unwrap());
    assert!(mvt_levels.len() > 1_000);
    assert!(mvt_levels == mlt_levels, "the building levels differ");

    let index = |archive: &Path| std::fs::read(osm_tile_compiler::search_index_path(archive));
    let (mvt_index, mut mlt_index) = (index(&mvt).unwrap(), index(&mlt).unwrap());
    // The metadata names the source, a.pmtiles or b.pmtiles: same length.
    let at = mlt_index
        .windows(9)
        .position(|w| w == b"b.pmtiles")
        .expect("the index names its source");
    mlt_index[at] = b'a';
    assert!(mvt_index.len() > 1_000);
    assert!(mvt_index == mlt_index, "the search indexes differ");

    let found = osm_tile_compiler::SearchIndex::open(&osm_tile_compiler::search_index_path(&mlt))
        .unwrap()
        .search(&osm_tile_compiler::SearchQuery::new("Monaco"))
        .unwrap();
    assert!(!found.is_empty());
}

/// The tile id's z, x and y.
fn zxy(id: u64) -> (u8, u32, u32) {
    let coord = pmtiles::TileCoord::from(pmtiles::TileId::new(id).unwrap());
    (coord.z(), coord.x(), coord.y())
}

/// Writes the conformance corpus for `scripts/check-mlt-conformance.sh`
/// into `MLT_CONFORMANCE_DIR`: every tile of Monaco compiled to MLT (or of
/// the MLT archive `MLT_CONFORMANCE_ARCHIVE`) as `<z>-<x>-<y>.mlt`, beside the
/// `.json` it must decode to, in the specification's fixture format. The
/// reference decoders print each tile in that format, and
/// `reference_decoders_agree` compares them.
#[test]
#[ignore = "run by scripts/check-mlt-conformance.sh"]
fn write_conformance_corpus() {
    let out = PathBuf::from(std::env::var_os("MLT_CONFORMANCE_DIR").expect("MLT_CONFORMANCE_DIR"));
    std::fs::create_dir_all(&out).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let archive = match std::env::var_os("MLT_CONFORMANCE_ARCHIVE") {
        Some(archive) => PathBuf::from(archive),
        None => {
            let sources = temp.path().join("sources");
            synthetic_sources(&sources);
            compile(temp.path(), &sources, TileFormat::Mlt, "monaco.pmtiles")
        }
    };
    let (tile_type, tiles) = tiles(&archive);
    assert_eq!(tile_type, 6, "{} is not an MLT archive", archive.display());
    let mut features = 0;
    for (id, tile) in &tiles {
        let (z, x, y) = zxy(*id);
        let name = format!("{z}-{x}-{y}");
        let layers = decode::decode(tile).unwrap();
        features += layers.iter().map(|l| l.features.len()).sum::<usize>();
        let json = serde_json::json!({
            "type": "FeatureCollection",
            "features": decode::json::geojson(&layers),
        });
        std::fs::write(out.join(format!("{name}.mlt")), tile).unwrap();
        std::fs::write(out.join(format!("{name}.json")), json.to_string()).unwrap();
    }
    eprintln!(
        "{} tiles, {features} features in {}",
        tiles.len(),
        out.display()
    );
}

/// Compares what each reference decoder printed for the corpus tiles,
/// `<tile>.<decoder>.json`, with what the tile must decode to.
#[test]
#[ignore = "run by scripts/check-mlt-conformance.sh"]
fn reference_decoders_agree() {
    use serde_json::Value as Json;
    let dir = PathBuf::from(std::env::var_os("MLT_CONFORMANCE_DIR").expect("MLT_CONFORMANCE_DIR"));
    let decoders: Vec<String> = std::env::var("MLT_CONFORMANCE_DECODERS")
        .unwrap_or_else(|_| "rust ts cpp".into())
        .split_whitespace()
        .map(String::from)
        .collect();
    let mut names: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| {
            let name = entry.unwrap().file_name().into_string().unwrap();
            name.strip_suffix(".mlt").map(String::from)
        })
        .collect();
    names.sort();
    assert!(!names.is_empty(), "no tiles in {}", dir.display());
    let read = |path: PathBuf| -> Json {
        let text =
            std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
    };
    let mut failures = Vec::new();
    for decoder in &decoders {
        let (mut tiles, mut features) = (0, 0);
        for name in &names {
            let expected = read(dir.join(format!("{name}.json")));
            let actual = read(dir.join(format!("{name}.{decoder}.json")));
            let (expected, actual) = (&expected["features"], &actual["features"]);
            let (Some(expected), Some(actual)) = (expected.as_array(), actual.as_array()) else {
                failures.push(format!("{decoder} {name}: not a feature collection"));
                continue;
            };
            tiles += 1;
            features += expected.len();
            if expected.len() != actual.len() {
                failures.push(format!(
                    "{decoder} {name}: {} features for {}",
                    actual.len(),
                    expected.len()
                ));
                continue;
            }
            if let Some(i) =
                (0..expected.len()).find(|&i| !decode::json::same(&expected[i], &actual[i]))
            {
                failures.push(format!(
                    "{decoder} {name} feature #{i}:\n  expected {}\n  actual   {}",
                    expected[i], actual[i]
                ));
            }
        }
        eprintln!("{decoder}: {tiles} tiles, {features} features compared");
    }
    assert!(
        failures.is_empty(),
        "{} differences:\n{}",
        failures.len(),
        failures
            .iter()
            .take(20)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    );
}
