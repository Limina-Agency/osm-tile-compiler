//! Delta archives (experimental): the bytes that turn one PMTiles archive into
//! a newer one, so a device holding the old archive downloads only what
//! changed.
//!
//! A delta rebuilds the new archive byte for byte. It doesn't re-encode
//! anything: every part of the new archive (its header, directories,
//! metadata and each distinct tile) is either copied from the old archive,
//! where the old one holds the same bytes anywhere, or carried in the delta.
//! So it works for any PMTiles v3 archive, whichever writer made it, and the
//! result is checked against the new archive's SHA-256 before it is kept.
//!
//! The file, all integers little-endian:
//!
//! ```text
//! magic        8 bytes  "OTCDELTA"
//! version      u32      DELTA_VERSION
//! manifest     u32 length, then that many bytes of JSON (`DeltaManifest`)
//! operations   until the end operation, each led by a tag byte:
//!   0x00 end
//!   0x01 copy     varint offset, varint length: bytes of the old archive
//!   0x02 literal  varint length, then that many bytes
//!   0x03 patch    varint offset, varint length: an old tile's stored bytes;
//!                 u8 its compression (1 none, 2 gzip); u8 how the new tile
//!                 is stored (a gzip level, or 255 for no compression);
//!                 varint the new tile's stored length; varint patch length,
//!                 then the patch (`tile_patch`)
//! ```
//!
//! The operations, run in order, write the new archive from its first byte to
//! its last.
//!
//! A changed tile travels as a patch against the old tile at the same
//! position when the new tile's stored bytes can be reproduced from its
//! decoded bytes, as they can for archives this compiler wrote, and the
//! patch is smaller. Edits rarely change more than a little of a tile, so
//! this is what keeps a delta small.
use super::reader::{ArchiveReader, Entry};
use rayon::prelude::*;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// The manifest's `format`.
pub const DELTA_FORMAT: &str = "osm-tile-compiler-delta";
/// The file layout's version; see `docs/formats.md` for when it changes.
pub const DELTA_VERSION: u32 = 1;
const MAGIC: &[u8; 8] = b"OTCDELTA";
/// A manifest is a few hundred bytes; anything far larger is not a delta.
const MAX_MANIFEST_BYTES: u32 = 1 << 20;
const TAG_END: u8 = 0;
const TAG_COPY: u8 = 1;
const TAG_LITERAL: u8 = 2;
const TAG_PATCH: u8 = 3;
const BUFFER_BYTES: usize = 1 << 20;

/// An archive a delta names: its size and SHA-256, in lowercase hex.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArchiveDigest {
    pub bytes: u64,
    pub sha256: String,
}

/// How the tiles changed between the two archives, counted by tile ID: a
/// tile is changed when both archives address it with different bytes.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TileChanges {
    pub unchanged: u64,
    pub changed: u64,
    pub added: u64,
    pub removed: u64,
}

/// What a delta describes, stored at its start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeltaManifest {
    /// Always `DELTA_FORMAT`.
    pub format: String,
    /// Always `DELTA_VERSION`.
    pub version: u32,
    /// The archive the delta applies to.
    pub base: ArchiveDigest,
    /// The archive applying it writes.
    pub target: ArchiveDigest,
    /// Bytes of the target copied from the base.
    pub copied_bytes: u64,
    /// Bytes of the target the delta carries as they are.
    pub literal_bytes: u64,
    /// Bytes of the target rebuilt from tiles of the base and patches.
    #[serde(default)]
    pub patched_bytes: u64,
    /// How many tiles were rebuilt from patches.
    #[serde(default)]
    pub patched_tiles: u64,
    pub tiles: TileChanges,
}

/// What `diff` wrote.
#[derive(Debug, Clone, Serialize)]
pub struct DiffReport {
    pub delta_bytes: u64,
    #[serde(flatten)]
    pub manifest: DeltaManifest,
}

/// What `apply` wrote.
#[derive(Debug, Clone, Serialize)]
pub struct ApplyReport {
    pub delta_bytes: u64,
    #[serde(flatten)]
    pub manifest: DeltaManifest,
}

type Key = [u8; 16];

fn key(bytes: &[u8]) -> Key {
    Sha256::digest(bytes)[..16].try_into().unwrap()
}

/// A part of an archive file: the header, a directory, the metadata or a
/// tile's bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
struct Segment {
    offset: u64,
    length: u64,
}

/// An archive's parts and its tile entries, in tile ID order.
struct Layout {
    file_length: u64,
    tile_compression: u8,
    segments: Vec<Segment>,
    /// Tile entries, each with its data offset made absolute.
    tiles: Vec<Entry>,
}

fn layout(path: &Path) -> Result<Layout, String> {
    let mut reader = ArchiveReader::open(path)?;
    let header = reader.header.clone();
    let mut segments = vec![
        Segment {
            offset: 0,
            length: 127,
        },
        Segment {
            offset: header.root_offset,
            length: header.root_length,
        },
        Segment {
            offset: header.metadata_offset,
            length: header.metadata_length,
        },
    ];
    let mut tiles = Vec::new();
    reader.walk(|_, entry, leaf| {
        if leaf {
            segments.push(Segment {
                offset: header.leaf_offset + entry.offset,
                length: u64::from(entry.length),
            });
        } else {
            if entry.offset + u64::from(entry.length) > header.data_length {
                return Err(format!(
                    "tile {} lies beyond the data section",
                    entry.tile_id
                ));
            }
            tiles.push(Entry {
                offset: header.data_offset + entry.offset,
                ..entry
            });
            segments.push(Segment {
                offset: header.data_offset + entry.offset,
                length: u64::from(entry.length),
            });
        }
        Ok(())
    })?;
    segments.retain(|s| s.length > 0);
    segments.sort_unstable();
    segments.dedup();
    if let Some(last) = segments.last()
        && last.offset + last.length > reader.file_length
    {
        return Err(format!("{} is truncated", path.display()));
    }
    tiles.sort_unstable_by_key(|e| e.tile_id);
    Ok(Layout {
        file_length: reader.file_length,
        tile_compression: header.tile_compression,
        segments,
        tiles,
    })
}

/// Reads parts of a file mostly in order, without a system call per part.
struct Cursor {
    inner: BufReader<File>,
    position: u64,
    /// The file's length, where the caller needs it.
    length: u64,
}

impl Cursor {
    fn open(path: &Path) -> Result<Self, String> {
        let file =
            File::open(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
        Ok(Self {
            inner: BufReader::with_capacity(BUFFER_BYTES, file),
            position: 0,
            length: 0,
        })
    }

    fn seek(&mut self, offset: u64) -> std::io::Result<()> {
        if offset != self.position {
            let ahead = offset.wrapping_sub(self.position);
            if offset > self.position && ahead < BUFFER_BYTES as u64 {
                self.inner.seek_relative(ahead as i64)?;
            } else {
                self.inner.seek(SeekFrom::Start(offset))?;
            }
            self.position = offset;
        }
        Ok(())
    }

    fn read(&mut self, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
        self.seek(offset)?;
        let mut bytes = vec![0; length as usize];
        self.inner.read_exact(&mut bytes)?;
        self.position += length;
        Ok(bytes)
    }

    /// Copies `length` bytes at `offset` to `out`.
    fn copy_to(&mut self, offset: u64, length: u64, out: &mut impl Write) -> std::io::Result<()> {
        self.seek(offset)?;
        let copied = std::io::copy(&mut (&mut self.inner).take(length), out)?;
        self.position += copied;
        if copied != length {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        Ok(())
    }
}

fn digest(path: &Path) -> Result<ArchiveDigest, String> {
    let mut file =
        File::open(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
    let mut hasher = HashingWriter::new(std::io::sink());
    let bytes = std::io::copy(&mut file, &mut hasher)
        .map_err(|e| format!("Failed to read {}: {e}", path.display()))?;
    Ok(ArchiveDigest {
        bytes,
        sha256: hasher.finish(),
    })
}

/// Hashes and counts what passes through it.
struct HashingWriter<W> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        Self {
            inner,
            hasher: Sha256::new(),
            bytes: 0,
        }
    }

    fn finish(self) -> String {
        hex::encode(self.hasher.finalize())
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let written = self.inner.write(bytes)?;
        self.hasher.update(&bytes[..written]);
        self.bytes += written as u64;
        Ok(written)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Operation {
    /// Bytes of the base.
    Copy { offset: u64, length: u64 },
    /// Bytes of the target, carried in the delta.
    Literal { offset: u64, length: u64 },
    /// A tile of the target, rebuilt from a tile of the base and a patch.
    Patch(TilePatch),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct TilePatch {
    /// Where the base tile's stored bytes are.
    base: Segment,
    /// The base tile's compression, as the PMTiles header codes it.
    base_compression: u8,
    /// How the rebuilt tile is stored: a gzip level, or `STORED`.
    level: u8,
    /// The rebuilt tile's stored size.
    target_length: u64,
    patch: Vec<u8>,
}

/// A patch's `level` for tiles stored without compression.
const STORED: u8 = 255;
/// No tile, decoded, is larger; a delta asking for more is corrupt.
const MAX_TILE_BYTES: usize = 256 << 20;

/// Appends `next`, merging it into the last operation when they continue
/// each other.
fn push(operations: &mut Vec<Operation>, next: Operation) {
    use Operation::{Copy, Literal};
    if let (
        Some(Copy { offset, length }),
        Copy {
            offset: next_offset,
            length: next_length,
        },
    )
    | (
        Some(Literal { offset, length }),
        Literal {
            offset: next_offset,
            length: next_length,
        },
    ) = (operations.last_mut(), &next)
        && *offset + *length == *next_offset
    {
        *length += *next_length;
        return;
    }
    operations.push(next);
}

/// A tile's decoded bytes, for the compressions patches support.
fn decode_tile(bytes: &[u8], compression: u8) -> Option<Vec<u8>> {
    match compression {
        1 | 2 => super::reader::decompress(bytes, compression).ok(),
        _ => None,
    }
}

/// Stores a decoded tile as the compiler does: gzip at `level`, or as it is.
fn encode_tile(raw: &[u8], level: u8) -> Vec<u8> {
    if level == STORED {
        return raw.to_vec();
    }
    let mut encoder = flate2::write::GzEncoder::new(
        Vec::with_capacity(raw.len() / 2),
        flate2::Compression::new(u32::from(level)),
    );
    encoder.write_all(raw).unwrap();
    encoder.finish().unwrap()
}

/// Reads `length` bytes at `offset` without a shared position, so many
/// threads can read one file at once.
fn read_at(file: &File, offset: u64, length: u64) -> std::io::Result<Vec<u8>> {
    let mut bytes = vec![0; length as usize];
    #[cfg(unix)]
    std::os::unix::fs::FileExt::read_exact_at(file, &mut bytes, offset)?;
    #[cfg(windows)]
    {
        let mut done = 0;
        while done < bytes.len() {
            let read = std::os::windows::fs::FileExt::seek_read(
                file,
                &mut bytes[done..],
                offset + done as u64,
            )?;
            if read == 0 {
                return Err(std::io::ErrorKind::UnexpectedEof.into());
            }
            done += read;
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        let _held = LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let mut file = file;
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut bytes)?;
    }
    Ok(bytes)
}

/// The first gzip level, tried in the order `first`, 6, then 0 to 9, that
/// `reproduces`; `tried` remembers each level's answer so no level is
/// encoded twice for a tile.
fn find_level(
    first: Option<u8>,
    tried: &mut [Option<bool>; 10],
    mut reproduces: impl FnMut(u8) -> bool,
) -> Option<u8> {
    first
        .into_iter()
        .chain([6])
        .chain(0..=9)
        .find(|&level| *tried[usize::from(level)].get_or_insert_with(|| reproduces(level)))
}

/// What patching one changed tile would give, worked out without the
/// patcher's running state so that tiles can be tried in parallel.
/// `Patcher::settle` then takes the trials in tile order and decides each
/// exactly as trying the tiles one after another would.
enum Trial {
    /// Not tried: the patcher had given up, or the part is no tile.
    Skipped,
    /// No base tile at the position, or a tile that can't be decoded.
    Unpatchable,
    /// Reading the base tile failed.
    Failed(String),
    /// No gzip level stores the decoded tile as the target stores it.
    Unreproducible,
    Reproducible {
        base: Segment,
        /// Each gzip level's answer, for the levels tried so far.
        tried: [Option<bool>; 10],
        /// The patch, when it is smaller than the tile and rebuilds it.
        patch: Option<Vec<u8>>,
    },
}

/// Finds patches for the target's changed tiles. A patch needs the target's
/// stored bytes to be reproducible from the decoded tile, which holds for
/// archives this compiler wrote; for others it soon stops trying.
struct Patcher<'a> {
    base_tiles: &'a [Entry],
    base_file: &'a File,
    base_compression: u8,
    target_compression: u8,
    /// The gzip level that last reproduced a tile.
    level: Option<u8>,
    hits: u32,
    misses: u32,
}

impl Patcher<'_> {
    fn base_tile(&self, tile_id: u64) -> Option<Segment> {
        let at = self.base_tiles.partition_point(|e| e.tile_id <= tile_id);
        let entry = self.base_tiles.get(at.checked_sub(1)?)?;
        (tile_id < entry.tile_id + u64::from(entry.run_length)).then_some(Segment {
            offset: entry.offset,
            length: u64::from(entry.length),
        })
    }

    /// Whether it has stopped trying: 16 tiles in, none reproducible.
    fn given_up(&self) -> bool {
        self.hits == 0 && self.misses >= 16
    }

    /// Tries a patch for the target tile `tile_id`, stored as `stored`,
    /// trying the gzip level `guess` first. Changes nothing, so it can run on
    /// many tiles at once.
    fn trial(&self, guess: Option<u8>, tile_id: u64, stored: &[u8]) -> Trial {
        let Some(base) = self.base_tile(tile_id) else {
            return Trial::Unpatchable;
        };
        let old = match read_at(self.base_file, base.offset, base.length) {
            Ok(old) => old,
            Err(e) => return Trial::Failed(format!("Failed to read the old archive: {e}")),
        };
        let (Some(old), Some(new)) = (
            decode_tile(&old, self.base_compression),
            decode_tile(stored, self.target_compression),
        ) else {
            return Trial::Unpatchable;
        };
        let mut tried = [None; 10];
        if self.target_compression != 1
            && find_level(guess, &mut tried, |level| {
                encode_tile(&new, level) == stored
            })
            .is_none()
        {
            return Trial::Unreproducible;
        }
        let patch = super::tile_patch::encode(&old, &new);
        let patch = (patch.len() + 32 < stored.len()
            && super::tile_patch::decode(&old, &patch, new.len()).as_deref() == Ok(&new[..]))
        .then_some(patch);
        Trial::Reproducible { base, tried, patch }
    }

    /// Decides the patch for a tile stored as `stored` from its trial, in
    /// tile order: the patch, if any, that trying the tiles one after
    /// another would have found.
    fn settle(&mut self, trial: Trial, stored: &[u8]) -> Result<Option<TilePatch>, String> {
        if self.given_up() {
            return Ok(None);
        }
        let (base, mut tried, patch) = match trial {
            Trial::Skipped | Trial::Unpatchable => return Ok(None),
            Trial::Failed(error) => return Err(error),
            Trial::Unreproducible => {
                self.misses += 1;
                return Ok(None);
            }
            Trial::Reproducible { base, tried, patch } => (base, tried, patch),
        };
        let level = if self.target_compression == 1 {
            STORED
        } else {
            // The trial guessed which level the tiles before would leave
            // here; when it guessed wrong, try the levels in this order,
            // encoding those the trial didn't.
            let mut new = None;
            let level = find_level(self.level, &mut tried, |level| {
                let new = new.get_or_insert_with(|| {
                    decode_tile(stored, self.target_compression).unwrap_or_default()
                });
                encode_tile(new, level) == stored
            })
            .ok_or("a tile's gzip level was lost between trying and settling")?;
            self.level = Some(level);
            level
        };
        let Some(patch) = patch else {
            return Ok(None);
        };
        self.hits += 1;
        Ok(Some(TilePatch {
            base,
            base_compression: self.base_compression,
            level,
            target_length: stored.len() as u64,
            patch,
        }))
    }
}

/// Bytes of the target between its parts, such as a writer's padding: copied
/// when the base holds the same bytes at the same place.
fn gap(
    base: &mut Cursor,
    target: &mut Cursor,
    offset: u64,
    length: u64,
) -> Result<Operation, String> {
    let failed = |e: std::io::Error| format!("Failed to read an archive: {e}");
    if offset + length <= base.length
        && length <= BUFFER_BYTES as u64
        && base.read(offset, length).map_err(failed)?
            == target.read(offset, length).map_err(failed)?
    {
        return Ok(Operation::Copy { offset, length });
    }
    Ok(Operation::Literal { offset, length })
}

/// Counts tile changes by walking both archives' entries in tile ID order.
fn tile_changes(
    base: &[Entry],
    base_keys: &HashMap<Segment, Key>,
    target: &[Entry],
    target_keys: &HashMap<Segment, Key>,
) -> TileChanges {
    let expand = |entries: &[Entry], keys: &HashMap<Segment, Key>| {
        let keyed: Vec<(Entry, Key)> = entries
            .iter()
            .map(|e| {
                let segment = Segment {
                    offset: e.offset,
                    length: u64::from(e.length),
                };
                (*e, keys.get(&segment).copied().unwrap_or_default())
            })
            .collect();
        keyed.into_iter().flat_map(|(e, k)| {
            (e.tile_id..e.tile_id + u64::from(e.run_length)).map(move |id| (id, k))
        })
    };
    let mut changes = TileChanges::default();
    let mut base = expand(base, base_keys).peekable();
    let mut target = expand(target, target_keys).peekable();
    loop {
        match (base.peek().copied(), target.peek().copied()) {
            (None, None) => break,
            (Some(_), None) => {
                changes.removed += 1;
                base.next();
            }
            (None, Some(_)) => {
                changes.added += 1;
                target.next();
            }
            (Some((b, bk)), Some((t, tk))) => {
                if b < t {
                    changes.removed += 1;
                    base.next();
                } else if t < b {
                    changes.added += 1;
                    target.next();
                } else {
                    if bk == tk {
                        changes.unchanged += 1;
                    } else {
                        changes.changed += 1;
                    }
                    base.next();
                    target.next();
                }
            }
        }
    }
    changes
}

fn write_varint(out: &mut impl Write, mut value: u64) -> std::io::Result<()> {
    while value >= 0x80 {
        out.write_all(&[(value as u8) | 0x80])?;
        value >>= 7;
    }
    out.write_all(&[value as u8])
}

fn read_varint(input: &mut impl Read) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let mut byte = [0u8];
        input
            .read_exact(&mut byte)
            .map_err(|_| "the delta ends inside an operation".to_string())?;
        value |= u64::from(byte[0] & 0x7f) << shift;
        if byte[0] < 0x80 {
            return Ok(value);
        }
    }
    Err("the delta holds a varint longer than 10 bytes".into())
}

/// A file written beside its destination and moved into place only once it
/// is complete; dropped unfinished, it is removed.
struct PendingFile {
    temporary: PathBuf,
    destination: PathBuf,
    kept: bool,
}

impl PendingFile {
    fn create(destination: &Path) -> Result<(Self, File), String> {
        if destination.exists() {
            return Err(format!("{} already exists", destination.display()));
        }
        let mut name = destination
            .file_name()
            .ok_or_else(|| format!("{} is not a file path", destination.display()))?
            .to_os_string();
        name.push(".partial");
        let temporary = destination.with_file_name(name);
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(|e| format!("Failed to create {}: {e}", temporary.display()))?;
        Ok((
            Self {
                temporary,
                destination: destination.to_owned(),
                kept: false,
            },
            file,
        ))
    }

    fn keep(mut self, file: File) -> Result<(), String> {
        file.sync_all()
            .map_err(|e| format!("Failed to write {}: {e}", self.temporary.display()))?;
        drop(file);
        if self.destination.exists() {
            return Err(format!("{} already exists", self.destination.display()));
        }
        std::fs::rename(&self.temporary, &self.destination)
            .map_err(|e| format!("Failed to write {}: {e}", self.destination.display()))?;
        self.kept = true;
        Ok(())
    }
}

impl Drop for PendingFile {
    fn drop(&mut self) {
        if !self.kept {
            let _ = std::fs::remove_file(&self.temporary);
        }
    }
}

/// Target bytes `diff` reads before trying a batch's tiles, and `apply`
/// before rebuilding a batch's patched tiles: enough to keep every thread
/// busy, little enough to hold.
const BATCH_BYTES: u64 = 32 << 20;

/// A part of the target in a batch `diff` is working on.
enum Part {
    /// Its operation is already known: a gap or an overlap.
    Ready(Operation),
    /// A part read, waiting to be hashed and perhaps patched.
    Read { segment: Segment, bytes: Vec<u8> },
}

/// Writes the delta that turns the archive `base` into `target` to `output`,
/// which must not exist yet. Before keeping it, rebuilds `target` from it
/// and checks the result.
pub fn diff(base: &Path, target: &Path, output: &Path) -> Result<DiffReport, String> {
    let ((base_digest, target_digest), (base_layout, target_layout)) = rayon::join(
        || rayon::join(|| digest(base), || digest(target)),
        || rayon::join(|| layout(base), || layout(target)),
    );
    let (base_digest, target_digest) = (base_digest?, target_digest?);
    let (base_layout, target_layout) = (base_layout?, target_layout?);

    // Every distinct part of the base, by content. The parts are read in
    // order, hashed in parallel and indexed in order, so the first of equal
    // parts is the one kept.
    let mut base_index: HashMap<Key, Segment> = HashMap::with_capacity(base_layout.segments.len());
    let mut base_keys: HashMap<Segment, Key> = HashMap::with_capacity(base_layout.segments.len());
    let mut cursor = Cursor::open(base)?;
    for batch in base_layout.segments.chunks(4096) {
        let parts = batch
            .iter()
            .map(|segment| cursor.read(segment.offset, segment.length))
            .collect::<std::io::Result<Vec<_>>>()
            .map_err(|e| format!("Failed to read {}: {e}", base.display()))?;
        let keys: Vec<Key> = parts.par_iter().map(|bytes| key(bytes)).collect();
        for (&segment, k) in batch.iter().zip(keys) {
            base_index.entry(k).or_insert(segment);
            base_keys.insert(segment, k);
        }
    }

    // Cover the target from its first byte to its last. Its parts are read
    // in order a batch at a time; each batch's parts are hashed and its
    // changed tiles tried as patches in parallel, then the batch is settled
    // in order, so the operations are those one thread would choose.
    let mut operations = Vec::new();
    let mut target_keys: HashMap<Segment, Key> =
        HashMap::with_capacity(target_layout.segments.len());
    let mut cursor = Cursor::open(target)?;
    let mut base_cursor = Cursor::open(base)?;
    base_cursor.length = base_layout.file_length;
    let base_file =
        File::open(base).map_err(|e| format!("Failed to open {}: {e}", base.display()))?;
    let target_tile_of: HashMap<Segment, u64> = target_layout
        .tiles
        .iter()
        .rev()
        .map(|e| {
            let segment = Segment {
                offset: e.offset,
                length: u64::from(e.length),
            };
            (segment, e.tile_id)
        })
        .collect();
    let mut patcher = Patcher {
        base_tiles: &base_layout.tiles,
        base_file: &base_file,
        base_compression: base_layout.tile_compression,
        target_compression: target_layout.tile_compression,
        level: None,
        hits: 0,
        misses: 0,
    };
    // A batch starts small, so an archive whose tiles can't be patched is
    // given up on after a few tiles' work, as one thread would, and grows to
    // keep every thread busy.
    let mut batch_parts = 64;
    let mut parts: Vec<Part> = Vec::new();
    let mut batch_bytes = 0u64;
    let mut covered = 0u64;
    let mut segments = target_layout.segments.iter().peekable();
    while let Some(&segment) = segments.next() {
        let end = segment.offset + segment.length;
        if end > covered {
            if segment.offset < covered {
                // Overlaps the previous part: carry the rest as it is.
                parts.push(Part::Ready(Operation::Literal {
                    offset: covered,
                    length: end - covered,
                }));
            } else {
                if segment.offset > covered {
                    let gap = gap(
                        &mut base_cursor,
                        &mut cursor,
                        covered,
                        segment.offset - covered,
                    )?;
                    parts.push(Part::Ready(gap));
                }
                let bytes = cursor
                    .read(segment.offset, segment.length)
                    .map_err(|e| format!("Failed to read {}: {e}", target.display()))?;
                batch_bytes += segment.length;
                parts.push(Part::Read { segment, bytes });
            }
            covered = end;
        }
        if segments.peek().is_some() && parts.len() < batch_parts && batch_bytes < BATCH_BYTES {
            continue;
        }
        let guess = patcher.level;
        let given_up = patcher.given_up();
        let tried: Vec<Option<(Key, Trial)>> = parts
            .par_iter()
            .map(|part| {
                let Part::Read { segment, bytes } = part else {
                    return None;
                };
                let k = key(bytes);
                let trial = match (base_index.get(&k), target_tile_of.get(segment)) {
                    (Some(found), _) if found.length == segment.length => Trial::Skipped,
                    (_, Some(&tile_id)) if !given_up => patcher.trial(guess, tile_id, bytes),
                    _ => Trial::Skipped,
                };
                Some((k, trial))
            })
            .collect();
        for (part, tried) in parts.drain(..).zip(tried) {
            let operation = match (part, tried) {
                (Part::Ready(operation), _) => operation,
                (Part::Read { segment, bytes }, Some((k, trial))) => {
                    target_keys.insert(segment, k);
                    match base_index.get(&k) {
                        Some(found) if found.length == segment.length => Operation::Copy {
                            offset: found.offset,
                            length: found.length,
                        },
                        _ => patcher.settle(trial, &bytes)?.map_or(
                            Operation::Literal {
                                offset: segment.offset,
                                length: segment.length,
                            },
                            Operation::Patch,
                        ),
                    }
                }
                (Part::Read { .. }, None) => unreachable!("every part read is hashed"),
            };
            push(&mut operations, operation);
        }
        batch_bytes = 0;
        batch_parts = (batch_parts * 2).min(1 << 16);
    }
    if covered < target_layout.file_length {
        let gap = gap(
            &mut base_cursor,
            &mut cursor,
            covered,
            target_layout.file_length - covered,
        )?;
        push(&mut operations, gap);
    }
    if target_layout.file_length != target_digest.bytes
        || base_layout.file_length != base_digest.bytes
    {
        return Err("an archive changed while it was being read".into());
    }

    let (mut copied_bytes, mut literal_bytes, mut patched_bytes, mut patched_tiles) = (0, 0, 0, 0);
    for operation in &operations {
        match operation {
            Operation::Copy { length, .. } => copied_bytes += length,
            Operation::Literal { length, .. } => literal_bytes += length,
            Operation::Patch(patch) => {
                patched_bytes += patch.target_length;
                patched_tiles += 1;
            }
        }
    }
    let manifest = DeltaManifest {
        format: DELTA_FORMAT.into(),
        version: DELTA_VERSION,
        base: base_digest,
        target: target_digest,
        copied_bytes,
        literal_bytes,
        patched_bytes,
        patched_tiles,
        tiles: tile_changes(
            &base_layout.tiles,
            &base_keys,
            &target_layout.tiles,
            &target_keys,
        ),
    };

    let (pending, file) = PendingFile::create(output)?;
    let failed = |e: std::io::Error| format!("Failed to write {}: {e}", output.display());
    let mut out = BufWriter::with_capacity(BUFFER_BYTES, file);
    let manifest_json = serde_json::to_vec(&manifest).map_err(|e| e.to_string())?;
    out.write_all(MAGIC).map_err(failed)?;
    out.write_all(&DELTA_VERSION.to_le_bytes())
        .map_err(failed)?;
    out.write_all(&(manifest_json.len() as u32).to_le_bytes())
        .map_err(failed)?;
    out.write_all(&manifest_json).map_err(failed)?;
    for operation in &operations {
        match *operation {
            Operation::Patch(ref patch) => {
                out.write_all(&[TAG_PATCH]).map_err(failed)?;
                write_varint(&mut out, patch.base.offset).map_err(failed)?;
                write_varint(&mut out, patch.base.length).map_err(failed)?;
                out.write_all(&[patch.base_compression, patch.level])
                    .map_err(failed)?;
                write_varint(&mut out, patch.target_length).map_err(failed)?;
                write_varint(&mut out, patch.patch.len() as u64).map_err(failed)?;
                out.write_all(&patch.patch).map_err(failed)?;
            }
            Operation::Copy { offset, length } => {
                out.write_all(&[TAG_COPY]).map_err(failed)?;
                write_varint(&mut out, offset).map_err(failed)?;
                write_varint(&mut out, length).map_err(failed)?;
            }
            Operation::Literal { offset, length } => {
                out.write_all(&[TAG_LITERAL]).map_err(failed)?;
                write_varint(&mut out, length).map_err(failed)?;
                cursor
                    .copy_to(offset, length, &mut out)
                    .map_err(|e| format!("Failed to read {}: {e}", target.display()))?;
            }
        }
    }
    out.write_all(&[TAG_END]).map_err(failed)?;
    let file = out.into_inner().map_err(|e| failed(e.into_error()))?;
    let delta_bytes = file.metadata().map_err(failed)?.len();

    // Rebuild the target from what was written, without keeping it.
    let mut delta = BufReader::with_capacity(
        BUFFER_BYTES,
        File::open(&pending.temporary).map_err(failed)?,
    );
    let written = read_manifest_from(&mut delta)?;
    let mut rebuilt = HashingWriter::new(std::io::sink());
    run(base, &written, &mut delta, &mut rebuilt)?;
    if rebuilt.bytes != written.target.bytes || rebuilt.finish() != written.target.sha256 {
        return Err(format!(
            "the delta does not rebuild {}; nothing was written",
            target.display()
        ));
    }
    pending.keep(file)?;
    Ok(DiffReport {
        delta_bytes,
        manifest,
    })
}

fn read_manifest_from(input: &mut impl Read) -> Result<DeltaManifest, String> {
    let mut lead = [0u8; 16];
    input
        .read_exact(&mut lead)
        .map_err(|_| "not a delta: too short".to_string())?;
    if &lead[..8] != MAGIC {
        return Err("not a delta archive".into());
    }
    let version = u32::from_le_bytes(lead[8..12].try_into().unwrap());
    if version != DELTA_VERSION {
        return Err(format!(
            "delta version {version} is not supported; this build reads version {DELTA_VERSION}"
        ));
    }
    let length = u32::from_le_bytes(lead[12..16].try_into().unwrap());
    if length > MAX_MANIFEST_BYTES {
        return Err("not a delta: its manifest is too large".into());
    }
    let mut json = vec![0; length as usize];
    input
        .read_exact(&mut json)
        .map_err(|_| "the delta ends inside its manifest".to_string())?;
    let manifest: DeltaManifest =
        serde_json::from_slice(&json).map_err(|e| format!("the delta's manifest: {e}"))?;
    if manifest.format != DELTA_FORMAT || manifest.version != DELTA_VERSION {
        return Err("the delta's manifest names another format".into());
    }
    Ok(manifest)
}

/// Reads the manifest at the start of the delta at `path`.
pub fn read_manifest(path: &Path) -> Result<DeltaManifest, String> {
    let file = File::open(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
    read_manifest_from(&mut BufReader::new(file)).map_err(|e| format!("{}: {e}", path.display()))
}

/// One operation of a delta, read and checked, waiting to be run.
enum Step {
    Copy {
        offset: u64,
        length: u64,
    },
    Literal(Vec<u8>),
    /// A literal too large to hold: copied straight from the delta, so it
    /// is always the last step of its batch.
    LongLiteral(u64),
    Patch {
        offset: u64,
        base_length: u64,
        base_compression: u8,
        level: u8,
        length: u64,
        patch: Vec<u8>,
    },
}

fn corrupt(what: &str) -> String {
    format!("the delta is corrupt: {what}")
}

/// Reads and checks the delta's next operation, counting what it writes in
/// `written`; `None` at the end operation.
fn read_step(
    manifest: &DeltaManifest,
    delta: &mut impl Read,
    written: &mut u64,
) -> Result<Option<Step>, String> {
    let mut tag = [0u8];
    delta
        .read_exact(&mut tag)
        .map_err(|_| corrupt("it ends before its last operation"))?;
    let (step, length) = match tag[0] {
        TAG_END => return Ok(None),
        TAG_COPY => {
            let offset = read_varint(delta)?;
            let length = read_varint(delta)?;
            if offset
                .checked_add(length)
                .is_none_or(|end| end > manifest.base.bytes)
            {
                return Err(corrupt("it copies bytes beyond the old archive"));
            }
            if *written + length > manifest.target.bytes {
                return Err(corrupt("it writes past the new archive's size"));
            }
            (Step::Copy { offset, length }, length)
        }
        TAG_LITERAL => {
            let length = read_varint(delta)?;
            if *written + length > manifest.target.bytes {
                return Err(corrupt("it writes past the new archive's size"));
            }
            if length > BUFFER_BYTES as u64 {
                (Step::LongLiteral(length), length)
            } else {
                let mut bytes = vec![0; length as usize];
                delta
                    .read_exact(&mut bytes)
                    .map_err(|_| corrupt("it ends inside a literal"))?;
                (Step::Literal(bytes), length)
            }
        }
        TAG_PATCH => {
            let offset = read_varint(delta)?;
            let base_length = read_varint(delta)?;
            let mut codes = [0u8; 2];
            delta
                .read_exact(&mut codes)
                .map_err(|_| corrupt("it ends inside a patch"))?;
            let [base_compression, level] = codes;
            let length = read_varint(delta)?;
            let patch_length = read_varint(delta)?;
            if offset
                .checked_add(base_length)
                .is_none_or(|end| end > manifest.base.bytes)
                || base_length > MAX_TILE_BYTES as u64
            {
                return Err(corrupt("it patches bytes beyond the old archive"));
            }
            if *written + length > manifest.target.bytes || patch_length > length {
                return Err(corrupt("it writes past the new archive's size"));
            }
            if level != STORED && level > 9 {
                return Err(corrupt("a patch names an unknown compression"));
            }
            let mut patch = vec![0; patch_length as usize];
            delta
                .read_exact(&mut patch)
                .map_err(|_| corrupt("it ends inside a patch"))?;
            let step = Step::Patch {
                offset,
                base_length,
                base_compression,
                level,
                length,
                patch,
            };
            (step, length)
        }
        other => return Err(corrupt(&format!("unknown operation {other}"))),
    };
    *written += length;
    Ok(Some(step))
}

/// Rebuilds a patched tile's stored bytes from the base archive.
fn rebuild_tile(base: &Path, base_file: &File, step: &Step) -> Result<Vec<u8>, String> {
    let Step::Patch {
        offset,
        base_length,
        base_compression,
        level,
        length,
        ref patch,
    } = *step
    else {
        unreachable!("only patches are rebuilt");
    };
    let old = read_at(base_file, offset, base_length)
        .map_err(|e| format!("Failed to read {}: {e}", base.display()))?;
    let old = decode_tile(&old, base_compression)
        .ok_or_else(|| corrupt("a patch's old tile can't be decoded"))?;
    let new = super::tile_patch::decode(&old, patch, MAX_TILE_BYTES).map_err(|e| corrupt(&e))?;
    let stored = encode_tile(&new, level);
    if stored.len() as u64 != length {
        return Err(corrupt("a patched tile has the wrong size"));
    }
    Ok(stored)
}

/// Runs a delta's operations, after its manifest, writing the target to `out`.
/// Operations are read a batch at a time; a batch's patched tiles are
/// rebuilt in parallel and everything is written in order.
fn run(
    base: &Path,
    manifest: &DeltaManifest,
    delta: &mut impl Read,
    out: &mut impl Write,
) -> Result<(), String> {
    let mut cursor = Cursor::open(base)?;
    let base_file =
        File::open(base).map_err(|e| format!("Failed to open {}: {e}", base.display()))?;
    let failed = |e: std::io::Error| format!("Failed to write the archive: {e}");
    let mut written = 0u64;
    let mut steps = Vec::new();
    loop {
        // Read a batch, stopping early at the end, at a literal to copy
        // straight from the delta, or at a corrupt operation, whose error
        // comes after any error of the operations before it.
        let (mut ended, mut stopped) = (false, None);
        let batch_start = written;
        while written - batch_start < BATCH_BYTES && steps.len() < 1 << 16 {
            match read_step(manifest, delta, &mut written) {
                Ok(None) => {
                    ended = true;
                    break;
                }
                Ok(Some(step)) => {
                    let long = matches!(step, Step::LongLiteral(_));
                    steps.push(step);
                    if long {
                        break;
                    }
                }
                Err(error) => {
                    stopped = Some(error);
                    break;
                }
            }
        }
        let mut tiles: Vec<Option<Result<Vec<u8>, String>>> = steps
            .par_iter()
            .map(|step| {
                matches!(step, Step::Patch { .. }).then(|| rebuild_tile(base, &base_file, step))
            })
            .collect();
        for (step, tile) in steps.drain(..).zip(tiles.drain(..)) {
            match step {
                Step::Copy { offset, length } => cursor
                    .copy_to(offset, length, out)
                    .map_err(|e| format!("Failed to read {}: {e}", base.display()))?,
                Step::Literal(bytes) => out.write_all(&bytes).map_err(failed)?,
                Step::LongLiteral(length) => {
                    let copied = std::io::copy(&mut delta.take(length), out).map_err(failed)?;
                    if copied != length {
                        return Err(corrupt("it ends inside a literal"));
                    }
                }
                Step::Patch { .. } => {
                    let stored = tile.expect("every patch is rebuilt")?;
                    out.write_all(&stored).map_err(failed)?;
                }
            }
        }
        if let Some(error) = stopped {
            return Err(error);
        }
        if ended {
            break;
        }
    }
    out.flush().map_err(failed)?;
    Ok(())
}

/// Applies the delta at `delta` to the archive `base`, writing the new
/// archive to `output`, which must not exist yet. Checks `base` before and
/// the result after; on any failure nothing is left at `output`.
pub fn apply(base: &Path, delta: &Path, output: &Path) -> Result<ApplyReport, String> {
    let delta_file =
        File::open(delta).map_err(|e| format!("Failed to open {}: {e}", delta.display()))?;
    let delta_bytes = delta_file.metadata().map_err(|e| e.to_string())?.len();
    let mut input = BufReader::with_capacity(BUFFER_BYTES, delta_file);
    let manifest =
        read_manifest_from(&mut input).map_err(|e| format!("{}: {e}", delta.display()))?;
    let found = digest(base)?;
    if found != manifest.base {
        return Err(format!(
            "{} is not the archive this delta applies to: it needs {} bytes with SHA-256 {}, found {} bytes with SHA-256 {}",
            base.display(),
            manifest.base.bytes,
            manifest.base.sha256,
            found.bytes,
            found.sha256
        ));
    }
    let (pending, file) = PendingFile::create(output)?;
    let mut out = HashingWriter::new(BufWriter::with_capacity(BUFFER_BYTES, file));
    run(base, &manifest, &mut input, &mut out)?;
    let bytes = out.bytes;
    let HashingWriter { inner, hasher, .. } = out;
    let sha256 = hex::encode(hasher.finalize());
    if bytes != manifest.target.bytes || sha256 != manifest.target.sha256 {
        return Err(format!(
            "applying {} did not give the archive it promises; nothing was written",
            delta.display()
        ));
    }
    let file = inner
        .into_inner()
        .map_err(|e| format!("Failed to write {}: {e}", output.display()))?;
    pending.keep(file)?;
    Ok(ApplyReport {
        delta_bytes,
        manifest,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmtiles::{Compression, PmTilesWriter, TileCoord, TileId, TileType};

    fn archive(path: &Path, tiles: &[(u64, &[u8])], metadata: &str) {
        let mut writer = PmTilesWriter::new(TileType::Mvt)
            .precompressed(Compression::Gzip)
            .metadata(metadata)
            .create(File::create(path).unwrap())
            .unwrap();
        for &(id, bytes) in tiles {
            writer
                .add_tile(TileCoord::from(TileId::new(id).unwrap()), bytes)
                .unwrap();
        }
        writer.finalize().unwrap();
    }

    fn round_trip(base_tiles: &[(u64, &[u8])], target_tiles: &[(u64, &[u8])]) -> DiffReport {
        let dir = tempfile::tempdir().unwrap();
        let (base, target) = (dir.path().join("a.pmtiles"), dir.path().join("b.pmtiles"));
        archive(&base, base_tiles, r#"{"name":"a"}"#);
        archive(&target, target_tiles, r#"{"name":"b"}"#);
        let delta = dir.path().join("a-b.delta");
        let report = diff(&base, &target, &delta).unwrap();
        assert_eq!(read_manifest(&delta).unwrap(), report.manifest);
        let rebuilt = dir.path().join("rebuilt.pmtiles");
        apply(&base, &delta, &rebuilt).unwrap();
        assert_eq!(
            std::fs::read(&rebuilt).unwrap(),
            std::fs::read(&target).unwrap()
        );
        report
    }

    #[test]
    fn rebuilds_the_target_byte_for_byte() {
        let report = round_trip(
            &[
                (0, b"world"),
                (1, b"sea"),
                (2, b"sea"),
                (3, b"land"),
                (4, b"gone"),
            ],
            &[
                (0, b"world"),
                (1, b"sea"),
                (2, b"city"),
                (3, b"land"),
                (5, b"new"),
            ],
        );
        assert_eq!(
            report.manifest.tiles,
            TileChanges {
                unchanged: 3,
                changed: 1,
                added: 1,
                removed: 1
            }
        );
        // Of the tiles, only "city" and "new" travel in the delta.
        assert!(report.manifest.copied_bytes >= b"worldsealand".len() as u64);
    }

    #[test]
    fn copies_tiles_that_moved() {
        let big = vec![7u8; 10_000];
        let report = round_trip(&[(0, &big)], &[(1, &big)]);
        assert!(report.manifest.literal_bytes < 1_000, "{report:?}");
        assert_eq!(report.manifest.tiles.added, 1);
    }

    #[test]
    fn patches_changed_gzip_tiles() {
        let tile = |edit: &str, level: u8| {
            let raw: Vec<u8> = (0..5_000u32)
                .flat_map(|i| (i * 7919 % 65_521).to_le_bytes())
                .chain(edit.bytes())
                .collect();
            encode_tile(&raw, level)
        };
        for level in [6, 9, 1] {
            let (a, b, c) = (tile("", 6), tile("an edit", level), tile("other", level));
            let report = round_trip(&[(0, &a), (1, &a), (2, &a)], &[(0, &a), (1, &b), (2, &c)]);
            assert_eq!(report.manifest.patched_tiles, 2, "{report:?}");
            assert!(report.delta_bytes < 1_000, "{report:?}");
        }
    }

    #[test]
    fn rebuilds_unrelated_archives() {
        round_trip(&[(0, b"a")], &[(0, b"b"), (9, b"c"), (10, b"c")]);
    }

    #[test]
    fn refuses_another_base_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b, c) = (
            dir.path().join("a.pmtiles"),
            dir.path().join("b.pmtiles"),
            dir.path().join("c.pmtiles"),
        );
        archive(&a, &[(0, b"a")], "{}");
        archive(&b, &[(0, b"b")], "{}");
        archive(&c, &[(0, b"c")], "{}");
        let delta = dir.path().join("a-b.delta");
        diff(&a, &b, &delta).unwrap();
        let out = dir.path().join("out.pmtiles");
        let error = apply(&c, &delta, &out).unwrap_err();
        assert!(
            error.contains("is not the archive this delta applies to"),
            "{error}"
        );
        assert!(!out.exists());
        assert!(!dir.path().join("out.pmtiles.partial").exists());
    }

    #[test]
    fn refuses_a_corrupt_delta_and_leaves_nothing() {
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.pmtiles"), dir.path().join("b.pmtiles"));
        archive(&a, &[(0, b"one"), (1, b"two")], "{}");
        archive(&b, &[(0, b"one"), (1, b"three")], "{}");
        let delta = dir.path().join("a-b.delta");
        diff(&a, &b, &delta).unwrap();
        let mut bytes = std::fs::read(&delta).unwrap();
        let last = bytes.len() - 2;
        bytes[last] ^= 0xff;
        std::fs::write(&delta, &bytes).unwrap();
        let out = dir.path().join("out.pmtiles");
        assert!(apply(&a, &delta, &out).is_err());
        assert!(!out.exists());
        bytes.truncate(bytes.len() - 5);
        std::fs::write(&delta, &bytes).unwrap();
        assert!(apply(&a, &delta, &out).is_err());
        assert!(!out.exists());
    }

    #[test]
    fn will_not_overwrite() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        archive(&a, &[(0, b"a")], "{}");
        let delta = dir.path().join("a.delta");
        std::fs::write(&delta, b"keep").unwrap();
        assert!(diff(&a, &a, &delta).unwrap_err().contains("already exists"));
        assert_eq!(std::fs::read(&delta).unwrap(), b"keep");
    }

    /// Tiles at several gzip levels, some too small to patch, over more
    /// than one batch: the delta must not depend on how many threads made it.
    #[test]
    fn same_delta_on_any_number_of_threads() {
        let raw = |id: u64, edit: bool| {
            let words = if id.is_multiple_of(5) {
                4
            } else {
                1_000 + id % 300
            };
            let mut raw: Vec<u8> = (0..words)
                .flat_map(|i| ((i * 7919 + id * 31) % 65_521).to_le_bytes())
                .collect();
            if edit {
                raw.extend_from_slice(format!("edit {id}").as_bytes());
            }
            raw
        };
        let base: Vec<(u64, Vec<u8>)> = (0..400)
            .map(|id| (id, encode_tile(&raw(id, false), 6)))
            .collect();
        let target: Vec<(u64, Vec<u8>)> = (0..400)
            .map(|id| {
                let level = [1, 9, 6, 0, 3][(id / 7 % 5) as usize];
                (id, encode_tile(&raw(id, id % 4 != 0), level))
            })
            .collect();
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("a.pmtiles"), dir.path().join("b.pmtiles"));
        fn as_refs(tiles: &[(u64, Vec<u8>)]) -> Vec<(u64, &[u8])> {
            tiles.iter().map(|(id, t)| (*id, &t[..])).collect()
        }
        archive(&a, &as_refs(&base), "{}");
        archive(&b, &as_refs(&target), "{}");
        let deltas: Vec<Vec<u8>> = [1, 4]
            .into_iter()
            .map(|threads| {
                let delta = dir.path().join(format!("{threads}.delta"));
                rayon::ThreadPoolBuilder::new()
                    .num_threads(threads)
                    .build()
                    .unwrap()
                    .install(|| diff(&a, &b, &delta))
                    .unwrap();
                std::fs::read(&delta).unwrap()
            })
            .collect();
        assert_eq!(deltas[0], deltas[1]);
        let report = read_manifest(&dir.path().join("4.delta")).unwrap();
        assert!(report.patched_tiles > 200, "{report:?}");
        let rebuilt = dir.path().join("rebuilt.pmtiles");
        apply(&a, &dir.path().join("4.delta"), &rebuilt).unwrap();
        assert_eq!(std::fs::read(&rebuilt).unwrap(), std::fs::read(&b).unwrap());
    }

    /// A trial that guessed the wrong gzip level still settles on the level
    /// trying the tiles one after another would pick.
    #[test]
    fn settles_the_level_a_wrong_guess_missed() {
        let raw: Vec<u8> = (0..3_000u32)
            .flat_map(|i| (i * 7919 % 65_521).to_le_bytes())
            .collect();
        let mut edited = raw.clone();
        edited.extend_from_slice(b"an edit");
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.pmtiles");
        archive(&a, &[(0, &encode_tile(&raw, 6))], "{}");
        let base_file = File::open(&a).unwrap();
        let base_layout = layout(&a).unwrap();
        for stored_level in [1, 6, 9] {
            let stored = encode_tile(&edited, stored_level);
            for (guess, before) in [(None, Some(9)), (Some(1), Some(8)), (Some(9), None)] {
                let mut patcher = Patcher {
                    base_tiles: &base_layout.tiles,
                    base_file: &base_file,
                    base_compression: 2,
                    target_compression: 2,
                    level: before,
                    hits: 0,
                    misses: 0,
                };
                let trial = patcher.trial(guess, 0, &stored);
                let patch = patcher.settle(trial, &stored).unwrap().unwrap();
                let expected = find_level(before, &mut [None; 10], |level| {
                    encode_tile(&edited, level) == stored
                });
                assert_eq!(Some(patch.level), expected);
                assert_eq!(patcher.level, expected);
            }
        }
    }
}
