//! The human command line: `compile`, `building-levels`, `fetch-sources`,
//! `style`, `inspect`, `verify`, `diff`, `apply`, `update`, `help`,
//! `--version`.
//! Progress goes to stderr; stdout carries only what was asked for (help,
//! the version, `--json`). Usage errors exit with 2, failures with 1, and
//! a command stopped by Ctrl-C with 130 (see `interrupt`).
use crate::interrupt;
use osm_tile_compiler::{
    ArchiveCenter, Area, BasemapRequest, BuildingLevelsReport, ClipTo, CompileBounds, CompileError,
    CompileStage, DEFAULT_MAX_DIFFS, ExtractEvent, FetchEvent, FetchMode, LayerSelection,
    OMT_LANGUAGES as LANGUAGES, OMT_LAYERS as LAYERS, OutputFormat, ReadScope, Replication,
    SourcesPolicy, SupplementalDataset, UpdateEvent, UpdateReport, UpdateRequest, ZoomRange,
    archive::{self, ArchiveOptions, ArchiveReport, DeltaManifest},
    building_levels_path, compile_basemap, fetch_extract_from, fetch_sources as fetch_datasets,
    kib, resolve_languages, update_extract, write_building_levels_cancellable,
};
use std::ffi::OsString;
use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

const NAME: &str = "osm-tile-compiler";
/// Marks a default scratch directory (`<output>.tmp`) as this tool's.
const SCRATCH_MARKER: &str = ".osm-tile-compiler-scratch";
/// The updated PBF `compile --changes` writes in the scratch directory.
const UPDATED_PBF: &str = ".osm-tile-compiler-updated.osm.pbf";
/// OSM Bright, adapted to the archives `compile` writes (`style/LICENSE.md`).
/// `style` replaces its three URLs, each one JSON string in the template.
const STYLE: &str = include_str!("../style/osm-bright.json");
const STYLE_ARCHIVE: &str = "pmtiles://basemap.pmtiles";
/// Names the Geofabrik index `--area` reads instead of
/// [`osm_tile_compiler::GEOFABRIK_INDEX_URL`]: a mirror, or the
/// local server the tests run.
const GEOFABRIK_INDEX_VAR: &str = "OSM_TILE_COMPILER_GEOFABRIK_INDEX";
const STYLE_GLYPHS: &str = "https://demotiles.maplibre.org/font/{fontstack}/{range}.pbf";
const STYLE_SPRITE: &str = "https://openmaptiles.github.io/osm-bright-gl-style/sprite";

const USAGE: &str = "\
Usage:
  osm-tile-compiler compile <INPUT.pbf> --output <FILE.pmtiles|FILE.mbtiles> --sources <DIR> [OPTIONS]
  osm-tile-compiler compile --area <NAME> --output <FILE.pmtiles|FILE.mbtiles> --sources <DIR> [OPTIONS]
  osm-tile-compiler building-levels <INPUT.pmtiles> <OUTPUT.pmtiles> [--threads <N>]
  osm-tile-compiler fetch-sources <DIR>
  osm-tile-compiler style <ARCHIVE_URL> [--glyphs <URL>] [--sprite <URL>]
  osm-tile-compiler inspect <FILE.pmtiles> [--json]
  osm-tile-compiler verify <FILE.pmtiles> [--json]
  osm-tile-compiler diff <OLD.pmtiles> <NEW.pmtiles> --output <FILE.delta> [--json]
  osm-tile-compiler apply <OLD.pmtiles> <FILE.delta> --output <NEW.pmtiles> [--json]
  osm-tile-compiler update <INPUT.pbf> [CHANGES.osc...] --output <NEW.pbf> [OPTIONS]
  osm-tile-compiler help | --help
  osm-tile-compiler --version";

const HELP: &str = "\
Compiles an OpenStreetMap PBF extract into an OpenMapTiles-schema vector
basemap, written as one PMTiles or MBTiles archive.

Usage:
  osm-tile-compiler compile <INPUT.pbf> --output <FILE.pmtiles|FILE.mbtiles> --sources <DIR> [OPTIONS]
  osm-tile-compiler compile --area <NAME> --output <FILE.pmtiles|FILE.mbtiles> --sources <DIR> [OPTIONS]
  osm-tile-compiler building-levels <INPUT.pmtiles> <OUTPUT.pmtiles> [--threads <N>]
  osm-tile-compiler fetch-sources <DIR>
  osm-tile-compiler style <ARCHIVE_URL> [--glyphs <URL>] [--sprite <URL>]
  osm-tile-compiler inspect <FILE.pmtiles> [--json]
  osm-tile-compiler verify <FILE.pmtiles> [--json]
  osm-tile-compiler diff <OLD.pmtiles> <NEW.pmtiles> --output <FILE.delta> [--json]
  osm-tile-compiler apply <OLD.pmtiles> <FILE.delta> --output <NEW.pmtiles> [--json]
  osm-tile-compiler update <INPUT.pbf> [CHANGES.osc...] --output <NEW.pbf> [OPTIONS]
  osm-tile-compiler help | --help
  osm-tile-compiler --version

Commands:
  compile          Compile INPUT.pbf, or the Geofabrik extract of a region,
                   into a new PMTiles archive
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
  diff             Experimental: write a delta that turns OLD.pmtiles into
                   NEW.pmtiles, carrying only the parts OLD doesn't hold
  apply            Experimental: rebuild NEW.pmtiles from OLD.pmtiles and a
                   delta, byte for byte; checks both archives' SHA-256 and
                   leaves nothing behind on failure
  update           Experimental: apply OSM change files, or the diffs a
                   replication server published since INPUT.pbf, and write
                   the updated extract to NEW.pbf, without a database

Compile options:
  --area <NAME>         Instead of INPUT.pbf, compile Geofabrik's extract of a
                        region: its identifier (malopolskie, us/georgia), its
                        name (Małopolskie) or its ISO code (PL-12). It is
                        downloaded into --download-dir as <ID>.osm.pbf, or
                        the one already there is used; remove it to download
                        a newer one. OSM_TILE_COMPILER_GEOFABRIK_INDEX names
                        another index (a mirror) [default:
                        https://download.geofabrik.de/index-v1-nogeom.json]
  --download-dir <DIR>  Where --area keeps extracts [default: --sources, or
                        the current directory with --omit-sources all]
  --output <FILE>       Archive to write; must not exist yet
  --format <FORMAT>     pmtiles or mbtiles [default: mbtiles for a .mbtiles
                        --output, otherwise pmtiles]. Building levels,
                        --delta-from and the merge cache need pmtiles
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
  --profile <FILE>      Measure each stage's wall time, CPU time and thread
                        use; print a table and write the report as JSON
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
  --changes <FILE>      Experimental: apply an OSM change file (.osc, .osc.gz)
                        to the input before compiling; repeat for more, oldest
                        first. The input itself is left as it is
  --replication <SOURCE>
                        Experimental: also apply the diffs a replication
                        server published since the input; as for update
  --until <SEQ>, --max-diffs <N>, --no-clip
                        As for update
  --delta-from <OLD.pmtiles>
                        Experimental: also write <NAME>.delta beside the
                        archive, the delta that turns OLD.pmtiles into it
  --merge-cache         Experimental: also write <NAME>.merge-cache beside
                        the archive, so that a later compile can reuse its
                        tiles with --reuse-from
  --reuse-from <OLD.pmtiles>
                        Experimental: take each tile layer whose input is
                        unchanged from OLD.pmtiles, compiled with
                        --merge-cache, instead of merging and encoding it
                        again; the archive is the same as without it. Implies
                        --merge-cache

Diff and apply options:
  --output <FILE>       File to write; must not exist yet
  --json                Print the delta's manifest and size as JSON on stdout
  The delta format is experimental and may change between releases; a delta
  applies only with a build that reads its version.

Update options (experimental):
  --output <FILE>       Updated PBF to write; must not exist yet
  CHANGES.osc...        Change files to apply first, oldest first, gzipped or not
  --replication <SOURCE>
                        Also download and apply every diff published since
                        INPUT.pbf: header (the server its header names, as
                        Geofabrik extracts do), minute, hour or day (the
                        planet's servers), or a server's URL. Where to start
                        comes from the header's sequence number for its own
                        server, otherwise from its timestamp
  --until <SEQ>         Last diff to apply [default: the server's latest]
  --max-diffs <N>       Refuse to download more diffs than this [default: 2000]
  --bounds <W S E N>    Keep only changes that touch this box [default: the
                        header's box]
  --no-clip             Keep every change, for diffs already cut to the region;
                        planet diffs need the clip
  --threads <N>         Worker threads [default: available cores]
  --json                Print the update report as JSON on stdout
  Changes to objects the input holds are always kept; new objects are kept
  when they are inside the box or reach into the extract. Rewritten blocks
  carry no object metadata.

Building levels options:
  --threads <N>         Worker threads [default: available cores]; the archive
                        is the same for any number
  --profile <FILE>      Measure each stage, as for compile

Style options:
  --glyphs <URL>        Font glyphs, with {fontstack} and {range}
                        [default: https://demotiles.maplibre.org/font/{fontstack}/{range}.pbf]
  --sprite <URL>        Icon sprite, without its extension
                        [default: https://openmaptiles.github.io/osm-bright-gl-style/sprite]

Layers: {LAYERS}
Languages: {LANGUAGES}

Progress goes to stderr. Exit status: 0 on success, 1 on failure, 2 on a usage
error, 130 when stopped by Ctrl-C. Ctrl-C stops a compile and removes its
scratch space and partial outputs; press it twice to quit at once.

Apps can run this binary as a worker with
`[--parent-stdin] --request <request.json>`; that mode reports JSON lines on
stdout.";

#[derive(Debug)]
struct CompileArgs {
    input: Input,
    output: PathBuf,
    format: OutputFormat,
    sources: PathBuf,
    omit_sources: Vec<SupplementalDataset>,
    bounds: Option<CompileBounds>,
    polygon: Option<PathBuf>,
    area_only: bool,
    center: Option<ArchiveCenter>,
    archive: ArchiveOptions,
    zoom: ZoomRange,
    threads: Option<usize>,
    temp_dir: Option<PathBuf>,
    fetch: bool,
    building_levels: bool,
    json: bool,
    profile: Option<PathBuf>,
    layers: LayerSelection,
    languages: Option<Vec<String>>,
    changes: ChangeArgs,
    delta_from: Option<PathBuf>,
    merge_cache: osm_tile_compiler::MergeCacheOptions,
}

/// The change files and replication server `compile` and `update` apply.
#[derive(Debug, Default)]
struct ChangeArgs {
    files: Vec<PathBuf>,
    replication: Option<Replication>,
    until: Option<u64>,
    max_diffs: Option<u64>,
    no_clip: bool,
}

impl ChangeArgs {
    fn is_empty(&self) -> bool {
        self.files.is_empty() && self.replication.is_none()
    }

    /// Reads one of the shared options; false when `flag` is not one.
    fn parse<'a>(
        &mut self,
        flag: &str,
        args: &mut impl Iterator<Item = &'a OsString>,
    ) -> Result<bool, String> {
        match flag {
            "--changes" => self.files.push(PathBuf::from(value(args, flag)?)),
            "--replication" => {
                let source = value(args, flag)?.to_string_lossy();
                self.replication =
                    Some(Replication::parse(&source).map_err(|e| format!("{flag}: {e}"))?);
            }
            "--until" => self.until = Some(number(value(args, flag)?, flag)?),
            "--max-diffs" => self.max_diffs = Some(number(value(args, flag)?, flag)?),
            "--no-clip" => self.no_clip = true,
            _ => return Ok(false),
        }
        Ok(true)
    }

    /// The options that only mean something with changes to apply.
    fn check(&self, command: &str) -> Result<(), String> {
        if self.replication.is_none() && (self.until.is_some() || self.max_diffs.is_some()) {
            return Err(format!(
                "{command}: --until and --max-diffs need --replication"
            ));
        }
        if self.is_empty() && self.no_clip {
            return Err(format!("{command}: --no-clip needs changes to apply"));
        }
        Ok(())
    }

    fn request(&self, input: &Path, output: &Path, clip: ClipTo, threads: usize) -> UpdateRequest {
        let mut request = UpdateRequest::new(input, output);
        request.change_files = self.files.clone();
        request.replication = self.replication.clone();
        request.until = self.until;
        request.max_diffs = self.max_diffs.unwrap_or(DEFAULT_MAX_DIFFS);
        request.clip = if self.no_clip { ClipTo::Nothing } else { clip };
        request.threads = threads;
        request
    }
}

#[derive(Debug)]
struct UpdateArgs {
    input: PathBuf,
    output: PathBuf,
    changes: ChangeArgs,
    bounds: Option<CompileBounds>,
    threads: Option<usize>,
    json: bool,
}

/// What `compile` reads: a PBF, or a region's extract to download.
#[derive(Debug)]
enum Input {
    Pbf(PathBuf),
    Area { name: String, directory: PathBuf },
}

#[derive(Debug)]
struct BuildingLevelsArgs {
    input: PathBuf,
    output: PathBuf,
    threads: Option<usize>,
    profile: Option<PathBuf>,
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
    Diff(DeltaArgs),
    Apply(DeltaArgs),
    Update(Box<UpdateArgs>),
}

/// `diff` and `apply`: the old archive, then the new archive (`diff`) or the
/// delta (`apply`).
#[derive(Debug)]
struct DeltaArgs {
    base: PathBuf,
    other: PathBuf,
    output: PathBuf,
    json: bool,
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
        Command::Diff(args) => diff(args),
        Command::Apply(args) => apply(args),
        Command::Update(args) => update(*args),
    };
    match result {
        Ok(()) => 0,
        Err(message) => {
            eprintln!("error: {message}");
            if interrupt::requested() {
                interrupt::EXIT_STATUS
            } else {
                1
            }
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
        Some(name @ ("diff" | "apply")) => parse_delta_command(name, rest),
        Some("update") => parse_update(rest),
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

/// `--bounds W S E N`.
fn parse_bounds<'a>(
    args: &mut impl Iterator<Item = &'a OsString>,
    flag: &str,
) -> Result<CompileBounds, String> {
    let mut wsen = [0.0_f64; 4];
    for slot in &mut wsen {
        let arg = args.next().ok_or("--bounds needs four numbers: W S E N")?;
        *slot = number(arg, flag)?;
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
    Ok(CompileBounds::new(north, south, east, west))
}

fn parse_compile(args: &[OsString]) -> Result<Command, String> {
    let mut input = None;
    let mut area = None;
    let mut download_dir = None;
    let mut output = None;
    let mut format = None;
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
    let mut profile = None;
    let mut only_layers = None;
    let mut excluded_layers = None;
    let mut languages = None;
    let mut changes = ChangeArgs::default();
    let mut delta_from = None;
    let mut merge_cache = osm_tile_compiler::MergeCacheOptions::default();
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
            "--format" => {
                let id = value(&mut args, &flag)?.to_string_lossy().into_owned();
                format =
                    Some(OutputFormat::from_id(&id).ok_or_else(|| {
                        format!("--format must be pmtiles or mbtiles, got '{id}'")
                    })?);
            }
            "--sources" => sources = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--area" => area = Some(value(&mut args, &flag)?.to_string_lossy().into_owned()),
            "--download-dir" => download_dir = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--temp-dir" => temp_dir = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--profile" => profile = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--bounds" => bounds = Some(parse_bounds(&mut args, &flag)?),
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
                center = Some(ArchiveCenter::new(longitude, latitude, zoom as u8));
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
                resolve_languages(&list).map_err(|e| format!("--languages: {}", e.message()))?;
                languages = Some(list);
            }
            "--delta-from" => delta_from = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--merge-cache" => merge_cache.write = true,
            "--reuse-from" => {
                merge_cache.reuse_from = Some(PathBuf::from(value(&mut args, &flag)?));
                merge_cache.write = true;
            }
            _ if changes.parse(&flag, &mut args)? => {}
            _ => return Err(format!("unknown compile option '{flag}'")),
        }
    }
    let zoom = ZoomRange::new(min_zoom, max_zoom).map_err(|_| {
        format!(
            "zooms must satisfy 0 <= --min-zoom <= --max-zoom <= {}, got {min_zoom} and {max_zoom}",
            ZoomRange::MAX_ZOOM
        )
    })?;
    let (layers, flag) = match (only_layers, excluded_layers) {
        (Some(_), Some(_)) => {
            return Err("--layers and --exclude-layers cannot be combined".into());
        }
        (Some(only), None) => (LayerSelection::only(only), "--layers"),
        (None, Some(except)) => (LayerSelection::except(except), "--exclude-layers"),
        (None, None) => (LayerSelection::All, ""),
    };
    layers
        .resolve()
        .map_err(|e| format!("{flag}: {}", e.message()))?;
    if area_only && bounds.is_none() && polygon.is_none() {
        return Err("--area-only needs --bounds or --polygon".into());
    }
    changes.check("compile")?;
    if input.is_some() && area.is_some() {
        return Err("compile takes an input PBF or --area, not both".into());
    }
    if area.is_none() && download_dir.is_some() {
        return Err("--download-dir is only used with --area".into());
    }
    let input_given = input.is_some() || area.is_some();
    if !input_given {
        return Err("compile needs an input PBF or --area <NAME>".into());
    }
    let output = output.ok_or("compile needs --output <FILE.pmtiles>")?;
    if output.file_name().is_none() {
        return Err(format!(
            "--output must name a file, got {}",
            output.display()
        ));
    }
    let format = format.unwrap_or_else(|| OutputFormat::from_path(&output));
    if format != OutputFormat::PmTiles {
        if building_levels {
            return Err(format!(
                "--building-levels needs a PMTiles archive, not {}",
                format.name()
            ));
        }
        if delta_from.is_some() {
            return Err(format!(
                "--delta-from needs a PMTiles archive, not {}",
                format.name()
            ));
        }
        if merge_cache.write {
            return Err(format!(
                "--merge-cache and --reuse-from need a PMTiles archive, not {}",
                format.name()
            ));
        }
    }
    let sources = match sources {
        Some(sources) => sources,
        // Nothing is read from it.
        None if omit_sources.len() == SupplementalDataset::ALL.len() => PathBuf::new(),
        None => return Err("compile needs --sources <DIR>".into()),
    };
    let input = match (input, area) {
        (Some(path), _) => Input::Pbf(path),
        (None, Some(name)) => Input::Area {
            name,
            directory: download_dir.unwrap_or_else(|| sources.clone()),
        },
        (None, None) => unreachable!("checked above"),
    };
    Ok(Command::Compile(Box::new(CompileArgs {
        input,
        output,
        format,
        sources,
        omit_sources,
        bounds,
        polygon,
        area_only,
        center,
        archive,
        zoom,
        threads,
        temp_dir,
        fetch,
        building_levels,
        json,
        profile,
        layers,
        languages,
        changes,
        delta_from,
        merge_cache,
    })))
}

fn parse_update(args: &[OsString]) -> Result<Command, String> {
    let mut paths = Vec::new();
    let mut output = None;
    let mut changes = ChangeArgs::default();
    let mut bounds = None;
    let mut threads = None;
    let mut json = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(flag) = option(arg) else {
            paths.push(PathBuf::from(arg));
            continue;
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--output" | "-o" => output = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--bounds" => bounds = Some(parse_bounds(&mut args, &flag)?),
            "--threads" => {
                let count: usize = number(value(&mut args, &flag)?, &flag)?;
                if count == 0 {
                    return Err("--threads must be at least 1".into());
                }
                threads = Some(count);
            }
            "--json" => json = true,
            "--changes" => {
                return Err("update takes change files as arguments, not --changes".into());
            }
            _ if changes.parse(&flag, &mut args)? => {}
            _ => return Err(format!("unknown update option '{flag}'")),
        }
    }
    let mut paths = paths.into_iter();
    let input = paths.next().ok_or("update needs an input PBF")?;
    changes.files.extend(paths);
    if changes.is_empty() {
        return Err("update needs change files or --replication".into());
    }
    changes.check("update")?;
    if bounds.is_some() && changes.no_clip {
        return Err("--bounds and --no-clip cannot be combined".into());
    }
    let output = output.ok_or("update needs --output <NEW.pbf>")?;
    Ok(Command::Update(Box::new(UpdateArgs {
        input,
        output,
        changes,
        bounds,
        threads,
        json,
    })))
}

fn parse_building_levels(args: &[OsString]) -> Result<Command, String> {
    let mut paths = Vec::new();
    let mut threads = None;
    let mut profile = None;
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
            "--profile" => profile = Some(PathBuf::from(value(&mut args, &flag)?)),
            _ => return Err(format!("unknown building-levels option '{flag}'")),
        }
    }
    let mut paths = paths.into_iter();
    match (paths.next(), paths.next(), paths.next()) {
        (Some(input), Some(output), None) => Ok(Command::BuildingLevels(BuildingLevelsArgs {
            input,
            output,
            threads,
            profile,
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

fn parse_delta_command(name: &str, args: &[OsString]) -> Result<Command, String> {
    let mut paths = Vec::new();
    let mut output = None;
    let mut json = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match option(arg).as_deref() {
            Some("--help" | "-h") => return Ok(Command::Help),
            Some("--json") => json = true,
            Some("--output" | "-o") => output = Some(PathBuf::from(value(&mut args, "--output")?)),
            Some(flag) => return Err(format!("unknown {name} option '{flag}'")),
            None => paths.push(PathBuf::from(arg)),
        }
    }
    let second = if name == "diff" {
        "NEW.pmtiles"
    } else {
        "FILE.delta"
    };
    let [base, other]: [PathBuf; 2] = paths
        .try_into()
        .map_err(|_| format!("{name} takes two files: OLD.pmtiles and {second}"))?;
    let output = output.ok_or_else(|| format!("{name} needs --output"))?;
    let args = DeltaArgs {
        base,
        other,
        output,
        json,
    };
    Ok(if name == "diff" {
        Command::Diff(args)
    } else {
        Command::Apply(args)
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
            _ => {}
        },
        interrupt::requested,
    )
    .map(drop)
    .map_err(String::from)
}

fn fetch_sources(directory: &Path) -> Result<(), String> {
    let _cooperating = interrupt::cooperate();
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
    // directory in between keeps it. Unlock before closing: a process spawned
    // by another thread can hold a duplicate of the descriptor until it
    // execs, and on Unix that duplicate keeps the lock held after the close.
    let _ = marker.unlock();
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

/// Prints the stage profile and writes it as JSON to `path`.
fn write_profile(
    status: &mut Status,
    path: Option<&Path>,
    profile: Option<osm_tile_compiler::stage_profile::Report>,
) -> Result<(), String> {
    let (Some(path), Some(profile)) = (path, profile) else {
        return Ok(());
    };
    for line in profile.table().lines() {
        status.line(line);
    }
    let json = serde_json::to_string_pretty(&profile).map_err(|e| e.to_string())?;
    std::fs::write(path, json + "\n")
        .map_err(|e| format!("Failed to write {}: {e}", path.display()))?;
    status.line(&format!("Wrote the stage profile to {}", path.display()));
    Ok(())
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
    let _cooperating = interrupt::cooperate();
    if args.profile.is_some() {
        osm_tile_compiler::stage_profile::start(threads);
    }
    let report =
        write_building_levels_cancellable(&args.input, &args.output, threads, interrupt::requested);
    let profile = osm_tile_compiler::stage_profile::finish();
    let report = report.map_err(|error| match error {
        CompileError::Cancelled => {
            format!("cancelled; {} was not written", args.output.display())
        }
        error => error.to_string(),
    })?;
    report_levels(&mut status, &args.output, &report);
    write_profile(&mut status, args.profile.as_deref(), profile)
}

/// Downloads the Geofabrik extract `name` names into `directory`, or finds
/// it there, and returns its path.
fn area_extract(name: &str, directory: &Path, status: &mut Status) -> Result<PathBuf, String> {
    let mut file = String::new();
    let index_url = std::env::var(GEOFABRIK_INDEX_VAR)
        .ok()
        .filter(|url| !url.is_empty());
    fetch_extract_from(
        index_url
            .as_deref()
            .unwrap_or(osm_tile_compiler::GEOFABRIK_INDEX_URL),
        directory,
        name,
        |event| match event {
            ExtractEvent::Resolving { url } => {
                status.line(&format!("Looking up '{name}' in {url}"))
            }
            ExtractEvent::Found { extract } => {
                file = extract.file_name();
                status.line(&format!(
                    "'{name}' is Geofabrik's {} ({}{})",
                    extract.id,
                    extract.name,
                    extract
                        .parent
                        .as_deref()
                        .map(|parent| format!(", in {parent}"))
                        .unwrap_or_default()
                ))
            }
            ExtractEvent::Downloading { url } => status.line(&format!("{file}: downloading {url}")),
            ExtractEvent::Received { bytes, total } => status.update(&match total {
                Some(total) if total > 0 => format!(
                    "{file}: {} of {} ({:.0}%)",
                    megabytes(bytes),
                    megabytes(total),
                    bytes as f64 / total as f64 * 100.0
                ),
                _ => format!("{file}: {}", megabytes(bytes)),
            }),
            ExtractEvent::Ready {
                path,
                bytes,
                downloaded,
            } => status.line(&format!(
                "{}: {} ({})",
                path.display(),
                if downloaded {
                    "downloaded"
                } else {
                    "already present, not downloaded again"
                },
                megabytes(bytes)
            )),
            _ => {}
        },
        interrupt::requested,
    )
    .map_err(String::from)
}

fn compile(args: CompileArgs) -> Result<(), String> {
    // Downloads and the compile stop when asked, removing what they wrote.
    let _cooperating = interrupt::cooperate();
    let levels = building_levels_path(&args.output);
    for archive in [&args.output, &levels] {
        if archive.exists() {
            return Err(format!(
                "refusing to replace existing archive {}",
                archive.display()
            ));
        }
    }
    let delta = match &args.delta_from {
        Some(old) => {
            if !old.is_file() {
                return Err(format!("--delta-from: {} does not exist", old.display()));
            }
            let delta = args.output.with_extension("delta");
            if delta.exists() {
                return Err(format!(
                    "refusing to replace existing delta {}",
                    delta.display()
                ));
            }
            Some((old.clone(), delta))
        }
        None => None,
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
    // After the checks above, so that none fails after a long download.
    let input = match args.input {
        Input::Pbf(path) => path,
        Input::Area { name, directory } => area_extract(&name, &directory, &mut status)?,
    };
    let bounds = match args.bounds {
        Some(bounds) => bounds,
        None => pbf_bounds(&input)?,
    };
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
    // A second Ctrl-C ends the process without waiting for the compile to
    // clean up; a default scratch directory is then removed at least.
    let _scratch_on_exit = owned_temp.then(|| interrupt::remove_on_exit(temp_dir.clone()));
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let input = if args.changes.is_empty() {
        input
    } else {
        // The changes are clipped to the extract's header box, or to the
        // compiled area when the header has none.
        let path = temp_dir.join(UPDATED_PBF);
        let clip = ClipTo::HeaderBounds {
            fallback: Some(bounds),
        };
        let updated = std::fs::create_dir_all(&temp_dir)
            .map_err(|e| format!("Failed to create {}: {e}", temp_dir.display()))
            .and_then(|()| {
                // One left by an interrupted compile.
                let _ = std::fs::remove_file(&path);
                let request = args.changes.request(&input, &path, clip, threads);
                run_update(&request, &mut status)
            });
        if let Err(error) = updated {
            let _ = std::fs::remove_file(&path);
            if let Some(marker) = scratch {
                let _ = release_scratch(&temp_dir, marker);
            }
            return Err(error);
        }
        path
    };
    let updated_input = (!args.changes.is_empty()).then(|| input.clone());
    status.line(&format!(
        "Compiling {} into {} (bounds {} {} {} {}, zooms {}-{}, {threads} threads)",
        input.display(),
        args.output.display(),
        bounds.west,
        bounds.south,
        bounds.east,
        bounds.north,
        args.zoom.min(),
        args.zoom.max(),
    ));
    let mut request =
        BasemapRequest::new(input, args.output.clone(), args.sources, temp_dir.clone());
    request.format = Some(args.format);
    request.sources_policy = if args.fetch {
        SourcesPolicy::FetchMissing
    } else {
        SourcesPolicy::UsePresent
    };
    request.omit_sources = args.omit_sources;
    request.area = Area::bounds(bounds);
    request.area.polygon = args.polygon.map(Into::into);
    if args.area_only {
        request.area.read = ReadScope::NearArea;
    }
    request.center = args.center;
    request.archive = args.archive;
    request.zoom = args.zoom;
    request.threads = threads;
    request.layers = args.layers;
    request.languages = args.languages;
    request.building_levels = args.building_levels;
    request.merge_cache = args.merge_cache;
    if args.profile.is_some() {
        osm_tile_compiler::stage_profile::start(threads);
    }
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
        interrupt::requested,
    );
    let profile = osm_tile_compiler::stage_profile::finish();
    if let Some(updated) = &updated_input {
        let _ = std::fs::remove_file(updated);
    }
    let cleanup = match scratch {
        Some(marker) => release_scratch(&temp_dir, marker),
        None => Ok(()),
    };
    let output = output.map_err(|error| match error {
        CompileError::Cancelled => format!(
            "the compile was cancelled; {} was not written",
            args.output.display()
        ),
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
    if let Some(cache) = &result.merge_cache {
        status.line(&format!(
            "Merge cache: reused {} tiles and {} tile layers, encoded {} tile layers",
            cache.tiles_reused, cache.layers_reused, cache.layers_encoded
        ));
    }
    for warning in &result.warnings {
        status.line(&format!("warning: {warning}"));
    }
    if let Some((path, report)) = &output.building_levels {
        report_levels(&mut status, path, report);
    }
    write_profile(&mut status, args.profile.as_deref(), profile)?;
    if let Some((old, delta_path)) = &delta {
        let _partial = interrupt::remove_on_exit(archive::partial_path(delta_path));
        let report = archive::diff(old, &args.output, delta_path)?;
        status.line(&format!(
            "Wrote {}: {}",
            delta_path.display(),
            delta_summary(&report.manifest, report.delta_bytes)
        ));
    }
    if args.json {
        let json = serde_json::to_string_pretty(result).map_err(|e| e.to_string())?;
        println!("{json}");
    }
    Ok(())
}

fn update(args: UpdateArgs) -> Result<(), String> {
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    let clip = match args.bounds {
        Some(bounds) => ClipTo::Bounds(bounds),
        None => ClipTo::HeaderBounds { fallback: None },
    };
    let request = args
        .changes
        .request(&args.input, &args.output, clip, threads);
    let mut status = Status::new();
    let report = run_update(&request, &mut status)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    }
    Ok(())
}

/// Runs an update, reporting its progress on stderr.
fn run_update(request: &UpdateRequest, status: &mut Status) -> Result<UpdateReport, String> {
    status.line(&format!(
        "Updating {} into {}",
        request.input.display(),
        request.output.display()
    ));
    let report = update_extract(
        request,
        |event| match event {
            UpdateEvent::Scanned { objects, sorted } => status.line(&format!(
                "Read {objects} objects{}",
                if sorted {
                    ""
                } else {
                    " (not sorted by type and id: new objects go last)"
                }
            )),
            UpdateEvent::Replication {
                url,
                latest,
                first,
                last,
            } => status.line(&if first > last {
                format!("{url} is at {latest}: nothing newer than the input")
            } else {
                format!("{url} is at {latest}: applying diffs {first} to {last}")
            }),
            UpdateEvent::Read {
                name,
                index,
                total,
                report,
            } => status.update(&format!(
                "[{}/{total}] {name}: kept {} of {} changes",
                index + 1,
                report.kept,
                report.objects
            )),
            UpdateEvent::Applying { changes } => {
                status.line(&format!("Applying {changes} changes"))
            }
        },
        || false,
    )
    .map_err(String::from)?;
    status.line(&format!(
        "Wrote {} in {:.1} s: {} objects created, {} modified, {} deleted; {} blocks \
         rewritten, {} added, {} copied{}",
        report.output.display(),
        report.seconds,
        report.created,
        report.modified,
        report.deleted,
        report.blobs_rewritten,
        report.blobs_added,
        report.blobs_copied,
        report
            .diffs
            .map(|[first, last]| format!("; now at diff {last} (from {first})"))
            .unwrap_or_default()
    ));
    Ok(report)
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

fn delta_summary(manifest: &DeltaManifest, delta_bytes: u64) -> String {
    let megabytes = |bytes: u64| {
        if bytes < 1_000_000 {
            format!("{:.1} kB", bytes as f64 / 1e3)
        } else {
            megabytes(bytes)
        }
    };
    let tiles = &manifest.tiles;
    format!(
        "{} delta for a {} archive ({} copied from the old one, {} tiles patched); tiles: {} changed, {} added, {} removed, {} unchanged",
        megabytes(delta_bytes),
        megabytes(manifest.target.bytes),
        megabytes(manifest.copied_bytes),
        manifest.patched_tiles,
        tiles.changed,
        tiles.added,
        tiles.removed,
        tiles.unchanged
    )
}

fn diff(args: DeltaArgs) -> Result<(), String> {
    let _partial = interrupt::remove_on_exit(archive::partial_path(&args.output));
    let report = archive::diff(&args.base, &args.other, &args.output)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        eprintln!(
            "Wrote {}: {}",
            args.output.display(),
            delta_summary(&report.manifest, report.delta_bytes)
        );
    }
    Ok(())
}

fn apply(args: DeltaArgs) -> Result<(), String> {
    let _partial = interrupt::remove_on_exit(archive::partial_path(&args.output));
    let report = archive::apply(&args.base, &args.other, &args.output)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        eprintln!(
            "Wrote {} (SHA-256 {}): {}",
            args.output.display(),
            report.manifest.target.sha256,
            delta_summary(&report.manifest, report.delta_bytes)
        );
    }
    Ok(())
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
