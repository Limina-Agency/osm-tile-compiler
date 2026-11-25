//! `fetch_extract` against a local server playing Geofabrik, `fetch_sources`
//! with the datasets already present, and `otc_validate`.

use osm_tile_compiler_ffi::*;
use serde_json::{Value, json};
use std::ffi::{CStr, CString, c_char};
use std::io::{BufRead, BufReader, Write};
use std::net::TcpListener;
use std::path::Path;
use std::ptr::null_mut;

fn fixture(name: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../fixtures")
        .join(name)
}

fn take(string: *mut c_char) -> Value {
    assert!(!string.is_null());
    let value = serde_json::from_str(unsafe { CStr::from_ptr(string) }.to_str().unwrap()).unwrap();
    unsafe { otc_string_free(string) };
    value
}

fn envelope(kind: &str, request: Value) -> CString {
    let envelope = json!({ "abiVersion": ABI_VERSION, "kind": kind, "request": request });
    CString::new(envelope.to_string()).unwrap()
}

/// Runs a job to its end and returns its result and its last poll.
fn run(kind: &str, request: Value) -> (Value, Value) {
    let text = envelope(kind, request);
    let (mut job, mut error) = (null_mut(), null_mut());
    let status = unsafe { otc_job_start(text.as_ptr(), &mut job, &mut error) };
    assert_eq!(status, OTC_OK, "{kind}: {}", take(error));
    assert_ne!(
        unsafe { otc_job_wait(job, 60_000) },
        0,
        "{kind} still running"
    );
    let poll = take(unsafe { otc_job_poll(job) });
    let mut out = null_mut();
    assert_eq!(unsafe { otc_job_result(job, &mut out) }, OTC_OK);
    unsafe { otc_job_free(job) };
    (take(out), poll)
}

/// Serves Geofabrik's recorded index, its URLs pointed here, and Monaco's
/// extract; anything else is 404. Returns the index's URL.
fn geofabrik() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let index = std::fs::read_to_string(fixture("geofabrik-index-v1-nogeom.json"))
        .unwrap()
        .replace("https://download.geofabrik.de", &base);
    let pbf = std::fs::read(fixture("monaco.osm.pbf")).unwrap();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut request_line = String::new();
            let _ = reader.read_line(&mut request_line);
            loop {
                let mut header = String::new();
                if reader.read_line(&mut header).unwrap_or(0) == 0 || header == "\r\n" {
                    break;
                }
            }
            let path = request_line.split_whitespace().nth(1).unwrap_or_default();
            let (status, body) = match path {
                "/index-v1-nogeom.json" => ("200 OK", index.as_bytes()),
                "/europe/monaco/monaco-latest.osm.pbf" => ("200 OK", pbf.as_slice()),
                _ => ("404 Not Found", b"not found".as_slice()),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            let _ = stream.write_all(body);
        }
    });
    format!("{base}/index-v1-nogeom.json")
}

#[test]
fn fetch_extract_downloads_a_region_by_name() {
    let index = geofabrik();
    let dir = tempfile::tempdir().unwrap();
    let downloads = dir.path().join("downloads");
    let request = json!({ "directory": downloads, "area": "Monaco", "indexUrl": index });
    let (result, poll) = run("fetch_extract", request.clone());
    assert_eq!(result["ok"], true, "{result}");
    let fetched = &result["result"];
    let path = downloads.join("monaco.osm.pbf");
    assert_eq!(fetched["path"], json!(path));
    assert_eq!(fetched["downloaded"], true);
    assert_eq!(fetched["extract"]["id"], "monaco");
    assert_eq!(
        std::fs::read(&path).unwrap(),
        std::fs::read(fixture("monaco.osm.pbf")).unwrap()
    );
    assert_eq!(fetched["bytes"], std::fs::metadata(&path).unwrap().len());
    assert_eq!(poll["stage"], "fetch_extract");
    assert_eq!(poll["overall"], 1.0);

    // Already there: kept as it is.
    let (result, _) = run("fetch_extract", request);
    assert_eq!(result["result"]["downloaded"], false, "{result}");

    // A name the index lacks fails the job, and downloads nothing.
    let (result, _) = run(
        "fetch_extract",
        json!({ "directory": dir.path().join("none"), "area": "atlantis", "indexUrl": index }),
    );
    assert_eq!(result["ok"], false, "{result}");
    assert!(
        result["error"]["message"]
            .as_str()
            .unwrap()
            .contains("atlantis")
    );
    assert!(std::fs::read_dir(dir.path().join("none")).map_or(true, |mut d| d.next().is_none()));
}

#[test]
fn fetch_sources_keeps_datasets_already_present() {
    let dir = tempfile::tempdir().unwrap();
    for file in ["lake_centerline.shp.zip", "natural_earth_vector.sqlite.zip"] {
        std::fs::write(dir.path().join(file), b"present").unwrap();
    }
    let (result, poll) = run(
        "fetch_sources",
        json!({
            "directory": dir.path(),
            "datasets": ["natural_earth", "lake_centerlines"],
            "mode": "only_missing",
        }),
    );
    assert_eq!(result["ok"], true, "{result}");
    // In fetch order, whatever order the request gave.
    assert_eq!(
        result["result"]["datasets"],
        json!(["lake_centerlines", "natural_earth"])
    );
    // Files kept unchecked carry no provenance.
    assert_eq!(result["result"]["inputs"], json!([]), "{result}");
    assert_eq!(poll["stage"], "fetch_sources");
    assert_eq!(poll["overall"], 1.0);
}

#[test]
fn otc_validate_checks_without_starting() {
    let validate = |kind: &str, request: Value| {
        let text = envelope(kind, request);
        let mut error = null_mut();
        let status = unsafe { otc_validate(text.as_ptr(), &mut error) };
        let error = (!error.is_null()).then(|| take(error));
        (status, error)
    };
    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.pmtiles");
    assert_eq!(
        validate(
            "extract",
            json!({ "source": "/a.pmtiles", "output": output })
        ),
        (OTC_OK, None)
    );
    // Nothing ran: no output, no job.
    assert!(!output.exists());
    let (status, error) = validate(
        "extract",
        json!({ "source": "/a", "output": output, "zoom": { "min": 9, "max": 3 } }),
    );
    assert_eq!(status, OTC_ERROR_INVALID_REQUEST);
    let error = error.unwrap();
    assert_eq!(error["kind"], "invalid_request");
    assert!(
        error["message"].as_str().unwrap().contains("zooms"),
        "{error}"
    );
    let (status, _) = validate("nope", json!({}));
    assert_eq!(status, OTC_ERROR_INVALID_REQUEST);
    assert_eq!(
        unsafe { otc_validate(std::ptr::null(), null_mut()) },
        OTC_ERROR_INVALID_ARGUMENT
    );
}
