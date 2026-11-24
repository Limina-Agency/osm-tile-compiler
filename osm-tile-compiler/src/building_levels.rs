//! Building levels: a second PMTiles archive beside a finished OpenMapTiles
//! basemap, holding its buildings as instanced boxes for views too far away
//! for polygons.
//!
//! z14 has one oriented box per building, or several where a footprint fills
//! its box badly. z12 and z11 are coarse: every polygon goes into a grid per
//! z13 tile, the grids are reduced like a mip, and each is merged into
//! rectangles with no rotation. The few buildings that stand well over their
//! cell also go in whole. z13 is only the source of the others and is not
//! written.
//!
//! The archive is PMTiles v3 with tile type unknown, gzipped per tile. Each
//! tile is a little-endian `u32` count and then columns of that many values:
//! `x`, `y`, `half_u`, `half_v`, `angle`, `base`, `top` as `u16`, then `r`,
//! `g`, `b` and a zero byte as `u8`, 18 bytes a box. Every level is in a tile
//! of 4096 units, whatever the source extent. A tile with no boxes is not
//! written.
//!
//! Tiles are worked on in groups of one z11 tile, in a fixed order within a
//! group, so the bytes do not depend on the number of threads. The source
//! archive is read a tile at a time, and a group's z14 tiles go quadrant by
//! quadrant, so it has one grid a zoom open: memory is bounded by three grids
//! a thread, plus the finished tiles.

use crate::control::{CompileError, Control};
use crate::output::{OutputFormat, PartialArchive};
use fast_mvt::{MvtFeatureRef, MvtReaderRef, MvtValueRef};
use flate2::{read::GzDecoder, write::GzEncoder};
use pmtiles::{Compression, PmTilesWriter, TileCoord, TileId, TileType};
use rayon::prelude::*;
use std::borrow::Cow;
use std::f32::consts::PI;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::time::Instant;

/// Where `compile` writes an archive's building levels: beside it, named after it, so
/// `basemap.pmtiles` gets `basemap-buildings.pmtiles` and two archives in one directory
/// keep their own.
pub fn building_levels_path(archive: &Path) -> PathBuf {
    let stem = archive
        .file_stem()
        .map_or_else(|| "basemap".into(), |s| s.to_string_lossy());
    archive.with_file_name(format!("{stem}-buildings.pmtiles"))
}

/// The building-level archives' metadata `format`.
pub const BUILDING_LEVELS_FORMAT: &str = "building-levels";
/// Their metadata `version`. See `docs/formats.md` for when it changes.
pub const BUILDING_LEVELS_VERSION: u32 = 1;
/// Spelled out so the bytes stay fixed; a test below checks it against the
/// two constants above.
const METADATA: &str = r#"{"format":"building-levels","version":1,"source_layer":"building"}"#;
const SOURCE_LAYER: &str = "building";
const SOURCE_ZOOM: u8 = 14;
/// The zooms written, lowest first.
const LEVEL_ZOOMS: [u8; 3] = [11, 12, 14];
/// Multiplies z11's cover when sizing its boxes, up to a cover of 1: merged
/// solids have fewer walls than the houses they replace, and show less light.
/// Set against a picture of the source: 99.7% of its light, 15.9% error.
const Z11_COVER_GAIN: f32 = 1.15;

/// Units across a tile at every level.
const EXTENT: f32 = 4096.0;
const GRID: usize = 256;
/// Units across a grid cell of a tile at that cell's own zoom.
const CELL_UNITS: f32 = EXTENT / GRID as f32;
/// A z14 tile is a quarter of its z13 grid: half the cells each way.
const Z14_UNITS_PER_Z13_CELL: f32 = 2.0 * CELL_UNITS;
const SCANLINES: usize = 4;

/// Under this cover a cell hands its buildings on to the cells right and
/// below, so villages stay as fewer boxes, with their area.
const MIN_COVER: f32 = 0.06;
/// Cells cover goes along a row looking for more. It keeps its own centroid, but cover from
/// further apart would stand between the houses it came from.
const MAX_CARRY: u8 = 8;
/// A building this many cells over its cell's mean height also goes in whole.
const LANDMARK_CELLS: f32 = 1.5;
/// A footprint under this share of its box is split into several boxes.
const SPLIT_BELOW: f32 = 0.8;
/// Cell of the grid that splits a footprint: these metres, or the long side
/// over [`SPLIT_MAX_CELLS`] if that is more.
const SPLIT_M: f32 = 3.0;
const SPLIT_MAX_CELLS: f32 = 48.0;
/// Cells merge within this height: half a cell or this many metres.
const MIN_HEIGHT_STEP_M: f32 = 3.0;

const DEFAULT_TOP_M: f32 = 5.0;
const DEFAULT_RGB: [u8; 3] = [200, 200, 200];
const EQUATOR_M: f64 = 40_075_016.686;
const GZIP_LEVEL: u32 = 6;
const RECORD_BYTES: usize = 18;
const HEADER_BYTES: usize = 127;
const COMPRESSION_NONE: u8 = 1;
const COMPRESSION_GZIP: u8 = 2;
const TILE_TYPE_MVT: u8 = 1;
const TILE_TYPE_MLT: u8 = 6;

/// What one level of the archive holds.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct BuildingLevelReport {
    pub zoom: u8,
    pub tiles: u64,
    /// Records in those tiles, landmarks included.
    pub boxes: u64,
    /// Bytes of the tiles before gzip.
    pub raw_bytes: u64,
    pub gz_bytes: u64,
    /// Area of the level's boxes over the area of what they stand for,
    /// landmarks left out. z14 is against the footprints, and over 1 because
    /// a box stands round its building. z12 and z11 are against the cover of
    /// their grids, and z11 includes its cover gain, so it can pass 1 too.
    pub area_kept: f64,
}

/// What [`write_building_levels`] wrote.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct BuildingLevelsReport {
    /// One entry per zoom, lowest first.
    pub levels: Vec<BuildingLevelReport>,
    /// Wall time of the whole pass.
    pub seconds: f64,
}

/// Reads the z14 tiles of the OpenMapTiles archive `input` and writes its
/// building levels to the new archive `output`. The bytes are the same for any
/// number of `threads`. An existing `output` is never replaced.
pub fn write_building_levels(
    input: &Path,
    output: &Path,
    threads: usize,
) -> Result<BuildingLevelsReport, CompileError> {
    if threads == 0 {
        return Err(CompileError::InvalidRequest(
            "threads must be positive".into(),
        ));
    }
    if output.exists() {
        return Err(CompileError::InvalidRequest(format!(
            "refusing to replace existing archive {}",
            output.display()
        )));
    }
    write_levels(input, output, threads, &Control::detached()).map_err(CompileError::other)
}

/// [`write_building_levels`], stopping when `should_cancel` returns true,
/// which it is asked every 50 ms or so. Whatever the outcome, no partial
/// archive is left at `output`.
pub fn write_building_levels_cancellable(
    input: &Path,
    output: &Path,
    threads: usize,
    should_cancel: impl Fn() -> bool,
) -> Result<BuildingLevelsReport, CompileError> {
    crate::control::run(
        threads,
        false,
        |_| {},
        should_cancel,
        |control| write_levels(input, output, threads, control),
    )
}

/// [`write_building_levels`], with progress and cancelling. Inside a rayon
/// pool of `threads` threads it uses that pool rather than building one.
pub(crate) fn write_levels(
    input: &Path,
    output: &Path,
    threads: usize,
    control: &Control,
) -> Result<BuildingLevelsReport, String> {
    let start = Instant::now();
    if threads == 0 {
        return Err("threads must be positive".into());
    }
    if output.exists() {
        return Err(format!(
            "refusing to replace existing archive {}",
            output.display()
        ));
    }
    let scan_stage = crate::stage_profile::Stage::start("building_levels.scan");
    let archive = Archive::open(input)?;
    let header = Header::parse(&archive.section(0, HEADER_BYTES as u64)?)?;
    if !matches!(header.tile_type, TILE_TYPE_MVT | TILE_TYPE_MLT) {
        return Err(format!("{} holds no vector tiles", input.display()));
    }
    let mlt = header.tile_type == TILE_TYPE_MLT;
    let mut tiles = source_tiles(&archive, &header)?;
    tiles.sort_unstable_by_key(|tile| (tile.x >> 3, tile.y >> 3, tile.x, tile.y));
    let groups: Vec<&[SourceTile]> = tiles
        .chunk_by(|a, b| (a.x >> 3, a.y >> 3) == (b.x >> 3, b.y >> 3))
        .collect();
    control.set_total(groups.len() as u64);
    drop(scan_stage);
    let levels_stage = crate::stage_profile::Stage::start("building_levels.levels");
    let levels = || {
        groups
            .par_iter()
            .map_init(OpenGrids::default, |grids, group| {
                control.check()?;
                let levels = group_levels(&archive, header.tile_compression, mlt, group, grids);
                control.add_done(1);
                levels
            })
            .collect::<Result<Vec<_>, String>>()
    };
    let groups =
        if rayon::current_thread_index().is_some() && rayon::current_num_threads() == threads {
            levels()?
        } else {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .map_err(|e| format!("Failed to start {threads} threads: {e}"))?
                .install(levels)?
        };
    control.check()?;
    drop(levels_stage);

    let _write_stage = crate::stage_profile::Stage::start("building_levels.write");
    let mut encoded = Vec::new();
    let mut stats = [LevelStats::default(); LEVEL_ZOOMS.len()];
    for group in groups {
        encoded.extend(group.tiles);
        for (total, part) in stats.iter_mut().zip(&group.stats) {
            total.merge(part);
        }
    }
    encoded.sort_unstable_by_key(|tile| tile.0);
    write_archive(output, &header, &encoded)?;

    let levels = LEVEL_ZOOMS
        .iter()
        .zip(&stats)
        .map(|(&zoom, level)| level.report(zoom))
        .collect();
    Ok(BuildingLevelsReport {
        levels,
        seconds: start.elapsed().as_secs_f64(),
    })
}

/// Writes the levels at [`building_levels_path`] beside `archive` if it has a building
/// layer and reaches z14; None if it does not.
pub fn write_building_levels_beside(
    archive: &Path,
    threads: usize,
) -> Result<Option<BuildingLevelsReport>, CompileError> {
    if !has_building_layer(archive).map_err(CompileError::other)? {
        return Ok(None);
    }
    let output = building_levels_path(archive);
    write_building_levels(archive, &output, threads).map(Some)
}

/// Whether the archive has a building layer that reaches z14.
pub(crate) fn has_building_layer(archive: &Path) -> Result<bool, String> {
    let failed = |e: std::io::Error| format!("Failed to read {}: {e}", archive.display());
    let mut file = File::open(archive).map_err(failed)?;
    let mut head = [0_u8; HEADER_BYTES];
    file.read_exact(&mut head).map_err(failed)?;
    let header = Header::parse(&head)?;
    file.seek(SeekFrom::Start(header.metadata.0))
        .map_err(failed)?;
    let mut stored = Vec::new();
    file.take(header.metadata.1)
        .read_to_end(&mut stored)
        .map_err(failed)?;
    let text = unpack(&stored, header.internal_compression)?;
    let metadata: serde_json::Value = serde_json::from_slice(&text)
        .map_err(|e| format!("Failed to parse the metadata of {}: {e}", archive.display()))?;
    let has_layer = metadata["vector_layers"].as_array().is_some_and(|layers| {
        layers
            .iter()
            .any(|layer| layer["id"].as_str() == Some(SOURCE_LAYER))
    });
    Ok(has_layer && header.max_zoom >= SOURCE_ZOOM)
}

fn write_archive(output: &Path, header: &Header, tiles: &[(u64, Vec<u8>)]) -> Result<(), String> {
    let failed = |e: pmtiles::PmtError| format!("Failed to write {}: {e}", output.display());
    let partial = PartialArchive::create(output, OutputFormat::PmTiles)?;
    let [west, south, east, north] = header.bounds;
    let [longitude, latitude] = header.center;
    let mut writer = PmTilesWriter::new(TileType::Unknown)
        .precompressed(Compression::Gzip)
        .min_zoom(LEVEL_ZOOMS[0])
        .max_zoom(LEVEL_ZOOMS[LEVEL_ZOOMS.len() - 1])
        .bounds(west, south, east, north)
        .center(longitude, latitude)
        .center_zoom(LEVEL_ZOOMS[0])
        .metadata(METADATA)
        .create(partial.file())
        .map_err(failed)?;
    for (id, bytes) in tiles {
        let coord = TileCoord::from(TileId::new(*id).map_err(failed)?);
        writer.add_tile(coord, bytes).map_err(failed)?;
    }
    writer.finalize().map_err(failed)?;
    partial.persist(output)
}

// The source archive. The pmtiles crate reads only through async backends.

/// What the pass needs from a PMTiles v3 header. Sections are (offset, length).
struct Header {
    root: (u64, u64),
    metadata: (u64, u64),
    leaves: u64,
    tile_data: u64,
    internal_compression: u8,
    tile_compression: u8,
    tile_type: u8,
    max_zoom: u8,
    /// West, south, east, north, in degrees.
    bounds: [f64; 4],
    /// Longitude, latitude, in degrees.
    center: [f64; 2],
}

impl Header {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() < HEADER_BYTES || &bytes[..7] != b"PMTiles" || bytes[7] != 3 {
            return Err("not a PMTiles v3 archive".into());
        }
        let number = |at: usize| {
            let mut field = [0; 8];
            field.copy_from_slice(&bytes[at..at + 8]);
            u64::from_le_bytes(field)
        };
        let degrees = |at: usize| {
            let mut field = [0; 4];
            field.copy_from_slice(&bytes[at..at + 4]);
            f64::from(i32::from_le_bytes(field)) / 1e7
        };
        Ok(Self {
            root: (number(8), number(16)),
            metadata: (number(24), number(32)),
            leaves: number(40),
            tile_data: number(56),
            internal_compression: bytes[97],
            tile_compression: bytes[98],
            tile_type: bytes[99],
            max_zoom: bytes[101],
            bounds: [degrees(102), degrees(106), degrees(110), degrees(114)],
            center: [degrees(119), degrees(123)],
        })
    }
}

#[derive(Clone, Copy)]
struct SourceTile {
    x: u32,
    y: u32,
    offset: u64,
    length: u64,
}

struct DirectoryEntry {
    id: u64,
    run: u32,
    offset: u64,
    length: u64,
}

/// The source archive, read a part at a time: holding a whole basemap would
/// cost its size in memory.
struct Archive {
    file: File,
    length: u64,
    path: PathBuf,
}

impl Archive {
    fn open(path: &Path) -> Result<Self, String> {
        let failed = |e: std::io::Error| format!("Failed to read {}: {e}", path.display());
        let file = File::open(path).map_err(failed)?;
        let length = file.metadata().map_err(failed)?.len();
        Ok(Self {
            file,
            length,
            path: path.to_path_buf(),
        })
    }

    /// `length` bytes at `offset`. Safe to call from several threads at once.
    fn section(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let mut bytes = Vec::new();
        self.section_into(offset, length, &mut bytes)?;
        Ok(bytes)
    }

    /// [`Archive::section`] into `bytes`, reusing its room.
    fn section_into(&self, offset: u64, length: u64, bytes: &mut Vec<u8>) -> Result<(), String> {
        let outside = || format!("{length} bytes at {offset} are outside the archive");
        let end = offset.checked_add(length).ok_or_else(outside)?;
        if end > self.length {
            return Err(outside());
        }
        bytes.clear();
        bytes.resize(usize::try_from(length).map_err(|_| outside())?, 0);
        read_exact_at(&self.file, bytes, offset)
            .map_err(|e| format!("Failed to read {}: {e}", self.path.display()))
    }
}

#[cfg(unix)]
fn read_exact_at(file: &File, bytes: &mut [u8], offset: u64) -> std::io::Result<()> {
    std::os::unix::fs::FileExt::read_exact_at(file, bytes, offset)
}

#[cfg(windows)]
fn read_exact_at(file: &File, mut bytes: &mut [u8], mut offset: u64) -> std::io::Result<()> {
    use std::io::ErrorKind;
    use std::os::windows::fs::FileExt;
    while !bytes.is_empty() {
        match file.seek_read(bytes, offset) {
            Ok(0) => return Err(ErrorKind::UnexpectedEof.into()),
            Ok(read) => {
                bytes = &mut bytes[read..];
                offset += read as u64;
            }
            Err(e) if e.kind() == ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Elsewhere, through a handle of its own, so threads do not share a position.
#[cfg(not(any(unix, windows)))]
fn read_exact_at(file: &File, bytes: &mut [u8], offset: u64) -> std::io::Result<()> {
    let mut file = file.try_clone()?;
    file.seek(SeekFrom::Start(offset))?;
    file.read_exact(bytes)
}

fn unpack(bytes: &[u8], compression: u8) -> Result<Cow<'_, [u8]>, String> {
    match compression {
        COMPRESSION_NONE => Ok(Cow::Borrowed(bytes)),
        _ => {
            let mut raw = Vec::with_capacity(bytes.len() * 4);
            unpack_into(bytes, compression, &mut raw)?;
            Ok(Cow::Owned(raw))
        }
    }
}

/// [`unpack`] into `raw`, reusing its room; `bytes` themselves if they are
/// not compressed.
fn unpack_into<'a>(
    bytes: &'a [u8],
    compression: u8,
    raw: &'a mut Vec<u8>,
) -> Result<&'a [u8], String> {
    match compression {
        COMPRESSION_NONE => Ok(bytes),
        COMPRESSION_GZIP => {
            raw.clear();
            GzDecoder::new(bytes)
                .read_to_end(raw)
                .map_err(|e| format!("Failed to gunzip part of the archive: {e}"))?;
            Ok(raw)
        }
        other => Err(format!("Unsupported PMTiles compression {other}")),
    }
}

fn directory(bytes: &[u8]) -> Option<Vec<DirectoryEntry>> {
    let mut reader = Protobuf::new(bytes);
    let count = usize::try_from(reader.varint()?).ok()?;
    let mut entries = Vec::with_capacity(count.min(bytes.len()));
    let mut id = 0_u64;
    for _ in 0..count {
        id = id.checked_add(reader.varint()?)?;
        entries.push(DirectoryEntry {
            id,
            run: 0,
            offset: 0,
            length: 0,
        });
    }
    for entry in &mut entries {
        entry.run = u32::try_from(reader.varint()?).ok()?;
    }
    for entry in &mut entries {
        entry.length = reader.varint()?;
    }
    let mut next = 0_u64;
    for entry in &mut entries {
        // Zero means straight after the entry before.
        entry.offset = reader.varint()?.checked_sub(1).unwrap_or(next);
        next = entry.offset.checked_add(entry.length)?;
    }
    Some(entries)
}

fn source_tiles(archive: &Archive, header: &Header) -> Result<Vec<SourceTile>, String> {
    let wanted = pmtiles::PYRAMID_SIZE_BY_ZOOM[usize::from(SOURCE_ZOOM)]
        ..pmtiles::PYRAMID_SIZE_BY_ZOOM[usize::from(SOURCE_ZOOM) + 1];
    let mut directories = vec![header.root];
    let mut tiles = Vec::new();
    while let Some((offset, length)) = directories.pop() {
        let stored = archive.section(offset, length)?;
        let bytes = unpack(&stored, header.internal_compression)?;
        let entries = directory(&bytes).ok_or("Malformed PMTiles directory")?;
        for entry in entries {
            if entry.run == 0 {
                directories.push((header.leaves.saturating_add(entry.offset), entry.length));
                continue;
            }
            let end = entry.id.saturating_add(u64::from(entry.run));
            for id in entry.id.max(wanted.start)..end.min(wanted.end) {
                let id = TileId::new(id).map_err(|e| format!("Invalid tile id {id}: {e}"))?;
                let coord = TileCoord::from(id);
                tiles.push(SourceTile {
                    x: coord.x(),
                    y: coord.y(),
                    offset: header.tile_data.saturating_add(entry.offset),
                    length: entry.length,
                });
            }
        }
    }
    Ok(tiles)
}

/// Reads the varints of a PMTiles directory.
struct Protobuf<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Protobuf<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, at: 0 }
    }

    fn varint(&mut self) -> Option<u64> {
        let (mut value, mut shift) = (0_u64, 0_u32);
        while shift < u64::BITS {
            let byte = *self.bytes.get(self.at)?;
            self.at += 1;
            value |= u64::from(byte & 127) << shift;
            if byte < 128 {
                return Some(value);
            }
            shift += 7;
        }
        None
    }
}

// Vector tiles.

/// An attribute's value as a number of any MVT type, read as `f32`.
fn number(value: MvtValueRef<'_>) -> Option<f32> {
    Some(match value {
        MvtValueRef::Float(v) => v,
        MvtValueRef::Double(v) => v as f32,
        MvtValueRef::Int(v) | MvtValueRef::SInt(v) => v as f32,
        MvtValueRef::UInt(v) => v as f32,
        MvtValueRef::String(_) | MvtValueRef::Bool(_) | MvtValueRef::Null => return None,
    })
}

fn hex_colour(text: &str) -> Option<[u8; 3]> {
    let digits = text.strip_prefix('#')?;
    if !digits.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return None;
    }
    let value = u32::from_str_radix(digits, 16).ok()?;
    match digits.len() {
        6 => Some([(value >> 16) as u8, (value >> 8) as u8, value as u8]),
        3 => Some([8, 4, 0].map(|shift| ((value >> shift) & 15) as u8 * 17)),
        _ => None,
    }
}

type Ring = Vec<[f32; 2]>;

#[derive(Clone, Copy)]
struct Style {
    base: f32,
    top: f32,
    rgb: [u8; 3],
}

/// A building polygon clipped to its tile, in tile units. The first ring is
/// the outer one, and every ring has at least three points.
struct Footprint {
    rings: Vec<Ring>,
    area: f32,
    style: Style,
}

/// The building polygons of a z14 tile, or None if the tile is malformed.
fn parse_footprints(tile: &[u8]) -> Option<Vec<Footprint>> {
    let reader = MvtReaderRef::new(tile).ok()?;
    let mut footprints = Vec::new();
    for layer in reader.layers() {
        if layer.name() != SOURCE_LAYER {
            continue;
        }
        if layer.extent() == 0 {
            return None;
        }
        let scale = EXTENT / layer.extent() as f32;
        for feature in layer.features() {
            parse_feature(feature, scale, &mut footprints)?;
        }
    }
    Some(footprints)
}

fn parse_feature(
    feature: MvtFeatureRef<'_>,
    scale: f32,
    footprints: &mut Vec<Footprint>,
) -> Option<()> {
    if feature.geom_type_value() != Some(3) {
        return Some(());
    }
    let mut style = Style {
        base: 0.0,
        top: DEFAULT_TOP_M,
        rgb: DEFAULT_RGB,
    };
    let mut hidden = false;
    for property in feature.properties() {
        match property.ok()? {
            ("render_height", value) => style.top = number(value).unwrap_or(style.top),
            ("render_min_height", value) => style.base = number(value).unwrap_or(style.base),
            ("colour", MvtValueRef::String(text)) => {
                style.rgb = hex_colour(text).unwrap_or(style.rgb)
            }
            ("hide_3d", MvtValueRef::Bool(flag)) => hidden = flag,
            _ => {}
        }
    }
    // Outlines, drawn again by their parts.
    if hidden {
        return Some(());
    }
    let rings = parse_rings(feature.geometry_commands(), scale)?;
    let Some(first) = rings.first() else {
        return Some(());
    };
    // The first ring's winding is the outer one; the rings after an outer
    // ring are its holes, up to the next outer ring.
    let outer = signed_area(first).signum();
    let mut polygon: Vec<Ring> = Vec::new();
    for ring in rings {
        if signed_area(&ring).signum() == outer && !polygon.is_empty() {
            footprints.extend(clipped_footprint(&polygon, style));
            polygon.clear();
        }
        polygon.push(ring);
    }
    footprints.extend(clipped_footprint(&polygon, style));
    Some(())
}

fn parse_rings(mut commands: impl Iterator<Item = u32>, scale: f32) -> Option<Vec<Ring>> {
    let unzigzag = |value: u32| (value >> 1) as i32 ^ -((value & 1) as i32);
    let (mut x, mut y) = (0_i32, 0_i32);
    let (mut rings, mut ring) = (Vec::new(), Vec::new());
    while let Some(command) = commands.next() {
        match command & 7 {
            1 | 2 => {
                for _ in 0..command >> 3 {
                    x = x.wrapping_add(unzigzag(commands.next()?));
                    y = y.wrapping_add(unzigzag(commands.next()?));
                    ring.push([x as f32 * scale, y as f32 * scale]);
                }
            }
            7 => {
                let closed = std::mem::take(&mut ring);
                if closed.len() >= 3 {
                    rings.push(closed);
                }
            }
            _ => return None,
        }
    }
    Some(rings)
}

/// The shoelace area. Which sign is the outer one depends on the tile.
fn signed_area(ring: &[[f32; 2]]) -> f64 {
    let mut doubled = 0.0;
    let mut a = ring[ring.len() - 1];
    for &b in ring {
        doubled += f64::from(a[0]) * f64::from(b[1]) - f64::from(b[0]) * f64::from(a[1]);
        a = b;
    }
    doubled / 2.0
}

/// Clips a polygon to the tile, or None if nothing of it is inside. Tiles
/// carry a buffer, so without this a building on an edge would come twice.
fn clipped_footprint(rings: &[Ring], style: Style) -> Option<Footprint> {
    let rings: Vec<Ring> = rings
        .iter()
        .map(|ring| clip_ring(ring))
        .filter(|ring| ring.len() >= 3)
        .collect();
    let area: f64 = rings.iter().map(|ring| signed_area(ring)).sum();
    let area = area.abs() as f32;
    (area > 0.0).then_some(Footprint { rings, area, style })
}

/// Clips a ring to the tile square (Sutherland-Hodgman); edges along the
/// boundary then lie exactly on it.
fn clip_ring(ring: &[[f32; 2]]) -> Ring {
    let mut out = ring.to_vec();
    if ring.iter().flatten().all(|v| (0.0..=EXTENT).contains(v)) {
        return out;
    }
    let planes: [(usize, f32, f32); 4] = [
        (0, 0.0, 1.0),
        (0, EXTENT, -1.0),
        (1, 0.0, 1.0),
        (1, EXTENT, -1.0),
    ];
    for (axis, at, side) in planes {
        let inside = |p: [f32; 2]| (p[axis] - at) * side >= 0.0;
        let input = std::mem::take(&mut out);
        let Some(&last) = input.last() else {
            break;
        };
        let mut a = last;
        for &b in &input {
            if inside(a) != inside(b) {
                let t = (at - a[axis]) / (b[axis] - a[axis]);
                let mut crossing = [a[0] + (b[0] - a[0]) * t, a[1] + (b[1] - a[1]) * t];
                crossing[axis] = at;
                out.push(crossing);
            }
            if inside(b) {
                out.push(b);
            }
            a = b;
        }
    }
    out
}

// Boxes.

/// A box in tile units, `half_u` along the direction `angle`, in radians with
/// x right and y down.
#[derive(Clone, Copy)]
struct LevelBox {
    x: f32,
    y: f32,
    half_u: f32,
    half_v: f32,
    angle: f32,
    style: Style,
}

fn hull(points: &[[f32; 2]]) -> Ring {
    let mut sorted = points.to_vec();
    sorted.sort_unstable_by(|a, b| a[0].total_cmp(&b[0]).then(a[1].total_cmp(&b[1])));
    sorted.dedup();
    if sorted.len() < 3 {
        return sorted;
    }
    let mut hull: Ring = Vec::with_capacity(sorted.len() + 1);
    for &point in &sorted {
        extend_hull(&mut hull, 2, point);
    }
    let lower = hull.len() + 1;
    for &point in sorted.iter().rev().skip(1) {
        extend_hull(&mut hull, lower, point);
    }
    hull.pop();
    hull
}

/// Adds `point`, first dropping the last points that do not turn towards it,
/// while `hull` keeps at least `floor - 1` of them.
fn extend_hull(hull: &mut Ring, floor: usize, point: [f32; 2]) {
    while hull.len() >= floor {
        let (o, a) = (hull[hull.len() - 2], hull[hull.len() - 1]);
        let (ax, ay) = (f64::from(a[0] - o[0]), f64::from(a[1] - o[1]));
        let (bx, by) = (f64::from(point[0] - o[0]), f64::from(point[1] - o[1]));
        if ax * by - ay * bx > 0.0 {
            break;
        }
        hull.pop();
    }
    hull.push(point);
}

/// The smallest box round a ring: one side always lies on a hull edge.
fn min_box(ring: &[[f32; 2]], style: Style) -> Option<LevelBox> {
    let points = hull(ring);
    if points.len() < 3 {
        return None;
    }
    let mut smallest = f32::MAX;
    let mut best = None;
    for (index, &a) in points.iter().enumerate() {
        let b = points[(index + 1) % points.len()];
        let (dx, dy) = (b[0] - a[0], b[1] - a[1]);
        let length = dx.hypot(dy);
        let (ux, uy) = (dx / length, dy / length);
        let (mut u0, mut u1, mut v0, mut v1) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
        for p in &points {
            let (px, py) = (p[0] - a[0], p[1] - a[1]);
            let (u, v) = (px * ux + py * uy, py * ux - px * uy);
            u0 = u0.min(u);
            u1 = u1.max(u);
            v0 = v0.min(v);
            v1 = v1.max(v);
        }
        let area = (u1 - u0) * (v1 - v0);
        if area < smallest {
            smallest = area;
            let (cu, cv) = ((u0 + u1) / 2.0, (v0 + v1) / 2.0);
            best = Some(LevelBox {
                x: a[0] + cu * ux - cv * uy,
                y: a[1] + cu * uy + cv * ux,
                half_u: (u1 - u0) / 2.0,
                half_v: (v1 - v0) / 2.0,
                angle: uy.atan2(ux),
                style,
            });
        }
    }
    best
}

/// The x of every crossing of `rings` with the line at `y`, left to right.
fn crossings(rings: &[Ring], y: f32, line: &mut Vec<f32>) {
    line.clear();
    for ring in rings {
        let mut a = ring[ring.len() - 1];
        for &b in ring {
            if (a[1] <= y) != (b[1] <= y) {
                line.push(a[0] + (y - a[1]) / (b[1] - a[1]) * (b[0] - a[0]));
            }
            a = b;
        }
    }
    line.sort_unstable_by(f32::total_cmp);
}

/// One box for a footprint, or several if its box fits it badly.
fn fit_boxes(footprint: &Footprint, split_cell: f32, boxes: &mut Vec<LevelBox>) {
    let Some(whole) = min_box(&footprint.rings[0], footprint.style) else {
        return;
    };
    if footprint.area / (4.0 * whole.half_u * whole.half_v) < SPLIT_BELOW {
        split(footprint, whole, split_cell, boxes);
    } else {
        boxes.push(whole);
    }
}

/// A footprint as a grid in its box's own frame, merged into rectangles.
fn split(footprint: &Footprint, whole: LevelBox, cell: f32, boxes: &mut Vec<LevelBox>) {
    let (width, height) = (2.0 * whole.half_u, 2.0 * whole.half_v);
    let cell = cell.max(width.max(height) / SPLIT_MAX_CELLS);
    let columns = ((width / cell).ceil() as usize).max(1);
    let rows = ((height / cell).ceil() as usize).max(1);
    let (cell_u, cell_v) = (width / columns as f32, height / rows as f32);
    let (sin, cos) = whole.angle.sin_cos();
    let in_frame = |p: &[f32; 2]| {
        let (x, y) = (p[0] - whole.x, p[1] - whole.y);
        [
            x * cos + y * sin + whole.half_u,
            y * cos - x * sin + whole.half_v,
        ]
    };
    let rings: Vec<Ring> = footprint
        .rings
        .iter()
        .map(|ring| ring.iter().map(in_frame).collect())
        .collect();
    let mut covered = vec![false; columns * rows];
    let mut line = Vec::new();
    for row in 0..rows {
        crossings(&rings, (row as f32 + 0.5) * cell_v, &mut line);
        for span in line.as_chunks::<2>().0 {
            // The cells whose centres the span holds.
            let first = |x: f32| ((x / cell_u - 0.5).ceil().max(0.0) as usize).min(columns);
            let (from, to) = (first(span[0]), first(span[1]));
            covered[row * columns + from.min(to)..row * columns + to].fill(true);
        }
    }
    let before = boxes.len();
    for row in 0..rows {
        for column in 0..columns {
            if !covered[row * columns + column] {
                continue;
            }
            let wide = (column..columns)
                .take_while(|&c| covered[row * columns + c])
                .count();
            let tall = (row..rows)
                .take_while(|&r| (column..column + wide).all(|c| covered[r * columns + c]))
                .count();
            for r in row..row + tall {
                covered[r * columns + column..r * columns + column + wide].fill(false);
            }
            let u = (column as f32 + wide as f32 / 2.0) * cell_u - whole.half_u;
            let v = (row as f32 + tall as f32 / 2.0) * cell_v - whole.half_v;
            boxes.push(LevelBox {
                x: whole.x + u * cos - v * sin,
                y: whole.y + u * sin + v * cos,
                half_u: wide as f32 * cell_u / 2.0,
                half_v: tall as f32 * cell_v / 2.0,
                ..whole
            });
        }
    }
    if boxes.len() == before {
        boxes.push(whole);
    }
}

// Grids.

/// Sums over the buildings in a grid cell, each weighted by its cover.
#[derive(Clone, Copy, Default)]
struct Cell {
    cover: f32,
    /// Cover times height, in metres.
    volume: f32,
    /// Cover times colour.
    rgb: [f32; 3],
    /// Cover times position, in cells of this grid.
    at: [f32; 2],
    tallest: f32,
    /// The tallest building: the index of its boxed tile, and of its
    /// footprint in that tile.
    building: (u32, u32),
}

impl Cell {
    /// Adds `other` scaled by `weight`, with `at` already scaled.
    fn add(&mut self, other: &Cell, weight: f32, at: [f32; 2]) {
        self.cover += other.cover * weight;
        self.volume += other.volume * weight;
        self.at[0] += at[0];
        self.at[1] += at[1];
        for (sum, value) in self.rgb.iter_mut().zip(other.rgb) {
            *sum += value * weight;
        }
    }
}

type Grid = Vec<Cell>;

/// Adds a footprint to its z13 grid, exactly in x and on scanlines in y.
/// `origin` is where its z14 tile starts in the grid, in cells.
fn rasterise(
    footprint: &Footprint,
    building: (u32, u32),
    origin: [f32; 2],
    grid: &mut [Cell],
    line: &mut Vec<f32>,
) {
    let cells = |p: &[f32; 2]| {
        [
            p[0] / Z14_UNITS_PER_Z13_CELL + origin[0],
            p[1] / Z14_UNITS_PER_Z13_CELL + origin[1],
        ]
    };
    let rings: Vec<Ring> = footprint
        .rings
        .iter()
        .map(|ring| ring.iter().map(cells).collect())
        .collect();
    let (mut y0, mut y1) = (f32::MAX, f32::MIN);
    for point in rings.iter().flatten() {
        y0 = y0.min(point[1]);
        y1 = y1.max(point[1]);
    }
    let height = footprint.style.top;
    let rgb = footprint.style.rgb.map(f32::from);
    for row in y0.floor() as usize..(y1.ceil() as usize).min(GRID) {
        for scanline in 0..SCANLINES {
            let y = row as f32 + (scanline as f32 + 0.5) / SCANLINES as f32;
            crossings(&rings, y, line);
            for span in line.as_chunks::<2>().0 {
                let (left, right) = (span[0], span[1]);
                for x in left.floor() as usize..(right.ceil() as usize).min(GRID) {
                    let (a, b) = (left.max(x as f32), right.min(x as f32 + 1.0));
                    let weight = (b - a) / SCANLINES as f32;
                    let cell = &mut grid[row * GRID + x];
                    cell.cover += weight;
                    cell.volume += weight * height;
                    cell.at[0] += weight * (a + b) / 2.0;
                    cell.at[1] += weight * y;
                    for (sum, value) in cell.rgb.iter_mut().zip(rgb) {
                        *sum += weight * value;
                    }
                    if height > cell.tallest {
                        cell.tallest = height;
                        cell.building = building;
                    }
                }
            }
        }
    }
}

/// Adds the finished grid of tile `(x, y)` to its quarter of its parent's
/// `grid`, each parent cell the mean of four cells, with overlapping
/// building parts counted once. A quarter touches no other quarter's cells, so
/// children can come in any order, each as soon as it is finished.
fn reduce_into(child: &[Cell], (x, y): (u32, u32), grid: &mut [Cell]) {
    let (ox, oy) = ((x & 1) as usize * GRID / 2, (y & 1) as usize * GRID / 2);
    for (index, cell) in child.iter().enumerate() {
        if cell.cover <= 0.0 {
            continue;
        }
        let (x, y) = (index % GRID, index / GRID);
        let weight = 0.25 / cell.cover.max(1.0);
        let at = [
            weight * (cell.at[0] / 2.0 + ox as f32 * cell.cover),
            weight * (cell.at[1] / 2.0 + oy as f32 * cell.cover),
        ];
        let parent = &mut grid[(oy + y / 2) * GRID + ox + x / 2];
        parent.add(cell, weight, at);
        if cell.tallest > parent.tallest {
            parent.tallest = cell.tallest;
            parent.building = cell.building;
        }
    }
}

/// Hands thin cells on, then gives every cell left a key: equal keys may
/// merge, and zero is no cell.
fn merge_keys(grid: &mut [Cell], height_step: f32) -> Vec<u32> {
    let mut keys = vec![0; grid.len()];
    let mut carried = vec![0u8; grid.len()];
    for index in 0..grid.len() {
        let cell = grid[index];
        if cell.cover <= 0.0 {
            continue;
        }
        let cover = cell.cover.min(1.0);
        let last = index % GRID + 1 == GRID;
        if cover < MIN_COVER && !last && carried[index] < MAX_CARRY {
            grid[index] = Cell::default();
            grid[index + 1].add(&cell, 1.0, cell.at);
            carried[index + 1] = carried[index + 1].max(carried[index] + 1);
            continue;
        }
        let height = (cell.volume / cell.cover / height_step).round() as u32;
        let quarter = ((cover * 4.0) as u32).min(3);
        let [r, g, b] = cell.rgb.map(|sum| (((sum / cell.cover) as u32) >> 5) & 7);
        keys[index] = (height.min(4094) + 1) | (quarter << 12) | (r << 14) | (g << 17) | (b << 20);
    }
    keys
}

/// Merges equal keys greedily into rectangles and adds a box for each, of its
/// size times the root of its cover, at its centroid. Returns their area in
/// cells.
fn merged_boxes(grid: &[Cell], keys: &mut [u32], gain: f32, boxes: &mut Vec<LevelBox>) -> f64 {
    let mut area = 0.0;
    for row in 0..GRID {
        for column in 0..GRID {
            let key = keys[row * GRID + column];
            if key == 0 {
                continue;
            }
            let wide = (column..GRID)
                .take_while(|&c| keys[row * GRID + c] == key)
                .count();
            let tall = (row..GRID)
                .take_while(|&r| (column..column + wide).all(|c| keys[r * GRID + c] == key))
                .count();
            let mut sum = Cell::default();
            let mut covered = 0.0_f32;
            for r in row..row + tall {
                for index in r * GRID + column..r * GRID + column + wide {
                    keys[index] = 0;
                    sum.add(&grid[index], 1.0, grid[index].at);
                    covered += grid[index].cover.min(1.0);
                }
            }
            let cells = (wide * tall) as f32;
            let cover = (covered / cells * gain).min(1.0);
            let side = cover.sqrt() * CELL_UNITS / 2.0;
            area += f64::from(cover * cells);
            boxes.push(LevelBox {
                x: sum.at[0] / sum.cover * CELL_UNITS,
                y: sum.at[1] / sum.cover * CELL_UNITS,
                half_u: wide as f32 * side,
                half_v: tall as f32 * side,
                angle: 0.0,
                style: Style {
                    base: 0.0,
                    top: sum.volume / sum.cover,
                    rgb: sum.rgb.map(|value| (value / sum.cover).round() as u8),
                },
            });
        }
    }
    area
}

/// The boxes of a z14 tile, kept for the landmarks of its parents.
struct BoxedTile {
    x: u32,
    y: u32,
    boxes: Vec<LevelBox>,
    /// Each footprint's boxes.
    spans: Vec<Range<usize>>,
}

/// Adds the boxes of `landmarks` to the tile z/x/y, unmerged.
fn landmark_boxes(
    landmarks: &[(u32, u32)],
    sources: &[Option<BoxedTile>],
    (zoom, x, y): (u8, u32, u32),
    boxes: &mut Vec<LevelBox>,
) {
    let shift = SOURCE_ZOOM - zoom;
    let scale = 1.0 / (1_u32 << shift) as f32;
    for &(tile, footprint) in landmarks {
        let Some(source) = &sources[tile as usize] else {
            continue;
        };
        let ox = (source.x - (x << shift)) as f32 * EXTENT;
        let oy = (source.y - (y << shift)) as f32 * EXTENT;
        for boxed in &source.boxes[source.spans[footprint as usize].clone()] {
            boxes.push(LevelBox {
                x: (ox + boxed.x) * scale,
                y: (oy + boxed.y) * scale,
                half_u: boxed.half_u * scale,
                half_v: boxed.half_v * scale,
                ..*boxed
            });
        }
    }
}

/// A tile's boxes before they are encoded.
struct LevelTile {
    zoom: u8,
    x: u32,
    y: u32,
    boxes: Vec<LevelBox>,
    /// The area the boxes stand for, and their own.
    truth_m2: f64,
    kept_m2: f64,
}

fn far_tile(
    grid: &mut [Cell],
    zoom: u8,
    x: u32,
    y: u32,
    sources: &[Option<BoxedTile>],
) -> LevelTile {
    let cell_m = tile_width_m(zoom, y) / GRID as f32;
    let cell_area_m2 = f64::from(cell_m) * f64::from(cell_m);
    let mut truth = 0.0;
    let mut landmarks = Vec::new();
    for cell in grid.iter().filter(|cell| cell.cover > 0.0) {
        truth += f64::from(cell.cover.min(1.0));
        if cell.tallest >= cell.volume / cell.cover + LANDMARK_CELLS * cell_m {
            landmarks.push(cell.building);
        }
    }
    landmarks.sort_unstable();
    landmarks.dedup();
    let mut keys = merge_keys(grid, (cell_m * 0.5).max(MIN_HEIGHT_STEP_M));
    let gain = if zoom == 11 { Z11_COVER_GAIN } else { 1.0 };
    let mut boxes = Vec::new();
    let kept = merged_boxes(grid, &mut keys, gain, &mut boxes);
    landmark_boxes(&landmarks, sources, (zoom, x, y), &mut boxes);
    LevelTile {
        zoom,
        x,
        y,
        boxes,
        truth_m2: truth * cell_area_m2,
        kept_m2: kept * cell_area_m2,
    }
}

/// The ground width of a tile of zoom `zoom`, at the latitude of row `y`.
fn tile_width_m(zoom: u8, y: u32) -> f32 {
    let tiles = f64::from(1_u32 << zoom);
    let mercator = std::f64::consts::PI * (1.0 - 2.0 * (f64::from(y) + 0.5) / tiles);
    (EQUATOR_M / tiles * mercator.sinh().atan().cos()) as f32
}

// Encoding.

fn position(units: f32) -> u16 {
    let value = (f64::from(units) + 2048.0) * 65535.0 / 8192.0;
    value.round().clamp(0.0, 65535.0) as u16
}

/// In eighths of a unit, so a far box of half a tile still fits.
fn half_size(units: f32) -> u16 {
    (units * 8.0).ceil().clamp(1.0, 65535.0) as u16
}

fn decimetres(metres: f32) -> u16 {
    (metres * 10.0).round().clamp(0.0, 65535.0) as u16
}

fn encode_tile(boxes: &[LevelBox]) -> Vec<u8> {
    let columns: [fn(&LevelBox) -> u16; 7] = [
        |b| position(b.x),
        |b| position(b.y),
        |b| half_size(b.half_u),
        |b| half_size(b.half_v),
        |b| ((b.angle + PI) * 10_000.0).round() as u16,
        |b| decimetres(b.style.base),
        |b| decimetres(b.style.top),
    ];
    let mut raw = Vec::with_capacity(4 + RECORD_BYTES * boxes.len());
    raw.extend_from_slice(&(boxes.len() as u32).to_le_bytes());
    for column in columns {
        raw.extend(boxes.iter().flat_map(|b| column(b).to_le_bytes()));
    }
    for channel in 0..3 {
        raw.extend(boxes.iter().map(|b| b.style.rgb[channel]));
    }
    raw.resize(raw.len() + boxes.len(), 0);
    raw
}

fn gzip(raw: &[u8]) -> Result<Vec<u8>, String> {
    let failed = |e: std::io::Error| format!("Failed to gzip a tile: {e}");
    let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::new(GZIP_LEVEL));
    encoder.write_all(raw).map_err(failed)?;
    encoder.finish().map_err(failed)
}

#[derive(Clone, Copy, Default)]
struct LevelStats {
    tiles: u64,
    boxes: u64,
    raw_bytes: u64,
    gz_bytes: u64,
    truth_m2: f64,
    kept_m2: f64,
}

impl LevelStats {
    fn merge(&mut self, other: &LevelStats) {
        self.tiles += other.tiles;
        self.boxes += other.boxes;
        self.raw_bytes += other.raw_bytes;
        self.gz_bytes += other.gz_bytes;
        self.truth_m2 += other.truth_m2;
        self.kept_m2 += other.kept_m2;
    }

    fn report(&self, zoom: u8) -> BuildingLevelReport {
        BuildingLevelReport {
            zoom,
            tiles: self.tiles,
            boxes: self.boxes,
            raw_bytes: self.raw_bytes,
            gz_bytes: self.gz_bytes,
            area_kept: if self.truth_m2 > 0.0 {
                self.kept_m2 / self.truth_m2
            } else {
                1.0
            },
        }
    }
}

/// The finished tiles of one z11 tile, as (tile id, gzipped bytes).
#[derive(Default)]
struct GroupLevels {
    tiles: Vec<(u64, Vec<u8>)>,
    stats: [LevelStats; LEVEL_ZOOMS.len()],
    /// Each tile's level, x, y and areas, summed into `stats` in tile order
    /// by [`GroupLevels::finish`], so the sums do not depend on the order the
    /// tiles were made in.
    areas: Vec<(usize, u32, u32, f64, f64)>,
}

impl GroupLevels {
    fn add(&mut self, tile: &LevelTile) -> Result<(), String> {
        let level = LEVEL_ZOOMS.iter().position(|&zoom| zoom == tile.zoom);
        let level = level.ok_or("A zoom the pass does not write")?;
        self.areas
            .push((level, tile.x, tile.y, tile.truth_m2, tile.kept_m2));
        if tile.boxes.is_empty() {
            return Ok(());
        }
        let raw = encode_tile(&tile.boxes);
        let mut gz = gzip(&raw)?;
        // Held until the archive is written: the encoder's spare room adds up.
        gz.shrink_to_fit();
        let stats = &mut self.stats[level];
        stats.tiles += 1;
        stats.boxes += tile.boxes.len() as u64;
        stats.raw_bytes += raw.len() as u64;
        stats.gz_bytes += gz.len() as u64;
        let (zoom, x, y) = (tile.zoom, tile.x, tile.y);
        let coord =
            TileCoord::new(zoom, x, y).map_err(|e| format!("Invalid tile {zoom}/{x}/{y}: {e}"))?;
        self.tiles.push((TileId::from(coord).value(), gz));
        Ok(())
    }

    fn finish(mut self) -> Self {
        self.areas
            .sort_unstable_by_key(|&(level, x, y, ..)| (level, x, y));
        for &(level, _, _, truth_m2, kept_m2) in &self.areas {
            self.stats[level].truth_m2 += truth_m2;
            self.stats[level].kept_m2 += kept_m2;
        }
        self.areas = Vec::new();
        self
    }
}

/// The grids of one z11 tile still being filled: one each at z13, z12 and
/// z11, as the z14 tiles come a quadrant at a time. Finished grids are kept
/// to be filled again: a grid is megabytes, and an allocator that is handed
/// one back often holds on to it for a while.
#[derive(Default)]
struct OpenGrids {
    /// z13, z12 and z11, each with its tile.
    open: [Option<((u32, u32), Grid)>; 3],
    spare: Vec<Grid>,
    /// A tile as stored, and unpacked.
    stored: Vec<u8>,
    raw: Vec<u8>,
}

impl OpenGrids {
    /// The grid at `level` (0 for z13), which must be the tile `tile`'s if
    /// one is open; empty if it is new.
    fn grid(&mut self, level: usize, tile: (u32, u32)) -> &mut Grid {
        let spare = &mut self.spare;
        let (_, grid) = self.open[level].get_or_insert_with(|| {
            let grid = match spare.pop() {
                Some(mut grid) => {
                    grid.fill(Cell::default());
                    grid
                }
                None => vec![Cell::default(); GRID * GRID],
            };
            (tile, grid)
        });
        grid
    }

    /// Finishes every grid that does not hold the z14 tile `next`, or every
    /// grid if there is none: each goes into its parent, and z12 and z11 into
    /// tiles. The z14 tiles come a quadrant at a time, so a grid the next
    /// tile is not in is complete.
    fn finish_unless(
        &mut self,
        next: Option<(u32, u32)>,
        sources: &[Option<BoxedTile>],
        group: &mut GroupLevels,
    ) -> Result<(), String> {
        for level in 0..self.open.len() {
            let shift = level + 1;
            let holds_next =
                |tile: (u32, u32)| next.is_some_and(|(x, y)| (x >> shift, y >> shift) == tile);
            let finished = self.open[level]
                .as_ref()
                .is_some_and(|(tile, _)| !holds_next(*tile));
            if !finished {
                continue;
            }
            let Some(((x, y), mut grid)) = self.open[level].take() else {
                continue;
            };
            // A grid goes into its parent before its tile, which changes it.
            if level + 1 < self.open.len() {
                reduce_into(&grid, (x, y), self.grid(level + 1, (x >> 1, y >> 1)));
            }
            if level > 0 {
                let zoom = SOURCE_ZOOM - 1 - level as u8;
                group.add(&far_tile(&mut grid, zoom, x, y, sources))?;
            }
            self.spare.push(grid);
        }
        Ok(())
    }
}

/// Where a z14 tile comes in its z11 tile: quadrant by quadrant, so that only
/// one grid a zoom is open at a time.
fn quadrant_order(tile: &SourceTile) -> u32 {
    (0..3).fold(0, |order, bit| {
        order | ((tile.x >> bit) & 1) << (2 * bit) | ((tile.y >> bit) & 1) << (2 * bit + 1)
    })
}

/// Everything under one z11 tile, from its z14 tiles in tile order.
fn group_levels(
    archive: &Archive,
    compression: u8,
    mlt: bool,
    z14: &[SourceTile],
    grids: &mut OpenGrids,
) -> Result<GroupLevels, String> {
    let mut group = GroupLevels::default();
    // By the tile's place in `z14`, which is what orders the landmarks.
    let mut sources: Vec<Option<BoxedTile>> = Vec::new();
    sources.resize_with(z14.len(), || None);
    let mut order: Vec<usize> = (0..z14.len()).collect();
    order.sort_unstable_by_key(|&index| quadrant_order(&z14[index]));
    let mut line = Vec::new();
    for &source in &order {
        let tile = z14[source];
        grids.finish_unless(Some((tile.x, tile.y)), &sources, &mut group)?;
        archive.section_into(tile.offset, tile.length, &mut grids.stored)?;
        let mut bytes = unpack_into(&grids.stored, compression, &mut grids.raw)?;
        let transcoded;
        if mlt {
            transcoded = crate::mlt::to_mvt(bytes)
                .map_err(|e| format!("Malformed tile 14/{}/{}: {e}", tile.x, tile.y))?;
            bytes = &transcoded;
        }
        let footprints = parse_footprints(bytes)
            .ok_or_else(|| format!("Malformed tile 14/{}/{}", tile.x, tile.y))?;
        if footprints.is_empty() {
            continue;
        }
        let unit_m = tile_width_m(SOURCE_ZOOM, tile.y) / EXTENT;
        let mut boxes = Vec::with_capacity(footprints.len());
        let mut spans = Vec::with_capacity(footprints.len());
        for footprint in &footprints {
            let first = boxes.len();
            fit_boxes(footprint, SPLIT_M / unit_m, &mut boxes);
            spans.push(first..boxes.len());
        }
        let grid = grids.grid(0, (tile.x >> 1, tile.y >> 1));
        let half = (GRID / 2) as f32;
        let origin = [(tile.x & 1) as f32 * half, (tile.y & 1) as f32 * half];
        for (index, footprint) in footprints.iter().enumerate() {
            let building = (source as u32, index as u32);
            rasterise(footprint, building, origin, grid, &mut line);
        }
        let unit_area_m2 = f64::from(unit_m) * f64::from(unit_m);
        let truth: f64 = footprints.iter().map(|f| f64::from(f.area)).sum();
        let kept: f64 = boxes
            .iter()
            .map(|b| 4.0 * f64::from(b.half_u) * f64::from(b.half_v))
            .sum();
        let level = LevelTile {
            zoom: SOURCE_ZOOM,
            x: tile.x,
            y: tile.y,
            boxes,
            truth_m2: truth * unit_area_m2,
            kept_m2: kept * unit_area_m2,
        };
        group.add(&level)?;
        sources[source] = Some(BoxedTile {
            x: tile.x,
            y: tile.y,
            boxes: level.boxes,
            spans,
        });
    }
    grids.finish_unless(None, &sources, &mut group)?;
    Ok(group.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metadata_names_the_format_and_version() {
        let metadata: serde_json::Value = serde_json::from_str(METADATA).unwrap();
        assert_eq!(metadata["format"], BUILDING_LEVELS_FORMAT);
        assert_eq!(metadata["version"], BUILDING_LEVELS_VERSION);
        assert_eq!(metadata["source_layer"], SOURCE_LAYER);
    }
}
