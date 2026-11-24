//! Offline search: a small index of the names in a compiled archive, written
//! beside it, that finds places, points of interest, streets, peaks, waters
//! and search-and-rescue features by name with no network.
//!
//! The index is built from the archive's highest zoom (z14 unless the
//! compile set another), so it holds what the map shows and can be built
//! again on a device from an archive cut with [`crate::archive::extract`].
//! Names are matched by words, without regard to case or diacritics:
//! `lodz`, `Łódź` and `LODZ` are the same, and the last word of a query
//! matches as a prefix, so `krak` finds Kraków. A word may also match the
//! nearest town, so `dluga krakow` finds the streets called Długa in Kraków.
//!
//! The file is read with positioned reads: opening it reads its header and
//! dictionary index (tens of kilobytes for a region), and a query reads only
//! the parts it needs. The format is in `docs/search.md`.

mod build;
mod fold;
mod index;

pub(crate) use build::write_index;
pub use build::{
    SearchIndexReport, write_search_index, write_search_index_beside,
    write_search_index_cancellable,
};
pub use fold::fold as fold_search_text;
pub use index::{SearchHit, SearchIndex, SearchQuery};

use std::path::{Path, PathBuf};

/// The search index's metadata `format`.
pub const SEARCH_INDEX_FORMAT: &str = "search-index";
/// Its format version. See `docs/search.md` for when it changes.
pub const SEARCH_INDEX_VERSION: u32 = 1;

/// Where `compile --search-index` writes an archive's search index: beside
/// it, named after it, so `basemap.pmtiles` gets `basemap-search.idx`.
pub fn search_index_path(archive: &Path) -> PathBuf {
    let stem = archive
        .file_stem()
        .map_or_else(|| "basemap".into(), |s| s.to_string_lossy());
    archive.with_file_name(format!("{stem}-search.idx"))
}

const MAGIC: &[u8; 8] = b"OTCSRCH\0";
/// The sections, in file order.
const META: usize = 0;
const KINDS: usize = 1;
const POINTS: usize = 2;
const ATTRS: usize = 3;
const RECORD_BLOCKS: usize = 4;
const RECORDS: usize = 5;
const DICT_INDEX: usize = 6;
const DICT: usize = 7;
const POSTINGS: usize = 8;
const SECTIONS: usize = 9;
/// Magic, version, section count, then an offset and length per section.
const HEADER_BYTES: usize = 16 + SECTIONS * 16;
/// Longitude and latitude, `i32` in units of 1e-7 degrees.
const POINT_BYTES: usize = 8;
/// The kind (`u16`), the context record (`u32`, [`NO_CONTEXT`] for none) and
/// the rank (`u8`, lower is more prominent).
const ATTR_BYTES: usize = 7;
const NO_CONTEXT: u32 = u32::MAX;
const RECORDS_PER_BLOCK: usize = 64;
const TOKENS_PER_BLOCK: usize = 64;
/// Record flags: which optional fields follow the names.
const HAS_ELE: u8 = 1;
const HAS_REF: u8 = 2;
const HAS_ID: u8 = 4;

/// What a record is: its layer, class and subclass in the archive, and its
/// tier, 0 for the most prominent (countries, cities) to 9.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
struct Kind {
    tier: u8,
    layer: String,
    class: String,
    subclass: String,
}

fn put_varint(out: &mut Vec<u8>, mut value: u64) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

fn varint(bytes: &[u8], at: &mut usize) -> Result<u64, String> {
    let mut value = 0u64;
    for shift in (0..64).step_by(7) {
        let byte = *bytes
            .get(*at)
            .ok_or("the search index ends inside a number")?;
        *at += 1;
        value |= u64::from(byte & 0x7f) << shift;
        if byte < 0x80 {
            return Ok(value);
        }
    }
    Err("the search index has a number longer than 10 bytes".into())
}

fn zigzag(value: i64) -> u64 {
    ((value << 1) ^ (value >> 63)) as u64
}

fn unzigzag(value: u64) -> i64 {
    (value >> 1) as i64 ^ -((value & 1) as i64)
}

/// Metres between two points, by the haversine formula.
fn distance_m(a: [f64; 2], b: [f64; 2]) -> f64 {
    const EARTH_RADIUS_M: f64 = 6_371_008.8;
    let (lat1, lat2) = (a[1].to_radians(), b[1].to_radians());
    let dlat = lat2 - lat1;
    let dlon = (b[0] - a[0]).to_radians();
    let h = (dlat / 2.0).sin().powi(2) + lat1.cos() * lat2.cos() * (dlon / 2.0).sin().powi(2);
    2.0 * EARTH_RADIUS_M * h.sqrt().min(1.0).asin()
}
