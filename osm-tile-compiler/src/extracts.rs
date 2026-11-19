//! OSM extracts by region name, from Geofabrik's download server.
//!
//! [`find_extract`] resolves a name such as `malopolskie`, `Małopolskie`,
//! `us/georgia` or `PL` through Geofabrik's index, the way Planetiler's
//! `--area` does; [`fetch_extract`] downloads the extract it names, unless
//! it is already in the directory.
use crate::control::{CANCELLED, CompileError};
use crate::sources::{agent, transfer};
use serde::Deserialize;
use std::{
    fs::{self, File},
    path::{Path, PathBuf},
    time::{Duration, Instant},
};

/// Geofabrik's index of extracts, without their outlines.
pub const GEOFABRIK_INDEX_URL: &str = "https://download.geofabrik.de/index-v1-nogeom.json";

/// One extract Geofabrik offers.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct Extract {
    /// Geofabrik's identifier: `malopolskie`, `poland`, `us/georgia`.
    pub id: String,
    /// Its name as Geofabrik lists it, which may hold a translation and
    /// HTML: `Georgia`, `Województwo małopolskie<br />(Lesser Poland
    /// Voivodeship)`.
    pub name: String,
    /// The identifier of the region it is part of.
    pub parent: Option<String>,
    /// ISO 3166-1 alpha-2 and ISO 3166-2 codes Geofabrik lists for it.
    pub iso_codes: Vec<String>,
    /// Where its `.osm.pbf` is downloaded from.
    pub url: String,
}

impl Extract {
    /// Its file name in a download directory: the identifier with `/`
    /// replaced, as `us-georgia.osm.pbf`.
    pub fn file_name(&self) -> String {
        format!("{}.osm.pbf", self.id.replace('/', "-"))
    }
}

#[derive(Deserialize)]
struct Index {
    features: Vec<Feature>,
}

#[derive(Deserialize)]
struct Feature {
    properties: Properties,
}

#[derive(Deserialize)]
struct Properties {
    id: String,
    name: String,
    #[serde(default)]
    parent: Option<String>,
    #[serde(default, rename = "iso3166-1:alpha2")]
    iso3166_1: Vec<String>,
    #[serde(default, rename = "iso3166-2")]
    iso3166_2: Vec<String>,
    #[serde(default)]
    urls: Urls,
}

#[derive(Deserialize, Default)]
struct Urls {
    #[serde(default)]
    pbf: Option<String>,
}

/// The extracts in Geofabrik's index JSON (`index-v1.json` or
/// `index-v1-nogeom.json`); entries without a PBF download are left out.
pub fn parse_index(json: &[u8]) -> Result<Vec<Extract>, CompileError> {
    read_index(json).map_err(CompileError::Download)
}

fn read_index(json: &[u8]) -> Result<Vec<Extract>, String> {
    let index: Index = serde_json::from_slice(json)
        .map_err(|e| format!("Geofabrik's index could not be read: {e}"))?;
    Ok(index
        .features
        .into_iter()
        .filter_map(|feature| {
            let p = feature.properties;
            Some(Extract {
                url: p.urls.pbf?,
                id: p.id,
                name: p.name,
                parent: p.parent,
                iso_codes: p.iso3166_1.into_iter().chain(p.iso3166_2).collect(),
            })
        })
        .collect())
}

/// Lowercase ASCII words: diacritics are dropped (`ł` and `đ` too) and
/// anything else that is not a letter or digit separates words, so
/// `Małopolskie`, `malopolskie` and `MALOPOLSKIE` are the same, as are
/// `us/new-york` and `US New York`.
fn words(text: &str) -> Vec<String> {
    use unicode_normalization::{UnicodeNormalization, char::is_combining_mark};
    let folded: String = text
        .nfd()
        .filter(|c| !is_combining_mark(*c))
        .map(|c| match c {
            'ł' | 'Ł' => 'l',
            'đ' | 'Đ' => 'd',
            'ø' | 'Ø' => 'o',
            'ß' => 's',
            c => c,
        })
        .flat_map(char::to_lowercase)
        .collect();
    folded
        .split(|c: char| !c.is_alphanumeric())
        .filter(|word| !word.is_empty())
        .map(String::from)
        .collect()
}

/// `name` without HTML tags, each replaced by a space, and with spaces
/// collapsed.
fn plain_name(name: &str) -> String {
    let mut plain = String::new();
    let mut in_tag = false;
    for c in name.chars() {
        match c {
            '<' => in_tag = true,
            '>' => {
                in_tag = false;
                plain.push(' ');
            }
            c if !in_tag => plain.push(c),
            _ => {}
        }
    }
    plain.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The texts a query is compared with: the identifier, the ISO codes, and
/// the name without HTML, whole and in its parts outside and inside
/// parentheses, so that `Województwo małopolskie<br />(Lesser Poland
/// Voivodeship)` is found as `Województwo małopolskie` and as `Lesser
/// Poland Voivodeship`, and `Brandenburg (mit Berlin)` as `Brandenburg`.
fn keys(extract: &Extract) -> Vec<String> {
    let name = plain_name(&extract.name);
    let mut keys = vec![extract.id.clone()];
    keys.extend(extract.iso_codes.iter().cloned());
    if let Some((outside, rest)) = name.split_once('(') {
        keys.push(outside.to_owned());
        keys.push(rest.split(')').next().unwrap_or_default().to_owned());
    }
    keys.push(name);
    keys
}

/// The extract `query` names: one whose identifier is `query` exactly, or
/// else the only one with the same words as `query` in its identifier, its
/// ISO codes, or its name without HTML, whole or in its parts outside and
/// inside parentheses. Words ignore case, accents and punctuation, so
/// `Małopolskie`, `MALOPOLSKIE` and `Lesser Poland Voivodeship` all name
/// `malopolskie`. A `geofabrik:` prefix, as Planetiler takes, is ignored.
/// No match, or several, is an error that lists candidates.
pub fn find_extract<'a>(index: &'a [Extract], query: &str) -> Result<&'a Extract, CompileError> {
    find(index, query).map_err(CompileError::InvalidRequest)
}

fn find<'a>(index: &'a [Extract], query: &str) -> Result<&'a Extract, String> {
    let query = query.trim();
    let query = query.strip_prefix("geofabrik:").unwrap_or(query);
    if let Some(extract) = index.iter().find(|extract| extract.id == query) {
        return Ok(extract);
    }
    let wanted = words(query);
    if wanted.is_empty() {
        return Err(format!("'{query}' does not name a region"));
    }
    let matches: Vec<&Extract> = index
        .iter()
        .filter(|extract| keys(extract).iter().any(|key| words(key) == wanted))
        .collect();
    match matches[..] {
        [extract] => Ok(extract),
        [] => {
            let joined = wanted.join(" ");
            let mut similar: Vec<&str> = index
                .iter()
                .filter(|extract| {
                    keys(extract)
                        .iter()
                        .any(|key| words(key).join(" ").contains(&joined))
                })
                .map(|extract| extract.id.as_str())
                .collect();
            similar.sort_unstable();
            let hint = match similar.len() {
                0 => String::new(),
                count if count <= 10 => format!("; similar: {}", similar.join(", ")),
                count => format!(
                    "; similar: {}, and {} more",
                    similar[..10].join(", "),
                    count - 10
                ),
            };
            Err(format!(
                "Geofabrik has no extract named '{query}'{hint}. The regions are listed \
                 at https://download.geofabrik.de"
            ))
        }
        _ => Err(format!(
            "'{query}' matches several Geofabrik extracts: {}; pass one of these identifiers",
            matches
                .iter()
                .map(|extract| format!("{} ({})", extract.id, plain_name(&extract.name)))
                .collect::<Vec<_>>()
                .join(", ")
        )),
    }
}

/// What [`fetch_extract`] is doing, for progress reporting.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum ExtractEvent<'a> {
    /// Downloading Geofabrik's index from `url`.
    Resolving { url: &'a str },
    /// The query names this extract.
    Found { extract: &'a Extract },
    /// Starting the extract's download from `url`.
    Downloading { url: &'a str },
    /// `bytes` received so far, at most every 250 ms.
    Received { bytes: u64, total: Option<u64> },
    /// The extract is at `path`: freshly downloaded, or already there.
    Ready {
        path: &'a Path,
        bytes: u64,
        downloaded: bool,
    },
}

/// Resolves `query` through Geofabrik's index ([`find_extract`]) and
/// downloads that extract into `directory` as [`Extract::file_name`],
/// returning its path. An extract already there is kept as it is; remove
/// it to download a newer one. `should_cancel` is asked after every read of
/// 256 KB. A cancelled or failed download leaves no partial file.
pub fn fetch_extract(
    directory: &Path,
    query: &str,
    on_event: impl FnMut(ExtractEvent),
    should_cancel: impl Fn() -> bool,
) -> Result<PathBuf, CompileError> {
    fetch_extract_from(
        GEOFABRIK_INDEX_URL,
        directory,
        query,
        on_event,
        should_cancel,
    )
}

/// [`fetch_extract`] with the index read from `index_url` instead of
/// [`GEOFABRIK_INDEX_URL`]: a mirror, or a local server in tests. The
/// extract is downloaded from the URL the index gives for it.
pub fn fetch_extract_from(
    index_url: &str,
    directory: &Path,
    query: &str,
    mut on_event: impl FnMut(ExtractEvent),
    should_cancel: impl Fn() -> bool,
) -> Result<PathBuf, CompileError> {
    let agent = agent();
    on_event(ExtractEvent::Resolving { url: index_url });
    let index = agent
        .get(index_url)
        .call()
        .and_then(|mut response| {
            response
                .body_mut()
                .with_config()
                .limit(64 << 20)
                .read_to_vec()
        })
        .map_err(|e| {
            CompileError::Download(format!(
                "Could not download Geofabrik's index from {index_url}: {e}"
            ))
        })?;
    let index = parse_index(&index)?;
    let extract = find_extract(&index, query)?;
    on_event(ExtractEvent::Found { extract });
    download(directory, extract, &agent, on_event, &should_cancel).map_err(|error| {
        if should_cancel() || error == CANCELLED {
            CompileError::Cancelled
        } else {
            CompileError::classify(error, Some(crate::CompileStage::FetchSources))
        }
    })
}

fn download(
    directory: &Path,
    extract: &Extract,
    agent: &ureq::Agent,
    mut on_event: impl FnMut(ExtractEvent),
    should_cancel: &dyn Fn() -> bool,
) -> Result<PathBuf, String> {
    let destination = directory.join(extract.file_name());
    if let Ok(metadata) = fs::metadata(&destination) {
        on_event(ExtractEvent::Ready {
            path: &destination,
            bytes: metadata.len(),
            downloaded: false,
        });
        return Ok(destination);
    }
    if should_cancel() {
        return Err(CANCELLED.into());
    }
    fs::create_dir_all(directory)
        .map_err(|e| format!("Failed to create {}: {e}", directory.display()))?;
    on_event(ExtractEvent::Downloading { url: &extract.url });
    let mut response = agent
        .get(&extract.url)
        .call()
        .map_err(|e| format!("Could not download {}: {e}", extract.url))?;
    let expected = response
        .headers()
        .get("content-length")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok());
    let partial = directory.join(format!("{}.part", extract.file_name()));
    let mut output = File::create(&partial)
        .map_err(|e| format!("Failed to create {}: {e}", partial.display()))?;
    let mut last = Instant::now() - Duration::from_secs(1);
    let received = transfer(
        response.body_mut().as_reader(),
        &mut output,
        expected,
        |bytes| {
            if should_cancel() {
                return Err(CANCELLED.into());
            }
            if last.elapsed() >= Duration::from_millis(250) {
                last = Instant::now();
                on_event(ExtractEvent::Received {
                    bytes,
                    total: expected,
                });
            }
            Ok(())
        },
    )
    .and_then(|received| {
        output.sync_all().map_err(|e| e.to_string())?;
        Ok(received)
    });
    drop(output);
    let bytes = match received {
        Ok((bytes, _sha256)) => bytes,
        Err(error) => {
            let _ = fs::remove_file(&partial);
            // Name the URL: a dropped connection alone reads as "Peer
            // disconnected".
            return Err(if error == CANCELLED {
                error
            } else {
                format!("Could not download {}: {error}", extract.url)
            });
        }
    };
    fs::rename(&partial, &destination).map_err(|e| {
        let _ = fs::remove_file(&partial);
        format!("Failed to move {} into place: {e}", partial.display())
    })?;
    on_event(ExtractEvent::Ready {
        path: &destination,
        bytes,
        downloaded: true,
    });
    Ok(destination)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Entries shaped as in Geofabrik's `index-v1-nogeom.json`.
    const INDEX: &str = r#"{"type": "FeatureCollection", "features": [
      {"type": "Feature", "properties": {"id": "europe", "name": "Europe",
        "urls": {"pbf": "https://download.geofabrik.de/europe-latest.osm.pbf"}}},
      {"type": "Feature", "properties": {"id": "poland", "parent": "europe",
        "name": "Poland", "iso3166-1:alpha2": ["PL"],
        "urls": {"pbf": "https://download.geofabrik.de/europe/poland-latest.osm.pbf"}}},
      {"type": "Feature", "properties": {"id": "malopolskie", "parent": "poland",
        "name": "Województwo małopolskie<br />(Lesser Poland Voivodeship)",
        "iso3166-2": ["PL-12"],
        "urls": {"pbf": "https://download.geofabrik.de/europe/poland/malopolskie-latest.osm.pbf"}}},
      {"type": "Feature", "properties": {"id": "georgia", "parent": "europe",
        "name": "Georgia", "iso3166-1:alpha2": ["GE"],
        "urls": {"pbf": "https://download.geofabrik.de/europe/georgia-latest.osm.pbf"}}},
      {"type": "Feature", "properties": {"id": "us/georgia", "parent": "us",
        "name": "Georgia", "iso3166-2": ["US-GA"],
        "urls": {"pbf": "https://download.geofabrik.de/north-america/us/georgia-latest.osm.pbf"}}},
      {"type": "Feature", "properties": {"id": "brandenburg", "parent": "germany",
        "name": "Brandenburg (mit Berlin)",
        "urls": {"pbf": "https://download.geofabrik.de/europe/germany/brandenburg-latest.osm.pbf"}}},
      {"type": "Feature", "properties": {"id": "us/new-york", "parent": "us",
        "name": "New York", "iso3166-2": ["US-NY"],
        "urls": {"pbf": "https://download.geofabrik.de/north-america/us/new-york-latest.osm.pbf"}}},
      {"type": "Feature", "properties": {"id": "no-download", "name": "No Download",
        "urls": {}}}
    ]}"#;

    fn index() -> Vec<Extract> {
        parse_index(INDEX.as_bytes()).unwrap()
    }

    #[test]
    fn parses_the_index_and_skips_entries_without_a_pbf() {
        let index = index();
        assert_eq!(index.len(), 7);
        let poland = &index[1];
        assert_eq!(poland.id, "poland");
        assert_eq!(poland.parent.as_deref(), Some("europe"));
        assert_eq!(poland.iso_codes, ["PL"]);
        assert!(parse_index(b"{}").is_err());
    }

    #[test]
    fn finds_extracts_by_id_name_or_iso_code() {
        let index = index();
        let id = |query| find_extract(&index, query).map(|extract| extract.id.as_str());
        assert_eq!(id("malopolskie"), Ok("malopolskie"));
        assert_eq!(id("Małopolskie"), Ok("malopolskie"));
        assert_eq!(id("MALOPOLSKIE"), Ok("malopolskie"));
        assert_eq!(id("geofabrik:malopolskie"), Ok("malopolskie"));
        assert_eq!(id("PL-12"), Ok("malopolskie"));
        assert_eq!(id("pl"), Ok("poland"));
        assert_eq!(id("województwo małopolskie"), Ok("malopolskie"));
        assert_eq!(id("Lesser Poland Voivodeship"), Ok("malopolskie"));
        assert_eq!(id("Brandenburg"), Ok("brandenburg"));
        assert_eq!(id("new york"), Ok("us/new-york"));
        assert_eq!(id("us_new_york"), Ok("us/new-york"));
        // An exact identifier wins over a name that others share.
        assert_eq!(id("georgia"), Ok("georgia"));
        assert_eq!(id("us/georgia"), Ok("us/georgia"));
    }

    #[test]
    fn ambiguous_and_unknown_names_list_candidates() {
        let index = index();
        let error = find_extract(&index, "Georgia ").unwrap_err().to_string();
        // Trimmed, "Georgia" differs from the identifier only in case.
        assert!(
            error.contains("georgia (Georgia), us/georgia (Georgia)"),
            "{error}"
        );
        let error = find_extract(&index, "york").unwrap_err().to_string();
        assert!(error.contains("no extract named 'york'"), "{error}");
        assert!(error.contains("similar: us/new-york"), "{error}");
        assert!(find_extract(&index, " / ").is_err());
        assert_eq!(
            plain_name(&index[2].name),
            "Województwo małopolskie (Lesser Poland Voivodeship)"
        );
    }

    #[test]
    fn file_names_flatten_identifiers() {
        let index = index();
        assert_eq!(index[2].file_name(), "malopolskie.osm.pbf");
        assert_eq!(index[4].file_name(), "us-georgia.osm.pbf");
        assert_eq!(index[4].id, "us/georgia");
    }

    #[test]
    fn an_extract_already_present_is_kept_without_a_request() {
        let temp = tempfile::tempdir().unwrap();
        let index = index();
        let extract = &index[2];
        fs::write(temp.path().join(extract.file_name()), b"pbf").unwrap();
        let mut events = Vec::new();
        let path = download(
            temp.path(),
            extract,
            &agent(),
            |event| events.push(format!("{event:?}")),
            &|| false,
        )
        .unwrap();
        assert_eq!(path, temp.path().join("malopolskie.osm.pbf"));
        assert_eq!(events.len(), 1);
        assert!(events[0].contains("downloaded: false"), "{events:?}");
    }

    #[test]
    fn a_cancelled_download_leaves_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("extracts");
        let index = index();
        let error = download(&directory, &index[2], &agent(), |_| {}, &|| true).unwrap_err();
        assert_eq!(error, CANCELLED);
        assert!(!directory.exists());
    }
}
