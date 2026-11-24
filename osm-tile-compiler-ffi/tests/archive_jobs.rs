//! The job kinds besides the compile, through the C functions, the way an
//! app keeps a map current: update an extract with a change file, compile
//! the old and the new extract, diff the two archives, apply the delta,
//! verify and inspect the result, and cut an area out of it.

use osm_tile_compiler_ffi::*;
use serde_json::{Value, json};
use std::ffi::{CStr, CString, c_char};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;

/// A change creating a motorway in Monaco and a café in Sydney, which the
/// clip to Monaco's box drops.
const MONACO_CHANGES: &str = r#"<?xml version='1.0' encoding='UTF-8'?>
<osmChange version="0.6" generator="test">
  <create>
    <node id="99000000001" version="1" lat="43.7350" lon="7.4200"/>
    <node id="99000000002" version="1" lat="43.7370" lon="7.4250"/>
    <node id="99000000003" version="1" lat="-33.8688" lon="151.2093">
      <tag k="amenity" v="cafe"/>
    </node>
    <way id="99000000010" version="1">
      <nd ref="99000000001"/>
      <nd ref="99000000002"/>
      <tag k="highway" v="motorway"/>
      <tag k="name" v="Test motorway"/>
    </way>
  </create>
</osmChange>"#;

fn monaco_pbf() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../fixtures/monaco.osm.pbf")
}

fn take(string: *mut c_char) -> Value {
    assert!(!string.is_null());
    let value = serde_json::from_str(unsafe { CStr::from_ptr(string) }.to_str().unwrap()).unwrap();
    unsafe { otc_string_free(string) };
    value
}

fn start(kind: &str, request: Value) -> Result<*mut OtcJob, Value> {
    let envelope = json!({ "abiVersion": ABI_VERSION, "kind": kind, "request": request });
    let text = CString::new(envelope.to_string()).unwrap();
    let (mut job, mut error) = (null_mut(), null_mut());
    match unsafe { otc_job_start(text.as_ptr(), &mut job, &mut error) } {
        OTC_OK => Ok(job),
        status => {
            assert_eq!(status, OTC_ERROR_INVALID_REQUEST);
            Err(take(error))
        }
    }
}

/// Runs a job to its end, checking that its progress never goes back, and
/// returns its result and the stages it was seen in.
fn run(kind: &str, request: Value) -> (Value, Vec<String>) {
    let job = start(kind, request).unwrap_or_else(|error| panic!("{kind}: {error}"));
    let mut stages: Vec<String> = Vec::new();
    let mut overall = 0.0;
    loop {
        let poll = take(unsafe { otc_job_poll(job) });
        assert_eq!(poll["kind"], kind);
        if let Some(stage) = poll["stage"].as_str()
            && stages.last().map(String::as_str) != Some(stage)
        {
            stages.push(stage.to_string());
        }
        if let Some(now) = poll["overall"].as_f64() {
            assert!(now >= overall, "{kind}: overall went back: {poll}");
            overall = now;
        }
        if poll["state"] != "running" {
            break;
        }
        unsafe { otc_job_wait(job, 5) };
    }
    let mut out = null_mut();
    assert_eq!(unsafe { otc_job_result(job, &mut out) }, OTC_OK);
    unsafe { otc_job_free(job) };
    let result = take(out);
    assert_eq!(result["kind"], kind);
    (result, stages)
}

/// Runs a job that must succeed and returns its result.
fn ok(kind: &str, request: Value) -> Value {
    let (result, _) = run(kind, request);
    assert_eq!(result["ok"], true, "{kind}: {result}");
    result["result"].clone()
}

fn compile(input: &Path, output: &Path, temp: &Path) {
    ok(
        "compile_basemap",
        json!({
            "inputPbf": input,
            "output": output,
            "sources": temp.join("sources"),
            "tempDir": temp.join("temp"),
            "omitSources": ["water_polygons", "natural_earth", "lake_centerlines"],
            "threads": 2,
            "zoom": { "max": 12 },
        }),
    );
}

#[test]
fn an_app_updates_diffs_applies_verifies_and_extracts() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    let changes = path("changes.osc");
    std::fs::write(&changes, MONACO_CHANGES).unwrap();

    // update, clipped to the header's box by default.
    let updated = path("updated.osm.pbf");
    let (result, stages) = run(
        "update",
        json!({ "input": monaco_pbf(), "output": updated, "changeFiles": [changes], "threads": 2 }),
    );
    assert_eq!(result["ok"], true, "{result}");
    let report = &result["result"];
    assert_eq!(report["changes_read"], 4, "{report}");
    assert_eq!(report["changes_kept"], 3, "{report}");
    assert_eq!(report["created"], 3, "{report}");
    assert_eq!(report["output"], json!(updated));
    assert!(updated.is_file());
    let order = ["read_input", "read_changes", "write_output"];
    let seen: Vec<usize> = stages
        .iter()
        .map(|stage| order.iter().position(|known| known == stage).unwrap())
        .collect();
    assert!(seen.is_sorted(), "{stages:?}");
    // Never over an existing file: refused at start.
    let error = start(
        "update",
        json!({ "input": monaco_pbf(), "output": updated, "changeFiles": [changes] }),
    )
    .unwrap_err();
    assert!(
        error["message"]
            .as_str()
            .unwrap()
            .contains("refusing to replace")
    );

    // Two archives, before and after the change.
    let (old, new) = (path("old.pmtiles"), path("new.pmtiles"));
    compile(&monaco_pbf(), &old, dir.path());
    compile(&updated, &new, dir.path());

    // diff, then apply rebuilds the new archive byte for byte.
    let delta = path("new.delta");
    let diff = ok(
        "diff",
        json!({ "base": old, "target": new, "output": delta }),
    );
    assert!(diff["delta_bytes"].as_u64().unwrap() > 0, "{diff}");
    assert!(diff["tiles"]["changed"].as_u64().unwrap() > 0, "{diff}");
    let rebuilt = path("rebuilt.pmtiles");
    let apply = ok(
        "apply",
        json!({ "base": old, "delta": delta, "output": rebuilt }),
    );
    assert_eq!(apply["target"], diff["target"], "{apply}");
    assert_eq!(
        std::fs::read(&rebuilt).unwrap(),
        std::fs::read(&new).unwrap()
    );
    // A delta does not apply to the archive it leads to.
    let (result, _) = run(
        "apply",
        json!({ "base": new, "delta": delta, "output": path("wrong.pmtiles") }),
    );
    assert_eq!(result["ok"], false, "{result}");
    assert!(!path("wrong.pmtiles").exists());

    // verify passes the rebuilt archive; inspect reports on it.
    let verified = ok("verify", json!({ "archive": rebuilt }));
    assert_eq!(verified["problem_count"], 0, "{verified}");
    let inspected = ok("inspect", json!({ "archive": rebuilt }));
    assert_eq!(inspected["zooms"], json!([0, 12]));
    assert!(inspected["addressed_tiles"].as_u64().unwrap() > 0);

    // A damaged archive: inspect lists its problems, verify fails.
    let mut bytes = std::fs::read(&new).unwrap();
    let end = bytes.len();
    bytes[end - 4096..].fill(0);
    let damaged = path("damaged.pmtiles");
    std::fs::write(&damaged, bytes).unwrap();
    let inspected = ok("inspect", json!({ "archive": damaged }));
    let problems = inspected["problem_count"].as_u64().unwrap();
    assert!(problems > 0, "{inspected}");
    let (result, _) = run("verify", json!({ "archive": damaged }));
    assert_eq!(result["ok"], false, "{result}");
    assert_eq!(result["error"]["kind"], "failed");
    let message = result["error"]["message"].as_str().unwrap();
    assert!(
        message.contains(&format!("{problems} problem")),
        "{message}"
    );
    // Not an archive at all: the job fails.
    let (result, _) = run("verify", json!({ "archive": changes }));
    assert_eq!(result["ok"], false, "{result}");

    // extract: Monaco's harbour, zooms 5 to 10.
    let harbour = path("harbour.pmtiles");
    let cut = ok(
        "extract",
        json!({
            "source": rebuilt,
            "output": harbour,
            "bounds": { "north": 43.738, "south": 43.732, "east": 7.43, "west": 7.42 },
            "zoom": { "min": 5, "max": 10 },
        }),
    );
    assert_eq!(cut["zooms"], json!([5, 10]), "{cut}");
    assert!(cut["tiles"].as_u64().unwrap() > 0);
    assert!(cut["tiles"].as_u64().unwrap() < cut["source_tiles"].as_u64().unwrap());
    assert_eq!(
        ok("verify", json!({ "archive": harbour }))["problem_count"],
        0
    );
    // A polygon that cannot be read fails the job, and leaves nothing.
    let (result, _) = run(
        "extract",
        json!({ "source": rebuilt, "output": path("none.pmtiles"), "polygon": path("missing.geojson") }),
    );
    assert_eq!(result["error"]["kind"], "invalid_request", "{result}");
    assert!(!path("none.pmtiles").exists());
}

#[test]
fn bad_requests_for_the_new_kinds_fail_to_start() {
    let message = |kind: &str, request: Value| {
        start(kind, request).unwrap_err()["message"]
            .as_str()
            .unwrap()
            .to_string()
    };
    assert!(
        message("update", json!({ "input": "/a", "output": "/nowhere/b" })).contains("changeFiles")
    );
    assert!(
        message(
            "extract",
            json!({ "source": "/a", "output": "/nowhere/b", "zoom": { "min": 3, "max": 2 } })
        )
        .contains("zooms must satisfy")
    );
    assert!(message("apply", json!({ "base": "/a", "output": "/nowhere/b" })).contains("delta"));
    assert!(message("diff", json!({ "base": "/a", "target": "/b" })).contains("output"));
    assert!(message("inspect", json!({ "path": "/a" })).contains("unknown field `path`"));
    assert!(
        message("frobnicate", json!({})).contains("extract, update, diff, apply, inspect, verify")
    );
}
