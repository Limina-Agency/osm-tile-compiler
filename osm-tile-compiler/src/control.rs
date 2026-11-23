//! Progress and cancellation for a compile running inside an app.
//!
//! The work runs on a thread of a rayon pool built for the compile; the
//! calling thread meanwhile runs a monitor that calls the caller's progress
//! and cancel callbacks, so neither needs to be `Send` or `Sync`. The monitor
//! polls the cancel callback every [`POLL`] and raises a flag the work checks
//! between stages and inside its long loops.

use std::fmt;
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::Thread;
use std::time::{Duration, Instant};

/// How often the monitor asks the caller whether to stop.
const POLL: Duration = Duration::from_millis(50);
/// Progress within a stage is reported at most this often.
const THROTTLE: Duration = Duration::from_millis(200);

/// The message every cancelled step returns internally; [`CompileError`]
/// turns any failure after a cancel into [`CompileError::Cancelled`].
pub(crate) const CANCELLED: &str = "The compile was cancelled";

/// Why a compile or a download did not finish. Whatever the kind,
/// temporary files and partial outputs have been removed, and each message
/// is in words fit to show.
///
/// More kinds may be added in later versions, so match with a wildcard arm,
/// or use [`kind`](Self::kind).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum CompileError {
    /// The cancel callback asked to stop.
    Cancelled,
    /// The request cannot run as given, found before any stage started: an
    /// unknown layer or language, no threads, an output that already exists,
    /// a PBF or polygon that cannot be opened or read, or supplemental
    /// datasets missing when the request does not fetch them.
    InvalidRequest(String),
    /// Downloading or verifying a supplemental dataset failed: the network,
    /// the server, or a file that does not match its pinned hash.
    Download(String),
    /// A write failed because the disk is full.
    OutOfSpace(String),
    /// Anything else, with the stage it happened in, if one had started:
    /// for example a PBF that turns out corrupt while it is read.
    Failed {
        stage: Option<CompileStage>,
        message: String,
    },
}

impl CompileError {
    /// A stable identifier for the kind: `cancelled`, `invalid_request`,
    /// `download`, `out_of_space` or `failed`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Cancelled => "cancelled",
            Self::InvalidRequest(_) => "invalid_request",
            Self::Download(_) => "download",
            Self::OutOfSpace(_) => "out_of_space",
            Self::Failed { .. } => "failed",
        }
    }

    /// The message, in words fit to show.
    pub fn message(&self) -> &str {
        match self {
            Self::Cancelled => CANCELLED,
            Self::InvalidRequest(message)
            | Self::Download(message)
            | Self::OutOfSpace(message)
            | Self::Failed { message, .. } => message,
        }
    }

    /// A failure outside a compile's stages, such as reading or writing an
    /// archive: [`OutOfSpace`](Self::OutOfSpace) for a full disk, else
    /// [`Failed`](Self::Failed) with no stage.
    pub(crate) fn other(message: String) -> Self {
        if is_out_of_space(&message) {
            Self::OutOfSpace(message)
        } else {
            Self::Failed {
                stage: None,
                message,
            }
        }
    }

    /// Sorts a failure by where it happened: before any stage, it is the
    /// request's; in `FetchSources`, the download's. A full disk is
    /// recognised by its OS error code wherever it happens.
    pub(crate) fn classify(message: String, stage: Option<CompileStage>) -> Self {
        if is_out_of_space(&message) {
            return Self::OutOfSpace(message);
        }
        match stage {
            None => Self::InvalidRequest(message),
            Some(CompileStage::FetchSources) => Self::Download(message),
            stage => Self::Failed { stage, message },
        }
    }
}

/// Whether `message` carries the OS error for a full disk, as
/// `std::io::Error` writes it: ENOSPC is 28 on Linux, macOS and iOS;
/// Windows has ERROR_HANDLE_DISK_FULL (39) and ERROR_DISK_FULL (112).
fn is_out_of_space(message: &str) -> bool {
    let codes: &[i32] = if cfg!(windows) { &[39, 112] } else { &[28] };
    codes
        .iter()
        .any(|code| message.contains(&format!("(os error {code})")))
}

impl fmt::Display for CompileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.message())
    }
}

impl std::error::Error for CompileError {}

/// The message, for callers that report errors as text.
impl From<CompileError> for String {
    fn from(error: CompileError) -> Self {
        error.to_string()
    }
}

/// The stages of [`crate::compile_basemap`], in the order they run. Their
/// [`id`](Self::id)s are stable, for apps that store or match them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[non_exhaustive]
pub enum CompileStage {
    /// Checking the supplemental datasets against their recorded hashes and
    /// downloading any that are missing or changed. Its fraction is bytes
    /// checked or received over the bytes expected; it only runs when the
    /// request asks to fetch.
    FetchSources,
    /// Extracting (once; later compiles reuse it) and reading the
    /// supplemental datasets. Its fraction counts the datasets read.
    ReadSources,
    /// The PBF's first pass: node locations and relations. Its fraction
    /// counts the PBF blocks read.
    IndexOsm,
    /// The PBF's second pass: assembling ways and relations into features
    /// and cutting them into tiles. Its fraction is estimated from the ways
    /// read, then the relations.
    ReadOsm,
    /// Sorting the tiles' features, encoding the tiles and writing the
    /// archive. Its fraction counts the bytes merged.
    WriteTiles,
    /// Writing the building levels beside the archive. Its fraction counts
    /// the groups of tiles done.
    BuildingLevels,
}

impl CompileStage {
    /// Every stage, in order.
    pub const ALL: [Self; 6] = [
        Self::FetchSources,
        Self::ReadSources,
        Self::IndexOsm,
        Self::ReadOsm,
        Self::WriteTiles,
        Self::BuildingLevels,
    ];

    /// A stable identifier: `fetch_sources`, `read_sources`, `index_osm`,
    /// `read_osm`, `write_tiles` or `building_levels`.
    pub fn id(self) -> &'static str {
        match self {
            Self::FetchSources => "fetch_sources",
            Self::ReadSources => "read_sources",
            Self::IndexOsm => "index_osm",
            Self::ReadOsm => "read_osm",
            Self::WriteTiles => "write_tiles",
            Self::BuildingLevels => "building_levels",
        }
    }

    /// The share of a compile's time the stage typically takes, without
    /// downloads; the weights of all but `FetchSources` add up to 1. Set
    /// from a Małopolskie compile (200 MB of PBF) on 4 threads.
    pub fn weight(self) -> f64 {
        match self {
            Self::FetchSources => 0.0,
            Self::ReadSources => 0.05,
            Self::IndexOsm => 0.05,
            Self::ReadOsm => 0.47,
            Self::WriteTiles => 0.38,
            Self::BuildingLevels => 0.05,
        }
    }

    fn index(self) -> u8 {
        self as u8
    }

    fn from_index(index: u8) -> Self {
        Self::ALL[usize::from(index).min(Self::ALL.len() - 1)]
    }
}

/// One progress report.
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct CompileProgress {
    pub stage: CompileStage,
    /// What is happening, in words fit to show: "Reading OSM and preparing
    /// vector tiles", "Downloading water-polygons-split-3857.zip (120 of
    /// 1000 MB)".
    pub message: String,
    /// How much of the current stage is done, 0 to 1, when it can be told.
    pub fraction: Option<f64>,
    /// How much of the whole compile is done, 0 to 1, from the typical
    /// [`weight`](CompileStage::weight)s of the stages it runs (building
    /// levels count only when asked for). Downloads are not part of it: while
    /// `stage` is `FetchSources` it stays 0 and `fraction` is the download's.
    pub overall: f64,
}

impl CompileProgress {
    fn new(
        stage: CompileStage,
        message: String,
        fraction: Option<f64>,
        building_levels: bool,
    ) -> Self {
        let weight = |s: CompileStage| match s {
            CompileStage::BuildingLevels if !building_levels => 0.0,
            s => s.weight(),
        };
        let total: f64 = CompileStage::ALL.iter().map(|s| weight(*s)).sum();
        let before: f64 = CompileStage::ALL
            .iter()
            .take_while(|s| **s != stage)
            .map(|s| weight(*s))
            .sum();
        let overall = (before + weight(stage) * fraction.unwrap_or(0.0)) / total;
        Self {
            stage,
            message,
            fraction,
            overall: overall.clamp(0.0, 1.0),
        }
    }
}

/// What the work shares with the monitor: the cancel flag, the stage and its
/// counters, and the current message.
pub(crate) struct Control {
    /// Whether the compile writes building levels, which decides the
    /// stages' share of `overall`.
    building_levels: bool,
    cancelled: Arc<AtomicBool>,
    stage: AtomicU8,
    done: AtomicU64,
    total: AtomicU64,
    /// The stage's number, counting from 1, and its message.
    message: Mutex<(u64, String)>,
    /// The thread that runs the monitor, if there is one.
    monitor: Option<Thread>,
    /// The last stage number the monitor has reported.
    reported: Mutex<u64>,
    reported_changed: Condvar,
}

impl Control {
    fn new(monitor: Option<Thread>, building_levels: bool) -> Self {
        Self {
            building_levels,
            cancelled: Arc::new(AtomicBool::new(false)),
            stage: AtomicU8::new(0),
            done: AtomicU64::new(0),
            total: AtomicU64::new(0),
            message: Mutex::new((0, String::new())),
            monitor,
            reported: Mutex::new(0),
            reported_changed: Condvar::new(),
        }
    }

    /// A control with no monitor, for the functions that keep their old
    /// signatures: it is never cancelled and reports nothing.
    pub(crate) fn detached() -> Self {
        Self::new(None, false)
    }

    pub(crate) fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    /// The flag itself, for threads that must own what they read.
    pub(crate) fn flag(&self) -> Arc<AtomicBool> {
        self.cancelled.clone()
    }

    /// Fails with [`CANCELLED`] once a cancel was asked for.
    pub(crate) fn check(&self) -> Result<(), String> {
        if self.is_cancelled() {
            Err(CANCELLED.into())
        } else {
            Ok(())
        }
    }

    /// The stage running, or `None` before the first one starts.
    fn current_stage(&self) -> Option<CompileStage> {
        let started = lock(&self.message).0 > 0;
        started.then(|| CompileStage::from_index(self.stage.load(Ordering::Relaxed)))
    }

    /// Starts `stage`, with no fraction until [`Self::set_total`]. Waits
    /// until the monitor has reported it and asked whether to cancel, so
    /// every stage is reported and a cancel asked for on its report stops
    /// the compile before the stage does any work.
    pub(crate) fn stage(
        &self,
        stage: CompileStage,
        message: impl Into<String>,
    ) -> Result<(), String> {
        let number = {
            let mut current = lock(&self.message);
            self.total.store(0, Ordering::Relaxed);
            self.done.store(0, Ordering::Relaxed);
            self.stage.store(stage.index(), Ordering::Relaxed);
            *current = (current.0 + 1, message.into());
            current.0
        };
        if let Some(monitor) = &self.monitor {
            monitor.unpark();
            let mut reported = lock(&self.reported);
            while *reported < number {
                reported = self
                    .reported_changed
                    .wait(reported)
                    .unwrap_or_else(|e| e.into_inner());
            }
        }
        self.check()
    }

    /// Replaces the stage's message.
    pub(crate) fn message(&self, message: impl Into<String>) {
        lock(&self.message).1 = message.into();
    }

    pub(crate) fn set_total(&self, total: u64) {
        self.total.store(total, Ordering::Relaxed);
    }

    pub(crate) fn set_done(&self, done: u64) {
        self.done.store(done, Ordering::Relaxed);
    }

    pub(crate) fn add_done(&self, done: u64) {
        self.done.fetch_add(done, Ordering::Relaxed);
    }

    fn snapshot(&self) -> (u64, CompileProgress) {
        let (number, message) = lock(&self.message).clone();
        let total = self.total.load(Ordering::Relaxed);
        let fraction = (total > 0)
            .then(|| (self.done.load(Ordering::Relaxed) as f64 / total as f64).clamp(0.0, 1.0));
        let stage = CompileStage::from_index(self.stage.load(Ordering::Relaxed));
        (
            number,
            CompileProgress::new(stage, message, fraction, self.building_levels),
        )
    }

    fn poll(&self, should_cancel: &impl Fn() -> bool) {
        if !self.is_cancelled() && should_cancel() {
            self.cancelled.store(true, Ordering::Relaxed);
        }
    }

    fn acknowledge(&self, number: u64) {
        let mut reported = lock(&self.reported);
        if *reported < number {
            *reported = number;
            self.reported_changed.notify_all();
        }
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

/// Runs `work` on a rayon pool of `threads` threads while this thread reports
/// progress and polls `should_cancel`. Returns once `work` has returned, with
/// [`CompileError::Cancelled`] for any failure after a cancel. A panic in
/// `work` becomes an error.
pub(crate) fn run<T: Send>(
    threads: usize,
    building_levels: bool,
    mut progress: impl FnMut(&CompileProgress),
    should_cancel: impl Fn() -> bool,
    work: impl FnOnce(&Control) -> Result<T, String> + Send,
) -> Result<T, CompileError> {
    if threads == 0 {
        return Err(CompileError::InvalidRequest(
            "threads must be positive".into(),
        ));
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        // Linux keeps 15 bytes of a thread's name: this one keeps the index,
        // which the stage profiler needs to tell the threads apart.
        .thread_name(|index| format!("compile-pool-{index}"))
        .build()
        .map_err(|e| CompileError::Failed {
            stage: None,
            message: format!("Failed to start {threads} threads: {e}"),
        })?;
    let control = Control::new(Some(std::thread::current()), building_levels);
    let finished = AtomicBool::new(false);
    let mut result = None;
    pool.in_place_scope(|scope| {
        let (control, finished, result) = (&control, &finished, &mut result);
        let monitor = std::thread::current();
        scope.spawn(move |_| {
            let outcome = std::panic::catch_unwind(AssertUnwindSafe(|| work(control)))
                .unwrap_or_else(|payload| {
                    Err(format!(
                        "The compiler crashed: {}",
                        crate::panic_message(&*payload)
                    ))
                });
            *result = Some(outcome);
            finished.store(true, Ordering::Release);
            monitor.unpark();
        });
        let mut last: Option<(Instant, u64, CompileProgress)> = None;
        loop {
            let finished = finished.load(Ordering::Acquire);
            control.poll(&should_cancel);
            let (number, now) = control.snapshot();
            let due = match &last {
                None => number > 0,
                Some((at, reported, previous)) => {
                    number != *reported || (at.elapsed() >= THROTTLE && *previous != now)
                }
            };
            if due && !finished {
                progress(&now);
                // A cancel asked for from the progress callback counts at once.
                control.poll(&should_cancel);
                last = Some((Instant::now(), number, now));
            }
            control.acknowledge(number);
            if finished {
                break;
            }
            std::thread::park_timeout(POLL);
        }
    });
    // Work that finished despite a late cancel stands: its outputs are in
    // place.
    match result.expect("the work ran to completion") {
        Ok(value) => Ok(value),
        Err(_) if control.is_cancelled() => Err(CompileError::Cancelled),
        Err(message) => Err(CompileError::classify(message, control.current_stage())),
    }
}

/// Removes a path when dropped, unless kept: for outputs that must not
/// survive a failed or cancelled compile.
pub(crate) struct RemoveOnDrop {
    path: Option<std::path::PathBuf>,
}

impl RemoveOnDrop {
    pub(crate) fn file(path: std::path::PathBuf) -> Self {
        Self { path: Some(path) }
    }

    pub(crate) fn keep(mut self) {
        self.path = None;
    }
}

impl Drop for RemoveOnDrop {
    fn drop(&mut self) {
        if let Some(path) = self.path.take() {
            let _ = std::fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failures_are_classified_by_where_they_happen() {
        let classify = |message: &str, stage| CompileError::classify(message.into(), stage);
        assert_eq!(
            classify("unknown layer 'roads'", None),
            CompileError::InvalidRequest("unknown layer 'roads'".into())
        );
        assert_eq!(
            classify("connection refused", Some(CompileStage::FetchSources)),
            CompileError::Download("connection refused".into())
        );
        assert_eq!(
            classify("bad block", Some(CompileStage::ReadOsm)),
            CompileError::Failed {
                stage: Some(CompileStage::ReadOsm),
                message: "bad block".into()
            }
        );
        let full = std::io::Error::from_raw_os_error(if cfg!(windows) { 112 } else { 28 });
        let message = format!("Failed to write tile run: {full}");
        assert_eq!(
            classify(&message, Some(CompileStage::WriteTiles)),
            CompileError::OutOfSpace(message.clone())
        );
        assert_eq!(classify(&message, None).kind(), "out_of_space");
    }

    #[test]
    fn kinds_are_stable() {
        let failed = CompileError::Failed {
            stage: None,
            message: "x".into(),
        };
        let kinds = [
            CompileError::Cancelled,
            CompileError::InvalidRequest("x".into()),
            CompileError::Download("x".into()),
            CompileError::OutOfSpace("x".into()),
            failed,
        ]
        .map(|error| error.kind());
        assert_eq!(
            kinds,
            [
                "cancelled",
                "invalid_request",
                "download",
                "out_of_space",
                "failed"
            ]
        );
        assert_eq!(CompileError::Cancelled.to_string(), CANCELLED);
    }
}
