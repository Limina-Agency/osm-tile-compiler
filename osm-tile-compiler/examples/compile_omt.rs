//! Native OpenMapTiles benchmark entry point; production callers use the Rust API.
#[cfg(feature = "mimalloc")]
#[global_allocator]
static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

use osm_tile_compiler::{
    CompileBounds,
    omt::{OpenMapTilesRequest, compile_openmaptiles},
};
use std::path::PathBuf;
fn main() -> Result<(), String> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.len() < 3 {
        return Err("usage: compile_omt INPUT.pbf OUTPUT_DIR SOURCES_DIR --bounds W S E N [--min-zoom 0] [--max-zoom 14] [--threads 4]".into());
    }
    let mut bounds = None;
    let mut min_zoom = 0;
    let mut max_zoom = 14;
    let mut threads = 4;
    let mut i = 3;
    while i < args.len() {
        match args[i].as_str() {
            "--bounds" => {
                let v = args
                    .get(i + 1..i + 5)
                    .ok_or("bounds needs W S E N")?
                    .iter()
                    .map(|s| s.parse::<f64>().map_err(|e| e.to_string()))
                    .collect::<Result<Vec<_>, _>>()?;
                bounds = Some(CompileBounds::new(v[3], v[1], v[2], v[0]));
                i += 5;
            }
            "--min-zoom" | "--max-zoom" | "--threads" => {
                let value = args.get(i + 1).ok_or("missing option value")?;
                match args[i].as_str() {
                    "--min-zoom" => min_zoom = value.parse::<u8>().map_err(|e| e.to_string())?,
                    "--max-zoom" => max_zoom = value.parse::<u8>().map_err(|e| e.to_string())?,
                    _ => threads = value.parse::<usize>().map_err(|e| e.to_string())?,
                };
                i += 2;
            }
            other => return Err(format!("unknown argument {other}")),
        }
    }
    let out = PathBuf::from(&args[1]);
    let request = OpenMapTilesRequest {
        input_pbf: PathBuf::from(&args[0]),
        sources: PathBuf::from(&args[2]),
        output: out.join("basemap.pmtiles"),
        temp_dir: out.join("temp"),
        bounds: bounds.ok_or("--bounds is required")?,
        area: None,
        center: None,
        archive: Default::default(),
        min_zoom,
        max_zoom,
        threads,
        layers: None,
        languages: None,
        omit_sources: Vec::new(),
        area_only: false,
    };
    let result = compile_openmaptiles(&request)?;
    let json = serde_json::to_string_pretty(&result).map_err(|e| e.to_string())?;
    std::fs::write(out.join("compile-result.json"), &json).map_err(|e| e.to_string())?;
    println!("{json}");
    Ok(())
}
