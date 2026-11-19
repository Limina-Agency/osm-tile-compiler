//! Entry points for the cargo-fuzz targets in `fuzz/`, behind the `fuzzing`
//! feature. Not an API: they change whenever the internals do.
use crate::CompileArea;
use crate::compile_area::TileCoverage;

/// Builds the tile coverage of `area` from zoom 0 to `max_zoom` and asks it
/// about the tile at (`x`, `y`) of each zoom, scaled down from `max_zoom`.
pub fn tile_coverage(area: &CompileArea, max_zoom: u8, x: u32, y: u32) {
    let coverage = TileCoverage::new(area, 0, max_zoom);
    for z in 0..=max_zoom {
        let shift = max_zoom - z;
        coverage.contains(z, x >> shift, y >> shift);
    }
}
