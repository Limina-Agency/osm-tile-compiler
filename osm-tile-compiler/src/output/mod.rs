//! Where a compile's tiles go. The tile encoder hands finished tiles, in
//! tile id order, to a [`TileWriter`]; which archive format it writes is
//! decided here, so the encoder never depends on a format.

mod mbtiles;
mod pmtiles;

#[cfg(test)]
pub(crate) use self::mbtiles::MbtilesWriter;
pub(crate) use self::pmtiles::PmtilesArchiveWriter;
use crate::{ArchiveCenter, CompileBounds};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// The archive format a compile writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum OutputFormat {
    /// One PMTiles v3 archive, the default.
    PmTiles,
    /// One MBTiles 1.3 SQLite database. Identical tiles of up to 1 KiB are
    /// stored once: `tiles` is a view over `map` and `images`, as in
    /// Planetiler's compact MBTiles.
    MbTiles,
}

impl OutputFormat {
    pub const ALL: [Self; 2] = [Self::PmTiles, Self::MbTiles];

    /// The format a path's extension names: `.mbtiles` (in any case) is
    /// MBTiles, anything else PMTiles.
    pub fn from_path(path: &Path) -> Self {
        match path.extension().and_then(|e| e.to_str()) {
            Some(extension) if extension.eq_ignore_ascii_case("mbtiles") => Self::MbTiles,
            _ => Self::PmTiles,
        }
    }

    /// The identifier the command line and the C ABI take: `pmtiles` or
    /// `mbtiles`.
    pub fn id(self) -> &'static str {
        match self {
            Self::PmTiles => "pmtiles",
            Self::MbTiles => "mbtiles",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.id() == id)
    }

    /// The format's name in messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::PmTiles => "PMTiles",
            Self::MbTiles => "MBTiles",
        }
    }
}

/// The encoding of the tiles in an archive.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
#[non_exhaustive]
pub enum TileFormat {
    /// Mapbox Vector Tiles 2.1, the default, which every viewer reads.
    #[default]
    Mvt,
    /// MapLibre Tiles (MLT) v1, which MapLibre GL JS 5.12, MapLibre
    /// Android 13.4 and MapLibre iOS 6.28 and later read, given
    /// `"encoding": "mlt"` on the source. The tiles hold the same features as
    /// the MVT ones (`docs/mlt.md`).
    Mlt,
}

impl TileFormat {
    pub const ALL: [Self; 2] = [Self::Mvt, Self::Mlt];

    /// The identifier the command line takes: `mvt` or `mlt`.
    pub fn id(self) -> &'static str {
        match self {
            Self::Mvt => "mvt",
            Self::Mlt => "mlt",
        }
    }

    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|format| format.id() == id)
    }

    /// The format's name in messages.
    pub fn name(self) -> &'static str {
        match self {
            Self::Mvt => "MVT",
            Self::Mlt => "MLT",
        }
    }

    /// The PMTiles header's tile type.
    pub(crate) fn pmtiles(self) -> ::pmtiles::TileType {
        match self {
            Self::Mvt => ::pmtiles::TileType::Mvt,
            Self::Mlt => ::pmtiles::TileType::Mlt,
        }
    }

    /// The PMTiles header's tile type byte.
    pub(crate) fn pmtiles_code(self) -> u8 {
        match self {
            Self::Mvt => 1,
            Self::Mlt => 6,
        }
    }

    /// The metadata's `format`, as MBTiles and TileJSON name it.
    pub(crate) fn metadata_format(self) -> &'static str {
        match self {
            Self::Mvt => "pbf",
            Self::Mlt => "mlt",
        }
    }
}

/// What an archive declares besides its tiles.
pub(crate) struct ArchiveSpec<'a> {
    pub bounds: &'a CompileBounds,
    pub center: ArchiveCenter,
    /// The TileJSON-like metadata object, as JSON text.
    pub metadata_json: &'a str,
    pub min_zoom: u8,
    pub max_zoom: u8,
    pub tile_format: TileFormat,
}

/// Takes gzip-compressed tiles in ascending tile id order (the PMTiles
/// Hilbert id), each id at most once.
pub(crate) trait TileWriter: Send {
    fn write_tile(&mut self, tile_id: u64, bytes: &[u8]) -> Result<(), String>;
    /// Completes the archive. A writer dropped without it leaves an archive
    /// that is not valid, which the caller removes.
    fn finalize(self: Box<Self>) -> Result<(), String>;
}

/// Something a [`TileWriter`] can be opened on.
pub(crate) trait ArchiveTarget {
    fn open_writer(&self, spec: &ArchiveSpec) -> Result<Box<dyn TileWriter + '_>, String>;
}

/// A plain file takes a PMTiles archive.
impl ArchiveTarget for File {
    fn open_writer(&self, spec: &ArchiveSpec) -> Result<Box<dyn TileWriter + '_>, String> {
        Ok(Box::new(PmtilesArchiveWriter::create(self, spec)?))
    }
}

impl ArchiveTarget for PartialArchive {
    fn open_writer(&self, spec: &ArchiveSpec) -> Result<Box<dyn TileWriter + '_>, String> {
        match &self.database {
            None => self.file().open_writer(spec),
            Some(path) => Ok(Box::new(mbtiles::MbtilesWriter::create(path, spec)?)),
        }
    }
}

/// The archive while it is written: `<output>.<pid>.partial`, locked until it
/// is moved into place. Concurrent compiles to one output never share it, and
/// a later compile can tell one a killed compile left (unlocked) from one
/// being written. Dropped before `persist` succeeds, it is removed.
///
/// SQLite opens and locks the files it writes itself, and on Windows a lock
/// held on the same file would block it. So an MBTiles archive is written
/// beside the locked file, as `<output>.<pid>.partial-db`, and the locked
/// file only marks it as in use.
pub(crate) struct PartialArchive {
    path: PathBuf,
    file: Option<File>,
    /// Where an MBTiles archive is written.
    database: Option<PathBuf>,
    finished: bool,
}

const PARTIAL: &str = ".partial";
const DATABASE: &str = "-db";

impl PartialArchive {
    pub(crate) fn create(output: &Path, format: OutputFormat) -> Result<Self, String> {
        remove_abandoned_partials(output);
        let mut name = output.as_os_str().to_owned();
        name.push(format!(".{}{PARTIAL}", std::process::id()));
        let path = PathBuf::from(name);
        let file = File::options()
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|e| format!("Failed to create {}: {e}", path.display()))?;
        file.try_lock()
            .map_err(|e| format!("Failed to lock {}: {e}", path.display()))?;
        let database = (format == OutputFormat::MbTiles).then(|| {
            let mut name = path.as_os_str().to_owned();
            name.push(DATABASE);
            PathBuf::from(name)
        });
        let partial = Self {
            path,
            file: Some(file),
            database,
            finished: false,
        };
        if let Some(database) = &partial.database {
            // One left by a compile with this process id, whose marker is
            // gone, so no compile is writing it.
            let _ = std::fs::remove_file(database);
        }
        Ok(partial)
    }

    /// The locked file, which a PMTiles archive is written into.
    pub(crate) fn file(&self) -> &File {
        self.file
            .as_ref()
            .expect("the archive is open until persist")
    }

    /// Moves the finished archive to `output`. If that fails, it is left
    /// where it is and the error names it.
    pub(crate) fn persist(mut self, output: &Path) -> Result<(), String> {
        if output.exists() {
            return Err(format!(
                "refusing to replace existing archive {}",
                output.display()
            ));
        }
        let written = self.database.clone().unwrap_or_else(|| self.path.clone());
        self.finished = true;
        // Unix renames the file while it is still locked.
        let mut moved = std::fs::rename(&written, output);
        if moved.is_err() {
            // Windows cannot rename an open file, and virus scanners briefly
            // hold new ones.
            if self.database.is_none() {
                drop(self.file.take());
            }
            for attempt in 0..10 {
                moved = std::fs::rename(&written, output);
                if !moved
                    .as_ref()
                    .is_err_and(|e| e.kind() == std::io::ErrorKind::PermissionDenied)
                {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100 << attempt.min(4)));
            }
        }
        if moved.is_ok() && self.database.is_some() {
            // The marker has done its job.
            drop(self.file.take());
            let _ = std::fs::remove_file(&self.path);
        }
        moved.map_err(|e| {
            format!(
                "Failed to move the finished archive {} to {}: {e}",
                written.display(),
                output.display()
            )
        })
    }
}

impl Drop for PartialArchive {
    fn drop(&mut self) {
        if !self.finished {
            // The database first: while the marker stays locked, no other
            // compile removes it.
            if let Some(database) = &self.database {
                let _ = std::fs::remove_file(database);
            }
            drop(self.file.take());
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// Removes the `<output>.<pid>.partial` files that no compile holds locked,
/// those left by killed compiles, with the `<output>.<pid>.partial-db`
/// databases beside them.
fn remove_abandoned_partials(output: &Path) {
    let Some(name) = output.file_name().and_then(|name| name.to_str()) else {
        return;
    };
    let parent = output
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let Ok(entries) = std::fs::read_dir(parent) else {
        return;
    };
    for entry in entries.flatten() {
        let file_name = entry.file_name();
        let Some(file) = file_name.to_str() else {
            continue;
        };
        let Some(rest) = file
            .strip_prefix(name)
            .and_then(|rest| rest.strip_prefix('.'))
        else {
            continue;
        };
        let (pid, database) = match rest.strip_suffix(DATABASE) {
            Some(rest) => (rest.strip_suffix(PARTIAL), true),
            None => (rest.strip_suffix(PARTIAL), false),
        };
        if !pid.is_some_and(|pid| !pid.is_empty() && pid.bytes().all(|b| b.is_ascii_digit())) {
            continue;
        }
        let path = entry.path();
        if database {
            // A database whose marker is gone: its compile ended without
            // removing it. One with a marker goes with its marker.
            let marker = parent.join(&file[..file.len() - DATABASE.len()]);
            if !marker.exists() {
                let _ = std::fs::remove_file(&path);
            }
            continue;
        }
        let Ok(file) = File::options().write(true).open(&path) else {
            continue;
        };
        if file.try_lock().is_ok() {
            drop(file);
            let mut database = path.clone().into_os_string();
            database.push(DATABASE);
            let _ = std::fs::remove_file(database);
            let _ = std::fs::remove_file(&path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_follow_the_extension_and_ids() {
        assert_eq!(
            OutputFormat::from_path(Path::new("a/b.mbtiles")),
            OutputFormat::MbTiles
        );
        assert_eq!(
            OutputFormat::from_path(Path::new("B.MBTiles")),
            OutputFormat::MbTiles
        );
        assert_eq!(
            OutputFormat::from_path(Path::new("b.pmtiles")),
            OutputFormat::PmTiles
        );
        assert_eq!(
            OutputFormat::from_path(Path::new("b")),
            OutputFormat::PmTiles
        );
        for format in OutputFormat::ALL {
            assert_eq!(OutputFormat::from_id(format.id()), Some(format));
        }
        assert_eq!(OutputFormat::from_id("MBTiles"), None);
    }

    #[test]
    fn abandoned_mbtiles_partials_are_removed_and_live_ones_kept() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("map.mbtiles");
        let path = |name: &str| dir.path().join(name);
        // Killed compiles: an unlocked marker with its database, and a
        // database whose marker is gone.
        std::fs::write(path("map.mbtiles.11.partial"), b"").unwrap();
        std::fs::write(path("map.mbtiles.11.partial-db"), b"db").unwrap();
        std::fs::write(path("map.mbtiles.12.partial-db"), b"db").unwrap();
        // A running compile: a locked marker and its database.
        let live = File::create(path("map.mbtiles.13.partial")).unwrap();
        live.try_lock().unwrap();
        std::fs::write(path("map.mbtiles.13.partial-db"), b"db").unwrap();
        // Not ours.
        std::fs::write(path("map.mbtiles.x.partial-db"), b"db").unwrap();

        let partial = PartialArchive::create(&output, OutputFormat::MbTiles).unwrap();
        for gone in ["11.partial", "11.partial-db", "12.partial-db"] {
            assert!(!path(&format!("map.mbtiles.{gone}")).exists(), "{gone}");
        }
        for kept in ["13.partial", "13.partial-db", "x.partial-db"] {
            assert!(path(&format!("map.mbtiles.{kept}")).exists(), "{kept}");
        }
        live.unlock().unwrap();

        // A dropped partial archive leaves nothing; a persisted one only
        // the output.
        let database = partial.database.clone().unwrap();
        std::fs::write(&database, b"db").unwrap();
        drop(partial);
        assert!(!database.exists());
        let partial = PartialArchive::create(&output, OutputFormat::MbTiles).unwrap();
        let (marker, database) = (partial.path.clone(), partial.database.clone().unwrap());
        assert!(marker.exists());
        std::fs::write(&database, b"db").unwrap();
        partial.persist(&output).unwrap();
        assert_eq!(std::fs::read(&output).unwrap(), b"db");
        assert!(!marker.exists() && !database.exists());
    }
}
