//! `compile --data` and `--data-layers`: layers of other vector data.

use osm_tile_compiler::{DataFormat, DataInput, DataLayer};
use std::path::{Path, PathBuf};

/// The options a `--data` value takes after its file.
const KEYS: [&str; 15] = [
    "format",
    "table",
    "crs",
    "min-zoom",
    "max-zoom",
    "attributes",
    "exclude",
    "rename",
    "lon",
    "lat",
    "geometry",
    "delimiter",
    "id",
    "attribution",
    "zoom",
];

/// A `--data LAYER=FILE[,KEY=VALUE...]` value. A comma not followed by a
/// known key belongs to the value before it, so paths, WKT and PROJ
/// strings may hold commas; lists inside a value are separated by `|`.
pub(super) fn parse(spec: &str) -> Result<DataLayer, String> {
    let (layer, rest) = spec
        .split_once('=')
        .ok_or_else(|| format!("--data takes LAYER=FILE[,KEY=VALUE...], got '{spec}'"))?;
    let mut parts: Vec<(Option<&str>, String)> = Vec::new();
    for token in rest.split(',') {
        let option = token
            .split_once('=')
            .filter(|(key, _)| KEYS.contains(key) && !parts.is_empty());
        match option {
            Some((key, value)) => parts.push((Some(key), value.to_string())),
            None => match parts.last_mut() {
                Some((_, value)) => {
                    value.push(',');
                    value.push_str(token);
                }
                None => parts.push((None, token.to_string())),
            },
        }
    }
    let mut parts = parts.into_iter();
    let path = parts
        .next()
        .map(|(_, path)| path)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| format!("--data {layer}= needs a file"))?;
    let mut input = DataInput::new(PathBuf::from(path));
    let mut data = DataLayer::new(layer, input.clone());
    let list = |value: &str| {
        value
            .split('|')
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(String::from)
            .collect::<Vec<_>>()
    };
    let zoom = |key: &str, value: &str| {
        value
            .parse::<u8>()
            .map_err(|_| format!("--data {layer}: {key} must be a zoom, got '{value}'"))
    };
    for (key, value) in parts {
        let key = key.unwrap_or_default();
        match key {
            "format" => {
                input.format = Some(DataFormat::from_id(&value).ok_or_else(|| {
                    format!(
                        "--data {layer}: unknown format '{value}'; the formats are {}",
                        DataFormat::ALL.map(DataFormat::id).join(", ")
                    )
                })?);
            }
            "table" => input.table = Some(value),
            "crs" => input.crs = Some(value),
            "lon" => input.csv.longitude = Some(value),
            "lat" => input.csv.latitude = Some(value),
            "geometry" => input.csv.geometry = Some(value),
            "delimiter" => {
                let delimiter = match value.as_str() {
                    "tab" | "\\t" => '\t',
                    _ => {
                        let mut chars = value.chars();
                        match (chars.next(), chars.next()) {
                            (Some(c), None) => c,
                            _ => {
                                return Err(format!(
                                    "--data {layer}: delimiter must be one character or tab"
                                ));
                            }
                        }
                    }
                };
                input.csv.delimiter = Some(delimiter);
            }
            "id" => input.id_attribute = Some(value),
            "min-zoom" => data.min_zoom = Some(zoom(key, &value)?),
            "max-zoom" => data.max_zoom = Some(zoom(key, &value)?),
            "zoom" => {
                let (min, max) = value.split_once('-').unwrap_or((&value, &value));
                data.min_zoom = Some(zoom(key, min)?);
                data.max_zoom = Some(zoom(key, max)?);
            }
            "attributes" => data.attributes = Some(list(&value)),
            "exclude" => data.exclude_attributes = list(&value),
            "rename" => {
                for pair in list(&value) {
                    let (from, to) = pair.split_once(':').ok_or_else(|| {
                        format!("--data {layer}: rename takes FROM:TO|..., got '{pair}'")
                    })?;
                    data.rename_attributes.insert(from.into(), to.into());
                }
            }
            "attribution" => data.attribution = Some(value),
            _ => unreachable!("only known keys are split off"),
        }
    }
    data.source = input;
    Ok(data)
}

/// A `--data-layers` file: a JSON array of data layers, or an object with
/// one under `data_layers`. Relative paths in it are relative to the file.
pub(super) fn read_file(path: &Path) -> Result<Vec<DataLayer>, String> {
    let text = std::fs::read_to_string(path)
        .map_err(|e| format!("--data-layers {}: {e}", path.display()))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|e| format!("--data-layers {}: {e}", path.display()))?;
    let list = match value {
        serde_json::Value::Object(mut object) => object
            .remove("data_layers")
            .or_else(|| object.remove("dataLayers"))
            .ok_or_else(|| {
                format!(
                    "--data-layers {}: expected an array, or an object with data_layers",
                    path.display()
                )
            })?,
        array => array,
    };
    let mut layers: Vec<DataLayer> = serde_json::from_value(list)
        .map_err(|e| format!("--data-layers {}: {e}", path.display()))?;
    let base = path.parent().unwrap_or(Path::new(""));
    for layer in &mut layers {
        if layer.source.path.is_relative() {
            layer.source.path = base.join(&layer.source.path);
        }
    }
    Ok(layers)
}

/// `data-info <FILE>[,KEY=VALUE...]`: what a data file holds, read as
/// `compile --data` would read it.
pub(super) fn info(spec: &str, json: bool) -> Result<(), String> {
    use osm_tile_compiler::PropertyValue;
    use std::collections::BTreeMap;
    let source = parse(&format!("data-info={spec}"))?.source;
    let format = source.resolved_format().map_err(|e| e.to_string())?;
    let mut bounds: Option<[f64; 4]> = None;
    let mut kinds: BTreeMap<&'static str, u64> = BTreeMap::new();
    let mut attributes: BTreeMap<String, BTreeMap<&'static str, u64>> = BTreeMap::new();
    let report = source
        .read(None, |feature| {
            use geo::BoundingRect;
            if let Some(rect) = feature.geometry.bounding_rect() {
                let b = bounds.get_or_insert([f64::MAX, f64::MAX, f64::MIN, f64::MIN]);
                b[0] = b[0].min(rect.min().x);
                b[1] = b[1].min(rect.min().y);
                b[2] = b[2].max(rect.max().x);
                b[3] = b[3].max(rect.max().y);
            }
            let kind = match feature.geometry {
                geo_types::Geometry::Point(_) | geo_types::Geometry::MultiPoint(_) => "point",
                geo_types::Geometry::LineString(_) | geo_types::Geometry::MultiLineString(_) => {
                    "line"
                }
                _ => "polygon",
            };
            *kinds.entry(kind).or_default() += 1;
            for (key, value) in feature.properties {
                let kind = match value {
                    PropertyValue::Bool(_) => "boolean",
                    PropertyValue::Int(_) | PropertyValue::Float(_) => "number",
                    PropertyValue::String(_) => "string",
                };
                *attributes.entry(key).or_default().entry(kind).or_default() += 1;
            }
            Ok(())
        })
        .map_err(|e| e.to_string())?;
    if json {
        let value = serde_json::json!({
            "file": source.path,
            "format": format.id(),
            "crs": report.crs,
            "features": report.features,
            "without_geometry": report.without_geometry,
            "geometries": kinds,
            "bounds": bounds,
            "attributes": attributes,
        });
        println!(
            "{}",
            serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    println!("{} ({format}, {})", source.path.display(), report.crs);
    println!(
        "{} features{}",
        report.features,
        if report.without_geometry > 0 {
            format!(", and {} without geometry", report.without_geometry)
        } else {
            String::new()
        }
    );
    for (kind, count) in &kinds {
        println!("  {count} {kind}s");
    }
    if let Some([west, south, east, north]) = bounds {
        println!("Bounds (W S E N, WGS84): {west:.6} {south:.6} {east:.6} {north:.6}");
    }
    if !attributes.is_empty() {
        println!("Attributes:");
        for (name, kinds) in &attributes {
            let kinds = kinds
                .iter()
                .map(|(kind, count)| format!("{kind} in {count}"))
                .collect::<Vec<_>>()
                .join(", ");
            println!("  {name}: {kinds}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn data_values_split_on_known_keys_only() {
        let data = parse(
            "parcels=dir,with,commas/parcels.csv,lon=x,lat=y,min-zoom=12,crs=+proj=tmerc \
             +lat_0=0,attributes=name|kind,rename=kind:class,delimiter=;",
        )
        .unwrap();
        assert_eq!(data.layer.as_str(), "parcels");
        assert_eq!(
            data.source.path,
            PathBuf::from("dir,with,commas/parcels.csv")
        );
        assert_eq!(data.source.csv.longitude.as_deref(), Some("x"));
        assert_eq!(data.source.csv.delimiter, Some(';'));
        assert_eq!(data.min_zoom, Some(12));
        assert_eq!(data.source.crs.as_deref(), Some("+proj=tmerc +lat_0=0"));
        assert_eq!(
            data.attributes,
            Some(vec!["name".to_string(), "kind".to_string()])
        );
        assert_eq!(data.rename_attributes["kind"], "class");
        let zoomed = parse("x=a.fgb,zoom=4-9").unwrap();
        assert_eq!((zoomed.min_zoom, zoomed.max_zoom), (Some(4), Some(9)));
        assert!(parse("nofile").is_err());
        assert!(parse("x=a.csv,format=nope").is_err());
    }
}
