//! A job: work that runs on a thread of its own while the host polls its
//! progress, asks it to stop, and collects its outcome. Nothing here knows
//! what the work is; the request kinds in [`crate::kinds`] supply it.

use serde_json::{Value, json};
use std::panic::AssertUnwindSafe;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// Where a job is. The numbers are the C ABI's `OTC_JOB_*` constants.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i32)]
pub enum JobState {
    Running = 0,
    Succeeded = 1,
    Failed = 2,
    Cancelled = 3,
}

impl JobState {
    pub fn id(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Succeeded => "succeeded",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// Why a job, or a request to start one, did not succeed. `kind` is one of
/// `osm_tile_compiler::CompileError::kind`'s identifiers, or `internal` for
/// a fault of this library (a crash, a thread that could not start).
#[derive(Debug, Clone, PartialEq)]
pub struct JobError {
    pub kind: &'static str,
    pub message: String,
    /// The compile stage it happened in, if one had started.
    pub stage: Option<&'static str>,
}

impl JobError {
    pub fn invalid_request(message: impl Into<String>) -> Self {
        Self {
            kind: "invalid_request",
            message: message.into(),
            stage: None,
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: "internal",
            message: message.into(),
            stage: None,
        }
    }

    pub fn cancelled() -> Self {
        Self {
            kind: "cancelled",
            message: "The job was cancelled".into(),
            stage: None,
        }
    }

    pub fn to_json(&self) -> Value {
        json!({ "kind": self.kind, "message": self.message, "stage": self.stage })
    }
}

/// One progress report, as the work gives it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Progress {
    /// A stable stage identifier, such as `read_osm`.
    pub stage: Option<&'static str>,
    /// What is happening, in words fit to show.
    pub message: String,
    /// How much of the stage is done, 0 to 1, when it can be told.
    pub fraction: Option<f64>,
    /// How much of the whole job is done, 0 to 1, when it can be told.
    pub overall: Option<f64>,
}

/// What the work may do while it runs: report progress and check for a
/// cancel.
pub struct Reporter<'a> {
    shared: &'a Shared,
}

impl Reporter<'_> {
    pub fn report(&self, progress: Progress) {
        let mut inner = self.shared.lock();
        inner.progress = progress;
        inner.updates += 1;
    }

    pub fn is_cancelled(&self) -> bool {
        self.shared.cancel.load(Ordering::Relaxed)
    }
}

/// The work of a job: runs once, on the job's thread.
pub type Work = Box<dyn FnOnce(&Reporter) -> Result<Value, JobError> + Send>;

struct Inner {
    progress: Progress,
    /// How many reports the work has made.
    updates: u64,
    outcome: Option<Result<Value, JobError>>,
}

/// What the job's thread and every handle to the job share.
pub struct Shared {
    /// The request kind, as the result reports it.
    kind: &'static str,
    cancel: AtomicBool,
    inner: Mutex<Inner>,
    finished: Condvar,
}

impl Shared {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn state_of(inner: &Inner) -> JobState {
        match &inner.outcome {
            None => JobState::Running,
            Some(Ok(_)) => JobState::Succeeded,
            Some(Err(error)) if error.kind == "cancelled" => JobState::Cancelled,
            Some(Err(_)) => JobState::Failed,
        }
    }

    pub fn state(&self) -> JobState {
        Self::state_of(&self.lock())
    }

    /// Asks the work to stop. It does within about a second; the job then
    /// ends `cancelled`, unless it had already finished.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }

    /// Waits until the job ends or `timeout` passes, and returns its state.
    pub fn wait(&self, timeout: Duration) -> JobState {
        let deadline = Instant::now().checked_add(timeout);
        let mut inner = self.lock();
        while inner.outcome.is_none() {
            let left = match deadline {
                Some(deadline) => deadline.saturating_duration_since(Instant::now()),
                None => Duration::from_secs(3600),
            };
            if left.is_zero() {
                break;
            }
            inner = self
                .finished
                .wait_timeout(inner, left)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
        Self::state_of(&inner)
    }

    /// The poll JSON: the state, whether a cancel was asked for, and the
    /// latest progress.
    pub fn poll(&self) -> Value {
        let inner = self.lock();
        let state = Self::state_of(&inner);
        let progress = &inner.progress;
        let overall = match state {
            JobState::Succeeded => Some(1.0),
            _ => progress.overall,
        };
        json!({
            "state": state.id(),
            "kind": self.kind,
            "cancelRequested": self.cancel.load(Ordering::Relaxed),
            "updates": inner.updates,
            "stage": progress.stage,
            "message": progress.message,
            "fraction": progress.fraction,
            "overall": overall,
        })
    }

    /// The result JSON, once the job has ended.
    pub fn result(&self) -> Option<Value> {
        let inner = self.lock();
        inner.outcome.as_ref().map(|outcome| match outcome {
            Ok(value) => json!({ "ok": true, "kind": self.kind, "result": value }),
            Err(error) => json!({ "ok": false, "kind": self.kind, "error": error.to_json() }),
        })
    }
}

/// Starts `work` on a thread of its own.
pub fn spawn(kind: &'static str, work: Work) -> Result<Arc<Shared>, JobError> {
    let shared = Arc::new(Shared {
        kind,
        cancel: AtomicBool::new(false),
        inner: Mutex::new(Inner {
            progress: Progress::default(),
            updates: 0,
            outcome: None,
        }),
        finished: Condvar::new(),
    });
    let thread_shared = shared.clone();
    std::thread::Builder::new()
        .name(format!("otc-job-{kind}"))
        .spawn(move || {
            let shared = &*thread_shared;
            let reporter = Reporter { shared };
            let mut outcome = std::panic::catch_unwind(AssertUnwindSafe(|| work(&reporter)))
                .unwrap_or_else(|payload| {
                    Err(JobError::internal(format!(
                        "The job crashed: {}",
                        osm_tile_compiler::panic_message(&*payload)
                    )))
                });
            // Work that fails after a cancel was cancelled, whatever it says.
            if outcome.is_err() && reporter.is_cancelled() {
                outcome = Err(JobError::cancelled());
            }
            shared.lock().outcome = Some(outcome);
            shared.finished.notify_all();
        })
        .map_err(|e| JobError::internal(format!("Failed to start the job's thread: {e}")))?;
    Ok(shared)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wait(shared: &Shared) -> JobState {
        shared.wait(Duration::from_secs(10))
    }

    #[test]
    fn a_job_reports_and_succeeds() {
        let job = spawn(
            "test",
            Box::new(|reporter| {
                reporter.report(Progress {
                    stage: Some("one"),
                    message: "Halfway".into(),
                    fraction: Some(0.5),
                    overall: Some(0.5),
                });
                Ok(json!({ "answer": 42 }))
            }),
        )
        .unwrap();
        assert_eq!(wait(&job), JobState::Succeeded);
        let poll = job.poll();
        assert_eq!(poll["state"], "succeeded");
        assert_eq!(poll["stage"], "one");
        assert_eq!(poll["updates"], 1);
        assert_eq!(poll["overall"], 1.0);
        assert_eq!(
            job.result().unwrap(),
            json!({ "ok": true, "kind": "test", "result": { "answer": 42 } })
        );
    }

    #[test]
    fn a_cancelled_job_ends_cancelled() {
        let job = spawn(
            "test",
            Box::new(|reporter| {
                while !reporter.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Err(JobError::internal("stopped"))
            }),
        )
        .unwrap();
        assert_eq!(job.wait(Duration::from_millis(20)), JobState::Running);
        assert!(job.result().is_none());
        job.cancel();
        assert_eq!(wait(&job), JobState::Cancelled);
        assert_eq!(job.result().unwrap()["error"]["kind"], "cancelled");
    }

    #[test]
    fn a_panic_becomes_an_internal_error() {
        let job = spawn("test", Box::new(|_| panic!("boom"))).unwrap();
        assert_eq!(wait(&job), JobState::Failed);
        let error = &job.result().unwrap()["error"];
        assert_eq!(error["kind"], "internal");
        assert_eq!(error["message"], "The job crashed: boom");
    }

    #[test]
    fn success_after_a_late_cancel_stands() {
        let job = spawn(
            "test",
            Box::new(|reporter| {
                while !reporter.is_cancelled() {
                    std::thread::sleep(Duration::from_millis(5));
                }
                Ok(json!(null))
            }),
        )
        .unwrap();
        job.cancel();
        assert_eq!(wait(&job), JobState::Succeeded);
    }
}
