//! The built-in stage profiler behind `compile --profile <FILE.json>` and
//! `building-levels --profile <FILE.json>`. Call [`start`] before a compile
//! and [`finish`] after it for the [`Report`]. One profile runs per process:
//! two compiles at once would share it. The report's shape may change between
//! minor versions.
//!
//! It measures each top-level stage of a compile with its wall time, the
//! process's CPU time (so CPU / wall is how many threads were busy on
//! average), the CPU time of the thread that drives the stage (a driver busy
//! for most of the wall time is a serial bottleneck), and on Linux the CPU
//! time of each group of threads (render workers, encoders, PBF decoders,
//! rayon pool) and the peak resident memory. Stages are flat and run one after
//! another, so their wall times add up to the compile's.
//!
//! Nothing is measured unless a profile was started: every hook first checks
//! one relaxed atomic. Stage boundaries take a few system calls and, on
//! Linux, read `/proc/self/task`; a sampler thread reads it every 50 ms while
//! a profile runs, so threads that exit between boundaries still count.
use serde::Serialize;
use std::{
    sync::{
        Mutex, MutexGuard, PoisonError,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

static ENABLED: AtomicBool = AtomicBool::new(false);
static STATE: Mutex<Option<State>> = Mutex::new(None);

/// Work timed inside stages, summed over every thread that did it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Counter {
    /// Render workers evaluating, rendering and slicing features.
    RenderBusy,
    /// The driver waiting because the render queue was full.
    RenderQueueFull,
    /// Render workers sorting and spilling a full sort buffer to a run.
    SpillSortWrite,
    /// Encoder threads decoding, merging and encoding one tile.
    EncodeTile,
    /// The driver writing encoded tiles into the archive.
    ArchiveWrite,
}

impl Counter {
    const ALL: [Self; 5] = [
        Self::RenderBusy,
        Self::RenderQueueFull,
        Self::SpillSortWrite,
        Self::EncodeTile,
        Self::ArchiveWrite,
    ];
    const COUNT: usize = Self::ALL.len();

    fn name(self) -> &'static str {
        match self {
            Self::RenderBusy => "render_busy",
            Self::RenderQueueFull => "render_queue_full_wait",
            Self::SpillSortWrite => "spill_sort_write",
            Self::EncodeTile => "encode_tile",
            Self::ArchiveWrite => "archive_write",
        }
    }
}

/// Per counter: operations, nanoseconds and bytes.
static COUNTERS: [[AtomicU64; 3]; Counter::COUNT] =
    [const { [const { AtomicU64::new(0) }; 3] }; Counter::COUNT];

/// Whether a profile is running.
#[inline]
pub(crate) fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// Runs `operation`, adding its time to `counter` while a profile runs.
#[inline]
pub(crate) fn time<T>(counter: Counter, operation: impl FnOnce() -> T) -> T {
    if !enabled() {
        return operation();
    }
    let started = Instant::now();
    let result = operation();
    add(counter, started.elapsed(), 0);
    result
}

/// Adds one operation of `elapsed` and `bytes` to `counter`.
#[inline]
pub(crate) fn add(counter: Counter, elapsed: Duration, bytes: u64) {
    if !enabled() {
        return;
    }
    let slots = &COUNTERS[counter as usize];
    slots[0].fetch_add(1, Ordering::Relaxed);
    slots[1].fetch_add(elapsed.as_nanos() as u64, Ordering::Relaxed);
    slots[2].fetch_add(bytes, Ordering::Relaxed);
}

fn counters() -> [[u64; 3]; Counter::COUNT] {
    std::array::from_fn(|i| std::array::from_fn(|j| COUNTERS[i][j].load(Ordering::Relaxed)))
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

struct State {
    started: Instant,
    threads: usize,
    stages: Vec<StageReport>,
    #[cfg(target_os = "linux")]
    sampler: Option<linux::Sampler>,
}

/// Starts a profile of a compile given `threads` worker threads. A profile
/// already running is replaced.
pub fn start(threads: usize) {
    for slots in &COUNTERS {
        for slot in slots {
            slot.store(0, Ordering::Relaxed);
        }
    }
    let state = State {
        started: Instant::now(),
        threads,
        stages: Vec::new(),
        #[cfg(target_os = "linux")]
        sampler: linux::Sampler::start(),
    };
    *lock(&STATE) = Some(state);
    ENABLED.store(true, Ordering::Relaxed);
}

/// Ends the profile and returns its report; None if none was running.
pub fn finish() -> Option<Report> {
    ENABLED.store(false, Ordering::Relaxed);
    let state = lock(&STATE).take()?;
    #[cfg(target_os = "linux")]
    if let Some(sampler) = state.sampler {
        sampler.stop();
    }
    let total_seconds = state.started.elapsed().as_secs_f64();
    let staged: f64 = state.stages.iter().map(|s| s.wall_seconds).sum();
    let process_cpu = process_cpu().map(|cpu| cpu.as_secs_f64());
    Some(Report {
        machine: Machine::detect(),
        threads: state.threads,
        total_seconds,
        unstaged_seconds: (total_seconds - staged).max(0.0),
        process_cpu_seconds: process_cpu,
        peak_rss_bytes: peak_rss(),
        stages: state.stages,
    })
}

/// A stage being measured; it is recorded when dropped.
#[must_use = "a stage is measured until it is dropped"]
pub(crate) struct Stage(Option<Box<Started>>);

struct Started {
    name: &'static str,
    wall: Instant,
    process_cpu: Option<Duration>,
    driver_cpu: Option<Duration>,
    counters: [[u64; 3]; Counter::COUNT],
    #[cfg(target_os = "linux")]
    threads: linux::Snapshot,
}

impl Stage {
    /// Starts measuring the stage `name` on the calling thread, its driver.
    #[inline]
    pub(crate) fn start(name: &'static str) -> Self {
        if !enabled() {
            return Self(None);
        }
        #[cfg(target_os = "linux")]
        let threads = linux::begin_stage();
        Self(Some(Box::new(Started {
            name,
            process_cpu: process_cpu(),
            driver_cpu: thread_cpu(),
            counters: counters(),
            #[cfg(target_os = "linux")]
            threads,
            wall: Instant::now(),
        })))
    }
}

impl Drop for Stage {
    fn drop(&mut self) {
        let Some(started) = self.0.take() else {
            return;
        };
        let wall = started.wall.elapsed();
        let since = |end: Option<Duration>, start: Option<Duration>| {
            Some(end?.saturating_sub(start?).as_secs_f64())
        };
        let cpu = since(process_cpu(), started.process_cpu);
        let driver = since(thread_cpu(), started.driver_cpu);
        let end_counters = counters();
        let counters = Counter::ALL
            .iter()
            .filter_map(|&counter| {
                let (end, start) = (
                    end_counters[counter as usize],
                    started.counters[counter as usize],
                );
                let operations = end[0] - start[0];
                (operations > 0).then(|| CounterReport {
                    name: counter.name(),
                    operations,
                    thread_seconds: (end[1] - start[1]) as f64 / 1e9,
                    bytes: end[2] - start[2],
                })
            })
            .collect();
        #[cfg(target_os = "linux")]
        let (thread_groups, peak_rss) = linux::end_stage(&started.threads);
        #[cfg(not(target_os = "linux"))]
        let (thread_groups, peak_rss) = (Vec::new(), None);
        let mut guard = lock(&STATE);
        let Some(state) = guard.as_mut() else {
            return;
        };
        let wall_seconds = wall.as_secs_f64();
        let threads = state.threads.max(1) as f64;
        let mut thread_groups = thread_groups;
        for group in &mut thread_groups {
            group.busy_threads = group.cpu_seconds / wall_seconds.max(1e-9);
        }
        state.stages.push(StageReport {
            name: started.name,
            start_seconds: started
                .wall
                .saturating_duration_since(state.started)
                .as_secs_f64(),
            wall_seconds,
            cpu_seconds: cpu,
            busy_threads: cpu.map(|cpu| cpu / wall_seconds.max(1e-9)),
            efficiency: cpu.map(|cpu| cpu / (wall_seconds.max(1e-9) * threads)),
            driver_cpu_seconds: driver,
            driver_busy: driver.map(|cpu| cpu / wall_seconds.max(1e-9)),
            peak_rss_bytes: peak_rss,
            thread_groups,
            counters,
        });
    }
}

/// The table leaves out the ratios of stages shorter than this.
const SHORT_STAGE_SECONDS: f64 = 0.05;

/// Everything a profile measured.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct Report {
    pub machine: Machine,
    /// The worker threads the compile was given.
    pub threads: usize,
    pub total_seconds: f64,
    /// Wall time outside every stage (setup, small steps between stages).
    pub unstaged_seconds: f64,
    /// CPU time of the whole process, from its start.
    pub process_cpu_seconds: Option<f64>,
    /// Peak resident memory of the process, from its start.
    pub peak_rss_bytes: Option<u64>,
    pub stages: Vec<StageReport>,
}

#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct StageReport {
    pub name: &'static str,
    /// When the stage started, from the profile's start.
    pub start_seconds: f64,
    pub wall_seconds: f64,
    /// The process's CPU time during the stage.
    pub cpu_seconds: Option<f64>,
    /// CPU / wall: how many threads were busy on average.
    pub busy_threads: Option<f64>,
    /// CPU / (wall × threads): the share of the given threads kept busy.
    pub efficiency: Option<f64>,
    /// CPU time of the thread that ran the stage.
    pub driver_cpu_seconds: Option<f64>,
    /// The driver's CPU / wall. Near 1 the driver is the bottleneck.
    pub driver_busy: Option<f64>,
    /// Peak resident memory sampled during the stage (Linux only).
    pub peak_rss_bytes: Option<u64>,
    /// CPU time per group of threads (Linux only), most first.
    pub thread_groups: Vec<ThreadGroup>,
    /// Work timed inside the stage, summed over threads.
    pub counters: Vec<CounterReport>,
}

#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct ThreadGroup {
    pub name: String,
    /// Threads of the group that ran during the stage.
    pub threads: usize,
    pub cpu_seconds: f64,
    /// CPU / stage wall: threads of the group busy on average.
    pub busy_threads: f64,
}

#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct CounterReport {
    pub name: &'static str,
    pub operations: u64,
    /// Time summed over every thread that did the work.
    pub thread_seconds: f64,
    pub bytes: u64,
}

#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct Machine {
    pub os: &'static str,
    pub arch: &'static str,
    /// Hardware threads the process may use.
    pub available_threads: usize,
    /// Physical cores, where the OS says (Linux).
    pub physical_cores: Option<usize>,
    pub cpu: Option<String>,
    pub memory_bytes: Option<u64>,
}

impl Machine {
    fn detect() -> Self {
        #[cfg(target_os = "linux")]
        let (physical_cores, cpu, memory_bytes) = linux::machine();
        #[cfg(not(target_os = "linux"))]
        let (physical_cores, cpu, memory_bytes) = (None, None, None);
        Self {
            os: std::env::consts::OS,
            arch: std::env::consts::ARCH,
            available_threads: std::thread::available_parallelism().map_or(1, |n| n.get()),
            physical_cores,
            cpu,
            memory_bytes,
        }
    }
}

impl Report {
    /// The report as a table for a terminal.
    pub fn table(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let m = &self.machine;
        let _ = writeln!(
            out,
            "Stage profile: {} threads given; {} hardware threads{}{}",
            self.threads,
            m.available_threads,
            m.physical_cores
                .map(|c| format!(", {c} physical cores"))
                .unwrap_or_default(),
            m.cpu
                .as_ref()
                .map(|c| format!(" ({c})"))
                .unwrap_or_default(),
        );
        let _ = writeln!(
            out,
            "{:<24} {:>8} {:>8} {:>6} {:>6} {:>7} {:>9}",
            "stage", "wall s", "cpu s", "busy", "eff", "driver", "peak RSS"
        );
        let opt = |v: Option<f64>, f: &dyn Fn(f64) -> String| v.map_or("-".into(), f);
        for stage in &self.stages {
            // The CPU clocks tick too coarsely for ratios of a stage this short.
            let ratio = |v: Option<f64>| v.filter(|_| stage.wall_seconds >= SHORT_STAGE_SECONDS);
            let _ = writeln!(
                out,
                "{:<24} {:>8.2} {:>8} {:>6} {:>6} {:>7} {:>9}",
                stage.name,
                stage.wall_seconds,
                opt(stage.cpu_seconds, &|v| format!("{v:.2}")),
                opt(ratio(stage.busy_threads), &|v| format!("{v:.2}")),
                opt(ratio(stage.efficiency), &|v| format!("{:.0}%", v * 100.0)),
                opt(ratio(stage.driver_busy), &|v| format!("{:.0}%", v * 100.0)),
                stage
                    .peak_rss_bytes
                    .map_or("-".into(), |b| format!("{} MiB", b >> 20)),
            );
            if !stage.thread_groups.is_empty() && stage.wall_seconds >= SHORT_STAGE_SECONDS {
                let groups = stage
                    .thread_groups
                    .iter()
                    .filter(|g| g.cpu_seconds >= 0.01)
                    .map(|g| format!("{} ×{} {:.2}", g.name, g.threads, g.busy_threads))
                    .collect::<Vec<_>>();
                if !groups.is_empty() {
                    let _ = writeln!(out, "    threads busy: {}", groups.join(", "));
                }
            }
            if !stage.counters.is_empty() {
                let counters = stage
                    .counters
                    .iter()
                    .map(|c| format!("{} {:.2} s", c.name, c.thread_seconds))
                    .collect::<Vec<_>>();
                let _ = writeln!(out, "    timed work: {}", counters.join(", "));
            }
        }
        let _ = writeln!(
            out,
            "{:<24} {:>8.2}\n{:<24} {:>8.2} {:>8}",
            "(between stages)",
            self.unstaged_seconds,
            "total",
            self.total_seconds,
            opt(self.process_cpu_seconds, &|v| format!("{v:.2}")),
        );
        let _ = write!(
            out,
            "busy = CPU / wall; eff = busy / {} threads; driver = the stage's own \
             thread's CPU / wall",
            self.threads
        );
        out
    }
}

/// CPU time of the whole process, user and system.
#[cfg(unix)]
fn process_cpu() -> Option<Duration> {
    // SAFETY: getrusage writes only into the zeroed struct it is given.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        (libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0).then_some(usage)?
    };
    let time = |t: libc::timeval| Duration::new(t.tv_sec as u64, t.tv_usec as u32 * 1000);
    Some(time(usage.ru_utime) + time(usage.ru_stime))
}

#[cfg(not(unix))]
fn process_cpu() -> Option<Duration> {
    None
}

/// CPU time of the calling thread.
#[cfg(unix)]
fn thread_cpu() -> Option<Duration> {
    // SAFETY: clock_gettime writes only into the zeroed struct it is given.
    let time = unsafe {
        let mut time = std::mem::zeroed::<libc::timespec>();
        (libc::clock_gettime(libc::CLOCK_THREAD_CPUTIME_ID, &mut time) == 0).then_some(time)?
    };
    Some(Duration::new(time.tv_sec as u64, time.tv_nsec as u32))
}

#[cfg(not(unix))]
fn thread_cpu() -> Option<Duration> {
    None
}

/// Peak resident memory of the process so far.
#[cfg(unix)]
fn peak_rss() -> Option<u64> {
    // SAFETY: as in process_cpu.
    let usage = unsafe {
        let mut usage = std::mem::zeroed::<libc::rusage>();
        (libc::getrusage(libc::RUSAGE_SELF, &mut usage) == 0).then_some(usage)?
    };
    let max = usage.ru_maxrss as u64;
    // macOS counts bytes, the others KiB.
    Some(if cfg!(target_vendor = "apple") {
        max
    } else {
        max * 1024
    })
}

#[cfg(not(unix))]
fn peak_rss() -> Option<u64> {
    None
}

#[cfg(target_os = "linux")]
mod linux {
    use super::{ThreadGroup, lock};
    use std::{
        collections::HashMap,
        fs,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, AtomicU64, Ordering},
        },
        thread::JoinHandle,
        time::Duration,
    };

    const SAMPLE_EVERY: Duration = Duration::from_millis(50);

    /// The last CPU time seen of every thread, by thread ID, kept after the
    /// thread exits.
    static SEEN: Mutex<Option<HashMap<i32, Seen>>> = Mutex::new(None);
    /// The peak resident memory sampled since the current stage began.
    static STAGE_PEAK_RSS: AtomicU64 = AtomicU64::new(0);

    #[derive(Clone)]
    struct Seen {
        name: String,
        cpu_ns: u64,
    }

    /// Thread CPU times at a stage's start, and its driver.
    pub(super) struct Snapshot {
        driver: i32,
        driver_name: String,
        threads: HashMap<i32, Seen>,
    }

    pub(super) struct Sampler {
        stop: Arc<AtomicBool>,
        handle: JoinHandle<()>,
    }

    impl Sampler {
        pub(super) fn start() -> Option<Self> {
            *lock(&SEEN) = Some(HashMap::new());
            let stop = Arc::new(AtomicBool::new(false));
            let flag = stop.clone();
            let handle = std::thread::Builder::new()
                .name("stage-profiler".into())
                .spawn(move || {
                    while !flag.load(Ordering::Relaxed) {
                        sample();
                        std::thread::park_timeout(SAMPLE_EVERY);
                    }
                })
                .ok()?;
            Some(Self { stop, handle })
        }

        pub(super) fn stop(self) {
            self.stop.store(true, Ordering::Relaxed);
            self.handle.thread().unpark();
            let _ = self.handle.join();
        }
    }

    /// Reads every thread's CPU time into `SEEN` and the resident memory into
    /// the stage's peak; returns a copy of `SEEN`.
    fn sample() -> HashMap<i32, Seen> {
        if let Some(rss) = rss() {
            STAGE_PEAK_RSS.fetch_max(rss, Ordering::Relaxed);
        }
        let mut current = Vec::new();
        if let Ok(tasks) = fs::read_dir("/proc/self/task") {
            for task in tasks.flatten() {
                let Some(tid) = task.file_name().to_str().and_then(|s| s.parse().ok()) else {
                    continue;
                };
                let path = task.path();
                let name = fs::read_to_string(path.join("comm")).unwrap_or_default();
                // The first field of schedstat is the time on CPU in ns.
                let cpu_ns = fs::read_to_string(path.join("schedstat"))
                    .ok()
                    .and_then(|s| s.split_whitespace().next()?.parse().ok());
                if let Some(cpu_ns) = cpu_ns {
                    current.push((
                        tid,
                        Seen {
                            name: name.trim_end().to_string(),
                            cpu_ns,
                        },
                    ));
                }
            }
        }
        let mut seen = lock(&SEEN);
        let seen = seen.get_or_insert_with(HashMap::new);
        for (tid, thread) in current {
            seen.insert(tid, thread);
        }
        seen.clone()
    }

    fn rss() -> Option<u64> {
        let statm = fs::read_to_string("/proc/self/statm").ok()?;
        let pages: u64 = statm.split_whitespace().nth(1)?.parse().ok()?;
        // SAFETY: sysconf has no preconditions.
        let page = unsafe { libc::sysconf(libc::_SC_PAGESIZE) };
        Some(pages * page.max(4096) as u64)
    }

    fn current_tid() -> i32 {
        // SAFETY: gettid has no preconditions.
        unsafe { libc::gettid() }
    }

    pub(super) fn begin_stage() -> Snapshot {
        let threads = sample();
        STAGE_PEAK_RSS.store(rss().unwrap_or(0), Ordering::Relaxed);
        let driver = current_tid();
        let driver_name = threads
            .get(&driver)
            .map(|t| t.name.clone())
            .unwrap_or_default();
        Snapshot {
            driver,
            driver_name,
            threads,
        }
    }

    pub(super) fn end_stage(start: &Snapshot) -> (Vec<ThreadGroup>, Option<u64>) {
        let end = sample();
        let peak = STAGE_PEAK_RSS.load(Ordering::Relaxed);
        // SAFETY: getpid has no preconditions.
        let pid = unsafe { libc::getpid() };
        let mut groups: HashMap<String, (usize, u64)> = HashMap::new();
        for (tid, thread) in &end {
            let before = start
                .threads
                .get(tid)
                .filter(|before| before.name == thread.name && before.cpu_ns <= thread.cpu_ns)
                .map_or(0, |before| before.cpu_ns);
            let cpu = thread.cpu_ns - before;
            if cpu == 0 {
                continue;
            }
            let group = if *tid == start.driver {
                "driver".to_string()
            } else if *tid == pid {
                "main".to_string()
            } else if thread.name == start.driver_name {
                // Unnamed threads take the name of the thread that started
                // them: the PBF reader and decoders, the intermediate merge.
                "unnamed".to_string()
            } else if thread.name == "stage-profiler" {
                continue;
            } else {
                group_name(&thread.name)
            };
            let entry = groups.entry(group).or_default();
            entry.0 += 1;
            entry.1 += cpu;
        }
        let mut groups = groups
            .into_iter()
            .map(|(name, (threads, cpu_ns))| ThreadGroup {
                name,
                threads,
                cpu_seconds: cpu_ns as f64 / 1e9,
                busy_threads: 0.0,
            })
            .collect::<Vec<_>>();
        groups.sort_by(|a, b| b.cpu_seconds.total_cmp(&a.cpu_seconds));
        (groups, (peak > 0).then_some(peak))
    }

    /// `omt-render-3` → `omt-render`.
    fn group_name(name: &str) -> String {
        let trimmed = name.trim_end_matches(|c: char| c.is_ascii_digit());
        let trimmed = trimmed.trim_end_matches(['-', '_', ' ', '#']);
        if trimmed.is_empty() {
            name.to_string()
        } else {
            trimmed.to_string()
        }
    }

    pub(super) fn machine() -> (Option<usize>, Option<String>, Option<u64>) {
        let cpuinfo = fs::read_to_string("/proc/cpuinfo").unwrap_or_default();
        let mut cores = std::collections::HashSet::new();
        let (mut physical, mut model) = (None::<String>, None);
        for line in cpuinfo.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let (key, value) = (key.trim(), value.trim());
            match key {
                "physical id" => physical = Some(value.to_string()),
                "core id" => {
                    cores.insert((physical.clone(), value.to_string()));
                }
                "model name" if model.is_none() => model = Some(value.to_string()),
                _ => {}
            }
        }
        let memory = fs::read_to_string("/proc/meminfo").ok().and_then(|m| {
            let line = m.lines().find(|l| l.starts_with("MemTotal:"))?;
            let kib: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
            Some(kib * 1024)
        });
        ((!cores.is_empty()).then_some(cores.len()), model, memory)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_are_recorded_only_while_a_profile_runs() {
        drop(Stage::start("before"));
        start(4);
        {
            let _stage = Stage::start("work");
            let handles = (0..2)
                .map(|_| {
                    std::thread::spawn(|| {
                        let started = Instant::now();
                        let mut x = 0u64;
                        while started.elapsed() < Duration::from_millis(120) {
                            x = x.wrapping_mul(31).wrapping_add(7);
                        }
                        std::hint::black_box(x);
                    })
                })
                .collect::<Vec<_>>();
            time(Counter::RenderBusy, || {
                for handle in handles {
                    handle.join().unwrap();
                }
            });
        }
        let report = finish().unwrap();
        drop(Stage::start("after"));
        assert_eq!(report.threads, 4);
        // Tests running beside this one may add stages of their own.
        assert!(report.stages.iter().all(|s| s.name != "before"));
        let stage = report.stages.iter().find(|s| s.name == "work").unwrap();
        assert!(stage.wall_seconds >= 0.1);
        assert!(stage.counters.iter().any(|c| c.name == "render_busy"));
        #[cfg(unix)]
        {
            // Two threads spinning for the whole stage.
            let busy = stage.busy_threads.unwrap();
            assert!(busy > 1.2, "busy threads {busy}");
        }
        assert!(report.table().contains("work"));
        assert!(finish().is_none());
    }
}
