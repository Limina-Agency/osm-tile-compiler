/*
 * osm-tile-compiler C ABI, version 1.
 *
 * A host passes a JSON request to otc_job_start, which runs it as a job on a
 * thread of its own. The host polls the job's progress, may cancel it, and
 * collects its result as JSON. docs/c-abi.md in the repository describes the
 * JSON; tests/header.rs checks this file against the library.
 *
 * Strings: every char * the library returns is UTF-8 JSON, NUL-terminated,
 * owned by the host, and freed with otc_string_free (never free()).
 * otc_version's string is static and is not freed.
 *
 * Threads: every function but otc_job_free may be called on a job from any
 * thread, at the same time. No function calls back into the host.
 *
 * Paths in requests are used as given, so pass absolute ones.
 */
#ifndef OSM_TILE_COMPILER_H
#define OSM_TILE_COMPILER_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

#define OTC_ABI_VERSION 1

/* Status codes. */
#define OTC_OK 0
/* A null pointer where one is not allowed, or a string that is not UTF-8. */
#define OTC_ERROR_INVALID_ARGUMENT 1
/* The request cannot run; the error JSON says why. */
#define OTC_ERROR_INVALID_REQUEST 2
/* The job has not ended yet, so it has no result. */
#define OTC_ERROR_NOT_FINISHED 3
/* A fault of this library: a crash, or a thread that could not start. */
#define OTC_ERROR_INTERNAL 4

/* Job states. */
#define OTC_JOB_RUNNING 0
#define OTC_JOB_SUCCEEDED 1
#define OTC_JOB_FAILED 2
#define OTC_JOB_CANCELLED 3

/* A job. Opaque; freed with otc_job_free. */
typedef struct OtcJob OtcJob;

/* The C ABI version, OTC_ABI_VERSION for a matching library. */
uint32_t otc_abi_version(void);

/* The library's version, such as "0.1.3". Static: do not free. */
const char *otc_version(void);

/*
 * Starts the job that request_json describes:
 *   {"abiVersion": 1, "kind": "compile_basemap", "request": {...}}
 * On success returns OTC_OK and sets *job_out. Otherwise sets *job_out to
 * NULL, returns OTC_ERROR_INVALID_ARGUMENT, OTC_ERROR_INVALID_REQUEST or
 * OTC_ERROR_INTERNAL, and sets *error_out (when error_out is not NULL) to
 * {"kind", "message", "stage"}. *error_out is NULL on success.
 */
int32_t otc_job_start(const char *request_json, OtcJob **job_out, char **error_out);

/*
 * Checks request_json as otc_job_start does, without starting a job.
 * Returns OTC_OK when otc_job_start would start it; otherwise returns the
 * status otc_job_start would and sets *error_out (when error_out is not
 * NULL) to {"kind", "message", "stage"}. *error_out is NULL on OTC_OK.
 */
int32_t otc_validate(const char *request_json, char **error_out);

/* The job's state, an OTC_JOB_* value; -OTC_ERROR_INVALID_ARGUMENT for NULL. */
int32_t otc_job_state(const OtcJob *job);

/*
 * The job's progress:
 *   {"state", "kind", "cancelRequested", "updates", "stage", "message",
 *    "fraction", "overall"}
 * NULL for a NULL job.
 */
char *otc_job_poll(const OtcJob *job);

/*
 * Waits until the job ends or timeout_ms passes; returns its state as
 * otc_job_state does. 0 does not wait.
 */
int32_t otc_job_wait(const OtcJob *job, uint32_t timeout_ms);

/*
 * Asks the job to stop and returns at once. The job stops within about a
 * second, removes its partial outputs, and ends OTC_JOB_CANCELLED, unless
 * it had already finished.
 */
void otc_job_cancel(const OtcJob *job);

/*
 * Once the job has ended, returns OTC_OK and sets *result_out to
 *   {"ok": true, "kind", "result": {...}} or
 *   {"ok": false, "kind", "error": {"kind", "message", "stage"}}.
 * While it runs, returns OTC_ERROR_NOT_FINISHED and sets *result_out to NULL.
 */
int32_t otc_job_result(const OtcJob *job, char **result_out);

/*
 * Frees a job. A job still running is cancelled and cleans up on its own
 * thread; this does not wait. The handle must not be used again.
 */
void otc_job_free(OtcJob *job);

/* Frees a string the library returned. NULL is allowed. */
void otc_string_free(char *string);

#ifdef __cplusplus
}
#endif

#endif /* OSM_TILE_COMPILER_H */
