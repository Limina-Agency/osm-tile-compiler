//! Compiles `fixtures/monaco.osm.pbf` into MBTiles and PMTiles and checks
//! that the MBTiles database holds the same tiles and metadata.

use osm_tile_compiler::{Area, BasemapRequest, CompileError, OutputFormat, compile_basemap};
mod common;

use common::{monaco_pbf, synthetic_sources};
use pmtiles::{AsyncPmTilesReader, TileCoord};
use rusqlite::Connection;
use serde_json::Value;
use std::collections::BTreeMap;
use std::path::Path;

fn request(sources: &Path, output: &Path) -> BasemapRequest {
    let mut request =
        BasemapRequest::new(monaco_pbf(), output, sources, output.with_extension("tmp"));
    request.area = Area::bounds(osm_tile_compiler::pbf_bounds(&monaco_pbf()).unwrap());
    request.threads = 2;
    request
}

#[tokio::test(flavor = "multi_thread")]
async fn mbtiles_holds_the_tiles_and_metadata_of_the_pmtiles_archive() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let pmtiles_path = temp.path().join("monaco.pmtiles");
    let mbtiles_path = temp.path().join("out/monaco.mbtiles");

    let pmtiles = compile_basemap(&request(&sources, &pmtiles_path), |_| {}, || false).unwrap();
    assert_eq!(pmtiles.format, OutputFormat::PmTiles);
    let mut stages = Vec::new();
    let mbtiles = compile_basemap(
        &request(&sources, &mbtiles_path),
        |progress| stages.push(progress.message.clone()),
        || false,
    )
    .unwrap();
    assert_eq!(mbtiles.format, OutputFormat::MbTiles);
    assert!(
        stages
            .iter()
            .any(|m| m == "Sorting, encoding and writing MBTiles"),
        "{stages:?}"
    );
    assert_eq!(mbtiles.result.tiles, pmtiles.result.tiles);
    assert_eq!(mbtiles.result.tile_stats, pmtiles.result.tile_stats);
    // Nothing but the archive is left beside it.
    let leftovers = std::fs::read_dir(temp.path().join("out"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != "monaco.mbtiles")
        .collect::<Vec<_>>();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");

    let reader = AsyncPmTilesReader::new_with_path(&pmtiles_path)
        .await
        .unwrap();
    let db = Connection::open(&mbtiles_path).unwrap();
    let mut select = db
        .prepare("SELECT zoom_level, tile_column, tile_row, tile_data FROM tiles")
        .unwrap();
    let rows = select
        .query_map([], |row| {
            Ok((
                row.get::<_, u8>(0)?,
                row.get::<_, u32>(1)?,
                row.get::<_, u32>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(rows.len() as u64, pmtiles.result.tiles);
    for (z, x, row, data) in rows {
        let y = (1 << z) - 1 - row;
        let expected = reader
            .get_tile(TileCoord::new(z, x, y).unwrap())
            .await
            .unwrap()
            .unwrap_or_else(|| panic!("{z}/{x}/{y} is not in the PMTiles archive"));
        assert_eq!(data, expected.as_ref(), "{z}/{x}/{y} differs");
    }
    let images: i64 = db
        .query_row("SELECT count(*) FROM images", [], |r| r.get(0))
        .unwrap();
    assert!(images as u64 <= pmtiles.result.tiles);

    let metadata = db
        .prepare("SELECT name, value FROM metadata")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<BTreeMap<_, _>, _>>()
        .unwrap();
    let expected: Value = serde_json::from_str(&reader.get_metadata().await.unwrap()).unwrap();
    let json: Value = serde_json::from_str(&metadata["json"]).unwrap();
    assert_eq!(json["vector_layers"], expected["vector_layers"]);
    for key in [
        "name",
        "format",
        "description",
        "attribution",
        "version",
        "type",
    ] {
        assert_eq!(
            Some(&metadata[key]),
            expected[key].as_str().map(String::from).as_ref(),
            "{key}"
        );
    }
    let header = reader.get_header();
    assert_eq!(metadata["minzoom"], header.min_zoom.to_string());
    assert_eq!(metadata["maxzoom"], header.max_zoom.to_string());
    let bounds = metadata["bounds"]
        .split(',')
        .map(|v| v.parse::<f64>().unwrap())
        .collect::<Vec<_>>();
    let declared = [
        header.min_longitude,
        header.min_latitude,
        header.max_longitude,
        header.max_latitude,
    ];
    for (mb, pm) in bounds.iter().zip(declared) {
        assert!((mb - pm).abs() < 1e-6, "{bounds:?} vs {declared:?}");
    }
    let center = metadata["center"].split(',').collect::<Vec<_>>();
    assert_eq!(center[2], header.center_zoom.to_string());
}

#[test]
fn building_levels_need_pmtiles() {
    let temp = tempfile::tempdir().unwrap();
    let mut request = request(temp.path(), &temp.path().join("monaco.mbtiles"));
    request.building_levels = true;
    let error = request.validate().unwrap_err();
    assert!(matches!(error, CompileError::InvalidRequest(_)), "{error}");
    assert!(error.to_string().contains("PMTiles"), "{error}");
    // An explicit format wins over the extension.
    request.format = Some(OutputFormat::PmTiles);
    request.validate().unwrap();
    request.building_levels = false;
    request.output = temp.path().join("monaco.db");
    request.format = Some(OutputFormat::MbTiles);
    assert_eq!(request.output_format(), OutputFormat::MbTiles);
}
