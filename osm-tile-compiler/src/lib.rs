//! Compiles an OpenStreetMap `.osm.pbf` into an OpenMapTiles-schema basemap,
//! written as a single PMTiles or MBTiles archive. Its profile is a native Rust port of the
//! OpenMapTiles 3.16 profile as pinned by Planetiler 0.10.2, checked against
//! Planetiler's output: [`CompileResult::parity_verified`] is `true` for the
//! compiles the parity measurement covers (see `docs/parity.md`). An
//! `osm-tile-compiler` binary is included.
//!
//! Output derived from OpenStreetMap data is under the ODbL: credit
//! "© OpenStreetMap contributors". The OpenMapTiles schema and each
//! supplemental dataset carry their own terms; see the crate README.
//!
//! [`compile_basemap`] is the one entry point for a compile. An app runs it
//! in its own process: it reports [`CompileProgress`] (a [`CompileStage`], a
//! message and fractions) and stops promptly with [`CompileError::Cancelled`]
//! when asked:
//!
//! ```no_run
//! use osm_tile_compiler::{
//!     Area, BasemapRequest, CompileBounds, CompileError, LayerSelection, ReadScope,
//!     SourcesPolicy, SupplementalDataset, ZoomRange, compile_basemap,
//! };
//! use std::sync::atomic::{AtomicBool, Ordering};
//!
//! # fn main() -> Result<(), CompileError> {
//! let cancel = AtomicBool::new(false);
//! let mut request = BasemapRequest::new(
//!     "/data/poland.osm.pbf",
//!     "/data/maps/krakow.pmtiles",
//!     "/data/omt-sources",
//!     "/data/tmp",
//! );
//! request.threads = 2;
//! request.zoom = ZoomRange::new(0, 14)?;
//! // A city from a country extract, reading only the data near it.
//! request.area = Area::bounds(CompileBounds::new(50.13, 49.97, 20.22, 19.79))
//!     .reading(ReadScope::NearArea);
//! request.layers = LayerSelection::except(["housenumber"]);
//! request.sources_policy = SourcesPolicy::UsePresent;
//! // Without the shared map data: the map lacks what
//! // `SupplementalDataset::without_warning` says.
//! request.omit_sources = SupplementalDataset::ALL.to_vec();
//! // Optional: compile_basemap runs the same checks first.
//! request.validate()?;
//! match compile_basemap(
//!     &request,
//!     |progress| println!("{} {:.0}%", progress.message, progress.overall * 100.0),
//!     || cancel.load(Ordering::Relaxed),
//! ) {
//!     Ok(output) => println!("{} tiles", output.result.tiles),
//!     Err(CompileError::Cancelled) => println!("stopped"),
//!     // InvalidRequest, Download, OutOfSpace, Failed { stage, .. }, and any
//!     // kind added later; `kind()` gives a stable identifier.
//!     Err(error) => println!("{}: {error}", error.kind()),
//! }
//! # Ok(())
//! # }
//! ```
//!
//! Every public type that may grow is `#[non_exhaustive]`: build requests
//! with their constructors and set fields, and match enums with a wildcard
//! arm. Every fallible function returns [`CompileError`].
//!
//! Besides compiling, the crate fetches the inputs ([`fetch_sources`],
//! [`fetch_extract`]), writes building levels for an archive
//! ([`write_building_levels`]) and a search index of its names
//! ([`write_search_index`], queried with [`SearchIndex`]), inspects, cuts, diffs and patches archives
//! ([`archive`]), and writes the MapLibre style for an archive
//! ([`maplibre_style`]) or an offline pack of it with its fonts and icons
//! ([`write_style_pack`]), and serves archives and a pack over HTTP for a
//! preview ([`TileServer`]).

/// The message a panic was raised with, for errors that report a caught
/// panic. Shared with the binary.
#[doc(hidden)]
pub fn panic_message(payload: &(dyn std::any::Any + Send)) -> &str {
    payload
        .downcast_ref::<&str>()
        .copied()
        .or_else(|| payload.downcast_ref::<String>().map(String::as_str))
        .unwrap_or("no message")
}

pub mod archive;
mod basemap;
mod building_levels;
mod compile_area;
mod compile_bounds;
mod control;
mod coordinate_groups;
mod data_sources;
mod extracts;
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing;
mod generalization;
mod hd_roads;
pub mod martin;
mod memory;
mod mlt;
mod omt;
mod output;
mod profiling;
mod request;
mod resume;
mod search;
mod serve;
mod sources;
pub mod stage_profile;
mod style_pack;
mod tile_geometry;
mod tile_list;
mod tile_slicer;
mod tile_stats;
mod updates;

pub use basemap::{
    BasemapOutput, BasemapRequest, MANIFEST_FORMAT, MANIFEST_VERSION, SourcesPolicy,
    checkpoint_directory, compile_basemap, discard_checkpoint, pbf_bounds,
};
pub use building_levels::{
    BUILDING_LEVELS_FORMAT, BUILDING_LEVELS_VERSION, BuildingLevelReport, BuildingLevelsReport,
    building_levels_path, write_building_levels, write_building_levels_beside,
    write_building_levels_cancellable,
};
pub use compile_area::CompileArea;
pub use compile_bounds::{ArchiveCenter, CompileBounds};
pub use control::{CompileError, CompileProgress, CompileStage};
pub use data_sources::{
    CsvColumns, DataFormat, DataInput, DataLayer, DataLayerReport, DataReadReport, PropertyValue,
    SourceFeature,
};
pub use extracts::{
    Extract, ExtractEvent, GEOFABRIK_INDEX_URL, fetch_extract, fetch_extract_from, find_extract,
    parse_index,
};
pub use hd_roads::{
    HD_ROADS_FORMAT, HD_ROADS_VERSION, HdRoadsReport, hd_roads_path, write_hd_roads,
};
pub use omt::{
    CompileResult, LANGUAGES as OMT_LANGUAGES, LAYERS as OMT_LAYERS, MergeCacheOptions,
    MergeCacheStats, OMT_SCHEMA_VERSION, OsmDataInfo, PROFILE_REVISION as OMT_PROFILE_REVISION,
    SAR_LAYER, SAR_SCHEMA_VERSION, merge_cache_path,
    schema::{ExampleResult, Schema, TileSchema, shortbread},
};
pub use output::{OutputFormat, TileCompression, TileFormat};
pub use request::{Area, AreaPolygon, LayerName, LayerSelection, ReadScope, ZoomRange};
pub use search::{
    SEARCH_INDEX_FORMAT, SEARCH_INDEX_VERSION, SearchHit, SearchIndex, SearchIndexReport,
    SearchQuery, fold_search_text, search_index_path, write_search_index,
    write_search_index_beside, write_search_index_cancellable,
};
pub use serve::{ServeRequest, ServedArchive, TileServer};
pub use sources::{CachedInput, FetchEvent, FetchMode, SupplementalDataset, fetch_sources};
pub use style_pack::{
    DEFAULT_GLYPHS_URL, DEFAULT_SPRITE_URL, STYLE_FONTSTACKS, StylePackEvent, StylePackReport,
    StylePackRequest, maplibre_style, with_sar_layers, with_tile_format, write_style_pack,
};
pub use tile_list::{CHANGED_TILE_BUFFER_PIXELS, TileAddress, read_tile_list};
pub use tile_stats::{LayerStats, TILE_WARNING_BYTES, TileSize, TileStats, ZoomStats};
pub use updates::{
    ChangedTilesReport, ChangedTilesRequest, ClipTo, DEFAULT_MAX_DIFFS, PLANET_REPLICATION,
    Replication, UpdateEvent, UpdateReport, UpdateRequest, update_extract,
};

/// The languages whose `name:<lang>` attributes a
/// [`BasemapRequest::languages`] list keeps, in [`OMT_LANGUAGES`] order. It
/// reads like Planetiler's `--languages` with the profile's languages as the
/// default set: a code, optionally prefixed with `name:`; `default` for every
/// language of the profile; `-code` to leave a language out even if listed.
/// As in Planetiler, a list of exclusions alone keeps nothing; `default,-ru`
/// keeps all but Russian. Unlike Planetiler, codes outside the profile's
/// languages, and `*` (every `name:*` tag), are refused with
/// [`CompileError::InvalidRequest`]: the profile only ever writes its own
/// languages.
pub fn resolve_languages(languages: &[String]) -> Result<Vec<&'static str>, CompileError> {
    omt::resolve_languages(languages).map_err(CompileError::InvalidRequest)
}

/// Formats a byte count in KiB, as the command line prints tile sizes.
#[doc(hidden)]
pub fn kib(bytes: u64) -> String {
    tile_stats::kib(bytes)
}
