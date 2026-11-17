//! A minimal MVT decoder (spec 2.1), enough to describe a tile's content
//! canonically without depending on another protobuf implementation.

use std::fmt::Write as _;

fn varint(bytes: &[u8], at: &mut usize) -> u64 {
    let mut value = 0;
    for shift in (0..64).step_by(7) {
        let byte = bytes[*at];
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            break;
        }
    }
    value
}

fn packed_varints(bytes: &[u8]) -> Vec<u64> {
    let mut at = 0;
    let mut values = Vec::new();
    while at < bytes.len() {
        values.push(varint(bytes, &mut at));
    }
    values
}

enum Field<'a> {
    Varint(u64),
    Fixed64(u64),
    Bytes(&'a [u8]),
    Fixed32(u32),
}

/// Calls `field` with the number and value of each protobuf field in `bytes`.
fn fields<'a>(bytes: &'a [u8], mut field: impl FnMut(u64, Field<'a>)) {
    let mut at = 0;
    while at < bytes.len() {
        let key = varint(bytes, &mut at);
        let value = match key & 7 {
            0 => Field::Varint(varint(bytes, &mut at)),
            1 => {
                at += 8;
                Field::Fixed64(u64::from_le_bytes(bytes[at - 8..at].try_into().unwrap()))
            }
            2 => {
                let len = varint(bytes, &mut at) as usize;
                at += len;
                Field::Bytes(&bytes[at - len..at])
            }
            5 => {
                at += 4;
                Field::Fixed32(u32::from_le_bytes(bytes[at - 4..at].try_into().unwrap()))
            }
            wire => panic!("unexpected protobuf wire type {wire}"),
        };
        field(key >> 3, value);
    }
}

fn zigzag(value: u64) -> i64 {
    (value >> 1) as i64 ^ -((value & 1) as i64)
}

/// An MVT value as text that keeps its type: `"s"` strings, `1.5f32`, `1.5f64`,
/// `-3i`, `3u`, `-3s` and `true`.
fn mvt_value(bytes: &[u8]) -> String {
    let mut text = String::new();
    fields(bytes, |number, value| {
        text = match (number, value) {
            (1, Field::Bytes(s)) => format!("{:?}", std::str::from_utf8(s).unwrap()),
            (2, Field::Fixed32(f)) => format!("{:?}f32", f32::from_bits(f)),
            (3, Field::Fixed64(d)) => format!("{:?}f64", f64::from_bits(d)),
            (4, Field::Varint(i)) => format!("{}i", i as i64),
            (5, Field::Varint(u)) => format!("{u}u"),
            (6, Field::Varint(s)) => format!("{}s", zigzag(s)),
            (7, Field::Varint(b)) => (b != 0).to_string(),
            _ => panic!("malformed MVT value"),
        }
    });
    text
}

/// Geometry commands with absolute coordinates: `M x y`, `L x y x y …`, `Z`.
fn mvt_geometry(commands: &[u64]) -> String {
    let (mut x, mut y) = (0_i64, 0_i64);
    let mut text = String::new();
    let mut at = 0;
    while at < commands.len() {
        let (id, count) = (commands[at] & 7, commands[at] >> 3);
        at += 1;
        match id {
            1 | 2 => {
                text.push_str(if id == 1 { " M" } else { " L" });
                for _ in 0..count {
                    x += zigzag(commands[at]);
                    y += zigzag(commands[at + 1]);
                    at += 2;
                    write!(text, " {x} {y}").unwrap();
                }
            }
            7 => text.push_str(" Z"),
            _ => panic!("unknown MVT command {id}"),
        }
    }
    text.trim_start().to_string()
}

/// One decoded feature: its identity and attributes, and its geometry, each
/// as canonical text.
pub struct Feature {
    pub attributes: String,
    pub geometry: String,
}

/// A decompressed MVT tile's layers, in tile order, with their features in
/// layer order.
pub fn decode_tile(tile: &[u8]) -> Vec<(String, Vec<Feature>)> {
    let mut layers = Vec::new();
    fields(tile, |number, layer| {
        let (3, Field::Bytes(layer)) = (number, layer) else {
            return;
        };
        let (mut name, mut version, mut extent) = (String::new(), 1, 4096);
        let (mut keys, mut values, mut raw_features) = (vec![], vec![], vec![]);
        fields(layer, |number, value| match (number, value) {
            (1, Field::Bytes(s)) => name = String::from_utf8(s.to_vec()).unwrap(),
            (2, Field::Bytes(f)) => raw_features.push(f),
            (3, Field::Bytes(k)) => keys.push(String::from_utf8(k.to_vec()).unwrap()),
            (4, Field::Bytes(v)) => values.push(mvt_value(v)),
            (5, Field::Varint(e)) => extent = e,
            (15, Field::Varint(v)) => version = v,
            _ => panic!("unexpected field {number} in layer"),
        });
        assert_eq!((version, extent), (2, 4096), "layer {name}");
        let features = raw_features
            .into_iter()
            .map(|raw| {
                let (mut id, mut tags, mut kind, mut commands) = (None, vec![], 0, vec![]);
                fields(raw, |number, value| match (number, value) {
                    (1, Field::Varint(i)) => id = Some(i),
                    (2, Field::Bytes(t)) => tags = packed_varints(t),
                    (3, Field::Varint(k)) => kind = k,
                    (4, Field::Bytes(g)) => commands = packed_varints(g),
                    _ => panic!("unexpected field {number} in feature"),
                });
                let id = id.map_or("-".into(), |id| id.to_string());
                let mut attributes = format!("id={id} type={kind}");
                for pair in tags.chunks(2) {
                    let (key, value) = (&keys[pair[0] as usize], &values[pair[1] as usize]);
                    write!(attributes, " {key}={value}").unwrap();
                }
                Feature {
                    attributes,
                    geometry: mvt_geometry(&commands),
                }
            })
            .collect();
        layers.push((name, features));
    });
    layers
}
