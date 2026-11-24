//! Runs the built `osm-tile-compiler` binary: the command line's usage
//! handling, a real `compile` of `fixtures/monaco.osm.pbf` against synthetic
//! supplemental inputs, the `style` it is drawn with, a delta between two
//! compiles, an area cut out of a compile, locally and over HTTP range
//! requests, and the desktop worker protocol's argument errors.

mod common;

use common::{monaco_pbf, synthetic_sources};
use pmtiles::{AsyncPmTilesReader, TileType};
use std::ffi::OsStr;
use std::path::Path;
use std::process::{Command, Output};

fn run<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    Command::new(env!("CARGO_BIN_EXE_osm-tile-compiler"))
        .args(args)
        .output()
        .unwrap()
}

fn text(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[test]
fn help_and_version_print_to_stdout() {
    for args in [&["--help"][..], &["help"], &["-h"], &["compile", "--help"]] {
        let output = run(args);
        assert!(output.status.success(), "{args:?}: {output:?}");
        let stdout = text(&output.stdout);
        assert!(stdout.contains("Usage:"), "{args:?}: {stdout}");
        assert!(stdout.contains("compile <INPUT.pbf>"), "{args:?}: {stdout}");
        assert!(
            stdout.contains("building-levels <INPUT.pmtiles>"),
            "{args:?}: {stdout}"
        );
        assert!(stdout.contains("fetch-sources <DIR>"), "{args:?}: {stdout}");
        assert!(stdout.contains("style <ARCHIVE_URL>"), "{args:?}: {stdout}");
        assert!(stdout.contains("update <INPUT.pbf>"), "{args:?}: {stdout}");
    }
    let output = run(["--version"]);
    assert!(output.status.success());
    assert_eq!(
        text(&output.stdout),
        format!("osm-tile-compiler {}\n", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn usage_errors_exit_with_2() {
    let cases: &[(&[&str], &str)] = &[
        (&[], "no command given"),
        (&["frobnicate"], "unknown command 'frobnicate'"),
        (&["--bogus"], "unknown command '--bogus'"),
        (&["compile"], "compile needs an input PBF"),
        (
            &["extract", "a.pmtiles", "--output", "b.pmtiles"],
            "extract needs --bounds, --polygon, --min-zoom or --max-zoom",
        ),
        (&["extract", "--max-zoom", "8"], "extract needs an archive"),
        (
            &["extract", "a.pmtiles", "--max-zoom", "8"],
            "extract needs --output",
        ),
        (
            &["extract", "a.pmtiles", "--min-zoom", "9", "--max-zoom", "8"],
            "--min-zoom must not exceed --max-zoom",
        ),
        (
            &["compile", "in.pbf", "--area", "monaco"],
            "an input PBF or --area, not both",
        ),
        (&["compile", "--area"], "--area needs a value"),
        (
            &["compile", "in.pbf", "--download-dir", "d"],
            "--download-dir is only used with --area",
        ),
        (
            &["compile", "--area", "monaco", "--output", "o.pmtiles"],
            "--sources",
        ),
        (&["compile", "in.pbf", "--sources", "s"], "--output"),
        (&["compile", "in.pbf", "--output", "o.pmtiles"], "--sources"),
        (&["compile", "in.pbf", "--output"], "--output needs a value"),
        (
            &["compile", "in.pbf", "--format", "geojson"],
            "--format must be pmtiles or mbtiles, got 'geojson'",
        ),
        (
            &[
                "compile",
                "in.pbf",
                "--sources",
                "s",
                "--output",
                "o.mbtiles",
                "--building-levels",
            ],
            "--building-levels needs a PMTiles archive, not MBTiles",
        ),
        (
            &[
                "compile",
                "in.pbf",
                "--sources",
                "s",
                "--output",
                "o.pmtiles",
                "--format",
                "mbtiles",
                "--delta-from",
                "old.pmtiles",
            ],
            "--delta-from needs a PMTiles archive, not MBTiles",
        ),
        (
            &[
                "compile",
                "in.pbf",
                "--sources",
                "s",
                "--output",
                "o.mbtiles",
                "--reuse-from",
                "old.pmtiles",
            ],
            "--merge-cache and --reuse-from need a PMTiles archive, not MBTiles",
        ),
        (
            &["compile", "in.pbf", "--bounds", "1", "2", "3"],
            "four numbers",
        ),
        (
            &["compile", "in.pbf", "--bounds", "7.4", "43.7", "x", "43.8"],
            "--bounds expects a number, got 'x'",
        ),
        (
            &[
                "compile", "in.pbf", "--bounds", "7.4", "43.7", "7.4", "43.8",
            ],
            "W different from E",
        ),
        (
            &["compile", "in.pbf", "--bounds", "177", "-19", "182", "-15"],
            "within -180..180",
        ),
        (
            &["compile", "in.pbf", "--polygon"],
            "--polygon needs a value",
        ),
        (
            &["compile", "in.pbf", "--center", "7.4", "43.7"],
            "three numbers",
        ),
        (
            &["compile", "in.pbf", "--center", "7.4", "43.7", "12.5"],
            "whole number 0-14",
        ),
        (&["compile", "in.pbf", "--max-zoom", "15"], "<= 14"),
        (
            &["compile", "in.pbf", "--min-zoom", "9", "--max-zoom", "4"],
            "--min-zoom <= --max-zoom",
        ),
        (&["compile", "in.pbf", "--threads", "0"], "at least 1"),
        (
            &[
                "compile",
                "a.pbf",
                "b.pbf",
                "--replication",
                "hour",
                "--omit-sources",
                "all",
                "--output",
                "x.pmtiles",
            ],
            "apply to one input PBF",
        ),
        (
            &["compile", "in.pbf", "--frob"],
            "unknown compile option '--frob'",
        ),
        (&["building-levels"], "needs an input and an output archive"),
        (&["building-levels", "a"], "needs an input and an output"),
        (
            &["building-levels", "a", "b", "c"],
            "unexpected argument 'c'",
        ),
        (
            &["building-levels", "a", "b", "--threads", "0"],
            "at least 1",
        ),
        (
            &["building-levels", "a", "b", "--frob"],
            "unknown building-levels option '--frob'",
        ),
        (&["fetch-sources"], "fetch-sources needs a directory"),
        (&["fetch-sources", "a", "b"], "unexpected argument 'b'"),
        (&["style"], "style needs an archive URL"),
        (&["inspect"], "inspect needs an archive"),
        (&["verify", "a", "b"], "verify takes one archive"),
        (&["verify", "a", "--frob"], "unknown verify option '--frob'"),
        (&["diff", "a", "--output", "d"], "diff takes two files"),
        (&["diff", "a", "b"], "diff needs --output"),
        (
            &["apply", "a", "b", "c", "-o", "d"],
            "apply takes two files",
        ),
        (&["apply", "a", "b", "--output"], "--output needs a value"),
        (
            &["apply", "a", "b", "--frob"],
            "unknown apply option '--frob'",
        ),
        (
            &["compile", "in.pbf", "--gzip-level", "10"],
            "--gzip-level must be 0-9",
        ),
        (&["style", "a", "b"], "unexpected argument 'b'"),
        (
            &["style", "a", "--glyphs", "g.pbf"],
            "{fontstack} and {range}",
        ),
        (&["style", "a", "--sprite"], "--sprite needs a value"),
        (
            &["style", "a", "--languages", "uk"],
            "--languages needs --offline",
        ),
        (
            &["style", "a", "--base-url", "x"],
            "--base-url needs --offline",
        ),
        (
            &["style", "a", "--archive-file", "a"],
            "--archive-file needs --offline",
        ),
        (
            &["style", "a", "--offline", "d", "--languages", "xx"],
            "--languages: unknown language 'xx'",
        ),
        (
            &["compile", "in.pbf", "--layers", "water,roads"],
            "--layers: unknown layer 'roads'; the layers are water, waterway,",
        ),
        (
            &["compile", "in.pbf", "--exclude-layers", "poi,parks"],
            "--exclude-layers: unknown layer 'parks'",
        ),
        (
            &[
                "compile",
                "in.pbf",
                "--layers",
                "poi",
                "--exclude-layers",
                "water",
            ],
            "--layers and --exclude-layers cannot be combined",
        ),
        (&["compile", "in.pbf", "--layers", ","], "no layer is left"),
        (
            &["compile", "in.pbf", "--languages", "en,xx"],
            "--languages: unknown language 'xx'; the languages are af,",
        ),
        (
            &["compile", "in.pbf", "--languages", "*"],
            "'*' is not supported",
        ),
        (
            &["compile", "in.pbf", "--languages"],
            "--languages needs a value",
        ),
        (&["update"], "update needs an input PBF"),
        (
            &["update", "in.pbf"],
            "update needs change files or --replication",
        ),
        (&["update", "in.pbf", "c.osc"], "update needs --output"),
        (
            &["update", "in.pbf", "--replication", "weekly"],
            "unknown replication source 'weekly'",
        ),
        (
            &["update", "in.pbf", "c.osc", "--until", "5", "-o", "x"],
            "--until and --max-diffs need --replication",
        ),
        (
            &[
                "update",
                "in.pbf",
                "c.osc",
                "--no-clip",
                "--bounds",
                "1",
                "2",
                "3",
                "4",
                "-o",
                "x",
            ],
            "--bounds and --no-clip cannot be combined",
        ),
        (
            &["update", "in.pbf", "--changes", "c.osc"],
            "update takes change files as arguments",
        ),
        (
            &["update", "in.pbf", "c.osc", "--frob"],
            "unknown update option '--frob'",
        ),
        (
            &["compile", "in.pbf", "--no-clip"],
            "--no-clip needs changes to apply",
        ),
        (
            &["compile", "in.pbf", "--max-diffs", "3"],
            "--until and --max-diffs need --replication",
        ),
    ];
    for (args, expected) in cases {
        let output = run(*args);
        let stderr = text(&output.stderr);
        assert_eq!(output.status.code(), Some(2), "{args:?}: {stderr}");
        assert!(output.stdout.is_empty(), "{args:?}");
        assert!(stderr.starts_with("error: "), "{args:?}: {stderr}");
        assert!(stderr.contains(expected), "{args:?}: {stderr}");
        assert!(stderr.contains("Usage:"), "{args:?}: {stderr}");
    }
}

#[test]
fn compile_without_sources_points_at_fetch_sources() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    let output = temp.path().join("monaco.pmtiles");
    let pbf = monaco_pbf();
    let result = run::<[&OsStr; 6], _>([
        "compile".as_ref(),
        pbf.as_os_str(),
        "--output".as_ref(),
        output.as_os_str(),
        "--sources".as_ref(),
        sources.as_os_str(),
    ]);
    let stderr = text(&result.stderr);
    assert_eq!(result.status.code(), Some(1), "{stderr}");
    assert!(stderr.contains("fetch-sources"), "{stderr}");
    assert!(stderr.contains("water-polygons-split-3857.zip"), "{stderr}");
    assert!(!output.exists());
    assert!(!sources.exists(), "a failed check must not create sources");

    // Leaving the datasets out needs none of them, nor --sources.
    let result = run::<[&OsStr; 6], _>([
        "compile".as_ref(),
        pbf.as_os_str(),
        "--output".as_ref(),
        output.as_os_str(),
        "--omit-sources".as_ref(),
        "all".as_ref(),
    ]);
    let stderr = text(&result.stderr);
    assert!(result.status.success(), "{stderr}");
    assert!(
        stderr.contains("Leaving out water_polygons: No seas"),
        "{stderr}"
    );
    let metadata: serde_json::Value = serde_json::from_str(
        &osm_tile_compiler::archive::ArchiveReader::open(&output)
            .unwrap()
            .metadata()
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        metadata["omitted_sources"],
        serde_json::json!(["lake_centerlines", "natural_earth", "water_polygons"])
    );
    let unknown = run::<[&OsStr; 6], _>([
        "compile".as_ref(),
        pbf.as_os_str(),
        "--output".as_ref(),
        temp.path().join("other.pmtiles").as_os_str(),
        "--omit-sources".as_ref(),
        "coastlines".as_ref(),
    ]);
    assert_eq!(unknown.status.code(), Some(2));
    assert!(text(&unknown.stderr).contains("unknown dataset 'coastlines'"));
}

#[tokio::test(flavor = "multi_thread")]
async fn compiles_monaco_into_a_valid_archive() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let archive = temp.path().join("out/monaco.pmtiles");
    let pbf = monaco_pbf();
    let args: [&OsStr; 10] = [
        "compile".as_ref(),
        pbf.as_os_str(),
        "--output".as_ref(),
        archive.as_os_str(),
        "--sources".as_ref(),
        sources.as_os_str(),
        "--threads".as_ref(),
        "2".as_ref(),
        "--json".as_ref(),
        "--building-levels".as_ref(),
    ];
    // A default scratch directory this tool did not create is never removed.
    let scratch = temp.path().join("out/monaco.pmtiles.tmp");
    std::fs::create_dir_all(&scratch).unwrap();
    let refused = run(args);
    assert_eq!(refused.status.code(), Some(1));
    assert!(
        text(&refused.stderr).contains("is not an osm-tile-compiler scratch directory"),
        "{}",
        text(&refused.stderr)
    );
    // One a running compile holds is refused and left alone.
    let marker = std::fs::File::create(scratch.join(".osm-tile-compiler-scratch")).unwrap();
    std::fs::write(scratch.join("node-store.bin"), b"in use").unwrap();
    marker.try_lock().unwrap();
    let busy = run(args);
    assert_eq!(busy.status.code(), Some(1));
    assert!(
        text(&busy.stderr).contains("is in use by another compile"),
        "{}",
        text(&busy.stderr)
    );
    assert!(scratch.join("node-store.bin").exists());
    // One left by an interrupted compile, and an archive it was writing, are
    // cleared and reused. Unlock rather than only close: a process another
    // test spawns at the same moment can inherit a duplicate of this
    // descriptor until it execs, and that duplicate keeps an flock held.
    marker.unlock().unwrap();
    drop(marker);
    let abandoned = temp.path().join("out/monaco.pmtiles.1.partial");
    std::fs::write(&abandoned, b"half an archive").unwrap();
    let output = run(args);
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(
        stderr.contains("Removed what an interrupted compile left"),
        "{stderr}"
    );
    assert!(
        !abandoned.exists(),
        "an unlocked partial archive is removed"
    );
    let leftovers = std::fs::read_dir(temp.path().join("out"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .filter(|name| name != "monaco.pmtiles" && name != "monaco-buildings.pmtiles")
        .collect::<Vec<_>>();
    assert!(leftovers.is_empty(), "left behind: {leftovers:?}");
    assert!(
        temp.path().join("out/monaco-buildings.pmtiles").is_file(),
        "--building-levels writes the building levels beside the archive"
    );
    assert!(stderr.contains("Wrote "), "{stderr}");
    let result: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    let tiles = result["tiles"].as_u64().unwrap();
    assert!(tiles > 0, "{result}");
    assert!(result["profile_features"].as_u64().unwrap() > 0, "{result}");
    assert!(
        stderr.contains(&format!("{tiles} tiles")),
        "the summary reports the tile count: {stderr}"
    );
    let largest = &result["tile_stats"]["largest"][0];
    assert!(
        stderr.contains(&format!(
            "Largest tile: {} at ",
            largest["tile"].as_str().unwrap()
        )),
        "the summary names the largest tile: {stderr}"
    );
    let zooms = result["tile_stats"]["zooms"].as_array().unwrap();
    assert_eq!(
        zooms
            .iter()
            .map(|z| z["tiles"].as_u64().unwrap())
            .sum::<u64>(),
        tiles,
        "{result}"
    );
    assert!(
        !temp.path().join("out/monaco.pmtiles.tmp").exists(),
        "the default temp directory is removed"
    );

    // verify accepts the archive; inspect reports what the compile did.
    let verified = run(["verify".as_ref(), archive.as_os_str()]);
    assert!(verified.status.success(), "{}", text(&verified.stderr));
    assert!(
        text(&verified.stdout).contains(&format!("valid, {tiles} tiles checked")),
        "{}",
        text(&verified.stdout)
    );
    let inspected = run(["inspect".as_ref(), archive.as_os_str(), "--json".as_ref()]);
    assert!(inspected.status.success(), "{}", text(&inspected.stderr));
    let report: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
    assert_eq!(report["problem_count"], 0, "{report}");
    assert_eq!(report["tile_stats"], result["tile_stats"]);
    assert_eq!(report["metadata"]["name"], "OpenMapTiles");
    let described = run(["inspect".as_ref(), archive.as_os_str()]);
    assert!(text(&described.stdout).contains("No problems found."));
    // A tile's bytes overwritten: verify finds it and fails.
    let damaged = temp.path().join("damaged.pmtiles");
    let mut bytes = std::fs::read(&archive).unwrap();
    let data_offset = u64::from_le_bytes(bytes[56..64].try_into().unwrap()) as usize;
    bytes[data_offset..data_offset + 16].fill(0xff);
    std::fs::write(&damaged, bytes).unwrap();
    let rejected = run(["verify".as_ref(), damaged.as_os_str()]);
    assert_eq!(rejected.status.code(), Some(1));
    assert!(
        text(&rejected.stderr).contains("not valid gzip"),
        "{}",
        text(&rejected.stderr)
    );

    let reader = AsyncPmTilesReader::new_with_path(&archive).await.unwrap();
    let header = reader.get_header();
    assert_eq!(header.tile_type, TileType::Mvt);
    assert_eq!((header.min_zoom, header.max_zoom), (0, 14));
    assert_eq!(header.n_addressed_tiles().map(u64::from), Some(tiles));
    // Defaults to the PBF header's bounding box, which covers Monaco.
    assert!(header.min_longitude < 7.42 && header.max_longitude > 7.42);
    assert!(header.min_latitude < 43.73 && header.max_latitude > 43.73);

    // Every layer of the style draws one of the archive's source layers.
    let metadata: serde_json::Value =
        serde_json::from_str(&reader.get_metadata().await.unwrap()).unwrap();
    let declared = metadata["vector_layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|layer| layer["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    let style: serde_json::Value =
        serde_json::from_slice(&run(["style", "monaco.pmtiles"]).stdout).unwrap();
    for layer in style["layers"].as_array().unwrap() {
        if let Some(source_layer) = layer["source-layer"].as_str() {
            assert!(
                declared.contains(&source_layer),
                "{} draws {source_layer}, not in {declared:?}",
                layer["id"]
            );
        }
    }

    // An offline pack reads the archive's labels for the glyph ranges.
    let mirror = temp.path().join("mirror");
    style_mirror(&mirror);
    let pack = temp.path().join("pack");
    let output = run([
        "style".as_ref(),
        archive.as_os_str(),
        "--offline".as_ref(),
        pack.as_os_str(),
        "--glyphs".as_ref(),
        mirror.join("{fontstack}/{range}.pbf").as_os_str(),
        "--sprite".as_ref(),
        mirror.join("sprite").as_os_str(),
    ]);
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(!stderr.contains("not a local file"), "{stderr}");
    assert!(stderr.contains("glyph ranges of 3 fonts"), "{stderr}");
    let style: serde_json::Value =
        serde_json::from_slice(&std::fs::read(pack.join("style.json")).unwrap()).unwrap();
    assert_eq!(style["glyphs"], "fonts/{fontstack}/{range}.pbf");
    assert!(pack.join("fonts/Noto Sans Bold/0-255.pbf").is_file());

    // A selection writes only its layers and languages, and verifies.
    let selected = temp.path().join("out/selected.pmtiles");
    let output = run::<[&OsStr; 13], _>([
        "compile".as_ref(),
        pbf.as_os_str(),
        "--output".as_ref(),
        selected.as_os_str(),
        "--sources".as_ref(),
        sources.as_os_str(),
        "--threads".as_ref(),
        "2".as_ref(),
        "--layers".as_ref(),
        "poi, place".as_ref(),
        "--languages".as_ref(),
        "en".as_ref(),
        "--json".as_ref(),
    ]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let verified = run(["verify".as_ref(), selected.as_os_str()]);
    assert!(verified.status.success(), "{}", text(&verified.stdout));
    let reader = AsyncPmTilesReader::new_with_path(&selected).await.unwrap();
    let metadata: serde_json::Value =
        serde_json::from_str(&reader.get_metadata().await.unwrap()).unwrap();
    let layers = metadata["vector_layers"].as_array().unwrap();
    let ids = layers
        .iter()
        .map(|layer| layer["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["place", "poi"]);
    for layer in layers {
        let fields = layer["fields"].as_object().unwrap();
        let names = fields
            .keys()
            .filter(|key| key.starts_with("name"))
            .map(String::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            names,
            [
                "name",
                "name:en",
                "name:latin",
                "name_de",
                "name_en",
                "name_int"
            ],
            "{}",
            layer["id"]
        );
    }

    // --sar adds the search-and-rescue layer, last, and `style --sar` draws
    // only layers the archive declares.
    let with_sar = temp.path().join("out/sar.pmtiles");
    let output = run::<[&OsStr; 11], _>([
        "compile".as_ref(),
        pbf.as_os_str(),
        "--output".as_ref(),
        with_sar.as_os_str(),
        "--sources".as_ref(),
        sources.as_os_str(),
        "--threads".as_ref(),
        "2".as_ref(),
        "--layers".as_ref(),
        "place".as_ref(),
        "--sar".as_ref(),
    ]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let verified = run(["verify".as_ref(), with_sar.as_os_str()]);
    assert!(verified.status.success(), "{}", text(&verified.stdout));
    let reader = AsyncPmTilesReader::new_with_path(&with_sar).await.unwrap();
    let metadata: serde_json::Value =
        serde_json::from_str(&reader.get_metadata().await.unwrap()).unwrap();
    let ids = metadata["vector_layers"]
        .as_array()
        .unwrap()
        .iter()
        .map(|layer| layer["id"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, ["place", "sar"]);
    assert_eq!(metadata["sar_schema"], 1);
    let style: serde_json::Value =
        serde_json::from_slice(&run(["style", "sar.pmtiles", "--sar"]).stdout).unwrap();
    let sar_layers = style["layers"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|layer| layer["source-layer"] == "sar")
        .count();
    assert!(sar_layers >= 4, "{style}");
    for layer in style["layers"].as_array().unwrap() {
        if let Some(source_layer) = layer["source-layer"].as_str() {
            assert!(
                declared.contains(&source_layer) || source_layer == "sar",
                "{} draws {source_layer}",
                layer["id"]
            );
        }
    }

    // The archive is never replaced.
    let again = run(args);
    assert_eq!(again.status.code(), Some(1));
    assert!(
        text(&again.stderr).contains("refusing to replace existing archive"),
        "{}",
        text(&again.stderr)
    );
}

#[test]
fn profile_reports_every_stage_and_keeps_the_archive() {
    let dir = tempfile::tempdir().unwrap();
    let pbf = monaco_pbf();
    let compile = |name: &str, extra: &[&str]| {
        let output = dir.path().join(name);
        let mut args = vec![
            "compile".as_ref(),
            pbf.as_os_str(),
            "--output".as_ref(),
            output.as_os_str(),
            "--omit-sources".as_ref(),
            "all".as_ref(),
            "--max-zoom".as_ref(),
            "12".as_ref(),
            "--threads".as_ref(),
            "2".as_ref(),
        ];
        args.extend(extra.iter().map(OsStr::new));
        let result = run(args);
        assert!(result.status.success(), "{result:?}");
        (std::fs::read(output).unwrap(), text(&result.stderr))
    };
    let report = dir.path().join("profile.json");
    let (plain, _) = compile("plain.pmtiles", &[]);
    let (profiled, stderr) = compile("profiled.pmtiles", &["--profile", report.to_str().unwrap()]);
    assert_eq!(plain, profiled, "the profiler changed the archive");
    assert!(
        stderr.contains("Stage profile: 2 threads given"),
        "{stderr}"
    );
    let report: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&report).unwrap()).unwrap();
    assert_eq!(report["threads"], 2);
    let stages = report["stages"].as_array().unwrap();
    let names = stages
        .iter()
        .map(|s| s["name"].as_str().unwrap())
        .collect::<Vec<_>>();
    for stage in ["index_osm", "ways", "relations", "final_merge"] {
        assert!(names.contains(&stage), "{stage} missing from {names:?}");
    }
    for stage in stages {
        assert!(stage["wall_seconds"].as_f64().unwrap() >= 0.0);
    }
}

#[test]
fn diff_and_apply_rebuild_a_compiled_archive() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    let pbf = monaco_pbf();
    let compile = |output: &std::path::Path, extra: &[&str]| {
        let mut args = vec![
            "compile".as_ref(),
            pbf.as_os_str(),
            "--output".as_ref(),
            output.as_os_str(),
            "--omit-sources".as_ref(),
            "all".as_ref(),
            "--max-zoom".as_ref(),
            "12".as_ref(),
        ];
        args.extend(extra.iter().map(OsStr::new));
        let output = run(args);
        assert!(output.status.success(), "{output:?}");
    };
    let (old, new) = (path("old.pmtiles"), path("new.pmtiles"));
    compile(&old, &["--exclude-layers", "poi"]);
    compile(&new, &[]);

    let delta = path("update.delta");
    let output = run([
        "diff".as_ref(),
        old.as_os_str(),
        new.as_os_str(),
        "--output".as_ref(),
        delta.as_os_str(),
        "--json".as_ref(),
    ]);
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["format"], "osm-tile-compiler-delta");
    assert!(report["copied_bytes"].as_u64().unwrap() > 0, "{report}");
    assert!(report["tiles"]["changed"].as_u64().unwrap() > 0, "{report}");

    let rebuilt = path("rebuilt.pmtiles");
    let output = run([
        "apply".as_ref(),
        old.as_os_str(),
        delta.as_os_str(),
        "-o".as_ref(),
        rebuilt.as_os_str(),
    ]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read(&rebuilt).unwrap(),
        std::fs::read(&new).unwrap()
    );

    // Applied to the wrong archive, it fails and writes nothing.
    let wrong = path("wrong.pmtiles");
    let output = run([
        "apply".as_ref(),
        new.as_os_str(),
        delta.as_os_str(),
        "-o".as_ref(),
        wrong.as_os_str(),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(text(&output.stderr).contains("is not the archive this delta applies to"));
    assert!(!wrong.exists());
}

/// Serves one file on 127.0.0.1 as a static file server or object store
/// does: `Range` requests get `206 Partial Content`, unless `ranges` is
/// false, when every request gets the whole file. Counts the requests.
struct RangeServer {
    url: String,
    requests: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl RangeServer {
    fn start(file: &Path, ranges: bool) -> Self {
        use std::io::{BufRead, BufReader, Write};
        use std::sync::atomic::Ordering;
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}/archive.pmtiles", listener.local_addr().unwrap());
        let bytes = std::sync::Arc::new(std::fs::read(file).unwrap());
        let requests = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let counted = std::sync::Arc::clone(&requests);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let (bytes, counted) = (bytes.clone(), counted.clone());
                std::thread::spawn(move || -> std::io::Result<()> {
                    let mut reader = BufReader::new(stream.try_clone()?);
                    let mut range = None;
                    let mut line = String::new();
                    reader.read_line(&mut line)?;
                    loop {
                        let mut header = String::new();
                        if reader.read_line(&mut header)? == 0 || header == "\r\n" {
                            break;
                        }
                        if let Some(value) =
                            header.to_ascii_lowercase().strip_prefix("range: bytes=")
                        {
                            let (first, last) = value.trim().split_once('-').unwrap();
                            range = Some((
                                first.parse::<usize>().unwrap(),
                                last.parse::<usize>().unwrap(),
                            ));
                        }
                    }
                    counted.fetch_add(1, Ordering::Relaxed);
                    let mut stream = stream;
                    match range.filter(|_| ranges) {
                        Some((first, last)) => {
                            let last = last.min(bytes.len() - 1);
                            let body = &bytes[first..=last];
                            write!(
                                stream,
                                "HTTP/1.1 206 Partial Content\r\nContent-Length: {}\r\n\
                                 Content-Range: bytes {first}-{last}/{}\r\nETag: \"v1\"\r\n\
                                 Connection: close\r\n\r\n",
                                body.len(),
                                bytes.len()
                            )?;
                            stream.write_all(body)?;
                        }
                        None => {
                            write!(
                                stream,
                                "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                                bytes.len()
                            )?;
                            stream.write_all(&bytes)?;
                        }
                    }
                    stream.flush()
                });
            }
        });
        Self { url, requests }
    }

    fn requests(&self) -> u64 {
        self.requests.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// Runs the binary with no proxy, so that requests to 127.0.0.1 go straight
/// to a [`RangeServer`].
fn run_direct<I, S>(args: I) -> Output
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut command = Command::new(env!("CARGO_BIN_EXE_osm-tile-compiler"));
    command.args(args);
    for proxy in ["HTTPS_PROXY", "HTTP_PROXY", "ALL_PROXY"] {
        command.env_remove(proxy).env_remove(proxy.to_lowercase());
    }
    command.output().unwrap()
}

/// Every tile of a PMTiles archive: tile ID and stored bytes.
fn stored_tiles(path: &std::path::Path) -> std::collections::BTreeMap<u64, Vec<u8>> {
    let mut reader = osm_tile_compiler::archive::ArchiveReader::open(path).unwrap();
    let mut tiles = std::collections::BTreeMap::new();
    reader
        .visit_entries(|reader, entry| {
            let bytes = reader.tile(&entry)?;
            for i in 0..u64::from(entry.run_length) {
                tiles.insert(entry.tile_id + i, bytes.clone());
            }
            Ok(())
        })
        .unwrap();
    tiles
}

#[tokio::test]
async fn extract_cuts_the_tiles_a_bounded_compile_writes() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    let pbf = monaco_pbf();
    let compile = |output: &std::path::Path, extra: &[&str]| {
        let mut args = vec![
            "compile".as_ref(),
            pbf.as_os_str(),
            "--output".as_ref(),
            output.as_os_str(),
            "--omit-sources".as_ref(),
            "all".as_ref(),
            "--max-zoom".as_ref(),
            "14".as_ref(),
        ];
        args.extend(extra.iter().map(OsStr::new));
        let output = run(args);
        assert!(output.status.success(), "{output:?}");
    };
    let extract = |output: &std::path::Path, extra: &[&str]| {
        let mut args: Vec<std::ffi::OsString> = vec![
            "extract".into(),
            path("full.pmtiles").into_os_string(),
            "--output".into(),
            output.as_os_str().to_owned(),
            "--json".into(),
        ];
        args.extend(extra.iter().map(Into::into));
        run(args)
    };
    let verify = |archive: &std::path::Path| {
        let output = run(["verify".as_ref(), archive.as_os_str()]);
        assert!(output.status.success(), "{output:?}");
    };
    compile(&path("full.pmtiles"), &[]);
    let full = stored_tiles(&path("full.pmtiles"));

    // The port and the old town: a box well inside the extract.
    let bounds = ["7.418", "43.728", "7.426", "43.735"];
    let mut boxed_args = vec!["--bounds"];
    boxed_args.extend(bounds);
    compile(&path("boxed.pmtiles"), &boxed_args);
    let output = extract(&path("cut.pmtiles"), &boxed_args);
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    verify(&path("cut.pmtiles"));
    let cut = stored_tiles(&path("cut.pmtiles"));
    assert_eq!(report["tiles"], cut.len() as u64);
    assert_eq!(report["source_tiles"], full.len() as u64);
    assert!(cut.len() < full.len(), "{} of {}", cut.len(), full.len());
    // A subset of the source, byte for byte...
    for (id, bytes) in &cut {
        assert_eq!(
            full.get(id),
            Some(bytes),
            "tile {id} differs from the source"
        );
    }
    // ...at the positions a compile limited to the box writes.
    let boxed = stored_tiles(&path("boxed.pmtiles"));
    assert_eq!(
        cut.keys().collect::<Vec<_>>(),
        boxed.keys().collect::<Vec<_>>()
    );
    let reader = AsyncPmTilesReader::new_with_path(path("cut.pmtiles"))
        .await
        .unwrap();
    let header = reader.get_header();
    assert_eq!((header.min_longitude, header.max_latitude), (7.418, 43.735));
    assert_eq!(header.tile_type, TileType::Mvt);

    // A polygon, and the zooms: the tiles of a compile with --polygon.
    let polygon = path("port.geojson");
    std::fs::write(
        &polygon,
        r#"{"type":"Polygon","coordinates":[[[7.419,43.731],[7.428,43.733],[7.423,43.738],[7.419,43.731]]]}"#,
    )
    .unwrap();
    let polygon = polygon.to_str().unwrap();
    compile(
        &path("polygon.pmtiles"),
        &["--polygon", polygon, "--min-zoom", "10"],
    );
    let output = extract(
        &path("polygon-cut.pmtiles"),
        &["--polygon", polygon, "--min-zoom", "10"],
    );
    assert!(output.status.success(), "{output:?}");
    verify(&path("polygon-cut.pmtiles"));
    let cut = stored_tiles(&path("polygon-cut.pmtiles"));
    let compiled = stored_tiles(&path("polygon.pmtiles"));
    assert_eq!(
        cut.keys().collect::<Vec<_>>(),
        compiled.keys().collect::<Vec<_>>()
    );
    let metadata: serde_json::Value = serde_json::from_str(
        &AsyncPmTilesReader::new_with_path(path("polygon-cut.pmtiles"))
            .await
            .unwrap()
            .get_metadata()
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(
        (&metadata["minzoom"], &metadata["maxzoom"]),
        (&10.into(), &14.into())
    );

    // Zooms alone.
    let output = extract(&path("low.pmtiles"), &["--max-zoom", "8"]);
    assert!(output.status.success(), "{output:?}");
    verify(&path("low.pmtiles"));
    let low = stored_tiles(&path("low.pmtiles"));
    let expected = full
        .iter()
        .filter(|(id, _)| pmtiles::TileCoord::from(pmtiles::TileId::new(**id).unwrap()).z() <= 8)
        .map(|(id, bytes)| (*id, bytes.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    assert_eq!(low, expected);

    // A box the archive does not reach, or an existing output: an error,
    // and nothing written.
    let output = extract(
        &path("nowhere.pmtiles"),
        &["--bounds", "10", "50", "11", "51"],
    );
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        text(&output.stderr).contains("does not overlap"),
        "{output:?}"
    );
    assert!(!path("nowhere.pmtiles").exists());

    // Over HTTP, by range requests: the same archive as from the file.
    let server = RangeServer::start(&path("full.pmtiles"), true);
    let remote = |output: &std::path::Path, extra: &[&str]| {
        let mut args: Vec<std::ffi::OsString> = vec![
            "extract".into(),
            server.url.clone().into(),
            "--output".into(),
            output.as_os_str().to_owned(),
        ];
        args.extend(extra.iter().map(Into::into));
        run_direct(args)
    };
    let output = remote(&path("remote-cut.pmtiles"), &boxed_args);
    assert!(output.status.success(), "{output:?}");
    assert!(
        text(&output.stderr).contains("range requests"),
        "{output:?}"
    );
    assert_eq!(
        std::fs::read(path("remote-cut.pmtiles")).unwrap(),
        std::fs::read(path("cut.pmtiles")).unwrap()
    );
    let mut json_args = boxed_args.clone();
    json_args.push("--json");
    let output = remote(&path("remote-cut-2.pmtiles"), &json_args);
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert!(report["requests"].as_u64().unwrap() >= 2, "{report}");
    // Monaco is small enough that its nearby tiles come in one range.
    let full_bytes = std::fs::metadata(path("full.pmtiles")).unwrap().len();
    assert!(
        report["bytes_read"].as_u64().unwrap() <= full_bytes,
        "{report}, of {full_bytes} bytes"
    );
    let output = remote(
        &path("remote-polygon-cut.pmtiles"),
        &["--polygon", polygon, "--min-zoom", "10"],
    );
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read(path("remote-polygon-cut.pmtiles")).unwrap(),
        std::fs::read(path("polygon-cut.pmtiles")).unwrap()
    );
    assert!(server.requests() >= 6);
    // A server that ignores ranges is refused, before anything is written.
    let whole = RangeServer::start(&path("full.pmtiles"), false);
    let output = run_direct([
        "extract".as_ref(),
        whole.url.as_ref(),
        "--output".as_ref(),
        path("whole.pmtiles").as_os_str(),
        "--max-zoom".as_ref(),
        "8".as_ref(),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        text(&output.stderr).contains("does not answer range requests"),
        "{output:?}"
    );
    assert!(!path("whole.pmtiles").exists());

    let output = extract(&path("cut.pmtiles"), &boxed_args);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        text(&output.stderr).contains("refusing to replace"),
        "{output:?}"
    );
    let leftovers = std::fs::read_dir(dir.path())
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().contains("partial"))
        .count();
    assert_eq!(leftovers, 0);
}

/// Two planet-style changes: a motorway drawn in Monaco, which the update
/// keeps, and a node in Sydney, which it clips away.
const MONACO_CHANGES: &str = r#"<?xml version='1.0' encoding='UTF-8'?>
<osmChange version="0.6" generator="test">
  <create>
    <node id="99000000001" version="1" lat="43.7350" lon="7.4200"/>
    <node id="99000000002" version="1" lat="43.7370" lon="7.4250"/>
    <node id="99000000003" version="1" lat="-33.8688" lon="151.2093">
      <tag k="amenity" v="cafe"/>
    </node>
    <way id="99000000010" version="1">
      <nd ref="99000000001"/>
      <nd ref="99000000002"/>
      <tag k="highway" v="motorway"/>
      <tag k="name" v="Test motorway"/>
    </way>
  </create>
</osmChange>"#;

#[test]
fn change_files_update_an_extract_and_a_compile() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    let pbf = monaco_pbf();
    let changes = path("changes.osc");
    std::fs::write(&changes, MONACO_CHANGES).unwrap();

    let updated = path("updated.osm.pbf");
    let output = run([
        "update".as_ref(),
        pbf.as_os_str(),
        changes.as_os_str(),
        "--output".as_ref(),
        updated.as_os_str(),
        "--json".as_ref(),
    ]);
    assert!(output.status.success(), "{output:?}");
    let report: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(report["changes_read"], 4, "{report}");
    assert_eq!(report["changes_kept"], 3, "{report}");
    assert_eq!(report["created"], 3, "{report}");
    assert_eq!(report["modified"], 0, "{report}");
    assert!(report["blobs_copied"].as_u64().unwrap() > 0, "{report}");
    assert!(updated.is_file());
    // Never over an existing file.
    let output = run([
        "update".as_ref(),
        pbf.as_os_str(),
        changes.as_os_str(),
        "--output".as_ref(),
        updated.as_os_str(),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");

    let compile = |output: &std::path::Path, extra: &[&OsStr]| {
        let mut args = vec![
            "compile".as_ref(),
            pbf.as_os_str(),
            "--output".as_ref(),
            output.as_os_str(),
            "--omit-sources".as_ref(),
            "all".as_ref(),
            "--max-zoom".as_ref(),
            "12".as_ref(),
        ];
        args.extend(extra.iter().copied());
        let output = run(args);
        assert!(output.status.success(), "{output:?}");
        output
    };
    let (old, new) = (path("old.pmtiles"), path("new.pmtiles"));
    compile(&old, &[]);
    let output = compile(
        &new,
        &[
            "--changes".as_ref(),
            changes.as_os_str(),
            "--delta-from".as_ref(),
            old.as_os_str(),
        ],
    );
    let stderr = text(&output.stderr);
    assert!(stderr.contains("3 objects created"), "{stderr}");
    assert!(!path("new.pmtiles.tmp").exists(), "{stderr}");
    let delta = path("new.delta");
    assert!(stderr.contains("new.delta"), "{stderr}");
    assert!(!stderr.contains(" 0 changed, 0 added"), "{stderr}");

    let rebuilt = path("rebuilt.pmtiles");
    let output = run([
        "apply".as_ref(),
        old.as_os_str(),
        delta.as_os_str(),
        "-o".as_ref(),
        rebuilt.as_os_str(),
    ]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        std::fs::read(&rebuilt).unwrap(),
        std::fs::read(&new).unwrap()
    );
}

/// A glyph and sprite mirror holding only the first Latin range.
fn style_mirror(mirror: &Path) {
    for font in ["Noto Sans Regular", "Noto Sans Bold", "Noto Sans Italic"] {
        std::fs::create_dir_all(mirror.join(font)).unwrap();
        std::fs::write(mirror.join(font).join("0-255.pbf"), b"glyphs").unwrap();
    }
    std::fs::write(mirror.join("sprite.json"), b"{}").unwrap();
    std::fs::write(mirror.join("sprite.png"), b"png").unwrap();
}

#[test]
fn an_offline_style_pack_for_a_remote_archive_has_the_latin_glyphs() {
    let temp = tempfile::tempdir().unwrap();
    let mirror = temp.path().join("mirror");
    style_mirror(&mirror);
    let pack = temp.path().join("pack");
    let output = run([
        "style".as_ref(),
        "https://example.com/region.pmtiles".as_ref(),
        "--offline".as_ref(),
        pack.as_os_str(),
        "--glyphs".as_ref(),
        mirror.join("{fontstack}/{range}.pbf").as_os_str(),
        "--sprite".as_ref(),
        mirror.join("sprite").as_os_str(),
        "--base-url".as_ref(),
        "file:///sdcard/maps/pack".as_ref(),
    ]);
    let stderr = text(&output.stderr);
    assert!(output.status.success(), "{stderr}");
    assert!(output.stdout.is_empty());
    assert!(stderr.contains("not a local file"), "{stderr}");
    assert!(
        stderr.contains("left out: fonts/Noto Sans Bold/256-511.pbf"),
        "{stderr}"
    );
    let style: serde_json::Value =
        serde_json::from_slice(&std::fs::read(pack.join("style.json")).unwrap()).unwrap();
    assert_eq!(
        style["sources"]["openmaptiles"]["url"],
        "pmtiles://https://example.com/region.pmtiles"
    );
    assert_eq!(
        style["glyphs"],
        "file:///sdcard/maps/pack/fonts/{fontstack}/{range}.pbf"
    );
    assert_eq!(style["sprite"], "file:///sdcard/maps/pack/sprite");
    assert!(pack.join("LICENSE.md").is_file());

    // The pack is not overwritten.
    let again = run([
        "style".as_ref(),
        "a.pmtiles".as_ref(),
        "--offline".as_ref(),
        pack.as_os_str(),
    ]);
    assert_eq!(again.status.code(), Some(1));
    assert!(text(&again.stderr).contains("already exists"));
}

#[test]
fn style_points_at_the_archive() {
    let output = run(["style", "https://example.com/tiles/region.pmtiles"]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let style: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(style["version"], 8);
    let source = &style["sources"]["openmaptiles"];
    assert_eq!(source["type"], "vector");
    assert_eq!(
        source["url"],
        "pmtiles://https://example.com/tiles/region.pmtiles"
    );
    assert!(
        source["attribution"]
            .as_str()
            .unwrap()
            .contains("OpenMapTiles"),
        "{source}"
    );
    assert_eq!(
        style["glyphs"],
        "https://demotiles.maplibre.org/font/{fontstack}/{range}.pbf"
    );
    assert_eq!(
        style["sprite"],
        "https://openmaptiles.github.io/osm-bright-gl-style/sprite"
    );

    let output = run([
        "style",
        "pmtiles://region.pmtiles",
        "--glyphs",
        "/fonts/{fontstack}/{range}.pbf",
        "--sprite",
        "/sprites/bright",
    ]);
    assert!(output.status.success(), "{}", text(&output.stderr));
    let style: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(
        style["sources"]["openmaptiles"]["url"],
        "pmtiles://region.pmtiles"
    );
    assert_eq!(style["glyphs"], "/fonts/{fontstack}/{range}.pbf");
    assert_eq!(style["sprite"], "/sprites/bright");
}

#[test]
fn worker_mode_still_reports_bad_arguments_as_error_events() {
    let usage = serde_json::json!({
        "protocolVersion": 1,
        "event": "error",
        "message": "usage: osm-tile-compiler [--parent-stdin] --request <request.json>",
        "errorKind": "invalid_request",
    });
    for args in [&["--request"][..], &["--request", "a.json", "extra"]] {
        let output = run(args);
        assert_eq!(output.status.code(), Some(1), "{args:?}");
        let stdout = text(&output.stdout);
        let lines = stdout.lines().collect::<Vec<_>>();
        assert_eq!(lines.len(), 1, "{args:?}: one event line: {stdout}");
        let event: serde_json::Value = serde_json::from_str(lines[0]).unwrap();
        assert_eq!(event, usage, "{args:?}");
    }

    let temp = tempfile::tempdir().unwrap();
    // The selection fields are read, and checked before any download.
    let pbf = monaco_pbf();
    for (selection, expected) in [
        (
            serde_json::json!({"layers": ["poi"], "excludeLayers": ["water"]}),
            "not both",
        ),
        (
            serde_json::json!({"excludeLayers": ["roads"]}),
            "unknown layer 'roads'",
        ),
        (
            serde_json::json!({"layers": ["poi"], "languages": ["en", "xx"]}),
            "unknown language 'xx'",
        ),
    ] {
        let mut request = serde_json::json!({
            "inputPbfPath": pbf,
            "sourcesDir": temp.path().join("sources"),
            "outputPmtilesPath": temp.path().join("out.pmtiles"),
            "tempDir": temp.path().join("temp"),
            "manifestPath": temp.path().join("manifest.json"),
        });
        request
            .as_object_mut()
            .unwrap()
            .extend(selection.as_object().unwrap().clone());
        let path = temp.path().join("request.json");
        std::fs::write(&path, request.to_string()).unwrap();
        let output = run::<[&OsStr; 2], _>(["--request".as_ref(), path.as_os_str()]);
        assert_eq!(output.status.code(), Some(1), "{selection}");
        let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        assert_eq!(event["event"], "error", "{selection}");
        let message = event["message"].as_str().unwrap();
        assert!(message.contains(expected), "{selection}: {message}");
        assert_eq!(event["errorKind"], "invalid_request", "{selection}");
        assert!(!temp.path().join("sources").exists(), "{selection}");
    }

    let missing = temp.path().join("missing.json");
    let output = run::<[&OsStr; 2], _>(["--request".as_ref(), missing.as_os_str()]);
    assert_eq!(output.status.code(), Some(1));
    let event: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
    assert_eq!(event["event"], "error");
    assert_eq!(event["protocolVersion"], 1);
    assert_eq!(event["errorKind"], "invalid_request");
    assert!(
        event["message"]
            .as_str()
            .unwrap()
            .starts_with("Failed to read compiler request"),
        "{event}"
    );
}

/// Ctrl-C during a compile stops it with status 130 and leaves nothing
/// behind: no archive, no partial archive, no scratch directory.
#[cfg(unix)]
#[test]
fn ctrl_c_cancels_a_compile_and_removes_its_scratch() {
    use std::io::{BufRead, BufReader};
    use std::process::Stdio;

    let dir = tempfile::tempdir().unwrap();
    let output = dir.path().join("out.pmtiles");
    let mut child = Command::new(env!("CARGO_BIN_EXE_osm-tile-compiler"))
        .args(["compile".as_ref(), monaco_pbf().as_os_str()])
        .args(["--output".as_ref(), output.as_os_str()])
        .args(["--omit-sources", "all", "--threads", "2"])
        .stderr(Stdio::piped())
        .spawn()
        .unwrap();
    // The first stage line means the compile is under way, with its scratch
    // directory claimed.
    let mut stderr = BufReader::new(child.stderr.take().unwrap());
    let mut seen = String::new();
    loop {
        let mut line = String::new();
        assert!(stderr.read_line(&mut line).unwrap() > 0, "{seen}");
        seen.push_str(&line);
        if line.starts_with('[') {
            break;
        }
    }
    assert!(dir.path().join("out.pmtiles.tmp").is_dir(), "{seen}");
    let kill = Command::new("kill")
        .args(["-INT", &child.id().to_string()])
        .status()
        .unwrap();
    assert!(kill.success());
    let mut rest = String::new();
    std::io::Read::read_to_string(&mut stderr, &mut rest).unwrap();
    seen.push_str(&rest);
    let status = child.wait().unwrap();
    assert_eq!(status.code(), Some(130), "{seen}");
    assert!(seen.contains("the compile was cancelled"), "{seen}");
    let left: Vec<_> = std::fs::read_dir(dir.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(left.is_empty(), "left behind: {left:?}\n{seen}");
}

#[test]
fn a_merge_cache_reuses_unchanged_tiles_and_changes_nothing() {
    let dir = tempfile::tempdir().unwrap();
    let path = |name: &str| dir.path().join(name);
    let pbf = monaco_pbf();
    let changes = path("changes.osc");
    std::fs::write(&changes, MONACO_CHANGES).unwrap();
    let compile = |output: &std::path::Path, extra: &[&OsStr]| {
        let mut args = vec![
            "compile".as_ref(),
            pbf.as_os_str(),
            "--output".as_ref(),
            output.as_os_str(),
            "--omit-sources".as_ref(),
            "all".as_ref(),
            "--max-zoom".as_ref(),
            "13".as_ref(),
            "--json".as_ref(),
        ];
        args.extend(extra.iter().copied());
        let output = run(args);
        assert!(output.status.success(), "{output:?}");
        serde_json::from_slice::<serde_json::Value>(&output.stdout).unwrap()
    };
    let old = path("old.pmtiles");
    let result = compile(&old, &["--merge-cache".as_ref()]);
    assert_eq!(result["merge_cache"]["layers_reused"], 0, "{result}");
    assert!(path("old.merge-cache").is_file());

    // After a change, reuse writes the archive a compile without it writes,
    // and takes most tile layers from the old archive.
    let (plain, cached) = (path("plain.pmtiles"), path("cached.pmtiles"));
    let with_changes = ["--changes".as_ref(), changes.as_os_str()];
    let result = compile(&plain, &with_changes);
    assert!(result.get("merge_cache").is_none(), "{result}");
    let result = compile(
        &cached,
        &[
            with_changes[0],
            with_changes[1],
            "--reuse-from".as_ref(),
            old.as_os_str(),
        ],
    );
    assert_eq!(
        std::fs::read(&plain).unwrap(),
        std::fs::read(&cached).unwrap()
    );
    let stats = &result["merge_cache"];
    let (reused, encoded) = (
        stats["layers_reused"].as_u64().unwrap(),
        stats["layers_encoded"].as_u64().unwrap(),
    );
    assert!(encoded > 0 && reused > encoded, "{result}");
    assert!(stats["tiles_reused"].as_u64().unwrap() > 0, "{result}");
    assert!(path("cached.merge-cache").is_file());

    // At another gzip level whole tiles cannot be copied, but layers can.
    let (plain9, cached9) = (path("plain9.pmtiles"), path("cached9.pmtiles"));
    let level = ["--gzip-level".as_ref(), "9".as_ref()];
    compile(&plain9, &level);
    let result = compile(
        &cached9,
        &[level[0], level[1], "--reuse-from".as_ref(), old.as_os_str()],
    );
    assert_eq!(
        std::fs::read(&plain9).unwrap(),
        std::fs::read(&cached9).unwrap()
    );
    assert_eq!(result["merge_cache"]["tiles_reused"], 0, "{result}");
    assert_eq!(result["merge_cache"]["layers_encoded"], 0, "{result}");

    // An index beside another archive is refused.
    std::fs::copy(path("cached.merge-cache"), path("old.merge-cache")).unwrap();
    let wrong = path("wrong.pmtiles");
    let output = run([
        "compile".as_ref(),
        pbf.as_os_str(),
        "--output".as_ref(),
        wrong.as_os_str(),
        "--omit-sources".as_ref(),
        "all".as_ref(),
        "--max-zoom".as_ref(),
        "13".as_ref(),
        "--reuse-from".as_ref(),
        old.as_os_str(),
    ]);
    assert_eq!(output.status.code(), Some(1), "{output:?}");
    assert!(
        text(&output.stderr).contains("does not belong to"),
        "{output:?}"
    );
    assert!(!wrong.exists());
}
