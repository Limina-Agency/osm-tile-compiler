//! The library entry point an app calls: `compile_basemap` on
//! `fixtures/monaco.osm.pbf`, with progress, cancelled at the start of each
//! stage, without the supplemental datasets, and against the command line.

mod common;
#[path = "common/mvt.rs"]
#[allow(dead_code)]
mod mvt;

use common::{monaco_pbf, synthetic_sources};
use osm_tile_compiler::{
    BasemapRequest, CompileError, CompileProgress, CompileStage, SourcesPolicy,
    SupplementalDataset, archive::ArchiveReader, building_levels_path, compile_basemap,
};
use std::cell::{Cell, RefCell};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

fn request(dir: &Path, sources: &Path, threads: usize) -> BasemapRequest {
    let mut request = BasemapRequest::new(
        monaco_pbf(),
        dir.join("out/monaco.pmtiles"),
        sources,
        dir.join("temp"),
    );
    request.threads = threads;
    // Every stage, building levels included.
    request.building_levels = true;
    request
}

/// Every file under `dir`, relative to it.
fn files(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else {
                found.push(path.strip_prefix(dir).unwrap().to_path_buf());
            }
        }
    }
    found.sort();
    found
}

fn metadata(archive: &Path) -> serde_json::Value {
    let text = ArchiveReader::open(archive).unwrap().metadata().unwrap();
    serde_json::from_str(&text).unwrap()
}

/// The `class` of every feature of `layer` in the archive's tiles.
fn classes(archive: &Path, layer: &str) -> Vec<String> {
    let mut reader = ArchiveReader::open(archive).unwrap();
    let mut classes = Vec::new();
    reader
        .visit_entries(|reader, entry| {
            let mut tile = Vec::new();
            flate2::read::GzDecoder::new(&reader.tile(&entry)?[..])
                .read_to_end(&mut tile)
                .map_err(|e| e.to_string())?;
            for (name, features) in mvt::decode_tile(&tile) {
                if name != layer {
                    continue;
                }
                for feature in features {
                    if let Some(class) = feature
                        .attributes
                        .split(['\n', ' ', ','])
                        .find_map(|field| field.strip_prefix("class="))
                    {
                        classes.push(class.trim_matches('"').to_string());
                    }
                }
            }
            Ok(())
        })
        .unwrap();
    classes
}

#[test]
fn compiles_monaco_reporting_every_stage() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let mut request = request(temp.path(), &sources, 2);
    // Every dataset is present, so nothing is downloaded, but the stage runs.
    request.sources_policy = SourcesPolicy::FetchMissing;
    // A second extract, the same one, so that the merge runs too.
    request.extra_inputs = vec![monaco_pbf()];
    let reports = RefCell::new(Vec::<CompileProgress>::new());
    let output = compile_basemap(
        &request,
        |progress| reports.borrow_mut().push(progress.clone()),
        || false,
    )
    .unwrap();

    let reports = reports.into_inner();
    let mut stages = reports.iter().map(|r| r.stage).collect::<Vec<_>>();
    stages.dedup();
    assert_eq!(stages, CompileStage::ALL, "{reports:#?}");
    let mut overall = 0.0;
    for report in &reports {
        assert!(!report.message.is_empty(), "{report:?}");
        assert!(
            report.fraction.is_none_or(|f| (0.0..=1.0).contains(&f)),
            "{report:?}"
        );
        assert!(
            report.overall >= overall && report.overall <= 1.0,
            "{reports:#?}"
        );
        overall = report.overall;
    }
    assert!(output.result.tiles > 0);
    assert!(output.result.omitted_sources.is_empty());
    assert_eq!(output.archive, request.output);
    let (levels, report) = output.building_levels.as_ref().unwrap();
    assert_eq!(levels, &building_levels_path(&request.output));
    assert!(!report.levels.is_empty());
    assert!(metadata(&request.output).get("omitted_sources").is_none());
    // The scratch directory the compile made is gone; only the outputs and
    // the extracted datasets remain.
    assert!(!request.temp_dir.exists());
    assert_eq!(
        files(&temp.path().join("out")),
        [
            PathBuf::from("monaco-buildings.pmtiles"),
            PathBuf::from("monaco.pmtiles")
        ]
    );
    assert_eq!(output.manifest()["format"], "basemap-manifest");
    assert_eq!(output.manifest()["formatVersion"], 1);
    assert_eq!(output.manifest()["schema"], "OpenMapTiles");
    assert_eq!(output.manifest()["version"], "3.16.0");
    assert!(output.manifest().get("omittedSources").is_none());
}

#[test]
fn cancelling_at_each_stage_stops_at_once_and_leaves_nothing() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    // Extract the datasets once, so every run below sees the same sources.
    let warm = temp.path().join("warm");
    compile_basemap(&request(&warm, &sources, 2), |_| {}, || false).unwrap();
    let sources_before = files(&sources);

    for stage in CompileStage::ALL {
        let dir = temp.path().join(stage.id());
        let mut request = request(&dir, &sources, 2);
        request.sources_policy = SourcesPolicy::FetchMissing;
        request.extra_inputs = vec![monaco_pbf()];
        let asked = Cell::new(None::<Instant>);
        let seen = RefCell::new(Vec::new());
        let result = compile_basemap(
            &request,
            |progress| {
                seen.borrow_mut().push(progress.stage);
                if progress.stage == stage && asked.get().is_none() {
                    asked.set(Some(Instant::now()));
                }
            },
            || asked.get().is_some(),
        );
        let latency = asked.get().expect("the stage was reported").elapsed();
        assert_eq!(result.unwrap_err(), CompileError::Cancelled, "{stage:?}");
        assert!(latency < Duration::from_secs(1), "{stage:?}: {latency:?}");
        // Nothing after the stage was started.
        assert_eq!(seen.borrow().last(), Some(&stage), "{stage:?}");
        assert!(!request.output.exists(), "{stage:?}");
        assert!(!building_levels_path(&request.output).exists(), "{stage:?}");
        assert!(!request.temp_dir.exists(), "{stage:?}");
        assert_eq!(files(&dir), Vec::<PathBuf>::new(), "{stage:?}");
        assert_eq!(files(&sources), sources_before, "{stage:?}");
    }
}

#[test]
fn an_extract_merged_with_itself_compiles_to_the_same_archive() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let mut single = request(&temp.path().join("single"), &sources, 2);
    single.building_levels = false;
    compile_basemap(&single, |_| {}, || false).unwrap();
    let mut merged = request(&temp.path().join("merged"), &sources, 2);
    merged.building_levels = false;
    merged.extra_inputs = vec![monaco_pbf(), monaco_pbf()];
    let output = compile_basemap(&merged, |_| {}, || false).unwrap();
    assert_eq!(output.bounds, single_bounds(&single));
    assert_eq!(
        std::fs::read(&single.output).unwrap(),
        std::fs::read(&merged.output).unwrap()
    );
    // The merged file was scratch, removed with the rest.
    assert!(!merged.temp_dir.exists());
}

/// The area a single-extract compile of `request` covers: its header box.
fn single_bounds(request: &BasemapRequest) -> osm_tile_compiler::CompileBounds {
    osm_tile_compiler::pbf_bounds(&request.input_pbf).unwrap()
}

#[test]
fn compiles_without_supplemental_datasets_and_says_so() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);

    // The synthetic water polygon is the sea south of Monaco.
    let full = request(&temp.path().join("full"), &sources, 2);
    compile_basemap(&full, |_| {}, || false).unwrap();
    assert!(classes(&full.output, "water").iter().any(|c| c == "ocean"));

    let mut no_water = request(&temp.path().join("no-water"), &sources, 2);
    no_water.omit_sources = vec![SupplementalDataset::WaterPolygons];
    let output = compile_basemap(&no_water, |_| {}, || false).unwrap();
    assert_eq!(output.result.omitted_sources, ["water_polygons"]);
    assert_eq!(output.omitted_sources, [SupplementalDataset::WaterPolygons]);
    assert_eq!(
        metadata(&no_water.output)["omitted_sources"],
        serde_json::json!(["water_polygons"])
    );
    assert!(
        !classes(&no_water.output, "water")
            .iter()
            .any(|c| c == "ocean")
    );

    // Without any: the sources directory need not exist, and is not made.
    let missing = temp.path().join("no-sources");
    for policy in [SourcesPolicy::UsePresent, SourcesPolicy::FetchAndVerify] {
        let mut none = request(&temp.path().join(format!("none-{policy:?}")), &missing, 2);
        none.sources_policy = policy;
        none.omit_sources = SupplementalDataset::ALL.to_vec();
        let output = compile_basemap(&none, |_| {}, || false).unwrap();
        assert!(output.result.tiles > 0);
        assert_eq!(
            output.result.omitted_sources,
            ["lake_centerlines", "natural_earth", "water_polygons"]
        );
        assert_eq!(
            metadata(&none.output)["omitted_sources"],
            serde_json::json!(["lake_centerlines", "natural_earth", "water_polygons"])
        );
        assert_eq!(
            output.manifest()["omittedSources"],
            serde_json::json!(["lake_centerlines", "natural_earth", "water_polygons"])
        );
        assert!(!missing.exists());
    }

    // A dataset neither omitted nor present is an error, before any work.
    let mut incomplete = request(&temp.path().join("incomplete"), &missing, 2);
    incomplete.omit_sources = vec![SupplementalDataset::WaterPolygons];
    match compile_basemap(&incomplete, |_| {}, || false) {
        Err(CompileError::InvalidRequest(message)) => assert!(
            message.contains("lake_centerline.shp.zip")
                && message.contains("natural_earth_vector.sqlite.zip")
                && !message.contains("water-polygons"),
            "{message}"
        ),
        other => panic!("{other:?}"),
    }
    assert!(!incomplete.output.exists());
}

#[test]
fn building_levels_are_written_only_when_asked_for() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let mut request = request(temp.path(), &sources, 2);
    request.building_levels = BasemapRequest::new("a", "b", "c", "d").building_levels;
    assert!(!request.building_levels, "off by default");
    let reports = RefCell::new(Vec::<CompileProgress>::new());
    let output = compile_basemap(
        &request,
        |progress| reports.borrow_mut().push(progress.clone()),
        || false,
    )
    .unwrap();
    assert!(output.building_levels.is_none());
    assert!(!building_levels_path(&request.output).exists());
    let reports = reports.into_inner();
    assert!(
        reports
            .iter()
            .all(|r| r.stage != CompileStage::BuildingLevels),
        "{reports:#?}"
    );
    // Without building levels the other stages share all of `overall`, so
    // writing the tiles takes it to 1.
    let total: f64 = CompileStage::ALL
        .iter()
        .filter(|s| **s != CompileStage::BuildingLevels)
        .map(|s| s.weight())
        .sum();
    let before = total - CompileStage::WriteTiles.weight();
    let mut checked = 0;
    for report in reports
        .iter()
        .filter(|r| r.stage == CompileStage::WriteTiles)
    {
        let expected =
            (before + CompileStage::WriteTiles.weight() * report.fraction.unwrap_or(0.0)) / total;
        assert!((report.overall - expected).abs() < 1e-9, "{report:?}");
        checked += 1;
    }
    assert!(checked > 0, "{reports:#?}");
}

#[test]
fn the_library_writes_what_the_command_line_writes() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let library = request(&temp.path().join("library"), &sources, 2);
    compile_basemap(&library, |_| {}, || false).unwrap();

    let cli = temp.path().join("cli/monaco.pmtiles");
    let output = Command::new(env!("CARGO_BIN_EXE_osm-tile-compiler"))
        .arg("compile")
        .arg(monaco_pbf())
        .arg("--output")
        .arg(&cli)
        .arg("--sources")
        .arg(&sources)
        .args(["--threads", "4", "--building-levels"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    for (ours, theirs) in [
        (library.output.clone(), cli.clone()),
        (
            building_levels_path(&library.output),
            building_levels_path(&cli),
        ),
    ] {
        assert!(
            std::fs::read(&ours).unwrap() == std::fs::read(&theirs).unwrap(),
            "{} differs from {}",
            ours.display(),
            theirs.display()
        );
    }
}

#[test]
fn a_failed_compile_reports_the_failure_and_cleans_up() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let mut request = request(temp.path(), &sources, 1);
    request.input_pbf = temp.path().join("missing.osm.pbf");
    match compile_basemap(&request, |_| {}, || false) {
        Err(CompileError::InvalidRequest(message)) => assert!(message.contains("missing.osm.pbf")),
        other => panic!("{other:?}"),
    }
    request.input_pbf = monaco_pbf();
    request.threads = 0;
    assert!(matches!(
        compile_basemap(&request, |_| {}, || false),
        Err(CompileError::InvalidRequest(_))
    ));
    request.threads = 1;
    request.layers = osm_tile_compiler::LayerSelection::except(["roads"]);
    match compile_basemap(&request, |_| {}, || false) {
        Err(CompileError::InvalidRequest(message)) => {
            assert!(message.contains("roads"), "{message}")
        }
        other => panic!("{other:?}"),
    }
    assert!(!request.temp_dir.exists());
    assert!(!request.output.exists());
}
