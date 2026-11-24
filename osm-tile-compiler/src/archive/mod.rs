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
mod reader;
mod tile_patch;

pub use check::ArchiveReport;
pub(crate) use check::glyph_ranges_used;
#[doc(hidden)]
pub use delta::partial_path;
pub use delta::{
    ApplyReport, ArchiveDigest, DELTA_FORMAT, DELTA_VERSION, DeltaManifest, DiffReport, TileChanges,
};
pub use extract::{ExtractReport, ExtractRequest, extract};
pub(crate) use reader::decompress as decompress_tile;
#[doc(hidden)]
pub use reader::{ArchiveReader, Entry, Header};

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
    delta::diff(base, target, output).map_err(CompileError::other)
}

/// Applies the delta at `delta` to the archive `base`, writing the new
/// archive to `output`, which must not exist yet. Checks `base` before and
/// the result after; on any failure nothing is left at `output`.
pub fn apply(base: &Path, delta: &Path, output: &Path) -> Result<ApplyReport, CompileError> {
    delta::apply(base, delta, output).map_err(CompileError::other)
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
}

impl Default for ArchiveOptions {
    fn default() -> Self {
        Self {
            name: "OpenMapTiles".into(),
            description: "Native Rust OpenMapTiles profile".into(),
            attribution: DEFAULT_ATTRIBUTION.into(),
            gzip_level: 6,
        }
    }
}
