//! GeoJSON and newline-delimited GeoJSON. A FeatureCollection streams
//! feature by feature, so a large file is never held whole.

use super::{Crs, PropertyValue, Sink};
use geo_types::Geometry;
use std::collections::BTreeMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::LazyLock;

/// How much of the file's start and end is looked at for its type and its
/// old-style `crs` member.
const PEEK_BYTES: usize = 64 * 1024;

pub(super) fn read(path: &Path, sink: &mut Sink) -> Result<(), String> {
    let mut file = File::open(path).map_err(|e| e.to_string())?;
    let length = file.metadata().map_err(|e| e.to_string())?.len();
    let mut head = Vec::with_capacity(PEEK_BYTES);
    file.by_ref()
        .take(PEEK_BYTES as u64)
        .read_to_end(&mut head)
        .map_err(|e| e.to_string())?;
    // Some writers (GeoServer) put the crs member after the features.
    let mut tail = Vec::new();
    if length > PEEK_BYTES as u64 {
        file.seek(SeekFrom::Start(
            length
                .saturating_sub(PEEK_BYTES as u64)
                .max(PEEK_BYTES as u64),
        ))
        .map_err(|e| e.to_string())?;
        file.read_to_end(&mut tail).map_err(|e| e.to_string())?;
    }
    file.seek(SeekFrom::Start(0)).map_err(|e| e.to_string())?;
    let head = String::from_utf8_lossy(&head);
    let crs = legacy_crs(&head).or_else(|| legacy_crs(&String::from_utf8_lossy(&tail)));
    if let Some(crs) = crs {
        sink.declare_crs(Crs::parse(&crs)?);
    }
    if head.contains("FeatureCollection") {
        let reader = geojson::FeatureReader::from_reader(BufReader::new(file));
        for feature in reader.features() {
            feature_into(feature.map_err(|e| e.to_string())?, sink)?;
        }
        return Ok(());
    }
    let document =
        geojson::GeoJson::from_reader(BufReader::new(file)).map_err(|e| e.to_string())?;
    document_into(document, sink)
}

pub(super) fn read_seq(path: &Path, sink: &mut Sink) -> Result<(), String> {
    let file = File::open(path).map_err(|e| e.to_string())?;
    for (number, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| e.to_string())?;
        // RFC 8142 starts each record with an ASCII record separator.
        let line = line.trim_matches(|c: char| c == '\u{1e}' || c.is_whitespace());
        if line.is_empty() {
            continue;
        }
        let document = line
            .parse::<geojson::GeoJson>()
            .map_err(|e| format!("line {}: {e}", number + 1))?;
        document_into(document, sink)?;
    }
    Ok(())
}

fn document_into(document: geojson::GeoJson, sink: &mut Sink) -> Result<(), String> {
    match document {
        geojson::GeoJson::FeatureCollection(collection) => {
            for feature in collection.features {
                feature_into(feature, sink)?;
            }
            Ok(())
        }
        geojson::GeoJson::Feature(feature) => feature_into(feature, sink),
        geojson::GeoJson::Geometry(geometry) => {
            sink.push(None, Some(geometry_of(&geometry)?), BTreeMap::new())
        }
    }
}

fn geometry_of(geometry: &geojson::Geometry) -> Result<Geometry<f64>, String> {
    Geometry::<f64>::try_from(&geometry.value).map_err(|e| e.to_string())
}

fn feature_into(feature: geojson::Feature, sink: &mut Sink) -> Result<(), String> {
    let geometry = feature.geometry.as_ref().map(geometry_of).transpose()?;
    let id = match &feature.id {
        Some(geojson::feature::Id::Number(n)) => n.as_u64(),
        Some(geojson::feature::Id::String(s)) => s.parse().ok(),
        None => None,
    };
    let properties = feature
        .properties
        .unwrap_or_default()
        .into_iter()
        .filter_map(|(key, value)| Some((key, PropertyValue::from_json(value)?)))
        .collect();
    sink.push(id, geometry, properties)
}

/// The name in a GeoJSON 2008 `"crs": {"type": "name", "properties":
/// {"name": …}}` member.
fn legacy_crs(head: &str) -> Option<String> {
    static CRS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r#""crs"\s*:\s*\{\s*"type"\s*:\s*"name"\s*,\s*"properties"\s*:\s*\{\s*"name"\s*:\s*"([^"]+)""#,
        )
        .unwrap()
    });
    Some(CRS.captures(head)?[1].to_string())
}
