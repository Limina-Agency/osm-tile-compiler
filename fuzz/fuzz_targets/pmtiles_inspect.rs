//! The PMTiles reader behind `inspect` and `verify`: header, directories,
//! metadata and every tile's MVT.
#![no_main]
use libfuzzer_sys::fuzz_target;

#[path = "scratch.rs"]
mod scratch;

fuzz_target!(|data: &[u8]| {
    let path = scratch::file("input.pmtiles", data);
    let _ = osm_tile_compiler::archive::inspect(&path);
});
