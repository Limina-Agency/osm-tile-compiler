//! `martin-config`: a Martin tile server config for archives.
use super::{Command, option, value};
use osm_tile_compiler::martin::{MartinConfigRequest, write_martin_config};
use std::ffi::OsString;
use std::path::PathBuf;

#[derive(Debug)]
pub(super) struct MartinArgs {
    inputs: Vec<PathBuf>,
    output: Option<PathBuf>,
    fonts: Option<PathBuf>,
    sprites: Option<PathBuf>,
    json: bool,
}

pub(super) fn parse(args: &[OsString]) -> Result<Command, String> {
    let mut parsed = MartinArgs {
        inputs: Vec::new(),
        output: None,
        fonts: None,
        sprites: None,
        json: false,
    };
    let mut args = args.iter();
    while let Some(arg) = args.next() {
        let Some(flag) = option(arg) else {
            parsed.inputs.push(PathBuf::from(arg));
            continue;
        };
        match flag.as_str() {
            "--help" | "-h" => return Ok(Command::Help),
            "--output" | "-o" => parsed.output = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--fonts" => parsed.fonts = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--sprites" => parsed.sprites = Some(PathBuf::from(value(&mut args, &flag)?)),
            "--json" => parsed.json = true,
            _ => return Err(format!("unknown martin-config option '{flag}'")),
        }
    }
    if parsed.inputs.is_empty() {
        return Err("martin-config needs at least one archive or directory".into());
    }
    if parsed.output.is_none() {
        return Err("martin-config needs --output".into());
    }
    Ok(Command::MartinConfig(parsed))
}

pub(super) fn run(args: MartinArgs) -> Result<(), String> {
    let mut request = MartinConfigRequest::new(args.inputs, args.output.expect("parsed"));
    request.fonts = args.fonts;
    request.sprites = args.sprites;
    let report = write_martin_config(&request).map_err(|e| e.to_string())?;
    for warning in &report.warnings {
        eprintln!("Warning: {warning}");
    }
    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&report).map_err(|e| e.to_string())?
        );
        return Ok(());
    }
    for source in &report.sources {
        eprintln!(
            "{} ({}, {}{}): tiles /{id}/{{z}}/{{x}}/{{y}}, style /style/{id}",
            source.path.display(),
            source.format,
            source.tile_format,
            if source.sar { ", sar" } else { "" },
            id = source.id,
        );
    }
    eprintln!(
        "Wrote {}; run: martin --config {}",
        report.config.display(),
        report.config.display()
    );
    Ok(())
}
