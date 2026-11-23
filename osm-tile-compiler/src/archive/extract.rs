//! Cuts an area out of an existing PMTiles archive: the tiles within a box,
//! a polygon and a zoom range, copied byte for byte into a new archive.
//!
//! Nothing is decoded or re-encoded, so the result holds exactly the source's
//! tiles at those positions, and it works for any PMTiles v3 archive, from
//! this compiler or another writer. The tiles kept are the ones a compile
//! limited to the same box or polygon writes: those whose square meets the
//! box, and, with a polygon, those whose square meets the polygon.
use super::reader::{ArchiveReader, Entry};
use crate::compile_area::TileCoverage;
use crate::compile_bounds::ArchiveCenter;
use crate::coordinate_groups::lng_lat_to_world;
use crate::output::{OutputFormat, PartialArchive};
use crate::{CompileArea, CompileBounds, CompileError, ZoomRange};
use pmtiles::{Compression, PmTilesWriter, TileCoord, TileId, TileType};
use serde::Serialize;
use serde_json::Value;
use std::path::PathBuf;

/// The latitude limit of Web Mercator tiles.
const MAX_LATITUDE: f64 = 85.051_128_779_806_59;
/// How many directory entries pass between checks of the cancel callback.
const CANCEL_CHECK_ENTRIES: u64 = 4096;

/// What [`extract`] cuts out, and where it writes it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ExtractRequest {
    /// The PMTiles archive to read.
    pub source: PathBuf,
    /// The PMTiles archive to write; it must not exist yet.
    pub output: PathBuf,
    /// Keep only the tiles that meet this box [default: the source's bounds].
    pub bounds: Option<CompileBounds>,
    /// Keep only the tiles that meet this polygon.
    pub polygon: Option<CompileArea>,
    /// Keep only these zooms [default: all the source holds].
    pub zoom: Option<ZoomRange>,
}

impl ExtractRequest {
    /// Copies every tile of `source` into `output`; set
    /// [`bounds`](Self::bounds), [`polygon`](Self::polygon) or
    /// [`zoom`](Self::zoom) to keep fewer.
    pub fn new(source: impl Into<PathBuf>, output: impl Into<PathBuf>) -> Self {
        Self {
            source: source.into(),
            output: output.into(),
            bounds: None,
            polygon: None,
            zoom: None,
        }
    }
}

/// What [`extract`] wrote.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct ExtractReport {
    /// Tiles the output addresses.
    pub tiles: u64,
    /// Distinct tile contents the output stores.
    pub tile_contents: u64,
    /// Tiles the source addresses.
    pub source_tiles: u64,
    /// The output's size.
    pub bytes: u64,
    /// The bytes of the tiles copied, as stored.
    pub tile_bytes: u64,
    /// The output's zoom range.
    pub zooms: [u8; 2],
    /// The output's bounds: west, south, east, north.
    pub bounds: [f64; 4],
    /// The output's center: longitude, latitude and zoom.
    pub center: (f64, f64, u8),
}

/// The rows and columns of tiles a box meets at one zoom.
struct ZoomWindow {
    /// One run of columns, or two for a box crossing the antimeridian.
    columns: Vec<(u32, u32)>,
    rows: (u32, u32),
}

impl ZoomWindow {
    /// The tiles a compile limited to `bounds` writes at zoom `z`: those
    /// whose square meets the box, edges included.
    fn new(bounds: CompileBounds, z: u8) -> Self {
        let last = (1_i64 << z) - 1;
        let row = |latitude: f64| {
            let y = lng_lat_to_world(0.0, latitude.clamp(-MAX_LATITUDE, MAX_LATITUDE), z).y;
            (y.floor() as i64).clamp(0, last) as u32
        };
        let columns = bounds
            .tile_columns(z)
            .map(|(first, last_column)| {
                let clamp = |c: i32| i64::from(c).clamp(0, last) as u32;
                (clamp(first), clamp(last_column))
            })
            .collect();
        Self {
            columns,
            rows: (row(bounds.north), row(bounds.south)),
        }
    }

    fn contains(&self, x: u32, y: u32) -> bool {
        (self.rows.0..=self.rows.1).contains(&y)
            && self
                .columns
                .iter()
                .any(|&(first, last)| (first..=last).contains(&x))
    }
}

/// Which tiles of the source the output keeps.
struct Selection {
    /// The lowest and highest zoom kept. Archives of other writers may go
    /// past [`ZoomRange::MAX_ZOOM`].
    zoom: (u8, u8),
    /// Per zoom from the lowest.
    windows: Vec<ZoomWindow>,
    coverage: Option<TileCoverage>,
}

impl Selection {
    fn keeps(&self, coord: TileCoord) -> bool {
        let z = coord.z();
        (self.zoom.0..=self.zoom.1).contains(&z)
            && self.windows[usize::from(z - self.zoom.0)].contains(coord.x(), coord.y())
            && self
                .coverage
                .as_ref()
                .is_none_or(|coverage| coverage.contains(z, coord.x(), coord.y()))
    }
}

/// The bounds a header declares, or the whole world where they are empty, as
/// writers that do not track them leave them.
fn header_bounds([west, south, east, north]: [f64; 4]) -> CompileBounds {
    let finite = [west, south, east, north].iter().all(|v| v.is_finite());
    if !finite || west == east || south >= north {
        CompileBounds::new(MAX_LATITUDE, -MAX_LATITUDE, 180.0, -180.0)
    } else {
        CompileBounds::new(north, south, east, west)
    }
}

/// The source's metadata with the zooms and, where it states them, the
/// bounds and center of the output. A layer the output's zooms leave out is
/// dropped from `vector_layers`. Metadata that is not a JSON object is kept
/// as it is.
fn metadata(
    source: &str,
    (min, max): (u8, u8),
    bounds: CompileBounds,
    center: ArchiveCenter,
) -> String {
    let Ok(Value::Object(mut metadata)) = serde_json::from_str::<Value>(source) else {
        return source.to_owned();
    };
    let clamp_zooms = |object: &mut serde_json::Map<String, Value>| {
        if object.contains_key("minzoom") {
            let lo = object["minzoom"]
                .as_u64()
                .map_or(min, |v| (v as u8).max(min));
            object["minzoom"] = numeric_like(&object["minzoom"], lo);
        }
        if object.contains_key("maxzoom") {
            let hi = object["maxzoom"]
                .as_u64()
                .map_or(max, |v| (v as u8).min(max));
            object["maxzoom"] = numeric_like(&object["maxzoom"], hi);
        }
    };
    if let Some(minzoom) = metadata.get_mut("minzoom") {
        *minzoom = numeric_like(minzoom, min);
    }
    if let Some(maxzoom) = metadata.get_mut("maxzoom") {
        *maxzoom = numeric_like(maxzoom, max);
    }
    if let Some(Value::Array(layers)) = metadata.get_mut("vector_layers") {
        layers.retain_mut(|layer| {
            let Value::Object(layer) = layer else {
                return true;
            };
            let zooms = |key: &str| layer.get(key).and_then(Value::as_u64);
            if zooms("minzoom").is_some_and(|lo| lo > u64::from(max))
                || zooms("maxzoom").is_some_and(|hi| hi < u64::from(min))
            {
                return false;
            }
            clamp_zooms(layer);
            true
        });
    }
    let archive = bounds.archive_bounds();
    let wsen = [archive.west, archive.south, archive.east, archive.north];
    if let Some(value) = metadata.get_mut("bounds") {
        *value = coordinates_like(value, &wsen);
    }
    if let Some(value) = metadata.get_mut("center") {
        let lon_lat_zoom = [center.longitude, center.latitude, f64::from(center.zoom)];
        *value = coordinates_like(value, &lon_lat_zoom);
    }
    Value::Object(metadata).to_string()
}

/// `number` as a JSON number, or as a string where `like` is one (MBTiles
/// style metadata keeps its numbers as text).
fn numeric_like(like: &Value, number: u8) -> Value {
    match like {
        Value::String(_) => Value::String(number.to_string()),
        _ => Value::from(number),
    }
}

/// `values` as a JSON array, or as comma-separated text where `like` is a
/// string.
fn coordinates_like(like: &Value, values: &[f64]) -> Value {
    match like {
        Value::String(_) => Value::String(
            values
                .iter()
                .map(|v| format!("{}", round7(*v)))
                .collect::<Vec<_>>()
                .join(","),
        ),
        _ => Value::from(values.iter().map(|v| round7(*v)).collect::<Vec<_>>()),
    }
}

/// The header's precision: 1e-7 degrees.
fn round7(value: f64) -> f64 {
    (value * 1e7).round() / 1e7
}

/// Writes the tiles of `request.source` within its box, polygon and zooms to
/// `request.output`, byte for byte. Fails with
/// [`CompileError::InvalidRequest`] when the output exists, the source cannot
/// be read as PMTiles v3, or the area or zooms hold none of the source's
/// tiles; with [`CompileError::Cancelled`] when `should_cancel` asks. On any
/// failure nothing is left at the output.
pub fn extract(
    request: &ExtractRequest,
    should_cancel: impl Fn() -> bool,
) -> Result<ExtractReport, CompileError> {
    let invalid = CompileError::InvalidRequest;
    if request.output.exists() {
        return Err(invalid(format!(
            "refusing to replace existing archive {}",
            request.output.display()
        )));
    }
    let mut reader = ArchiveReader::open(&request.source).map_err(invalid)?;
    let header = reader.header.clone();
    let source = request.source.display();
    if header.min_zoom > header.max_zoom {
        return Err(invalid(format!(
            "{source} declares zooms {} to {}",
            header.min_zoom, header.max_zoom
        )));
    }
    let (min_zoom, max_zoom) = match request.zoom {
        Some(zoom) => (
            zoom.min().max(header.min_zoom),
            zoom.max().min(header.max_zoom),
        ),
        None => (header.min_zoom, header.max_zoom),
    };
    if min_zoom > max_zoom {
        return Err(invalid(format!(
            "{source} holds zooms {} to {}, none of those asked for",
            header.min_zoom, header.max_zoom
        )));
    }

    let mut bounds = header_bounds(header.bounds);
    for (name, limit) in [
        ("box", request.bounds),
        ("polygon", request.polygon.as_ref().map(CompileArea::bounds)),
    ] {
        if let Some(limit) = limit {
            bounds = bounds.intersection(limit).ok_or_else(|| {
                invalid(format!(
                    "the {name} does not overlap {source}, whose bounds are {} {} {} {}",
                    header.bounds[0], header.bounds[1], header.bounds[2], header.bounds[3]
                ))
            })?;
        }
    }
    let selection = Selection {
        zoom: (min_zoom, max_zoom),
        windows: (min_zoom..=max_zoom)
            .map(|z| ZoomWindow::new(bounds, z))
            .collect(),
        coverage: request
            .polygon
            .as_ref()
            .map(|area| TileCoverage::new(area, min_zoom, max_zoom)),
    };

    let center = {
        let [longitude, latitude] = header.center;
        if bounds.contains(longitude, latitude)
            && request
                .polygon
                .as_ref()
                .is_none_or(|area| area.contains(longitude, latitude))
        {
            ArchiveCenter::new(
                longitude,
                latitude,
                header.center_zoom.clamp(min_zoom, max_zoom),
            )
        } else {
            let (mut longitude, mut latitude) =
                (bounds.center_longitude(), bounds.center_latitude());
            if let Some(area) = &request.polygon
                && !area.contains(longitude, latitude)
                && let Some((x, y)) = area.interior_point()
                && bounds.contains(x, y)
            {
                (longitude, latitude) = (x, y);
            }
            ArchiveCenter::new(
                longitude,
                latitude,
                bounds.fit_zoom().clamp(min_zoom, max_zoom),
            )
        }
    };

    let source_metadata = reader.metadata().map_err(|e| {
        CompileError::other(format!("the metadata of {source} cannot be read: {e}"))
    })?;
    let metadata = metadata(&source_metadata, (min_zoom, max_zoom), bounds, center);
    let tile_type = TileType::try_from(header.tile_type).unwrap_or(TileType::Unknown);
    let compression =
        Compression::try_from(header.tile_compression).unwrap_or(Compression::Unknown);
    let archive_bounds = bounds.archive_bounds();

    let partial = PartialArchive::create(&request.output, OutputFormat::PmTiles)
        .map_err(CompileError::other)?;
    let mut writer = PmTilesWriter::new(tile_type)
        .precompressed(compression)
        .min_zoom(min_zoom)
        .max_zoom(max_zoom)
        .bounds(
            archive_bounds.west,
            archive_bounds.south,
            archive_bounds.east,
            archive_bounds.north,
        )
        .center(center.longitude, center.latitude)
        .center_zoom(center.zoom)
        .metadata(metadata)
        // Tiles the source stores once, at one offset, are stored once here.
        .dedup_by_key::<u64>()
        .create(partial.file())
        .map_err(|e| CompileError::other(format!("Failed to start the PMTiles writer: {e}")))?;

    let (mut tiles, mut source_tiles, mut tile_bytes, mut visited) = (0u64, 0u64, 0u64, 0u64);
    let mut contents = std::collections::HashSet::new();
    let mut cancelled = false;
    reader
        .visit_entries(|reader, entry: Entry| {
            visited += 1;
            if visited.is_multiple_of(CANCEL_CHECK_ENTRIES) && should_cancel() {
                cancelled = true;
                return Err("cancelled".into());
            }
            source_tiles += u64::from(entry.run_length);
            let mut stored: Option<Vec<u8>> = None;
            for i in 0..u64::from(entry.run_length) {
                let tile_id = entry.tile_id + i;
                let coord = TileCoord::from(
                    TileId::new(tile_id).map_err(|e| format!("tile ID {tile_id}: {e}"))?,
                );
                if !selection.keeps(coord) {
                    continue;
                }
                if stored.is_none() {
                    stored = Some(reader.tile(&entry)?);
                }
                let bytes = stored.as_deref().unwrap();
                writer
                    .add_tile(coord, bytes, Some(entry.offset))
                    .map_err(|e| format!("Failed to write tile {coord:?}: {e}"))?;
                tiles += 1;
                if contents.insert(entry.offset) {
                    tile_bytes += bytes.len() as u64;
                }
            }
            Ok(())
        })
        .map_err(|e| {
            if cancelled {
                CompileError::Cancelled
            } else {
                CompileError::other(format!("Failed to read {source}: {e}"))
            }
        })?;
    if tiles == 0 {
        return Err(invalid(format!(
            "{source} holds no tiles in the area and zooms asked for"
        )));
    }
    if should_cancel() {
        return Err(CompileError::Cancelled);
    }
    writer
        .finalize()
        .map_err(|e| CompileError::other(format!("Failed to finish the PMTiles archive: {e}")))?;
    partial
        .file()
        .sync_all()
        .map_err(|e| CompileError::other(format!("Failed to write the archive: {e}")))?;
    partial
        .persist(&request.output)
        .map_err(CompileError::other)?;
    let bytes = std::fs::metadata(&request.output)
        .map(|m| m.len())
        .unwrap_or_default();
    Ok(ExtractReport {
        tiles,
        tile_contents: contents.len() as u64,
        source_tiles,
        bytes,
        tile_bytes,
        zooms: [min_zoom, max_zoom],
        bounds: [
            round7(archive_bounds.west),
            round7(archive_bounds.south),
            round7(archive_bounds.east),
            round7(archive_bounds.north),
        ],
        center: (
            round7(center.longitude),
            round7(center.latitude),
            center.zoom,
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn windows_hold_the_tiles_meeting_the_box() {
        // Monaco at zoom 14: columns 8529-8530 as `tile_columns` gives them.
        let monaco = CompileBounds::new(43.7517, 43.7247, 7.4396, 7.4092);
        let window = ZoomWindow::new(monaco, 14);
        assert_eq!(window.columns, [(8529, 8530)]);
        assert_eq!(window.rows, (5973, 5974));
        assert!(window.contains(8530, 5974) && !window.contains(8531, 5974));
        // A box crossing the antimeridian keeps both edges of the world.
        let fiji = CompileBounds::new(-12.0, -21.0, -178.0, 177.0);
        let window = ZoomWindow::new(fiji, 3);
        assert_eq!(window.columns, [(0, 0), (7, 7)]);
        assert!(window.contains(0, 4) && window.contains(7, 4) && !window.contains(3, 4));
        // Latitudes past Web Mercator's stay within the world.
        let world = CompileBounds::new(90.0, -90.0, 180.0, -180.0);
        assert_eq!(ZoomWindow::new(world, 2).rows, (0, 3));
        assert_eq!(ZoomWindow::new(world, 2).columns, [(0, 3)]);
    }

    #[test]
    fn empty_header_bounds_mean_the_world() {
        assert_eq!(header_bounds([0.0; 4]).width(), 360.0);
        let bounds = header_bounds([7.4, 43.7, 7.5, 43.8]);
        assert_eq!((bounds.west, bounds.north), (7.4, 43.8));
    }

    #[test]
    fn metadata_takes_the_new_zooms_bounds_and_center() {
        let source = serde_json::json!({
            "name": "OpenMapTiles",
            "minzoom": 0,
            "maxzoom": 14,
            "bounds": "7.4,43.7,7.5,43.8",
            "vector_layers": [
                {"id": "water", "minzoom": 0, "maxzoom": 14},
                {"id": "housenumber", "minzoom": 14, "maxzoom": 14},
                {"id": "custom"},
            ],
        })
        .to_string();
        let bounds = CompileBounds::new(43.75, 43.72, 7.44, 7.41);
        let center = ArchiveCenter::new(7.425, 43.735, 12);
        let metadata: Value =
            serde_json::from_str(&metadata(&source, (2, 12), bounds, center)).unwrap();
        assert_eq!(metadata["name"], "OpenMapTiles");
        assert_eq!(
            (&metadata["minzoom"], &metadata["maxzoom"]),
            (&2.into(), &12.into())
        );
        assert_eq!(metadata["bounds"], "7.41,43.72,7.44,43.75");
        assert!(metadata.get("center").is_none());
        let layers = metadata["vector_layers"].as_array().unwrap();
        assert_eq!(layers.len(), 2, "{layers:?}");
        assert_eq!(
            (&layers[0]["minzoom"], &layers[0]["maxzoom"]),
            (&2.into(), &12.into())
        );
        assert_eq!(layers[1], serde_json::json!({"id": "custom"}));
        // MBTiles-style text numbers stay text; a center array stays one.
        let text = r#"{"minzoom":"0","maxzoom":"14","center":[0,0,1]}"#;
        let metadata: Value =
            serde_json::from_str(&super::metadata(text, (2, 12), bounds, center)).unwrap();
        assert_eq!(metadata["minzoom"], "2");
        assert_eq!(metadata["center"], serde_json::json!([7.425, 43.735, 12.0]));
        // Anything but an object is copied.
        assert_eq!(super::metadata("[1]", (2, 12), bounds, center), "[1]");
    }
}
