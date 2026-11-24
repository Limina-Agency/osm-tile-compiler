//! Configurable schemas: layers, tag filters, attributes and zooms described
//! in YAML rather than code.
//!
//! The format is Planetiler's configurable schema
//! (<https://github.com/onthegomap/planetiler/tree/main/planetiler-custommap>),
//! so a schema written for Planetiler compiles here unchanged within the
//! subset documented in `docs/schemas.md`, and its `examples` are its tests.
//! Shortbread ships built in ([`shortbread`]).
//!
//! YAML rather than a scripting language such as Lua: a schema is data, so
//! it is checked completely when it is loaded, cannot loop or reach the file
//! system, and runs the same in every embedding (iOS, Android, Python, the
//! command line) with no interpreter to bundle. The `${ ... }` scripts it
//! may hold are a small, terminating expression language ([`expr`]).
pub(crate) mod expr;
pub(crate) mod matcher;
pub(crate) mod profile;
pub(crate) mod value;
pub(crate) mod verify;
pub(crate) mod yaml;

pub(crate) use profile::{LayerFilter, SchemaProfile};
pub use verify::ExampleResult;

use matcher::{Cond, Output, ValueExpr};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use value::Val;
use yaml::Node;

/// Planetiler's Shortbread schema, as its v0.10.2 release ships it.
const SHORTBREAD_YAML: &str = include_str!("shortbread.yml");
/// Its examples: the Shortbread specification as test cases.
const SHORTBREAD_SPEC_YAML: &str = include_str!("shortbread.spec.yml");

/// A loaded, checked schema.
pub struct Schema {
    pub(crate) name: String,
    pub(crate) description: Option<String>,
    pub(crate) attribution: Option<String>,
    pub(crate) version: Option<String>,
    pub(crate) overlay: bool,
    pub(crate) sources: Vec<SourceDef>,
    pub(crate) layers: Vec<LayerDef>,
    pub(crate) features: Vec<FeatureDef>,
    pub(crate) args: BTreeMap<String, Val>,
    pub(crate) examples: Vec<verify::Example>,
    /// Parts of the schema that are read but not applied.
    pub(crate) warnings: Vec<String>,
    /// A hash of the schema's text, which the archive's metadata records.
    pub(crate) digest: String,
}

// Its name and digest identify it, as in resume fingerprints, which print
// the request.
impl std::fmt::Debug for Schema {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Schema")
            .field("name", &self.name)
            .field("digest", &self.digest)
            .finish()
    }
}

/// A `sources` entry.
#[derive(Debug, Clone)]
pub(crate) struct SourceDef {
    pub id: String,
    pub input: SourceInput,
}

/// What a schema source reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceInput {
    /// The compile's OSM extract.
    Osm,
    /// The water polygons supplemental dataset
    /// (`water-polygons-split-3857.zip`).
    WaterPolygons,
}

#[derive(Debug, Clone)]
pub(crate) struct LayerDef {
    pub id: String,
}

/// How a feature's geometry is made from its source's.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum GeometryKind {
    Any,
    Point,
    Line,
    Polygon,
    PolygonCentroid,
    LineCentroid,
    LineMidpoint,
    Centroid,
    PolygonCentroidIfConvex,
    PolygonPointOnSurface,
    PointOnLine,
    InnermostPoint,
}

impl GeometryKind {
    fn from_name(name: &str) -> Result<Self, String> {
        Ok(match name {
            "any" => Self::Any,
            "point" => Self::Point,
            "line" => Self::Line,
            "polygon" => Self::Polygon,
            "polygon_centroid" => Self::PolygonCentroid,
            "line_centroid" => Self::LineCentroid,
            "line_midpoint" => Self::LineMidpoint,
            "centroid" => Self::Centroid,
            "polygon_centroid_if_convex" => Self::PolygonCentroidIfConvex,
            "polygon_point_on_surface" => Self::PolygonPointOnSurface,
            "point_on_line" => Self::PointOnLine,
            "innermost_point" => Self::InnermostPoint,
            "split_line" => {
                return Err("geometry 'split_line' is not supported yet".into());
            }
            other => return Err(format!("unknown geometry '{other}'")),
        })
    }
}

/// One `features` entry of a layer.
#[derive(Debug, Clone)]
pub(crate) struct FeatureDef {
    /// Index into [`Schema::layers`].
    pub layer: usize,
    /// Indexes into [`Schema::sources`]; empty for every source.
    pub sources: Vec<usize>,
    pub geometry: GeometryKind,
    /// `include_when`, the sources and not `exclude_when`.
    pub test: Cond,
    /// Keys one of which a source must have to pass `test`, if any.
    pub required_keys: Option<Vec<String>>,
    pub attributes: Vec<AttributeDef>,
    pub id: Option<ValueExpr>,
    pub min_zoom: Option<ValueExpr>,
    pub max_zoom: Option<ValueExpr>,
    pub min_size: Option<ValueExpr>,
    pub min_size_at_max_zoom: Option<ValueExpr>,
    pub tolerance: Option<ValueExpr>,
    pub tolerance_at_max_zoom: Option<ValueExpr>,
    pub buffer: Option<f64>,
}

#[derive(Debug, Clone)]
pub(crate) struct AttributeDef {
    pub key: String,
    pub value: ValueExpr,
    pub test: Cond,
    pub fallback: Val,
    pub min_zoom: Option<ValueExpr>,
    pub min_zoom_by_value: Vec<(String, u8)>,
    pub min_tile_cover_size: Option<f64>,
}

/// Which schema a compile writes.
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub enum TileSchema {
    /// The native port of OpenMapTiles 3.16 [default].
    #[default]
    OpenMapTiles,
    /// Shortbread 1.0, the schema Planetiler ships.
    Shortbread,
    /// A schema of the caller's.
    Custom(Arc<Schema>),
}

impl TileSchema {
    /// The schema a name gives: `openmaptiles`, `shortbread`, or the path of
    /// a YAML file.
    pub fn from_name_or_path(name: &str) -> Result<Self, String> {
        match name {
            "openmaptiles" | "omt" => Ok(Self::OpenMapTiles),
            "shortbread" => Ok(Self::Shortbread),
            path => Ok(Self::Custom(Arc::new(Schema::from_file(Path::new(path))?))),
        }
    }

    /// A short name for messages and metadata.
    pub fn name(&self) -> String {
        match self {
            Self::OpenMapTiles => "OpenMapTiles".into(),
            Self::Shortbread => "Shortbread".into(),
            Self::Custom(schema) => schema.name.clone(),
        }
    }

    /// The configured schema, unless this is OpenMapTiles.
    pub fn configured(&self) -> Option<Arc<Schema>> {
        match self {
            Self::OpenMapTiles => None,
            Self::Shortbread => Some(shortbread()),
            Self::Custom(schema) => Some(schema.clone()),
        }
    }
}

/// The built-in Shortbread schema.
pub fn shortbread() -> Arc<Schema> {
    static SCHEMA: std::sync::LazyLock<Arc<Schema>> = std::sync::LazyLock::new(|| {
        Arc::new(
            Schema::parse(SHORTBREAD_YAML, &mut |path| {
                (path == "shortbread.spec.yml")
                    .then(|| SHORTBREAD_SPEC_YAML.to_string())
                    .ok_or_else(|| format!("no file {path}"))
            })
            .expect("the built-in Shortbread schema is valid"),
        )
    });
    SCHEMA.clone()
}

impl Schema {
    /// Reads a schema file; `examples` that name a file are read beside it.
    pub fn from_file(path: &Path) -> Result<Self, String> {
        let read = |path: &Path| {
            let metadata = std::fs::metadata(path)
                .map_err(|e| format!("could not read schema {}: {e}", path.display()))?;
            if metadata.len() > yaml::MAX_SCHEMA_BYTES as u64 {
                return Err(format!("schema {} is too large", path.display()));
            }
            std::fs::read_to_string(path)
                .map_err(|e| format!("could not read schema {}: {e}", path.display()))
        };
        let text = read(path)?;
        let base = path.parent().map(Path::to_path_buf).unwrap_or_default();
        Self::parse(&text, &mut |name| read(&base.join(name)))
            .map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Reads a schema from its YAML or JSON text. `examples` that name a
    /// file are left out.
    pub fn from_yaml(text: &str) -> Result<Self, String> {
        Self::parse(text, &mut |_| Ok("[]".into()))
    }

    /// The schema's name (`schema_name`).
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The layers, in the order the schema lists them.
    pub fn layers(&self) -> Vec<&str> {
        self.layers.iter().map(|l| l.id.as_str()).collect()
    }

    /// Parts of the schema that are read but not applied yet.
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }

    /// Whether the schema reads the water polygons dataset.
    pub fn reads_water_polygons(&self) -> bool {
        self.sources
            .iter()
            .any(|s| s.input == SourceInput::WaterPolygons)
    }

    fn parse(
        text: &str,
        read_file: &mut dyn FnMut(&str) -> Result<String, String>,
    ) -> Result<Self, String> {
        use sha2::Digest;
        let root = yaml::parse(text)?;
        if !matches!(root, Node::Map(_)) {
            return Err("the schema must be a map".into());
        }
        let mut warnings = Vec::new();
        check_keys(
            &root,
            "the schema",
            &[
                "schema_name",
                "schema_description",
                "attribution",
                "version",
                "is_overlay",
                "sources",
                "tag_mappings",
                "layers",
                "examples",
                "args",
                "definitions",
            ],
        )?;
        let text_of = |key: &str| root.get(key).filter(|n| !n.is_null()).map(Node::text);
        let name = text_of("schema_name").unwrap_or_else(|| "Custom schema".into());
        if let Some(mappings) = root.get("tag_mappings")
            && !matches!(mappings, Node::Null)
            && !matches!(mappings, Node::Map(m) if m.is_empty())
        {
            return Err("tag_mappings is not supported yet".into());
        }
        let args = parse_args(root.get("args"))?;
        let sources = parse_sources(root.get("sources"))?;
        let Some(Node::List(layer_nodes)) = root.get("layers") else {
            return Err("the schema needs a list of layers".into());
        };
        if layer_nodes.is_empty() {
            return Err("the schema has no layers".into());
        }
        let mut layers = Vec::new();
        let mut features = Vec::new();
        for (index, node) in layer_nodes.iter().enumerate() {
            let at = format!("layers[{index}]");
            check_keys(
                node,
                &at,
                &["id", "buffer", "features", "tile_post_process"],
            )?;
            let id = node
                .get("id")
                .and_then(Node::as_str)
                .ok_or_else(|| format!("{at}: a layer needs an id"))?
                .to_string();
            if id.is_empty() || id.len() > 64 {
                return Err(format!("{at}: a layer id must be 1 to 64 bytes"));
            }
            if layers.iter().any(|l: &LayerDef| l.id == id) {
                return Err(format!("{at}: layer {id} is listed twice"));
            }
            let at = format!("layer {id}");
            let buffer = match node.get("buffer") {
                None | Some(Node::Null) => None,
                Some(b) => {
                    Some(number_of(b).ok_or_else(|| format!("{at}: buffer must be a number"))?)
                }
            };
            if node.get("tile_post_process").is_some_and(|n| !n.is_null()) {
                warnings.push(format!(
                    "layer {id}: tile_post_process is not applied yet; its features are written \
                     unmerged"
                ));
            }
            let layer = layers.len();
            layers.push(LayerDef { id: id.clone() });
            let feature_nodes = match node.get("features") {
                Some(Node::List(items)) => items.as_slice(),
                None | Some(Node::Null) => &[],
                Some(_) => return Err(format!("{at}: features must be a list")),
            };
            for (i, feature) in feature_nodes.iter().enumerate() {
                let at = format!("{at}, features[{i}]");
                features.push(
                    parse_feature(feature, layer, buffer, &sources)
                        .map_err(|e| format!("{at}: {e}"))?,
                );
            }
        }
        let examples = match root.get("examples") {
            None | Some(Node::Null) => Vec::new(),
            Some(Node::Str(file)) => {
                let text = read_file(file).map_err(|e| format!("examples: {e}"))?;
                let node = yaml::parse(&text).map_err(|e| format!("{file}: {e}"))?;
                let list = match &node {
                    Node::Map(_) => node.get("examples").cloned().unwrap_or(Node::List(vec![])),
                    other => other.clone(),
                };
                verify::parse_examples(&list).map_err(|e| format!("{file}: {e}"))?
            }
            Some(list) => verify::parse_examples(list).map_err(|e| format!("examples: {e}"))?,
        };
        Ok(Schema {
            name,
            description: text_of("schema_description"),
            attribution: text_of("attribution"),
            version: text_of("version"),
            overlay: matches!(root.get("is_overlay"), Some(Node::Bool(true))),
            sources,
            layers,
            features,
            args,
            examples,
            warnings,
            digest: hex::encode(&sha2::Sha256::digest(text.as_bytes())[..8]),
        })
    }
}

fn check_keys(node: &Node, at: &str, allowed: &[&str]) -> Result<(), String> {
    let Node::Map(entries) = node else {
        return Err(format!("{at} must be a map, not {}", node.kind()));
    };
    for (key, _) in entries {
        if !allowed.contains(&key.as_str()) {
            return Err(format!(
                "{at}: unknown key '{key}'; the keys are {}",
                allowed.join(", ")
            ));
        }
    }
    Ok(())
}

fn number_of(node: &Node) -> Option<f64> {
    match node {
        Node::Int(i) => Some(*i as f64),
        Node::Double(d) => Some(*d),
        Node::Str(s) => s.parse().ok(),
        _ => None,
    }
}

/// The schema's `args`, their defaults resolved in order: a default may be
/// a script that reads the arguments before it.
fn parse_args(node: Option<&Node>) -> Result<BTreeMap<String, Val>, String> {
    let mut args = BTreeMap::new();
    let Some(Node::Map(entries)) = node else {
        return Ok(args);
    };
    for (name, definition) in entries {
        let default = match definition {
            Node::Map(_)
                if definition.get("default").is_some()
                    || definition.get("description").is_some() =>
            {
                definition.get("default").cloned().unwrap_or(Node::Null)
            }
            other => other.clone(),
        };
        let value = ValueExpr::parse(&default, Output::Any)
            .map_err(|e| format!("args.{name}: {e}"))?
            .eval(&profile::ArgsScope(&args));
        args.insert(name.clone(), value);
    }
    Ok(args)
}

fn parse_sources(node: Option<&Node>) -> Result<Vec<SourceDef>, String> {
    let Some(Node::Map(entries)) = node else {
        return Err("the schema needs its sources, such as `osm: {type: osm}`".into());
    };
    let mut sources = Vec::new();
    for (id, definition) in entries {
        let at = format!("sources.{id}");
        check_keys(
            definition,
            &at,
            &["type", "url", "local_path", "projection"],
        )?;
        let kind = definition.get("type").map(Node::text).unwrap_or_default();
        let url = definition
            .get("url")
            .or(definition.get("local_path"))
            .map(Node::text)
            .unwrap_or_default();
        let input = match kind.as_str() {
            "osm" => SourceInput::Osm,
            "shapefile" | "geopackage" if url.contains("water-polygons-split") => {
                SourceInput::WaterPolygons
            }
            "shapefile" | "geopackage" | "geojson" | "flatgeobuf" | "geoparquet" | "csv" => {
                return Err(format!(
                    "{at}: {kind} sources are not supported yet, other than the OSM water \
                     polygons (water-polygons-split-3857.zip)"
                ));
            }
            other => return Err(format!("{at}: unknown source type '{other}'")),
        };
        if input == SourceInput::Osm
            && sources
                .iter()
                .any(|s: &SourceDef| s.input == SourceInput::Osm)
        {
            return Err(format!("{at}: a schema reads one OSM extract"));
        }
        sources.push(SourceDef {
            id: id.clone(),
            input,
        });
    }
    if sources.is_empty() {
        return Err("the schema needs at least one source".into());
    }
    Ok(sources)
}

fn parse_feature(
    node: &Node,
    layer: usize,
    buffer: Option<f64>,
    sources: &[SourceDef],
) -> Result<FeatureDef, String> {
    check_keys(
        node,
        "the feature",
        &[
            "source",
            "id",
            "min_zoom",
            "max_zoom",
            "min_size",
            "min_size_at_max_zoom",
            "tolerance",
            "tolerance_at_max_zoom",
            "geometry",
            "include_when",
            "exclude_when",
            "attributes",
        ],
    )?;
    let source_ids = match node.get("source") {
        None | Some(Node::Null) => Vec::new(),
        Some(Node::List(items)) => items.iter().map(Node::text).collect(),
        Some(other) => vec![other.text()],
    };
    let source_indexes = source_ids
        .iter()
        .map(|id| {
            sources
                .iter()
                .position(|s| s.id == *id)
                .ok_or_else(|| format!("unknown source '{id}'"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let geometry = match node.get("geometry") {
        None | Some(Node::Null) => GeometryKind::Any,
        Some(g) => GeometryKind::from_name(&g.text())?,
    };
    let mut test = match node.get("include_when") {
        None => Cond::True,
        Some(n) => Cond::parse(n).map_err(|e| format!("include_when: {e}"))?,
    };
    if let Some(exclude) = node.get("exclude_when") {
        let exclude = Cond::parse(exclude).map_err(|e| format!("exclude_when: {e}"))?;
        test = Cond::And(vec![test, Cond::Not(Box::new(exclude))]);
    }
    let required_keys = test.required_keys();
    let expression = |key: &str, output| -> Result<Option<ValueExpr>, String> {
        match node.get(key) {
            None | Some(Node::Null) => Ok(None),
            Some(n) => ValueExpr::parse(n, output)
                .map(Some)
                .map_err(|e| format!("{key}: {e}")),
        }
    };
    let attributes = match node.get("attributes") {
        None | Some(Node::Null) => Vec::new(),
        Some(Node::List(items)) => items
            .iter()
            .enumerate()
            .map(|(i, a)| parse_attribute(a).map_err(|e| format!("attributes[{i}]: {e}")))
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("attributes must be a list".into()),
    };
    Ok(FeatureDef {
        layer,
        sources: source_indexes,
        geometry,
        test,
        required_keys,
        attributes,
        id: expression("id", Output::Any)?,
        min_zoom: expression("min_zoom", Output::Integer)?,
        max_zoom: expression("max_zoom", Output::Integer)?,
        min_size: expression("min_size", Output::Double)?,
        min_size_at_max_zoom: expression("min_size_at_max_zoom", Output::Double)?,
        tolerance: expression("tolerance", Output::Double)?,
        tolerance_at_max_zoom: expression("tolerance_at_max_zoom", Output::Double)?,
        buffer,
    })
}

fn parse_attribute(node: &Node) -> Result<AttributeDef, String> {
    check_keys(
        node,
        "the attribute",
        &[
            "key",
            "include_when",
            "exclude_when",
            "min_zoom",
            "min_zoom_by_value",
            "min_tile_cover_size",
            "else",
            "value",
            "tag_value",
            "arg_value",
            "type",
            "coalesce",
        ],
    )?;
    let key = node
        .get("key")
        .filter(|k| !k.is_null())
        .map(Node::text)
        .ok_or("an attribute needs a key")?;
    if key.starts_with('_') {
        return Err(format!(
            "attribute key '{key}': keys starting with '_' are reserved for the compiler"
        ));
    }
    // As Planetiler: `type: match_key` and `type: match_value` set the value;
    // another type converts whichever of coalesce, value, tag_value or
    // arg_value is given, or else the tag of the attribute's key.
    let kind = node.get("type").filter(|t| !t.is_null()).map(Node::text);
    let mut value = Vec::new();
    match kind.as_deref() {
        Some("match_key") => value.push(("value".to_string(), Node::Str("${match_key}".into()))),
        Some("match_value") => {
            value.push(("value".to_string(), Node::Str("${match_value}".into())));
        }
        _ => {
            if let Some(kind) = &kind {
                value.push(("type".to_string(), Node::Str(kind.clone())));
            }
            let source = ["coalesce", "value", "tag_value", "arg_value"]
                .into_iter()
                .find_map(|k| node.get(k).filter(|v| !v.is_null()).map(|v| (k, v.clone())));
            match source {
                Some((k, v)) => value.push((k.to_string(), v)),
                None => value.push(("tag_value".to_string(), Node::Str(key.clone()))),
            }
        }
    }
    let value = ValueExpr::parse(&Node::Map(value), Output::Any)?;
    let include = match node.get("include_when") {
        None => Cond::True,
        Some(n) => Cond::parse(n).map_err(|e| format!("include_when: {e}"))?,
    };
    let exclude = match node.get("exclude_when") {
        None => Cond::True,
        Some(n) => Cond::Not(Box::new(
            Cond::parse(n).map_err(|e| format!("exclude_when: {e}"))?,
        )),
    };
    let min_zoom = match node.get("min_zoom") {
        None | Some(Node::Null) => None,
        Some(n) => {
            let expr =
                ValueExpr::parse(n, Output::Integer).map_err(|e| format!("min_zoom: {e}"))?;
            (expr.constant() != Some(&Val::Int(0))).then_some(expr)
        }
    };
    let min_zoom_by_value = match node.get("min_zoom_by_value") {
        None | Some(Node::Null) => Vec::new(),
        Some(Node::Map(entries)) => entries
            .iter()
            .map(|(value, zoom)| match zoom {
                Node::Int(z) if (0..=255).contains(z) => Ok((value.clone(), *z as u8)),
                _ => Err(format!("min_zoom_by_value.{value} must be a zoom")),
            })
            .collect::<Result<_, _>>()?,
        Some(_) => return Err("min_zoom_by_value must be a map".into()),
    };
    let min_tile_cover_size = match node.get("min_tile_cover_size") {
        None | Some(Node::Null) => None,
        Some(n) => Some(number_of(n).ok_or("min_tile_cover_size must be a number")?),
    }
    .filter(|_| node.get("include_when").is_some());
    Ok(AttributeDef {
        key,
        value,
        test: Cond::And(vec![include, exclude]),
        fallback: node.get("else").map_or(Val::Null, Node::to_val),
        min_zoom,
        min_zoom_by_value,
        min_tile_cover_size,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_built_in_shortbread_schema_loads() {
        let schema = shortbread();
        assert_eq!(schema.name(), "Shortbread");
        assert_eq!(schema.layers().len(), 26);
        assert!(schema.reads_water_polygons());
        assert_eq!(schema.examples.len(), 67);
        assert!(schema.warnings.is_empty(), "{:?}", schema.warnings);
    }

    #[test]
    fn schema_errors_name_the_place() {
        let error = |yaml: &str| Schema::from_yaml(yaml).unwrap_err();
        let sources = "sources: {osm: {type: osm}}\n";
        assert!(error("layers: []").contains("sources"));
        assert!(error(&format!("{sources}layers: []")).contains("no layers"));
        assert!(
            error(&format!(
                "{sources}layers: [{{id: a, features: [{{source: x}}]}}]"
            ))
            .contains("unknown source 'x'")
        );
        assert!(
            error(&format!("{sources}layers: [{{id: a, feature: []}}]"))
                .contains("unknown key 'feature'")
        );
        assert!(
            error(&format!(
                "{sources}layers: [{{id: a, features: [{{include_when: '${{ 1 + }}'}}]}}]"
            ))
            .contains("invalid script")
        );
        assert!(
            error(&format!(
                "{sources}layers: [{{id: a, features: [{{attributes: [{{key: _x}}]}}]}}]"
            ))
            .contains("reserved")
        );
        assert!(
            error("sources: {o: {type: geopackage, url: x.gpkg}}\nlayers: [{id: a}]")
                .contains("not supported yet")
        );
    }
}
