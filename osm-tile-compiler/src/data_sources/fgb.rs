//! FlatGeobuf, read in order without its spatial index.

use super::{Crs, PropertyValue, Sink};
use flatgeobuf::{FallibleStreamingIterator, FeatureProperties, FgbReader};
use geozero::{ColumnValue, PropertyProcessor, ToGeo};
use std::collections::BTreeMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

pub(super) fn read(path: &Path, sink: &mut Sink) -> Result<(), String> {
    let file = BufReader::new(File::open(path).map_err(|e| e.to_string())?);
    let reader = FgbReader::open(file).map_err(|e| e.to_string())?;
    if let Some(crs) = reader.header().crs() {
        let organization = crs.org().unwrap_or("EPSG");
        let declared = if crs.code() > 0 {
            Some(Crs::from_authority(organization, &crs.code().to_string()))
        } else if let Some(code) = crs.code_string().filter(|c| !c.is_empty()) {
            Some(Crs::from_authority(organization, code))
        } else {
            crs.wkt().filter(|w| !w.trim().is_empty()).map(Crs::parse)
        };
        if let Some(declared) = declared {
            sink.declare_crs(declared?);
        }
    }
    let mut features = reader.select_all_seq().map_err(|e| e.to_string())?;
    let mut position = 0u64;
    while let Some(feature) = features.next().map_err(|e| e.to_string())? {
        position += 1;
        let geometry = feature.to_geo().ok();
        let mut properties = Properties::default();
        feature
            .process_properties(&mut properties)
            .map_err(|e| format!("feature {position}: {e}"))?;
        sink.push(Some(position), geometry, properties.0)?;
    }
    Ok(())
}

#[derive(Default)]
struct Properties(BTreeMap<String, PropertyValue>);

impl PropertyProcessor for Properties {
    fn property(
        &mut self,
        _index: usize,
        name: &str,
        value: &ColumnValue,
    ) -> geozero::error::Result<bool> {
        let value = match *value {
            ColumnValue::Bool(v) => PropertyValue::Bool(v),
            ColumnValue::Byte(v) => PropertyValue::Int(v.into()),
            ColumnValue::UByte(v) => PropertyValue::Int(v.into()),
            ColumnValue::Short(v) => PropertyValue::Int(v.into()),
            ColumnValue::UShort(v) => PropertyValue::Int(v.into()),
            ColumnValue::Int(v) => PropertyValue::Int(v.into()),
            ColumnValue::UInt(v) => PropertyValue::Int(v.into()),
            ColumnValue::Long(v) => PropertyValue::Int(v),
            ColumnValue::ULong(v) => match i64::try_from(v) {
                Ok(v) => PropertyValue::Int(v),
                Err(_) => PropertyValue::Float(v as f64),
            },
            ColumnValue::Float(v) => PropertyValue::Float(v.into()),
            ColumnValue::Double(v) => PropertyValue::Float(v),
            ColumnValue::String(v) | ColumnValue::Json(v) | ColumnValue::DateTime(v) => {
                PropertyValue::String(v.to_string())
            }
            ColumnValue::Binary(_) => return Ok(false),
        };
        self.0.insert(name.to_string(), value);
        Ok(false)
    }
}
