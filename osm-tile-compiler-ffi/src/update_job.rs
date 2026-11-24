//! The `update` request kind: brings an extract up to date with OSM change
//! files and replication diffs, as the command line's `update` does, through
//! `osm_tile_compiler::update_extract`.

use crate::archive_jobs::{new_output, request};
use crate::job::{JobError, Progress, Work};
use crate::kinds::BoundsJson;
use osm_tile_compiler::{
    ClipTo, DEFAULT_MAX_DIFFS, Replication, UpdateEvent, UpdateRequest, update_extract,
};
use serde::Deserialize;
use serde_json::Value;
use std::path::PathBuf;

/// `update`'s request. `input` and `output` are required, and at least one
/// of `changeFiles` and `replication`.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct UpdateJson {
    input: PathBuf,
    output: PathBuf,
    /// `.osc` or `.osc.gz` files, applied in order before any diffs.
    #[serde(default)]
    change_files: Vec<PathBuf>,
    /// `header`, `minute`, `hour`, `day` or a replication server's URL.
    replication: Option<String>,
    /// The last diff to download [default: the server's latest].
    until: Option<u64>,
    /// The most diffs to download [default: `DEFAULT_MAX_DIFFS`].
    max_diffs: Option<u64>,
    /// Keep only the changes in this box [default: the input header's box].
    bounds: Option<BoundsJson>,
    /// Keep every change, for diffs already cut to the extract.
    #[serde(default)]
    no_clip: bool,
    threads: Option<usize>,
}

fn update_request(request: Value) -> Result<UpdateRequest, JobError> {
    let json: UpdateJson = self::request("update", request)?;
    let invalid = |message: &str| Err(JobError::invalid_request(message));
    if json.change_files.is_empty() && json.replication.is_none() {
        return invalid("update needs changeFiles or replication");
    }
    if json.replication.is_none() && (json.until.is_some() || json.max_diffs.is_some()) {
        return invalid("until and maxDiffs need replication");
    }
    if json.bounds.is_some() && json.no_clip {
        return invalid("bounds and noClip cannot be combined");
    }
    if json.threads == Some(0) {
        return invalid("threads must be positive");
    }
    new_output(&json.output)?;
    let mut request = UpdateRequest::new(json.input, json.output);
    request.change_files = json.change_files;
    request.replication = json
        .replication
        .map(|source| Replication::parse(&source))
        .transpose()
        .map_err(JobError::invalid_request)?;
    request.until = json.until;
    request.max_diffs = json.max_diffs.unwrap_or(DEFAULT_MAX_DIFFS);
    request.clip = match (json.bounds, json.no_clip) {
        (Some(bounds), _) => ClipTo::Bounds(bounds.checked()?),
        (None, true) => ClipTo::Nothing,
        (None, false) => ClipTo::HeaderBounds { fallback: None },
    };
    request.threads = json.threads.unwrap_or(request.threads);
    Ok(request)
}

pub fn update_work(request: Value) -> Result<Work, JobError> {
    let request = update_request(request)?;
    Ok(Box::new(move |reporter| {
        let report = |stage, message, fraction: Option<f64>, overall| {
            reporter.report(Progress {
                stage: Some(stage),
                message,
                fraction,
                overall: Some(overall),
            })
        };
        report(
            "read_input",
            format!("Reading {}", request.input.display()),
            None,
            0.0,
        );
        // Reading the input takes about a tenth of an update, the changes
        // most of the rest, and writing the output what remains.
        let report = update_extract(
            &request,
            |event| match event {
                UpdateEvent::Scanned { objects, .. } => report(
                    "read_changes",
                    format!("Read {objects} objects"),
                    Some(0.0),
                    0.1,
                ),
                UpdateEvent::Replication {
                    url, first, last, ..
                } => report(
                    "read_changes",
                    if first > last {
                        format!("{url} has nothing newer than the input")
                    } else {
                        format!("Downloading diffs {first} to {last} from {url}")
                    },
                    Some(0.0),
                    0.1,
                ),
                UpdateEvent::Read {
                    name,
                    index,
                    total,
                    report: read,
                } => {
                    let fraction = (index + 1) as f64 / total.max(1) as f64;
                    report(
                        "read_changes",
                        format!(
                            "[{}/{total}] {name}: kept {} of {} changes",
                            index + 1,
                            read.kept,
                            read.objects
                        ),
                        Some(fraction),
                        0.1 + 0.7 * fraction,
                    )
                }
                UpdateEvent::Applying { changes } => report(
                    "write_output",
                    format!("Applying {changes} changes"),
                    None,
                    0.8,
                ),
            },
            || reporter.is_cancelled(),
        )?;
        // The same report as the command line's `update --json`.
        serde_json::to_value(report).map_err(|e| JobError::internal(e.to_string()))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn refused(request: Value) -> String {
        update_request(request).unwrap_err().message
    }

    #[test]
    fn update_requests_map_onto_update_request() {
        let request = update_request(json!({
            "input": "/in.osm.pbf",
            "output": "/nowhere/out.osm.pbf",
            "changeFiles": ["/a.osc", "/b.osc.gz"],
            "replication": "hour",
            "until": 7,
            "maxDiffs": 20,
            "threads": 3,
        }))
        .unwrap();
        assert_eq!(request.change_files.len(), 2);
        assert_eq!(
            request.replication,
            Some(Replication::Url(
                "https://planet.openstreetmap.org/replication/hour".into()
            ))
        );
        assert_eq!(request.until, Some(7));
        assert_eq!(request.max_diffs, 20);
        assert_eq!(request.threads, 3);
        assert_eq!(request.clip, ClipTo::HeaderBounds { fallback: None });

        let request = update_request(json!({
            "input": "/in", "output": "/nowhere/out", "replication": "header", "noClip": true,
        }))
        .unwrap();
        assert_eq!(request.replication, Some(Replication::Header));
        assert_eq!(request.max_diffs, DEFAULT_MAX_DIFFS);
        assert_eq!(request.clip, ClipTo::Nothing);

        let bounds = json!({ "north": 2.0, "south": 1.0, "east": 4.0, "west": 3.0 });
        let request = update_request(json!({
            "input": "/in", "output": "/nowhere/out", "changeFiles": ["/a.osc"], "bounds": bounds,
        }))
        .unwrap();
        assert!(matches!(request.clip, ClipTo::Bounds(b) if b.west == 3.0));
    }

    #[test]
    fn bad_update_requests_fail_at_start() {
        let base = |extra: Value| {
            let mut request = json!({ "input": "/in", "output": "/nowhere/out" });
            request
                .as_object_mut()
                .unwrap()
                .extend(extra.as_object().unwrap().clone());
            refused(request)
        };
        assert!(base(json!({})).contains("changeFiles or replication"));
        assert!(base(json!({ "changeFiles": ["/a"], "until": 3 })).contains("need replication"));
        assert!(base(json!({ "replication": "weekly" })).contains("unknown replication source"));
        assert!(base(json!({ "replication": "day", "threads": 0 })).contains("threads"));
        assert!(
            base(json!({
                "replication": "day",
                "noClip": true,
                "bounds": { "north": 2.0, "south": 1.0, "east": 4.0, "west": 3.0 },
            }))
            .contains("cannot be combined")
        );
        assert!(base(json!({ "changes": ["/a"] })).contains("unknown field `changes`"));
        assert!(
            refused(json!({
                "input": "/in", "output": std::env::temp_dir(), "replication": "day",
            }))
            .contains("refusing to replace")
        );
    }
}
