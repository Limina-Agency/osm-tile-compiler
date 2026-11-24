//! Checkpoints that let a compile continue after it was interrupted: killed
//! by the system (iOS ends an app at its memory limit or in the background),
//! crashed, cancelled or out of space.
//!
//! A resumable compile ([`crate::BasemapRequest::resume`]) works in a scratch
//! directory named after its output, `resume-<hash>` inside the request's
//! `temp_dir`, and keeps it when the compile does not finish. Beside the
//! scratch files it holds `checkpoint.json`, which names the stages already
//! done and what they left:
//!
//! - the merged extract, when the request gives several (`MergeExtracts`);
//! - the tile sort's runs, its layer metadata and the counts of the compile
//!   so far, once every OSM and supplemental source is rendered (the end of
//!   `ReadOsm`), and again after each pass of the sort's intermediate merge;
//! - the archive, once it is in place (`WriteTiles`), and the building
//!   levels.
//!
//! The file is replaced atomically, after what it names is on disk, so a
//! compile killed at any moment leaves either the previous checkpoint or the
//! next one. It records a fingerprint of the request and its inputs (paths,
//! sizes and modification times of the extracts and datasets, every option
//! that changes the archive, and the library's version); a compile that
//! finds a checkpoint for anything else starts over. A resumed compile
//! continues from the last stage done and writes the same archive, byte for
//! byte, as an uninterrupted one. The thread count and sort memory may
//! change between attempts.

use crate::basemap::BasemapRequest;
use crate::building_levels::BuildingLevelsReport;
use crate::control::CompileStage;
use crate::omt::{CompileResult, OpenMapTilesRequest, OsmDataInfo};
use crate::sources::SupplementalDataset;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

/// The checkpoint file's name inside the resumable scratch directory.
pub(crate) const CHECKPOINT_FILE: &str = "checkpoint.json";
const LOCK_FILE: &str = "resume.lock";
const FORMAT: &str = "osm-tile-compiler-checkpoint";
/// Raised whenever what a checkpoint records, or the scratch files it names,
/// change meaning.
const VERSION: u32 = 1;
/// The merged extract's name in the scratch directory.
pub(crate) const MERGED_EXTRACT: &str = "merged.osm.pbf";
/// The tile sort's directory in the scratch directory (see `omt::store`).
const SORT_DIRECTORY: &str = "omt-tile-sort";

/// Test hook: a process with this variable set to a checkpoint's name
/// (`merged_extract`, `rendered`, `merge_pass`, `archive`,
/// `levels`) aborts right after
/// committing it, as a kill would end it: no destructor runs.
const ABORT_AFTER: &str = "OSM_TILE_COMPILER_TEST_ABORT_AFTER";

/// The resumable scratch directory of a compile writing `output`, inside
/// `temp_dir`.
pub(crate) fn checkpoint_directory(temp_dir: &Path, output: &Path) -> PathBuf {
    let output = std::path::absolute(output).unwrap_or_else(|_| output.to_path_buf());
    let digest = Sha256::digest(output.as_os_str().as_encoded_bytes());
    temp_dir.join(format!("resume-{}", &hex(&digest)[..16]))
}

/// What a checkpoint file holds.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct Manifest {
    format: String,
    version: u32,
    fingerprint: String,
    /// Whether the first attempt created the request's `temp_dir`, which is
    /// then removed with the checkpoint if it is empty.
    #[serde(default)]
    made_temp_dir: bool,
    /// The merged extract's size, once it is complete.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    merged_extract: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    rendered: Option<Rendered>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    archive: Option<ArchiveDone>,
}

/// The state of a compile once every source is rendered into the sort.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct Rendered {
    pub sort: SortCheckpoint,
    pub sources_processed: u64,
    pub profile_features: u64,
    pub supplement_seconds: f64,
    pub osm_seconds: f64,
    /// Warnings of the stages done, without the merge cache's, which a
    /// resumed compile gives again.
    pub warnings: Vec<String>,
    pub osm_data: OsmDataInfo,
}

/// The tile sort's runs and layer metadata, in the sort directory.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub(crate) struct SortCheckpoint {
    /// The runs, in order, by file name and size.
    pub runs: Vec<(String, u64)>,
    /// A pass of the intermediate merge that may have been interrupted: each
    /// group's runs and the run they merge into. A group's runs are removed
    /// only after its output is complete, so a group with a run missing is
    /// done, and one with every run present is merged again.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub pending: Vec<(Vec<String>, String)>,
    /// The next merge output's number.
    pub sequence: usize,
    /// Uncompressed bytes the sorters spilled, for the merge's progress.
    pub spilled: u64,
    pub fields: BTreeMap<String, BTreeMap<String, String>>,
    pub zooms: BTreeMap<String, (u8, u8)>,
}

/// The archive in place, and the building levels once written.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct ArchiveDone {
    file: FileIdentity,
    result: CompileResult,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    levels: Option<(FileIdentity, BuildingLevelsReport)>,
}

/// A file's size and modification time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct FileIdentity {
    bytes: u64,
    modified: (u64, u32),
}

impl FileIdentity {
    fn of(path: &Path) -> Option<Self> {
        let metadata = fs::metadata(path).ok()?;
        let modified = metadata
            .modified()
            .ok()
            .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
            .map_or((0, 0), |since| (since.as_secs(), since.subsec_nanos()));
        Some(Self {
            bytes: metadata.len(),
            modified,
        })
    }
}

/// What a resumable compile found, and the stage it continued from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ResumePoint {
    /// After merging the extracts: the compile reads them next.
    ReadSources,
    /// After rendering, or partway through the sort's merge.
    WriteTiles,
    /// After writing the archive.
    BuildingLevels,
    /// Everything was written; only cleaning up was left.
    Done,
}

impl ResumePoint {
    /// The stage a compile resuming here continues from.
    pub(crate) fn stage(self) -> CompileStage {
        match self {
            Self::ReadSources => CompileStage::ReadSources,
            Self::WriteTiles => CompileStage::WriteTiles,
            Self::BuildingLevels | Self::Done => CompileStage::BuildingLevels,
        }
    }
}

/// The checkpoints of one resumable compile: its locked scratch directory
/// and the checkpoint file in it. Clones share the file, so the tile sort can
/// commit its own progress.
#[derive(Clone)]
pub(crate) struct Checkpoints {
    inner: Arc<Inner>,
}

struct Inner {
    directory: PathBuf,
    manifest: Mutex<Manifest>,
    /// Locked while the compile runs; a second compile of the same output
    /// is refused rather than sharing the directory.
    _lock: File,
}

impl Checkpoints {
    /// Claims the resumable scratch directory of `output` in `temp_dir`.
    /// Returns the checkpoints and whether a checkpoint for another request,
    /// or one that could not be read, was discarded.
    pub(crate) fn open(
        temp_dir: &Path,
        output: &Path,
        fingerprint: String,
    ) -> Result<(Self, bool), String> {
        let made_temp_dir = !temp_dir.exists();
        let directory = checkpoint_directory(temp_dir, output);
        fs::create_dir_all(&directory)
            .map_err(|e| format!("Failed to create {}: {e}", directory.display()))?;
        let lock_path = directory.join(LOCK_FILE);
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| format!("Failed to create {}: {e}", lock_path.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(format!(
                    "{} is in use by another compile of {}",
                    directory.display(),
                    output.display()
                ));
            }
            Err(fs::TryLockError::Error(e)) => {
                return Err(format!("Failed to lock {}: {e}", lock_path.display()));
            }
        }
        let path = directory.join(CHECKPOINT_FILE);
        let found = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Manifest>(&bytes).ok());
        let usable = found.filter(|manifest| {
            manifest.format == FORMAT
                && manifest.version == VERSION
                && manifest.fingerprint == fingerprint
        });
        let discarded = usable.is_none() && has_entries_besides(&directory, &lock_path);
        let checkpoints = Self {
            inner: Arc::new(Inner {
                directory: directory.clone(),
                manifest: Mutex::new(usable.clone().unwrap_or_else(|| Manifest {
                    format: FORMAT.into(),
                    version: VERSION,
                    fingerprint,
                    made_temp_dir,
                    ..Manifest::default()
                })),
                _lock: lock,
            }),
        };
        if usable.is_none() {
            // Whatever is here belongs to no usable checkpoint.
            remove_all_but(&directory, &lock_path)
                .map_err(|e| format!("Failed to clear {}: {e}", directory.display()))?;
            checkpoints.commit(None)?;
        }
        Ok((checkpoints, discarded))
    }

    /// Whether the compile's first attempt created the request's
    /// `temp_dir`.
    pub(crate) fn made_temp_dir(&self) -> bool {
        self.manifest().made_temp_dir
    }

    /// The scratch directory, which the compile works in.
    pub(crate) fn directory(&self) -> &Path {
        &self.inner.directory
    }

    fn manifest(&self) -> std::sync::MutexGuard<'_, Manifest> {
        self.inner
            .manifest
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Where the compile continues, given the archive and building levels
    /// paths.
    pub(crate) fn resume_point(&self, output: &Path, levels: Option<&Path>) -> Option<ResumePoint> {
        let manifest = self.manifest();
        if let Some(archive) = &manifest.archive
            && FileIdentity::of(output).as_ref() == Some(&archive.file)
        {
            let levels_done = match (levels, &archive.levels) {
                (None, _) => true,
                (Some(path), Some((file, _))) => FileIdentity::of(path).as_ref() == Some(file),
                (Some(_), None) => false,
            };
            return Some(if levels_done {
                ResumePoint::Done
            } else {
                ResumePoint::BuildingLevels
            });
        }
        if manifest.rendered.is_some() {
            return Some(ResumePoint::WriteTiles);
        }
        let merged = self.inner.directory.join(MERGED_EXTRACT);
        if manifest.merged_extract.is_some()
            && manifest.merged_extract == fs::metadata(merged).ok().map(|m| m.len())
        {
            return Some(ResumePoint::ReadSources);
        }
        None
    }

    /// The merged extract, if a checkpoint holds it complete.
    pub(crate) fn merged_extract(&self) -> Option<PathBuf> {
        let path = self.inner.directory.join(MERGED_EXTRACT);
        let bytes = self.manifest().merged_extract?;
        (fs::metadata(&path).ok()?.len() == bytes).then_some(path)
    }

    pub(crate) fn commit_merged_extract(&self, path: &Path) -> Result<(), String> {
        sync_file(path)?;
        let bytes = fs::metadata(path)
            .map_err(|e| format!("Failed to read {}: {e}", path.display()))?
            .len();
        self.update("merged_extract", |manifest| {
            manifest.merged_extract = Some(bytes)
        })
    }

    /// The rendered state, once checkpointed.
    pub(crate) fn rendered(&self) -> Option<Rendered> {
        self.manifest().rendered.clone()
    }

    /// Records the rendered state. The sort's runs must already be on disk
    /// (see `Store::checkpoint`).
    pub(crate) fn commit_rendered(&self, rendered: Rendered) -> Result<(), String> {
        self.update("rendered", |manifest| manifest.rendered = Some(rendered))
    }

    /// Records the sort's runs after, or before, a pass of its merge.
    pub(crate) fn commit_sort(&self, sort: SortCheckpoint) -> Result<(), String> {
        self.update("merge_pass", |manifest| {
            if let Some(rendered) = &mut manifest.rendered {
                rendered.sort = sort;
            }
        })
    }

    /// The compile's result, once its archive is in place at `output`.
    pub(crate) fn archive_result(&self, output: &Path) -> Option<CompileResult> {
        let manifest = self.manifest();
        let archive = manifest.archive.as_ref()?;
        (FileIdentity::of(output).as_ref() == Some(&archive.file)).then(|| archive.result.clone())
    }

    /// Records the archive at `output`, and removes what only writing it
    /// needed.
    pub(crate) fn commit_archive(
        &self,
        output: &Path,
        result: &CompileResult,
    ) -> Result<(), String> {
        sync_file(output)?;
        let file = FileIdentity::of(output)
            .ok_or_else(|| format!("Failed to read {}", output.display()))?;
        self.update("archive", |manifest| {
            manifest.archive = Some(ArchiveDone {
                file,
                result: result.clone(),
                levels: None,
            });
            manifest.rendered = None;
            manifest.merged_extract = None;
        })?;
        let _ = fs::remove_dir_all(self.inner.directory.join(SORT_DIRECTORY));
        let _ = fs::remove_file(self.inner.directory.join(MERGED_EXTRACT));
        Ok(())
    }

    /// The building levels at `path`, once written.
    pub(crate) fn levels_report(&self, path: &Path) -> Option<BuildingLevelsReport> {
        let manifest = self.manifest();
        let (file, report) = manifest.archive.as_ref()?.levels.as_ref()?;
        (FileIdentity::of(path).as_ref() == Some(file)).then(|| report.clone())
    }

    pub(crate) fn commit_levels(
        &self,
        path: &Path,
        report: &BuildingLevelsReport,
    ) -> Result<(), String> {
        sync_file(path)?;
        let file =
            FileIdentity::of(path).ok_or_else(|| format!("Failed to read {}", path.display()))?;
        self.update("levels", |manifest| {
            if let Some(archive) = &mut manifest.archive {
                archive.levels = Some((file, report.clone()));
            }
        })
    }

    fn update(&self, name: &str, change: impl FnOnce(&mut Manifest)) -> Result<(), String> {
        change(&mut self.manifest());
        self.commit(Some(name))
    }

    /// Replaces the checkpoint file with the current manifest: written
    /// beside it, synced, then renamed over it.
    fn commit(&self, name: Option<&str>) -> Result<(), String> {
        let json = serde_json::to_vec(&*self.manifest()).map_err(|e| e.to_string())?;
        let path = self.inner.directory.join(CHECKPOINT_FILE);
        let partial = self
            .inner
            .directory
            .join(format!("{CHECKPOINT_FILE}.partial"));
        let failed = |e: std::io::Error| format!("Failed to write {}: {e}", path.display());
        let mut file = File::create(&partial).map_err(failed)?;
        file.write_all(&json).map_err(failed)?;
        file.sync_all().map_err(failed)?;
        drop(file);
        fs::rename(&partial, &path).map_err(failed)?;
        sync_directory(&self.inner.directory);
        if let Some(name) = name
            && std::env::var_os(ABORT_AFTER).is_some_and(|value| value == name)
        {
            std::process::abort();
        }
        Ok(())
    }

    /// Removes the scratch directory once the compile is complete.
    pub(crate) fn finish(self) {
        let directory = self.inner.directory.clone();
        // The lock is closed first: Windows cannot remove an open file.
        drop(self);
        let _ = fs::remove_dir_all(directory);
    }
}

/// Removes a request's checkpoint: the resumable scratch directory of its
/// output in its `temp_dir`. Returns whether there was one. A compile
/// holding the directory is refused.
pub(crate) fn discard(temp_dir: &Path, output: &Path) -> Result<bool, String> {
    let directory = checkpoint_directory(temp_dir, output);
    if !directory.exists() {
        return Ok(false);
    }
    let lock_path = directory.join(LOCK_FILE);
    if let Ok(lock) = File::options().write(true).open(&lock_path) {
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(format!("{} is in use by a compile", directory.display()));
            }
            Err(fs::TryLockError::Error(e)) => {
                return Err(format!("Failed to lock {}: {e}", lock_path.display()));
            }
        }
        remove_all_but(&directory, &lock_path)
            .map_err(|e| format!("Failed to remove {}: {e}", directory.display()))?;
    }
    fs::remove_dir_all(&directory)
        .map_err(|e| format!("Failed to remove {}: {e}", directory.display()))?;
    Ok(true)
}

/// A digest of everything that decides what a compile writes: the request's
/// options, the identity of each input file, and the library's version. The
/// thread count, the sort memory and the scratch directory are left out:
/// they change how the archive is made, not its bytes.
pub(crate) fn fingerprint(
    request: &BasemapRequest,
    compile: &OpenMapTilesRequest,
    used: &[SupplementalDataset],
) -> String {
    let mut options = compile.clone();
    options.threads = 0;
    options.sort_memory = None;
    options.temp_dir = PathBuf::new();
    let mut hasher = Sha256::new();
    let mut add = |part: &str| {
        hasher.update((part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    };
    add(FORMAT);
    add(&VERSION.to_string());
    add(env!("CARGO_PKG_VERSION"));
    add(crate::omt::PROFILE_REVISION);
    add(&format!("{options:?}"));
    add(&format!("building_levels={}", request.building_levels));
    let files = std::iter::once(request.input_pbf.clone())
        .chain(request.extra_inputs.iter().cloned())
        .chain(
            used.iter()
                .map(|dataset| request.sources.join(dataset.file_name())),
        );
    for file in files {
        add(&format!("{file:?}={:?}", FileIdentity::of(&file)));
    }
    hex(&hasher.finalize())
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Forces a file's data to disk, so that a checkpoint naming it survives a
/// power loss, not only a kill.
pub(crate) fn sync_file(path: &Path) -> Result<(), String> {
    // Windows flushes only a handle opened for writing (FlushFileBuffers
    // fails with "Access is denied" on a read-only one). Opening for writing
    // without truncating leaves the file as it is.
    File::options()
        .read(true)
        .write(cfg!(windows))
        .open(path)
        .and_then(|file| file.sync_all())
        .map_err(|e| format!("Failed to sync {}: {e}", path.display()))
}

/// Makes a rename in `directory` durable where the system allows it.
fn sync_directory(directory: &Path) {
    #[cfg(unix)]
    if let Ok(dir) = File::open(directory) {
        let _ = dir.sync_all();
    }
    #[cfg(not(unix))]
    let _ = directory;
}

fn has_entries_besides(directory: &Path, keep: &Path) -> bool {
    fs::read_dir(directory).is_ok_and(|entries| {
        entries
            .flatten()
            .any(|entry| entry.path() != keep && entry.file_name() != CHECKPOINT_FILE)
    })
}

fn remove_all_but(directory: &Path, keep: &Path) -> std::io::Result<()> {
    for entry in fs::read_dir(directory)? {
        let path = entry?.path();
        if path == keep {
            continue;
        }
        if path.is_dir() {
            fs::remove_dir_all(&path)?;
        } else {
            fs::remove_file(&path)?;
        }
    }
    Ok(())
}
