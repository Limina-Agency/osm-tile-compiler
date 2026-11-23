//! Pins the versions of the formats the compiler writes. Changing one is a
//! deliberate act: follow `docs/formats.md`, then update this test.

use osm_tile_compiler::OMT_SCHEMA_VERSION;
use osm_tile_compiler::archive::{DELTA_FORMAT, DELTA_VERSION};
use osm_tile_compiler::{
    BUILDING_LEVELS_FORMAT, BUILDING_LEVELS_VERSION, MANIFEST_FORMAT, MANIFEST_VERSION,
    TREES_FORMAT, TREES_VERSION,
};

#[test]
fn format_versions_are_pinned() {
    assert_eq!((MANIFEST_FORMAT, MANIFEST_VERSION), ("basemap-manifest", 1));
    assert_eq!(
        (BUILDING_LEVELS_FORMAT, BUILDING_LEVELS_VERSION),
        ("building-levels", 1)
    );
    assert_eq!((TREES_FORMAT, TREES_VERSION), ("trees", 1));
    assert_eq!(OMT_SCHEMA_VERSION, "3.16.0");
    assert_eq!(
        (DELTA_FORMAT, DELTA_VERSION),
        ("osm-tile-compiler-delta", 1)
    );
}
