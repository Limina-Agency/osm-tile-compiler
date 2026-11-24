//! Resumable compiles of `fixtures/monaco.osm.pbf`: interrupted at each
//! checkpoint, by a cancel through the API or by the process aborting as a
//! kill would end it, and resumed into the archive an uninterrupted compile
//! writes, byte for byte.

mod common;

use common::{monaco_pbf, synthetic_sources};
use osm_tile_compiler::{
    BasemapRequest, CompileError, CompileStage, checkpoint_directory, compile_basemap,
    discard_checkpoint,
};
use std::cell::Cell;
use std::ffi::OsString;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn request(dir: &Path, sources: &Path) -> BasemapRequest {
    let mut request = BasemapRequest::new(
        monaco_pbf(),
        dir.join("basemap.pmtiles"),
        sources,
        dir.join("temp"),
    );
    request.threads = 2;
    request.building_levels = true;
    request
}

/// The archive and building levels of an uninterrupted compile.
fn reference(temp: &Path, sources: &Path) -> (Vec<u8>, Vec<u8>) {
    let dir = temp.join("reference");
    let output = compile_basemap(&request(&dir, sources), |_| {}, || false).unwrap();
    let levels = output.building_levels.expect("Monaco has buildings").0;
    (
        fs::read(&output.archive).unwrap(),
        fs::read(levels).unwrap(),
    )
}

#[test]
fn a_cancelled_compile_resumes_from_its_last_checkpoint() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let (archive, levels) = reference(temp.path(), &sources);
    // Cancelled as each stage starts, and the stage the next attempt
    // continues from.
    for (cancel_at, resumes_at) in [
        (CompileStage::ReadOsm, None),
        (CompileStage::WriteTiles, Some(CompileStage::WriteTiles)),
        (
            CompileStage::BuildingLevels,
            Some(CompileStage::BuildingLevels),
        ),
    ] {
        let dir = temp.path().join(cancel_at.id());
        let mut request = request(&dir, &sources);
        request.resume = true;
        let cancel = Cell::new(false);
        let error = compile_basemap(
            &request,
            |progress| cancel.set(cancel.get() || progress.stage == cancel_at),
            || cancel.get(),
        )
        .unwrap_err();
        assert!(matches!(error, CompileError::Cancelled), "{error}");
        let checkpoint = checkpoint_directory(&request);
        assert!(
            checkpoint.join("checkpoint.json").is_file(),
            "{cancel_at:?}"
        );
        // The archive stays once checkpointed, and blocks a compile that is
        // not resuming.
        let archive_kept = cancel_at == CompileStage::BuildingLevels;
        assert_eq!(request.output.exists(), archive_kept, "{cancel_at:?}");
        // A different thread count writes the same archive.
        request.threads = 3;
        let output = compile_basemap(&request, |_| {}, || false).unwrap();
        assert_eq!(output.resumed_from, resumes_at, "{cancel_at:?}");
        assert!(
            fs::read(&output.archive).unwrap() == archive,
            "{cancel_at:?}: the resumed archive differs"
        );
        let (levels_path, _) = output.building_levels.unwrap();
        assert!(fs::read(levels_path).unwrap() == levels, "{cancel_at:?}");
        assert!(!checkpoint.exists(), "{cancel_at:?}: checkpoint left");
        assert!(!request.temp_dir.exists(), "{cancel_at:?}: scratch left");
    }
}

#[test]
fn a_checkpoint_for_another_request_is_discarded() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let dir = temp.path().join("compile");
    let mut request = request(&dir, &sources);
    request.building_levels = false;
    request.resume = true;
    let cancel = Cell::new(false);
    let error = compile_basemap(
        &request,
        |progress| cancel.set(cancel.get() || progress.stage == CompileStage::WriteTiles),
        || cancel.get(),
    )
    .unwrap_err();
    assert!(matches!(error, CompileError::Cancelled), "{error}");
    // Fewer zooms: the sorted features no longer fit.
    request.zoom = osm_tile_compiler::ZoomRange::new(0, 12).unwrap();
    let output = compile_basemap(&request, |_| {}, || false).unwrap();
    assert_eq!(output.resumed_from, None);
    assert!(
        output
            .result
            .warnings
            .iter()
            .any(|warning| warning.contains("discarded the checkpoint")),
        "{:?}",
        output.result.warnings
    );
    let mut plain = self::request(&temp.path().join("plain"), &sources);
    plain.building_levels = false;
    plain.zoom = request.zoom;
    let plain = compile_basemap(&plain, |_| {}, || false).unwrap();
    assert!(fs::read(output.archive).unwrap() == fs::read(plain.archive).unwrap());
}

#[test]
fn a_checkpoint_can_be_discarded() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    let mut request = request(&temp.path().join("compile"), &sources);
    request.resume = true;
    assert!(!discard_checkpoint(&request).unwrap());
    let error = compile_basemap(&request, |_| {}, || true).unwrap_err();
    assert!(matches!(error, CompileError::Cancelled), "{error}");
    assert!(checkpoint_directory(&request).exists());
    assert!(discard_checkpoint(&request).unwrap());
    assert!(!checkpoint_directory(&request).exists());
}

fn cli(args: &[OsString], abort_after: Option<&str>) -> std::process::Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_osm-tile-compiler"));
    command.args(args);
    if let Some(checkpoint) = abort_after {
        command.env("OSM_TILE_COMPILER_TEST_ABORT_AFTER", checkpoint);
    }
    command.output().unwrap()
}

fn compile_args(inputs: &[PathBuf], output: &Path, sources: &Path, resume: bool) -> Vec<OsString> {
    let mut args = vec!["compile".into(), inputs[0].clone().into_os_string()];
    for extra in &inputs[1..] {
        args.push(extra.clone().into_os_string());
    }
    args.extend([
        "--output".into(),
        output.as_os_str().to_owned(),
        "--sources".into(),
        sources.as_os_str().to_owned(),
        "--building-levels".into(),
        "--threads".into(),
        "2".into(),
    ]);
    if resume {
        args.push("--resume".into());
    }
    args
}

#[test]
fn a_killed_cli_compile_resumes_with_the_same_command() {
    let temp = tempfile::tempdir().unwrap();
    let sources = temp.path().join("sources");
    synthetic_sources(&sources);
    // Two extracts, so that merging them is checkpointed too.
    let second = temp.path().join("monaco-copy.osm.pbf");
    fs::copy(monaco_pbf(), &second).unwrap();
    let inputs = [monaco_pbf(), second];
    let reference = temp.path().join("reference.pmtiles");
    let result = cli(&compile_args(&inputs, &reference, &sources, false), None);
    assert!(result.status.success(), "{result:?}");
    let reference_levels = temp.path().join("reference-buildings.pmtiles");
    for checkpoint in ["merged_extract", "rendered", "archive", "levels"] {
        let output = temp.path().join(format!("{checkpoint}.pmtiles"));
        let args = compile_args(&inputs, &output, &sources, true);
        let killed = cli(&args, Some(checkpoint));
        assert!(!killed.status.success(), "{checkpoint}: {killed:?}");
        // An abort is a signal on Unix; on Windows it is the fast-fail exit
        // status STATUS_STACK_BUFFER_OVERRUN.
        let aborted = if cfg!(windows) {
            Some(0xC000_0409_u32 as i32)
        } else {
            None
        };
        assert_eq!(killed.status.code(), aborted, "{checkpoint}: not aborted");
        let scratch = temp.path().join(format!("{checkpoint}.pmtiles.tmp"));
        assert!(scratch.is_dir(), "{checkpoint}: the scratch is gone");
        let resumed = cli(&args, None);
        let stderr = String::from_utf8_lossy(&resumed.stderr);
        assert!(resumed.status.success(), "{checkpoint}: {stderr}");
        assert!(stderr.contains("Resuming from the checkpoint"), "{stderr}");
        assert!(
            stderr.contains("Resumed an interrupted compile"),
            "{stderr}"
        );
        assert!(
            fs::read(&output).unwrap() == fs::read(&reference).unwrap(),
            "{checkpoint}: the resumed archive differs"
        );
        let levels = temp.path().join(format!("{checkpoint}-buildings.pmtiles"));
        assert!(
            fs::read(levels).unwrap() == fs::read(&reference_levels).unwrap(),
            "{checkpoint}: the resumed building levels differ"
        );
        assert!(!scratch.exists(), "{checkpoint}: scratch left");
    }
}

#[test]
fn resume_refuses_change_files() {
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("out.pmtiles");
    let mut args = compile_args(&[monaco_pbf()], &output, temp.path(), true);
    args.extend([
        "--changes".into(),
        temp.path().join("x.osc").into_os_string(),
    ]);
    let result = cli(&args, None);
    assert_eq!(result.status.code(), Some(2), "{result:?}");
    assert!(String::from_utf8_lossy(&result.stderr).contains("--resume does not work"));
}
