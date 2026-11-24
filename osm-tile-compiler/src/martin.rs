//! Experimental: serving archives with [Martin](https://github.com/maplibre/martin),
//! MapLibre's tile server, instead of `serve`.
//!
//! [`write_martin_config`] writes a Martin config file that serves the
//! archives, and an OSM Bright style for each that Martin publishes beside
//! them. Nothing of Martin is linked in: Martin reads the archives as it
//! reads any PMTiles or MBTiles file. What this adds over pointing Martin at
//! the files is the style (its source names Martin's TileJSON, says
//! `"encoding": "mlt"` for MapLibre Tiles, and draws the `sar` layer when the
//! archive holds it) and a check of what Martin will refuse.
//!
//! Updates: Martin watches the directories it is given and checks each
//! PMTiles file it serves for changes about once a second, so an archive
//! replaced with [`crate::archive::apply_in_place`] (`apply --in-place`) is
//! served from then on with no restart, and one added to a directory is
//! published. The signature of a delta is checked before the archive is
//! replaced, so Martin only ever sees archives that passed it.
//!
//! What Martin does not serve: the search index, deltas, and the glyphs and
//! sprite of a style pack as `style --offline` writes them (Martin builds
//! glyphs from font files and the sprite from SVG icons, so give it those
//! with `fonts` and `sprites`). `serve` still does all of these.

use crate::CompileError;
use crate::serve::{ArchiveSummary, summarize};
use serde::Serialize;
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// The config file [`write_martin_config`] writes in its output directory.
pub const CONFIG_FILE: &str = "martin.yaml";
/// The directory beside it that holds the styles.
pub const STYLES_DIR: &str = "styles";
/// The id Martin serves the `sprites` directory's sprite under.
pub const SPRITE_ID: &str = "osm-bright";

/// What [`write_martin_config`] writes.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct MartinConfigRequest {
    /// PMTiles or MBTiles archives, or directories of them. A directory is
    /// watched by Martin: archives added to it, replaced in it or removed
    /// from it are served, or no longer, without a restart. A file named
    /// here is served under its file name without the extension.
    pub inputs: Vec<PathBuf>,
    /// Directory to write `martin.yaml` and `styles/` to; must not exist.
    pub output: PathBuf,
    /// TrueType or OpenType files of the fonts the style uses (Noto Sans
    /// Regular, Bold and Italic), for Martin to serve the glyphs from.
    /// Without it, the styles load glyphs from the network.
    pub fonts: Option<PathBuf>,
    /// SVG icons for Martin to build the sprite from (OSM Bright's icons).
    /// Without it, the styles load OSM Bright's sprite from the network.
    pub sprites: Option<PathBuf>,
}

impl MartinConfigRequest {
    pub fn new(inputs: Vec<PathBuf>, output: impl Into<PathBuf>) -> Self {
        Self {
            inputs,
            output: output.into(),
            fonts: None,
            sprites: None,
        }
    }
}

/// What [`write_martin_config`] wrote.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct MartinConfigReport {
    /// The config file, for `martin --config`.
    pub config: PathBuf,
    /// The archives found now, each with its style.
    pub sources: Vec<MartinSource>,
    /// Archives Martin will refuse or serve differently from `serve`.
    pub warnings: Vec<String>,
}

/// An archive Martin will serve.
#[derive(Debug, Clone, Serialize)]
#[non_exhaustive]
pub struct MartinSource {
    /// Martin's id for it: its tiles are `/<id>/<z>/<x>/<y>`, its TileJSON
    /// `/<id>` and its style `/style/<id>`.
    pub id: String,
    pub path: PathBuf,
    /// `pmtiles` or `mbtiles`.
    pub format: &'static str,
    /// `mvt` or `mlt`.
    pub tile_format: &'static str,
    /// Whether the archive holds the `sar` layer (its style draws it).
    pub sar: bool,
    /// The style file written for it.
    pub style: PathBuf,
}

/// Writes a Martin config that serves `request`'s archives, and a style for
/// each archive found now in `request.output`. Archives added to a watched
/// directory later are served but get no style until this runs again.
pub fn write_martin_config(
    request: &MartinConfigRequest,
) -> Result<MartinConfigReport, CompileError> {
    let invalid = CompileError::InvalidRequest;
    if request.inputs.is_empty() {
        return Err(invalid("Give at least one archive or directory".into()));
    }
    if request.output.exists() {
        return Err(invalid(format!(
            "{} already exists",
            request.output.display()
        )));
    }
    for (dir, what) in [(&request.fonts, "fonts"), (&request.sprites, "sprites")] {
        if let Some(dir) = dir
            && !dir.is_dir()
        {
            return Err(invalid(format!(
                "The {what} directory {} does not exist",
                dir.display()
            )));
        }
    }
    let mut config = Config::default();
    let mut found: Vec<(PathBuf, bool)> = Vec::new();
    for input in &request.inputs {
        let input = absolute(input)?;
        if input.is_dir() {
            let mut kind = Kind::default();
            for entry in std::fs::read_dir(&input)
                .map_err(|e| CompileError::other(format!("{}: {e}", input.display())))?
            {
                let path = entry
                    .map_err(|e| CompileError::other(format!("{}: {e}", input.display())))?
                    .path();
                if let Some(format) = archive_format(&path) {
                    kind.set(format);
                    found.push((path, true));
                }
            }
            if !kind.pmtiles && !kind.mbtiles {
                // Empty for now: watch it for PMTiles archives added later.
                kind.pmtiles = true;
            }
            if kind.pmtiles {
                config.pmtiles_dirs.push(input.clone());
            }
            if kind.mbtiles {
                config.mbtiles_dirs.push(input);
            }
        } else if archive_format(&input).is_some() {
            found.push((input, false));
        } else {
            return Err(invalid(format!(
                "{} is not a .pmtiles or .mbtiles archive, nor a directory",
                input.display()
            )));
        }
    }
    found.sort();
    let mut sources = Vec::new();
    let mut warnings = Vec::new();
    let mut styles = Vec::new();
    for (path, in_dir) in found {
        let summary = summarize(&path).map_err(invalid)?;
        let id = summary.info.name.clone();
        if let Some(other) = sources.iter().find(|s: &&MartinSource| s.id == id) {
            return Err(invalid(format!(
                "{} and {} would both be served as '{id}'; rename one",
                other.path.display(),
                path.display()
            )));
        }
        if let Some(warning) = refused(&summary) {
            warnings.push(warning);
        }
        if !in_dir {
            match summary.info.format {
                "mbtiles" => config.mbtiles_files.push((id.clone(), path.clone())),
                _ => config.pmtiles_files.push((id.clone(), path.clone())),
            }
        }
        styles.push((id.clone(), style(&summary, request)));
        sources.push(MartinSource {
            id: id.clone(),
            style: request.output.join(STYLES_DIR).join(format!("{id}.json")),
            path,
            format: summary.info.format,
            tile_format: if summary.mlt { "mlt" } else { "mvt" },
            sar: summary.sar,
        });
    }
    config.styles = absolute(&request.output.join(STYLES_DIR))?;
    config.fonts = request.fonts.as_deref().map(absolute).transpose()?;
    config.sprites = request.sprites.as_deref().map(absolute).transpose()?;

    let write = |path: &Path, text: &str| {
        std::fs::write(path, text)
            .map_err(|e| CompileError::other(format!("Failed to write {}: {e}", path.display())))
    };
    let styles_dir = request.output.join(STYLES_DIR);
    std::fs::create_dir_all(&styles_dir).map_err(|e| {
        CompileError::other(format!("Failed to create {}: {e}", styles_dir.display()))
    })?;
    for (id, style) in &styles {
        let text = serde_json::to_string_pretty(style).expect("a style serializes");
        write(&styles_dir.join(format!("{id}.json")), &text)?;
    }
    let config_path = request.output.join(CONFIG_FILE);
    write(&config_path, &config.to_yaml())?;
    Ok(MartinConfigReport {
        config: config_path,
        sources,
        warnings,
    })
}

#[derive(Default)]
struct Kind {
    pmtiles: bool,
    mbtiles: bool,
}

impl Kind {
    fn set(&mut self, format: &str) {
        match format {
            "mbtiles" => self.mbtiles = true,
            _ => self.pmtiles = true,
        }
    }
}

fn archive_format(path: &Path) -> Option<&'static str> {
    if !path.is_file() {
        return None;
    }
    match path.extension()?.to_str()? {
        "pmtiles" => Some("pmtiles"),
        "mbtiles" => Some("mbtiles"),
        _ => None,
    }
}

fn absolute(path: &Path) -> Result<PathBuf, CompileError> {
    std::path::absolute(path).map_err(|e| CompileError::other(format!("{}: {e}", path.display())))
}

/// Why Martin (2.0.0-beta.7) will not serve the archive, if it will not.
fn refused(summary: &ArchiveSummary) -> Option<String> {
    // martin-core's PMTiles source accepts compressed tiles only for MVT.
    (summary.info.format == "pmtiles" && summary.mlt && summary.encoding.is_some()).then(|| {
        format!(
            "{}: Martin refuses PMTiles archives of {} compressed MapLibre Tiles \
             (it accepts compression for MVT only); compile with --tile-compression none, \
             or with --tile-format mvt and let Martin convert (convert_to_mlt: auto)",
            summary.info.path.display(),
            summary.encoding.unwrap_or_default()
        )
    })
}

/// OSM Bright for one archive served by Martin. URLs are relative to
/// Martin's root; Martin makes them absolute for each client, from the
/// address the client reached it at.
fn style(summary: &ArchiveSummary, request: &MartinConfigRequest) -> Value {
    let mut style = crate::maplibre_style("unused", None, None);
    if summary.sar {
        style = crate::with_sar_layers(&style);
    }
    let mut style: Value = serde_json::from_str(&style).expect("the bundled style is JSON");
    let id = &summary.info.name;
    if let Some(sources) = style["sources"].as_object_mut() {
        for source in sources.values_mut() {
            if source["type"] == "vector" {
                source["url"] = format!("/{}", url_segment(id)).into();
                if summary.mlt {
                    source["encoding"] = "mlt".into();
                }
                if let Some(source) = source.as_object_mut() {
                    source.remove("tiles");
                }
            }
        }
    }
    if request.fonts.is_some() {
        style["glyphs"] = "/font/{fontstack}/{range}".into();
    }
    if request.sprites.is_some() {
        style["sprite"] = format!("/sprite/{SPRITE_ID}").into();
    }
    let center = &summary.tilejson["center"];
    if let (Some(lon), Some(lat), Some(zoom)) =
        (center[0].as_f64(), center[1].as_f64(), center[2].as_f64())
    {
        style["center"] = json!([lon, lat]);
        style["zoom"] = json!(zoom);
    }
    style
}

/// `id` as one URL path segment.
fn url_segment(id: &str) -> String {
    id.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[derive(Default)]
struct Config {
    pmtiles_dirs: Vec<PathBuf>,
    pmtiles_files: Vec<(String, PathBuf)>,
    mbtiles_dirs: Vec<PathBuf>,
    mbtiles_files: Vec<(String, PathBuf)>,
    styles: PathBuf,
    fonts: Option<PathBuf>,
    sprites: Option<PathBuf>,
}

impl Config {
    /// The config as YAML. Every scalar is written as a JSON string, which
    /// YAML reads as the same string.
    fn to_yaml(&self) -> String {
        let q = |path: &Path| Value::from(path.to_string_lossy()).to_string();
        let mut yaml = String::from(
            "# Martin config written by osm-tile-compiler martin-config.\n\
             # Run: martin --config martin.yaml\n",
        );
        let section = |yaml: &mut String,
                       name: &str,
                       dirs: &[PathBuf],
                       files: &[(String, PathBuf)]| {
            if dirs.is_empty() && files.is_empty() {
                return;
            }
            yaml.push_str(&format!("{name}:\n"));
            if !dirs.is_empty() {
                yaml.push_str("  # Watched: archives added, replaced or removed here are served at once.\n  paths:\n");
                for dir in dirs {
                    yaml.push_str(&format!("    - {}\n", q(dir)));
                }
            }
            if !files.is_empty() {
                yaml.push_str("  sources:\n");
                for (id, path) in files {
                    yaml.push_str(&format!("    {}: {}\n", Value::from(id.as_str()), q(path)));
                }
            }
        };
        section(
            &mut yaml,
            "pmtiles",
            &self.pmtiles_dirs,
            &self.pmtiles_files,
        );
        section(
            &mut yaml,
            "mbtiles",
            &self.mbtiles_dirs,
            &self.mbtiles_files,
        );
        yaml.push_str(&format!("styles:\n  paths:\n    - {}\n", q(&self.styles)));
        if let Some(fonts) = &self.fonts {
            yaml.push_str(&format!("fonts:\n  paths:\n    - {}\n", q(fonts)));
        }
        if let Some(sprites) = &self.sprites {
            yaml.push_str(&format!(
                "sprites:\n  sources:\n    {}: {}\n",
                Value::from(SPRITE_ID),
                q(sprites)
            ));
        }
        yaml
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use pmtiles::{Compression, PmTilesWriter, TileCoord, TileId, TileType};
    use std::fs::File;

    fn archive(path: &Path, tile_type: TileType, metadata: &str) {
        let mut writer = PmTilesWriter::new(tile_type)
            .precompressed(Compression::Gzip)
            .metadata(metadata)
            .create(File::create(path).unwrap())
            .unwrap();
        let mut gzip = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut gzip, b"tile").unwrap();
        writer
            .add_tile(
                TileCoord::from(TileId::new(0).unwrap()),
                &gzip.finish().unwrap(),
            )
            .unwrap();
        writer.finalize().unwrap();
    }

    #[test]
    fn writes_a_config_and_a_style_per_archive() {
        let dir = tempfile::tempdir().unwrap();
        let watched = dir.path().join("live");
        std::fs::create_dir(&watched).unwrap();
        archive(
            &watched.join("berlin.pmtiles"),
            TileType::Mvt,
            r#"{"vector_layers":[{"id":"water"},{"id":"sar"}]}"#,
        );
        let single = dir.path().join("tatras.pmtiles");
        archive(
            &single,
            TileType::Mlt,
            r#"{"vector_layers":[{"id":"water"}]}"#,
        );
        let fonts = dir.path().join("fonts");
        std::fs::create_dir(&fonts).unwrap();
        let mut request =
            MartinConfigRequest::new(vec![watched.clone(), single.clone()], dir.path().join("m"));
        request.fonts = Some(fonts);
        let report = write_martin_config(&request).unwrap();

        let ids: Vec<_> = report.sources.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, ["berlin", "tatras"]);
        assert!(report.sources[0].sar && !report.sources[1].sar);
        assert_eq!(report.sources[1].tile_format, "mlt");
        // The MLT archive is gzipped, which Martin refuses.
        assert_eq!(report.warnings.len(), 1);
        assert!(report.warnings[0].contains("tatras.pmtiles"));

        let yaml = std::fs::read_to_string(&report.config).unwrap();
        let watched = std::path::absolute(&watched).unwrap();
        assert!(
            yaml.contains(&format!(
                "  paths:\n    - {}\n",
                Value::from(watched.to_string_lossy())
            )),
            "{yaml}"
        );
        assert!(yaml.contains("  sources:\n    \"tatras\": "), "{yaml}");
        assert!(yaml.contains("fonts:\n  paths:\n"), "{yaml}");
        assert!(!yaml.contains("sprites:"), "{yaml}");

        let style: Value =
            serde_json::from_str(&std::fs::read_to_string(&report.sources[0].style).unwrap())
                .unwrap();
        assert_eq!(style["sources"]["openmaptiles"]["url"], "/berlin");
        assert_eq!(style["glyphs"], "/font/{fontstack}/{range}");
        assert!(
            style["layers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["source-layer"] == "sar")
        );
        let style: Value =
            serde_json::from_str(&std::fs::read_to_string(&report.sources[1].style).unwrap())
                .unwrap();
        assert_eq!(style["sources"]["openmaptiles"]["encoding"], "mlt");
        assert!(
            !style["layers"]
                .as_array()
                .unwrap()
                .iter()
                .any(|l| l["source-layer"] == "sar")
        );

        // Written once: the output must not exist.
        assert!(write_martin_config(&request).is_err());
    }

    #[test]
    fn refuses_inputs_that_are_not_archives() {
        let dir = tempfile::tempdir().unwrap();
        let text = dir.path().join("notes.txt");
        std::fs::write(&text, "x").unwrap();
        let request = MartinConfigRequest::new(vec![text], dir.path().join("m"));
        assert!(write_martin_config(&request).is_err());
        assert!(!dir.path().join("m").exists());
    }
}
