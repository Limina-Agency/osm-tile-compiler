//! Pins the versions of the formats the compiler writes. Changing one is a
//! deliberate act: follow `docs/formats.md`, then update this test.

use osm_tile_compiler::OMT_SCHEMA_VERSION;
use osm_tile_compiler::archive::{
    DELTA_FORMAT, DELTA_VERSION, SIGNATURE_FORMAT, SIGNATURE_VERSION,
};
use osm_tile_compiler::{
    BUILDING_LEVELS_FORMAT, BUILDING_LEVELS_VERSION, HD_ROADS_FORMAT, HD_ROADS_VERSION,
    MANIFEST_FORMAT, MANIFEST_VERSION, SEARCH_INDEX_FORMAT, SEARCH_INDEX_VERSION, TREES_FORMAT,
    TREES_VERSION,
};

#[test]
fn format_versions_are_pinned() {
    assert_eq!((MANIFEST_FORMAT, MANIFEST_VERSION), ("basemap-manifest", 1));
    assert_eq!(
        (BUILDING_LEVELS_FORMAT, BUILDING_LEVELS_VERSION),
        ("building-levels", 1)
    );
    assert_eq!(
        (SEARCH_INDEX_FORMAT, SEARCH_INDEX_VERSION),
        ("search-index", 1)
    );
    assert_eq!((HD_ROADS_FORMAT, HD_ROADS_VERSION), ("hd-roads", 1));
    assert_eq!((TREES_FORMAT, TREES_VERSION), ("trees", 2));
    assert_eq!(OMT_SCHEMA_VERSION, "3.16.0");
    assert_eq!(
        (DELTA_FORMAT, DELTA_VERSION),
        ("osm-tile-compiler-delta", 2)
    );
    assert_eq!(
        (SIGNATURE_FORMAT, SIGNATURE_VERSION),
        ("osm-tile-compiler-signature", 1)
    );
}
