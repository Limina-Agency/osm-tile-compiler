//! Drives `compile_basemap` as an app would, printing its progress, and can
//! cancel part-way to time how fast a cancel takes effect:
//!
//! ```sh
//! cargo run --release --example compile_basemap -- INPUT.pbf OUTPUT.pmtiles \
//!   [--sources DIR] [--omit-sources all|ID,...] [--threads N] \
//!   [--cancel-at STAGE_ID] [--after SECONDS]
//! ```
//!
//! `--cancel-at read_osm --after 2` cancels two seconds into that stage and
//! prints how long the compile took to return. Scratch space is
//! `<OUTPUT>.tmp`.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use osm_tile_compiler::{
    BasemapRequest, CompileError, CompileStage, SupplementalDataset, compile_basemap,
};
use std::cell::Cell;
use std::path::PathBuf;
use std::time::{Duration, Instant};

fn main() -> Result<(), String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let [input, output, ..] = &args[..] else {
        return Err(
            "usage: compile_basemap INPUT.pbf OUTPUT.pmtiles [--sources DIR] \
             [--omit-sources all|ID,...] [--threads N] [--cancel-at STAGE_ID] [--after SECONDS]"
                .into(),
        );
    };
    let output = PathBuf::from(output);
    let mut temp = output.clone().into_os_string();
    temp.push(".tmp");
    let mut request = BasemapRequest::new(input, &output, "", PathBuf::from(temp));
    let mut cancel_at = None;
    let mut after = Duration::ZERO;
    let mut rest = args[2..].iter();
    while let Some(flag) = rest.next() {
        let value = rest.next().ok_or(format!("{flag} needs a value"))?;
        match flag.as_str() {
            "--sources" => request.sources = value.into(),
            "--threads" => request.threads = value.parse().map_err(|e| format!("{e}"))?,
            "--omit-sources" if value == "all" => {
                request.omit_sources = SupplementalDataset::ALL.to_vec()
            }
            "--omit-sources" => {
                request.omit_sources = value
                    .split(',')
                    .map(|id| SupplementalDataset::from_id(id).ok_or(format!("unknown {id}")))
                    .collect::<Result<_, _>>()?
            }
            "--cancel-at" => {
                cancel_at = Some(
                    CompileStage::ALL
                        .into_iter()
                        .find(|stage| stage.id() == value)
                        .ok_or(format!("unknown stage {value}"))?,
                )
            }
            "--after" => {
                after = Duration::from_secs_f64(value.parse().map_err(|e| format!("{e}"))?)
            }
            _ => return Err(format!("unknown option {flag}")),
        }
    }
    let start = Instant::now();
    let reached = Cell::new(None::<Instant>);
    let asked = Cell::new(None::<Instant>);
    let result = compile_basemap(
        &request,
        |progress| {
            if Some(progress.stage) == cancel_at && reached.get().is_none() {
                reached.set(Some(Instant::now()));
            }
            eprintln!(
                "[{:>7.2}s] {:>3.0}% {} {}{}",
                start.elapsed().as_secs_f64(),
                progress.overall * 100.0,
                progress.stage.id(),
                progress.message,
                progress
                    .fraction
                    .map(|f| format!(" ({:.0}%)", f * 100.0))
                    .unwrap_or_default()
            );
        },
        || {
            if asked.get().is_some() {
                return true;
            }
            if reached.get().is_some_and(|at| at.elapsed() >= after) {
                asked.set(Some(Instant::now()));
                return true;
            }
            false
        },
    );
    match (result, asked.get()) {
        (Err(CompileError::Cancelled), Some(at)) => {
            eprintln!(
                "cancelled: returned {:.3} s after the cancel was asked for",
                at.elapsed().as_secs_f64()
            );
            Ok(())
        }
        (Ok(output), _) => {
            eprintln!(
                "{} tiles in {:.1} s, omitted: {:?}",
                output.result.tiles,
                start.elapsed().as_secs_f64(),
                output.result.omitted_sources
            );
            Ok(())
        }
        (Err(error), _) => Err(error.to_string()),
    }
}
