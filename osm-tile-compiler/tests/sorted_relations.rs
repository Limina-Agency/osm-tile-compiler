//! End-to-end test: in a file that declares `Sort.Type_then_ID`, relations are
//! assembled while pass 2 still reads the ways, as soon as the ways they join
//! have been read. Monaco does not declare the order, so it is read with
//! relations queued after the last way; the same extract, declared sorted and
//! with its ways split into many small blocks so that relations queue in
//! several waves, must compile to the same archive byte for byte.

use osm_tile_compiler::{
    Area, BasemapRequest, CompileBounds, SupplementalDataset, compile_basemap,
};
mod common;
#[allow(dead_code)]
#[path = "common/pbf.rs"]
mod pbf;

use common::monaco_pbf;
use osmpbfreader::fileformat::{Blob, BlobHeader};
use osmpbfreader::osmformat::{HeaderBlock, PrimitiveBlock, PrimitiveGroup};
use protobuf::Message;
use std::io::Read;
use std::path::Path;

/// Ways per block in the rewritten extract.
const WAYS_PER_BLOCK: usize = 40;

/// The payload of each frame of a PBF, with its blob type.
fn frames(bytes: &[u8]) -> Vec<(String, Vec<u8>)> {
    let mut frames = Vec::new();
    let mut at = 0;
    while at < bytes.len() {
        let length = u32::from_be_bytes(bytes[at..at + 4].try_into().unwrap()) as usize;
        at += 4;
        let header = BlobHeader::parse_from_bytes(&bytes[at..at + length]).unwrap();
        at += length;
        let size = header.datasize() as usize;
        let blob = Blob::parse_from_bytes(&bytes[at..at + size]).unwrap();
        at += size;
        let payload = if blob.has_raw() {
            blob.raw().to_vec()
        } else {
            let mut payload = Vec::new();
            flate2::read::ZlibDecoder::new(blob.zlib_data())
                .read_to_end(&mut payload)
                .unwrap();
            payload
        };
        frames.push((header.type_().to_string(), payload));
    }
    frames
}

/// Writes `input` to `output` declaring `Sort.Type_then_ID`, with at most
/// `WAYS_PER_BLOCK` ways per block. Returns how many way blocks it wrote.
fn declare_sorted_with_small_way_blocks(input: &Path, output: &Path) -> usize {
    let mut bytes = Vec::new();
    let mut way_blocks = 0;
    let mut last_way = i64::MIN;
    for (kind, payload) in frames(&std::fs::read(input).unwrap()) {
        if kind == "OSMHeader" {
            let mut header = HeaderBlock::parse_from_bytes(&payload).unwrap();
            assert!(
                !header
                    .optional_features
                    .iter()
                    .any(|f| f == "Sort.Type_then_ID"),
                "monaco.osm.pbf now declares its order; this test needs one that does not"
            );
            header.optional_features.push("Sort.Type_then_ID".into());
            bytes.extend(pbf::frame(&kind, &header.write_to_bytes().unwrap()));
            continue;
        }
        let block = PrimitiveBlock::parse_from_bytes(&payload).unwrap();
        if block.primitivegroup.iter().all(|g| g.ways.is_empty()) {
            bytes.extend(pbf::frame(&kind, &payload));
            continue;
        }
        for group in &block.primitivegroup {
            assert!(
                group.nodes.is_empty() && group.dense.is_none() && group.relations.is_empty(),
                "a way block holds other objects too"
            );
            for ways in group.ways.chunks(WAYS_PER_BLOCK) {
                // The early queueing relies on the order the header declares.
                for way in ways {
                    assert!(way.id() > last_way, "monaco.osm.pbf's ways are not sorted");
                    last_way = way.id();
                }
                let mut small = block.clone();
                let mut group = PrimitiveGroup::new();
                group.ways = ways.to_vec();
                small.primitivegroup = vec![group];
                bytes.extend(pbf::frame(&kind, &small.write_to_bytes().unwrap()));
                way_blocks += 1;
            }
        }
    }
    std::fs::write(output, bytes).unwrap();
    way_blocks
}

fn compile(pbf: &Path, output: &Path, bounds: CompileBounds) -> Vec<u8> {
    let mut request = BasemapRequest::new(pbf, output, "", output.with_extension("tmp"));
    request.area = Area::bounds(bounds);
    request.omit_sources = SupplementalDataset::ALL.to_vec();
    request.threads = 4;
    compile_basemap(&request, |_| {}, || false).unwrap();
    std::fs::read(output).unwrap()
}

#[test]
fn relations_queued_while_ways_are_read_compile_the_same_archive() {
    let temp = tempfile::tempdir().unwrap();
    let sorted = temp.path().join("monaco-sorted.osm.pbf");
    let way_blocks = declare_sorted_with_small_way_blocks(&monaco_pbf(), &sorted);
    assert!(way_blocks >= 20, "{way_blocks} way blocks");
    let index = osm_pbf_ingest::blob_index::PbfBlobIndex::scan(&sorted).unwrap();
    assert!(index.header().sorted_type_then_id());
    let b = index
        .header()
        .bounds
        .expect("monaco.osm.pbf has a bounding box");
    let bounds = CompileBounds::new(b.north, b.south, b.east, b.west);

    let declared = compile(&sorted, &temp.path().join("declared.pmtiles"), bounds);
    let plain = compile(&monaco_pbf(), &temp.path().join("plain.pmtiles"), bounds);
    assert!(declared == plain, "the archives differ");
}
