//! The PMTiles archive a compile writes: options for its metadata and tile
//! compression, a reader that inspects and verifies finished archives, delta
//! archives that update one archive to a newer one, and extracts that cut an
//! area out of one.
use crate::CompileError;
use serde::{Deserialize, Serialize};
use std::path::Path;

mod check;
mod delta;
mod extract;
mod http;
mod reader;
mod signing;
mod tile_patch;

pub use check::ArchiveReport;
pub(crate) use check::glyph_ranges_used;
#[doc(hidden)]
pub use delta::partial_path;
pub use delta::{
    ApplyReport, ArchiveDigest, DELTA_FORMAT, DELTA_VERSION, DeltaManifest, DeltaVerification,
    DiffOptions, DiffReport, TileChanges,
};
pub use extract::{ExtractReport, ExtractRequest, extract, is_remote_source};
pub(crate) use reader::decompress as decompress_tile;
#[doc(hidden)]
pub use reader::{ArchiveReader, Entry, Header};
pub use signing::{
    PublicKey, SIGNATURE_FORMAT, SIGNATURE_VERSION, SecretKey, SignatureCheck, signature_path,
};

/// Decompresses a gzip-compressed tile.
pub(crate) fn decompress_gzip(bytes: &[u8]) -> Result<Vec<u8>, String> {
    reader::decompress(bytes, 2)
}

/// Reads every part of the archive at `path`. Fails only when the file is
/// not a readable PMTiles v3 archive; anything else wrong is listed in the
/// report's problems.
pub fn inspect(path: &Path) -> Result<ArchiveReport, CompileError> {
    check::inspect(path).map_err(CompileError::other)
}

/// Writes the delta that turns the archive `base` into `target` to `output`,
/// which must not exist yet; [`apply`] replays it. Before keeping it,
/// rebuilds `target` from it and checks the result.
pub fn diff(base: &Path, target: &Path, output: &Path) -> Result<DiffReport, CompileError> {
    diff_with(base, target, output, &DiffOptions::default())
}

/// [`diff`], signing the delta or listing the tiles that differ as
/// `options` asks.
pub fn diff_with(
    base: &Path,
    target: &Path,
    output: &Path,
    options: &DiffOptions,
) -> Result<DiffReport, CompileError> {
    delta::diff(base, target, output, options).map_err(CompileError::other)
}

/// Applies the delta at `delta` to the archive `base`, writing the new
/// archive to `output`, which must not exist yet. Checks `base` before and
/// the result after; on any failure nothing is left at `output`. A delta
/// need not be signed, but one whose signature doesn't match is refused; to
/// require a signature by a key you trust, use [`apply_trusted`].
pub fn apply(base: &Path, delta: &Path, output: &Path) -> Result<ApplyReport, CompileError> {
    apply_trusted(base, delta, output, &[])
}

/// [`apply`], requiring a valid signature by one of `trusted` unless it is
/// empty. The signatures are checked before anything is written, and the
/// signed bytes again as they are applied.
pub fn apply_trusted(
    base: &Path,
    delta: &Path,
    output: &Path,
    trusted: &[PublicKey],
) -> Result<ApplyReport, CompileError> {
    delta::apply(base, delta, output, trusted).map_err(CompileError::other)
}

/// Checks the signatures of the delta at `path` without applying it: every
/// signature must be valid, and when `trusted` names keys, one must be by a
/// trusted key.
pub fn verify_delta(path: &Path, trusted: &[PublicKey]) -> Result<DeltaVerification, CompileError> {
    delta::verify(path, trusted).map_err(CompileError::other)
}

/// Signs the delta at `path` with `key`, in place: the signature goes in
/// the delta's footer, beside any others, replacing an earlier one by the
/// same key.
pub fn sign_delta(path: &Path, key: &SecretKey) -> Result<SignatureCheck, CompileError> {
    delta::sign(path, key).map_err(CompileError::other)
}

/// Signs `file`, such as an archive, with `key` in a detached signature
/// file beside it, [`signature_path`] (`<FILE>.sig`), keeping the signatures
/// already there by other keys when they sign the same bytes.
pub fn sign_file(file: &Path, key: &SecretKey) -> Result<SignatureCheck, CompileError> {
    signing::sign_file(file, key).map_err(CompileError::other)
}

/// Checks `file` against its detached signature file (`signature`, or
/// [`signature_path`] by default): the bytes must be the ones signed, every
/// signature valid, and when `trusted` names keys, one by a trusted key.
pub fn verify_file_signature(
    file: &Path,
    signature: Option<&Path>,
    trusted: &[PublicKey],
) -> Result<SignatureCheck, CompileError> {
    signing::verify_file(file, signature, trusted).map_err(CompileError::other)
}

/// Whether the file at `path` is a delta archive, by its first bytes.
pub fn is_delta(path: &Path) -> bool {
    let mut lead = [0u8; 8];
    std::fs::File::open(path)
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut lead))
        .is_ok()
        && &lead == b"OTCDELTA"
}

/// The manifest at the start of the delta at `path`.
pub fn read_delta_manifest(path: &Path) -> Result<DeltaManifest, CompileError> {
    delta::read_manifest(path).map_err(CompileError::other)
}

/// The attribution OpenMapTiles' license and the ODbL require for archives in
/// this schema built from OpenStreetMap.
pub const DEFAULT_ATTRIBUTION: &str = "<a href=\"https://www.openmaptiles.org/\" target=\"_blank\">&copy; OpenMapTiles</a> <a href=\"https://www.openstreetmap.org/copyright\" target=\"_blank\">&copy; OpenStreetMap contributors</a>";

/// What a compile writes into the archive besides its tiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
#[non_exhaustive]
pub struct ArchiveOptions {
    /// The metadata's `name`.
    pub name: String,
    /// The metadata's `description`.
    pub description: String,
    /// The metadata's `attribution`, HTML that viewers show on the map. It
    /// must keep crediting OpenMapTiles and OpenStreetMap.
    pub attribution: String,
    /// The gzip level of the tiles, 0 (stored) to 9 (smallest, slowest).
    pub gzip_level: u32,
    /// How the tiles are encoded: MVT, or (experimental) MLT.
    pub tile_format: crate::TileFormat,
}

impl Default for ArchiveOptions {
    fn default() -> Self {
        Self {
            name: "OpenMapTiles".into(),
            description: "Native Rust OpenMapTiles profile".into(),
            attribution: DEFAULT_ATTRIBUTION.into(),
            gzip_level: 6,
            tile_format: crate::TileFormat::Mvt,
        }
    }
}
