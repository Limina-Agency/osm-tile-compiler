//! Writes small synthetic PBF files for the end-to-end tests, at the
//! versions osm-pbf-ingest reads with.
use osmpbfreader::fileformat::{Blob, BlobHeader};
use osmpbfreader::osmformat::relation::MemberType;
use osmpbfreader::osmformat::{
    DenseNodes, HeaderBBox, HeaderBlock, PrimitiveBlock, PrimitiveGroup, Relation, StringTable, Way,
};
use protobuf::Message;
use std::path::Path;

/// A PBF frame: big-endian length, `BlobHeader`, uncompressed blob.
fn frame(blob_type: &str, payload: &[u8]) -> Vec<u8> {
    let mut blob = Blob::new();
    blob.set_raw(payload.to_vec());
    blob.set_raw_size(payload.len() as i32);
    let blob = blob.write_to_bytes().unwrap();
    let mut header = BlobHeader::new();
    header.set_type(blob_type.into());
    header.set_datasize(blob.len() as i32);
    let header = header.write_to_bytes().unwrap();
    let mut out = (header.len() as u32).to_be_bytes().to_vec();
    out.extend(header);
    out.extend(blob);
    out
}

/// Interns strings for a block's string table; index 0 is the empty string.
#[derive(Default)]
struct Strings(Vec<String>);

impl Strings {
    fn index(&mut self, s: &str) -> u32 {
        if self.0.is_empty() {
            self.0.push(String::new());
        }
        let at = self.0.iter().position(|t| t == s).unwrap_or_else(|| {
            self.0.push(s.into());
            self.0.len() - 1
        });
        at as u32
    }

    fn block(mut self, group: PrimitiveGroup) -> Vec<u8> {
        self.index("");
        let mut table = StringTable::new();
        table.s = self.0.into_iter().map(String::into_bytes).collect();
        let mut block = PrimitiveBlock::new();
        block.stringtable = Some(table).into();
        block.primitivegroup.push(group);
        block.write_to_bytes().unwrap()
    }
}

pub type Tags<'a> = &'a [(&'a str, &'a str)];

/// A relation's way members, as (way ID, role).
pub type Members<'a> = &'a [(i64, &'a str)];

/// Writes a sorted PBF with dense nodes `(id, lon, lat, tags)`, ways
/// `(id, nodes, tags)` and relations of ways `(id, members, tags)`, its
/// header box from `west` to `east`.
pub fn write_pbf(
    path: &Path,
    [west, south, east, north]: [f64; 4],
    nodes: &[(i64, f64, f64, Tags)],
    ways: &[(i64, &[i64], Tags)],
    relations: &[(i64, Members, Tags)],
) {
    let nano = |degrees: f64| (degrees * 1e9).round() as i64;
    let mut bbox = HeaderBBox::new();
    bbox.set_left(nano(west));
    bbox.set_right(nano(east));
    bbox.set_top(nano(north));
    bbox.set_bottom(nano(south));
    let mut header = HeaderBlock::new();
    header.bbox = Some(bbox).into();
    header.required_features = vec!["OsmSchema-V0.6".into(), "DenseNodes".into()];
    header.optional_features = vec!["Sort.Type_then_ID".into()];
    let mut bytes = frame("OSMHeader", &header.write_to_bytes().unwrap());

    let mut strings = Strings::default();
    let mut dense = DenseNodes::new();
    let (mut id, mut lat, mut lon) = (0, 0, 0);
    for &(node, x, y, tags) in nodes {
        // Units of 100 nanodegrees, the default granularity.
        let (x, y) = ((x * 1e7).round() as i64, (y * 1e7).round() as i64);
        dense.id.push(node - id);
        dense.lon.push(x - lon);
        dense.lat.push(y - lat);
        (id, lon, lat) = (node, x, y);
        for (key, value) in tags {
            dense.keys_vals.push(strings.index(key) as i32);
            dense.keys_vals.push(strings.index(value) as i32);
        }
        dense.keys_vals.push(0);
    }
    let mut group = PrimitiveGroup::new();
    group.dense = Some(dense).into();
    bytes.extend(frame("OSMData", &strings.block(group)));

    let mut strings = Strings::default();
    let mut group = PrimitiveGroup::new();
    for &(way, refs, tags) in ways {
        let mut encoded = Way::new();
        encoded.set_id(way);
        for (key, value) in tags {
            encoded.keys.push(strings.index(key));
            encoded.vals.push(strings.index(value));
        }
        let mut last = 0;
        for &node in refs {
            encoded.refs.push(node - last);
            last = node;
        }
        group.ways.push(encoded);
    }
    bytes.extend(frame("OSMData", &strings.block(group)));

    if !relations.is_empty() {
        let mut strings = Strings::default();
        let mut group = PrimitiveGroup::new();
        for &(id, members, tags) in relations {
            let mut encoded = Relation::new();
            encoded.set_id(id);
            for (key, value) in tags {
                encoded.keys.push(strings.index(key));
                encoded.vals.push(strings.index(value));
            }
            let mut last = 0;
            for &(way, role) in members {
                encoded.memids.push(way - last);
                last = way;
                encoded.roles_sid.push(strings.index(role) as i32);
                encoded.types.push(MemberType::WAY.into());
            }
            group.relations.push(encoded);
        }
        bytes.extend(frame("OSMData", &strings.block(group)));
    }
    std::fs::write(path, bytes).unwrap();
}
