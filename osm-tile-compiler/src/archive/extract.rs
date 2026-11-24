//! Cuts an area out of an existing PMTiles archive: the tiles within a box,
//! a polygon and a zoom range, copied byte for byte into a new archive.
//!
//! Nothing is decoded or re-encoded, so the result holds exactly the source's
//! tiles at those positions, and it works for any PMTiles v3 archive, from
//! this compiler or another writer. The tiles kept are the ones a compile
//! limited to the same box or polygon writes: those whose square meets the
//! box, and, with a polygon, those whose square meets the polygon.
//!
//! The source may also be an `http://` or `https://` URL of an archive on a
//! server that answers range requests. Then only the header, the root
//! directory, the leaf directories that may address kept tiles and the kept
//! tiles themselves are requested, nearby tiles together in one range.
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
/// Kept tiles are read in batches of at most this many tiles...
const BATCH_TILES: usize = 65_536;
/// ...or this many stored bytes.
const BATCH_BYTES: u64 = 64 << 20;
/// Over HTTP, tiles this close in the data section are read in one range,
/// along with the bytes between them...
const HTTP_GAP_BYTES: u64 = 256 << 10;
/// ...up to ranges this long.
const HTTP_RANGE_BYTES: u64 = 8 << 20;

/// What [`extract`] cuts out, and where it writes it.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct ExtractRequest {
    /// The PMTiles archive to read, unless [`source_url`](Self::source_url)
    /// is set.
    pub source: PathBuf,
    /// The `http://` or `https://` URL of the archive to read instead of
    /// [`source`](Self::source). The server must answer range requests.
    pub source_url: Option<String>,
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
            source_url: None,
            output: output.into(),
            bounds: None,
            polygon: None,
            zoom: None,
        }
    }

    /// Copies every tile of the archive at `url`, read by HTTP range
    /// requests, into `output`.
    pub fn remote(url: impl Into<String>, output: impl Into<PathBuf>) -> Self {
        let mut request = Self::new(PathBuf::new(), output);
        request.source_url = Some(url.into());
        request
    }
}

/// Whether `text` is a URL [`ExtractRequest::remote`] reads.
pub fn is_remote_source(text: &str) -> bool {
    let lower = text.get(..8).unwrap_or(text).to_ascii_lowercase();
    lower.starts_with("http://") || lower.starts_with("https://")
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
    /// For a source read over HTTP, the range requests made; otherwise 0.
    pub requests: u64,
    /// For a source read over HTTP, the bytes those requests returned;
    /// otherwise 0.
    pub bytes_read: u64,
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

    /// Whether any tile of the `2^k` by `2^k` square of this zoom's tiles
    /// from column `x` and row `y` is in the window.
    fn meets_square(&self, x: u32, y: u32, k: u8) -> bool {
        let side = 1u64 << k;
        let (x, y) = (u64::from(x), u64::from(y));
        let (top, bottom) = (u64::from(self.rows.0), u64::from(self.rows.1));
        y <= bottom
            && top < y + side
            && self
                .columns
                .iter()
                .any(|&(first, last)| x <= u64::from(last) && u64::from(first) < x + side)
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

    /// Whether any tile ID in `first..end` may be kept, judged by zoom and
    /// box alone. Within a zoom, a run of tile IDs splits into aligned runs
    /// of `4^k`, and the Hilbert curve fills each such run with one square
    /// of `2^k` by `2^k` tiles: the children of one tile `k` zooms up.
    fn may_keep(&self, first: u64, end: u64) -> bool {
        (self.zoom.0..=self.zoom.1).any(|z| {
            let base = zoom_base(z);
            let count = 1u64 << (2 * u32::from(z));
            let mut lo = first.max(base).saturating_sub(base);
            let hi = end.min(base + count).saturating_sub(base);
            let window = &self.windows[usize::from(z - self.zoom.0)];
            while lo < hi {
                let mut k = 0u8;
                while k < z
                    && lo.is_multiple_of(1u64 << (2 * (k + 1)))
                    && lo + (1u64 << (2 * (k + 1))) <= hi
                {
                    k += 1;
                }
                let parent = TileCoord::from(
                    TileId::new(zoom_base(z - k) + (lo >> (2 * k))).expect("a valid tile ID"),
                );
                let (x, y) = (parent.x() << k, parent.y() << k);
                if window.meets_square(x, y, k) {
                    return true;
                }
                lo += 1u64 << (2 * k);
            }
            false
        })
    }
}

/// The first tile ID of zoom `z`: the tiles of all lower zooms.
fn zoom_base(z: u8) -> u64 {
    ((1u64 << (2 * u32::from(z))) - 1) / 3
}

/// Stored tile bytes of one batch, by their offset in the data section.
type Contents = std::collections::HashMap<u64, Vec<u8>>;

/// Reads the stored bytes of `entries` (offset, length), each distinct
/// offset once. Over HTTP, nearby tiles share one range request.
fn read_contents(reader: &mut ArchiveReader, entries: &[Entry]) -> Result<Contents, String> {
    let mut tiles: Vec<(u64, u32)> = entries.iter().map(|e| (e.offset, e.length)).collect();
    tiles.sort_unstable();
    tiles.dedup_by_key(|tile| tile.0);
    let data_offset = reader.header.data_offset;
    let data_length = reader.header.data_length;
    for &(offset, length) in &tiles {
        if offset
            .checked_add(u64::from(length))
            .is_none_or(|end| end > data_length)
        {
            return Err(format!("a tile at {offset} lies beyond the data section"));
        }
    }
    let (gap, longest) = if reader.is_remote() {
        (HTTP_GAP_BYTES, HTTP_RANGE_BYTES)
    } else {
        (0, 0)
    };
    // Spans of the data section: (start, end, the tiles in it).
    let mut spans: Vec<(u64, u64, std::ops::Range<usize>)> = Vec::new();
    for (i, &(offset, length)) in tiles.iter().enumerate() {
        let end = offset + u64::from(length);
        match spans.last_mut() {
            Some((start, span_end, members))
                if offset <= span_end.saturating_add(gap) && end - *start <= longest.max(1) =>
            {
                *span_end = (*span_end).max(end);
                members.end = i + 1;
            }
            _ => spans.push((offset, end, i..i + 1)),
        }
    }
    let ranges: Vec<(u64, u64)> = spans
        .iter()
        .map(|(start, end, _)| (data_offset + start, end - start))
        .collect();
    let mut contents = Contents::with_capacity(tiles.len());
    for ((start, _, members), bytes) in spans.into_iter().zip(reader.read_many(&ranges)?) {
        for &(offset, length) in &tiles[members] {
            let at = (offset - start) as usize;
            contents.insert(offset, bytes[at..at + length as usize].to_vec());
        }
    }
    Ok(contents)
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
    let (mut reader, source) = match &request.source_url {
        Some(url) => {
            if !is_remote_source(url) {
                return Err(invalid(format!("{url} is not an http:// or https:// URL")));
            }
            (ArchiveReader::open_url(url).map_err(invalid)?, url.clone())
        }
        None => (
            ArchiveReader::open(&request.source).map_err(invalid)?,
            request.source.display().to_string(),
        ),
    };
    let header = reader.header.clone();
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
    // Kept tiles wait here, in tile ID order, until a batch is read.
    let mut pending: Vec<(TileCoord, Entry)> = Vec::new();
    let mut pending_bytes = 0u64;
    let mut flush =
        |reader: &mut ArchiveReader, pending: &mut Vec<(TileCoord, Entry)>| -> Result<(), String> {
            if pending.is_empty() {
                return Ok(());
            }
            let entries: Vec<Entry> = pending.iter().map(|(_, entry)| *entry).collect();
            let stored = read_contents(reader, &entries)?;
            for (coord, entry) in pending.drain(..) {
                let bytes = &stored[&entry.offset];
                writer
                    .add_tile(coord, bytes, Some(entry.offset))
                    .map_err(|e| format!("Failed to write tile {coord:?}: {e}"))?;
                tiles += 1;
                if contents.insert(entry.offset) {
                    tile_bytes += bytes.len() as u64;
                }
            }
            Ok(())
        };
    // The header's count stands in for the tiles under the leaves that are
    // skipped; without one, every leaf is read to count them.
    let skip_leaves = header.addressed_tiles > 0;
    reader
        .visit_entries_within(
            |first, end| !skip_leaves || selection.may_keep(first, end),
            |reader, entry: Entry| {
                visited += 1;
                if visited.is_multiple_of(CANCEL_CHECK_ENTRIES) && should_cancel() {
                    cancelled = true;
                    return Err("cancelled".into());
                }
                source_tiles += u64::from(entry.run_length);
                let mut kept_entry = false;
                for i in 0..u64::from(entry.run_length) {
                    let tile_id = entry.tile_id + i;
                    let coord = TileCoord::from(
                        TileId::new(tile_id).map_err(|e| format!("tile ID {tile_id}: {e}"))?,
                    );
                    if !selection.keeps(coord) {
                        continue;
                    }
                    pending.push((coord, entry));
                    kept_entry = true;
                }
                if kept_entry {
                    pending_bytes += u64::from(entry.length);
                }
                if pending.len() >= BATCH_TILES || pending_bytes >= BATCH_BYTES {
                    if should_cancel() {
                        cancelled = true;
                        return Err("cancelled".into());
                    }
                    flush(reader, &mut pending)?;
                    pending_bytes = 0;
                }
                Ok(())
            },
        )
        .and_then(|()| flush(&mut reader, &mut pending))
        .map_err(|e| {
            if cancelled {
                CompileError::Cancelled
            } else {
                CompileError::other(format!("Failed to read {source}: {e}"))
            }
        })?;
    if skip_leaves {
        source_tiles = header.addressed_tiles;
    }
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
        requests: reader.transferred().0,
        bytes_read: reader.transferred().1,
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
    fn leaf_spans_are_kept_when_they_may_hold_a_kept_tile() {
        // Brute force: a span may keep a tile when one of its tiles is in a
        // window. Spans of every alignment, over zooms 0 to 6.
        let boxes = [
            CompileBounds::new(43.7517, 43.7247, 7.4396, 7.4092),
            CompileBounds::new(-12.0, -21.0, -178.0, 177.0),
            CompileBounds::new(60.0, 10.0, 40.0, -20.0),
        ];
        for bounds in boxes {
            for zoom in [(0, 6), (3, 5), (6, 6)] {
                let selection = Selection {
                    zoom,
                    windows: (zoom.0..=zoom.1)
                        .map(|z| ZoomWindow::new(bounds, z))
                        .collect(),
                    coverage: None,
                };
                let keeps = |id: u64| selection.keeps(TileCoord::from(TileId::new(id).unwrap()));
                let last = zoom_base(7);
                let mut state = 12_345u64;
                for _ in 0..3000 {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    let first = (state >> 33) % last;
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    let length = 1 + (state >> 33) % [5, 70, 1500][(state % 3) as usize];
                    let end = (first + length).min(last);
                    let expected = (first..end).any(keeps);
                    assert_eq!(
                        selection.may_keep(first, end),
                        expected,
                        "{first}..{end} for {bounds:?} at {zoom:?}"
                    );
                }
                // Past the last zoom kept, and the root's open end.
                assert!(!selection.may_keep(zoom_base(7), zoom_base(9)));
                assert!(selection.may_keep(0, u64::MAX));
            }
        }
    }

    #[test]
    fn leaves_outside_the_area_are_skipped() {
        // Every tile of zooms 0-8, each with its own bytes: a directory
        // that needs leaves.
        let dir = tempfile::tempdir().unwrap();
        let source = dir.path().join("world.pmtiles");
        let mut writer = PmTilesWriter::new(TileType::Mvt)
            .precompressed(Compression::None)
            .min_zoom(0)
            .max_zoom(8)
            .create(std::fs::File::create(&source).unwrap())
            .unwrap();
        let count = zoom_base(9);
        for id in 0..count {
            let coord = TileCoord::from(TileId::new(id).unwrap());
            writer.add_tile(coord, &id.to_le_bytes()).unwrap();
        }
        writer.finalize().unwrap();
        let reader = ArchiveReader::open(&source).unwrap();
        assert!(reader.header.leaf_length > 0, "{:?}", reader.header);
        drop(reader);

        let bounds = CompileBounds::new(43.75, 43.72, 7.44, 7.41);
        let mut request = ExtractRequest::new(&source, dir.path().join("monaco.pmtiles"));
        request.bounds = Some(bounds);
        let report = extract(&request, || false).unwrap();
        assert_eq!(report.source_tiles, count);
        let mut reader = ArchiveReader::open(&request.output).unwrap();
        let mut kept = Vec::new();
        reader
            .visit_entries(|reader, entry| {
                assert_eq!(reader.tile(&entry)?, entry.tile_id.to_le_bytes());
                kept.push(entry.tile_id);
                Ok(())
            })
            .unwrap();
        let expected: Vec<u64> = (0..count)
            .filter(|&id| {
                let coord = TileCoord::from(TileId::new(id).unwrap());
                ZoomWindow::new(bounds, coord.z()).contains(coord.x(), coord.y())
            })
            .collect();
        assert_eq!(kept, expected);
        assert_eq!(report.tiles, expected.len() as u64);
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
