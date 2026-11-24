//! The request kinds that download a compile's inputs ahead of it, as the
//! command line's `fetch-sources` and `compile --area` do:
//! `fetch_sources` fetches the supplemental datasets, `fetch_extract` a
//! Geofabrik extract by region name.

use crate::archive_jobs::request;
use crate::job::{JobError, Progress, Work};
use osm_tile_compiler::{
    ExtractEvent, FetchEvent, FetchMode, GEOFABRIK_INDEX_URL, SupplementalDataset,
    fetch_extract_from, fetch_sources,
};
use serde::Deserialize;
use serde_json::{Value, json};
use std::path::PathBuf;

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
enum FetchModeJson {
    /// Keep a file whose recorded hash still matches; download the others.
    #[default]
    Verify,
    /// Keep any file present, unchecked; download only the missing ones.
    OnlyMissing,
}

/// `fetch_sources`' request: the sources directory, and optionally which
/// datasets [default: all three] and how to treat files already there.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FetchSourcesJson {
    directory: PathBuf,
    datasets: Option<Vec<String>>,
    #[serde(default)]
    mode: FetchModeJson,
}

fn fetch_sources_request(
    request: Value,
) -> Result<(PathBuf, Vec<SupplementalDataset>, FetchMode), JobError> {
    let json: FetchSourcesJson = self::request("fetch_sources", request)?;
    let datasets = match json.datasets {
        None => SupplementalDataset::ALL.to_vec(),
        Some(ids) => {
            let mut datasets = Vec::new();
            for id in ids {
                let dataset = SupplementalDataset::from_id(&id).ok_or_else(|| {
                    JobError::invalid_request(format!("Unknown supplemental dataset '{id}'"))
                })?;
                if !datasets.contains(&dataset) {
                    datasets.push(dataset);
                }
            }
            // In the order a fetch of all of them takes.
            datasets.sort_by_key(|dataset| {
                SupplementalDataset::ALL
                    .iter()
                    .position(|known| known == dataset)
            });
            datasets
        }
    };
    if datasets.is_empty() {
        return Err(JobError::invalid_request(
            "datasets is empty: leave it out to fetch all three",
        ));
    }
    let mode = match json.mode {
        FetchModeJson::Verify => FetchMode::Verify,
        FetchModeJson::OnlyMissing => FetchMode::OnlyMissing,
    };
    Ok((json.directory, datasets, mode))
}

pub fn fetch_sources_work(request: Value) -> Result<Work, JobError> {
    let (directory, datasets, mode) = fetch_sources_request(request)?;
    Ok(Box::new(move |reporter| {
        // `overall` weighs each dataset by its size, as the compile's
        // fetch_sources stage does: water polygons are most of it.
        let total: u64 = datasets.iter().map(|d| d.approximate_bytes()).sum();
        let before = |dataset: SupplementalDataset| -> u64 {
            datasets
                .iter()
                .take_while(|d| **d != dataset)
                .map(|d| d.approximate_bytes())
                .sum()
        };
        let report = |dataset: SupplementalDataset, message: String, fraction: Option<f64>| {
            let done = before(dataset) as f64
                + fraction.unwrap_or(0.0) * dataset.approximate_bytes() as f64;
            reporter.report(Progress {
                stage: Some("fetch_sources"),
                message,
                fraction,
                overall: Some((done / total.max(1) as f64).min(1.0)),
            })
        };
        let inputs = fetch_sources(
            &directory,
            &datasets,
            mode,
            |event| match event {
                FetchEvent::Checking {
                    dataset,
                    bytes,
                    total,
                } => report(
                    dataset,
                    format!("Verifying {}", dataset.file_name()),
                    Some(bytes as f64 / total.max(1) as f64),
                ),
                FetchEvent::Downloading { dataset, url } => {
                    report(dataset, format!("Downloading {url}"), Some(0.0))
                }
                FetchEvent::Received {
                    dataset,
                    bytes,
                    total,
                } => report(
                    dataset,
                    format!("Downloading {}", dataset.file_name()),
                    // Without a length, the dataset's usual size stands in.
                    Some(
                        (bytes as f64 / total.unwrap_or(dataset.approximate_bytes()).max(1) as f64)
                            .min(1.0),
                    ),
                ),
                FetchEvent::Ready {
                    dataset,
                    downloaded,
                    ..
                } => report(
                    dataset,
                    format!(
                        "{} {}",
                        dataset.file_name(),
                        if downloaded {
                            "downloaded"
                        } else {
                            "up to date"
                        }
                    ),
                    Some(1.0),
                ),
                _ => {}
            },
            || reporter.is_cancelled(),
        )?;
        Ok(json!({
            "directory": directory,
            "datasets": datasets.iter().map(|d| d.id()).collect::<Vec<_>>(),
            "inputs": inputs,
        }))
    }))
}

/// `fetch_extract`'s request: where to download, and the region, as
/// `compile --area` takes it.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct FetchExtractJson {
    directory: PathBuf,
    /// A Geofabrik identifier, name or ISO code: `malopolskie`,
    /// `Małopolskie`, `us/georgia`, `PL`.
    area: String,
    /// Geofabrik's index, or a mirror's [default: `GEOFABRIK_INDEX_URL`].
    index_url: Option<String>,
}

pub fn fetch_extract_work(request: Value) -> Result<Work, JobError> {
    let json: FetchExtractJson = self::request("fetch_extract", request)?;
    if json.area.trim().is_empty() {
        return Err(JobError::invalid_request("area is empty"));
    }
    let index_url = json.index_url.unwrap_or(GEOFABRIK_INDEX_URL.to_string());
    if !index_url.starts_with("https://") && !index_url.starts_with("http://") {
        return Err(JobError::invalid_request(format!(
            "indexUrl must be an http(s) URL, got '{index_url}'"
        )));
    }
    Ok(Box::new(move |reporter| {
        let report = |message: String, fraction: Option<f64>, overall: f64| {
            reporter.report(Progress {
                stage: Some("fetch_extract"),
                message,
                fraction,
                overall: Some(overall),
            })
        };
        let mut found = Value::Null;
        let mut ready = (0, false);
        let path = fetch_extract_from(
            &index_url,
            &json.directory,
            &json.area,
            |event| match event {
                ExtractEvent::Resolving { url } => {
                    report(format!("Looking up '{}' in {url}", json.area), None, 0.0)
                }
                ExtractEvent::Found { extract } => {
                    found = json!({
                        "id": extract.id,
                        "name": extract.name,
                        "parent": extract.parent,
                        "isoCodes": extract.iso_codes,
                        "url": extract.url,
                    });
                    report(format!("Found {}", extract.id), None, 0.0)
                }
                ExtractEvent::Downloading { url } => {
                    report(format!("Downloading {url}"), Some(0.0), 0.0)
                }
                ExtractEvent::Received { bytes, total } => {
                    let fraction = total.map(|total| (bytes as f64 / total.max(1) as f64).min(1.0));
                    report(
                        format!("Downloaded {:.1} MB", bytes as f64 / 1e6),
                        fraction,
                        fraction.unwrap_or(0.0),
                    )
                }
                ExtractEvent::Ready {
                    bytes, downloaded, ..
                } => ready = (bytes, downloaded),
                _ => {}
            },
            || reporter.is_cancelled(),
        )?;
        Ok(json!({
            "path": path,
            "bytes": ready.0,
            "downloaded": ready.1,
            "extract": found,
        }))
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fetch_sources_requests_pick_datasets_and_a_mode() {
        let (directory, datasets, mode) =
            fetch_sources_request(json!({ "directory": "/sources" })).unwrap();
        assert_eq!(directory, PathBuf::from("/sources"));
        assert_eq!(datasets, SupplementalDataset::ALL.to_vec());
        assert_eq!(mode, FetchMode::Verify);
        let (_, datasets, mode) = fetch_sources_request(json!({
            "directory": "/sources",
            "datasets": ["water_polygons", "lake_centerlines", "water_polygons"],
            "mode": "only_missing",
        }))
        .unwrap();
        assert_eq!(
            datasets,
            vec![
                SupplementalDataset::LakeCenterlines,
                SupplementalDataset::WaterPolygons
            ]
        );
        assert_eq!(mode, FetchMode::OnlyMissing);
        let refused = |request: Value| fetch_sources_request(request).unwrap_err().message;
        assert!(
            refused(json!({ "directory": "/s", "datasets": ["coastline"] }))
                .contains("Unknown supplemental dataset 'coastline'")
        );
        assert!(refused(json!({ "directory": "/s", "datasets": [] })).contains("empty"));
        assert!(
            refused(json!({ "directory": "/s", "mode": "always" })).contains("unknown variant")
        );
    }

    #[test]
    fn bad_fetch_extract_requests_fail_at_start() {
        let refused = |request: Value| match fetch_extract_work(request) {
            Ok(_) => panic!("taken"),
            Err(error) => error.message,
        };
        assert!(refused(json!({ "directory": "/d" })).contains("area"));
        assert!(refused(json!({ "directory": "/d", "area": " " })).contains("area is empty"));
        assert!(
            refused(json!({ "directory": "/d", "area": "pl", "indexUrl": "/index.json" }))
                .contains("http(s) URL")
        );
    }
}
