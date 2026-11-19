//! Columnar, block-compressed sort runs.
//!
//! A run file is `MAGIC`, then a sequence of blocks, each holding whole records, in order:
//! `[record count: u32 LE]`, then for each of `STREAMS` streams
//! `[compressed length: u32 LE][raw length: u32 LE]`, then each stream's zstd
//! frame (none for an empty stream). A record's bytes are spread over the
//! streams by kind: sort keys, insertion sequences, then the payload's parts
//! (see `record_codec::split_payload`), so that each zstd frame holds bytes
//! that look alike. Keys and sequences are deltas from the previous record of
//! the block, which in a sorted run are mostly zero.
//!
//! Blocks are compressed independently so that a reader holds one raw block,
//! not a zstd window: a merge keeps up to `MERGE_FAN_IN` readers open at
//! once. The decompression context and the compressed bytes are per thread
//! and shared by every reader on it.
use super::record_codec::{
    LAYER_NAMES, PAYLOAD_STREAMS, Reader, join_payload, put_varint, put_zigzag, split_payload,
};
use super::store::{
    EncodedRecord, LAYER_VERBATIM, Layer, MAX_LAYER_BYTES, MAX_RECORD_BYTES, Seq, SortKey,
    frame_bytes,
};
use std::cell::RefCell;
use std::fs::File;
use std::io::{self, BufWriter, Read, Write};
use std::path::Path;

/// Raw bytes per block, give or take the last record. Larger blocks
/// compress a little better and cost each open reader as much memory.
pub(super) const BLOCK_BYTES: usize = 64 * 1024;
/// zstd's fastest level already shrinks records about as much as level 3, at
/// a fraction of the time.
const LEVEL: i32 = 1;
/// Starts every run file, so that a file of another format is refused.
const MAGIC: [u8; 4] = *b"OTR2";
const KEYS: usize = 0;
const STREAMS: usize = 2 + PAYLOAD_STREAMS;
const BLOCK_HEADER_BYTES: usize = 4 + 8 * STREAMS;
/// A stream of one block holds at most a block and one record, which
/// splitting can grow by a byte for each ring.
const MAX_STREAM_BYTES: usize = BLOCK_BYTES + 2 * MAX_RECORD_BYTES + 1024;

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// The key and sequence of the previous record of a block.
#[derive(Default)]
struct Previous {
    tile_id: u64,
    layer: Option<Layer>,
    sort_key: i64,
    id: u64,
    seq: Seq,
}

impl Previous {
    fn same_layer(&self, tile_id: u64, layer: &Layer) -> bool {
        self.tile_id == tile_id && self.layer.as_ref() == Some(layer)
    }

    /// A sort key is a delta within a tile's layer.
    fn sort_base(&self, tile_id: u64, layer: &Layer) -> i64 {
        if self.same_layer(tile_id, layer) {
            self.sort_key
        } else {
            0
        }
    }

    /// An id is a delta among records of equal sort key.
    fn id_base(&self, tile_id: u64, layer: &Layer, sort_key: i64) -> u64 {
        if self.same_layer(tile_id, layer) && self.sort_key == sort_key {
            self.id
        } else {
            0
        }
    }

    fn feature_base(&self, source: u64) -> u32 {
        if self.seq.source == source {
            self.seq.feature
        } else {
            0
        }
    }

    fn set(&mut self, key: &SortKey) {
        self.tile_id = key.tile_id;
        self.layer = Some(key.layer.clone());
        self.sort_key = key.sort_key;
        self.id = key.id;
        self.seq = key.seq;
    }
}

/// Writes a run. `finish` writes the last block; a writer dropped without it
/// leaves a truncated file, which the caller deletes.
pub(super) struct RunWriter<W: Write = BufWriter<File>> {
    out: W,
    streams: [Vec<u8>; STREAMS],
    records: u32,
    previous: Previous,
    /// The block header, then the compressed streams.
    block: Vec<u8>,
    /// One compressed stream.
    frame: Vec<u8>,
    compressor: zstd::bulk::Compressor<'static>,
    raw_bytes: u64,
}

impl RunWriter {
    pub(super) fn create(path: &Path) -> Result<Self, String> {
        let file =
            File::create(path).map_err(|e| format!("Failed to create {}: {e}", path.display()))?;
        Self::new(BufWriter::with_capacity(BLOCK_BYTES, file))
    }
}

impl<W: Write> RunWriter<W> {
    pub(super) fn new(mut out: W) -> Result<Self, String> {
        out.write_all(&MAGIC).map_err(|e| e.to_string())?;
        let compressor = zstd::bulk::Compressor::new(LEVEL).map_err(|e| e.to_string())?;
        Ok(Self {
            out,
            streams: Default::default(),
            records: 0,
            previous: Previous::default(),
            block: Vec::new(),
            frame: Vec::new(),
            compressor,
            raw_bytes: 0,
        })
    }

    /// Appends `record`, which sorts after the previous one.
    pub(super) fn write(&mut self, record: &EncodedRecord) -> Result<(), String> {
        self.write_parts(&record.key, &record.payload)
    }

    /// Appends the record of `key` and `payload`, which sorts after the
    /// previous one.
    pub(super) fn write_parts(&mut self, key: &SortKey, payload: &[u8]) -> Result<(), String> {
        if payload.is_empty() || payload.len() > MAX_RECORD_BYTES {
            return Err("OMT record exceeds bounded frame limit".into());
        }
        if let Layer::Other(name) = &key.layer
            && name.len() > MAX_LAYER_BYTES
        {
            return Err("OMT record exceeds bounded frame limit".into());
        }
        let record_payload = payload;
        let lengths = self.streams.each_ref().map(Vec::len);
        let [keys, sequences, payload @ ..] = &mut self.streams;
        if let Err(error) = split_payload(record_payload, payload) {
            for (stream, len) in self.streams.iter_mut().zip(lengths) {
                stream.truncate(len);
            }
            return Err(error);
        }
        let sort_base = self.previous.sort_base(key.tile_id, &key.layer);
        let id_base = self.previous.id_base(key.tile_id, &key.layer, key.sort_key);
        put_varint(keys, key.tile_id.wrapping_sub(self.previous.tile_id));
        match &key.layer {
            Layer::Known(id) => keys.push(*id),
            Layer::Other(name) => {
                keys.push(LAYER_VERBATIM);
                put_varint(keys, name.len() as u64);
                keys.extend_from_slice(name.as_bytes());
            }
        }
        put_zigzag(keys, key.sort_key.wrapping_sub(sort_base));
        put_zigzag(keys, key.id.wrapping_sub(id_base) as i64);
        put_zigzag(
            sequences,
            key.seq.source.wrapping_sub(self.previous.seq.source) as i64,
        );
        let feature_base = self.previous.feature_base(key.seq.source);
        put_zigzag(
            sequences,
            i64::from(key.seq.feature) - i64::from(feature_base),
        );
        put_varint(sequences, u64::from(key.seq.record));
        self.previous.set(key);
        self.records += 1;
        self.raw_bytes += frame_bytes(key, record_payload.len()) as u64;
        if self.streams.iter().map(Vec::len).sum::<usize>() >= BLOCK_BYTES {
            self.write_block().map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    fn write_block(&mut self) -> io::Result<()> {
        if self.records == 0 {
            return Ok(());
        }
        self.block.clear();
        self.block.resize(BLOCK_HEADER_BYTES, 0);
        self.block[..4].copy_from_slice(&self.records.to_le_bytes());
        for (index, stream) in self.streams.iter_mut().enumerate() {
            let mut compressed = 0;
            if !stream.is_empty() {
                // The frame replaces the buffer's contents.
                self.frame
                    .reserve(zstd::zstd_safe::compress_bound(stream.len()));
                compressed = self
                    .compressor
                    .compress_to_buffer(stream, &mut self.frame)?;
                self.block.extend_from_slice(&self.frame);
            }
            let at = 4 + 8 * index;
            let compressed = compressed as u32;
            self.block[at..at + 4].copy_from_slice(&compressed.to_le_bytes());
            self.block[at + 4..at + 8].copy_from_slice(&(stream.len() as u32).to_le_bytes());
            stream.clear();
        }
        self.out.write_all(&self.block)?;
        self.records = 0;
        self.previous = Previous::default();
        Ok(())
    }

    /// Writes the last block and returns the records' size as frames, the
    /// uncompressed measure the progress counts in.
    pub(super) fn finish(mut self) -> Result<u64, String> {
        self.write_block()
            .and_then(|_| self.out.flush())
            .map_err(|e| e.to_string())?;
        Ok(self.raw_bytes)
    }

    #[cfg(test)]
    pub(super) fn into_inner(mut self) -> W {
        self.write_block().unwrap();
        self.out
    }
}

thread_local! {
    static DECOMPRESS: RefCell<Option<(zstd::bulk::Decompressor<'static>, Vec<u8>)>> =
        const { RefCell::new(None) };
}

/// Reads a run written by `RunWriter`, record by record.
pub(super) struct RunReader<R: Read = File> {
    input: R,
    streams: [Vec<u8>; STREAMS],
    /// How far each stream has been read.
    positions: [usize; STREAMS],
    records: u32,
    previous: Previous,
    started: bool,
}

impl RunReader {
    pub(super) fn open(path: &Path) -> Result<Self, String> {
        let file =
            File::open(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
        Ok(Self::new(file))
    }
}

impl<R: Read> RunReader<R> {
    pub(super) fn new(input: R) -> Self {
        Self {
            input,
            streams: Default::default(),
            positions: [0; STREAMS],
            records: 0,
            previous: Previous::default(),
            started: false,
        }
    }

    /// Reads the next block; false at the end of the input.
    fn next_block(&mut self) -> io::Result<bool> {
        if !self.started {
            let mut magic = [0u8; MAGIC.len()];
            self.input
                .read_exact(&mut magic)
                .map_err(|e| other(format!("Truncated OMT run: {e}")))?;
            if magic != MAGIC {
                return Err(other("Unsupported OMT run format"));
            }
            self.started = true;
        }
        let mut header = [0u8; BLOCK_HEADER_BYTES];
        let mut filled = 0;
        while filled < header.len() {
            match self.input.read(&mut header[filled..]) {
                Ok(0) if filled == 0 => return Ok(false),
                Ok(0) => return Err(other("Truncated OMT run block header")),
                Ok(read) => filled += read,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let word = |at: usize| u32::from_le_bytes(header[at..at + 4].try_into().unwrap()) as usize;
        let records = word(0);
        let mut total = 0usize;
        let mut sizes = [(0usize, 0usize); STREAMS];
        for (index, size) in sizes.iter_mut().enumerate() {
            let (compressed, raw) = (word(4 + 8 * index), word(8 + 8 * index));
            if raw > MAX_STREAM_BYTES
                || (raw == 0) != (compressed == 0)
                || compressed > zstd::zstd_safe::compress_bound(raw)
            {
                return Err(other("Malformed OMT run block header"));
            }
            total += raw;
            *size = (compressed, raw);
        }
        // Every record takes at least a byte of keys.
        if records == 0 || records > sizes[KEYS].1 || total == 0 {
            return Err(other("Malformed OMT run block header"));
        }
        DECOMPRESS.with_borrow_mut(|state| {
            let (decompressor, frame) = match state {
                Some(state) => state,
                None => state.insert((zstd::bulk::Decompressor::new()?, Vec::new())),
            };
            for (stream, (compressed, raw)) in self.streams.iter_mut().zip(sizes) {
                stream.clear();
                if raw == 0 {
                    continue;
                }
                frame.resize(compressed, 0);
                self.input
                    .read_exact(frame)
                    .map_err(|e| other(format!("Truncated OMT run block: {e}")))?;
                stream.reserve_exact(raw);
                let size = decompressor
                    .decompress_to_buffer(frame.as_slice(), stream)
                    .map_err(|e| other(format!("Corrupt OMT run block: {e}")))?;
                if size != raw {
                    return Err(other("OMT run block has the wrong length"));
                }
            }
            Ok(())
        })?;
        self.positions = [0; STREAMS];
        self.records = records as u32;
        self.previous = Previous::default();
        Ok(true)
    }

    /// The next record, or `None` at the end of the run.
    pub(super) fn next(&mut self) -> Result<Option<EncodedRecord>, String> {
        if self.records == 0 {
            if self
                .positions
                .iter()
                .zip(&self.streams)
                .any(|(p, s)| *p != s.len())
            {
                return Err("Trailing bytes in OMT run block".into());
            }
            if !self.next_block().map_err(|e| e.to_string())? {
                return Ok(None);
            }
        }
        let mut readers: [Reader; STREAMS] =
            std::array::from_fn(|index| Reader::new(&self.streams[index][self.positions[index]..]));
        let [keys, sequences, payload_streams @ ..] = &mut readers;
        let tile_id = keys.varint()?.wrapping_add(self.previous.tile_id);
        let layer = match keys.u8()? {
            LAYER_VERBATIM => {
                let name = keys.str()?;
                if name.len() > MAX_LAYER_BYTES {
                    return Err("OMT record exceeds bounded frame limit".into());
                }
                Layer::new(name)
            }
            id if usize::from(id) < LAYER_NAMES.len() => Layer::Known(id),
            _ => return Err("Unknown OMT frame layer".into()),
        };
        let sort_key = keys
            .zigzag()?
            .wrapping_add(self.previous.sort_base(tile_id, &layer));
        let id =
            (keys.zigzag()? as u64).wrapping_add(self.previous.id_base(tile_id, &layer, sort_key));
        let source = (sequences.zigzag()? as u64).wrapping_add(self.previous.seq.source);
        let feature = sequences
            .zigzag()?
            .checked_add(i64::from(self.previous.feature_base(source)))
            .and_then(|feature| u32::try_from(feature).ok())
            .ok_or("Invalid OMT record sequence")?;
        let record = sequences
            .varint()?
            .try_into()
            .map_err(|_| "Invalid OMT record sequence")?;
        let mut payload = Vec::new();
        let payload_streams: &mut [Reader; PAYLOAD_STREAMS] = payload_streams;
        join_payload(payload_streams, &mut payload)?;
        if payload.len() > MAX_RECORD_BYTES {
            return Err("OMT record exceeds bounded record limit".into());
        }
        for (position, (reader, stream)) in self
            .positions
            .iter_mut()
            .zip(readers.iter().zip(&self.streams))
        {
            *position = stream.len() - reader.remaining();
        }
        self.records -= 1;
        let key = SortKey {
            tile_id,
            layer,
            sort_key,
            id,
            seq: Seq {
                source,
                feature,
                record,
            },
        };
        self.previous.set(&key);
        Ok(Some(EncodedRecord { key, payload }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::omt::record_codec::tests::Lcg;

    fn records(count: usize, sorted: bool) -> Vec<EncodedRecord> {
        let mut rng = Lcg(5);
        let mut records: Vec<_> = (0..count)
            .map(|index| {
                let mut record = rng.record();
                // Neighbours share tiles, layers and sort keys, as in a run.
                if index % 3 != 0 {
                    record.tile_id = index as u64 / 7;
                    record.layer = "building".into();
                    record.sort_key = (index % 2) as i64;
                }
                let seq = Seq {
                    source: index as u64 % 5,
                    feature: index as u32,
                    record: 0,
                };
                EncodedRecord::encode(&record, seq).unwrap()
            })
            .collect();
        if sorted {
            records.sort_unstable_by(|a, b| a.key.cmp(&b.key));
        }
        records
    }

    fn write(records: &[EncodedRecord]) -> Vec<u8> {
        let mut writer = RunWriter::new(vec![]).unwrap();
        for record in records {
            writer.write(record).unwrap();
        }
        writer.into_inner()
    }

    fn read(bytes: &[u8]) -> Result<Vec<EncodedRecord>, String> {
        let mut reader = RunReader::new(bytes);
        let mut out = vec![];
        while let Some(record) = reader.next()? {
            out.push(record);
        }
        Ok(out)
    }

    #[test]
    fn round_trips_across_blocks_sorted_or_not() {
        for (count, sorted) in [(0, true), (1, true), (5000, true), (5000, false)] {
            let records = records(count, sorted);
            let bytes = write(&records);
            let frames: usize = records.iter().map(EncodedRecord::frame_bytes).sum();
            if count == 5000 {
                assert!(frames > 3 * BLOCK_BYTES, "{frames} bytes of frames");
            }
            let read = read(&bytes).unwrap();
            assert_eq!(read.len(), records.len());
            for (a, b) in read.iter().zip(&records) {
                assert_eq!(a.key, b.key);
                assert_eq!(a.payload, b.payload);
            }
        }
    }

    #[test]
    fn file_runs_report_frame_bytes() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("run");
        let records = records(2000, true);
        let mut writer = RunWriter::create(&path).unwrap();
        for record in &records {
            writer.write(record).unwrap();
        }
        let frames: usize = records.iter().map(EncodedRecord::frame_bytes).sum();
        assert_eq!(writer.finish().unwrap(), frames as u64);
        let mut reader = RunReader::open(&path).unwrap();
        let mut count = 0;
        while reader.next().unwrap().is_some() {
            count += 1;
        }
        assert_eq!(count, records.len());
    }

    #[test]
    fn rejects_damage() {
        let bytes = write(&records(3000, true));
        assert!(bytes.len() > 2 * BLOCK_HEADER_BYTES);
        // Cut anywhere but between blocks.
        let first_block = MAGIC.len()
            + BLOCK_HEADER_BYTES
            + (0..STREAMS)
                .map(|i| {
                    let at = MAGIC.len() + 4 + 8 * i;
                    u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap()) as usize
                })
                .sum::<usize>();
        for end in [
            1,
            3,
            5,
            MAGIC.len() + BLOCK_HEADER_BYTES,
            first_block - 1,
            first_block + 1,
            bytes.len() - 1,
        ] {
            assert!(read(&bytes[..end]).is_err(), "prefix {end}");
        }
        assert_eq!(
            read(&bytes[..first_block]).unwrap().len() as u32,
            u32::from_le_bytes(bytes[4..8].try_into().unwrap())
        );
        let damaged = |at: usize, value: u32| {
            let mut copy = bytes.clone();
            copy[at..at + 4].copy_from_slice(&value.to_le_bytes());
            read(&copy)
        };
        // The record count, then a stream's lengths.
        assert!(damaged(4, 0).is_err());
        assert!(damaged(4, u32::MAX).is_err());
        assert!(damaged(4, 1).is_err());
        assert!(damaged(8, 0).is_err());
        assert!(damaged(12, MAX_STREAM_BYTES as u32 + 1).is_err());
        assert!(damaged(12, 1).is_err());
        assert!(damaged(0, u32::from_le_bytes(*b"OTR1")).is_err());
        assert!(read(&[]).is_err());
        assert!(read(&MAGIC).unwrap().is_empty());
    }

    #[test]
    fn refuses_records_beyond_the_limits() {
        let mut writer = RunWriter::new(vec![]).unwrap();
        let mut long = records(1, true).pop().unwrap();
        long.key.layer = Layer::Other("z".repeat(MAX_LAYER_BYTES + 1).into());
        assert!(writer.write(&long).is_err());
        let mut empty = records(1, true).pop().unwrap();
        empty.payload.clear();
        assert!(writer.write(&empty).is_err());
        assert!(read(&writer.into_inner()).unwrap().is_empty());
    }
}
