//! Compiles an OpenStreetMap `.osm.pbf` into an OpenMapTiles-schema basemap,
//! written as a single PMTiles archive. [`omt`] is a native Rust port of the
//! OpenMapTiles 3.16 profile as pinned by Planetiler 0.10.2, checked against
//! Planetiler's output: `OpenMapTilesResult::parity_verified` is `true` for the
//! compiles the parity measurement covers (see `docs/parity.md`). An
//! `osm-tile-compiler` binary is included.
//!
//! Output derived from OpenStreetMap data is under the ODbL: credit
//! "© OpenStreetMap contributors". The OpenMapTiles schema and each
//! supplemental dataset carry their own terms; see the crate README.
//!
//! An app runs a compile in its own process with [`compile_basemap`]: it
//! reports [`CompileProgress`] (a [`CompileStage`], a message and fractions)
//! and stops promptly with [`CompileError::Cancelled`] when asked:
//!
//! ```no_run
//! use osm_tile_compiler::{
//!     BasemapRequest, CompileError, SourcesPolicy, SupplementalDataset, compile_basemap,
//! };
//! use std::sync::atomic::{AtomicBool, Ordering};
//!
//! let cancel = AtomicBool::new(false);
//! let mut request = BasemapRequest::new(
//!     "/data/monaco.osm.pbf",
//!     "/data/maps/monaco.pmtiles",
//!     "/data/omt-sources",
//!     "/data/tmp",
//! );
//! request.threads = 2;
//! request.sources_policy = SourcesPolicy::UsePresent;
//! // Without the shared map data: the map lacks what
//! // `SupplementalDataset::without_warning` says.
//! request.omit_sources = SupplementalDataset::ALL.to_vec();
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
//! ```
//!
//! [`omt::compile_openmaptiles`] is the compile alone, from local inputs, on
//! the calling thread, without stages or cancelling.

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
mod chunk_store;
mod compile_area;
mod compile_bounds;
mod control;
mod coordinate_groups;
pub mod extracts;
#[cfg(feature = "fuzzing")]
#[doc(hidden)]
pub mod fuzzing;
mod generalization;
pub mod omt;
mod profiling;
pub mod sources;
mod tile_geometry;
mod tile_slicer;
pub mod tile_stats;

pub use basemap::{
    BasemapOutput, BasemapRequest, MANIFEST_FORMAT, MANIFEST_VERSION, SourcesPolicy,
    compile_basemap, pbf_bounds,
};
pub use building_levels::{
    BUILDING_LEVELS_FORMAT, BUILDING_LEVELS_VERSION, BuildingLevelReport, BuildingLevelsReport,
    building_levels_path, write_building_levels, write_building_levels_beside,
    write_building_levels_cancellable,
};
pub use compile_area::CompileArea;
pub use compile_bounds::{ArchiveCenter, CompileBounds};
pub use control::{CompileError, CompileProgress, CompileStage};
pub use extracts::{Extract, ExtractEvent, fetch_extract};
pub use sources::{CachedInput, FetchEvent, FetchMode, SupplementalDataset, fetch_sources};
