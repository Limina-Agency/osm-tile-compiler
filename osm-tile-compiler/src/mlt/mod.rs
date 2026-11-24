//! MapLibre Tile (MLT) output: each finished MVT layer transcoded to an MLT
//! v1 layer (frame tag `0x01`).
//!
//! The encoder is this project's own rather than `mlt-core`'s, whose API
//! changed in four minor versions in a month and which needs a newer Rust
//! than this crate's `rust-version`. It writes only the stable v1 layout
//! (`docs/specification/v1.md` of maplibre/maplibre-tile-spec) and only
//! encodings every v1 decoder reads: plain or run-length integers in
//! varints, componentwise-delta vertices, ORC byte-RLE booleans, and plain or
//! dictionary strings. No FastPFOR, FSST, Morton, shared dictionaries or
//! tessellation, which would make tiles smaller but give decoders more to
//! get wrong.
//!
//! Each layer is transcoded from the MVT bytes the compiler already wrote,
//! so the MLT tile holds exactly what the MVT tile does: the same features
//! in the same order, the same rounded coordinates and ring winding, the same
//! ids and attributes. Planetiler parity is checked on MVT; MLT follows it.

pub(crate) mod decode;

use fast_mvt::{MvtReaderRef, MvtValueRef};
use std::collections::HashMap;

use decode::TAG_V1;

/// Column types (`v1.md`, Column Types); the nullable variant is `+ 1`.
mod column {
    pub const ID: u8 = 0;
    pub const LONG_ID: u8 = 2;
    pub const GEOMETRY: u8 = 4;
    pub const BOOL: u8 = 10;
    pub const INT32: u8 = 16;
    pub const UINT32: u8 = 18;
    pub const INT64: u8 = 20;
    pub const UINT64: u8 = 22;
    pub const FLOAT: u8 = 24;
    pub const DOUBLE: u8 = 26;
    pub const STRING: u8 = 28;
}

/// Stream types: category in the high nibble, subtype in the low one.
mod stream {
    pub const PRESENT: u8 = 0x00;
    pub const DATA: u8 = 0x10;
    pub const DATA_SINGLE_DICTIONARY: u8 = 0x11;
    pub const DATA_VERTEX: u8 = 0x13;
    pub const OFFSET_STRING: u8 = 0x22;
    /// The geometry types stream. The specification's table names it
    /// `Data/None`, but every encoder writes, and every decoder takes, the
    /// first stream of a geometry column by position as `Length/VarBinary`.
    pub const LENGTH_VAR_BINARY: u8 = 0x30;
    pub const LENGTH_GEOMETRIES: u8 = 0x31;
    pub const LENGTH_PARTS: u8 = 0x32;
    pub const LENGTH_RINGS: u8 = 0x33;
    pub const LENGTH_DICTIONARY: u8 = 0x36;
}

/// Encoding bytes: logical1 (bits 7-5), logical2 (bits 4-2), physical (1-0).
mod encoding {
    /// Fixed-width little-endian words (floats), or bytes (strings).
    pub const RAW: u8 = 0x00;
    pub const VARINT: u8 = 0x02;
    pub const DELTA_VARINT: u8 = 0x22;
    pub const DELTA_RLE_VARINT: u8 = 0x2e;
    pub const COMPONENTWISE_DELTA_VARINT: u8 = 0x42;
    pub const RLE_VARINT: u8 = 0x62;
    /// Booleans: the bitmap in ORC byte runs.
    pub const BOOL_RLE: u8 = 0x60;
}

/// The geometry types, as MLT numbers them.
mod geometry_type {
    pub const POINT: u8 = 0;
    pub const LINE_STRING: u8 = 1;
    pub const POLYGON: u8 = 2;
    pub const MULTI_POINT: u8 = 3;
    pub const MULTI_LINE_STRING: u8 = 4;
    pub const MULTI_POLYGON: u8 = 5;
}

/// Transcodes an uncompressed MVT tile to MLT: one v1 frame per layer that
/// keeps a feature, in the tile's layer order. Layers are encoded
/// independently, so the frames of separately transcoded layers can be
/// concatenated as MVT layers can.
pub(crate) fn from_mvt(tile: &[u8]) -> Result<Vec<u8>, String> {
    let reader = MvtReaderRef::new(tile).map_err(|e| format!("not an MVT tile: {e}"))?;
    let mut out = Vec::with_capacity(tile.len());
    for layer in reader.layers() {
        let name = layer.name();
        let extent = layer.extent();
        let mut features = Vec::with_capacity(layer.feature_count());
        for feature in layer.features() {
            let Some(geometry) = Geometry::from_commands(
                feature.geom_type_value().unwrap_or(0),
                feature.geometry_commands(),
            )
            .map_err(|e| format!("layer {name}: {e}"))?
            else {
                continue;
            };
            let properties = feature
                .properties()
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| format!("layer {name}: {e}"))?;
            features.push(Feature {
                id: feature.id(),
                geometry,
                properties,
            });
        }
        if features.is_empty() {
            continue;
        }
        write_frame(&mut out, &encode_layer(name, extent, &features)?);
    }
    Ok(out)
}

/// Transcodes an uncompressed MLT tile back to MVT: one MVT layer per v1
/// frame, in order, with the features' ids, geometries and attributes. What
/// reads a basemap's tiles (the search index, building levels) reads an MLT
/// archive through it, so it sees the MVT tile the MLT one was made from:
/// [`from_mvt`] keeps every feature, coordinate, ring and attribute, and a
/// column's values keep their kinds unless one column mixes kinds.
pub(crate) fn to_mvt(tile: &[u8]) -> Result<Vec<u8>, String> {
    use decode::{Geometry as G, Value};
    use fast_mvt::{MvtCoord, MvtGeomType, MvtLayerBuilder};

    fn coords(path: &[Vertex]) -> impl Iterator<Item = MvtCoord> + '_ {
        path.iter().map(|&[x, y]| MvtCoord { x, y })
    }
    let failed = |e: fast_mvt::MvtError| format!("Failed to write an MVT tile: {e}");
    let mut out = Vec::with_capacity(tile.len() * 2);
    for layer in decode::decode(tile)? {
        let mut builder = MvtLayerBuilder::new(layer.name).map_err(failed)?;
        let extent = std::num::NonZeroU32::new(layer.extent).ok_or("an MLT layer of extent 0")?;
        builder.extent(extent);
        for feature in &layer.features {
            let kind = match feature.geometry {
                G::Point(_) | G::MultiPoint(_) => MvtGeomType::Point,
                G::LineString(_) | G::MultiLineString(_) => MvtGeomType::LineString,
                G::Polygon(_) | G::MultiPolygon(_) => MvtGeomType::Polygon,
            };
            let mut mvt = builder.feature_of(kind);
            mvt.id(feature.id);
            match &feature.geometry {
                G::Point(point) => {
                    mvt.points(coords(std::slice::from_ref(point)))
                        .map_err(failed)?;
                }
                G::MultiPoint(points) => {
                    mvt.points(coords(points)).map_err(failed)?;
                }
                G::LineString(line) => {
                    mvt.line(coords(line)).map_err(failed)?;
                }
                G::MultiLineString(lines) => {
                    for line in lines {
                        mvt.line(coords(line)).map_err(failed)?;
                    }
                }
                G::Polygon(rings) => {
                    for (i, ring) in rings.iter().enumerate() {
                        mvt.ring(coords(ring), i == 0).map_err(failed)?;
                    }
                }
                G::MultiPolygon(polygons) => {
                    for rings in polygons {
                        for (i, ring) in rings.iter().enumerate() {
                            mvt.ring(coords(ring), i == 0).map_err(failed)?;
                        }
                    }
                }
            }
            for (key, value) in &feature.properties {
                let value = match value {
                    Value::Bool(v) => MvtValueRef::Bool(*v),
                    Value::Int(v) => MvtValueRef::Int(*v),
                    Value::UInt(v) => MvtValueRef::UInt(*v),
                    Value::Float(v) => MvtValueRef::Float(*v),
                    Value::Double(v) => MvtValueRef::Double(*v),
                    Value::String(v) => MvtValueRef::String(v),
                };
                mvt.tag_ref(key, value).map_err(failed)?;
            }
            builder = mvt.end();
        }
        out.extend(builder.encode());
    }
    Ok(out)
}

struct Feature<'a> {
    id: Option<u64>,
    geometry: Geometry,
    properties: Vec<(&'a str, MvtValueRef<'a>)>,
}

use decode::Vertex;

/// A feature's geometry as MLT models it: rings without their closing
/// vertex, polygons as an exterior followed by its holes.
#[derive(Debug, Clone, PartialEq)]
enum Geometry {
    Point(Vertex),
    MultiPoint(Vec<Vertex>),
    LineString(Vec<Vertex>),
    MultiLineString(Vec<Vec<Vertex>>),
    Polygon(Vec<Vec<Vertex>>),
    MultiPolygon(Vec<Vec<Vec<Vertex>>>),
}

impl Geometry {
    /// The geometry an MVT feature's commands draw, or `None` when they draw
    /// nothing.
    fn from_commands(
        mvt_type: i32,
        commands: impl Iterator<Item = u32>,
    ) -> Result<Option<Self>, String> {
        let mut commands = commands.peekable();
        let mut cursor = [0i32; 2];
        let mut paths: Vec<Vec<Vertex>> = Vec::new();
        let mut closed = Vec::new();
        while let Some(command) = commands.next() {
            let (id, count) = (command & 7, command >> 3);
            match id {
                1 | 2 => {
                    if id == 1 {
                        paths.push(Vec::with_capacity(count as usize));
                        closed.push(false);
                    }
                    let path = paths
                        .last_mut()
                        .ok_or("an MVT LineTo comes before any MoveTo")?;
                    for _ in 0..count {
                        let (Some(dx), Some(dy)) = (commands.next(), commands.next()) else {
                            return Err("MVT geometry ends inside a command".into());
                        };
                        cursor[0] = cursor[0].wrapping_add(unzigzag32(dx));
                        cursor[1] = cursor[1].wrapping_add(unzigzag32(dy));
                        path.push(cursor);
                    }
                }
                7 => {
                    *closed
                        .last_mut()
                        .ok_or("an MVT ClosePath comes before any MoveTo")? = true;
                }
                _ => return Err(format!("unknown MVT command {id}")),
            }
        }
        paths.retain(|path| !path.is_empty());
        if paths.is_empty() {
            return Ok(None);
        }
        Ok(Some(match mvt_type {
            1 => {
                let mut points = paths.concat();
                if points.len() == 1 {
                    Self::Point(points.remove(0))
                } else {
                    Self::MultiPoint(points)
                }
            }
            2 => {
                if paths.len() == 1 {
                    Self::LineString(paths.remove(0))
                } else {
                    Self::MultiLineString(paths)
                }
            }
            3 => {
                // An exterior ring has a positive area and a hole a negative
                // one, in MVT and MLT alike; a ring of no area stays with the
                // polygon before it.
                let mut polygons: Vec<Vec<Vec<Vertex>>> = Vec::new();
                for ring in paths {
                    match polygons.last_mut() {
                        Some(polygon) if signed_area(&ring) <= 0 => polygon.push(ring),
                        _ => polygons.push(vec![ring]),
                    }
                }
                if polygons.len() == 1 {
                    Self::Polygon(polygons.remove(0))
                } else {
                    Self::MultiPolygon(polygons)
                }
            }
            other => return Err(format!("MVT geometry type {other} is not one MLT has")),
        }))
    }

    fn type_code(&self) -> u8 {
        match self {
            Self::Point(_) => geometry_type::POINT,
            Self::LineString(_) => geometry_type::LINE_STRING,
            Self::Polygon(_) => geometry_type::POLYGON,
            Self::MultiPoint(_) => geometry_type::MULTI_POINT,
            Self::MultiLineString(_) => geometry_type::MULTI_LINE_STRING,
            Self::MultiPolygon(_) => geometry_type::MULTI_POLYGON,
        }
    }
}

/// Twice the ring's signed area, positive for an MVT exterior ring.
fn signed_area(ring: &[Vertex]) -> i64 {
    let mut area = 0i64;
    for (i, a) in ring.iter().enumerate() {
        let b = ring[(i + 1) % ring.len()];
        area += i64::from(a[0]) * i64::from(b[1]) - i64::from(b[0]) * i64::from(a[1]);
    }
    area
}

fn encode_layer(name: &str, extent: u32, features: &[Feature]) -> Result<Vec<u8>, String> {
    let mut meta = Vec::new();
    let mut data = Vec::new();
    let mut columns = 0u64;

    if let Some(ids) = encode_ids(features, &mut meta, &mut data) {
        columns += ids;
    }
    meta.push(column::GEOMETRY);
    encode_geometry(features, &mut data)?;
    columns += 1;

    // Property columns in the order their keys first appear.
    let mut keys: Vec<&str> = Vec::new();
    let mut index: HashMap<&str, usize> = HashMap::new();
    for feature in features {
        for (key, _) in &feature.properties {
            index.entry(key).or_insert_with(|| {
                keys.push(key);
                keys.len() - 1
            });
        }
    }
    let mut values: Vec<Vec<Option<MvtValueRef>>> = vec![vec![None; features.len()]; keys.len()];
    for (row, feature) in features.iter().enumerate() {
        for (key, value) in &feature.properties {
            if !matches!(value, MvtValueRef::Null) {
                values[index[key]][row] = Some(*value);
            }
        }
    }
    for (key, column) in keys.iter().zip(&values) {
        if column.iter().all(Option::is_none) {
            continue;
        }
        encode_property(key, column, &mut meta, &mut data);
        columns += 1;
    }

    let mut body = Vec::with_capacity(name.len() + meta.len() + data.len() + 8);
    put_string(&mut body, name);
    put_varint(&mut body, u64::from(extent));
    put_varint(&mut body, columns);
    body.extend_from_slice(&meta);
    body.extend_from_slice(&data);
    Ok(body)
}

fn write_frame(out: &mut Vec<u8>, body: &[u8]) {
    put_varint(out, body.len() as u64 + 1);
    out.push(TAG_V1);
    out.extend_from_slice(body);
}

/// Writes the id column, if any feature has an id, and returns 1 then.
fn encode_ids(features: &[Feature], meta: &mut Vec<u8>, data: &mut Vec<u8>) -> Option<u64> {
    let ids = features.iter().filter_map(|f| f.id).collect::<Vec<_>>();
    if ids.is_empty() {
        return None;
    }
    let nullable = ids.len() < features.len();
    let long = ids.iter().any(|&id| id > u64::from(u32::MAX));
    let kind = if long { column::LONG_ID } else { column::ID };
    meta.push(kind + u8::from(nullable));
    if nullable {
        let present = features.iter().map(|f| f.id.is_some()).collect::<Vec<_>>();
        bool_stream(data, stream::PRESENT, &present);
    }
    int_stream(data, stream::DATA, &ids, if long { 64 } else { 32 }, true);
    Some(1)
}

fn encode_geometry(features: &[Feature], data: &mut Vec<u8>) -> Result<(), String> {
    let types = features
        .iter()
        .map(|f| u64::from(f.geometry.type_code()))
        .collect::<Vec<_>>();
    let has_polygons = features
        .iter()
        .any(|f| matches!(f.geometry, Geometry::Polygon(_) | Geometry::MultiPolygon(_)));
    // Counts are stored only where an item can hold more than one: multi
    // geometries their parts, polygons their rings, rings and lines their
    // vertices. With polygons in the column, line vertex counts go in the
    // rings stream, beside the polygons' ring lengths; without, in the
    // parts stream.
    let (mut geometries, mut parts, mut rings) = (Vec::new(), Vec::new(), Vec::new());
    let mut vertices: Vec<Vertex> = Vec::new();
    let line = |line: &[Vertex], parts: &mut Vec<u64>, rings: &mut Vec<u64>| {
        let lengths = if has_polygons { rings } else { parts };
        lengths.push(line.len() as u64);
    };
    let polygon = |polygon: &[Vec<Vertex>], parts: &mut Vec<u64>, rings: &mut Vec<u64>| {
        parts.push(polygon.len() as u64);
        rings.extend(polygon.iter().map(|ring| ring.len() as u64));
    };
    for feature in features {
        match &feature.geometry {
            Geometry::Point(point) => vertices.push(*point),
            Geometry::MultiPoint(points) => {
                geometries.push(points.len() as u64);
                vertices.extend_from_slice(points);
            }
            Geometry::LineString(path) => {
                line(path, &mut parts, &mut rings);
                vertices.extend_from_slice(path);
            }
            Geometry::MultiLineString(paths) => {
                geometries.push(paths.len() as u64);
                for path in paths {
                    line(path, &mut parts, &mut rings);
                    vertices.extend_from_slice(path);
                }
            }
            Geometry::Polygon(shape) => {
                polygon(shape, &mut parts, &mut rings);
                vertices.extend(shape.iter().flatten());
            }
            Geometry::MultiPolygon(shapes) => {
                geometries.push(shapes.len() as u64);
                for shape in shapes {
                    polygon(shape, &mut parts, &mut rings);
                    vertices.extend(shape.iter().flatten());
                }
            }
        }
    }
    let streams = 2
        + u64::from(!geometries.is_empty())
        + u64::from(!parts.is_empty())
        + u64::from(!rings.is_empty());
    put_varint(data, streams);
    int_stream(data, stream::LENGTH_VAR_BINARY, &types, 32, false);
    for (kind, lengths) in [
        (stream::LENGTH_GEOMETRIES, &geometries),
        (stream::LENGTH_PARTS, &parts),
        (stream::LENGTH_RINGS, &rings),
    ] {
        if !lengths.is_empty() {
            if lengths.iter().any(|&n| n > u64::from(u32::MAX)) {
                return Err("an MLT geometry has more than 2^32 parts".into());
            }
            int_stream(data, kind, lengths, 32, false);
        }
    }
    vertex_stream(data, &vertices);
    Ok(())
}

/// The type a property column is written as: the narrowest that holds every
/// value its key has in the layer. A key with values of different kinds
/// becomes a double column when they are all numbers, else a string column,
/// as Planetiler's MLT encoder coerces them.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Bool,
    Int32,
    UInt32,
    Int64,
    UInt64,
    Float,
    Double,
    String,
}

fn column_kind(values: &[Option<MvtValueRef>]) -> Kind {
    let (mut bools, mut ints, mut floats, mut doubles, mut strings) = (0, 0, 0, 0, 0);
    let (mut min, mut max) = (i128::MAX, i128::MIN);
    for value in values.iter().flatten() {
        match *value {
            MvtValueRef::Bool(_) => bools += 1,
            MvtValueRef::Int(v) | MvtValueRef::SInt(v) => {
                ints += 1;
                min = min.min(i128::from(v));
                max = max.max(i128::from(v));
            }
            MvtValueRef::UInt(v) => {
                ints += 1;
                min = min.min(i128::from(v));
                max = max.max(i128::from(v));
            }
            MvtValueRef::Float(_) => floats += 1,
            MvtValueRef::Double(_) => doubles += 1,
            MvtValueRef::String(_) => strings += 1,
            MvtValueRef::Null => {}
        }
    }
    let total = bools + ints + floats + doubles + strings;
    if strings > 0 || (bools > 0 && bools < total) {
        return Kind::String;
    }
    if bools == total {
        return Kind::Bool;
    }
    if ints == total {
        return if min >= i128::from(i32::MIN) && max <= i128::from(i32::MAX) {
            Kind::Int32
        } else if min >= 0 && max <= i128::from(u32::MAX) {
            Kind::UInt32
        } else if min >= i128::from(i64::MIN) && max <= i128::from(i64::MAX) {
            Kind::Int64
        } else if min >= 0 {
            Kind::UInt64
        } else {
            Kind::Double
        };
    }
    if floats == total {
        return Kind::Float;
    }
    Kind::Double
}

fn as_f64(value: MvtValueRef) -> f64 {
    match value {
        MvtValueRef::Int(v) | MvtValueRef::SInt(v) => v as f64,
        MvtValueRef::UInt(v) => v as f64,
        MvtValueRef::Float(v) => f64::from(v),
        MvtValueRef::Double(v) => v,
        MvtValueRef::Bool(v) => f64::from(u8::from(v)),
        MvtValueRef::String(_) | MvtValueRef::Null => f64::NAN,
    }
}

fn as_i128(value: MvtValueRef) -> i128 {
    match value {
        MvtValueRef::Int(v) | MvtValueRef::SInt(v) => i128::from(v),
        MvtValueRef::UInt(v) => i128::from(v),
        _ => 0,
    }
}

/// A value as text, for a column of mixed kinds that is written as strings.
fn as_text(value: MvtValueRef<'_>) -> std::borrow::Cow<'_, str> {
    match value {
        MvtValueRef::String(v) => v.into(),
        MvtValueRef::Int(v) | MvtValueRef::SInt(v) => v.to_string().into(),
        MvtValueRef::UInt(v) => v.to_string().into(),
        MvtValueRef::Float(v) => v.to_string().into(),
        MvtValueRef::Double(v) => v.to_string().into(),
        MvtValueRef::Bool(v) => v.to_string().into(),
        MvtValueRef::Null => "".into(),
    }
}

fn encode_property(
    name: &str,
    values: &[Option<MvtValueRef>],
    meta: &mut Vec<u8>,
    data: &mut Vec<u8>,
) {
    let kind = column_kind(values);
    // String columns are always nullable: the C++ decoder in MapLibre Native
    // 6.4 rejects a string column without a present stream.
    let nullable = kind == Kind::String || values.iter().any(Option::is_none);
    let code = match kind {
        Kind::Bool => column::BOOL,
        Kind::Int32 => column::INT32,
        Kind::UInt32 => column::UINT32,
        Kind::Int64 => column::INT64,
        Kind::UInt64 => column::UINT64,
        Kind::Float => column::FLOAT,
        Kind::Double => column::DOUBLE,
        Kind::String => column::STRING,
    };
    meta.push(code + u8::from(nullable));
    put_string(meta, name);

    let present = values.iter().flatten().copied().collect::<Vec<_>>();
    if kind == Kind::String {
        let strings = present.iter().map(|v| as_text(*v)).collect::<Vec<_>>();
        let mut streams = Vec::new();
        let count = string_streams(&mut streams, &strings);
        put_varint(data, count + u64::from(nullable));
        if nullable {
            present_stream(data, values);
        }
        data.extend_from_slice(&streams);
        return;
    }
    if nullable {
        present_stream(data, values);
    }
    match kind {
        Kind::Bool => {
            let bits = present
                .iter()
                .map(|v| matches!(v, MvtValueRef::Bool(true)))
                .collect::<Vec<_>>();
            bool_stream(data, stream::DATA, &bits);
        }
        Kind::Int32 | Kind::Int64 => {
            let width = if kind == Kind::Int32 { 32 } else { 64 };
            let words = present
                .iter()
                .map(|v| zigzag64(as_i128(*v) as i64))
                .collect::<Vec<_>>();
            signed_int_stream(data, &words, &present, width);
        }
        Kind::UInt32 | Kind::UInt64 => {
            let width = if kind == Kind::UInt32 { 32 } else { 64 };
            let words = present
                .iter()
                .map(|v| as_i128(*v) as u64)
                .collect::<Vec<_>>();
            int_stream(data, stream::DATA, &words, width, true);
        }
        Kind::Float => {
            data.push(stream::DATA);
            data.push(encoding::RAW);
            put_varint(data, present.len() as u64);
            put_varint(data, present.len() as u64 * 4);
            for value in &present {
                data.extend_from_slice(&(as_f64(*value) as f32).to_le_bytes());
            }
        }
        Kind::Double => {
            data.push(stream::DATA);
            data.push(encoding::RAW);
            put_varint(data, present.len() as u64);
            put_varint(data, present.len() as u64 * 8);
            for value in &present {
                data.extend_from_slice(&as_f64(*value).to_le_bytes());
            }
        }
        Kind::String => unreachable!("written above"),
    }
}

/// A signed integer column. Plain and run-length words are zigzagged; delta
/// words are the zigzagged differences of the values themselves.
fn signed_int_stream(data: &mut Vec<u8>, zigzagged: &[u64], values: &[MvtValueRef], width: u32) {
    let plain = IntCandidates::new(zigzagged, width, false);
    let raw = values
        .iter()
        .map(|v| as_i128(*v) as u64)
        .collect::<Vec<_>>();
    let delta = IntCandidates::new(&raw, width, true);
    let best = plain.best_plain().min_by_key(|c| c.size()).into_iter();
    let best = best
        .chain(delta.best_delta())
        .min_by_key(|c| c.size())
        .expect("a plain candidate");
    best.write(data, stream::DATA);
}

/// Writes the strings as the plainer or the dictionary layout, whichever is
/// smaller, and returns how many streams that is.
fn string_streams(data: &mut Vec<u8>, strings: &[std::borrow::Cow<str>]) -> u64 {
    let mut plain = Vec::new();
    let lengths = strings.iter().map(|s| s.len() as u64).collect::<Vec<_>>();
    int_stream(&mut plain, stream::LENGTH_VAR_BINARY, &lengths, 32, false);
    raw_bytes_stream(
        &mut plain,
        stream::DATA,
        strings.len(),
        strings.iter().map(|s| s.as_bytes()),
    );

    let mut codes = Vec::with_capacity(strings.len());
    let mut distinct: Vec<&str> = Vec::new();
    let mut seen: HashMap<&str, u64> = HashMap::new();
    for string in strings {
        let code = *seen.entry(string).or_insert_with(|| {
            distinct.push(string);
            distinct.len() as u64 - 1
        });
        codes.push(code);
    }
    if distinct.len() < strings.len() {
        let mut dictionary = Vec::new();
        let lengths = distinct.iter().map(|s| s.len() as u64).collect::<Vec<_>>();
        int_stream(
            &mut dictionary,
            stream::LENGTH_DICTIONARY,
            &lengths,
            32,
            false,
        );
        int_stream(&mut dictionary, stream::OFFSET_STRING, &codes, 32, false);
        raw_bytes_stream(
            &mut dictionary,
            stream::DATA_SINGLE_DICTIONARY,
            distinct.len(),
            distinct.iter().map(|s| s.as_bytes()),
        );
        if dictionary.len() < plain.len() {
            data.extend_from_slice(&dictionary);
            return 3;
        }
    }
    data.extend_from_slice(&plain);
    2
}

fn raw_bytes_stream<'a>(
    data: &mut Vec<u8>,
    kind: u8,
    count: usize,
    values: impl Iterator<Item = &'a [u8]> + Clone,
) {
    data.push(kind);
    data.push(encoding::RAW);
    put_varint(data, count as u64);
    put_varint(data, values.clone().map(<[u8]>::len).sum::<usize>() as u64);
    for value in values {
        data.extend_from_slice(value);
    }
}

fn present_stream(data: &mut Vec<u8>, values: &[Option<MvtValueRef>]) {
    let present = values.iter().map(Option::is_some).collect::<Vec<_>>();
    bool_stream(data, stream::PRESENT, &present);
}

/// A boolean stream: the LSB-first bitmap in ORC byte runs.
fn bool_stream(data: &mut Vec<u8>, kind: u8, bits: &[bool]) {
    let mut bitmap = vec![0u8; bits.len().div_ceil(8)];
    for (i, _) in bits.iter().enumerate().filter(|(_, bit)| **bit) {
        bitmap[i / 8] |= 1 << (i % 8);
    }
    let payload = byte_rle(&bitmap);
    data.push(kind);
    data.push(encoding::BOOL_RLE);
    put_varint(data, bits.len() as u64);
    put_varint(data, payload.len() as u64);
    data.extend_from_slice(&payload);
}

/// ORC byte run-length encoding: repeated runs of 3 to 130 bytes, literal
/// runs of 1 to 128.
fn byte_rle(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len() + bytes.len() / 128 + 2);
    let mut literals: Vec<u8> = Vec::new();
    let flush = |out: &mut Vec<u8>, literals: &mut Vec<u8>| {
        for chunk in literals.chunks(128) {
            out.push((256 - chunk.len()) as u8);
            out.extend_from_slice(chunk);
        }
        literals.clear();
    };
    let mut at = 0;
    while at < bytes.len() {
        let byte = bytes[at];
        let run = bytes[at..]
            .iter()
            .take(130)
            .take_while(|&&b| b == byte)
            .count();
        if run >= 3 {
            flush(&mut out, &mut literals);
            out.push((run - 3) as u8);
            out.push(byte);
            at += run;
        } else {
            literals.push(byte);
            at += 1;
        }
    }
    flush(&mut out, &mut literals);
    out
}

/// The vertices, interleaved x and y, as componentwise zigzag deltas.
fn vertex_stream(data: &mut Vec<u8>, vertices: &[Vertex]) {
    let mut payload = Vec::with_capacity(vertices.len() * 3);
    let mut previous = [0i32; 2];
    for vertex in vertices {
        for axis in 0..2 {
            let delta = vertex[axis].wrapping_sub(previous[axis]);
            put_varint(&mut payload, u64::from(zigzag32(delta)));
        }
        previous = *vertex;
    }
    data.push(stream::DATA_VERTEX);
    data.push(encoding::COMPONENTWISE_DELTA_VARINT);
    put_varint(data, vertices.len() as u64 * 2);
    put_varint(data, payload.len() as u64);
    data.extend_from_slice(&payload);
}

/// Writes unsigned (or already zigzagged) words in the smallest of the
/// plain and run-length encodings, and with `delta` the delta ones too.
fn int_stream(data: &mut Vec<u8>, kind: u8, words: &[u64], width: u32, delta: bool) {
    let candidates = IntCandidates::new(words, width, delta);
    let best = candidates
        .best_plain()
        .chain(if delta { candidates.best_delta() } else { None })
        .min_by_key(|c| c.size())
        .expect("a plain candidate");
    best.write(data, kind);
}

/// One way of writing an integer stream.
struct Candidate {
    encoding: u8,
    num_values: u64,
    /// `runs` and `num_rle_values`, for the run-length encodings.
    rle: Option<(u64, u64)>,
    payload: Vec<u8>,
}

impl Candidate {
    fn size(&self) -> usize {
        let header = 2
            + varint_len(self.num_values)
            + varint_len(self.payload.len() as u64)
            + self
                .rle
                .map_or(0, |(runs, values)| varint_len(runs) + varint_len(values));
        header + self.payload.len()
    }

    fn write(&self, data: &mut Vec<u8>, kind: u8) {
        data.push(kind);
        data.push(self.encoding);
        put_varint(data, self.num_values);
        put_varint(data, self.payload.len() as u64);
        if let Some((runs, values)) = self.rle {
            put_varint(data, runs);
            put_varint(data, values);
        }
        data.extend_from_slice(&self.payload);
    }

    fn plain(encoding: u8, words: &[u64]) -> Self {
        let mut payload = Vec::with_capacity(words.len());
        for &word in words {
            put_varint(&mut payload, word);
        }
        Self {
            encoding,
            num_values: words.len() as u64,
            rle: None,
            payload,
        }
    }

    /// All run lengths, then all values.
    fn rle(encoding: u8, words: &[u64]) -> Self {
        let mut runs: Vec<u64> = Vec::new();
        let mut values: Vec<u64> = Vec::new();
        for &word in words {
            match (values.last(), runs.last_mut()) {
                (Some(&last), Some(run)) if last == word => *run += 1,
                _ => {
                    runs.push(1);
                    values.push(word);
                }
            }
        }
        let mut payload = Vec::new();
        for &word in runs.iter().chain(&values) {
            put_varint(&mut payload, word);
        }
        Self {
            encoding,
            num_values: runs.len() as u64 * 2,
            rle: Some((runs.len() as u64, words.len() as u64)),
            payload,
        }
    }
}

struct IntCandidates<'a> {
    words: &'a [u64],
    /// The zigzagged differences, wrapping at the word width.
    deltas: Option<Vec<u64>>,
}

impl<'a> IntCandidates<'a> {
    fn new(words: &'a [u64], width: u32, delta: bool) -> Self {
        let deltas = delta.then(|| {
            let mut previous = 0u64;
            words
                .iter()
                .map(|&word| {
                    let difference = word.wrapping_sub(previous);
                    previous = word;
                    if width == 32 {
                        u64::from(zigzag32(difference as u32 as i32))
                    } else {
                        zigzag64(difference as i64)
                    }
                })
                .collect()
        });
        Self { words, deltas }
    }

    /// The plain and run-length encodings of the words.
    fn best_plain(&self) -> impl Iterator<Item = Candidate> {
        let plain = Candidate::plain(encoding::VARINT, self.words);
        let rle = rle_pays(self.words).then(|| Candidate::rle(encoding::RLE_VARINT, self.words));
        std::iter::once(plain).chain(rle)
    }

    /// The smaller of the delta and delta run-length encodings.
    fn best_delta(&self) -> Option<Candidate> {
        let deltas = self.deltas.as_ref()?;
        let plain = Candidate::plain(encoding::DELTA_VARINT, deltas);
        let rle = rle_pays(deltas).then(|| Candidate::rle(encoding::DELTA_RLE_VARINT, deltas));
        std::iter::once(plain)
            .chain(rle)
            .min_by_key(Candidate::size)
    }
}

/// Whether the words make fewer runs than half their count, so that
/// run-length encoding has at most as many words as values. MapLibre Native
/// 6.4's C++ decoder rejects a nullable column whose run-length stream is
/// longer than the values it holds.
fn rle_pays(words: &[u64]) -> bool {
    let runs = 1 + words.windows(2).filter(|pair| pair[0] != pair[1]).count();
    words.len() >= 2 && runs * 2 <= words.len()
}

fn zigzag32(value: i32) -> u32 {
    ((value << 1) ^ (value >> 31)) as u32
}

fn unzigzag32(word: u32) -> i32 {
    ((word >> 1) as i32) ^ -((word & 1) as i32)
}

fn zigzag64(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn varint_len(value: u64) -> usize {
    (64 - value.max(1).leading_zeros() as usize).div_ceil(7)
}

fn put_string(out: &mut Vec<u8>, value: &str) {
    put_varint(out, value.len() as u64);
    out.extend_from_slice(value.as_bytes());
}

#[cfg(test)]
mod tests;
