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
        let (section_offset, section_length, section) = if leaf {
            (header.leaf_offset, header.leaf_length, "leaf directories")
        } else {
            (header.data_offset, header.data_length, "data section")
        };
        let length = u64::from(entry.length);
        let offset = entry
            .offset
            .checked_add(length)
            .filter(|&end| end <= section_length)
            .and_then(|_| section_offset.checked_add(entry.offset))
            .ok_or_else(|| format!("an entry at {} lies beyond the {section}", entry.offset))?;
        if !leaf {
            tiles.push(Entry { offset, ..entry });
        }
        segments.push(Segment { offset, length });
        Ok(())
    })?;
    segments.retain(|s| s.length > 0);
    segments.sort_unstable();
    segments.dedup();
    if segments.iter().any(|s| {
        s.offset
            .checked_add(s.length)
            .is_none_or(|end| end > reader.file_length)
    }) {
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

/// Finds patches for the target's changed tiles. A patch needs the target's
/// stored bytes to be reproducible from the decoded tile, which holds for
/// archives this compiler wrote; for others it soon stops trying.
struct Patcher<'a> {
    base_tiles: &'a [Entry],
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

    /// The level that stores `raw` as `stored`, if any.
    fn level(&mut self, raw: &[u8], stored: &[u8]) -> Option<u8> {
        if self.target_compression == 1 {
            return Some(STORED);
        }
        let mut tried = [false; 10];
        for level in self.level.into_iter().chain([6]).chain(0..=9) {
            if std::mem::replace(&mut tried[usize::from(level)], true) {
                continue;
            }
            if encode_tile(raw, level) == stored {
                self.level = Some(level);
                return Some(level);
            }
        }
        None
    }

    /// A patch for the target tile `tile_id`, stored as `stored`, when one
    /// is smaller than the tile.
    fn patch(
        &mut self,
        base: &mut Cursor,
        tile_id: u64,
        stored: &[u8],
    ) -> Result<Option<TilePatch>, String> {
        if self.hits == 0 && self.misses >= 16 {
            return Ok(None);
        }
        let Some(segment) = self.base_tile(tile_id) else {
            return Ok(None);
        };
        let old = base
            .read(segment.offset, segment.length)
            .map_err(|e| format!("Failed to read the old archive: {e}"))?;
        let (Some(old), Some(new)) = (
            decode_tile(&old, self.base_compression),
            decode_tile(stored, self.target_compression),
        ) else {
            return Ok(None);
        };
        let Some(level) = self.level(&new, stored) else {
            self.misses += 1;
            return Ok(None);
        };
        let patch = super::tile_patch::encode(&old, &new);
        if patch.len() + 32 >= stored.len()
            || super::tile_patch::decode(&old, &patch, new.len()).as_deref() != Ok(&new[..])
        {
            return Ok(None);
        }
        self.hits += 1;
        Ok(Some(TilePatch {
            base: segment,
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

/// Where [`diff`] and [`apply`] write `output` until it is complete:
/// `<output>.partial`, removed if they fail.
pub fn partial_path(output: &Path) -> PathBuf {
    let mut name = output.file_name().unwrap_or_default().to_os_string();
    name.push(".partial");
    output.with_file_name(name)
}

impl PendingFile {
    fn create(destination: &Path) -> Result<(Self, File), String> {
        if destination.exists() {
            return Err(format!("{} already exists", destination.display()));
        }
        if destination.file_name().is_none() {
            return Err(format!("{} is not a file path", destination.display()));
        }
        let temporary = partial_path(destination);
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

/// Writes the delta that turns the archive `base` into `target` to `output`,
/// which must not exist yet. Before keeping it, rebuilds `target` from it
/// and checks the result.
pub fn diff(base: &Path, target: &Path, output: &Path) -> Result<DiffReport, String> {
    let base_digest = digest(base)?;
    let target_digest = digest(target)?;
    let base_layout = layout(base)?;
    let target_layout = layout(target)?;

    // Every distinct part of the base, by content.
    let mut base_index: HashMap<Key, Segment> = HashMap::with_capacity(base_layout.segments.len());
    let mut base_keys: HashMap<Segment, Key> = HashMap::with_capacity(base_layout.segments.len());
    let mut cursor = Cursor::open(base)?;
    for &segment in &base_layout.segments {
        let bytes = cursor
            .read(segment.offset, segment.length)
            .map_err(|e| format!("Failed to read {}: {e}", base.display()))?;
        let k = key(&bytes);
        base_index.entry(k).or_insert(segment);
        base_keys.insert(segment, k);
    }

    // Cover the target from its first byte to its last.
    let mut operations = Vec::new();
    let mut target_keys: HashMap<Segment, Key> =
        HashMap::with_capacity(target_layout.segments.len());
    let mut cursor = Cursor::open(target)?;
    let mut base_cursor = Cursor::open(base)?;
    base_cursor.length = base_layout.file_length;
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
        base_compression: base_layout.tile_compression,
        target_compression: target_layout.tile_compression,
        level: None,
        hits: 0,
        misses: 0,
    };
    let mut covered = 0u64;
    for &segment in &target_layout.segments {
        let end = segment.offset + segment.length;
        if end <= covered {
            continue;
        }
        if segment.offset < covered {
            // Overlaps the previous part: carry the rest as it is.
            push(
                &mut operations,
                Operation::Literal {
                    offset: covered,
                    length: end - covered,
                },
            );
            covered = end;
            continue;
        }
        if segment.offset > covered {
            let gap = gap(
                &mut base_cursor,
                &mut cursor,
                covered,
                segment.offset - covered,
            )?;
            push(&mut operations, gap);
        }
        let bytes = cursor
            .read(segment.offset, segment.length)
            .map_err(|e| format!("Failed to read {}: {e}", target.display()))?;
        let k = key(&bytes);
        target_keys.insert(segment, k);
        let operation = match base_index.get(&k) {
            Some(found) if found.length == segment.length => Operation::Copy {
                offset: found.offset,
                length: found.length,
            },
            _ => {
                let patch = match target_tile_of.get(&segment) {
                    Some(&tile_id) => patcher.patch(&mut base_cursor, tile_id, &bytes)?,
                    None => None,
                };
                patch.map_or(
                    Operation::Literal {
                        offset: segment.offset,
                        length: segment.length,
                    },
                    Operation::Patch,
                )
            }
        };
        push(&mut operations, operation);
        covered = end;
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

/// Runs a delta's operations, after its manifest, writing the target to `out`.
fn run(
    base: &Path,
    manifest: &DeltaManifest,
    delta: &mut impl Read,
    out: &mut impl Write,
) -> Result<(), String> {
    let mut cursor = Cursor::open(base)?;
    let corrupt = |what: &str| format!("the delta is corrupt: {what}");
    let failed = |e: std::io::Error| format!("Failed to write the archive: {e}");
    let mut written = 0u64;
    loop {
        let mut tag = [0u8];
        delta
            .read_exact(&mut tag)
            .map_err(|_| corrupt("it ends before its last operation"))?;
        let length = match tag[0] {
            TAG_END => break,
            TAG_COPY => {
                let offset = read_varint(delta)?;
                let length = read_varint(delta)?;
                if offset
                    .checked_add(length)
                    .is_none_or(|end| end > manifest.base.bytes)
                {
                    return Err(corrupt("it copies bytes beyond the old archive"));
                }
                if written + length > manifest.target.bytes {
                    return Err(corrupt("it writes past the new archive's size"));
                }
                cursor
                    .copy_to(offset, length, out)
                    .map_err(|e| format!("Failed to read {}: {e}", base.display()))?;
                length
            }
            TAG_LITERAL => {
                let length = read_varint(delta)?;
                if written + length > manifest.target.bytes {
                    return Err(corrupt("it writes past the new archive's size"));
                }
                let copied = std::io::copy(&mut delta.take(length), out).map_err(failed)?;
                if copied != length {
                    return Err(corrupt("it ends inside a literal"));
                }
                length
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
                if written + length > manifest.target.bytes || patch_length > length {
                    return Err(corrupt("it writes past the new archive's size"));
                }
                if level != STORED && level > 9 {
                    return Err(corrupt("a patch names an unknown compression"));
                }
                let mut patch = vec![0; patch_length as usize];
                delta
                    .read_exact(&mut patch)
                    .map_err(|_| corrupt("it ends inside a patch"))?;
                let old = cursor
                    .read(offset, base_length)
                    .map_err(|e| format!("Failed to read {}: {e}", base.display()))?;
                let old = decode_tile(&old, base_compression)
                    .ok_or_else(|| corrupt("a patch's old tile can't be decoded"))?;
                let new = super::tile_patch::decode(&old, &patch, MAX_TILE_BYTES)
                    .map_err(|e| corrupt(&e))?;
                let stored = encode_tile(&new, level);
                if stored.len() as u64 != length {
                    return Err(corrupt("a patched tile has the wrong size"));
                }
                out.write_all(&stored).map_err(failed)?;
                length
            }
            other => return Err(corrupt(&format!("unknown operation {other}"))),
        };
        written += length;
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
}
