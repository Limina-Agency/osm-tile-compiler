//! Builds the search index from the highest zoom of a compiled archive.
//!
//! Points (places, POIs, peaks, SAR points, lake labels) are taken where
//! they lie in their tile, once per OSM object: a label that sits in the
//! buffers of the tiles around it is read once. Named lines (streets,
//! rivers, ridges, trails) are cut into pieces at tile edges; the pieces of
//! one name whose ends meet are joined back into one record, placed at a
//! vertex near their middle. Every record but a city, town or village gets
//! the nearest settlement as its context.
use super::fold::{fold, words};
use super::{
    ATTR_BYTES, HAS_ELE, HAS_ID, HAS_REF, HEADER_BYTES, Kind, MAGIC, NO_CONTEXT, POINT_BYTES,
    RECORDS_PER_BLOCK, SEARCH_INDEX_FORMAT, SEARCH_INDEX_VERSION, SECTIONS, TOKENS_PER_BLOCK,
    distance_m, put_varint, search_index_path, zigzag,
};
use crate::archive::{ArchiveReader, Entry};
use crate::control::{CompileError, Control};
use crate::output::{OutputFormat, PartialArchive};
use fast_mvt::{MvtFeatureRef, MvtReaderRef, MvtValueRef};
use pmtiles::{TileCoord, TileId};
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::collections::{BTreeMap, HashMap, hash_map};
use std::io::Write;
use std::path::Path;
use std::time::Instant;

/// The layers read, by priority: when one OSM object is in two of them (a
/// hut in `poi` and `sar`), the first one's record is kept.
const LAYERS: [&str; 9] = [
    "place",
    "aerodrome_label",
    "mountain_peak",
    "sar",
    "poi",
    "park",
    "water_name",
    "waterway",
    "transportation_name",
];
/// Line ends of one name closer than this are joined.
const JOIN_M: f64 = 300.0;
/// How far a settlement reaches as context: a city, a town, a village.
const CITY_REACH_M: f64 = 15_000.0;
const TOWN_REACH_M: f64 = 6_000.0;
const VILLAGE_REACH_M: f64 = 3_000.0;
const TILES_PER_TASK: usize = 32;

/// What [`write_search_index`] wrote.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[non_exhaustive]
pub struct SearchIndexReport {
    /// Records in the index: one per place, POI, peak or joined line.
    pub records: u64,
    /// Distinct folded words.
    pub words: u64,
    /// Records per archive layer.
    pub layers: BTreeMap<String, u64>,
    /// The archive zoom read.
    pub zoom: u8,
    /// Tiles read at that zoom.
    pub tiles: u64,
    /// Size of the index file.
    pub bytes: u64,
    /// Wall time of the whole pass.
    pub seconds: f64,
}

/// Reads the highest zoom of the OpenMapTiles archive `input` and writes
/// its search index to `output`, which must not exist yet. The bytes are
/// the same for any number of `threads`.
pub fn write_search_index(
    input: &Path,
    output: &Path,
    threads: usize,
) -> Result<SearchIndexReport, CompileError> {
    write_search_index_cancellable(input, output, threads, || false)
}

/// [`write_search_index`], stopping when `should_cancel` returns true, which
/// it is asked every 50 ms or so. Whatever the outcome, no partial index is
/// left at `output`.
pub fn write_search_index_cancellable(
    input: &Path,
    output: &Path,
    threads: usize,
    should_cancel: impl Fn() -> bool,
) -> Result<SearchIndexReport, CompileError> {
    if output.exists() {
        return Err(CompileError::InvalidRequest(format!(
            "refusing to replace existing search index {}",
            output.display()
        )));
    }
    crate::control::run(
        threads,
        false,
        |_| {},
        should_cancel,
        |control| write_index(input, output, threads, control),
    )
}

/// Writes the index at [`search_index_path`] beside `archive`.
pub fn write_search_index_beside(
    archive: &Path,
    threads: usize,
) -> Result<SearchIndexReport, CompileError> {
    write_search_index(archive, &search_index_path(archive), threads)
}

/// A named point read from a tile.
#[derive(Debug, Clone)]
struct Point {
    layer: u8,
    class: String,
    subclass: String,
    /// `name` first, then the other names that fold differently.
    names: Vec<String>,
    ele: Option<i64>,
    reference: Option<String>,
    id: Option<u64>,
    /// `capital` in the place layer: 2 for a country's, 3 to 6 for a
    /// region's.
    capital: Option<i64>,
    lon: f64,
    lat: f64,
}

/// A piece of a named line in one tile.
#[derive(Debug, Clone)]
struct Piece {
    layer: u8,
    class: String,
    subclass: String,
    names: Vec<String>,
    /// The first and last vertex of each part.
    ends: Vec<[f64; 2]>,
    /// The middle vertex of the longest part.
    middle: [f64; 2],
}

#[derive(Default)]
struct TileItems {
    points: Vec<Point>,
    pieces: Vec<Piece>,
}

/// A record before it is written.
struct Record {
    kind: Kind,
    /// How much more prominent than its tier: a capital, a high peak, a
    /// long street.
    boost: f64,
    /// Its tier × 10 less its boosts; lower is more prominent.
    rank: u8,
    names: Vec<String>,
    folded: String,
    ele: Option<i64>,
    reference: Option<String>,
    id: Option<u64>,
    lon: f64,
    lat: f64,
}

/// [`write_search_index`], with progress and cancelling.
pub(crate) fn write_index(
    input: &Path,
    output: &Path,
    threads: usize,
    control: &Control,
) -> Result<SearchIndexReport, String> {
    let start = Instant::now();
    if output.exists() {
        return Err(format!(
            "refusing to replace existing search index {}",
            output.display()
        ));
    }
    let mut reader = ArchiveReader::open(input)?;
    // MVT, or MLT read as the MVT tiles it was transcoded from.
    let mlt = match reader.header.tile_type {
        1 => false,
        6 => true,
        _ => return Err(format!("{} holds no vector tiles", input.display())),
    };
    let zoom = reader.header.max_zoom;
    let compression = reader.header.tile_compression;
    let metadata: serde_json::Value = serde_json::from_str(&reader.metadata()?)
        .map_err(|e| format!("Failed to parse the metadata of {}: {e}", input.display()))?;
    let mut tiles: Vec<(Entry, u32, u32)> = Vec::new();
    reader.visit_entries(|_, entry| {
        for id in entry.tile_id..entry.tile_id + u64::from(entry.run_length) {
            let coord = TileCoord::from(TileId::new(id).map_err(|e| e.to_string())?);
            if coord.z() == zoom {
                tiles.push((entry, coord.x(), coord.y()));
            }
        }
        Ok(())
    })?;
    drop(reader);
    let tile_count = tiles.len() as u64;
    control.set_total(tile_count);
    let read = || {
        tiles
            .par_chunks(TILES_PER_TASK)
            .map_init(
                || ArchiveReader::open(input),
                |reader, chunk| {
                    control.check()?;
                    let reader = reader.as_mut().map_err(|e| e.clone())?;
                    let mut items = TileItems::default();
                    for (entry, x, y) in chunk {
                        let stored = reader.tile(entry)?;
                        let mut tile = crate::archive::decompress_tile(&stored, compression)?;
                        if mlt {
                            tile = crate::mlt::to_mvt(&tile).map_err(|e| {
                                format!(
                                    "tile {zoom}/{x}/{y} of {} is malformed: {e}",
                                    input.display()
                                )
                            })?;
                        }
                        read_tile(&tile, zoom, *x, *y, &mut items).ok_or_else(|| {
                            format!("tile {zoom}/{x}/{y} of {} is malformed", input.display())
                        })?;
                    }
                    control.add_done(chunk.len() as u64);
                    Ok(items)
                },
            )
            .collect::<Result<Vec<_>, String>>()
    };
    let parts =
        if rayon::current_thread_index().is_some() && rayon::current_num_threads() == threads {
            read()?
        } else {
            rayon::ThreadPoolBuilder::new()
                .num_threads(threads)
                .build()
                .map_err(|e| format!("Failed to start {threads} threads: {e}"))?
                .install(read)?
        };
    control.check()?;

    let mut records = points_to_records(parts.iter().flat_map(|p| &p.points));
    records.extend(join_lines(parts.iter().flat_map(|p| &p.pieces)));
    drop(parts);
    if records.len() >= NO_CONTEXT as usize {
        return Err("too many names for one search index".into());
    }
    let contexts = contexts(&records);
    rank(&mut records, &contexts);
    // Record ids go by rank, so a word's records come most prominent first.
    let mut order: Vec<usize> = (0..records.len()).collect();
    order.sort_by(|&a, &b| {
        let (a, b) = (&records[a], &records[b]);
        (a.rank, &a.folded, &a.names[0], &a.kind)
            .cmp(&(b.rank, &b.folded, &b.names[0], &b.kind))
            .then(a.lon.total_cmp(&b.lon))
            .then(a.lat.total_cmp(&b.lat))
            .then(a.id.cmp(&b.id))
    });
    let mut new_id = vec![0u32; records.len()];
    for (new, &old) in order.iter().enumerate() {
        new_id[old] = new as u32;
    }
    let contexts: Vec<u32> = order
        .iter()
        .map(|&old| match contexts[old] {
            NO_CONTEXT => NO_CONTEXT,
            context => new_id[context as usize],
        })
        .collect();
    let mut slots: Vec<Option<Record>> = records.into_iter().map(Some).collect();
    let records: Vec<Record> = order
        .iter()
        .map(|&old| slots[old].take().expect("each record once"))
        .collect();
    control.check()?;

    let mut layers = BTreeMap::new();
    for record in &records {
        *layers.entry(record.kind.layer.clone()).or_insert(0) += 1;
    }
    let source = input
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let meta = serde_json::json!({
        "format": SEARCH_INDEX_FORMAT,
        "version": SEARCH_INDEX_VERSION,
        "records": records.len(),
        "source": source,
        "zoom": zoom,
        "bounds": metadata.get("bounds").cloned().unwrap_or(serde_json::Value::Null),
        "osm_data_timestamp": metadata
            .get("osm_data_timestamp")
            .cloned()
            .unwrap_or(serde_json::Value::Null),
    });
    let (bytes, words) = encode(&records, &contexts, &meta)?;
    let partial = PartialArchive::create(output, OutputFormat::PmTiles)?;
    partial
        .file()
        .write_all(&bytes)
        .map_err(|e| format!("Failed to write {}: {e}", output.display()))?;
    control.check()?;
    partial.persist(output)?;
    Ok(SearchIndexReport {
        records: records.len() as u64,
        words,
        layers,
        zoom,
        tiles: tile_count,
        bytes: bytes.len() as u64,
        seconds: start.elapsed().as_secs_f64(),
    })
}

/// Reads the named features of one tile, or None if it is malformed.
fn read_tile(tile: &[u8], z: u8, x: u32, y: u32, items: &mut TileItems) -> Option<()> {
    let reader = MvtReaderRef::new(tile).ok()?;
    for layer in reader.layers() {
        let Some(layer_index) = LAYERS.iter().position(|name| *name == layer.name()) else {
            continue;
        };
        let extent = layer.extent();
        if extent == 0 {
            return None;
        }
        let project = |px: i32, py: i32| tile_point(z, x, y, extent, px, py);
        for feature in layer.features() {
            read_feature(feature, layer_index as u8, extent, &project, items)?;
        }
    }
    Some(())
}

fn read_feature(
    feature: MvtFeatureRef<'_>,
    layer: u8,
    extent: u32,
    project: &impl Fn(i32, i32) -> [f64; 2],
    items: &mut TileItems,
) -> Option<()> {
    let layer_name = LAYERS[usize::from(layer)];
    let mut name = None;
    let mut others: Vec<&str> = Vec::new();
    let mut class = "";
    let mut subclass = "";
    let mut ele = None;
    let mut capital = None;
    let mut reference = None;
    for property in feature.properties() {
        let (key, value) = property.ok()?;
        match (key, value) {
            ("name", MvtValueRef::String(text)) => name = Some(text),
            ("name_en" | "name_de" | "name_int" | "name:latin", MvtValueRef::String(text)) => {
                others.push(text)
            }
            ("class", MvtValueRef::String(text)) => class = text,
            ("subclass", MvtValueRef::String(text)) => subclass = text,
            ("ele", value) => ele = integer(value),
            ("capital", value) => capital = integer(value),
            // An emergency access point's number, a helipad's code.
            ("ref", MvtValueRef::String(text)) if layer_name == "sar" => reference = Some(text),
            ("iata" | "icao", MvtValueRef::String(text)) if reference.is_none() => {
                reference = Some(text)
            }
            _ => {}
        }
    }
    let name = name.filter(|n| !fold(n).is_empty());
    let names = match (name, reference) {
        (Some(name), _) => {
            let mut names = vec![name.to_string()];
            let mut seen = vec![fold(name)];
            for other in others {
                let folded = fold(other);
                if !folded.is_empty() && !seen.contains(&folded) {
                    seen.push(folded);
                    names.push(other.to_string());
                }
            }
            names
        }
        // An access point with a number but no name is found by its number.
        (None, Some(reference)) if layer_name == "sar" => vec![reference.to_string()],
        _ => return Some(()),
    };
    let reference = reference.map(str::to_string);
    let geometry = parts(feature.geometry_commands())?;
    match feature.geom_type_value() {
        Some(1) => {
            let &[px, py] = geometry.first()?.first()?;
            let id = feature.id().filter(|&id| id != 0);
            // Without an id, the copies in the buffers of neighbouring
            // tiles cannot be told apart: take the one inside its tile.
            let inside = (0..extent as i32).contains(&px) && (0..extent as i32).contains(&py);
            if id.is_none() && !inside {
                return Some(());
            }
            let [lon, lat] = project(px, py);
            items.points.push(Point {
                layer,
                class: class.to_string(),
                subclass: subclass.to_string(),
                names,
                ele,
                reference,
                id,
                capital,
                lon,
                lat,
            });
        }
        Some(2) if line_group(layer_name).is_some() => {
            let longest = geometry.iter().max_by_key(|part| part.len())?;
            let [mx, my] = longest[longest.len() / 2];
            let ends = geometry
                .iter()
                .flat_map(|part| [part[0], part[part.len() - 1]])
                .map(|[px, py]| project(px, py))
                .collect();
            items.pieces.push(Piece {
                layer,
                class: class.to_string(),
                subclass: subclass.to_string(),
                names,
                ends,
                middle: project(mx, my),
            });
        }
        _ => {}
    }
    Some(())
}

fn integer(value: MvtValueRef<'_>) -> Option<i64> {
    match value {
        MvtValueRef::Int(v) | MvtValueRef::SInt(v) => Some(v),
        MvtValueRef::UInt(v) => i64::try_from(v).ok(),
        MvtValueRef::Float(v) if v.is_finite() => Some(v.round() as i64),
        MvtValueRef::Double(v) if v.is_finite() => Some(v.round() as i64),
        _ => None,
    }
}

/// The parts of a geometry, in tile units: a point's points, a line's
/// lines. Rings are closed by command, which a line does not have.
fn parts(mut commands: impl Iterator<Item = u32>) -> Option<Vec<Vec<[i32; 2]>>> {
    let unzigzag = |value: u32| (value >> 1) as i32 ^ -((value & 1) as i32);
    let (mut x, mut y) = (0_i32, 0_i32);
    let mut parts: Vec<Vec<[i32; 2]>> = Vec::new();
    while let Some(command) = commands.next() {
        let count = command >> 3;
        match command & 7 {
            1 => {
                for _ in 0..count {
                    x = x.wrapping_add(unzigzag(commands.next()?));
                    y = y.wrapping_add(unzigzag(commands.next()?));
                    parts.push(vec![[x, y]]);
                }
            }
            2 => {
                let part = parts.last_mut()?;
                for _ in 0..count {
                    x = x.wrapping_add(unzigzag(commands.next()?));
                    y = y.wrapping_add(unzigzag(commands.next()?));
                    part.push([x, y]);
                }
            }
            7 => {}
            _ => return None,
        }
    }
    Some(parts)
}

/// Longitude and latitude of a point in tile units.
fn tile_point(z: u8, x: u32, y: u32, extent: u32, px: i32, py: i32) -> [f64; 2] {
    let n = 2f64.powi(i32::from(z));
    let fx = (f64::from(x) + f64::from(px) / f64::from(extent)) / n;
    let fy = (f64::from(y) + f64::from(py) / f64::from(extent)) / n;
    let lon = fx * 360.0 - 180.0;
    let lat = (std::f64::consts::PI * (1.0 - 2.0 * fy))
        .sinh()
        .atan()
        .to_degrees();
    [
        round7(lon.clamp(-180.0, 180.0)),
        round7(lat.clamp(-85.06, 85.06)),
    ]
}

/// Rounds to the precision the index stores, 1e-7 degrees.
fn round7(degrees: f64) -> f64 {
    (degrees * 1e7).round() / 1e7
}

/// Which lines of a layer are joined into records, and with which others:
/// a river and a lake's long label share a name space, streets their own.
fn line_group(layer: &str) -> Option<u8> {
    match layer {
        "transportation_name" => Some(0),
        "waterway" | "water_name" => Some(1),
        "mountain_peak" => Some(2),
        "sar" => Some(3),
        _ => None,
    }
}

/// The tier of a record: 0 for the most prominent to 9.
fn tier(layer: &str, class: &str, subclass: &str) -> u8 {
    match (layer, class) {
        ("place", "continent" | "country") => 0,
        ("place", "state" | "province" | "city") => 1,
        ("place", "town") => 2,
        ("place", "village" | "suburb") => 3,
        ("place", "island" | "quarter") => 4,
        ("place", "hamlet" | "neighbourhood") => 5,
        ("place", _) => 6,
        ("aerodrome_label", _) => 3,
        ("mountain_peak", "peak" | "volcano") => 4,
        ("mountain_peak", "saddle") => 5,
        ("mountain_peak", _) => 6,
        ("park", "national_park") => 3,
        ("park", "nature_reserve" | "protected_area") => 4,
        ("park", _) => 5,
        ("sar", "shelter") => 4,
        ("sar", "emergency") if subclass == "mountain_rescue" => 4,
        ("sar", "hazard") => 6,
        ("sar", _) => 5,
        (
            "poi",
            "railway" | "hospital" | "college" | "town_hall" | "stadium" | "museum" | "castle"
            | "attraction" | "aerialway" | "alpine_hut" | "shelter",
        ) => 4,
        ("poi", _) => 5,
        ("water_name", "ocean" | "sea") => 1,
        ("water_name", "lake" | "bay" | "strait") => 4,
        ("water_name", _) => 5,
        ("waterway", "river") => 4,
        ("waterway", "canal") => 5,
        ("waterway", _) => 6,
        ("transportation_name", "motorway" | "trunk" | "primary" | "secondary") => 4,
        ("transportation_name", "tertiary" | "minor") => 5,
        // Old-town streets closed to traffic.
        ("transportation_name", "path") if subclass == "pedestrian" || subclass == "footway" => 5,
        ("transportation_name", _) => 6,
        _ => 7,
    }
}

fn kind(layer: u8, class: &str, subclass: &str) -> Kind {
    let layer = LAYERS[usize::from(layer)];
    Kind {
        tier: tier(layer, class, subclass),
        layer: layer.to_string(),
        class: class.to_string(),
        subclass: subclass.to_string(),
    }
}

/// One record per OSM object, the first layer's (in [`LAYERS`] order); a
/// point without an id is one record per name and place.
fn points_to_records<'a>(points: impl Iterator<Item = &'a Point>) -> Vec<Record> {
    let mut by_id: FxHashMap<u64, &Point> = FxHashMap::default();
    let mut without_id: FxHashMap<(u8, &str, i64, i64), &Point> = FxHashMap::default();
    let mut order: Vec<&Point> = Vec::new();
    for point in points {
        match point.id {
            Some(id) => match by_id.entry(id) {
                hash_map::Entry::Vacant(slot) => {
                    slot.insert(point);
                    order.push(point);
                }
                hash_map::Entry::Occupied(mut slot) => {
                    if point.layer < slot.get().layer {
                        slot.insert(point);
                    }
                }
            },
            None => {
                let key = (
                    point.layer,
                    point.names[0].as_str(),
                    (point.lon * 1e7) as i64,
                    (point.lat * 1e7) as i64,
                );
                if let hash_map::Entry::Vacant(slot) = without_id.entry(key) {
                    slot.insert(point);
                    order.push(point);
                }
            }
        }
    }
    order
        .into_iter()
        .filter_map(|first| match first.id {
            // The id's winner: the same point unless a later layer won.
            Some(id) => by_id.remove(&id),
            None => Some(first),
        })
        .map(|point| Record {
            kind: kind(point.layer, &point.class, &point.subclass),
            boost: match (point.capital, point.ele) {
                (Some(2), _) => 6.0,
                (Some(_), _) => 3.0,
                (None, Some(ele)) if LAYERS[usize::from(point.layer)] == "mountain_peak" => {
                    (ele as f64 / 400.0).clamp(0.0, 5.0)
                }
                _ => 0.0,
            },
            rank: 0,
            folded: fold(&point.names[0]),
            names: point.names.clone(),
            ele: point.ele,
            reference: point.reference.clone(),
            id: point.id,
            lon: point.lon,
            lat: point.lat,
        })
        .collect()
}

/// A piece's index and one of its ends.
type PieceEnd = (usize, [f64; 2]);

/// Joins the pieces of each name whose ends meet into one record each.
fn join_lines<'a>(pieces: impl Iterator<Item = &'a Piece>) -> Vec<Record> {
    let mut by_name: BTreeMap<(u8, &str), Vec<&Piece>> = BTreeMap::new();
    for piece in pieces {
        let group = line_group(LAYERS[usize::from(piece.layer)]).unwrap_or(u8::MAX);
        by_name
            .entry((group, piece.names[0].as_str()))
            .or_default()
            .push(piece);
    }
    let mut records = Vec::new();
    for pieces in by_name.into_values() {
        let mut sets = UnionFind::new(pieces.len());
        let mut cells: HashMap<(i64, i64), Vec<PieceEnd>> = HashMap::new();
        for (index, piece) in pieces.iter().enumerate() {
            for &end in &piece.ends {
                let (cx, cy) = cell(end, JOIN_M);
                for dx in -1..=1 {
                    for dy in -1..=1 {
                        for &(other, at) in cells.get(&(cx + dx, cy + dy)).into_iter().flatten() {
                            if distance_m(end, at) <= JOIN_M {
                                sets.union(index, other);
                            }
                        }
                    }
                }
                cells.entry((cx, cy)).or_default().push((index, end));
            }
        }
        let mut clusters: BTreeMap<usize, Vec<&Piece>> = BTreeMap::new();
        for (index, piece) in pieces.iter().enumerate() {
            clusters.entry(sets.find(index)).or_default().push(piece);
        }
        for cluster in clusters.into_values() {
            records.push(line_record(&cluster));
        }
    }
    records
}

/// A joined line: its most prominent piece's kind and names, placed at the
/// piece middle nearest the mean of all of them.
fn line_record(cluster: &[&Piece]) -> Record {
    let best = cluster
        .iter()
        .min_by_key(|piece| kind(piece.layer, &piece.class, &piece.subclass))
        .expect("a cluster has a piece");
    let n = cluster.len() as f64;
    let mean = [
        cluster.iter().map(|p| p.middle[0]).sum::<f64>() / n,
        cluster.iter().map(|p| p.middle[1]).sum::<f64>() / n,
    ];
    let middle = cluster
        .iter()
        .map(|piece| piece.middle)
        .min_by(|a, b| distance_m(*a, mean).total_cmp(&distance_m(*b, mean)))
        .expect("a cluster has a piece");
    Record {
        kind: kind(best.layer, &best.class, &best.subclass),
        // Longer lines cross more tiles.
        boost: ((cluster.len() as f64).log2() / 2.0).min(3.0),
        rank: 0,
        folded: fold(&best.names[0]),
        names: best.names.clone(),
        ele: None,
        reference: None,
        id: None,
        lon: middle[0],
        lat: middle[1],
    }
}

/// A grid cell about `size_m` across around `point`.
fn cell(point: [f64; 2], size_m: f64) -> (i64, i64) {
    let metres_per_degree = 111_320.0;
    let x = point[0] * metres_per_degree * point[1].to_radians().cos().max(0.01);
    let y = point[1] * metres_per_degree;
    ((x / size_m).floor() as i64, (y / size_m).floor() as i64)
}

struct UnionFind(Vec<usize>);

impl UnionFind {
    fn new(n: usize) -> Self {
        Self((0..n).collect())
    }

    fn find(&mut self, mut i: usize) -> usize {
        while self.0[i] != i {
            self.0[i] = self.0[self.0[i]];
            i = self.0[i];
        }
        i
    }

    fn union(&mut self, a: usize, b: usize) {
        let (a, b) = (self.find(a), self.find(b));
        // The smaller index is the root, so clusters do not depend on order.
        if a != b {
            self.0[a.max(b)] = a.min(b);
        }
    }
}

/// Sets each record's rank from its tier, its boost and its context: a
/// record in a city is more prominent than one in a village.
fn rank(records: &mut [Record], contexts: &[u32]) {
    let context_boost: Vec<f64> = contexts
        .iter()
        .map(|&context| match context {
            NO_CONTEXT => 0.0,
            context => match records[context as usize].kind.class.as_str() {
                "city" => 3.0,
                "town" => 1.5,
                _ => 0.0,
            },
        })
        .collect();
    for (record, extra) in records.iter_mut().zip(context_boost) {
        let rank = 10.0 * f64::from(record.kind.tier) - (record.boost + extra).round();
        record.rank = rank.clamp(0.0, 99.0) as u8;
    }
}

/// The settlement each record lies in or near: the city, town or village
/// with the least distance over its reach. Cities, towns and villages have
/// none.
fn contexts(records: &[Record]) -> Vec<u32> {
    let reach = |record: &Record| match (record.kind.layer.as_str(), record.kind.class.as_str()) {
        ("place", "city") => Some(CITY_REACH_M),
        ("place", "town") => Some(TOWN_REACH_M),
        ("place", "village") => Some(VILLAGE_REACH_M),
        _ => None,
    };
    const CELL_M: f64 = CITY_REACH_M;
    let mut grid: HashMap<(i64, i64), Vec<(u32, f64)>> = HashMap::new();
    for (index, record) in records.iter().enumerate() {
        if let Some(reach) = reach(record) {
            grid.entry(cell([record.lon, record.lat], CELL_M))
                .or_default()
                .push((index as u32, reach));
        }
    }
    records
        .iter()
        .map(|record| {
            if reach(record).is_some() {
                return NO_CONTEXT;
            }
            let at = [record.lon, record.lat];
            let (cx, cy) = cell(at, CELL_M);
            let mut best = (f64::INFINITY, NO_CONTEXT);
            for dx in -1..=1 {
                for dy in -1..=1 {
                    for &(index, reach) in grid.get(&(cx + dx, cy + dy)).into_iter().flatten() {
                        let place = &records[index as usize];
                        let d = distance_m(at, [place.lon, place.lat]);
                        if d > reach {
                            continue;
                        }
                        let score = d / reach;
                        if (score, index) < best {
                            best = (score, index);
                        }
                    }
                }
            }
            best.1
        })
        .collect()
}

/// The index file's bytes, and the number of words in its dictionary.
fn encode(
    records: &[Record],
    contexts: &[u32],
    meta: &serde_json::Value,
) -> Result<(Vec<u8>, u64), String> {
    let mut kinds: Vec<Kind> = records.iter().map(|r| r.kind.clone()).collect();
    kinds.sort();
    kinds.dedup();
    if kinds.len() > usize::from(u16::MAX) {
        return Err("too many kinds of feature for one search index".into());
    }
    let kind_ids: HashMap<&Kind, u16> = kinds
        .iter()
        .enumerate()
        .map(|(i, k)| (k, i as u16))
        .collect();
    let kinds_json = serde_json::Value::Array(
        kinds
            .iter()
            .map(|k| serde_json::json!([k.layer, k.class, k.subclass, k.tier]))
            .collect(),
    );

    let mut points = Vec::with_capacity(records.len() * POINT_BYTES);
    let mut attrs = Vec::with_capacity(records.len() * ATTR_BYTES);
    let mut blocks = Vec::new();
    let mut data = Vec::new();
    let mut postings: BTreeMap<String, Vec<u32>> = BTreeMap::new();
    for (index, (record, &context)) in records.iter().zip(contexts).enumerate() {
        points.extend(((record.lon * 1e7).round() as i32).to_le_bytes());
        points.extend(((record.lat * 1e7).round() as i32).to_le_bytes());
        attrs.extend(kind_ids[&record.kind].to_le_bytes());
        attrs.extend(context.to_le_bytes());
        attrs.push(record.rank);
        if index % RECORDS_PER_BLOCK == 0 {
            blocks.extend(
                u32::try_from(data.len())
                    .map_err(|_| too_large())?
                    .to_le_bytes(),
            );
        }
        put_varint(&mut data, record.names.len() as u64);
        for name in &record.names {
            put_varint(&mut data, name.len() as u64);
            data.extend(name.as_bytes());
        }
        let flags = u8::from(record.ele.is_some()) * HAS_ELE
            + u8::from(record.reference.is_some()) * HAS_REF
            + u8::from(record.id.is_some()) * HAS_ID;
        data.push(flags);
        if let Some(ele) = record.ele {
            put_varint(&mut data, zigzag(ele));
        }
        if let Some(reference) = &record.reference {
            put_varint(&mut data, reference.len() as u64);
            data.extend(reference.as_bytes());
        }
        if let Some(id) = record.id {
            put_varint(&mut data, id);
        }
        let mut seen: Vec<String> = Vec::new();
        for text in record.names.iter().chain(&record.reference) {
            for word in words(text) {
                if !seen.contains(&word) {
                    seen.push(word);
                }
            }
        }
        for word in seen {
            postings.entry(word).or_default().push(index as u32);
        }
    }
    blocks.extend(
        u32::try_from(data.len())
            .map_err(|_| too_large())?
            .to_le_bytes(),
    );

    let mut dict_index = Vec::new();
    let mut dict = Vec::new();
    let mut posting_bytes = Vec::new();
    let mut previous: &[u8] = &[];
    for (i, (word, ids)) in postings.iter().enumerate() {
        let key = word.as_bytes();
        if i % TOKENS_PER_BLOCK == 0 {
            dict_index.extend(
                u32::try_from(dict.len())
                    .map_err(|_| too_large())?
                    .to_le_bytes(),
            );
            dict_index.extend(
                u32::try_from(posting_bytes.len())
                    .map_err(|_| too_large())?
                    .to_le_bytes(),
            );
            put_varint(&mut dict_index, key.len() as u64);
            dict_index.extend(key);
            previous = &[];
        }
        let shared = previous.iter().zip(key).take_while(|(a, b)| a == b).count();
        put_varint(&mut dict, shared as u64);
        put_varint(&mut dict, (key.len() - shared) as u64);
        dict.extend(&key[shared..]);
        put_varint(&mut dict, ids.len() as u64);
        put_varint(&mut dict, u64::from(ids[0]));
        let mut rest = Vec::new();
        for pair in ids.windows(2) {
            put_varint(&mut rest, u64::from(pair[1] - pair[0]));
        }
        put_varint(&mut dict, rest.len() as u64);
        posting_bytes.extend(rest);
        previous = key;
    }

    let meta = serde_json::to_vec(meta).map_err(|e| e.to_string())?;
    let kinds_json = serde_json::to_vec(&kinds_json).map_err(|e| e.to_string())?;
    let sections: [&[u8]; SECTIONS] = [
        &meta,
        &kinds_json,
        &points,
        &attrs,
        &blocks,
        &data,
        &dict_index,
        &dict,
        &posting_bytes,
    ];
    let mut out =
        Vec::with_capacity(HEADER_BYTES + sections.iter().map(|s| s.len()).sum::<usize>());
    out.extend(MAGIC);
    out.extend(SEARCH_INDEX_VERSION.to_le_bytes());
    out.extend((SECTIONS as u32).to_le_bytes());
    let mut offset = HEADER_BYTES as u64;
    for section in sections {
        out.extend(offset.to_le_bytes());
        out.extend((section.len() as u64).to_le_bytes());
        offset += section.len() as u64;
    }
    for section in sections {
        out.extend(section);
    }
    Ok((out, postings.len() as u64))
}

fn too_large() -> String {
    "the search index would pass 4 GiB in one section".into()
}
