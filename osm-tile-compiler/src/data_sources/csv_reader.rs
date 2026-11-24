//! CSV and TSV with a header row: points from two coordinate columns, or
//! any geometry from a WKT or hex WKB column. Other columns become
//! attributes, typed as Tippecanoe types them: whole numbers and decimals
//! as numbers, the rest as text, empty cells left out.

use super::{CsvColumns, PropertyValue, Sink, wkb};
use geo_types::{Geometry, Point};
use std::collections::BTreeMap;
use std::path::Path;

const LONGITUDE: [&str; 5] = ["longitude", "lon", "lng", "long", "x"];
const LATITUDE: [&str; 3] = ["latitude", "lat", "y"];
const GEOMETRY: [&str; 5] = ["wkt", "geometry", "geom", "the_geom", "wkb"];

enum Columns {
    Point { x: usize, y: usize },
    Geometry(usize),
}

pub(super) fn read(path: &Path, options: &CsvColumns, sink: &mut Sink) -> Result<(), String> {
    let tsv = path
        .extension()
        .is_some_and(|e| e.eq_ignore_ascii_case("tsv"));
    let delimiter = options.delimiter.unwrap_or(if tsv { '\t' } else { ',' });
    let delimiter = u8::try_from(delimiter)
        .map_err(|_| format!("the CSV delimiter {delimiter:?} is not a single byte"))?;
    let mut reader = csv::ReaderBuilder::new()
        .delimiter(delimiter)
        .flexible(true)
        .from_path(path)
        .map_err(|e| e.to_string())?;
    let headers = reader
        .headers()
        .map_err(|e| e.to_string())?
        .iter()
        // A byte order mark sticks to the first name.
        .map(|h| h.trim_start_matches('\u{feff}').trim().to_string())
        .collect::<Vec<_>>();
    let columns = columns(&headers, options)?;
    let geometry_columns = match columns {
        Columns::Point { x, y } => vec![x, y],
        Columns::Geometry(g) => vec![g],
    };
    for (row, record) in reader.records().enumerate() {
        let record = record.map_err(|e| format!("row {}: {e}", row + 2))?;
        let cell = |index: usize| record.get(index).unwrap_or("").trim();
        let geometry = match columns {
            Columns::Point { x, y } => match (cell(x).parse::<f64>(), cell(y).parse::<f64>()) {
                (Ok(x), Ok(y)) => Some(Geometry::Point(Point::new(x, y))),
                _ => None,
            },
            Columns::Geometry(index) => {
                let text = cell(index);
                if text.is_empty() {
                    None
                } else if wkb::looks_like_hex(text) {
                    wkb::parse_hex(text)
                        .map_err(|e| format!("row {}: {e}", row + 2))?
                        .0
                } else {
                    Some(parse_wkt(text).map_err(|e| format!("row {}: {e}", row + 2))?)
                }
            }
        };
        let properties = headers
            .iter()
            .enumerate()
            .filter(|(index, name)| !geometry_columns.contains(index) && !name.is_empty())
            .filter_map(|(index, name)| Some((name.clone(), infer(cell(index))?)))
            .collect::<BTreeMap<_, _>>();
        sink.push(None, geometry, properties)?;
    }
    Ok(())
}

fn columns(headers: &[String], options: &CsvColumns) -> Result<Columns, String> {
    let find = |name: &str| {
        headers
            .iter()
            .position(|h| h == name)
            .or_else(|| headers.iter().position(|h| h.eq_ignore_ascii_case(name)))
            .ok_or_else(|| format!("no column {name} among {}", headers.join(", ")))
    };
    let guess = |names: &[&str]| names.iter().find_map(|name| find(name).ok());
    if let Some(geometry) = &options.geometry {
        return Ok(Columns::Geometry(find(geometry)?));
    }
    match (&options.longitude, &options.latitude) {
        (Some(x), Some(y)) => {
            return Ok(Columns::Point {
                x: find(x)?,
                y: find(y)?,
            });
        }
        (Some(_), None) | (None, Some(_)) => {
            return Err("give both the longitude and the latitude column".into());
        }
        (None, None) => {}
    }
    if let (Some(x), Some(y)) = (guess(&LONGITUDE), guess(&LATITUDE)) {
        return Ok(Columns::Point { x, y });
    }
    if let Some(geometry) = guess(&GEOMETRY) {
        return Ok(Columns::Geometry(geometry));
    }
    Err(format!(
        "no geometry columns among {}: name them (longitude and latitude, or a WKT column)",
        headers.join(", ")
    ))
}

fn parse_wkt(text: &str) -> Result<Geometry<f64>, String> {
    use std::str::FromStr;
    // PostGIS's EWKT prefixes an SRID, which the column's CRS covers.
    let text = match text.split_once(';') {
        Some((srid, rest)) if srid.trim().to_ascii_uppercase().starts_with("SRID=") => rest,
        _ => text,
    };
    let wkt = wkt::Wkt::<f64>::from_str(text).map_err(|e| format!("WKT: {e}"))?;
    Geometry::try_from(wkt).map_err(|e| format!("WKT: {e}"))
}

/// A cell's value: a whole number, a decimal, or text. Numbers with
/// leading zeros (postcodes, codes) and anything not plainly a number stay
/// text.
pub(super) fn infer(text: &str) -> Option<PropertyValue> {
    if text.is_empty() {
        return None;
    }
    let digits = text.strip_prefix('-').unwrap_or(text);
    let leading_zero = digits.len() > 1 && digits.starts_with('0') && !digits.starts_with("0.");
    let numeric = !digits.is_empty()
        && digits.as_bytes()[0].is_ascii_digit()
        && digits
            .bytes()
            .all(|b| b.is_ascii_digit() || matches!(b, b'.' | b'e' | b'E' | b'-' | b'+'));
    if numeric && !leading_zero {
        if let Ok(value) = text.parse::<i64>() {
            return Some(PropertyValue::Int(value));
        }
        if let Ok(value) = text.parse::<f64>()
            && value.is_finite()
        {
            return Some(PropertyValue::Float(value));
        }
    }
    Some(PropertyValue::String(text.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cells_are_typed_like_tippecanoe() {
        assert_eq!(infer("12"), Some(PropertyValue::Int(12)));
        assert_eq!(infer("-3"), Some(PropertyValue::Int(-3)));
        assert_eq!(infer("2.5"), Some(PropertyValue::Float(2.5)));
        assert_eq!(infer("0.5"), Some(PropertyValue::Float(0.5)));
        assert_eq!(infer("1e3"), Some(PropertyValue::Float(1000.0)));
        assert_eq!(infer("0"), Some(PropertyValue::Int(0)));
        assert_eq!(infer("007"), Some(PropertyValue::String("007".into())));
        assert_eq!(
            infer("30-001"),
            Some(PropertyValue::String("30-001".into()))
        );
        assert_eq!(infer("NaN"), Some(PropertyValue::String("NaN".into())));
        assert_eq!(infer("true"), Some(PropertyValue::String("true".into())));
        assert_eq!(infer(""), None);
    }

    #[test]
    fn ewkt_srid_prefixes_are_dropped() {
        assert_eq!(
            parse_wkt("SRID=4326;POINT(1 2)").unwrap(),
            Geometry::Point(Point::new(1.0, 2.0))
        );
    }
}
