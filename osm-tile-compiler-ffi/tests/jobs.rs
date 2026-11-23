//! The C functions, called from Rust: a Monaco compile polled to the end,
//! one cancelled, and requests that fail to start.

use osm_tile_compiler_ffi::*;
use serde_json::{Value, json};
use std::ffi::{CStr, CString, c_char};
use std::path::Path;
use std::ptr::null_mut;

fn monaco_pbf() -> String {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures/monaco.osm.pbf")
        .to_str()
        .unwrap()
        .to_string()
}

/// Takes a string the library returned.
fn take(string: *mut c_char) -> Value {
    assert!(!string.is_null());
    let value = serde_json::from_str(unsafe { CStr::from_ptr(string) }.to_str().unwrap()).unwrap();
    unsafe { otc_string_free(string) };
    value
}

fn start(envelope: &Value) -> Result<*mut OtcJob, (i32, Value)> {
    let text = CString::new(envelope.to_string()).unwrap();
    let (mut job, mut error) = (null_mut(), null_mut());
    let status = unsafe { otc_job_start(text.as_ptr(), &mut job, &mut error) };
    if status == OTC_OK {
        assert!(error.is_null());
        Ok(job)
    } else {
        assert!(job.is_null());
        Err((status, take(error)))
    }
}

fn compile(dir: &Path) -> Value {
    json!({
        "abiVersion": ABI_VERSION,
        "kind": "compile_basemap",
        "request": {
            "inputPbf": monaco_pbf(),
            "output": dir.join("monaco.pmtiles"),
            "sources": dir.join("sources"),
            "tempDir": dir.join("temp"),
            "omitSources": ["water_polygons", "natural_earth", "lake_centerlines"],
            "threads": 2,
            "zoom": { "max": 10 },
        },
    })
}

fn result(job: *const OtcJob) -> Value {
    let mut out = null_mut();
    assert_eq!(unsafe { otc_job_result(job, &mut out) }, OTC_OK);
    take(out)
}

/// The compile's stage identifiers, in the order a compile runs them.
const STAGE_ORDER: [&str; 6] = [
    "fetch_sources",
    "read_sources",
    "index_osm",
    "read_osm",
    "write_tiles",
    "building_levels",
];

#[test]
fn a_compile_is_polled_to_its_result() {
    let dir = tempfile::tempdir().unwrap();
    let job = start(&compile(dir.path())).unwrap();
    // A poll shows only the latest report, and in a release build Monaco's
    // stages can each end between two polls, so which stages a poll catches
    // is timing. What holds every time: the stages seen are known and come
    // in pipeline order, and progress never goes back.
    let mut stages = Vec::new();
    let mut overall = 0.0;
    let mut updates = 0;
    loop {
        let poll = take(unsafe { otc_job_poll(job) });
        if let Some(stage) = poll["stage"].as_str()
            && stages.last().map(String::as_str) != Some(stage)
        {
            stages.push(stage.to_string());
        }
        if let Some(now) = poll["overall"].as_f64() {
            assert!(
                now >= overall,
                "overall went from {overall} to {now}: {poll}"
            );
            overall = now;
        }
        let now = poll["updates"].as_u64().unwrap();
        assert!(
            now >= updates,
            "updates went from {updates} to {now}: {poll}"
        );
        updates = now;
        if poll["state"] != "running" {
            assert_eq!(poll["state"], "succeeded", "{poll}");
            assert_eq!(poll["overall"], 1.0);
            break;
        }
        let mut out = null_mut();
        assert_eq!(
            unsafe { otc_job_result(job, &mut out) },
            OTC_ERROR_NOT_FINISHED
        );
        assert!(out.is_null());
        unsafe { otc_job_wait(job, 5) };
    }
    assert_eq!(unsafe { otc_job_state(job) }, 1);
    let result = result(job);
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["kind"], "compile_basemap");
    let output = &result["result"];
    assert_eq!(output["format"], "basemap-manifest");
    assert!(output["result"]["tiles"].as_u64().unwrap() > 0);
    assert_eq!(output["archive"], json!(dir.path().join("monaco.pmtiles")));
    assert!(dir.path().join("monaco.pmtiles").is_file());
    // The work reported, and the last poll still shows its last report.
    assert!(updates > 0);
    assert!(!stages.is_empty(), "no stage was ever polled");
    let order: Vec<usize> = stages
        .iter()
        .map(|stage| {
            STAGE_ORDER
                .iter()
                .position(|known| known == stage)
                .unwrap_or_else(|| panic!("unknown stage {stage}"))
        })
        .collect();
    assert!(order.is_sorted(), "stages out of order: {stages:?}");
    unsafe { otc_job_free(job) };
}

#[test]
fn a_style_pack_is_written_from_a_local_mirror() {
    let dir = tempfile::tempdir().unwrap();
    let mirror = dir.path().join("mirror");
    for font in ["Noto Sans Regular", "Noto Sans Bold", "Noto Sans Italic"] {
        std::fs::create_dir_all(mirror.join(font)).unwrap();
        std::fs::write(mirror.join(font).join("0-255.pbf"), b"glyphs").unwrap();
    }
    std::fs::write(mirror.join("sprite.json"), b"{}").unwrap();
    std::fs::write(mirror.join("sprite.png"), b"png").unwrap();
    let pack = dir.path().join("pack");
    let job = start(&json!({
        "abiVersion": ABI_VERSION,
        "kind": "write_style_pack",
        "request": {
            "directory": pack,
            "archiveUrl": "asset://monaco.pmtiles",
            "glyphs": format!("{}/{{fontstack}}/{{range}}.pbf", mirror.display()),
            "sprite": mirror.join("sprite"),
            "baseUrl": "asset://pack",
        },
    }))
    .unwrap();
    while unsafe { otc_job_state(job) } == 0 {
        unsafe { otc_job_wait(job, 100) };
    }
    let result = result(job);
    assert_eq!(result["ok"], true, "{result}");
    assert_eq!(result["kind"], "write_style_pack");
    let report = &result["result"];
    assert_eq!(report["style"], json!(pack.join("style.json")));
    assert_eq!(report["ranges"].as_array().unwrap().len(), 7);
    // Every Latin range but the first, in each font, and the @2x sprite.
    assert_eq!(report["missing"].as_array().unwrap().len(), 3 * 6 + 2);
    let style: Value =
        serde_json::from_slice(&std::fs::read(pack.join("style.json")).unwrap()).unwrap();
    assert_eq!(
        style["glyphs"],
        "asset://pack/fonts/{fontstack}/{range}.pbf"
    );
    unsafe { otc_job_free(job) };
}

#[test]
fn a_cancelled_compile_leaves_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let job = start(&compile(dir.path())).unwrap();
    unsafe { otc_job_cancel(job) };
    assert_eq!(unsafe { otc_job_wait(job, 30_000) }, 3);
    let result = result(job);
    assert_eq!(result["ok"], false);
    assert_eq!(result["error"]["kind"], "cancelled");
    assert!(take(unsafe { otc_job_poll(job) })["cancelRequested"] == true);
    assert!(!dir.path().join("monaco.pmtiles").exists());
    unsafe { otc_job_free(job) };
}

#[test]
fn a_freed_running_job_cleans_up() {
    let dir = tempfile::tempdir().unwrap();
    let job = start(&compile(dir.path())).unwrap();
    // Once a stage has started, the compile has its scratch directory.
    while take(unsafe { otc_job_poll(job) })["updates"] == 0 {
        unsafe { otc_job_wait(job, 5) };
    }
    unsafe { otc_job_free(job) };
    // The cancelled compile removes its scratch directory on its own thread.
    let temp = dir.path().join("temp");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    while std::fs::read_dir(&temp).is_ok_and(|mut entries| entries.next().is_some()) {
        assert!(std::time::Instant::now() < deadline, "scratch left behind");
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    assert!(!dir.path().join("monaco.pmtiles").exists());
}

#[test]
fn a_request_that_fails_once_it_reads_fails_the_job() {
    let dir = tempfile::tempdir().unwrap();
    let mut envelope = compile(dir.path());
    envelope["request"]["inputPbf"] = json!(dir.path().join("missing.osm.pbf"));
    let job = start(&envelope).unwrap();
    assert_eq!(unsafe { otc_job_wait(job, 30_000) }, 2);
    let error = &result(job)["error"];
    assert_eq!(error["kind"], "invalid_request", "{error}");
    unsafe { otc_job_free(job) };
}

#[test]
fn bad_requests_fail_to_start() {
    let (status, error) = start(&json!({ "abiVersion": 1, "kind": "nope" })).unwrap_err();
    assert_eq!(status, OTC_ERROR_INVALID_REQUEST);
    assert_eq!(error["kind"], "invalid_request");
    assert!(error["message"].as_str().unwrap().contains("nope"));

    // What BasemapRequest::validate refuses fails the start, not the job.
    let dir = tempfile::tempdir().unwrap();
    let mut envelope = compile(dir.path());
    envelope["request"]["layers"] = json!({ "only": ["no_such_layer"] });
    let (status, error) = start(&envelope).unwrap_err();
    assert_eq!(status, OTC_ERROR_INVALID_REQUEST);
    assert!(
        error["message"].as_str().unwrap().contains("no_such_layer"),
        "{error}"
    );

    let mut job = null_mut();
    let status = unsafe { otc_job_start(std::ptr::null(), &mut job, null_mut()) };
    assert_eq!(status, OTC_ERROR_INVALID_ARGUMENT);
    let invalid = [0xff_u8, 0];
    let mut error = null_mut();
    let status = unsafe { otc_job_start(invalid.as_ptr().cast(), &mut job, &mut error) };
    assert_eq!(status, OTC_ERROR_INVALID_ARGUMENT);
    assert!(take(error)["message"].as_str().unwrap().contains("UTF-8"));
}

#[test]
fn null_handles_are_harmless() {
    unsafe {
        assert_eq!(otc_job_state(std::ptr::null()), -OTC_ERROR_INVALID_ARGUMENT);
        assert!(otc_job_poll(std::ptr::null()).is_null());
        otc_job_cancel(std::ptr::null());
        otc_job_free(null_mut());
        otc_string_free(null_mut());
    }
    let version = unsafe { CStr::from_ptr(otc_version()) };
    assert_eq!(version.to_str().unwrap(), env!("CARGO_PKG_VERSION"));
}
