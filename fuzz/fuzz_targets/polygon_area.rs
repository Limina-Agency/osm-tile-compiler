// Shared by the polygon targets: everything a compile asks of an area.
use osm_tile_compiler::CompileArea;

pub fn exercise(area: &CompileArea, data: &[u8]) {
    let bounds = area.bounds();
    assert!(bounds.north >= bounds.south, "{bounds:?}");
    if let Some((longitude, latitude)) = area.interior_point() {
        assert!(longitude.abs() <= 180.0, "{longitude}");
        area.contains(longitude, latitude);
    }
    // A zoom and a tile from the input's last bytes; coverage is built up to
    // z10 at most, which keeps each run short.
    let tail = |n: usize| data.get(data.len().saturating_sub(n)).copied().unwrap_or(0);
    let max_zoom = tail(1) % 11;
    let x = u32::from_le_bytes([tail(2), tail(3), tail(4), 0]);
    let y = u32::from_le_bytes([tail(5), tail(6), tail(7), 0]);
    osm_tile_compiler::fuzzing::tile_coverage(area, max_zoom, x, y);
}
