//! Streaming parallel renderer. Long-lived workers pull queued work, evaluate
//! it, render the resulting features, and write their records into their own
//! sorters. Work is either features the caller already classified (supplements,
//! deferred features) or OSM assembly jobs that workers turn into sources and
//! classify themselves. Every record carries its source's ordinal, so the final
//! merge reproduces serial insertion order no matter which worker rendered what.
use super::{
    Contribution,
    ingest::Job,
    render,
    selection::Selection,
    store::{EncodedRecord, MetadataSummary, Seq, Sorter, Store},
    types::{Feature, Source},
};
use crate::compile_area::Region;
use crate::tile_geometry::TileGeometry;
use crossbeam_channel::{Receiver, Sender, TrySendError, bounded};
use std::{
    fs,
    path::PathBuf,
    sync::{
        Arc, Condvar, Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

// In-memory sort budget per worker, counted in spilled frame bytes. It is
// fixed, not divided among workers, so the number of spilled runs follows the
// data size rather than the thread count. Compact frames hold about four times
// as many records per byte as the earlier format did at 32 MiB, so 8 MiB keeps
// the records per run, the run count and the peak memory where they were.
const WORKER_SORT_BYTES: usize = 8 * 1024 * 1024;
// Features travel in small batches: a wake-up per feature costs the producer
// more than rendering most features does.
const BATCH_FEATURES: usize = 16;
const QUEUE_BATCHES_PER_WORKER: usize = 2;

enum Work {
    /// Features with their source ordinal and position in that source's output.
    Features(Vec<(u64, u32, Feature)>),
    Osm(Job),
}

struct Output {
    runs: Vec<PathBuf>,
    metadata: MetadataSummary,
    sources: u64,
    features: u64,
}
/// A worker failure with the ordinal of the source that caused it.
type Failure = (u64, String);

/// Counts of what the workers evaluated from OSM jobs.
#[derive(Debug)]
pub(super) struct Totals {
    pub sources: u64,
    pub features: u64,
}

/// State shared by the producer and the workers.
#[derive(Default)]
struct Shared {
    failed: AtomicBool,
    /// Raised when the compile is cancelled: workers drop what is queued.
    cancelled: Arc<AtomicBool>,
    /// Messages fully processed; `Pipeline::sync` waits for it to catch up.
    done: Mutex<u64>,
    progress: Condvar,
    contributions: Mutex<Vec<(u64, Contribution)>>,
}

pub(super) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

pub(super) struct Pipeline {
    sender: Option<Sender<Work>>,
    workers: Vec<JoinHandle<Result<Output, Failure>>>,
    shared: Arc<Shared>,
    batch: Vec<(u64, u32, Feature)>,
    sent: u64,
    blocked: crate::profiling::Accumulator,
}

impl Pipeline {
    pub fn new(
        store: &Store,
        threads: usize,
        region: Region,
        min_zoom: u8,
        max_zoom: u8,
        selection: Arc<Selection>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        Self::with_budget(
            store,
            threads,
            WORKER_SORT_BYTES,
            region,
            (min_zoom, max_zoom),
            selection,
            cancelled,
        )
    }

    fn with_budget(
        store: &Store,
        threads: usize,
        worker_budget: usize,
        region: impl Into<Region>,
        (min_zoom, max_zoom): (u8, u8),
        selection: Arc<Selection>,
        cancelled: Arc<AtomicBool>,
    ) -> Result<Self, String> {
        let region = region.into();
        let (sender, receiver) = bounded(threads * QUEUE_BATCHES_PER_WORKER);
        let mut pipeline = Self {
            sender: Some(sender),
            workers: Vec::new(),
            shared: Arc::new(Shared {
                cancelled,
                ..Shared::default()
            }),
            batch: Vec::with_capacity(BATCH_FEATURES),
            sent: 0,
            blocked: crate::profiling::Accumulator::new("omt_render_channel_blocked"),
        };
        // On a spawn error `pipeline` drops, which stops and joins any workers.
        for worker in 0..threads {
            let receiver = receiver.clone();
            let sorter = store.sorter(worker, worker_budget);
            let shared = pipeline.shared.clone();
            let region = region.clone();
            let selection = selection.clone();
            let handle = std::thread::Builder::new()
                .name(format!("omt-render-{worker}"))
                .spawn(move || {
                    let target = Target {
                        region: &region,
                        min_zoom,
                        max_zoom,
                        selection: &selection,
                    };
                    work(&receiver, sorter, &shared, target)
                })
                .map_err(|e| format!("Start render worker: {e}"))?;
            pipeline.workers.push(handle);
        }
        Ok(pipeline)
    }

    /// Queues a source's features, draining `features`; blocks while the queue
    /// is full. `source` orders the source among all others.
    pub fn push_source(&mut self, source: u64, features: &mut Vec<Feature>) -> Result<(), String> {
        if self.stopped() {
            return Err(self.abort());
        }
        for (index, feature) in features.drain(..).enumerate() {
            self.batch.push((source, index as u32, feature));
            if self.batch.len() >= BATCH_FEATURES {
                self.send_batch()?;
            }
        }
        Ok(())
    }

    /// Queues an OSM assembly job; blocks while the queue is full.
    pub fn submit(&mut self, job: Job) -> Result<(), String> {
        if self.stopped() {
            return Err(self.abort());
        }
        self.send(Work::Osm(job))
    }

    fn send_batch(&mut self) -> Result<(), String> {
        if self.batch.is_empty() {
            return Ok(());
        }
        let batch = std::mem::replace(&mut self.batch, Vec::with_capacity(BATCH_FEATURES));
        self.send(Work::Features(batch))
    }

    fn send(&mut self, work: Work) -> Result<(), String> {
        let sender = self
            .sender
            .as_ref()
            .ok_or("Render pipeline already finished")?;
        let sent = match sender.try_send(work) {
            Ok(()) => Ok(()),
            // Only time the main thread when it actually has to wait.
            Err(TrySendError::Full(work)) => {
                self.blocked.measure(|| sender.send(work)).map_err(drop)
            }
            Err(TrySendError::Disconnected(_)) => Err(()),
        };
        match sent {
            Ok(()) => {
                self.sent += 1;
                Ok(())
            }
            Err(()) => Err(self.abort()),
        }
    }

    /// Whether a worker failed or the compile was cancelled.
    fn stopped(&self) -> bool {
        self.shared.failed.load(Ordering::Relaxed) || self.shared.cancelled.load(Ordering::Relaxed)
    }

    /// How many queued items the workers have finished.
    pub fn processed(&self) -> u64 {
        *lock(&self.shared.done)
    }

    /// Waits until the workers have processed everything queued so far.
    pub fn sync(&mut self) -> Result<(), String> {
        self.sync_with(|_, _| {})
    }

    /// [`Self::sync`], calling `progress` with the items processed and the
    /// items queued while it waits.
    pub fn sync_with(&mut self, mut progress: impl FnMut(u64, u64)) -> Result<(), String> {
        self.send_batch()?;
        let shared = self.shared.clone();
        let mut done = lock(&shared.done);
        while *done < self.sent {
            progress(*done, self.sent);
            // A worker that stopped while the queue is open failed or panicked.
            if self.stopped() || self.workers.iter().any(|w| w.is_finished()) {
                drop(done);
                return Err(self.abort());
            }
            done = shared
                .progress
                .wait_timeout(done, Duration::from_millis(20))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
        Ok(())
    }

    /// What OSM sources added to the profile state, in source order. Call after
    /// `sync`.
    pub fn take_contributions(&mut self) -> Vec<(u64, Contribution)> {
        let mut all = std::mem::take(&mut *lock(&self.shared.contributions));
        all.sort_unstable_by_key(|(ordinal, _)| *ordinal);
        all
    }

    /// Waits for every queued item and hands all runs and metadata to `store`.
    pub fn finish(&mut self, store: &mut Store) -> Result<Totals, String> {
        self.send_batch()?;
        let drain = crate::profiling::Phase::start("omt_render_drain");
        let outputs = self.join()?;
        drop(drain);
        let mut metadata = MetadataSummary::default();
        let mut totals = Totals {
            sources: 0,
            features: 0,
        };
        for output in outputs {
            store.add_runs(output.runs);
            metadata.merge(output.metadata);
            totals.sources += output.sources;
            totals.features += output.features;
        }
        metadata.apply(store);
        Ok(totals)
    }

    /// Stops the workers and returns the failure with the lowest ordinal.
    fn abort(&mut self) -> String {
        if self.shared.cancelled.load(Ordering::Relaxed) {
            // Workers drop what is queued and remove their runs.
            let _ = self.join().map(remove_runs);
            return crate::control::CANCELLED.into();
        }
        self.join()
            .err()
            .unwrap_or_else(|| "Render workers stopped unexpectedly".into())
    }

    /// Closes the queue and joins all workers. If any failed, every run file
    /// written so far is deleted and the earliest failure is returned.
    fn join(&mut self) -> Result<Vec<Output>, String> {
        self.sender = None;
        let mut outputs = Vec::new();
        let mut failure: Option<Failure> = None;
        for worker in self.workers.drain(..) {
            match worker.join() {
                Ok(Ok(output)) => outputs.push(output),
                Ok(Err(f)) => {
                    failure = Some(match failure.take() {
                        Some(old) => old.min(f),
                        None => f,
                    })
                }
                Err(payload) => {
                    failure.get_or_insert_with(|| {
                        let message = crate::panic_message(&*payload);
                        (u64::MAX, format!("Render worker panicked: {message}"))
                    });
                }
            }
        }
        match failure {
            None => Ok(outputs),
            Some((_, error)) => {
                remove_runs(outputs);
                Err(error)
            }
        }
    }
}

impl Drop for Pipeline {
    fn drop(&mut self) {
        if let Ok(outputs) = self.join() {
            remove_runs(outputs);
        }
    }
}

fn remove_runs(outputs: Vec<Output>) {
    for run in outputs.iter().flat_map(|o| &o.runs) {
        let _ = fs::remove_file(run);
    }
}

/// What a worker renders features into: the area, the zooms, and the layers
/// and names selected.
struct Target<'a> {
    region: &'a Region,
    min_zoom: u8,
    max_zoom: u8,
    selection: &'a Selection,
}

struct Worker<'a> {
    sorter: Sorter,
    metadata: MetadataSummary,
    shared: &'a Shared,
    target: Target<'a>,
    /// Output of the source being evaluated.
    features: Vec<Feature>,
    sources: u64,
    feature_count: u64,
    /// Set when work stopped because another worker failed.
    stopped: bool,
}

impl Worker<'_> {
    fn handle(&mut self, work: Work) -> Result<(), Failure> {
        match work {
            Work::Features(batch) => {
                for (source, index, feature) in batch {
                    if self.shared.failed.load(Ordering::Relaxed)
                        || self.shared.cancelled.load(Ordering::Relaxed)
                    {
                        return Ok(());
                    }
                    self.render(
                        Seq {
                            source,
                            feature: index,
                            record: 0,
                        },
                        feature,
                    )
                    .map_err(|error| (source, error))?;
                }
                Ok(())
            }
            Work::Osm(job) => {
                let profile = job.profile.clone();
                let base = job.ordinal;
                let result = job.run(&mut |ordinal: u64, source: Source| {
                    if self.shared.failed.load(Ordering::Relaxed)
                        || self.shared.cancelled.load(Ordering::Relaxed)
                    {
                        self.stopped = true;
                        return Err(String::new());
                    }
                    self.sources += 1;
                    let mut contribution = Contribution::default();
                    profile.evaluate(&source, &mut self.features, &mut contribution);
                    drop(source);
                    self.feature_count += self.features.len() as u64;
                    let mut features = std::mem::take(&mut self.features);
                    for (index, feature) in features.drain(..).enumerate() {
                        let seq = Seq {
                            source: ordinal,
                            feature: index as u32,
                            record: 0,
                        };
                        self.render(seq, feature)?;
                    }
                    self.features = features;
                    if !contribution.is_empty() {
                        lock(&self.shared.contributions).push((ordinal, contribution));
                    }
                    Ok(())
                });
                match result {
                    Err(_) if self.stopped => Ok(()),
                    Err(error) => Err((base, error)),
                    Ok(()) => Ok(()),
                }
            }
        }
    }

    fn render(&mut self, seq: Seq, mut feature: Feature) -> Result<(), String> {
        // The profile has already evaluated the feature's source, so leaving
        // a feature out here changes no other layer; `seq` still counts it,
        // which keeps the order of the features kept.
        if !self.target.selection.has_layer(&feature.layer) {
            return Ok(());
        }
        self.target.selection.strip_names(&mut feature);
        let mut index = 0u32;
        render::emit_with_record_sink(
            feature,
            self.target.region,
            self.target.min_zoom,
            self.target.max_zoom,
            &mut |record| {
                let seq = Seq {
                    record: index,
                    ..seq
                };
                index = index
                    .checked_add(1)
                    .ok_or("Feature emitted too many records")?;
                // A line that collapsed on the tile grid only takes part in
                // merging; it is never written, so the layer summary skips it.
                if !matches!(&record.geometry, TileGeometry::Line { paths, .. } if paths.is_empty())
                {
                    self.metadata.observe(&record, seq);
                }
                self.sorter.add(EncodedRecord::encode(&record, seq)?)
            },
        )
    }
}

fn work(
    receiver: &Receiver<Work>,
    sorter: Sorter,
    shared: &Shared,
    target: Target,
) -> Result<Output, Failure> {
    let mut worker = Worker {
        sorter,
        metadata: MetadataSummary::default(),
        shared,
        target,
        features: Vec::with_capacity(8),
        sources: 0,
        feature_count: 0,
        stopped: false,
    };
    let mut busy = crate::profiling::Accumulator::new("omt_render_worker_busy");
    while let Ok(work) = receiver.recv() {
        if shared.failed.load(Ordering::Relaxed) {
            break;
        }
        if shared.cancelled.load(Ordering::Relaxed) {
            // Dropping the sorter removes the runs it wrote; nothing more is
            // spilled.
            return Err((u64::MAX, crate::control::CANCELLED.into()));
        }
        if let Err(failure) = busy.measure(|| worker.handle(work)) {
            shared.failed.store(true, Ordering::Relaxed);
            shared.progress.notify_all();
            return Err(failure);
        }
        *lock(&shared.done) += 1;
        shared.progress.notify_all();
    }
    if shared.cancelled.load(Ordering::Relaxed) {
        return Err((u64::MAX, crate::control::CANCELLED.into()));
    }
    let Worker {
        sorter,
        metadata,
        sources,
        feature_count,
        ..
    } = worker;
    sorter
        .finish()
        .map(|runs| Output {
            runs,
            metadata,
            sources,
            features: feature_count,
        })
        .map_err(|error| {
            shared.failed.store(true, Ordering::Relaxed);
            (u64::MAX, error)
        })
}

#[cfg(test)]
mod tests {
    use super::super::store::{ArchiveHeader, MERGE_FAN_IN};
    use super::super::types::{Source, SourceKind};
    use super::*;
    use crate::CompileBounds;
    use geo_types::{Geometry, LineString, Point, Polygon};

    fn feature(ordinal: usize, layer: &str, geometry: &Geometry<f64>) -> Feature {
        let source = Source {
            id: 12,
            osm_id: 1,
            kind: SourceKind::Way,
            source_layer: "osm".into(),
            tags: Default::default(),
            geometry: geometry.clone(),
            relations: vec![],
        };
        let mut feature = Feature::new(&source, layer, geometry.clone(), 2, 4);
        feature
            .attrs
            .insert("ordinal".into(), serde_json::json!(ordinal));
        feature
            .attrs
            .insert("class".into(), serde_json::json!("cemetery"));
        feature.min_size = 0.;
        feature.min_size_at_max_zoom = 0.;
        feature
    }
    fn shapes() -> [(&'static str, Geometry<f64>); 3] {
        [
            ("poi", Geometry::Point(Point::new(1., 1.))),
            (
                "transportation",
                Geometry::LineString(LineString::from(vec![(-10., -8.), (0., 7.), (10., 8.)])),
            ),
            (
                "landuse",
                Geometry::Polygon(Polygon::new(
                    LineString::from(vec![(-8., -8.), (8., -8.), (8., 8.), (-8., 8.), (-8., -8.)]),
                    vec![],
                )),
            ),
        ]
    }

    #[test]
    fn serial_and_parallel_pipeline_publish_identical_archives() {
        let temp = tempfile::tempdir().unwrap();
        let bounds = CompileBounds::new(20., -20., 20., -20.);
        let shapes = shapes();
        // Repeated IDs/sort keys deliberately tie across workers; distinct
        // ordinal attributes reveal changed order.
        let features = (0..180)
            .map(|ordinal| {
                let (layer, geometry) = &shapes[ordinal % shapes.len()];
                feature(ordinal, layer, geometry)
            })
            .collect::<Vec<_>>();
        let mut archives = Vec::new();
        // The tiny budget makes every worker spill many runs.
        for (threads, budget) in [(1, WORKER_SORT_BYTES), (3, 4096), (7, 1)] {
            let workspace = temp.path().join(format!("work-{threads}"));
            fs::create_dir(&workspace).unwrap();
            let mut store = Store::new(&workspace).unwrap();
            let mut pipeline = Pipeline::with_budget(
                &store,
                threads,
                budget,
                bounds,
                (2, 4),
                Default::default(),
                Default::default(),
            )
            .unwrap();
            for (ordinal, feature) in features.iter().cloned().enumerate() {
                pipeline
                    .push_source(ordinal as u64, &mut vec![feature])
                    .unwrap();
            }
            pipeline.finish(&mut store).unwrap();
            drop(pipeline);
            let runs = fs::read_dir(workspace.join("omt-tile-sort"))
                .unwrap()
                .count();
            // One-byte budgets force more runs than the merge fan-in, so
            // `finish` merges them in parallel groups before the final merge.
            assert!(budget != 1 || runs > MERGE_FAN_IN, "{runs} runs");
            let output = temp.path().join(format!("out-{threads}.pmtiles"));
            let archive = fs::File::create(&output).unwrap();
            assert!(
                store
                    .finish(
                        &archive,
                        &ArchiveHeader::for_test(bounds, 2, 4),
                        threads,
                        &crate::control::Control::detached()
                    )
                    .unwrap()
                    .tiles()
                    > 0
            );
            archives.push(fs::read(output).unwrap());
            assert!(!workspace.join("omt-tile-sort").exists());
        }
        assert_eq!(archives[0], archives[1]);
        assert_eq!(archives[0], archives[2]);
    }

    #[test]
    fn worker_failure_surfaces_earliest_error_and_removes_runs() {
        let temp = tempfile::tempdir().unwrap();
        let bounds = CompileBounds::new(20., -20., 20., -20.);
        let shapes = shapes();
        let mut store = Store::new(temp.path()).unwrap();
        let mut pipeline = Pipeline::with_budget(
            &store,
            3,
            1,
            bounds,
            (2, 4),
            Default::default(),
            Default::default(),
        )
        .unwrap();
        let mut error = None;
        // A layer name beyond the frame limit fails when encoded. Keep pushing
        // so a stopped pipeline must report the error rather than deadlock.
        for ordinal in 0..10_000 {
            let (layer, geometry) = &shapes[ordinal % shapes.len()];
            let layer = if ordinal == 40 {
                "x".repeat(70_000)
            } else {
                layer.to_string()
            };
            let feature = feature(ordinal, &layer, geometry);
            if let Err(e) = pipeline.push_source(ordinal as u64, &mut vec![feature]) {
                error = Some(e);
                break;
            }
        }
        let error = error.unwrap_or_else(|| pipeline.finish(&mut store).unwrap_err());
        assert!(error.contains("frame limit"), "{error}");
        drop(pipeline);
        assert_eq!(
            fs::read_dir(temp.path().join("omt-tile-sort"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn sync_waits_for_queued_work_and_reports_a_worker_failure() {
        let temp = tempfile::tempdir().unwrap();
        let bounds = CompileBounds::new(20., -20., 20., -20.);
        let (layer, geometry) = &shapes()[0];
        let store = Store::new(temp.path()).unwrap();
        let mut pipeline = Pipeline::with_budget(
            &store,
            3,
            1,
            bounds,
            (2, 4),
            Default::default(),
            Default::default(),
        )
        .unwrap();
        for ordinal in 0..50 {
            let feature = feature(ordinal, layer, geometry);
            pipeline
                .push_source(ordinal as u64, &mut vec![feature])
                .unwrap();
        }
        pipeline.sync().unwrap();
        assert!(pipeline.take_contributions().is_empty());
        let bad = feature(50, &"x".repeat(70_000), geometry);
        pipeline.push_source(50, &mut vec![bad]).unwrap();
        let error = pipeline.sync().unwrap_err();
        assert!(error.contains("frame limit"), "{error}");
        drop(pipeline);
        assert_eq!(
            fs::read_dir(temp.path().join("omt-tile-sort"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn dropping_an_unfinished_pipeline_removes_runs() {
        let temp = tempfile::tempdir().unwrap();
        let bounds = CompileBounds::new(20., -20., 20., -20.);
        let store = Store::new(temp.path()).unwrap();
        let mut pipeline = Pipeline::with_budget(
            &store,
            2,
            1,
            bounds,
            (2, 4),
            Default::default(),
            Default::default(),
        )
        .unwrap();
        let (layer, geometry) = &shapes()[0];
        for ordinal in 0..20 {
            let feature = feature(ordinal, layer, geometry);
            pipeline
                .push_source(ordinal as u64, &mut vec![feature])
                .unwrap();
        }
        drop(pipeline);
        assert_eq!(
            fs::read_dir(temp.path().join("omt-tile-sort"))
                .unwrap()
                .count(),
            0
        );
    }
}
