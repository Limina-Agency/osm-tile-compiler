//! The request kinds that work on finished archives, as the command line's
//! `extract`, `diff`, `apply`, `inspect`, `verify` and `verify-signature` do:
//! cut an area out of a PMTiles archive, write or apply a delta between two
//! archives, read an archive whole to report on it or check it, and check a
//! delta's signatures.
//!
//! `diff`, `apply`, `inspect`, `verify` and `verify_delta` have no progress
//! of their own and
//! cannot stop part way, so a cancel takes effect before they start or once
//! they end: a `diff` or `apply` cancelled while it runs then removes what it
//! wrote and ends `cancelled`.

use crate::job::{JobError, Progress, Reporter, Work};
use crate::kinds::{BoundsJson, ZoomJson};
use osm_tile_compiler::archive::{self, ExtractRequest, PublicKey};
use osm_tile_compiler::{CompileArea, ZoomRange};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Parses `kind`'s request, naming the kind in the error.
pub(crate) fn request<T: DeserializeOwned>(kind: &str, request: Value) -> Result<T, JobError> {
    serde_json::from_value(request)
        .map_err(|e| JobError::invalid_request(format!("Invalid {kind} request: {e}")))
}

/// Refuses an output that already exists, as every archive command does.
pub(crate) fn new_output(path: &Path) -> Result<(), JobError> {
    if path.exists() {
        return Err(JobError::invalid_request(format!(
            "refusing to replace existing file {}",
            path.display()
        )));
    }
    Ok(())
}

fn to_json(value: impl serde::Serialize) -> Result<Value, JobError> {
    serde_json::to_value(value).map_err(|e| JobError::internal(e.to_string()))
}

/// Fails with a cancel when one was asked for.
fn not_cancelled(reporter: &Reporter) -> Result<(), JobError> {
    if reporter.is_cancelled() {
        Err(JobError::cancelled())
    } else {
        Ok(())
    }
}

/// One report saying what the job is doing; these kinds tell no fraction.
fn started(reporter: &Reporter, stage: &'static str, message: String) {
    reporter.report(Progress {
        stage: Some(stage),
        message,
        fraction: None,
        overall: Some(0.0),
    });
}

/// `extract`'s request: the archive to cut from, the one to write, and what
/// to keep. With none of `bounds`, `polygon` and `zoom`, every tile is
/// copied.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ExtractJson {
    source: PathBuf,
    output: PathBuf,
    bounds: Option<BoundsJson>,
    /// A GeoJSON or Osmosis `.poly` file.
    polygon: Option<PathBuf>,
    zoom: Option<ZoomJson>,
}

fn extract_request(request: Value) -> Result<(ExtractRequest, Option<PathBuf>), JobError> {
    let json: ExtractJson = self::request("extract", request)?;
    new_output(&json.output)?;
    let mut request = ExtractRequest::new(json.source, json.output);
    request.bounds = json.bounds.map(BoundsJson::checked).transpose()?;
    if let Some(zoom) = json.zoom {
        let max = zoom.max.unwrap_or(ZoomRange::LIMIT);
        let min = zoom.min.unwrap_or(0);
        request.zoom = Some(ZoomRange::new(min, max)?);
    }
    Ok((request, json.polygon))
}

pub fn extract_work(request: Value) -> Result<Work, JobError> {
    let (mut request, polygon) = extract_request(request)?;
    Ok(Box::new(move |reporter| {
        started(
            reporter,
            "extract",
            format!("Cutting {}", request.output.display()),
        );
        // Reading the polygon needs the file, so it fails the job, as a
        // compile's polygon does.
        if let Some(polygon) = polygon {
            request.polygon = Some(CompileArea::read(&polygon)?);
        }
        let report = archive::extract(&request, || reporter.is_cancelled())?;
        to_json(report)
    }))
}

/// `diff`'s request: the old archive, the new one, and the delta to write.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DiffJson {
    base: PathBuf,
    target: PathBuf,
    output: PathBuf,
}

/// `apply`'s request: the old archive, the delta, and the archive to write.
/// With `trusted_keys`, the delta must carry a valid signature by one of them.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ApplyJson {
    base: PathBuf,
    delta: PathBuf,
    output: PathBuf,
    #[serde(default)]
    trusted_keys: Vec<String>,
}

/// `verify_delta`'s request: the delta whose signatures to check, and the
/// keys one of which must have signed it, if any.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct VerifyDeltaJson {
    delta: PathBuf,
    #[serde(default)]
    trusted_keys: Vec<String>,
}

/// Parses `trustedKeys`, as `--trusted-key` does: 64 hex digits, or the path
/// of a file holding them, such as the `.pub` file `keygen` writes.
fn trusted_keys(keys: &[String]) -> Result<Vec<PublicKey>, JobError> {
    keys.iter()
        .map(|key| {
            PublicKey::parse(key)
                .map_err(|e| JobError::invalid_request(format!("trustedKeys: {e}")))
        })
        .collect()
}

/// Runs a delta step that cannot stop part way: a cancel asked for while it
/// ran removes its output once it ends.
fn delta_step<R: serde::Serialize>(
    reporter: &Reporter,
    output: &Path,
    step: impl FnOnce() -> Result<R, osm_tile_compiler::CompileError>,
) -> Result<Value, JobError> {
    not_cancelled(reporter)?;
    let report = step()?;
    if reporter.is_cancelled() {
        // Ours: the job refused to start over an existing file.
        let _ = std::fs::remove_file(output);
        return Err(JobError::cancelled());
    }
    to_json(report)
}

pub fn diff_work(request: Value) -> Result<Work, JobError> {
    let json: DiffJson = self::request("diff", request)?;
    new_output(&json.output)?;
    Ok(Box::new(move |reporter| {
        started(
            reporter,
            "diff",
            format!("Writing the delta {}", json.output.display()),
        );
        delta_step(reporter, &json.output, || {
            archive::diff(&json.base, &json.target, &json.output)
        })
    }))
}

pub fn apply_work(request: Value) -> Result<Work, JobError> {
    let json: ApplyJson = self::request("apply", request)?;
    new_output(&json.output)?;
    let trusted = trusted_keys(&json.trusted_keys)?;
    Ok(Box::new(move |reporter| {
        started(
            reporter,
            "apply",
            format!(
                "Applying {} to {}",
                json.delta.display(),
                json.base.display()
            ),
        );
        delta_step(reporter, &json.output, || {
            archive::apply_trusted(&json.base, &json.delta, &json.output, &trusted)
        })
    }))
}

/// Checks a delta's signatures without applying it, as `verify-signature`
/// does for a delta: every signature must match, and with `trustedKeys` one
/// must be by a trusted key, or the job fails.
pub fn verify_delta_work(request: Value) -> Result<Work, JobError> {
    let json: VerifyDeltaJson = self::request("verify_delta", request)?;
    let trusted = trusted_keys(&json.trusted_keys)?;
    Ok(Box::new(move |reporter| {
        not_cancelled(reporter)?;
        started(
            reporter,
            "verify_delta",
            format!("Checking the signatures of {}", json.delta.display()),
        );
        let verified = archive::verify_delta(&json.delta, &trusted)?;
        to_json(verified)
    }))
}

/// `inspect`'s and `verify`'s request: the archive to read.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchiveJson {
    archive: PathBuf,
}

/// How many problems a failed `verify` names in its message.
const PROBLEMS_IN_MESSAGE: usize = 5;

/// `inspect` reports on the archive, problems and all; `verify` also fails
/// the job when the archive has any problem, as the command line's `verify`
/// exits non-zero.
pub fn archive_report_work(kind: &'static str, request: Value) -> Result<Work, JobError> {
    let json: ArchiveJson = self::request(kind, request)?;
    let verify = kind == "verify";
    Ok(Box::new(move |reporter| {
        not_cancelled(reporter)?;
        started(
            reporter,
            kind,
            format!("Reading {}", json.archive.display()),
        );
        let report = archive::inspect(&json.archive)?;
        not_cancelled(reporter)?;
        if verify && report.problem_count > 0 {
            let mut message = format!(
                "{} has {} problem{}: {}",
                json.archive.display(),
                report.problem_count,
                if report.problem_count == 1 { "" } else { "s" },
                report
                    .problems
                    .iter()
                    .take(PROBLEMS_IN_MESSAGE)
                    .cloned()
                    .collect::<Vec<_>>()
                    .join("; ")
            );
            let unnamed =
                report.problem_count - report.problems.len().min(PROBLEMS_IN_MESSAGE) as u64;
            if unnamed > 0 {
                message.push_str(&format!("; and {unnamed} more"));
            }
            return Err(JobError {
                kind: "failed",
                message,
                stage: None,
            });
        }
        to_json(report)
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn refused(work: Result<Work, JobError>) -> String {
        match work {
            Ok(_) => panic!("the request was taken"),
            Err(error) => {
                assert_eq!(error.kind, "invalid_request");
                error.message
            }
        }
    }

    #[test]
    fn extract_requests_map_onto_extract_request() {
        let (request, polygon) = extract_request(json!({
            "source": "/nowhere/in.pmtiles",
            "output": "/nowhere/out.pmtiles",
            "bounds": { "north": 2.0, "south": 1.0, "east": 4.0, "west": 3.0 },
            "polygon": "/nowhere/area.geojson",
            "zoom": { "max": 12 },
        }))
        .unwrap();
        assert_eq!(request.source, PathBuf::from("/nowhere/in.pmtiles"));
        assert_eq!(request.bounds.unwrap().west, 3.0);
        assert_eq!(request.zoom, Some(ZoomRange::new(0, 12).unwrap()));
        assert_eq!(polygon, Some(PathBuf::from("/nowhere/area.geojson")));
        let (request, _) =
            extract_request(json!({ "source": "/a", "output": "/nowhere/b" })).unwrap();
        assert!(request.bounds.is_none() && request.zoom.is_none());
    }

    #[test]
    fn bad_archive_requests_fail_at_start() {
        let extract = |extra: Value| {
            let mut request = json!({ "source": "/a", "output": "/nowhere/b" });
            request
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            refused(extract_work(request))
        };
        assert!(extract(json!({ "zoom": { "min": 9, "max": 3 } })).contains("zooms must satisfy"));
        assert!(extract(json!({ "zoom": { "max": 17 } })).contains("zooms must satisfy"));
        assert!(
            extract(json!({ "bounds": { "north": 1.0, "south": 2.0, "east": 4.0, "west": 3.0 } }))
                .contains("bounds must")
        );
        assert!(extract(json!({ "tiles": 3 })).contains("unknown field `tiles`"));
        assert!(refused(extract_work(json!({ "source": "/a" }))).contains("output"));
        let existing = std::env::temp_dir();
        assert!(
            refused(extract_work(json!({ "source": "/a", "output": existing })))
                .contains("refusing to replace")
        );
        assert!(
            refused(apply_work(
                json!({ "base": "/a", "delta": "/b", "output": existing })
            ))
            .contains("refusing to replace")
        );
        assert!(
            refused(diff_work(
                json!({ "base": "/a", "delta": "/b", "output": "/c" })
            ))
            .contains("Invalid diff request")
        );
        assert!(refused(archive_report_work("verify", json!({}))).contains("archive"));
    }

    #[test]
    fn a_delta_step_cancelled_while_it_runs_removes_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let output = dir.path().join("out.pmtiles");
        let written = output.clone();
        let job = crate::job::spawn(
            "apply",
            Box::new(move |reporter| {
                delta_step(reporter, &written.clone(), || {
                    std::fs::write(&written, b"archive").unwrap();
                    // The cancel arrives while the step runs.
                    while !reporter.is_cancelled() {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                    Ok(json!({ "written": true }))
                })
            }),
        )
        .unwrap();
        while !output.exists() {
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        job.cancel();
        let state = job.wait(std::time::Duration::from_secs(10));
        assert_eq!(state, crate::job::JobState::Cancelled);
        assert!(!output.exists());

        // Cancelled before it starts, the step never runs.
        let job = crate::job::spawn(
            "apply",
            Box::new(|reporter| {
                while !reporter.is_cancelled() {
                    std::thread::sleep(std::time::Duration::from_millis(5));
                }
                delta_step(reporter, Path::new("/nowhere"), || -> Result<Value, _> {
                    panic!("the step ran")
                })
            }),
        )
        .unwrap();
        job.cancel();
        let state = job.wait(std::time::Duration::from_secs(10));
        assert_eq!(state, crate::job::JobState::Cancelled);
    }
}
