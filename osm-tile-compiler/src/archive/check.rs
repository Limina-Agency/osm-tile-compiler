//! Reads a whole archive once: its header and metadata, the sizes of its
//! tiles, and every problem found in its layout or its tiles' encoding.
use super::reader::{ArchiveReader, Entry, compression_name, decompress, tile_type_name};
use crate::CompileBounds;
use crate::tile_stats::{TILE_WARNING_BYTES, TileStats, TileStatsBuilder};
use pmtiles::{TileCoord, TileId};
use serde::Serialize;
use std::collections::BTreeSet;
use std::path::Path;

/// Problems beyond this many are counted but not listed.
const MAX_LISTED_PROBLEMS: usize = 50;
/// The spec keeps the header and root directory in the first 16 KiB, so one
/// request fetches both.
const ROOT_LIMIT: u64 = 16_384;

/// What `inspect` found.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct ArchiveReport {
    pub file_bytes: u64,
    pub tile_type: String,
    pub tile_compression: String,
    pub internal_compression: String,
    pub clustered: bool,
    pub zooms: [u8; 2],
    /// West, south, east, north.
    pub bounds: [f64; 4],
    /// Longitude, latitude, zoom.
    pub center: (f64, f64, u8),
    pub addressed_tiles: u64,
    pub tile_entries: u64,
    pub tile_contents: u64,
    /// The metadata, parsed, or `null` if it is not JSON.
    pub metadata: serde_json::Value,
    pub tile_stats: TileStats,
    /// Every problem found; the archive is valid when it is empty. Only the
    /// first ones are listed, and `problem_count` counts all of them.
    pub problems: Vec<String>,
    pub problem_count: u64,
}

impl ArchiveReport {
    fn problem(&mut self, problem: String) {
        self.problem_count += 1;
        if self.problems.len() < MAX_LISTED_PROBLEMS {
            self.problems.push(problem);
        }
    }
}

fn tile_name(tile_id: u64) -> String {
    TileId::new(tile_id).map_or_else(
        |_| format!("tile ID {tile_id}"),
        |id| {
            let coord = TileCoord::from(id);
            format!("{}/{}/{}", coord.z(), coord.x(), coord.y())
        },
    )
}

/// Reads every part of the archive at `path`. Fails only when the file is
/// not a readable PMTiles v3 archive; anything else wrong is listed in the
/// report's problems.
pub(super) fn inspect(path: &Path) -> Result<ArchiveReport, String> {
    let mut reader = ArchiveReader::open(path)?;
    let header = reader.header.clone();
    let mut report = ArchiveReport {
        file_bytes: reader.file_length,
        tile_type: tile_type_name(header.tile_type).into(),
        tile_compression: compression_name(header.tile_compression).into(),
        internal_compression: compression_name(header.internal_compression).into(),
        clustered: header.clustered,
        zooms: [header.min_zoom, header.max_zoom],
        bounds: header.bounds,
        center: (header.center[0], header.center[1], header.center_zoom),
        addressed_tiles: header.addressed_tiles,
        tile_entries: header.tile_entries,
        tile_contents: header.tile_contents,
        metadata: serde_json::Value::Null,
        tile_stats: TileStats::default(),
        problems: vec![],
        problem_count: 0,
    };
    check_header(&header, reader.file_length, &mut report);
    let layer_names = match reader.metadata() {
        Ok(text) => match serde_json::from_str::<serde_json::Value>(&text) {
            Ok(metadata) => {
                let names = check_metadata(&metadata, header.tile_type, &mut report);
                report.metadata = metadata;
                names
            }
            Err(e) => {
                report.problem(format!("metadata is not JSON: {e}"));
                None
            }
        },
        Err(e) => {
            report.problem(format!("metadata cannot be read: {e}"));
            None
        }
    };

    let mut stats = TileStatsBuilder::new(TILE_WARNING_BYTES);
    let mut previous: Option<Entry> = None;
    let (mut addressed, mut entries) = (0u64, 0u64);
    let mut contents = BTreeSet::new();
    // With clustered data each new tile's bytes follow the previous new one.
    let mut data_end = 0u64;
    let mut problems = Vec::new();
    let walk = reader.visit_entries(|reader, entry| {
        entries += 1;
        addressed = addressed.saturating_add(u64::from(entry.run_length));
        let name = tile_name(entry.tile_id);
        if let Some(previous) = previous
            && entry.tile_id
                < previous
                    .tile_id
                    .saturating_add(u64::from(previous.run_length))
        {
            problems.push(format!(
                "{name} is out of order or overlaps the entry before it"
            ));
        }
        previous = Some(entry);
        if entry.length == 0 {
            problems.push(format!("{name} has no bytes"));
            return Ok(());
        }
        let new_content = contents.insert(entry.offset);
        if header.clustered && new_content {
            if entry.offset != data_end {
                problems.push(format!(
                    "{name} is stored at {} but the archive is marked clustered and the \
                     previous tile ended at {data_end}",
                    entry.offset
                ));
            }
            data_end = entry.offset.saturating_add(u64::from(entry.length));
        }
        let Ok(id) = TileId::new(entry.tile_id) else {
            problems.push(format!("tile ID {} is not valid", entry.tile_id));
            return Ok(());
        };
        let coord = TileCoord::from(id);
        if !(header.min_zoom..=header.max_zoom).contains(&coord.z()) {
            problems.push(format!(
                "{name} lies outside the zoom range {}-{}",
                header.min_zoom, header.max_zoom
            ));
        }
        if !tile_meets_bounds(coord, header.bounds) {
            problems.push(format!("{name} lies outside the bounds"));
        }
        let stored = match reader.tile(&entry) {
            Ok(stored) => stored,
            Err(e) => {
                problems.push(format!("{name}: {e}"));
                return Ok(());
            }
        };
        let mut layers = Vec::new();
        if header.tile_type == 1 {
            match decompress(&stored, header.tile_compression)
                .and_then(|tile| check_mvt(&tile, layer_names.as_ref()))
            {
                Ok(sizes) => layers = sizes,
                Err(e) => problems.push(format!("{name}: {e}")),
            }
        }
        let layers = layers
            .iter()
            .map(|(layer, size)| (layer.as_str(), *size))
            .collect::<Vec<_>>();
        // A run is the same tile repeated: count each copy.
        for i in 0..u64::from(entry.run_length) {
            stats.add(entry.tile_id + i, stored.len() as u64, &layers);
        }
        Ok(())
    });
    if let Err(e) = walk {
        problems.push(format!("directories cannot be read: {e}"));
    }
    for problem in problems {
        report.problem(problem);
    }
    for (field, declared, actual) in [
        ("addressed tiles", header.addressed_tiles, addressed),
        ("tile entries", header.tile_entries, entries),
        ("tile contents", header.tile_contents, contents.len() as u64),
    ] {
        // Zero means the writer did not count.
        if declared != 0 && declared != actual {
            report.problem(format!(
                "the header declares {declared} {field} but the directories hold {actual}"
            ));
        }
    }
    report.tile_stats = stats.finish();
    Ok(report)
}

fn check_header(header: &super::reader::Header, file_length: u64, report: &mut ArchiveReport) {
    let sections = [
        ("root directory", header.root_offset, header.root_length),
        ("metadata", header.metadata_offset, header.metadata_length),
        ("leaf directories", header.leaf_offset, header.leaf_length),
        ("tile data", header.data_offset, header.data_length),
    ];
    for (name, offset, length) in sections {
        if length > 0
            && offset
                .checked_add(length)
                .is_none_or(|end| end > file_length)
        {
            report.problem(format!(
                "the {name} ({offset}+{length}) lies beyond the end of the file"
            ));
        }
    }
    let mut used = sections
        .iter()
        .filter(|s| s.2 > 0)
        .map(|s| (s.1, s.1.saturating_add(s.2), s.0))
        .collect::<Vec<_>>();
    used.sort();
    for pair in used.windows(2) {
        if pair[1].0 < pair[0].1 {
            report.problem(format!("the {} overlaps the {}", pair[1].2, pair[0].2));
        }
    }
    let root_end = header.root_offset.saturating_add(header.root_length);
    if root_end > ROOT_LIMIT {
        report.problem(format!(
            "the root directory ends at byte {root_end}, past the first {ROOT_LIMIT}"
        ));
    }
    if header.min_zoom > header.max_zoom {
        report.problem(format!(
            "the minimum zoom {} exceeds the maximum {}",
            header.min_zoom, header.max_zoom
        ));
    }
    // West above east is a box crossing the antimeridian.
    let [west, south, east, north] = header.bounds;
    if !(west != east && south < north) {
        report.problem(format!(
            "the bounds {west} {south} {east} {north} are empty"
        ));
    }
    let [longitude, latitude] = header.center;
    if !CompileBounds::new(north, south, east, west).contains(longitude, latitude) {
        report.problem(format!(
            "the center {longitude} {latitude} lies outside the bounds"
        ));
    }
    if header.tile_type == 1 && !matches!(header.tile_compression, 1 | 2) {
        report.problem(format!(
            "MVT tiles are {}-compressed, which viewers may not read",
            compression_name(header.tile_compression)
        ));
    }
}

/// Checks the metadata and returns the vector layer names it declares.
fn check_metadata(
    metadata: &serde_json::Value,
    tile_type: u8,
    report: &mut ArchiveReport,
) -> Option<BTreeSet<String>> {
    if !metadata.is_object() {
        report.problem("metadata is not a JSON object".into());
        return None;
    }
    if tile_type != 1 {
        return None;
    }
    let Some(layers) = metadata["vector_layers"].as_array() else {
        report.problem("metadata lists no vector_layers".into());
        return None;
    };
    let names = layers
        .iter()
        .filter_map(|layer| layer["id"].as_str().map(str::to_string))
        .collect::<BTreeSet<_>>();
    if names.len() != layers.len() {
        report.problem("vector_layers has an entry without an id, or a repeated id".into());
    }
    Some(names)
}

/// Whether a tile's square touches the bounds.
fn tile_meets_bounds(coord: TileCoord, [west, south, east, north]: [f64; 4]) -> bool {
    let n = f64::from(1u32 << coord.z());
    let longitude = |x: f64| x / n * 360.0 - 180.0;
    let latitude = |y: f64| {
        (std::f64::consts::PI * (1.0 - 2.0 * y / n))
            .sinh()
            .atan()
            .to_degrees()
    };
    let (x, y) = (f64::from(coord.x()), f64::from(coord.y()));
    // A tile touching the bounds only within this margin still meets them,
    // so rounding in the header's 1e-7 degree precision is not a problem.
    let margin = 1e-6;
    let bounds = CompileBounds::new(north + margin, south - margin, east, west);
    bounds.meets(
        longitude(x) - margin,
        latitude(y + 1.0),
        longitude(x + 1.0) + margin,
        latitude(y),
    )
}

fn varint(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).ok_or("message ends inside a varint")?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    Err("varint longer than 10 bytes".into())
}

/// The protobuf fields of `bytes`: (number, wire type, value or payload).
/// A protobuf field: its number, wire type, varint value and payload.
type Field<'a> = (u64, u8, u64, &'a [u8]);

fn fields(bytes: &[u8]) -> Result<Vec<Field<'_>>, String> {
    let mut at = 0;
    let mut out = Vec::new();
    while at < bytes.len() {
        let key = varint(bytes, &mut at)?;
        let (number, wire) = (key >> 3, (key & 7) as u8);
        match wire {
            0 => out.push((number, wire, varint(bytes, &mut at)?, &bytes[..0])),
            1 | 5 => {
                let size = if wire == 1 { 8 } else { 4 };
                let payload = bytes
                    .get(at..at + size)
                    .ok_or("message ends inside a field")?;
                at += size;
                out.push((number, wire, 0, payload));
            }
            2 => {
                let length = varint(bytes, &mut at)? as usize;
                let payload = at
                    .checked_add(length)
                    .and_then(|end| bytes.get(at..end))
                    .ok_or("message ends inside a field")?;
                at += length;
                out.push((number, wire, 0, payload));
            }
            other => return Err(format!("unknown protobuf wire type {other}")),
        }
    }
    Ok(out)
}

fn packed(bytes: &[u8]) -> Result<Vec<u64>, String> {
    let mut at = 0;
    let mut values = Vec::new();
    while at < bytes.len() {
        values.push(varint(bytes, &mut at)?);
    }
    Ok(values)
}

/// The 256-code-point glyph ranges, by their first code point, that the
/// string values of the tags named `keys` use across every tile of a
/// PMTiles archive. Code points past U+FFFF, which MapLibre draws from no
/// glyph range, are left out. `should_cancel` is asked before each tile.
pub(crate) fn glyph_ranges_used(
    path: &Path,
    keys: &[String],
    should_cancel: &dyn Fn() -> bool,
) -> Result<BTreeSet<u32>, String> {
    let mut reader = ArchiveReader::open(path)?;
    let compression = reader.header.tile_compression;
    let mut ranges = BTreeSet::new();
    reader.visit_entries(|reader, entry| {
        if should_cancel() {
            return Err(crate::control::CANCELLED.into());
        }
        let tile = decompress(&reader.tile(&entry)?, compression)?;
        tile_glyph_ranges(&tile, keys, &mut ranges)
            .map_err(|e| format!("tile {}: {e}", entry.tile_id))
    })?;
    Ok(ranges)
}

fn tile_glyph_ranges(
    tile: &[u8],
    keys: &[String],
    ranges: &mut BTreeSet<u32>,
) -> Result<(), String> {
    for (number, wire, _, layer) in fields(tile)? {
        if (number, wire) != (3, 2) {
            continue;
        }
        let (mut wanted, mut values, mut features) = (Vec::new(), Vec::new(), Vec::new());
        for (number, wire, _, payload) in fields(layer)? {
            match (number, wire) {
                (2, 2) => features.push(payload),
                (3, 2) => wanted.push(keys.iter().any(|key| key.as_bytes() == payload)),
                (4, 2) => values.push(payload),
                _ => {}
            }
        }
        if !wanted.contains(&true) {
            continue;
        }
        let mut used = vec![false; values.len()];
        for feature in features {
            for (number, wire, _, payload) in fields(feature)? {
                if (number, wire) != (2, 2) {
                    continue;
                }
                for [key, value] in packed(payload)?.as_chunks::<2>().0 {
                    if wanted.get(*key as usize) == Some(&true)
                        && let Some(used) = used.get_mut(*value as usize)
                    {
                        *used = true;
                    }
                }
            }
        }
        for (value, _) in values.iter().zip(used).filter(|(_, used)| *used) {
            for (number, wire, _, text) in fields(value)? {
                if (number, wire) == (1, 2)
                    && let Ok(text) = std::str::from_utf8(text)
                {
                    ranges.extend(
                        text.chars()
                            .map(u32::from)
                            .filter(|&c| c < 0x10000)
                            .map(|c| c / 256 * 256),
                    );
                }
            }
        }
    }
    Ok(())
}

/// Checks an uncompressed MVT tile against the spec (version 2) and returns
/// each layer's name and encoded size in tile order.
fn check_mvt(
    tile: &[u8],
    declared: Option<&BTreeSet<String>>,
) -> Result<Vec<(String, u64)>, String> {
    let mut layers = Vec::new();
    let mut at = 0;
    while at < tile.len() {
        let start = at;
        let key = varint(tile, &mut at)?;
        if key & 7 != 2 {
            return Err(format!("top-level field {} is not a message", key >> 3));
        }
        let length = varint(tile, &mut at)? as usize;
        let payload = at
            .checked_add(length)
            .and_then(|end| tile.get(at..end))
            .ok_or("tile ends inside a layer")?;
        at += length;
        if key >> 3 != 3 {
            continue;
        }
        let name = check_layer(payload)?;
        if declared.is_some_and(|names| !names.contains(&name)) {
            return Err(format!(
                "layer {name} is not in the metadata's vector_layers"
            ));
        }
        if layers.iter().any(|(other, _)| *other == name) {
            return Err(format!("layer {name} appears twice"));
        }
        layers.push((name, (at - start) as u64));
    }
    Ok(layers)
}

fn check_layer(layer: &[u8]) -> Result<String, String> {
    let (mut name, mut version, mut extent) = (None, 1, 4096);
    let (mut keys, mut values, mut features) = (0u64, 0u64, Vec::new());
    for (number, wire, value, payload) in fields(layer)? {
        match (number, wire) {
            (1, 2) => {
                name = Some(
                    String::from_utf8(payload.to_vec()).map_err(|_| "a layer name is not UTF-8")?,
                )
            }
            (2, 2) => features.push(payload),
            (3, 2) => keys += 1,
            (4, 2) => values += 1,
            (5, 0) => extent = value,
            (15, 0) => version = value,
            _ => {}
        }
    }
    let name = name.ok_or("a layer has no name")?;
    if version != 2 {
        return Err(format!("layer {name} is MVT version {version}, not 2"));
    }
    if extent == 0 {
        return Err(format!("layer {name} has extent 0"));
    }
    for (index, feature) in features.into_iter().enumerate() {
        check_feature(feature, keys, values)
            .map_err(|e| format!("layer {name} feature #{index}: {e}"))?;
    }
    Ok(name)
}

fn check_feature(feature: &[u8], keys: u64, values: u64) -> Result<(), String> {
    let (mut kind, mut tags, mut commands) = (0, Vec::new(), Vec::new());
    for (number, wire, value, payload) in fields(feature)? {
        match (number, wire) {
            (2, 2) => tags = packed(payload)?,
            (3, 0) => kind = value,
            (4, 2) => commands = packed(payload)?,
            _ => {}
        }
    }
    if tags.len() % 2 != 0 {
        return Err("odd number of tag indices".into());
    }
    for pair in tags.chunks(2) {
        if pair[0] >= keys || pair[1] >= values {
            return Err(format!(
                "tag {}={} indexes past the layer's keys or values",
                pair[0], pair[1]
            ));
        }
    }
    if !(1..=3).contains(&kind) {
        return Err(format!(
            "geometry type {kind} is not point, line or polygon"
        ));
    }
    let mut at = 0;
    let mut moves = 0;
    while at < commands.len() {
        let (id, count) = (commands[at] & 7, (commands[at] >> 3) as usize);
        at += 1;
        match id {
            1 | 2 => {
                if count == 0 {
                    return Err("a MoveTo or LineTo with no points".into());
                }
                at = at
                    .checked_add(count * 2)
                    .filter(|&end| end <= commands.len())
                    .ok_or("geometry ends inside a command's points")?;
                if id == 1 {
                    moves += 1;
                }
            }
            7 if kind == 3 && count == 1 => {}
            _ => {
                return Err(format!(
                    "command {id}×{count} is not valid in this geometry"
                ));
            }
        }
    }
    if moves == 0 {
        return Err("empty geometry".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use fast_mvt::{MvtCoord, MvtGeomType, MvtLayerBuilder};

    fn tile() -> Vec<u8> {
        let mut feature = MvtLayerBuilder::new("poi")
            .unwrap()
            .feature_of(MvtGeomType::Point);
        feature.id(Some(1));
        feature.tag_string("name", "x").unwrap();
        feature.points([MvtCoord { x: 5, y: 5 }]).unwrap();
        feature.end().encode()
    }

    fn varint(out: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            out.push(value as u8 | 0x80);
            value >>= 7;
        }
        out.push(value as u8);
    }

    /// An archive whose header sections and directory offsets are near
    /// u64::MAX, so that offset + length overflows.
    fn overflowing_archive(path: &Path, root_offset: u64) {
        let mut root = Vec::new();
        // One tile entry and one leaf pointer, each at offset u64::MAX - 1.
        for value in [2, 0, 1, 1, 0, 10, 10, u64::MAX, u64::MAX] {
            varint(&mut root, value);
        }
        let mut bytes = b"PMTiles\x03".to_vec();
        let fields = [
            root_offset,
            root.len() as u64,
            u64::MAX - 3,
            8,
            u64::MAX - 3,
            u64::MAX,
            u64::MAX - 3,
            u64::MAX,
        ];
        for field in fields {
            bytes.extend(field.to_le_bytes());
        }
        bytes.resize(127, 0);
        bytes[97] = 1; // directories stored without compression
        bytes[99] = 1;
        bytes.extend(&root);
        std::fs::write(path, bytes).unwrap();
    }

    /// Found by the pmtiles_inspect fuzz target: these overflowed, which
    /// panics in debug builds and wraps in release ones.
    #[test]
    fn offsets_near_the_end_of_u64_are_problems_not_panics() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("overflow.pmtiles");
        for root_offset in [127, u64::MAX - 1] {
            overflowing_archive(&path, root_offset);
            let report = inspect(&path).unwrap();
            assert!(report.problem_count > 0, "{:?}", report.problems);
        }
        overflowing_archive(&path, 127);
        let mut reader = ArchiveReader::open(&path).unwrap();
        let mut leaves = 0;
        let walked = reader.walk(|reader, entry, leaf| {
            if leaf {
                leaves += 1;
            } else {
                assert!(reader.tile(&entry).unwrap_err().contains("beyond"));
            }
            Ok(())
        });
        assert!(walked.unwrap_err().contains("beyond the leaf section"));
        assert_eq!(leaves, 1);
    }

    #[test]
    fn measures_valid_layers() {
        let tile = tile();
        let declared = BTreeSet::from(["poi".to_string()]);
        let layers = check_mvt(&tile, Some(&declared)).unwrap();
        assert_eq!(layers, [("poi".to_string(), tile.len() as u64)]);
    }

    #[test]
    fn reports_layers_missing_from_metadata_and_broken_tiles() {
        let tile = tile();
        let declared = BTreeSet::from(["water".to_string()]);
        assert!(
            check_mvt(&tile, Some(&declared))
                .unwrap_err()
                .contains("not in the metadata")
        );
        assert!(check_mvt(&tile[..tile.len() - 1], None).is_err());
        assert!(
            check_mvt(&[0x1a, 0x01, 0x00], None).is_err(),
            "a layer without a name"
        );
    }

    #[test]
    fn rejects_malformed_features() {
        // Field 3 (type) = 1, field 4 (geometry) = [LineTo×0].
        assert!(check_feature(&[0x18, 0x01, 0x22, 0x01, 0x02], 0, 0).is_err());
        // A point with MoveTo(1) but only one coordinate.
        assert!(check_feature(&[0x18, 0x01, 0x22, 0x02, 0x09, 0x02], 0, 0).is_err());
        // Tags indexing past the keys.
        assert!(
            check_feature(
                &[
                    0x12, 0x02, 0x05, 0x00, 0x18, 0x01, 0x22, 0x03, 0x09, 0x02, 0x02
                ],
                1,
                1
            )
            .is_err()
        );
        assert!(
            check_feature(
                &[
                    0x12, 0x02, 0x00, 0x00, 0x18, 0x01, 0x22, 0x03, 0x09, 0x02, 0x02
                ],
                1,
                1
            )
            .is_ok()
        );
    }

    #[test]
    fn glyph_ranges_come_from_the_label_keys_only() {
        let mut feature = MvtLayerBuilder::new("place")
            .unwrap()
            .feature_of(MvtGeomType::Point);
        feature.tag_string("name:latin", "Kyiv").unwrap();
        feature.tag_string("name:nonlatin", "Київ").unwrap();
        feature.tag_string("name:zh", "基辅").unwrap();
        feature.tag_string("class", "Ωmega").unwrap();
        feature.points([MvtCoord { x: 5, y: 5 }]).unwrap();
        let tile = feature.end().encode();
        let keys = ["name:latin".to_string(), "name:nonlatin".to_string()];
        let mut ranges = BTreeSet::new();
        tile_glyph_ranges(&tile, &keys, &mut ranges).unwrap();
        assert_eq!(ranges, BTreeSet::from([0, 1024]));
        assert!(tile_glyph_ranges(&tile[..tile.len() - 1], &keys, &mut ranges).is_err());
    }

    #[test]
    fn tiles_meet_bounds_by_their_squares() {
        let bounds = [7.40, 43.72, 7.45, 43.76];
        assert!(tile_meets_bounds(
            TileCoord::new(14, 8529, 5974).unwrap(),
            bounds
        ));
        assert!(tile_meets_bounds(TileCoord::new(0, 0, 0).unwrap(), bounds));
        assert!(!tile_meets_bounds(
            TileCoord::new(14, 0, 0).unwrap(),
            bounds
        ));
    }
}
