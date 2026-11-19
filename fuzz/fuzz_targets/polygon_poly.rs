//! `--polygon` files in the Osmosis `.poly` format.
#![no_main]
use libfuzzer_sys::fuzz_target;
use osm_tile_compiler::CompileArea;

#[path = "polygon_area.rs"]
mod polygon_area;

fuzz_target!(|data: &[u8]| {
    let Ok(text) = std::str::from_utf8(data) else {
        return;
    };
    if let Ok(area) = CompileArea::from_poly(text) {
        polygon_area::exercise(&area, data);
    }
});
