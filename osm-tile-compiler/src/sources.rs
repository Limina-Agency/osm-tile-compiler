//! The supplemental datasets the OpenMapTiles layers draw on besides the
//! OSM extract, and their download.
//!
//! Downloads stream to disk; only completed files are published. Inputs with
//! a pinned SHA-256 must match it; the mutable upstream input keeps its
//! actual SHA-256 as provenance.
//!
//! A compile can leave any of them out ([`crate::BasemapRequest::omit_sources`]);
//! [`SupplementalDataset::without_warning`] says what the map then lacks.
use crate::control::{CANCELLED, CompileError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    fs::{self, File},
    io::{Read, Write},
    path::Path,
    time::{Duration, Instant},
};

/// One of the three supplemental datasets.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SupplementalDataset {
    /// OSM-derived ocean polygons from osmdata.openstreetmap.de, about 1 GB.
    WaterPolygons,
    /// Natural Earth's vector data as SQLite, 434 MB.
    NaturalEarth,
    /// Lake centerlines (osm-lakelines), 81 MB.
    LakeCenterlines,
}

impl SupplementalDataset {
    /// All three, in the order they are fetched.
    pub const ALL: [Self; 3] = [
        Self::LakeCenterlines,
        Self::NaturalEarth,
        Self::WaterPolygons,
    ];

    /// A stable identifier: `water_polygons`, `natural_earth` or
    /// `lake_centerlines`. The archive's metadata and the compile's result
    /// list omitted datasets by it.
    pub fn id(self) -> &'static str {
        match self {
            Self::WaterPolygons => "water_polygons",
            Self::NaturalEarth => "natural_earth",
            Self::LakeCenterlines => "lake_centerlines",
        }
    }

    /// The dataset with this [`id`](Self::id).
    pub fn from_id(id: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|dataset| dataset.id() == id)
    }

    /// Its archive's name in a sources directory.
    pub fn file_name(self) -> &'static str {
        match self {
            Self::WaterPolygons => "water-polygons-split-3857.zip",
            Self::NaturalEarth => "natural_earth_vector.sqlite.zip",
            Self::LakeCenterlines => "lake_centerline.shp.zip",
        }
    }

    /// Where it is downloaded from.
    pub fn url(self) -> &'static str {
        match self {
            Self::WaterPolygons => {
                "https://osmdata.openstreetmap.de/download/water-polygons-split-3857.zip"
            }
            Self::NaturalEarth => {
                "https://naciscdn.org/naturalearth/packages/natural_earth_vector.sqlite.zip"
            }
            Self::LakeCenterlines => {
                "https://github.com/acalcutt/osm-lakelines/releases/download/v12/lake_centerline.shp.zip"
            }
        }
    }

    /// The SHA-256 its download must have. The lake centerlines (a release
    /// asset) and Natural Earth have not changed since 2022, and their
    /// hashes match the inputs of the Planetiler reference runs in
    /// `benchmarks/compiler-parity/`. The water polygons are regenerated
    /// daily, so they cannot be pinned.
    pub fn pinned_sha256(self) -> Option<&'static str> {
        match self {
            Self::WaterPolygons => None,
            Self::NaturalEarth => {
                Some("375da61836d4779dffa8b87887bc4faa94dac77745ba0ee3914bd7cbedf40a02")
            }
            Self::LakeCenterlines => {
                Some("6c900507c88fc9f5b5a386f90fd0a42d0495e8755a03d075538fb9a6801a3192")
            }
        }
    }

    /// The download's approximate size in bytes, as of November 2026. The
    /// first compile that reads a dataset also extracts it beside the
    /// download (`<sources>/.extracted/`), which takes about twice as much
    /// again; later compiles reuse it.
    pub fn approximate_bytes(self) -> u64 {
        match self {
            Self::WaterPolygons => 1_000_000_000,
            Self::NaturalEarth => 434_210_731,
            Self::LakeCenterlines => 80_906_805,
        }
    }

    /// What a map compiled without this dataset lacks, in a sentence an app
    /// can show as it is. Everything the OSM extract itself holds (lakes,
    /// rivers, roads, buildings, places, borders from zoom 5) is unaffected.
    ///
    /// - Water polygons: OpenMapTiles takes the sea from them, not from the
    ///   OSM coastline, so from zoom 6 in there is no sea at all.
    /// - Natural Earth: the only source of zooms 0 to 5 for oceans, lakes,
    ///   rivers, glaciers, urban areas and country and state borders (0 to
    ///   4), and of the ranks that decide from which zoom country, state,
    ///   city and sea labels show. Without it, state and province labels are
    ///   left out, cities show only from their OSM default zooms, and seas
    ///   are named from zoom 8, bays from 13.
    /// - Lake centerlines: large lakes are named along a line through them;
    ///   without it, each lake is named at one point inside it.
    pub fn without_warning(self) -> &'static str {
        match self {
            Self::WaterPolygons => {
                "No seas or oceans from zoom 6 in: the sea is drawn as land, so coastlines \
                 and islands do not show. Lakes and rivers are not affected."
            }
            Self::NaturalEarth => {
                "Little to see when zoomed out (zooms 0 to 5): no oceans, large lakes, \
                 rivers, glaciers or country and state borders there. State and province \
                 names are missing, and city and sea names appear only when zoomed further in."
            }
            Self::LakeCenterlines => {
                "Lake names are placed at one point in each lake instead of along it, \
                 which suits long, narrow lakes less well."
            }
        }
    }

    /// Whether its archive is in `directory` (unchecked).
    pub fn is_present(self, directory: &Path) -> bool {
        directory.join(self.file_name()).is_file()
    }
}

/// Checks a downloaded file's hash against its pin, if it has one.
fn check_pin(dataset: SupplementalDataset, sha256: &str) -> Result<(), String> {
    match dataset.pinned_sha256() {
        Some(pinned) if pinned != sha256 => Err(format!(
            "{} from {} has SHA-256 {sha256}, but {pinned} is pinned: the upstream \
             file has changed or the download is corrupt",
            dataset.file_name(),
            dataset.url()
        )),
        _ => Ok(()),
    }
}

/// Where a supplemental dataset in a compile came from: its record beside
/// the download, `<file>.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedInput {
    pub file: String,
    pub url: String,
    pub bytes: u64,
    pub sha256: String,
}

/// Copies `reader` to `output`, hashing it; `progress` gets the bytes so far
/// after every read, and an error stops the copy.
pub(crate) fn transfer(
    mut reader: impl Read,
    mut output: impl Write,
    expected: Option<u64>,
    mut progress: impl FnMut(u64) -> Result<(), String>,
) -> Result<(u64, String), String> {
    let mut buffer = vec![0; 256 * 1024];
    let mut hash = Sha256::new();
    let mut bytes = 0;
    loop {
        let count = reader.read(&mut buffer).map_err(|e| e.to_string())?;
        if count == 0 {
            break;
        }
        output
            .write_all(&buffer[..count])
            .map_err(|e| e.to_string())?;
        hash.update(&buffer[..count]);
        bytes += count as u64;
        progress(bytes)?;
    }
    if bytes == 0 || expected.is_some_and(|length| length != bytes) {
        return Err("The download is empty or incomplete; retry".into());
    }
    Ok((bytes, hex::encode(hash.finalize())))
}

/// What [`fetch_sources`] is doing, for progress reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FetchEvent<'a> {
    /// Verifying a cached file against its recorded hash: `bytes` of
    /// `total` read, at most every 250 ms after the first.
    Checking {
        dataset: SupplementalDataset,
        bytes: u64,
        total: u64,
    },
    /// Starting a download from `url`.
    Downloading {
        dataset: SupplementalDataset,
        url: &'a str,
    },
    /// `bytes` received so far, at most every 250 ms.
    Received {
        dataset: SupplementalDataset,
        bytes: u64,
        total: Option<u64>,
    },
    /// The dataset is in place: freshly downloaded, or already present.
    Ready {
        dataset: SupplementalDataset,
        bytes: u64,
        downloaded: bool,
    },
}

/// How [`fetch_sources`] treats files already in the directory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FetchMode {
    /// Keep a file whose recorded hash still matches; download the others.
    Verify,
    /// Keep any file present, unchecked and left out of the returned
    /// provenance; download only the missing ones.
    OnlyMissing,
}

/// Downloads `datasets` into `directory`, as `mode` says, and returns where
/// each came from. `should_cancel` is asked after every read of 256 KB, so a
/// cancel takes effect at once while data flows; a stalled connection
/// delays it until data arrives or the connection times out. A cancelled or
/// failed download leaves no partial file; complete files stay.
pub fn fetch_sources(
    directory: &Path,
    datasets: &[SupplementalDataset],
    mode: FetchMode,
    on_event: impl FnMut(FetchEvent),
    should_cancel: impl Fn() -> bool,
) -> Result<Vec<CachedInput>, CompileError> {
    fetch(directory, datasets, mode, on_event, &should_cancel).map_err(|error| {
        if should_cancel() || error == CANCELLED {
            CompileError::Cancelled
        } else {
            CompileError::classify(error, Some(crate::CompileStage::FetchSources))
        }
    })
}

/// The HTTP client downloads use: a connection must open within 30 s and
/// answer within 60 s, and a download may take two hours.
pub(crate) fn agent() -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_connect(Some(Duration::from_secs(30)))
        .timeout_recv_response(Some(Duration::from_secs(60)))
        .timeout_global(Some(Duration::from_secs(7200)))
        .build()
        .into()
}

pub(crate) fn fetch(
    directory: &Path,
    datasets: &[SupplementalDataset],
    mode: FetchMode,
    mut on_event: impl FnMut(FetchEvent),
    should_cancel: &dyn Fn() -> bool,
) -> Result<Vec<CachedInput>, String> {
    let cancelled = || -> Result<(), String> {
        if should_cancel() {
            Err(CANCELLED.into())
        } else {
            Ok(())
        }
    };
    fs::create_dir_all(directory).map_err(|e| e.to_string())?;
    let agent = agent();
    let mut inputs = Vec::new();
    for dataset in SupplementalDataset::ALL {
        if !datasets.contains(&dataset) {
            continue;
        }
        cancelled()?;
        let (file, url) = (dataset.file_name(), dataset.url());
        let destination = directory.join(file);
        let record = directory.join(format!("{file}.json"));
        if mode == FetchMode::OnlyMissing
            && let Ok(metadata) = fs::metadata(&destination)
        {
            on_event(FetchEvent::Ready {
                dataset,
                bytes: metadata.len(),
                downloaded: false,
            });
            continue;
        }
        let cached = fs::read(&record)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<CachedInput>(&bytes).ok())
            .filter(|cached| cached.url == url && cached.file == file);
        if let Some(cached) = cached
            && let Ok(input) = File::open(&destination)
        {
            on_event(FetchEvent::Checking {
                dataset,
                bytes: 0,
                total: cached.bytes,
            });
            let mut last = Instant::now();
            let checked = transfer(input, std::io::sink(), Some(cached.bytes), |bytes| {
                cancelled()?;
                if last.elapsed() >= Duration::from_millis(250) {
                    last = Instant::now();
                    on_event(FetchEvent::Checking {
                        dataset,
                        bytes,
                        total: cached.bytes,
                    });
                }
                Ok(())
            });
            cancelled()?;
            if let Ok((_bytes, hash)) = checked
                && hash == cached.sha256
                && check_pin(dataset, &hash).is_ok()
            {
                on_event(FetchEvent::Ready {
                    dataset,
                    bytes: cached.bytes,
                    downloaded: false,
                });
                inputs.push(cached);
                continue;
            }
        }
        on_event(FetchEvent::Downloading { dataset, url });
        let mut response = agent
            .get(url)
            .call()
            .map_err(|e| format!("Could not download {file}: {e}"))?;
        cancelled()?;
        let expected = response
            .headers()
            .get("content-length")
            .and_then(|v| v.to_str().ok())
            .and_then(|v| v.parse::<u64>().ok());
        let partial = directory.join(format!("{file}.part"));
        let mut output = File::create(&partial).map_err(|e| e.to_string())?;
        let mut last = Instant::now() - Duration::from_secs(1);
        let received = transfer(
            response.body_mut().as_reader(),
            &mut output,
            expected,
            |bytes| {
                cancelled()?;
                if last.elapsed() >= Duration::from_millis(250) {
                    last = Instant::now();
                    on_event(FetchEvent::Received {
                        dataset,
                        bytes,
                        total: expected,
                    });
                }
                Ok(())
            },
        )
        .and_then(|received| {
            output.sync_all().map_err(|e| e.to_string())?;
            check_pin(dataset, &received.1)?;
            Ok(received)
        });
        drop(output);
        let (bytes, sha256) = match received {
            Ok(received) => received,
            Err(error) => {
                let _ = fs::remove_file(&partial);
                return Err(error);
            }
        };
        fs::rename(partial, &destination).map_err(|e| e.to_string())?;
        let input = CachedInput {
            file: file.into(),
            url: url.into(),
            bytes,
            sha256,
        };
        let temporary_record = record.with_extension("json.part");
        fs::write(
            &temporary_record,
            serde_json::to_vec_pretty(&input).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        fs::rename(temporary_record, record).map_err(|e| e.to_string())?;
        on_event(FetchEvent::Ready {
            dataset,
            bytes,
            downloaded: true,
        });
        inputs.push(input);
    }
    Ok(inputs)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn rejects_truncated_downloads_and_records_content_hash() {
        let ok = |_| Ok(());
        assert!(transfer(&b"short"[..], std::io::sink(), Some(20), ok).is_err());
        assert!(transfer(&b""[..], std::io::sink(), None, ok).is_err());
        let mut output = Vec::new();
        let (bytes, hash) = transfer(&b"abc"[..], &mut output, Some(3), ok).unwrap();
        assert_eq!(output, b"abc");
        assert_eq!(bytes, 3);
        assert_eq!(
            hash,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let stopped = transfer(&b"abc"[..], std::io::sink(), None, |_| {
            Err(CANCELLED.to_string())
        });
        assert_eq!(stopped, Err(CANCELLED.to_string()));
    }

    #[test]
    fn pinned_sources_reject_other_content() {
        let dataset = SupplementalDataset::LakeCenterlines;
        let pin = dataset.pinned_sha256().unwrap();
        assert!(check_pin(dataset, pin).is_ok());
        let error = check_pin(dataset, "00").unwrap_err();
        assert!(error.contains(pin) && error.contains("changed"), "{error}");
        assert!(check_pin(SupplementalDataset::WaterPolygons, "anything").is_ok());
    }

    #[test]
    fn ids_round_trip() {
        for dataset in SupplementalDataset::ALL {
            assert_eq!(SupplementalDataset::from_id(dataset.id()), Some(dataset));
        }
        assert_eq!(SupplementalDataset::from_id("coastlines"), None);
    }

    #[test]
    fn a_cancelled_fetch_leaves_the_directory_as_it_was() {
        let temp = tempfile::tempdir().unwrap();
        let error = fetch_sources(
            temp.path(),
            &SupplementalDataset::ALL,
            FetchMode::Verify,
            |_| {},
            || true,
        )
        .unwrap_err();
        assert_eq!(error, CompileError::Cancelled);
        assert_eq!(fs::read_dir(temp.path()).unwrap().count(), 0);
    }
}
