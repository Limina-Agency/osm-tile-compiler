//! An MLT v1 decoder. `verify` decodes every tile of an MLT archive with it,
//! and the tests decode the encoder's output with it. It reads what the
//! encoder writes and every v1 encoding but FastPFOR, FSST, Morton vertices,
//! shared dictionaries and tessellation, which it reports as unsupported.
//! The specification's synthetic fixtures (`fixtures/mlt-synthetic`) check
//! it.
//!
//! It depends on nothing else in the crate, so that the integration tests
//! can include it as it is.

/// The frame tag of an MLT v1 layer.
pub(crate) const TAG_V1: u8 = 0x01;

/// A vertex, `[x, y]` in tile coordinates.
pub(crate) type Vertex = [i32; 2];

/// The prefix of the errors for encodings this decoder doesn't read.
pub(crate) const UNSUPPORTED: &str = "unsupported: ";

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Value {
    Bool(bool),
    Int(i64),
    UInt(u64),
    Float(f32),
    Double(f64),
    String(String),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Geometry {
    Point(Vertex),
    MultiPoint(Vec<Vertex>),
    LineString(Vec<Vertex>),
    MultiLineString(Vec<Vec<Vertex>>),
    Polygon(Vec<Vec<Vertex>>),
    MultiPolygon(Vec<Vec<Vec<Vertex>>>),
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Feature {
    pub id: Option<u64>,
    pub geometry: Geometry,
    /// In column order; a null value is left out.
    pub properties: Vec<(String, Value)>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Layer {
    pub name: String,
    pub extent: u32,
    pub features: Vec<Feature>,
}

/// The v1 layers of a tile; frames with other tags are skipped.
pub(crate) fn decode(tile: &[u8]) -> Result<Vec<Layer>, String> {
    let mut layers = Vec::new();
    for (tag, frame) in frames(tile)? {
        if tag != TAG_V1 {
            continue;
        }
        let mut at = 0;
        read_varint(frame, &mut at)?;
        at += 1;
        let mut input = Input { bytes: frame, at };
        layers.push(input.layer()?);
        if input.at != frame.len() {
            return Err(format!(
                "layer {} ends {} bytes before its frame",
                layers.last().unwrap().name,
                frame.len() - input.at
            ));
        }
    }
    Ok(layers)
}

struct Column {
    kind: u8,
    name: String,
}

struct Header {
    kind: u8,
    logical: (u8, u8),
    physical: u8,
    num_values: u64,
    byte_length: usize,
    rle: Option<(u64, u64)>,
}

enum Ids {
    None,
    Ids(Vec<Option<u64>>),
}

struct Input<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Input<'a> {
    fn varint(&mut self) -> Result<u64, String> {
        read_varint(self.bytes, &mut self.at)
    }

    fn byte(&mut self) -> Result<u8, String> {
        let byte = *self.bytes.get(self.at).ok_or("the layer ends early")?;
        self.at += 1;
        Ok(byte)
    }

    fn take(&mut self, length: usize) -> Result<&'a [u8], String> {
        let bytes = self
            .at
            .checked_add(length)
            .and_then(|end| self.bytes.get(self.at..end))
            .ok_or("a stream runs past the end of its layer")?;
        self.at += length;
        Ok(bytes)
    }

    fn string(&mut self) -> Result<String, String> {
        let length = self.varint()? as usize;
        String::from_utf8(self.take(length)?.to_vec()).map_err(|_| "a name is not UTF-8".into())
    }

    fn layer(&mut self) -> Result<Layer, String> {
        let name = self.string()?;
        let extent = u32::try_from(self.varint()?).map_err(|_| "extent overflows")?;
        let count = self.varint()?;
        let mut columns = Vec::new();
        for _ in 0..count {
            let kind = self.byte()?;
            let name = match kind {
                0..=4 => String::new(),
                10 | 11 | 16..=29 => self.string()?,
                30 => return Err(format!("{UNSUPPORTED}shared dictionaries")),
                other => return Err(format!("column type {other} is not assigned")),
            };
            columns.push(Column { kind, name });
        }
        let mut ids = Ids::None;
        let mut geometries = None;
        let mut properties: Vec<(String, Vec<Option<Value>>)> = Vec::new();
        for column in &columns {
            let nullable = column.kind % 2 == 1;
            match column.kind {
                0..=3 => {
                    let present = if nullable { Some(self.bools()?) } else { None };
                    let header = self.header(false)?;
                    let width = if column.kind >= 2 { 64 } else { 32 };
                    let words = self.ints(&header, width, false)?;
                    ids = Ids::Ids(spread(present, words.into_iter())?);
                }
                4 => geometries = Some(self.geometry()?),
                _ => {
                    let streams = if (28..=29).contains(&column.kind) {
                        self.varint()?
                    } else {
                        1 + u64::from(nullable)
                    };
                    let present = if nullable { Some(self.bools()?) } else { None };
                    let values = match column.kind & !1 {
                        10 => {
                            let header = self.header(true)?;
                            let bits = self.bool_payload(&header)?;
                            bits.into_iter().map(Value::Bool).collect::<Vec<_>>()
                        }
                        16 | 20 => {
                            let header = self.header(false)?;
                            let width = if column.kind & !1 == 16 { 32 } else { 64 };
                            let words = self.ints(&header, width, true)?;
                            words.into_iter().map(|w| Value::Int(w as i64)).collect()
                        }
                        18 | 22 => {
                            let header = self.header(false)?;
                            let width = if column.kind & !1 == 18 { 32 } else { 64 };
                            let words = self.ints(&header, width, false)?;
                            words.into_iter().map(Value::UInt).collect()
                        }
                        24 => {
                            let header = self.header(false)?;
                            let bytes = self.take(header.byte_length)?;
                            bytes
                                .chunks_exact(4)
                                .map(|b| Value::Float(f32::from_le_bytes(b.try_into().unwrap())))
                                .collect()
                        }
                        26 => {
                            let header = self.header(false)?;
                            let bytes = self.take(header.byte_length)?;
                            bytes
                                .chunks_exact(8)
                                .map(|b| Value::Double(f64::from_le_bytes(b.try_into().unwrap())))
                                .collect()
                        }
                        28 => self
                            .strings(streams - u64::from(nullable))?
                            .into_iter()
                            .map(Value::String)
                            .collect(),
                        other => return Err(format!("column type {other} is not assigned")),
                    };
                    properties.push((column.name.clone(), spread(present, values.into_iter())?));
                }
            }
        }
        let geometries = geometries.ok_or("a layer has no geometry column")?;
        let ids = match ids {
            Ids::None => vec![None; geometries.len()],
            Ids::Ids(ids) => ids,
        };
        if ids.len() != geometries.len()
            || properties.iter().any(|(_, v)| v.len() != geometries.len())
        {
            return Err("columns hold different numbers of features".into());
        }
        let features = geometries
            .into_iter()
            .enumerate()
            .map(|(row, geometry)| Feature {
                id: ids[row],
                geometry,
                properties: properties
                    .iter()
                    .filter_map(|(name, values)| Some((name.clone(), values[row].clone()?)))
                    .collect(),
            })
            .collect();
        Ok(Layer {
            name,
            extent,
            features,
        })
    }

    fn header(&mut self, boolean: bool) -> Result<Header, String> {
        let kind = self.byte()?;
        let encoding = self.byte()?;
        let logical = (encoding >> 5, (encoding >> 2) & 7);
        let physical = encoding & 3;
        let num_values = self.varint()?;
        let byte_length = self.varint()? as usize;
        let rle = match logical {
            (3, 0) | (1, 3) if !boolean => Some((self.varint()?, self.varint()?)),
            (4, 1) => return Err(format!("{UNSUPPORTED}Morton vertices")),
            _ => None,
        };
        Ok(Header {
            kind,
            logical,
            physical,
            num_values,
            byte_length,
            rle,
        })
    }

    /// A present stream.
    fn bools(&mut self) -> Result<Vec<bool>, String> {
        let header = self.header(true)?;
        self.bool_payload(&header)
    }

    fn bool_payload(&mut self, header: &Header) -> Result<Vec<bool>, String> {
        let payload = self.take(header.byte_length)?;
        let bytes = header.num_values.div_ceil(8) as usize;
        let bitmap = match header.logical {
            (3, 0) => {
                let mut out = Vec::with_capacity(bytes);
                let mut at = 0;
                while at < payload.len() {
                    let control = payload[at];
                    at += 1;
                    if control < 128 {
                        let byte = *payload.get(at).ok_or("a byte run ends early")?;
                        at += 1;
                        out.extend(std::iter::repeat_n(byte, usize::from(control) + 3));
                    } else {
                        let count = 256 - usize::from(control);
                        out.extend_from_slice(
                            payload.get(at..at + count).ok_or("a byte run ends early")?,
                        );
                        at += count;
                    }
                }
                out
            }
            (0, 0) => payload.to_vec(),
            other => return Err(format!("{UNSUPPORTED}boolean encoding {other:?}")),
        };
        if bitmap.len() != bytes {
            return Err(format!(
                "a boolean stream expands to {} bytes, not {bytes}",
                bitmap.len()
            ));
        }
        Ok((0..header.num_values as usize)
            .map(|i| bitmap[i / 8] >> (i % 8) & 1 == 1)
            .collect())
    }

    /// The values of an integer stream, sign-extended to 64 bits when
    /// `signed`.
    fn ints(&mut self, header: &Header, width: u32, signed: bool) -> Result<Vec<u64>, String> {
        let payload = self.take(header.byte_length)?;
        let words = words(payload, header, width)?;
        let unzigzag = |w: u64| -> u64 {
            if width == 32 {
                let w = w as u32;
                (((w >> 1) as i32) ^ -((w & 1) as i32)) as i64 as u64
            } else {
                ((w >> 1) as i64 ^ -((w & 1) as i64)) as u64
            }
        };
        let expand = |words: &[u64]| -> Result<Vec<u64>, String> {
            let (runs, count) = header.rle.ok_or("an RLE stream has no run count")?;
            let runs = runs as usize;
            if words.len() != runs * 2 {
                return Err("an RLE stream holds the wrong number of words".into());
            }
            let mut out = Vec::with_capacity(count as usize);
            for i in 0..runs {
                out.extend(std::iter::repeat_n(words[runs + i], words[i] as usize));
            }
            if out.len() as u64 != count {
                return Err("RLE runs do not add up to the value count".into());
            }
            Ok(out)
        };
        let undelta = |deltas: Vec<u64>| -> Vec<u64> {
            let mut value = 0u64;
            deltas
                .into_iter()
                .map(|d| {
                    value = value.wrapping_add(unzigzag(d));
                    if width == 32 {
                        value = if signed {
                            value as u32 as i32 as i64 as u64
                        } else {
                            u64::from(value as u32)
                        };
                    }
                    value
                })
                .collect()
        };
        let values = match header.logical {
            (0, 0) => words,
            (1, 0) => return Ok(undelta(words)),
            (3, 0) => expand(&words)?,
            (1, 3) => return Ok(undelta(expand(&words)?)),
            other => return Err(format!("{UNSUPPORTED}integer encoding {other:?}")),
        };
        Ok(if signed {
            values.into_iter().map(unzigzag).collect()
        } else {
            values
        })
    }

    fn strings(&mut self, streams: u64) -> Result<Vec<String>, String> {
        let (mut lengths, mut dictionary_lengths, mut codes, mut data, mut dictionary) =
            (None, None, None, None, None);
        for _ in 0..streams {
            let header = self.header(false)?;
            match header.kind {
                0x30 => lengths = Some(self.ints(&header, 32, false)?),
                0x36 => dictionary_lengths = Some(self.ints(&header, 32, false)?),
                0x22 => codes = Some(self.ints(&header, 32, false)?),
                0x10 => data = Some(self.take(header.byte_length)?),
                0x11 => dictionary = Some(self.take(header.byte_length)?),
                0x35 | 0x15 => return Err(format!("{UNSUPPORTED}FSST")),
                other => return Err(format!("unexpected string stream {other:#x}")),
            }
        }
        let split = |lengths: &[u64], bytes: &[u8]| -> Result<Vec<String>, String> {
            let mut at = 0;
            lengths
                .iter()
                .map(|&length| {
                    let value = bytes
                        .get(at..at + length as usize)
                        .ok_or("string lengths run past their bytes")?;
                    at += length as usize;
                    String::from_utf8(value.to_vec()).map_err(|_| "a string is not UTF-8".into())
                })
                .collect()
        };
        match (lengths, data, dictionary_lengths, codes, dictionary) {
            (Some(lengths), Some(data), None, None, None) => split(&lengths, data),
            (None, None, Some(lengths), Some(codes), Some(dictionary)) => {
                let values = split(&lengths, dictionary)?;
                codes
                    .iter()
                    .map(|&code| {
                        values
                            .get(code as usize)
                            .cloned()
                            .ok_or_else(|| "a dictionary code is out of range".into())
                    })
                    .collect()
            }
            _ => Err("a string column has an unknown set of streams".into()),
        }
    }

    fn geometry(&mut self) -> Result<Vec<Geometry>, String> {
        let streams = self.varint()?;
        let header = self.header(false)?;
        let types = self.ints(&header, 32, false)?;
        let (mut geometries, mut parts, mut rings) = (None, None, None);
        let (mut vertices, mut offsets) = (None, None);
        for _ in 1..streams {
            let header = self.header(false)?;
            match header.kind {
                0x31 => geometries = Some(self.ints(&header, 32, false)?),
                0x32 => parts = Some(self.ints(&header, 32, false)?),
                0x33 => rings = Some(self.ints(&header, 32, false)?),
                0x20 => offsets = Some(self.ints(&header, 32, false)?),
                0x13 => vertices = Some(self.vertices(&header)?),
                0x34 | 0x21 => return Err(format!("{UNSUPPORTED}tessellation")),
                other => return Err(format!("unexpected geometry stream {other:#x}")),
            }
        }
        let mut vertices = vertices.unwrap_or_default();
        if let Some(offsets) = offsets {
            vertices = offsets
                .iter()
                .map(|&i| vertices.get(i as usize).copied())
                .collect::<Option<Vec<_>>>()
                .ok_or("a vertex offset is out of range")?;
        }
        let has_rings = rings.is_some();
        let mut counts = Counts {
            geometries: geometries.unwrap_or_default().into_iter(),
            parts: parts.unwrap_or_default().into_iter(),
            rings: rings.unwrap_or_default().into_iter(),
        };
        let mut vertices = vertices.into_iter();
        let mut take = |n: u64| -> Result<Vec<Vertex>, String> {
            let taken = vertices.by_ref().take(n as usize).collect::<Vec<_>>();
            if taken.len() as u64 != n {
                return Err("geometry lengths run past the vertices".into());
            }
            Ok(taken)
        };
        let mut out = Vec::with_capacity(types.len());
        for kind in types {
            let count = if kind > 2 {
                Counts::next(&mut counts.geometries)?
            } else {
                1
            };
            let mut items = Vec::new();
            for _ in 0..count {
                items.push(match kind {
                    0 | 3 => vec![take(1)?],
                    1 | 4 => {
                        let n = if has_rings {
                            Counts::next(&mut counts.rings)?
                        } else {
                            Counts::next(&mut counts.parts)?
                        };
                        vec![take(n)?]
                    }
                    2 | 5 => {
                        let n = Counts::next(&mut counts.parts)?;
                        let mut polygon = Vec::new();
                        for _ in 0..n {
                            polygon.push(take(Counts::next(&mut counts.rings)?)?);
                        }
                        polygon
                    }
                    other => return Err(format!("geometry type {other} is not one MLT has")),
                });
            }
            out.push(match kind {
                0 => Geometry::Point(items[0][0][0]),
                3 => Geometry::MultiPoint(items.into_iter().map(|i| i[0][0]).collect()),
                1 => Geometry::LineString(items.remove(0).remove(0)),
                4 => {
                    Geometry::MultiLineString(items.into_iter().map(|mut i| i.remove(0)).collect())
                }
                2 => Geometry::Polygon(items.remove(0)),
                _ => Geometry::MultiPolygon(items),
            });
        }
        if vertices.next().is_some() {
            return Err("vertices are left over".into());
        }
        Ok(out)
    }

    fn vertices(&mut self, header: &Header) -> Result<Vec<Vertex>, String> {
        let payload = self.take(header.byte_length)?;
        let words = words(payload, header, 32)?;
        let unzigzag = |w: u64| ((w as u32 >> 1) as i32) ^ -((w as u32 & 1) as i32);
        if words.len() % 2 != 0 {
            return Err("a vertex stream holds an odd number of words".into());
        }
        let mut previous = [0i32; 2];
        let mut out = Vec::with_capacity(words.len() / 2);
        for pair in words.chunks_exact(2) {
            let vertex = match header.logical {
                (2, 0) => [
                    previous[0].wrapping_add(unzigzag(pair[0])),
                    previous[1].wrapping_add(unzigzag(pair[1])),
                ],
                (0, 0) => [unzigzag(pair[0]), unzigzag(pair[1])],
                other => return Err(format!("{UNSUPPORTED}vertex encoding {other:?}")),
            };
            previous = vertex;
            out.push(vertex);
        }
        Ok(out)
    }
}

struct Counts {
    geometries: std::vec::IntoIter<u64>,
    parts: std::vec::IntoIter<u64>,
    rings: std::vec::IntoIter<u64>,
}

impl Counts {
    fn next(stream: &mut std::vec::IntoIter<u64>) -> Result<u64, String> {
        stream
            .next()
            .ok_or_else(|| "a geometry length stream ends early".into())
    }
}

/// The payload's words, before the logical encoding is undone.
fn words(payload: &[u8], header: &Header, width: u32) -> Result<Vec<u64>, String> {
    match header.physical {
        2 => {
            let mut words = Vec::new();
            let mut at = 0;
            while at < payload.len() {
                words.push(read_varint(payload, &mut at)?);
            }
            if words.len() as u64 != header.num_values {
                return Err(format!(
                    "a stream declares {} values but holds {}",
                    header.num_values,
                    words.len()
                ));
            }
            Ok(words)
        }
        0 => {
            let size = (width / 8) as usize;
            Ok(payload
                .chunks_exact(size)
                .map(|chunk| {
                    let mut bytes = [0u8; 8];
                    bytes[..size].copy_from_slice(chunk);
                    u64::from_le_bytes(bytes)
                })
                .collect())
        }
        1 => Err(format!("{UNSUPPORTED}FastPFOR")),
        other => Err(format!("physical encoding {other} is not assigned")),
    }
}

/// The column's values for every feature, from its present stream and the
/// values of the features that have one.
fn spread<T>(
    present: Option<Vec<bool>>,
    mut values: impl Iterator<Item = T>,
) -> Result<Vec<Option<T>>, String> {
    let out = match present {
        None => values.by_ref().map(Some).collect(),
        Some(present) => present
            .into_iter()
            .map(|p| {
                if p {
                    values.next().map(Some)
                } else {
                    Some(None)
                }
            })
            .collect::<Option<Vec<_>>>()
            .ok_or("a column has fewer values than its present stream")?,
    };
    if values.next().is_some() {
        return Err("a column has more values than its present stream".into());
    }
    Ok(out)
}

/// Each frame of `tile`, whole, with its tag; an error when one runs past the
/// end of the tile.
pub(crate) fn frames(tile: &[u8]) -> Result<Vec<(u8, &[u8])>, String> {
    let mut frames = Vec::new();
    let mut at = 0;
    while at < tile.len() {
        let start = at;
        let size = read_varint(tile, &mut at)? as usize;
        let end = at
            .checked_add(size)
            .filter(|&end| end <= tile.len())
            .ok_or("an MLT frame runs past the end of the tile")?;
        let &tag = tile
            .get(at)
            .filter(|_| size > 0)
            .ok_or("an MLT frame is empty")?;
        frames.push((tag, &tile[start..end]));
        at = end;
    }
    Ok(frames)
}

/// The name of the v1 layer in `frame`, a whole frame as [`frames`] gives
/// it, checked to be non-empty and to have a non-zero extent.
pub(crate) fn layer_name(frame: &[u8]) -> Result<String, String> {
    let mut at = 0;
    read_varint(frame, &mut at)?;
    at += 1;
    let mut input = Input { bytes: frame, at };
    let name = input.string()?;
    if name.is_empty() {
        return Err("an MLT layer has an empty name".into());
    }
    if input.varint()? == 0 {
        return Err(format!("MLT layer {name} has extent 0"));
    }
    Ok(name)
}

pub(crate) fn read_varint(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).ok_or("an MLT varint runs past the end")?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    Err("an MLT varint is longer than 10 bytes".into())
}

/// Whether `mlt` holds what the MVT tile `mvt` does: every layer that keeps
/// a feature, in order, with its extent, and every feature that has a
/// geometry, in order, with its id, its geometry (rings without their
/// closing vertex) and its attributes. Attributes are compared as text,
/// since a column of mixed kinds is written as the one kind that holds them
/// all.
#[cfg(test)]
pub(crate) fn same_as_mvt(mlt: &[u8], mvt: &[u8]) -> Result<(), String> {
    use fast_mvt::{MvtReaderRef, MvtValueRef};
    use geo_types::{Geometry as G, LineString};

    fn text(value: &MvtValueRef) -> Option<String> {
        Some(match value {
            MvtValueRef::String(v) => v.to_string(),
            MvtValueRef::Int(v) | MvtValueRef::SInt(v) => v.to_string(),
            MvtValueRef::UInt(v) => v.to_string(),
            MvtValueRef::Float(v) => v.to_string(),
            MvtValueRef::Double(v) => v.to_string(),
            MvtValueRef::Bool(v) => v.to_string(),
            MvtValueRef::Null => return None,
        })
    }
    fn value_text(value: &Value) -> String {
        match value {
            Value::String(v) => v.clone(),
            Value::Int(v) => v.to_string(),
            Value::UInt(v) => v.to_string(),
            Value::Float(v) => v.to_string(),
            Value::Double(v) => v.to_string(),
            Value::Bool(v) => v.to_string(),
        }
    }
    fn path(line: &LineString<i32>) -> Vec<Vertex> {
        line.0.iter().map(|c| [c.x, c.y]).collect()
    }
    fn ring(line: &LineString<i32>) -> Vec<Vertex> {
        let mut ring = path(line);
        ring.pop();
        ring
    }
    fn polygon(polygon: &geo_types::Polygon<i32>) -> Vec<Vec<Vertex>> {
        std::iter::once(polygon.exterior())
            .chain(polygon.interiors())
            .map(ring)
            .collect()
    }

    let reader = MvtReaderRef::new(mvt).map_err(|e| e.to_string())?;
    let layers = decode(mlt)?;
    let mut expected_layers = Vec::new();
    for layer in reader.layers() {
        let mut features = Vec::new();
        for feature in layer.features() {
            if feature.geometry_commands().len() == 0 {
                continue;
            }
            let geometry = match feature.geometry().map_err(|e| e.to_string())? {
                G::Point(p) => Geometry::Point([p.x(), p.y()]),
                G::MultiPoint(m) => {
                    Geometry::MultiPoint(m.iter().map(|p| [p.x(), p.y()]).collect())
                }
                G::LineString(l) => Geometry::LineString(path(&l)),
                G::MultiLineString(m) => Geometry::MultiLineString(m.iter().map(path).collect()),
                G::Polygon(p) => Geometry::Polygon(polygon(&p)),
                G::MultiPolygon(m) => Geometry::MultiPolygon(m.iter().map(polygon).collect()),
                other => return Err(format!("unexpected MVT geometry {other:?}")),
            };
            let mut properties = feature
                .properties()
                .filter_map(|p| {
                    p.map(|(k, v)| text(&v).map(|v| (k.to_string(), v)))
                        .transpose()
                })
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            properties.sort();
            features.push((feature.id(), geometry, properties));
        }
        if !features.is_empty() {
            expected_layers.push((layer.name().to_string(), layer.extent(), features));
        }
    }
    if layers.len() != expected_layers.len() {
        return Err(format!(
            "{} MLT layers for {} MVT layers",
            layers.len(),
            expected_layers.len()
        ));
    }
    for (layer, (name, extent, features)) in layers.iter().zip(expected_layers) {
        if layer.name != name || layer.extent != extent {
            return Err(format!(
                "MLT layer {} (extent {}) for MVT layer {name} (extent {extent})",
                layer.name, layer.extent
            ));
        }
        if layer.features.len() != features.len() {
            return Err(format!(
                "layer {name}: {} MLT features for {} MVT features",
                layer.features.len(),
                features.len()
            ));
        }
        for (index, (feature, (id, geometry, properties))) in
            layer.features.iter().zip(features).enumerate()
        {
            let mut actual = feature
                .properties
                .iter()
                .map(|(k, v)| (k.clone(), value_text(v)))
                .collect::<Vec<_>>();
            actual.sort();
            if feature.id != id || feature.geometry != geometry || actual != properties {
                return Err(format!(
                    "layer {name} feature #{index}: MLT {:?} {:?} {actual:?}, MVT {id:?} {geometry:?} {properties:?}",
                    feature.id, feature.geometry
                ));
            }
        }
    }
    Ok(())
}
