//! The MapLibre style for the archives `compile` writes (OSM Bright, see
//! `style/LICENSE.md`), and the offline style pack: the style with its font
//! glyphs and icon sprite as local files, so that a device draws labels and
//! icons with no network.
//!
//! A pack is a directory:
//!
//! ```text
//! style.json                          glyphs and sprite as relative URLs
//! fonts/<fontstack>/<start>-<end>.pbf the glyph ranges the labels need
//! sprite.json, sprite.png             the icon sprite, and its @2x pair
//! sprite@2x.json, sprite@2x.png
//! LICENSE.md                          the style's, sprite's and fonts' terms
//! ```
//!
//! [`write_style_pack`] downloads the glyphs and sprite once (or copies them
//! from a local mirror); the result needs nothing more.

use crate::control::CANCELLED;
use crate::{CompileError, OMT_LANGUAGES};
use serde::Serialize;
use std::collections::BTreeSet;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

/// OSM Bright, adapted to the archives `compile` writes. Its three URLs are
/// each one JSON string in the template, which [`maplibre_style`] replaces.
const STYLE: &str = include_str!("../style/osm-bright.json");
const STYLE_LICENSE: &str = include_str!("../style/LICENSE.md");
const STYLE_ARCHIVE: &str = "pmtiles://basemap.pmtiles";
/// The style layers that draw the opt-in `sar` layer (docs/sar.md), which
/// [`with_sar_layers`] appends.
const SAR_STYLE: &str = include_str!("../style/sar-layers.json");

/// Where the style loads its font glyphs from unless told otherwise: the
/// MapLibre demo server, not meant for production.
pub const DEFAULT_GLYPHS_URL: &str = "https://demotiles.maplibre.org/font/{fontstack}/{range}.pbf";
/// Where the style loads its icon sprite from unless told otherwise (without
/// the extension): OSM Bright's GitHub Pages site, not meant for production.
pub const DEFAULT_SPRITE_URL: &str = "https://openmaptiles.github.io/osm-bright-gl-style/sprite";
/// The fonts the style's labels use, as MapLibre names their font stacks.
pub const STYLE_FONTSTACKS: [&str; 3] = ["Noto Sans Regular", "Noto Sans Bold", "Noto Sans Italic"];

/// The glyph URL inside a pack's `style.json`, relative to the style.
const PACK_GLYPHS: &str = "fonts/{fontstack}/{range}.pbf";
/// The sprite URL inside a pack's `style.json`, relative to the style.
const PACK_SPRITE: &str = "sprite";
/// The sprite's files; the first two are required.
const SPRITE_FILES: [&str; 4] = [".json", ".png", "@2x.json", "@2x.png"];
/// Downloads running at once.
const WORKERS: usize = 8;

/// The MapLibre style (OSM Bright) for an archive that MapLibre loads from
/// `archive_url` through the PMTiles protocol (`pmtiles://` is prepended
/// unless present), with its glyphs and sprite from the given URLs or the
/// defaults. The template's layout is kept: only its URLs change.
pub fn maplibre_style(archive_url: &str, glyphs: Option<&str>, sprite: Option<&str>) -> String {
    let archive = if archive_url.starts_with("pmtiles://") {
        archive_url.to_owned()
    } else {
        format!("pmtiles://{archive_url}")
    };
    let quoted = |url: &str| serde_json::Value::from(url).to_string();
    [
        (STYLE_ARCHIVE, Some(archive.as_str())),
        (DEFAULT_GLYPHS_URL, glyphs),
        (DEFAULT_SPRITE_URL, sprite),
    ]
    .into_iter()
    .fold(STYLE.to_owned(), |style, (default, url)| match url {
        Some(url) => style.replacen(&quoted(default), &quoted(url), 1),
        None => style,
    })
}

/// `style` with the layers that draw the opt-in `sar` layer appended after
/// the basemap's, so that they draw above it: the style for an archive
/// compiled with `sar`. The result is pretty-printed JSON.
pub fn with_sar_layers(style: &str) -> String {
    let mut style: serde_json::Value = serde_json::from_str(style).expect("the style is JSON");
    let sar: Vec<serde_json::Value> =
        serde_json::from_str(SAR_STYLE).expect("the bundled SAR style is JSON");
    style["layers"]
        .as_array_mut()
        .expect("the style has layers")
        .extend(sar);
    serde_json::to_string_pretty(&style).expect("a style serializes")
}

/// `style` for an archive whose tiles are in `format`: for MLT, the source
/// says `"encoding": "mlt"`, without which MapLibre reads the tiles as MVT.
/// The result is pretty-printed JSON.
pub fn with_tile_format(style: &str, format: crate::TileFormat) -> String {
    let mut style: serde_json::Value = serde_json::from_str(style).expect("the style is JSON");
    if format == crate::TileFormat::Mlt {
        style["sources"]["openmaptiles"]["encoding"] = "mlt".into();
    }
    serde_json::to_string_pretty(&style).expect("a style serializes")
}

/// What [`write_style_pack`] writes, and where it fetches from.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct StylePackRequest {
    /// The pack's directory; it must not exist yet. It is built beside
    /// itself as `<NAME>.part` and renamed into place when complete.
    pub directory: PathBuf,
    /// The archive's URL as MapLibre will load it, written into the style
    /// as given (with `pmtiles://` prepended unless present).
    pub archive_url: String,
    /// The languages whose scripts the labels need glyphs for, in
    /// [`crate::resolve_languages`]'s syntax. The Latin ranges (with
    /// punctuation and symbols) are always written; `None` writes only them,
    /// enough for every `name:latin` label. List the languages of the region
    /// for its `name:nonlatin` labels (`uk` for Cyrillic, `el` for Greek),
    /// or `default` for every script of the profile's languages.
    pub languages: Option<Vec<String>>,
    /// A local copy of the archive: every glyph range that the text its
    /// labels show (`name:latin`, `name:nonlatin`, `ref`, ...) uses is
    /// written too. This is the way to be sure of every label: MapLibre
    /// Native fails to draw a view that needs a glyph range it cannot load.
    pub archive_file: Option<PathBuf>,
    /// Where to fetch the glyphs from, with `{fontstack}` and `{range}`: an
    /// `http(s)://` or `file://` URL, or a local path.
    pub glyphs: String,
    /// Where to fetch the sprite from, without its extension: an
    /// `http(s)://` or `file://` URL, or a local path.
    pub sprite: String,
    /// Put before the style's relative glyph and sprite URLs, for hosts
    /// that do not resolve URLs relative to the style (MapLibre Native wants
    /// `file://`, `asset://` or `http(s)://` URLs). A `/` is added unless it
    /// ends with one.
    pub base_url: Option<String>,
    /// Also draw the opt-in `sar` layer, for an archive compiled with it
    /// (see [`with_sar_layers`]).
    pub sar: bool,
    /// The encoding of the archive's tiles (see [`with_tile_format`]).
    pub tile_format: crate::TileFormat,
}

impl StylePackRequest {
    /// A request for a pack in `directory` for the archive at `archive_url`,
    /// with the Latin glyph ranges, fetched from the default servers.
    pub fn new(directory: impl Into<PathBuf>, archive_url: impl Into<String>) -> Self {
        Self {
            directory: directory.into(),
            archive_url: archive_url.into(),
            languages: None,
            archive_file: None,
            glyphs: DEFAULT_GLYPHS_URL.into(),
            sprite: DEFAULT_SPRITE_URL.into(),
            base_url: None,
            sar: false,
            tile_format: crate::TileFormat::Mvt,
        }
    }

    /// Checks everything that needs no network: the directory and its
    /// `.part` sibling do not exist, the glyph URL has both placeholders,
    /// and the languages are known. [`write_style_pack`] runs it first.
    pub fn validate(&self) -> Result<(), CompileError> {
        let invalid = |message: String| Err(CompileError::InvalidRequest(message));
        if self.archive_url.is_empty() {
            return invalid("The style pack needs an archive URL".into());
        }
        if !self.glyphs.contains("{fontstack}") || !self.glyphs.contains("{range}") {
            return invalid(format!(
                "The glyph URL must contain {{fontstack}} and {{range}}, got '{}'",
                self.glyphs
            ));
        }
        if self.directory.file_name().is_none() {
            return invalid(format!(
                "The style pack directory '{}' has no name",
                self.directory.display()
            ));
        }
        for path in [&self.directory, &partial_directory(&self.directory)] {
            if path.symlink_metadata().is_ok() {
                return invalid(format!(
                    "{} already exists; choose a new directory or remove it",
                    path.display()
                ));
            }
        }
        glyph_ranges(self.languages.as_deref())?;
        if let Some(file) = &self.archive_file
            && !file.is_file()
        {
            return invalid(format!("The archive {} does not exist", file.display()));
        }
        Ok(())
    }
}

/// Progress of [`write_style_pack`].
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum StylePackEvent {
    /// Reading the archive's labels for the glyph ranges they use.
    ReadingArchive,
    /// About to fetch `total` files: every glyph range of every font, and
    /// the sprite.
    Started { total: usize },
    /// `done` of `total` files fetched (or found missing), `bytes` written.
    Fetched {
        done: usize,
        total: usize,
        bytes: u64,
    },
}

/// What [`write_style_pack`] wrote.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
#[non_exhaustive]
pub struct StylePackReport {
    /// The pack's directory.
    pub directory: PathBuf,
    /// Its `style.json`.
    pub style: PathBuf,
    /// The font stacks under `fonts/`.
    pub fontstacks: Vec<String>,
    /// The glyph ranges written for each font, as `<start>-<end>`.
    pub ranges: Vec<String>,
    /// Files written, `style.json` and `LICENSE.md` included.
    pub files: usize,
    /// Their size in bytes.
    pub bytes: u64,
    /// Files the source did not have (HTTP 404, or absent from a local
    /// mirror), relative to the pack. A missing glyph range leaves its
    /// characters out of labels; a missing `@2x` sprite makes high-density
    /// screens use the 1x one.
    pub missing: Vec<String>,
}

/// Writes an offline style pack (see the module docs) as `request` says,
/// fetching the glyph ranges for its languages and the sprite.
/// `should_cancel` is asked before each file. A failed or cancelled pack
/// leaves nothing behind; a glyph range or `@2x` sprite file the source
/// lacks is skipped and listed in [`StylePackReport::missing`], while a
/// missing 1x sprite fails.
pub fn write_style_pack(
    request: &StylePackRequest,
    mut on_event: impl FnMut(StylePackEvent),
    should_cancel: impl Fn() -> bool + Sync,
) -> Result<StylePackReport, CompileError> {
    request.validate()?;
    let mut ranges = glyph_ranges(request.languages.as_deref())?;
    let partial = partial_directory(&request.directory);
    let result = match &request.archive_file {
        Some(file) => {
            on_event(StylePackEvent::ReadingArchive);
            crate::archive::glyph_ranges_used(file, &label_keys(), &should_cancel)
                .map_err(|e| format!("Could not read {}: {e}", file.display()))
        }
        None => Ok(Default::default()),
    }
    .and_then(|used| {
        ranges.extend(used);
        ranges.sort_unstable();
        ranges.dedup();
        write_pack(request, &ranges, &partial, &mut on_event, &should_cancel)
    });
    let result = result.and_then(|mut report| {
        fs::rename(&partial, &request.directory).map_err(|e| {
            format!(
                "Could not move {} to {}: {e}",
                partial.display(),
                request.directory.display()
            )
        })?;
        report.style = request.directory.join("style.json");
        Ok(report)
    });
    result.map_err(|error| {
        let _ = fs::remove_dir_all(&partial);
        if should_cancel() || error == CANCELLED {
            CompileError::Cancelled
        } else if let Some(message) = error.strip_prefix(DOWNLOAD) {
            CompileError::Download(message.into())
        } else {
            CompileError::other(error)
        }
    })
}

/// Marks an error message as a failed download.
const DOWNLOAD: &str = "download: ";

fn partial_directory(directory: &Path) -> PathBuf {
    let mut name = directory.file_name().unwrap_or_default().to_os_string();
    name.push(".part");
    directory.with_file_name(name)
}

/// One file to fetch: from where, to where in the pack, and whether the
/// pack can do without it.
struct Fetch {
    source: String,
    path: String,
    optional: bool,
}

fn write_pack(
    request: &StylePackRequest,
    ranges: &[u32],
    partial: &Path,
    on_event: &mut dyn FnMut(StylePackEvent),
    should_cancel: &(dyn Fn() -> bool + Sync),
) -> Result<StylePackReport, String> {
    let mut fetches = Vec::new();
    for fontstack in STYLE_FONTSTACKS {
        for &start in ranges {
            let range = range_name(start);
            fetches.push(Fetch {
                source: request
                    .glyphs
                    .replace("{fontstack}", &fontstack_in(&request.glyphs, fontstack))
                    .replace("{range}", &range),
                path: format!("fonts/{fontstack}/{range}.pbf"),
                optional: true,
            });
        }
    }
    for (index, suffix) in SPRITE_FILES.into_iter().enumerate() {
        fetches.push(Fetch {
            source: format!("{}{suffix}", request.sprite),
            path: format!("{PACK_SPRITE}{suffix}"),
            optional: index >= 2,
        });
    }

    for fontstack in STYLE_FONTSTACKS {
        fs::create_dir_all(partial.join("fonts").join(fontstack))
            .map_err(|e| format!("Could not create {}: {e}", partial.display()))?;
    }
    let total = fetches.len();
    on_event(StylePackEvent::Started { total });

    let agent = crate::sources::agent();
    let next = AtomicUsize::new(0);
    let stop = AtomicBool::new(false);
    // (done, bytes, missing, first error), and the events to pass on.
    let state = Mutex::new((0usize, 0u64, Vec::new(), None::<String>));
    let (events, receive) = std::sync::mpsc::channel();
    std::thread::scope(|scope| {
        for _ in 0..WORKERS.min(total) {
            let events = events.clone();
            scope.spawn(|| {
                let events = events;
                loop {
                    let index = next.fetch_add(1, Ordering::Relaxed);
                    if index >= total || stop.load(Ordering::Relaxed) {
                        return;
                    }
                    let fetch = &fetches[index];
                    let outcome = if should_cancel() {
                        Err(CANCELLED.to_string())
                    } else {
                        fetch_one(&agent, fetch, partial)
                    };
                    let mut state = state.lock().unwrap_or_else(|e| e.into_inner());
                    match outcome {
                        Ok(Some(bytes)) => state.1 += bytes,
                        Ok(None) => state.2.push(fetch.path.clone()),
                        Err(error) => {
                            stop.store(true, Ordering::Relaxed);
                            state.3.get_or_insert(error);
                            return;
                        }
                    }
                    state.0 += 1;
                    let _ = events.send(StylePackEvent::Fetched {
                        done: state.0,
                        total,
                        bytes: state.1,
                    });
                }
            });
        }
        drop(events);
        for event in receive {
            on_event(event);
        }
    });
    let (_, bytes, mut missing, error) = state.into_inner().unwrap_or_else(|e| e.into_inner());
    if let Some(error) = error {
        return Err(error);
    }
    missing.sort();

    // The sprite is checked so that a wrong URL fails here, not on a device.
    let sprite = fs::read(partial.join(format!("{PACK_SPRITE}.json")))
        .map_err(|e| format!("Could not read the sprite index: {e}"))?;
    if serde_json::from_slice::<serde_json::Map<String, serde_json::Value>>(&sprite).is_err() {
        return Err(format!(
            "{DOWNLOAD}{}.json is not a sprite index (a JSON object)",
            request.sprite
        ));
    }

    let base = request.base_url.as_deref().map_or(String::new(), |base| {
        if base.ends_with('/') {
            base.to_owned()
        } else {
            format!("{base}/")
        }
    });
    let style = maplibre_style(
        &request.archive_url,
        Some(&format!("{base}{PACK_GLYPHS}")),
        Some(&format!("{base}{PACK_SPRITE}")),
    );
    let style = if request.sar {
        with_sar_layers(&style)
    } else {
        style
    };
    let style = match request.tile_format {
        crate::TileFormat::Mvt => style,
        format => with_tile_format(&style, format),
    };
    let license = license(request);
    for (name, contents) in [("style.json", &style), ("LICENSE.md", &license)] {
        fs::write(partial.join(name), contents)
            .map_err(|e| format!("Could not write {name}: {e}"))?;
    }
    Ok(StylePackReport {
        directory: request.directory.clone(),
        style: request.directory.join("style.json"),
        fontstacks: STYLE_FONTSTACKS.iter().map(|s| s.to_string()).collect(),
        ranges: ranges.iter().map(|&start| range_name(start)).collect(),
        files: total - missing.len() + 2,
        bytes: bytes + (style.len() + license.len()) as u64,
        missing,
    })
}

/// The pack's `LICENSE.md`: where the fonts and sprite came from, then the
/// style's license, which the sprite shares.
fn license(request: &StylePackRequest) -> String {
    format!(
        "# Licenses\n\n\
         `style.json` and the `sprite` files are OSM Bright, under the license \
         below; the sprite was fetched from {}.\n\n\
         The glyphs in `fonts/` are rendered from Noto Sans, under the SIL Open \
         Font License 1.1 (https://openfontlicense.org); they were fetched from \
         {}.\n\n\
         Tiles drawn with this style are a derived database of OpenStreetMap \
         under the Open Database License 1.0: credit \"© OpenStreetMap \
         contributors\".\n\n---\n\n{STYLE_LICENSE}",
        request.sprite, request.glyphs
    )
}

/// The font stack as the glyph source names it: percent-encoded for a URL
/// (as MapLibre requests it), as is for a local path.
fn fontstack_in(source: &str, fontstack: &str) -> String {
    if is_url(source) {
        fontstack.replace(' ', "%20")
    } else {
        fontstack.to_owned()
    }
}

fn is_url(source: &str) -> bool {
    source.starts_with("http://") || source.starts_with("https://")
}

/// Fetches one file into the pack: its size, or `None` if the source does
/// not have it and it is optional.
fn fetch_one(agent: &ureq::Agent, fetch: &Fetch, partial: &Path) -> Result<Option<u64>, String> {
    let bytes = if is_url(&fetch.source) {
        match agent.get(&fetch.source).call() {
            Ok(mut response) => {
                let mut bytes = Vec::new();
                response
                    .body_mut()
                    .as_reader()
                    .read_to_end(&mut bytes)
                    .map_err(|e| format!("{DOWNLOAD}Could not download {}: {e}", fetch.source))?;
                Some(bytes)
            }
            Err(ureq::Error::StatusCode(404)) => None,
            Err(e) => {
                return Err(format!(
                    "{DOWNLOAD}Could not download {}: {e}",
                    fetch.source
                ));
            }
        }
    } else {
        let path = local_path(&fetch.source);
        match fs::read(&path) {
            Ok(bytes) => Some(bytes),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("Could not read {}: {e}", path.display())),
        }
    };
    let Some(bytes) = bytes else {
        return if fetch.optional {
            Ok(None)
        } else {
            Err(format!("{DOWNLOAD}{} was not found", fetch.source))
        };
    };
    if bytes.is_empty() {
        return Err(format!("{DOWNLOAD}{} is empty", fetch.source));
    }
    let destination = partial.join(&fetch.path);
    fs::write(&destination, &bytes)
        .map_err(|e| format!("Could not write {}: {e}", destination.display()))?;
    Ok(Some(bytes.len() as u64))
}

/// A `file://` URL or a plain path as a path; `%20` in a URL is a space.
fn local_path(source: &str) -> PathBuf {
    match source.strip_prefix("file://") {
        Some(path) => PathBuf::from(path.replace("%20", " ")),
        None => PathBuf::from(source),
    }
}

/// The tag keys whose values the style's labels show: the `{key}` tokens
/// of its `text-field`s.
fn label_keys() -> Vec<String> {
    let style: serde_json::Value = serde_json::from_str(STYLE).expect("the style is JSON");
    let mut keys = BTreeSet::new();
    for layer in style["layers"].as_array().into_iter().flatten() {
        let Some(field) = layer["layout"]["text-field"].as_str() else {
            continue;
        };
        for token in field.split('{').skip(1) {
            if let Some((key, _)) = token.split_once('}') {
                keys.insert(key.to_owned());
            }
        }
    }
    keys.into_iter().collect()
}

/// `<start>-<end>`, as MapLibre names the glyph range starting at `start`.
fn range_name(start: u32) -> String {
    format!("{start}-{}", start + 255)
}

/// A script's code points, as inclusive ranges.
type Blocks = &'static [(u32, u32)];

/// Latin with its extensions and combining marks, punctuation, currency
/// and letter-like symbols: every `name:latin` label.
const LATIN: Blocks = &[(0x0000, 0x036F), (0x1E00, 0x1EFF), (0x2000, 0x214F)];
const GREEK: Blocks = &[(0x0370, 0x03FF), (0x1F00, 0x1FFF)];
const CYRILLIC: Blocks = &[(0x0400, 0x052F)];
const ARMENIAN: Blocks = &[(0x0530, 0x058F)];
const HEBREW: Blocks = &[(0x0590, 0x05FF), (0xFB1D, 0xFB4F)];
const ARABIC: Blocks = &[
    (0x0600, 0x06FF),
    (0x0750, 0x077F),
    (0x08A0, 0x08FF),
    (0xFB50, 0xFDFF),
    (0xFE70, 0xFEFF),
];
const DEVANAGARI: Blocks = &[(0x0900, 0x097F)];
const BENGALI: Blocks = &[(0x0980, 0x09FF)];
const GURMUKHI: Blocks = &[(0x0A00, 0x0A7F)];
const TAMIL: Blocks = &[(0x0B80, 0x0BFF)];
const TELUGU: Blocks = &[(0x0C00, 0x0C7F)];
const KANNADA: Blocks = &[(0x0C80, 0x0CFF)];
const MALAYALAM: Blocks = &[(0x0D00, 0x0D7F)];
const THAI: Blocks = &[(0x0E00, 0x0E7F)];
const GEORGIAN: Blocks = &[(0x10A0, 0x10FF), (0x1C90, 0x1CBF)];
const ETHIOPIC: Blocks = &[(0x1200, 0x139F)];
/// CJK punctuation, kana and full-width forms.
const KANA: Blocks = &[(0x3000, 0x30FF), (0xFF00, 0xFFEF)];
const HAN: Blocks = &[(0x3000, 0x303F), (0x4E00, 0x9FFF), (0xFF00, 0xFFEF)];
const HANGUL: Blocks = &[
    (0x1100, 0x11FF),
    (0x3000, 0x303F),
    (0x3130, 0x318F),
    (0xAC00, 0xD7AF),
    (0xFF00, 0xFFEF),
];

/// The scripts beyond Latin that a language's names are written in.
fn scripts(language: &str) -> &'static [Blocks] {
    match language {
        "el" => &[GREEK],
        "be" | "bg" | "kk" | "mk" | "ru" | "sr" | "uk" => &[CYRILLIC],
        "hy" => &[ARMENIAN],
        "he" => &[HEBREW],
        "ar" | "fa" | "ku" | "pnb" | "ur" => &[ARABIC],
        "hi" => &[DEVANAGARI],
        "bn" => &[BENGALI],
        "pa" => &[GURMUKHI],
        "ta" => &[TAMIL],
        "te" => &[TELUGU],
        "kn" => &[KANNADA],
        "ml" => &[MALAYALAM],
        "th" => &[THAI],
        "ka" => &[GEORGIAN],
        "am" => &[ETHIOPIC],
        "ja" => &[KANA, HAN],
        "ja_kana" | "ja-Hira" => &[KANA],
        "ko" => &[HANGUL],
        "zh" | "zh-Hans" | "zh-Hant" => &[HAN],
        _ => &[],
    }
}

/// The starts of the 256-code-point glyph ranges for `languages`, sorted:
/// Latin always, plus the scripts of the languages listed.
fn glyph_ranges(languages: Option<&[String]>) -> Result<Vec<u32>, CompileError> {
    let languages = match languages {
        Some(list) => crate::resolve_languages(list)?,
        None => Vec::new(),
    };
    let mut ranges = BTreeSet::new();
    let mut add = |blocks: Blocks| {
        for &(first, last) in blocks {
            ranges.extend((first / 256..=last / 256).map(|range| range * 256));
        }
    };
    add(LATIN);
    for language in languages {
        debug_assert!(OMT_LANGUAGES.contains(&language));
        scripts(language).iter().copied().for_each(&mut add);
    }
    Ok(ranges.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn strings(values: &[&str]) -> Vec<String> {
        values.iter().map(|v| v.to_string()).collect()
    }

    #[test]
    fn sar_layers_go_after_the_basemap_layers() {
        let style = maplibre_style("region.pmtiles", None, None);
        let layers = |style: &str| {
            let style: serde_json::Value = serde_json::from_str(style).unwrap();
            style["layers"].as_array().unwrap().clone()
        };
        let (basemap, with_sar) = (layers(&style), layers(&with_sar_layers(&style)));
        assert!(with_sar.len() > basemap.len());
        assert_eq!(with_sar[..basemap.len()], basemap[..]);
        for layer in &with_sar[basemap.len()..] {
            assert_eq!(layer["source-layer"], "sar", "{layer}");
        }
    }
    #[test]
    fn latin_ranges_are_always_written() {
        let latin = glyph_ranges(None).unwrap();
        assert_eq!(latin, [0, 256, 512, 768, 7680, 8192, 8448]);
        // Latin-script languages need nothing more.
        assert_eq!(glyph_ranges(Some(&strings(&["pl", "de"]))).unwrap(), latin);
        let cyrillic = glyph_ranges(Some(&strings(&["uk"]))).unwrap();
        assert_eq!(cyrillic, [0, 256, 512, 768, 1024, 1280, 7680, 8192, 8448]);
        let greek = glyph_ranges(Some(&strings(&["el"]))).unwrap();
        assert!(greek.contains(&7936) && greek.contains(&768), "{greek:?}");
        // Han is 82 ranges.
        let chinese = glyph_ranges(Some(&strings(&["zh"]))).unwrap();
        assert_eq!(chinese.len(), latin.len() + 82 + 2);
        assert!(glyph_ranges(Some(&strings(&["xx"]))).is_err());
    }

    #[test]
    fn every_profile_language_maps_to_known_scripts() {
        let all = glyph_ranges(Some(&strings(&["default"]))).unwrap();
        assert!(all.windows(2).all(|pair| pair[0] < pair[1]));
        assert!(all.iter().all(|start| start % 256 == 0 && *start < 65536));
        assert!(all.contains(&(0xAC00 / 256 * 256)), "Hangul");
    }

    #[test]
    fn labels_show_these_keys() {
        assert_eq!(label_keys(), ["name:latin", "name:nonlatin", "ref"]);
    }

    #[test]
    fn the_style_takes_the_given_urls() {
        let style: serde_json::Value =
            serde_json::from_str(&maplibre_style("region.pmtiles", None, Some("sprite"))).unwrap();
        assert_eq!(
            style["sources"]["openmaptiles"]["url"],
            "pmtiles://region.pmtiles"
        );
        assert_eq!(style["glyphs"], DEFAULT_GLYPHS_URL);
        assert_eq!(style["sprite"], "sprite");
        // Every font the style draws with is one the pack writes.
        let fonts = style["layers"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|layer| layer["layout"]["text-font"].as_array())
            .flatten()
            .map(|font| font.as_str().unwrap().to_owned())
            .collect::<BTreeSet<_>>();
        assert_eq!(
            fonts,
            STYLE_FONTSTACKS
                .iter()
                .map(|s| s.to_string())
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn local_paths_and_file_urls_are_read_as_paths() {
        assert_eq!(
            local_path("file:///srv/fonts/Noto%20Sans%20Bold/0-255.pbf"),
            Path::new("/srv/fonts/Noto Sans Bold/0-255.pbf")
        );
        assert_eq!(fontstack_in("/srv/{fontstack}", "Noto Sans"), "Noto Sans");
        assert_eq!(
            fontstack_in("https://x/{fontstack}", "Noto Sans"),
            "Noto%20Sans"
        );
    }

    /// A local mirror with every glyph range of one font missing but the
    /// first, and no @2x sprite.
    fn mirror(dir: &Path) -> (String, String) {
        for fontstack in STYLE_FONTSTACKS {
            let fonts = dir.join("fonts").join(fontstack);
            fs::create_dir_all(&fonts).unwrap();
            for start in glyph_ranges(None).unwrap() {
                if fontstack == "Noto Sans Italic" && start > 0 {
                    continue;
                }
                fs::write(fonts.join(format!("{}.pbf", range_name(start))), b"glyphs").unwrap();
            }
        }
        fs::write(dir.join("bright.json"), br#"{"star_11":{}}"#).unwrap();
        fs::write(dir.join("bright.png"), b"png").unwrap();
        (
            format!("file://{}/fonts/{{fontstack}}/{{range}}.pbf", dir.display()),
            dir.join("bright").display().to_string(),
        )
    }

    #[test]
    fn a_pack_from_a_mirror_has_relative_urls_and_lists_what_was_missing() {
        let temp = tempfile::tempdir().unwrap();
        let (glyphs, sprite) = mirror(&temp.path().join("mirror"));
        let mut request = StylePackRequest::new(temp.path().join("pack"), "berlin.pmtiles");
        request.glyphs = glyphs;
        request.sprite = sprite;
        let mut events = Vec::new();
        let report = write_style_pack(&request, |event| events.push(event), || false).unwrap();
        let pack = temp.path().join("pack");
        assert!(!temp.path().join("pack.part").exists());
        assert_eq!(report.style, pack.join("style.json"));
        assert_eq!(report.ranges.len(), 7);
        // 7 ranges of 3 fonts, less 6 Italic ones; 2 sprite files; style and license.
        assert_eq!(report.files, 21 - 6 + 2 + 2);
        assert_eq!(report.missing.len(), 6 + 2, "{:?}", report.missing);
        assert!(report.missing.contains(&"sprite@2x.png".to_string()));
        assert_eq!(events.first(), Some(&StylePackEvent::Started { total: 25 }));
        assert!(matches!(
            events.last(),
            Some(StylePackEvent::Fetched {
                done: 25,
                total: 25,
                ..
            })
        ));
        assert_eq!(
            fs::read(pack.join("fonts/Noto Sans Bold/8448-8703.pbf")).unwrap(),
            b"glyphs"
        );
        assert_eq!(fs::read(pack.join("sprite.png")).unwrap(), b"png");
        let style: serde_json::Value =
            serde_json::from_slice(&fs::read(pack.join("style.json")).unwrap()).unwrap();
        assert_eq!(style["glyphs"], "fonts/{fontstack}/{range}.pbf");
        assert_eq!(style["sprite"], "sprite");
        assert_eq!(
            style["sources"]["openmaptiles"]["url"],
            "pmtiles://berlin.pmtiles"
        );
        let license = fs::read_to_string(pack.join("LICENSE.md")).unwrap();
        assert!(license.contains("Open Font License") && license.contains("BSD 3-Clause"));

        // The directory now exists.
        let error = write_style_pack(&request, |_| {}, || false).unwrap_err();
        assert_eq!(error.kind(), "invalid_request", "{error}");

        request.directory = temp.path().join("based");
        request.base_url = Some("asset://maps".into());
        write_style_pack(&request, |_| {}, || false).unwrap();
        let style: serde_json::Value =
            serde_json::from_slice(&fs::read(temp.path().join("based/style.json")).unwrap())
                .unwrap();
        assert_eq!(
            style["glyphs"],
            "asset://maps/fonts/{fontstack}/{range}.pbf"
        );
        assert_eq!(style["sprite"], "asset://maps/sprite");
    }

    #[test]
    fn a_failed_or_cancelled_pack_leaves_nothing() {
        let temp = tempfile::tempdir().unwrap();
        let (glyphs, sprite) = mirror(&temp.path().join("mirror"));
        let mut request = StylePackRequest::new(temp.path().join("pack"), "a.pmtiles");
        request.glyphs = glyphs;
        request.sprite = temp.path().join("nowhere").display().to_string();
        let error = write_style_pack(&request, |_| {}, || false).unwrap_err();
        assert_eq!(error.kind(), "download", "{error}");
        // The sprite's files download at once: either required one may be
        // the first to fail.
        let message = error.message();
        assert!(
            message.contains("nowhere.json") || message.contains("nowhere.png"),
            "{error}"
        );
        request.sprite = sprite;
        let error = write_style_pack(&request, |_| {}, || true).unwrap_err();
        assert_eq!(error, CompileError::Cancelled);
        let left: Vec<_> = fs::read_dir(temp.path()).unwrap().collect();
        assert_eq!(left.len(), 1, "only the mirror: {left:?}");

        request.glyphs = "fonts/{range}.pbf".into();
        assert_eq!(request.validate().unwrap_err().kind(), "invalid_request");
    }
}
