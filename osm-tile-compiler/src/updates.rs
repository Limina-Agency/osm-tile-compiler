//! Experimental: bringing an extract up to date with OSM change files
//! (`.osc`, `.osc.gz`), read from disk or downloaded from a replication
//! server, without a database. [`update_extract`] writes the updated PBF,
//! which compiles like any other; `diff` then turns the old and new archives
//! into a delta.
//!
//! Only the PBF blocks a change touches are decoded and written again
//! (`osm_pbf_ingest::changes`), so bringing the Małopolskie extract forward by
//! 13 hours of planet diffs takes about 20 seconds on 4 cores, most of it
//! downloading and reading the diffs; the compile itself takes 30.
//!
//! Planet diffs hold the world's edits; by default they are clipped to the
//! extract: see `osm_pbf_ingest::changes::Clip` for what is kept.

use crate::sources::agent;
use crate::{CompileBounds, CompileError};
use osm_pbf_ingest::blob_index::PbfHeaderBounds;
use osm_pbf_ingest::changes::{
    ApplyOptions, ApplyReport, BaseIndex, ChangeSet, Clip, ReadReport, apply_changes,
};
use serde::Serialize;
use std::path::PathBuf;
use std::time::Instant;

/// The planet replication servers, by the names `--replication` takes.
pub const PLANET_REPLICATION: [(&str, &str); 3] = [
    (
        "minute",
        "https://planet.openstreetmap.org/replication/minute",
    ),
    ("hour", "https://planet.openstreetmap.org/replication/hour"),
    ("day", "https://planet.openstreetmap.org/replication/day"),
];

/// Diffs downloaded at most in one update, unless the request says otherwise:
/// a day and a half of minutely diffs.
pub const DEFAULT_MAX_DIFFS: u64 = 2000;

/// Where the changes come from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replication {
    /// The server named in the PBF's header (Geofabrik extracts name their
    /// `-updates` server), continuing from the header's sequence number.
    Header,
    /// A replication server's base URL, holding `state.txt` and
    /// `AAA/BBB/CCC.osc.gz`.
    Url(String),
}

impl Replication {
    /// `header`, `minute`, `hour`, `day` or an `http(s)://` URL.
    pub fn parse(text: &str) -> Result<Self, String> {
        if text == "header" {
            return Ok(Self::Header);
        }
        if let Some((_, url)) = PLANET_REPLICATION.iter().find(|(name, _)| *name == text) {
            return Ok(Self::Url((*url).to_string()));
        }
        if text.starts_with("https://") || text.starts_with("http://") {
            return Ok(Self::Url(text.trim_end_matches('/').to_string()));
        }
        Err(format!(
            "unknown replication source '{text}': give header, minute, hour, day or a URL"
        ))
    }
}

/// Which changes to keep.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum ClipTo {
    /// The box in the PBF's header, or, when it has none, `fallback`.
    HeaderBounds {
        fallback: Option<CompileBounds>,
    },
    Bounds(CompileBounds),
    /// Every change: for diffs already cut to the extract's region.
    Nothing,
}

#[derive(Debug, Clone)]
pub struct UpdateRequest {
    pub input: PathBuf,
    /// Must not exist yet.
    pub output: PathBuf,
    /// Applied in order, before any downloaded diffs.
    pub change_files: Vec<PathBuf>,
    pub replication: Option<Replication>,
    /// The last sequence number to download [default: the server's latest].
    pub until: Option<u64>,
    pub max_diffs: u64,
    pub clip: ClipTo,
    pub threads: usize,
}

impl UpdateRequest {
    pub fn new(input: impl Into<PathBuf>, output: impl Into<PathBuf>) -> Self {
        Self {
            input: input.into(),
            output: output.into(),
            change_files: Vec::new(),
            replication: None,
            until: None,
            max_diffs: DEFAULT_MAX_DIFFS,
            clip: ClipTo::HeaderBounds { fallback: None },
            threads: std::thread::available_parallelism().map_or(4, |n| n.get()),
        }
    }
}

/// What [`update_extract`] reports as it goes.
#[derive(Debug, Clone)]
pub enum UpdateEvent<'a> {
    /// The input was read: its objects, and whether it is sorted.
    Scanned { objects: usize, sorted: bool },
    /// The replication server is at `latest`; diffs `first..=last` are needed
    /// (none when `first > last`).
    Replication {
        url: &'a str,
        latest: u64,
        first: u64,
        last: u64,
    },
    /// One change file or diff was read.
    Read {
        name: &'a str,
        index: usize,
        total: usize,
        report: ReadReport,
    },
    /// Writing the updated PBF.
    Applying { changes: usize },
}

#[derive(Debug, Clone, Serialize)]
pub struct UpdateReport {
    pub input: PathBuf,
    pub output: PathBuf,
    pub change_files: usize,
    /// The replication server's base URL, when diffs were downloaded.
    pub replication_url: Option<String>,
    /// The diffs applied, first and last sequence number.
    pub diffs: Option<[u64; 2]>,
    /// The header's replication timestamp and sequence number after the
    /// update, in seconds since 1970.
    pub replication_timestamp: Option<i64>,
    pub replication_sequence_number: Option<i64>,
    /// Changes read, and kept by the clip.
    pub changes_read: usize,
    pub changes_kept: usize,
    pub blobs_copied: usize,
    pub blobs_rewritten: usize,
    pub blobs_added: usize,
    pub created: usize,
    pub modified: usize,
    pub deleted: usize,
    pub output_bytes: u64,
    pub seconds: f64,
}

/// Write `request.input` with the change files and downloaded diffs applied
/// to `request.output`. Diffs are downloaded one at a time and not kept.
pub fn update_extract(
    request: &UpdateRequest,
    mut on_event: impl FnMut(UpdateEvent<'_>),
    should_cancel: impl Fn() -> bool,
) -> Result<UpdateReport, CompileError> {
    let started = Instant::now();
    if request.output.exists() {
        return Err(CompileError::InvalidRequest(format!(
            "refusing to replace existing file {}",
            request.output.display()
        )));
    }
    if request.change_files.is_empty() && request.replication.is_none() {
        return Err(CompileError::InvalidRequest(
            "no change files and no replication server given".to_string(),
        ));
    }
    for file in &request.change_files {
        if !file.is_file() {
            return Err(CompileError::InvalidRequest(format!(
                "change file {} does not exist",
                file.display()
            )));
        }
    }
    let pool = rayon::ThreadPoolBuilder::new()
        .num_threads(request.threads.max(1))
        .build()
        .map_err(|error| CompileError::Failed {
            stage: None,
            message: error.to_string(),
        })?;
    update(request, &pool, &mut on_event, &should_cancel, started)
}

fn update(
    request: &UpdateRequest,
    pool: &rayon::ThreadPool,
    on_event: &mut impl FnMut(UpdateEvent<'_>),
    should_cancel: &impl Fn() -> bool,
    started: Instant,
) -> Result<UpdateReport, CompileError> {
    let cancelled = || {
        if should_cancel() {
            Err(CompileError::Cancelled)
        } else {
            Ok(())
        }
    };
    let clipping = request.clip != ClipTo::Nothing;
    let base = pool
        .install(|| BaseIndex::scan(&request.input, clipping))
        .map_err(|message| {
            CompileError::InvalidRequest(format!("{}: {message}", request.input.display()))
        })?;
    on_event(UpdateEvent::Scanned {
        objects: base.object_count(),
        sorted: base.is_sorted(),
    });
    cancelled()?;
    let clip = match request.clip {
        ClipTo::Nothing => None,
        ClipTo::Bounds(bounds) => Some(clip_to(&base, bounds)?),
        ClipTo::HeaderBounds { fallback } => match (base.header().bounds, fallback) {
            (Some(bounds), _) => Some(Clip::new(&base, bounds).map_err(CompileError::other)?),
            (None, Some(bounds)) => Some(clip_to(&base, bounds)?),
            (None, None) => {
                return Err(CompileError::InvalidRequest(format!(
                    "{} has no bounding box in its header to clip the changes to; \
                     pass bounds, or apply every change",
                    request.input.display()
                )));
            }
        },
    };

    // What to download, settled before reading anything.
    let agent = agent();
    let mut plan = None;
    if let Some(replication) = &request.replication {
        let header = base.header();
        let url = match replication {
            Replication::Url(url) => url.clone(),
            Replication::Header => header.replication_base_url.clone().ok_or_else(|| {
                CompileError::InvalidRequest(format!(
                    "{} names no replication server in its header; give one",
                    request.input.display()
                ))
            })?,
        };
        let latest = fetch_state(&agent, &url, None)?;
        let last = request
            .until
            .map_or(latest.sequence, |until| until.min(latest.sequence));
        let same_server = header
            .replication_base_url
            .as_deref()
            .map(|base| base.trim_end_matches('/'))
            == Some(url.as_str());
        let first = match (same_server, header.replication_sequence_number) {
            (true, Some(sequence)) => sequence as u64 + 1,
            _ => {
                let timestamp = header.replication_timestamp.ok_or_else(|| {
                    CompileError::InvalidRequest(format!(
                        "{} has no replication timestamp in its header, so where to start \
                         in {url} is unknown",
                        request.input.display()
                    ))
                })?;
                first_after(&agent, &url, timestamp, latest, &cancelled)?
            }
        };
        on_event(UpdateEvent::Replication {
            url: &url,
            latest: latest.sequence,
            first,
            last,
        });
        if first <= last && last - first + 1 > request.max_diffs {
            return Err(CompileError::InvalidRequest(format!(
                "{} diffs are needed from {url}, more than the limit of {}; raise the limit, \
                 use a server with longer intervals (hour, day) or start from a newer extract",
                last - first + 1,
                request.max_diffs
            )));
        }
        plan = Some((url, first, last));
    }

    let diffs = plan.as_ref().map_or(0, |(_, first, last)| {
        (last + 1).saturating_sub(*first) as usize
    });
    let total = request.change_files.len() + diffs;
    let mut changes = ChangeSet::new();
    let mut read = 0;
    let mut kept = 0;
    let mut record = |name: &str,
                      index: usize,
                      report: ReadReport,
                      on_event: &mut dyn FnMut(UpdateEvent<'_>)| {
        read += report.objects;
        kept += report.kept;
        on_event(UpdateEvent::Read {
            name,
            index,
            total,
            report,
        });
    };
    for (index, file) in request.change_files.iter().enumerate() {
        cancelled()?;
        let report = changes
            .read_osc(file, clip.as_ref())
            .map_err(CompileError::InvalidRequest)?;
        record(&file.display().to_string(), index, report, on_event);
    }
    let mut options = ApplyOptions {
        writing_program: Some(concat!("osm-tile-compiler ", env!("CARGO_PKG_VERSION")).to_string()),
        ..ApplyOptions::default()
    };
    let mut applied = None;
    if let Some((url, first, last)) = &plan {
        for sequence in *first..=*last {
            cancelled()?;
            let diff_url = sequence_url(url, sequence, "osc.gz");
            let bytes = download(&agent, &diff_url, 1 << 30)?;
            let report = changes
                .read_osc_bytes(&bytes, clip.as_ref())
                .map_err(|message| CompileError::Download(format!("{diff_url}: {message}")))?;
            let index = request.change_files.len() + (sequence - first) as usize;
            record(&diff_url, index, report, on_event);
        }
        if first <= last {
            let state = fetch_state(&agent, url, Some(*last))?;
            options.replication_timestamp = Some(state.timestamp);
            options.replication_sequence_number = Some(*last as i64);
            options.replication_base_url = Some(url.clone());
            applied = Some([*first, *last]);
        }
    }
    cancelled()?;
    on_event(UpdateEvent::Applying {
        changes: changes.len(),
    });
    let report: ApplyReport = pool
        .install(|| apply_changes(&base, &changes, &request.output, &options))
        .map_err(|message| CompileError::classify(message, None))?;
    Ok(UpdateReport {
        input: request.input.clone(),
        output: request.output.clone(),
        change_files: request.change_files.len(),
        replication_url: plan.map(|(url, _, _)| url),
        diffs: applied,
        replication_timestamp: options
            .replication_timestamp
            .or(base.header().replication_timestamp),
        replication_sequence_number: options
            .replication_sequence_number
            .or(base.header().replication_sequence_number),
        changes_read: read,
        changes_kept: kept,
        blobs_copied: report.blobs_copied,
        blobs_rewritten: report.blobs_rewritten,
        blobs_added: report.blobs_added,
        created: report.created,
        modified: report.modified,
        deleted: report.deleted,
        output_bytes: report.output_bytes,
        seconds: started.elapsed().as_secs_f64(),
    })
}

fn clip_to<'a>(base: &'a BaseIndex, bounds: CompileBounds) -> Result<Clip<'a>, CompileError> {
    let bounds = PbfHeaderBounds {
        west: bounds.west,
        south: bounds.south,
        east: if bounds.east > 180.0 {
            bounds.east - 360.0
        } else {
            bounds.east
        },
        north: bounds.north,
    };
    Clip::new(base, bounds).map_err(CompileError::other)
}

/// A replication state: a sequence number and when its diff ends.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ReplicationState {
    pub sequence: u64,
    /// Seconds since 1970.
    pub timestamp: i64,
}

/// `state.txt`, or with `sequence` that sequence's `AAA/BBB/CCC.state.txt`.
pub(crate) fn fetch_state(
    agent: &ureq::Agent,
    url: &str,
    sequence: Option<u64>,
) -> Result<ReplicationState, CompileError> {
    let state_url = match sequence {
        Some(sequence) => sequence_url(url, sequence, "state.txt"),
        None => format!("{url}/state.txt"),
    };
    let text = download(agent, &state_url, 1 << 16)?;
    parse_state(&String::from_utf8_lossy(&text))
        .map_err(|message| CompileError::Download(format!("{state_url}: {message}")))
}

/// A diff's or state's URL: the sequence number zero-padded to nine digits
/// and split into three directories, as Osmosis lays them out.
pub(crate) fn sequence_url(base: &str, sequence: u64, extension: &str) -> String {
    let digits = format!("{sequence:09}");
    format!(
        "{base}/{}/{}/{}.{extension}",
        &digits[0..3],
        &digits[3..6],
        &digits[6..9]
    )
}

/// Reads `sequenceNumber` and `timestamp` (`2026-11-19T10\:00\:00Z`, with
/// Java properties escaping) from a state file.
pub(crate) fn parse_state(text: &str) -> Result<ReplicationState, String> {
    let mut sequence = None;
    let mut timestamp = None;
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('#') {
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let value = value.trim().replace('\\', "");
        match key.trim() {
            "sequenceNumber" => sequence = value.parse::<u64>().ok(),
            "timestamp" => timestamp = parse_timestamp(&value),
            _ => {}
        }
    }
    match (sequence, timestamp) {
        (Some(sequence), Some(timestamp)) => Ok(ReplicationState {
            sequence,
            timestamp,
        }),
        _ => Err("not a replication state file".to_string()),
    }
}

/// `YYYY-MM-DDTHH:MM:SSZ` in seconds since 1970.
fn parse_timestamp(text: &str) -> Option<i64> {
    let bytes = text.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[19] != b'Z'
    {
        return None;
    }
    let number = |range: std::ops::Range<usize>| text.get(range)?.parse::<i64>().ok();
    let (year, month, day) = (number(0..4)?, number(5..7)?, number(8..10)?);
    let (hour, minute, second) = (number(11..13)?, number(14..16)?, number(17..19)?);
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 60
    {
        return None;
    }
    // Days from the civil date (Howard Hinnant's algorithm).
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    let days = era * 146_097 + day_of_era - 719_468;
    Some(days * 86_400 + hour * 3600 + minute * 60 + second)
}

/// The first sequence whose state is later than `timestamp`: its diff is the
/// first holding changes the extract lacks. Gallops back from `latest`, then
/// bisects. An overlap with what the extract holds is harmless, since a
/// change set keeps the highest version of each object.
fn first_after(
    agent: &ureq::Agent,
    url: &str,
    timestamp: i64,
    latest: ReplicationState,
    cancelled: &impl Fn() -> Result<(), CompileError>,
) -> Result<u64, CompileError> {
    if latest.timestamp <= timestamp {
        return Ok(latest.sequence + 1);
    }
    let mut later = latest.sequence;
    let mut step = 1;
    let earlier = loop {
        cancelled()?;
        if later <= step {
            break 0;
        }
        let probe = later - step;
        if fetch_state(agent, url, Some(probe))?.timestamp <= timestamp {
            break probe;
        }
        later = probe;
        step *= 2;
    };
    let mut earlier = earlier;
    while later - earlier > 1 {
        cancelled()?;
        let middle = earlier + (later - earlier) / 2;
        if fetch_state(agent, url, Some(middle))?.timestamp <= timestamp {
            earlier = middle;
        } else {
            later = middle;
        }
    }
    Ok(later)
}

fn download(agent: &ureq::Agent, url: &str, limit: u64) -> Result<Vec<u8>, CompileError> {
    agent
        .get(url)
        .call()
        .and_then(|mut response| response.body_mut().with_config().limit(limit).read_to_vec())
        .map_err(|error| CompileError::Download(format!("Could not download {url}: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_files_parse() {
        let state = parse_state(
            "#Thu Nov 19 10:02:08 UTC 2026\nsequenceNumber=124347\ntimestamp=2026-11-19T10\\:00\\:00Z\n",
        )
        .unwrap();
        assert_eq!(state.sequence, 124_347);
        assert_eq!(state.timestamp, 1_795_082_400);
        assert!(parse_state("<html>").is_err());
        assert_eq!(parse_timestamp("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_timestamp("2000-03-01T00:00:00Z"), Some(951_868_800));
        assert_eq!(parse_timestamp("2026-11-18T21:21:02Z"), Some(1_795_036_862));
        assert_eq!(parse_timestamp("2026-13-18T21:21:02Z"), None);
    }

    #[test]
    fn sequence_urls_split_into_directories() {
        assert_eq!(
            sequence_url(
                "https://planet.openstreetmap.org/replication/hour",
                124_347,
                "osc.gz"
            ),
            "https://planet.openstreetmap.org/replication/hour/000/124/347.osc.gz"
        );
        assert_eq!(sequence_url("u", 7, "state.txt"), "u/000/000/007.state.txt");
    }

    #[test]
    fn replication_sources_parse() {
        assert_eq!(Replication::parse("header"), Ok(Replication::Header));
        assert_eq!(
            Replication::parse("hour"),
            Ok(Replication::Url(
                "https://planet.openstreetmap.org/replication/hour".to_string()
            ))
        );
        assert_eq!(
            Replication::parse("https://download.geofabrik.de/europe/poland/malopolskie-updates/"),
            Ok(Replication::Url(
                "https://download.geofabrik.de/europe/poland/malopolskie-updates".to_string()
            ))
        );
        assert!(Replication::parse("weekly").is_err());
    }
}
