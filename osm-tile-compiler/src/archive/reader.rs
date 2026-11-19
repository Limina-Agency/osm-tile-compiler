//! A synchronous reader of PMTiles v3 archives that exposes their layout:
//! header fields, directory entries with their offsets, and stored tile bytes.
//! Spec: <https://github.com/protomaps/PMTiles/blob/main/spec/v3/spec.md>.
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

const HEADER_BYTES: usize = 127;
/// Directories nest at most this deep: the spec has a root and one level of
/// leaves, and writers may nest leaves further; deeper means a cycle.
const MAX_DIRECTORY_DEPTH: usize = 4;

/// The header, field by field.
#[derive(Debug, Clone, PartialEq)]
pub struct Header {
    pub root_offset: u64,
    pub root_length: u64,
    pub metadata_offset: u64,
    pub metadata_length: u64,
    pub leaf_offset: u64,
    pub leaf_length: u64,
    pub data_offset: u64,
    pub data_length: u64,
    /// Zero when unknown.
    pub addressed_tiles: u64,
    pub tile_entries: u64,
    pub tile_contents: u64,
    pub clustered: bool,
    /// 0 unknown, 1 none, 2 gzip, 3 brotli, 4 zstd.
    pub internal_compression: u8,
    pub tile_compression: u8,
    /// 0 unknown, 1 MVT, 2 PNG, 3 JPEG, 4 WebP, 5 AVIF.
    pub tile_type: u8,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// West, south, east, north, in degrees.
    pub bounds: [f64; 4],
    pub center_zoom: u8,
    /// Longitude and latitude.
    pub center: [f64; 2],
}

/// A directory entry that addresses tiles: `run_length` consecutive tile
/// IDs from `tile_id` share the `length` bytes at `offset` in the data section.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Entry {
    pub tile_id: u64,
    pub offset: u64,
    pub length: u32,
    pub run_length: u32,
}

pub struct ArchiveReader {
    file: File,
    pub file_length: u64,
    pub header: Header,
}

pub(crate) fn compression_name(code: u8) -> &'static str {
    match code {
        1 => "none",
        2 => "gzip",
        3 => "brotli",
        4 => "zstd",
        _ => "unknown",
    }
}

pub(crate) fn tile_type_name(code: u8) -> &'static str {
    match code {
        1 => "mvt",
        2 => "png",
        3 => "jpeg",
        4 => "webp",
        5 => "avif",
        _ => "unknown",
    }
}

fn u64_at(bytes: &[u8], at: usize) -> u64 {
    u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap())
}

fn e7_at(bytes: &[u8], at: usize) -> f64 {
    f64::from(i32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())) / 1e7
}

fn varint(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes.get(*at).ok_or("directory ends inside a varint")?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    Err("varint longer than 10 bytes".into())
}

/// Decompresses bytes stored with a PMTiles compression code.
pub(crate) fn decompress(bytes: &[u8], compression: u8) -> Result<Vec<u8>, String> {
    match compression {
        1 => Ok(bytes.to_vec()),
        2 => {
            let mut out = Vec::new();
            flate2::read::GzDecoder::new(bytes)
                .read_to_end(&mut out)
                .map_err(|e| format!("not valid gzip: {e}"))?;
            Ok(out)
        }
        other => Err(format!(
            "{} compression is not supported",
            compression_name(other)
        )),
    }
}

impl ArchiveReader {
    pub fn open(path: &Path) -> Result<Self, String> {
        let mut file =
            File::open(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
        let file_length = file.metadata().map_err(|e| e.to_string())?.len();
        let mut bytes = [0u8; HEADER_BYTES];
        file.read_exact(&mut bytes)
            .map_err(|_| format!("{} is too short for a PMTiles header", path.display()))?;
        if &bytes[..7] != b"PMTiles" {
            return Err(format!("{} is not a PMTiles archive", path.display()));
        }
        if bytes[7] != 3 {
            return Err(format!(
                "{} is PMTiles version {}; only version 3 is supported",
                path.display(),
                bytes[7]
            ));
        }
        let header = Header {
            root_offset: u64_at(&bytes, 8),
            root_length: u64_at(&bytes, 16),
            metadata_offset: u64_at(&bytes, 24),
            metadata_length: u64_at(&bytes, 32),
            leaf_offset: u64_at(&bytes, 40),
            leaf_length: u64_at(&bytes, 48),
            data_offset: u64_at(&bytes, 56),
            data_length: u64_at(&bytes, 64),
            addressed_tiles: u64_at(&bytes, 72),
            tile_entries: u64_at(&bytes, 80),
            tile_contents: u64_at(&bytes, 88),
            clustered: bytes[96] == 1,
            internal_compression: bytes[97],
            tile_compression: bytes[98],
            tile_type: bytes[99],
            min_zoom: bytes[100],
            max_zoom: bytes[101],
            bounds: [
                e7_at(&bytes, 102),
                e7_at(&bytes, 106),
                e7_at(&bytes, 110),
                e7_at(&bytes, 114),
            ],
            center_zoom: bytes[118],
            center: [e7_at(&bytes, 119), e7_at(&bytes, 123)],
        };
        Ok(Self {
            file,
            file_length,
            header,
        })
    }

    /// Reads `length` bytes at `offset` of the file.
    pub fn read(&mut self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        if offset
            .checked_add(length)
            .is_none_or(|end| end > self.file_length)
        {
            return Err(format!(
                "bytes {offset}..+{length} lie beyond the end of the file ({} bytes)",
                self.file_length
            ));
        }
        let mut bytes = vec![0; length as usize];
        self.file
            .seek(SeekFrom::Start(offset))
            .and_then(|_| self.file.read_exact(&mut bytes))
            .map_err(|e| e.to_string())?;
        Ok(bytes)
    }

    /// The metadata JSON text.
    pub fn metadata(&mut self) -> Result<String, String> {
        let (offset, length) = (self.header.metadata_offset, self.header.metadata_length);
        let bytes = self.read(offset, length)?;
        let text = decompress(&bytes, self.header.internal_compression)?;
        String::from_utf8(text).map_err(|_| "metadata is not UTF-8".into())
    }

    /// The stored bytes of an entry's tile.
    pub fn tile(&mut self, entry: &Entry) -> Result<Vec<u8>, String> {
        if entry.offset + u64::from(entry.length) > self.header.data_length {
            return Err(format!(
                "tile {} lies beyond the data section",
                entry.tile_id
            ));
        }
        self.read(
            self.header.data_offset + entry.offset,
            u64::from(entry.length),
        )
    }

    fn directory(&mut self, offset: u64, length: u64) -> Result<Vec<(Entry, bool)>, String> {
        let bytes = self.read(offset, length)?;
        let bytes = decompress(&bytes, self.header.internal_compression)?;
        let at = &mut 0;
        let count = varint(&bytes, at)? as usize;
        if count > bytes.len() {
            return Err(format!(
                "directory claims {count} entries in {} bytes",
                bytes.len()
            ));
        }
        let mut entries = vec![
            Entry {
                tile_id: 0,
                offset: 0,
                length: 0,
                run_length: 0
            };
            count
        ];
        let mut tile_id = 0u64;
        for entry in &mut entries {
            tile_id = tile_id
                .checked_add(varint(&bytes, at)?)
                .ok_or("tile ID overflows")?;
            entry.tile_id = tile_id;
        }
        for entry in &mut entries {
            entry.run_length = u32::try_from(varint(&bytes, at)?).map_err(|e| e.to_string())?;
        }
        for entry in &mut entries {
            entry.length = u32::try_from(varint(&bytes, at)?).map_err(|e| e.to_string())?;
        }
        for i in 0..count {
            let value = varint(&bytes, at)?;
            entries[i].offset = if value == 0 && i > 0 {
                entries[i - 1].offset + u64::from(entries[i - 1].length)
            } else {
                value.checked_sub(1).ok_or("offset 0 for the first entry")?
            };
        }
        if *at != bytes.len() {
            return Err(format!(
                "directory has {} bytes after its entries",
                bytes.len() - *at
            ));
        }
        // A run length of 0 marks a leaf directory pointer.
        Ok(entries
            .into_iter()
            .map(|e| (e, e.run_length == 0))
            .collect())
    }

    /// Calls `visit` with every tile entry, in directory order: the root's
    /// entries, each leaf's in place of its pointer.
    pub fn visit_entries(
        &mut self,
        mut visit: impl FnMut(&mut Self, Entry) -> Result<(), String>,
    ) -> Result<(), String> {
        self.walk(|reader, entry, leaf| if leaf { Ok(()) } else { visit(reader, entry) })
    }

    /// Calls `visit` with every directory entry in directory order, tile
    /// entries with `false` and leaf directory pointers with `true`, each
    /// pointer before its leaf's entries. A pointer's offset is relative to
    /// the leaf section.
    pub fn walk(
        &mut self,
        mut visit: impl FnMut(&mut Self, Entry, bool) -> Result<(), String>,
    ) -> Result<(), String> {
        let root = self.directory(self.header.root_offset, self.header.root_length)?;
        let mut stack = vec![(root.into_iter(), 0usize)];
        while let Some((entries, depth)) = stack.last_mut() {
            let depth = *depth;
            let Some((entry, leaf)) = entries.next() else {
                stack.pop();
                continue;
            };
            visit(self, entry, leaf)?;
            if !leaf {
                continue;
            }
            if depth + 1 >= MAX_DIRECTORY_DEPTH {
                return Err("leaf directories nest too deep".into());
            }
            if entry.offset + u64::from(entry.length) > self.header.leaf_length {
                return Err(format!(
                    "leaf directory at {} lies beyond the leaf section",
                    entry.offset
                ));
            }
            let leaf = self.directory(
                self.header.leaf_offset + entry.offset,
                u64::from(entry.length),
            )?;
            stack.push((leaf.into_iter(), depth + 1));
        }
        Ok(())
    }
}
