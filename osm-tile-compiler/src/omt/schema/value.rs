//! Values of schema expressions and their conversions.
use std::collections::BTreeMap;

/// A value an expression computes: an attribute value, a zoom, a test.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Val {
    Null,
    Bool(bool),
    Int(i64),
    Double(f64),
    Str(String),
    List(Vec<Val>),
    Map(BTreeMap<String, Val>),
}

impl Val {
    /// The value from a parsed YAML or JSON scalar, list or map.
    pub fn from_json(value: &serde_json::Value) -> Self {
        match value {
            serde_json::Value::Null => Self::Null,
            serde_json::Value::Bool(b) => Self::Bool(*b),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(i) => Self::Int(i),
                None => Self::Double(n.as_f64().unwrap_or(f64::NAN)),
            },
            serde_json::Value::String(s) => Self::Str(s.clone()),
            serde_json::Value::Array(items) => {
                Self::List(items.iter().map(Self::from_json).collect())
            }
            serde_json::Value::Object(map) => Self::Map(
                map.iter()
                    .map(|(k, v)| (k.clone(), Self::from_json(v)))
                    .collect(),
            ),
        }
    }

    /// The value as a vector tile attribute: `None` for null and for values
    /// a tile cannot hold (non-finite numbers); lists and maps are written
    /// as their JSON text.
    pub fn to_attribute(&self) -> Option<serde_json::Value> {
        Some(match self {
            Self::Null => return None,
            Self::Bool(b) => serde_json::Value::Bool(*b),
            Self::Int(i) => serde_json::Value::from(*i),
            Self::Double(d) => serde_json::Value::Number(serde_json::Number::from_f64(*d)?),
            Self::Str(s) => serde_json::Value::String(s.clone()),
            Self::List(_) | Self::Map(_) => serde_json::Value::String(self.text()),
        })
    }

    /// The value as Java's `Objects.toString` writes it, which is how
    /// Planetiler compares a configured value with a tag.
    pub fn text(&self) -> String {
        match self {
            Self::Null => "null".into(),
            Self::Bool(b) => b.to_string(),
            Self::Int(i) => i.to_string(),
            Self::Double(d) => java_double(*d),
            Self::Str(s) => s.clone(),
            Self::List(items) => format!(
                "[{}]",
                items.iter().map(Self::text).collect::<Vec<_>>().join(", ")
            ),
            Self::Map(map) => format!(
                "{{{}}}",
                map.iter()
                    .map(|(k, v)| format!("{k}={}", v.text()))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        }
    }
}

/// Java's `Double.toString` for the values a schema writes: integers keep a
/// trailing `.0`, as in `2.0`.
fn java_double(d: f64) -> String {
    if d.is_finite() && d.fract() == 0.0 && d.abs() < 1e7 {
        format!("{d:.1}")
    } else {
        d.to_string()
    }
}

/// A number as a double, for arithmetic and comparison across int and double.
pub(crate) fn number(value: &Val) -> Option<f64> {
    match value {
        Val::Int(i) => Some(*i as f64),
        Val::Double(d) => Some(*d),
        _ => None,
    }
}

/// The data types of Planetiler's schemas, which `type:` names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DataType {
    /// 0, "0", "no", "false" and false are false; anything else is true.
    Boolean,
    String,
    /// OSM's `oneway`: "-1" is -1; "1", "yes" and "true" are 1; anything
    /// else is 0.
    Direction,
    Long,
    Integer,
    Double,
}

impl DataType {
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "boolean" => Self::Boolean,
            "string" => Self::String,
            "direction" => Self::Direction,
            "long" => Self::Long,
            "integer" => Self::Integer,
            "double" => Self::Double,
            _ => return None,
        })
    }
}

/// `value` as `kind`, or `None` (null) when it cannot be one.
pub(crate) fn convert(value: &Val, kind: DataType) -> Option<Val> {
    if *value == Val::Null {
        return None;
    }
    let text = || match value {
        Val::Str(s) => s.clone(),
        other => other.text(),
    };
    Some(match kind {
        DataType::Boolean => Val::Bool(match value {
            Val::Bool(b) => *b,
            Val::Int(i) => *i != 0,
            Val::Double(d) => *d != 0.0,
            _ => !matches!(text().as_str(), "0" | "no" | "false"),
        }),
        DataType::String => Val::Str(text()),
        DataType::Direction => Val::Int(match value {
            Val::Int(i) if *i == -1 || *i == 1 => *i,
            Val::Bool(true) => 1,
            _ => match text().as_str() {
                "-1" => -1,
                "1" | "yes" | "true" => 1,
                _ => 0,
            },
        }),
        DataType::Long | DataType::Integer => {
            let long = match value {
                Val::Int(i) => *i,
                Val::Double(d) if d.is_finite() => *d as i64,
                Val::Bool(b) => i64::from(*b),
                Val::Str(s) => crate::omt::types::parse_profile_long(s)?,
                _ => return None,
            };
            if kind == DataType::Integer {
                Val::Int(i64::from(i32::try_from(long).ok()?))
            } else {
                Val::Int(long)
            }
        }
        DataType::Double => Val::Double(match value {
            Val::Int(i) => *i as f64,
            Val::Double(d) => *d,
            Val::Bool(b) => f64::from(u8::from(*b)),
            Val::Str(s) => crate::omt::types::parse_profile_double(s)?,
            _ => return None,
        }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conversions_follow_planetilers_data_types() {
        let s = |v: &str| Val::Str(v.into());
        assert_eq!(convert(&s("no"), DataType::Boolean), Some(Val::Bool(false)));
        assert_eq!(convert(&s("yes"), DataType::Boolean), Some(Val::Bool(true)));
        assert_eq!(
            convert(&Val::Int(0), DataType::Boolean),
            Some(Val::Bool(false))
        );
        assert_eq!(convert(&s("-1"), DataType::Direction), Some(Val::Int(-1)));
        assert_eq!(convert(&s("yes"), DataType::Direction), Some(Val::Int(1)));
        assert_eq!(
            convert(&s("reversible"), DataType::Direction),
            Some(Val::Int(0))
        );
        assert_eq!(convert(&s("2"), DataType::Integer), Some(Val::Int(2)));
        assert_eq!(convert(&s("x"), DataType::Integer), None);
        assert_eq!(convert(&s("99999999999"), DataType::Integer), None);
        assert_eq!(
            convert(&s("99999999999"), DataType::Long),
            Some(Val::Int(99_999_999_999))
        );
        assert_eq!(convert(&s("1.5"), DataType::Double), Some(Val::Double(1.5)));
        assert_eq!(convert(&Val::Int(3), DataType::String), Some(s("3")));
        assert_eq!(convert(&Val::Null, DataType::String), None);
        assert_eq!(Val::Double(2.0).text(), "2.0");
        assert_eq!(Val::Bool(true).text(), "true");
    }
}
