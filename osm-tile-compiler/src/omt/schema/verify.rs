//! A schema's `examples`: input features and the output features they must
//! make, as Planetiler's `verify` command checks them.
use super::profile::SchemaProfile;
use super::value::Val;
use super::yaml::Node;
use super::{Schema, SourceInput};
use crate::omt::types::{Feature, Source, SourceKind};
use geo_types::{Coord, Geometry, LineString, MultiLineString, MultiPolygon, Point, Polygon};
use std::sync::Arc;

/// Planetiler's `at_zoom` default (`PlanetilerConfig.MAX_MAXZOOM`).
const DEFAULT_AT_ZOOM: u8 = 15;
/// The highest zoom the examples are evaluated for.
const MAX_ZOOM: u8 = 14;

#[derive(Debug, Clone)]
pub(crate) struct Example {
    name: String,
    source: String,
    geometry: Geometry<f64>,
    tags: Vec<(String, String)>,
    outputs: Vec<ExpectedFeature>,
}

#[derive(Debug, Clone)]
struct ExpectedFeature {
    layer: Option<String>,
    geometry: Option<String>,
    min_zoom: Option<i64>,
    max_zoom: Option<i64>,
    min_size: Option<f64>,
    tags: Vec<(String, Node)>,
    allow_extra_tags: bool,
    at_zoom: u8,
}

/// The outcome of one example.
#[derive(Debug, Clone, serde::Serialize)]
#[non_exhaustive]
pub struct ExampleResult {
    pub name: String,
    /// What differed from the expected output; empty when it passed.
    pub issues: Vec<String>,
}

pub(crate) fn parse_examples(node: &Node) -> Result<Vec<Example>, String> {
    let Node::List(items) = node else {
        return Err("examples must be a list".into());
    };
    items
        .iter()
        .enumerate()
        .map(|(i, item)| parse_example(item).map_err(|e| format!("examples[{i}]: {e}")))
        .collect()
}

fn parse_example(node: &Node) -> Result<Example, String> {
    let name = node.get("name").map(Node::text).unwrap_or_default();
    let input = node.get("input").ok_or("an example needs an input")?;
    let source = input.get("source").map(Node::text).unwrap_or_default();
    let geometry = geometry(&input.get("geometry").map(Node::text).unwrap_or_default())
        .map_err(|e| format!("{name}: {e}"))?;
    let tags = match input.get("tags") {
        Some(Node::Map(entries)) => entries
            .iter()
            .filter(|(_, v)| !v.is_null())
            .map(|(k, v)| (k.clone(), v.text()))
            .collect(),
        _ => Vec::new(),
    };
    let outputs = match node.get("output") {
        None | Some(Node::Null) => Vec::new(),
        Some(Node::List(items)) => items.iter().map(expected).collect::<Result<_, _>>()?,
        Some(single) => vec![expected(single)?],
    };
    Ok(Example {
        name,
        source,
        geometry,
        tags,
        outputs,
    })
}

fn expected(node: &Node) -> Result<ExpectedFeature, String> {
    let int = |key| match node.get(key) {
        Some(Node::Int(i)) => Some(*i),
        _ => None,
    };
    Ok(ExpectedFeature {
        layer: node.get("layer").map(Node::text),
        geometry: node.get("geometry").map(Node::text),
        min_zoom: int("min_zoom"),
        max_zoom: int("max_zoom"),
        min_size: match node.get("min_size") {
            Some(Node::Int(i)) => Some(*i as f64),
            Some(Node::Double(d)) => Some(*d),
            _ => None,
        },
        tags: match node.get("tags") {
            Some(Node::Map(entries)) => entries.clone(),
            _ => Vec::new(),
        },
        allow_extra_tags: !matches!(node.get("allow_extra_tags"), Some(Node::Bool(false))),
        at_zoom: int("at_zoom").map_or(DEFAULT_AT_ZOOM, |z| z.clamp(0, 255) as u8),
    })
}

/// `point`, `line`, `polygon` (Planetiler's stand-ins) or a WKT geometry.
fn geometry(text: &str) -> Result<Geometry<f64>, String> {
    Ok(match text.trim() {
        "point" => Geometry::Point(Point::new(0.0, 0.0)),
        "line" => Geometry::LineString(LineString::from(vec![(0.0, 0.0), (1.0, 1.0)])),
        "polygon" => Geometry::Polygon(Polygon::new(
            LineString::from(vec![
                (0.0, 0.0),
                (1.0, 0.0),
                (1.0, 1.0),
                (0.0, 1.0),
                (0.0, 0.0),
            ]),
            vec![],
        )),
        wkt => parse_wkt(wkt)?,
    })
}

/// The WKT the examples use: points, lines and polygons, single or multi.
fn parse_wkt(text: &str) -> Result<Geometry<f64>, String> {
    let error = || format!("bad geometry '{text}': use point, line, polygon or WKT");
    let (kind, body) = text.split_once('(').ok_or_else(error)?;
    let body = format!("({body}");
    let kind = kind.trim().to_ascii_uppercase();
    let value = parse_parens(&body).ok_or_else(error)?;
    let coord = |v: &Wkt| match v {
        Wkt::Coord(c) => Some(*c),
        _ => None,
    };
    let coords = |v: &Wkt| match v {
        Wkt::List(items) => items.iter().map(coord).collect::<Option<Vec<_>>>(),
        _ => None,
    };
    let polygon = |v: &Wkt| -> Option<Polygon<f64>> {
        let Wkt::List(rings) = v else { return None };
        let mut rings = rings.iter().map(|r| coords(r).map(LineString::new));
        let exterior = rings.next()??;
        Some(Polygon::new(exterior, rings.collect::<Option<Vec<_>>>()?))
    };
    let list = |v: &Wkt| match v {
        Wkt::List(items) => Some(items.clone()),
        _ => None,
    };
    Ok(match kind.as_str() {
        "POINT" => Geometry::Point(Point(
            coord(list(&value).ok_or_else(error)?.first().ok_or_else(error)?).ok_or_else(error)?,
        )),
        "LINESTRING" => Geometry::LineString(LineString::new(coords(&value).ok_or_else(error)?)),
        "POLYGON" => Geometry::Polygon(polygon(&value).ok_or_else(error)?),
        "MULTILINESTRING" => Geometry::MultiLineString(MultiLineString(
            list(&value)
                .ok_or_else(error)?
                .iter()
                .map(|l| coords(l).map(LineString::new))
                .collect::<Option<_>>()
                .ok_or_else(error)?,
        )),
        "MULTIPOLYGON" => Geometry::MultiPolygon(MultiPolygon(
            list(&value)
                .ok_or_else(error)?
                .iter()
                .map(polygon)
                .collect::<Option<_>>()
                .ok_or_else(error)?,
        )),
        _ => return Err(error()),
    })
}

#[derive(Debug, Clone)]
enum Wkt {
    Coord(Coord<f64>),
    List(Vec<Wkt>),
}

/// `(a b, c d)` and nested lists of them.
fn parse_parens(text: &str) -> Option<Wkt> {
    fn list(chars: &mut std::iter::Peekable<std::str::Chars>, depth: usize) -> Option<Wkt> {
        if depth > 8 {
            return None;
        }
        let mut items = Vec::new();
        loop {
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            match chars.peek()? {
                '(' => {
                    chars.next();
                    items.push(list(chars, depth + 1)?);
                }
                _ => {
                    let mut item = String::new();
                    while let Some(&c) = chars.peek() {
                        if c == ',' || c == ')' {
                            break;
                        }
                        item.push(c);
                        chars.next();
                    }
                    let mut numbers = item.split_whitespace().map(str::parse::<f64>);
                    let x = numbers.next()?.ok()?;
                    let y = numbers.next()?.ok()?;
                    items.push(Wkt::Coord(Coord { x, y }));
                }
            }
            while chars.peek().is_some_and(|c| c.is_whitespace()) {
                chars.next();
            }
            match chars.next()? {
                ',' => continue,
                ')' => return Some(Wkt::List(items)),
                _ => return None,
            }
        }
    }
    let mut chars = text.trim().chars().peekable();
    if chars.next()? != '(' {
        return None;
    }
    let value = list(&mut chars, 0)?;
    chars.all(char::is_whitespace).then_some(value)
}

impl Schema {
    /// Runs the schema's examples.
    pub fn verify(self: &Arc<Self>) -> Vec<ExampleResult> {
        let profile = SchemaProfile::for_examples(self.clone(), MAX_ZOOM);
        self.examples
            .iter()
            .map(|example| ExampleResult {
                name: example.name.clone(),
                issues: self.run_example(&profile, example),
            })
            .collect()
    }

    fn run_example(&self, profile: &SchemaProfile, example: &Example) -> Vec<String> {
        let Some(input) = self
            .sources
            .iter()
            .find(|s| s.id == example.source)
            .map(|s| s.input)
        else {
            return vec![format!("unknown source '{}'", example.source)];
        };
        let (kind, source_layer) = match input {
            SourceInput::Osm => (
                match example.geometry {
                    Geometry::Point(_) | Geometry::MultiPoint(_) => SourceKind::Node,
                    Geometry::LineString(_) | Geometry::Polygon(_) => SourceKind::Way,
                    _ => SourceKind::Relation,
                },
                "osm",
            ),
            SourceInput::WaterPolygons => (SourceKind::Supplement, "water_polygons"),
        };
        let source = Source {
            id: 10,
            osm_id: 1,
            kind,
            source_layer: source_layer.into(),
            tags: example.tags.iter().cloned().collect(),
            geometry: example.geometry.clone(),
            relations: vec![],
        };
        let mut actual = Vec::new();
        profile.features(&source, &mut actual);
        let mut issues = Vec::new();
        if actual.len() != example.outputs.len() {
            issues.push(format!(
                "Different number of elements, expected={} actual={}",
                example.outputs.len(),
                actual.len()
            ));
            return issues;
        }
        for (i, expected) in example.outputs.iter().enumerate() {
            let best = (0..actual.len())
                .max_by_key(|&j| proximity(expected, &actual[j]))
                .expect("as many features as expected");
            let feature = actual.remove(best);
            compare(&format!("feature[{i}]"), expected, &feature, &mut issues);
        }
        issues
    }
}

fn geometry_type(feature: &Feature) -> &'static str {
    match feature.geometry {
        Geometry::Point(_) | Geometry::MultiPoint(_) => "point",
        Geometry::LineString(_) | Geometry::MultiLineString(_) | Geometry::Line(_) => "line",
        Geometry::Polygon(_)
        | Geometry::MultiPolygon(_)
        | Geometry::Rect(_)
        | Geometry::Triangle(_) => "polygon",
        Geometry::GeometryCollection(_) => "unknown",
    }
}

/// The feature's attributes at `zoom`.
fn attrs_at(feature: &Feature, zoom: u8) -> Vec<(&String, &serde_json::Value)> {
    feature
        .attrs
        .iter()
        .filter(|(k, _)| feature.attr_minzoom.get(*k).is_none_or(|z| zoom >= *z))
        .collect()
}

fn proximity(expected: &ExpectedFeature, feature: &Feature) -> usize {
    let mut score = 0;
    if expected.layer.as_deref() == Some(feature.layer.as_str()) {
        score += 2;
    }
    if expected.geometry.as_deref() == Some(geometry_type(feature)) {
        score += 1;
    }
    let attrs = attrs_at(feature, expected.at_zoom);
    for (key, value) in &expected.tags {
        if let Some((_, actual)) = attrs.iter().find(|(k, _)| *k == key) {
            score += 1;
            if same(value, actual) {
                score += 1;
            }
        }
    }
    score
}

/// Whether an expected tag value equals an attribute, numbers compared by
/// value as Planetiler does.
fn same(expected: &Node, actual: &serde_json::Value) -> bool {
    let actual = Val::from_json(actual);
    match (expected.to_val(), actual) {
        (Val::Int(a), Val::Int(b)) => a == b,
        (Val::Double(a), Val::Double(b)) => a == b || (a - b).abs() <= 1e-9 * a.abs().max(b.abs()),
        (Val::Int(a), Val::Double(b)) | (Val::Double(b), Val::Int(a)) => (a as f64) == b,
        // Input tags are strings here, as OSM's are, where Planetiler's
        // examples keep YAML's numbers: `addr:housenumber: 123` passes
        // through `tag_value` as the text "123".
        (expected @ (Val::Int(_) | Val::Double(_)), Val::Str(actual)) => expected.text() == actual,
        (expected, actual) => expected == actual,
    }
}

fn compare(prefix: &str, expected: &ExpectedFeature, feature: &Feature, issues: &mut Vec<String>) {
    let mut check = |field: &str, expected: Option<String>, actual: String| {
        if let Some(expected) = expected
            && expected != actual
        {
            issues.push(format!(
                "{prefix}.{field}: expected <{expected}> actual <{actual}>"
            ));
        }
    };
    check("layer", expected.layer.clone(), feature.layer.clone());
    check(
        "minzoom",
        expected.min_zoom.map(|z| z.to_string()),
        feature.min_zoom.to_string(),
    );
    check(
        "maxzoom",
        expected.max_zoom.map(|z| z.to_string()),
        feature.max_zoom.to_string(),
    );
    let size = if expected.at_zoom == MAX_ZOOM {
        feature.min_size_at_max_zoom
    } else {
        feature.min_size
    };
    check(
        "minsize",
        expected.min_size.map(|s| s.to_string()),
        size.to_string(),
    );
    check(
        "geometry",
        expected.geometry.clone(),
        geometry_type(feature).to_string(),
    );
    let attrs = attrs_at(feature, expected.at_zoom);
    let mut extra = attrs.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>();
    for (key, value) in &expected.tags {
        extra.retain(|k| *k != key);
        let actual = attrs.iter().find(|(k, _)| *k == key).map(|(_, v)| *v);
        let ok = match actual {
            None => value.is_null(),
            Some(actual) => same(value, actual),
        };
        if !ok {
            issues.push(format!(
                "{prefix}.tags[\"{key}\"]: expected <{}> actual <{}>",
                if value.is_null() {
                    "null".into()
                } else {
                    value.text()
                },
                actual.map_or("null".into(), |a| Val::from_json(a).text())
            ));
        }
    }
    if !expected.allow_extra_tags {
        for key in extra {
            issues.push(format!(
                "{prefix}.tags[\"{key}\"]: expected <null> actual <set>"
            ));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shortbread_passes_its_specification() {
        let schema = super::super::shortbread();
        let results = schema.verify();
        let failed = results
            .iter()
            .filter(|r| !r.issues.is_empty())
            .map(|r| format!("{}: {}", r.name, r.issues.join("; ")))
            .collect::<Vec<_>>();
        assert_eq!(results.len(), 67);
        assert!(
            failed.is_empty(),
            "{} of {} failed:\n{}",
            failed.len(),
            results.len(),
            failed.join("\n")
        );
    }

    #[test]
    fn reads_the_wkt_the_examples_use() {
        let g = parse_wkt("POLYGON ((-0.5 -0.5, 0.5 -0.5, 0.5 0.5, -0.5 0.5, -0.5 -0.5))").unwrap();
        assert!(matches!(g, Geometry::Polygon(p) if p.exterior().0.len() == 5));
        assert!(matches!(parse_wkt("POINT (1 2)").unwrap(), Geometry::Point(p) if p.x() == 1.0));
        assert!(parse_wkt("POLYGON ((1 2, 3").is_err());
        assert!(parse_wkt("CIRCLE (1 2)").is_err());
    }
}
