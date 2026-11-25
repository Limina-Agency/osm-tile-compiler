//! The human command line: `compile`, `building-levels`, `hd-roads`, `search-index`,
//! `search`, `fetch-sources`, `style`, `inspect`, `verify`, `extract`, `diff`,
//! `apply`, `update`, `serve`, `martin-config`, `keygen`, `sign`, `verify-signature`,
//! `completions`, `help`, `--version`.
//! Progress goes to stderr; stdout carries only what was asked for (help,
//! the version, `--json`). Usage errors exit with 2, failures with 1, and
//! a command stopped by Ctrl-C with 130 (see `interrupt`).
mod martin;
mod serve;

use crate::interrupt;
use osm_tile_compiler::{
    ArchiveCenter, Area, BasemapRequest, BuildingLevelsReport, ChangedTilesRequest, ClipTo,
    CompileBounds, CompileError, CompileStage, DEFAULT_MAX_DIFFS, ExtractEvent, FetchEvent,
    FetchMode, LayerSelection, OMT_LANGUAGES as LANGUAGES, OMT_LAYERS as LAYERS, OutputFormat,
    ReadScope, Replication, SourcesPolicy, StylePackEvent, StylePackRequest, SupplementalDataset,
    UpdateEvent, UpdateReport, UpdateRequest, ZoomRange,
    archive::{self, ArchiveOptions, ArchiveReport, DeltaManifest, PublicKey, SecretKey},
    building_levels_path, compile_basemap, fetch_extract_from, fetch_sources as fetch_datasets,
    kib, maplibre_style, resolve_languages, update_extract, with_sar_layers,
    write_building_levels_cancellable, write_hd_roads, write_style_pack,
};
use std::ffi::OsString;
use std::fs::File;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

mod data_layers;
mod search;

const NAME: &str = "osm-tile-compiler";
/// Marks a default scratch directory (`<output>.tmp`) as this tool's.
const SCRATCH_MARKER: &str = ".osm-tile-compiler-scratch";
/// The updated PBF `compile --changes` writes in the scratch directory.
const UPDATED_PBF: &str = ".osm-tile-compiler-updated.osm.pbf";
/// Names the Geofabrik index `--area` reads instead of
/// [`osm_tile_compiler::GEOFABRIK_INDEX_URL`]: a mirror, or the
/// local server the tests run.
const GEOFABRIK_INDEX_VAR: &str = "OSM_TILE_COMPILER_GEOFABRIK_INDEX";

pub(crate) const USAGE: &str = "\
Usage:
  osm-tile-compiler compile <INPUT.pbf>... --output <FILE.pmtiles|FILE.mbtiles> --sources <DIR> [OPTIONS]
  osm-tile-compiler compile --area <NAME> --output <FILE.pmtiles|FILE.mbtiles> --sources <DIR> [OPTIONS]
  osm-tile-compiler compile --request <REQUEST.json> [--threads <N>] [--json]
  osm-tile-compiler building-levels <INPUT.pmtiles> <OUTPUT.pmtiles> [--threads <N>]
  osm-tile-compiler search-index <INPUT.pmtiles> [--output <FILE.idx>] [--threads <N>] [--json]
  osm-tile-compiler search <FILE.idx|FILE.pmtiles> <QUERY>... [--near <LON LAT>] [OPTIONS]
  osm-tile-compiler hd-roads <INPUT.pbf> <OUTPUT.pmtiles> [--threads <N>]
  osm-tile-compiler fetch-sources <DIR>
  osm-tile-compiler schema verify <FILE.yml|shortbread> [--json]
  osm-tile-compiler schema layers <FILE.yml|shortbread|openmaptiles>
  osm-tile-compiler style <ARCHIVE_URL> [--glyphs <URL>] [--sprite <URL>] [--offline <DIR>] [--sar] [--tile-format <F>]
  osm-tile-compiler inspect <FILE.pmtiles> [--json]
  osm-tile-compiler data-info <FILE>[,<KEY>=<VALUE>...] [--json]
  osm-tile-compiler verify <FILE.pmtiles> [--json]
  osm-tile-compiler extract <INPUT.pmtiles|URL> --output <FILE.pmtiles> [--bounds <W S E N>] [--polygon <FILE>] [OPTIONS]
  osm-tile-compiler diff <OLD.pmtiles> <NEW.pmtiles> --output <FILE.delta> [--sign <KEY>] [--changed-tiles <FILE>] [--json]
  osm-tile-compiler apply <OLD.pmtiles> <FILE.delta> (--output <NEW.pmtiles> | --in-place) [--trusted-key <KEY>]... [--json]
  osm-tile-compiler update <INPUT.pbf> [CHANGES.osc...] --output <NEW.pbf> [OPTIONS]
  osm-tile-compiler serve <ARCHIVE>... [--style-pack <DIR>] [--host <ADDR>] [--port <N>]
  osm-tile-compiler martin-config <ARCHIVE|DIR>... --output <DIR> [--fonts <DIR>] [--sprites <DIR>] [--json]
  osm-tile-compiler keygen <FILE.key>
  osm-tile-compiler sign <FILE> --key <FILE.key> [--json]
  osm-tile-compiler verify-signature <FILE> [--trusted-key <KEY>]... [--signature <FILE.sig>] [--json]
  osm-tile-compiler completions <bash|zsh|fish>
  osm-tile-compiler help | --help
  osm-tile-compiler --version";

pub(crate) const HELP: &str = "\
Compiles an OpenStreetMap PBF extract into an OpenMapTiles-schema vector
basemap, written as one PMTiles or MBTiles archive.

Usage:
  osm-tile-compiler compile <INPUT.pbf>... --output <FILE.pmtiles|FILE.mbtiles> --sources <DIR> [OPTIONS]
  osm-tile-compiler compile --area <NAME> --output <FILE.pmtiles|FILE.mbtiles> --sources <DIR> [OPTIONS]
  osm-tile-compiler compile --request <REQUEST.json> [--threads <N>] [--json]
  osm-tile-compiler building-levels <INPUT.pmtiles> <OUTPUT.pmtiles> [--threads <N>]
  osm-tile-compiler search-index <INPUT.pmtiles> [--output <FILE.idx>] [--threads <N>] [--json]
  osm-tile-compiler search <FILE.idx|FILE.pmtiles> <QUERY>... [--near <LON LAT>] [OPTIONS]
  osm-tile-compiler hd-roads <INPUT.pbf> <OUTPUT.pmtiles> [--threads <N>]
  osm-tile-compiler fetch-sources <DIR>
  osm-tile-compiler schema verify <FILE.yml|shortbread> [--json]
  osm-tile-compiler schema layers <FILE.yml|shortbread|openmaptiles>
  osm-tile-compiler style <ARCHIVE_URL> [--glyphs <URL>] [--sprite <URL>] [--offline <DIR>] [--sar] [--tile-format <F>]
  osm-tile-compiler inspect <FILE.pmtiles> [--json]
  osm-tile-compiler data-info <FILE>[,<KEY>=<VALUE>...] [--json]
  osm-tile-compiler verify <FILE.pmtiles> [--json]
  osm-tile-compiler extract <INPUT.pmtiles|URL> --output <FILE.pmtiles> [--bounds <W S E N>] [--polygon <FILE>] [OPTIONS]
  osm-tile-compiler diff <OLD.pmtiles> <NEW.pmtiles> --output <FILE.delta> [--sign <KEY>] [--changed-tiles <FILE>] [--json]
  osm-tile-compiler apply <OLD.pmtiles> <FILE.delta> (--output <NEW.pmtiles> | --in-place) [--trusted-key <KEY>]... [--json]
  osm-tile-compiler update <INPUT.pbf> [CHANGES.osc...] --output <NEW.pbf> [OPTIONS]
  osm-tile-compiler serve <ARCHIVE>... [--style-pack <DIR>] [--host <ADDR>] [--port <N>]
  osm-tile-compiler martin-config <ARCHIVE|DIR>... --output <DIR> [--fonts <DIR>] [--sprites <DIR>] [--json]
  osm-tile-compiler keygen <FILE.key>
  osm-tile-compiler sign <FILE> --key <FILE.key> [--json]
  osm-tile-compiler verify-signature <FILE> [--trusted-key <KEY>]... [--signature <FILE.sig>] [--json]
  osm-tile-compiler completions <bash|zsh|fish>
  osm-tile-compiler help | --help
  osm-tile-compiler --version

Commands:
  compile          Compile INPUT.pbf (several are merged first, for an area
                   across extracts), or the Geofabrik extract of a region,
                   into a new PMTiles archive
  building-levels  Write the building levels of a compiled archive to a new
                   PMTiles archive: its buildings as boxes for far views
  search-index     Write the search index of a compiled archive beside it, as
                   <NAME>-search.idx: its places, POIs, streets, peaks, waters
                   and sar features by name, for search with no network
  search           Find names in a search index (or the one beside an
                   archive), ignoring case and diacritics; the last word
                   matches as a prefix
  hd-roads         Experimental: write the roads of an extract at their true
                   width, with their lane markings, to a new PMTiles archive
                   of z16 vector tiles
  fetch-sources    Download, or verify, the supplemental datasets into DIR:
                   lake_centerline.shp.zip, natural_earth_vector.sqlite.zip and
                   water-polygons-split-3857.zip
  schema           Check a tile schema (docs/schemas.md): 'verify' runs the
                   examples it lists and exits with 1 if any fails; 'layers'
                   lists its layers
  style            Print a MapLibre style (OSM Bright) for an archive that
                   MapLibre loads from ARCHIVE_URL through the PMTiles protocol;
                   'pmtiles://' is prepended unless present. With --offline,
                   write it to a directory with its fonts and icons instead
  inspect          Describe a PMTiles archive: header, metadata, tile sizes per
                   zoom and the largest tiles, and any problems 'verify' finds
  data-info        Describe a vector data file as compile --data reads it:
                   its format, coordinate system, features, bounds in WGS84
                   and attributes; takes the same keys as --data
  verify           Check a PMTiles archive's layout and every tile's MVT or
                   MLT encoding; exits with 1 if anything is wrong
  extract          Cut an area out of a PMTiles archive into a new one: the
                   tiles within a box, a polygon and zooms, copied byte for
                   byte. The archive may be an http:// or https:// URL; it
                   is read by range requests, so only the parts the area
                   needs are downloaded
  diff             Experimental: write a delta that turns OLD.pmtiles into
                   NEW.pmtiles, carrying only the parts OLD doesn't hold
  apply            Experimental: rebuild NEW.pmtiles from OLD.pmtiles and a
                   delta, byte for byte; checks both archives' SHA-256 and
                   leaves nothing behind on failure
  update           Experimental: apply OSM change files, or the diffs a
                   replication server published since INPUT.pbf, and write
                   the updated extract to NEW.pbf, without a database
  serve            Serve archives (PMTiles or MBTiles) over HTTP for a
                   preview: tiles, TileJSON, a MapLibre style for each, and
                   the glyphs and sprite of an offline style pack
  martin-config    Experimental: write a Martin tile server config that
                   serves the archives (and any later added to a directory
                   given), with an OSM Bright style for each
  keygen           Write a new ed25519 signing key to FILE.key and its public
                   key to FILE.key.pub, for sign, diff --sign and
                   --trusted-key
  sign             Sign FILE with a key: a delta in place, in its footer;
                   any other file, such as an archive, in FILE.sig beside it.
                   Signatures by other keys are kept
  verify-signature Check FILE's signatures (a delta's own, or FILE.sig's):
                   exits with 1 unless every signature matches and, with
                   --trusted-key, one is by a trusted key
  completions      Print a completion script for bash, zsh or fish:
                   'source <(osm-tile-compiler completions bash)' in
                   ~/.bashrc, the same with zsh in ~/.zshrc, or
                   'osm-tile-compiler completions fish | source' in fish

Compile options:
  --request <FILE>      Compile what a worker request asks for (the JSON the
                        worker mode below reads), printing progress as the
                        command line does, and write its manifest where the
                        request says. Only --threads and --json go with it
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
  --min-zoom <N>        Lowest zoom, 0-16 [default: 0]
  --max-zoom <N>        Highest zoom, 0-16 [default: 14]. Its tiles keep
                        every detail; viewers overzoom them. Each zoom above
                        14 takes about four times the tiles and time
  --threads <N>         Worker threads [default: available cores]
  --sort-memory <MIB>   Memory the tile sort may hold before it writes runs
                        to scratch, at least 8 MiB per thread [default: half
                        of the memory available when rendering starts]
  --temp-dir <DIR>      Scratch space [default: <FILE>.tmp, removed afterwards]
  --resume              Make the compile resumable: it records each stage it
                        completes in the scratch space and keeps them when it
                        is interrupted (killed, crashed, Ctrl-C, out of
                        space). Run the same command again, with --resume, to
                        continue from the last stage done; the archive is the
                        same as an uninterrupted compile's. Not with --changes
                        or --replication
  --fetch               Download any supplemental dataset missing from --sources
  --building-levels     Also write the building levels beside the archive, as
                        <NAME>-buildings.pmtiles
  --search-index        Also write the search index beside the archive, as
                        <NAME>-search.idx (see search-index)
  --sar                 Also write the search-and-rescue layer 'sar' into the
                        archive: shelters, water, emergency points, landing
                        sites, hazards and graded trails (docs/sar.md). The
                        OpenMapTiles layers stay the same
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
  --tile-format <F>     mvt, or mlt for MapLibre Tiles, about 20% smaller
                        (MapLibre GL JS 5.12+, Android 13.4+, iOS 6.28+, given
                        encoding mlt on the source) [default: mvt]
  --tile-compression <C>
                        gzip, or none to store each tile as encoded, for
                        servers that refuse compressed MLT, such as Martin
                        2.0 (PMTiles only; not with --merge-cache or
                        --reuse-from) [default: gzip]
  --schema <S>          The tile schema: openmaptiles, shortbread, or a YAML
                        file in Planetiler's custom map format
                        (docs/schemas.md) [default: openmaptiles]. With
                        another schema, --layers names its layers, only the
                        supplemental datasets it reads are needed, and
                        --sar, --languages, --building-levels,
                        --search-index and the merge cache do not apply
  --layers <A,B,...>    Write only these layers [default: all]; each is the
                        same as in a full build. Naming sar writes it, as
                        --sar does
  --exclude-layers <A,B,...>
                        Write every layer but these
  --languages <A,B,...> Write name:<lang> attributes for these languages only
                        [default: all the schema lists]; 'default' adds them
                        all, '-xx' leaves one out (so 'default,-ru'), and an
                        empty list writes none. name, name_en, name_de,
                        name_int, name:latin and name:nonlatin stay
  --data <LAYER>=<FILE>[,<KEY>=<VALUE>...]
                        Also write the features of FILE as the layer LAYER:
                        GeoJSON (.geojson, .json), newline-delimited GeoJSON
                        (.geojsonl, .ndjson), CSV (.csv, .tsv), Shapefile
                        (.shp), GeoPackage (.gpkg), FlatGeobuf (.fgb) or
                        GeoParquet (.parquet). Repeat for more layers. Keys:
                        format, table (GeoPackage), crs (EPSG:<code>, a PROJ
                        string or WKT, over what the file says), zoom=<MIN>-<MAX>
                        or min-zoom and max-zoom, attributes=<A|B> (keep
                        only these), exclude=<A|B>, rename=<FROM:TO|...>,
                        id (the attribute for feature ids), attribution, and
                        for CSV lon, lat, geometry (WKT or hex WKB) and
                        delimiter. Coordinates are reprojected to WGS84
  --data-layers <FILE.json>
                        Also write the data layers a JSON file lists: an
                        array of objects with layer, source (path, format,
                        crs, ...), min_zoom, max_zoom and attributes, as
                        docs/data-layers.md describes
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
  --sign-delta <FILE.key>
                        Sign the --delta-from delta with this key
  --merge-cache         Experimental: also write <NAME>.merge-cache beside
                        the archive, so that a later compile can reuse its
                        tiles with --reuse-from
  --reuse-from <OLD.pmtiles>
                        Experimental: take each tile layer whose input is
                        unchanged from OLD.pmtiles, compiled with
                        --merge-cache, instead of merging and encoding it
                        again; the archive is the same as without it. Implies
                        --merge-cache

Extract options:
  --output <FILE>       Archive to write; must not exist yet
  --bounds <W S E N>    Keep the tiles that meet this box, in degrees; W > E
                        crosses the antimeridian [default: the archive's]
  --polygon <FILE>      Keep the tiles that meet a polygon, read from GeoJSON
                        or an Osmosis .poly file
  --min-zoom <N>        Lowest zoom to keep [default: the archive's]
  --max-zoom <N>        Highest zoom to keep [default: the archive's]
  --json                Print the extract report as JSON on stdout
  The tiles kept are those a compile limited to the same box or polygon
  writes. Tiles are not cut: features in kept tiles keep their full
  geometry, as in the source. A URL's server must answer range requests,
  as any server PMTiles viewers read from does.

Diff and apply options:
  --output <FILE>       File to write; must not exist yet
  --sign <FILE.key>     diff: sign the delta with this key (see keygen)
  --changed-tiles <FILE>
                        diff: also write the tiles that differ between the
                        archives (changed, added or removed), one z/x/y per
                        line; must not exist yet
  --trusted-key <KEY>   apply: require a signature by this public key, given
                        as 64 hex digits or a .pub file; repeat to trust
                        several. The signature is checked before anything is
                        written. Without it, an unsigned delta applies, but
                        one whose signature does not match never does
  --in-place            apply: replace OLD.pmtiles with the result instead of
                        writing --output. It is written beside it and renamed
                        over it once checked, so a server reading it, such as
                        Martin, sees the old archive or the new one, never a
                        mix; on failure OLD.pmtiles is left as it was
  --json                Print the delta's manifest and size as JSON on stdout
  The delta format is experimental and may change between releases; a delta
  applies only with a build that reads its version.

Signing options:
  --key <FILE.key>      sign: the secret key keygen wrote
  --trusted-key <KEY>   verify-signature: as for apply
  --signature <FILE>    verify-signature: the signature file of a file that is
                        not a delta [default: FILE.sig]
  --json                Print what was checked as JSON on stdout

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
  --changed-tiles <FILE>
                        Also write the tiles the changes reach, one z/x/y per
                        line, as osm2pgsql's expire lists are: every tile a
                        changed object, a way whose node moved or a
                        multipolygon whose member changed reaches, before or
                        after, with a quarter tile around it for labels; must
                        not exist yet
  --min-zoom <N>, --max-zoom <N>
                        Zooms --changed-tiles lists, 0-16 [default: 0 and 14]
  --json                Print the update report as JSON on stdout
  Changes to objects the input holds are always kept; new objects are kept
  when they are inside the box or reach into the extract. Rewritten blocks
  carry no object metadata.

Serve options:
  --style-pack <DIR>    A pack 'style --offline' wrote: its style is served for
                        every archive, with its glyphs and sprite, so the map
                        needs no network [default: none, glyphs and sprite
                        from the network]
  --host <ADDR>         Address to listen on; 0.0.0.0 for other devices on the
                        network too [default: 127.0.0.1]
  --port <N>            Port to listen on; 0 picks a free one [default: 8080]
  Each archive is served under its file name without the extension:
  /styles/<NAME>.json (its style), /tiles/<NAME>.json (TileJSON),
  /tiles/<NAME>/<Z>/<X>/<Y>.mvt (.mlt for MapLibre Tiles) and /maps/<NAME>,
  a page that shows the map (it loads MapLibre GL JS from unpkg.com). Runs
  until Ctrl-C.

Martin config options (experimental):
  ARCHIVE|DIR...        PMTiles or MBTiles archives, or directories of them.
                        Martin watches a directory: archives added, replaced
                        (apply --in-place) or removed there are served at once
  --output <DIR>        Directory to write martin.yaml and styles/<NAME>.json
                        to; must not exist yet
  --fonts <DIR>         TrueType or OpenType fonts (Noto Sans Regular, Bold and
                        Italic) for Martin to serve the glyphs from [default:
                        glyphs from the network]
  --sprites <DIR>       SVG icons for Martin to build the sprite from [default:
                        OSM Bright's sprite from the network]
  --json                Print what was written as JSON on stdout
  Give DIR/martin.yaml to martin as its config file. Each archive's style is
  /style/<NAME>; its tiles are /<NAME>/<Z>/<X>/<Y>. The styles name Martin by
  relative URLs, which Martin makes absolute for each client.

Building levels options:
  --threads <N>         Worker threads [default: available cores]; the archive
                        is the same for any number
  --profile <FILE>      Measure each stage, as for compile

Search index options:
  --output <FILE>       Index to write; must not exist yet [default:
                        <NAME>-search.idx beside the archive]
  --threads <N>         Worker threads [default: available cores]; the index
                        is the same for any number
  --json                Print what the index holds as JSON on stdout

Search options:
  --near <LON LAT>      Rank nearer hits first among equally good ones, and
                        print their distance
  --within <W S E N>    Only hits inside this box
  --layers <A,B,...>    Only hits from these layers: place, poi, sar,
                        mountain_peak, transportation_name, waterway,
                        water_name, park, aerodrome_label
  --limit <N>           Most hits to print [default: 10]
  --json                Print the hits as JSON on stdout

Style options:
  --glyphs <URL>        Font glyphs, with {fontstack} and {range}
                        [default: https://demotiles.maplibre.org/font/{fontstack}/{range}.pbf]
  --sprite <URL>        Icon sprite, without its extension
                        [default: https://openmaptiles.github.io/osm-bright-gl-style/sprite]
  --offline <DIR>       Write an offline style pack to DIR, which must not exist:
                        style.json, the glyph ranges of the three Noto Sans
                        fonts under fonts/, the sprite and LICENSE.md. The
                        glyphs and sprite are fetched once from --glyphs and
                        --sprite (URLs, file:// URLs or local paths), and the
                        style points at them with relative URLs
  --archive-file <FILE> With --offline: also write every glyph range the
                        archive's labels use, read from this local copy of it
                        [default: ARCHIVE_URL, when it is a local file]. The
                        Latin ranges are always written, enough for name:latin
  --languages <A,B,...> With --offline: also write glyphs for these languages'
                        scripts, as compile's --languages reads them ('uk' for
                        Cyrillic, 'default' for every script)
  --base-url <URL>      With --offline: put URL before the style's glyph and
                        sprite paths, for hosts that need absolute URLs
                        (MapLibre Native: file://, asset:// or https://)
  --sar                 Also draw the 'sar' layer, above the basemap, for an
                        archive compiled with --sar
  --tile-format <F>     The archive's tiles: mvt, or mlt to set encoding mlt
                        on the source [default: mvt]

Layers: {LAYERS}; and sar, only when asked for
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
    /// More extracts to merge with `input`.
    extra_inputs: Vec<PathBuf>,
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
    sort_memory: Option<u64>,
    temp_dir: Option<PathBuf>,
    fetch: bool,
    /// Keep checkpoints when interrupted, and continue from them.
    resume: bool,
    building_levels: bool,
    search_index: bool,
    sar: bool,
    json: bool,
    profile: Option<PathBuf>,
    layers: LayerSelection,
    languages: Option<Vec<String>>,
    changes: ChangeArgs,
    delta_from: Option<PathBuf>,
    sign_delta: Option<PathBuf>,
    merge_cache: osm_tile_compiler::MergeCacheOptions,
    schema: osm_tile_compiler::TileSchema,
    data_layers: Vec<osm_tile_compiler::DataLayer>,
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
    changed_tiles: Option<ChangedTilesRequest>,
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
struct HdRoadsArgs {
    input: PathBuf,
    output: PathBuf,
    threads: Option<usize>,
}

#[derive(Debug)]
struct StyleArgs {
    archive: String,
    glyphs: Option<String>,
    sprite: Option<String>,
    offline: Option<PathBuf>,
    archive_file: Option<PathBuf>,
    languages: Option<Vec<String>>,
    base_url: Option<String>,
    sar: bool,
    tile_format: osm_tile_compiler::TileFormat,
}

#[derive(Debug)]
enum Command {
    Help,
    Version,
    Compile(Box<CompileArgs>),
    CompileRequest(RequestArgs),
    BuildingLevels(BuildingLevelsArgs),
    SearchIndex(search::SearchIndexArgs),
    Search(search::SearchArgs),
    HdRoads(HdRoadsArgs),
    FetchSources(PathBuf),
    SchemaVerify {
        schema: osm_tile_compiler::TileSchema,
        json: bool,
    },
    SchemaLayers(osm_tile_compiler::TileSchema),
    Style(StyleArgs),
    Inspect {
        archive: PathBuf,
        json: bool,
    },
    DataInfo {
        spec: String,
        json: bool,
    },
    Verify {
        archive: PathBuf,
        json: bool,
    },
    Extract(Box<ExtractArgs>),
    Diff(DeltaArgs),
    Apply(DeltaArgs),
    Update(Box<UpdateArgs>),
    Serve(serve::ServeArgs),
    MartinConfig(martin::MartinArgs),
    Completions(String),
    Keygen(PathBuf),
    Sign {
        file: PathBuf,
        key: PathBuf,
        json: bool,
    },
    VerifySignature {
        file: PathBuf,
        trusted: Vec<PublicKey>,
        signature: Option<PathBuf>,
        json: bool,
    },
}

/// `compile --request <FILE>`.
#[derive(Debug)]
struct RequestArgs {
    request: PathBuf,
    threads: Option<usize>,
    json: bool,
}

#[derive(Debug)]
struct ExtractArgs {
    input: PathBuf,
    output: PathBuf,
    bounds: Option<CompileBounds>,
    polygon: Option<PathBuf>,
    min_zoom: Option<u8>,
    max_zoom: Option<u8>,
    json: bool,
}

/// `diff` and `apply`: the old archive, then the new archive (`diff`) or the
/// delta (`apply`).
#[derive(Debug)]
struct DeltaArgs {
    base: PathBuf,
    other: PathBuf,
    output: PathBuf,
    json: bool,
    /// `diff --sign`.
    sign: Option<PathBuf>,
    /// `diff --changed-tiles`.
    changed_tiles: Option<PathBuf>,
    /// `apply --trusted-key`.
    trusted: Vec<PublicKey>,
    /// `apply --in-place`: `output` is `base`, replaced atomically.
    in_place: bool,
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
        Command::CompileRequest(args) => compile_request(args),
        Command::BuildingLevels(args) => building_levels(args),
        Command::SearchIndex(args) => search::search_index(args),
        Command::Search(args) => search::search(args),
        Command::HdRoads(args) => hd_roads(args),
        Command::FetchSources(directory) => fetch_sources(&directory),
        Command::SchemaVerify { schema, json } => schema_verify(&schema, json),
        Command::SchemaLayers(schema) => {
            match schema.configured() {
                None => LAYERS.iter().for_each(|layer| println!("{layer}")),
                Some(schema) => schema.layers().iter().for_each(|layer| println!("{layer}")),
            }
            Ok(())
        }
        Command::Style(args) => style(args),
        Command::Inspect { archive, json } => inspect(&archive, json),
        Command::DataInfo { spec, json } => data_layers::info(&spec, json),
        Command::Verify { archive, json } => verify(&archive, json),
        Command::Extract(args) => extract(*args),
        Command::Diff(args) => diff(args),
        Command::Apply(args) => apply(args),
        Command::Update(args) => update(*args),
        Command::Serve(args) => serve::run(args),
        Command::MartinConfig(args) => martin::run(args),
        Command::Keygen(path) => keygen(&path),
        Command::Sign { file, key, json } => sign(&file, &key, json),
        Command::VerifySignature {
            file,
            trusted,
            signature,
            json,
        } => verify_signature(&file, &trusted, signature.as_deref(), json),
        Command::Completions(shell) => {
            let script = crate::completions::script(&shell, NAME).expect("checked when parsed");
            print!("{script}");
            Ok(())
        }
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

/// The usage error `args` give, if any. For the completions' test.
#[cfg(test)]
pub(crate) fn usage_error(args: &[OsString]) -> Option<String> {
    parse(args).err()
}

fn parse(args: &[OsString]) -> Result<Command, String> {
    let Some(first) = args.first() else {
        return Err("no command given".into());
    };
    let rest = &args[1..];
    match first.to_str() {
        Some("help" | "--help" | "-h") => Ok(Command::Help),
        Some("--version" | "-V") => Ok(Command::Version),
        Some("compile") if rest.iter().any(|arg| arg == "--request") => parse_compile_request(rest),
        Some("compile") => parse_compile(rest),
        Some("building-levels") => parse_building_levels(rest),
        Some("search-index") => search::parse_search_index(rest),
        Some("search") => search::parse_search(rest),
        Some("hd-roads") => parse_hd_roads(rest),
        Some("fetch-sources") => parse_fetch_sources(rest),
        Some("schema") => parse_schema(rest),
        Some("style") => parse_style(rest),
        Some(name @ ("inspect" | "verify")) => parse_archive_command(name, rest),
        Some("data-info") => parse_data_info(rest),
        Some("extract") => parse_extract(rest),
        Some(name @ ("diff" | "apply")) => parse_delta_command(name, rest),
        Some("update") => parse_update(rest),
        Some("serve") => serve::parse(rest),
        Some("martin-config") => martin::parse(rest),
        Some("keygen") => parse_keygen(rest),
        Some(name @ ("sign" | "verify-signature")) => parse_signature_command(name, rest),
        Some("completions") => parse_completions(rest),
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

/// `compile --request <FILE> [--threads <N>] [--json]`: everything else
/// comes from the request.
fn parse_compile_request(args: &[OsString]) -> Result<Command, String> {
    let mut request = None;
    let mut threads = None;
    let mut json = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match option(arg).as_deref() {
            Some("--help" | "-h") => return Ok(Command::Help),
            Some("--request") => request = Some(PathBuf::from(value(&mut args, "--request")?)),
            Some("--json") => json = true,
            Some("--threads") => {
                let count: usize = number(value(&mut args, "--threads")?, "--threads")?;
                if count == 0 {
                    return Err("--threads must be at least 1".into());
                }
                threads = Some(count);
            }
            Some(flag) => {
                return Err(format!(
                    "compile --request takes its options from the request; only \
                     --threads and --json go with it, not '{flag}'"
                ));
            }
            None => {
                return Err(format!(
                    "compile --request takes its input from the request, not '{}'",
                    arg.to_string_lossy()
                ));
            }
        }
    }
    Ok(Command::CompileRequest(RequestArgs {
        request: request.ok_or("--request needs a value")?,
        threads,
        json,
    }))
}

fn parse_compile(args: &[OsString]) -> Result<Command, String> {
    let mut input = None;
    let mut extra_inputs = Vec::new();
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
    let mut max_zoom = ZoomRange::DEFAULT_MAX_ZOOM;
    let mut threads = None;
    let mut sort_memory = None;
    let mut temp_dir = None;
    let mut fetch = false;
    let mut resume = false;
    let mut building_levels = false;
    let mut search_index = false;
    let mut sar = false;
    let mut omit_sources = Vec::new();
    let mut json = false;
    let mut profile = None;
    let mut only_layers = None;
    let mut excluded_layers = None;
    let mut data_layers = Vec::new();
    let mut languages = None;
    let mut schema = osm_tile_compiler::TileSchema::OpenMapTiles;
    let mut changes = ChangeArgs::default();
    let mut delta_from = None;
    let mut sign_delta = None;
    let mut merge_cache = osm_tile_compiler::MergeCacheOptions::default();
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(flag) = option(arg) else {
            // Inputs after the first are merged with it.
            if input.is_some() {
                extra_inputs.push(PathBuf::from(arg));
            } else {
                input = Some(PathBuf::from(arg));
            }
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
            "--tile-format" => {
                let id = value(&mut args, &flag)?.to_string_lossy().into_owned();
                archive.tile_format = osm_tile_compiler::TileFormat::from_id(&id)
                    .ok_or_else(|| format!("--tile-format must be mvt or mlt, got '{id}'"))?;
            }
            "--tile-compression" => {
                let id = value(&mut args, &flag)?.to_string_lossy().into_owned();
                archive.tile_compression = osm_tile_compiler::TileCompression::from_id(&id)
                    .ok_or_else(|| {
                        format!("--tile-compression must be gzip or none, got '{id}'")
                    })?;
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
                if zoom.fract() != 0.0 || !(0.0..=f64::from(ZoomRange::LIMIT)).contains(&zoom) {
                    return Err(format!(
                        "--center zoom must be a whole number 0-{}, got {zoom}",
                        ZoomRange::LIMIT
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
            "--sort-memory" => {
                let mib: u64 = number(value(&mut args, &flag)?, &flag)?;
                sort_memory = Some(
                    mib.checked_mul(1 << 20)
                        .ok_or("--sort-memory is too large")?,
                );
            }
            "--fetch" => fetch = true,
            "--resume" => resume = true,
            "--building-levels" => building_levels = true,
            "--search-index" => search_index = true,
            "--sar" => sar = true,
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
            "--schema" => {
                let name = value(&mut args, &flag)?.to_string_lossy().into_owned();
                schema = osm_tile_compiler::TileSchema::from_name_or_path(&name)
                    .map_err(|e| format!("--schema: {e}"))?;
            }
            "--layers" => only_layers = Some(list(value(&mut args, &flag)?)),
            "--exclude-layers" => excluded_layers = Some(list(value(&mut args, &flag)?)),
            "--languages" => {
                let list = list(value(&mut args, &flag)?);
                resolve_languages(&list).map_err(|e| format!("--languages: {}", e.message()))?;
                languages = Some(list);
            }
            "--data" => {
                let spec = value(&mut args, &flag)?.to_string_lossy().into_owned();
                data_layers.push(data_layers::parse(&spec)?);
            }
            "--data-layers" => {
                data_layers.extend(data_layers::read_file(Path::new(value(&mut args, &flag)?))?);
            }
            "--delta-from" => delta_from = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--sign-delta" => sign_delta = Some(PathBuf::from(value(&mut args, &flag)?)),
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
            ZoomRange::LIMIT
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
    // A configured schema's layers are checked with the request.
    if schema.configured().is_none() {
        layers
            .resolve()
            .map_err(|e| format!("{flag}: {}", e.message()))?;
    }
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
        if search_index {
            return Err(format!(
                "--search-index needs a PMTiles archive, not {}",
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
        None if schema
            .configured()
            .is_some_and(|schema| !schema.reads_water_polygons()) =>
        {
            PathBuf::new()
        }
        None => return Err("compile needs --sources <DIR>".into()),
    };
    if resume && !changes.is_empty() {
        // The updated extract is written again by every attempt, so no
        // checkpoint would match it.
        return Err("--resume does not work with change files or --replication yet".into());
    }
    if !extra_inputs.is_empty() && !changes.is_empty() {
        return Err(
            "change files and --replication apply to one input PBF; update each extract first"
                .into(),
        );
    }
    let input = match (input, area) {
        (Some(path), _) => Input::Pbf(path),
        (None, Some(name)) => Input::Area {
            name,
            directory: download_dir.unwrap_or_else(|| sources.clone()),
        },
        (None, None) => unreachable!("checked above"),
    };
    if sign_delta.is_some() && delta_from.is_none() {
        return Err("--sign-delta signs the --delta-from delta; give --delta-from".into());
    }
    Ok(Command::Compile(Box::new(CompileArgs {
        input,
        extra_inputs,
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
        sort_memory,
        temp_dir,
        fetch,
        resume,
        building_levels,
        search_index,
        sar,
        json,
        profile,
        layers,
        languages,
        changes,
        delta_from,
        sign_delta,
        merge_cache,
        schema,
        data_layers,
    })))
}

/// `schema verify <SCHEMA> [--json]` and `schema layers <SCHEMA>`.
fn parse_schema(args: &[OsString]) -> Result<Command, String> {
    let mut action = None;
    let mut name = None;
    let mut json = false;
    for arg in args {
        match option(arg).as_deref() {
            Some("--help" | "-h") => return Ok(Command::Help),
            Some("--json") => json = true,
            Some(flag) => return Err(format!("unknown schema option '{flag}'")),
            None if action.is_none() => action = Some(arg.to_string_lossy().into_owned()),
            None if name.is_none() => name = Some(arg.to_string_lossy().into_owned()),
            None => return Err(format!("unexpected argument '{}'", arg.to_string_lossy())),
        }
    }
    let action = action.ok_or("schema needs 'verify' or 'layers'")?;
    let name = name.ok_or_else(|| format!("schema {action} needs a schema file or name"))?;
    let schema = osm_tile_compiler::TileSchema::from_name_or_path(&name)?;
    match action.as_str() {
        "verify" if schema.configured().is_none() => {
            Err("the OpenMapTiles schema is built in and has no examples to verify".into())
        }
        "verify" => Ok(Command::SchemaVerify { schema, json }),
        "layers" if !json => Ok(Command::SchemaLayers(schema)),
        "layers" => Err("schema layers takes no --json".into()),
        other => Err(format!(
            "unknown schema action '{other}'; use verify or layers"
        )),
    }
}

/// Runs a schema's examples and prints what fails.
fn schema_verify(schema: &osm_tile_compiler::TileSchema, json: bool) -> Result<(), String> {
    let schema = schema.configured().expect("checked when parsed");
    let results = schema.verify();
    let failed = results.iter().filter(|r| !r.issues.is_empty()).count();
    if json {
        let value = serde_json::json!({
            "schema": schema.name(),
            "examples": results.len(),
            "failed": failed,
            "warnings": schema.warnings(),
            "results": results
                .iter()
                .map(|r| serde_json::json!({"name": r.name, "issues": r.issues}))
                .collect::<Vec<_>>(),
        });
        println!("{value}");
    } else {
        for warning in schema.warnings() {
            eprintln!("warning: {warning}");
        }
        for result in results.iter().filter(|r| !r.issues.is_empty()) {
            println!("FAIL {}", result.name);
            for issue in &result.issues {
                println!("  {issue}");
            }
        }
        println!(
            "{}: {} of {} examples pass",
            schema.name(),
            results.len() - failed,
            results.len()
        );
    }
    if failed > 0 {
        return Err(format!("{failed} schema examples failed"));
    }
    Ok(())
}

fn parse_update(args: &[OsString]) -> Result<Command, String> {
    let mut paths = Vec::new();
    let mut output = None;
    let mut changes = ChangeArgs::default();
    let mut bounds = None;
    let mut threads = None;
    let mut json = false;
    let mut changed_tiles = None;
    let (mut min_zoom, mut max_zoom) = (None, None);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(flag) = option(arg) else {
            paths.push(PathBuf::from(arg));
            continue;
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--output" | "-o" => output = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--changed-tiles" => changed_tiles = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--min-zoom" => min_zoom = Some(number(value(&mut args, &flag)?, &flag)?),
            "--max-zoom" => max_zoom = Some(number(value(&mut args, &flag)?, &flag)?),
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
    if changed_tiles.is_none() && (min_zoom.is_some() || max_zoom.is_some()) {
        return Err("--min-zoom and --max-zoom choose the zooms of --changed-tiles".into());
    }
    let changed_tiles = match changed_tiles {
        Some(path) => Some(ChangedTilesRequest {
            output: path,
            zooms: ZoomRange::new(
                min_zoom.unwrap_or(0),
                max_zoom.unwrap_or(ZoomRange::DEFAULT_MAX_ZOOM),
            )
            .map_err(String::from)?,
        }),
        None => None,
    };
    Ok(Command::Update(Box::new(UpdateArgs {
        input,
        output,
        changes,
        bounds,
        threads,
        json,
        changed_tiles,
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

fn parse_hd_roads(args: &[OsString]) -> Result<Command, String> {
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
            _ => return Err(format!("unknown hd-roads option '{flag}'")),
        }
    }
    let mut paths = paths.into_iter();
    match (paths.next(), paths.next(), paths.next()) {
        (Some(input), Some(output), None) => Ok(Command::HdRoads(HdRoadsArgs {
            input,
            output,
            threads,
        })),
        (_, _, Some(extra)) => Err(format!(
            "unexpected argument '{}': hd-roads takes an extract and an archive",
            extra.display()
        )),
        _ => Err("hd-roads needs an input PBF and an output archive".into()),
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

fn parse_data_info(args: &[OsString]) -> Result<Command, String> {
    let mut spec = None;
    let mut json = false;
    for arg in args {
        match option(arg).as_deref() {
            Some("--help" | "-h") => return Ok(Command::Help),
            Some("--json") => json = true,
            Some(flag) => return Err(format!("unknown data-info option '{flag}'")),
            None if spec.is_some() => {
                return Err("data-info takes one file".into());
            }
            None => spec = Some(arg.to_string_lossy().into_owned()),
        }
    }
    let spec = spec.ok_or("data-info needs a file")?;
    Ok(Command::DataInfo { spec, json })
}

fn parse_extract(args: &[OsString]) -> Result<Command, String> {
    let mut input = None;
    let mut output = None;
    let mut bounds = None;
    let mut polygon = None;
    let (mut min_zoom, mut max_zoom) = (None, None);
    let mut json = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(flag) = option(arg) else {
            if input.is_some() {
                return Err(format!(
                    "unexpected argument '{}': extract takes one archive",
                    arg.to_string_lossy()
                ));
            }
            input = Some(PathBuf::from(arg));
            continue;
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--output" | "-o" => output = Some(PathBuf::from(value(&mut args, "--output")?)),
            "--bounds" => bounds = Some(parse_bounds(&mut args, &flag)?),
            "--polygon" => polygon = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--min-zoom" => min_zoom = Some(number(value(&mut args, &flag)?, &flag)?),
            "--max-zoom" => max_zoom = Some(number(value(&mut args, &flag)?, &flag)?),
            "--json" => json = true,
            _ => return Err(format!("unknown extract option '{flag}'")),
        }
    }
    if let (Some(min), Some(max)) = (min_zoom, max_zoom)
        && min > max
    {
        return Err(format!(
            "--min-zoom must not exceed --max-zoom, got {min} and {max}"
        ));
    }
    if bounds.is_none() && polygon.is_none() && min_zoom.is_none() && max_zoom.is_none() {
        return Err("extract needs --bounds, --polygon, --min-zoom or --max-zoom".into());
    }
    Ok(Command::Extract(Box::new(ExtractArgs {
        input: input.ok_or("extract needs an archive")?,
        output: output.ok_or("extract needs --output")?,
        bounds,
        polygon,
        min_zoom,
        max_zoom,
        json,
    })))
}

fn parse_completions(args: &[OsString]) -> Result<Command, String> {
    let shells = "bash, zsh or fish";
    match args {
        [shell] if option(shell).is_none() => {
            let shell = shell.to_string_lossy().into_owned();
            if crate::completions::SHELLS.contains(&shell.as_str()) {
                Ok(Command::Completions(shell))
            } else {
                Err(format!(
                    "unknown shell '{shell}'; completions are for {shells}"
                ))
            }
        }
        [flag] if matches!(flag.to_str(), Some("--help" | "-h")) => Ok(Command::Help),
        [] => Err(format!("completions needs a shell: {shells}")),
        _ => Err(format!("completions takes one shell: {shells}")),
    }
}

fn parse_delta_command(name: &str, args: &[OsString]) -> Result<Command, String> {
    let mut paths = Vec::new();
    let mut output = None;
    let mut json = false;
    let (mut sign, mut changed_tiles, mut trusted) = (None, None, Vec::new());
    let mut in_place = false;
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match option(arg).as_deref() {
            Some("--help" | "-h") => return Ok(Command::Help),
            Some("--json") => json = true,
            Some("--output" | "-o") => output = Some(PathBuf::from(value(&mut args, "--output")?)),
            Some(flag @ "--sign") if name == "diff" => {
                sign = Some(PathBuf::from(value(&mut args, flag)?));
            }
            Some(flag @ "--changed-tiles") if name == "diff" => {
                changed_tiles = Some(PathBuf::from(value(&mut args, flag)?));
            }
            Some(flag @ "--trusted-key") if name == "apply" => {
                trusted.push(trusted_key(value(&mut args, flag)?)?);
            }
            Some("--in-place") if name == "apply" => in_place = true,
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
    let output = match (output, in_place) {
        (Some(_), true) => return Err("apply takes --output or --in-place, not both".into()),
        (None, true) => base.clone(),
        (output, false) => output.ok_or_else(|| {
            if name == "apply" {
                "apply needs --output or --in-place".to_string()
            } else {
                format!("{name} needs --output")
            }
        })?,
    };
    let args = DeltaArgs {
        base,
        other,
        output,
        json,
        sign,
        changed_tiles,
        trusted,
        in_place,
    };
    Ok(if name == "diff" {
        Command::Diff(args)
    } else {
        Command::Apply(args)
    })
}

/// A `--trusted-key` value: 64 hex digits or a public key file.
fn trusted_key(text: &std::ffi::OsStr) -> Result<PublicKey, String> {
    PublicKey::parse(&text.to_string_lossy()).map_err(|e| format!("--trusted-key: {e}"))
}

fn parse_keygen(args: &[OsString]) -> Result<Command, String> {
    let mut path = None;
    for arg in args {
        match option(arg).as_deref() {
            Some("--help" | "-h") => return Ok(Command::Help),
            Some(flag) => return Err(format!("unknown keygen option '{flag}'")),
            None if path.is_some() => return Err("keygen takes one key file".into()),
            None => path = Some(PathBuf::from(arg)),
        }
    }
    Ok(Command::Keygen(
        path.ok_or("keygen needs a key file to write")?,
    ))
}

fn parse_signature_command(name: &str, args: &[OsString]) -> Result<Command, String> {
    let mut file = None;
    let (mut key, mut signature, mut trusted, mut json) = (None, None, Vec::new(), false);
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        match option(arg).as_deref() {
            Some("--help" | "-h") => return Ok(Command::Help),
            Some("--json") => json = true,
            Some(flag @ "--key") if name == "sign" => {
                key = Some(PathBuf::from(value(&mut args, flag)?));
            }
            Some(flag @ "--trusted-key") if name == "verify-signature" => {
                trusted.push(trusted_key(value(&mut args, flag)?)?);
            }
            Some(flag @ "--signature") if name == "verify-signature" => {
                signature = Some(PathBuf::from(value(&mut args, flag)?));
            }
            Some(flag) => return Err(format!("unknown {name} option '{flag}'")),
            None if file.is_some() => return Err(format!("{name} takes one file")),
            None => file = Some(PathBuf::from(arg)),
        }
    }
    let file = file.ok_or_else(|| format!("{name} needs a file"))?;
    Ok(if name == "sign" {
        Command::Sign {
            file,
            key: key.ok_or("sign needs --key <FILE.key>")?,
            json,
        }
    } else {
        Command::VerifySignature {
            file,
            trusted,
            signature,
            json,
        }
    })
}

fn parse_style(args: &[OsString]) -> Result<Command, String> {
    let mut archive = None;
    let mut glyphs = None;
    let mut sprite = None;
    let mut offline = None;
    let mut archive_file = None;
    let mut languages = None;
    let mut base_url = None;
    let mut sar = false;
    let mut tile_format = osm_tile_compiler::TileFormat::Mvt;
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
            "--offline" => offline = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--archive-file" => archive_file = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--languages" => {
                let list = list(value(&mut args, &flag)?);
                resolve_languages(&list).map_err(|e| format!("--languages: {}", e.message()))?;
                languages = Some(list);
            }
            "--base-url" => {
                base_url = Some(value(&mut args, &flag)?.to_string_lossy().into_owned());
            }
            "--sar" => sar = true,
            "--tile-format" => {
                let id = value(&mut args, &flag)?.to_string_lossy().into_owned();
                tile_format = osm_tile_compiler::TileFormat::from_id(&id)
                    .ok_or_else(|| format!("--tile-format must be mvt or mlt, got '{id}'"))?;
            }
            _ => return Err(format!("unknown style option '{flag}'")),
        }
    }
    if offline.is_none() {
        for (given, flag) in [
            (languages.is_some(), "--languages"),
            (archive_file.is_some(), "--archive-file"),
            (base_url.is_some(), "--base-url"),
        ] {
            if given {
                return Err(format!("{flag} needs --offline"));
            }
        }
    }
    Ok(Command::Style(StyleArgs {
        archive: archive.ok_or("style needs an archive URL")?,
        glyphs,
        sprite,
        offline,
        archive_file,
        languages,
        base_url,
        sar,
        tile_format,
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

/// Prints the style for the archive, or with `--offline` writes the pack.
fn style(args: StyleArgs) -> Result<(), String> {
    let Some(directory) = args.offline else {
        let style = maplibre_style(
            &args.archive,
            args.glyphs.as_deref(),
            args.sprite.as_deref(),
        );
        let style = if args.sar {
            with_sar_layers(&style)
        } else {
            style
        };
        match args.tile_format {
            osm_tile_compiler::TileFormat::Mvt => print!("{style}"),
            format => print!("{}", osm_tile_compiler::with_tile_format(&style, format)),
        }
        return Ok(());
    };
    let _cooperating = interrupt::cooperate();
    let mut status = Status::new();
    // ARCHIVE_URL may name the archive on this machine.
    let local = args
        .archive
        .strip_prefix("pmtiles://")
        .unwrap_or(&args.archive);
    let local = Path::new(local.strip_prefix("file://").unwrap_or(local));
    let archive_file = args
        .archive_file
        .or_else(|| local.is_file().then(|| local.to_owned()));
    if archive_file.is_none() {
        status.line(&format!(
            "{} is not a local file: writing the Latin glyph ranges{}, not \
             necessarily every one the labels use; --archive-file reads them",
            args.archive,
            if args.languages.is_some() {
                " and --languages' scripts"
            } else {
                ""
            }
        ));
    }
    let mut request = StylePackRequest::new(&directory, args.archive);
    request.archive_file = archive_file;
    request.languages = args.languages;
    request.base_url = args.base_url;
    request.sar = args.sar;
    request.tile_format = args.tile_format;
    if let Some(glyphs) = args.glyphs {
        request.glyphs = glyphs;
    }
    if let Some(sprite) = args.sprite {
        request.sprite = sprite;
    }
    status.line(&format!(
        "Writing an offline style pack to {}",
        directory.display()
    ));
    let report = write_style_pack(
        &request,
        |event| match event {
            StylePackEvent::ReadingArchive => {
                status.update("Reading the archive's labels for the glyphs they use")
            }
            StylePackEvent::Fetched { done, total, bytes } => {
                status.update(&format!("Fetched {done} of {total} files, {}", kib(bytes)))
            }
            _ => {}
        },
        interrupt::requested,
    )
    .map_err(|e| e.message().to_string())?;
    for path in &report.missing {
        status.line(&format!("Not at the source, left out: {path}"));
    }
    status.line(&format!(
        "Wrote {}: style.json, {} glyph ranges of {} fonts, the sprite; {} files, {}",
        report.directory.display(),
        report.ranges.len(),
        report.fontstacks.len(),
        report.files,
        kib(report.bytes)
    ));
    Ok(())
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
fn claim_scratch(directory: &Path, resume: bool, status: &mut Status) -> Result<File, String> {
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
    // A resumable compile keeps what it finds: its checkpoint decides.
    if !resume
        && remove_all_but(directory, &marker_path)
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

fn hd_roads(args: HdRoadsArgs) -> Result<(), String> {
    let mut status = Status::new();
    let threads = args
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    status.line(&format!(
        "Writing the roads of {} into {} ({threads} threads)",
        args.input.display(),
        args.output.display()
    ));
    let mut temp = args.output.clone().into_os_string();
    temp.push(".tmp");
    let temp = PathBuf::from(temp);
    let _cooperating = interrupt::cooperate();
    let marker = claim_scratch(&temp, false, &mut status)?;
    let _scratch_on_exit = interrupt::remove_on_exit(temp.clone());
    let report = write_hd_roads(
        &args.input,
        &args.output,
        &temp,
        threads,
        interrupt::requested,
    );
    let cleanup = release_scratch(&temp, marker);
    let report = report.map_err(|error| match error {
        CompileError::Cancelled => format!("cancelled; {} was not written", args.output.display()),
        error => error.to_string(),
    })?;
    cleanup?;
    status.line(&format!(
        "Wrote {} in {:.1} s, {:.1} of them reading: {} ways and {} paths ({} incomplete left out), \
         {} junctions ({} of more nodes than one), {} cross-sections",
        args.output.display(),
        report.seconds,
        report.read_seconds,
        report.ways,
        report.paths,
        report.incomplete_ways,
        report.junctions,
        report.clusters,
        report.profiles
    ));
    status.line(&format!(
        "  {} tiles, {} surfaces, {} markings ({} crossings, stops, arrows and other \
         paint, {} areas, of them {} crossings' islands and {} carriageways), \
         {} turning circles, {} roads fitted to paths, {} tracks, {} barriers, {} fences, {} things, {} pitches ({} with lines), \
         {} bridge groups ({} pieces, {} crossings), {} raw, {} gzipped",
        report.tiles,
        report.surfaces,
        report.markings,
        report.details,
        report.areas,
        report.refuges,
        report.carriageways,
        report.turning_circles,
        report.fitted,
        report.tracks,
        report.barriers,
        report.fences,
        report.things,
        report.pitches,
        report.lined,
        report.groups,
        report.pieces,
        report.crossings,
        megabytes(report.raw_bytes),
        megabytes(report.gz_bytes)
    ));
    Ok(())
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
    let search_index = osm_tile_compiler::search_index_path(&args.output);
    let search_index = args.search_index.then_some(&search_index);
    // A resumable compile checks them against its checkpoint: an attempt
    // that was interrupted may have written them.
    for archive in [Some(&args.output), Some(&levels), search_index]
        .into_iter()
        .flatten()
        .filter(|_| !args.resume)
    {
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
    // Read before the compile, so a bad key fails before any work.
    let sign_key = match &args.sign_delta {
        Some(path) => Some(SecretKey::read(path)?),
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
        None => {
            let mut bounds = pbf_bounds(&input)?;
            for extra in &args.extra_inputs {
                let more = pbf_bounds(extra)?;
                if bounds.wraps() || more.wraps() {
                    return Err(
                        "an extract's header box crosses the antimeridian; give --bounds".into(),
                    );
                }
                bounds = CompileBounds::new(
                    bounds.north.max(more.north),
                    bounds.south.min(more.south),
                    bounds.east.max(more.east),
                    bounds.west.min(more.west),
                );
            }
            bounds
        }
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
        Some(claim_scratch(&temp_dir, args.resume, &mut status)?)
    } else {
        None
    };
    // A second Ctrl-C ends the process without waiting for the compile to
    // clean up; a default scratch directory is then removed at least, unless
    // it holds checkpoints to resume from.
    let _scratch_on_exit =
        (owned_temp && !args.resume).then(|| interrupt::remove_on_exit(temp_dir.clone()));
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
    request.extra_inputs = args.extra_inputs;
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
    request.search_index = args.search_index;
    request.sar = args.sar;
    request.sort_memory = args.sort_memory;
    request.merge_cache = args.merge_cache;
    request.schema = args.schema;
    request.resume = args.resume;
    request.data_layers = args.data_layers;
    if args.resume && osm_tile_compiler::checkpoint_directory(&request).is_dir() {
        status.line(&format!(
            "Resuming from the checkpoint in {}",
            osm_tile_compiler::checkpoint_directory(&request).display()
        ));
    }
    if args.profile.is_some() {
        osm_tile_compiler::stage_profile::start(threads);
    }
    let output = run_compile(&request, &mut status);
    let profile = osm_tile_compiler::stage_profile::finish();
    if let Some(updated) = &updated_input {
        let _ = std::fs::remove_file(updated);
    }
    // A resumable compile that did not finish keeps its checkpoints.
    let cleanup = match scratch {
        Some(marker) if !args.resume || output.is_ok() => release_scratch(&temp_dir, marker),
        _ => Ok(()),
    };
    let resume_hint = if args.resume {
        "; run the same command again to resume it"
    } else {
        ""
    };
    let output = output.map_err(|error| match error {
        CompileError::Cancelled => format!(
            "the compile was cancelled; {} was not written{resume_hint}",
            args.output.display()
        ),
        error if args.resume => format!("{error}{resume_hint}"),
        error => error.to_string(),
    })?;
    if let Some(stage) = output.resumed_from {
        status.line(&format!("Resumed an interrupted compile at {}", stage.id()));
    }
    cleanup?;
    let result = &output.result;
    report_compile(&mut status, &output);
    write_profile(&mut status, args.profile.as_deref(), profile)?;
    if let Some((old, delta_path)) = &delta {
        let _partial = interrupt::remove_on_exit(archive::partial_path(delta_path));
        let mut options = archive::DiffOptions::default();
        options.sign_with = sign_key.clone();
        let report = archive::diff_with(old, &args.output, delta_path, &options)?;
        status.line(&format!(
            "Wrote {}: {}{}",
            delta_path.display(),
            delta_summary(&report.manifest, report.delta_bytes),
            signed_summary(&report.signed_by)
        ));
    }
    if args.json {
        let json = serde_json::to_string_pretty(result).map_err(|e| e.to_string())?;
        println!("{json}");
    }
    Ok(())
}

/// Runs a compile, printing a line as each stage starts and download
/// progress in place.
fn run_compile(
    request: &BasemapRequest,
    status: &mut Status,
) -> Result<osm_tile_compiler::BasemapOutput, CompileError> {
    let start = Instant::now();
    let mut stage = None;
    compile_basemap(
        request,
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
    )
}

/// The summary of a finished compile: what it wrote, how current its data
/// is, its largest tile, the merge cache, warnings and building levels.
fn report_compile(status: &mut Status, output: &osm_tile_compiler::BasemapOutput) {
    let result = &output.result;
    status.line(&format!(
        "Wrote {}: {} tiles, {} features in {:.1} s",
        output.archive.display(),
        result.tiles,
        result.profile_features,
        result.total_seconds
    ));
    if let Some(time) = result.osm_data.timestamp_rfc3339() {
        status.line(&format!("OSM data as of {time}"));
    }
    if let Some(largest) = result.tile_stats.largest.first() {
        status.line(&format!(
            "Largest tile: {} at {}",
            largest.tile,
            kib(largest.bytes)
        ));
    }
    for data in &result.data_layers {
        status.line(&format!(
            "Data layer {}: {} features from {} ({}, {}){}",
            data.layer,
            data.read.features,
            data.file,
            data.format,
            data.read.crs,
            match (data.read.outside_bounds, data.read.without_geometry) {
                (0, 0) => String::new(),
                (outside, empty) =>
                    format!("; skipped {outside} outside the area, {empty} without geometry"),
            }
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
        report_levels(status, path, report);
    }
    if let Some((path, report)) = &output.search_index {
        search::report_index(status, path, report);
    }
}

/// `compile --request`: the compile a worker request asks for, with the
/// command line's progress, and its manifest written where it says.
fn compile_request(args: RequestArgs) -> Result<(), String> {
    let _cooperating = interrupt::cooperate();
    let (mut request, manifest) = crate::worker_request::read(&args.request)?;
    if let Some(threads) = args.threads {
        request.threads = threads;
    }
    let mut status = Status::new();
    status.line(&format!(
        "Compiling {} into {} as {} asks ({} threads)",
        request.input_pbf.display(),
        request.output.display(),
        args.request.display(),
        request.threads
    ));
    let output = run_compile(&request, &mut status).map_err(|error| match error {
        CompileError::Cancelled => format!(
            "the compile was cancelled; {} was not written",
            request.output.display()
        ),
        error => error.to_string(),
    })?;
    report_compile(&mut status, &output);
    std::fs::write(
        &manifest,
        serde_json::to_vec_pretty(&output.manifest()).map_err(|e| e.to_string())?,
    )
    .map_err(|e| format!("Failed to write the manifest {}: {e}", manifest.display()))?;
    status.line(&format!("Wrote {}", manifest.display()));
    if args.json {
        let json = serde_json::to_string_pretty(&output.result).map_err(|e| e.to_string())?;
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
    let mut request = args
        .changes
        .request(&args.input, &args.output, clip, threads);
    request.changed_tiles = args.changed_tiles;
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
    if let Some(tiles) = &report.changed_tiles {
        status.line(&format!(
            "Wrote {}: {} tiles the changes reach, from {} boxes{}",
            tiles.output.display(),
            tiles.tiles,
            tiles.boxes,
            if tiles.nodes_missing > 0 {
                format!(
                    "; {} nodes neither file places were left out",
                    tiles.nodes_missing
                )
            } else {
                String::new()
            }
        ));
    }
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

fn extract(args: ExtractArgs) -> Result<(), String> {
    let _cooperating = interrupt::cooperate();
    let url = args
        .input
        .to_str()
        .filter(|text| archive::is_remote_source(text))
        .map(str::to_owned);
    let mut request = match &url {
        Some(url) => archive::ExtractRequest::remote(url, &args.output),
        None => archive::ExtractRequest::new(&args.input, &args.output),
    };
    request.bounds = args.bounds;
    if let Some(polygon) = &args.polygon {
        request.polygon = Some(osm_tile_compiler::CompileArea::read(polygon)?);
    }
    if args.min_zoom.is_some() || args.max_zoom.is_some() {
        // Without --max-zoom, up to whatever the source holds.
        let max = args.max_zoom.unwrap_or(ZoomRange::LIMIT);
        let min = args.min_zoom.unwrap_or(0).min(max);
        request.zoom = Some(ZoomRange::new(min, max).map_err(|_| {
            format!(
                "zooms must satisfy 0 <= --min-zoom <= --max-zoom <= {}, got {min} and {max}",
                ZoomRange::LIMIT
            )
        })?);
    }
    let started = Instant::now();
    if let Some(url) = &url
        && !args.json
    {
        eprintln!("Reading {url} by range requests");
    }
    let report = archive::extract(&request, interrupt::requested)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        let [west, south, east, north] = report.bounds;
        eprintln!(
            "Wrote {}: {}, {} of {} tiles, zooms {}-{}, bounds {west} {south} {east} {north}, in {:.1} s",
            args.output.display(),
            megabytes(report.bytes),
            report.tiles,
            report.source_tiles,
            report.zooms[0],
            report.zooms[1],
            started.elapsed().as_secs_f64()
        );
        if url.is_some() {
            eprintln!(
                "Downloaded {} in {} range requests",
                megabytes(report.bytes_read),
                report.requests
            );
        }
    }
    Ok(())
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
    let mut options = archive::DiffOptions::default();
    if let Some(key) = &args.sign {
        options.sign_with = Some(SecretKey::read(key)?);
    }
    options.changed_tiles = args.changed_tiles.clone();
    let _partial = interrupt::remove_on_exit(archive::partial_path(&args.output));
    let report = archive::diff_with(&args.base, &args.other, &args.output, &options)?;
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        eprintln!(
            "Wrote {}: {}{}",
            args.output.display(),
            delta_summary(&report.manifest, report.delta_bytes),
            signed_summary(&report.signed_by)
        );
        if let Some((path, tiles)) = &report.changed_tiles {
            eprintln!("Wrote {}: {tiles} tiles differ", path.display());
        }
    }
    Ok(())
}

fn signed_summary(signed_by: &[String]) -> String {
    if signed_by.is_empty() {
        "; not signed".into()
    } else {
        format!("; signed by {}", signed_by.join(", "))
    }
}

fn apply(args: DeltaArgs) -> Result<(), String> {
    let _partial = interrupt::remove_on_exit(archive::partial_path(&args.output));
    let report = if args.in_place {
        archive::apply_in_place(&args.base, &args.other, &args.trusted)?
    } else {
        archive::apply_trusted(&args.base, &args.other, &args.output, &args.trusted)?
    };
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
    } else {
        eprintln!(
            "Wrote {} (SHA-256 {}): {}{}{}",
            args.output.display(),
            report.manifest.target.sha256,
            delta_summary(&report.manifest, report.delta_bytes),
            signed_summary(&report.signed_by),
            if report.trusted {
                ", a trusted key"
            } else {
                ""
            }
        );
    }
    Ok(())
}

fn keygen(path: &Path) -> Result<(), String> {
    let key = SecretKey::generate()?;
    let public = key.write_pair(path)?;
    eprintln!(
        "Wrote the secret key to {} and the public key to {}; keep the secret key private",
        path.display(),
        public.display()
    );
    println!("{}", key.public_key());
    Ok(())
}

fn sign(file: &Path, key: &Path, json: bool) -> Result<(), String> {
    let key = SecretKey::read(key)?;
    let (checked, place) = if archive::is_delta(file) {
        (archive::sign_delta(file, &key)?, file.to_path_buf())
    } else {
        (
            archive::sign_file(file, &key)?,
            archive::signature_path(file),
        )
    };
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&checked).map_err(|e| e.to_string())?
        );
    } else {
        eprintln!(
            "Signed {} with {}: {} now holds {} signature{}",
            file.display(),
            key.public_key(),
            place.display(),
            checked.signed_by.len(),
            if checked.signed_by.len() == 1 {
                ""
            } else {
                "s"
            }
        );
    }
    Ok(())
}

fn verify_signature(
    file: &Path,
    trusted: &[PublicKey],
    signature: Option<&Path>,
    json: bool,
) -> Result<(), String> {
    let (json_text, signed_by, trusted) = if archive::is_delta(file) {
        if signature.is_some() {
            return Err(
                "a delta carries its own signatures; --signature is for other files".into(),
            );
        }
        let verified = archive::verify_delta(file, trusted)?;
        let text = serde_json::to_string_pretty(&verified).map_err(|e| e.to_string())?;
        (text, verified.signed_by, verified.trusted)
    } else {
        let checked = archive::verify_file_signature(file, signature, trusted)?;
        let text = serde_json::to_string_pretty(&checked).map_err(|e| e.to_string())?;
        (text, checked.signed_by, checked.trusted)
    };
    if json {
        println!("{json_text}");
    } else if signed_by.is_empty() {
        eprintln!("{}: not signed", file.display());
    } else {
        eprintln!(
            "{}: every signature matches; signed by {}{}",
            file.display(),
            signed_by.join(", "),
            if trusted { ", a trusted key" } else { "" }
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
    if let Some(generator) = metadata["generator"].as_str() {
        let _ = writeln!(text, "Generator: {generator}");
    }
    match metadata["osm_data_timestamp"].as_str() {
        Some(time) => {
            let mut line = format!("OSM data as of {time}");
            if let Some(sequence) = metadata["osm_replication_sequence"].as_i64() {
                let _ = write!(line, ", sequence {sequence}");
            }
            if let Some(url) = metadata["osm_replication_url"].as_str() {
                let _ = write!(line, " of {url}");
            }
            let _ = writeln!(text, "{line}");
        }
        None => {
            let _ = writeln!(
                text,
                "OSM data: date unknown (the extract's header gives none)"
            );
        }
    }
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
