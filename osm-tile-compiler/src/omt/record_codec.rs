//! Compact, lossless encoding of a spilled `Record`.
//!
//! The sort key (tile, layer, sort key, id) travels in the frame header, so the
//! payload holds only the rest: fixed fields, tile-local geometry as zigzag
//! varint deltas, and attributes whose keys are one-byte ids from a static
//! table. Ids never reach the output: they decode back to the exact strings,
//! and keys outside the table are stored verbatim.
use super::{
    render::Record,
    store::{AttrKey, AttrMap, AttrValue},
};
use crate::tile_geometry::{TileGeometry, TilePoint, TileRingGroup};

/// The OMT layer names in byte order, so an index orders like its name.
pub(super) const LAYER_NAMES: [&str; 16] = [
    "aerodrome_label",
    "aeroway",
    "boundary",
    "building",
    "housenumber",
    "landcover",
    "landuse",
    "mountain_peak",
    "park",
    "place",
    "poi",
    "transportation",
    "transportation_name",
    "water",
    "water_name",
    "waterway",
];

#[test]
fn layer_names_are_the_sorted_schema_layers() {
    let mut layers = super::LAYERS;
    layers.sort_unstable();
    assert_eq!(LAYER_NAMES, layers);
}

pub(super) fn layer_id(name: &str) -> Option<u8> {
    LAYER_NAMES.binary_search(&name).ok().map(|id| id as u8)
}

// Every attribute name the profile can produce, in byte order (at most 254).
// A missing name only costs bytes.
const ATTR_KEYS: [&str; 153] = [
    "__agg_stop_key",
    "__agg_stop_order",
    "__housenumber_has_name",
    "__housenumber_partition",
    "__university_min_size",
    "_numpoints",
    "access",
    "admin_level",
    "agg_stop",
    "bicycle",
    "brunnel",
    "capital",
    "class",
    "colour",
    "customary_ft",
    "disputed",
    "ele",
    "ele_ft",
    "expressway",
    "foot",
    "hide_3d",
    "horse",
    "housenumber",
    "iata",
    "icao",
    "id",
    "indoor",
    "intermittent",
    "layer",
    "level",
    "maritime",
    "mtb_scale",
    "name",
    "name:af",
    "name:am",
    "name:ar",
    "name:az",
    "name:be",
    "name:bg",
    "name:bn",
    "name:br",
    "name:bs",
    "name:ca",
    "name:co",
    "name:cs",
    "name:cy",
    "name:da",
    "name:de",
    "name:el",
    "name:en",
    "name:eo",
    "name:es",
    "name:et",
    "name:eu",
    "name:fa",
    "name:fi",
    "name:fr",
    "name:fy",
    "name:ga",
    "name:gd",
    "name:he",
    "name:hi",
    "name:hr",
    "name:hu",
    "name:hy",
    "name:id",
    "name:is",
    "name:it",
    "name:ja",
    "name:ja-Hira",
    "name:ja-Latn",
    "name:ka",
    "name:kk",
    "name:kn",
    "name:ko",
    "name:ko-Latn",
    "name:ku",
    "name:la",
    "name:latin",
    "name:lb",
    "name:lt",
    "name:lv",
    "name:mk",
    "name:ml",
    "name:mt",
    "name:nl",
    "name:no",
    "name:nonlatin",
    "name:oc",
    "name:pa",
    "name:pl",
    "name:pnb",
    "name:pt",
    "name:rm",
    "name:ro",
    "name:ru",
    "name:sk",
    "name:sl",
    "name:sq",
    "name:sr",
    "name:sr-Latn",
    "name:sv",
    "name:ta",
    "name:te",
    "name:th",
    "name:tok",
    "name:tr",
    "name:uk",
    "name:ur",
    "name:vi",
    "name:zh",
    "name:zh-Hans",
    "name:zh-Hant",
    "name_de",
    "name_en",
    "name_int",
    "network",
    "official",
    "oneway",
    "ramp",
    "rank",
    "ref",
    "ref_length",
    "render_height",
    "render_min_height",
    "route_1_colour",
    "route_1_name",
    "route_1_network",
    "route_1_ref",
    "route_2_colour",
    "route_2_name",
    "route_2_network",
    "route_2_ref",
    "route_3_colour",
    "route_3_name",
    "route_3_network",
    "route_3_ref",
    "route_4_colour",
    "route_4_name",
    "route_4_network",
    "route_4_ref",
    "route_5_colour",
    "route_5_name",
    "route_5_network",
    "route_5_ref",
    "route_6_colour",
    "route_6_name",
    "route_6_network",
    "route_6_ref",
    "service",
    "subclass",
    "surface",
    "toll",
];
/// Introduces a key stored as length-prefixed UTF-8; table ids are index + 1.
const KEY_VERBATIM: u8 = 0;

const GEOMETRY_POINT: u8 = 1;
const GEOMETRY_LINE: u8 = 2;
const GEOMETRY_POLYGON: u8 = 3;
const GEOMETRY_FILLED: u8 = 4;

const VALUE_STRING: u8 = 0;
const VALUE_INT: u8 = 1;
const VALUE_UINT: u8 = 2;
const VALUE_DOUBLE: u8 = 3;
const VALUE_FALSE: u8 = 4;
const VALUE_TRUE: u8 = 5;
const VALUE_REPEAT: u8 = 6;
const REPEAT_WINDOW: usize = 8;
const MIN_REPEAT_BYTES: usize = 3;

const FLAG_RANK: u8 = 1;
const FLAG_GRID_CELL: u8 = 2;
const FLAG_GRID_SIZE: u8 = 4;

pub(super) fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    (value >> 1) as i64 ^ -((value & 1) as i64)
}

/// Writes `value` as a little-endian base-128 varint; returns its length.
pub(super) fn encode_varint(mut value: u64, bytes: &mut [u8; 10]) -> usize {
    let mut len = 0;
    while value >= 0x80 {
        bytes[len] = value as u8 | 0x80;
        value >>= 7;
        len += 1;
    }
    bytes[len] = value as u8;
    len + 1
}

pub(super) fn put_varint(out: &mut Vec<u8>, value: u64) {
    let mut bytes = [0; 10];
    let len = encode_varint(value, &mut bytes);
    out.extend_from_slice(&bytes[..len]);
}

pub(super) fn put_zigzag(out: &mut Vec<u8>, value: i64) {
    put_varint(out, zigzag(value));
}

fn put_str(out: &mut Vec<u8>, value: &str) {
    put_varint(out, value.len() as u64);
    out.extend_from_slice(value.as_bytes());
}

#[derive(Clone)]
pub(super) struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    pub(super) fn new(bytes: &'a [u8]) -> Self {
        Self(bytes)
    }

    pub(super) fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub(super) fn u8(&mut self) -> Result<u8, String> {
        let (&first, rest) = self.0.split_first().ok_or("Truncated OMT record")?;
        self.0 = rest;
        Ok(first)
    }

    pub(super) fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
        if len > self.0.len() {
            return Err("Truncated OMT record".into());
        }
        let (taken, rest) = self.0.split_at(len);
        self.0 = rest;
        Ok(taken)
    }

    pub(super) fn varint(&mut self) -> Result<u64, String> {
        let mut value = 0u64;
        for shift in (0..64).step_by(7) {
            let byte = self.u8()?;
            let bits = u64::from(byte & 0x7f);
            if shift == 63 && bits > 1 {
                break;
            }
            value |= bits << shift;
            if byte < 0x80 {
                return Ok(value);
            }
        }
        Err("Invalid OMT record varint".into())
    }

    pub(super) fn zigzag(&mut self) -> Result<i64, String> {
        self.varint().map(unzigzag)
    }

    /// Reads a varint and appends it to `out` as `put_varint` would: its own
    /// bytes when they are already the shortest form.
    fn copy_varint(&mut self, out: &mut Vec<u8>) -> Result<u64, String> {
        let before = self.0;
        let value = self.varint()?;
        let used = before.len() - self.0.len();
        let shortest = (64 - (value | 1).leading_zeros() as usize).div_ceil(7);
        if used == shortest {
            out.extend_from_slice(&before[..used]);
        } else {
            put_varint(out, value);
        }
        Ok(value)
    }

    pub(super) fn str(&mut self) -> Result<&'a str, String> {
        let len = self.length()?;
        std::str::from_utf8(self.take(len)?).map_err(|e| e.to_string())
    }

    /// A length or element count. Every element takes at least one byte, so a
    /// count beyond the remaining bytes is corrupt, however large.
    pub(super) fn length(&mut self) -> Result<usize, String> {
        usize::try_from(self.varint()?)
            .ok()
            .filter(|&len| len <= self.0.len())
            .ok_or_else(|| "Truncated OMT record".into())
    }

    /// The bytes not read yet.
    pub(super) fn remaining(&self) -> usize {
        self.0.len()
    }

    fn coordinate(&mut self, previous: i32) -> Result<i32, String> {
        i64::from(previous)
            .checked_add(self.zigzag()?)
            .and_then(|value| i32::try_from(value).ok())
            .ok_or_else(|| "OMT record coordinate out of range".into())
    }

    fn points(&mut self, previous: &mut TilePoint) -> Result<Vec<TilePoint>, String> {
        let count = self.length()?;
        let mut points = Vec::with_capacity(count);
        for _ in 0..count {
            let point = TilePoint::new(self.coordinate(previous.x)?, self.coordinate(previous.y)?);
            points.push(point);
            *previous = point;
        }
        Ok(points)
    }
}

#[cfg(test)]
pub(super) fn encode_payload(record: &Record, out: &mut Vec<u8>) {
    encode_payload_head(record, out);
    put_attrs(out, &record.attrs);
}

/// The payload up to its attributes, which `encode_attrs` writes and which
/// come last: `encode_payload` is the two in turn.
pub(super) fn encode_payload_head(record: &Record, out: &mut Vec<u8>) {
    out.push(record.zoom);
    let grid_size = record.grid_size.to_bits();
    out.push(
        if record.rank { FLAG_RANK } else { 0 }
            | if record.grid_cell.is_some() {
                FLAG_GRID_CELL
            } else {
                0
            }
            | if grid_size != 0 { FLAG_GRID_SIZE } else { 0 },
    );
    if grid_size != 0 {
        out.extend_from_slice(&grid_size.to_le_bytes());
    }
    if let Some((x, y)) = record.grid_cell {
        put_zigzag(out, x.into());
        put_zigzag(out, y.into());
    }
    put_varint(out, record.grid_limit as u64);
    put_geometry(out, &record.geometry);
}

/// The attribute part of a payload, the bytes `encode_payload` ends with.
pub(super) fn encode_attrs(attrs: &AttrMap, out: &mut Vec<u8>) {
    put_attrs(out, attrs);
}

fn put_points(out: &mut Vec<u8>, previous: &mut TilePoint, points: &[TilePoint]) {
    put_varint(out, points.len() as u64);
    for point in points {
        put_zigzag(out, i64::from(point.x) - i64::from(previous.x));
        put_zigzag(out, i64::from(point.y) - i64::from(previous.y));
        *previous = *point;
    }
}

// Deltas run on through every path of a tile geometry: its paths are close.
fn put_geometry(out: &mut Vec<u8>, geometry: &TileGeometry) {
    let mut previous = TilePoint::default();
    match geometry {
        TileGeometry::Point(points) => {
            out.push(GEOMETRY_POINT);
            put_points(out, &mut previous, points);
        }
        TileGeometry::Line { scale_exp, paths } => {
            out.push(GEOMETRY_LINE);
            out.push(*scale_exp);
            put_varint(out, paths.len() as u64);
            for path in paths {
                put_points(out, &mut previous, path);
            }
        }
        TileGeometry::Polygon(groups) => {
            out.push(GEOMETRY_POLYGON);
            put_varint(out, groups.len() as u64);
            for group in groups {
                put_points(out, &mut previous, &group.exterior);
                put_varint(out, group.holes.len() as u64);
                for hole in &group.holes {
                    put_points(out, &mut previous, hole);
                }
            }
        }
        TileGeometry::FilledPolygon { buffer_pixels } => {
            out.push(GEOMETRY_FILLED);
            put_zigzag(out, (*buffer_pixels).into());
        }
    }
}

// The name variants of a feature (`name`, `name:latin`, `name_en`, ...) mostly
// share one value, so a string equal to one of the last few string values of
// the record is stored as the distance back to it.
fn put_attrs(out: &mut Vec<u8>, attrs: &AttrMap) {
    put_varint(out, attrs.len() as u64);
    let mut recent = [""; REPEAT_WINDOW];
    let mut strings = 0;
    for (key, value) in attrs {
        match ATTR_KEYS.binary_search(&&**key) {
            Ok(index) => out.push(index as u8 + 1),
            Err(_) => {
                out.push(KEY_VERBATIM);
                put_str(out, key);
            }
        }
        match value {
            AttrValue::String(value) => {
                let back = (value.len() >= MIN_REPEAT_BYTES)
                    .then(|| {
                        (1..=strings.min(REPEAT_WINDOW))
                            .find(|back| recent[(strings - back) % REPEAT_WINDOW] == value)
                    })
                    .flatten();
                if let Some(back) = back {
                    out.extend_from_slice(&[VALUE_REPEAT, back as u8]);
                } else {
                    out.push(VALUE_STRING);
                    put_str(out, value);
                }
                recent[strings % REPEAT_WINDOW] = value;
                strings += 1;
            }
            AttrValue::Int(value) => {
                out.push(VALUE_INT);
                put_zigzag(out, *value);
            }
            AttrValue::UInt(value) => {
                out.push(VALUE_UINT);
                put_varint(out, *value);
            }
            AttrValue::Double(value) => {
                out.push(VALUE_DOUBLE);
                out.extend_from_slice(&value.to_bits().to_le_bytes());
            }
            AttrValue::Bool(value) => out.push(if *value { VALUE_TRUE } else { VALUE_FALSE }),
        }
    }
}

/// Rebuilds the record whose sort-key fields are given and whose remaining
/// fields `bytes` hold; every byte must be consumed.
pub(super) fn decode_payload(
    bytes: &[u8],
    tile_id: u64,
    layer: AttrKey,
    sort_key: i64,
    id: u64,
) -> Result<Record, String> {
    let mut reader = Reader::new(bytes);
    let zoom = reader.u8()?;
    let flags = reader.u8()?;
    if flags & !(FLAG_RANK | FLAG_GRID_CELL | FLAG_GRID_SIZE) != 0 {
        return Err("Invalid OMT record flags".into());
    }
    let grid_size = if flags & FLAG_GRID_SIZE != 0 {
        f64::from_bits(u64::from_le_bytes(reader.take(8)?.try_into().unwrap()))
    } else {
        0.0
    };
    let grid_cell = if flags & FLAG_GRID_CELL != 0 {
        let mut cell = || {
            reader
                .zigzag()
                .and_then(|v| i32::try_from(v).map_err(|e| e.to_string()))
        };
        Some((cell()?, cell()?))
    } else {
        None
    };
    let grid_limit = usize::try_from(reader.varint()?).map_err(|e| e.to_string())?;
    let geometry = read_geometry(&mut reader)?;
    let attrs = read_attrs(&mut reader)?;
    if !reader.is_empty() {
        return Err("Trailing bytes in OMT record".into());
    }
    Ok(Record {
        tile_id,
        zoom,
        id,
        layer,
        geometry,
        attrs,
        sort_key,
        grid_size,
        grid_cell,
        grid_limit,
        rank: flags & FLAG_RANK != 0,
    })
}

fn read_geometry(reader: &mut Reader) -> Result<TileGeometry, String> {
    let mut previous = TilePoint::default();
    Ok(match reader.u8()? {
        GEOMETRY_POINT => TileGeometry::Point(reader.points(&mut previous)?),
        GEOMETRY_LINE => {
            let scale_exp = reader.u8()?;
            let count = reader.length()?;
            let mut paths = Vec::with_capacity(count);
            for _ in 0..count {
                paths.push(reader.points(&mut previous)?);
            }
            TileGeometry::Line { scale_exp, paths }
        }
        GEOMETRY_POLYGON => {
            let count = reader.length()?;
            let mut groups = Vec::with_capacity(count);
            for _ in 0..count {
                let exterior = reader.points(&mut previous)?;
                let hole_count = reader.length()?;
                let mut holes = Vec::with_capacity(hole_count);
                for _ in 0..hole_count {
                    holes.push(reader.points(&mut previous)?);
                }
                groups.push(TileRingGroup { exterior, holes });
            }
            TileGeometry::Polygon(groups)
        }
        GEOMETRY_FILLED => TileGeometry::FilledPolygon {
            buffer_pixels: i16::try_from(reader.zigzag()?).map_err(|e| e.to_string())?,
        },
        _ => return Err("Unknown OMT record geometry".into()),
    })
}

fn read_attrs(reader: &mut Reader) -> Result<AttrMap, String> {
    let count = reader.length()?;
    let mut attrs = Vec::with_capacity(count);
    for _ in 0..count {
        let key = match reader.u8()? {
            KEY_VERBATIM => AttrKey::Owned(reader.str()?.to_owned()),
            id => AttrKey::Borrowed(
                ATTR_KEYS
                    .get(usize::from(id) - 1)
                    .copied()
                    .ok_or("Unknown OMT record attribute key")?,
            ),
        };
        let value = match reader.u8()? {
            VALUE_STRING => AttrValue::String(reader.str()?.to_owned()),
            VALUE_REPEAT => {
                let back = usize::from(reader.u8()?);
                let repeated = attrs
                    .iter()
                    .rev()
                    .filter_map(|(_, value)| match value {
                        AttrValue::String(value) => Some(value),
                        _ => None,
                    })
                    .nth(back.wrapping_sub(1))
                    .ok_or("Invalid OMT record string repeat")?;
                AttrValue::String(repeated.clone())
            }
            VALUE_INT => AttrValue::Int(reader.zigzag()?),
            VALUE_UINT => AttrValue::UInt(reader.varint()?),
            VALUE_DOUBLE => AttrValue::Double(f64::from_bits(u64::from_le_bytes(
                reader.take(8)?.try_into().unwrap(),
            ))),
            VALUE_FALSE => AttrValue::Bool(false),
            VALUE_TRUE => AttrValue::Bool(true),
            _ => return Err("Unknown OMT record attribute value".into()),
        };
        attrs.push((key, value));
    }
    // Encoded in key order, so this is a bulk build of an already sorted map.
    Ok(attrs.into_iter().collect())
}

/// The streams `split_payload` sorts a payload's bytes into. Like bytes
/// compress better together: coordinates with coordinates, attribute keys
/// with attribute keys, and so on.
pub(super) const PAYLOAD_STREAMS: usize = 5;
const META: usize = 0;
const COUNTS: usize = 1;
const COORDS: usize = 2;
const ATTR_KEYS_STREAM: usize = 3;
const ATTR_VALUES: usize = 4;

/// Appends the parts of `payload`, a well-formed `encode_payload` output, to
/// `streams`. `join_payload` rebuilds the payload byte for byte. Every ring
/// of a polygon closes on its first point, so a ring's last point is dropped
/// when it equals its first and its count says so.
pub(super) fn split_payload(
    payload: &[u8],
    streams: &mut [Vec<u8>; PAYLOAD_STREAMS],
) -> Result<(), String> {
    let mut reader = Reader::new(payload);
    let reader = &mut reader;
    let zoom = reader.u8()?;
    let flags = reader.u8()?;
    streams[META].extend_from_slice(&[zoom, flags]);
    if flags & FLAG_GRID_SIZE != 0 {
        streams[META].extend_from_slice(reader.take(8)?);
    }
    if flags & FLAG_GRID_CELL != 0 {
        put_varint(&mut streams[META], reader.varint()?);
        put_varint(&mut streams[META], reader.varint()?);
    }
    put_varint(&mut streams[META], reader.varint()?);
    let kind = reader.u8()?;
    streams[META].push(kind);
    match kind {
        GEOMETRY_POINT => split_path(reader, streams, false)?,
        GEOMETRY_LINE => {
            streams[META].push(reader.u8()?);
            let paths = reader.length()?;
            put_varint(&mut streams[COUNTS], paths as u64);
            for _ in 0..paths {
                split_path(reader, streams, false)?;
            }
        }
        GEOMETRY_POLYGON => {
            let groups = reader.length()?;
            put_varint(&mut streams[COUNTS], groups as u64);
            for _ in 0..groups {
                split_path(reader, streams, true)?;
                let holes = reader.length()?;
                put_varint(&mut streams[COUNTS], holes as u64);
                for _ in 0..holes {
                    split_path(reader, streams, true)?;
                }
            }
        }
        GEOMETRY_FILLED => put_varint(&mut streams[META], reader.varint()?),
        _ => return Err("Unknown OMT record geometry".into()),
    }
    let attrs = reader.length()?;
    put_varint(&mut streams[ATTR_KEYS_STREAM], attrs as u64);
    for _ in 0..attrs {
        let key = reader.u8()?;
        streams[ATTR_KEYS_STREAM].push(key);
        if key == KEY_VERBATIM {
            let len = reader.length()?;
            put_varint(&mut streams[ATTR_KEYS_STREAM], len as u64);
            streams[ATTR_KEYS_STREAM].extend_from_slice(reader.take(len)?);
        }
        let tag = reader.u8()?;
        streams[ATTR_KEYS_STREAM].push(tag);
        let values = &mut streams[ATTR_VALUES];
        match tag {
            VALUE_STRING => {
                let len = reader.length()?;
                put_varint(values, len as u64);
                values.extend_from_slice(reader.take(len)?);
            }
            VALUE_REPEAT => values.push(reader.u8()?),
            VALUE_INT | VALUE_UINT => put_varint(values, reader.varint()?),
            VALUE_DOUBLE => values.extend_from_slice(reader.take(8)?),
            VALUE_FALSE | VALUE_TRUE => {}
            _ => return Err("Unknown OMT record attribute value".into()),
        }
    }
    if !reader.is_empty() {
        return Err("Trailing bytes in OMT record".into());
    }
    Ok(())
}

fn split_path(
    reader: &mut Reader,
    streams: &mut [Vec<u8>; PAYLOAD_STREAMS],
    ring: bool,
) -> Result<(), String> {
    let count = reader.length()?;
    // The last point equals the first when the deltas after the first sum
    // to zero; it is then taken off the stream again.
    let coords = &mut streams[COORDS];
    let (mut x, mut y) = (0i64, 0i64);
    let mut last = coords.len();
    for point in 0..count {
        last = coords.len();
        let (dx, dy) = (reader.copy_varint(coords)?, reader.copy_varint(coords)?);
        if point > 0 {
            x = x.wrapping_add(unzigzag(dx));
            y = y.wrapping_add(unzigzag(dy));
        }
    }
    let closed = ring && count >= 2 && x == 0 && y == 0;
    if closed {
        coords.truncate(last);
    }
    if ring {
        put_varint(
            &mut streams[COUNTS],
            (count as u64) << 1 | u64::from(closed),
        );
    } else {
        put_varint(&mut streams[COUNTS], count as u64);
    }
    Ok(())
}

/// Appends the payload whose parts `split_payload` wrote to `streams` (now
/// being read) to `out`.
pub(super) fn join_payload(
    streams: &mut [Reader; PAYLOAD_STREAMS],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let meta = &mut streams[META];
    let zoom = meta.u8()?;
    let flags = meta.u8()?;
    out.extend_from_slice(&[zoom, flags]);
    if flags & FLAG_GRID_SIZE != 0 {
        out.extend_from_slice(meta.take(8)?);
    }
    if flags & FLAG_GRID_CELL != 0 {
        put_varint(out, meta.varint()?);
        put_varint(out, meta.varint()?);
    }
    put_varint(out, meta.varint()?);
    let kind = meta.u8()?;
    out.push(kind);
    match kind {
        GEOMETRY_POINT => join_path(streams, out, false)?,
        GEOMETRY_LINE => {
            out.push(streams[META].u8()?);
            let paths = streams[COUNTS].varint()?;
            put_varint(out, paths);
            for _ in 0..paths {
                join_path(streams, out, false)?;
            }
        }
        GEOMETRY_POLYGON => {
            let groups = streams[COUNTS].varint()?;
            put_varint(out, groups);
            for _ in 0..groups {
                join_path(streams, out, true)?;
                let holes = streams[COUNTS].varint()?;
                put_varint(out, holes);
                for _ in 0..holes {
                    join_path(streams, out, true)?;
                }
            }
        }
        GEOMETRY_FILLED => put_varint(out, streams[META].varint()?),
        _ => return Err("Unknown OMT record geometry".into()),
    }
    let keys = &mut streams[ATTR_KEYS_STREAM];
    let attrs = keys.length()?;
    put_varint(out, attrs as u64);
    for _ in 0..attrs {
        let keys = &mut streams[ATTR_KEYS_STREAM];
        let key = keys.u8()?;
        out.push(key);
        if key == KEY_VERBATIM {
            let len = keys.length()?;
            put_varint(out, len as u64);
            out.extend_from_slice(keys.take(len)?);
        }
        let tag = keys.u8()?;
        out.push(tag);
        let values = &mut streams[ATTR_VALUES];
        match tag {
            VALUE_STRING => {
                let len = values.length()?;
                put_varint(out, len as u64);
                out.extend_from_slice(values.take(len)?);
            }
            VALUE_REPEAT => out.push(values.u8()?),
            VALUE_INT | VALUE_UINT => put_varint(out, values.varint()?),
            VALUE_DOUBLE => out.extend_from_slice(values.take(8)?),
            VALUE_FALSE | VALUE_TRUE => {}
            _ => return Err("Unknown OMT record attribute value".into()),
        }
    }
    Ok(())
}

fn join_path(
    streams: &mut [Reader; PAYLOAD_STREAMS],
    out: &mut Vec<u8>,
    ring: bool,
) -> Result<(), String> {
    let count = streams[COUNTS].varint()?;
    let (count, closed) = if ring {
        (count >> 1, count & 1 == 1)
    } else {
        (count, false)
    };
    if closed && count < 2 {
        return Err("Invalid OMT run ring".into());
    }
    put_varint(out, count);
    let coords = &mut streams[COORDS];
    let (mut x, mut y) = (0i64, 0i64);
    for point in 0..count - u64::from(closed) {
        let (dx, dy) = (coords.varint()?, coords.varint()?);
        put_varint(out, dx);
        put_varint(out, dy);
        if point > 0 {
            x = x.wrapping_add(unzigzag(dx));
            y = y.wrapping_add(unzigzag(dy));
        }
    }
    if closed {
        put_zigzag(out, x.wrapping_neg());
        put_zigzag(out, y.wrapping_neg());
    }
    Ok(())
}

#[cfg(test)]
pub(super) mod tests {
    use super::*;

    /// Random records that reach every corner of the encoding.
    pub(in crate::omt) struct Lcg(pub(in crate::omt) u64);
    impl Lcg {
        fn next(&mut self) -> u64 {
            self.0 = self
                .0
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            self.0 >> 11
        }
        fn below(&mut self, bound: u64) -> u64 {
            self.next() % bound
        }
        fn word(&mut self) -> String {
            // Common words make repeated values likely.
            const COMMON: [&str; 3] = ["Kraków", "żółć żółć", "abc"];
            if self.below(2) == 0 {
                return COMMON[self.below(3) as usize].to_owned();
            }
            const PARTS: [&str; 8] = ["a", "é", "żółć", "名前", "🗺", "", "Ωmega", "x y"];
            (0..self.below(4))
                .map(|_| PARTS[self.below(8) as usize])
                .collect()
        }
        fn coordinate(&mut self) -> i32 {
            match self.below(6) {
                0 => i32::MIN,
                1 => i32::MAX,
                2 => -(self.below(300) as i32),
                3 => (self.below(1 << 24) as i32) << 4,
                _ => self.below(4097) as i32,
            }
        }
        fn path(&mut self) -> Vec<TilePoint> {
            let mut path: Vec<_> = (0..self.below(6))
                .map(|_| TilePoint::new(self.coordinate(), self.coordinate()))
                .collect();
            // Rings close on their first point, which splitting drops.
            if self.below(2) == 0
                && let Some(&first) = path.first()
            {
                path.push(first);
            }
            path
        }
        fn geometry(&mut self) -> TileGeometry {
            match self.below(4) {
                0 => TileGeometry::Point(self.path()),
                1 => TileGeometry::Line {
                    scale_exp: self.below(18) as u8,
                    paths: (0..self.below(4)).map(|_| self.path()).collect(),
                },
                2 => TileGeometry::Polygon(
                    (0..self.below(4))
                        .map(|_| TileRingGroup {
                            exterior: self.path(),
                            holes: (0..self.below(3)).map(|_| self.path()).collect(),
                        })
                        .collect(),
                ),
                _ => TileGeometry::FilledPolygon {
                    buffer_pixels: [i16::MIN, -1, 0, 4, i16::MAX][self.below(5) as usize],
                },
            }
        }
        fn double(&mut self) -> f64 {
            match self.below(8) {
                0 => f64::NAN,
                1 => f64::from_bits(0x7ff8_0000_0000_0001),
                2 => -0.0,
                3 => f64::INFINITY,
                4 => f64::NEG_INFINITY,
                5 => f64::MIN_POSITIVE / 2.0,
                _ => f64::from_bits(self.next() << 11 | self.next()),
            }
        }
        fn value(&mut self) -> AttrValue {
            match self.below(5) {
                0 => AttrValue::String(self.word()),
                1 => AttrValue::Int([i64::MIN, -1, 0, 7, i64::MAX][self.below(5) as usize]),
                2 => AttrValue::UInt([0, 127, 128, u64::MAX][self.below(4) as usize]),
                3 => AttrValue::Double(self.double()),
                _ => AttrValue::Bool(self.below(2) == 0),
            }
        }
        pub(in crate::omt) fn record(&mut self) -> Record {
            let mut attrs = AttrMap::new();
            for _ in 0..self.below(8) {
                let key: AttrKey = if self.below(3) == 0 {
                    self.word().into()
                } else {
                    ATTR_KEYS[self.below(ATTR_KEYS.len() as u64) as usize].into()
                };
                attrs.insert(key, self.value());
            }
            Record {
                tile_id: self.next(),
                zoom: self.below(256) as u8,
                id: self.next() << 11 | self.next(),
                layer: if self.below(4) == 0 {
                    self.word().into()
                } else {
                    LAYER_NAMES[self.below(16) as usize].into()
                },
                geometry: self.geometry(),
                attrs,
                sort_key: self.next() as i64 - (1 << 52),
                grid_size: if self.below(2) == 0 {
                    0.0
                } else {
                    self.double()
                },
                grid_cell: (self.below(2) == 0).then(|| (self.coordinate(), self.coordinate())),
                grid_limit: [0, 1, 300, usize::MAX][self.below(4) as usize],
                rank: self.below(2) == 0,
            }
        }
    }

    // Bitwise equality: NaN payloads and the sign of zero must survive.
    fn assert_same(a: &Record, b: &Record) {
        assert_eq!(
            (a.tile_id, a.zoom, a.id, &a.layer, &a.geometry, a.sort_key),
            (b.tile_id, b.zoom, b.id, &b.layer, &b.geometry, b.sort_key)
        );
        assert_eq!(a.grid_size.to_bits(), b.grid_size.to_bits());
        assert_eq!(
            (a.grid_cell, a.grid_limit, a.rank),
            (b.grid_cell, b.grid_limit, b.rank)
        );
        assert_eq!(a.attrs.len(), b.attrs.len());
        for ((ka, va), (kb, vb)) in a.attrs.iter().zip(&b.attrs) {
            assert_eq!(ka, kb);
            match (va, vb) {
                (AttrValue::Double(x), AttrValue::Double(y)) => {
                    assert_eq!(x.to_bits(), y.to_bits())
                }
                _ => assert_eq!(va, vb),
            }
        }
    }

    fn roundtrip(record: &Record) -> Vec<u8> {
        let mut bytes = vec![];
        encode_payload(record, &mut bytes);
        let decoded = decode_payload(
            &bytes,
            record.tile_id,
            record.layer.clone(),
            record.sort_key,
            record.id,
        )
        .unwrap();
        assert_same(record, &decoded);
        bytes
    }

    #[test]
    fn random_records_roundtrip_exactly() {
        let mut random = Lcg(0x5eed);
        for _ in 0..20_000 {
            roundtrip(&random.record());
        }
    }

    #[test]
    fn split_payloads_join_byte_for_byte() {
        let mut rng = Lcg(11);
        let payloads: Vec<Vec<u8>> = (0..3000).map(|_| roundtrip(&rng.record())).collect();
        let mut streams: [Vec<u8>; PAYLOAD_STREAMS] = Default::default();
        for payload in &payloads {
            split_payload(payload, &mut streams).unwrap();
        }
        let mut readers = streams.each_ref().map(|s| Reader::new(s));
        for payload in &payloads {
            let mut joined = vec![];
            join_payload(&mut readers, &mut joined).unwrap();
            assert_eq!(&joined, payload);
        }
        assert!(readers.iter().all(Reader::is_empty));
        // A closed ring keeps its first point only.
        let square = [(0, 0), (40, 0), (40, 40), (0, 40), (0, 0)];
        let mut ring = |points: &[(i32, i32)]| {
            let mut record = rng.record();
            record.geometry = TileGeometry::Polygon(vec![TileRingGroup {
                exterior: points.iter().map(|&(x, y)| TilePoint::new(x, y)).collect(),
                holes: vec![],
            }]);
            let mut streams: [Vec<u8>; PAYLOAD_STREAMS] = Default::default();
            split_payload(&roundtrip(&record), &mut streams).unwrap();
            streams[COORDS].len()
        };
        assert_eq!(ring(&square), 8);
        assert_eq!(ring(&square[..4]), 8);
        assert_eq!(ring(&[(5, 5), (5, 5)]), 2);
        // Damaged streams are refused, not misread.
        for stream in 0..PAYLOAD_STREAMS {
            let mut short = streams.clone();
            if short[stream].pop().is_some() {
                let mut readers = short.each_ref().map(|s| Reader::new(s));
                let joined =
                    (0..payloads.len()).try_for_each(|_| join_payload(&mut readers, &mut vec![]));
                assert!(joined.is_err(), "stream {stream}");
            }
        }
        assert!(
            split_payload(
                &payloads[0][..payloads[0].len() - 1],
                &mut Default::default()
            )
            .is_err()
        );
    }

    #[test]
    fn empty_and_extreme_records_roundtrip() {
        let mut record = Lcg(1).record();
        for geometry in [
            TileGeometry::Point(vec![]),
            TileGeometry::Line {
                scale_exp: 17,
                paths: vec![vec![], vec![TilePoint::new(i32::MIN, i32::MAX)]],
            },
            TileGeometry::Polygon(vec![
                TileRingGroup::default(),
                TileRingGroup {
                    exterior: vec![TilePoint::new(i32::MAX, i32::MIN), TilePoint::new(-5, -5)],
                    holes: vec![vec![], vec![TilePoint::new(i32::MIN, i32::MIN)]],
                },
            ]),
            TileGeometry::Polygon(vec![]),
        ] {
            record.geometry = geometry;
            roundtrip(&record);
        }
        record.attrs.clear();
        record.attrs.insert("".into(), AttrValue::Bool(true));
        record
            .attrs
            .insert("name".into(), AttrValue::String(String::new()));
        roundtrip(&record);
    }

    #[test]
    fn typical_records_are_compact() {
        let mut record = Lcg(1).record();
        record.geometry = TileGeometry::Polygon(vec![TileRingGroup {
            exterior: [(10, 10), (60, 10), (60, 50), (10, 50), (10, 10)]
                .map(|(x, y)| TilePoint::new(x, y))
                .to_vec(),
            holes: vec![],
        }]);
        record.attrs = AttrMap::from([
            ("render_height".into(), AttrValue::Int(9)),
            ("class".into(), AttrValue::String("building".into())),
        ]);
        record.grid_size = 0.0;
        record.grid_limit = 0;
        assert!(roundtrip(&record).len() < 45);
    }

    #[test]
    fn equal_strings_are_stored_once_within_the_window() {
        let mut record = Lcg(1).record();
        record.attrs = ["name", "name:latin", "name_en", "name_int"]
            .into_iter()
            .map(|key| (key.into(), AttrValue::String("Kraków".into())))
            .collect();
        let repeated = roundtrip(&record);
        record
            .attrs
            .insert("name_en".into(), AttrValue::String("Cracow".into()));
        assert!(roundtrip(&record).len() > repeated.len());
        // A repeat further back than the window, and a short one, are verbatim.
        record.attrs = (0..12)
            .map(|i| {
                (
                    format!("k{i:02}").into(),
                    AttrValue::String(format!("value {}", i % 10)),
                )
            })
            .collect();
        record
            .attrs
            .insert("s1".into(), AttrValue::String("ab".into()));
        record
            .attrs
            .insert("s2".into(), AttrValue::String("ab".into()));
        roundtrip(&record);
    }

    #[test]
    fn corrupt_payloads_are_rejected() {
        let mut random = Lcg(7);
        for _ in 0..200 {
            let record = random.record();
            let mut bytes = vec![];
            encode_payload(&record, &mut bytes);
            let decode = |bytes: &[u8]| {
                decode_payload(
                    bytes,
                    record.tile_id,
                    record.layer.clone(),
                    record.sort_key,
                    record.id,
                )
            };
            for end in 0..bytes.len() {
                assert!(decode(&bytes[..end]).is_err(), "prefix {end}");
            }
            bytes.push(0);
            assert!(decode(&bytes).is_err());
        }
        // Counts beyond the remaining bytes must not allocate or panic.
        let huge = [
            0, 0, 0, 1, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x01,
        ];
        assert!(decode_payload(&huge, 0, "poi".into(), 0, 0).is_err());
    }

    #[test]
    fn tables_are_sorted_and_fit_one_byte() {
        assert!(LAYER_NAMES.windows(2).all(|pair| pair[0] < pair[1]));
        let mut known = LAYER_NAMES;
        known.sort_unstable();
        let mut layers = super::super::LAYERS;
        layers.sort_unstable();
        assert_eq!(known, layers);
        assert!(ATTR_KEYS.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(ATTR_KEYS.len() < usize::from(u8::MAX));
        for (id, name) in LAYER_NAMES.iter().enumerate() {
            assert_eq!(layer_id(name), Some(id as u8));
        }
        assert_eq!(layer_id("not a layer"), None);
    }

    #[test]
    fn varints_roundtrip() {
        for value in [0, 1, 127, 128, 16383, 16384, u64::MAX / 2, u64::MAX] {
            let mut bytes = vec![];
            put_varint(&mut bytes, value);
            assert_eq!(Reader::new(&bytes).varint().unwrap(), value);
        }
        for value in [0, -1, 1, i64::MIN, i64::MAX] {
            let mut bytes = vec![];
            put_zigzag(&mut bytes, value);
            assert_eq!(Reader::new(&bytes).zigzag().unwrap(), value);
        }
        assert!(Reader::new(&[0x80; 11]).varint().is_err());
        assert!(
            Reader::new(&[0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x02])
                .varint()
                .is_err()
        );
    }
}
