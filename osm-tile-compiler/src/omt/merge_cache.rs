//! Reuse of encoded tile layers between compiles.
//!
//! Merging a layer's features (`merge_geometries`) is the largest part of
//! encoding a tile, and after an OSM update almost every tile layer gets the
//! same records as before. A compile can write a cache index beside its
//! archive: for every tile, each layer's name, a hash of the exact records
//! that went into it, and the size of the layer it encoded. A later compile
//! given that archive hashes each layer's records the same way; where a hash
//! matches, it takes the layer's bytes from the old archive instead of
//! decoding, merging and encoding the records again. Where every layer of a
//! tile matches, the old tile's stored bytes are written as they are.
//!
//! The hash covers the build's code fingerprint (sources and lock file), the
//! tile, the layer's name and every record's sort key, id and payload in
//! order, which is everything the layer's encoding reads. A reused layer is
//! therefore the bytes this build would encode, and the archive is the same
//! as a compile without the cache.
//!
//! The index is a short header and a zstd stream of tile entries in tile
//! order; both the index and the old archive are read in step with the tiles
//! being encoded, so the cache costs little memory.
use super::store::EncodedRecord;
use crate::archive::{ArchiveReader, Entry};
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

/// The build's code fingerprint; caches from other builds are not reused.
pub(super) const FINGERPRINT: &str = env!("OTC_CODE_FINGERPRINT");
const MAGIC: &[u8; 8] = b"OTCMRGC1";
const ARCHIVE_HEADER_BYTES: usize = 127;
const ZSTD_LEVEL: i32 = 3;

/// The cache index beside an archive: `<NAME>.merge-cache`.
pub fn merge_cache_path(archive: &Path) -> PathBuf {
    archive.with_extension("merge-cache")
}

/// Whether a compile writes a merge cache index beside its archive, and
/// which earlier archive it reuses tile layers from.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
#[non_exhaustive]
pub struct MergeCacheOptions {
    /// Write `<NAME>.merge-cache` beside the archive ([`merge_cache_path`]),
    /// so that a later compile can reuse this one's tile layers.
    pub write: bool,
    /// An archive compiled with `write`, whose tile layers are reused where
    /// their input is unchanged; the archive written is the same as without
    /// it. Its index is written for the new archive too.
    pub reuse_from: Option<PathBuf>,
}

pub(super) type Key = [u8; 16];

/// A layer of a tile: its name, the hash of its records, and the bytes it
/// encoded to, 0 when it had no features and was left out of the tile.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct LayerEntry {
    pub name: Box<str>,
    pub key: Key,
    pub size: u32,
}

/// A tile: its layers in tile order (by name), and the bytes stored for it
/// in the archive, 0 when it was not written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct TileEntry {
    pub tile_id: u64,
    pub stored: u32,
    pub layers: Vec<LayerEntry>,
}

thread_local! {
    static SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// The hash of a layer's records, in the order the encoder receives them.
pub(super) fn layer_key(tile_id: u64, name: &str, records: &[EncodedRecord]) -> Key {
    let mut hash = Sha256::new();
    hash.update((FINGERPRINT.len() as u64).to_le_bytes());
    hash.update(FINGERPRINT.as_bytes());
    hash.update(tile_id.to_le_bytes());
    hash.update((name.len() as u64).to_le_bytes());
    hash.update(name.as_bytes());
    hash.update((records.len() as u64).to_le_bytes());
    // Records are small: hashing them as one buffer is faster than four
    // updates each.
    SCRATCH.with_borrow_mut(|bytes| {
        bytes.clear();
        for record in records {
            bytes.extend_from_slice(&record.key.sort_key.to_le_bytes());
            bytes.extend_from_slice(&record.key.id.to_le_bytes());
            bytes.extend_from_slice(&(record.payload.len() as u64).to_le_bytes());
            bytes.extend_from_slice(&record.payload);
        }
        hash.update(&bytes[..]);
        if bytes.capacity() > 64 << 20 {
            *bytes = Vec::new();
        }
    });
    let digest = hash.finalize();
    let mut key = [0; 16];
    key.copy_from_slice(&digest[..16]);
    key
}

/// What a compile did with the cache.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[non_exhaustive]
pub struct MergeCacheStats {
    /// Tiles whose stored bytes were copied from the old archive.
    pub tiles_reused: u64,
    /// Tile layers taken from the old archive, those of reused tiles
    /// included.
    pub layers_reused: u64,
    /// Tile layers merged and encoded.
    pub layers_encoded: u64,
    /// Bytes of the index written beside the archive, 0 when none was.
    pub index_bytes: u64,
}

#[derive(Default)]
pub(super) struct Counters {
    pub tiles_reused: AtomicU64,
    pub layers_reused: AtomicU64,
    pub layers_encoded: AtomicU64,
}
impl Counters {
    pub fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }
    pub fn stats(&self, index_bytes: u64) -> MergeCacheStats {
        MergeCacheStats {
            tiles_reused: self.tiles_reused.load(Ordering::Relaxed),
            layers_reused: self.layers_reused.load(Ordering::Relaxed),
            layers_encoded: self.layers_encoded.load(Ordering::Relaxed),
            index_bytes,
        }
    }
}

/// The cache of one compile: what it reuses and where it writes its index.
pub(super) struct MergeCache {
    pub reuse: Option<Reuse>,
    pub writer: Option<IndexWriter>,
    pub counters: Counters,
}
impl MergeCache {
    /// The cache `options` ask for, or `None`; a reason the old index cannot
    /// be used goes to `warnings`, and the compile then encodes every tile.
    pub fn open(
        options: &MergeCacheOptions,
        temp_dir: &Path,
        gzip_level: u32,
        warnings: &mut Vec<String>,
    ) -> Result<Option<Self>, String> {
        if !options.write && options.reuse_from.is_none() {
            return Ok(None);
        }
        let reuse = match &options.reuse_from {
            Some(archive) => match Reuse::open(archive, gzip_level)? {
                Ok(reuse) => Some(reuse),
                Err(reason) => {
                    warnings.push(reason);
                    None
                }
            },
            None => None,
        };
        Ok(Some(Self {
            reuse,
            writer: Some(IndexWriter::create(temp_dir)?),
            counters: Counters::default(),
        }))
    }
}

fn put_varint(out: &mut impl Write, mut value: u64) -> std::io::Result<()> {
    let mut bytes = [0u8; 10];
    let mut n = 0;
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            bytes[n] = byte;
            n += 1;
            break;
        }
        bytes[n] = byte | 0x80;
        n += 1;
    }
    out.write_all(&bytes[..n])
}

/// A varint, or `None` at the end of the stream before its first byte.
fn get_varint(input: &mut impl Read) -> Result<Option<u64>, String> {
    let mut value = 0u64;
    for (i, shift) in (0..64).step_by(7).enumerate() {
        let mut byte = [0u8];
        match input.read(&mut byte) {
            Ok(0) if i == 0 => return Ok(None),
            Ok(0) => return Err("merge cache ends inside a number".into()),
            Ok(_) => {}
            Err(e) => return Err(format!("Failed to read the merge cache: {e}")),
        }
        value |= u64::from(byte[0] & 0x7f) << shift;
        if byte[0] < 0x80 {
            return Ok(Some(value));
        }
    }
    Err("merge cache number longer than 10 bytes".into())
}

fn need(value: Option<u64>) -> Result<u64, String> {
    value.ok_or_else(|| "merge cache ends inside a tile".to_string())
}

/// Writes the tile entries of a compile, in tile order, to a body file in
/// the scratch directory; `finish` puts the header before it.
pub(super) struct IndexWriter {
    body_path: PathBuf,
    body: zstd::Encoder<'static, BufWriter<File>>,
    last_tile: Option<u64>,
}
impl IndexWriter {
    pub fn create(temp_dir: &Path) -> Result<Self, String> {
        let body_path = temp_dir.join("merge-cache.body");
        let file = File::create(&body_path)
            .map_err(|e| format!("Failed to create {}: {e}", body_path.display()))?;
        let body = zstd::Encoder::new(BufWriter::new(file), ZSTD_LEVEL)
            .map_err(|e| format!("Failed to start the merge cache: {e}"))?;
        Ok(Self {
            body_path,
            body,
            last_tile: None,
        })
    }

    pub fn add(&mut self, tile: &TileEntry) -> Result<(), String> {
        if self.last_tile.is_some_and(|last| tile.tile_id <= last) {
            return Err("merge cache tiles out of order".into());
        }
        let delta = tile.tile_id - self.last_tile.map_or(0, |last| last + 1);
        self.last_tile = Some(tile.tile_id);
        let out = &mut self.body;
        let write = |out: &mut zstd::Encoder<'static, BufWriter<File>>| -> std::io::Result<()> {
            put_varint(out, delta)?;
            put_varint(out, u64::from(tile.stored))?;
            put_varint(out, tile.layers.len() as u64)?;
            for layer in &tile.layers {
                put_varint(out, layer.name.len() as u64)?;
                out.write_all(layer.name.as_bytes())?;
                out.write_all(&layer.key)?;
                put_varint(out, u64::from(layer.size))?;
            }
            Ok(())
        };
        write(out).map_err(|e| format!("Failed to write the merge cache: {e}"))
    }

    /// Writes the index to `path` for the finished archive `archive`, and
    /// returns its size.
    pub fn finish(self, path: &Path, gzip_level: u32, archive: &Path) -> Result<u64, String> {
        let io = |e: std::io::Error| format!("Failed to write {}: {e}", path.display());
        let mut body = self.body.finish().map_err(io)?;
        body.flush().map_err(io)?;
        drop(body);
        let mut archive = File::open(archive)
            .map_err(|e| format!("Failed to read {}: {e}", archive.display()))?;
        let archive_length = archive.seek(SeekFrom::End(0)).map_err(io)?;
        let mut header = [0u8; ARCHIVE_HEADER_BYTES];
        archive.seek(SeekFrom::Start(0)).map_err(io)?;
        archive.read_exact(&mut header).map_err(io)?;
        let mut out = BufWriter::new(File::create(path).map_err(io)?);
        out.write_all(MAGIC).map_err(io)?;
        out.write_all(&(FINGERPRINT.len() as u16).to_le_bytes())
            .map_err(io)?;
        out.write_all(FINGERPRINT.as_bytes()).map_err(io)?;
        out.write_all(&[gzip_level as u8]).map_err(io)?;
        out.write_all(&archive_length.to_le_bytes()).map_err(io)?;
        out.write_all(&header).map_err(io)?;
        let mut body = File::open(&self.body_path).map_err(io)?;
        std::io::copy(&mut body, &mut out).map_err(io)?;
        out.flush().map_err(io)?;
        drop(body);
        let _ = std::fs::remove_file(&self.body_path);
        Ok(std::fs::metadata(path).map_err(io)?.len())
    }
}

/// A tile the old archive holds an entry for.
pub(super) struct OldTile {
    pub entry: TileEntry,
    /// Where its stored bytes are in the old archive file.
    pub offset: u64,
    /// Whether its stored bytes can be copied: written at the same gzip level.
    pub copy: bool,
}

/// The old archive and its index, read in tile order.
pub(super) struct Reuse {
    pub archive: PathBuf,
    index: BufReader<zstd::Decoder<'static, BufReader<BufReader<File>>>>,
    next: Option<TileEntry>,
    last_tile: Option<u64>,
    entries: Vec<Entry>,
    entry: usize,
    data_offset: u64,
    same_gzip: bool,
}
impl Reuse {
    /// Opens `archive` and the index beside it for a compile at
    /// `gzip_level`. `Ok(Err(reason))` when the index is from another build
    /// and cannot be used; an error when it does not belong to the archive.
    pub fn open(archive: &Path, gzip_level: u32) -> Result<Result<Self, String>, String> {
        let index_path = merge_cache_path(archive);
        let io = |e: std::io::Error| format!("Failed to read {}: {e}", index_path.display());
        let file = File::open(&index_path).map_err(io)?;
        let mut input = BufReader::new(file);
        let mut magic = [0u8; 8];
        input.read_exact(&mut magic).map_err(io)?;
        if &magic != MAGIC {
            return Ok(Err(format!(
                "{} is not a merge cache this build reads",
                index_path.display()
            )));
        }
        let mut length = [0u8; 2];
        input.read_exact(&mut length).map_err(io)?;
        let mut fingerprint = vec![0u8; usize::from(u16::from_le_bytes(length))];
        input.read_exact(&mut fingerprint).map_err(io)?;
        if fingerprint != FINGERPRINT.as_bytes() {
            return Ok(Err(format!(
                "{} was written by another build ({}); every tile is encoded again",
                index_path.display(),
                String::from_utf8_lossy(&fingerprint)
            )));
        }
        let mut level = [0u8];
        input.read_exact(&mut level).map_err(io)?;
        let mut length = [0u8; 8];
        input.read_exact(&mut length).map_err(io)?;
        let mut header = [0u8; ARCHIVE_HEADER_BYTES];
        input.read_exact(&mut header).map_err(io)?;
        let mut reader = ArchiveReader::open(archive)?;
        let mut actual = [0u8; ARCHIVE_HEADER_BYTES];
        File::open(archive)
            .and_then(|mut f| f.read_exact(&mut actual))
            .map_err(|e| format!("Failed to read {}: {e}", archive.display()))?;
        if u64::from_le_bytes(length) != reader.file_length || actual != header {
            return Err(format!(
                "{} does not belong to {}: it was written for another archive",
                index_path.display(),
                archive.display()
            ));
        }
        let mut entries = Vec::new();
        reader.visit_entries(|_, entry| {
            entries.push(entry);
            Ok(())
        })?;
        if entries.windows(2).any(|w| w[0].tile_id >= w[1].tile_id) {
            return Err(format!("{} is not in tile order", archive.display()));
        }
        let index = zstd::Decoder::new(input).map_err(io)?;
        let mut reuse = Self {
            archive: archive.to_path_buf(),
            index: BufReader::new(index),
            next: None,
            last_tile: None,
            entries,
            entry: 0,
            data_offset: reader.header.data_offset,
            same_gzip: u32::from(level[0]) == gzip_level,
        };
        reuse.advance()?;
        Ok(Ok(reuse))
    }

    fn read_entry(&mut self) -> Result<Option<TileEntry>, String> {
        if self.index.fill_buf().map_err(|e| e.to_string())?.is_empty() {
            return Ok(None);
        }
        let input = &mut self.index;
        let delta = need(get_varint(input)?)?;
        let tile_id = self
            .last_tile
            .map_or(Some(0), |last| last.checked_add(1))
            .and_then(|base| base.checked_add(delta))
            .ok_or("merge cache tile id overflows")?;
        self.last_tile = Some(tile_id);
        let stored = u32::try_from(need(get_varint(input)?)?).map_err(|e| e.to_string())?;
        let count = need(get_varint(input)?)?;
        if count > 1024 {
            return Err("merge cache tile has too many layers".into());
        }
        let mut layers = Vec::with_capacity(count as usize);
        for _ in 0..count {
            let length = need(get_varint(input)?)?;
            if length > 1024 {
                return Err("merge cache layer name is too long".into());
            }
            let mut name = vec![0u8; length as usize];
            let mut key = [0u8; 16];
            input
                .read_exact(&mut name)
                .and_then(|()| input.read_exact(&mut key))
                .map_err(|_| "merge cache ends inside a tile".to_string())?;
            let size = u32::try_from(need(get_varint(input)?)?).map_err(|e| e.to_string())?;
            let name =
                String::from_utf8(name).map_err(|_| "merge cache layer name is not UTF-8")?;
            layers.push(LayerEntry {
                name: name.into(),
                key,
                size,
            });
        }
        Ok(Some(TileEntry {
            tile_id,
            stored,
            layers,
        }))
    }

    fn advance(&mut self) -> Result<(), String> {
        self.next = self.read_entry()?;
        Ok(())
    }

    /// The old tile `tile_id`; tiles must be asked for in increasing order.
    pub fn take(&mut self, tile_id: u64) -> Result<Option<OldTile>, String> {
        while self
            .next
            .as_ref()
            .is_some_and(|next| next.tile_id < tile_id)
        {
            self.advance()?;
        }
        if self
            .next
            .as_ref()
            .is_none_or(|next| next.tile_id != tile_id)
        {
            return Ok(None);
        }
        let entry = self.next.take().expect("checked above");
        self.advance()?;
        let mut offset = 0;
        if entry.stored > 0 {
            while self
                .entries
                .get(self.entry)
                .is_some_and(|e| e.tile_id.saturating_add(u64::from(e.run_length)) <= tile_id)
            {
                self.entry += 1;
            }
            let stored = self
                .entries
                .get(self.entry)
                .filter(|e| e.tile_id <= tile_id && e.length == entry.stored)
                .ok_or_else(|| {
                    format!(
                        "{} lacks tile {tile_id} that its merge cache lists",
                        self.archive.display()
                    )
                })?;
            offset = self.data_offset + stored.offset;
        }
        Ok(Some(OldTile {
            entry,
            offset,
            copy: self.same_gzip,
        }))
    }
}

/// Reads an old tile's stored bytes from the archive, opening it on first use.
pub(super) fn read_stored(
    file: &mut Option<File>,
    archive: &Path,
    tile: &OldTile,
) -> Result<Vec<u8>, String> {
    let io = |e: std::io::Error| format!("Failed to read {}: {e}", archive.display());
    if file.is_none() {
        *file = Some(File::open(archive).map_err(io)?);
    }
    let file = file.as_mut().expect("opened above");
    let mut bytes = vec![0; tile.entry.stored as usize];
    file.seek(SeekFrom::Start(tile.offset)).map_err(io)?;
    file.read_exact(&mut bytes).map_err(io)?;
    Ok(bytes)
}

/// The layer bytes of an old tile: its stored bytes unzipped, which must be
/// its layers' sizes in all.
pub(super) fn read_layers(
    file: &mut Option<File>,
    archive: &Path,
    tile: &OldTile,
) -> Result<Vec<u8>, String> {
    let stored = read_stored(file, archive, tile)?;
    let mut bytes = Vec::new();
    flate2::read::GzDecoder::new(&stored[..])
        .read_to_end(&mut bytes)
        .map_err(|e| {
            format!(
                "{}: tile {} is not gzip: {e}",
                archive.display(),
                tile.entry.tile_id
            )
        })?;
    let total: u64 = tile.entry.layers.iter().map(|l| u64::from(l.size)).sum();
    if bytes.len() as u64 != total {
        return Err(format!(
            "{}: tile {} does not match its merge cache entry",
            archive.display(),
            tile.entry.tile_id
        ));
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn varints_round_trip() {
        let mut bytes = Vec::new();
        for value in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            put_varint(&mut bytes, value).unwrap();
        }
        let mut input = &bytes[..];
        for value in [0, 1, 127, 128, 300, u64::from(u32::MAX), u64::MAX] {
            assert_eq!(get_varint(&mut input).unwrap(), Some(value));
        }
        assert_eq!(get_varint(&mut input).unwrap(), None);
    }
}
