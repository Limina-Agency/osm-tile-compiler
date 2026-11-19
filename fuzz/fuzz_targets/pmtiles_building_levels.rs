//! The PMTiles reader of `building-levels`, which reads an archive's
//! metadata and its z14 building tiles.
#![no_main]
use libfuzzer_sys::fuzz_target;
use osm_tile_compiler::{building_levels_path, write_building_levels_beside};

#[path = "scratch.rs"]
mod scratch;

fuzz_target!(|data: &[u8]| {
    let input = scratch::file("input.pmtiles", data);
    let _ = std::fs::remove_file(building_levels_path(&input));
    let _ = write_building_levels_beside(&input, 1);
});
