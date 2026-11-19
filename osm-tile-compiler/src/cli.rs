//! The human command line: `compile`, `building-levels`, `fetch-sources`,
//! `style`, `help`, `--version`.
//! Progress goes to stderr; stdout carries only what was asked for (help,
//! the version, `--json`). Usage errors exit with 2, failures with 1.
use osm_tile_compiler::{
    ArchiveCenter, BasemapRequest, BuildingLevelsReport, CompileBounds, CompileError, CompileStage,
    FetchEvent, FetchMode, SourcesPolicy, SupplementalDataset,
    archive::{ArchiveOptions, ArchiveReport},
    building_levels_path, compile_basemap, fetch_sources as fetch_datasets,
    omt::{LANGUAGES, LAYERS, resolve_languages, select_layers},
    tile_stats::kib,
    write_building_levels,
};
use std::ffi::OsString;
use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const NAME: &str = "osm-tile-compiler";
/// Marks a default scratch directory (`<output>.tmp`) as this tool's.
const SCRATCH_MARKER: &str = ".osm-tile-compiler-scratch";
/// OSM Bright, adapted to the archives `compile` writes (`style/LICENSE.md`).
/// `style` replaces its three URLs, each one JSON string in the template.
const STYLE: &str = include_str!("../style/osm-bright.json");
const STYLE_ARCHIVE: &str = "pmtiles://basemap.pmtiles";
const STYLE_GLYPHS: &str = "https://demotiles.maplibre.org/font/{fontstack}/{range}.pbf";
const STYLE_SPRITE: &str = "https://openmaptiles.github.io/osm-bright-gl-style/sprite";

const USAGE: &str = "\
Usage:
  osm-tile-compiler compile <INPUT.pbf> --output <FILE.pmtiles> --sources <DIR> [OPTIONS]
  osm-tile-compiler building-levels <INPUT.pmtiles> <OUTPUT.pmtiles> [--threads <N>]
  osm-tile-compiler fetch-sources <DIR>
  osm-tile-compiler style <ARCHIVE_URL> [--glyphs <URL>] [--sprite <URL>]
  osm-tile-compiler inspect <FILE.pmtiles> [--json]
  osm-tile-compiler verify <FILE.pmtiles> [--json]
  osm-tile-compiler help | --help
  osm-tile-compiler --version";

const HELP: &str = "\
Compiles an OpenStreetMap PBF extract into an OpenMapTiles-schema vector
basemap, written as one PMTiles archive.

Usage:
  osm-tile-compiler compile <INPUT.pbf> --output <FILE.pmtiles> --sources <DIR> [OPTIONS]
  osm-tile-compiler building-levels <INPUT.pmtiles> <OUTPUT.pmtiles> [--threads <N>]
  osm-tile-compiler fetch-sources <DIR>
  osm-tile-compiler style <ARCHIVE_URL> [--glyphs <URL>] [--sprite <URL>]
  osm-tile-compiler inspect <FILE.pmtiles> [--json]
  osm-tile-compiler verify <FILE.pmtiles> [--json]
  osm-tile-compiler help | --help
  osm-tile-compiler --version

Commands:
  compile          Compile INPUT.pbf into a new PMTiles archive
  building-levels  Write the building levels of a compiled archive to a new
                   PMTiles archive: its buildings as boxes for far views
  fetch-sources    Download, or verify, the supplemental datasets into DIR:
                   lake_centerline.shp.zip, natural_earth_vector.sqlite.zip and
                   water-polygons-split-3857.zip
  style            Print a MapLibre style (OSM Bright) for an archive that
                   MapLibre loads from ARCHIVE_URL through the PMTiles protocol;
                   'pmtiles://' is prepended unless present
  inspect          Describe a PMTiles archive: header, metadata, tile sizes per
                   zoom and the largest tiles, and any problems 'verify' finds
  verify           Check a PMTiles archive's layout and every tile's MVT
                   encoding; exits with 1 if anything is wrong

Compile options:
  --output <FILE>       Archive to write; must not exist yet
  --sources <DIR>       Directory holding the supplemental datasets
  --bounds <W S E N>    Area to compile, in degrees [default: the PBF header's box];
                        W > E crosses the antimeridian
  --polygon <FILE>      Write only the tiles that a polygon covers, read from
                        GeoJSON or an Osmosis .poly file
  --area-only           Read only the OSM data in and near the area: a small
                        area compiles from a country extract in seconds. The
                        highest zoom's tiles are as in a full read; lower
                        zooms show only the area's surroundings
  --center <LON LAT Z>  Where viewers start [default: the middle of the area,
                        at the highest zoom that shows all of it]
  --min-zoom <N>        Lowest zoom, 0-14 [default: 0]
  --max-zoom <N>        Highest zoom, 0-14 [default: 14]
  --threads <N>         Worker threads [default: available cores]
  --temp-dir <DIR>      Scratch space [default: <FILE>.tmp, removed afterwards]
  --fetch               Download any supplemental dataset missing from --sources
  --building-levels     Also write the building levels beside the archive, as
                        <NAME>-buildings.pmtiles
  --omit-sources <A,B,...>
                        Compile without these supplemental datasets:
                        water_polygons, natural_earth, lake_centerlines, or
                        all (then --sources is not needed). The archive's
                        metadata lists them as omitted_sources
  --json                Print the compile result as JSON on stdout
  --name <TEXT>         Archive metadata name [default: OpenMapTiles]
  --description <TEXT>  Archive metadata description
  --attribution <HTML>  Attribution viewers show; must keep crediting
                        OpenMapTiles and OpenStreetMap [default: both]
  --gzip-level <N>      Tile compression, 0 (none) to 9 (smallest) [default: 6]
  --layers <A,B,...>    Write only these layers [default: all]; each is the
                        same as in a full build
  --exclude-layers <A,B,...>
                        Write every layer but these
  --languages <A,B,...> Write name:<lang> attributes for these languages only
                        [default: all the schema lists]; 'default' adds them
                        all, '-xx' leaves one out (so 'default,-ru'), and an
                        empty list writes none. name, name_en, name_de,
                        name_int, name:latin and name:nonlatin stay

Building levels options:
  --threads <N>         Worker threads [default: available cores]; the archive
                        is the same for any number

Style options:
  --glyphs <URL>        Font glyphs, with {fontstack} and {range}
                        [default: https://demotiles.maplibre.org/font/{fontstack}/{range}.pbf]
  --sprite <URL>        Icon sprite, without its extension
                        [default: https://openmaptiles.github.io/osm-bright-gl-style/sprite]

Layers: {LAYERS}
Languages: {LANGUAGES}

Progress goes to stderr. Exit status: 0 on success, 1 on failure, 2 on a usage
error.

Apps can run this binary as a worker with
`[--parent-stdin] --request <request.json>`; that mode reports JSON lines on
stdout.";

#[derive(Debug)]
struct CompileArgs {
    input: PathBuf,
    output: PathBuf,
    sources: PathBuf,
    omit_sources: Vec<SupplementalDataset>,
    bounds: Option<CompileBounds>,
    polygon: Option<PathBuf>,
    area_only: bool,
    center: Option<ArchiveCenter>,
    archive: ArchiveOptions,
    min_zoom: u8,
    max_zoom: u8,
    threads: Option<usize>,
    temp_dir: Option<PathBuf>,
    fetch: bool,
    building_levels: bool,
    json: bool,
    layers: Option<Vec<String>>,
    languages: Option<Vec<String>>,
}

#[derive(Debug)]
struct BuildingLevelsArgs {
    input: PathBuf,
    output: PathBuf,
    threads: Option<usize>,
}

#[derive(Debug)]
struct StyleArgs {
    archive: String,
    glyphs: Option<String>,
    sprite: Option<String>,
}

#[derive(Debug)]
enum Command {
    Help,
    Version,
    Compile(Box<CompileArgs>),
    BuildingLevels(BuildingLevelsArgs),
    FetchSources(PathBuf),
    Style(StyleArgs),
    Inspect { archive: PathBuf, json: bool },
    Verify { archive: PathBuf, json: bool },
}

/// Runs the command line on `args` (without the program name) and returns the
/// process exit status.
pub fn main(args: Vec<OsString>) -> i32 {
    let command = match parse(&args) {
        Ok(command) => command,
        Err(message) => {
            eprintln!("error: {message}\n\n{USAGE}\n\nRun '{NAME} --help' for details.");
            return 2;
        }
    };
    let result = match command {
        Command::Help => {
            println!(
                "{}",
                HELP.replace("{LAYERS}", &wrapped(&LAYERS))
                    .replace("{LANGUAGES}", &wrapped(LANGUAGES))
            );
            Ok(())
        }
        Command::Version => {
            println!("{NAME} {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Compile(args) => compile(*args),
        Command::BuildingLevels(args) => building_levels(args),
        Command::FetchSources(directory) => fetch_sources(&directory),
        Command::Style(args) => {
            print!("{}", style(args));
            Ok(())
        }
        Command::Inspect { archive, json } => inspect(&archive, json),
        Command::Verify { archive, json } => verify(&archive, json),
    };
    match result {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("error: {message}");
            1
        }
    }
}

fn parse(args: &[OsString]) -> Result<Command, String> {
    let Some(first) = args.first() else {
        return Err("no command given".into());
    };
    let rest = &args[1..];
    match first.to_str() {
        Some("help" | "--help" | "-h") => Ok(Command::Help),
        Some("--version" | "-V") => Ok(Command::Version),
        Some("compile") => parse_compile(rest),
        Some("building-levels") => parse_building_levels(rest),
        Some("fetch-sources") => parse_fetch_sources(rest),
        Some("style") => parse_style(rest),
        Some(name @ ("inspect" | "verify")) => parse_archive_command(name, rest),
        _ => Err(format!("unknown command '{}'", first.to_string_lossy())),
    }
}

/// An option is anything starting with `-`; there are no positional values
/// that could, since `--bounds` consumes its four numbers itself.
fn option(arg: &OsString) -> Option<String> {
    let text = arg.to_string_lossy();
    text.starts_with('-').then(|| text.into_owned())
}

fn value<'a>(
    args: &mut impl Iterator<Item = &'a OsString>,
    flag: &str,
) -> Result<&'a OsString, String> {
    args.next().ok_or_else(|| format!("{flag} needs a value"))
}

/// A comma-separated list; spaces around items and empty items are ignored.
fn list(arg: &OsString) -> Vec<String> {
    arg.to_string_lossy()
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(String::from)
        .collect()
}

/// `names` joined by commas, in lines that fit the help's width after the
/// label they follow.
fn wrapped(names: &[&str]) -> String {
    let mut text = String::new();
    let mut width = 11;
    for (index, name) in names.iter().enumerate() {
        let item = if index + 1 < names.len() {
            format!("{name},")
        } else {
            name.to_string()
        };
        if width + 1 + item.len() > 79 {
            text.push_str("\n  ");
            width = 2;
        } else if index > 0 {
            text.push(' ');
            width += 1;
        }
        text.push_str(&item);
        width += item.len();
    }
    text
}

fn number<T: std::str::FromStr>(arg: &OsString, flag: &str) -> Result<T, String> {
    let text = arg.to_string_lossy();
    text.parse()
        .map_err(|_| format!("{flag} expects a number, got '{text}'"))
}

fn parse_compile(args: &[OsString]) -> Result<Command, String> {
    let mut input = None;
    let mut output = None;
    let mut sources = None;
    let mut bounds = None;
    let mut polygon = None;
    let mut area_only = false;
    let mut center = None;
    let mut archive = ArchiveOptions::default();
    let mut min_zoom = 0;
    let mut max_zoom = 14;
    let mut threads = None;
    let mut temp_dir = None;
    let mut fetch = false;
    let mut building_levels = false;
    let mut omit_sources = Vec::new();
    let mut json = false;
    let mut only_layers = None;
    let mut excluded_layers = None;
    let mut languages = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(flag) = option(arg) else {
            if input.is_some() {
                return Err(format!(
                    "unexpected argument '{}': compile takes one input PBF",
                    arg.to_string_lossy()
                ));
            }
            input = Some(PathBuf::from(arg));
            continue;
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--output" => output = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--sources" => sources = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--temp-dir" => temp_dir = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--bounds" => {
                let mut wsen = [0.0_f64; 4];
                for slot in &mut wsen {
                    let arg = args.next().ok_or("--bounds needs four numbers: W S E N")?;
                    *slot = number(arg, &flag)?;
                }
                let [west, south, east, north] = wsen;
                if !wsen.iter().all(|v| v.is_finite())
                    || west == east
                    || south >= north
                    || west.abs() > 180.0
                    || east.abs() > 180.0
                {
                    return Err(format!(
                        "--bounds must be W S E N within -180..180 with S < N and W different \
                         from E (W > E crosses the antimeridian), got {west} {south} {east} \
                         {north}"
                    ));
                }
                bounds = Some(CompileBounds::new(north, south, east, west));
            }
            "--polygon" => polygon = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--area-only" => area_only = true,
            "--name" => archive.name = value(&mut args, &flag)?.to_string_lossy().into_owned(),
            "--description" => {
                archive.description = value(&mut args, &flag)?.to_string_lossy().into_owned()
            }
            "--attribution" => {
                archive.attribution = value(&mut args, &flag)?.to_string_lossy().into_owned()
            }
            "--gzip-level" => {
                archive.gzip_level = number(value(&mut args, &flag)?, &flag)?;
                if archive.gzip_level > 9 {
                    return Err(format!(
                        "--gzip-level must be 0-9, got {}",
                        archive.gzip_level
                    ));
                }
            }
            "--center" => {
                let mut values = [0.0_f64; 3];
                for slot in &mut values {
                    let arg = args
                        .next()
                        .ok_or("--center needs three numbers: LON LAT ZOOM")?;
                    *slot = number(arg, &flag)?;
                }
                let [longitude, latitude, zoom] = values;
                if zoom.fract() != 0.0 || !(0.0..=14.0).contains(&zoom) {
                    return Err(format!(
                        "--center zoom must be a whole number 0-14, got {zoom}"
                    ));
                }
                center = Some(ArchiveCenter {
                    longitude,
                    latitude,
                    zoom: zoom as u8,
                });
            }
            "--min-zoom" => min_zoom = number(value(&mut args, &flag)?, &flag)?,
            "--max-zoom" => max_zoom = number(value(&mut args, &flag)?, &flag)?,
            "--threads" => {
                let count: usize = number(value(&mut args, &flag)?, &flag)?;
                if count == 0 {
                    return Err("--threads must be at least 1".into());
                }
                threads = Some(count);
            }
            "--fetch" => fetch = true,
            "--building-levels" => building_levels = true,
            "--omit-sources" => {
                omit_sources.clear();
                for id in list(value(&mut args, &flag)?) {
                    if id == "all" {
                        omit_sources.extend(SupplementalDataset::ALL);
                        continue;
                    }
                    let dataset = SupplementalDataset::from_id(&id).ok_or_else(|| {
                        format!(
                            "--omit-sources: unknown dataset '{id}'; the datasets are {}, or all",
                            SupplementalDataset::ALL
                                .map(SupplementalDataset::id)
                                .join(", ")
                        )
                    })?;
                    omit_sources.push(dataset);
                }
                omit_sources.sort();
                omit_sources.dedup();
            }
            "--json" => json = true,
            "--layers" => only_layers = Some(list(value(&mut args, &flag)?)),
            "--exclude-layers" => excluded_layers = Some(list(value(&mut args, &flag)?)),
            "--languages" => {
                let list = list(value(&mut args, &flag)?);
                resolve_languages(&list).map_err(|e| format!("--languages: {e}"))?;
                languages = Some(list);
            }
            _ => return Err(format!("unknown compile option '{flag}'")),
        }
    }
    if max_zoom > 14 || min_zoom > max_zoom {
        return Err(format!(
            "zooms must satisfy 0 <= --min-zoom <= --max-zoom <= 14, got {min_zoom} and {max_zoom}"
        ));
    }
    if only_layers.is_some() && excluded_layers.is_some() {
        return Err("--layers and --exclude-layers cannot be combined".into());
    }
    let layers =
        select_layers(only_layers.as_deref(), excluded_layers.as_deref()).map_err(|e| {
            let flag = if only_layers.is_some() {
                "--layers"
            } else {
                "--exclude-layers"
            };
            format!("{flag}: {e}")
        })?;
    let input = input.ok_or("compile needs an input PBF")?;
    let output = output.ok_or("compile needs --output <FILE.pmtiles>")?;
    if output.file_name().is_none() {
        return Err(format!(
            "--output must name a file, got {}",
            output.display()
        ));
    }
    let sources = match sources {
        Some(sources) => sources,
        // Nothing is read from it.
        None if omit_sources.len() == SupplementalDataset::ALL.len() => PathBuf::new(),
        None => return Err("compile needs --sources <DIR>".into()),
    };
    Ok(Command::Compile(Box::new(CompileArgs {
        input,
        output,
        sources,
        omit_sources,
        bounds,
        polygon,
        area_only,
        center,
        archive,
        min_zoom,
        max_zoom,
        threads,
        temp_dir,
        fetch,
        building_levels,
        json,
        layers,
        languages,
    })))
}

fn parse_building_levels(args: &[OsString]) -> Result<Command, String> {
    let mut paths = Vec::new();
    let mut threads = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(flag) = option(arg) else {
            paths.push(PathBuf::from(arg));
            continue;
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--threads" => {
                let count: usize = number(value(&mut args, &flag)?, &flag)?;
                if count == 0 {
                    return Err("--threads must be at least 1".into());
                }
                threads = Some(count);
            }
            _ => return Err(format!("unknown building-levels option '{flag}'")),
        }
    }
    let mut paths = paths.into_iter();
    match (paths.next(), paths.next(), paths.next()) {
        (Some(input), Some(output), None) => Ok(Command::BuildingLevels(BuildingLevelsArgs {
            input,
            output,
            threads,
        })),
        (_, _, Some(extra)) => Err(format!(
            "unexpected argument '{}': building-levels takes two archives",
            extra.display()
        )),
        _ => Err("building-levels needs an input and an output archive".into()),
    }
}

fn parse_fetch_sources(args: &[OsString]) -> Result<Command, String> {
    let mut directory = None;
    for arg in args {
        match option(arg).as_deref() {
            Some("--help" | "-h") => return Ok(Command::Help),
            Some(flag) => return Err(format!("unknown fetch-sources option '{flag}'")),
            None if directory.is_some() => {
                return Err(format!(
                    "unexpected argument '{}': fetch-sources takes one directory",
                    arg.to_string_lossy()
                ));
            }
            None => directory = Some(PathBuf::from(arg)),
        }
    }
    Ok(Command::FetchSources(
        directory.ok_or("fetch-sources needs a directory")?,
    ))
}

fn parse_archive_command(name: &str, args: &[OsString]) -> Result<Command, String> {
    let mut archive = None;
    let mut json = false;
    for arg in args {
        match option(arg).as_deref() {
            Some("--help" | "-h") => return Ok(Command::Help),
            Some("--json") => json = true,
            Some(flag) => return Err(format!("unknown {name} option '{flag}'")),
            None if archive.is_some() => {
                return Err(format!(
                    "unexpected argument '{}': {name} takes one archive",
                    arg.to_string_lossy()
                ));
            }
            None => archive = Some(PathBuf::from(arg)),
        }
    }
    let archive = archive.ok_or_else(|| format!("{name} needs an archive"))?;
    Ok(if name == "inspect" {
        Command::Inspect { archive, json }
    } else {
        Command::Verify { archive, json }
    })
}

fn parse_style(args: &[OsString]) -> Result<Command, String> {
    let mut archive = None;
    let mut glyphs = None;
    let mut sprite = None;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(flag) = option(arg) else {
            if archive.is_some() {
                return Err(format!(
                    "unexpected argument '{}': style takes one archive URL",
                    arg.to_string_lossy()
                ));
            }
            archive = Some(arg.to_string_lossy().into_owned());
            continue;
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--glyphs" => {
                let url = value(&mut args, &flag)?.to_string_lossy().into_owned();
                if !url.contains("{fontstack}") || !url.contains("{range}") {
                    return Err(format!(
                        "--glyphs must contain {{fontstack}} and {{range}}, got '{url}'"
                    ));
                }
                glyphs = Some(url);
            }
            "--sprite" => sprite = Some(value(&mut args, &flag)?.to_string_lossy().into_owned()),
            _ => return Err(format!("unknown style option '{flag}'")),
        }
    }
    Ok(Command::Style(StyleArgs {
        archive: archive.ok_or("style needs an archive URL")?,
        glyphs,
        sprite,
    }))
}

/// Progress on stderr. On a terminal, transient updates (download counters)
/// rewrite one line; elsewhere they are printed at most every five seconds.
struct Status {
    terminal: bool,
    transient: bool,
    last: Instant,
}

impl Status {
    fn new() -> Self {
        Self {
            terminal: std::io::stderr().is_terminal(),
            transient: false,
            last: Instant::now(),
        }
    }

    fn line(&mut self, text: &str) {
        if self.transient {
            eprint!("\r\x1b[K");
            self.transient = false;
        }
        eprintln!("{text}");
        self.last = Instant::now();
    }

    fn update(&mut self, text: &str) {
        if self.terminal {
            eprint!("\r\x1b[K{text}");
            let _ = std::io::stderr().flush();
            self.transient = true;
        } else if self.last.elapsed() >= Duration::from_secs(5) {
            self.line(text);
        }
    }
}

fn megabytes(bytes: u64) -> String {
    format!("{:.1} MB", bytes as f64 / 1e6)
}

/// Fetches every dataset into `directory`, reporting each on `status`.
fn fetch(directory: &Path, status: &mut Status) -> Result<(), String> {
    fetch_datasets(
        directory,
        &SupplementalDataset::ALL,
        FetchMode::Verify,
        |event| match event {
            FetchEvent::Checking { dataset, .. } => {
                status.update(&format!("{}: verifying", dataset.file_name()))
            }
            FetchEvent::Downloading { dataset, url } => {
                status.line(&format!("{}: downloading {url}", dataset.file_name()))
            }
            FetchEvent::Received {
                dataset,
                bytes,
                total,
            } => status.update(&match total {
                Some(total) if total > 0 => format!(
                    "{}: {} of {} ({:.0}%)",
                    dataset.file_name(),
                    megabytes(bytes),
                    megabytes(total),
                    bytes as f64 / total as f64 * 100.0
                ),
                _ => format!("{}: {}", dataset.file_name(), megabytes(bytes)),
            }),
            FetchEvent::Ready {
                dataset,
                bytes,
                downloaded,
            } => status.line(&format!(
                "{}: {} ({})",
                dataset.file_name(),
                if downloaded {
                    "downloaded"
                } else {
                    "up to date"
                },
                megabytes(bytes)
            )),
        },
        || false,
    )
    .map(drop)
    .map_err(String::from)
}

fn fetch_sources(directory: &Path) -> Result<(), String> {
    let mut status = Status::new();
    status.line(&format!(
        "Fetching supplemental datasets into {}",
        directory.display()
    ));
    fetch(directory, &mut status)?;
    status.line(&format!(
        "Supplemental datasets ready in {}",
        directory.display()
    ));
    Ok(())
}

/// The style template with the archive, and any glyph or sprite URL given,
/// substituted; replacing the JSON strings keeps the template's layout.
fn style(args: StyleArgs) -> String {
    let archive = if args.archive.starts_with("pmtiles://") {
        args.archive
    } else {
        format!("pmtiles://{}", args.archive)
    };
    let quoted = |url: &str| serde_json::Value::from(url).to_string();
    [
        (STYLE_ARCHIVE, Some(archive)),
        (STYLE_GLYPHS, args.glyphs),
        (STYLE_SPRITE, args.sprite),
    ]
    .into_iter()
    .fold(STYLE.to_owned(), |style, (default, url)| match url {
        Some(url) => style.replacen(&quoted(default), &quoted(&url), 1),
        None => style,
    })
}

fn pbf_bounds(input: &Path) -> Result<CompileBounds, String> {
    let index = osm_pbf_ingest::blob_index::PbfBlobIndex::scan(input)
        .map_err(|e| format!("Failed to inspect {}: {e}", input.display()))?;
    let b = index.header().bounds.ok_or_else(|| {
        format!(
            "{} has no bounding box in its header; pass --bounds W S E N",
            input.display()
        )
    })?;
    // A box crossing the antimeridian has west above east, or east past 180.
    Ok(CompileBounds::new(b.north, b.south, b.east, b.west).normalized())
}

/// Creates the default scratch directory, or reuses one an interrupted
/// compile left, and returns its marker file, locked until
/// `release_scratch`.
fn claim_scratch(directory: &Path, status: &mut Status) -> Result<File, String> {
    let marker_path = directory.join(SCRATCH_MARKER);
    if directory.exists() && !marker_path.is_file() {
        return Err(format!(
            "{} already exists and is not an {NAME} scratch directory; \
             remove it or pass --temp-dir",
            directory.display()
        ));
    }
    let marker = std::fs::create_dir_all(directory)
        .and_then(|()| {
            File::options()
                .create(true)
                .truncate(false)
                .write(true)
                .open(&marker_path)
        })
        .map_err(|e| format!("Failed to create {}: {e}", directory.display()))?;
    match marker.try_lock() {
        Ok(()) => {}
        Err(std::fs::TryLockError::WouldBlock) => {
            return Err(format!(
                "{} is in use by another compile of the same output; \
                 wait for it or pass --temp-dir",
                directory.display()
            ));
        }
        Err(std::fs::TryLockError::Error(e)) => {
            return Err(format!("Failed to lock {}: {e}", marker_path.display()));
        }
    }
    if remove_all_but(directory, &marker_path)
        .map_err(|e| format!("Failed to clear {}: {e}", directory.display()))?
    {
        status.line(&format!(
            "Removed what an interrupted compile left in {}",
            directory.display()
        ));
    }
    Ok(marker)
}

/// Empties the scratch directory while its marker is still locked, then
/// removes the marker and the directory.
fn release_scratch(directory: &Path, marker: File) -> Result<(), String> {
    let marker_path = directory.join(SCRATCH_MARKER);
    remove_all_but(directory, &marker_path)
        .map_err(|e| format!("Failed to remove {}: {e}", directory.display()))?;
    // Windows cannot remove an open file. A compile that claims the empty
    // directory in between keeps it.
    drop(marker);
    let _ = std::fs::remove_file(&marker_path);
    let _ = std::fs::remove_dir(directory);
    Ok(())
}

/// Removes everything in `directory` except `keep`; true if there was any.
fn remove_all_but(directory: &Path, keep: &Path) -> std::io::Result<bool> {
    let mut removed = false;
    for entry in std::fs::read_dir(directory)? {
        let path = entry?.path();
        if path == keep {
            continue;
        }
        if path.is_dir() {
            std::fs::remove_dir_all(&path)?;
        } else {
            std::fs::remove_file(&path)?;
        }
        removed = true;
    }
    Ok(removed)
}

/// Reports what was written, on `status`.
fn report_levels(status: &mut Status, path: &Path, report: &BuildingLevelsReport) {
    status.line(&format!(
        "Wrote {} in {:.1} s",
        path.display(),
        report.seconds
    ));
    for level in &report.levels {
        status.line(&format!(
            "  z{}: {} tiles, {} boxes, {} raw, {} gzipped, {:.1}% of the area",
            level.zoom,
            level.tiles,
            level.boxes,
            megabytes(level.raw_bytes),
            megabytes(level.gz_bytes),
            level.area_kept * 100.0
        ));
    }
}

fn building_levels(args: BuildingLevelsArgs) -> Result<(), String> {
    let mut status = Status::new();
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    status.line(&format!(
        "Writing the building levels of {} into {} ({threads} threads)",
        args.input.display(),
        args.output.display()
    ));
    let report = write_building_levels(&args.input, &args.output, threads)?;
    report_levels(&mut status, &args.output, &report);
    Ok(())
}

fn compile(args: CompileArgs) -> Result<(), String> {
    let levels = building_levels_path(&args.output);
    for archive in [&args.output, &levels] {
        if archive.exists() {
            return Err(format!(
                "refusing to replace existing archive {}",
                archive.display()
            ));
        }
    }
    let bounds = match args.bounds {
        Some(bounds) => bounds,
        None => pbf_bounds(&args.input)?,
    };
    if let Some(polygon) = &args.polygon {
        // Read here too, so a bad polygon fails before any download.
        osm_tile_compiler::CompileArea::read(polygon)?;
    }
    let mut status = Status::new();
    if !args.fetch {
        let missing = SupplementalDataset::ALL
            .into_iter()
            .filter(|dataset| !args.omit_sources.contains(dataset))
            .filter(|dataset| !dataset.is_present(&args.sources))
            .map(SupplementalDataset::file_name)
            .collect::<Vec<_>>();
        if !missing.is_empty() {
            return Err(format!(
                "{} is missing supplemental datasets: {}. Download them with \
                 '{NAME} fetch-sources {}', pass --fetch, or leave them out with \
                 --omit-sources",
                args.sources.display(),
                missing.join(", "),
                args.sources.display()
            ));
        }
    }
    for dataset in &args.omit_sources {
        status.line(&format!(
            "Leaving out {}: {}",
            dataset.id(),
            dataset.without_warning()
        ));
    }
    // A default scratch directory is ours alone, and it is removed afterwards
    // whether or not the compile succeeds. Its marker file, locked for the
    // whole compile, tells one left by an interrupted compile, which is
    // reused, from one in use, and from anything else, which are refused.
    let owned_temp = args.temp_dir.is_none();
    let temp_dir = args.temp_dir.unwrap_or_else(|| {
        let mut name = args.output.file_name().unwrap_or_default().to_os_string();
        name.push(".tmp");
        args.output.with_file_name(name)
    });
    let scratch = if owned_temp {
        Some(claim_scratch(&temp_dir, &mut status)?)
    } else {
        None
    };
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    status.line(&format!(
        "Compiling {} into {} (bounds {} {} {} {}, zooms {}-{}, {threads} threads)",
        args.input.display(),
        args.output.display(),
        bounds.west,
        bounds.south,
        bounds.east,
        bounds.north,
        args.min_zoom,
        args.max_zoom,
    ));
    let mut request = BasemapRequest::new(
        args.input,
        args.output.clone(),
        args.sources,
        temp_dir.clone(),
    );
    request.sources_policy = if args.fetch {
        SourcesPolicy::FetchMissing
    } else {
        SourcesPolicy::UsePresent
    };
    request.omit_sources = args.omit_sources;
    request.bounds = Some(bounds);
    request.polygon = args.polygon;
    request.area_only = args.area_only;
    request.center = args.center;
    request.archive = args.archive;
    request.min_zoom = args.min_zoom;
    request.max_zoom = args.max_zoom;
    request.threads = threads;
    request.layers = args.layers;
    request.languages = args.languages;
    request.building_levels = args.building_levels;
    let start = Instant::now();
    let mut stage = None;
    let output = compile_basemap(
        &request,
        |progress| {
            if progress.stage == CompileStage::FetchSources {
                let percent = progress
                    .fraction
                    .map(|f| format!(" {:.0}%", f * 100.0))
                    .unwrap_or_default();
                status.update(&format!("{}{percent}", progress.message));
            } else if stage != Some(progress.stage) {
                status.line(&format!(
                    "[{:>7.1}s] {}",
                    start.elapsed().as_secs_f64(),
                    progress.message
                ));
            }
            stage = Some(progress.stage);
        },
        || false,
    );
    let cleanup = match scratch {
        Some(marker) => release_scratch(&temp_dir, marker),
        None => Ok(()),
    };
    let output = output.map_err(|error| match error {
        CompileError::Cancelled => "the compile was cancelled".to_string(),
        error => error.to_string(),
    })?;
    cleanup?;
    let result = &output.result;
    status.line(&format!(
        "Wrote {}: {} tiles, {} features in {:.1} s",
        args.output.display(),
        result.tiles,
        result.profile_features,
        result.total_seconds
    ));
    if let Some(largest) = result.tile_stats.largest.first() {
        status.line(&format!(
            "Largest tile: {} at {}",
            largest.tile,
            kib(largest.bytes)
        ));
    }
    for warning in &result.warnings {
        status.line(&format!("warning: {warning}"));
    }
    if let Some((path, report)) = &output.building_levels {
        report_levels(&mut status, path, report);
    }
    if args.json {
        let json = serde_json::to_string_pretty(result).map_err(|e| e.to_string())?;
        println!("{json}");
    }
    Ok(())
}

fn inspect(path: &Path, json: bool) -> Result<(), String> {
    let report = osm_tile_compiler::archive::inspect(path)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    print!("{}", describe(path, &report));
    Ok(())
}

fn verify(path: &Path, json: bool) -> Result<(), String> {
    let report = osm_tile_compiler::archive::inspect(path)?;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    }
    if report.problem_count == 0 {
        if !json {
            println!(
                "{}: valid, {} tiles checked",
                path.display(),
                report.tile_stats.tiles()
            );
        }
        return Ok(());
    }
    if !json {
        for problem in &report.problems {
            eprintln!("problem: {problem}");
        }
    }
    let unlisted = report.problem_count - report.problems.len() as u64;
    Err(format!(
        "{} has {} problems{}",
        path.display(),
        report.problem_count,
        if unlisted > 0 {
            format!(" ({unlisted} not listed)")
        } else {
            String::new()
        }
    ))
}

/// `inspect`'s text: the header, metadata, size statistics and problems.
fn describe(path: &Path, report: &ArchiveReport) -> String {
    use std::fmt::Write as _;
    let mut text = String::new();
    let [west, south, east, north] = report.bounds;
    let (longitude, latitude, zoom) = report.center;
    let _ = writeln!(
        text,
        "{}: {:.1} MB, {} tiles ({}), zooms {}-{}{}",
        path.display(),
        report.file_bytes as f64 / 1e6,
        report.tile_type,
        report.tile_compression,
        report.zooms[0],
        report.zooms[1],
        if report.clustered { ", clustered" } else { "" }
    );
    let _ = writeln!(text, "Bounds: {west} {south} {east} {north}");
    let _ = writeln!(text, "Center: {longitude} {latitude} at zoom {zoom}");
    let _ = writeln!(
        text,
        "Tiles: {} addressed, {} entries, {} distinct",
        report.addressed_tiles, report.tile_entries, report.tile_contents
    );
    let metadata = &report.metadata;
    let layers = metadata["vector_layers"]
        .as_array()
        .map(|layers| {
            layers
                .iter()
                .filter_map(|layer| layer["id"].as_str())
                .collect::<Vec<_>>()
                .join(", ")
        })
        .unwrap_or_default();
    let _ = writeln!(
        text,
        "Metadata: {} ({}); layers: {layers}",
        metadata["name"].as_str().unwrap_or("no name"),
        metadata["version"].as_str().unwrap_or("no version"),
    );
    let _ = writeln!(
        text,
        "\nzoom    tiles       total      p50      p90      p99      max"
    );
    let number = |bytes: u64| format!("{:.1}", bytes as f64 / 1024.0);
    for zoom in &report.tile_stats.zooms {
        let _ = writeln!(
            text,
            "{:>4} {:>8} {:>11} {:>8} {:>8} {:>8} {:>8}",
            zoom.zoom,
            zoom.tiles,
            number(zoom.bytes),
            number(zoom.p50_bytes),
            number(zoom.p90_bytes),
            number(zoom.p99_bytes),
            number(zoom.max_bytes),
        );
    }
    let _ = writeln!(text, "(KiB as stored)\n\nLargest tiles:");
    for tile in &report.tile_stats.largest {
        let layers = tile
            .layers
            .iter()
            .take(3)
            .map(|(name, bytes)| format!("{name} {}", kib(*bytes)))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(
            text,
            "  {:<16} {:>12}  ({layers})",
            tile.tile,
            kib(tile.bytes)
        );
    }
    if report.problem_count == 0 {
        let _ = writeln!(text, "\nNo problems found.");
    } else {
        let _ = writeln!(text, "\n{} problems:", report.problem_count);
        for problem in &report.problems {
            let _ = writeln!(text, "  {problem}");
        }
    }
    text
}
