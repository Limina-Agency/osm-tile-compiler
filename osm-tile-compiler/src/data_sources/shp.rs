//! ESRI Shapefiles: the `.shp` geometry, the `.dbf` attributes and the
//! `.prj` coordinate system beside it.

use super::{Crs, PropertyValue, Sink};
use geo_types::Geometry;
use shapefile::dbase::FieldValue;
use std::collections::BTreeMap;
use std::path::Path;

pub(super) fn read(path: &Path, sink: &mut Sink) -> Result<(), String> {
    if let Some(prj) = super::sidecar(path, "prj")
        && let Ok(text) = std::fs::read_to_string(&prj)
    {
        sink.declare_crs(Crs::parse(&text).map_err(|e| format!("{}: {e}", prj.display()))?);
    }
    let mut reader = shapefile::Reader::from_path(path).map_err(|e| e.to_string())?;
    for (index, item) in reader.iter_shapes_and_records().enumerate() {
        let (shape, record) = item.map_err(|e| format!("record {}: {e}", index + 1))?;
        let geometry = match shape {
            shapefile::Shape::NullShape => None,
            shape => Geometry::<f64>::try_from(shape).ok(),
        };
        let properties = record
            .into_iter()
            .filter_map(|(key, value)| Some((key, value_of(value)?)))
            .collect::<BTreeMap<_, _>>();
        sink.push(Some(index as u64 + 1), geometry, properties)?;
    }
    Ok(())
}

fn value_of(value: FieldValue) -> Option<PropertyValue> {
    Some(match value {
        FieldValue::Character(v) => PropertyValue::String(v?.trim_end().to_string()),
        FieldValue::Memo(v) => PropertyValue::String(v),
        FieldValue::Numeric(v) => number(v?),
        FieldValue::Float(v) => number(f64::from(v?)),
        FieldValue::Integer(v) => PropertyValue::Int(v.into()),
        FieldValue::Double(v) | FieldValue::Currency(v) => number(v),
        FieldValue::Logical(v) => PropertyValue::Bool(v?),
        FieldValue::Date(v) => {
            let date = v?;
            PropertyValue::String(format!(
                "{:04}-{:02}-{:02}",
                date.year(),
                date.month(),
                date.day()
            ))
        }
        FieldValue::DateTime(v) => {
            let (date, time) = (v.date(), v.time());
            PropertyValue::String(format!(
                "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}",
                date.year(),
                date.month(),
                date.day(),
                time.hours(),
                time.minutes(),
                time.seconds()
            ))
        }
    })
}

/// A dBase number: whole numbers as integers, the rest as decimals.
fn number(value: f64) -> PropertyValue {
    if value.fract() == 0.0 && value.abs() < 9.0e15 {
        PropertyValue::Int(value as i64)
    } else {
        PropertyValue::Float(value)
    }
}
