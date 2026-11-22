//! The MBTiles 1.3 writer (<https://github.com/mapbox/mbtiles-spec>).
//!
//! Tiles go into `map` (one row per tile) and `images` (one row per distinct
//! tile), and `tiles` is the view over both that readers query, as in
//! Planetiler's compact MBTiles. Only tiles of up to [`DEDUPLICATE_BYTES`]
//! are looked up for an identical one: the repeated tiles of a basemap are
//! the small fills of sea and land, and the lookup's memory stays small.

use super::{ArchiveSpec, TileWriter};
use pmtiles::{TileCoord, TileId};
use rusqlite::{Connection, OpenFlags, params};
use rustc_hash::FxHashMap;
use serde_json::{Map, Value};
use sha2::{Digest, Sha256};
use std::path::Path;

/// Tiles up to this size are stored once however often they repeat.
const DEDUPLICATE_BYTES: usize = 1024;
/// MBTiles' `application_id`: "MPBX".
const APPLICATION_ID: i32 = 0x4d50_4258;
/// SQLite's page cache while the archive is written, in KiB.
const CACHE_KIB: i64 = 256 * 1024;

pub(crate) struct MbtilesWriter {
    connection: Connection,
    /// The image id of each small tile written, by a hash of its bytes.
    images: FxHashMap<[u8; 16], i64>,
    next_image: i64,
}

impl MbtilesWriter {
    /// Creates the database at `path`, which must not exist.
    pub fn create(path: &Path, spec: &ArchiveSpec) -> Result<Self, String> {
        let failed = |e: rusqlite::Error| format!("Failed to create {}: {e}", path.display());
        if path.exists() {
            return Err(format!("{} already exists", path.display()));
        }
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(failed)?;
        // The file is a partial archive until it is moved into place, and
        // a failed compile removes it: no journal, and no syncs, protect it.
        connection
            .execute_batch(&format!(
                "PRAGMA application_id = {APPLICATION_ID};
                 PRAGMA page_size = 8192;
                 PRAGMA journal_mode = OFF;
                 PRAGMA synchronous = OFF;
                 PRAGMA locking_mode = EXCLUSIVE;
                 PRAGMA cache_size = -{CACHE_KIB};
                 CREATE TABLE metadata (name TEXT NOT NULL, value TEXT);
                 CREATE TABLE map (
                     zoom_level INTEGER NOT NULL,
                     tile_column INTEGER NOT NULL,
                     tile_row INTEGER NOT NULL,
                     tile_id INTEGER NOT NULL,
                     PRIMARY KEY (zoom_level, tile_column, tile_row)
                 ) WITHOUT ROWID;
                 CREATE TABLE images (tile_id INTEGER PRIMARY KEY, tile_data BLOB NOT NULL);
                 CREATE VIEW tiles AS
                     SELECT map.zoom_level AS zoom_level,
                            map.tile_column AS tile_column,
                            map.tile_row AS tile_row,
                            images.tile_data AS tile_data
                     FROM map JOIN images ON images.tile_id = map.tile_id;
                 BEGIN;"
            ))
            .map_err(failed)?;
        {
            let mut insert = connection
                .prepare("INSERT INTO metadata (name, value) VALUES (?1, ?2)")
                .map_err(failed)?;
            for (name, value) in metadata_rows(spec)? {
                insert.execute(params![name, value]).map_err(failed)?;
            }
        }
        Ok(Self {
            connection,
            images: FxHashMap::default(),
            next_image: 0,
        })
    }
}

impl TileWriter for MbtilesWriter {
    fn write_tile(&mut self, tile_id: u64, bytes: &[u8]) -> Result<(), String> {
        let coord = TileCoord::from(
            TileId::new(tile_id).map_err(|error| format!("Invalid tile id {tile_id}: {error}"))?,
        );
        let failed = |e: rusqlite::Error| format!("Failed to write tile {coord:?}: {e}");
        let key = (bytes.len() <= DEDUPLICATE_BYTES).then(|| {
            let mut key = [0; 16];
            key.copy_from_slice(&Sha256::digest(bytes)[..16]);
            key
        });
        let known = key.and_then(|key| self.images.get(&key).copied());
        let image = match known {
            Some(image) => image,
            None => {
                let image = self.next_image;
                self.next_image += 1;
                self.connection
                    .prepare_cached("INSERT INTO images (tile_id, tile_data) VALUES (?1, ?2)")
                    .and_then(|mut insert| insert.execute(params![image, bytes]))
                    .map_err(failed)?;
                if let Some(key) = key {
                    self.images.insert(key, image);
                }
                image
            }
        };
        // MBTiles numbers rows from the south (TMS), PMTiles from the north.
        let z = coord.z();
        let row = (1u32 << z) - 1 - coord.y();
        self.connection
            .prepare_cached(
                "INSERT INTO map (zoom_level, tile_column, tile_row, tile_id) \
                 VALUES (?1, ?2, ?3, ?4)",
            )
            .and_then(|mut insert| insert.execute(params![z, coord.x(), row, image]))
            .map_err(failed)?;
        Ok(())
    }

    fn finalize(self: Box<Self>) -> Result<(), String> {
        let failed = |e: rusqlite::Error| format!("Failed to finalize MBTiles archive: {e}");
        self.connection.execute_batch("COMMIT;").map_err(failed)?;
        self.connection.close().map_err(|(_, error)| failed(error))
    }
}

/// The `metadata` rows: MBTiles' keys, then every other string or number
/// of the metadata object as a row of its own. `vector_layers` and anything
/// else that is neither go into the `json` row.
fn metadata_rows(spec: &ArchiveSpec) -> Result<Vec<(String, String)>, String> {
    let metadata: Map<String, Value> = serde_json::from_str(spec.metadata_json)
        .map_err(|e| format!("Invalid archive metadata: {e}"))?;
    let bounds = spec.bounds.archive_bounds();
    let center = spec.center;
    let mut rows = vec![
        (
            "bounds".to_string(),
            format!(
                "{},{},{},{}",
                bounds.west, bounds.south, bounds.east, bounds.north
            ),
        ),
        (
            "center".to_string(),
            format!("{},{},{}", center.longitude, center.latitude, center.zoom),
        ),
        ("minzoom".to_string(), spec.min_zoom.to_string()),
        ("maxzoom".to_string(), spec.max_zoom.to_string()),
    ];
    let mut json = Map::new();
    for (name, value) in metadata {
        if rows.iter().any(|(row, _)| *row == name) {
            continue;
        }
        match value {
            Value::String(text) => rows.push((name, text)),
            Value::Number(number) => rows.push((name, number.to_string())),
            Value::Bool(flag) => rows.push((name, flag.to_string())),
            Value::Null => {}
            other => {
                json.insert(name, other);
            }
        }
    }
    if !rows.iter().any(|(name, _)| name == "format") {
        rows.push(("format".into(), "pbf".into()));
    }
    rows.push(("json".into(), Value::Object(json).to_string()));
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ArchiveCenter, CompileBounds};

    fn spec<'a>(bounds: &'a CompileBounds, metadata: &'a str) -> ArchiveSpec<'a> {
        ArchiveSpec {
            bounds,
            center: ArchiveCenter {
                longitude: 20.05,
                latitude: 50.05,
                zoom: 3,
            },
            metadata_json: metadata,
            min_zoom: 0,
            max_zoom: 14,
        }
    }

    #[test]
    fn writes_tms_rows_shares_small_tiles_and_declares_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("out.mbtiles");
        let bounds = CompileBounds::new(50.1, 50.0, 20.1, 20.0);
        let metadata = r#"{"name":"OpenMapTiles","format":"pbf","version":"3.16.0",
            "vector_layers":[{"id":"water","minzoom":0,"maxzoom":14,"fields":{}}],
            "omitted_sources":["natural_earth"]}"#;
        let mut writer = Box::new(MbtilesWriter::create(&path, &spec(&bounds, metadata)).unwrap());
        let small = [7u8; 100];
        let large = vec![9u8; DEDUPLICATE_BYTES + 1];
        let ids = [
            (0, 0, 0),
            (14, 9101, 5546),
            (14, 9102, 5546),
            (14, 9103, 5546),
            (14, 9104, 5546),
        ];
        let tiles: Vec<(u64, &[u8])> = ids
            .iter()
            .enumerate()
            .map(|(i, &(z, x, y))| {
                let id = TileId::from(TileCoord::new(z, x, y).unwrap()).value();
                (id, if i < 3 { &small[..] } else { &large[..] })
            })
            .collect();
        let mut sorted = tiles.clone();
        sorted.sort();
        for (id, bytes) in &sorted {
            writer.write_tile(*id, bytes).unwrap();
        }
        writer.finalize().unwrap();

        let db = Connection::open(&path).unwrap();
        let application_id: i32 = db
            .query_row("PRAGMA application_id", [], |r| r.get(0))
            .unwrap();
        assert_eq!(application_id, APPLICATION_ID);
        for &(z, x, y) in &ids {
            let row = (1u32 << z) - 1 - y;
            let data: Vec<u8> = db
                .query_row(
                    "SELECT tile_data FROM tiles WHERE zoom_level=?1 AND tile_column=?2 AND tile_row=?3",
                    params![z, x, row],
                    |r| r.get(0),
                )
                .unwrap();
            let id = TileId::from(TileCoord::new(z, x, y).unwrap()).value();
            let expected = tiles.iter().find(|(t, _)| *t == id).unwrap().1;
            assert_eq!(data, expected);
        }
        let images: i64 = db
            .query_row("SELECT count(*) FROM images", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            images, 3,
            "three small tiles share one image; large ones don't"
        );
        let meta = |name: &str| -> String {
            db.query_row("SELECT value FROM metadata WHERE name=?1", [name], |r| {
                r.get(0)
            })
            .unwrap()
        };
        assert_eq!(meta("format"), "pbf");
        assert_eq!(meta("name"), "OpenMapTiles");
        assert_eq!(meta("bounds"), "20,50,20.1,50.1");
        assert_eq!(meta("center"), "20.05,50.05,3");
        assert_eq!(meta("minzoom"), "0");
        assert_eq!(meta("maxzoom"), "14");
        let json: Value = serde_json::from_str(&meta("json")).unwrap();
        assert_eq!(json["vector_layers"][0]["id"], "water");
        assert_eq!(json["omitted_sources"][0], "natural_earth");
        assert!(json.get("name").is_none());
    }

    #[test]
    fn antimeridian_bounds_are_declared_as_every_longitude() {
        let bounds = CompileBounds::new(10.0, -10.0, -170.0, 170.0);
        let rows = metadata_rows(&spec(&bounds, "{}")).unwrap();
        let declared = rows.iter().find(|(n, _)| n == "bounds").unwrap();
        assert_eq!(declared.1, "-180,-10,180,10");
    }
}
