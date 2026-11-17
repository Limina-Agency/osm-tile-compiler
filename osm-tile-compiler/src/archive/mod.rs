//! The PMTiles archive a compile writes: options for its metadata and tile
//! compression, and a reader that inspects and verifies finished archives.
use serde::{Deserialize, Serialize};

mod check;
mod reader;

pub use check::{ArchiveReport, inspect};
pub use reader::{ArchiveReader, Entry};

/// The attribution OpenMapTiles' license and the ODbL require for archives in
/// this schema built from OpenStreetMap.
pub const DEFAULT_ATTRIBUTION: &str = "<a href=\"https://www.openmaptiles.org/\" target=\"_blank\">&copy; OpenMapTiles</a> <a href=\"https://www.openstreetmap.org/copyright\" target=\"_blank\">&copy; OpenStreetMap contributors</a>";

/// What a compile writes into the archive besides its tiles.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
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
