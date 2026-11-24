//! A configured schema as a [`Profile`]: each source is tested against every
//! feature of every layer, in the schema's order, and each feature that
//! matches makes one output feature.
use super::matcher::MatchKeys;
use super::value::{Val, number};
use super::{AttributeDef, FeatureDef, GeometryKind, Schema, SourceInput};
use crate::omt::interior_point;
use crate::omt::profile::{FeatureFilter, NoContribution, Profile};
use crate::omt::types::{Feature, Source, SourceKind};
use geo::{Centroid, CoordsIter};
use geo_types::{Coord, Geometry, LineString, Point};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

/// Evaluates a configured schema.
pub(crate) struct SchemaProfile {
    schema: Arc<Schema>,
    /// The highest zoom written, which features end at by default.
    max_zoom: u8,
    /// Whether OSM coordinates are taken as Planetiler's node store packs
    /// them, as they are when it reads a PBF; its schema examples use them
    /// as written.
    packed: bool,
}

impl SchemaProfile {
    pub fn new(schema: Arc<Schema>, max_zoom: u8) -> Self {
        Self {
            schema,
            max_zoom,
            packed: true,
        }
    }

    /// For a schema's examples, whose geometries are taken as written.
    pub fn for_examples(schema: Arc<Schema>, max_zoom: u8) -> Self {
        Self {
            packed: false,
            ..Self::new(schema, max_zoom)
        }
    }

    /// The schema source a compile source comes from.
    fn source_index(&self, source: &Source) -> Option<usize> {
        let input = match source.kind {
            SourceKind::Supplement if source.source_layer == "water_polygons" => {
                SourceInput::WaterPolygons
            }
            SourceKind::Supplement => return None,
            _ => SourceInput::Osm,
        };
        self.schema.sources.iter().position(|s| s.input == input)
    }

    /// Appends the features `source` makes, in the schema's order.
    pub fn features(&self, source: &Source, out: &mut Vec<Feature>) {
        let Some(source_index) = self.source_index(source) else {
            return;
        };
        let source_id = self.schema.sources[source_index].id.as_str();
        let shape = Shape::of(source);
        let mut keys = MatchKeys::new();
        for def in &self.schema.features {
            if !def.sources.is_empty() && !def.sources.contains(&source_index) {
                continue;
            }
            if !shape.matches(def.geometry) {
                continue;
            }
            if let Some(required) = &def.required_keys
                && !required.iter().any(|key| source.tags.contains_key(key))
            {
                continue;
            }
            keys.clear();
            let scope = FeatureScope {
                source,
                source_id,
                schema: &self.schema,
                match_key: None,
                value: None,
                packed: self.packed,
            };
            if !def.test.eval(&scope, &mut keys) {
                continue;
            }
            let match_key = keys.first().cloned().flatten();
            let scope = FeatureScope {
                match_key: match_key.as_deref(),
                ..scope
            };
            if let Some(feature) = self.feature(def, source, &scope) {
                out.push(feature);
            }
        }
    }

    fn feature(&self, def: &FeatureDef, source: &Source, scope: &FeatureScope) -> Option<Feature> {
        let layer = &self.schema.layers[def.layer].id;
        let mut feature = make_geometry(def.geometry, source, layer, self.max_zoom, self.packed)?;
        for attribute in &def.attributes {
            set_attribute(&mut feature, attribute, source, scope);
        }
        let int = |expr: &Option<super::matcher::ValueExpr>| match expr.as_ref()?.eval(scope) {
            Val::Int(i) => Some(i),
            _ => None,
        };
        let double = |expr: &Option<super::matcher::ValueExpr>| {
            number(&expr.as_ref()?.eval(scope)).filter(|v| v.is_finite())
        };
        if let Some(id) = int(&def.id) {
            feature.id = id as u64;
        }
        if let Some(zoom) = int(&def.min_zoom) {
            feature.min_zoom = zoom.clamp(0, 255) as u8;
        }
        if let Some(zoom) = int(&def.max_zoom) {
            feature.max_zoom = zoom.clamp(0, 255) as u8;
        }
        if let Some(buffer) = def.buffer {
            feature.buffer_pixels = buffer;
        }
        if let Some(size) = double(&def.min_size) {
            feature.min_size = size;
        }
        if let Some(size) = double(&def.min_size_at_max_zoom) {
            feature.min_size_at_max_zoom = size;
        }
        if let Some(tolerance) = double(&def.tolerance) {
            feature.tolerance = tolerance;
        }
        if let Some(tolerance) = double(&def.tolerance_at_max_zoom) {
            feature.tolerance_at_max_zoom = tolerance;
        }
        (feature.min_zoom <= feature.max_zoom).then_some(feature)
    }
}

/// Planetiler's attribute processor: the value when the attribute's test
/// passes (an empty string counts as none), else the `else` value, written
/// from its minimum zoom.
fn set_attribute(feature: &mut Feature, def: &AttributeDef, source: &Source, scope: &FeatureScope) {
    let mut value = Val::Null;
    if def.test.eval(scope, &mut Vec::new()) {
        value = def.value.eval(scope);
        if matches!(&value, Val::Str(s) if s.is_empty()) {
            value = Val::Null;
        }
    }
    if value == Val::Null {
        value = def.fallback.clone();
    }
    let Some(attribute) = value.to_attribute() else {
        return;
    };
    let mut min_zoom = None;
    if def.min_zoom.is_some() || !def.min_zoom_by_value.is_empty() {
        let text = value.text();
        min_zoom = match def.min_zoom_by_value.iter().find(|(v, _)| *v == text) {
            Some((_, zoom)) => Some(*zoom),
            None => {
                let attribute_scope = FeatureScope {
                    value: Some(&value),
                    ..*scope
                };
                let configured = match def.min_zoom.as_ref().map(|z| z.eval(&attribute_scope)) {
                    Some(Val::Int(z)) => z,
                    _ => 0,
                };
                let cover = min_zoom_from_tile_cover(source, scope.packed, def.min_tile_cover_size);
                Some(configured.max(cover).clamp(0, 255) as u8)
            }
        };
    }
    feature.attrs.insert(def.key.clone(), attribute);
    match min_zoom {
        Some(zoom) if zoom > 0 => {
            feature.attr_minzoom.insert(def.key.clone(), zoom);
        }
        _ => {
            feature.attr_minzoom.remove(&def.key);
        }
    }
}

/// The zoom from which a polygon covers `share` of a tile: Planetiler's
/// `(int) (log(share / area) / log(4))`, the area in world units.
fn min_zoom_from_tile_cover(source: &Source, packed: bool, share: Option<f64>) -> i64 {
    let Some(share) = share else { return 0 };
    let area = if source.is_polygon() {
        world_area(&world_geometry(source, packed))
    } else {
        0.0
    };
    let zoom = (share / area).ln() / 4f64.ln();
    if zoom.is_nan() {
        0
    } else {
        // Java's (int) cast saturates.
        zoom.clamp(i32::MIN as f64, i32::MAX as f64) as i64
    }
}

impl Profile for SchemaProfile {
    type Contribution = NoContribution;

    fn process(&mut self, source: &Source, features: &mut Vec<Feature>) {
        self.features(source, features);
    }

    fn evaluate(&self, source: &Source, features: &mut Vec<Feature>, _: &mut NoContribution) {
        self.features(source, features);
    }

    fn apply(&mut self, _: NoContribution) {}

    fn finish(&mut self, _: &mut Vec<Feature>) {}
}

/// Which geometry tests a source passes.
struct Shape {
    point: bool,
    line: bool,
    polygon: bool,
    /// The line view of a closed way that also has a polygon view, which
    /// geometry `any` and the other any-geometry kinds skip: Planetiler sees
    /// one feature there, which is a polygon.
    shadowed_line: bool,
}

impl Shape {
    fn of(source: &Source) -> Self {
        let shadowed_line = match &source.geometry {
            Geometry::LineString(line) if source.kind == SourceKind::Way => {
                line.is_closed() && line.0.len() >= 4 && source.tag("area") != "no"
            }
            _ => false,
        };
        Self {
            point: source.is_point(),
            line: source.is_line(),
            polygon: source.is_polygon(),
            shadowed_line,
        }
    }

    fn matches(&self, kind: GeometryKind) -> bool {
        use GeometryKind::*;
        match kind {
            Point => self.point,
            Line | LineCentroid | LineMidpoint | PointOnLine => self.line,
            Polygon | PolygonCentroid | PolygonCentroidIfConvex | PolygonPointOnSurface => {
                self.polygon
            }
            Any | Centroid | InnermostPoint => !self.shadowed_line,
        }
    }
}

/// The output feature with its geometry, or `None` when none can be made
/// (an empty or degenerate source).
fn make_geometry(
    kind: GeometryKind,
    source: &Source,
    layer: &str,
    max_zoom: u8,
    packed: bool,
) -> Option<Feature> {
    use GeometryKind as G;
    let osm_outline = packed && matches!(source.kind, SourceKind::Way | SourceKind::Relation);
    let point_feature = |world: Option<Coord<f64>>, latlon: Option<Point<f64>>| match world {
        Some(world) => Some(Feature::new_world_point(source, layer, world, 0, max_zoom)),
        None => latlon.map(|p| Feature::new(source, layer, Geometry::Point(p), 0, max_zoom)),
    };
    // Planetiler reads a node's location from its node store, packed into
    // 31-bit world coordinates, as it does the nodes of ways.
    let node_point = || match (&source.kind, &source.geometry) {
        (SourceKind::Node, Geometry::Point(_)) if packed => {
            match interior_point::projected_osm(&source.geometry) {
                Geometry::Point(world) => Some(Feature::new_world_point(
                    source, layer, world.0, 0, max_zoom,
                )),
                _ => unreachable!("projecting a point preserves its geometry type"),
            }
        }
        _ => Some(Feature::new(
            source,
            layer,
            source.geometry.clone(),
            0,
            max_zoom,
        )),
    };
    match kind {
        G::Point => node_point(),
        G::Line | G::Polygon => Some(Feature::new_original_geometry(
            source,
            layer,
            source.geometry.clone(),
            0,
            max_zoom,
        )),
        G::Any => {
            if source.is_point() {
                node_point()
            } else {
                Some(Feature::new_original_geometry(
                    source,
                    layer,
                    source.geometry.clone(),
                    0,
                    max_zoom,
                ))
            }
        }
        G::PolygonCentroid | G::LineCentroid | G::Centroid => {
            if source.is_point() {
                return node_point();
            }
            if osm_outline {
                point_feature(interior_point::osm_centroid_world(&source.geometry), None)
            } else {
                point_feature(None, interior_point::centroid(&source.geometry))
            }
        }
        G::PolygonPointOnSurface | G::PointOnLine | G::InnermostPoint => {
            if source.is_point() {
                return node_point();
            }
            if osm_outline {
                point_feature(interior_point::osm_point_world(&source.geometry), None)
            } else {
                point_feature(None, interior_point::point(&source.geometry))
            }
        }
        G::PolygonCentroidIfConvex => {
            if osm_outline {
                point_feature(
                    interior_point::osm_point_then_centroid_if_convex_world(&source.geometry),
                    None,
                )
            } else {
                point_feature(
                    None,
                    interior_point::point_then_centroid_if_convex(&source.geometry),
                )
            }
        }
        G::LineMidpoint => {
            let world = if osm_outline {
                interior_point::projected_osm(&source.geometry)
            } else {
                interior_point::projected(&source.geometry)
            };
            point_feature(midpoint(&world), None)
        }
    }
}

/// The point halfway along a line, in world coordinates; for a multiline,
/// halfway along its longest part.
fn midpoint(world: &Geometry<f64>) -> Option<Coord<f64>> {
    let length = |line: &LineString<f64>| {
        line.lines()
            .map(|l| (l.dx().powi(2) + l.dy().powi(2)).sqrt())
            .sum::<f64>()
    };
    let line = match world {
        Geometry::LineString(line) => line,
        Geometry::MultiLineString(lines) => lines
            .0
            .iter()
            .max_by(|a, b| length(a).total_cmp(&length(b)))?,
        _ => return None,
    };
    let half = length(line) / 2.0;
    let mut walked = 0.0;
    for segment in line.lines() {
        let l = (segment.dx().powi(2) + segment.dy().powi(2)).sqrt();
        if walked + l >= half && l > 0.0 {
            let t = (half - walked) / l;
            return Some(Coord {
                x: segment.start.x + t * segment.dx(),
                y: segment.start.y + t * segment.dy(),
            });
        }
        walked += l;
    }
    line.0.first().copied()
}

/// A source's geometry in world units (the whole world is 1). Planetiler
/// reads OSM geometries from its node store, packed into 31-bit world
/// coordinates, so their measures come from those.
fn world_geometry(source: &Source, packed: bool) -> Geometry<f64> {
    match source.kind {
        SourceKind::Supplement => interior_point::projected(&source.geometry),
        _ if !packed => interior_point::projected(&source.geometry),
        _ => interior_point::projected_osm(&source.geometry),
    }
}

/// A source's geometry in degrees, as Planetiler's `latLonGeometry()`: an
/// OSM geometry's packed world coordinates projected back.
fn latlon_geometry(source: &Source, packed: bool) -> Geometry<f64> {
    match source.kind {
        SourceKind::Supplement => source.geometry.clone(),
        _ if !packed => source.geometry.clone(),
        _ => {
            use geo::MapCoords;
            world_geometry(source, true).map_coords(|c| interior_point::world_to_latlon(c).0)
        }
    }
}

/// Area of a world geometry, as Planetiler's `SourceFeature.area()`.
fn world_area(world: &Geometry<f64>) -> f64 {
    use geo::Area;
    world.unsigned_area()
}

const AVERAGE_WORLD_RADIUS_METERS: f64 = 6_371_008.8;

/// Planetiler's `GeoUtils.ringAreaMeters`: a ring's area on a sphere.
fn ring_area_meters(ring: &LineString<f64>) -> f64 {
    let coords = &ring.0;
    let edges = coords.len().saturating_sub(1);
    if edges < 2 {
        return 0.0;
    }
    let mut total = 0.0;
    for i in 0..edges {
        let lower = coords[i].x.to_radians();
        let middle = coords[if i + 1 == edges { 0 } else { i + 1 }]
            .y
            .to_radians();
        let upper = coords[if i + 2 >= edges {
            (i + 2) % edges
        } else {
            i + 2
        }]
        .x
        .to_radians();
        total += (upper - lower) * middle.sin();
    }
    // Planetiler's AREA_FACTOR, folded first as Java does.
    const AREA_FACTOR: f64 = AVERAGE_WORLD_RADIUS_METERS * AVERAGE_WORLD_RADIUS_METERS / 2.0;
    total.abs() * AREA_FACTOR
}

/// Planetiler's `GeoUtils.areaInMeters`.
fn area_meters(geometry: &Geometry<f64>) -> f64 {
    match geometry {
        // Holes are subtracted one by one, in Planetiler's order of rounding.
        Geometry::Polygon(p) => p
            .interiors()
            .iter()
            .fold(ring_area_meters(p.exterior()), |area, hole| {
                area - ring_area_meters(hole)
            }),
        Geometry::MultiPolygon(m) => {
            m.0.iter()
                .map(|p| area_meters(&Geometry::Polygon(p.clone())))
                .sum()
        }
        _ => 0.0,
    }
}

/// Planetiler's `GeoUtils.lengthInMeters`: the haversine length of lines.
fn length_meters(geometry: &Geometry<f64>) -> f64 {
    let line = |line: &LineString<f64>| {
        line.lines()
            .map(|segment| {
                let (a, b) = (segment.start, segment.end);
                // GeoUtils.metersBetween, term for term.
                const RADIANS_PER_DEGREE: f64 = std::f64::consts::PI / 180.0;
                let sin_lat = ((b.y - a.y) * RADIANS_PER_DEGREE / 2.0).sin();
                let sin_lon = ((b.x - a.x) * RADIANS_PER_DEGREE / 2.0).sin();
                let h = sin_lat * sin_lat
                    + sin_lon
                        * sin_lon
                        * (a.y * RADIANS_PER_DEGREE).cos()
                        * (b.y * RADIANS_PER_DEGREE).cos();
                AVERAGE_WORLD_RADIUS_METERS * 2.0 * h.sqrt().asin()
            })
            .sum::<f64>()
    };
    match geometry {
        Geometry::LineString(l) => line(l),
        Geometry::MultiLineString(m) => m.0.iter().map(line).sum(),
        _ => 0.0,
    }
}

/// Meters per unit of length, or `None` for the z0 units.
fn length_unit(name: &str) -> Option<LengthUnit> {
    let name = name.trim().to_ascii_lowercase();
    Some(match name.as_str() {
        "m" | "meter" | "meters" | "metre" | "metres" => LengthUnit::Meters(1.0),
        "km" | "kilometer" | "kilometers" | "kilometre" | "kilometres" => {
            LengthUnit::Meters(1000.0)
        }
        "ft" | "foot" | "feet" => LengthUnit::Meters(0.3048),
        "yd" | "yard" | "yards" => LengthUnit::Meters(0.9144),
        "mi" | "mile" | "miles" => LengthUnit::Meters(1609.344),
        "nm" | "nautical mile" | "nautical miles" => LengthUnit::Meters(1852.0),
        "z0 px" | "z0 pixel" | "z0 pixels" => LengthUnit::World(256.0),
        "z0 ti" | "z0 tile" | "z0 tiles" => LengthUnit::World(1.0),
        _ => return None,
    })
}

#[derive(Clone, Copy)]
enum LengthUnit {
    /// Meters in one unit.
    Meters(f64),
    /// Units across the world.
    World(f64),
}

fn area_unit(name: &str) -> Option<AreaUnit> {
    let name = name.trim().to_ascii_lowercase();
    Some(match name.as_str() {
        "a" | "are" | "ares" => AreaUnit::SquareMeters(100.0),
        "ha" | "hectare" | "hectares" => AreaUnit::SquareMeters(10_000.0),
        "ac" | "acre" | "acres" => AreaUnit::SquareMeters(4_046.856_422_4),
        other => {
            let base = other
                .strip_suffix('2')
                .or_else(|| other.strip_prefix("square "))?
                .trim();
            match length_unit(base)? {
                LengthUnit::Meters(m) => AreaUnit::SquareMeters(m * m),
                LengthUnit::World(w) => AreaUnit::World(w * w),
            }
        }
    })
}

#[derive(Clone, Copy)]
enum AreaUnit {
    SquareMeters(f64),
    World(f64),
}

/// What expressions read about the feature being evaluated.
#[derive(Clone, Copy)]
pub(crate) struct FeatureScope<'a> {
    pub source: &'a Source,
    pub source_id: &'a str,
    pub schema: &'a Schema,
    pub match_key: Option<&'a str>,
    /// The attribute's value, in an attribute's `min_zoom`.
    pub value: Option<&'a Val>,
    /// As [`SchemaProfile::packed`].
    pub packed: bool,
}

impl super::expr::Scope for FeatureScope<'_> {
    fn tag(&self, key: &str) -> Option<&str> {
        self.source.tags.get(key).map(String::as_str)
    }

    fn tags(&self) -> BTreeMap<String, Val> {
        self.source
            .tags
            .iter()
            .filter(|(k, _)| !k.starts_with('_'))
            .map(|(k, v)| (k.clone(), Val::Str(v.clone())))
            .collect()
    }

    fn feature(&self, name: &str) -> Option<Val> {
        let source = self.source;
        let osm = !matches!(source.kind, SourceKind::Supplement);
        Some(match name {
            "id" => Val::Int(source.osm_id),
            "source" => Val::Str(self.source_id.into()),
            "source_layer" => {
                if osm {
                    Val::Null
                } else {
                    Val::Str(source.source_layer.clone())
                }
            }
            "osm_type" => match source.kind {
                SourceKind::Node => Val::Str("node".into()),
                SourceKind::Way => Val::Str("way".into()),
                SourceKind::Relation => Val::Str("relation".into()),
                SourceKind::Supplement => Val::Null,
            },
            "min_lat" | "max_lat" | "min_lon" | "max_lon" => {
                use geo::BoundingRect;
                let rect = source.geometry.bounding_rect()?;
                Val::Double(match name {
                    "min_lat" => rect.min().y,
                    "max_lat" => rect.max().y,
                    "min_lon" => rect.min().x,
                    _ => rect.max().x,
                })
            }
            "lat" | "lon" => {
                let point = source
                    .geometry
                    .coords_iter()
                    .next()
                    .map(Point::from)
                    .or_else(|| source.geometry.centroid())?;
                Val::Double(if name == "lat" { point.y() } else { point.x() })
            }
            _ => return None,
        })
    }

    fn feature_call(&self, name: &str, args: &[Val]) -> Option<Val> {
        let [Val::Str(unit)] = args else { return None };
        match name {
            "area" => {
                if !self.source.is_polygon() {
                    return Some(Val::Double(0.0));
                }
                Some(Val::Double(match area_unit(unit)? {
                    AreaUnit::SquareMeters(per) => {
                        area_meters(&latlon_geometry(self.source, self.packed)) / per
                    }
                    AreaUnit::World(per) => {
                        world_area(&world_geometry(self.source, self.packed)) * per
                    }
                }))
            }
            "length" => {
                if !self.source.is_line() {
                    return Some(Val::Double(0.0));
                }
                Some(Val::Double(match length_unit(unit)? {
                    LengthUnit::Meters(per) => {
                        length_meters(&latlon_geometry(self.source, self.packed)) / per
                    }
                    LengthUnit::World(per) => {
                        use geo::{Euclidean, Length};
                        let world = world_geometry(self.source, self.packed);
                        let length = match &world {
                            Geometry::LineString(l) => Euclidean.length(l),
                            Geometry::MultiLineString(m) => Euclidean.length(m),
                            _ => 0.0,
                        };
                        length * per
                    }
                }))
            }
            _ => None,
        }
    }

    fn variable(&self, name: &str) -> Option<Val> {
        match name {
            "match_key" => Some(self.match_key.map_or(Val::Null, |k| Val::Str(k.into()))),
            "match_value" => Some(
                self.match_key
                    .and_then(|key| self.source.tags.get(key))
                    .map_or(Val::Null, |v| Val::Str(v.clone())),
            ),
            "value" => self.value.cloned(),
            "args" => Some(Val::Map(self.schema.args.clone())),
            _ => None,
        }
    }
}

/// The root scope, where only `args` is defined: argument defaults.
pub(crate) struct ArgsScope<'a>(pub &'a BTreeMap<String, Val>);

impl super::expr::Scope for ArgsScope<'_> {
    fn tag(&self, _: &str) -> Option<&str> {
        None
    }
    fn tags(&self) -> BTreeMap<String, Val> {
        BTreeMap::new()
    }
    fn feature(&self, _: &str) -> Option<Val> {
        None
    }
    fn feature_call(&self, _: &str, _: &[Val]) -> Option<Val> {
        None
    }
    fn variable(&self, name: &str) -> Option<Val> {
        (name == "args").then(|| Val::Map(self.0.clone()))
    }
}

/// The layers a compile writes of a configured schema.
#[derive(Clone)]
pub(crate) struct LayerFilter {
    layers: BTreeSet<String>,
}

impl LayerFilter {
    /// Every layer of `schema`, or those of `only`, which must all be layers
    /// of it.
    pub fn new(schema: &Schema, only: Option<&[String]>) -> Result<Self, String> {
        let all = schema.layers();
        let layers = match only {
            None => all.iter().map(|l| l.to_string()).collect(),
            Some(only) => {
                for name in only {
                    if !all.contains(&name.as_str()) {
                        return Err(format!(
                            "unknown layer '{name}'; the {} schema's layers are {}",
                            schema.name,
                            all.join(", ")
                        ));
                    }
                }
                only.iter().cloned().collect()
            }
        };
        Ok(Self { layers })
    }

    /// The layers written, in the schema's order.
    pub fn layers<'a>(&self, schema: &'a Schema) -> Vec<&'a str> {
        schema
            .layers()
            .into_iter()
            .filter(|l| self.layers.contains(*l))
            .collect()
    }
}

impl FeatureFilter for LayerFilter {
    fn keep(&self, feature: &mut Feature) -> bool {
        self.layers.contains(&feature.layer)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spherical_area_matches_planetilers_example() {
        // shortbread.spec.yml: a 1° square on the equator.
        let square = Geometry::Polygon(geo_types::Polygon::new(
            LineString::from(vec![
                (-0.5, -0.5),
                (0.5, -0.5),
                (0.5, 0.5),
                (-0.5, 0.5),
                (-0.5, -0.5),
            ]),
            vec![],
        ));
        let area = area_meters(&square);
        assert!((area - 1.236418893560872E10).abs() < 1.0, "{area}");
    }

    #[test]
    fn midpoint_walks_half_the_length() {
        let line = Geometry::LineString(LineString::from(vec![(0.0, 0.0), (1.0, 0.0), (1.0, 3.0)]));
        assert_eq!(midpoint(&line), Some(Coord { x: 1.0, y: 1.0 }));
    }
}
