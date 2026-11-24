//! OGC GeoPackage feature tables, read with the bundled SQLite.

use super::{Crs, PropertyValue, Sink, wkb};
use rusqlite::{Connection, OpenFlags, types::ValueRef};
use std::collections::BTreeMap;
use std::path::Path;

pub(super) fn read(path: &Path, table: Option<&str>, sink: &mut Sink) -> Result<(), String> {
    let db = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )
    .map_err(|e| e.to_string())?;
    let tables = feature_tables(&db)?;
    let table = match table {
        Some(table) => tables
            .iter()
            .find(|t| t.as_str() == table)
            .ok_or_else(|| {
                format!(
                    "no feature table {table}; it holds {}",
                    list_or_none(&tables)
                )
            })?
            .clone(),
        None => match tables.as_slice() {
            [only] => only.clone(),
            [] => return Err("it holds no feature table".into()),
            many => {
                return Err(format!(
                    "it holds several feature tables, so name one as the input's table: {}",
                    many.join(", ")
                ));
            }
        },
    };
    let (geometry_column, srs_id): (String, i64) = db
        .query_row(
            "SELECT column_name, srs_id FROM gpkg_geometry_columns WHERE table_name = ?1",
            [&table],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .map_err(|e| format!("table {table} has no geometry column: {e}"))?;
    if let Some(crs) = srs(&db, srs_id)? {
        sink.declare_crs(crs);
    }
    let primary_key = primary_key(&db, &table)?;
    let mut statement = db
        .prepare(&format!("SELECT * FROM {}", quote(&table)))
        .map_err(|e| e.to_string())?;
    let names = statement
        .column_names()
        .map(String::from)
        .collect::<Vec<_>>();
    let geometry_index = names
        .iter()
        .position(|n| n.eq_ignore_ascii_case(&geometry_column))
        .ok_or_else(|| format!("table {table} lacks its geometry column {geometry_column}"))?;
    let key_index = primary_key
        .as_ref()
        .and_then(|key| names.iter().position(|n| n == key));
    let mut rows = statement.query([]).map_err(|e| e.to_string())?;
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let mut id = None;
        let mut geometry = None;
        let mut properties = BTreeMap::new();
        for (index, name) in names.iter().enumerate() {
            let value = row.get_ref(index).map_err(|e| e.to_string())?;
            if index == geometry_index {
                if let ValueRef::Blob(blob) = value {
                    geometry = gpkg_geometry(blob)?;
                }
                continue;
            }
            if Some(index) == key_index {
                if let ValueRef::Integer(key) = value {
                    id = u64::try_from(key).ok();
                }
                continue;
            }
            let value = match value {
                ValueRef::Integer(v) => PropertyValue::Int(v),
                ValueRef::Real(v) => PropertyValue::Float(v),
                ValueRef::Text(v) => PropertyValue::String(String::from_utf8_lossy(v).into()),
                ValueRef::Null | ValueRef::Blob(_) => continue,
            };
            properties.insert(name.clone(), value);
        }
        sink.push(id, geometry, properties)?;
    }
    Ok(())
}

fn list_or_none(tables: &[String]) -> String {
    if tables.is_empty() {
        "none".into()
    } else {
        tables.join(", ")
    }
}

fn feature_tables(db: &Connection) -> Result<Vec<String>, String> {
    let mut statement = db
        .prepare(
            "SELECT table_name FROM gpkg_contents WHERE data_type = 'features' ORDER BY \
             table_name",
        )
        .map_err(|e| format!("not a GeoPackage: {e}"))?;
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())
}

/// The integer primary key, which GeoPackage feature tables have.
fn primary_key(db: &Connection, table: &str) -> Result<Option<String>, String> {
    let mut statement = db
        .prepare(&format!("PRAGMA table_info({})", quote(table)))
        .map_err(|e| e.to_string())?;
    let keys = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(5)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    Ok(keys
        .into_iter()
        .find(|(_, kind, pk)| *pk == 1 && kind.eq_ignore_ascii_case("INTEGER"))
        .map(|(name, _, _)| name))
}

/// The coordinate system of an `srs_id`: by its organization's code, else
/// its WKT. GeoPackage's undefined systems (-1, 0) are read as WGS84.
fn srs(db: &Connection, srs_id: i64) -> Result<Option<Crs>, String> {
    if srs_id <= 0 {
        return Ok(None);
    }
    let found = db
        .query_row(
            "SELECT organization, organization_coordsys_id, definition FROM \
             gpkg_spatial_ref_sys WHERE srs_id = ?1",
            [srs_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .map_err(|e| format!("spatial reference system {srs_id}: {e}"))?;
    let (organization, code, definition) = found;
    if let Ok(crs) = Crs::from_authority(&organization, &code.to_string()) {
        return Ok(Some(crs));
    }
    match definition.filter(|d| !d.trim().is_empty() && d.trim() != "undefined") {
        Some(definition) => Crs::parse(&definition).map(Some),
        None => Err(format!(
            "spatial reference system {organization}:{code} is unknown; give the input's crs"
        )),
    }
}

/// A GeoPackage geometry blob: its header, then WKB.
fn gpkg_geometry(blob: &[u8]) -> Result<Option<geo_types::Geometry<f64>>, String> {
    if blob.len() < 8 || &blob[..2] != b"GP" {
        // Some writers store plain WKB.
        return wkb::parse(blob).map(|(geometry, _)| geometry);
    }
    let flags = blob[3];
    if flags & 0b1_0000 != 0 {
        return Ok(None);
    }
    let envelope = match (flags >> 1) & 0b111 {
        0 => 0,
        1 => 32,
        2 | 3 => 48,
        4 => 64,
        other => return Err(format!("GeoPackage envelope type {other} is unknown")),
    };
    let start = 8 + envelope;
    let body = blob
        .get(start..)
        .ok_or("GeoPackage geometry ends in its header")?;
    wkb::parse(body).map(|(geometry, _)| geometry)
}

fn quote(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}
