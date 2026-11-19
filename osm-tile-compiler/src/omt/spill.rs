//! Block-compressed sort runs.
//!
//! A run file is a sequence of blocks, each `[compressed length: u32 LE]
//! [raw length: u32 LE][one zstd frame]`, holding at most `BLOCK_BYTES` of the
//! frame stream `store::write_record` writes. Blocks are compressed
//! independently so that a reader holds one raw block, not a zstd window: a
//! merge keeps up to `MERGE_FAN_IN` readers open at once. The decompression
//! context and the compressed bytes are per thread and shared by every reader
//! on it.
use std::cell::RefCell;
use std::fs::File;
use std::io::{self, Read, Write};
use std::path::Path;

/// Raw bytes per block. Larger blocks compress a little better and cost each
/// open reader as much memory.
pub(super) const BLOCK_BYTES: usize = 64 * 1024;
/// zstd's fastest level already shrinks records about as much as level 3, at
/// a fraction of the time.
const LEVEL: i32 = 1;
const BLOCK_HEADER_BYTES: usize = 8;

fn other(message: impl Into<String>) -> io::Error {
    io::Error::other(message.into())
}

/// Writes a run file. `finish` writes the last block; a writer dropped
/// without it leaves a truncated file, which the caller deletes.
pub(super) struct RunWriter {
    file: File,
    raw: Vec<u8>,
    /// The compressed block, then its header.
    block: Vec<u8>,
    compressor: zstd::bulk::Compressor<'static>,
    raw_bytes: u64,
}

impl RunWriter {
    pub(super) fn create(path: &Path) -> Result<Self, String> {
        let file =
            File::create(path).map_err(|e| format!("Failed to create {}: {e}", path.display()))?;
        let compressor = zstd::bulk::Compressor::new(LEVEL).map_err(|e| e.to_string())?;
        Ok(Self {
            file,
            raw: Vec::with_capacity(BLOCK_BYTES),
            block: Vec::with_capacity(
                BLOCK_HEADER_BYTES + zstd::zstd_safe::compress_bound(BLOCK_BYTES),
            ),
            compressor,
            raw_bytes: 0,
        })
    }

    fn write_block(&mut self) -> io::Result<()> {
        if self.raw.is_empty() {
            return Ok(());
        }
        // The frame replaces the buffer's contents; the header follows it.
        let compressed = self
            .compressor
            .compress_to_buffer(&self.raw, &mut self.block)?;
        let header_at = self.block.len();
        self.block
            .extend_from_slice(&(compressed as u32).to_le_bytes());
        self.block
            .extend_from_slice(&(self.raw.len() as u32).to_le_bytes());
        self.file.write_all(&self.block[header_at..])?;
        self.file.write_all(&self.block[..header_at])?;
        self.raw_bytes += self.raw.len() as u64;
        self.raw.clear();
        Ok(())
    }

    /// Writes the last block and returns the raw bytes the file holds.
    pub(super) fn finish(mut self) -> Result<u64, String> {
        self.write_block().map_err(|e| e.to_string())?;
        Ok(self.raw_bytes)
    }
}

impl Write for RunWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let taken = bytes.len().min(BLOCK_BYTES - self.raw.len());
        self.raw.extend_from_slice(&bytes[..taken]);
        if self.raw.len() == BLOCK_BYTES {
            self.write_block()?;
        }
        Ok(taken)
    }

    /// Blocks are written when full or at `finish`, never partly.
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

thread_local! {
    static DECOMPRESS: RefCell<Option<(zstd::bulk::Decompressor<'static>, Vec<u8>)>> =
        const { RefCell::new(None) };
}

/// Reads a run file written by `RunWriter`, as the raw frame stream.
pub(super) struct RunReader {
    file: File,
    raw: Vec<u8>,
    position: usize,
}

impl RunReader {
    pub(super) fn open(path: &Path) -> Result<Self, String> {
        let file =
            File::open(path).map_err(|e| format!("Failed to open {}: {e}", path.display()))?;
        Ok(Self {
            file,
            raw: Vec::new(),
            position: 0,
        })
    }

    /// Reads the next block; false at the end of the file.
    fn next_block(&mut self) -> io::Result<bool> {
        let mut header = [0u8; BLOCK_HEADER_BYTES];
        let mut filled = 0;
        while filled < header.len() {
            match self.file.read(&mut header[filled..]) {
                Ok(0) if filled == 0 => return Ok(false),
                Ok(0) => return Err(other("Truncated OMT run block header")),
                Ok(read) => filled += read,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        let compressed = u32::from_le_bytes(header[..4].try_into().unwrap()) as usize;
        let raw = u32::from_le_bytes(header[4..].try_into().unwrap()) as usize;
        if raw == 0
            || raw > BLOCK_BYTES
            || compressed > zstd::zstd_safe::compress_bound(BLOCK_BYTES)
        {
            return Err(other("Malformed OMT run block header"));
        }
        DECOMPRESS.with_borrow_mut(|state| {
            let (decompressor, frame) = match state {
                Some(state) => state,
                None => state.insert((
                    zstd::bulk::Decompressor::new()?,
                    Vec::with_capacity(zstd::zstd_safe::compress_bound(BLOCK_BYTES)),
                )),
            };
            frame.resize(compressed, 0);
            self.file
                .read_exact(frame)
                .map_err(|e| other(format!("Truncated OMT run block: {e}")))?;
            self.raw.clear();
            self.raw.reserve_exact(BLOCK_BYTES);
            let size = decompressor
                .decompress_to_buffer(frame.as_slice(), &mut self.raw)
                .map_err(|e| other(format!("Corrupt OMT run block: {e}")))?;
            if size != raw {
                return Err(other("OMT run block has the wrong length"));
            }
            Ok(())
        })?;
        self.position = 0;
        Ok(true)
    }
}

impl Read for RunReader {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position == self.raw.len() && !self.next_block()? {
            return Ok(0);
        }
        let read = buffer.len().min(self.raw.len() - self.position);
        buffer[..read].copy_from_slice(&self.raw[self.position..self.position + read]);
        self.position += read;
        Ok(read)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bytes(len: usize) -> Vec<u8> {
        // Compressible but not trivial.
        (0..len)
            .map(|i| ((i * 7) % 251) as u8 ^ (i >> 10) as u8)
            .collect()
    }

    fn roundtrip(data: &[u8], chunk: usize) -> Vec<u8> {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("run");
        let mut writer = RunWriter::create(&path).unwrap();
        for piece in data.chunks(chunk.max(1)) {
            writer.write_all(piece).unwrap();
        }
        assert_eq!(writer.finish().unwrap(), data.len() as u64);
        let mut read = vec![];
        RunReader::open(&path)
            .unwrap()
            .read_to_end(&mut read)
            .unwrap();
        read
    }

    #[test]
    fn round_trips_across_block_boundaries() {
        for len in [
            0,
            1,
            BLOCK_BYTES - 1,
            BLOCK_BYTES,
            BLOCK_BYTES + 1,
            3 * BLOCK_BYTES + 17,
        ] {
            let data = bytes(len);
            for chunk in [1, 1000, BLOCK_BYTES + 5, len] {
                assert_eq!(roundtrip(&data, chunk), data, "len {len} chunk {chunk}");
            }
        }
    }

    #[test]
    fn compresses_and_rejects_damage() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("run");
        let data = bytes(5 * BLOCK_BYTES);
        let mut writer = RunWriter::create(&path).unwrap();
        writer.write_all(&data).unwrap();
        writer.finish().unwrap();
        let file = std::fs::read(&path).unwrap();
        assert!(file.len() < data.len() / 4, "{} bytes", file.len());
        let read = |bytes: &[u8]| {
            std::fs::write(&path, bytes).unwrap();
            let mut out = vec![];
            RunReader::open(&path).unwrap().read_to_end(&mut out)
        };
        for end in [
            1,
            BLOCK_HEADER_BYTES,
            BLOCK_HEADER_BYTES + 3,
            file.len() - 1,
        ] {
            assert!(read(&file[..end]).is_err(), "prefix {end}");
        }
        let mut corrupt = file.clone();
        corrupt[BLOCK_HEADER_BYTES + 2] ^= 0xFF;
        corrupt[BLOCK_HEADER_BYTES + 5] ^= 0xFF;
        assert!(read(&corrupt).is_err());
        let mut long = file.clone();
        long[4..8].copy_from_slice(&(BLOCK_BYTES as u32 + 1).to_le_bytes());
        assert!(read(&long).is_err());
    }
}
