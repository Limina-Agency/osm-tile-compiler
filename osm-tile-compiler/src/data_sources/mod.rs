//! Vector data besides the OSM extract: GeoJSON, CSV, Shapefile,
//! GeoPackage, FlatGeobuf and GeoParquet files, read into tile layers of
//! their own beside the basemap's.
//!
//! Two types split the job. A [`DataInput`] says where features come from
//! and how to read them: the file, its format, the table or columns that
//! hold the geometry, and its coordinate system. It knows nothing about
//! tiles or schemas, so anything that wants a file's features can read one
//! ([`DataInput::read`]), each as a [`SourceFeature`] in WGS84 longitude and
//! latitude. A [`DataLayer`] puts one input's features into a tile layer of
//! the compile ([`BasemapRequest::data_layers`](crate::BasemapRequest::data_layers)),
//! at its zooms, with the attributes it keeps.
//!
//! Every reader is pure Rust and none needs GDAL. GeoJSON, CSV, Shapefile
//! and GeoPackage are always built; FlatGeobuf, GeoParquet and
//! reprojection from coordinate systems other than WGS84 and Web Mercator
//! come with the crate features `flatgeobuf`, `geoparquet` and `reproject`
//! (all three in the default `data-sources`).

mod crs;
mod csv_reader;
#[cfg(feature = "flatgeobuf")]
mod fgb;
mod geojson_reader;
#[cfg(feature = "geoparquet")]
mod geoparquet;
mod gpkg;
mod shp;
mod wkb;

use crate::{CompileBounds, CompileError, LayerName};
use geo_types::Geometry;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

pub(crate) use crs::Crs;

/// The file formats a [`DataInput`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum DataFormat {
    /// A GeoJSON FeatureCollection, Feature or geometry (`.geojson`,
    /// `.json`). RFC 7946 puts it in WGS84; [`DataInput::crs`] can say
    /// otherwise for older files.
    GeoJson,
    /// Newline-delimited GeoJSON: one Feature or geometry per line, with or
    /// without RFC 8142's record separators (`.geojsonl`, `.geojsons`,
    /// `.ndjson`, `.jsonl`).
    GeoJsonSeq,
    /// Comma- or tab-separated values with a header row (`.csv`, `.tsv`):
    /// points from longitude and latitude columns, or any geometry from a
    /// WKT or hex WKB column ([`CsvColumns`]).
    Csv,
    /// An ESRI Shapefile (`.shp`) with its `.dbf` attributes and its `.prj`
    /// coordinate system beside it.
    Shapefile,
    /// An OGC GeoPackage (`.gpkg`): one feature table of it
    /// ([`DataInput::table`]).
    GeoPackage,
    /// FlatGeobuf (`.fgb`). Needs the `flatgeobuf` feature.
    FlatGeobuf,
    /// GeoParquet 1.0 or 1.1 with WKB geometry (`.parquet`, `.geoparquet`).
    /// Needs the `geoparquet` feature.
    GeoParquet,
}

impl DataFormat {
    /// Every format, whether this build reads it or not.
    pub const ALL: [Self; 7] = [
        Self::GeoJson,
        Self::GeoJsonSeq,
        Self::Csv,
        Self::Shapefile,
        Self::GeoPackage,
        Self::FlatGeobuf,
        Self::GeoParquet,
    ];

    /// A stable identifier: `geojson`, `geojson_seq`, `csv`, `shapefile`,
    /// `geopackage`, `flatgeobuf` or `geoparquet`.
    pub fn id(self) -> &'static str {
        match self {
            Self::GeoJson => "geojson",
            Self::GeoJsonSeq => "geojson_seq",
            Self::Csv => "csv",
            Self::Shapefile => "shapefile",
            Self::GeoPackage => "geopackage",
            Self::FlatGeobuf => "flatgeobuf",
            Self::GeoParquet => "geoparquet",
        }
    }

    /// The format with this [`id`](Self::id), or one of its file extensions
    /// (`fgb`, `gpkg`, `shp`, `parquet`, `ndjson` and so on).
    pub fn from_id(id: &str) -> Option<Self> {
        let id = id.to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|format| format.id() == id)
            .or_else(|| Self::from_extension(&id))
    }

    /// The format a file's extension names, if any.
    pub fn from_path(path: &Path) -> Option<Self> {
        Self::from_extension(&path.extension()?.to_str()?.to_ascii_lowercase())
    }

    fn from_extension(extension: &str) -> Option<Self> {
        Some(match extension {
            "geojson" | "json" => Self::GeoJson,
            "geojsonl" | "geojsons" | "geojsonseq" | "ndjson" | "jsonl" => Self::GeoJsonSeq,
            "csv" | "tsv" => Self::Csv,
            "shp" => Self::Shapefile,
            "gpkg" => Self::GeoPackage,
            "fgb" => Self::FlatGeobuf,
            "parquet" | "geoparquet" => Self::GeoParquet,
            _ => return None,
        })
    }

    /// Whether this build reads the format: FlatGeobuf and GeoParquet need
    /// their crate features.
    #[allow(clippy::match_like_matches_macro, reason = "the arms differ by build")]
    pub fn is_available(self) -> bool {
        match self {
            Self::FlatGeobuf => cfg!(feature = "flatgeobuf"),
            Self::GeoParquet => cfg!(feature = "geoparquet"),
            _ => true,
        }
    }
}

impl fmt::Display for DataFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.id())
    }
}

/// Which columns of a CSV file hold the geometry, and how it is separated.
/// Without a column named, the header is searched (ignoring case) for
/// `longitude`/`lon`/`lng`/`long`/`x` and `latitude`/`lat`/`y`, then for
/// `wkt`/`geometry`/`geom`/`the_geom`/`wkb`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct CsvColumns {
    /// The longitude (or easting) column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub longitude: Option<String>,
    /// The latitude (or northing) column.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latitude: Option<String>,
    /// A column of WKT, or of WKB in hexadecimal, for any geometry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub geometry: Option<String>,
    /// The field separator [default: a tab for `.tsv` files, else a comma].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delimiter: Option<char>,
}

/// Where a layer's features come from, and how to read them. It is not
/// tied to any tile layer or schema.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct DataInput {
    /// The file.
    pub path: PathBuf,
    /// Its format [default: the one its extension names].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<DataFormat>,
    /// The GeoPackage feature table to read [default: the file's only one].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    /// The coordinate system the file's coordinates are in, overriding what
    /// the file says: `EPSG:<code>`, `OGC:CRS84`, a PROJ string (`+proj=…`)
    /// or WKT [default: what the file declares, or WGS84 when it declares
    /// nothing]. Without the `reproject` feature only WGS84 (and the datums
    /// within a metre of it: ETRS89, NAD83, GDA94) and Web Mercator are
    /// read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub crs: Option<String>,
    /// The geometry columns of a CSV file.
    #[serde(default, skip_serializing_if = "is_default")]
    pub csv: CsvColumns,
    /// The attribute whose whole-number value becomes the feature's id in
    /// the tiles [default: the format's own feature id where it has one
    /// (GeoJSON's `id`, GeoPackage's primary key, FlatGeobuf's and
    /// Shapefile's record number), else the feature's position in the
    /// file, from 1].
    #[serde(
        alias = "idAttribute",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub id_attribute: Option<String>,
}

fn is_default<T: Default + PartialEq>(value: &T) -> bool {
    *value == T::default()
}

/// An attribute value read from a [`DataInput`]. Empty values (nulls) are
/// left out of [`SourceFeature::properties`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PropertyValue {
    Bool(bool),
    Int(i64),
    Float(f64),
    String(String),
}

impl PropertyValue {
    fn json(&self) -> serde_json::Value {
        match self {
            Self::Bool(v) => serde_json::Value::Bool(*v),
            Self::Int(v) => serde_json::Value::from(*v),
            Self::Float(v) => serde_json::Number::from_f64(*v)
                .map(serde_json::Value::Number)
                .unwrap_or(serde_json::Value::Null),
            Self::String(v) => serde_json::Value::String(v.clone()),
        }
    }

    /// The value as a whole number, if it is one.
    fn as_id(&self) -> Option<u64> {
        match self {
            Self::Int(v) => u64::try_from(*v).ok(),
            Self::Float(v) if v.fract() == 0.0 && *v >= 0.0 && *v < 1.8e19 => Some(*v as u64),
            Self::String(v) => v.trim().parse().ok(),
            _ => None,
        }
    }

    /// A JSON value: strings, numbers and booleans as themselves; arrays
    /// and objects as their JSON text, as Tippecanoe writes them.
    fn from_json(value: serde_json::Value) -> Option<Self> {
        Some(match value {
            serde_json::Value::Null => return None,
            serde_json::Value::Bool(v) => Self::Bool(v),
            serde_json::Value::Number(n) => match n.as_i64() {
                Some(v) => Self::Int(v),
                None => Self::Float(n.as_f64()?),
            },
            serde_json::Value::String(v) => Self::String(v),
            other => Self::String(other.to_string()),
        })
    }
}

/// One feature of a [`DataInput`], in WGS84 longitude and latitude.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SourceFeature {
    /// The feature's id, as [`DataInput::id_attribute`] decides.
    pub id: u64,
    /// Points, lines or polygons; never a geometry collection (its parts
    /// come as features of their own, with the same id and properties).
    pub geometry: Geometry<f64>,
    pub properties: BTreeMap<String, PropertyValue>,
}

/// What reading a [`DataInput`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DataReadReport {
    /// Features handed on.
    pub features: u64,
    /// Features skipped because they lie outside the bounds asked for.
    pub outside_bounds: u64,
    /// Features skipped because they have no geometry, an empty one, or
    /// coordinates that are not finite or could not be reprojected.
    pub without_geometry: u64,
    /// The coordinate system the coordinates were read in, such as
    /// `EPSG:4326` or `EPSG:2180`.
    pub crs: String,
}

impl DataInput {
    /// The file at `path`, in the format its extension names.
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            format: None,
            table: None,
            crs: None,
            csv: CsvColumns::default(),
            id_attribute: None,
        }
    }

    /// The format read: [`format`](Self::format), or the one the extension
    /// names.
    pub fn resolved_format(&self) -> Result<DataFormat, CompileError> {
        let format = self
            .format
            .or_else(|| DataFormat::from_path(&self.path))
            .ok_or_else(|| {
                CompileError::InvalidRequest(format!(
                    "{}: no data format is given and its extension names none (formats: {})",
                    self.path.display(),
                    DataFormat::ALL.map(DataFormat::id).join(", ")
                ))
            })?;
        if !format.is_available() {
            return Err(CompileError::InvalidRequest(format!(
                "{}: this build of osm-tile-compiler reads no {} (it needs the `{}` feature)",
                self.path.display(),
                format.id(),
                format.id()
            )));
        }
        Ok(format)
    }

    /// Checks what can be checked without reading: the format, the
    /// coordinate system override, and that the file exists.
    pub fn validate(&self) -> Result<(), CompileError> {
        self.resolved_format()?;
        if let Some(crs) = &self.crs {
            Crs::parse(crs).map_err(|e| {
                CompileError::InvalidRequest(format!("{}: {e}", self.path.display()))
            })?;
        }
        if !self.path.is_file() {
            return Err(CompileError::InvalidRequest(format!(
                "data file {} does not exist",
                self.path.display()
            )));
        }
        Ok(())
    }

    /// The files reading it opens: the file, and a Shapefile's `.dbf`,
    /// `.shx`, `.prj` and `.cpg`.
    pub fn files(&self) -> Vec<PathBuf> {
        let mut files = vec![self.path.clone()];
        if matches!(self.resolved_format(), Ok(DataFormat::Shapefile)) {
            for extension in ["dbf", "shx", "prj", "cpg"] {
                files.extend(sidecar(&self.path, extension));
            }
        }
        files
    }

    /// Reads every feature, reprojected to WGS84, and calls `visit` with
    /// each that meets `bounds` (every one without bounds). An error from
    /// `visit` stops the read and is returned as
    /// [`CompileError::Failed`].
    pub fn read(
        &self,
        bounds: Option<CompileBounds>,
        mut visit: impl FnMut(SourceFeature) -> Result<(), String>,
    ) -> Result<DataReadReport, CompileError> {
        self.validate()?;
        self.read_inner(bounds, &mut visit)
            .map_err(|message| CompileError::Failed {
                stage: Some(crate::CompileStage::ReadSources),
                message,
            })
    }

    pub(crate) fn read_inner(
        &self,
        bounds: Option<CompileBounds>,
        visit: &mut dyn FnMut(SourceFeature) -> Result<(), String>,
    ) -> Result<DataReadReport, String> {
        let format = self.resolved_format().map_err(|e| e.to_string())?;
        let forced = self.crs.as_deref().map(Crs::parse).transpose()?;
        let mut sink = Sink {
            forced,
            crs: None,
            bounds: bounds.map(CompileBounds::normalized),
            id_attribute: self.id_attribute.as_deref(),
            visit,
            report: DataReadReport::default(),
            position: 0,
        };
        let path = &self.path;
        let result = match format {
            DataFormat::GeoJson => geojson_reader::read(path, &mut sink),
            DataFormat::GeoJsonSeq => geojson_reader::read_seq(path, &mut sink),
            DataFormat::Csv => csv_reader::read(path, &self.csv, &mut sink),
            DataFormat::Shapefile => shp::read(path, &mut sink),
            DataFormat::GeoPackage => gpkg::read(path, self.table.as_deref(), &mut sink),
            #[cfg(feature = "flatgeobuf")]
            DataFormat::FlatGeobuf => fgb::read(path, &mut sink),
            #[cfg(feature = "geoparquet")]
            DataFormat::GeoParquet => geoparquet::read(path, &mut sink),
            #[allow(unreachable_patterns)]
            _ => Err(format!("this build reads no {format}")),
        };
        result.map_err(|e| format!("{}: {e}", path.display()))?;
        let mut report = sink.report;
        report.crs = sink
            .forced
            .as_ref()
            .or(sink.crs.as_ref())
            .map_or_else(|| Crs::Wgs84.name(), Crs::name);
        Ok(report)
    }
}

/// A file beside `path` with another extension, in lower or upper case,
/// as ESRI tools write `.PRJ`.
fn sidecar(path: &Path, extension: &str) -> Option<PathBuf> {
    [
        path.with_extension(extension),
        path.with_extension(extension.to_ascii_uppercase()),
    ]
    .into_iter()
    .find(|candidate| candidate.is_file())
}

/// Where readers hand their features: it sets the coordinate system,
/// reprojects, splits collections, filters by bounds and assigns ids.
pub(crate) struct Sink<'a> {
    /// The request's override, which wins over what the file says.
    forced: Option<Crs>,
    /// What the file says, once the reader knows.
    crs: Option<Crs>,
    bounds: Option<CompileBounds>,
    id_attribute: Option<&'a str>,
    visit: &'a mut dyn FnMut(SourceFeature) -> Result<(), String>,
    report: DataReadReport,
    /// Features seen, for default ids.
    position: u64,
}

impl Sink<'_> {
    /// The coordinate system the file declares; call before `push`. The
    /// request's override still wins.
    pub fn declare_crs(&mut self, crs: Crs) {
        self.crs = Some(crs);
    }

    /// Hands on one feature in the file's coordinates. `id` is the
    /// format's own id for it, if it has one.
    pub fn push(
        &mut self,
        id: Option<u64>,
        geometry: Option<Geometry<f64>>,
        properties: BTreeMap<String, PropertyValue>,
    ) -> Result<(), String> {
        self.position += 1;
        let Some(geometry) = geometry else {
            self.report.without_geometry += 1;
            return Ok(());
        };
        let crs = self.forced.as_ref().or(self.crs.as_ref());
        let geographic = matches!(crs, None | Some(Crs::Wgs84));
        if geographic && let Some(coord) = not_geographic(&geometry) {
            return Err(format!(
                "coordinate ({}, {}) is not a longitude and latitude; {}",
                coord.x,
                coord.y,
                if self.forced.is_some() {
                    "the crs given is geographic, so give the file's own"
                } else {
                    "the file declares no other coordinate system, so give the input's crs"
                }
            ));
        }
        let geometry = match crs {
            Some(crs) => crs.to_wgs84(geometry),
            None => Some(geometry),
        };
        let mut parts = Vec::new();
        if let Some(geometry) = geometry {
            flatten(geometry, &mut parts);
        }
        parts.retain(finite_and_nonempty);
        if parts.is_empty() {
            self.report.without_geometry += 1;
            return Ok(());
        }
        if let Some(bounds) = self.bounds {
            parts.retain(|part| meets(part, bounds));
            if parts.is_empty() {
                self.report.outside_bounds += 1;
                return Ok(());
            }
        }
        let id = self
            .id_attribute
            .and_then(|key| properties.get(key))
            .and_then(PropertyValue::as_id)
            .or(id)
            .unwrap_or(self.position);
        self.report.features += 1;
        let last = parts.len() - 1;
        let mut properties = Some(properties);
        for (index, geometry) in parts.into_iter().enumerate() {
            let properties = if index == last {
                properties.take().unwrap_or_default()
            } else {
                properties.clone().unwrap_or_default()
            };
            (self.visit)(SourceFeature {
                id,
                geometry,
                properties,
            })?;
        }
        Ok(())
    }
}

/// Splits collections into their parts, dropping any third dimension the
/// reader kept (geo-types has none).
fn flatten(geometry: Geometry<f64>, out: &mut Vec<Geometry<f64>>) {
    match geometry {
        Geometry::GeometryCollection(collection) => {
            for part in collection {
                flatten(part, out);
            }
        }
        Geometry::Rect(rect) => out.push(Geometry::Polygon(rect.to_polygon())),
        Geometry::Triangle(triangle) => out.push(Geometry::Polygon(triangle.to_polygon())),
        Geometry::Line(line) => out.push(Geometry::LineString(line.into())),
        other => out.push(other),
    }
}

/// The first coordinate that cannot be a longitude and latitude, allowing
/// longitudes from -360 to 360 as some datasets write them.
fn not_geographic(geometry: &Geometry<f64>) -> Option<geo_types::Coord<f64>> {
    use geo::CoordsIter;
    geometry
        .coords_iter()
        .find(|c| c.x.abs() > 360.0 || c.y.abs() > 90.000_001)
}

fn finite_and_nonempty(geometry: &Geometry<f64>) -> bool {
    use geo::CoordsIter;
    let mut any = false;
    for coord in geometry.coords_iter() {
        if !(coord.x.is_finite() && coord.y.is_finite()) {
            return false;
        }
        any = true;
    }
    any
}

fn meets(geometry: &Geometry<f64>, bounds: CompileBounds) -> bool {
    use geo::BoundingRect;
    let Some(rect) = geometry.bounding_rect() else {
        return false;
    };
    bounds.meets(rect.min().x, rect.min().y, rect.max().x, rect.max().y)
}

/// One [`DataInput`]'s features as a tile layer of a compile.
///
/// The layer is written after the profile's layers, at its zooms, with the
/// attributes it keeps. Polygons and lines are clipped and simplified per
/// zoom as the profile's own are, and small polygons are dropped at the
/// zooms where they are under a pixel; features are not merged or
/// thinned. A data layer may also be named like one of the profile's layers
/// (adding, say, points to `poi`); that layer must then be among the ones
/// written, and the features go in with the profile's.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
#[non_exhaustive]
pub struct DataLayer {
    /// The tile layer, at most 128 bytes.
    pub layer: LayerName,
    pub source: DataInput,
    /// The lowest zoom the features appear at [default: the compile's
    /// lowest].
    #[serde(alias = "minZoom", default, skip_serializing_if = "Option::is_none")]
    pub min_zoom: Option<u8>,
    /// The highest zoom they are written at [default: the compile's
    /// highest].
    #[serde(alias = "maxZoom", default, skip_serializing_if = "Option::is_none")]
    pub max_zoom: Option<u8>,
    /// The attributes to keep [default: all of them].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attributes: Option<Vec<String>>,
    /// Attributes to leave out, even if `attributes` lists them.
    #[serde(
        alias = "excludeAttributes",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub exclude_attributes: Vec<String>,
    /// New names for attributes kept, from the file's name to the tile's.
    #[serde(
        alias = "renameAttributes",
        default,
        skip_serializing_if = "BTreeMap::is_empty"
    )]
    pub rename_attributes: BTreeMap<String, String>,
    /// Credit for the data, added to the archive's attribution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<String>,
}

impl DataLayer {
    /// The features of `source`, in the layer `layer`, at every zoom of the
    /// compile with every attribute.
    pub fn new(layer: impl Into<LayerName>, source: DataInput) -> Self {
        Self {
            layer: layer.into(),
            source,
            min_zoom: None,
            max_zoom: None,
            attributes: None,
            exclude_attributes: Vec::new(),
            rename_attributes: BTreeMap::new(),
            attribution: None,
        }
    }

    /// The same layer at zooms `min` to `max`.
    pub fn zooms(mut self, min: u8, max: u8) -> Self {
        self.min_zoom = Some(min);
        self.max_zoom = Some(max);
        self
    }

    /// Checks the layer's name and zooms against a compile's `zoom` range,
    /// then its source ([`DataInput::validate`]).
    pub fn validate(&self, zoom: crate::ZoomRange) -> Result<(), CompileError> {
        let invalid = |message: String| Err(CompileError::InvalidRequest(message));
        let name = self.layer.as_str();
        if name.is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
            return invalid(format!(
                "data layer name '{name}' must be 1 to 128 bytes without control characters"
            ));
        }
        let (min, max) = self.zoom_range(zoom);
        if min > max {
            return invalid(format!(
                "data layer {name}: its zooms {}..{} leave nothing within the compile's {}..{}",
                self.min_zoom.unwrap_or(zoom.min()),
                self.max_zoom.unwrap_or(zoom.max()),
                zoom.min(),
                zoom.max()
            ));
        }
        if let (Some(lo), Some(hi)) = (self.min_zoom, self.max_zoom)
            && lo > hi
        {
            return invalid(format!(
                "data layer {name}: min zoom {lo} is above max zoom {hi}"
            ));
        }
        for (from, to) in &self.rename_attributes {
            if to.is_empty() {
                return invalid(format!(
                    "data layer {name}: attribute {from} is renamed to nothing"
                ));
            }
        }
        self.source.validate()
    }

    /// The zooms written, within the compile's.
    pub(crate) fn zoom_range(&self, zoom: crate::ZoomRange) -> (u8, u8) {
        (
            self.min_zoom.unwrap_or(zoom.min()).max(zoom.min()),
            self.max_zoom.unwrap_or(zoom.max()).min(zoom.max()),
        )
    }

    /// The attributes a feature keeps, renamed, as tile attributes.
    pub(crate) fn tile_attributes(
        &self,
        properties: BTreeMap<String, PropertyValue>,
    ) -> BTreeMap<String, serde_json::Value> {
        properties
            .into_iter()
            .filter(|(key, _)| {
                self.attributes
                    .as_ref()
                    .is_none_or(|keep| keep.iter().any(|k| k == key))
                    && !self.exclude_attributes.iter().any(|k| k == key)
            })
            .filter_map(|(key, value)| {
                let value = value.json();
                if value.is_null() {
                    return None;
                }
                let key = self.rename_attributes.get(&key).cloned().unwrap_or(key);
                Some((key, value))
            })
            .collect()
    }
}

/// What a compile read for one [`DataLayer`].
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub struct DataLayerReport {
    pub layer: String,
    /// The source file's name.
    pub file: String,
    pub format: String,
    #[serde(flatten)]
    pub read: DataReadReport,
}

#[cfg(test)]
mod tests;
