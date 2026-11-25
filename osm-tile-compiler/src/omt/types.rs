//! Profile data model. OSM rules live in the profile, not Tactical role enums.
use geo_types::{Coord, Geometry, MultiLineString};
use serde_json::Value;
use std::collections::BTreeMap;

/// A feature's OSM tags, in key order like the `BTreeMap` they replace.
///
/// Features carry a handful of tags, and the profile looks up dozens of keys
/// per feature, most of them absent. A sorted vector found by a linear scan
/// that compares lengths before bytes rejects most keys without touching
/// their bytes, and builds with one allocation instead of a tree's nodes.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Tags(Vec<(String, String)>);

// Above this many tags a lookup binary-searches instead of scanning.
const SCAN_TAGS: usize = 16;

impl Tags {
    pub fn new() -> Self {
        Self(Vec::new())
    }
    fn position(&self, key: &str) -> Result<usize, usize> {
        if self.0.len() <= SCAN_TAGS {
            for (index, (k, _)) in self.0.iter().enumerate() {
                if k.len() == key.len() && k.as_bytes() == key.as_bytes() {
                    return Ok(index);
                }
            }
            // Only inserts need the slot, and they are rare.
            return Err(self.0.partition_point(|(k, _)| k.as_str() < key));
        }
        self.0.binary_search_by(|(k, _)| k.as_str().cmp(key))
    }
    pub fn get(&self, key: &str) -> Option<&String> {
        self.position(key).ok().map(|index| &self.0[index].1)
    }
    pub fn contains_key(&self, key: &str) -> bool {
        self.position(key).is_ok()
    }
    pub fn insert(&mut self, key: String, value: String) -> Option<String> {
        match self.position(&key) {
            Ok(index) => Some(std::mem::replace(&mut self.0[index].1, value)),
            Err(index) => {
                self.0.insert(index, (key, value));
                None
            }
        }
    }
    pub fn remove(&mut self, key: &str) -> Option<String> {
        self.position(key).ok().map(|index| self.0.remove(index).1)
    }
    pub fn len(&self) -> usize {
        self.0.len()
    }
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// The tags in key order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = (&String, &String)> + Clone {
        self.0.iter().map(|(k, v)| (k, v))
    }
    pub fn keys(&self) -> impl DoubleEndedIterator<Item = &String> + Clone {
        self.0.iter().map(|(k, _)| k)
    }
    pub fn values(&self) -> impl DoubleEndedIterator<Item = &String> + Clone {
        self.0.iter().map(|(_, v)| v)
    }
}

impl FromIterator<(String, String)> for Tags {
    /// Later pairs replace earlier ones with the same key, as in a map.
    fn from_iter<I: IntoIterator<Item = (String, String)>>(pairs: I) -> Self {
        let mut tags: Vec<(String, String)> = pairs.into_iter().collect();
        // Stable, so the last of equal keys stays last; keep it.
        tags.sort_by(|a, b| a.0.cmp(&b.0));
        let mut unique: Vec<(String, String)> = Vec::with_capacity(tags.len());
        for pair in tags {
            match unique.last_mut() {
                Some(last) if last.0 == pair.0 => *last = pair,
                _ => unique.push(pair),
            }
        }
        Self(unique)
    }
}

impl Extend<(String, String)> for Tags {
    fn extend<I: IntoIterator<Item = (String, String)>>(&mut self, pairs: I) {
        for (key, value) in pairs {
            self.insert(key, value);
        }
    }
}

impl<'a> IntoIterator for &'a Tags {
    type Item = (&'a String, &'a String);
    type IntoIter = std::iter::Map<
        std::slice::Iter<'a, (String, String)>,
        fn(&'a (String, String)) -> (&'a String, &'a String),
    >;
    fn into_iter(self) -> Self::IntoIter {
        self.0.iter().map(|(k, v)| (k, v))
    }
}

impl<'de> serde::Deserialize<'de> for Tags {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let map = BTreeMap::<String, String>::deserialize(deserializer)?;
        Ok(Self(map.into_iter().collect()))
    }
}

impl std::ops::Index<&str> for Tags {
    type Output = String;
    fn index(&self, key: &str) -> &String {
        self.get(key).expect("no such tag")
    }
}

impl From<BTreeMap<String, String>> for Tags {
    fn from(map: BTreeMap<String, String>) -> Self {
        Self(map.into_iter().collect())
    }
}

impl<const N: usize> From<[(String, String); N]> for Tags {
    fn from(pairs: [(String, String); N]) -> Self {
        pairs.into_iter().collect()
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceKind {
    Node,
    Way,
    Relation,
    Supplement,
}
#[derive(Debug, Clone)]
pub struct Source {
    pub id: u64,
    pub osm_id: i64,
    pub kind: SourceKind,
    pub source_layer: String,
    pub tags: Tags,
    pub geometry: Geometry<f64>,
    pub relations: Vec<Tags>,
}
impl Source {
    pub fn tag(&self, key: &str) -> &str {
        self.tags.get(key).map(String::as_str).unwrap_or("")
    }
    pub fn has(&self, key: &str) -> bool {
        self.tags.contains_key(key)
    }
    pub fn is_point(&self) -> bool {
        matches!(self.geometry, Geometry::Point(_) | Geometry::MultiPoint(_))
    }
    pub fn is_line(&self) -> bool {
        matches!(
            self.geometry,
            Geometry::LineString(_) | Geometry::MultiLineString(_)
        )
    }
    pub fn is_polygon(&self) -> bool {
        matches!(
            self.geometry,
            Geometry::Polygon(_) | Geometry::MultiPolygon(_)
        )
    }
    pub fn point(&self) -> Option<Geometry<f64>> {
        super::interior_point::point(&self.geometry).map(Geometry::Point)
    }
    pub fn line(&self) -> Option<Geometry<f64>> {
        match &self.geometry {
            Geometry::LineString(_) | Geometry::MultiLineString(_) => Some(self.geometry.clone()),
            Geometry::Polygon(p) => Some(Geometry::MultiLineString(MultiLineString(
                std::iter::once(p.exterior().clone())
                    .chain(p.interiors().iter().cloned())
                    .collect(),
            ))),
            Geometry::MultiPolygon(p) => Some(Geometry::MultiLineString(MultiLineString(
                p.0.iter()
                    .flat_map(|p| {
                        std::iter::once(p.exterior().clone()).chain(p.interiors().iter().cloned())
                    })
                    .collect(),
            ))),
            _ => None,
        }
    }
}

/// The top of a feature's zooms when the profile leaves it open, as
/// Planetiler's default `maxzoom` does: the feature reaches the archive's
/// highest zoom, whatever that is. Rendering stops at the archive's top, so a
/// zoom 14 archive gets exactly what it got when this was 14.
pub const TOP: u8 = crate::ZoomRange::LIMIT;

#[derive(Debug, Clone)]
pub struct Feature {
    pub id: u64,
    pub layer: String,
    pub geometry: Geometry<f64>,
    /// Original OSM outline coordinates require reference node-store quantization.
    pub osm_outline_projection: bool,
    /// A generated OSM label is already in normalized world coordinates.
    /// Keep it there through rendering so half-grid ties survive projection.
    pub world_point: Option<Coord<f64>>,
    pub attrs: BTreeMap<String, Value>,
    pub attr_minzoom: BTreeMap<String, u8>,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// Screen pixels on a 256-pixel tile, not MVT extent units.
    pub buffer_pixels: f64,
    pub min_size: f64,
    pub min_size_at_max_zoom: f64,
    pub min_size_source_geometry: Option<Geometry<f64>>,
    pub tolerance: f64,
    pub tolerance_at_max_zoom: f64,
    pub sort_key: i64,
    pub grid_size: f64,
    pub grid_limit: usize,
    pub grid_max_zoom: u8,
    pub grid_limits_by_zoom: Vec<(u8, usize)>,
    pub grid_size_by_zoom: Vec<(u8, f64)>,
    pub rank: bool,
}
impl Feature {
    pub fn new_world_point(
        source: &Source,
        layer: &str,
        world: Coord<f64>,
        min_zoom: u8,
        max_zoom: u8,
    ) -> Self {
        let geometry = Geometry::Point(super::interior_point::world_to_latlon(world));
        let mut feature = Self::new(source, layer, geometry, min_zoom, max_zoom);
        feature.world_point = Some(world);
        feature
    }

    /// Construct a feature from original source vertices (or extracted rings).
    /// Do not use for generated label points or supplemental lake centerlines.
    pub fn new_original_geometry(
        source: &Source,
        layer: &str,
        geometry: Geometry<f64>,
        min_zoom: u8,
        max_zoom: u8,
    ) -> Self {
        let quantized = matches!(source.kind, SourceKind::Way | SourceKind::Relation)
            && matches!(
                &geometry,
                Geometry::LineString(_)
                    | Geometry::MultiLineString(_)
                    | Geometry::Polygon(_)
                    | Geometry::MultiPolygon(_)
            );
        let mut feature = Self::new(source, layer, geometry, min_zoom, max_zoom);
        feature.osm_outline_projection = quantized;
        feature
    }

    pub fn new(
        source: &Source,
        layer: &str,
        geometry: Geometry<f64>,
        min_zoom: u8,
        max_zoom: u8,
    ) -> Self {
        Self {
            id: source.id,
            layer: layer.into(),
            geometry,
            osm_outline_projection: false,
            world_point: None,
            attrs: BTreeMap::new(),
            attr_minzoom: BTreeMap::new(),
            min_zoom,
            max_zoom,
            buffer_pixels: 4.0,
            min_size: 1.0,
            min_size_at_max_zoom: 0.0625,
            min_size_source_geometry: None,
            tolerance: 0.1,
            tolerance_at_max_zoom: 0.0625,
            sort_key: 0,
            grid_size: 0.0,
            grid_limit: 0,
            grid_max_zoom: 14,
            grid_limits_by_zoom: vec![],
            grid_size_by_zoom: vec![],
            rank: false,
        }
    }
}

/// Pinned Planetiler `Parse.meters` for a string tag. It searches rather than
/// matching the whole tag, so a decimal comma is not a decimal separator:
/// "2,5" finds the trailing "5". Do not replace this with locale
/// normalization.
pub fn java_meters(tag: &str) -> Option<f64> {
    static DISTANCE: std::sync::LazyLock<regex::Regex> = std::sync::LazyLock::new(|| {
        regex::Regex::new(r#"(?i)(?P<value>-?[0-9.]+)\s*((?P<mi>mi)|(?P<m>m|$)|(?P<km>km|kilom)|(?P<ft>ft|')|(?P<inch>in|")|(?P<nmi>nmi|international nautical mile|nautical))"#).unwrap()
    });
    // Every match starts with a digit or a point; most tags are empty.
    if !tag.bytes().any(|b| b.is_ascii_digit() || b == b'.') {
        return None;
    }
    let mut matches = DISTANCE.captures_iter(tag);
    let first = matches.next()?;
    let mut value = first.name("value")?.as_str().parse::<f64>().ok()?;
    if first.name("m").is_some() {
        // Already meters (including a unitless number at the end of the tag).
    } else if first.name("km").is_some() {
        value *= 1000.0;
    } else if first.name("mi").is_some() {
        value *= 1609.344;
    } else if first.name("nmi").is_some() {
        value *= 1852.0;
    } else if first.name("ft").is_some() {
        value *= 12.0 * 0.0254;
        if let Some(inches) = matches.next().filter(|m| m.name("inch").is_some()) {
            value += inches.name("value")?.as_str().parse::<f64>().ok()? * 0.0254;
        }
    } else if first.name("inch").is_some() {
        value *= 0.0254;
    } else {
        return None;
    }
    Some(value)
}

/// Java `Double.parseDouble`: surrounding control characters and spaces are
/// ignored and a trailing `d` or `f` type suffix is allowed. Hexadecimal
/// literals, which Java also accepts, are not.
fn java_parse_double(tag: &str) -> Option<f64> {
    let tag = tag.trim_matches(|c: char| c <= ' ');
    let (negative, body) = match tag.as_bytes().first() {
        Some(b'-') => (true, &tag[1..]),
        Some(b'+') => (false, &tag[1..]),
        _ => (false, tag),
    };
    let value = match body {
        "NaN" => f64::NAN,
        "Infinity" => f64::INFINITY,
        _ => {
            let body = body.strip_suffix(['d', 'D', 'f', 'F']).unwrap_or(body);
            let bytes = body.as_bytes();
            let digits = |mut i: usize| {
                while bytes.get(i).is_some_and(u8::is_ascii_digit) {
                    i += 1;
                }
                i
            };
            let mut i = digits(0);
            let mut mantissa = i;
            if bytes.get(i) == Some(&b'.') {
                let end = digits(i + 1);
                mantissa += end - i - 1;
                i = end;
            }
            if mantissa == 0 {
                return None;
            }
            if matches!(bytes.get(i), Some(b'e' | b'E')) {
                i += 1;
                if matches!(bytes.get(i), Some(b'+' | b'-')) {
                    i += 1;
                }
                let end = digits(i);
                if end == i {
                    return None;
                }
                i = end;
            }
            if i != bytes.len() {
                return None;
            }
            body.parse::<f64>().ok()?
        }
    };
    Some(if negative { -value } else { value })
}

pub fn parse_number(value: &str) -> Option<f64> {
    value
        .trim()
        .replace(',', ".")
        .trim_end_matches(" m")
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}
/// Pinned Planetiler `Parse.parseLongOrNull` numeric-prefix fallback for OSM
/// level/layer tags. The direct integer case remains exact; decimal and
/// grouping cases follow Locale.ROOT NumberFormat's observed prefix policy.
pub fn parse_profile_long(raw: &str) -> Option<i64> {
    if raw.is_empty() {
        return None;
    }
    if let Ok(value) = raw.parse::<i64>() {
        return Some(value);
    }
    if raw.starts_with("NaN") {
        return Some(0);
    }
    number_format_prefix(raw)?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
        .map(|v| v as i64)
}

/// Pinned Planetiler `Parse.parseDoubleOrNull`: `Double.parseDouble`, then
/// the same Locale.ROOT `NumberFormat` prefix fallback as
/// [`parse_profile_long`]. A decimal comma is a grouping separator there, so
/// "0,5" is 5.
pub fn parse_profile_double(raw: &str) -> Option<f64> {
    if let Some(value) = java_parse_double(raw) {
        return Some(value);
    }
    if raw.starts_with("NaN") {
        return Some(f64::NAN);
    }
    number_format_prefix(raw)?.parse::<f64>().ok()
}

/// The numeric prefix Locale.ROOT `NumberFormat.parse` reads from `raw`, as
/// Rust float syntax, or `None` when it reads no digit.
fn number_format_prefix(raw: &str) -> Option<String> {
    // Locale.ROOT NumberFormat does not accept a leading plus sign, although
    // Long.parseLong and Double.parseDouble do.
    if raw.starts_with('+') {
        return None;
    }
    let mut prefix = String::new();
    let mut decimal = false;
    let mut digit = false;
    for (i, c) in raw.chars().enumerate() {
        match c {
            '-' if i == 0 => prefix.push(c),
            '0'..='9' => {
                prefix.push(c);
                digit = true;
            }
            ',' if !decimal => {} // Locale.ROOT grouping separator.
            '.' if !decimal => {
                prefix.push(c);
                decimal = true;
            }
            'E' if digit => {
                let rest = &raw[i + 1..];
                let exponent = rest.strip_prefix('-').unwrap_or(rest);
                let count = exponent.bytes().take_while(u8::is_ascii_digit).count();
                if count > 0 {
                    prefix.push('E');
                    if rest.starts_with('-') {
                        prefix.push('-');
                    }
                    prefix.push_str(&exponent[..count]);
                }
                break;
            }
            _ => break,
        }
    }
    digit.then_some(prefix)
}
pub fn add_names(source: &Source, feature: &mut Feature) {
    super::labels::add_profile_names(source, feature);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_profile_double_matches_planetiler() {
        // Expected values printed by Planetiler 0.10.2's Parse.parseDoubleOrNull.
        for (raw, expected) in [
            ("2", 2.0),
            (" 2.5\t", 2.5),
            ("+.5", 0.5),
            ("1.", 1.0),
            ("-1e2", -100.0),
            ("3d", 3.0),
            ("4.5F", 4.5),
            ("Infinity", f64::INFINITY),
            ("-Infinity", f64::NEG_INFINITY),
            ("0,5", 5.0),
            ("1e", 1.0),
            ("1.5m", 1.5),
            ("1 2", 1.0),
            ("1dd", 1.0),
            ("1d", 1.0),
        ] {
            assert_eq!(parse_profile_double(raw), Some(expected), "{raw}");
        }
        for raw in ["NaN", "NaNd"] {
            assert!(parse_profile_double(raw).unwrap().is_nan(), "{raw}");
        }
        for raw in ["", ".", "e5", "inf", "nan", "infinity", "x"] {
            assert_eq!(parse_profile_double(raw), None, "{raw}");
        }
    }

    #[test]
    fn java_meters_searches_like_parse_meters() {
        // Expected values printed by Planetiler 0.10.2's Parse.meters.
        assert_eq!(java_meters("2,5"), Some(5.0));
        assert_eq!(java_meters("12m"), Some(12.0));
        assert_eq!(java_meters("1.2 km"), Some(1200.0));
        assert_eq!(java_meters("10 ft"), Some(10.0 * (12.0 * 0.0254)));
        assert_eq!(
            java_meters("5'6\""),
            Some(5.0 * (12.0 * 0.0254) + 6.0 * 0.0254)
        );
        assert_eq!(java_meters("12 xyz"), None);
        assert_eq!(java_meters("241..4"), None);
    }

    #[test]
    fn tags_behave_like_the_map_they_replace() {
        let pairs = |n: usize| (0..n).map(move |i| (format!("k{:02}", (i * 7) % n), i.to_string()));
        for n in [0, 3, super::SCAN_TAGS, super::SCAN_TAGS + 9] {
            let map: BTreeMap<String, String> = pairs(n).collect();
            let mut tags: Tags = pairs(n).collect();
            assert!(tags.iter().eq(map.iter()), "{n} tags iterate in key order");
            for key in map.keys() {
                assert_eq!(tags.get(key), map.get(key));
                assert!(tags.contains_key(key));
            }
            for absent in ["", "k", "k0", "k999", "zz", "a"] {
                assert_eq!(tags.get(absent), None, "{absent}");
            }
            let mut map = map;
            for (key, value) in [("k01", "x"), ("a", "y"), ("zz", "z"), ("k015", "w")] {
                assert_eq!(
                    tags.insert(key.into(), value.into()),
                    map.insert(key.into(), value.into())
                );
            }
            assert_eq!(tags.remove("a"), map.remove("a"));
            assert_eq!(tags.remove("a"), None);
            assert!(tags.iter().eq(map.iter()));
            assert_eq!(tags.len(), map.len());
        }
        // A repeated key keeps its last value, as collecting into a map does.
        let tags: Tags = [("a", "1"), ("b", "2"), ("a", "3")]
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .into_iter()
            .collect();
        assert_eq!(tags.get("a").map(String::as_str), Some("3"));
        assert_eq!(tags.len(), 2);
    }
}
