//! A C ABI for osm-tile-compiler, for apps on iOS, Android and other native
//! hosts. A host passes a JSON request to [`otc_job_start`], which runs it as
//! a job on a thread of its own; the host then polls the job's progress,
//! may cancel it, and collects its result as JSON. `include/osm_tile_compiler.h`
//! declares these functions, and `docs/c-abi.md` describes the JSON.
//!
//! Every function catches panics, so none unwinds into the host. That needs
//! `panic = "unwind"`, the default: a build with `panic = "abort"` ends the
//! app on a panic instead.

#![allow(clippy::missing_safety_doc)]

mod archive_jobs;
mod job;
mod kinds;
mod update_job;

use job::{JobError, JobState, Shared};
use std::ffi::{CStr, CString, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::Arc;
use std::time::Duration;

/// The version of this C ABI: the functions' signatures and the envelope,
/// poll and result JSON. A host sends it as the envelope's `abiVersion`. It
/// changes only when a host written for the old version would misbehave.
pub const ABI_VERSION: u32 = 1;

/// Status codes, the header's `OTC_*` constants.
pub const OTC_OK: i32 = 0;
/// A null pointer where one is not allowed, or a string that is not UTF-8.
pub const OTC_ERROR_INVALID_ARGUMENT: i32 = 1;
/// The request cannot run; the error JSON says why.
pub const OTC_ERROR_INVALID_REQUEST: i32 = 2;
/// The job has not ended yet, so it has no result.
pub const OTC_ERROR_NOT_FINISHED: i32 = 3;
/// A fault of this library: a crash, or a thread that could not start.
pub const OTC_ERROR_INTERNAL: i32 = 4;

/// A job, as the host holds it. Every function but [`otc_job_free`] may be
/// called on it from any thread, at the same time.
pub struct OtcJob {
    shared: Arc<Shared>,
}

/// Hands a JSON value to the host as a string it frees with
/// [`otc_string_free`].
fn to_c(value: &serde_json::Value) -> *mut c_char {
    // serde_json escapes NUL, so the text never holds one.
    CString::new(value.to_string()).map_or(std::ptr::null_mut(), CString::into_raw)
}

/// Writes `value` to `out` when `out` is not null.
unsafe fn put(out: *mut *mut c_char, value: &serde_json::Value) {
    if !out.is_null() {
        unsafe { *out = to_c(value) };
    }
}

/// Runs `body`, turning a panic into `fallback`.
fn guard<T>(fallback: T, body: impl FnOnce() -> T) -> T {
    catch_unwind(AssertUnwindSafe(body)).unwrap_or(fallback)
}

/// The C ABI version, [`ABI_VERSION`].
#[unsafe(no_mangle)]
pub extern "C" fn otc_abi_version() -> u32 {
    ABI_VERSION
}

/// The library's version, such as `0.1.3`: a static string the host must
/// not free.
#[unsafe(no_mangle)]
pub extern "C" fn otc_version() -> *const c_char {
    concat!(env!("CARGO_PKG_VERSION"), "\0").as_ptr().cast()
}

/// Starts the job `request_json` describes, an envelope
/// `{"abiVersion": 1, "kind": "...", "request": {...}}`.
///
/// On success, returns `OTC_OK` and sets `*job_out` to the job, which the
/// host frees with [`otc_job_free`]. Otherwise sets `*job_out` to null,
/// returns `OTC_ERROR_INVALID_ARGUMENT`, `OTC_ERROR_INVALID_REQUEST` or
/// `OTC_ERROR_INTERNAL`, and, if `error_out` is not null, sets `*error_out`
/// to an error JSON `{"kind", "message", "stage"}` for the host to free with
/// [`otc_string_free`]. On success `*error_out` is set to null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otc_job_start(
    request_json: *const c_char,
    job_out: *mut *mut OtcJob,
    error_out: *mut *mut c_char,
) -> i32 {
    if !error_out.is_null() {
        unsafe { *error_out = std::ptr::null_mut() };
    }
    if job_out.is_null() {
        let error = JobError {
            kind: "invalid_request",
            message: "job_out is null".into(),
            stage: None,
        };
        unsafe { put(error_out, &error.to_json()) };
        return OTC_ERROR_INVALID_ARGUMENT;
    }
    unsafe { *job_out = std::ptr::null_mut() };
    let started = guard(
        Err((
            OTC_ERROR_INTERNAL,
            JobError::internal("The library crashed while starting the job"),
        )),
        || {
            if request_json.is_null() {
                return Err((
                    OTC_ERROR_INVALID_ARGUMENT,
                    JobError::invalid_request("request_json is null"),
                ));
            }
            let text = unsafe { CStr::from_ptr(request_json) }
                .to_str()
                .map_err(|_| {
                    (
                        OTC_ERROR_INVALID_ARGUMENT,
                        JobError::invalid_request("request_json is not UTF-8"),
                    )
                })?;
            let (kind, work) = kinds::parse(text).map_err(|e| (OTC_ERROR_INVALID_REQUEST, e))?;
            job::spawn(kind, work).map_err(|e| (OTC_ERROR_INTERNAL, e))
        },
    );
    match started {
        Ok(shared) => {
            unsafe { *job_out = Box::into_raw(Box::new(OtcJob { shared })) };
            OTC_OK
        }
        Err((status, error)) => {
            unsafe { put(error_out, &error.to_json()) };
            status
        }
    }
}

/// Borrows the job behind a handle.
unsafe fn job<'a>(job: *const OtcJob) -> Option<&'a OtcJob> {
    unsafe { job.as_ref() }
}

/// The job's state: `OTC_JOB_RUNNING`, `OTC_JOB_SUCCEEDED`, `OTC_JOB_FAILED`
/// or `OTC_JOB_CANCELLED`; for a null job, `-OTC_ERROR_INVALID_ARGUMENT`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otc_job_state(handle: *const OtcJob) -> i32 {
    match unsafe { job(handle) } {
        Some(job) => guard(-OTC_ERROR_INTERNAL, || job.shared.state() as i32),
        None => -OTC_ERROR_INVALID_ARGUMENT,
    }
}

/// The job's progress as JSON: `{"state", "kind", "cancelRequested",
/// "updates", "stage", "message", "fraction", "overall"}`, for the host to
/// free with [`otc_string_free`]. Null for a null job.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otc_job_poll(handle: *const OtcJob) -> *mut c_char {
    match unsafe { job(handle) } {
        Some(job) => guard(std::ptr::null_mut(), || to_c(&job.shared.poll())),
        None => std::ptr::null_mut(),
    }
}

/// Waits until the job ends or `timeout_ms` milliseconds pass, and returns
/// its state as [`otc_job_state`] does. A timeout of 0 does not wait.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otc_job_wait(handle: *const OtcJob, timeout_ms: u32) -> i32 {
    match unsafe { job(handle) } {
        Some(job) => guard(-OTC_ERROR_INTERNAL, || {
            job.shared
                .wait(Duration::from_millis(u64::from(timeout_ms))) as i32
        }),
        None => -OTC_ERROR_INVALID_ARGUMENT,
    }
}

/// Asks the job to stop. It returns at once; the job stops within about a
/// second and then ends `OTC_JOB_CANCELLED`, having removed its partial
/// outputs, unless it had already finished. Does nothing for a null job.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otc_job_cancel(handle: *const OtcJob) {
    if let Some(job) = unsafe { job(handle) } {
        job.shared.cancel();
    }
}

/// The job's result, once it has ended: returns `OTC_OK` and sets
/// `*result_out` to `{"ok": true, "kind", "result": {...}}` or
/// `{"ok": false, "kind", "error": {"kind", "message", "stage"}}`, for the
/// host to free with [`otc_string_free`]. Each call returns a fresh copy.
/// While the job runs, returns `OTC_ERROR_NOT_FINISHED` and sets
/// `*result_out` to null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otc_job_result(
    handle: *const OtcJob,
    result_out: *mut *mut c_char,
) -> i32 {
    if result_out.is_null() {
        return OTC_ERROR_INVALID_ARGUMENT;
    }
    unsafe { *result_out = std::ptr::null_mut() };
    let Some(job) = (unsafe { job(handle) }) else {
        return OTC_ERROR_INVALID_ARGUMENT;
    };
    guard(OTC_ERROR_INTERNAL, || match job.shared.result() {
        Some(result) => {
            unsafe { put(result_out, &result) };
            OTC_OK
        }
        None => OTC_ERROR_NOT_FINISHED,
    })
}

/// Frees a job handle. A job still running is cancelled and finishes its
/// clean-up on its own thread; this does not wait for it. Does nothing for a
/// null job. The handle must not be used again.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otc_job_free(handle: *mut OtcJob) {
    if handle.is_null() {
        return;
    }
    let job = unsafe { Box::from_raw(handle) };
    job.shared.cancel();
}

/// Frees a string this library returned. Does nothing for null.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn otc_string_free(string: *mut c_char) {
    if !string.is_null() {
        drop(unsafe { CString::from_raw(string) });
    }
}

/// The header's job states match [`JobState`].
const _: () = {
    assert!(JobState::Running as i32 == 0);
    assert!(JobState::Succeeded as i32 == 1);
    assert!(JobState::Failed as i32 == 2);
    assert!(JobState::Cancelled as i32 == 3);
};
